use crate::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub customer_identifier: String,
    pub content_cap_bytes: usize,
    pub spool_max_bytes: u64,
    pub turn_timeout_secs: u64,
    pub base_url: String,
    pub tool_spans: bool,
    pub subagent_spans: bool,
    pub aux_spans: bool,
    pub redact_patterns: Vec<String>,
    pub proxy: ProxyConfig,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyConfig { pub listen: String, pub idle_exit_secs: u64, pub upstream: BTreeMap<String, String> }
impl Default for ProxyConfig {
    fn default() -> Self { Self { listen: "127.0.0.1:0".into(), idle_exit_secs: 600, upstream: BTreeMap::from([
        ("anthropic".into(), "https://api.anthropic.com".into()), ("openai".into(), "https://api.openai.com".into()),
        ("gemini".into(), "https://generativelanguage.googleapis.com".into()),
    ]) } }
}
impl Default for Config {
    fn default() -> Self { Self { customer_identifier: String::new(), content_cap_bytes: 16384, spool_max_bytes: 268435456,
        turn_timeout_secs: 3600, base_url: "https://api.respan.ai".into(), tool_spans: true, subagent_spans: true, aux_spans: true,
        redact_patterns: Vec::new(), proxy: ProxyConfig::default() } }
}
impl Config {
    pub fn load(dir: &Path) -> Result<Self> {
        match std::fs::read_to_string(dir.join("config.toml")) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}
#[derive(Debug)]
pub struct Credentials { pub key: String, pub source: &'static str }
pub fn credentials(config_dir: &Path, home: &Path, env_key: Option<String>) -> Result<Option<Credentials>> {
    if let Ok(text) = std::fs::read_to_string(config_dir.join(".env")) {
        if let Some(key) = dotenv(&text).remove("RESPAN_API_KEY").filter(|s| !s.is_empty()) {
            return Ok(Some(Credentials { key, source: "config .env" }));
        }
    }
    let root = home.join(".respan");
    // A broken profile store is not an authoritative key; continue to the environment.
    let profile = match std::fs::read(root.join("config.json")) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok().map(|v| v.get("activeProfile").and_then(|v| v.as_str()).unwrap_or("default").to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some("default".into()),
        Err(_) => None,
    };
    let value = std::fs::read(root.join("credentials.json")).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if let (Some(profile), Some(value)) = (profile, value) {
        if let Some(cred) = value.get(&profile).filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("api_key")) {
            if let Some(key) = cred.get("apiKey").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                return Ok(Some(Credentials { key: key.into(), source: "Respan active profile" }));
            }
        }
    }
    Ok(env_key.filter(|s| !s.is_empty()).map(|key| Credentials { key, source: "RESPAN_API_KEY" }))
}
pub fn dotenv(text: &str) -> BTreeMap<String, String> {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.starts_with('#') { return None; }
        let (key, raw) = line.split_once('=')?;
        let raw = raw.trim();
        let value = if raw.starts_with('"') || raw.starts_with('\'') {
            let quote = raw.chars().next()?;
            let end = raw[1..].find(quote)? + 1;
            raw[1..end].to_string()
        } else { raw.split(" #").next()?.trim().to_string() };
        Some((key.trim().trim_start_matches("export ").to_string(), value))
    }).collect()
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn broken_profile_stores_fall_back_to_env() {
        let tmp = crate::tests::Temp::new(); let root = tmp.path.join(".respan");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("credentials.json"), "bad-json").unwrap();
        assert_eq!(credentials(&tmp.path.join("cfg"), &tmp.path, Some("env".into())).unwrap().unwrap().source, "RESPAN_API_KEY");
        assert!(credentials(&tmp.path.join("cfg"), &tmp.path, None).unwrap().is_none());
        std::fs::write(root.join("config.json"), "bad-json").unwrap();
        std::fs::write(root.join("credentials.json"), r#"{"default":{"type":"api_key","apiKey":"profile"}}"#).unwrap();
        assert_eq!(credentials(&tmp.path.join("cfg"), &tmp.path, Some("env".into())).unwrap().unwrap().key, "env");
    }
    #[test] fn precedence_and_defaults() {
        let tmp = crate::tests::Temp::new(); let cfg = tmp.path.join("cfg"); let home = tmp.path.join("home");
        std::fs::create_dir_all(home.join(".respan")).unwrap(); std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(home.join(".respan/config.json"), r#"{"activeProfile":"work"}"#).unwrap();
        std::fs::write(home.join(".respan/credentials.json"), r#"{"work":{"type":"api_key","apiKey":"profile"}}"#).unwrap();
        assert_eq!(credentials(&cfg, &home, Some("env".into())).unwrap().unwrap().key, "profile");
        std::fs::write(cfg.join(".env"), "# comment\nRESPAN_API_KEY='file' # comment\n").unwrap();
        assert_eq!(credentials(&cfg, &home, Some("env".into())).unwrap().unwrap().key, "file");
        std::fs::remove_file(cfg.join(".env")).unwrap();
        std::fs::write(home.join(".respan/credentials.json"), r#"{"work":{"type":"oauth","accessToken":"never"}}"#).unwrap();
        assert_eq!(credentials(&cfg, &home, Some("env".into())).unwrap().unwrap().key, "env");
        assert_eq!(Config::load(&cfg).unwrap().content_cap_bytes, 16384);
        assert_eq!(dotenv("A=hello # world\nB=\"a#b\"\n")["B"], "a#b");
    }
}
