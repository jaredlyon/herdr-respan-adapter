use crate::Result;
use std::{collections::BTreeMap, path::{Path, PathBuf}};
#[derive(Clone)]
pub struct Staged { pub before: Option<Vec<u8>>, pub after: Option<Vec<u8>> }
pub type Files = BTreeMap<PathBuf, Staged>;
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) { Ok(b) => Ok(Some(b)), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(e) => Err(e.into()) }
}
pub fn get<'a>(files: &'a mut Files, path: &Path) -> Result<&'a mut Staged> {
    if !files.contains_key(path) {
        let before = read(path)?;
        files.insert(path.into(), Staged { after: before.clone(), before });
    }
    Ok(files.get_mut(path).expect("staged file"))
}
fn put(path: &Path, value: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = value {
        let tmp = path.with_extension(format!("herdr-respan-{}-{}", std::process::id(), crate::record::now_ns()));
        crate::paths::write_private(&tmp, bytes)?;
        if let Ok(metadata) = path.metadata() { std::fs::set_permissions(&tmp, metadata.permissions())?; }
        if let Err(e) = std::fs::rename(&tmp, path) { let _ = std::fs::remove_file(&tmp); return Err(e.into()); }
    } else { match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) } }
    Ok(())
}
/// Persist only operations plus committed ownership; foreign snapshots cannot enter this API.
pub(super) fn journal(owned: &super::owned::Owned, state: &Path) -> Result<super::owned::Owned> {
    let old = super::owned::Owned::load(state)?;
    super::owned::Owned { entries: old.entries.clone(), pending: owned.pending.clone() }.save(state)?;
    Ok(old)
}
/// The caller holds the state transaction lock from ownership load through this save.
pub fn commit(files: &Files, owned: &super::owned::Owned, state: &Path, lock: &crate::spool::Lock) -> Result<()> {
    lock.refresh()?;
    // Snapshots stay in memory only, and are checked before touching a foreign file.
    for (path, staged) in files {
        if read(path)? != staged.before { return Err("config changed while awaiting confirmation".into()); }
    }
    let old = journal(owned, state)?;
    let mut applied: Vec<(&PathBuf, &Staged)> = Vec::new();
    for (path, staged) in files {
        if staged.after == staged.before { continue; }
        lock.refresh()?;
        let result = if read(path)? != staged.before { Err("config changed during setup transaction".into()) } else { put(path, staged.after.as_deref()) };
        if let Err(e) = result {
            let mut rollback_ok = true;
            for (path, staged) in applied.iter().rev() {
                // Never clobber a foreign edit made after our successful write.
                rollback_ok &= read(path).is_ok_and(|b| b == staged.after) && put(path, staged.before.as_deref()).is_ok();
            }
            if rollback_ok { old.save(state)?; }
            return Err(e);
        }
        applied.push((path, staged));
    }
    let mut committed = owned.clone();
    committed.pending = None;
    committed.save(state)
}
