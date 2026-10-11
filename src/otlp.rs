use crate::{record::SpanRecord,Result};
use serde::{Deserialize,Serialize};
use std::{collections::BTreeMap,path::{Path,PathBuf}};
use serde_json::{json, Value};

// Match Respan shared.ts exactly: JS charCodeAt iterates UTF-16, not UTF-8.
pub fn trace_id(key: &str) -> String {
    let units: Vec<u16> = key.encode_utf16().collect();
    let a = hash(units.iter().copied(), 31); let b = hash(units.iter().rev().copied(), 127);
    format!("{a:08x}{b:08x}{:08x}{:08x}", (units.len() as u32).wrapping_mul(2654435761), a ^ b)
}
pub fn span_id(key: &str) -> String {
    let units: Vec<u16> = key.encode_utf16().collect();
    format!("{:08x}{:08x}", hash(units.iter().copied(), 31), hash(units.iter().rev().copied(), 7))
}
fn hash(units: impl Iterator<Item = u16>, multiplier: u32) -> u32 { units.fold(0u32, |h, c| h.wrapping_mul(multiplier).wrapping_add(c as u32)) }
// In-memory batch helper; the sender uses durable correlate_proxy so early requests can be held.
pub fn reparent(records: &mut [SpanRecord]) {
    for i in 0..records.len() {
        if records[i].source != "proxy" || records[i].run_id.is_none() { continue; }
        let parent = records.iter().filter(|r| r.source != "proxy" && ["agent", "workflow"].contains(&r.log_type.as_str())
            && r.run_id == records[i].run_id && r.start_ns <= records[i].start_ns)
            .max_by_key(|r| r.start_ns).map(|r| (r.span_key.clone(), r.trace_key.clone().or_else(|| Some(r.span_key.clone()))));
        if let Some((key, trace)) = parent { records[i].parent_key = Some(key); records[i].trace_key = trace; }
    }
}
// Persist correlation at turn start, rather than waiting for the root span's spool record.
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
pub(crate) struct TurnWindow {pub trace_key:String,pub span_key:String,pub start_ns:u64,pub end_ns:Option<u64>}
fn run_file(state:&Path,directory:&str,run:&str)->Result<PathBuf> {
    if run.is_empty()||run.len()>128||!run.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_') {return Err("invalid proxy run identifier".into());}
    Ok(state.join("proxy").join(directory).join(format!("{run}.json")))
}
fn load_turns(path:&Path)->Result<Vec<TurnWindow>> {
    match std::fs::read(path) {Ok(bytes)=>Ok(serde_json::from_slice(&bytes)?),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(Vec::new()),Err(e)=>Err(e.into())}
}
pub(crate) fn save_turn(state:&Path,run:&str,trace:&str,start:u64,end:Option<u64>)->Result<()> {
    let path=run_file(state,"turns",run)?;let mut turns=load_turns(&path)?;let span_key=format!("{trace}:turn");
    if let Some(turn)=turns.iter_mut().find(|t|t.span_key==span_key) {
        if turn.start_ns==start&&turn.end_ns==end {return Ok(());}turn.start_ns=start;turn.end_ns=end;
    } else {turns.push(TurnWindow {trace_key:trace.into(),span_key,start_ns:start,end_ns:end});}
    crate::paths::atomic_json(&path,&turns)
}
/// True entries are recent unmatched proxy requests that must remain in the spool.
pub(crate) fn correlate_proxy(state:&Path,records:&mut [SpanRecord],now:u64)->Result<Vec<bool>> {
    let mut cache:BTreeMap<String,(Vec<TurnWindow>,bool)>=BTreeMap::new();let mut held=Vec::with_capacity(records.len());
    for record in records {
        if record.source!="proxy" {held.push(false);continue;}let Some(run)=record.run_id.as_deref() else {held.push(false);continue;};
        if !cache.contains_key(run) {let turns=load_turns(&run_file(state,"turns",run)?)?;let registered=run_file(state,"runs",run)?.exists();cache.insert(run.into(),(turns,registered));}
        let (turns,registered)=cache.get(run).expect("run cached");
        if let Some(turn)=turns.iter().filter(|t|t.start_ns<=record.start_ns).max_by_key(|t|t.start_ns) {
            record.trace_key=Some(turn.trace_key.clone());record.parent_key=Some(turn.span_key.clone());held.push(false);
        } else if *registered&&now.saturating_sub(record.start_ns)<=120_000_000_000 {held.push(true);}
        else {record.trace_key=Some(run.into());record.parent_key=None;held.push(false);}
    }Ok(held)
}
pub(crate) fn gc_turns(state:&Path,now:u64)->Result<()> {
    let directory=state.join("proxy/turns");let entries=match std::fs::read_dir(&directory) {Ok(entries)=>entries,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into())};
    for entry in entries {
        let entry=entry?;let path=entry.path();if !entry.file_type()?.is_file()||path.extension().is_none_or(|x|x!="json") {continue;}
        let Some(run)=path.file_stem().and_then(|s|s.to_str()) else {continue;};let gone=path.with_extension("gone");
        if run_file(state,"runs",run)?.exists() {match std::fs::remove_file(&gone) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}continue;}
        // First-observed absence is conservative: an old turn may belong to a run removed just now.
        let since=match std::fs::read(&gone) {Ok(bytes)=>serde_json::from_slice::<u64>(&bytes)?,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{crate::paths::atomic_json(&gone,&now)?;now},Err(e)=>return Err(e.into())};
        if now.saturating_sub(since)>3_600_000_000_000 {std::fs::remove_file(&path)?;std::fs::remove_file(gone)?;}
    }Ok(())
}
fn string(attrs: &mut Vec<Value>, key: &str, value: impl Into<String>) { attrs.push(json!({"key":key,"value":{"stringValue":value.into()}})); }
fn int(attrs: &mut Vec<Value>, key: &str, value: Option<u64>) { if let Some(value) = value { attrs.push(json!({"key":key,"value":{"intValue":value.to_string()}})); } }
fn double(attrs: &mut Vec<Value>, key: &str, value: f64) { attrs.push(json!({"key":key,"value":{"doubleValue":value}})); }
fn content(value: &Value) -> String { if let Value::String(s) = value { s.clone() } else { value.to_string() } }
fn messages(attrs: &mut Vec<Value>, prefix: &str, value: &Value, default_role: &str) {
    let values = value.as_array().map(Vec::as_slice).unwrap_or_else(|| std::slice::from_ref(value));
    for (n, message) in values.iter().enumerate() {
        string(attrs, &format!("{prefix}.{n}.role"), message.get("role").and_then(Value::as_str).unwrap_or(default_role));
        string(attrs, &format!("{prefix}.{n}.content"), content(message.get("content").unwrap_or(message)));
        if let Some(calls) = tool_calls(message) { string(attrs, &format!("{prefix}.{n}.tool_calls"), calls.to_string()); }
    }
}
/// OpenAI-shaped `tool_calls` (the contract's shape) from native assistant blocks: Anthropic `tool_use`,
/// OpenAI Responses `function_call` and Gemini `functionCall` parts.
fn tool_calls(message: &Value) -> Option<Value> {
    if let Some(calls) = message.get("tool_calls") { return Some(calls.clone()); }
    let blocks = ["content", "parts"].iter().filter_map(|k| message.get(k)?.as_array()).flatten().chain(std::iter::once(message));
    let calls: Vec<Value> = blocks.filter_map(|b| {
        let (id, name, args) = match b.get("type").and_then(Value::as_str) {
            Some("tool_use") => (b.get("id"), b.get("name")?, b.get("input")),
            Some("function_call") => (b.get("call_id").or_else(|| b.get("id")), b.get("name")?, b.get("arguments")),
            _ => { let call = b.get("functionCall")?; (call.get("id"), call.get("name")?, call.get("args")) }
        };
        let arguments = match args { Some(Value::String(s)) => s.clone(), Some(v) => v.to_string(), None => "{}".into() };
        let mut call = json!({"type":"function","function":{"name":name,"arguments":arguments}});
        if let Some(id) = id.filter(|v| v.is_string()) { call["id"] = id.clone(); }
        Some(call)
    }).collect();
    (!calls.is_empty()).then(|| Value::Array(calls))
}
pub fn payload(records: &[SpanRecord]) -> Value {
    let spans: Vec<Value> = records.iter().map(|r| {
        let mut attrs = Vec::new();
        string(&mut attrs, "respan.entity.log_type", &r.log_type);
        string(&mut attrs, "traceloop.entity.name", &r.name);
        if r.parent_key.is_none() { string(&mut attrs, "traceloop.entity.path", ""); }
        if let Some(thread) = &r.thread_id { string(&mut attrs, "respan.threads.thread_identifier", thread); }
        if let Some(customer) = r.metadata.get("customer_identifier").and_then(Value::as_str) { string(&mut attrs, "respan.customer_params.customer_identifier", customer); }
        // Custom IDs view: one row per repository (requests, tokens, cost, errors, TTFT).
        if let Some(repo) = r.metadata.get("git.repo").and_then(Value::as_str).filter(|s| !s.is_empty()) { string(&mut attrs, "respan.span_params.custom_identifier", repo); }
        // Canonical JSON bucket plus flattened `respan.metadata.<key>`: live ingest (verified 2026-10-10) only promotes the
        // flattened form into searchable metadata, as Respan's own JS instrumentations do (respan-instrumentation-eve shared.ts).
        let mut metadata: serde_json::Map<String, Value> = r.metadata.iter().filter(|(k, _)| k.as_str() != "customer_identifier").map(|(k, v)| (k.clone(), v.clone())).collect();
        metadata.insert("agent".into(), json!(r.agent)); metadata.insert("source".into(), json!(r.source));
        for (key, value) in &metadata { string(&mut attrs, &format!("respan.metadata.{key}"), content(value)); }
        string(&mut attrs, "respan.metadata", Value::Object(metadata).to_string());
        if r.log_type == "chat" {
            string(&mut attrs, "llm.request.type", "chat");
            if let Some(model) = &r.model { string(&mut attrs, "gen_ai.request.model", model); string(&mut attrs, "gen_ai.response.model", model); }
            if let Some(provider) = &r.provider { string(&mut attrs, "gen_ai.system", provider.to_ascii_lowercase()); }
            int(&mut attrs, "gen_ai.usage.input_tokens", r.usage.input_tokens); int(&mut attrs, "gen_ai.usage.prompt_tokens", r.usage.input_tokens);
            int(&mut attrs, "gen_ai.usage.output_tokens", r.usage.output_tokens); int(&mut attrs, "gen_ai.usage.completion_tokens", r.usage.output_tokens);
            int(&mut attrs, "llm.usage.total_tokens", r.usage.input_tokens.zip(r.usage.output_tokens).map(|(a,b)| a.saturating_add(b)));
            int(&mut attrs, "llm.usage.cache_read_input_tokens", r.usage.cache_read_input_tokens);
            int(&mut attrs, "llm.usage.cache_creation_input_tokens", r.usage.cache_creation_input_tokens);
            // Ingest prices cache only from these semconv keys; `llm.usage.*` alone left cache unpriced (verified live 2026-10-11).
            int(&mut attrs, "gen_ai.usage.cache_read_input_tokens", r.usage.cache_read_input_tokens);
            int(&mut attrs, "gen_ai.usage.cache_creation_input_tokens", r.usage.cache_creation_input_tokens);
            int(&mut attrs, "llm.usage.reasoning_tokens", r.usage.reasoning_tokens);
            // Ingest maps this key to `time_to_first_token` in seconds; `gen_ai.server.time_to_first_token` is not mapped (verified live 2026-10-11).
            if let Some(ms) = r.metadata.get("ttft_ms").and_then(Value::as_f64) { double(&mut attrs, "llm.openai.chat_completions.streaming_time_to_first_token", ms / 1000.0); }
            if let Some(input) = &r.input { messages(&mut attrs, "gen_ai.prompt", input, "user"); }
            if let Some(output) = &r.output { messages(&mut attrs, "gen_ai.completion", output, "assistant"); }
        } else {
            if let Some(input) = &r.input { string(&mut attrs, "traceloop.entity.input", input.to_string()); }
            if let Some(output) = &r.output { string(&mut attrs, "traceloop.entity.output", output.to_string()); }
            if r.log_type == "tool" { if let Some(id) = r.metadata.get("tool_call_id").and_then(Value::as_str) { string(&mut attrs, "gen_ai.tool.call.id", id); } }
        }
        let mut span = json!({"traceId":trace_id(r.trace_key.as_deref().unwrap_or(&r.span_key)),"spanId":span_id(&r.span_key),
            "name":r.name,"kind":if r.log_type == "chat" {3} else {1},"startTimeUnixNano":r.start_ns.to_string(),"endTimeUnixNano":r.end_ns.to_string(),
            "attributes":attrs,"status":{"code":if r.status == "error" {2} else {1}}});
        if let Some(parent) = &r.parent_key { span["parentSpanId"] = json!(span_id(parent)); }
        if let Some(message) = &r.status_message { span["status"]["message"] = json!(message); }
        span
    }).collect();
    json!({"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"herdr-respan"}}]},
        "scopeSpans":[{"scope":{"name":"herdr-respan","version":env!("CARGO_PKG_VERSION")},"spans":spans}]}]})
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn native_tool_calls_ttft_and_repo_reach_canonical_attributes() {
        let record = SpanRecord { span_key:"c".into(), log_type:"chat".into(), metadata:BTreeMap::from([("git.repo".into(),json!("hra")),("ttft_ms".into(),json!(350))]),
            output:Some(json!([{"role":"assistant","content":[{"type":"text","text":"x"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},
                {"role":"model","parts":[{"functionCall":{"name":"read","args":{"p":1}}}]},
                {"type":"function_call","call_id":"c3","name":"shell","arguments":"{\"cmd\":\"pwd\"}"},
                {"role":"assistant","content":"plain"}])), ..Default::default() };
        let spans = payload(&[record]);
        let attrs: BTreeMap<String,Value> = spans["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"].as_array().unwrap().iter()
            .map(|a| (a["key"].as_str().unwrap().to_string(), a["value"].as_object().unwrap().values().next().unwrap().clone())).collect();
        let calls = |n: usize| serde_json::from_str::<Value>(attrs[&format!("gen_ai.completion.{n}.tool_calls")].as_str().unwrap()).unwrap();
        assert_eq!(calls(0), json!([{"id":"t1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]));
        assert_eq!(calls(1), json!([{"type":"function","function":{"name":"read","arguments":"{\"p\":1}"}}]));
        assert_eq!(calls(2), json!([{"id":"c3","type":"function","function":{"name":"shell","arguments":"{\"cmd\":\"pwd\"}"}}]));
        assert!(!attrs.contains_key("gen_ai.completion.3.tool_calls"));
        assert_eq!(attrs["llm.openai.chat_completions.streaming_time_to_first_token"], json!(0.35));
        assert_eq!(attrs["respan.span_params.custom_identifier"], json!("hra"));
    }
    #[test] fn ids_and_proxy_parenting() {
        assert_eq!(trace_id("abc"), "0001786200188e62daa66d130019f600");
        assert_eq!(span_id("abc"), "0001786200001602");
        let mut records = vec![SpanRecord { trace_key: Some("turn".into()), span_key:"turn".into(), run_id:Some("r".into()),start_ns:10,end_ns:50,..Default::default() },
            SpanRecord { span_key:"request".into(),run_id:Some("r".into()),start_ns:20,end_ns:40,source:"proxy".into(),..Default::default() }];
        reparent(&mut records); assert_eq!(records[1].parent_key.as_deref(),Some("turn")); assert_eq!(records[1].trace_key.as_deref(),Some("turn"));
        records[1].run_id=Some("other".into()); records[1].parent_key=None; reparent(&mut records); assert!(records[1].parent_key.is_none());
    }
    #[test] fn registered_unknown_turns_expire_and_run_ids_are_safe_leaf_names() {
        let tmp=crate::tests::Temp::new();crate::paths::atomic_json(&tmp.path.join("proxy/runs/run.json"),&serde_json::json!({})).unwrap();
        let mut records=vec![SpanRecord {span_key:"p".into(),run_id:Some("run".into()),source:"proxy".into(),start_ns:1,trace_key:Some("old".into()),..Default::default()}];
        assert_eq!(correlate_proxy(&tmp.path,&mut records,120_000_000_001).unwrap(),vec![true]);assert_eq!(correlate_proxy(&tmp.path,&mut records,120_000_000_002).unwrap(),vec![false]);assert_eq!(records[0].trace_key.as_deref(),Some("run"));assert!(records[0].parent_key.is_none());
        assert!(save_turn(&tmp.path,"../outside","trace",1,None).is_err());
    }
    #[test] fn abandoned_turn_files_gc_after_an_hour_of_observed_absence() {
        let tmp=crate::tests::Temp::new();save_turn(&tmp.path,"run","trace",1,Some(2)).unwrap();let path=tmp.path.join("proxy/turns/run.json");let gone=path.with_extension("gone");
        gc_turns(&tmp.path,1).unwrap();assert!(path.exists());assert!(gone.exists());gc_turns(&tmp.path,3_600_000_000_001).unwrap();assert!(path.exists());gc_turns(&tmp.path,3_600_000_000_002).unwrap();assert!(!path.exists());assert!(!gone.exists());
        save_turn(&tmp.path,"run","trace",1,None).unwrap();gc_turns(&tmp.path,10).unwrap();crate::paths::atomic_json(&tmp.path.join("proxy/runs/run.json"),&serde_json::json!({})).unwrap();gc_turns(&tmp.path,10_000_000_000_000).unwrap();assert!(path.exists());assert!(!gone.exists());
    }
}
