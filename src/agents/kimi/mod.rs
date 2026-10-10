//! MoonshotAI kimi-cli native hooks and incremental wire records.
mod setup;
#[cfg(test)] mod tests;
use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl::{self, Cursor}}, AgentSpec}, record::{HookCtx, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "kimi", display: "Kimi Code CLI", hook, plan: setup::plan, reply: None, settle: Some(settle),
};

#[derive(Default, Serialize, Deserialize)]
struct State { sessions: BTreeMap<String, Session> }
#[derive(Default, Serialize, Deserialize)]
struct Session {
    metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool,
    cwd: String, share: Option<PathBuf>, wire: Option<PathBuf>, cursor: Cursor, baseline_ns: u64, backlog: Vec<Value>,
    turn: Option<String>, wire_turns: BTreeMap<u64, String>, awaiting_begin: bool, closed: bool, ended: bool, wire_end: u64,
    steps: BTreeMap<String, Step>, tools: BTreeMap<String, Tool>,
    closing: Option<Closing>, done: BTreeMap<String, Pending>, outputs: BTreeMap<String, Value>, interrupted: BTreeSet<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Step { start: u64, output: Vec<Value>, output_bytes: usize, last_tool: Option<String> }
#[derive(Default, Serialize, Deserialize)]
struct Closing { ts: u64, error: Option<String> }
#[derive(Clone, Default, Serialize, Deserialize)]
struct Tool { identified: bool, parent: Option<String>, start: u64, name: Option<String>, input: Option<Value> }
#[derive(Default, Serialize, Deserialize)]
struct Pending { payload: Value, ts: u64, share: Option<PathBuf>, metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool }

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> { v.get(key)?.as_str().filter(|s| !s.is_empty()) }
fn valid(payload: &Value) -> bool {
    if text(payload, "session_id").is_none() || text(payload, "cwd").is_none()
        || payload.get("transcript_path").is_some() || payload.get("conversation_id").is_some()
        || payload.get("hookEventName").is_some() || payload.get("timestamp").is_some()
        || payload.get("transcriptPath").is_some() || payload.get("sessionId").is_some() { return false; }
    match text(payload, "hook_event_name") {
        Some("SessionStart") => text(payload, "source").is_some(),
        Some("SessionEnd") => text(payload, "reason").is_some(),
        Some("UserPromptSubmit") => payload.get("prompt").is_some_and(Value::is_string),
        Some("PreToolUse" | "PostToolUse" | "PostToolUseFailure") =>
            text(payload, "tool_name").is_some() && payload.get("tool_input").is_some_and(Value::is_object)
                && payload.get("tool_call_id").is_some_and(Value::is_string),
        Some("Stop") => payload.get("stop_hook_active").is_some_and(Value::is_boolean),
        Some("StopFailure") => text(payload, "error_type").is_some() && payload.get("error_message").is_some_and(Value::is_string),
        Some("SubagentStart") => text(payload, "agent_name").is_some() && payload.get("prompt").is_some_and(Value::is_string),
        Some("SubagentStop") => text(payload, "agent_name").is_some() && payload.get("response").is_some_and(Value::is_string),
        _ => false,
    }
}
fn hook(ctx: &mut HookCtx, bytes: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(bytes)?;
    if !valid(&payload) { return Ok(()); }
    let ts = ctx.now_ns();
    ctx.save(&format!("native-{ts}-{}.json", crate::record::random_id()?), &Pending { payload, ts, share: Some(crate::paths::absolute(&share_dir()?)?), metadata: ctx.metadata.clone(), run_id: ctx.run_id.clone(), proxy_active: ctx.proxy_active })?;
    Ok(())
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let _lock = ctx.lock("native")?;
    let mut state: State = ctx.load("native.json")?;
    let mut queued = std::fs::read_dir(ctx.state_dir())?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("native-")) && p.extension().is_some_and(|e| e == "json"))
        .collect::<Vec<_>>();
    queued.sort();
    for file in queued {
        let pending: Pending = serde_json::from_slice(&std::fs::read(&file)?)?;
        let id = text(&pending.payload, "session_id").ok_or("missing Kimi session id")?;
        let session = state.sessions.entry(id.into()).or_default();
        session.share = pending.share; session.metadata = pending.metadata; session.run_id = pending.run_id; session.proxy_active = pending.proxy_active;
        session.cwd = text(&pending.payload, "cwd").unwrap_or_default().into();
        if session.baseline_ns == 0 { session.baseline_ns = pending.ts; }
        if session.wire.is_none() { session.wire = locate(id, session.share.as_deref())?; }
        let mut events = vec![];
        tail_until(ctx, session, &mut events, pending.ts)?;
        native(session, &pending.payload, pending.ts, &mut events);
        complete_tools(session, &mut events);
        ingest(ctx, id, session, events)?;
        ctx.save("native.json", &state)?;
        std::fs::remove_file(file)?;
    }
    let mut retry = false;
    for (id, session) in &mut state.sessions {
        let mut events = vec![];
        if session.wire.is_none() { session.wire = locate(id, session.share.as_deref())?; }
        tail(ctx, session, &mut events)?;
        complete_tools(session, &mut events);
        if session.closed && !session.ended {
            session.closing.get_or_insert(Closing { ts: session.wire_end, error: None });
            close(session, session.wire_end, &mut events);
        }
        retry |= session.turn.is_some() && (session.awaiting_begin || !session.closed || !session.steps.is_empty() || !session.done.is_empty());
        ingest(ctx, id, session, events)?;
    }
    ctx.save("native.json", &state)?;
    Ok(retry)
}
fn ingest(ctx: &mut HookCtx, id: &str, session: &Session, events: Vec<Event>) -> Result<()> {
    if events.is_empty() { return Ok(()); }
    let metadata = std::mem::replace(&mut ctx.metadata, session.metadata.clone());
    let run_id = std::mem::replace(&mut ctx.run_id, session.run_id.clone());
    let proxy_active = std::mem::replace(&mut ctx.proxy_active, session.proxy_active);
    let result = bridge::ingest(ctx, Batch { v: 1, agent: "kimi".into(), session_id: id.into(),
        session_path: session.wire.as_ref().map(|p| p.to_string_lossy().into()),
        cwd: Some(session.cwd.clone()), agent_version: None, events });
    ctx.metadata = metadata; ctx.run_id = run_id; ctx.proxy_active = proxy_active;
    result
}
fn native(s: &mut Session, p: &Value, now: u64, events: &mut Vec<Event>) {
    let event = text(p, "hook_event_name").unwrap_or_default();
    if event == "UserPromptSubmit" {
        let turn_id = now.to_string();
        events.push(Event::TurnStart { turn_id: turn_id.clone(), ts_ns: now, prompt: p.get("prompt").cloned(), metadata: BTreeMap::new() });
        s.tools.clear(); s.done.clear(); s.steps.clear(); s.outputs.clear(); s.interrupted.clear();
        s.turn = Some(turn_id); s.awaiting_begin = true; s.closed = false; s.ended = false; s.closing = None;
        return;
    }
    if s.turn.is_none() { return; }
    match event {
        "PreToolUse" => { if let Some(id) = text(p, "tool_call_id") {
            let tool = s.tools.entry(id.into()).or_default();
            tool.start = now; tool.name = text(p, "tool_name").map(str::to_string); tool.input = p.get("tool_input").cloned();
        } }
        "PostToolUse" | "PostToolUseFailure" => { if let Some(id) = text(p, "tool_call_id") {
            s.done.insert(id.into(), Pending { payload: p.clone(), ts: now, ..Default::default() });
        } }
        "Stop" | "StopFailure" | "SessionEnd" => {
            s.closing = Some(Closing { ts: now, error: if event == "StopFailure" {
                text(p, "error_message").map(str::to_string)
            } else { None } });
            if s.closed || s.wire.is_none() { close(s, now, events); }
        }
        _ => {}, // Hook subagent names cannot identify concurrent instances; wire IDs can.
    }
}

fn close(s: &mut Session, ts: u64, events: &mut Vec<Event>) {
    if s.ended { s.closing = None; return; }
    if let (Some(turn_id), Some(mut closing)) = (&s.turn, s.closing.take()) {
        if s.interrupted.remove("") && closing.error.is_none() { closing.error = Some("step interrupted".into()); }
        events.push(Event::TurnEnd { turn_id: turn_id.clone(), ts_ns: ts.max(closing.ts), output: s.outputs.remove(""),
            status: Some(if closing.error.is_some() { "error" } else { "ok" }.into()), error: closing.error });
        s.ended = true; s.closed = true; s.awaiting_begin = false;
    }
}
fn complete_tools(s: &mut Session, events: &mut Vec<Event>) {
    let Some(turn_id) = &s.turn else { return; };
    s.done.retain(|id, pending| {
        if s.wire.is_some() && !s.tools.get(id).is_some_and(|tool| tool.identified) { return true; }
        let tool = s.tools.remove(id).unwrap_or(Tool { start: pending.ts, ..Default::default() });
        let p = &pending.payload;
        let failure = text(p, "hook_event_name") == Some("PostToolUseFailure");
        events.push(Event::Tool { turn_id: turn_id.clone(), id: id.clone(), parent_id: tool.parent,
            name: text(p, "tool_name").map(str::to_string), start_ns: tool.start.min(pending.ts), end_ns: pending.ts,
            input: p.get("tool_input").cloned(), output: p.get("tool_output").cloned(),
            status: Some(if failure { "error" } else { "ok" }.into()), error: text(p, "error").map(str::to_string), metadata: BTreeMap::new() });
        false
    });
}

fn share_dir() -> Result<PathBuf> {
    std::env::var_os("KIMI_SHARE_DIR").map(PathBuf::from).map(Ok)
        .unwrap_or_else(|| crate::paths::home().map(|p| p.join(".kimi")))
}
fn locate(session_id: &str, share: Option<&Path>) -> Result<Option<PathBuf>> {
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { return Ok(None); }
    let root = match share { Some(path) => path.to_path_buf(), None => share_dir()? };
    let dirs = match std::fs::read_dir(root.join("sessions")) {
        Ok(d) => d, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(e) => return Err(e.into()),
    };
    let mut found = None;
    // ponytail: one directory level avoids an MD5 dependency; cache the unique session path.
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir() { continue; }
        let path = dir.path().join(session_id).join("wire.jsonl");
        if path.is_file() { if found.is_some() { return Err("ambiguous Kimi session path".into()); } found = Some(path); }
    }
    Ok(found)
}
fn tail(ctx: &HookCtx, s: &mut Session, events: &mut Vec<Event>) -> Result<()> {
    tail_until(ctx, s, events, u64::MAX)
}
fn tail_until(ctx: &HookCtx, s: &mut Session, events: &mut Vec<Event>, until: u64) -> Result<()> {
    if let Some(path) = &s.wire {
        let lines = match jsonl::read_new(path, &mut s.cursor) {
            Ok(lines) => lines,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => vec![],
            Err(e) => return Err(e),
        };
        for line in lines {
            let record: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
            if record.get("message").is_some() { s.backlog.push(record); }
        }
    }
    let backlog = std::mem::take(&mut s.backlog);
    for record in backlog {
        let Some(ts) = record.get("timestamp").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0) else { continue; };
        let ts_ns = (ts * 1e9) as u64;
        if ts_ns < s.baseline_ns { continue; }
        if ts_ns > until { s.backlog.push(record); continue; }
        wire(s, &record["message"], ts_ns, None, ctx.config.content_cap_bytes, events, 0);
    }
    Ok(())
}
fn wire(s: &mut Session, message: &Value, ts: u64, parent: Option<&str>, cap: usize, events: &mut Vec<Event>, depth: usize) {
    if depth > 32 { return; }
    let kind = text(message, "type").unwrap_or_default();
    let p = &message["payload"];
    if kind == "SubagentEvent" {
        let Some(id) = text(p, "agent_id") else { return; };
        let nested = &p["event"];
        if text(nested, "type") == Some("TurnBegin") {
            if let Some(turn_id) = &s.turn {
                events.push(Event::SubagentStart { turn_id: turn_id.clone(), id: id.into(),
                    parent_id: text(p, "parent_tool_call_id").or_else(|| text(p, "task_tool_call_id")).map(str::to_string),
                    ts_ns: ts, name: text(p, "subagent_type").map(str::to_string), metadata: BTreeMap::new() });
            }
        }
        wire(s, nested, ts, Some(id), cap, events, depth + 1);
        return;
    }
    let key = parent.unwrap_or("").to_string();
    if kind == "TurnBegin" && parent.is_none() {
        if let Some(turn_id) = s.wire_turns.get(&ts) {
            s.turn = Some(turn_id.clone()); s.awaiting_begin = false;
        } else {
            if s.awaiting_begin { s.awaiting_begin = false; }
            else {
                let turn_id = format!("wire-{ts}");
                events.push(Event::TurnStart { turn_id: turn_id.clone(), ts_ns: ts, prompt: p.get("user_input").cloned(), metadata: BTreeMap::new() });
                s.turn = Some(turn_id);
            }
            if let Some(turn_id) = &s.turn { s.wire_turns.insert(ts, turn_id.clone()); }
            if s.wire_turns.len() > 1024 { if let Some(first) = s.wire_turns.keys().next().copied() { s.wire_turns.remove(&first); } }
        }
        s.closed = false; s.ended = false;
    }
    let Some(turn_id) = s.turn.clone() else { return; };
    match kind {
        "StepBegin" => { s.interrupted.remove(&key); s.steps.insert(key, Step { start: ts, ..Default::default() }); }
        "StepInterrupted" => { s.interrupted.insert(key); }
        "StepRetry" => { s.steps.insert(key, Step { start: ts, ..Default::default() }); }
        "ContentPart" => {
            let step = s.steps.entry(key).or_default();
            if cap > step.output_bytes {
                let encoded = p.to_string();
                let remaining = cap - step.output_bytes;
                if encoded.len() <= remaining { step.output_bytes += encoded.len(); step.output.push(p.clone()); }
                else { step.output.push(Value::String(crate::redact::cap(&encoded, remaining))); step.output_bytes = cap; }
            }
        }
        "ToolCall" => { if let Some(id) = text(p, "id") {
            let function = &p["function"];
            s.tools.insert(id.into(), Tool { identified: true, parent: parent.map(str::to_string), start: ts,
                name: text(function, "name").map(str::to_string), input: function.get("arguments").cloned() });
            s.steps.entry(key).or_default().last_tool = Some(id.into());
        } }
        "ToolCallPart" => { if let Some(id) = s.steps.get(&key).and_then(|step| step.last_tool.as_ref()) {
            if let Some(tool) = s.tools.get_mut(id) {
                if let Some(part) = text(p, "arguments_part") {
                    let input = tool.input.get_or_insert_with(|| Value::String(String::new()));
                    if let Some(input) = input.as_str() { let mut joined = input.to_string(); joined.push_str(part); tool.input = Some(Value::String(crate::redact::cap(&joined, cap))); }
                }
            }
        } }
        "StatusUpdate" => { if let Some(usage) = p.get("token_usage").filter(|u| u.is_object()) {
            let step = s.steps.remove(&key).unwrap_or(Step { start: ts, ..Default::default() });
            if !step.output.is_empty() { s.outputs.insert(key.clone(), json!(step.output)); }
            let id = text(p, "message_id").map(str::to_string).unwrap_or_else(|| format!("step-{ts}"));
            let input_other = usage.get("input_other").and_then(Value::as_u64);
            let cache_read = usage.get("input_cache_read").and_then(Value::as_u64);
            let cache_creation = usage.get("input_cache_creation").and_then(Value::as_u64);
            let total = input_other.zip(cache_read).zip(cache_creation).and_then(|((a,b),c)| a.checked_add(b)?.checked_add(c));
            events.push(Event::Chat { turn_id, id, parent_id: parent.map(str::to_string), start_ns: step.start.min(ts), end_ns: ts,
                model: None, provider: None, input: None, output: (!step.output.is_empty()).then_some(json!([{"role":"assistant","content":step.output}])),
                usage: Some(Usage { input_tokens: total, output_tokens: usage.get("output").and_then(Value::as_u64),
                    cache_read_input_tokens: cache_read, cache_creation_input_tokens: cache_creation, reasoning_tokens: None }),
                metadata: BTreeMap::new(), cost_usd: None, aux_task: None, status: Some("ok".into()), error: None });
        } }
        "ToolResult" => { if let Some(id) = text(p, "tool_call_id") {
            // Root hooks are authoritative. Wire fills child calls that shell hooks cannot parent.
            if parent.is_some() {
                let tool = s.tools.get(id).cloned().unwrap_or(Tool { start: ts, parent: parent.map(str::to_string), ..Default::default() });
                let result = &p["return_value"];
                let failed = result.get("is_error") == Some(&Value::Bool(true));
                events.push(Event::Tool { turn_id, id: id.into(), parent_id: tool.parent, name: tool.name,
                    start_ns: tool.start.min(ts), end_ns: ts, input: tool.input, output: result.get("output").cloned(),
                    status: Some(if failed { "error" } else { "ok" }.into()), error: if failed { text(result, "message").map(str::to_string) } else { None }, metadata: BTreeMap::new() });
            }
        } }
        "TurnEnd" => {
            s.steps.remove(&key);
            if let Some(parent) = parent {
                let status = if s.interrupted.remove(&key) { "error" } else { "ok" };
                events.push(Event::SubagentEnd { turn_id, id: parent.into(), ts_ns: ts, status: Some(status.into()), output: s.outputs.remove(&key) });
            } else { s.closed = true; s.wire_end = ts; close(s, ts, events); }
        }
        _ => {},
    }
}
