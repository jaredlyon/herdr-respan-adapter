use crate::{agents::common::{claude_style, hookcmd::{self, Shell}}, setup::{Edit, Plan, SetupCtx}, Result};

pub(super) const EVENTS: &[&str] = &[
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure",
    "Stop", "StopFailure", "StopCancelled", "SubagentStart", "SubagentStop", "SessionEnd",
];

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let home = ctx.project.as_ref().map(|p| p.join(".grok"))
        .unwrap_or_else(|| ctx.agent_home("GROK_HOME", ".grok"));
    let home = crate::paths::absolute(&home)?;
    let shell = if cfg!(windows) {
        match std::env::var("GROK_SHELL").unwrap_or_default().to_ascii_lowercase().as_str() {
            "cmd" | "cmd.exe" => Shell::Cmd,
            "bash" | "gitbash" | "git-bash" => Shell::Posix,
            _ => Shell::PowerShell,
        }
    } else { Shell::Posix };
    let command = hookcmd::command(ctx, "grok", shell);
    Ok(Plan {
        edits: EVENTS.iter().map(|event| Edit::JsonInsert {
            file: home.join("hooks/herdr-respan.json"), pointer: format!("/hooks/{event}"),
            value: claude_style::hook_group(None, &command, Some(5)),
            id: format!("herdr-respan-grok-{event}"),
        }).collect(),
        notices: vec![
            "Grok hooks/transcript capture turns, chat without usage, tools and subagents. Use `herdr-respan run grok` for request usage via the opt-in proxy.".into(),
            "Project Grok hooks require folder trust; managed policy can disable hooks. Windows uses Grok's default PowerShell shell, or the GROK_SHELL override present during setup; remove grok and setup grok again after changing that override.".into(),
        ], ..Default::default()
    })
}
