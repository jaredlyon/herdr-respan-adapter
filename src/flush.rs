use crate::{config::{Config, Credentials}, paths::Paths, record::SpanRecord, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::{BufRead, BufReader, Read}, path::Path, time::Duration};

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
pub fn run(paths: &Paths, config: &Config, creds: Option<&Credentials>) -> Result<()> {
    let Some(lock) = crate::spool::Lock::acquire(&paths.state)? else { return Ok(()); };
    let mut info = status(&paths.state);
    let result = (|| {
        settle_agents(paths,config,crate::agents::AGENTS,&lock,std::thread::sleep)?;
        loop {
            drain(paths, config, creds, &lock, &mut info)?;
            if crate::spool::ready(&paths.state)?.is_empty() {
                if !crate::agents::common::bridge::has_pending(&paths.state)? { return Ok(()); }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    })();
    info.last_flush_ns = crate::record::now_ns();
    info.last_result = if result.is_ok() { "ok" } else { "failed (records retained)" }.into();
    crate::paths::atomic_json(&paths.state.join("status.json"), &info)?;
    drop(lock);
    if result.is_ok() && (!crate::spool::ready(&paths.state)?.is_empty() || crate::agents::common::bridge::has_pending(&paths.state)?) && !cfg!(test) { crate::spool::spawn_flush(paths)?; }
    result
}
fn settle_agents(paths:&Paths,config:&Config,agents:&[crate::agents::AgentSpec],lock:&crate::spool::Lock,mut wait:impl FnMut(Duration))->Result<()> {
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
                match std::fs::remove_file(marker) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
            }
        }
        if !pending||round==2 {break;}wait(Duration::from_secs(2));
    }Ok(())
}

fn drain(paths: &Paths, config: &Config, creds: Option<&Credentials>, lock: &crate::spool::Lock, info: &mut FlushStatus) -> Result<()> {
    crate::agents::common::bridge::at_flush(paths, config, crate::record::now_ns())?;
    let files = crate::spool::ready(&paths.state)?;
    if files.is_empty() { return Ok(()); }
    let creds = creds.ok_or("no Respan API key configured")?;
    let mut records = Vec::new(); let mut groups = Vec::new();
    for path in files {
        let start = records.len();
        match std::fs::File::open(&path) {
            Ok(file) => { for line in BufReader::new(file).lines() {
                let line = line?; if line.trim().is_empty() { continue; }
                let rec: SpanRecord = serde_json::from_str(&line)?;
                if rec.v != 1 { return Err("unsupported spool version".into()); }
                records.push(rec);
            } },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
        groups.push((path, start, records.len()));
    }
    crate::otlp::reparent(&mut records);
    for (path, start, end) in groups {
        let mut offset = start;
        while offset < end {
            lock.refresh()?;
            let next = (offset + 512).min(end);
            post(config, &creds.key, &crate::otlp::payload(&records[offset..next]))?;
            info.sent((next-offset) as u64,crate::record::now_ns()/1_000_000_000);
            if next < end {
                let mut rest = Vec::new();
                for rec in &records[next..end] { serde_json::to_writer(&mut rest, rec)?; rest.push(b'\n'); }
                let tmp = path.with_extension("ack"); crate::paths::write_private(&tmp, &rest)?; std::fs::rename(tmp, &path)?;
            }
            offset = next;
        }
        match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    }
    Ok(())
}
pub fn post(config: &Config, key: &str, payload: &Value) -> Result<()> {
    let base = config.base_url.trim_end_matches('/');
    if !base.starts_with("https://") {
        let authority = base.strip_prefix("http://").and_then(|s| s.split('/').next()).ok_or("base URL must use HTTPS (HTTP allowed only on loopback)")?;
        let host = authority.split(':').next().unwrap_or_default();
        if host != "127.0.0.1" && host != "localhost" { return Err("HTTP allowed only on loopback".into()); }
    }
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
                        if rejected > 0 || partial.get("errorMessage").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) { return Err("OTLP partial rejection (records retained)".into()); }
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
}
