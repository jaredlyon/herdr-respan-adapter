use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Owned { pub entries: Vec<Entry> }
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
impl Owned {
    pub fn load(state: &Path) -> Result<Self> {
        match std::fs::read(state.join("owned.json")) { Ok(bytes) => Ok(serde_json::from_slice(&bytes)?), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()), Err(e) => Err(e.into()) }
    }
    pub fn save(&self, state: &Path) -> Result<()> { crate::paths::atomic_json(&state.join("owned.json"), self) }
}
