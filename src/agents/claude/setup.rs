use crate::{agents::common::{claude_style, hookcmd::{self, Shell}}, setup::{Edit, Plan, SetupCtx}, Result};
use std::path::PathBuf;

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let file = match &ctx.project {
        Some(project) => project.join(".claude/settings.local.json"),
        None => config_home(ctx).join("settings.json"),
    };
    let shell = if cfg!(windows) { Shell::PowerShell } else { Shell::Posix };
    let command = hookcmd::command(ctx, "claude", shell);
    let mut plan = Plan::default();
    for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "SubagentStart", "SubagentStop", "Stop", "StopFailure", "SessionEnd"] {
        let matcher = (event == "SessionStart").then_some("^(startup|resume|clear|compact|fork)$");
        let mut value = claude_style::hook_group(matcher, &command, Some(10));
        if cfg!(windows) { value["hooks"][0]["shell"] = "powershell".into(); }
        plan.edits.push(Edit::JsonInsert { file: file.clone(), pointer: format!("/hooks/{event}"), value, id: format!("herdr-respan-claude-{event}") });
    }
    plan.notices.push("Claude hooks stay silent. Transcript-derived usage may arrive after Stop; no credentials or telemetry settings are changed.".into());
    Ok(plan)
}

fn config_home(ctx: &SetupCtx) -> PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|s| !s.trim().is_empty()) {
        Some(path) if path == "~" => ctx.home.clone(),
        Some(path) if path.starts_with("~/") || path.starts_with("~\\") => ctx.home.join(&path[2..]),
        Some(path) => PathBuf::from(path),
        None => ctx.home.join(".claude"),
    }
}
