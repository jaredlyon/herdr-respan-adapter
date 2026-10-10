//! Grok Build adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "grok", display: "Grok Build", hook, plan, reply: None };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("grok adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("grok adapter not built yet".into()) }
