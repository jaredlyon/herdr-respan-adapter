//! pi adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "pi", display: "pi", hook, plan, reply: None };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("pi adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("pi adapter not built yet".into()) }
