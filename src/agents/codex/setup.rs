use crate::{agents::common::{claude_style, hookcmd::{self, Shell}}, setup::{Edit, Plan, SetupCtx}, Result};

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let home = if let Some(project) = &ctx.project {
        project.join(".codex")
    } else {
        match std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()) {
            Some(path) => {
                let path = std::fs::canonicalize(path)?;
                if !path.is_dir() { return Err("CODEX_HOME must be a directory".into()); }
                path
            }
            None => ctx.home.join(".codex"),
        }
    };
    let command = hookcmd::command(ctx, "codex", if cfg!(windows) { Shell::Cmd } else { Shell::Posix });
    let mut edits = Vec::new();
    for event in ["SessionStart", "UserPromptSubmit", "PostToolUse", "SubagentStart", "SubagentStop", "Stop", "Interrupt", "SessionEnd"] {
        edits.push(Edit::JsonInsert {
            file: home.join("hooks.json"), pointer: format!("/hooks/{event}"),
            value: claude_style::hook_group(None, &command, Some(if event == "Interrupt" || event == "SessionEnd" { 3 } else { 10 })),
            id: format!("herdr-respan-codex-{event}"),
        });
    }
    edits.push(Edit::TomlInsert { file: home.join("config.toml"), path: vec!["features".into(), "hooks".into()], value: toml_edit::value(true), id: "herdr-respan-codex-hooks-feature".into() });
    Ok(Plan { edits, notices: vec![
        "Codex: open /hooks once and trust the herdr-respan command hooks after setup (and after changing their definition). Until the first Codex span arrives, status reports awaiting Codex trust. Never edit Codex's trust store.".into(),
        "Hooks are enabled by default in the inspected Codex source; setup ensures features.hooks=true without replacing an explicit false (remove that setting yourself to enable hooks). Project hooks also require project trust.".into(),
    ], offers: vec![] })
}
