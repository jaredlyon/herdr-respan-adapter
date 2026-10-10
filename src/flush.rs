use crate::{config::{Config, Credentials}, paths::Paths, record::SpanRecord, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet,io::{BufRead,BufReader,Read},path::{Path,PathBuf},time::Duration};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlushStatus { pub last_flush_ns: u64, pub last_result: String, pub delivered_spans: u64, pub month: String, pub month_spans: u64, pub spans_sent_month: u64 }
pub fn status(state: &Path) -> FlushStatus {
    let mut info:FlushStatus=std::fs::read(state.join("status.json")).ok().and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or_default();
    if info.spans_sent_month==0 {info.spans_sent_month=info.month_spans;}
    info.roll_month(crate::record::now_ns()/1_000_000_000);info
}
impl FlushStatus {
    fn roll_month(&mut self,seconds:u64) {
        let current=month(seconds);
        if self.month!=current {self.month=current;self.spans_sent_month=0;self.month_spans=0;}
    }
    fn sent(&mut self,count:u64,seconds:u64) {
        self.roll_month(seconds);self.delivered_spans+=count;self.spans_sent_month+=count;self.month_spans=self.spans_sent_month;
    }
}
pub fn run(paths:&Paths,config:&Config,creds:Option<&Credentials>)->Result<()> {
    run_with_agents(paths,config,creds,crate::agents::AGENTS,std::thread::sleep)
}
fn run_with_agents(paths:&Paths,config:&Config,creds:Option<&Credentials>,agents:&[crate::agents::AgentSpec],mut wait:impl FnMut(Duration))->Result<()> {
    let Some(lock) = crate::spool::Lock::acquire(&paths.state)? else { return Ok(()); };
    let mut info = status(&paths.state);
    let mut held_files=BTreeSet::new();
    let result = (|| {
        loop {
            let deferred=settle_agents(paths,config,agents,&lock,&mut wait)?;
            held_files=drain(paths,config,creds,&lock,&mut info)?;
            let sendable=crate::spool::ready(&paths.state)?.iter().any(|p|!held_files.contains(p));
            let bridge_pending=crate::agents::common::bridge::has_pending(&paths.state)?||crate::lifecycle::has_pending(&paths.state)?;
            let native_pending=crate::spool::has_settle_pending(&paths.state);
            if !sendable&&!bridge_pending&&!deferred&&!native_pending {return Ok(());}
            if deferred||native_pending {wait(Duration::from_secs(2));}
            else if !sendable {wait(Duration::from_millis(1));}
        }
    })();
    info.last_flush_ns = crate::record::now_ns();
    info.last_result = if result.is_ok() { "ok" } else { "failed (records retained)" }.into();
    crate::paths::atomic_json(&paths.state.join("status.json"), &info)?;
    drop(lock);
    if result.is_ok() && (crate::spool::ready(&paths.state)?.iter().any(|p|!held_files.contains(p)) || crate::agents::common::bridge::has_pending(&paths.state)? || crate::spool::has_settle_pending(&paths.state) || crate::lifecycle::has_pending(&paths.state)?) && !cfg!(test) { crate::spool::spawn_flush(paths)?; }
    result
}
fn settle_agents(paths:&Paths,config:&Config,agents:&[crate::agents::AgentSpec],lock:&crate::spool::Lock,mut wait:impl FnMut(Duration))->Result<bool> {
    for round in 0..3 {
        let mut pending=false;
        for agent in agents {
            let Some(settle)=agent.settle else {continue;};
            if !paths.state.join("agents").join(agent.kind).is_dir() {continue;}
            lock.refresh()?;
            let mut ctx=crate::record::HookCtx::new(paths.clone(),config.clone(),agent.kind,Default::default(),None,false)?;
            let marker=ctx.state_dir().join("settle-pending");let before=match std::fs::read(&marker) {Ok(bytes)=>Some(bytes),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>None,Err(e)=>return Err(e.into())};
            let still_pending=settle(&mut ctx)?;pending|=still_pending;ctx.finish()?;
            if !still_pending&&before.is_some()&&std::fs::read(&marker).ok()==before {
                match std::fs::remove_file(&marker) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
            }
            pending|=marker.exists();
        }
        if !pending||round==2 {return Ok(pending);}wait(Duration::from_secs(2));
    }Ok(false)
}

fn drain(paths:&Paths,config:&Config,creds:Option<&Credentials>,lock:&crate::spool::Lock,info:&mut FlushStatus)->Result<BTreeSet<PathBuf>> {
    let now=crate::record::now_ns();
    crate::agents::common::bridge::at_flush(paths,config,now)?;
    crate::lifecycle::at_flush(paths,config)?;
    crate::otlp::gc_turns(&paths.state,now)?;
    let files=crate::spool::ready(&paths.state)?;let mut held_files=BTreeSet::new();
    if files.is_empty() {return Ok(held_files);}
    let mut records=Vec::new();let mut groups=Vec::new();
    for path in files {
        let start=records.len();
        match std::fs::File::open(&path) {
            Ok(file)=>{for line in BufReader::new(file).lines() {
                let line=line?;if line.trim().is_empty() {continue;}
                let record:SpanRecord=serde_json::from_str(&line)?;
                if record.v!=1 {return Err("unsupported spool version".into());}records.push(record);
            }},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>continue,Err(e)=>return Err(e.into()),
        }
        groups.push((path,start,records.len()));
    }
    let mut held=crate::otlp::correlate_proxy(&paths.state,&mut records,now)?;
    for (path,start,end) in groups {
        // Partition in place: sendable records first, held records retained at the tail.
        // Successful batch acknowledgements therefore retain both unsent and held records.
        let mut send_end=start;
        for index in start..end {if !held[index] {records.swap(send_end,index);held.swap(send_end,index);send_end+=1;}}
        let mut offset=start;
        while offset<send_end {
            lock.refresh()?;let next=(offset+512).min(send_end);
            let creds=creds.ok_or("no Respan API key configured")?;
            post_with_warning(config,&creds.key,&crate::otlp::payload(&records[offset..next]),||crate::log::debug(&paths.state,"OTLP accepted spans with a warning"))?;
            info.sent((next-offset) as u64,crate::record::now_ns()/1_000_000_000);
            if next<end {retain_records(&paths.state,&path,&records[next..end])?;}
            offset=next;
        }
        if send_end<end {held_files.insert(path);}
        else {let _cap=cap_guard(&paths.state)?;match std::fs::remove_file(path) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}}
    }
    Ok(held_files)
}
fn cap_guard(state:&Path)->Result<std::fs::File> {
    loop {if let Some(guard)=crate::spool::cap_lock(state)? {return Ok(guard);}std::thread::sleep(Duration::from_millis(1));}
}
fn retain_records(state:&Path,path:&Path,records:&[SpanRecord])->Result<()> {
    let _cap=cap_guard(state)?;
    if !path.exists() {return Ok(());}
    let mut rest=Vec::new();for record in records {serde_json::to_writer(&mut rest,record)?;rest.push(b'\n');}
    let temporary=path.with_extension("ack");crate::paths::write_private(&temporary,&rest)?;std::fs::rename(temporary,path)?;Ok(())
}

fn validate_base_url(base:&str)->Result<()> {
    let uri:hyper::Uri=base.parse().map_err(|_|"invalid OTLP base URL")?;
    let authority=uri.authority().ok_or("OTLP base URL needs a host")?;
    if authority.as_str().contains('@') {return Err("OTLP URL userinfo is not allowed".into());}
    let scheme=uri.scheme_str().ok_or("OTLP base URL needs HTTP or HTTPS")?;
    let host=uri.host().ok_or("OTLP base URL needs a host")?;
    if scheme.eq_ignore_ascii_case("https") {return Ok(());}
    let address=host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok();
    if scheme.eq_ignore_ascii_case("http")&&(host.eq_ignore_ascii_case("localhost")||address.is_some_and(|ip|ip.is_loopback())) {return Ok(());}
    Err("base URL must use HTTPS (HTTP allowed only on loopback)".into())
}
pub fn post(config:&Config,key:&str,payload:&Value)->Result<()> {
    post_with_warning(config,key,payload,||eprintln!("OTLP accepted spans with a warning"))
}
fn post_with_warning(config:&Config,key:&str,payload:&Value,mut warning:impl FnMut())->Result<()> {
    let base=config.base_url.trim_end_matches('/');validate_base_url(base)?;
    let url = if base.ends_with("/api") { format!("{base}/v2/traces") } else { format!("{base}/api/v2/traces") };
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(15)).build();
    let body = serde_json::to_vec(payload)?;
    for attempt in 0..3 {
        match agent.post(&url).set("Authorization", &format!("Bearer {key}")).set("Content-Type", "application/json").set("X-Respan-Dogfood", "1").send_bytes(&body) {
            Ok(response) => {
                if !(200..300).contains(&response.status()) { return Err("OTLP redirect refused".into()); }
                let mut text = String::new(); response.into_reader().take(1024 * 1024).read_to_string(&mut text)?;
                if !text.trim().is_empty() {
                    let value: Value = serde_json::from_str(&text)?;
                    if let Some(partial) = value.get("partialSuccess") {
                        let rejected = partial.get("rejectedSpans").map(|v| v.as_u64().unwrap_or_else(|| v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0))).unwrap_or(0);
                        if rejected>0 {return Err("OTLP partial rejection (records retained)".into());}
                        if partial.get("errorMessage").and_then(Value::as_str).is_some_and(|s|!s.is_empty()) {warning();}
                    }
                }
                return Ok(());
            }
            Err(ureq::Error::Status(code, _)) if code < 500 => return Err(format!("OTLP HTTP {code}").into()),
            Err(_) if attempt < 2 => std::thread::sleep(Duration::from_millis(50 * (attempt+1))),
            Err(_) => return Err("OTLP delivery failed after 3 attempts".into()),
        }
    }
    Err("OTLP delivery failed".into())
}
// Gregorian civil date calculation avoids a date/time crate just for the MTD counter.
fn month(seconds: u64) -> String {
    let z = (seconds / 86400) as i64 + 719468; let era = z / 146097; let doe = z - era * 146097;
    let yoe = (doe - doe/1460 + doe/36524 - doe/146096)/365; let mut year = yoe + era*400;
    let doy = doe - (365*yoe + yoe/4 - yoe/100); let mp = (5*doy+2)/153;
    let m = mp + if mp < 10 {3} else {-9}; year += i64::from(m <= 2);
    format!("{year:04}-{m:02}")
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn calendar_month() { assert_eq!(month(0), "1970-01"); assert_eq!(month(1704067200), "2024-01"); }
    fn unused_hook(_: &mut crate::record::HookCtx,_:&[u8])->Result<()> {Ok(())}
    fn unused_plan(_: &crate::setup::SetupCtx)->Result<crate::setup::Plan> {Ok(Default::default())}
    fn settling(ctx:&mut crate::record::HookCtx)->Result<bool> {
        assert!(ctx.metadata.is_empty());assert!(ctx.run_id.is_none());assert!(!ctx.proxy_active);
        let count:u64=ctx.load("settle-count")?;let count=count+1;ctx.save("settle-count",&count)?;
        if count==3 {ctx.emit(SpanRecord {span_key:"settled".into(),..Default::default()})?;}Ok(count<3)
    }
    fn never_ready(ctx:&mut crate::record::HookCtx)->Result<bool> {let count:u64=ctx.load("settle-count")?;ctx.save("settle-count",&(count+1))?;Ok(true)}
    fn settle_error(_: &mut crate::record::HookCtx)->Result<bool> {Err("settle error".into())}
    fn spec(kind:&'static str,settle:fn(&mut crate::record::HookCtx)->Result<bool>)->crate::agents::AgentSpec {crate::agents::AgentSpec {kind,display:kind,hook:unused_hook,plan:unused_plan,reply:None,settle:Some(settle)}}
    #[test] fn settlement_retries_three_rounds_before_sending() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let config=Config::default();let agents=[spec("settler",settling),spec("never",never_ready),spec("absent",settle_error)];
        for kind in ["settler","never"] {let ctx=crate::record::HookCtx::new(paths.clone(),config.clone(),kind,Default::default(),None,false).unwrap();ctx.mark_settle_pending().unwrap();}let lock=crate::spool::Lock::acquire(&paths.state).unwrap().unwrap();let mut waits=Vec::new();
        settle_agents(&paths,&config,&agents,&lock,|d|waits.push(d)).unwrap();assert_eq!(waits,vec![Duration::from_secs(2),Duration::from_secs(2)]);
        for kind in ["settler","never"] {let bytes=std::fs::read(paths.state.join("agents").join(kind).join("settle-count")).unwrap();assert_eq!(serde_json::from_slice::<u64>(&bytes).unwrap(),3);}
        assert!(!paths.state.join("agents/absent").exists());assert!(!paths.state.join("agents/settler/settle-pending").exists());assert!(paths.state.join("agents/never/settle-pending").exists());let ready=crate::spool::ready(&paths.state).unwrap();assert_eq!(ready.len(),1);let rec:SpanRecord=serde_json::from_slice(&std::fs::read(&ready[0]).unwrap()).unwrap();assert_eq!(rec.span_key,"settled");
        let mut waits=Vec::new();settle_agents(&paths,&config,&[spec("settler",settling)],&lock,|d|waits.push(d)).unwrap();assert!(waits.is_empty());
        std::fs::create_dir_all(paths.state.join("agents/error")).unwrap();assert!(settle_agents(&paths,&config,&[spec("error",settle_error)],&lock,|_|{}).is_err());
    }
    #[test] fn utc_month_rollover_and_successful_send_counter() {
        let mut status=FlushStatus::default();status.sent(2,1706745599);assert_eq!(status.month,"2024-01");assert_eq!(status.spans_sent_month,2);
        status.sent(3,1706745600);assert_eq!(status.month,"2024-02");assert_eq!(status.spans_sent_month,3);assert_eq!(status.month_spans,3);assert_eq!(status.delivered_spans,5);
        status.roll_month(1709251200);assert_eq!(status.month,"2024-03");assert_eq!(status.spans_sent_month,0);assert_eq!(status.month_spans,0);assert_eq!(status.delivered_spans,5);
    }

    #[test] fn settlement_preserves_newly_queued_marker_generation() {
        fn new_queue(ctx:&mut crate::record::HookCtx)->Result<bool> {ctx.mark_settle_pending()?;Ok(false)}
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let config=Config::default();let ctx=crate::record::HookCtx::new(paths.clone(),config.clone(),"new-queue",Default::default(),None,false).unwrap();ctx.mark_settle_pending().unwrap();let marker=ctx.state_dir().join("settle-pending");let before=std::fs::read(&marker).unwrap();let lock=crate::spool::Lock::acquire(&paths.state).unwrap().unwrap();
        settle_agents(&paths,&config,&[spec("new-queue",new_queue)],&lock,|_|{}).unwrap();assert_ne!(std::fs::read(marker).unwrap(),before);
    }

}

#[cfg(test)] mod delivery_tests {
    use super::*;
    use crate::record::{HookCtx, Usage};
    use std::{collections::BTreeMap, io::Write, net::TcpListener, thread};
    fn mock(responses: Vec<(u16, &'static str)>) -> (String, thread::JoinHandle<Vec<(String, Value)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap(); listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let deadline = std::time::Instant::now()+Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream,_)) => break stream,
                        Err(e) if e.kind()==std::io::ErrorKind::WouldBlock && std::time::Instant::now()<deadline => thread::sleep(Duration::from_millis(1)),
                        Err(e) => panic!("mock accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap(); // Windows accepts inherit the listener mode.
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap()); let mut headers = String::new(); let mut length=0;
                loop { let mut line=String::new(); reader.read_line(&mut line).unwrap();
                    if line=="\r\n" {break;}
                    if let Some(raw)=line.to_ascii_lowercase().strip_prefix("content-length:") {length=raw.trim().parse::<usize>().unwrap();}
                    headers.push_str(&line);
                }
                let mut body=vec![0;length]; reader.read_exact(&mut body).unwrap();
                requests.push((headers,serde_json::from_slice(&body).unwrap()));
                write!(stream,"HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
            }
            requests
        });
        (format!("http://{address}"),task)
    }
    fn paths(tmp: &crate::tests::Temp) -> Paths { Paths {state:tmp.path.join("state"),config:tmp.path.join("config")} }
    fn key() -> Credentials { Credentials {key:"mock-key-not-a-provider-credential".into(),source:"test"} }
    #[test] fn redacted_spool_flush_matches_golden() {
        let tmp=crate::tests::Temp::new(); let paths=paths(&tmp); let (base_url,server)=mock(vec![(200,r#"{"partialSuccess":{}}"#)]);
        let config=Config {base_url,customer_identifier:"customer".into(),..Default::default()};
        let mut ctx=HookCtx::new(paths.clone(),config.clone(),"fake",BTreeMap::from([("herdr.workspace_id".into(),serde_json::json!("w"))]),None,false).unwrap();
        ctx.emit(SpanRecord {trace_key:Some("abc".into()),span_key:"abc".into(),name:"Golden chat".into(),log_type:"chat".into(),start_ns:100,end_ns:200,status:"error".into(),status_message:Some("bad".into()),model:Some("gpt-test".into()),provider:Some("OpenAI".into()),
            input:Some(serde_json::json!([{"role":"system","content":"system"},{"role":"user","content":"sk-abcdefghijklmnopqrstuvwxyz"}])),output:Some(serde_json::json!("ok")),thread_id:Some("session".into()),
            usage:Usage {input_tokens:Some(1),output_tokens:Some(2),cache_read_input_tokens:Some(3),cache_creation_input_tokens:Some(4),reasoning_tokens:Some(5)},..Default::default()}).unwrap();ctx.finish().unwrap();
        let spool=crate::spool::ready(&paths.state).unwrap();assert_eq!(spool.len(),1);assert!(!std::fs::read_to_string(&spool[0]).unwrap().contains("abcdefghijklmnopqrstuvwxyz"));
        run(&paths,&config,Some(&key())).unwrap(); let requests=server.join().unwrap();assert_eq!(requests.len(),1);
        let golden:Value=serde_json::from_str(include_str!("setup/otlp-golden.json")).unwrap();assert_eq!(requests[0].1,golden);
        assert!(requests[0].0.starts_with("POST /api/v2/traces HTTP/1.1"));assert!(requests[0].0.to_ascii_lowercase().contains("authorization: bearer mock-key-not-a-provider-credential"));
        assert!(crate::spool::ready(&paths.state).unwrap().is_empty());assert_eq!(status(&paths.state).last_result,"ok");assert_eq!(status(&paths.state).month_spans,1);assert_eq!(status(&paths.state).spans_sent_month,1);
    }
    #[test] fn batching_retry_and_rejection_retention() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);
        let (base_url,server)=mock(vec![(503,"{}"),(503,"{}"),(200,"{}"),(200,"{}")]);let config=Config {base_url,..Default::default()};
        let mut writer=crate::spool::Writer::new(&paths.state,u64::MAX).unwrap();
        for i in 0..513 {writer.append(&SpanRecord {span_key:format!("s{i}"),..Default::default()}).unwrap();}writer.finish().unwrap();
        run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();assert_eq!(requests.len(),4);
        assert_eq!(requests[2].1["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().len(),512);
        assert_eq!(requests[3].1["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().len(),1);
        for response in [(400,"{}"),(200,r#"{"partialSuccess":{"rejectedSpans":"1","errorMessage":"rejected"}}"#),(200,"bad-json")] {
            let (base_url,server)=mock(vec![response]);let config=Config {base_url,..Default::default()};
            let mut writer=crate::spool::Writer::new(&paths.state,u64::MAX).unwrap();writer.append(&SpanRecord {span_key:"retained".into(),..Default::default()}).unwrap();writer.finish().unwrap();
            let file=crate::spool::ready(&paths.state).unwrap()[0].clone();let original=std::fs::read(&file).unwrap();
            let sent=status(&paths.state).spans_sent_month;assert!(run(&paths,&config,Some(&key())).is_err());server.join().unwrap();assert_eq!(status(&paths.state).spans_sent_month,sent);assert_eq!(std::fs::read(&file).unwrap(),original);std::fs::remove_file(file).unwrap();
        }
    }
    #[test] fn acknowledged_batches_are_not_replayed() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}"),(400,"{}")]);let config=Config {base_url,..Default::default()};
        let mut writer=crate::spool::Writer::new(&paths.state,u64::MAX).unwrap();for i in 0..513 {writer.append(&SpanRecord {span_key:format!("s{i}"),..Default::default()}).unwrap();}writer.finish().unwrap();
        assert!(run(&paths,&config,Some(&key())).is_err());server.join().unwrap();let retained=crate::spool::ready(&paths.state).unwrap();assert_eq!(retained.len(),1);assert_eq!(std::fs::read_to_string(&retained[0]).unwrap().lines().count(),1);
        assert_eq!(status(&paths.state).delivered_spans,512);
    }
    #[test] fn network_failure_and_no_key_retain_spool() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);
        let mut writer=crate::spool::Writer::new(&paths.state,u64::MAX).unwrap();writer.append(&SpanRecord {span_key:"retained".into(),..Default::default()}).unwrap();writer.finish().unwrap();
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=listener.local_addr().unwrap();drop(listener);
        let config=Config {base_url:format!("http://{address}"),..Default::default()};
        assert!(run(&paths,&config,None).is_err());assert!(run(&paths,&config,Some(&key())).is_err());assert_eq!(crate::spool::ready(&paths.state).unwrap().len(),1);
        assert_eq!(status(&paths.state).last_result,"failed (records retained)");
    }
    fn register(paths:&Paths,run:&str) {crate::paths::atomic_json(&paths.state.join("proxy/runs").join(format!("{run}.json")),&serde_json::json!({})).unwrap();}
    fn turn(ctx:&mut HookCtx,id:&str,start:u64,end:Option<u64>) {
        let mut events=vec![serde_json::json!({"type":"turn_start","turn_id":id,"ts_ns":start})];if let Some(end)=end {events.push(serde_json::json!({"type":"turn_end","turn_id":id,"ts_ns":end}));}
        crate::agents::common::bridge::ingest(ctx,serde_json::from_value(serde_json::json!({"v":1,"agent":"fake","session_id":"s","events":events})).unwrap()).unwrap();
    }
    fn proxy_records(paths:&Paths,records:&[SpanRecord]) {
        let mut writer=crate::spool::Writer::new(&paths.state,u64::MAX).unwrap();for record in records {writer.append(record).unwrap();}writer.finish().unwrap();
    }
    fn proxy_record(key:&str,start:u64)->SpanRecord {SpanRecord {span_key:key.into(),run_id:Some("run".into()),source:"proxy".into(),log_type:"chat".into(),start_ns:start,end_ns:start+1,agent:"fake".into(),..Default::default()}}
    fn spans(requests:&[(String,Value)])->Vec<&Value> {requests.iter().flat_map(|(_,payload)|payload["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap()).collect()}
    #[test] fn proxy_before_delayed_turn_is_held_then_parented() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let config=Config {customer_identifier:"test".into(),..Default::default()};register(&paths,"run");let now=crate::record::now_ns();proxy_records(&paths,&[proxy_record("early",now)]);
        let file=crate::spool::ready(&paths.state).unwrap()[0].clone();let original=std::fs::read(&file).unwrap();let start=std::time::Instant::now();run(&paths,&config,None).unwrap();assert!(start.elapsed()<Duration::from_secs(1));assert_eq!(std::fs::read(&file).unwrap(),original);assert_eq!(status(&paths.state).spans_sent_month,0);
        let mut ctx=HookCtx::new(paths.clone(),config.clone(),"fake",BTreeMap::new(),Some("run".into()),true).unwrap();turn(&mut ctx,"t",now.saturating_sub(1),None);
        let (base_url,server)=mock(vec![(200,"{}")]);let config=Config {base_url,..config};run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();let sent=spans(&requests);assert_eq!(sent.len(),1);assert_eq!(sent[0]["traceId"],crate::otlp::trace_id("fake:s:t"));assert_eq!(sent[0]["parentSpanId"],crate::otlp::span_id("fake:s:t:turn"));assert!(crate::spool::ready(&paths.state).unwrap().is_empty());
    }
    #[test] fn proxy_during_open_turn_is_parented_without_a_root_spool_record() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}")]);let config=Config {base_url,customer_identifier:"test".into(),..Default::default()};register(&paths,"run");let now=crate::record::now_ns();let mut ctx=HookCtx::new(paths.clone(),config.clone(),"fake",BTreeMap::new(),Some("run".into()),true).unwrap();turn(&mut ctx,"open",now,None);assert!(crate::spool::ready(&paths.state).unwrap().is_empty());
        proxy_records(&paths,&[proxy_record("during",now+1)]);run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();let sent=spans(&requests);assert_eq!(sent.len(),1);assert_eq!(sent[0]["parentSpanId"],crate::otlp::span_id("fake:s:open:turn"));
    }
    #[test] fn ended_run_without_a_turn_is_sent_standalone() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}")]);let config=Config {base_url,..Default::default()};register(&paths,"run");std::fs::remove_file(paths.state.join("proxy/runs/run.json")).unwrap();proxy_records(&paths,&[proxy_record("standalone",crate::record::now_ns())]);run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();let sent=spans(&requests);assert_eq!(sent.len(),1);assert_eq!(sent[0]["traceId"],crate::otlp::trace_id("run"));assert!(sent[0].get("parentSpanId").is_none());
    }
    #[test] fn two_turns_and_post_stop_summary_use_the_correct_preceding_turn() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}"),(200,"{}")]);let config=Config {base_url,customer_identifier:"test".into(),..Default::default()};register(&paths,"run");let now=crate::record::now_ns();let mut ctx=HookCtx::new(paths.clone(),config.clone(),"fake",BTreeMap::new(),Some("run".into()),true).unwrap();
        turn(&mut ctx,"first",now,Some(now+10));turn(&mut ctx,"second",now+30,None);proxy_records(&paths,&[proxy_record("in-first",now+5),proxy_record("summary",now+20),proxy_record("in-second",now+35)]);run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();let sent=spans(&requests);
        for (key,turn) in [("in-first","first"),("summary","first"),("in-second","second")] {let span=sent.iter().find(|s|s["spanId"]==crate::otlp::span_id(key)).unwrap();assert_eq!(span["parentSpanId"],crate::otlp::span_id(&format!("fake:s:{turn}:turn")));assert_eq!(span["traceId"],crate::otlp::trace_id(&format!("fake:s:{turn}")));}
        let windows:Vec<crate::otlp::TurnWindow>=serde_json::from_slice(&std::fs::read(paths.state.join("proxy/turns/run.json")).unwrap()).unwrap();assert_eq!(windows.len(),2);assert_eq!(windows[0].end_ns,Some(now+10));assert!(windows[1].end_ns.is_none());
    }
    #[test] fn mixed_spool_acknowledges_sendable_records_but_retains_held_requests() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}")]);let config=Config {base_url,..Default::default()};register(&paths,"run");let now=crate::record::now_ns();let held=proxy_record("held",now);let regular=SpanRecord {span_key:"regular".into(),..Default::default()};proxy_records(&paths,&[held,regular]);
        run(&paths,&config,Some(&key())).unwrap();let requests=server.join().unwrap();assert_eq!(spans(&requests).len(),1);assert_eq!(status(&paths.state).spans_sent_month,1);let ready=crate::spool::ready(&paths.state).unwrap();assert_eq!(ready.len(),1);let retained:SpanRecord=serde_json::from_slice(&std::fs::read(&ready[0]).unwrap()).unwrap();assert_eq!(retained.span_key,"held");
    }
    #[test] fn native_work_finishing_after_the_third_settle_pass_still_delivers() {
        fn noop(_: &mut HookCtx,_:&[u8])->Result<()> {Ok(())}
        fn plan(_: &crate::setup::SetupCtx)->Result<crate::setup::Plan> {Ok(Default::default())}
        fn late(ctx:&mut HookCtx)->Result<bool> {
            let count:u64=ctx.load("late-count")?;ctx.save("late-count",&(count+1))?;
            if count<3 {return Ok(true);}ctx.emit(SpanRecord {span_key:"late-after-four-seconds".into(),..Default::default()})?;Ok(false)
        }
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,"{}")]);let config=Config {base_url,..Default::default()};let ctx=HookCtx::new(paths.clone(),config.clone(),"late",BTreeMap::new(),None,false).unwrap();ctx.mark_settle_pending().unwrap();let agent=crate::agents::AgentSpec {kind:"late",display:"late",hook:noop,plan,reply:None,settle:Some(late)};let mut waits=Vec::new();
        run_with_agents(&paths,&config,Some(&key()),&[agent],|delay|waits.push(delay)).unwrap();let requests=server.join().unwrap();assert_eq!(spans(&requests).len(),1);assert_eq!(status(&paths.state).spans_sent_month,1);assert!(!ctx.state_dir().join("settle-pending").exists());assert_eq!(waits,vec![Duration::from_secs(2);3]);assert_eq!(ctx.load::<u64>("late-count").unwrap(),4);
    }
    #[test] fn zero_rejection_partial_success_warning_acknowledges_the_batch() {
        let tmp=crate::tests::Temp::new();let paths=paths(&tmp);let (base_url,server)=mock(vec![(200,r#"{"partialSuccess":{"rejectedSpans":"0","errorMessage":"non-fatal warning"}}"#)]);let config=Config {base_url,..Default::default()};proxy_records(&paths,&[SpanRecord {span_key:"accepted".into(),..Default::default()}]);run(&paths,&config,Some(&key())).unwrap();server.join().unwrap();assert!(crate::spool::ready(&paths.state).unwrap().is_empty());assert_eq!(status(&paths.state).spans_sent_month,1);assert_eq!(status(&paths.state).last_result,"ok");
    }
    #[test] fn plaintext_url_guard_uses_the_real_authority_and_rejects_userinfo() {
        for base_url in ["http://localhost:80@127.0.0.1:0","https://username@127.0.0.1:0"] {
            let error=post(&Config {base_url:base_url.into(),..Default::default()},"mock-key",&serde_json::json!({})).unwrap_err().to_string();assert!(error.contains("userinfo"),"{error}");
        }
        for url in ["http://127.0.0.1:1","http://127.0.0.2:1","http://[::1]:1","http://localhost:1","https://collector.example"] {validate_base_url(url).unwrap();}
        for url in ["http://127.0.0.1.evil:1","http://collector.example","http://[::2]:1","ftp://localhost:1","http://localhost:80@collector.example"] {assert!(validate_base_url(url).is_err(),"{url}");}
    }
}
