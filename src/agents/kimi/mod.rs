//! Kimi Code CLI 2.x hooks and durable wire records.
mod setup;
#[cfg(test)] mod tests;
use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl::{self, Cursor}}, AgentSpec}, record::{HookCtx, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "kimi", display: "Kimi Code CLI", hook, plan: setup::plan, reply: None, settle: Some(settle),
};
#[derive(Default, Serialize, Deserialize)]
struct State { sessions: BTreeMap<String, Session> }
#[derive(Default, Serialize, Deserialize)]
struct Session {
    metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool,
    cwd: String, home: Option<PathBuf>, wire: Option<PathBuf>, cursor: Cursor, baseline_ns: u64, backlog: Vec<Value>,
    turn: Option<String>, ended: bool, model: Option<String>, provider: Option<String>,
    steps: BTreeMap<String, Step>, tools: BTreeMap<String, Tool>, done: BTreeMap<String, Pending>, output: Option<Value>,
}
#[derive(Default, Serialize, Deserialize)]
struct Step { start: u64, output: Vec<Value>, output_bytes: usize }
#[derive(Default, Serialize, Deserialize)]
struct Tool { identified: bool, start: u64 }
#[derive(Default, Serialize, Deserialize)]
struct Pending { payload: Value, ts: u64, home: Option<PathBuf>, metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool }
fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> { v.get(key)?.as_str().filter(|s| !s.is_empty()) }
fn valid(p: &Value) -> bool {
    if text(p, "client_type") != Some("kimi_code_cli") || text(p, "session_id").is_none() || text(p, "cwd").is_none() { return false; }
    match text(p, "hook_event_name") {
        Some("SessionStart") => text(p, "source").is_some(),
        Some("SessionEnd") => text(p, "reason").is_some(),
        Some("UserPromptSubmit") => p.get("prompt").is_some_and(|v| v.is_string() || v.is_array()),
        Some("PreToolUse" | "PostToolUse" | "PostToolUseFailure") => text(p, "tool_name").is_some()
            && p.get("tool_input").is_some_and(Value::is_object) && text(p, "tool_call_id").is_some(),
        Some("Stop") => p.get("stop_hook_active").is_some_and(Value::is_boolean),
        Some("StopFailure") => text(p, "error_type").is_some() && p.get("error_message").is_some_and(Value::is_string),
        _ => false,
    }
}
fn hook(ctx: &mut HookCtx, bytes: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(bytes)?;
    if !valid(&payload) { return Ok(()); }
    let ts = ctx.now_ns();
    ctx.save(&format!("native-{ts}-{}.json", crate::record::random_id()?), &Pending { payload, ts,
        home: Some(crate::paths::absolute(&home_dir()?)?), metadata: ctx.metadata.clone(), run_id: ctx.run_id.clone(), proxy_active: ctx.proxy_active })?;
    ctx.mark_settle_pending()?;
    Ok(())
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let _lock = ctx.lock("native")?;
    let mut state: State = ctx.load("native.json")?;
    let mut queued = std::fs::read_dir(ctx.state_dir())?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("native-")) && p.extension().is_some_and(|e| e == "json")).collect::<Vec<_>>();
    queued.sort();
    for file in queued {
        let pending: Pending = serde_json::from_slice(&std::fs::read(&file)?)?;
        let id = text(&pending.payload, "session_id").ok_or("missing Kimi session id")?;
        let s = state.sessions.entry(id.into()).or_default();
        s.home = pending.home; s.metadata = pending.metadata; s.run_id = pending.run_id; s.proxy_active = pending.proxy_active;
        s.cwd = text(&pending.payload, "cwd").unwrap_or_default().into();
        if s.baseline_ns == 0 { s.baseline_ns = pending.ts; }
        if s.wire.is_none() { s.wire = locate(id, s.home.as_deref())?; }
        let mut events = vec![];
        tail_until(ctx, s, &mut events, pending.ts)?;
        native(s, &pending.payload, pending.ts, &mut events);
        complete_tools(s, &mut events);
        ingest(ctx, id, s, events)?;
        ctx.save("native.json", &state)?;
        std::fs::remove_file(file)?;
    }
    let mut retry = false;
    for (id, s) in &mut state.sessions {
        if s.wire.is_none() { s.wire = locate(id, s.home.as_deref())?; }
        let mut events = vec![];
        tail_until(ctx, s, &mut events, u64::MAX)?;
        complete_tools(s, &mut events);
        retry |= (s.turn.is_some() && !s.ended) || !s.steps.is_empty() || !s.done.is_empty();
        ingest(ctx, id, s, events)?;
    }
    ctx.save("native.json", &state)?;
    Ok(retry)
}
fn ingest(ctx: &mut HookCtx, id: &str, s: &Session, events: Vec<Event>) -> Result<()> {
    if events.is_empty() { return Ok(()); }
    let metadata = std::mem::replace(&mut ctx.metadata, s.metadata.clone());
    let run_id = std::mem::replace(&mut ctx.run_id, s.run_id.clone());
    let proxy = std::mem::replace(&mut ctx.proxy_active, s.proxy_active);
    let result = bridge::ingest(ctx, Batch { v: 1, agent: "kimi".into(), session_id: id.into(),
        session_path: s.wire.as_ref().map(|p| p.to_string_lossy().into()), cwd: Some(s.cwd.clone()), agent_version: None, events });
    ctx.metadata = metadata; ctx.run_id = run_id; ctx.proxy_active = proxy;
    result
}
fn native(s: &mut Session, p: &Value, ts: u64, events: &mut Vec<Event>) {
    match text(p, "hook_event_name") {
        Some("UserPromptSubmit") if s.wire.is_none() => start(s, format!("hook-{ts}"), ts, p.get("prompt").cloned(), events),
        Some("PreToolUse") => { if let Some(id) = text(p, "tool_call_id") { s.tools.entry(id.into()).or_default().start = ts; } }
        Some("PostToolUse" | "PostToolUseFailure") => { if let Some(id) = text(p, "tool_call_id") {
            s.done.insert(id.into(), Pending { payload: p.clone(), ts, ..Default::default() });
        } }
        Some("Stop" | "StopFailure" | "SessionEnd") if s.wire.is_none() => end(s, ts, text(p,"error_message").map(str::to_string), events),
        _ => {},
    }
}
fn start(s: &mut Session, id: String, ts: u64, prompt: Option<Value>, events: &mut Vec<Event>) {
    if s.turn.as_deref() == Some(&id) { return; }
    s.turn = Some(id.clone()); s.ended = false; s.output = None;
    events.push(Event::TurnStart { turn_id: id, ts_ns: ts, prompt, metadata: BTreeMap::new() });
}
fn end(s: &mut Session, ts: u64, error: Option<String>, events: &mut Vec<Event>) {
    if s.ended { return; }
    if let Some(id) = &s.turn {
        events.push(Event::TurnEnd { turn_id: id.clone(), ts_ns: ts, output: s.output.take(),
            status: Some(if error.is_some() { "error" } else { "ok" }.into()), error });
        s.ended = true;
    }
}
fn complete_tools(s: &mut Session, events: &mut Vec<Event>) {
    let Some(turn) = &s.turn else { return; };
    s.done.retain(|id, pending| {
        if s.wire.is_some() && !s.tools.get(id).is_some_and(|t| t.identified) { return true; }
        let tool = s.tools.remove(id).unwrap_or_default();
        let p = &pending.payload;
        let failure = text(p, "hook_event_name") == Some("PostToolUseFailure");
        events.push(Event::Tool { turn_id: turn.clone(), id: id.clone(), parent_id: None, name: text(p,"tool_name").map(str::to_string),
            start_ns: if tool.start == 0 { pending.ts } else { tool.start.min(pending.ts) }, end_ns: pending.ts,
            input: p.get("tool_input").cloned(), output: p.get("tool_output").cloned(),
            status: Some(if failure { "error" } else { "ok" }.into()), error: text(p,"error").map(str::to_string), metadata: BTreeMap::new() });
        false
    });
}
fn home_dir() -> Result<PathBuf> {
    std::env::var_os("KIMI_CODE_HOME").map(PathBuf::from).map(Ok).unwrap_or_else(|| crate::paths::home().map(|p| p.join(".kimi-code")))
}
fn locate(id: &str, home: Option<&Path>) -> Result<Option<PathBuf>> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { return Ok(None); }
    let root = match home { Some(path) => path.to_path_buf(), None => home_dir()? };
    let dirs = match std::fs::read_dir(root.join("sessions")) {
        Ok(d) => d, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(e) => return Err(e.into()),
    };
    let mut found = None;
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir() { continue; }
        let path = dir.path().join(id).join("agents/main/wire.jsonl");
        if path.is_file() { if found.is_some() { return Err("ambiguous Kimi Code session path".into()); } found = Some(path); }
    }
    Ok(found)
}
fn tail_until(ctx: &HookCtx, s: &mut Session, events: &mut Vec<Event>, until: u64) -> Result<()> {
    if let Some(path) = &s.wire {
        for line in jsonl::read_new(path, &mut s.cursor)? {
            let r: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
            if matches!(text(&r,"type"), Some("turn.prompt" | "turn.ended" | "llm.request" | "context.append_loop_event")) { s.backlog.push(r); }
        }
    }
    for r in std::mem::take(&mut s.backlog) {
        let Some(ts) = r.get("time").and_then(Value::as_u64).and_then(|v| v.checked_mul(1_000_000)) else { continue; };
        if ts < s.baseline_ns { continue; }
        if ts > until { s.backlog.push(r); continue; }
        wire(s, &r, ts, ctx.config.content_cap_bytes, events);
    }
    Ok(())
}
fn wire(s: &mut Session, r: &Value, ts: u64, cap: usize, events: &mut Vec<Event>) {
    if text(r,"agentId") != Some("main") { return; }
    match text(r,"type") {
        Some("turn.prompt") => { if let Some(id) = r.get("turnId") { start(s, id.to_string(), ts, r.get("input").cloned(), events); } }
        Some("turn.ended") => {
            let reason = text(r,"reason").unwrap_or("unknown");
            end(s, ts, (reason != "completed").then(|| reason.to_string()), events);
        }
        Some("llm.request") => { s.model = text(r,"model").map(str::to_string); s.provider = text(r,"provider").map(str::to_string); }
        Some("context.append_loop_event") => {
            let e = &r["event"];
            match text(e,"type") {
                Some("step.begin") => { if let Some(id) = text(e,"uuid") { s.steps.insert(id.into(), Step { start: ts, ..Default::default() }); } }
                Some("tool.call") => { if let Some(id) = text(e,"toolCallId") { let tool = s.tools.entry(id.into()).or_default(); tool.identified = true; if tool.start == 0 { tool.start = ts; } } }
                Some("content.part") => { if let Some(step) = text(e,"stepUuid").and_then(|id| s.steps.get_mut(id)) {
                    let part = &e["part"]; let encoded = part.to_string(); let remaining = cap.saturating_sub(step.output_bytes);
                    if remaining > 0 { step.output.push(if encoded.len() <= remaining { part.clone() } else { json!(crate::redact::cap(&encoded, remaining)) }); step.output_bytes += encoded.len().min(remaining); }
                } }
                Some("step.end") => { if let (Some(turn), Some(id)) = (&s.turn, text(e,"uuid")) {
                    let step = s.steps.remove(id).unwrap_or(Step { start: ts, ..Default::default() });
                    if !step.output.is_empty() { s.output = Some(json!(step.output)); }
                    let usage = e.get("usage").map(|u| {
                        let other = u.get("inputOther").and_then(Value::as_u64); let read = u.get("inputCacheRead").and_then(Value::as_u64); let creation = u.get("inputCacheCreation").and_then(Value::as_u64);
                        Usage { input_tokens: other.zip(read).zip(creation).and_then(|((a,b),c)| a.checked_add(b)?.checked_add(c)),
                            output_tokens: u.get("output").and_then(Value::as_u64), cache_read_input_tokens: read, cache_creation_input_tokens: creation, reasoning_tokens: None }
                    });
                    events.push(Event::Chat { turn_id: turn.clone(), id: text(e,"messageId").unwrap_or(id).into(), parent_id: None,
                        start_ns: step.start.min(ts), end_ns: ts, model: s.model.clone(), provider: s.provider.clone(), input: None,
                        output: (!step.output.is_empty()).then(|| json!([{"role":"assistant","content":step.output}])), usage,
                        metadata: BTreeMap::new(), cost_usd: None, aux_task: None, status: Some("ok".into()), error: None });
                } }
                _ => {},
            }
        }
        _ => {},
    }
}
