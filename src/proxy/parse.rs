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

// Cache breakpoints move as Claude appends turns; they are not new conversation content.
fn same_content(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => left.len() == right.len()
            && left.iter().zip(right).all(|(a, b)| same_content(a, b)),
        (Value::Object(left), Value::Object(right)) => {
            left.keys().filter(|k| k.as_str() != "cache_control").count()
                == right.keys().filter(|k| k.as_str() != "cache_control").count()
                && left.iter().filter(|(k, _)| k.as_str() != "cache_control").all(|(key, value)| {
                    right.get(key).is_some_and(|other| {
                        if key == "content" { same_content(value, other) } else { value == other }
                    })
                })
        }
        _ => left == right,
    }
}

pub fn delta(request: &Value, previous: Option<&Value>) -> Value {
    let mut input = request.clone();
    for key in ["messages", "input", "contents"] {
        if let (Some(now), Some(old)) = (
            request.get(key).and_then(Value::as_array),
            previous.and_then(|v| v.get(key)).and_then(Value::as_array),
        ) {
            let prefix = now.iter().zip(old).take_while(|(a, b)| same_content(a, b)).count();
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
    data: Vec<u8>,
    skip_lf: bool,
    blocks: BTreeMap<u64, (Value, String)>,
    block_bytes: usize,
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
            data: Vec::new(),
            skip_lf: false,
            blocks: BTreeMap::new(),
            block_bytes: 0,
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
            for &byte in bytes {
                if self.skip_lf {
                    self.skip_lf = false;
                    if byte == b'\n' {
                        continue;
                    }
                }
                if matches!(byte, b'\r' | b'\n') {
                    self.line();
                    self.skip_lf = byte == b'\r';
                } else if self.pending.len() + self.data.len() < MAX_CAPTURE {
                    self.pending.push(byte);
                } else {
                    self.disabled = true;
                }
                if self.disabled {
                    self.pending.clear();
                    self.data.clear();
                    break;
                }
            }
        } else if self.raw.len().saturating_add(bytes.len()) <= MAX_CAPTURE {
            self.raw.extend_from_slice(bytes);
        } else {
            self.disabled = true;
            self.raw.clear();
        }
    }
    fn line(&mut self) {
        if self.pending.is_empty() {
            if self.data.last() == Some(&b'\n') {
                self.data.pop();
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&self.data) {
                self.event(&value);
            }
            self.data.clear();
        } else if self.pending == b"data" || self.pending.starts_with(b"data:") {
            let data = self.pending.get(5..).unwrap_or_default();
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if self.data.len() + data.len() + 1 <= MAX_CAPTURE {
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            } else {
                self.disabled = true;
            }
        }
        self.pending.clear();
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
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        if let Some(delta) = value.get("delta") {
            if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                if !fragment.is_empty() {
                    self.first_token();
                }
                if let Some((_, input)) = self.blocks.get_mut(&index) {
                    if self.block_bytes.saturating_add(fragment.len()) <= MAX_CAPTURE {
                        input.push_str(fragment);
                        self.block_bytes += fragment.len();
                    } else {
                        self.disabled = true;
                    }
                }
            } else if let Some(text) = delta.as_str().or_else(|| {
                delta.get("text").or_else(|| delta.get("thinking")).and_then(Value::as_str)
            }) {
                self.append(text);
                if let Some((block, _)) = self.blocks.get_mut(&index) {
                    let key = if delta.get("thinking").is_some() { "thinking" } else { "text" };
                    if self.block_bytes.saturating_add(text.len()) <= MAX_CAPTURE {
                        if let Some(Value::String(content)) = block.get_mut(key) {
                            content.push_str(text);
                            self.block_bytes += text.len();
                        }
                    } else {
                        self.disabled = true;
                    }
                }
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
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    self.append(text);
                }
                let size = block.to_string().len();
                if self.block_bytes.saturating_add(size) <= MAX_CAPTURE {
                    self.block_bytes += size;
                    self.blocks.insert(index, (block.clone(), String::new()));
                } else {
                    self.disabled = true;
                }
            }
        }
        if let Some(output) = value.get("output") {
            // Completed Responses supplies authoritative structured output.
            self.record.output = Some(output.clone());
        }
    }
    fn first_token(&mut self) {
        if !self.record.metadata.contains_key("ttft_ms") {
            self.record.metadata.insert(
                "ttft_ms".into(),
                json!(
                    crate::record::now_ns().saturating_sub(self.record.start_ns) as f64
                        / 1_000_000.0
                ),
            );
        }
    }
    fn append(&mut self, text: &str) {
        if !text.is_empty() {
            self.first_token();
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
            let content = if self.blocks.values().any(|(block, _)| block["type"] == "tool_use") {
                Value::Array(self.blocks.into_values().map(|(mut block, input)| {
                    if !input.is_empty() {
                        if let Ok(input) = serde_json::from_str::<Value>(&input) {
                            block["input"] = input;
                        }
                    }
                    block
                }).collect())
            } else {
                Value::String(self.text)
            };
            self.record.output = Some(json!([{"role":"assistant","content":content}]));
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
    fn anthropic_starts_are_structural_and_tools_assemble_json() {
        let mut p = Parser::new(("r", "fake"), "anthropic", "/messages", 1, &json!({}), json!({}));
        p.event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        assert!(!p.record.metadata.contains_key("ttft_ms"));
        p.event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}));
        assert!(p.record.metadata.contains_key("ttft_ms"));
        assert_eq!(p.finish(200, true).unwrap().output.unwrap()[0]["content"], "answer");

        let mut p = Parser::new(("r", "fake"), "anthropic", "/messages", 1, &json!({}), json!({}));
        p.event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"initial "}}));
        p.event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"text"}}));
        p.event(&json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_mock","name":"echo","input":{}}}));
        p.event(&json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"arg\":"}}));
        p.event(&json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"value\"}"}}));
        assert_eq!(p.finish(200, true).unwrap().output.unwrap()[0]["content"], json!([
            {"type":"text","text":"initial text"},
            {"type":"tool_use","id":"tool_mock","name":"echo","input":{"arg":"value"}}
        ]));

        let mut p = Parser::new(("r", "fake"), "anthropic", "/messages", 1, &json!({}), json!({}));
        p.event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_mock","name":"echo","input":{}}}));
        assert!(!p.record.metadata.contains_key("ttft_ms"));
        p.event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}));
        assert!(p.record.metadata.contains_key("ttft_ms"));
    }
    #[test]
    fn sse_multiline_events_accept_all_line_endings_and_fragmentation() {
        for ending in ["\n", "\r", "\r\n"] {
            for chunk_size in [1, 7, 1024] {
                let mut p = Parser::new(("r", "fake"), "openai", "/chat/completions", 1, &json!({}), json!({}));
                p.sse(true);
                let event = format!("data: {{{ending}data: \"choices\":[{{\"delta\":{{\"content\":\"encoded response\"}}}}],{ending}data: \"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":7,\"prompt_tokens_details\":{{\"cached_tokens\":3}}}}}}{ending}");
                for chunk in event.as_bytes().chunks(chunk_size) {
                    p.bytes(chunk);
                }
                assert!(p.text.is_empty(), "dispatch requires a blank line");
                p.bytes(ending.as_bytes());
                let record = p.finish(200, true).unwrap();
                assert_eq!(record.output.unwrap()[0]["content"], "encoded response");
                assert_eq!(record.usage, Usage {
                    input_tokens: Some(11), output_tokens: Some(7), cache_read_input_tokens: Some(3),
                    ..Default::default()
                });
            }
        }
        let mut p = Parser::new(("r", "fake"), "openai", "/responses", 1, &json!({}), json!({}));
        p.sse(true);
        p.bytes(b"data: ");
        p.bytes(&vec![b'x'; MAX_CAPTURE]);
        let record = p.finish(200, true).unwrap();
        assert_eq!(record.metadata["proxy.capture_limit_exceeded"], true);
    }
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
    fn captured_claude_cache_breakpoint_move_only_emits_new_messages() {
        let old=json!({"system":"same","messages":[{"role":"user","content":[{"type":"text","text":"RUN_TOOL: execute echo hra-e2e then reply.","cache_control":{"type":"ephemeral"}}]}]});
        let new=json!({"system":"same","messages":[{"role":"user","content":[{"type":"text","text":"RUN_TOOL: execute echo hra-e2e then reply."}]},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_mock","name":"Bash","input":{"command":"echo hra-e2e"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_mock","content":"hra-e2e","cache_control":{"type":"ephemeral"}}]}]});
        let actual=delta(&new,Some(&old));
        assert_eq!(actual,json!({"messages":new["messages"].as_array().unwrap()[1..]}));
        let mut changed=new.clone();changed["messages"][0]["content"][0]["text"]=json!("different prompt");
        assert_eq!(delta(&changed,Some(&old))["messages"],changed["messages"]);
        // A similarly named tool argument is real content, not a cache breakpoint.
        assert!(!same_content(&json!({"content":[{"type":"tool_use","input":{"cache_control":1}}]}),
            &json!({"content":[{"type":"tool_use","input":{"cache_control":2}}]})));
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
