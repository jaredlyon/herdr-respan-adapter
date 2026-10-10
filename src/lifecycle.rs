use crate::{config::Config, paths::Paths, record::{HookCtx, SpanRecord}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap,path::Path,time::Duration};
#[derive(Serialize, Deserialize)]
struct Run { agent: String, start_ns: u64, key: String, metadata: BTreeMap<String, Value> }
#[derive(Serialize,Deserialize)]
struct Transition {pane:String,workspace:Option<String>,agent:Option<String>,status:String,ts_ns:u64}
fn pane_key(pane:&str)->String {format!("{:x}",Sha256::digest(pane.as_bytes()))}
/// Event hooks enqueue first: a concurrent idle/working hook never races a read/emit/delete.
/// The envelope uses snake_case names; manifest events use dotted names. Both are accepted.
pub fn handle(paths:&Paths,config:Config,event:&Value)->Result<()> {
    let fields=event.get("data").unwrap_or(event);let pane=fields.get("pane_id").and_then(Value::as_str).ok_or("missing pane id")?;
    let workspace=fields.get("workspace_id").and_then(Value::as_str);
    if !crate::gate::allowed(&paths.state,std::env::var("HERDR_ENV").ok().as_deref(),std::env::var("HERDR_RESPAN").ok().as_deref(),workspace) {return Ok(());}
    let event_name=[event.get("event"),fields.get("type")].into_iter().flatten().filter_map(Value::as_str).next().unwrap_or_default();
    let status=if matches!(event_name,"pane.exited"|"pane_exited") {"exited"} else {fields.get("agent_status").and_then(Value::as_str).unwrap_or_default()};
    if !["working","idle","done","blocked","exited"].contains(&status) {return Ok(());}
    let agent=fields.get("agent").and_then(|v|v.as_str().or_else(||v.get("kind").and_then(Value::as_str))).map(str::to_string);
    let transition=Transition {pane:pane.into(),workspace:workspace.map(str::to_string),agent,status:status.into(),ts_ns:crate::record::now_ns()};
    let key=pane_key(pane);enqueue(paths,&key,&transition)?;process_pane(paths,&config,&key)?;
    if !cfg!(test) {crate::spool::spawn_flush(paths)?;}Ok(())
}
fn enqueue(paths:&Paths,key:&str,transition:&Transition)->Result<()> {
    let path=paths.state.join("lifecycle").join(key).join(format!("pending-{}-{}.json",transition.ts_ns,crate::record::random_id()?));
    crate::paths::atomic_json(&path,transition)
}
fn pending(directory:&Path)->Result<Vec<std::path::PathBuf>> {
    let entries=match std::fs::read_dir(directory) {Ok(e)=>e,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(Vec::new()),Err(e)=>return Err(e.into())};
    let mut files=entries.filter_map(|e|e.ok().map(|e|e.path())).filter(|p|p.file_name().is_some_and(|n|n.to_string_lossy().starts_with("pending-"))&&p.extension().is_some_and(|e|e=="json")).collect::<Vec<_>>();files.sort();Ok(files)
}
fn process_pane(paths:&Paths,config:&Config,key:&str)->Result<()> {
    let directory=paths.state.join("lifecycle");let lock=directory.join(format!("{key}.lock"));
    let Some(_lock)=crate::spool::Lock::acquire_at(&lock,Duration::from_secs(30))? else {return Ok(());};
    let file=directory.join(format!("{key}.json"));
    for queued in pending(&directory.join(key))? {
        let event:Transition=serde_json::from_slice(&std::fs::read(&queued)?)?;
        let previous=match std::fs::read(&file) {Ok(b)=>Some(serde_json::from_slice::<Run>(&b)?),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>None,Err(e)=>return Err(e.into())};
        let kind=event.agent.as_deref().or_else(||previous.as_ref().map(|r|r.agent.as_str())).unwrap_or("unknown");
        if crate::setup::installed(&paths.state,kind) {let _=std::fs::remove_file(&file);std::fs::remove_file(queued)?;continue;}
        if event.status=="working" {
            if previous.is_none() {
                let metadata=BTreeMap::from([("herdr.pane_id".into(),json!(event.pane)),("herdr.workspace_id".into(),json!(event.workspace.unwrap_or_default())),("herdr.machine".into(),json!(crate::meta::hostname())),("plugin_version".into(),json!(env!("CARGO_PKG_VERSION")))]);
                crate::paths::atomic_json(&file,&Run {agent:kind.into(),start_ns:event.ts_ns,key:format!("lifecycle:{}:{}",event.pane,event.ts_ns),metadata})?;
            }
        } else if let Some(run)=previous {
            let mut ctx=HookCtx::new(paths.clone(),config.clone(),&run.agent,run.metadata,None,false)?;
            ctx.emit(SpanRecord {trace_key:Some(run.key.clone()),span_key:run.key,name:format!("{} run",run.agent),source:"lifecycle".into(),start_ns:run.start_ns,end_ns:event.ts_ns.max(run.start_ns),metadata:BTreeMap::from([("final_status".into(),json!(event.status))]),..Default::default()})?;
            ctx.finish()?;std::fs::remove_file(&file)?;
        }
        std::fs::remove_file(queued)?;
    }Ok(())
}
pub(crate) fn has_pending(state:&Path)->Result<bool> {
    let entries=match std::fs::read_dir(state.join("lifecycle")) {Ok(e)=>e,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(false),Err(e)=>return Err(e.into())};
    for entry in entries {let entry=entry?;if entry.file_type()?.is_dir()&&!pending(&entry.path())?.is_empty() {return Ok(true);}}Ok(false)
}
pub(crate) fn at_flush(paths:&Paths,config:&Config)->Result<()> {
    let entries=match std::fs::read_dir(paths.state.join("lifecycle")) {Ok(e)=>e,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into())};
    for entry in entries {let entry=entry?;if !entry.file_type()?.is_dir() {continue;}let key=entry.file_name().to_string_lossy().into_owned();if key.len()==64&&key.bytes().all(|b|b.is_ascii_hexdigit()) {process_pane(paths,config,&key)?;}}Ok(())
}
#[cfg(test)] mod tests {
    use super::*;
    fn transition(status:&str,ts_ns:u64)->Transition {Transition {pane:"p".into(),workspace:Some("w".into()),agent:Some("other".into()),status:status.into(),ts_ns}}
    #[test] fn per_pane_contention_queues_working_idle_working_without_losing_the_second_run() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let config=Config::default();let key=pane_key("p");let file=paths.state.join("lifecycle").join(format!("{key}.json"));let held=crate::spool::Lock::acquire_at(&file.with_extension("lock"),Duration::from_secs(30)).unwrap().unwrap();
        for (status,time) in [("working",1),("idle",2),("working",3)] {enqueue(&paths,&key,&transition(status,time)).unwrap();process_pane(&paths,&config,&key).unwrap();}
        assert!(!file.exists());assert!(has_pending(&paths.state).unwrap());drop(held);at_flush(&paths,&config).unwrap();let run:Run=serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();assert_eq!(run.start_ns,3);assert_eq!(crate::spool::ready(&paths.state).unwrap().len(),1);
        enqueue(&paths,&key,&transition("idle",4)).unwrap();at_flush(&paths,&config).unwrap();assert!(!file.exists());assert!(!has_pending(&paths.state).unwrap());assert_eq!(crate::spool::ready(&paths.state).unwrap().len(),2);
    }
}
