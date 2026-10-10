//! Copilot hooks normalize into the shared assembler; native OTel supplies request usage.
mod setup;
mod otel;
#[cfg(test)] mod tests;

use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl}, AgentSpec}, record::HookCtx, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const SPEC: AgentSpec = AgentSpec { kind: "copilot", display: "GitHub Copilot CLI", hook, plan: setup::plan, reply: None, settle: Some(settle) };

#[derive(Default, Serialize, Deserialize)]
struct State {
    session: String,
    cwd: Option<String>,
    metadata: BTreeMap<String, Value>,
    run_id: Option<String>,
    proxy_active: bool,
    turns: Vec<Turn>,
    cursor: jsonl::Cursor,
    pending: Vec<otel::Request>,
}
#[derive(Serialize, Deserialize)]
struct Turn {
    id: String,
    native_id: Option<String>,
    start: u64,
    end: Option<u64>,
    prompt: Option<Value>,
    output: Option<Value>,
    error: Option<String>,
    failed: bool,
    settled: bool,
    rounds: u8,
}
fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> { value.get(camel).or_else(|| value.get(snake)) }
fn text(value: &Value, camel: &str, snake: &str) -> Option<String> { field(value, camel, snake).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned) }
fn event_name<'a>(value: &'a Value, configured: Option<&'a str>) -> Option<&'a str> { configured.or_else(|| value.get("hook_event_name").and_then(Value::as_str)).or_else(|| value.get("hookEventName").and_then(Value::as_str)) }
fn accepts(value: &Value, configured: Option<&str>) -> bool {
    if ["permission_mode", "workspaceRoot", "clientIdentifier", "toolUseId", "tool_use_id", "tool_response"].iter().any(|k| value.get(k).is_some()) { return false; }
    let native = value.get("sessionId").is_some_and(Value::is_string) && value.get("timestamp").is_some_and(Value::is_number);
    let compatible = value.get("session_id").is_some_and(Value::is_string) && value.get("timestamp").is_some_and(Value::is_string) && value.get("hook_event_name").is_some_and(Value::is_string);
    (native || compatible) && value.get("cwd").is_some_and(Value::is_string) && event_name(value, configured).is_some()
}
fn state_key(session: &str) -> String {
    // Hex encodes only the state filename, not span/trace keys (owned by Core).
    format!("session-{}.json", session.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
}
fn hook(ctx: &mut HookCtx, bytes: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(bytes)?;
    if std::env::var_os("GROK_SESSION_ID").is_some() || std::env::var_os("GROK_HOOK_EVENT").is_some() { return Ok(()); }
    let configured = std::env::var("HERDR_RESPAN_COPILOT_EVENT").ok();
    handle(ctx, &payload, configured.as_deref())
}
#[derive(Default, Serialize, Deserialize)]
struct Queued {
    payload: Value,
    configured: Option<String>,
    metadata: BTreeMap<String, Value>,
    run_id: Option<String>,
    proxy_active: bool,
    exporter: bool,
}
fn handle(ctx: &mut HookCtx, payload: &Value, configured: Option<&str>) -> Result<()> {
    if !accepts(payload, configured) { return Ok(()); }
    if text(payload, "sessionId", "session_id").is_some_and(|s| s.len() > 96) { return Err("Copilot session ID exceeds state filename bound".into()); }
    let mut payload = payload.clone();
    for key in ["prompt", "initialPrompt", "initial_prompt", "toolArgs", "tool_input", "toolResult", "tool_result", "response", "last_assistant_message"] {
        if let Some(value) = payload.get_mut(key) { let mut content = Some(value.take()); ctx.scrub_content(&mut content)?; *value = content.unwrap_or(Value::Null); }
    }
    let queued = Queued { payload, configured: configured.map(str::to_owned), metadata: ctx.metadata.clone(), run_id: ctx.run_id.clone(), proxy_active: ctx.proxy_active, exporter: std::env::var_os("COPILOT_OTEL_FILE_EXPORTER_PATH").is_some() };
    ctx.save(&format!("queue-{}-{}.json", ctx.now_ns(), crate::record::random_id()?), &queued)?;
    ctx.mark_settle_pending()?;
    let Some(_lock) = ctx.try_lock("copilot")? else { return Ok(()); };
    drain_queue(ctx)
}
fn drain_queue(ctx: &mut HookCtx) -> Result<()> {
    let mut files = std::fs::read_dir(ctx.state_dir())?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.file_name().is_some_and(|name| name.to_string_lossy().starts_with("queue-"))).collect::<Vec<_>>();
    files.sort();
    let old_metadata = ctx.metadata.clone(); let old_run = ctx.run_id.clone(); let old_proxy = ctx.proxy_active;
    let result = (|| -> Result<()> {
        for file in files {
            let name = file.file_name().ok_or("missing queue filename")?.to_string_lossy();
            let queued: Queued = ctx.load(&name)?;
            ctx.metadata = queued.metadata; ctx.run_id = queued.run_id; ctx.proxy_active = queued.proxy_active;
            process(ctx, &queued.payload, queued.configured.as_deref(), queued.exporter)?;
            std::fs::remove_file(file)?;
        }
        Ok(())
    })();
    ctx.metadata = old_metadata; ctx.run_id = old_run; ctx.proxy_active = old_proxy;
    result
}
fn process(ctx: &mut HookCtx, payload: &Value, configured: Option<&str>, exporter: bool) -> Result<()> {
    let session = text(payload, "sessionId", "session_id").ok_or("missing Copilot session")?;
    if session.len() > 96 { return Err("Copilot session ID exceeds state filename bound".into()); }
    let key = state_key(&session);
    let mut state: State = ctx.load(&key)?;
    state.session = session;
    state.cwd = text(payload, "cwd", "cwd");
    state.metadata = ctx.metadata.clone(); state.run_id = ctx.run_id.clone(); state.proxy_active = ctx.proxy_active;
    let now = payload.get("timestamp").and_then(otel::hook_time).unwrap_or_else(|| ctx.now_ns());
    let name = event_name(payload, configured).unwrap_or_default();
    let mut events = Vec::new();
    match name {
        "sessionStart" | "SessionStart" => {
            if let Some(prompt) = field(payload, "initialPrompt", "initial_prompt") { start_turn(ctx, &mut state, payload, now, Some(prompt.clone()), &mut events)?; }
        }
        "userPromptSubmitted" | "UserPromptSubmit" => start_turn(ctx, &mut state, payload, now, payload.get("prompt").cloned(), &mut events)?,
        "postToolUse" | "PostToolUse" | "postToolUseFailure" | "PostToolUseFailure" => {
            let Some(turn) = state.turns.last().filter(|t| t.end.is_none()) else { return Ok(()); };
            let Some(tool) = text(payload, "toolName", "tool_name") else { return Ok(()); };
            let failed = name.ends_with("Failure");
            let result = field(payload, "toolResult", "tool_result");
            let output = result.and_then(|r| field(r, "textResultForLlm", "text_result_for_llm")).cloned();
            events.push(Event::Tool { turn_id: turn.id.clone(), id: text(payload, "toolCallId", "tool_call_id").unwrap_or_else(|| format!("{tool}-{now}")), parent_id: None, name: Some(tool), start_ns: now, end_ns: now, input: field(payload, "toolArgs", "tool_input").cloned(), output, status: Some(if failed { "error" } else { "ok" }.into()), error: payload.get("error").and_then(Value::as_str).map(str::to_owned), metadata: BTreeMap::new() });
        }
        "agentStop" | "Stop" | "sessionEnd" | "SessionEnd" => {
            let Some(turn) = state.turns.last_mut().filter(|t| t.end.is_none()) else { return Ok(()); };
            turn.end = Some(now.max(turn.start));
            turn.output = field(payload, "response", "last_assistant_message").cloned(); ctx.scrub_content(&mut turn.output)?;
            turn.error = payload.get("error").and_then(Value::as_str).map(str::to_owned);
            turn.failed = turn.error.is_some() || payload.get("reason").and_then(Value::as_str).is_some_and(|s| matches!(s, "error" | "abort" | "timeout"));
            let wait = ctx.state_dir().join("otel.jsonl").exists() || exporter;
            collect_finished(ctx, &mut state, !wait, &mut events)?;
        }
        _ => return Ok(()),
    }
    ingest(ctx, &state, events)?;
    if state.turns.len() > 128 && state.turns.first().is_some_and(|t| t.settled) { state.turns.remove(0); }
    ctx.save(&key, &state)?;
    if state.turns.iter().any(|t| t.end.is_some() && !t.settled) { ctx.mark_settle_pending()?; }
    Ok(())
}
fn ingest(ctx: &mut HookCtx, state: &State, events: Vec<Event>) -> Result<()> {
    if events.is_empty() { return Ok(()); }
    bridge::ingest(ctx, Batch { v: 1, agent: "copilot".into(), session_id: state.session.clone(), session_path: None, cwd: state.cwd.clone(), agent_version: None, events })
}
fn start_turn(ctx: &HookCtx, state: &mut State, payload: &Value, now: u64, mut prompt: Option<Value>, events: &mut Vec<Event>) -> Result<()> {
    let native = text(payload, "turnId", "turn_id");
    let id = native.clone().unwrap_or_else(|| format!("hook-{now}"));
    if state.turns.iter().any(|t| t.id == id) { return Ok(()); }
    ctx.scrub_content(&mut prompt)?;
    events.push(Event::TurnStart { turn_id: id.clone(), ts_ns: now, prompt: prompt.clone(), metadata: BTreeMap::new() });
    state.turns.push(Turn { id, native_id: native, start: now, end: None, prompt, output: None, error: None, failed: false, settled: false, rounds: 0 });
    Ok(())
}
fn collect_finished(ctx: &HookCtx, state: &mut State, force: bool, events: &mut Vec<Event>) -> Result<()> {
    let path = ctx.state_dir().join("otel.jsonl");
    if path.exists() {
        for line in jsonl::read_new(&path, &mut state.cursor)? {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                state.pending.extend(otel::requests(&value).into_iter().filter(|r| r.session == state.session));
            }
        }
    }
    for turn in state.turns.iter_mut().filter(|t| t.end.is_some() && !t.settled) {
        let end = turn.end.expect("closed turn");
        let mut requests = Vec::new();
        state.pending.retain(|request| {
            let matches = if let Some(id) = &turn.native_id { id == &request.turn } else { request.start >= turn.start && request.start <= end };
            if matches { requests.push(request.event(&turn.id)); }
            !matches
        });
        if requests.is_empty() && !force && turn.rounds < 2 { continue; }
        if requests.is_empty() {
            requests.push(Event::Chat { turn_id: turn.id.clone(), id: "hook-chat".into(), parent_id: None, start_ns: turn.start, end_ns: end, model: None, provider: None, input: turn.prompt.clone().map(|p| json!([{ "role": "user", "content": p }])), output: turn.output.clone().map(|p| json!([{ "role": "assistant", "content": p }])), usage: None, cost_usd: None, aux_task: None, status: Some(if turn.failed { "error" } else { "ok" }.into()), error: turn.error.clone(), metadata: BTreeMap::new() });
        }
        events.extend(requests);
        events.push(Event::TurnEnd { turn_id: turn.id.clone(), ts_ns: end, output: turn.output.clone(), status: Some(if turn.failed { "error" } else { "ok" }.into()), error: turn.error.clone() });
        turn.settled = true;
    }
    // Metadata-only rows outside known windows cannot belong to a closed turn here.
    state.pending.retain(|r| !state.turns.iter().any(|t| t.settled && r.start >= t.start && t.end.is_some_and(|end| r.start <= end)));
    if state.pending.len() > 128 { state.pending.drain(..state.pending.len() - 128); }
    Ok(())
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let _lock = ctx.lock("copilot")?;
    drain_queue(ctx)?;
    let mut pending = false;
    let old_metadata = ctx.metadata.clone(); let old_run = ctx.run_id.clone(); let old_proxy = ctx.proxy_active;
    let result = (|| -> Result<()> {
        for entry in std::fs::read_dir(ctx.state_dir())? {
            let entry = entry?; let name = entry.file_name(); let name = name.to_string_lossy();
            if !name.starts_with("session-") || !name.ends_with(".json") { continue; }
            let mut state: State = ctx.load(&name)?;
            if !state.turns.iter().any(|t| t.end.is_some() && !t.settled) { continue; }
            ctx.metadata = state.metadata.clone(); ctx.run_id = state.run_id.clone(); ctx.proxy_active = state.proxy_active;
            let mut events = Vec::new(); collect_finished(ctx, &mut state, false, &mut events)?;
            ingest(ctx, &state, events)?;
            for turn in state.turns.iter_mut().filter(|t| t.end.is_some() && !t.settled) { turn.rounds += 1; pending = true; }
            ctx.save(&name, &state)?;
        }
        Ok(())
    })();
    ctx.metadata = old_metadata; ctx.run_id = old_run; ctx.proxy_active = old_proxy;
    result?; Ok(pending)
}
