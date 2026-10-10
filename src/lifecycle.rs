use crate::{config::Config, paths::Paths, record::{HookCtx, SpanRecord}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[derive(Serialize, Deserialize)]
struct Run { agent: String, start_ns: u64, key: String, metadata: BTreeMap<String, Value> }
/// `event` is herdr's HERDR_PLUGIN_EVENT_JSON envelope: `{"event":"pane_agent_status_changed","data":{"type":...,"pane_id":...}}`.
/// The envelope uses snake_case names while manifest `on`/HERDR_PLUGIN_EVENT use dotted names; accept both.
pub fn handle(paths: &Paths, config: Config, event: &Value) -> Result<()> {
    let fields = event.get("data").unwrap_or(event);
    let pane = fields.get("pane_id").and_then(Value::as_str).ok_or("missing pane id")?;
    let workspace = fields.get("workspace_id").and_then(Value::as_str);
    if !crate::gate::allowed(&paths.state, std::env::var("HERDR_ENV").ok().as_deref(), std::env::var("HERDR_RESPAN").ok().as_deref(), workspace) { return Ok(()); }
    let dir = paths.state.join("lifecycle");
    let file = dir.join(format!("{:x}.json", Sha256::digest(pane.as_bytes())));
    let previous = std::fs::read(&file).ok().map(|b| serde_json::from_slice::<Run>(&b)).transpose()?;
    let kind = fields.get("agent").and_then(|v| v.as_str().or_else(|| v.get("kind").and_then(Value::as_str)))
        .or_else(|| previous.as_ref().map(|r| r.agent.as_str())).unwrap_or("unknown");
    if crate::setup::installed(&paths.state, kind) { let _ = std::fs::remove_file(file); return Ok(()); }
    let event_name = [event.get("event"), fields.get("type")].into_iter().flatten().filter_map(Value::as_str).next().unwrap_or_default();
    let status = if matches!(event_name, "pane.exited" | "pane_exited") { "exited" } else { fields.get("agent_status").and_then(Value::as_str).unwrap_or_default() };
    if status == "working" {
        if previous.is_none() {
            let now = crate::record::now_ns();
            // No cwd/git here: event hooks run in the plugin root, not the agent's pane.
            let metadata = BTreeMap::from([("herdr.pane_id".into(), json!(pane)), ("herdr.workspace_id".into(), json!(workspace.unwrap_or_default())),
                ("herdr.machine".into(), json!(crate::meta::hostname())), ("plugin_version".into(), json!(env!("CARGO_PKG_VERSION")))]);
            crate::paths::atomic_json(&file, &Run { agent: kind.into(), start_ns: now, key: format!("lifecycle:{pane}:{now}"), metadata })?;
        }
    } else if ["idle", "done", "blocked", "exited"].contains(&status) {
        if let Some(run) = previous {
            let mut ctx = HookCtx::new(paths.clone(), config, &run.agent, run.metadata, None, false)?;
            let rec = SpanRecord { trace_key: Some(run.key.clone()), span_key: run.key, name: format!("{} run", run.agent), source: "lifecycle".into(),
                start_ns: run.start_ns, end_ns: ctx.now_ns(), metadata: BTreeMap::from([("final_status".into(), json!(status))]), ..Default::default() };
            ctx.emit(rec)?; ctx.finish()?; std::fs::remove_file(file)?; if !cfg!(test) { crate::spool::spawn_flush(paths)?; }
        }
    }
    Ok(())
}
