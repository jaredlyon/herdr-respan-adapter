use crate::{paths::Paths, record::{now_ns, SpanRecord}, Result};
use std::{fs::{self, File, OpenOptions}, io::Write, path::{Path, PathBuf}, process::{Command, Stdio}, time::Duration};

pub struct Writer { state: PathBuf, path: PathBuf, file: Option<File>, max_bytes: u64 }
impl Writer {
    pub fn new(state: &Path, max_bytes: u64) -> Result<Self> {
        let dir = state.join("spool"); fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.jsonl", std::process::id(), now_ns()));
        let mut options = OpenOptions::new(); options.create_new(true).append(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        Ok(Self { file: Some(options.open(&path)?), path, state: state.into(), max_bytes })
    }
    pub fn append(&mut self, rec: &SpanRecord) -> Result<()> {
        let mut bytes = serde_json::to_vec(rec)?; bytes.push(b'\n');
        self.file.as_mut().ok_or("closed spool writer")?.write_all(&bytes)?;
        Ok(())
    }
    pub fn finish(mut self) -> Result<()> { self.close() }
    fn close(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.sync_data()?; drop(file);
            fs::rename(&self.path, self.path.with_extension("ready"))?;
            enforce_cap(&self.state, self.max_bytes)?;
        }
        Ok(())
    }
}
impl Drop for Writer { fn drop(&mut self) { if self.close().is_err() { crate::log::error(&self.state, "spool close failed"); } } }
pub fn ready(state: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = match fs::read_dir(state.join("spool")) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "ready")).collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(), Err(e) => return Err(e.into()),
    };
    paths.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    Ok(paths)
}
pub fn enforce_cap(state: &Path, max: u64) -> Result<()> {
    let paths = ready(state)?;
    let mut total: u64 = paths.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
    for path in paths {
        if total <= max { break; }
        let len = path.metadata().map(|m| m.len()).unwrap_or(0);
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            let mut file = OpenOptions::new().create(true).append(true).open(state.join("dropped.jsonl"))?;
            file.write_all(b"1\n")?;
        }
    }
    Ok(())
}
pub fn dropped(state: &Path) -> u64 { fs::read(state.join("dropped.jsonl")).map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64).unwrap_or(0) }
/// Closed-file owner-token lock. Windows sharing violations count as a held lock.
pub struct Lock { path: PathBuf, token: String }
impl Lock {
    pub fn acquire(state: &Path) -> Result<Option<Self>> {
        Self::acquire_at(&state.join("spool/flush.lock"),Duration::from_secs(60))
    }
    pub(crate) fn acquire_at(path:&Path,stale_after:Duration)->Result<Option<Self>> {
        if let Some(parent)=path.parent() {fs::create_dir_all(parent)?;}
        let mut options = OpenOptions::new(); options.create_new(true).write(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let open = || options.open(path);
        let mut file = match open() {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale=match path.metadata() {Ok(m)=>m.modified()?.elapsed().unwrap_or_default()>stale_after,Err(e) if e.kind()==std::io::ErrorKind::NotFound||lock_unavailable(&e)=>return Ok(None),Err(e)=>return Err(e.into())};
                if !stale { return Ok(None); }
                match fs::remove_file(path) {
                    Ok(()) => {},
                    Err(e) if e.kind()==std::io::ErrorKind::NotFound||lock_unavailable(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
                match open() { Ok(f) => f, Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists||lock_unavailable(&e) => return Ok(None), Err(e) => return Err(e.into()) }
            }, Err(e) if lock_unavailable(&e) => return Ok(None), Err(e) => return Err(e.into()),
        };
        let token = format!("{}-{}", std::process::id(), now_ns()); file.write_all(token.as_bytes())?;drop(file); // No open handle prevents Windows reclamation.
        Ok(Some(Self { path:path.into(), token }))
    }
    pub fn refresh(&self) -> Result<()> { crate::paths::write_private(&self.path, self.token.as_bytes()) }
}
fn lock_unavailable(error:&std::io::Error)->bool {
    if error.kind()==std::io::ErrorKind::PermissionDenied {return true;}
    // Rust leaves ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION uncategorized.
    #[cfg(windows)] {matches!(error.raw_os_error(),Some(32|33))}
    #[cfg(not(windows))] {false}
}
impl Drop for Lock { fn drop(&mut self) { if fs::read_to_string(&self.path).ok().as_deref() == Some(&self.token) { let _ = fs::remove_file(&self.path); } } }
fn settle_pending(state:&Path,agents:&[crate::agents::AgentSpec])->bool {
    agents.iter().any(|a|a.settle.is_some()&&state.join("agents").join(a.kind).join("settle-pending").exists())
}
pub fn spawn_flush(paths: &Paths) -> Result<()> {
    if paths.state.join("spool/flush.lock").exists() || (ready(&paths.state)?.is_empty() && !crate::agents::common::bridge::has_pending(&paths.state)? && !settle_pending(&paths.state,crate::agents::AGENTS)) { return Ok(()); }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("flush").arg("--detach").arg("--state-dir").arg(&paths.state).arg("--config-dir").arg(&paths.config);
    detached(&mut cmd); cmd.spawn()?; Ok(())
}
pub fn detach_sender(paths: &Paths) -> Result<()> {
    if std::env::var("HERDR_RESPAN_DETACH_STAGE").ok().as_deref() == Some("1") {
        #[cfg(unix)] {
            extern "C" { fn setsid() -> i32; }
            // This runs in the second fresh subprocess, never in pre_exec or the agent.
            // SAFETY: setsid has no pointer arguments and changes only this process's session.
            if unsafe { setsid() } == -1 { return Err("cannot detach sender session".into()); }
        }
        return crate::flush::run(paths, &crate::config::Config::load(&paths.config)?, crate::credentials(paths)?.as_ref());
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("flush").arg("--detach").arg("--state-dir").arg(&paths.state).arg("--config-dir").arg(&paths.config).env("HERDR_RESPAN_DETACH_STAGE", "1");
    detached(&mut cmd); cmd.spawn()?; Ok(())
}
fn detached(cmd: &mut Command) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000 | 0x00000008); }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn spool_caps_and_lock() {
        let tmp = crate::tests::Temp::new();
        { let mut writer = Writer::new(&tmp.path, u64::MAX).unwrap(); writer.append(&SpanRecord { span_key: "s".into(), ..Default::default() }).unwrap(); }
        assert_eq!(ready(&tmp.path).unwrap().len(), 1);
        enforce_cap(&tmp.path, 0).unwrap(); assert!(ready(&tmp.path).unwrap().is_empty()); assert_eq!(dropped(&tmp.path), 1);
        let lock = Lock::acquire(&tmp.path).unwrap().unwrap(); assert!(Lock::acquire(&tmp.path).unwrap().is_none()); lock.refresh().unwrap(); drop(lock);
        assert!(Lock::acquire(&tmp.path).unwrap().is_some());
    }
    #[test] fn stale_sender_lock_is_reclaimed() {
        let tmp=crate::tests::Temp::new();let first=Lock::acquire(&tmp.path).unwrap().unwrap();let path=tmp.path.join("spool/flush.lock");
        let file=OpenOptions::new().write(true).open(&path).unwrap();file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()-Duration::from_secs(61))).unwrap();drop(file);
        let second=Lock::acquire(&tmp.path).unwrap().unwrap();drop(first);assert!(path.exists());assert!(Lock::acquire(&tmp.path).unwrap().is_none());drop(second);assert!(!path.exists());
    }
    #[cfg(windows)]
    #[test] fn stale_lock_with_exclusive_handle_is_held_until_closed() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp=crate::tests::Temp::new();let first=Lock::acquire(&tmp.path).unwrap().unwrap();let path=tmp.path.join("spool/flush.lock");
        let file=OpenOptions::new().write(true).share_mode(0).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()-Duration::from_secs(61))).unwrap();
        assert!(Lock::acquire(&tmp.path).unwrap().is_none());drop(file);
        let second=Lock::acquire(&tmp.path).unwrap().unwrap();drop(first);assert!(path.exists());drop(second);assert!(!path.exists());
    }
    #[test] fn settlement_markers_trigger_only_registered_settlers() {
        fn hook(_: &mut crate::record::HookCtx,_:&[u8])->Result<()> {Ok(())}
        fn plan(_: &crate::setup::SetupCtx)->Result<crate::setup::Plan> {Ok(Default::default())}
        fn settle(_: &mut crate::record::HookCtx)->Result<bool> {Ok(false)}
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=crate::record::HookCtx::new(paths.clone(),crate::config::Config::default(),"deferred",Default::default(),None,false).unwrap();let mut spec=crate::agents::AgentSpec {kind:"deferred",display:"deferred",hook,plan,reply:None,settle:Some(settle)};
        assert!(!settle_pending(&paths.state,std::slice::from_ref(&spec)));ctx.mark_settle_pending().unwrap();assert!(settle_pending(&paths.state,std::slice::from_ref(&spec)));spec.settle=None;assert!(!settle_pending(&paths.state,std::slice::from_ref(&spec)));assert!(!settle_pending(&paths.state,&[]));
    }
}
