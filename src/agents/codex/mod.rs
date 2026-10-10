//! Codex hooks translate rollout records into the shared bridge; no transport here.
mod parse;
mod setup;
#[cfg(test)] mod tests;

use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl}, AgentSpec}, record::HookCtx, Result};
use parse::{text, Stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec { kind: "codex", display: "Codex", hook, plan: setup::plan, reply: None, settle: Some(settle) };
const EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PostToolUse", "SubagentStart", "SubagentStop", "Stop", "Interrupt", "SessionEnd"];

#[derive(Default, Serialize, Deserialize)]
struct State { streams: BTreeMap<String, Stream>, children: BTreeMap<String, Child> }
#[derive(Serialize, Deserialize)]
struct Child { turn: String, parent: Option<String> }
#[derive(Serialize, Deserialize)]
struct Pending { payload: Value, ts: u64, metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool }
fn digest(s: &str) -> String { format!("{:x}", Sha256::digest(s.as_bytes())) }
fn rollout(path: &str) -> bool {
    // Both separators matter when validating Windows payload fixtures on Unix.
    path.rsplit(['/', '\\']).next().is_some_and(|p| p.starts_with("rollout-") && p.ends_with(".jsonl"))
}
fn native(v: &Value) -> bool {
    text(v, "session_id").is_some() && text(v, "cwd").is_some()
        && v["hook_event_name"].as_str().is_some_and(|e| EVENTS.contains(&e))
        && v["transcript_path"].as_str().is_some_and(rollout)
        && (matches!(v["hook_event_name"].as_str(), Some("SessionStart" | "SessionEnd")) || text(v, "turn_id").is_some())
}
fn hook(ctx: &mut HookCtx, stdin: &[u8]) -> Result<()> {
    let mut v: Value = serde_json::from_slice(stdin)?;
    if !native(&v) { return Ok(()); }
    for field in ["prompt", "tool_input", "tool_response", "last_assistant_message"] {
        if let Some(value) = v.get_mut(field) { let mut content=Some(value.take());ctx.scrub_content(&mut content)?;*value=content.unwrap_or(Value::Null); }
    }
    let session = v["session_id"].as_str().unwrap(); let hash = digest(session);
    let prefix = format!("codex-pending-{hash}-");
    let pending = format!("{prefix}{}-{}.json", ctx.now_ns(), crate::record::random_id()?);
    ctx.save(&pending, &Pending { payload: v, ts: ctx.now_ns(), metadata: ctx.metadata.clone(), run_id: ctx.run_id.clone(), proxy_active: ctx.proxy_active })?;
    ctx.mark_settle_pending()?;
    drain(ctx, &hash, false)?;
    Ok(())
}
fn drain(ctx: &mut HookCtx, hash: &str, blocking: bool) -> Result<bool> {
    let name = format!("codex-{hash}");
    let lock = if blocking { Some(ctx.lock(&name)?) } else { ctx.try_lock(&name)? };
    let Some(_lock) = lock else { return Ok(true); };
    let prefix = format!("codex-pending-{hash}-");
    let key = format!("codex-{hash}.json"); let mut state: State = ctx.load(&key)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(ctx.state_dir())?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix))).collect(); files.sort();
    let mut waiting = false;
    for file in files {
        let pending: Pending = serde_json::from_slice(&std::fs::read(&file)?)?;
        let old_metadata = std::mem::replace(&mut ctx.metadata, pending.metadata.clone());
        let old_run = std::mem::replace(&mut ctx.run_id, pending.run_id.clone());
        let old_proxy = ctx.proxy_active; ctx.proxy_active = pending.proxy_active;
        let result = process(ctx, &mut state, pending);
        ctx.metadata = old_metadata; ctx.run_id = old_run; ctx.proxy_active = old_proxy;
        let pending = result?;
        for stream in state.streams.values_mut() { stream.scrub_content(ctx)?; }
        ctx.save(&key, &state)?;
        if pending { waiting = true; } else { std::fs::remove_file(file)?; }
    }
    Ok(waiting)
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let mut hashes = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(ctx.state_dir())? {
        let entry = entry?;
        if !entry.path().extension().is_some_and(|e| e == "json") { continue; }
        let name = entry.file_name(); let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("codex-pending-") {
            if let Some((hash, _)) = rest.split_once('-') { hashes.insert(hash.to_owned()); }
        }
    }
    let mut waiting = false;
    for hash in hashes { waiting |= drain(ctx, &hash, true)?; }
    Ok(waiting)
}

fn tail(state: &mut State, path: &str, child: Option<(&str, &str)>, now: u64, session: &str) -> Result<(Vec<Event>, Option<String>)> {
    let hash = digest(path); let stream = state.streams.entry(hash).or_default(); let mut cursor = stream.cursor.clone();
    let lines = match jsonl::read_new(Path::new(path), &mut cursor) {
        Ok(lines) => lines,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => return Ok((vec![], None)),
        Err(e) => return Err(e),
    };
    if stream.cursor.offset == 0 || cursor.offset < stream.cursor.offset || cursor.identity != stream.cursor.identity {
        *stream = Stream::default();
        stream.cursor = cursor.clone();
        let meta = lines.iter().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find(|v| v["type"] == "session_meta");
        let Some(meta) = meta else { return Ok((vec![], None)); };
        let expected = child.map_or(session, |(id, _)| id);
        if meta["payload"]["id"].as_str() != Some(expected) { return Ok((vec![], None)); }
    }
    let events = stream.parse(&lines, child, now); stream.cursor = cursor;
    Ok((events, stream.version.clone()))
}
fn respan_provider(path: &Path, provider: Option<&str>) -> bool {
    let Some(home) = path.ancestors().find(|p| p.file_name().is_some_and(|n| n == "sessions" || n == "archived_sessions")).and_then(Path::parent) else { return false; };
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else { return false; };
    let Ok(config) = text.parse::<toml::Value>() else { return false; };
    let provider = provider.or_else(|| config.get("model_provider").and_then(toml::Value::as_str)).unwrap_or("openai");
    config.get("openai_base_url").filter(|_| provider == "openai").and_then(toml::Value::as_str)
        .or_else(|| config.get("model_providers")?.get(provider)?.get("base_url")?.as_str())
        .is_some_and(|url| url.contains("api.respan.ai"))
}
fn process(ctx: &mut HookCtx, state: &mut State, pending: Pending) -> Result<bool> {
    let v = pending.payload; let now = pending.ts;
    let session = v["session_id"].as_str().unwrap(); let path = v["transcript_path"].as_str().unwrap();
    let event = v["hook_event_name"].as_str().unwrap(); let turn = text(&v, "turn_id").unwrap_or_default();
    let agent = text(&v, "agent_id");
    let normal_child = if !matches!(event, "SubagentStart" | "SubagentStop") { agent.as_deref().and_then(|id| state.children.get(id).map(|c| (id.to_owned(), c.turn.clone()))) } else { None };
    let (mut events, version) = tail(state, path, normal_child.as_ref().map(|(id, turn)| (id.as_str(), turn.as_str())), now, session)?;
    // Reject imported Claude/Grok hooks using a real, non-Codex transcript.
    if !state.streams.get(&digest(path)).is_some_and(|s| !s.thread.is_empty()) { return Ok(!Path::new(path).exists() || state.streams.get(&digest(path)).is_some_and(|s| s.cursor.offset == 0)); }
    let provider = state.streams.get(&digest(path)).and_then(|s| s.provider.as_deref());
    ctx.proxy_active |= respan_provider(Path::new(path), provider);
    let parent = normal_child.as_ref().map(|(id, _)| id.clone());
    let effective_turn = normal_child.as_ref().map_or_else(|| turn.clone(), |(_, turn)| turn.clone());
    let mut waiting = false;
    match event {
        "UserPromptSubmit" if parent.is_none() => {
            if let Some(Event::TurnStart { prompt, .. }) = events.iter_mut().find(|e| matches!(e, Event::TurnStart { turn_id, .. } if turn_id == &turn)) {
                *prompt = v.get("prompt").cloned();
            } else {
                events.insert(0, Event::TurnStart { metadata: BTreeMap::new(), turn_id: turn.clone(), ts_ns: now, prompt: v.get("prompt").cloned() });
            }
        },
        "PostToolUse" => {
            if let Some(id) = text(&v, "tool_use_id") {
                events.push(Event::Tool { metadata: BTreeMap::new(), turn_id: effective_turn.clone(), id, parent_id: parent.clone(), name: text(&v, "tool_name"), start_ns: now, end_ns: now, input: v.get("tool_input").cloned(), output: v.get("tool_response").cloned(), status: Some("ok".into()), error: None });
            }
        }
        "SubagentStart" | "SubagentStop" => {
            if let Some(id) = agent {
                let stream = state.streams.get(&digest(path)).unwrap();
                let parent = stream.spawn_parent(&id);
                let relation = state.children.entry(id.clone()).or_insert_with(|| Child { turn: turn.clone(), parent });
                let root_turn = relation.turn.clone(); let spawn = relation.parent.clone();
                events.push(Event::SubagentStart { metadata: BTreeMap::new(), turn_id: root_turn.clone(), id: id.clone(), parent_id: spawn, ts_ns: now, name: text(&v, "agent_type") });
                if event == "SubagentStop" {
                    if let Some(child_path) = v["agent_transcript_path"].as_str().filter(|p| rollout(p)) {
                        let (child_events, _) = tail(state, child_path, Some((&id, &root_turn)), now, session)?;
                        events.extend(child_events);
                        waiting = !state.streams.get(&digest(child_path)).is_some_and(|s| !s.completed.is_empty());
                    }
                    if !waiting { events.push(Event::SubagentEnd { turn_id: root_turn, id, ts_ns: now, status: Some("ok".into()), output: v.get("last_assistant_message").cloned() }); }
                }
            }
        }
        "Stop" | "Interrupt" => {
            waiting = !state.streams.get(&digest(path)).is_some_and(|s| s.completed.contains(&turn));
            if !waiting { if let Some(stream) = state.streams.get_mut(&digest(path)) { stream.finish_legacy(&turn, &effective_turn, parent.as_deref(), &mut events); } }
            if !waiting && parent.is_none() { events.push(Event::TurnEnd { turn_id: turn, ts_ns: now, output: v.get("last_assistant_message").cloned(), status: Some(if event == "Interrupt" { "error" } else { "ok" }.into()), error: if event == "Interrupt" { Some("interrupted".into()) } else { None } }); }
        }
        _ => {}
    }
    if !events.is_empty() {
        bridge::ingest(ctx, Batch { v: 1, agent: "codex".into(), session_id: session.into(), session_path: Some(path.into()), cwd: text(&v, "cwd"), agent_version: version, events })?;
    }
    Ok(waiting)
}
