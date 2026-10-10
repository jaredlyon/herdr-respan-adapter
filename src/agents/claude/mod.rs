//! Claude Code adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "claude", display: "Claude Code", hook, plan };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("claude adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("claude adapter not built yet".into()) }
