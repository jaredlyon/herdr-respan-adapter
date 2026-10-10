//! Official xai-org/grok-build hooks and raw conversation history.
mod setup;
#[cfg(test)] mod tests;

use crate::{agents::{AgentSpec, common::{bridge::{self, Batch, Event}, jsonl::{self, Cursor}}}, record::HookCtx, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "grok", display: "Grok Build", hook, plan: setup::plan, reply: None, settle: Some(settle),
};

#[derive(Default, Serialize, Deserialize)]
struct State {
    session: String, cwd: Option<String>, proxy: bool, settle_left: u8,
    turn: Option<String>, start: u64, closed: bool, input: Vec<Value>,
    chats: u64, final_output: Option<Value>,
    transcript: Option<PathBuf>, cursor: Cursor,
    tools: BTreeMap<String, u64>, subs: BTreeMap<String, Sub>, parents: BTreeMap<String, String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Sub {
    turn: String, name: Option<String>, start: Option<u64>, end: Option<u64>,
    output: Option<Value>, started: bool, ended: bool,
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> { v.get(key)?.as_str().filter(|s| !s.is_empty()) }
fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

fn identity(v: &Value, session_env: Option<&str>, event_env: Option<&str>) -> bool {
    let Some(session) = text(v, "sessionId") else { return false; };
    text(v, "hookEventName").is_some() && text(v, "workspaceRoot").is_some()
        && text(v, "timestamp").is_some()
        && session_env.is_none_or(|s| s == session)
        && event_env.is_none_or(|s| text(v, "hookEventName") == Some(s))
        && text(v, "session_id").is_none_or(|s| s == session)
}

fn hook(ctx: &mut HookCtx, stdin: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(stdin)?;
    let session_env = std::env::var("GROK_SESSION_ID").ok();
    let event_env = std::env::var("GROK_HOOK_EVENT").ok();
    if !identity(&payload, session_env.as_deref(), event_env.as_deref()) { return Ok(()); }
    let session = text(&payload, "sessionId").ok_or("missing Grok session")?;
    let key = format!("native-{}.json", digest(session.as_bytes()));
    let _lock = ctx.lock(&format!("{key}.lock"))?;
    let mut state: State = ctx.load(&key)?;
    let now = timestamp(text(&payload, "timestamp").unwrap_or_default()).unwrap_or_else(|| ctx.now_ns());
    let duplicate_models = ctx.proxy_active || ["GROK_MODELS_BASE_URL", "GROK_XAI_API_BASE_URL", "GROK_CLI_CHAT_PROXY_BASE_URL"]
        .iter().any(|name| std::env::var(name).is_ok_and(|url| url.contains("api.respan.ai")));
    let batch = normalize(&payload, &mut state, now, duplicate_models)?;
    bridge::ingest(ctx, batch)?;
    save_state(ctx, &key, &mut state)?;
    if state.settle_left > 0 { ctx.mark_settle_pending()?; }
    Ok(())
}


fn save_state(ctx: &HookCtx, key: &str, state: &mut State) -> Result<()> {
    if ctx.config.content_cap_bytes == 0 {
        state.input.clear();
        if let Some(output) = &mut state.final_output { *output = Value::Null; }
        for sub in state.subs.values_mut() { sub.output = None; }
    }
    ctx.save(key, state)
}

fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let mut pending = false;
    for entry in std::fs::read_dir(ctx.state_dir())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("native-") || !name.ends_with(".json") { continue; }
        let _lock = ctx.lock(&format!("{name}.lock"))?;
        let mut state: State = ctx.load(&name)?;
        if state.settle_left == 0 { continue; }
        let mut events = Vec::new();
        transcript(&mut state, ctx.now_ns(), false, &mut events)?;
        state.settle_left -= 1;
        if state.settle_left == 0 { fallback_chat(&mut state, ctx.now_ns(), &mut events); }
        if !events.is_empty() {
            bridge::ingest(ctx, Batch { v: 1, agent: "grok".into(), session_id: state.session.clone(),
                session_path: state.transcript.as_ref().map(|p| p.to_string_lossy().into_owned()),
                cwd: state.cwd.clone(), agent_version: None, events })?;
        }
        pending |= state.settle_left != 0;
        save_state(ctx, &name, &mut state)?;
    }
    Ok(pending)
}

fn normalize(v: &Value, state: &mut State, now: u64, proxy: bool) -> Result<Batch> {
    state.session = text(v, "sessionId").ok_or("missing Grok session")?.into();
    state.cwd = text(v, "cwd").map(str::to_owned);
    state.proxy = proxy;
    let mut events = Vec::new();
    let event = text(v, "hookEventName").unwrap_or_default();
    if let Some(path) = text(v, "transcriptPath") {
        // Grok's hook path is updates.jsonl; raw chat rows are its sibling cache.
        let path = Path::new(path).with_file_name("chat_history.jsonl");
        if state.transcript.as_ref() != Some(&path) {
            state.transcript = Some(path);
            state.cursor = Cursor::default();
        }
    }
    if event == "session_start" || state.turn.is_none() {
        baseline(state)?;
    } else {
        transcript(state, now, proxy, &mut events)?;
    }
    if event == "user_prompt_submit" {
        state.turn = Some(text(v, "promptId").map(str::to_owned).unwrap_or_else(|| format!("prompt-{now}")));
        state.start = now;
        state.closed = false;
        state.input.clear();
        state.chats = 0;
        state.final_output = None;
        if let Some(prompt) = v.get("prompt") { state.input.push(json!({"role":"user","content":prompt})); }
        state.tools.clear();
        let turn = state.turn.clone().ok_or("missing Grok turn")?;
        events.push(Event::TurnStart { turn_id: turn, ts_ns: now, prompt: v.get("prompt").cloned(), metadata: BTreeMap::new() });
    }
    let turn = text(v, "promptId").map(str::to_owned).or_else(|| state.turn.clone());
    if let Some(turn) = turn {
        match event {
            "pre_tool_use" => {
                if let Some(id) = text(v, "toolUseId") {
                    state.tools.insert(id.into(), now);
                    bind_spawn(v, id, state);
                }
            }
            "post_tool_use" | "post_tool_use_failure" => {
                if let Some(id) = text(v, "toolUseId") {
                    bind_spawn(v, id, state);
                    let start = state.tools.remove(id).unwrap_or_else(|| now.saturating_sub(
                        v.get("durationMs").and_then(Value::as_u64).unwrap_or(0).saturating_mul(1_000_000)));
                    let failed = event == "post_tool_use_failure";
                    // Keep upstream clipping flags alongside their corresponding values.
                    let input = v.get("toolInput").map(|value| json!({"value":value,"truncated":v.get("toolInputTruncated")}));
                    let output = v.get("toolResult").map(|value| json!({"value":value,"truncated":v.get("toolResultTruncated")}));
                    events.push(Event::Tool { turn_id: turn.clone(), id: id.into(), parent_id: None,
                        name: text(v, "toolName").map(str::to_owned), start_ns: start.min(now), end_ns: now,
                        input, output, status: Some(if failed { "error" } else { "ok" }.into()),
                        error: text(v, "error").map(str::to_owned), metadata: BTreeMap::new() });
                }
            }
            "subagent_start" | "subagent_stop" => {
                if let Some(id) = text(v, "subagentId") {
                    let sub = state.subs.entry(id.into()).or_insert_with(|| Sub { turn: turn.clone(), ..Default::default() });
                    sub.name = text(v, "subagentType").map(str::to_owned);
                    if event == "subagent_start" { sub.start = Some(now); }
                    // The gate phase can block stopping; only observe is a completed lifetime.
                    else if text(v, "phase") == Some("observe") {
                        sub.end = Some(now);
                        sub.output = v.get("lastAssistantMessage").cloned();
                    }
                }
            }
            "stop" | "stop_failure" | "stop_cancelled" | "session_end" if !state.closed && state.turn.is_some() => {
                    let failed = matches!(event, "stop_failure" | "stop_cancelled")
                        || (event == "session_end" && matches!(text(v, "reason"), Some("error" | "abort" | "timeout" | "cancelled")));
                    events.push(Event::TurnEnd { turn_id: turn, ts_ns: now,
                        output: v.get("lastAssistantMessage").cloned(),
                        status: Some(if failed { "error" } else { "ok" }.into()),
                        error: text(v, "errorDetails").or_else(|| text(v, "error")).or_else(|| if failed { text(v, "reason") } else { None }).map(str::to_owned) });
                    state.final_output = v.get("lastAssistantMessage").cloned();
                    state.closed = true;
                    state.settle_left = if state.transcript.is_some() && !proxy { 3 } else { 0 };
            }
            _ => {}
        }
    }
    if state.closed && state.settle_left == 0 && !proxy { fallback_chat(state, now, &mut events); }
    for (id, sub) in &mut state.subs {
        // Never guess a concurrent child's spawning tool from its name or timing.
        let Some(parent) = state.parents.get(id) else { continue; };
        if let Some(start) = sub.start {
            if !sub.started {
                events.push(Event::SubagentStart { turn_id: sub.turn.clone(), id: id.clone(), parent_id: Some(parent.clone()), ts_ns: start, name: sub.name.clone(), metadata: BTreeMap::new() });
                sub.started = true;
            }
            if let Some(end) = sub.end {
                if !sub.ended {
                    events.push(Event::SubagentEnd { turn_id: sub.turn.clone(), id: id.clone(), ts_ns: end, status: Some("ok".into()), output: sub.output.take() });
                    sub.ended = true;
                }
            }
        }
    }
    let completed: Vec<_> = state.subs.iter().filter(|(_, sub)| sub.ended).map(|(id, _)| id.clone()).collect();
    for id in completed { state.subs.remove(&id); state.parents.remove(&id); }
    Ok(Batch { v: 1, agent: "grok".into(), session_id: text(v, "sessionId").ok_or("missing Grok session")?.into(),
        session_path: text(v, "transcriptPath").map(str::to_owned), cwd: text(v, "cwd").map(str::to_owned), agent_version: None, events })
}

fn fallback_chat(state: &mut State, now: u64, events: &mut Vec<Event>) {
    let (Some(turn), Some(output)) = (&state.turn, &state.final_output) else { return; };
    if state.chats != 0 || state.proxy { return; }
    events.push(Event::Chat { turn_id: turn.clone(), id: "final-hook".into(), parent_id: None,
        start_ns: now, end_ns: now, model: None, provider: None,
        input: Some(Value::Array(std::mem::take(&mut state.input))),
        output: Some(json!([{"role":"assistant","content":output}])), usage: None,
        cost_usd: None, aux_task: None, status: Some("ok".into()), error: None,
        metadata: BTreeMap::from([("grok.timing_source".into(), json!("hook_observed"))]) });
    state.chats += 1;
}

fn bind_spawn(v: &Value, tool: &str, state: &mut State) {
    if !matches!(text(v, "toolName"), Some("task" | "Task" | "spawn_subagent")) { return; }
    let result = v.get("toolResult").and_then(|r| r.get("output")).or_else(|| v.get("toolResult"));
    let child = v.pointer("/toolInput/task_id").and_then(Value::as_str)
        .or_else(|| result.and_then(|r| text(r, "subagent_id")))
        .or_else(|| result.and_then(|r| text(r, "task_id")));
    if let Some(child) = child.filter(|s| !s.is_empty()) { state.parents.insert(child.into(), tool.into()); }
}

fn baseline(state: &mut State) -> Result<()> {
    if let Some(path) = &state.transcript {
        match jsonl::read_new(path, &mut state.cursor) {
            Ok(_) => {}, Err(e) if missing(e.as_ref()) => {}, Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn missing(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool { e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) }

fn transcript(state: &mut State, now: u64, proxy: bool, events: &mut Vec<Event>) -> Result<()> {
    let (Some(path), Some(turn)) = (&state.transcript, &state.turn) else { return Ok(()); };
    let lines = match jsonl::read_new(path, &mut state.cursor) {
        Ok(lines) => lines, Err(e) if missing(e.as_ref()) => return Ok(()), Err(e) => return Err(e),
    };
    let mut offset = state.cursor.offset.saturating_sub(lines.iter().map(|line| line.len() as u64 + 1).sum::<u64>());
    for line in lines {
        let id = format!("row-{offset}-{}", digest(line.as_bytes()));
        offset = offset.saturating_add(line.len() as u64 + 1);
        let Ok(row) = serde_json::from_str::<Value>(&line) else { continue; };
        let role = text(&row, "type").or_else(|| text(&row, "role")).unwrap_or_default();
        if role != "assistant" {
            if !proxy && matches!(role, "user" | "system" | "tool_result" | "tool") {
                let content = row.get("content").cloned().unwrap_or(Value::Null);
                let same_prompt = role == "user" && state.input.last().is_some_and(|last| {
                    last.get("role") == Some(&json!("user")) && last.get("content").and_then(Value::as_str).is_some_and(|prompt|
                        content.as_array().and_then(|parts| if parts.len() == 1 { text(&parts[0], "text") } else { None }) == Some(prompt))
                });
                if !same_prompt { state.input.push(json!({"role":if role == "tool_result" { "tool" } else { role },"content":content,"tool_call_id":row.get("tool_call_id")})); }
            }
            continue;
        }
        if proxy { continue; }
        state.chats += 1;
        events.push(Event::Chat { turn_id: turn.clone(), id, parent_id: None,
            start_ns: now, end_ns: now, model: text(&row, "model_id").map(str::to_owned), provider: None,
            input: Some(Value::Array(std::mem::take(&mut state.input))), output: Some(json!([{"role":"assistant","content":row.get("content"),"tool_calls":row.get("tool_calls")}])) ,
            usage: None, cost_usd: None, aux_task: None, status: Some("ok".into()), error: None, metadata: BTreeMap::from([("grok.timing_source".into(), json!("hook_observed"))]) });
    }
    Ok(())
}

// Hook envelopes use chrono UTC RFC3339. Parse that exact source format without another dependency.
fn timestamp(s: &str) -> Option<u64> {
    if !s.ends_with('Z') && !s.ends_with("+00:00") { return None; }
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    if s.get(4..5)? != "-" || s.get(7..8)? != "-" || s.get(10..11)? != "T" || s.get(13..14)? != ":" || s.get(16..17)? != ":" { return None; }
    let (mut y, m, d, h, minute, second) = (n(0,4)?, n(5,7)?, n(8,10)?, n(11,13)?, n(14,16)?, n(17,19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(0..24).contains(&h) || !(0..60).contains(&minute) || !(0..60).contains(&second) { return None; }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max_day = match m { 2 => if leap { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, _ => 31 };
    if d > max_day { return None; }
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d - 1 - 719468;
    let seconds = u64::try_from(days * 86400 + h * 3600 + minute * 60 + second).ok()?;
    let tail = s.get(19..)?;
    let tail = tail.strip_suffix('Z').or_else(|| tail.strip_suffix("+00:00"))?;
    let fraction = if tail.is_empty() { "" } else { tail.strip_prefix('.')? };
    if !fraction.bytes().all(|c| c.is_ascii_digit()) || fraction.len() > 9 { return None; }
    let nanos = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? * 10_u64.pow(9 - fraction.len() as u32) };
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}
