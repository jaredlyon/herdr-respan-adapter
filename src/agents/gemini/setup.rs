use crate::{agents::common::{claude_style, hookcmd::{self, Shell}}, setup::{Edit, Plan, SetupCtx}, Result};
use std::path::PathBuf;

pub(super) const EVENTS: [&str; 8] = ["SessionStart", "BeforeAgent", "BeforeModel", "AfterModel", "BeforeTool", "AfterTool", "AfterAgent", "SessionEnd"];

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let root = ctx.project.clone().unwrap_or_else(|| std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from).unwrap_or_else(|| ctx.home.clone()));
    let file = root.join(".gemini/settings.json");
    let shell = if cfg!(windows) { Shell::PowerShell } else { Shell::Posix };
    let command = hookcmd::command(ctx, "gemini", shell);
    let edits = EVENTS.iter().map(|event| {
        let mut group = claude_style::hook_group(Some("*"), &command, Some(5000));
        group["hooks"][0]["name"] = serde_json::json!("herdr-respan");
        group["hooks"][0]["env"] = serde_json::json!({"HERDR_RESPAN_ADAPTER":"gemini"});
        Edit::JsonInsert { file: file.clone(), pointer: format!("/hooks/{event}"), value: group, id: format!("herdr-respan-gemini-{event}") }
    }).collect();
    Ok(Plan { edits, notices: vec![
        "Gemini CLI hooks require v0.26.0 or newer; current JSONL usage support is based on the pinned current source, not guaranteed on that oldest release.".into(),
        "Gemini hook timeout is milliseconds. Existing telemetry and hooksConfig controls remain untouched; disabled hooks must be enabled explicitly by the user. Project hook commands may require Gemini's trust approval.".into(),
    ], offers: Vec::new() })
}
