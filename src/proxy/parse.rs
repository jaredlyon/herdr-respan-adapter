//! Bounded, observation-only parsers. Forwarding never depends on successful parsing.
use crate::record::SpanRecord;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const MAX_CAPTURE: usize = 8 * 1024 * 1024;

pub fn inference(protocol: &str, path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    match protocol {
        "anthropic" => path.ends_with("/messages"),
        "openai" => path.ends_with("/responses") || path.ends_with("/chat/completions"),
        "gemini" => path.contains("generateContent") || path.contains("streamGenerateContent"),
        _ => false,
    }
}

pub fn delta(request: &Value, previous: Option<&Value>) -> Value {
    let mut input = request.clone();
    for key in ["messages", "input", "contents"] {
        if let (Some(now), Some(old)) = (
            request.get(key).and_then(Value::as_array),
            previous.and_then(|v| v.get(key)).and_then(Value::as_array),
        ) {
            let prefix = now.iter().zip(old).take_while(|(a, b)| a == b).count();
            if prefix == old.len() {
                input[key] = json!(&now[prefix..]);
            }
        }
    }
    if let (Some(now), Some(old)) = (
        request.get("request"),
        previous.and_then(|v| v.get("request")),
    ) {
        input["request"] = delta(now, Some(old));
    }
    if let Some(previous) = previous {
        for key in ["system", "systemInstruction", "instructions"] {
            if request.get(key).is_some() && request.get(key) == previous.get(key) {
                input.as_object_mut().map(|v| v.remove(key));
            }
        }
    }
    input
}

pub struct Parser {
    pub record: SpanRecord,
    pub warmup: bool,
    pending: Vec<u8>,
    raw: Vec<u8>,
    text: String,
    sse: bool,
    disabled: bool,
}
impl Parser {
    pub fn new(
        identity: (&str, &str),
        protocol: &str,
        path: &str,
        start: u64,
        request: &Value,
        input: Value,
    ) -> Self {
        let (run, agent) = identity;
        let model = request
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                path.split("models/")
                    .nth(1)
                    .and_then(|p| p.split(':').next())
                    .map(str::to_owned)
            });
        Self {
            record: SpanRecord {
                span_key: format!(
                    "proxy:{run}:{}",
                    crate::record::random_id().unwrap_or_else(|_| start.to_string())
                ),
                run_id: Some(run.into()),
                name: "model request".into(),
                log_type: "chat".into(),
                start_ns: start,
                end_ns: start,
                model,
                provider: Some(protocol.into()),
                input: Some(input),
                source: "proxy".into(),
                agent: agent.into(),
                metadata: BTreeMap::new(),
                ..Default::default()
            },
            warmup: request.get("generate") == Some(&Value::Bool(false)),
            pending: Vec::new(),
            raw: Vec::new(),
            text: String::new(),
            sse: false,
            disabled: false,
        }
    }
    pub fn sse(&mut self, yes: bool) {
        self.sse = yes;
    }
    pub fn bytes(&mut self, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        if self.sse {
            if self.pending.len().saturating_add(bytes.len()) > MAX_CAPTURE {
                self.disabled = true;
                self.pending.clear();
                return;
            }
            self.pending.extend_from_slice(bytes);
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                if let Some(data) = line.trim_end().strip_prefix("data:") {
                    if let Ok(value) = serde_json::from_str::<Value>(data.trim_start()) {
                        self.event(&value);
                    }
                }
            }
        } else if self.raw.len().saturating_add(bytes.len()) <= MAX_CAPTURE {
            self.raw.extend_from_slice(bytes);
        } else {
            self.disabled = true;
            self.raw.clear();
        }
    }
    pub fn event(&mut self, value: &Value) {
        let value = value.get("response").unwrap_or(value); // Responses final envelope and Code Assist.
        if let Some(model) = value
            .get("model")
            .or_else(|| value.get("modelVersion"))
            .and_then(Value::as_str)
        {
            self.record.model = Some(model.into());
        }
        if let Some(message) = value.get("message") {
            self.event(message);
        }
        if let Some(usage) = value.get("usage") {
            self.usage(usage);
        }
        if let Some(usage) = value.get("usageMetadata") {
            merge(
                &mut self.record.usage.input_tokens,
                usage.get("promptTokenCount"),
            );
            merge(
                &mut self.record.usage.output_tokens,
                usage.get("candidatesTokenCount"),
            );
            merge(
                &mut self.record.usage.cache_read_input_tokens,
                usage.get("cachedContentTokenCount"),
            );
            merge(
                &mut self.record.usage.reasoning_tokens,
                usage.get("thoughtsTokenCount"),
            );
        }
        if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
            self.record.status = "error".into();
            self.record.status_message = Some(error.to_string());
        }
        if let Some(delta) = value.get("delta") {
            if let Some(text) = delta.as_str().or_else(|| {
                delta
                    .get("text")
                    .or_else(|| delta.get("thinking"))
                    .or_else(|| delta.get("partial_json"))
                    .and_then(Value::as_str)
            }) {
                self.append(text);
            }
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(text) = choice
                    .pointer("/delta/content")
                    .or_else(|| choice.pointer("/message/content"))
                    .and_then(Value::as_str)
                {
                    self.append(text);
                }
                if let Some(calls) = choice
                    .pointer("/delta/tool_calls")
                    .or_else(|| choice.pointer("/message/tool_calls"))
                {
                    self.append(&calls.to_string());
                }
            }
        }
        if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                if let Some(parts) = candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            self.append(text);
                        } else {
                            self.append(&part.to_string());
                        }
                    }
                }
            }
        }
        if let Some(content) = value.get("content").and_then(Value::as_array) {
            for part in content {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    self.append(text);
                } else {
                    self.append(&part.to_string());
                }
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("content_block_start") {
            if let Some(block) = value.get("content_block") {
                self.append(&block.to_string());
            }
        }
        if let Some(output) = value.get("output") {
            // Completed Responses supplies authoritative structured output.
            self.record.output = Some(output.clone());
        }
    }
    fn append(&mut self, text: &str) {
        if self.text.is_empty() && !text.is_empty() {
            self.record.metadata.insert(
                "ttft_ms".into(),
                json!(
                    crate::record::now_ns().saturating_sub(self.record.start_ns) as f64
                        / 1_000_000.0
                ),
            );
        }
        let remaining = MAX_CAPTURE.saturating_sub(self.text.len());
        let mut end = remaining.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        if end < text.len() {
            self.record
                .metadata
                .insert("proxy.capture_limit_exceeded".into(), json!(true));
        }
    }
    fn usage(&mut self, usage: &Value) {
        merge(
            &mut self.record.usage.input_tokens,
            usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens")),
        );
        merge(
            &mut self.record.usage.output_tokens,
            usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens")),
        );
        merge(
            &mut self.record.usage.cache_read_input_tokens,
            usage
                .get("cache_read_input_tokens")
                .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
                .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens")),
        );
        merge(
            &mut self.record.usage.cache_creation_input_tokens,
            usage
                .get("cache_creation_input_tokens")
                .or_else(|| usage.pointer("/input_tokens_details/cache_write_tokens")),
        );
        merge(
            &mut self.record.usage.reasoning_tokens,
            usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens")),
        );
    }
    pub fn finish(mut self, status: u16, complete: bool) -> Option<SpanRecord> {
        if !self.sse {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.raw) {
                self.event(&value);
            }
        }
        if self.warmup {
            return None;
        }
        self.record.end_ns = crate::record::now_ns().max(self.record.start_ns);
        self.record
            .metadata
            .insert("http.status_code".into(), json!(status));
        self.record.metadata.insert(
            "latency_ms".into(),
            json!((self.record.end_ns - self.record.start_ns) as f64 / 1_000_000.0),
        );
        if !complete || status >= 400 {
            self.record.status = "error".into();
        }
        if !complete {
            self.record.status_message = Some("transport interrupted".into());
        }
        if self.disabled {
            self.record
                .metadata
                .insert("proxy.capture_limit_exceeded".into(), json!(true));
        }
        if self.record.output.is_none() {
            self.record.output = Some(json!([{"role":"assistant","content":self.text}]));
        }
        Some(self.record)
    }
}
fn merge(target: &mut Option<u64>, value: Option<&Value>) {
    if let Some(value) = value.and_then(Value::as_u64) {
        *target = Some(value);
    }
}

/// RFC 6455 frame tee: original bytes are relayed, including masking/control frames.
/// Observation stops for oversized/invalid frames; the transport keeps going.
#[derive(Default)]
pub struct Frames {
    pending: Vec<u8>,
    message: Vec<u8>,
    opcode: u8,
    disabled: bool,
}
impl Frames {
    pub fn bytes(&mut self, bytes: &[u8]) -> Vec<Value> {
        let mut values = Vec::new();
        if self.disabled {
            return values;
        }
        if self.pending.len().saturating_add(bytes.len()) > MAX_CAPTURE + 14 {
            self.disabled = true;
            self.pending.clear();
            return values;
        }
        self.pending.extend_from_slice(bytes);
        loop {
            if self.pending.len() < 2 {
                break;
            }
            let first = self.pending[0];
            let masked = self.pending[1] & 128 != 0;
            let mut header = 2;
            let short = self.pending[1] & 127;
            let length = match short {
                126 => {
                    if self.pending.len() < 4 {
                        break;
                    }
                    header = 4;
                    u16::from_be_bytes([self.pending[2], self.pending[3]]) as u64
                }
                127 => {
                    if self.pending.len() < 10 {
                        break;
                    }
                    header = 10;
                    u64::from_be_bytes(self.pending[2..10].try_into().expect("eight bytes"))
                }
                n => n as u64,
            };
            if length > MAX_CAPTURE as u64 || first & 0x70 != 0 {
                self.disabled = true;
                self.pending.clear();
                break;
            }
            let mask_start = header;
            if masked {
                header += 4;
            }
            let end = header + length as usize;
            if self.pending.len() < end {
                break;
            }
            let opcode = first & 15;
            if opcode < 8 {
                if opcode != 0 {
                    self.opcode = opcode;
                    self.message.clear();
                }
                if self.message.len() + length as usize > MAX_CAPTURE {
                    self.disabled = true;
                    self.pending.clear();
                    break;
                }
                for i in header..end {
                    self.message.push(
                        self.pending[i]
                            ^ if masked {
                                self.pending[mask_start + (i - header) % 4]
                            } else {
                                0
                            },
                    );
                }
                if first & 128 != 0 {
                    if self.opcode == 1 {
                        if let Ok(value) = serde_json::from_slice(&self.message) {
                            values.push(value);
                        }
                    }
                    self.message.clear();
                }
            }
            self.pending.drain(..end);
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Usage;
    #[test]
    fn protocol_usage_and_delta() {
        let cases = [
            (
                "anthropic",
                json!({"type":"message_start","message":{"model":"a","usage":{"input_tokens":10,"cache_read_input_tokens":3}}}),
                json!({"type":"message_delta","usage":{"output_tokens":4}}),
            ),
            (
                "openai",
                json!({"choices":[{"delta":{"content":"x"}}]}),
                json!({"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3}}}),
            ),
            (
                "openai",
                json!({"type":"response.output_text.delta","delta":"x"}),
                json!({"type":"response.completed","response":{"model":"a","usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3}},"output":[]}}),
            ),
            (
                "gemini",
                json!({"response":{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}}),
                json!({"response":{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"cachedContentTokenCount":3}}}),
            ),
        ];
        for (protocol, first, last) in cases {
            let mut p = Parser::new(
                ("r", "fake"),
                protocol,
                "/models/a:streamGenerateContent",
                1,
                &json!({}),
                json!({}),
            );
            p.sse(true);
            let bytes = format!("data: {first}\r\n\r\ndata: {last}\n\ndata: [DONE]\n\n");
            for byte in bytes.as_bytes() {
                p.bytes(&[*byte]);
            }
            let record = p.finish(200, true).unwrap();
            assert_eq!(
                record.usage,
                Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(4),
                    cache_read_input_tokens: Some(3),
                    ..Default::default()
                }
            );
        }
        let old = json!({"system":"same","messages":[{"role":"user","content":"one"}]});
        let new = json!({"system":"same","messages":[{"role":"user","content":"one"},{"role":"user","content":"two"}]});
        assert_eq!(
            delta(&new, Some(&old)),
            json!({"messages":[{"role":"user","content":"two"}]})
        );
        assert_eq!(delta(&old, None), old);
        let mut p = Parser::new(
            ("r", "fake"),
            "openai",
            "/responses",
            1,
            &json!({"generate":false}),
            json!({}),
        );
        p.event(&json!({"usage":{"input_tokens":1}}));
        assert!(p.finish(200, true).is_none());
    }
    #[test]
    fn frames_survive_fragmentation_and_masking() {
        let data = br#"{"type":"response.create"}"#;
        let mask = [1, 2, 3, 4];
        let mut frame = vec![0x81, 128 | data.len() as u8];
        frame.extend(mask);
        frame.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        let mut f = Frames::default();
        let values: Vec<_> = frame.chunks(3).flat_map(|b| f.bytes(b)).collect();
        assert_eq!(values, vec![json!({"type":"response.create"})]);
    }
}
