//! Cursor's observational hooks, translated into the shared assembler.
mod setup;
#[cfg(test)]
mod tests;

use crate::{agents::{common::bridge, AgentSpec}, record::HookCtx, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, path::PathBuf};

pub const EVENTS: &[&str] = &["postToolUse", "postToolUseFailure", "afterAgentResponse", "stop"];
pub const SPEC: AgentSpec = AgentSpec {
    kind: "cursor", display: "Cursor Agent", hook, plan: setup::plan, reply: Some(reply), settle: Some(settle),
};

fn reply(stdin: &[u8]) -> String {
    if serde_json::from_slice::<Value>(stdin).ok().is_some_and(|p|
        p.get("hook_event_name").and_then(Value::as_str) == Some("beforeSubmitPrompt")) {
        "{\"continue\":true}".into()
    } else {
        "{}".into()
    }
}

fn text<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn identify(payload: &Value, grok_env: bool) -> bool {
    !grok_env && !(payload.get("hookEventName").is_some() && payload.get("workspaceRoot").is_some())
        && ["conversation_id", "generation_id", "cursor_version", "hook_event_name"]
            .iter().all(|field| text(payload, field).is_some())
        && EVENTS.contains(&text(payload, "hook_event_name").unwrap_or_default())
}

#[derive(Default, Serialize, Deserialize)]
struct Response { output: Option<Value> }

fn response_key(payload: &Value) -> String {
    // Hash native ids only to keep state filenames safe; span keys belong to Core.
    let ids = json!([payload["conversation_id"], payload["generation_id"]]).to_string();
    format!("response-{:x}.json", Sha256::digest(ids.as_bytes()))
}

fn hook(ctx: &mut HookCtx, stdin: &[u8]) -> Result<()> {
    let Ok(payload) = serde_json::from_slice::<Value>(stdin) else { return Ok(()); };
    let grok = ["GROK_HOOK_EVENT", "GROK_SESSION_ID", "GROK_HOOK_NAME"]
        .iter().any(|key| std::env::var_os(key).is_some());
    if !identify(&payload, grok) { return Ok(()); }
    if text(&payload, "hook_event_name") == Some("afterAgentResponse") && text(&payload, "text").is_none() {return Ok(());}
    let key = response_key(&payload);
    let now = ctx.now_ns();
    let mut payload = payload;
    payload.as_object_mut().expect("validated payload").retain(|name, _| [
        "conversation_id", "generation_id", "cursor_version", "hook_event_name", "duration",
        "transcript_path", "model", "model_id", "cwd", "workspace_roots", "tool_use_id", "tool_name",
        "tool_input", "tool_output", "error_message", "text", "status", "input_tokens", "output_tokens",
        "cache_read_tokens", "cache_write_tokens"
    ].contains(&name.as_str()));
    for name in ["tool_input", "tool_output", "error_message", "text"] {
        if let Some(field) = payload.get_mut(name) {
            let mut content = Some(std::mem::take(field));
            ctx.scrub_content(&mut content)?;
            *field = content.unwrap_or(Value::Null);
        }
    }
    let name = format!("queue-{key}-{now:020}-{}.json", crate::record::random_id()?);
    ctx.save(&name, &Queued {payload, now, metadata:ctx.metadata.clone(), run_id:ctx.run_id.clone(), proxy_active:ctx.proxy_active})?;
    ctx.mark_settle_pending()?;
    let Some(_lock) = ctx.try_lock(&key)? else {return Ok(());};
    drain(ctx, &key)
}

#[derive(Default, Serialize, Deserialize)]
struct Queued {
    payload: Value, now: u64, metadata: BTreeMap<String,Value>, run_id: Option<String>, proxy_active: bool,
}
fn queued(ctx: &HookCtx, key: Option<&str>) -> Result<Vec<PathBuf>> {
    let prefix = key.map_or_else(|| "queue-".into(), |key|format!("queue-{key}-"));
    let mut files = Vec::new();
    for entry in std::fs::read_dir(ctx.state_dir())? {
        let path = entry?.path();
        if path.file_name().is_some_and(|n|n.to_string_lossy().starts_with(&prefix)) && path.extension().is_some_and(|e|e=="json") {files.push(path);}
    }
    files.sort();
    Ok(files)
}
fn drain(ctx: &mut HookCtx, key: &str) -> Result<()> {
    for file in queued(ctx, Some(key))? {
        let queued: Queued = ctx.load(file.file_name().and_then(|n|n.to_str()).ok_or("invalid queue filename")?)?;
        let metadata = std::mem::replace(&mut ctx.metadata, queued.metadata);
        let run_id = std::mem::replace(&mut ctx.run_id, queued.run_id);
        let proxy = std::mem::replace(&mut ctx.proxy_active, queued.proxy_active);
        let result = process(ctx, &queued.payload, queued.now);
        ctx.metadata=metadata;ctx.run_id=run_id;ctx.proxy_active=proxy;
        result?;
        std::fs::remove_file(file)?;
    }
    Ok(())
}
fn settle(ctx: &mut HookCtx) -> Result<bool> {
    let mut keys = BTreeSet::new();
    for file in queued(ctx, None)? {
        let queued: Queued = ctx.load(file.file_name().and_then(|n|n.to_str()).ok_or("invalid queue filename")?)?;
        keys.insert(response_key(&queued.payload));
    }
    for key in keys {
        let _lock = ctx.lock(&key)?;
        drain(ctx, &key)?;
    }
    Ok(!queued(ctx, None)?.is_empty())
}
fn process(ctx: &mut HookCtx, payload: &Value, now: u64) -> Result<()> {
    let key = response_key(payload);
    let event = text(&payload, "hook_event_name").unwrap_or_default();
    if event == "afterAgentResponse" {
        let mut response = Response { output: payload.get("text").filter(|v| !v.is_null()).cloned() };
        ctx.scrub_content(&mut response.output)?;
        ctx.save(&key, &response)?;
    }
    let response: Response = if event == "stop" { ctx.load(&key)? } else { Response::default() };
    if let Some(batch) = translate(payload, now, response.output)? {
        bridge::ingest(ctx, batch)?;
        if event == "stop" {
            match std::fs::remove_file(ctx.state_dir().join(key)) {
                Ok(()) => {},
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn translate(payload: &Value, now: u64, output: Option<Value>) -> Result<Option<bridge::Batch>> {
    if !identify(payload, false) { return Ok(None); }
    let turn = &payload["generation_id"];
    let event = text(payload, "hook_event_name").unwrap_or_default();
    let start = now.saturating_sub(payload.get("duration").and_then(Value::as_u64)
        .unwrap_or(0).saturating_mul(1_000_000));
    let mut metadata = serde_json::Map::new();
    if let Some(path) = text(payload, "transcript_path") { metadata.insert("session_path".into(), json!(path)); }
    if let Some(slug) = text(payload, "model") { metadata.insert("cursor.model_slug".into(), json!(slug)); }
    let mut events = vec![json!({"type":"turn_start", "turn_id":turn, "ts_ns":start, "metadata":metadata})];
    match event {
        "postToolUse" | "postToolUseFailure" => {
            let (Some(id), Some(name)) = (text(payload, "tool_use_id"), text(payload, "tool_name")) else { return Ok(None); };
            let failed = event == "postToolUseFailure";
            events.push(json!({
                "type":"tool", "turn_id":turn, "id":id, "name":name,
                "start_ns":start, "end_ns":now, "input":payload.get("tool_input"),
                "output":payload.get("tool_output"), "status":if failed {"error"} else {"ok"},
                "error":payload.get("error_message"), "metadata":metadata
            }));
        },
        "afterAgentResponse" => {
            if payload.get("text").is_none() { return Ok(None); }
        },
        "stop" => {
            let Some(status) = text(payload, "status").filter(|s| ["completed", "aborted", "error"].contains(s)) else { return Ok(None); };
            let status = if status == "completed" { "ok" } else { "error" };
            let model = text(payload, "model_id").or_else(|| text(payload, "model"))
                .filter(|s| !s.eq_ignore_ascii_case("auto") && !s.eq_ignore_ascii_case("default"));
            metadata.insert("cursor.usage_scope".into(), json!("generation"));
            let count = |name| payload.get(name).and_then(Value::as_u64);
            // These are observed generation totals, NOT inferred provider-request usage.
            events.push(json!({
                "type":"chat", "turn_id":turn, "id":turn, "start_ns":now, "end_ns":now,
                "model":model, "output":output, "status":status, "error":payload.get("error_message"),
                "usage":{
                    "input_tokens":count("input_tokens"), "output_tokens":count("output_tokens"),
                    "cache_read_input_tokens":count("cache_read_tokens"),
                    "cache_creation_input_tokens":count("cache_write_tokens")
                }, "metadata":metadata
            }));
            events.push(json!({"type":"turn_end", "turn_id":turn, "ts_ns":now,
                "output":output, "status":status, "error":payload.get("error_message")}));
        },
        _ => return Ok(None),
    }
    Ok(Some(serde_json::from_value(json!({
        "v":1, "agent":"cursor", "session_id":payload["conversation_id"],
        "session_path":payload.get("transcript_path"),
        "cwd":text(payload,"cwd").or_else(|| payload.get("workspace_roots")?.as_array()?.first()?.as_str()),
        "agent_version":payload["cursor_version"], "events":events
    }))?))
}
