use crate::Result;
use serde::{Deserialize, Serialize};
use std::{io::{BufRead, BufReader, Seek, SeekFrom}, path::Path};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor { pub offset: u64, pub len_seen: u64, #[serde(default)] pub identity: Option<String> }
pub fn read_new(path: &Path, cursor: &mut Cursor) -> Result<Vec<String>> {
    let mut next=cursor.clone();
    let mut file=std::fs::File::open(path)?;let metadata=file.metadata()?;let len=metadata.len();
    #[cfg(unix)] let identity={use std::os::unix::fs::MetadataExt;Some(format!("{}:{}",metadata.dev(),metadata.ino()))};
    #[cfg(not(unix))] let identity=metadata.created().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d|d.as_nanos().to_string());
    if len<next.len_seen || next.offset>len || next.identity!=identity { next.offset=0; }
    next.identity=identity;
    file.seek(SeekFrom::Start(next.offset))?;let mut reader=BufReader::new(file);let mut lines=Vec::new();
    loop {
        let mut bytes=Vec::new();let count=reader.read_until(b'\n',&mut bytes)?;
        if count==0 || bytes.last()!=Some(&b'\n') { break; }
        bytes.pop();if bytes.last()==Some(&b'\r') {bytes.pop();}
        let line=String::from_utf8(bytes)?;next.offset+=count as u64;lines.push(line);
    }
    next.len_seen=len;*cursor=next;Ok(lines)
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn partial_tail_and_truncation() {
        let tmp=crate::tests::Temp::new();let path=tmp.path.join("events.jsonl");std::fs::write(&path,b"one\npar").unwrap();let mut c=Cursor::default();
        assert_eq!(read_new(&path,&mut c).unwrap(),vec!["one"]);assert_eq!(c.offset,4);assert!(read_new(&path,&mut c).unwrap().is_empty());
        std::fs::write(&path,b"one\npartial\r\n").unwrap();assert_eq!(read_new(&path,&mut c).unwrap(),vec!["partial"]);
        std::fs::write(&path,b"new\n").unwrap();assert_eq!(read_new(&path,&mut c).unwrap(),vec!["new"]);assert_eq!(c.offset,4);
    }
    #[test] fn replacement_rotation_restarts_cursor() {
        let tmp=crate::tests::Temp::new();let path=tmp.path.join("events.jsonl");let replacement=tmp.path.join("replacement");std::fs::write(&path,b"old\n").unwrap();let mut cursor=Cursor::default();read_new(&path,&mut cursor).unwrap();
        std::fs::write(&replacement,b"new-longer\n").unwrap();std::fs::rename(replacement,&path).unwrap();assert_eq!(read_new(&path,&mut cursor).unwrap(),vec!["new-longer"]);
    }
    #[test] fn invalid_complete_utf8_does_not_advance_cursor() {
        let tmp=crate::tests::Temp::new();let path=tmp.path.join("events.jsonl");std::fs::write(&path,b"good\n\xff\n").unwrap();let mut cursor=Cursor::default();assert!(read_new(&path,&mut cursor).is_err());assert_eq!(cursor.offset,0);
    }
}
