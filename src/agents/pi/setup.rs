use crate::{setup::{Edit, Plan, SetupCtx}, Result};
use std::path::Path;

const ASSET: &str = include_str!("assets/herdr-respan.ts");

pub fn plan(ctx: &SetupCtx) -> Result<Plan> {
    plan_for(ctx, std::env::var_os("PI_CODING_AGENT_DIR").as_deref().map(Path::new))
}

pub(super) fn plan_for(ctx: &SetupCtx, relocated: Option<&Path>) -> Result<Plan> {
    let directory = if let Some(project) = &ctx.project {
        project.join(".pi/extensions")
    } else {
        let agent = match relocated {
            Some(path) if path == Path::new("~") => ctx.home.clone(),
            Some(path) if path.starts_with("~/") => ctx.home.join(path.strip_prefix("~")?),
            Some(path) => crate::paths::absolute(path)?,
            None => ctx.home.join(".pi/agent"),
        };
        agent.join("extensions")
    };
    let mut source = ASSET.to_string();
    for (placeholder, path) in [
        ("\"__HERDR_RESPAN_BIN__\"", &ctx.binary),
        ("\"__HERDR_RESPAN_STATE__\"", &ctx.state_dir),
        ("\"__HERDR_RESPAN_CONFIG__\"", &ctx.config_dir),
    ] {
        source = source.replace(placeholder, &serde_json::to_string(&path.to_string_lossy())?);
    }
    Ok(Plan {
        edits: vec![Edit::WriteFile {
            path: directory.join("herdr-respan.ts"),
            contents: source.into_bytes(), id: "herdr-respan-pi-extension".into(),
        }],
        notices: vec!["pi: requires the pi 1.1.0 event contract (agent_settled and ctx.mode). Native subagent tools are traced as tools; pi exposes no native child lifecycle/usage events. Compaction is a tool span.".into()],
        ..Default::default()
    })
}
