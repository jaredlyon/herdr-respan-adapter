use crate::record::SpanRecord;
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
pub fn reparent(records: &mut [SpanRecord]) {
    for i in 0..records.len() {
        if records[i].source != "proxy" || records[i].run_id.is_none() { continue; }
        let parent = records.iter().filter(|r| r.source != "proxy" && ["agent", "workflow"].contains(&r.log_type.as_str())
            && r.run_id == records[i].run_id && r.start_ns <= records[i].start_ns && r.end_ns >= records[i].end_ns)
            .min_by_key(|r| r.end_ns - r.start_ns).map(|r| (r.span_key.clone(), r.trace_key.clone().or_else(|| Some(r.span_key.clone()))));
        if let Some((key, trace)) = parent { records[i].parent_key = Some(key); records[i].trace_key = trace; }
    }
}
fn string(attrs: &mut Vec<Value>, key: &str, value: impl Into<String>) { attrs.push(json!({"key":key,"value":{"stringValue":value.into()}})); }
fn int(attrs: &mut Vec<Value>, key: &str, value: Option<u64>) { if let Some(value) = value { attrs.push(json!({"key":key,"value":{"intValue":value.to_string()}})); } }
fn content(value: &Value) -> String { if let Value::String(s) = value { s.clone() } else { value.to_string() } }
fn messages(attrs: &mut Vec<Value>, prefix: &str, value: &Value, default_role: &str) {
    let values = value.as_array().map(Vec::as_slice).unwrap_or_else(|| std::slice::from_ref(value));
    for (n, message) in values.iter().enumerate() {
        string(attrs, &format!("{prefix}.{n}.role"), message.get("role").and_then(Value::as_str).unwrap_or(default_role));
        string(attrs, &format!("{prefix}.{n}.content"), content(message.get("content").unwrap_or(message)));
        if let Some(calls) = message.get("tool_calls") { string(attrs, &format!("{prefix}.{n}.tool_calls"), calls.to_string()); }
    }
}
pub fn payload(records: &[SpanRecord]) -> Value {
    let spans: Vec<Value> = records.iter().map(|r| {
        let mut attrs = Vec::new();
        string(&mut attrs, "respan.entity.log_type", &r.log_type);
        string(&mut attrs, "traceloop.entity.name", &r.name);
        if r.parent_key.is_none() { string(&mut attrs, "traceloop.entity.path", ""); }
        if let Some(thread) = &r.thread_id { string(&mut attrs, "respan.threads.thread_identifier", thread); }
        if let Some(customer) = r.metadata.get("customer_identifier").and_then(Value::as_str) { string(&mut attrs, "respan.customer_params.customer_identifier", customer); }
        // Canonical contract requires a JSON string, never respan.metadata.<key> aliases.
        let mut metadata: serde_json::Map<String, Value> = r.metadata.iter().filter(|(k, _)| k.as_str() != "customer_identifier").map(|(k, v)| (k.clone(), v.clone())).collect();
        metadata.insert("agent".into(), json!(r.agent)); metadata.insert("source".into(), json!(r.source));
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
            int(&mut attrs, "llm.usage.reasoning_tokens", r.usage.reasoning_tokens);
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
    #[test] fn ids_and_proxy_parenting() {
        assert_eq!(trace_id("abc"), "0001786200188e62daa66d130019f600");
        assert_eq!(span_id("abc"), "0001786200001602");
        let mut records = vec![SpanRecord { trace_key: Some("turn".into()), span_key:"turn".into(), run_id:Some("r".into()),start_ns:10,end_ns:50,..Default::default() },
            SpanRecord { span_key:"request".into(),run_id:Some("r".into()),start_ns:20,end_ns:40,source:"proxy".into(),..Default::default() }];
        reparent(&mut records); assert_eq!(records[1].parent_key.as_deref(),Some("turn")); assert_eq!(records[1].trace_key.as_deref(),Some("turn"));
        records[1].run_id=Some("other".into()); records[1].parent_key=None; reparent(&mut records); assert!(records[1].parent_key.is_none());
    }
}
