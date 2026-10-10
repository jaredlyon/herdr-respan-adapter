use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Owned {
    /// Committed ownership only; pending operations are not installed entries.
    pub entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}
/// Replay owned operations against current files, never journal foreign config snapshots.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub install: Vec<Entry>,
    pub remove: Vec<Entry>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry { pub agent: String, pub id: String, pub file: PathBuf, pub existed: bool, pub change: Change }
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag="kind")]
pub enum Change {
    Json { pointer: String, value: Value, created: Vec<String> },
    JsonSet { pointer: String, value: Value, created: Vec<String> },
    Toml { path: Vec<String>, value: String, appended: bool, created: Vec<Vec<String>> },
    File { contents: Vec<u8> },
    Text,
    TextArg { arg: String },
}
impl Entry {
    pub fn same_edit(&self, other: &Self) -> bool { self.agent == other.agent && self.id == other.id && self.file == other.file }
}
impl Owned {
    pub fn load(state: &Path) -> Result<Self> {
        match std::fs::read(state.join("owned.json")) { Ok(bytes) => Ok(serde_json::from_slice(&bytes)?), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()), Err(e) => Err(e.into()) }
    }
    pub fn save(&self, state: &Path) -> Result<()> { crate::paths::atomic_json(&state.join("owned.json"), self) }
    pub fn record_install(&mut self, entry: Entry) {
        self.entries.retain(|old| !old.same_edit(&entry));
        self.entries.push(entry.clone());
        let pending = self.pending.get_or_insert_with(Pending::default);
        pending.install.retain(|old| !old.same_edit(&entry));
        pending.install.push(entry);
    }
    pub fn record_remove(&mut self, entry: Entry) {
        self.entries.retain(|old| !old.same_edit(&entry));
        let pending = self.pending.get_or_insert_with(Pending::default);
        pending.install.retain(|old| !old.same_edit(&entry));
        if !pending.remove.contains(&entry) { pending.remove.push(entry); }
    }
}
