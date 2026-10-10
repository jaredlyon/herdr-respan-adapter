use crate::Result;
use serde::{Deserialize, Serialize};
use std::{io::{BufRead, BufReader, Read, Seek, SeekFrom}, path::Path};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor { pub offset: u64, pub len_seen: u64, #[serde(default)] pub identity: Option<String> }
pub fn read_new(path: &Path, cursor: &mut Cursor) -> Result<Vec<String>> {
    let mut next=cursor.clone();
    let mut file=std::fs::File::open(path)?;let metadata=file.metadata()?;let len=metadata.len();
    let identity=identity(&metadata);
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
fn identity(metadata:&std::fs::Metadata)->Option<String> {
    #[cfg(unix)] {use std::os::unix::fs::MetadataExt;Some(format!("{}:{}",metadata.dev(),metadata.ino()))}
    #[cfg(not(unix))] {metadata.created().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d|d.as_nanos().to_string())}
}
/// Skip existing complete records, retaining an incomplete tail for the next read_new call.
/// Reads backwards in bounded chunks, stopping at the final newline.
pub fn at_end(path:&Path)->Result<Cursor> {
    let mut file=match std::fs::File::open(path) {Ok(file)=>file,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(Cursor::default()),Err(e)=>return Err(e.into())};
    let metadata=file.metadata()?;let len=metadata.len();let mut end=len;let mut bytes=[0;4096];let mut offset=0;
    while end>0 {
        let start=end.saturating_sub(bytes.len() as u64);let count=(end-start) as usize;
        file.seek(SeekFrom::Start(start))?;file.read_exact(&mut bytes[..count])?;
        if let Some(newline)=bytes[..count].iter().rposition(|&b|b==b'\n') {offset=start+newline as u64+1;break;}
        end=start;
    }
    Ok(Cursor {offset,len_seen:len,identity:identity(&metadata)})
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
    #[test] fn at_end_uses_last_complete_newline_and_retains_partial_tail() {
        use std::io::Write;
        let tmp=crate::tests::Temp::new();let path=tmp.path.join("events.jsonl");let missing=at_end(&path).unwrap();assert_eq!(missing.offset,0);assert_eq!(missing.len_seen,0);assert!(missing.identity.is_none());
        for (bytes,expected) in [(b"".as_slice(),0),(b"partial".as_slice(),0),(b"first\nsecond\n".as_slice(),13),(b"first\nsecond\npartial".as_slice(),13)] {
            std::fs::write(&path,bytes).unwrap();let cursor=at_end(&path).unwrap();assert_eq!(cursor.offset,expected);assert_eq!(cursor.len_seen,bytes.len() as u64);assert_eq!(cursor.identity,identity(&std::fs::metadata(&path).unwrap()));
        }
        let mut bytes=vec![b'x';100_000];bytes.extend_from_slice(b"\npartial");std::fs::write(&path,&bytes).unwrap();let mut cursor=at_end(&path).unwrap();assert_eq!(cursor.offset,100_001);
        let mut file=std::fs::OpenOptions::new().append(true).open(&path).unwrap();file.write_all(b"-finish\n").unwrap();drop(file);assert_eq!(read_new(&path,&mut cursor).unwrap(),vec!["partial-finish"]);
    }
}
