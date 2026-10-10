//! GitHub Copilot CLI adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "copilot", display: "GitHub Copilot CLI", hook, plan, reply: None };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("copilot adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("copilot adapter not built yet".into()) }
