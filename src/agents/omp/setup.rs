use crate::{setup::{Edit, Plan, SetupCtx}, Result};
use std::{ffi::OsString, path::{Component, Path, PathBuf}};

const ASSET: &str = include_str!("assets/herdr-respan.ts");

fn expand(home: &Path, value: OsString) -> PathBuf {
    let path = PathBuf::from(value);
    if let Some(raw) = path.to_str() {
        if raw == "~" { return home.into(); }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")).or_else(|| raw.strip_prefix('~')) {
            return home.join(rest);
        }
    }
    path
}
fn directories(home: &Path, agent: Option<OsString>, config: Option<OsString>) -> (PathBuf, PathBuf) {
    if let Some(agent) = agent.filter(|s| !s.is_empty()) {
        let extensions = expand(home, agent).join("extensions");
        return (extensions.clone(), extensions);
    }
    let config = config.filter(|s| !s.is_empty()).unwrap_or_else(|| ".omp".into());
    (home.join(config).join("agent/extensions"), home.join(".pi/agent/extensions"))
}
fn identity(path: &Path) -> Result<PathBuf> {
    let absolute = crate::paths::absolute(path)?;
    let mut normalized = PathBuf::new();
    for c in absolute.components() {
        match c { Component::CurDir => {}, Component::ParentDir => { normalized.pop(); }, _ => normalized.push(c.as_os_str()) }
    }
    let mut existing = normalized.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(existing.file_name().ok_or("missing extension directory ancestor")?);
        existing = existing.parent().ok_or("missing extension directory ancestor")?;
    }
    let mut canonical = existing.canonicalize()?;
    for part in suffix.iter().rev() { canonical.push(part); }
    Ok(canonical)
}
fn plan_at(ctx: &SetupCtx, omp: PathBuf, pi: PathBuf) -> Result<Plan> {
    if identity(&omp)? == identity(&pi)? {
        return Err(format!("Pi and OMP resolve to the same extension directory at {}; configure separate agent directories before installing OMP", omp.display()).into());
    }
    let argv = crate::agents::common::hookcmd::argv(ctx, "omp");
    // Substitute only the initializer, preserving TS declarations and safely escaping all paths.
    let mut source = ASSET.to_string();
    for (token, value) in [("__HERDR_RESPAN_BIN__", &argv[0]), ("__HERDR_RESPAN_STATE__", &argv[4]), ("__HERDR_RESPAN_CONFIG__", &argv[6])] {
        source = source.replace(&format!("= {token};"), &format!("= {};", serde_json::to_string(value)?));
    }
    Ok(Plan { edits: vec![Edit::WriteFile { path: crate::paths::absolute(&omp)?.join("herdr-respan-omp.ts"),
        contents: source.into_bytes(), id: "herdr-respan-omp-extension".into() }],
        notices: vec!["OMP tracing requires its Bun extension runtime and an interactive herdr root session; named profiles need their own explicit extension installation. No models.yml or auth files are changed.".into()],
        ..Default::default() })
}
pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    let (omp, pi) = if let Some(project) = &ctx.project {
        (project.join(".omp/extensions"), project.join(".pi/extensions"))
    } else { directories(&ctx.home, std::env::var_os("PI_CODING_AGENT_DIR"), std::env::var_os("PI_CONFIG_DIR")) };
    plan_at(ctx, omp, pi)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relocations_and_shared_directory_refusal() {
        let tmp = crate::tests::Temp::new();
        let home = tmp.path.join("home");
        assert_eq!(directories(&home, None, None), (home.join(".omp/agent/extensions"), home.join(".pi/agent/extensions")));
        assert_eq!(directories(&home, None, Some("custom".into())).0, home.join("custom/agent/extensions"));
        assert_eq!(directories(&home, Some("~/relocated".into()), Some("ignored".into())).0, home.join("relocated/extensions"));
        assert_eq!(directories(&home, Some("".into()), Some("".into())).0, home.join(".omp/agent/extensions"));
        let paths = crate::paths::Paths {state: tmp.path.join("state"), config: tmp.path.join("config")};
        let ctx = SetupCtx::new(&paths, home.clone(), None).unwrap();
        let shared = directories(&home, Some("~/shared".into()), None);
        assert!(plan_at(&ctx, shared.0, shared.1).err().expect("shared directory must fail").to_string().contains("same extension directory"));
        assert!(plan_at(&ctx, home.join(".pi/agent/../agent/extensions"), home.join(".pi/agent/extensions")).is_err());
    }
    #[test]
    fn setup_remove_preserves_herdr_and_escapes_paths() {
        let tmp = crate::tests::Temp::new();
        let paths = crate::paths::Paths {state: tmp.path.join("state ' space"), config: tmp.path.join("config \" space")};
        let ctx = SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
        let dir = ctx.home.join(".omp/agent/extensions");
        std::fs::create_dir_all(&dir).unwrap();
        let herdr = dir.join("herdr-agent-state.ts");
        let original = b"// installed by herdr\n// HERDR_INTEGRATION_ID=omp\n";
        std::fs::write(&herdr, original).unwrap();
        let plan = plan_at(&ctx, dir.clone(), ctx.home.join(".pi/agent/extensions")).unwrap();
        assert!(crate::setup::preview(&ctx, "omp", &plan).unwrap().contains("herdr-respan"));
        crate::setup::apply(&ctx, "omp", &plan).unwrap();
        crate::setup::apply(&ctx, "omp", &plan).unwrap();
        let source = std::fs::read_to_string(dir.join("herdr-respan-omp.ts")).unwrap();
        assert!(source.contains(&format!("const state = {};", serde_json::to_string(&ctx.state_dir.to_string_lossy()).unwrap())));
        assert!(!source.contains("= __HERDR_RESPAN_BIN__;"));
        crate::setup::remove(&ctx, &["omp".into()]).unwrap();
        assert!(!dir.join("herdr-respan-omp.ts").exists());
        assert_eq!(std::fs::read(herdr).unwrap(), original);
    }
}