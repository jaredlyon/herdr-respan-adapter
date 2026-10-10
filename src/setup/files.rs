use crate::Result;
use std::{collections::BTreeMap, path::{Path, PathBuf}};
#[derive(Clone)]
pub struct Staged { pub before: Option<Vec<u8>>, pub after: Option<Vec<u8>> }
pub type Files = BTreeMap<PathBuf, Staged>;
pub fn get<'a>(files: &'a mut Files, path: &Path) -> Result<&'a mut Staged> {
    if !files.contains_key(path) {
        let before = match std::fs::read(path) { Ok(b) => Some(b), Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, Err(e) => return Err(e.into()) };
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
pub fn commit(files: &Files, owned: &super::owned::Owned, state: &Path) -> Result<()> {
    // Verify snapshots before changing anything; never overwrite intervening user edits.
    for (path, staged) in files {
        let current = match std::fs::read(path) { Ok(b) => Some(b), Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, Err(e) => return Err(e.into()) };
        if current != staged.before { return Err("config changed while awaiting confirmation".into()); }
    }
    let old = super::owned::Owned::load(state)?;
    // Journal ownership first: a crash never leaves an inserted value unowned.
    let mut journal = old.clone();
    for entry in &owned.entries { if !journal.entries.contains(entry) { journal.entries.push(entry.clone()); } }
    journal.save(state)?;
    let mut applied: Vec<(&PathBuf, &Staged)> = Vec::new();
    for (path, staged) in files {
        if staged.after == staged.before { continue; }
        if let Err(e) = put(path, staged.after.as_deref()) {
            let mut rollback_ok = true;
            for (path, staged) in applied.iter().rev() { rollback_ok &= put(path, staged.before.as_deref()).is_ok(); }
            if rollback_ok { old.save(state)?; }
            return Err(e);
        }
        applied.push((path, staged));
    }
    owned.save(state)?;
    Ok(())
}
