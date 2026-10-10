//! Silent native hooks and incremental Claude transcript translation.
mod setup;
#[cfg(test)] mod tests;
#[cfg(test)] mod live_tests;
use crate::{agents::{common::{bridge::{self, Batch, Event}, jsonl::{self, Cursor}}, AgentSpec}, record::{HookCtx, Usage}, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, path::{Path, PathBuf}};

pub const SPEC: AgentSpec = AgentSpec { kind: "claude", display: "Claude Code", hook, plan: setup::plan, reply: None, settle: Some(settle_pending) };

#[derive(Default, Serialize, Deserialize)]
struct Session {
    id: String, path: Option<String>, cwd: Option<String>, version: Option<String>,
    metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool,
    turn: Option<String>, sequence: u64, turn_start: u64, stopped: Option<u64>,
    output: Option<Value>, error: Option<String>, files: BTreeMap<String, Transcript>,
    tools: BTreeMap<String, Tool>, children: BTreeMap<String, Child>,
}
#[derive(Default, Serialize, Deserialize)]
struct Transcript {
    cursor: Cursor, current: Option<Response>, input: Vec<Value>,
    seen: BTreeSet<String>, parent: Option<String>, turn: Option<String>, terminal_turn: Option<String>,
    stable_since: u64, last_offset: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Response {
    id: String, message_id: Option<String>, request_id: Option<String>, turn: String,
    start: u64, end: u64, model: Option<String>, input: Vec<Value>,
    parts: Vec<Value>, rows: BTreeSet<String>, usage: Option<Usage>, finished: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Tool { turn: String, parent: Option<String>, start: u64, name: Option<String>, input: Option<Value> }
#[derive(Default, Serialize, Deserialize)]
struct Child {
    turn: String, start: u64, end: Option<u64>, tool: Option<String>, name: Option<String>,
    output: Option<Value>, path: Option<String>, announced: bool, completed: bool,
}

fn string(value: &Value, key: &str) -> Option<String> { value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned) }
fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) }
fn foreign(value: &Value) -> bool {
    ["GROK_HOOK_EVENT", "GROK_SESSION_ID", "GROK_HOOK_NAME", "CURSOR_TRACE_ID", "CURSOR_AGENT"].iter().any(|key| std::env::var_os(key).is_some())
        || (value.get("hookEventName").is_some() && value.get("workspaceRoot").is_some())
        || value.get("conversation_id").is_some() || value.get("generation_id").is_some()
}
fn hook(ctx: &mut HookCtx, bytes: &[u8]) -> Result<()> {
    let mut payload: Value = match serde_json::from_slice(bytes) { Ok(value) => value, Err(_) => return Ok(()) };
    if foreign(&payload) { return Ok(()); }
    let Some(session) = string(&payload, "session_id").filter(|s| valid_id(s)) else { return Ok(()); };
    if string(&payload, "hook_event_name").is_none() || string(&payload, "transcript_path").is_none() || string(&payload, "cwd").is_none() { return Ok(()); }
    let child_relation=payload.get("tool_response").and_then(Value::as_object).map(|object|object.iter().filter(|(key,_)|["agentId","agent_id","outputFile"].contains(&key.as_str())).map(|(key,value)|(key.clone(),value.clone())).collect::<serde_json::Map<_,_>>());
    let child_type=payload.pointer("/tool_input/subagent_type").cloned();
    for field in ["prompt", "tool_input", "tool_response", "last_assistant_message"] {
        if let Some(value) = payload.get_mut(field) { let mut content = Some(value.take()); ctx.scrub_content(&mut content)?; *value = content.unwrap_or(Value::Null); }
    }
    if ctx.config.content_cap_bytes == 0 {
        if let Some(relation)=child_relation.filter(|object|!object.is_empty()) { payload["tool_response"]=Value::Object(relation); }
        if let Some(name)=child_type { payload["tool_input"]=json!({"subagent_type":name}); }
    }
    let key = format!("native-{}-{}.json", ctx.now_ns(), crate::record::random_id()?);
    ctx.save(&key, &json!({"session":session,"received":ctx.now_ns(),"payload":payload,"metadata":ctx.metadata,"run_id":ctx.run_id,"proxy_active":ctx.proxy_active}))?;
    ctx.mark_settle_pending()?;
    drain(ctx)
}

fn files(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut paths = match std::fs::read_dir(dir) { Ok(entries) => entries.filter_map(|entry| entry.ok().map(|e| e.path())).filter(|path| path.file_name().is_some_and(|s| s.to_string_lossy().starts_with(prefix)) && path.extension().is_some_and(|s| s == "json")).collect::<Vec<_>>(), Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), Err(error) => return Err(error.into()) };
    paths.sort(); Ok(paths)
}
fn drain(ctx: &mut HookCtx) -> Result<()> {
    let Some(_lock) = ctx.try_lock("native")? else { return Ok(()); };
    for path in files(ctx.state_dir(), "native-")? {
        let queued: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let id = string(&queued, "session").ok_or("missing queued session")?;
        let key = format!("session-{id}.json");
        let mut state: Session = ctx.load(&key)?; state.id = id;
        state.metadata = serde_json::from_value(queued["metadata"].clone())?; state.run_id=string(&queued,"run_id"); state.proxy_active=queued["proxy_active"]==true;
        let mut events = Vec::new();
        process(&mut state, &queued["payload"], queued["received"].as_u64().unwrap_or_else(||ctx.now_ns()), &mut events)?;
        ingest(ctx, &state, events)?; save_session(ctx, &key, &mut state)?; std::fs::remove_file(path)?;
    }
    Ok(())
}
fn save_session(ctx: &HookCtx, key: &str, state: &mut Session) -> Result<()> {
    ctx.scrub_content(&mut state.output)?;
    for tool in state.tools.values_mut() { ctx.scrub_content(&mut tool.input)?; }
    for child in state.children.values_mut() { ctx.scrub_content(&mut child.output)?; }
    if ctx.config.content_cap_bytes == 0 {
        for file in state.files.values_mut() {
            file.input.clear();
            if let Some(response) = &mut file.current { response.input.clear(); response.parts.clear(); }
        }
    }
    ctx.save(key, state)
}

fn ingest(ctx: &mut HookCtx, state: &Session, mut events: Vec<Event>) -> Result<()> {
    if state.proxy_active {events.retain(|event|!matches!(event,Event::Chat {..}));}
    if events.is_empty() { return Ok(()); }
    ctx.metadata=state.metadata.clone();ctx.run_id=state.run_id.clone();ctx.proxy_active=state.proxy_active;
    bridge::ingest(ctx, Batch { v:1, agent:"claude".into(), session_id:state.id.clone(), session_path:state.path.clone(), cwd:state.cwd.clone(), agent_version:state.version.clone(), events })
}

fn process(state: &mut Session, payload: &Value, now: u64, events: &mut Vec<Event>) -> Result<()> {
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    let child = string(payload, "agent_id");
    state.cwd = string(payload, "cwd").or_else(||state.cwd.take());
    state.path = string(payload, "transcript_path").or_else(||state.path.take());
    // SessionStart resumes large histories without scanning or tracing old responses.
    if matches!(event,"SessionStart"|"UserPromptSubmit") && child.is_none() && state.turn.is_none() {
        if let Some(path)=&state.path {
            if !state.files.contains_key(path) {
                let mut file=Transcript::default();
                if let Ok(metadata)=std::fs::metadata(path) {
                    file.cursor.offset=metadata.len();file.cursor.len_seen=metadata.len();
                    #[cfg(unix)] {use std::os::unix::fs::MetadataExt;file.cursor.identity=Some(format!("{}:{}",metadata.dev(),metadata.ino()));}
                    #[cfg(not(unix))] {file.cursor.identity=metadata.created().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d|d.as_nanos().to_string());}
                }
                state.files.insert(path.clone(),file);
            }
        }
    }
    // Flush the preceding turn's delayed rows before opening the next one.
    read_transcripts(state, now, events)?;
    if event == "UserPromptSubmit" && child.is_none() {
        if state.turn.is_some() && state.stopped.is_none() { state.stopped=Some(now);state.error=Some("turn superseded before Stop".into()); }
        close_turn(state, now, true, events);
        let completed:Vec<_>=state.children.iter().filter(|(_,child)|child.completed).map(|(id,child)|(id.clone(),child.path.clone())).collect();
        for (id,path) in completed {state.children.remove(&id);if let Some(path)=path {state.files.remove(&path);}}
        state.sequence += 1;
        let turn = string(payload,"prompt_id").unwrap_or_else(||format!("prompt-{}-{now}",state.sequence));
        state.turn=Some(turn.clone());state.turn_start=now;state.stopped=None;state.output=None;state.error=None;
        if let Some(path)=&state.path { let transcript=state.files.entry(path.clone()).or_default(); transcript.turn=Some(turn.clone()); transcript.terminal_turn=None; transcript.input.clear(); if let Some(prompt)=payload.get("prompt") {transcript.input.push(json!({"role":"user","content":prompt}));} }
        events.push(Event::TurnStart {metadata:BTreeMap::new(), turn_id:turn, ts_ns:now, prompt:payload.get("prompt").cloned() });
    }
    let Some(turn) = state.turn.clone() else { return Ok(()); };
    let child_turn = child.as_ref().and_then(|id|state.children.get(id)).map(|c|c.turn.clone()).unwrap_or_else(||turn.clone());
    match event {
        "PreToolUse" => if let Some(id)=string(payload,"tool_use_id") { state.tools.entry(id).or_insert(Tool {turn:child_turn,parent:child,start:now,name:string(payload,"tool_name"),input:payload.get("tool_input").cloned()}); },
        "PostToolUse" | "PostToolUseFailure" => if let Some(id)=string(payload,"tool_use_id") {
            let duration=payload["duration_ms"].as_u64().unwrap_or(0).saturating_mul(1_000_000);
            let tool=state.tools.remove(&id).unwrap_or(Tool {turn:child_turn,parent:child,start:now.saturating_sub(duration),name:string(payload,"tool_name"),input:payload.get("tool_input").cloned()});
            if matches!(tool.name.as_deref(),Some("Agent"|"Task")) {
                let response=&payload["tool_response"];
                if let Some(agent)=string(response,"agentId").or_else(||string(response,"agent_id")) {
                    let sub=state.children.entry(agent).or_insert_with(||Child {turn:tool.turn.clone(),start:tool.start,..Default::default()});
                    sub.tool=Some(id.clone());sub.name=tool.input.as_ref().and_then(|v|string(v,"subagent_type"));
                    if let Some(path)=string(response,"outputFile").filter(|p|p.ends_with(".jsonl")) {sub.path=Some(path);}
                }
            }
            events.push(Event::Tool {metadata:BTreeMap::new(),turn_id:tool.turn,id,parent_id:tool.parent,name:tool.name,start_ns:tool.start,end_ns:now.max(tool.start),input:tool.input,output:payload.get("tool_response").cloned(),status:Some(if event=="PostToolUseFailure" {"error"} else {"ok"}.into()),error:string(payload,"error")});
        },
        "SubagentStart" => if let Some(id)=child {state.children.entry(id).or_insert(Child {turn,start:now,name:string(payload,"agent_type"),..Default::default()});},
        "SubagentStop" => if let Some(id)=child {
            let sub=state.children.entry(id).or_insert(Child {turn,start:now,..Default::default()});
            sub.end=Some(now);sub.output=payload.get("last_assistant_message").cloned();sub.path=string(payload,"agent_transcript_path").or_else(||sub.path.take());
        },
        "Stop" | "StopFailure" | "SessionEnd" if child.is_none() => {state.stopped=Some(now);state.output=payload.get("last_assistant_message").cloned();state.error=string(payload,"error").or_else(||(event!="Stop").then(||"turn failed or abandoned".into()));},
        _ => {},
    }
    announce_children(state,events);
    read_transcripts(state,now,events)?;
    close_turn(state,now,false,events);
    Ok(())
}
fn announce_children(state:&mut Session, events:&mut Vec<Event>) {
    for (id,child) in &mut state.children {
        // Never guess which tool spawned an internal/suggestion agent.
        if !child.announced {
            if let Some(tool)=&child.tool {events.push(Event::SubagentStart {metadata:BTreeMap::new(),turn_id:child.turn.clone(),id:id.clone(),parent_id:Some(tool.clone()),ts_ns:child.start,name:child.name.clone()});child.announced=true;}
        }
        if child.announced { if let Some(path)=&child.path {let file=state.files.entry(path.clone()).or_default();file.parent=Some(id.clone());file.turn=Some(child.turn.clone());} }
    }
}
fn read_transcripts(state:&mut Session, now:u64, events:&mut Vec<Event>) -> Result<()> {
    if let Some(path)=&state.path {let file=state.files.entry(path.clone()).or_default();if file.turn.is_none() {file.turn=state.turn.clone();}}
    let paths:Vec<_>=state.files.keys().cloned().collect();
    for path in paths {
        let file=state.files.get_mut(&path).expect("known transcript");
        let lines=match jsonl::read_new(Path::new(&path),&mut file.cursor) {Ok(lines)=>lines,Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e|e.kind()==std::io::ErrorKind::NotFound)=>continue,Err(error)=>return Err(error)};
        if file.cursor.offset!=file.last_offset {file.stable_since=now;file.last_offset=file.cursor.offset;}
        for line in lines {
            let row:Value=match serde_json::from_str(&line) {Ok(row)=>row,Err(_)=>continue};
            if row.get("sessionId").and_then(Value::as_str).is_some_and(|s|s!=state.id) {continue;}
            if file.parent.is_none() && (row["isSidechain"]==true || row.get("agentId").is_some_and(|v|!v.is_null())) {continue;}
            state.version=string(&row,"version").or_else(||state.version.take());
            let ts=string(&row,"timestamp").and_then(|s|timestamp(&s)).unwrap_or(now);
            match row["type"].as_str() {
                Some("assistant") => {
                    let message=&row["message"];
                    let request=string(&row,"requestId");let message_id=string(message,"id");
                    let Some(id)=request.clone().or_else(||message_id.clone()) else {continue;};
                    if file.seen.contains(&id) || message_id.as_ref().is_some_and(|id|file.seen.contains(id)) {continue;}
                    let same=file.current.as_ref().is_some_and(|response|response.id==id || (message_id.is_some() && response.message_id==message_id) || (request.is_some() && response.request_id==request));
                    if !same {settle(file,events);}
                    let Some(turn)=file.turn.clone() else {continue;};
                    let response=file.current.get_or_insert_with(||Response {id,message_id,request_id:request,turn,start:ts,end:ts,model:string(message,"model"),input:file.input.clone(),..Default::default()});
                    let row_id=string(&row,"uuid").unwrap_or_else(||format!("{:x}",Sha256::digest(line.as_bytes())));
                    if response.rows.insert(row_id) {
                        if let Some(parts)=message["content"].as_array() {response.parts.extend(parts.iter().cloned());}
                    }
                    response.end=response.end.max(ts);response.model=string(message,"model").or_else(||response.model.take());
                    if let Some(usage)=message.get("usage").filter(|v|v.is_object()) {response.usage=Some(serde_json::from_value(usage.clone())?);}
                    response.finished=message["stop_reason"].as_str().is_some();
                    if matches!(message["stop_reason"].as_str(),Some("end_turn"|"max_tokens"|"stop_sequence")) {file.terminal_turn=Some(response.turn.clone());}
                },
                Some("user") => {
                    settle(file,events);
                    if let Some(message)=row.get("message") {if file.input.last()!=Some(message) {file.input.push(message.clone());}}
                    if let Some(agent)=string(&row["toolUseResult"],"agentId").or_else(||string(&row["toolUseResult"],"agent_id")) {
                        let parent=string(&row,"sourceToolUseID").or_else(||row["message"]["content"].as_array().and_then(|parts|parts.iter().find_map(|part|string(part,"tool_use_id"))));
                        if let (Some(tool),Some(turn))=(parent,file.turn.clone()) {let child=state.children.entry(agent).or_insert(Child {turn,start:ts,..Default::default()});child.tool=Some(tool);}
                    }
                },
                Some("system") if row["subtype"]=="turn_duration" => {file.terminal_turn=file.turn.clone();settle(file,events);},
                _=>{},
            }
        }
    }
    announce_children(state,events);
    Ok(())
}
fn settle(file:&mut Transcript,events:&mut Vec<Event>) {
    let Some(response)=file.current.take() else {return;};
    for id in [&Some(response.id.clone()),&response.message_id,&response.request_id].into_iter().flatten() {file.seen.insert(id.clone());}
    // ponytail: bounded IDs; bridge also deduplicates within its recent-key window.
    while file.seen.len()>8192 {file.seen.pop_first();}
    let output=json!([{"role":"assistant","content":response.parts}]);
    file.input.push(output[0].clone());
    events.push(Event::Chat {metadata:BTreeMap::new(),turn_id:response.turn,id:if let Some(parent)=&file.parent {format!("{parent}:{}",response.id)} else {response.id},parent_id:file.parent.clone(),start_ns:response.start,end_ns:response.end,model:response.model,provider:Some("anthropic".into()),input:Some(json!(response.input)),output:Some(output),usage:if response.finished {response.usage} else {None},cost_usd:None,aux_task:None,status:Some(if response.finished {"ok"} else {"error"}.into()),error:(!response.finished).then(||"incomplete model response".into())});
}
fn close_turn(state:&mut Session,now:u64,force:bool,events:&mut Vec<Event>) {
    for file in state.files.values_mut() {
        let ended=state.stopped.is_some() || file.parent.as_ref().is_some_and(|id|state.children.get(id).is_some_and(|c|c.end.is_some()));
        if file.current.is_some() && (force || (ended && (file.current.as_ref().is_some_and(|r|r.finished) && now.saturating_sub(file.stable_since)>=100_000_000))) {settle(file,events);}
    }
    for (id,child) in &mut state.children {
        if !child.completed && child.announced && child.end.is_some() && child.path.as_ref().is_some_and(|p|state.files.get(p).is_some_and(|f|f.cursor.offset>0 && f.current.is_none())) {
            events.push(Event::SubagentEnd {turn_id:child.turn.clone(),id:id.clone(),ts_ns:child.end.unwrap_or(now),status:Some("ok".into()),output:child.output.clone()});child.completed=true;
        }
    }
    let Some(stopped)=state.stopped else {return;};
    // Stop may arrive before every assistant row, not only before the final chunk.
    // Keep the detached sender alive through Core's three two-second retry rounds.
    let terminal=state.path.as_ref().and_then(|path|state.files.get(path)).is_some_and(|file|file.terminal_turn==state.turn);
    if !force && state.error.is_none() && !terminal && now.saturating_sub(stopped)<6_000_000_000 {return;}
    if !force && (now.saturating_sub(stopped)<100_000_000 || (state.error.is_none() && state.files.values().any(|f|f.current.is_some()))) {return;}
    if let Some(turn)=state.turn.take() {events.push(Event::TurnEnd {turn_id:turn,ts_ns:stopped,output:state.output.clone(),status:Some(if state.error.is_some() {"error"} else {"ok"}.into()),error:state.error.clone()});}
    state.stopped=None;
}

fn settle_pending(ctx:&mut HookCtx)->Result<bool> {
    drain(ctx)?;
    let _lock=ctx.lock("native")?;
    let now=ctx.now_ns();let mut pending=false;
    for path in files(ctx.state_dir(),"session-")? {
        let key=path.file_name().ok_or("missing state filename")?.to_string_lossy();
        let mut state:Session=ctx.load(&key)?;
        let mut events=Vec::new();read_transcripts(&mut state,now,&mut events)?;close_turn(&mut state,now,false,&mut events);ingest(ctx,&state,events)?;save_session(ctx,&key,&mut state)?;
        pending |= state.stopped.is_some() || state.children.values().any(|c|!c.completed && c.announced && c.end.is_some() && c.path.as_ref().is_some_and(|p|state.files.get(p).is_none_or(|f|f.cursor.offset==0 || f.current.is_some())));
    }
    Ok(pending || !files(ctx.state_dir(),"native-")?.is_empty())
}

// Claude timestamps are ISO-8601 UTC; malformed values fall back to hook arrival.
fn timestamp(text:&str)->Option<u64> {
    let (date,time)=text.strip_suffix('Z')?.split_once('T')?;
    let mut date=date.split('-').map(str::parse::<i64>);let year=date.next()?.ok()?;let month=date.next()?.ok()?;let day=date.next()?.ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) {return None;}
    let leap=year%4==0 && (year%100!=0 || year%400==0);
    let max=match month {2=>if leap {29} else {28},4|6|9|11=>30,_=>31};if !(1..=max).contains(&day) {return None;}
    let mut time=time.split(':');let hour=time.next()?.parse::<i64>().ok()?;let minute=time.next()?.parse::<i64>().ok()?;
    let seconds=time.next()?;let (second,fraction)=seconds.split_once('.').unwrap_or((seconds,""));
    let second=second.parse::<i64>().ok()?;if time.next().is_some() || !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) || fraction.len()>9 || !fraction.bytes().all(|b|b.is_ascii_digit()) {return None;}
    let year=year-i64::from(month<=2);let era=year.div_euclid(400);let yoe=year-era*400;let m=month+if month>2 {-3} else {9};let doy=(153*m+2)/5+day-1;let days=era*146097+yoe*365+yoe/4-yoe/100+doy-719468;
    let seconds=u64::try_from(days*86400+hour*3600+minute*60+second).ok()?;
    let nanos=if fraction.is_empty() {0} else {fraction.parse::<u64>().ok()?*10u64.pow(9-fraction.len() as u32)};
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}
