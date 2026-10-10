use crate::{record::HookCtx, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{path::PathBuf,time::{Duration,Instant}};
fn path(ctx: &HookCtx, key: &str) -> Result<PathBuf> {
    if key.is_empty() || key == "." || key == ".." || !key.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) { return Err("state key must be a leaf filename".into()); }
    Ok(ctx.state_dir().join(key))
}
/// Owner-token guard; dropping it releases only this process's lock instance.
/// Locks older than 30 seconds may be reclaimed by another caller.
pub struct StateLock { _lock: crate::spool::Lock }
impl HookCtx {
    /// Request a detached settlement pass even when this hook has not produced spool records yet.
    pub fn mark_settle_pending(&self)->Result<()> {
        crate::paths::atomic_json(&self.state_dir().join("settle-pending"),&crate::record::random_id()?)
    }
    /// Non-blocking variant for latency-sensitive hooks; None means a live lock is held.
    pub fn try_lock(&self,name:&str)->Result<Option<StateLock>> {
        let path=path(self,name)?.with_file_name(format!("{name}.lock"));
        Ok(crate::spool::Lock::acquire_at(&path,Duration::from_secs(30))?.map(|lock|StateLock {_lock:lock}))
    }
    /// Serialize native translation with a leaf-named lock; acquisition waits at most two seconds.
    pub fn lock(&self,name:&str)->Result<StateLock> {
        let path=path(self,name)?.with_file_name(format!("{name}.lock"));let deadline=Instant::now()+Duration::from_secs(2);
        loop {
            if let Some(lock)=crate::spool::Lock::acquire_at(&path,Duration::from_secs(30))? {return Ok(StateLock {_lock:lock});}
            let remaining=deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {return Err("adapter state lock acquisition timed out".into());}
            std::thread::sleep(remaining.min(Duration::from_millis(2)));
        }
    }
    pub fn load<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        match std::fs::read(path(self,key)?) { Ok(bytes) => Ok(serde_json::from_slice(&bytes)?), Err(e) if e.kind()==std::io::ErrorKind::NotFound => Ok(T::default()), Err(e) => Err(e.into()) }
    }
    pub fn save<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let mut value=serde_json::to_value(value)?;
        self.scrub_value(&mut value);
        crate::paths::atomic_json(&path(self,key)?, &value)
    }
}

#[cfg(test)] mod tests {
    use super::*;
    use std::{sync::mpsc,fs::FileTimes};
    fn ctx(paths:crate::paths::Paths)->HookCtx {HookCtx::new(paths,crate::config::Config::default(),"fake",Default::default(),None,false).unwrap()}
    #[test] fn two_contending_threads_try_lock_and_raii_release() {
        let tmp=crate::tests::Temp::new();let paths=crate::paths::Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let first=ctx(paths.clone());let held=first.lock("session").unwrap();assert!(first.try_lock("session").unwrap().is_none());
        let (started,ready)=mpsc::channel();let (acquired,received)=mpsc::channel();let other=std::thread::spawn(move|| {let ctx=ctx(paths);started.send(()).unwrap();let guard=ctx.lock("session").unwrap();acquired.send(()).unwrap();drop(guard);});
        ready.recv().unwrap();assert!(received.recv_timeout(Duration::from_millis(20)).is_err());drop(held);received.recv_timeout(Duration::from_secs(1)).unwrap();other.join().unwrap();assert!(!first.state_dir().join("session.lock").exists());assert!(first.try_lock("session").unwrap().is_some());
    }
    #[test] fn stale_recovery_owner_tokens_and_leaf_validation() {
        let tmp=crate::tests::Temp::new();let ctx=ctx(crate::paths::Paths {state:tmp.path.join("state"),config:tmp.path.join("config")});let old=ctx.try_lock("stale").unwrap().unwrap();let path=ctx.state_dir().join("stale.lock");
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(std::time::SystemTime::now()-Duration::from_secs(31))).unwrap();let replacement=ctx.try_lock("stale").unwrap().unwrap();drop(old);assert!(path.exists());drop(replacement);assert!(!path.exists());
        for name in ["../outside","a/b",".","..",""] {assert!(ctx.try_lock(name).is_err());assert!(ctx.lock(name).is_err());}
    }
    #[test] fn blocking_acquisition_has_a_two_second_deadline() {
        let tmp=crate::tests::Temp::new();let ctx=ctx(crate::paths::Paths {state:tmp.path.join("state"),config:tmp.path.join("config")});let _held=ctx.lock("held").unwrap();let start=Instant::now();assert!(ctx.lock("held").is_err());let elapsed=start.elapsed();assert!(elapsed>=Duration::from_secs(2)&&elapsed<Duration::from_secs(3));
    }
    #[test] fn settlement_marker_is_private_and_refreshable() {
        let tmp=crate::tests::Temp::new();let ctx=ctx(crate::paths::Paths {state:tmp.path.join("state"),config:tmp.path.join("config")});ctx.mark_settle_pending().unwrap();let path=ctx.state_dir().join("settle-pending");let first=std::fs::read(&path).unwrap();ctx.mark_settle_pending().unwrap();assert_ne!(first,std::fs::read(&path).unwrap());
        #[cfg(unix)] {use std::os::unix::fs::MetadataExt;assert_eq!(std::fs::metadata(path).unwrap().mode()&0o777,0o600);}
    }
}
