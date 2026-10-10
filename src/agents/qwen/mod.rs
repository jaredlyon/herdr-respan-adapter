//! Qwen's Claude-style hooks, not Gemini's model-chunk vocabulary.
mod setup;
#[cfg(test)] mod tests;

use crate::{agents::{AgentSpec, common::{bridge::{self, Batch, Event}, jsonl::{self, Cursor}}}, record::{HookCtx, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec {kind:"qwen",display:"Qwen Code",hook,plan:setup::plan,reply:None,settle:Some(settle)};

#[derive(Default, Serialize, Deserialize)]
struct Session {turn: String}
#[derive(Default, Serialize, Deserialize)]
struct ToolStart {ts: u64, turn: String}
#[derive(Default, Serialize, Deserialize)]
struct Child {turn: String, ts: u64, name: Option<String>}
#[derive(Default, Serialize, Deserialize)]
struct Transcript {cursor: Cursor, turn: String, input: Vec<Value>, last_ns: u64}

fn key(prefix: &str, identity: &str) -> String {
    format!("{prefix}-{:x}.json", Sha256::digest(identity.as_bytes()))
}
fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name)?.as_str().filter(|s| !s.is_empty())
}
fn value(value: &Value, name: &str) -> Option<Value> {
    value.get(name).filter(|v| !v.is_null()).cloned()
}
fn hook(ctx: &mut HookCtx, stdin: &[u8]) -> Result<()> {
    if std::env::var("HERDR_RESPAN_QWEN_HOOK").ok().as_deref() != Some("1") {return Ok(());}
    let payload: Value = serde_json::from_slice(stdin)?;
    process(ctx, &payload)
}

fn process(ctx: &mut HookCtx, p: &Value) -> Result<()> {
    let (Some(_session), Some(event), Some(path), Some(_cwd)) = (text(p,"session_id"),text(p,"hook_event_name"),text(p,"transcript_path"),text(p,"cwd")) else {return Ok(());};
    if text(p,"timestamp").and_then(timestamp_ns).is_none() {return Ok(());}
    if !setup::EVENTS.contains(&event) || p.get("llm_response").is_some() || p.get("conversation_id").is_some()
        || p.get("permission_mode").is_some_and(|v| !["default","plan","auto_edit","auto","yolo"].contains(&v.as_str().unwrap_or(""))) {return Ok(());}
    let baseline = if event == "SessionStart" && text(p,"agent_id").is_none() {Some(jsonl::at_end(Path::new(path))?)} else {None};
    let mut payload = p.clone();
    // Retain only fields consumed by this parser; unused native content aliases never reach disk.
    payload.as_object_mut().expect("validated payload").retain(|name, _| [
        "session_id", "hook_event_name", "transcript_path", "cwd", "timestamp", "permission_mode",
        "prompt_id", "agent_id", "agent_type", "agent_transcript_path", "tool_use_id", "tool_name",
        "duration_ms", "prompt", "tool_input", "tool_response", "last_assistant_message", "error", "error_details"
    ].contains(&name.as_str()));
    for name in ["prompt", "tool_input", "tool_response", "last_assistant_message", "error", "error_details"] {
        if let Some(field) = payload.get_mut(name) {
            let mut content = Some(std::mem::take(field));
            ctx.scrub_content(&mut content)?;
            *field = content.unwrap_or(Value::Null);
        }
    }
    let name = format!("queue-{:020}-{}.json", ctx.now_ns(), crate::record::random_id()?);
    ctx.save(&name, &Queued {payload, baseline, metadata:ctx.metadata.clone(), run_id:ctx.run_id.clone(), proxy_active:ctx.proxy_active})?;
    ctx.mark_settle_pending()?;
    let Some(_lock) = ctx.try_lock("qwen-native")? else {return Ok(());};
    drain(ctx)
}

#[derive(Default, Serialize, Deserialize)]
struct Queued {
    payload: Value, baseline: Option<Cursor>, metadata: BTreeMap<String,Value>,
    run_id: Option<String>, proxy_active: bool,
}
fn queued(ctx: &HookCtx) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(ctx.state_dir())? {
        let path = entry?.path();
        if path.file_name().is_some_and(|n|n.to_string_lossy().starts_with("queue-")) && path.extension().is_some_and(|e|e=="json") {files.push(path);}
    }
    files.sort();
    Ok(files)
}
// Caller holds the native lock; each file is removed only after state and bridge acceptance.
fn drain(ctx: &mut HookCtx) -> Result<()> {
    for file in queued(ctx)? {
        let queued: Queued = ctx.load(file.file_name().and_then(|n|n.to_str()).ok_or("invalid queue filename")?)?;
        let metadata = std::mem::replace(&mut ctx.metadata, queued.metadata);
        let run_id = std::mem::replace(&mut ctx.run_id, queued.run_id);
        let proxy = std::mem::replace(&mut ctx.proxy_active, queued.proxy_active);
        let result = process_locked(ctx, &queued.payload, queued.baseline);
        ctx.metadata=metadata;ctx.run_id=run_id;ctx.proxy_active=proxy;
        result?;
        std::fs::remove_file(file)?;
    }
    Ok(())
}
fn process_locked(ctx: &mut HookCtx, p: &Value, baseline: Option<Cursor>) -> Result<()> {
    let (Some(session), Some(event), Some(path), Some(cwd)) = (text(p,"session_id"),text(p,"hook_event_name"),text(p,"transcript_path"),text(p,"cwd")) else {return Ok(());};
    let Some(ts) = text(p,"timestamp").and_then(timestamp_ns) else {return Ok(());};
    let state_key = key("session",session);
    let mut state: Session = ctx.load(&state_key)?;
    let child_id = text(p,"agent_id");
    let child: Child = if let Some(id) = child_id {ctx.load(&key("child",&format!("{session}:{id}")))?} else {Child::default()};
    let mut turn = if !child.turn.is_empty() {child.turn.clone()} else {
        text(p,"prompt_id").map(str::to_owned)
            .unwrap_or_else(|| state.turn.clone())
    };
    let mut batch = Batch {v:1,agent:"qwen".into(),session_id:session.into(),session_path:Some(path.into()),cwd:Some(cwd.into()),agent_version:None,events:Vec::new()};
    if event == "SessionStart" && child_id.is_none() {
        // A resumed session must not replay its historical requests as new usage.
        prime(ctx, Path::new(path), baseline)?;
        return Ok(());
    }
    if event == "UserPromptSubmit" && child_id.is_none() {
        // Drain the previous turn before changing fallback correlation; a lagging
        // transcript's promptId remains authoritative when it is available.
        transcript(ctx, Path::new(path), session, &state.turn, None, &mut batch)?;
        state.turn = text(p,"prompt_id").map(str::to_owned).unwrap_or_else(|| format!("prompt-{ts}"));
        batch.events.push(Event::TurnStart {turn_id:state.turn.clone(),ts_ns:ts,prompt:value(p,"prompt"),metadata:BTreeMap::new()});
        bridge::ingest(ctx,batch)?;
        ctx.save(&state_key,&state)?;
        return Ok(());
    }
    if turn.is_empty() {return Ok(());}
    if event == "SubagentStart" {
        if let Some(id) = child_id {
            ctx.save(&key("child",&format!("{session}:{id}")),&Child {turn,ts,name:text(p,"agent_type").map(str::to_owned)})?;
        }
        return Ok(());
    }
    if event == "SubagentStop" {
        if let Some(id) = child_id {
            let child_path = text(p,"agent_transcript_path").map(Path::new);
            let parent = child_path.and_then(|path| parent_tool(path,session,id));
            if let Some(tool_id) = &parent {
                let spawning: ToolStart = ctx.load(&key("tool",&format!("{session}:{tool_id}")))?;
                if !spawning.turn.is_empty() {turn=spawning.turn;}
            }
            batch.events.push(Event::SubagentStart {turn_id:turn.clone(),id:id.into(),parent_id:parent,ts_ns:if child.ts == 0 {ts} else {child.ts},name:child.name.or_else(||text(p,"agent_type").map(str::to_owned)),metadata:BTreeMap::new()});
            if let Some(path) = child_path {transcript(ctx,path,session,&turn,Some(id),&mut batch)?; remember_tail(ctx,path,session,&turn,Some(id),cwd)?;}
            batch.events.push(Event::SubagentEnd {turn_id:turn,id:id.into(),ts_ns:ts,status:Some("ok".into()),output:value(p,"last_assistant_message")});
        }
    } else {
        transcript(ctx,Path::new(path),session,&turn,child_id,&mut batch)?;
        if matches!(event,"Stop"|"StopFailure"|"SessionEnd") {remember_tail(ctx,Path::new(path),session,&turn,child_id,cwd)?;}
        match event {
            "PreToolUse" => {
                if let Some(id) = text(p,"tool_use_id") {ctx.save(&key("tool",&format!("{session}:{id}")),&ToolStart {ts,turn:turn.clone()})?;}
            }
            "PostToolUse" | "PostToolUseFailure" => {
                if let Some(id) = text(p,"tool_use_id") {
                    let start: ToolStart = ctx.load(&key("tool",&format!("{session}:{id}")))?;
                    let duration = p.get("duration_ms").and_then(Value::as_f64).filter(|d| d.is_finite() && *d >= 0.0).map(|d| (d * 1_000_000.0).min(u64::MAX as f64) as u64);
                    let start_ns = duration.map(|d| ts.saturating_sub(d)).unwrap_or(if start.ts == 0 {ts} else {start.ts.min(ts)});
                    batch.events.push(Event::Tool {turn_id:turn,id:id.into(),parent_id:child_id.map(str::to_owned),name:text(p,"tool_name").map(str::to_owned),start_ns,end_ns:ts,input:value(p,"tool_input"),output:value(p,"tool_response"),status:Some(if event == "PostToolUseFailure" {"error"} else {"ok"}.into()),error:text(p,"error").map(str::to_owned),metadata:BTreeMap::new()});
                }
            }
            "Stop" | "StopFailure" | "SessionEnd" if child_id.is_none() => {
                batch.events.push(Event::TurnEnd {turn_id:turn,ts_ns:ts,output:value(p,"last_assistant_message"),status:Some(if event == "Stop" {"ok"} else {"error"}.into()),error:if event == "SessionEnd" {Some("session ended before Stop".into())} else {text(p,"error_details").or_else(||text(p,"error")).map(str::to_owned)}});
            }
            _ => {}
        }
    }
    bridge::ingest(ctx,batch)
}

fn prime(ctx: &HookCtx, path: &Path, baseline: Option<Cursor>) -> Result<()> {
    let state = Transcript {cursor:match baseline {Some(cursor)=>cursor,None=>jsonl::at_end(path)?},..Default::default()};
    ctx.save(&key("transcript",&path.to_string_lossy()),&state)
}
fn transcript(ctx: &mut HookCtx, path: &Path, session: &str, fallback_turn: &str, parent: Option<&str>, batch: &mut Batch) -> Result<()> {
    let state_key = key("transcript",&path.to_string_lossy());
    let mut state: Transcript = ctx.load(&state_key)?;
    let lines = match jsonl::read_new(path,&mut state.cursor) {
        Ok(lines) => lines,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind()==std::io::ErrorKind::NotFound) => return Ok(()),
        Err(e) => return Err(e),
    };
    for line in lines {
        let Ok(r) = serde_json::from_str::<Value>(&line) else {continue;};
        if text(&r,"sessionId") != Some(session) {continue;}
        if parent.is_some() && text(&r,"agentId") != parent {continue;}
        if parent.is_none() && text(&r,"agentId").is_some() {continue;}
        let Some(ts) = text(&r,"timestamp").and_then(timestamp_ns) else {continue;};
        if parent.is_none() {
            if let Some(id) = text(&r,"promptId") {state.turn=id.into();}
            else if text(&r,"type")==Some("user") && text(&r,"subtype").is_none() {
                state.turn=if !fallback_turn.is_empty() {fallback_turn.into()} else {text(&r,"uuid").unwrap_or("unattributed").into()};
            }
        } else {state.turn=fallback_turn.into();}
        let turn = if state.turn.is_empty() {fallback_turn} else {&state.turn};
        match text(&r,"type") {
            Some("user"|"tool_result") => {
                if text(&r,"type")==Some("user") {state.input.clear();}
                if let Some(message) = value(&r,"message") {state.input.push(message);}
                state.last_ns=ts;
            }
            Some("assistant") if !turn.is_empty() && !ctx.proxy_active => {
                let Some(id) = text(&r,"uuid") else {continue;};
                let usage = r.get("usageMetadata").map(|u| Usage {
                    input_tokens:u.get("promptTokenCount").and_then(Value::as_u64),
                    output_tokens:u.get("candidatesTokenCount").and_then(Value::as_u64),
                    cache_read_input_tokens:u.get("cachedContentTokenCount").and_then(Value::as_u64),
                    reasoning_tokens:u.get("thoughtsTokenCount").and_then(Value::as_u64),
                    ..Default::default()
                });
                let mut metadata = BTreeMap::new();
                if usage.as_ref().is_some_and(|u|u.reasoning_tokens.is_some()) {
                    metadata.insert("qwen.reasoning_usage_source".into(),json!("provider_or_text_estimate"));
                }
                if batch.agent_version.is_none() {batch.agent_version=text(&r,"version").map(str::to_owned);}
                batch.events.push(Event::Chat {turn_id:turn.into(),id:id.into(),parent_id:parent.map(str::to_owned),start_ns:if state.last_ns==0 {ts} else {state.last_ns.min(ts)},end_ns:ts,model:text(&r,"model").map(str::to_owned),provider:None,input:if state.input.is_empty() {None} else {Some(json!(state.input))},output:value(&r,"message"),usage,cost_usd:None,aux_task:None,status:Some("ok".into()),error:None,metadata});
                state.input.clear();state.last_ns=ts;
            }
            _ => {}
        }
    }
    // Persist only after the scrubbed assembler accepted the events; replay after
    // a failed save is harmless because bridge owns idempotency.
    if !batch.events.is_empty() {
        let events = std::mem::take(&mut batch.events);
        bridge::ingest(ctx,Batch {events,..batch.clone()})?;
    }
    let mut input = Some(Value::Array(std::mem::take(&mut state.input)));
    ctx.scrub_content(&mut input)?;
    state.input = match input {Some(Value::Array(values))=>values,Some(value)=>vec![value],None=>Vec::new()};
    ctx.save(&state_key,&state)
}

fn parent_tool(path: &Path, session: &str, child: &str) -> Option<String> {
    let meta: Value = serde_json::from_slice(&std::fs::read(path.with_extension("meta.json")).ok()?).ok()?;
    if text(&meta,"agentId") != Some(child) || text(&meta,"parentSessionId") != Some(session) {return None;}
    text(&meta,"toolUseId").map(str::to_owned)
}

// Qwen writes JavaScript Date.toISOString(): UTC with optional fractional seconds.
fn timestamp_ns(s: &str) -> Option<u64> {
    if !s.is_ascii() || s.len()<20 || &s[4..5]!="-" || &s[7..8]!="-" || &s[10..11]!="T" || &s[13..14]!=":" || &s[16..17]!=":" || !s.ends_with('Z') {return None;}
    let n = |a,b| s.get(a..b)?.parse::<u64>().ok();
    let (year,month,day,hour,minute,second)=(n(0,4)?,n(5,7)?,n(8,10)?,n(11,13)?,n(14,16)?,n(17,19)?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || hour>23 || minute>59 || second>59 {return None;}
    let leap = |y:u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let months=[31,if leap(year) {29} else {28},31,30,31,30,31,31,30,31,30,31];
    if day==0 || day>months[month as usize-1] {return None;}
    let leaps = |y:u64| y/4-y/100+y/400;
    let days=(year-1970)*365+leaps(year-1)-leaps(1969)+months[..month as usize-1].iter().sum::<u64>()+day-1;
    let suffix=&s[19..s.len()-1];
    let fraction=if suffix.is_empty() {0} else {
        let digits=suffix.strip_prefix('.')?;
        if digits.is_empty() || digits.len()>9 || !digits.bytes().all(|b|b.is_ascii_digit()) {return None;}
        digits.parse::<u64>().ok()? * 10u64.pow(9-digits.len() as u32)
    };
    (days*86400+hour*3600+minute*60+second).checked_mul(1_000_000_000)?.checked_add(fraction)
}

#[derive(Default, Serialize, Deserialize)]
struct Tail {
    path: String, session: String, turn: String, parent: Option<String>, cwd: String,
    until_ns: u64, metadata: BTreeMap<String,Value>, run_id: Option<String>, proxy_active: bool,
}
fn remember_tail(ctx: &HookCtx, path: &Path, session: &str, turn: &str, parent: Option<&str>, cwd: &str) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    ctx.save(&key("tail",&path),&Tail {path,session:session.into(),turn:turn.into(),parent:parent.map(str::to_owned),cwd:cwd.into(),until_ns:ctx.now_ns().saturating_add(6_000_000_000),metadata:ctx.metadata.clone(),run_id:ctx.run_id.clone(),proxy_active:ctx.proxy_active})?;
    ctx.mark_settle_pending()
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let _lock = ctx.lock("qwen-native")?;
    drain(ctx)?;
    let mut pending = false;
    for entry in std::fs::read_dir(ctx.state_dir())? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|s|s.starts_with("tail-") && s.ends_with(".json")) else {continue;};
        let tail: Tail = ctx.load(name)?;
        let mut batch = Batch {v:1,agent:"qwen".into(),session_id:tail.session.clone(),session_path:Some(tail.path.clone()),cwd:Some(tail.cwd),agent_version:None,events:Vec::new()};
        let old_metadata = std::mem::replace(&mut ctx.metadata,tail.metadata);
        let old_run = std::mem::replace(&mut ctx.run_id,tail.run_id);
        let old_proxy = std::mem::replace(&mut ctx.proxy_active,tail.proxy_active);
        let result = transcript(ctx,Path::new(&tail.path),&tail.session,&tail.turn,tail.parent.as_deref(),&mut batch);
        ctx.metadata=old_metadata;ctx.run_id=old_run;ctx.proxy_active=old_proxy;
        result?;
        if ctx.now_ns() >= tail.until_ns {std::fs::remove_file(entry.path())?;} else {pending=true;}
    }
    Ok(pending || !queued(ctx)?.is_empty())
}
