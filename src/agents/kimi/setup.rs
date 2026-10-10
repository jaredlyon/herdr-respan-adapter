use crate::{agents::common::hookcmd::{self, Shell}, setup::{Edit, Plan, SetupCtx}, Result};
use std::{io::ErrorKind, process::Command};

pub(super) const EVENTS: &[&str] = &[
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse",
    "PostToolUseFailure", "Stop", "StopFailure",
];

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let output = Command::new("kimi").arg("--version").output().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            "Kimi Code CLI 2.x is required; put its kimi binary on PATH (typically ~/.local/share/kimi-code/bin). Legacy kimi-cli is not supported.".into()
        } else { Box::new(error) as Box<dyn std::error::Error + Send + Sync> }
    })?;
    let version = String::from_utf8(output.stdout)?;
    if !output.status.success() {
        return Err("kimi --version failed; Kimi Code CLI 2.x is required, not legacy kimi-cli".into());
    }
    check_version(&version)?;
    make_plan(ctx, format!("Kimi Code CLI detected: {}", version.trim()))
}

fn check_version(text: &str) -> Result<()> {
    let version = text.split_whitespace().find_map(|word| {
        let mut parts = word.trim_start_matches('v').split('.');
        let major = parts.next()?.parse::<u64>().ok()?;
        parts.next()?.parse::<u64>().ok()?;
        parts.next()?.split(['-', '+']).next()?.parse::<u64>().ok()?;
        if parts.next().is_some() { return None; }
        Some(major)
    });
    if version != Some(2) {
        return Err(format!("Kimi Code CLI 2.x is required; detected {}. Legacy MoonshotAI kimi-cli is not supported; install Kimi Code and put its kimi binary on PATH.", text.trim()).into());
    }
    Ok(())
}

pub(super) fn make_plan(ctx: &SetupCtx, notice: String) -> Result<Plan> {
    let command = hookcmd::command(ctx, "kimi", if cfg!(windows) { Shell::Cmd } else { Shell::Posix });
    let file = crate::paths::absolute(&ctx.agent_home("KIMI_CODE_HOME", ".kimi-code"))?.join("config.toml");
    let mut edits = Vec::new();
    for event in EVENTS {
        let mut hooks = toml_edit::ArrayOfTables::new();
        let mut hook = toml_edit::Table::new();
        hook.insert("event", toml_edit::value(*event));
        hook.insert("command", toml_edit::value(command.clone()));
        hook.insert("timeout", toml_edit::value(10));
        hooks.push(hook);
        edits.push(Edit::TomlInsert { file: file.clone(), path: vec!["hooks".into()],
            value: toml_edit::Item::ArrayOfTables(hooks), id: format!("herdr-respan-kimi-{event}") });
    }
    let mut notices = vec![notice,
        "Kimi Code: model-step usage comes from sessions/<workDirKey>/<sessionId>/agents/main/wire.jsonl. Legacy kimi-cli configs and KIMI_SHARE_DIR are not used.".into()];
    if ctx.project.is_some() {
        notices.push("Kimi Code hooks are user-global; --project does not select a different Kimi config.".into());
    }
    Ok(Plan { edits, notices, offers: vec![] })
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn identifies_successor_and_refuses_legacy() {
        for version in ["2.1.1", "Kimi Code CLI 2.0.0", "v2.2.0"] { check_version(version).unwrap(); }
        for version in ["kimi, version 1.28.0", "1.52.0", "unknown", "kimi-cli is no longer maintained. Please use the new Kimi Code CLI."] {
            assert!(check_version(version).unwrap_err().to_string().contains("Legacy"), "{version}");
        }
    }
}
