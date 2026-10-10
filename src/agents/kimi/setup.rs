use crate::{agents::common::hookcmd::{self, Shell}, setup::{Edit, Plan, SetupCtx}, Result};
use std::{io::ErrorKind, process::Command};

pub(super) const EVENTS: &[&str] = &[
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse",
    "PostToolUseFailure", "Stop", "StopFailure", "SubagentStart", "SubagentStop",
];

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let notice = match Command::new("kimi").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8(output.stdout)?;
            check_version(&version)?;
            format!("Kimi detected: {}", version.trim())
        }
        Ok(_) => return Err("kimi --version failed; cannot verify required Kimi >= 1.28.0".into()),
        Err(error) if error.kind() == ErrorKind::NotFound =>
            "kimi is not on PATH; install MoonshotAI kimi-cli >= 1.28.0 before using these hooks".into(),
        Err(error) => return Err(error.into()),
    };
    make_plan(ctx, notice)
}

fn check_version(text: &str) -> Result<()> {
    let version = text.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let mut parts = word.split('.');
        let major = parts.next()?.parse::<u64>().ok()?;
        let minor = parts.next()?.parse::<u64>().ok()?;
        let patch_text = parts.next()?;
        let patch = patch_text.split(['-', '+']).next()?.parse::<u64>().ok()?;
        if parts.next().is_some() { return None; }
        Some(((major, minor, patch), patch_text.contains('-')))
    }).ok_or("cannot parse kimi --version; MoonshotAI kimi-cli >= 1.28.0 is required")?;
    if version.0 < (1, 28, 0) || (version.0 == (1, 28, 0) && version.1) {
        return Err(format!("MoonshotAI kimi-cli >= 1.28.0 is required for hooks; detected {}", text.trim()).into());
    }
    Ok(())
}

pub(super) fn make_plan(ctx: &SetupCtx, notice: String) -> Result<Plan> {
    let command = hookcmd::command(ctx, "kimi", if cfg!(windows) { Shell::Cmd } else { Shell::Posix });
    let file = crate::paths::absolute(&ctx.agent_home("KIMI_SHARE_DIR", ".kimi"))?.join("config.toml");
    let mut edits = Vec::new();
    for event in EVENTS {
        let mut hooks = toml_edit::ArrayOfTables::new();
        let mut hook = toml_edit::Table::new();
        hook.insert("event", toml_edit::value(*event));
        hook.insert("command", toml_edit::value(command.clone()));
        hook.insert("matcher", toml_edit::value(""));
        hook.insert("timeout", toml_edit::value(10));
        hooks.push(hook);
        edits.push(Edit::TomlInsert { file: file.clone(), path: vec!["hooks".into()],
            value: toml_edit::Item::ArrayOfTables(hooks), id: format!("herdr-respan-kimi-{event}") });
    }
    let mut notices = vec![notice,
        "Kimi: successful-step usage/subagent correlation comes from wire.jsonl; model identity stays unknown. Custom --config-file paths are not selected automatically.".into()];
    if ctx.project.is_some() {
        notices.push("Kimi hooks are user-global; --project does not select a different Kimi config.".into());
    }
    Ok(Plan { edits, notices, offers: vec![] })
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn version_gate() {
        for version in ["kimi, version 1.28.0", "1.29.1", "v2.0.0", "1.28.1-rc1"] { check_version(version).unwrap(); }
        for version in ["1.27.9", "0.14.0", "1.28.0-rc1", "unknown"] { assert!(check_version(version).is_err(), "{version}"); }
    }
}
