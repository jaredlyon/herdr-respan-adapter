use std::path::{Path, PathBuf};
use crate::Result;

#[derive(Clone, Debug)]
pub struct Paths { pub state: PathBuf, pub config: PathBuf }
impl Paths {
    pub fn resolve(state: Option<PathBuf>, config: Option<PathBuf>) -> Result<Self> {
        let state = state.or_else(|| std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from)).ok_or("missing state directory")?;
        let config = config.or_else(|| std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from)).ok_or("missing config directory")?;
        Ok(Self { state: absolute(&state)?, config: absolute(&config)? })
    }
    pub fn binary(&self) -> PathBuf { self.state.join("bin").join(if cfg!(windows) { "herdr-respan.exe" } else { "herdr-respan" }) }
}
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).ok_or_else(|| "missing home directory".into())
}
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() { Ok(path.to_path_buf()) } else { Ok(std::env::current_dir()?.join(path)) }
}
/// State and credential files are owner-only on Unix; inherited ACLs apply on Windows.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}
pub fn atomic_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), crate::record::now_ns()));
    write_private(&tmp, &serde_json::to_vec_pretty(value)?)?;
    if let Err(e) = std::fs::rename(&tmp, path) { let _ = std::fs::remove_file(&tmp); return Err(e.into()); }
    Ok(())
}
