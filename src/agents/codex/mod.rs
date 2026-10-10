//! Codex adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "codex", display: "Codex", hook, plan };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("codex adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("codex adapter not built yet".into()) }
