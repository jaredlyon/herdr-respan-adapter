use crate::{config::Credentials, paths::Paths, Result};
use serde_json::{json, Value};
pub fn report(paths: &Paths, credentials: Option<&Credentials>) -> Result<Value> {
    let files = crate::spool::ready(&paths.state)?;
    let bytes: u64 = files.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
    let spans: u64 = files.iter().filter_map(|p| std::fs::read(p).ok()).map(|b| b.iter().filter(|&&c| c==b'\n').count() as u64).sum();
    let installed=crate::setup::installed_agents(&paths.state)?;
    let agents:Vec<Value>=crate::agents::AGENTS.iter().map(|agent| {
        let installed=installed.contains(agent.kind);
        let last_span_ns=std::fs::read(paths.state.join("agents").join(agent.kind).join("last_span_ns")).ok().and_then(|b|serde_json::from_slice::<u64>(&b).ok());
        json!({"kind":agent.kind,"installed":installed,"last_span_ns":last_span_ns,"agent":agent.kind,"adapter_available":true,"coverage":if installed {"adapter installed"} else {"lifecycle"}})
    }).collect();
    let flush=crate::flush::status(&paths.state);
    Ok(json!({"agents":agents,"spool_files":files.len(),"spool_bytes":bytes,"spool_spans":spans,"dropped_files":crate::spool::dropped(&paths.state),
        "month":flush.month,"spans_sent_month":flush.spans_sent_month,"flush":flush,"key_source":credentials.map(|c| c.source).unwrap_or("not configured"),
        "proxy_active":paths.state.join("proxy.json").exists()}))
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn registered_agent_ownership_activity_and_month_fields() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let first=crate::agents::AGENTS[0].kind;
        let initial=report(&paths,None).unwrap();assert_eq!(initial["agents"].as_array().unwrap().len(),crate::agents::AGENTS.len());assert_eq!(initial["spans_sent_month"],0);assert_eq!(initial["month"].as_str().unwrap().len(),7);
        for agent in initial["agents"].as_array().unwrap() {assert_eq!(agent["installed"],false);assert!(agent["last_span_ns"].is_null());}
        let ctx=crate::setup::SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();let plan=crate::setup::Plan {edits:vec![crate::setup::Edit::JsonSetIfAbsent {file:tmp.path.join("settings.json"),pointer:"/version".into(),value:json!(1),id:"herdr-respan-status".into()}],..Default::default()};crate::setup::apply(&ctx,first,&plan).unwrap();
        let mut hook=crate::record::HookCtx::new(paths.clone(),crate::config::Config::default(),first,Default::default(),None,false).unwrap();hook.emit(crate::record::SpanRecord {span_key:"status-test".into(),..Default::default()}).unwrap();hook.finish().unwrap();
        let mut flush=crate::flush::status(&paths.state);flush.spans_sent_month=7;flush.month_spans=7;crate::paths::atomic_json(&paths.state.join("status.json"),&flush).unwrap();
        let result=report(&paths,None).unwrap();assert_eq!(result["spans_sent_month"],7);let agent=result["agents"].as_array().unwrap().iter().find(|a|a["kind"]==first).unwrap();assert_eq!(agent["installed"],true);assert!(agent["last_span_ns"].as_u64().unwrap()>0);
        crate::setup::remove(&ctx,&[first.into()]).unwrap();let removed=report(&paths,None).unwrap();assert_eq!(removed["agents"].as_array().unwrap().iter().find(|a|a["kind"]==first).unwrap()["installed"],false);
    }
}
