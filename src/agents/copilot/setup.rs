use crate::{agents::common::hookcmd::{self, Shell}, setup::{Edit, Offer, Plan, SetupCtx}, Result};
use serde_json::json;
use std::path::PathBuf;

const EVENTS: &[&str] = &["sessionStart", "userPromptSubmitted", "postToolUse", "postToolUseFailure", "agentStop", "sessionEnd"];
const MARKER: &str = "herdr-respan copilot-otel-profile";
type Transform = fn(&str, &str) -> Result<String>;

pub(super) fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let file = ctx.project.as_ref().map(|p| p.join(".github/copilot/settings.local.json")).unwrap_or_else(|| ctx.agent_home("COPILOT_HOME", ".copilot").join("settings.json"));
    let mut plan = Plan::default();
    for event in EVENTS {
        plan.edits.push(Edit::JsonInsert {
            file: file.clone(), pointer: format!("/hooks/{event}"),
            value: json!({"type":"command", "bash":hookcmd::command(ctx, "copilot", Shell::Posix), "powershell":hookcmd::command(ctx, "copilot", Shell::PowerShell), "timeoutSec":10, "env":{"HERDR_RESPAN_COPILOT_EVENT":event}}),
            id: format!("herdr-respan-copilot-{event}"),
        });
    }
    if file.is_file() && serde_json::from_slice::<serde_json::Value>(&std::fs::read(&file)?).is_err() {
        let mut hooks = serde_json::Map::new();
        for edit in &plan.edits { if let Edit::JsonInsert { pointer, value, .. } = edit { hooks.insert(pointer.trim_start_matches("/hooks/").into(), json!([value])); } }
        return Err(format!("Copilot settings at {} are not strict JSON (JSONC comments/trailing commas are not automatically merged). No files changed. Merge these exact entries by hand into existing event arrays, without replacing herdr hooks:\n{}", file.display(), serde_json::to_string_pretty(&json!({"hooks":hooks}))?).into());
    }
    let exporter = ctx.state_dir.join("agents/copilot/otel.jsonl").to_string_lossy().into_owned();
    let mut edits = Vec::new();
    for (file, shell) in profiles(ctx) {
        if !file.is_file() { continue; }
        let (apply, revert): (Transform, Transform) = match shell {
            "fish" => (apply_fish, revert_fish), "pwsh" => (apply_pwsh, revert_pwsh), _ => (apply_posix, revert_posix),
        };
        edits.push(Edit::TextArg { file, id: format!("herdr-respan-copilot-profile-{shell}"), arg: exporter.clone(), apply, revert });
    }
    plan.notices.push("Copilot hooks expose turns/tools/chat but no request tokens. Usage requires COPILOT_OTEL_FILE_EXPORTER_PATH at launch; run copilot enables the metadata-only exporter. The optional copilot-otel-profile offer edits existing shell profiles only; restart the shell in herdr after accepting. Message-content capture is never enabled.".into());
    if edits.is_empty() { plan.notices.push(format!("No existing shell profile found. To enable usage manually, set COPILOT_OTEL_FILE_EXPORTER_PATH to {exporter} only when HERDR_ENV=1, or use run copilot.")); }
    plan.offers.push(Offer { id: "copilot-otel-profile", prompt: "Add a herdr-only Copilot metadata exporter line to existing bash/zsh/fish/PowerShell profiles?".into(), edits });
    Ok(plan)
}
fn profiles(ctx: &SetupCtx) -> Vec<(PathBuf, &'static str)> {
    let mut paths = Vec::new();
    for name in [".bashrc", ".bash_profile", ".bash_login", ".profile"] { paths.push((ctx.home.join(name), "posix")); }
    let zsh = std::env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or_else(|| ctx.home.clone());
    for name in [".zshrc", ".zprofile", ".zshenv"] { paths.push((zsh.join(name), "posix")); }
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| ctx.home.join(".config"));
    paths.push((config.join("fish/config.fish"), "fish"));
    for dir in [config.join("powershell"), ctx.home.join("Documents/PowerShell"), ctx.home.join("Documents/WindowsPowerShell")] {
        for name in ["profile.ps1", "Microsoft.PowerShell_profile.ps1"] { paths.push((dir.join(name), "pwsh")); }
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0)); paths.dedup_by(|a, b| a.0 == b.0); paths
}
fn line(path: &str, shell: &str) -> Result<String> {
    if path.contains(['\n', '\r', '\0']) { return Err("invalid exporter path".into()); }
    let quote = match shell {
        "pwsh" => path.replace('\'', "''"),
        "fish" => path.replace('\\', "\\\\").replace('\'', "\\'"),
        _ => path.replace('\'', "'\\''"),
    };
    Ok(match shell {
        "fish" => format!("if test \"$HERDR_ENV\" = 1; set -gx COPILOT_OTEL_FILE_EXPORTER_PATH '{quote}'; end"),
        "pwsh" => format!("if ($env:HERDR_ENV -eq '1') {{ $env:COPILOT_OTEL_FILE_EXPORTER_PATH = '{quote}' }}"),
        _ => format!("if [ \"${{HERDR_ENV:-}}\" = 1 ]; then export COPILOT_OTEL_FILE_EXPORTER_PATH='{quote}'; fi"),
    })
}
fn block(path: &str, shell: &str, newline: &str, separator: bool) -> Result<String> {
    Ok(format!("{}# >>> {MARKER} separator={}{}{}{}# <<< {MARKER}{}", if separator { newline } else { "" }, u8::from(separator), newline, line(path, shell)?, newline, newline))
}
fn apply(text: &str, path: &str, shell: &str) -> Result<String> {
    if text.contains(MARKER) {
        for nl in ["\n", "\r\n"] { for separator in [false, true] { if text.contains(&block(path, shell, nl, separator)?) { return Ok(text.into()); } } }
        return Err("Copilot profile marker exists with different contents; refusing overwrite".into());
    }
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    Ok(format!("{text}{}", block(path, shell, nl, !text.is_empty() && !text.ends_with('\n'))?))
}
fn revert(text: &str, path: &str, shell: &str) -> Result<String> {
    for nl in ["\r\n", "\n"] { for separator in [true, false] {
        let owned = block(path, shell, nl, separator)?;
        if let Some(index) = text.find(&owned) {
            let mut result = text.to_owned(); result.replace_range(index..index + owned.len(), ""); return Ok(result);
        }
    } }
    if text.contains(MARKER) { return Err("Copilot profile line changed externally; refusing removal".into()); }
    Ok(text.into())
}
fn apply_posix(text: &str, arg: &str) -> Result<String> { apply(text, arg, "posix") }
fn revert_posix(text: &str, arg: &str) -> Result<String> { revert(text, arg, "posix") }
fn apply_fish(text: &str, arg: &str) -> Result<String> { apply(text, arg, "fish") }
fn revert_fish(text: &str, arg: &str) -> Result<String> { revert(text, arg, "fish") }
fn apply_pwsh(text: &str, arg: &str) -> Result<String> { apply(text, arg, "pwsh") }
fn revert_pwsh(text: &str, arg: &str) -> Result<String> { revert(text, arg, "pwsh") }
