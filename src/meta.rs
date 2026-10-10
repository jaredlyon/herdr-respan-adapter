use std::{collections::BTreeMap, path::Path, process::{Command, Stdio}};
use serde_json::Value;
pub fn hostname() -> String {
    // HOSTNAME is usually an unexported shell variable (macOS has no /etc/hostname), so fall back to `hostname`.
    std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| Command::new("hostname").stderr(Stdio::null()).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into())
}
pub fn collect(cwd: &Path) -> BTreeMap<String, Value> {
    let mut meta = BTreeMap::new();
    for (key, env) in [("herdr.workspace_id", "HERDR_WORKSPACE_ID"), ("herdr.tab_id", "HERDR_TAB_ID"), ("herdr.pane_id", "HERDR_PANE_ID")] {
        meta.insert(key.into(), Value::String(std::env::var(env).unwrap_or_default()));
    }
    meta.insert("herdr.machine".into(), Value::String(hostname()));
    meta.insert("cwd".into(), Value::String(cwd.to_string_lossy().into()));
    meta.insert("plugin_version".into(), Value::String(env!("CARGO_PKG_VERSION").into()));
    meta.insert("agent_version".into(), Value::String(String::new()));
    if let Ok(output) = Command::new("git").arg("-C").arg(cwd).args(["rev-parse", "--show-toplevel", "HEAD", "--abbrev-ref", "HEAD"]).stderr(Stdio::null()).output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout); let mut lines = text.lines();
            if let (Some(root), Some(sha), Some(branch)) = (lines.next(), lines.next(), lines.next()) {
                meta.insert("git.repo".into(), Value::String(Path::new(root).file_name().unwrap_or_default().to_string_lossy().into()));
                meta.insert("git.branch".into(), if branch == "HEAD" { Value::Null } else { Value::String(branch.into()) }); meta.insert("git.sha".into(), Value::String(sha.into()));
            }
        }
    }
    meta
}

#[cfg(test)] mod tests {
    use super::*;
    use std::io::Write;
    #[test] fn repository_detached_and_nonrepo_metadata() {
        let tmp=crate::tests::Temp::new();let repo=tmp.path.join("repo");
        assert!(Command::new("git").args(["init","--quiet","-b","main"]).arg(&repo).status().unwrap().success());
        let mut tree=Command::new("git").arg("-C").arg(&repo).args(["hash-object","-w","-t","tree","--stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();tree.stdin.take().unwrap().write_all(b"").unwrap();let tree=String::from_utf8(tree.wait_with_output().unwrap().stdout).unwrap();
        let raw=format!("tree {}\nauthor Test <test@example.invalid> 1704067200 +0000\ncommitter Test <test@example.invalid> 1704067200 +0000\n\nfixture\n",tree.trim());
        let mut commit=Command::new("git").arg("-C").arg(&repo).args(["hash-object","-w","-t","commit","--stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();commit.stdin.take().unwrap().write_all(raw.as_bytes()).unwrap();let sha=String::from_utf8(commit.wait_with_output().unwrap().stdout).unwrap();
        std::fs::write(repo.join(".git/refs/heads/main"),&sha).unwrap();let meta=collect(&repo);assert_eq!(meta["git.branch"],"main");assert_eq!(meta["git.sha"],sha.trim());assert_eq!(meta["git.repo"],"repo");
        std::fs::write(repo.join(".git/HEAD"),&sha).unwrap();let detached=collect(&repo);assert!(detached["git.branch"].is_null());assert_eq!(detached["git.sha"],sha.trim());
        assert!(!collect(&tmp.path).contains_key("git.repo"));
    }
}
