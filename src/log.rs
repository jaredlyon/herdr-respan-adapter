use std::{fs::{self, OpenOptions}, io::Write, path::Path};
pub fn error(state: &Path, message: &'static str) { write(state, "ERROR", message); }
pub fn debug(state: &Path, message: &'static str) {
    if std::env::var("HERDR_RESPAN_LOG").ok().as_deref() == Some("debug") { write(state, "DEBUG", message); }
}
fn write(state: &Path, level: &str, message: &str) {
    // Only static operation names belong here. Never format payloads, errors, paths or keys.
    let _ = fs::create_dir_all(state); let path = state.join("herdr-respan.log");
    if path.metadata().is_ok_and(|m| m.len() >= 1024*1024) {
        let rotated = state.join("herdr-respan.log.1"); let _ = fs::remove_file(&rotated); let _ = fs::rename(&path, rotated);
    }
    let mut options = OpenOptions::new(); options.create(true).append(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    if let Ok(mut file) = options.open(path) { let _ = writeln!(file, "{} [{level}] {message}", crate::record::now_ns()); }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn rotates_two_files() {
        let tmp = crate::tests::Temp::new(); std::fs::write(tmp.path.join("herdr-respan.log"), vec![b'x'; 1024*1024]).unwrap();
        error(&tmp.path,"test error"); assert!(tmp.path.join("herdr-respan.log.1").exists()); assert!(std::fs::metadata(tmp.path.join("herdr-respan.log")).unwrap().len()<100);
    }
}
