use crate::{agents::common::{claude_style, hookcmd::{self, Shell}}, setup::{Edit, Plan, SetupCtx}, Result};
use serde_json::json;

pub const EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "SubagentStart", "SubagentStop", "Stop", "StopFailure", "SessionEnd"];

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let directory = ctx.project.as_ref().map(|p| p.join(".qwen")).unwrap_or_else(|| ctx.agent_home("QWEN_HOME", ".qwen"));
    plan_at(ctx, directory.join("settings.json"), cfg!(windows))
}

fn plan_at(ctx: &SetupCtx, file: std::path::PathBuf, windows: bool) -> Result<Plan> {
    let command = hookcmd::command(ctx, "qwen", if windows { Shell::PowerShell } else { Shell::Posix });
    let mut group = claude_style::hook_group(Some("*"), &command, Some(5000));
    group["hooks"][0]["env"] = json!({"HERDR_RESPAN_QWEN_HOOK":"1"});
    if windows { group["hooks"][0]["shell"] = json!("powershell"); }
    Ok(Plan {
        edits: EVENTS.iter().map(|event| Edit::JsonInsert {
            file: file.clone(), pointer: format!("/hooks/{event}"), value: group.clone(), id: format!("herdr-respan-qwen-{event}"),
        }).collect(),
        notices: vec!["Qwen Code hooks require v0.14.0 or later; full schema coverage targets c3968ace. disableAllHooks, --safe-mode and --bare suppress tracing. Qwen OAuth was discontinued on 2026-04-15; use an API-key/Coding Plan provider.".into()],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paths::Paths, setup};
    #[test]
    fn merges_beside_herdr_and_removes_only_owned_entries() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(tmp.path.join("project"))).unwrap();
        let file = ctx.project.as_ref().unwrap().join(".qwen/settings.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = json!({"hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"herdr-agent-session.sh","timeout":10000}]}]},"disableAllHooks":false,"telemetry":{"enabled":true}});
        std::fs::write(&file, original.to_string()).unwrap();
        let plan = plan(&ctx).unwrap();
        assert!(setup::preview(&ctx, "qwen", &plan).unwrap().contains("herdr-respan"));
        setup::apply(&ctx, "qwen", &plan).unwrap();
        let once = std::fs::read(&file).unwrap();
        setup::apply(&ctx, "qwen", &plan).unwrap();
        assert_eq!(once, std::fs::read(&file).unwrap());
        let installed: serde_json::Value = serde_json::from_slice(&once).unwrap();
        assert_eq!(installed["hooks"]["SessionStart"][0], original["hooks"]["SessionStart"][0]);
        for event in EVENTS { assert!(installed["hooks"][event].is_array()); }
        setup::remove(&ctx, &["qwen".into()]).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(file).unwrap()).unwrap(), original);
    }
    #[test]
    fn explicit_windows_shell_and_relocated_target() {
        let tmp = crate::tests::Temp::new();
        let ctx = SetupCtx {home:tmp.path.join("home"),config_dir:tmp.path.join("config with spaces"),state_dir:tmp.path.join("state"),project:None,binary:tmp.path.join("bin/herdr-respan.exe")};
        let file = tmp.path.join("relocated/settings.json");
        let plan = plan_at(&ctx, file.clone(), true).unwrap();
        let Edit::JsonInsert {file:actual,value,..} = &plan.edits[0] else {panic!("not JSON")};
        assert_eq!(actual, &file);
        assert_eq!(value["hooks"][0]["shell"], "powershell");
        assert!(value["hooks"][0]["command"].as_str().unwrap().starts_with("& '"));
        assert_eq!(value["hooks"][0]["timeout"], 5000);
        assert_eq!(value["hooks"][0]["env"]["HERDR_RESPAN_QWEN_HOOK"], "1");
        assert_eq!(ctx.agent_home("HERDR_RESPAN_UNSET_QWEN_TEST", ".qwen"), ctx.home.join(".qwen"));
    }
}