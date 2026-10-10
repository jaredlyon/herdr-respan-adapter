use crate::{record::HookCtx, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::path::PathBuf;
fn path(ctx: &HookCtx, key: &str) -> Result<PathBuf> {
    if key.is_empty() || key == "." || key == ".." || !key.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) { return Err("state key must be a leaf filename".into()); }
    Ok(ctx.state_dir().join(key))
}
impl HookCtx {
    pub fn load<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        match std::fs::read(path(self,key)?) { Ok(bytes) => Ok(serde_json::from_slice(&bytes)?), Err(e) if e.kind()==std::io::ErrorKind::NotFound => Ok(T::default()), Err(e) => Err(e.into()) }
    }
    pub fn save<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let mut value=serde_json::to_value(value)?;
        self.scrub_value(&mut value);
        crate::paths::atomic_json(&path(self,key)?, &value)
    }
}
