use super::EVENTS;
use crate::{agents::common::hookcmd::{self, Shell}, setup::{Edit, Plan, SetupCtx}, Result};
use serde_json::{json, Value};

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let root = ctx.project.as_ref().map(|project| project.join(".cursor"))
        .unwrap_or_else(|| ctx.agent_home("CURSOR_CONFIG_DIR", ".cursor"));
    let file = crate::paths::absolute(&root)?.join("hooks.json");
    match std::fs::read(&file) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)?;
            if value.get("version").is_some_and(|v| v != &json!(1)) {
                return Err("Cursor hooks require version 1; existing version is not replaced".into());
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e.into()),
    }
    let command = hookcmd::command(ctx, "cursor", if cfg!(windows) { Shell::Cmd } else { Shell::Posix });
    let mut edits = vec![Edit::JsonSetIfAbsent {
        file: file.clone(), pointer: "/version".into(), value: json!(1), id: "herdr-respan-cursor-version".into(),
    }];
    for event in EVENTS {
        edits.push(Edit::JsonInsert {
            file: file.clone(), pointer: format!("/hooks/{event}"), value: json!({"command":command}),
            id: format!("herdr-respan-cursor-{event}"),
        });
    }
    Ok(Plan { edits, notices: vec![
        "Cursor: observational hooks only; no permission or prompt-validation hooks are installed. CLI event support varies by version. Stop counters, when present, are generation totals, not provider-request usage. Native Windows command quoting requires installed-CLI verification.".into(),
    ], ..Default::default() })
}
