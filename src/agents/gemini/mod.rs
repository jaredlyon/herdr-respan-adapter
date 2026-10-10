//! Gemini CLI adapter. Placeholder until its agent slice lands; see the design spec "# Agent slices".
use crate::{agents::AgentSpec, record::HookCtx, setup::{Plan, SetupCtx}, Result};
pub const SPEC: AgentSpec = AgentSpec { kind: "gemini", display: "Gemini CLI", hook, plan, reply: None, settle: None };
fn hook(_: &mut HookCtx, _: &[u8]) -> Result<()> { Err("gemini adapter not built yet".into()) }
fn plan(_: &SetupCtx) -> Result<Plan> { Err("gemini adapter not built yet".into()) }
