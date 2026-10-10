use crate::{paths::Paths, record::{now_ns, SpanRecord}, Result};
use std::{fs::{self, File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, path::{Path, PathBuf}, process::{Command, Stdio}, time::Duration};

pub struct Writer { state: PathBuf, path: PathBuf, file: Option<File>, max_bytes: u64, complete_bytes: u64 }
impl Writer {
    pub fn new(state: &Path, max_bytes: u64) -> Result<Self> {
        let dir = state.join("spool"); fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.jsonl", std::process::id(), now_ns()));
        let mut options = OpenOptions::new(); options.create_new(true).read(true).append(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let file = options.open(&path)?;
        file.try_lock()?;
        Ok(Self { file: Some(file), path, state: state.into(), max_bytes, complete_bytes: 0 })
    }
    pub fn append(&mut self, rec: &SpanRecord) -> Result<()> {
        let mut bytes = serde_json::to_vec(rec)?; bytes.push(b'\n');
        self.append_bytes(&bytes, |file, bytes| file.write_all(bytes))
    }
    fn append_bytes(&mut self, bytes: &[u8], write: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>) -> Result<()> {
        let file = self.file.as_mut().ok_or("closed spool writer")?;
        if let Err(error) = write(file, bytes) {
            // Never append after an unrepairable tail or publish it from Drop.
            if file.set_len(self.complete_bytes).is_err() { self.file.take(); }
            return Err(error.into());
        }
        self.complete_bytes += bytes.len() as u64;
        Ok(())
    }
    pub fn finish(mut self) -> Result<()> { self.close() }
    fn close(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.set_len(self.complete_bytes)?;
            file.sync_data()?; drop(file);
            fs::rename(&self.path, self.path.with_extension("ready"))?;
            enforce_cap(&self.state, self.max_bytes)?;
        }
        Ok(())
    }
}
impl Drop for Writer { fn drop(&mut self) { if self.close().is_err() { crate::log::error(&self.state, "spool close failed"); } } }
fn spool_paths(state: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(state.join("spool")) {
        Ok(entries) => Ok(entries.filter_map(|e| e.ok().map(|e| e.path())).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()), Err(e) => Err(e.into()),
    }
}
fn recover(state: &Path, paths: &[PathBuf]) -> Result<()> {
    let Some(_guard) = coordinate(&state.join("spool/recovery.coord"))? else { return Ok(()); };
    for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")) {
        let Some((pid, stamp)) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.split_once('-')) else { continue; };
        let (Ok(pid), Ok(_)) = (pid.parse::<u32>(), stamp.parse::<u64>()) else { continue; };
        if crate::proxy::live(pid) { continue; }
        let mut file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || lock_unavailable(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        // The writer's OS lock also protects it if a liveness probe fails.
        if !try_lock_file(&file)? { continue; }
        // Like common::jsonl::at_end, scan backwards without reading full records.
        // Use this locked handle: a second handle cannot read locked bytes on Windows.
        let mut buffer = [0; 8192]; let mut end = file.metadata()?.len(); let mut complete = 0;
        while end > 0 {
            let start = end.saturating_sub(buffer.len() as u64); let len = (end - start) as usize;
            file.seek(SeekFrom::Start(start))?; file.read_exact(&mut buffer[..len])?;
            if let Some(index) = buffer[..len].iter().rposition(|&b| b == b'\n') { complete = start + index as u64 + 1; break; }
            end = start;
        }
        file.set_len(complete)?; file.sync_data()?; drop(file);
        if complete == 0 { fs::remove_file(path)?; }
        else { fs::rename(path, path.with_extension("ready"))?; }
    }
    Ok(())
}
/// Recovers complete records from dead writers before listing sendable files.
pub fn ready(state: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = spool_paths(state)?;
    if paths.iter().any(|p| p.extension().is_some_and(|e| e == "jsonl")) {
        recover(state, &paths)?; paths = spool_paths(state)?;
    }
    paths.retain(|p| p.extension().is_some_and(|e| e == "ready"));
    paths.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    Ok(paths)
}
/// Sender acknowledgement/removal shares this guard with cap eviction.
/// Drop drains requests from nonblocking closers before releasing ownership.
pub(crate) struct CapGuard { state: PathBuf, file: Option<File> }
impl CapGuard {
    fn release(&mut self) -> Result<()> { self.release_with(|| Ok(())) }
    fn release_with(&mut self, mut before_unlock: impl FnMut() -> Result<()>) -> Result<()> {
        loop {
            let Some(file) = self.file.take() else { return Ok(()); };
            let result = drain_cap_requests(&self.state).and_then(|_| before_unlock());
            let unlocked = file.unlock(); drop(file);
            result?; unlocked?;
            // A request may have appeared after the final empty check while we
            // still owned the lock. Recheck AFTER unlocking, then hand off or
            // reacquire without waiting. Requests queued later acquire it themselves.
            if cap_requests(&self.state)?.is_empty() { return Ok(()); }
            self.file = coordinate(&self.state.join("spool/cap.coord"))?;
        }
    }
}
impl Drop for CapGuard {
    fn drop(&mut self) {
        if self.release().is_err() { crate::log::error(&self.state, "spool cap handoff failed"); }
    }
}
pub(crate) fn cap_lock(state: &Path) -> Result<Option<CapGuard>> {
    Ok(coordinate(&state.join("spool/cap.coord"))?.map(|file| CapGuard { state: state.into(), file: Some(file) }))
}
fn request_cap(state: &Path, max: u64) -> Result<()> {
    let directory = state.join("spool/cap-requests"); fs::create_dir_all(&directory)?;
    // Empty, atomically created markers encode only our numeric cap and ownership
    // identifier. No records or unrelated file contents become spool publications.
    let path = directory.join(format!("cap-{max}-{}.request", crate::record::random_id()?));
    let mut options = OpenOptions::new(); options.create_new(true).write(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    drop(options.open(path)?); Ok(())
}
fn cap_requests(state: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let entries = match fs::read_dir(state.join("spool/cap-requests")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), Err(e) => return Err(e.into()),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let entry = entry?; let path = entry.path();
        let file_type = match entry.file_type() { Ok(kind) => kind, Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()) };
        if !file_type.is_file() || !path.extension().is_some_and(|e| e == "request") { continue; }
        let Some((max, id)) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix("cap-")).and_then(|s| s.split_once('-')) else { continue; };
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { continue; }
        let Ok(max) = max.parse::<u64>() else { continue; };
        match entry.metadata() {
            Ok(metadata) if metadata.len() == 0 => requests.push((path, max)),
            Ok(_) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()),
        }
    }
    Ok(requests)
}
fn drain_cap_requests(state: &Path) -> Result<()> {
    loop {
        let requests = cap_requests(state)?;
        let Some(max) = requests.iter().map(|(_, max)| *max).min() else { return Ok(()); };
        evict_ready(state, max)?;
        for (path, _) in requests {
            match fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
        }
    }
}
pub fn enforce_cap(state: &Path, max: u64) -> Result<()> {
    request_cap(state, max)?; // Publish the request BEFORE testing ownership.
    let Some(mut guard) = cap_lock(state)? else { return Ok(()); };
    guard.release()
}
fn evict_ready(state: &Path, max: u64) -> Result<()> {
    let paths = ready(state)?;
    for (index, path) in paths.iter().enumerate() {
        // Ignore concurrently removed files rather than subtracting a stale size.
        let total: u64 = paths[index..].iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
        if total <= max { break; }
        match fs::remove_file(path) {
            Ok(()) => {
                let mut file = OpenOptions::new().create(true).append(true).open(state.join("dropped.jsonl"))?;
                file.write_all(b"1\n")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
        // Recount surviving lengths, not a stale total, after every deletion.
    }
    Ok(())
}
pub fn dropped(state: &Path) -> u64 { fs::read(state.join("dropped.jsonl")).map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64).unwrap_or(0) }
fn try_lock_file(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) if lock_unavailable(&e) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}
// Never unlink a coordination file: every contender must lock the same inode.
fn coordinate(path: &Path) -> Result<Option<File>> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let mut options = OpenOptions::new(); options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let file = match options.open(path) {
        Ok(file) => file, Err(e) if lock_unavailable(&e) => return Ok(None), Err(e) => return Err(e.into()),
    };
    Ok(if try_lock_file(&file)? { Some(file) } else { None })
}
fn coordination_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string(); name.push(".coord"); PathBuf::from(name)
}
fn stale(path: &Path, stale_after: Duration) -> Result<bool> {
    match path.metadata() {
        Ok(m) => Ok(m.modified()?.elapsed().unwrap_or_default() > stale_after),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) if lock_unavailable(&e) => Ok(false), Err(e) => Err(e.into()),
    }
}
/// Owner-token lease with a lifetime-held OS lock on a permanent sidecar.
/// Live OS ownership outranks mtime; only orphan leases can be reclaimed.
/// Windows sharing violations and coordination contention count as held locks.
pub struct Lock { path: PathBuf, token: String, _guard: File }
impl Lock {
    pub fn acquire(state: &Path) -> Result<Option<Self>> {
        Self::acquire_at(&state.join("spool/flush.lock"), Duration::from_secs(60))
    }
    pub(crate) fn acquire_at(path: &Path, stale_after: Duration) -> Result<Option<Self>> {
        let Some(_guard) = coordinate(&coordination_path(path))? else { return Ok(None); };
        let mut options = OpenOptions::new(); options.create_new(true).write(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !stale(path, stale_after)? { return Ok(None); }
                match fs::remove_file(path) {
                    Ok(()) => {},
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound || lock_unavailable(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
                match options.open(path) { Ok(f) => f, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || lock_unavailable(&e) => return Ok(None), Err(e) => return Err(e.into()) }
            }, Err(e) if lock_unavailable(&e) => return Ok(None), Err(e) => return Err(e.into()),
        };
        let token = format!("{}-{}", std::process::id(), now_ns());
        if let Err(e) = file.write_all(token.as_bytes()) { drop(file); let _ = fs::remove_file(path); return Err(e.into()); }
        drop(file); // No lease handle prevents Windows stale reclamation.
        Ok(Some(Self { path: path.into(), token, _guard }))
    }
    pub fn refresh(&self) -> Result<()> {
        if fs::read_to_string(&self.path)?.as_str() != self.token { return Err("spool lock ownership lost".into()); }
        crate::paths::write_private(&self.path, self.token.as_bytes())
    }
}
fn lock_unavailable(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::PermissionDenied { return true; }
    // Rust leaves ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION uncategorized.
    #[cfg(windows)] { matches!(error.raw_os_error(), Some(32 | 33)) }
    #[cfg(not(windows))] { false }
}
impl Drop for Lock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).ok().as_deref() == Some(&self.token) { let _ = fs::remove_file(&self.path); }
        // Explicit unlock also releases fork/dup copies of the same OS lock description.
        let _ = self._guard.unlock();
    }
}
fn settle_pending(state: &Path, agents: &[crate::agents::AgentSpec]) -> bool {
    agents.iter().any(|a| a.settle.is_some() && state.join("agents").join(a.kind).join("settle-pending").exists())
}
pub(crate) fn has_settle_pending(state: &Path) -> bool { settle_pending(state, crate::agents::AGENTS) }
fn flush_eligible(paths: &Paths) -> Result<bool> {
    let Some(_guard) = coordinate(&coordination_path(&paths.state.join("spool/flush.lock")))? else { return Ok(false); };
    Ok(stale(&paths.state.join("spool/flush.lock"), Duration::from_secs(60))? &&
        (!ready(&paths.state)?.is_empty() || crate::agents::common::bridge::has_pending(&paths.state)? || has_settle_pending(&paths.state) || crate::lifecycle::has_pending(&paths.state)?))
}
pub fn spawn_flush(paths: &Paths) -> Result<()> {
    if !flush_eligible(paths)? { return Ok(()); }
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
    fn writer_bytes(writer: &mut Writer) -> Vec<u8> {
        let file = writer.file.as_mut().unwrap(); file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new(); file.read_to_end(&mut bytes).unwrap(); bytes
    }
    fn orphan(state: &Path) -> PathBuf {
        let path = state.join("spool/flush.lock"); fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "abandoned-owner").unwrap(); age(&path); path
    }
    fn age(path: &Path) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now() - Duration::from_secs(61))).unwrap();
    }
    #[test] fn spool_caps_and_lock() {
        let tmp = crate::tests::Temp::new();
        { let mut writer = Writer::new(&tmp.path, u64::MAX).unwrap(); writer.append(&SpanRecord { span_key: "s".into(), ..Default::default() }).unwrap(); }
        assert_eq!(ready(&tmp.path).unwrap().len(), 1);
        enforce_cap(&tmp.path, 0).unwrap(); assert!(ready(&tmp.path).unwrap().is_empty()); assert_eq!(dropped(&tmp.path), 1);
        let lock = Lock::acquire(&tmp.path).unwrap().unwrap(); assert!(Lock::acquire(&tmp.path).unwrap().is_none()); lock.refresh().unwrap(); drop(lock);
        assert!(Lock::acquire(&tmp.path).unwrap().is_some());
    }
    #[test] fn stale_sender_lock_is_reclaimed() {
        let tmp = crate::tests::Temp::new(); let path = orphan(&tmp.path);
        let lock = Lock::acquire(&tmp.path).unwrap().unwrap(); assert!(path.exists());
        assert!(Lock::acquire(&tmp.path).unwrap().is_none()); drop(lock); assert!(!path.exists());
    }
    #[test] fn live_os_owner_outranks_backdated_mtime() {
        let tmp = crate::tests::Temp::new(); let first = Lock::acquire(&tmp.path).unwrap().unwrap(); let path = tmp.path.join("spool/flush.lock"); age(&path);
        assert!(Lock::acquire(&tmp.path).unwrap().is_none());
        let paths = Paths { state: tmp.path.clone(), config: tmp.path.join("config") }; assert!(!flush_eligible(&paths).unwrap());
        first.refresh().unwrap(); drop(first);
        assert!(Lock::acquire(&tmp.path).unwrap().is_some());
    }
    #[test] fn displaced_owner_drop_preserves_replacement_token() {
        let tmp = crate::tests::Temp::new(); let first = Lock::acquire(&tmp.path).unwrap().unwrap(); let path = tmp.path.join("spool/flush.lock");
        fs::write(&path, "replacement-owner").unwrap(); drop(first);
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement-owner");
    }
    #[test] fn concurrent_stale_contenders_enter_once() {
        let tmp = crate::tests::Temp::new(); let path = orphan(&tmp.path);
        let barrier = std::sync::Barrier::new(32);
        let locks = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..32).map(|_| { let state = &tmp.path; let barrier = &barrier; scope.spawn(move || { barrier.wait(); Lock::acquire(state).unwrap() }) }).collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(locks.iter().filter(|l| l.is_some()).count(), 1);
        assert!(Lock::acquire(&tmp.path).unwrap().is_none()); drop(locks); assert!(!path.exists());
    }
    #[test] fn reclamation_coordination_is_exclusive() {
        let tmp = crate::tests::Temp::new(); let path = orphan(&tmp.path);
        let guard = coordinate(&coordination_path(&path)).unwrap().unwrap();
        assert!(Lock::acquire(&tmp.path).unwrap().is_none()); assert_eq!(fs::read_to_string(&path).unwrap(), "abandoned-owner");
        drop(guard); assert!(Lock::acquire(&tmp.path).unwrap().is_some());
    }
    #[test] fn stale_owner_refresh_cannot_replace_fresh_token() {
        let tmp = crate::tests::Temp::new(); let first = Lock::acquire(&tmp.path).unwrap().unwrap(); let path = tmp.path.join("spool/flush.lock");
        fs::write(&path, "replacement-owner").unwrap(); let token = fs::read(&path).unwrap();
        assert!(first.refresh().is_err()); assert_eq!(fs::read(&path).unwrap(), token); drop(first); assert!(path.exists());
    }
    #[cfg(windows)]
    #[test] fn stale_lock_with_exclusive_handle_is_held_until_closed() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp=crate::tests::Temp::new();let path=orphan(&tmp.path);
        let file=OpenOptions::new().write(true).share_mode(0).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()-Duration::from_secs(61))).unwrap();
        assert!(Lock::acquire(&tmp.path).unwrap().is_none());drop(file);
        let second=Lock::acquire(&tmp.path).unwrap().unwrap();assert!(path.exists());drop(second);assert!(!path.exists());
    }
    #[test] fn partial_write_rolls_back_before_retry_and_publish() {
        let tmp = crate::tests::Temp::new(); let mut writer = Writer::new(&tmp.path, u64::MAX).unwrap();
        let rec = SpanRecord { span_key: "complete".into(), ..Default::default() }; writer.append(&rec).unwrap();
        let complete = writer_bytes(&mut writer);
        assert!(writer.append_bytes(b"{\"partial\":true}\n", |file, bytes| { file.write_all(&bytes[..5])?; Err(std::io::Error::other("injected short write")) }).is_err());
        assert_eq!(writer_bytes(&mut writer), complete);
        writer.append(&rec).unwrap(); writer.finish().unwrap();
        let files = ready(&tmp.path).unwrap(); assert_eq!(files.len(), 1);
        let mut expected = complete.clone(); expected.extend_from_slice(&complete); assert_eq!(fs::read(&files[0]).unwrap(), expected);
    }
    #[test] fn close_truncates_partial_tail_at_last_complete_record() {
        let tmp = crate::tests::Temp::new(); let mut writer = Writer::new(&tmp.path, u64::MAX).unwrap();
        let rec = SpanRecord::default(); let mut expected = serde_json::to_vec(&rec).unwrap(); expected.push(b'\n'); writer.append(&rec).unwrap();
        writer.file.as_mut().unwrap().write_all(b"{\"truncated\":").unwrap(); writer.finish().unwrap();
        assert_eq!(fs::read(&ready(&tmp.path).unwrap()[0]).unwrap(), expected);
    }
    #[test] fn ready_salvages_dead_writers_but_leaves_live_writers_untouched() {
        let tmp = crate::tests::Temp::new(); let mut live = Writer::new(&tmp.path, u64::MAX).unwrap(); live.append(&SpanRecord::default()).unwrap();
        let mut expected = serde_json::to_vec(&SpanRecord::default()).unwrap(); expected.push(b'\n');
        live.file.as_mut().unwrap().write_all(b"{\"live_tail\":").unwrap(); let live_len = live.file.as_ref().unwrap().metadata().unwrap().len();
        let mut child = Command::new(std::env::current_exe().unwrap()).arg("--list").stdout(Stdio::null()).spawn().unwrap(); let pid = child.id(); assert!(child.wait().unwrap().success()); assert!(!crate::proxy::live(pid));
        let dead = tmp.path.join("spool").join(format!("{pid}-101.jsonl")); let mut abandoned = expected.clone(); abandoned.extend_from_slice(b"{\"partial\":"); fs::write(&dead, abandoned).unwrap();
        let empty = tmp.path.join("spool").join(format!("{pid}-102.jsonl")); fs::write(&empty, b"{\"partial\":").unwrap();
        let unrelated = tmp.path.join("spool/unknown.jsonl"); fs::write(&unrelated, b"{}\n").unwrap();
        let files = ready(&tmp.path).unwrap(); assert_eq!(files, vec![dead.with_extension("ready")]); assert_eq!(fs::read(&files[0]).unwrap(), expected);
        assert!(!dead.exists()); assert!(!empty.exists()); assert!(unrelated.exists()); assert_eq!(live.path.metadata().unwrap().len(), live_len);
        assert_eq!(ready(&tmp.path).unwrap(), files); live.finish().unwrap();
    }
    #[test] fn concurrent_cap_closers_do_not_over_evict() {
        let tmp = crate::tests::Temp::new(); let rec = SpanRecord::default(); let size = serde_json::to_vec(&rec).unwrap().len() as u64 + 1;
        let mut writers = Vec::new(); for _ in 0..32 { let mut writer = Writer::new(&tmp.path, size * 4).unwrap(); writer.append(&rec).unwrap(); writers.push(writer); }
        let barrier = std::sync::Barrier::new(writers.len());
        std::thread::scope(|scope| { let threads: Vec<_> = writers.into_iter().map(|writer| { let barrier = &barrier; scope.spawn(move || { barrier.wait(); writer.finish().unwrap(); }) }).collect(); for thread in threads { thread.join().unwrap(); } });
        assert_eq!(ready(&tmp.path).unwrap().len(), 4); assert_eq!(dropped(&tmp.path), 28); assert!(cap_requests(&tmp.path).unwrap().is_empty());
    }
    #[test] fn cap_does_not_evict_while_sender_acknowledges() {
        let tmp = crate::tests::Temp::new(); for _ in 0..3 { let mut writer = Writer::new(&tmp.path, u64::MAX).unwrap(); writer.append(&SpanRecord::default()).unwrap(); writer.finish().unwrap(); }
        let files = ready(&tmp.path).unwrap(); let size = files[0].metadata().unwrap().len(); let acknowledgement = cap_lock(&tmp.path).unwrap().unwrap();
        std::thread::scope(|scope| { let state = &tmp.path; scope.spawn(move || enforce_cap(state, size).unwrap()).join().unwrap(); });
        assert_eq!(dropped(&tmp.path), 0); // Eviction cannot enter this acknowledgement critical section.
        fs::remove_file(&files[1]).unwrap(); fs::remove_file(&files[2]).unwrap();
        assert!(files[0].exists()); assert_eq!(dropped(&tmp.path), 0); drop(acknowledgement); assert!(files[0].exists()); assert_eq!(dropped(&tmp.path), 0); assert!(cap_requests(&tmp.path).unwrap().is_empty());
    }
    #[test] fn held_cap_publications_are_enforced_on_owner_release() {
        let tmp = crate::tests::Temp::new(); let rec = SpanRecord::default(); let size = serde_json::to_vec(&rec).unwrap().len() as u64 + 1;
        for _ in 0..2 { let mut writer = Writer::new(&tmp.path, size * 2).unwrap(); writer.append(&rec).unwrap(); writer.finish().unwrap(); }
        let owner = cap_lock(&tmp.path).unwrap().unwrap(); assert_eq!(ready(&tmp.path).unwrap().len(), 2);
        std::thread::scope(|scope| {
            let state = &tmp.path; let rec = &rec;
            scope.spawn(move || { for _ in 0..3 { let mut writer = Writer::new(state, size * 2).unwrap(); writer.append(rec).unwrap(); writer.finish().unwrap(); } }).join().unwrap();
        });
        assert_eq!(ready(&tmp.path).unwrap().len(), 5); assert_eq!(dropped(&tmp.path), 0); assert_eq!(cap_requests(&tmp.path).unwrap().len(), 3);
        drop(owner); // No manual enforcement: ownership release must perform the handoff.
        assert_eq!(ready(&tmp.path).unwrap().len(), 2); assert_eq!(dropped(&tmp.path), 3); assert!(cap_requests(&tmp.path).unwrap().is_empty());
    }
    #[test] fn request_between_empty_check_and_unlock_is_not_stranded() {
        let tmp = crate::tests::Temp::new(); let rec = SpanRecord::default();
        let mut late = Writer::new(&tmp.path, 0).unwrap(); late.append(&rec).unwrap(); let mut late = Some(late);
        let mut owner = cap_lock(&tmp.path).unwrap().unwrap();
        owner.release_with(|| { if let Some(writer) = late.take() { writer.finish()?; assert_eq!(ready(&tmp.path)?.len(), 1); } Ok(()) }).unwrap();
        assert!(ready(&tmp.path).unwrap().is_empty()); assert_eq!(dropped(&tmp.path), 1); assert!(cap_requests(&tmp.path).unwrap().is_empty());
    }
    #[test] fn cap_requests_never_publish_or_delete_unowned_contents() {
        let tmp = crate::tests::Temp::new(); let owner = cap_lock(&tmp.path).unwrap().unwrap();
        let directory = tmp.path.join("spool/cap-requests"); fs::create_dir_all(&directory).unwrap();
        let unowned = directory.join("cap-0-00000000000000000000000000000000.request"); fs::write(&unowned, b"unrelated contents").unwrap();
        enforce_cap(&tmp.path, 0).unwrap(); drop(owner);
        assert_eq!(fs::read(&unowned).unwrap(), b"unrelated contents"); assert!(ready(&tmp.path).unwrap().is_empty()); assert_eq!(dropped(&tmp.path), 0); assert!(cap_requests(&tmp.path).unwrap().is_empty());
    }
    #[test] fn stale_sender_lock_allows_spawn_eligibility() {
        let tmp = crate::tests::Temp::new(); let paths = Paths { state: tmp.path.clone(), config: tmp.path.join("config") };
        assert!(!flush_eligible(&paths).unwrap()); let mut writer = Writer::new(&paths.state, u64::MAX).unwrap(); writer.append(&SpanRecord::default()).unwrap(); writer.finish().unwrap();
        let lock = Lock::acquire(&paths.state).unwrap().unwrap(); assert!(!flush_eligible(&paths).unwrap());
        drop(lock); orphan(&paths.state); assert!(flush_eligible(&paths).unwrap());
    }
    #[test] fn settlement_markers_trigger_only_registered_settlers() {
        fn hook(_: &mut crate::record::HookCtx,_:&[u8])->Result<()> {Ok(())}
        fn plan(_: &crate::setup::SetupCtx)->Result<crate::setup::Plan> {Ok(Default::default())}
        fn settle(_: &mut crate::record::HookCtx)->Result<bool> {Ok(false)}
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=crate::record::HookCtx::new(paths.clone(),crate::config::Config::default(),"deferred",Default::default(),None,false).unwrap();let mut spec=crate::agents::AgentSpec {kind:"deferred",display:"deferred",hook,plan,reply:None,settle:Some(settle)};
        assert!(!settle_pending(&paths.state,std::slice::from_ref(&spec)));ctx.mark_settle_pending().unwrap();assert!(settle_pending(&paths.state,std::slice::from_ref(&spec)));spec.settle=None;assert!(!settle_pending(&paths.state,std::slice::from_ref(&spec)));assert!(!settle_pending(&paths.state,&[]));
        assert_eq!(has_settle_pending(&paths.state), settle_pending(&paths.state, crate::agents::AGENTS));
    }
    #[test] fn lock_drop_releases_duplicated_descriptor_before_reacquisition() {
        let tmp=crate::tests::Temp::new();let held=Lock::acquire(&tmp.path).unwrap().unwrap();let inherited=held._guard.try_clone().unwrap();drop(held);let next=Lock::acquire(&tmp.path).unwrap();assert!(next.is_some());drop(next);drop(inherited);
    }
}
