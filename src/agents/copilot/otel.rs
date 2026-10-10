use crate::{agents::common::bridge::Event, record::Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    pub session: String,
    pub turn: String,
    id: String,
    pub start: u64,
    end: u64,
    model: Option<String>,
    provider: Option<String>,
    usage: Usage,
    error: Option<String>,
}
impl Request {
    pub fn event(&self, turn_id: &str) -> Event {
        Event::Chat { turn_id: turn_id.into(), id: self.id.clone(), parent_id: None, start_ns: self.start, end_ns: self.end, model: self.model.clone(), provider: self.provider.clone(), input: None, output: None, usage: Some(self.usage.clone()), cost_usd: None, aux_task: None, status: Some(if self.error.is_some() { "error" } else { "ok" }.into()), error: self.error.clone(), metadata: BTreeMap::from([("copilot.turn_id".into(), Value::String(self.turn.clone()))]) }
    }
}
pub(super) fn requests(value: &Value) -> Vec<Request> {
    let mut result = Vec::new();
    collect(value, &mut result);
    result
}
fn collect(value: &Value, result: &mut Vec<Request>) {
    if let Some(array) = value.as_array() { for item in array { collect(item, result); } return; }
    for key in ["resourceSpans", "scopeSpans", "instrumentationLibrarySpans", "spans", "span"] {
        if let Some(nested) = value.get(key) { collect(nested, result); }
    }
    let Some(attributes) = value.get("attributes") else { return; };
    let attr = |name: &str| -> Option<Value> {
        if let Some(map) = attributes.as_object() { return map.get(name).cloned(); }
        attributes.as_array()?.iter().find(|a| a.get("key").and_then(Value::as_str) == Some(name)).and_then(|a| a.get("value")).and_then(|v| {
            for key in ["stringValue", "intValue", "doubleValue", "boolValue"] { if let Some(value) = v.get(key) { return Some(value.clone()); } }
            None
        })
    };
    let string = |name: &str| attr(name).and_then(|v| match v { Value::String(s) => Some(s), Value::Number(n) => Some(n.to_string()), _ => None });
    if string("gen_ai.operation.name").as_deref() != Some("chat") { return; }
    let (Some(session), Some(turn), Some(id)) = (string("gen_ai.conversation.id"), string("github.copilot.turn_id"), string("gen_ai.response.id").or_else(|| value.get("spanId").and_then(Value::as_str).map(str::to_owned)).or_else(|| value.pointer("/spanContext/spanId").and_then(Value::as_str).map(str::to_owned))) else { return; };
    if session.is_empty() || turn.is_empty() || id.is_empty() { return; }
    let Some(start) = value.get("startTimeUnixNano").and_then(number).or_else(|| value.get("startTime").and_then(hrtime)) else { return; };
    let Some(end) = value.get("endTimeUnixNano").and_then(number).or_else(|| value.get("endTime").and_then(hrtime)) else { return; };
    if end < start { return; }
    let usage = |name: &str| attr(name).as_ref().and_then(number);
    let status_error = value.pointer("/status/code").and_then(number) == Some(2);
    result.push(Request { session, turn, id, start, end, model: string("gen_ai.response.model").or_else(|| string("gen_ai.request.model")), provider: string("gen_ai.provider.name"), usage: Usage { input_tokens: usage("gen_ai.usage.input_tokens"), output_tokens: usage("gen_ai.usage.output_tokens"), cache_read_input_tokens: usage("gen_ai.usage.cache_read.input_tokens"), cache_creation_input_tokens: usage("gen_ai.usage.cache_creation.input_tokens"), reasoning_tokens: None }, error: string("error.type").or_else(|| status_error.then(|| value.pointer("/status/message").and_then(Value::as_str).unwrap_or("Copilot request failed").into())) });
}
fn number(value: &Value) -> Option<u64> { value.as_u64().or_else(|| value.as_str()?.parse().ok()) }
fn hrtime(value: &Value) -> Option<u64> {
    let time = value.as_array()?;
    number(time.first()?)?.checked_mul(1_000_000_000)?.checked_add(number(time.get(1)?)?)
}
pub(super) fn hook_time(value: &Value) -> Option<u64> {
    if value.is_number() { return value.as_u64()?.checked_mul(1_000_000); }
    iso_time(value.as_str()?)
}
// ISO timestamps in PascalCase hook payloads; nanoseconds are preserved.
fn iso_time(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let date = date.split('-').map(str::parse::<i64>).collect::<std::result::Result<Vec<_>, _>>().ok()?;
    if date.len() != 3 { return None; }
    let (year, month, day) = (date[0], date[1], date[2]);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) { return None; }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=days[(month - 1) as usize]).contains(&day) { return None; }
    let (clock, offset) = if let Some(clock) = time.strip_suffix('Z') { (clock, 0i64) } else {
        let position = time.char_indices().find(|(_, c)| *c == '+' || *c == '-')?.0;
        let (clock, zone) = time.split_at(position);
        let (hours, minutes) = zone[1..].split_once(':')?;
        let hours = hours.parse::<i64>().ok()?; let minutes = minutes.parse::<i64>().ok()?;
        if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) { return None; }
        (clock, (hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 })
    };
    let components = clock.split(':').collect::<Vec<_>>();
    if components.len() != 3 { return None; }
    let hour = components[0].parse::<i64>().ok()?; let minute = components[1].parse::<i64>().ok()?;
    let (second, fraction) = components[2].split_once('.').unwrap_or((components[2], ""));
    let second = second.parse::<i64>().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) { return None; }
    let nanos = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? * 10u64.pow(9 - fraction.len() as u32) };
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400); let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()?.checked_mul(1_000_000_000)?.checked_add(nanos)
}
