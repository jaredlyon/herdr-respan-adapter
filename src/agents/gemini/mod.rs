//! Gemini's chunk hooks are observations, not completed model responses.
mod setup;
#[cfg(test)] mod tests;

use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl}, AgentSpec}, record::{HookCtx, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, VecDeque}, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec { kind: "gemini", display: "Gemini CLI", hook, plan: setup::plan, reply: None, settle: None };

#[derive(Default, Serialize, Deserialize)]
struct State {
    turn: Option<String>,
    request: Option<Request>,
    tools: VecDeque<Tool>,
    #[serde(default)]
    tool_sequence: u64,
    transcript: jsonl::Cursor,
    transcript_path: Option<String>,
    token_ids: VecDeque<String>,
    seen: VecDeque<String>,
}
#[derive(Serialize, Deserialize)]
struct Request {
    id: String, start: u64, end: u64, model: Option<String>, input: Option<Value>,
    text: Option<Value>, usage: Usage, chunks: u64,
}
#[derive(Serialize, Deserialize)]
struct Tool { id: String, name: String, input: Option<Value>, fingerprint: String, start: u64 }

fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> { value.get(key).and_then(Value::as_str) }
fn model_request(payload: &Value) -> bool {
    let request = &payload["llm_request"];
    text(request, "model").is_some_and(|m| !m.is_empty()) && request.get("messages").is_some_and(Value::is_array)
}
fn identity(payload: &Value) -> bool {
    if ["permission_mode", "tool_use_id", "prompt_id", "conversation_id"].iter().any(|k| payload.get(k).is_some()) { return false; }
    if ["session_id", "transcript_path", "cwd", "timestamp"].iter().any(|k| text(payload, k).is_none_or(str::is_empty)) { return false; }
    match text(payload, "hook_event_name") {
        Some("BeforeAgent") => text(payload, "prompt").is_some(),
        Some("AfterAgent") => text(payload, "prompt_response").is_some(),
        Some("BeforeModel") => model_request(payload),
        Some("AfterModel") => model_request(payload) && payload.pointer("/llm_response/candidates").is_some_and(Value::is_array),
        Some("BeforeTool") => text(payload, "tool_name").is_some() && payload.get("tool_input").is_some_and(Value::is_object),
        Some("AfterTool") => text(payload, "tool_name").is_some() && payload.get("tool_input").is_some_and(Value::is_object) && payload.get("tool_response").is_some_and(Value::is_object),
        Some("SessionStart" | "SessionEnd") => {
            let shape = if payload["hook_event_name"] == "SessionStart" {
                matches!(text(payload, "source"), Some("startup" | "resume" | "clear"))
            } else {
                matches!(text(payload, "reason"), Some("exit" | "clear" | "logout" | "prompt_input_exit" | "other"))
            };
            shape && (std::env::var("HERDR_RESPAN_ADAPTER").as_deref() == Ok("gemini") || text(payload, "transcript_path").is_some_and(|p| p.replace('\\', "/").contains("/.gemini/")))
        }
        _ => false,
    }
}

// Native tool hooks may run concurrently; bridge's lock only protects assembled events.
struct Lock(PathBuf);
impl Lock {
    fn acquire(path: PathBuf) -> Result<Self> {
        let start = std::time::Instant::now();
        loop {
            let mut options = std::fs::OpenOptions::new(); options.write(true).create_new(true);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
            match options.open(&path) {
                Ok(_) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if path.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|t| t.as_secs() > 60) {
                        match std::fs::remove_file(&path) { Ok(()) => continue, Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()) }
                    }
                    if start.elapsed().as_millis() >= 40 { return Err("Gemini adapter state busy".into()); }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for Lock { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }

fn hook(ctx: &mut HookCtx, bytes: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(bytes)?;
    if !identity(&payload) { return Ok(()); }
    let session = text(&payload, "session_id").expect("validated session");
    let key = format!("session-{}.json", digest(session.as_bytes()));
    let _lock = Lock::acquire(ctx.state_dir().join(format!("{key}.lock")))?;
    let mut state: State = ctx.load(&key)?;
    let event_id = digest(&serde_json::to_vec(&payload)?);
    if state.seen.contains(&event_id) { return Ok(()); }
    let now = ctx.now_ns();
    let mut batch = Batch { v: 1, agent: "gemini".into(), session_id: session.into(), session_path: text(&payload, "transcript_path").map(str::to_owned), cwd: text(&payload, "cwd").map(str::to_owned), agent_version: None, events: Vec::new() };
    translate(ctx, &mut state, &payload, now, &mut batch.events)?;
    bridge::ingest(ctx, batch)?;
    state.seen.push_back(event_id);
    while state.seen.len() > 1024 { state.seen.pop_front(); }
    ctx.save(&key, &state)
}

fn next_tool_id(state: &mut State, payload: &Value) -> Result<String> {
    state.tool_sequence = state.tool_sequence.checked_add(1).ok_or("Gemini tool sequence exhausted")?;
    Ok(format!("tool-{}-{}", text(payload, "timestamp").expect("validated timestamp"), state.tool_sequence))
}
fn ensure_turn(state: &mut State, payload: &Value, now: u64, events: &mut Vec<Event>) -> String {
    state.turn.get_or_insert_with(|| {
        let id = format!("turn-{}", text(payload, "timestamp").unwrap_or("unknown"));
        events.push(Event::TurnStart { turn_id: id.clone(), ts_ns: now, prompt: None, metadata: Default::default() }); id
    }).clone()
}
fn start_request(ctx: &HookCtx, payload: &Value, now: u64) -> Result<Request> {
    let request = &payload["llm_request"];
    let mut input = request.get("messages").cloned();
    if let Some(Value::Array(messages)) = &mut input {
        for message in messages { if message["role"] == "model" { message["role"] = json!("assistant"); } }
    }
    ctx.scrub_content(&mut input)?;
    Ok(Request { id: format!("request-{}", text(payload, "timestamp").unwrap_or("unknown")), start: now, end: now, model: text(request, "model").map(str::to_owned), input, text: None, usage: Usage::default(), chunks: 0 })
}
fn update_usage(usage: &mut Usage, native: &Value, names: [&str; 4]) {
    for (slot, name) in [(&mut usage.input_tokens, names[0]), (&mut usage.output_tokens, names[1]), (&mut usage.cache_read_input_tokens, names[2]), (&mut usage.reasoning_tokens, names[3])] {
        if let Some(value) = native.get(name).and_then(Value::as_u64) { *slot = Some(value); }
    }
}
fn transcript(state: &mut State, path: &str) -> Result<Vec<(String, Option<String>, Usage)>> {
    if state.transcript_path.as_deref() != Some(path) {
        state.transcript = jsonl::Cursor::default(); state.token_ids.clear(); state.transcript_path = Some(path.into());
    }
    let lines = match jsonl::read_new(Path::new(path), &mut state.transcript) {
        Ok(lines) => lines,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut messages = BTreeMap::new();
    for line in lines {
        let Ok(record) = serde_json::from_str::<Value>(&line) else { continue; };
        // A repeated full message replaces the previous version. Patches contain no usage.
        if record["type"] != "gemini" { continue; }
        let (Some(id), Some(tokens)) = (text(&record, "id"), record.get("tokens").filter(|t| t.is_object())) else { continue; };
        if state.token_ids.iter().any(|old| old == id) { continue; }
        let mut usage = Usage::default(); update_usage(&mut usage, tokens, ["input", "output", "cached", "thoughts"]);
        messages.insert(id.to_owned(), (text(&record, "model").map(str::to_owned), usage));
    }
    let records = messages.into_iter().map(|(id, (model, usage))| {
        state.token_ids.push_back(id.clone()); (id, model, usage)
    }).collect();
    while state.token_ids.len() > 1024 { state.token_ids.pop_front(); }
    Ok(records)
}
fn close_request(state: &mut State, payload: &Value, now: u64, events: &mut Vec<Event>) -> Result<()> {
    let records = transcript(state, text(payload, "transcript_path").expect("validated transcript"))?;
    let Some(mut request) = state.request.take() else { return Ok(()); };
    if records.len() == 1 {
        let (_, model, usage) = &records[0];
        let matches = model.as_ref().is_none_or(|m| request.model.as_ref().is_none_or(|r| r == m))
            && usage.input_tokens.is_none_or(|n| request.usage.input_tokens.is_none_or(|r| r == n))
            && usage.output_tokens.is_none_or(|n| request.usage.output_tokens.is_none_or(|r| r == n));
        if matches {
            request.usage.input_tokens = request.usage.input_tokens.or(usage.input_tokens);
            request.usage.output_tokens = request.usage.output_tokens.or(usage.output_tokens);
            request.usage.cache_read_input_tokens = usage.cache_read_input_tokens;
            request.usage.reasoning_tokens = usage.reasoning_tokens;
        }
    }
    let output = request.text.map(|text| json!([{"role":"assistant", "content":text}]));
    let interrupted = request.chunks == 0;
    events.push(Event::Chat { turn_id: state.turn.clone().expect("request has turn"), id: request.id, parent_id: None, start_ns: request.start, end_ns: request.end.max(now), model: request.model, provider: Some("google".into()), input: request.input, output, usage: Some(request.usage), cost_usd: None, aux_task: None, status: Some(if interrupted { "error" } else { "ok" }.into()), error: interrupted.then(|| "model response not observed".into()), metadata: Default::default() });
    Ok(())
}
fn translate(ctx: &HookCtx, state: &mut State, payload: &Value, now: u64, events: &mut Vec<Event>) -> Result<()> {
    let event = text(payload, "hook_event_name").expect("validated event");
    if matches!(event, "BeforeAgent" | "BeforeModel" | "BeforeTool" | "AfterAgent" | "SessionEnd") {
        close_request(state, payload, now, events)?;
    }
    match event {
        "SessionStart" => { let _ = transcript(state, text(payload, "transcript_path").unwrap())?; }
        "BeforeAgent" => {
            if let Some(turn_id) = state.turn.take() {
                events.push(Event::TurnEnd { turn_id, ts_ns: now, output: None, status: Some("error".into()), error: Some("agent turn superseded".into()) });
            }
            let turn_id = format!("turn-{}", text(payload, "timestamp").unwrap());
            state.turn = Some(turn_id.clone());
            let mut prompt = payload.get("prompt").cloned(); ctx.scrub_content(&mut prompt)?;
            events.push(Event::TurnStart { turn_id, ts_ns: now, prompt, metadata: Default::default() });
        }
        "BeforeModel" => { ensure_turn(state, payload, now, events); state.request = Some(start_request(ctx, payload, now)?); }
        "AfterModel" => {
            ensure_turn(state, payload, now, events);
            if state.request.is_none() { state.request = Some(start_request(ctx, payload, now)?); }
            let request = state.request.as_mut().unwrap(); request.end = now; request.chunks += 1;
            let response = &payload["llm_response"];
            // text is the translator's convenience copy of candidate parts, not additional content.
            let chunk = if let Some(text) = text(response, "text") { text.to_owned() } else {
                response["candidates"].as_array().and_then(|c| c.first()).and_then(|c| c.pointer("/content/parts")).and_then(Value::as_array)
                    .map(|parts| parts.iter().filter_map(Value::as_str).collect::<String>()).unwrap_or_default()
            };
            if ctx.config.content_cap_bytes > 0 && !chunk.is_empty() {
                let accumulated = request.text.get_or_insert_with(|| json!(""));
                if let Some(existing) = accumulated.as_str() { *accumulated = json!(format!("{existing}{chunk}")); }
                ctx.scrub_content(&mut request.text)?;
            }
            if let Some(usage) = response.get("usageMetadata") { update_usage(&mut request.usage, usage, ["promptTokenCount", "candidatesTokenCount", "cachedContentTokenCount", "thoughtsTokenCount"]); }
        }
        "BeforeTool" => {
            ensure_turn(state, payload, now, events);
            let mut input = payload.get("tool_input").cloned(); ctx.scrub_content(&mut input)?;
            let id = next_tool_id(state, payload)?;
            state.tools.push_back(Tool { id, name: text(payload, "tool_name").unwrap().into(), input, fingerprint: digest(&serde_json::to_vec(&payload["tool_input"])?), start: now });
        }
        "AfterTool" => {
            let turn_id = ensure_turn(state, payload, now, events);
            let name = text(payload, "tool_name").unwrap();
            let fingerprint = digest(&serde_json::to_vec(&payload["tool_input"])?);
            let index = state.tools.iter().position(|tool| tool.name == name && tool.fingerprint == fingerprint);
            let tool = index.and_then(|i| state.tools.remove(i));
            let id = match &tool { Some(tool) => tool.id.clone(), None => next_tool_id(state, payload)? };
            let mut input = tool.as_ref().and_then(|t| t.input.clone()).or_else(|| payload.get("tool_input").cloned()); ctx.scrub_content(&mut input)?;
            let error = payload.pointer("/tool_response/error").filter(|e| !e.is_null()).map(|e| e.as_str().map(str::to_owned).unwrap_or_else(|| e.to_string()));
            let mut output = payload.get("tool_response").cloned(); ctx.scrub_content(&mut output)?;
            events.push(Event::Tool { turn_id, id, parent_id: None, name: Some(name.into()), start_ns: tool.as_ref().map_or(now, |t| t.start), end_ns: now, input, output, status: Some(if error.is_some() { "error" } else { "ok" }.into()), error, metadata: Default::default() });
        }
        "AfterAgent" | "SessionEnd" => {
            if let Some(turn_id) = state.turn.take() {
                for tool in state.tools.drain(..) {
                    events.push(Event::Tool { turn_id: turn_id.clone(), id: tool.id, parent_id: None, name: Some(tool.name), start_ns: tool.start, end_ns: now, input: tool.input, output: None, status: Some("error".into()), error: Some("tool response not observed".into()), metadata: Default::default() });
                }
                let mut output = payload.get("prompt_response").cloned(); ctx.scrub_content(&mut output)?;
                events.push(Event::TurnEnd { turn_id, ts_ns: now, output, status: Some(if event == "SessionEnd" { "error" } else { "ok" }.into()), error: (event == "SessionEnd").then(|| "session ended during agent turn".into()) });
            }
        }
        _ => {}
    }
    Ok(())
}
