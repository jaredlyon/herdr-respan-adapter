use crate::{config::Credentials, paths::Paths, Result};
use serde_json::{json, Value};
pub fn report(paths: &Paths, credentials: Option<&Credentials>) -> Result<Value> {
    let files = crate::spool::ready(&paths.state)?;
    let bytes: u64 = files.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
    let spans: u64 = files.iter().filter_map(|p| std::fs::read(p).ok()).map(|b| b.iter().filter(|&&c| c==b'\n').count() as u64).sum();
    let agents: Vec<Value> = ["claude","codex","omp","pi","gemini","opencode","cursor","hermes","copilot","kimi","qwen","grok"].iter().map(|kind| {
        let installed = crate::setup::installed(&paths.state, kind);
        json!({"agent":kind,"adapter_available":crate::agents::find(kind).is_some(),"coverage":if installed {"adapter installed"} else {"lifecycle"}})
    }).collect();
    Ok(json!({"agents":agents,"spool_files":files.len(),"spool_bytes":bytes,"spool_spans":spans,"dropped_files":crate::spool::dropped(&paths.state),
        "flush":crate::flush::status(&paths.state),"key_source":credentials.map(|c| c.source).unwrap_or("not configured"),
        "proxy_active":paths.state.join("proxy.json").exists()}))
}
