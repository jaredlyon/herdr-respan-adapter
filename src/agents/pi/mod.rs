//! pi's in-process observer sends one shared bridge batch per settled user run.
mod setup;
#[cfg(test)] mod tests;
use crate::agents::{common::bridge, AgentSpec};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "pi", display: "pi", hook: bridge::hook, plan: setup::plan, reply: None,
};
