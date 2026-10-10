use crate::{config::Config, paths::Paths, record::{HookCtx, SpanRecord, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap,BTreeSet,VecDeque}, path::{Path,PathBuf}};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub v: u8, pub agent: String, pub session_id: String,
    pub session_path: Option<String>, pub cwd: Option<String>, pub agent_version: Option<String>,
    pub events: Vec<Event>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag="type",rename_all="snake_case")]
pub enum Event {
    TurnStart {turn_id:String,ts_ns:u64,prompt:Option<Value>,#[serde(default)] metadata:BTreeMap<String,Value>},
    Chat {turn_id:String,id:String,parent_id:Option<String>,start_ns:u64,end_ns:u64,model:Option<String>,provider:Option<String>,input:Option<Value>,output:Option<Value>,usage:Option<Usage>,cost_usd:Option<f64>,aux_task:Option<String>,status:Option<String>,error:Option<String>,#[serde(default)] metadata:BTreeMap<String,Value>},
    Tool {turn_id:String,id:String,parent_id:Option<String>,name:Option<String>,start_ns:u64,end_ns:u64,input:Option<Value>,output:Option<Value>,status:Option<String>,error:Option<String>,#[serde(default)] metadata:BTreeMap<String,Value>},
    SubagentStart {turn_id:String,id:String,parent_id:Option<String>,ts_ns:u64,name:Option<String>,#[serde(default)] metadata:BTreeMap<String,Value>},
    SubagentEnd {turn_id:String,id:String,ts_ns:u64,status:Option<String>,output:Option<Value>},
    TurnEnd {turn_id:String,ts_ns:u64,output:Option<Value>,status:Option<String>,error:Option<String>},
}
impl Event {
    fn turn_id(&self) -> &str {match self {Self::TurnStart {turn_id,..}|Self::Chat {turn_id,..}|Self::Tool {turn_id,..}|Self::SubagentStart {turn_id,..}|Self::SubagentEnd {turn_id,..}|Self::TurnEnd {turn_id,..}=>turn_id}}
    fn key(&self,trace:&str)->String {match self {Self::TurnStart {..}=>format!("{trace}:start"),Self::TurnEnd {..}=>format!("{trace}:end"),Self::Chat {id,..}=>format!("{trace}:chat:{id}"),Self::Tool {id,..}=>format!("{trace}:tool:{id}"),Self::SubagentStart {id,..}=>format!("{trace}:sub:{id}:start"),Self::SubagentEnd {id,..}=>format!("{trace}:sub:{id}:end")}}
    fn scrub(&mut self,ctx:&HookCtx)->Result<()> {match self {
        Self::TurnStart {prompt,..}=>ctx.scrub_content(prompt),
        Self::Chat {input,output,..}|Self::Tool {input,output,..}=>{ctx.scrub_content(input)?;ctx.scrub_content(output)},
        Self::TurnEnd {output,..}|Self::SubagentEnd {output,..}=>ctx.scrub_content(output),
        Self::SubagentStart {..}=>Ok(()),
    }}
    fn validate(&self)->Result<()> {
        if self.turn_id().is_empty() {return Err("bridge turn_id is empty".into());}
        match self {
            Self::Chat {id,start_ns,end_ns,status,..}|Self::Tool {id,start_ns,end_ns,status,..}=>{if id.is_empty()||end_ns<start_ns {return Err("invalid bridge child event".into());}validate_status(status)?;},
            Self::SubagentStart {id,..}|Self::SubagentEnd {id,..} if id.is_empty()=>return Err("bridge child id is empty".into()),
            Self::TurnEnd {status,..}|Self::SubagentEnd {status,..}=>validate_status(status)?,_=>{},
        }Ok(())
    }
}
fn validate_status(status:&Option<String>)->Result<()> {if status.as_deref().is_some_and(|s| s!="ok"&&s!="error") {Err("invalid bridge status".into())} else {Ok(())}}
#[derive(Serialize,Deserialize)]
struct Envelope {batch:Batch,metadata:BTreeMap<String,Value>,run_id:Option<String>,proxy_active:bool}
#[derive(Default,Serialize,Deserialize)]
struct Open {turns:BTreeMap<String,Turn>}
#[derive(Default,Serialize,Deserialize)]
struct Turn {
    session_id:String,metadata:BTreeMap<String,Value>,#[serde(default)] root_metadata:BTreeMap<String,Value>,run_id:Option<String>,start:Option<u64>,explicit_start:bool,end:Option<u64>,latest_end:u64,
    prompt:Option<Value>,output:Option<Value>,status:Option<String>,error:Option<String>,emitted:bool,touched:u64,subs:BTreeMap<String,Sub>,
}
#[derive(Default,Serialize,Deserialize)]
struct Sub {#[serde(default)] metadata:BTreeMap<String,Value>,start:Option<u64>,end:Option<u64>,parent_id:Option<String>,name:Option<String>,status:Option<String>,output:Option<Value>,emitted:bool}
const SEEN_LIMIT:usize=8192;
struct Seen {order:VecDeque<String>,set:BTreeSet<String>}
impl Seen {
    fn load(ctx:&HookCtx)->Result<Self> {let order:VecDeque<String>=ctx.load("seen")?;let set=order.iter().cloned().collect();Ok(Self {order,set})}
    fn add(&mut self,key:String) {if self.set.insert(key.clone()) {self.order.push_back(key);while self.order.len()>SEEN_LIMIT {if let Some(old)=self.order.pop_front() {self.set.remove(&old);}}}}
}
struct Lock {_lock:crate::spool::Lock}
impl Lock {
    fn acquire(dir:&Path)->Result<Option<Self>> {
        Ok(crate::spool::Lock::acquire_at(&dir.join("bridge.lock"),std::time::Duration::from_secs(60))?.map(|lock|Self {_lock:lock}))
    }
}
pub fn hook(ctx:&mut HookCtx,stdin:&[u8])->Result<()> {ingest(ctx,serde_json::from_slice(stdin)?)}
pub fn ingest(ctx:&mut HookCtx,mut batch:Batch)->Result<()> {
    if batch.v!=1||batch.agent!=ctx.agent||batch.session_id.is_empty() {return Err("invalid bridge batch identity/version".into());}
    for event in &mut batch.events {event.validate()?;event.scrub(ctx)?;}
    // One atomic inbox file per caller prevents concurrent hook processes from losing events.
    let name=format!("pending-{}-{}.json",ctx.now_ns(),crate::record::random_id()?);
    let envelope=Envelope {batch,metadata:ctx.metadata.clone(),run_id:ctx.run_id.clone(),proxy_active:ctx.proxy_active};
    ctx.save(&name,&envelope)?;let now=ctx.now_ns();drain(ctx,false,now)
}
fn pending(dir:&Path)->Result<Vec<PathBuf>> {
    let mut files=std::fs::read_dir(dir)?.filter_map(|e|e.ok().map(|e|e.path())).filter(|p|p.file_name().is_some_and(|n|n.to_string_lossy().starts_with("pending-")&&p.extension().is_some_and(|x|x=="json"))).collect::<Vec<_>>();files.sort();Ok(files)
}
fn drain(ctx:&mut HookCtx,flush:bool,now:u64)->Result<()> {
    let Some(_lock)=Lock::acquire(ctx.state_dir())? else {return Ok(());};
    let mut open:Open=ctx.load("open.json")?;let mut seen=Seen::load(ctx)?;let files=pending(ctx.state_dir())?;
    let old_proxy=ctx.proxy_active;
    for file in &files {
        let envelope:Envelope=serde_json::from_slice(&std::fs::read(file)?)?;ctx.proxy_active=envelope.proxy_active;
        for event in envelope.batch.events {
            let trace=format!("{}:{}:{}",ctx.agent,envelope.batch.session_id,event.turn_id());let event_key=event.key(&trace);
            if seen.set.contains(&event_key) {continue;}
            let turn=open.turns.entry(trace.clone()).or_insert_with(|| {
                let mut metadata=envelope.metadata.clone();
                for (key,value) in [("cwd",&envelope.batch.cwd),("agent_version",&envelope.batch.agent_version),("session_path",&envelope.batch.session_path)] {if let Some(value)=value {metadata.insert(key.into(),json!(value));}}
                Turn {session_id:envelope.batch.session_id.clone(),metadata,run_id:envelope.run_id.clone(),..Default::default()}
            });
            turn.touched=now;
            match event {
                Event::TurnStart {ts_ns,prompt,metadata,..}=>{turn.start=Some(ts_ns);turn.explicit_start=true;turn.prompt=prompt;turn.root_metadata=metadata;index_turn(ctx,&trace,turn)?;},
                Event::TurnEnd {ts_ns,output,status,error,..}=>{turn.end=Some(ts_ns);turn.output=output;turn.status=status;turn.error=error;index_turn(ctx,&trace,turn)?;},
                Event::Chat {id,parent_id,start_ns,end_ns,model,provider,input,output,usage,cost_usd,aux_task,status,error,metadata,..}=>{
                    infer_time(turn,start_ns,end_ns);let mut rec=child(turn,&trace,&format!("chat:{id}"),parent_id.as_deref(),start_ns,end_ns);
                    rec.name=if let Some(model)=&model {format!("{model}.chat")} else {"chat".into()};rec.log_type="chat".into();rec.model=model;rec.provider=provider;rec.input=input;rec.output=output;rec.usage=usage.unwrap_or_default();rec.status=status.unwrap_or_else(||"ok".into());rec.status_message=error;
                    if let Some(cost)=cost_usd {rec.metadata.insert("estimated_cost_usd".into(),json!(cost));}
                    if let Some(aux)=aux_task {rec.metadata.insert("hermes.aux_task".into(),json!(aux));}rec.metadata.extend(metadata);ctx.emit(rec)?;
                },
                Event::Tool {id,parent_id,name,start_ns,end_ns,input,output,status,error,mut metadata,..}=>{
                    infer_time(turn,start_ns,end_ns);let mut rec=child(turn,&trace,&format!("tool:{id}"),parent_id.as_deref(),start_ns,end_ns);
                    // `parent_tool_id` metadata nests a tool under the tool that ran it (e.g. Codex code-mode `exec` scripts).
                    if let Some(Value::String(tool)) = metadata.remove("parent_tool_id") { rec.parent_key=Some(format!("{trace}:tool:{tool}")); }
                    rec.name=name.unwrap_or_else(||"tool".into());rec.log_type="tool".into();rec.input=input;rec.output=output;rec.status=status.unwrap_or_else(||"ok".into());rec.status_message=error;rec.metadata.insert("tool_call_id".into(),json!(id));rec.metadata.extend(metadata);ctx.emit(rec)?;
                },
                Event::SubagentStart {id,parent_id,ts_ns,name,metadata,..}=>{infer_time(turn,ts_ns,ts_ns);let sub=turn.subs.entry(id).or_default();sub.start=Some(ts_ns);sub.parent_id=parent_id;sub.name=name;sub.metadata=metadata;},
                Event::SubagentEnd {id,ts_ns,status,output,..}=>{turn.latest_end=turn.latest_end.max(ts_ns);let sub=turn.subs.entry(id).or_default();sub.end=Some(ts_ns);sub.status=status;sub.output=output;},
            }
            emit_subs(ctx,&trace,turn)?;
            if turn.explicit_start&&turn.end.is_some() {emit_turn(ctx,&trace,turn)?;}
            seen.add(event_key);
        }
    }
    ctx.proxy_active=old_proxy;
    if flush {
        for (trace,turn) in &mut open.turns {
            if turn.end.is_none()&&turn.start.is_some_and(|s|now.saturating_sub(s)>=ctx.config.turn_timeout_secs.saturating_mul(1_000_000_000)) {
                turn.end=Some(now);turn.status=Some("error".into());turn.error=Some("turn not closed".into());index_turn(ctx,trace,turn)?;
            }
            if turn.end.is_some() && (turn.explicit_start || now.saturating_sub(turn.touched)>=ctx.config.turn_timeout_secs.saturating_mul(1_000_000_000)) {emit_turn(ctx,trace,turn)?;}
            emit_subs(ctx,trace,turn)?;
        }
    }
    let mut closed:Vec<_>=open.turns.iter().filter(|(_,t)|t.emitted&&t.subs.values().all(|s|s.emitted)).map(|(k,t)|(k.clone(),t.touched)).collect();closed.sort_by_key(|(_,ts)|*ts);
    for (key,_) in closed.iter().take(closed.len().saturating_sub(1024)) {open.turns.remove(key);}
    ctx.finish()?;ctx.save("open.json",&open)?;ctx.save("seen",&seen.order)?;
    for file in files {std::fs::remove_file(file)?;}Ok(())
}
fn index_turn(ctx:&HookCtx,trace:&str,turn:&Turn)->Result<()> {
    if let (Some(run),Some(start))=(&turn.run_id,turn.start) {crate::otlp::save_turn(ctx.global_state_dir(),run,trace,start,turn.end)?;}Ok(())
}
fn infer_time(turn:&mut Turn,start:u64,end:u64) {if !turn.explicit_start {turn.start=Some(turn.start.map_or(start,|s|s.min(start)));}turn.latest_end=turn.latest_end.max(end);}
fn child(turn:&Turn,trace:&str,suffix:&str,parent:Option<&str>,start:u64,end:u64)->SpanRecord {
    { let mut metadata=turn.metadata.clone(); if parent.is_some() {metadata.insert("subagent".into(),json!(true));}
    SpanRecord {trace_key:Some(trace.into()),span_key:format!("{trace}:{suffix}"),parent_key:Some(parent.map_or_else(||format!("{trace}:turn"),|id|format!("{trace}:sub:{id}"))),run_id:turn.run_id.clone(),start_ns:start,end_ns:end,thread_id:Some(turn.session_id.clone()),metadata,..Default::default()} }
}
fn emit_turn(ctx:&mut HookCtx,trace:&str,turn:&mut Turn)->Result<()> {
    if turn.emitted {return Ok(());}let Some(end)=turn.end else {return Ok(());};let start=turn.start.unwrap_or(end);let end=end.max(turn.latest_end).max(start);
    let mut metadata=turn.metadata.clone();metadata.extend(turn.root_metadata.clone());
    ctx.emit(SpanRecord {trace_key:Some(trace.into()),span_key:format!("{trace}:turn"),run_id:turn.run_id.clone(),name:format!("{} turn",ctx.agent),start_ns:start,end_ns:end,input:turn.prompt.clone(),output:turn.output.clone(),status:turn.status.clone().unwrap_or_else(||"ok".into()),status_message:turn.error.clone(),thread_id:Some(turn.session_id.clone()),metadata,..Default::default()})?;turn.emitted=true;Ok(())
}
fn emit_subs(ctx:&mut HookCtx,trace:&str,turn:&mut Turn)->Result<()> {
    for (id,sub) in &mut turn.subs {
        if sub.emitted {continue;}let (Some(start),Some(end))=(sub.start,sub.end) else {continue;};
        let mut metadata=turn.metadata.clone();metadata.insert("subagent".into(),json!(true));metadata.extend(sub.metadata.clone());
        ctx.emit(SpanRecord {trace_key:Some(trace.into()),span_key:format!("{trace}:sub:{id}"),parent_key:Some(sub.parent_id.as_ref().map_or_else(||format!("{trace}:turn"),|id|format!("{trace}:tool:{id}"))),run_id:turn.run_id.clone(),name:sub.name.clone().unwrap_or_else(||"subagent".into()),start_ns:start,end_ns:end.max(start),output:sub.output.clone(),status:sub.status.clone().unwrap_or_else(||"ok".into()),thread_id:Some(turn.session_id.clone()),metadata,..Default::default()})?;sub.emitted=true;
    }Ok(())
}
pub(crate) fn has_pending(state:&Path)->Result<bool> {
    let dirs=match std::fs::read_dir(state.join("agents")) {Ok(d)=>d,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(false),Err(e)=>return Err(e.into())};
    for dir in dirs {let dir=dir?;if dir.file_type()?.is_dir()&&!pending(&dir.path())?.is_empty() {return Ok(true);}}Ok(false)
}
pub(crate) fn at_flush(paths:&Paths,config:&Config,now:u64)->Result<()> {
    let dirs=match std::fs::read_dir(paths.state.join("agents")) {Ok(d)=>d,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into())};
    for dir in dirs {let dir=dir?;if !dir.file_type()?.is_dir() {continue;}
        if !dir.path().join("open.json").exists()&&pending(&dir.path())?.is_empty() {continue;}let kind=dir.file_name().to_string_lossy().into_owned();
        let mut ctx=HookCtx::new(paths.clone(),config.clone(),&kind,BTreeMap::new(),None,false)?;drain(&mut ctx,true,now)?;
    }Ok(())
}

#[cfg(test)] mod tests {
    use super::*;
    fn context(tmp:&crate::tests::Temp)->HookCtx {HookCtx::new(Paths {state:tmp.path.join("state"),config:tmp.path.join("config")},Config {customer_identifier:"test".into(),..Default::default()},"fake",BTreeMap::new(),Some("run".into()),false).unwrap()}
    fn batch(events:Value)->Batch {serde_json::from_value(json!({"v":1,"agent":"fake","session_id":"s","events":events})).unwrap()}
    fn records(ctx:&HookCtx)->Vec<SpanRecord> {crate::spool::ready(ctx.state_dir().parent().unwrap().parent().unwrap()).unwrap().iter().flat_map(|p|std::fs::read_to_string(p).unwrap().lines().map(|s|serde_json::from_str(s).unwrap()).collect::<Vec<_>>()).collect()}
    #[test] fn out_of_order_and_idempotent_separate_ingests() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
        let end=batch(json!([{"type":"turn_end","turn_id":"t","ts_ns":100,"output":"done"}]));ingest(&mut ctx,end.clone()).unwrap();assert!(records(&ctx).is_empty());let now=ctx.now_ns();drain(&mut ctx,true,now).unwrap();assert!(records(&ctx).is_empty());
        let chat=batch(json!([{"type":"chat","turn_id":"t","id":"c","start_ns":20,"end_ns":80,"model":"m"}]));ingest(&mut ctx,chat.clone()).unwrap();
        let start=batch(json!([{"type":"turn_start","turn_id":"t","ts_ns":10,"prompt":"prompt"}]));ingest(&mut ctx,start.clone()).unwrap();
        let all=records(&ctx);assert_eq!(all.len(),2);let chat=all.iter().find(|r|r.log_type=="chat").unwrap();assert_eq!(chat.span_key,"fake:s:t:chat:c");assert_eq!(chat.parent_key.as_deref(),Some("fake:s:t:turn"));assert_eq!(chat.usage,Usage::default());
        let turn=all.iter().find(|r|r.log_type=="agent").unwrap();assert_eq!(turn.start_ns,10);assert_eq!(turn.end_ns,100);assert_eq!(turn.thread_id.as_deref(),Some("s"));
        ingest(&mut ctx,end).unwrap();ingest(&mut ctx,start).unwrap();ingest(&mut ctx,batch(json!([{"type":"chat","turn_id":"t","id":"c","start_ns":20,"end_ns":80}]))).unwrap();assert_eq!(records(&ctx).len(),2);
    }
    #[test] fn subagent_parenting_and_persisted_secret_scrubbing() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
        ingest(&mut ctx,batch(json!([
            {"type":"turn_start","turn_id":"t","ts_ns":10,"prompt":"password=hunter2"},
            {"type":"tool","turn_id":"t","id":"spawn","name":"delegate","start_ns":20,"end_ns":90},
            {"type":"subagent_end","turn_id":"t","id":"sub","ts_ns":80,"status":"ok"},
            {"type":"chat","turn_id":"t","id":"c","parent_id":"sub","start_ns":40,"end_ns":60,"model":"m"}
        ]))).unwrap();
        ingest(&mut ctx,batch(json!([{ "type":"subagent_start","turn_id":"t","id":"sub","parent_id":"spawn","ts_ns":30,"name":"explore"},{"type":"turn_end","turn_id":"t","ts_ns":100}]))).unwrap();
        let all=records(&ctx);assert_eq!(all.len(),4);let sub=all.iter().find(|r|r.span_key.ends_with(":sub:sub")).unwrap();assert_eq!(sub.parent_key.as_deref(),Some("fake:s:t:tool:spawn"));
        let chat=all.iter().find(|r|r.log_type=="chat").unwrap();assert_eq!(chat.parent_key.as_deref(),Some("fake:s:t:sub:sub"));
        assert!(!std::fs::read_to_string(ctx.state_dir().join("open.json")).unwrap().contains("hunter2"));assert!(!all.iter().any(|r|r.input.as_ref().is_some_and(|v|v.to_string().contains("hunter2"))));
    }
    #[test] fn next_flush_closes_unfinished_turn() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.config.turn_timeout_secs=1;
        ingest(&mut ctx,batch(json!([{"type":"turn_start","turn_id":"timeout","ts_ns":0,"prompt":"hi"}]))).unwrap();assert!(records(&ctx).is_empty());
        let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};assert!(crate::flush::run(&paths,&ctx.config,None).is_err());
        let all=records(&ctx);assert_eq!(all.len(),1);assert_eq!(all[0].status,"error");assert_eq!(all[0].status_message.as_deref(),Some("turn not closed"));
        assert!(crate::flush::run(&paths,&ctx.config,None).is_err());assert_eq!(records(&ctx).len(),1);
    }
    #[test] fn metadata_only_and_bounded_seen_state() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.config.content_cap_bytes=0;
        ingest(&mut ctx,batch(json!([{"type":"turn_start","turn_id":"t","ts_ns":1,"prompt":"private prompt"},{"type":"turn_end","turn_id":"t","ts_ns":2,"output":"private output"}]))).unwrap();
        assert!(!std::fs::read_to_string(ctx.state_dir().join("open.json")).unwrap().contains("private"));
        let mut seen=Seen {order:VecDeque::new(),set:BTreeSet::new()};for i in 0..SEEN_LIMIT+1 {seen.add(i.to_string());}assert_eq!(seen.order.len(),SEEN_LIMIT);assert!(!seen.set.contains("0"));
        assert!(ctx.save("../outside",&json!({})).is_err());
    }
    #[test] fn contested_ingestion_queues_scrubbed_events_for_flush() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let lock=Lock::acquire(ctx.state_dir()).unwrap().unwrap();
        ingest(&mut ctx,batch(json!([{"type":"turn_start","turn_id":"t","ts_ns":1,"prompt":"password=hunter2"},{"type":"turn_end","turn_id":"t","ts_ns":2}]))).unwrap();
        let files=pending(ctx.state_dir()).unwrap();assert_eq!(files.len(),1);assert!(!std::fs::read_to_string(&files[0]).unwrap().contains("hunter2"));assert!(records(&ctx).is_empty());drop(lock);
        let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};at_flush(&paths,&ctx.config,ctx.now_ns()).unwrap();assert_eq!(records(&ctx).len(),1);assert!(pending(ctx.state_dir()).unwrap().is_empty());
    }
    #[test] fn per_event_metadata_wins_without_leaking_turn_overrides_to_children() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.metadata=BTreeMap::from([("shared".into(),json!("base")),("base-only".into(),json!(true))]);
        ingest(&mut ctx,batch(json!([
            {"type":"turn_start","turn_id":"meta","ts_ns":1,"metadata":{"shared":"turn","turn-only":true}},
            {"type":"chat","turn_id":"meta","id":"c","start_ns":2,"end_ns":3,"metadata":{"shared":"chat","secret-note":"password=hidden"}},
            {"type":"tool","turn_id":"meta","id":"spawn","name":"delegate","start_ns":3,"end_ns":7,"metadata":{"shared":"tool"}},
            {"type":"subagent_start","turn_id":"meta","id":"sub","parent_id":"spawn","ts_ns":4,"metadata":{"shared":"sub"}},
            {"type":"subagent_end","turn_id":"meta","id":"sub","ts_ns":6},
            {"type":"turn_end","turn_id":"meta","ts_ns":8}
        ]))).unwrap();
        let all=records(&ctx);assert_eq!(all.len(),4);
        for rec in all {assert_eq!(rec.metadata["base-only"],true);let expected=if rec.span_key.ends_with(":turn") {"turn"} else if rec.log_type=="chat" {"chat"} else if rec.log_type=="tool" {"tool"} else {"sub"};assert_eq!(rec.metadata["shared"],expected);assert_eq!(rec.metadata.contains_key("turn-only"),expected=="turn");assert!(!serde_json::to_string(&rec).unwrap().contains("hidden"));}
        let parsed=batch(json!([{"type":"turn_start","turn_id":"default","ts_ns":1}]));assert!(matches!(&parsed.events[0],Event::TurnStart {metadata,..} if metadata.is_empty()));
    }
    #[test] fn restored_settlement_context_is_snapshotted_per_batch() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
        for (id,proxy) in [("first",true),("second",false)] {
            ctx.metadata=BTreeMap::from([("restored".into(),json!(id))]);ctx.run_id=Some(format!("run-{id}"));ctx.proxy_active=proxy;
            ingest(&mut ctx,batch(json!([{ "type":"turn_start","turn_id":id,"ts_ns":1},{"type":"chat","turn_id":id,"id":"c","start_ns":2,"end_ns":3},{"type":"turn_end","turn_id":id,"ts_ns":4}]))).unwrap();
        }
        let all=records(&ctx);assert_eq!(all.len(),3);assert!(!all.iter().any(|r|r.trace_key.as_deref()==Some("fake:s:first")&&r.log_type=="chat"));
        for rec in all {let id=rec.metadata["restored"].as_str().unwrap();assert_eq!(rec.run_id,Some(format!("run-{id}")));}
    }
    #[test] fn inferred_start_does_not_finalize_root_before_late_explicit_prompt() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
        ingest(&mut ctx,batch(json!([{"type":"turn_end","turn_id":"late","ts_ns":100,"output":"done"}]))).unwrap();
        ingest(&mut ctx,batch(json!([{"type":"chat","turn_id":"late","id":"c","start_ns":20,"end_ns":80}]))).unwrap();let now=ctx.now_ns();drain(&mut ctx,true,now).unwrap();assert_eq!(records(&ctx).len(),1);assert_eq!(records(&ctx)[0].log_type,"chat");
        ingest(&mut ctx,batch(json!([{"type":"turn_start","turn_id":"late","ts_ns":10,"prompt":"original question","metadata":{"explicit":"yes"}}]))).unwrap();let all=records(&ctx);let root=all.iter().find(|r|r.log_type=="agent").unwrap();assert_eq!(root.start_ns,10);assert_eq!(root.input,Some(json!("original question")));assert_eq!(root.metadata["explicit"],"yes");
    }
    #[test] fn missing_explicit_start_can_finalize_after_the_reordering_timeout() {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.config.turn_timeout_secs=1;
        ingest(&mut ctx,batch(json!([{"type":"turn_end","turn_id":"fallback","ts_ns":100},{"type":"chat","turn_id":"fallback","id":"c","start_ns":20,"end_ns":80}]))).unwrap();let open:Open=ctx.load("open.json").unwrap();let touched=open.turns["fake:s:fallback"].touched;drain(&mut ctx,true,touched+1_000_000_000).unwrap();assert_eq!(records(&ctx).len(),2);
    }
}
