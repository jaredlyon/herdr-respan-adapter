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
}
impl HookCtx {
    pub fn new(paths: Paths, config: Config, agent: &str, metadata: BTreeMap<String, Value>, run_id: Option<String>, proxy_active: bool) -> Result<Self> {
        if agent.is_empty() || !agent.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { return Err("invalid agent kind".into()); }
        let customer_identifier = if config.customer_identifier.is_empty() { crate::meta::hostname() } else { config.customer_identifier.clone() };
        let redactor = Redactor::new(&config.redact_patterns)?;
        let state_dir = paths.state.join("agents").join(agent); std::fs::create_dir_all(&state_dir)?;
        Ok(Self { config, agent: agent.into(), metadata, run_id, customer_identifier, state_dir, paths, redactor, writer: None, proxy_active })
    }
    pub fn state_dir(&self) -> &Path { &self.state_dir }
    pub fn now_ns(&self) -> u64 { now_ns() }
    /// Append an accepted record and stamp last_span_ns with the current UTC emission time.
    pub fn emit(&mut self, mut rec: SpanRecord) -> Result<()> {
        if rec.v != 1 || rec.span_key.is_empty() || rec.end_ns < rec.start_ns || !["agent", "chat", "tool", "task", "workflow"].contains(&rec.log_type.as_str()) || !["ok", "error"].contains(&rec.status.as_str()) { return Err("invalid span record".into()); }
        if (rec.log_type == "tool" && !self.config.tool_spans)
            || (rec.metadata.get("subagent").is_some_and(truthy) && !self.config.subagent_spans)
            || (rec.metadata.contains_key("hermes.aux_task") && !self.config.aux_spans)
            || (rec.log_type == "chat" && self.proxy_active && rec.source == "adapter") { return Ok(()); }
        rec.agent = self.agent.clone();
        if rec.run_id.is_none() { rec.run_id = self.run_id.clone(); }
        for (key, value) in &self.metadata { rec.metadata.entry(key.clone()).or_insert_with(|| value.clone()); }
        // Customer attribution stays within the record's declared metadata field.
        rec.metadata.insert("customer_identifier".into(), Value::String(self.customer_identifier.clone()));
        self.scrub_content(&mut rec.input)?; self.scrub_content(&mut rec.output)?;
        let mut value = serde_json::to_value(&rec)?;
        // Scrub all strings, including adapter metadata and error messages, before disk.
        self.redactor.value(&mut value, self.config.content_cap_bytes.max(16384));
        rec = serde_json::from_value(value)?;
        if self.writer.is_none() { self.writer = Some(Writer::new(&self.paths.state, self.config.spool_max_bytes)?); }
        self.writer.as_mut().expect("writer initialized").append(&rec)?;
        crate::paths::atomic_json(&self.state_dir.join("last_span_ns"),&self.now_ns())
    }
    pub(crate) fn scrub_value(&self, value: &mut Value) { self.redactor.value(value, self.config.content_cap_bytes.max(16384)); }
    pub(crate) fn scrub_content(&self, field: &mut Option<Value>) -> Result<()> {
        if self.config.content_cap_bytes == 0 { *field = None; }
        else if let Some(value) = field {
            self.redactor.value(value, self.config.content_cap_bytes);
            let text = if let Value::String(s) = value { s.clone() } else { serde_json::to_string(value)? };
            if text.len() > self.config.content_cap_bytes { *value = Value::String(crate::redact::cap(&text, self.config.content_cap_bytes)); }
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
    #[test] fn proxy_suppresses_all_adapter_chats_and_tracks_only_emitted_activity() {
        let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let mut ctx=HookCtx::new(paths.clone(),Config {customer_identifier:"test".into(),..Default::default()},"fake",BTreeMap::new(),None,true).unwrap();
        for model in [None,Some("m".into())] {ctx.emit(SpanRecord {span_key:format!("suppressed-{model:?}"),log_type:"chat".into(),model,..Default::default()}).unwrap();}
        let marker=ctx.state_dir().join("last_span_ns");assert!(!marker.exists());assert!(crate::spool::ready(&paths.state).unwrap().is_empty());
        let before=now_ns();ctx.emit(SpanRecord {span_key:"proxy".into(),log_type:"chat".into(),source:"proxy".into(),..Default::default()}).unwrap();let emitted:u64=serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();assert!(emitted>=before&&emitted<=now_ns());ctx.finish().unwrap();
        let files=crate::spool::ready(&paths.state).unwrap();assert_eq!(files.len(),1);let rec:SpanRecord=serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();assert_eq!(rec.source,"proxy");
    }
}
