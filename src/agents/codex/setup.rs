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
    let config = home.join("config.toml");
    let text = match std::fs::read_to_string(&config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let settings: ::toml::Value = text.parse()?;
    let hooks = settings.get("features").and_then(|features| features.get("hooks"));
    if hooks.is_none() {
        edits.push(Edit::TomlInsert { file: config, path: vec!["features".into(), "hooks".into()], value: toml_edit::value(true), id: "herdr-respan-codex-hooks-feature".into() });
    }
    let mut notices = vec![
        "Codex: open /hooks once and trust the herdr-respan command hooks after setup (and after changing their definition). Until the first Codex span arrives, status reports awaiting Codex trust. Never edit Codex's trust store.".into(),
        "Hooks are enabled by default in the inspected Codex source; setup adds features.hooks=true only when absent. Project hooks also require project trust.".into(),
    ];
    if hooks.and_then(::toml::Value::as_bool) == Some(false) {
        notices.push("Codex hooks are installed but disabled by features.hooks = false; remove that setting to enable them.".into());
    }
    Ok(Plan { edits, notices, offers: vec![] })
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn preserves_existing_regular_and_inline_hook_features() {
        for text in ["[features]\nhooks = false\n", "[features]\nhooks = true\n", "features = { hooks = true }\n", "features = { hooks = false }\n"] {
            let tmp = crate::tests::Temp::new();
            let paths = crate::paths::Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
            let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(tmp.path.join("project"))).unwrap();
            let home = ctx.project.as_ref().unwrap().join(".codex");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("config.toml"), text).unwrap();
            let plan = plan(&ctx).unwrap();
            assert_eq!(plan.notices.iter().any(|s| s.contains("disabled by features.hooks = false")), text.contains("false"));
            crate::setup::apply(&ctx, "codex", &plan).unwrap();
            assert!(home.join("hooks.json").is_file());
            assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), text);
            crate::setup::remove(&ctx, &["codex".into()]).unwrap();
            assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), text);
        }
    }
}
