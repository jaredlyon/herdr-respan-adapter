use crate::{config::Config, paths::Paths, redact::Redactor, spool::Writer, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::{Path, PathBuf}};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>, pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>, pub cache_creation_input_tokens: Option<u64>, pub reasoning_tokens: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SpanRecord {
    pub v: u8, pub trace_key: Option<String>, pub span_key: String, pub parent_key: Option<String>, pub run_id: Option<String>,
    pub name: String, pub log_type: String, pub start_ns: u64, pub end_ns: u64, pub status: String, pub status_message: Option<String>,
    pub model: Option<String>, pub provider: Option<String>, pub input: Option<Value>, pub output: Option<Value>, pub usage: Usage,
    pub thread_id: Option<String>, pub agent: String, pub source: String, pub metadata: BTreeMap<String, Value>,
}
impl Default for SpanRecord {
    fn default() -> Self { Self { v: 1, trace_key: None, span_key: String::new(), parent_key: None, run_id: None,
        name: String::new(), log_type: "agent".into(), start_ns: 0, end_ns: 0, status: "ok".into(), status_message: None,
        model: None, provider: None, input: None, output: None, usage: Usage::default(), thread_id: None, agent: String::new(), source: "adapter".into(), metadata: BTreeMap::new() } }
}
pub struct HookCtx {
    pub config: Config, pub agent: String, pub metadata: BTreeMap<String, Value>, pub run_id: Option<String>,
    pub customer_identifier: String,
    state_dir: PathBuf, paths: Paths, redactor: Redactor, writer: Option<Writer>, pub proxy_active: bool,
    suppressed_parents: BTreeMap<String,Option<String>>,
}
impl HookCtx {
    pub fn new(paths: Paths, config: Config, agent: &str, metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool) -> Result<Self> {
        if agent.is_empty() || !agent.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { return Err("invalid agent kind".into()); }
        let customer_identifier = if config.customer_identifier.is_empty() { crate::meta::hostname() } else { config.customer_identifier.clone() };
        let redactor = Redactor::new(&config.redact_patterns)?;
        let state_dir = paths.state.join("agents").join(agent); std::fs::create_dir_all(&state_dir)?;
        let suppressed_parents=match std::fs::read(state_dir.join("suppressed-parents.json")) {
            Ok(bytes)=>serde_json::from_slice(&bytes)?,
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>BTreeMap::new(),
            Err(e)=>return Err(e.into()),
        };
        Ok(Self { config, agent: agent.into(), metadata, run_id, customer_identifier, state_dir, paths, redactor, writer: None, proxy_active, suppressed_parents })
    }
    pub fn state_dir(&self) -> &Path { &self.state_dir }
    pub(crate) fn global_state_dir(&self)->&Path {&self.paths.state}
    pub fn now_ns(&self) -> u64 { now_ns() }
    /// Append an accepted record and stamp last_span_ns with the current UTC emission time.
    pub fn emit(&mut self, mut rec: SpanRecord) -> Result<()> {
        if rec.v != 1 || rec.span_key.is_empty() || rec.end_ns < rec.start_ns || !["agent", "chat", "tool", "task", "workflow"].contains(&rec.log_type.as_str()) || !["ok", "error"].contains(&rec.status.as_str()) { return Err("invalid span record".into()); }
        if rec.log_type=="tool" && !self.config.tool_spans {
            self.suppressed_parents.insert(rec.span_key.clone(),rec.parent_key.clone());
            self.save("suppressed-parents.json",&self.suppressed_parents)?;
            return Ok(());
        }
        // Resolve chains, not just spawning tools: retained children attach to the
        // nearest retained ancestor even when a suppressed tool ran under a subagent.
        let mut visited=std::collections::BTreeSet::new();
        while let Some(parent)=rec.parent_key.as_ref() {
            if self.config.tool_spans {break;}
            if !visited.insert(parent.clone()) {return Err("cyclic suppressed span parents".into());}
            if let Some(ancestor)=self.suppressed_parents.get(parent) {rec.parent_key=ancestor.clone();}
            else if !self.config.tool_spans && rec.trace_key.as_ref().is_some_and(|trace| parent.starts_with(&format!("{trace}:tool:"))) {
                rec.parent_key=rec.trace_key.as_ref().map(|trace| format!("{trace}:turn"));
            } else {break;}
        }
        if (rec.metadata.get("subagent").is_some_and(truthy) && !self.config.subagent_spans)
            || (rec.metadata.contains_key("hermes.aux_task") && !self.config.aux_spans)
            || (rec.log_type == "chat" && self.proxy_active && rec.source == "adapter") { return Ok(()); }
        rec.agent = self.agent.clone();
        if rec.run_id.is_none() { rec.run_id = self.run_id.clone(); }
        for (key, value) in &self.metadata { rec.metadata.entry(key.clone()).or_insert_with(|| value.clone()); }
        // Customer attribution stays within the record's declared metadata field.
        rec.metadata.insert("customer_identifier".into(), Value::String(self.customer_identifier.clone()));
        self.scrub_content(&mut rec.input)?; self.scrub_content(&mut rec.output)?;
        if rec.log_type!="chat" {self.cap_serialized(&mut rec.input)?;self.cap_serialized(&mut rec.output)?;}
        let mut value = serde_json::to_value(&rec)?;
        // Identity strings are redacted, but never truncated into collisions.
        for (key,field) in value.as_object_mut().expect("record object") {
            let cap=if ["trace_key","span_key","parent_key","run_id","thread_id","metadata"].contains(&key.as_str()) {usize::MAX} else {self.config.content_cap_bytes.max(16384)};
            self.redactor.value(field,cap);
        }
        rec = serde_json::from_value(value)?;
        if self.writer.is_none() { self.writer = Some(Writer::new(&self.paths.state, self.config.spool_max_bytes)?); }
        self.writer.as_mut().expect("writer initialized").append(&rec)?;
        crate::paths::atomic_json(&self.state_dir.join("last_span_ns"),&self.now_ns())
    }
    // State includes session/event ids and identity-keyed maps. Content is capped
    // explicitly before saving; a blanket string cap corrupts those identities.
    pub(crate) fn scrub_value(&self, value: &mut Value) { self.redactor.value(value, usize::MAX); }
    fn cap_serialized(&self, field: &mut Option<Value>) -> Result<()> {
        let Some(value)=field else {return Ok(());};
        let cap=self.config.content_cap_bytes;
        if serde_json::to_string(value)?.len()<=cap {return Ok(());}
        if cap<2 {*field=None;return Ok(());}
        let text=match value {Value::String(s)=>s.clone(),_=>serde_json::to_string(value)?};
        // JSON escaping consumes bytes too. Find a UTF-8-safe marked truncation
        // whose serialized representation fits, including the surrounding quotes.
        let (mut low,mut high)=(0,cap);let mut best=String::new();
        while low<=high {
            let budget=low+(high-low)/2;let candidate=crate::redact::cap(&text,budget);
            if serde_json::to_string(&candidate)?.len()<=cap {best=candidate;low=budget+1;}
            else if budget==0 {break;} else {high=budget-1;}
        }
        *value=Value::String(best);Ok(())
    }
    pub(crate) fn begin_staged(&mut self, name: &str) -> Result<()> {
        self.finish()?;
        self.writer=Some(Writer::staged(&self.paths.state,self.config.spool_max_bytes,name)?);
        Ok(())
    }
    pub(crate) fn scrub_content(&self, field: &mut Option<Value>) -> Result<()> {
        if self.config.content_cap_bytes == 0 { *field = None; }
        else if let Some(value) = field {
            self.redactor.value(value, self.config.content_cap_bytes);
            let text = if let Value::String(s) = value { s.clone() } else { serde_json::to_string(value)? };
            if text.len() > self.config.content_cap_bytes { *value = Value::String(crate::redact::cap_content(&text,self.config.content_cap_bytes)); }
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take() { writer.finish()?; }
        Ok(())
    }
}
fn truthy(v: &Value) -> bool { !v.is_null() && v != &Value::Bool(false) && v != &Value::String(String::new()) }
pub fn now_ns() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos().min(u64::MAX as u128) as u64 }
pub fn random_id() -> Result<String> { let mut bytes = [0; 16]; getrandom::getrandom(&mut bytes).map_err(|_| "random generator failed")?; Ok(bytes.iter().map(|b| format!("{b:02x}")).collect()) }

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn suppressed_tools_reparent_to_nearest_retained_ancestor() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let config=Config {tool_spans:false,..Default::default()};
        let mut ctx=HookCtx::new(paths.clone(),config.clone(),"fake",BTreeMap::new(),None,false).unwrap();
        ctx.emit(SpanRecord {span_key:"tool".into(),log_type:"tool".into(),parent_key:Some("sub".into()),..Default::default()}).unwrap();
        drop(ctx);
        let mut ctx=HookCtx::new(paths.clone(),config,"fake",BTreeMap::new(),None,false).unwrap();
        ctx.emit(SpanRecord {span_key:"child".into(),parent_key:Some("tool".into()),..Default::default()}).unwrap();ctx.finish().unwrap();
        let rec:SpanRecord=serde_json::from_slice(&std::fs::read(&crate::spool::ready(&paths.state).unwrap()[0]).unwrap()).unwrap();
        assert_eq!(rec.parent_key.as_deref(),Some("sub"));
    }
    #[test] fn serialized_non_chat_content_respects_tiny_and_newline_caps() {
        let tmp=crate::tests::Temp::new();let mut ctx=HookCtx::new(Paths {state:tmp.path.join("state"),config:tmp.path.join("config")},Config::default(),"fake",BTreeMap::new(),None,false).unwrap();
        for cap in [0,1,2,17,16384] {
            ctx.config.content_cap_bytes=cap;
            for text in ["\n".repeat(20000),"🧪é\n".repeat(15000)] {
                let mut field=Some(Value::String(text));ctx.scrub_content(&mut field).unwrap();ctx.cap_serialized(&mut field).unwrap();
                if let Some(value)=field {assert!(value.to_string().len()<=cap);}
            }
        }
    }
    #[test] fn proxy_suppresses_all_adapter_chats_and_tracks_only_emitted_activity() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let mut ctx=HookCtx::new(paths.clone(),Config {customer_identifier:"test".into(),..Default::default()},"fake",BTreeMap::new(),None,true).unwrap();
        for model in [None,Some("m".into())] {ctx.emit(SpanRecord {span_key:format!("suppressed-{model:?}"),log_type:"chat".into(),model,..Default::default()}).unwrap();}
        let marker=ctx.state_dir().join("last_span_ns");assert!(!marker.exists());assert!(crate::spool::ready(&paths.state).unwrap().is_empty());
        let before=now_ns();ctx.emit(SpanRecord {span_key:"proxy".into(),log_type:"chat".into(),source:"proxy".into(),..Default::default()}).unwrap();let emitted:u64=serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();assert!(emitted>=before&&emitted<=now_ns());ctx.finish().unwrap();
        let files=crate::spool::ready(&paths.state).unwrap();assert_eq!(files.len(),1);let rec:SpanRecord=serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();assert_eq!(rec.source,"proxy");
    }
    #[test] fn scrub_content_tiny_cap_does_not_erase_open_pem_state() {
        let tmp=crate::tests::Temp::new();let mut ctx=HookCtx::new(Paths {state:tmp.path.join("state"),config:tmp.path.join("config")},Config {content_cap_bytes:1,..Default::default()},"fake",BTreeMap::new(),None,false).unwrap();let mut value=Some(Value::String("-----BEGIN PRIVATE KEY-----secret".into()));ctx.scrub_content(&mut value).unwrap();let marker=value.as_ref().unwrap().as_str().unwrap().to_string();assert!(marker.contains("private-key:open"));value=Some(Value::String(format!("{marker}A")));ctx.scrub_content(&mut value).unwrap();assert!(!value.as_ref().unwrap().as_str().unwrap().ends_with('A'));
        ctx.config.content_cap_bytes=0;ctx.scrub_content(&mut value).unwrap();assert!(value.is_none());
    }
}
