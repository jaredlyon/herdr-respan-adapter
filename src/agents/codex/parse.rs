use crate::{agents::common::{bridge::Event, jsonl::Cursor}, record::Usage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Stream {
    pub cursor: Cursor,
    pub thread: String,
    pub version: Option<String>,
    pub provider: Option<String>,
    turn: String,
    model: Option<String>,
    models: BTreeMap<String, String>,
    start: u64,
    input: Vec<Value>,
    output: Vec<Value>,
    #[serde(default)]
    output_ids: BTreeMap<String, Option<usize>>,
    modern: BTreeSet<String>,
    legacy: BTreeMap<String, Vec<(String, Usage, u64)>>,
    last_total: u64,
    tools: BTreeMap<String, Tool>,
    pub spawned: BTreeMap<String, String>,
    history_floor: Option<u64>,
    pub completed: BTreeSet<String>,
}
#[derive(Serialize, Deserialize)]
struct Tool { turn: String, name: String, input: Option<Value>, start: u64 }

pub(super) fn text(v: &Value, key: &str) -> Option<String> { v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned) }
fn usage(v: &Value) -> Usage {
    Usage { input_tokens: v["input_tokens"].as_u64(), output_tokens: v["output_tokens"].as_u64(),
        cache_read_input_tokens: v["cached_input_tokens"].as_u64(), cache_creation_input_tokens: v["cache_write_input_tokens"].as_u64(), reasoning_tokens: v["reasoning_output_tokens"].as_u64() }
}
fn content(items: &mut Vec<Value>) -> Option<Value> {
    if items.is_empty() { None } else { Some(Value::Array(std::mem::take(items))) }
}
impl Stream {
    pub fn scrub_content(&mut self, ctx: &crate::record::HookCtx) -> crate::Result<()> {
        if ctx.config.content_cap_bytes == 0 { self.input.clear();self.take_output(); }
        for tool in self.tools.values_mut() { ctx.scrub_content(&mut tool.input)?; }
        Ok(())
    }
    fn assistant_output(&mut self,item:&Value,canonical:bool) {
        if let Some(id)=text(item,"id") {
            if let Some(index)=self.output_ids.get(&id) {
                if canonical {if let Some(value_at)=index.and_then(|i|self.output.get_mut(i)) {*value_at=json!({"role":"assistant","content":item["content"]});}}
                return;
            }
            self.output_ids.insert(id,Some(self.output.len()));
        }
        self.output.push(json!({"role":"assistant","content":item["content"]}));
    }
    fn take_output(&mut self)->Option<Value> {
        for index in self.output_ids.values_mut() {*index=None;}
        content(&mut self.output)
    }
    pub fn spawn_parent(&self, child: &str) -> Option<String> {
        self.spawned.get(child).cloned().or_else(|| self.tools.iter().filter(|(_, t)| t.name == "spawn_agent").max_by_key(|(_, t)| t.start).map(|(id, _)| id.clone()))
    }
    pub fn parse(&mut self, lines: &[String], child: Option<(&str, &str)>, now: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for line in lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue; };
            let p = &v["payload"];
            let ts = v["timestamp"].as_str().and_then(timestamp).unwrap_or(now);
            let kind = v["type"].as_str().unwrap_or("");
            if kind == "session_meta" {
                self.thread = text(p, "id").unwrap_or_default();
                self.version = text(p, "cli_version"); self.provider = text(p, "model_provider");
                self.history_floor = p["subagent_history_start_ordinal"].as_u64().or_else(|| p["forked_from_ordinal_exclusive"].as_u64().map(|n| n.saturating_add(1)));
                continue;
            }
            if self.history_floor.is_some_and(|floor| v["ordinal"].as_u64().is_some_and(|n| n < floor)) { continue; }
            if kind == "turn_context" {
                self.turn = text(p, "turn_id").unwrap_or_else(|| self.turn.clone());
                self.model = text(p, "model");
                if let Some(model) = &self.model { self.models.insert(self.turn.clone(), model.clone()); }
                continue;
            }
            let native_turn = text(p, "turn_id").unwrap_or_else(|| self.turn.clone());
            let turn = child.map_or_else(|| native_turn.clone(), |(_, turn)| turn.to_owned());
            let parent = child.map(|(id, _)| id.to_owned());
            if turn.is_empty() { continue; }
            if kind == "token_usage_record" {
                // Child histories can include inherited parent records even without ordinals.
                if text(p, "thread_id").is_some_and(|id| id != self.thread) { continue; }
                let Some(id) = text(p, "response_id") else { continue; };
                self.modern.insert(native_turn.clone()); self.legacy.remove(&native_turn);
                events.push(Event::Chat { metadata: BTreeMap::from([("codex.usage_source".into(), json!("token_usage_record")), ("codex.timing".into(), json!("rollout-boundaries"))]), turn_id: turn, id, parent_id: parent, start_ns: if self.start == 0 { ts } else { self.start.min(ts) }, end_ns: ts,
                    model: self.models.get(&native_turn).cloned().or_else(|| self.model.clone()), provider: self.provider.clone(), input: content(&mut self.input), output: self.take_output(),
                    usage: Some(usage(&p["usage"])), cost_usd: None, aux_task: None, status: Some("ok".into()), error: None });
                self.start = ts;
            } else if kind == "response_item" {
                match p["type"].as_str().unwrap_or("") {
                    "message" => {
                        let role = p["role"].as_str().unwrap_or("");
                        if role == "assistant" { self.assistant_output(p,true); } else { self.input.push(json!({"role":role,"content":p["content"]})); }
                    }
                    "function_call" | "custom_tool_call" | "local_shell_call" => {
                        if let Some(id) = text(p, "call_id") {
                            let name = text(p, "name").unwrap_or_else(|| "local_shell".into());
                            let input = p.get("arguments").or_else(|| p.get("input")).or_else(|| p.get("action")).cloned();
                            self.tools.insert(id, Tool { turn, name, input, start: ts });
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        if let Some(id) = text(p, "call_id") {
                            if let Some(tool) = self.tools.remove(&id) {
                                let output = p.get("output").cloned();
                                if tool.name == "spawn_agent" {
                                    let parsed = output.as_ref().and_then(|v| v.as_str()).and_then(|s| serde_json::from_str::<Value>(s).ok()).or_else(|| output.clone());
                                    if let Some(child) = parsed.as_ref().and_then(|p| text(p, "agent_id")) { self.spawned.insert(child, id.clone()); }
                                }
                                events.push(Event::Tool { metadata: BTreeMap::new(), turn_id: tool.turn, id, parent_id: parent, name: Some(tool.name), start_ns: tool.start.min(ts), end_ns: ts, input: tool.input, output, status: Some("ok".into()), error: None });
                            }
                        }
                    }
                    "web_search_call" => {
                        if let Some(id) = text(p, "id") {
                            events.push(Event::Tool { metadata: BTreeMap::new(), turn_id: turn, id, parent_id: parent, name: Some("web_search".into()), start_ns: ts, end_ns: ts, input: p.get("action").cloned(), output: None, status: Some("ok".into()), error: None });
                        }
                    }
                    _ => {}
                }
            } else if kind == "event_msg" {
                match p["type"].as_str().unwrap_or("") {
                    "task_started" | "turn_started" => {
                        self.input.clear(); self.output.clear(); self.output_ids.clear();
                        self.turn = native_turn; self.start = ts; self.model = self.models.get(&self.turn).cloned();
                        if child.is_none() { events.push(Event::TurnStart { metadata: BTreeMap::new(), turn_id: turn, ts_ns: ts, prompt: None }); }
                    }
                    "user_message" => {
                        self.input.push(json!({"role":"user","content":p["message"]}));
                        if let Some(Event::TurnStart { prompt, .. }) = events.iter_mut().find(|e| matches!(e, Event::TurnStart { turn_id, .. } if turn_id == &turn)) { *prompt = p.get("message").cloned(); }
                    }
                    "token_count" => {
                        let info = &p["info"]; let last = usage(&info["last_token_usage"]);
                        let total = info["total_token_usage"]["total_tokens"].as_u64().unwrap_or(0);
                        if last.input_tokens.unwrap_or(0).saturating_add(last.output_tokens.unwrap_or(0)) > 0 {
                            if total > self.last_total && !self.modern.contains(&native_turn) {
                                self.legacy.entry(native_turn).or_default().push((format!("legacy-token-count-{total}"), last, ts));
                            }
                            self.last_total = self.last_total.max(total);
                        }
                    }
                    "task_complete" | "turn_complete" | "turn_aborted" => {
                        self.completed.insert(native_turn.clone());
                        self.finish_legacy(&native_turn, &turn, parent.as_deref(), &mut events);
                        if child.is_none() { events.push(Event::TurnEnd { turn_id: turn, ts_ns: ts, output: p.get("last_agent_message").cloned(), status: Some(if p["type"] == "turn_aborted" || p.get("error").is_some_and(|v| !v.is_null()) { "error" } else { "ok" }.into()), error: text(p, "error") }); }
                    }
                    "item_completed" => {
                        let item = &p["item"]; let kind = item["type"].as_str().unwrap_or("");
                        if ["CommandExecution", "McpToolCall", "DynamicToolCall", "FileChange", "CollabAgentToolCall", "WebSearch"].contains(&kind) {
                            if let Some(id) = text(item, "id") {
                                let failed = matches!(item["status"].as_str(), Some("failed" | "declined"));
                                let end = p["completed_at_ms"].as_u64().filter(|n| *n > 0).and_then(|n| n.checked_mul(1_000_000)).unwrap_or(ts);
                                let start = p["started_at_ms"].as_u64().filter(|n| *n > 0).and_then(|n| n.checked_mul(1_000_000)).unwrap_or(end).min(end);
                                // Code-mode: the model's `exec` custom tool runs a script whose inner tool calls arrive as items
                                // while that call is still open. Nest under it when exactly one is open; never guess between several.
                                let mut open = self.tools.iter().filter(|(_, t)| t.turn == turn && t.name == "exec").map(|(id, _)| id.clone());
                                let metadata = match (open.next(), open.next()) { (Some(parent), None) => BTreeMap::from([("parent_tool_id".into(), json!(parent))]), _ => BTreeMap::new() };
                                events.push(Event::Tool { metadata, turn_id: turn, id, parent_id: parent, name: text(item, "tool").or_else(|| Some(kind.into())), start_ns: start, end_ns: end, input: item.get("arguments").or_else(|| item.get("command")).or_else(|| item.get("changes")).cloned(), output: item.get("aggregated_output").or_else(|| item.get("result")).or_else(|| item.get("content")).cloned(), status: Some(if failed { "error" } else { "ok" }.into()), error: text(item, "error") });
                            }
                        } else if kind == "AgentMessage" { self.assistant_output(item,false); }
                    }
                    _ => {}
                }
            }
        }
        events
    }
    pub fn finish_legacy(&mut self, native: &str, turn: &str, parent: Option<&str>, events: &mut Vec<Event>) {
        if self.modern.contains(native) { self.legacy.remove(native); return; }
        for (id, usage, ts) in self.legacy.remove(native).unwrap_or_default() {
            events.push(Event::Chat { metadata: BTreeMap::from([("codex.usage_source".into(), json!("legacy-token-count")), ("codex.timing".into(), json!("rollout-boundaries"))]), turn_id: turn.into(), id, parent_id: parent.map(str::to_owned), start_ns: if self.start == 0 { ts } else { self.start.min(ts) }, end_ns: ts,
                model: self.models.get(native).cloned().or_else(|| self.model.clone()), provider: self.provider.clone(), input: content(&mut self.input), output: self.take_output(), usage: Some(usage), cost_usd: None, aux_task: None, status: Some("ok".into()), error: None });
            self.start = ts;
        }
    }
}

// Codex recorder timestamps are UTC RFC3339. No date/time dependency needed.
fn timestamp(s: &str) -> Option<u64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let d: Vec<i64> = date.split('-').map(str::parse).collect::<std::result::Result<_, _>>().ok()?;
    let t: Vec<&str> = time.split(':').collect();
    if d.len() != 3 || t.len() != 3 || !(1..=12).contains(&d[1]) || !(1..=31).contains(&d[2]) { return None; }
    let hour: u64 = t[0].parse().ok()?; let minute: u64 = t[1].parse().ok()?;
    let (second, fraction) = t[2].split_once('.').unwrap_or((t[2], "")); let second: u64 = second.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 || fraction.len() > 9 || !fraction.bytes().all(|c| c.is_ascii_digit()) { return None; }
    let year = d[0] - i64::from(d[1] <= 2); let era = year.div_euclid(400); let y = year - era * 400;
    let month = d[1] + if d[1] > 2 { -3 } else { 9 };
    let days = era * 146097 + y * 365 + y / 4 - y / 100 + (153 * month + 2) / 5 + d[2] - 1 - 719468;
    let seconds = u64::try_from(days).ok()?.checked_mul(86400)?.checked_add(hour * 3600 + minute * 60 + second)?;
    let nanos = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? * 10u64.pow((9 - fraction.len()) as u32) };
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}
