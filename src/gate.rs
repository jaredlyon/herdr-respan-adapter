use crate::Result;
use serde::{Serialize, Deserialize};
use std::path::Path;
#[derive(Default, Serialize, Deserialize)]
struct Paused { #[serde(default)] all: bool, #[serde(default)] workspaces: Vec<String> }
fn load(state: &Path) -> Result<Paused> {
    match std::fs::read(state.join("paused.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Paused::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn allowed(state: &Path, herdr: Option<&str>, optout: Option<&str>, workspace: Option<&str>) -> bool {
    herdr == Some("1") && optout != Some("off") && load(state).map(|p| !p.all && !p.workspaces.iter().any(|w| Some(w.as_str()) == workspace)).unwrap_or(false)
}
/// Machine-wide switch (`herdr-respan off`/`on`): every hook, lifecycle event, proxy span and `run` honor it.
pub fn is_off(state: &Path) -> bool { load(state).map(|p| p.all).unwrap_or(true) }
pub fn set_off(state: &Path, off: bool) -> Result<()> {
    let mut value = load(state)?; value.all = off;
    crate::paths::atomic_json(&state.join("paused.json"), &value)
}
pub fn from_env(state: &Path) -> bool {
    allowed(state, std::env::var("HERDR_ENV").ok().as_deref(), std::env::var("HERDR_RESPAN").ok().as_deref(), std::env::var("HERDR_WORKSPACE_ID").ok().as_deref())
}
pub fn pause(state: &Path, workspace: &str, paused: bool) -> Result<()> {
    if workspace.is_empty() { return Err("no workspace: run this from a workspace or pane, pass --workspace <id>, or use `herdr-respan off` to stop tracing everywhere".into()); }
    let mut value = load(state)?;
    value.workspaces.retain(|w| w != workspace);
    if paused { value.workspaces.push(workspace.into()); }
    crate::paths::atomic_json(&state.join("paused.json"), &value)
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn gate_and_pause() {
        let tmp = crate::tests::Temp::new();
        assert!(!allowed(&tmp.path, None, None, None));
        assert!(!allowed(&tmp.path, Some("true"), None, None));
        assert!(!allowed(&tmp.path, Some("1"), Some("off"), Some("w")));
        assert!(allowed(&tmp.path, Some("1"), None, Some("w")));
        pause(&tmp.path, "w", true).unwrap();
        assert!(!allowed(&tmp.path, Some("1"), None, Some("w")));
        assert!(allowed(&tmp.path, Some("1"), None, Some("other")));
        pause(&tmp.path, "w", false).unwrap();
        assert!(allowed(&tmp.path, Some("1"), None, Some("w")));
        set_off(&tmp.path, true).unwrap(); assert!(is_off(&tmp.path));
        assert!(!allowed(&tmp.path, Some("1"), None, Some("w")) && !allowed(&tmp.path, Some("1"), None, None));
        pause(&tmp.path, "other", true).unwrap(); set_off(&tmp.path, false).unwrap(); // workspace pauses survive the switch
        assert!(allowed(&tmp.path, Some("1"), None, Some("w")) && !allowed(&tmp.path, Some("1"), None, Some("other")) && !is_off(&tmp.path));
        std::fs::write(tmp.path.join("paused.json"), "broken").unwrap();
        assert!(!allowed(&tmp.path, Some("1"), None, Some("w")));
    }
}
