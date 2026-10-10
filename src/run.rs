//! Owned by the Proxy slice (design spec). Placeholder until it lands.
use crate::{config::Config, paths::Paths, Result};
/// Launch wrapper. Exits the process with the child's exit code once built.
pub fn run(_paths: &Paths, _config: &Config, _args: &[String]) -> Result<()> { Err("not built yet: run".into()) }
