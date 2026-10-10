use crate::Result;
use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::LazyLock;

pub struct Redactor { custom: Vec<Regex> }
static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| [
        (r"sk-ant-[A-Za-z0-9_-]{20,}", "anthropic"),
        (r"sk-[A-Za-z0-9_-]{20,}", "openai"),
        (r"gh[pousr]_[A-Za-z0-9]{30,}", "github"),
        (r"github_pat_[A-Za-z0-9_]{30,}", "github"),
        (r"(?:AKIA|ASIA)[0-9A-Z]{16}", "aws"),
        (r"xox[abposr]-[A-Za-z0-9-]{10,}", "slack"),
        (r"(?i:bearer)\s+[A-Za-z0-9._~+/=-]{20,}", "bearer"),
    ].into_iter().map(|(p, k)| (Regex::new(p).expect("constant regex"), k)).collect());
const KEY_PATTERN: &str = r"(?ix:password|passwd|secret|token|authorization|proxy[-_]?authorization|
    (?:[a-z0-9]+[-_])*(?:api[-_]?key|access[-_]?token|refresh[-_]?token|id[-_]?token|
    auth[-_]?token|bearer[-_]?token|client[-_]?secret|consumer[-_]?(?:key|secret)|
    private[-_]?key|secret[-_]?key|secret[-_]?access[-_]?key|access[-_]?key(?:[-_]?id)?|
    signing[-_]?key))";
static SENSITIVE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^{KEY_PATTERN}$")).expect("constant regex"));
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    // Match credential keys only: a harmless enclosing JSON field must not hide
    // assignments inside its quoted content from the same redaction pass.
    let prefix = format!(r#"(?x)(?:^|[^A-Za-z0-9_-])
        (?:"(?P<double>{KEY_PATTERN})"|'(?P<single>{KEY_PATTERN})'|(?P<bare>{KEY_PATTERN}))
        \s*[:=]\s*"#);
    Regex::new(&(prefix + r#"(?P<value>
        (?P<closed>"(?:\\(?s:.)|[^"\\])*"|'(?:\\(?s:.)|[^'\\])*')|
        (?P<open>"(?:\\(?s:.)|[^"\\])+\z|'(?:\\(?s:.)|[^'\\])+\z)|
        \[REDACTED:[^\]\r\n]*\]|
        (?i:Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+|[^\s,;}\]"']+)"#)).expect("constant regex")
});
static REDACTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[REDACTED:[^\]\r\n]*\]").expect("constant regex"));
static PEM_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(
    r"-----BEGIN (?P<begin>[A-Z ]*PRIVATE KEY)-----|\[REDACTED:private-key:open:(?P<open>[A-Z ]*PRIVATE KEY):(?P<pending>[0-9]+)\]"
).expect("constant regex"));
impl Redactor {
    pub fn new(custom: &[String]) -> Result<Self> { Ok(Self { custom: custom.iter().map(|s| Regex::new(s)).collect::<std::result::Result<_, _>>()? }) }
    /// Open PEMs retain only their public label and partial END delimiter length.
    /// The marker survives serialization, so a fresh redactor suppresses appended
    /// chunks too; state is per string, never shared between unrelated fields.
    pub fn text(&self, input: &str) -> String {
        let mut text = String::with_capacity(input.len());
        let mut rest = input;
        let mut closed_pem = false;
        while let Some(captures) = PEM_START.captures(rest) {
            let start = captures.get(0).expect("PEM match");
            text.push_str(&self.plain(&rest[..start.start()]));
            let label = captures.name("begin").or_else(|| captures.name("open")).expect("PEM label").as_str();
            let end = format!("-----END {label}-----");
            let pending = captures.name("pending").and_then(|m| m.as_str().parse::<usize>().ok()).unwrap_or(0).min(end.len() - 1);
            let tail = &rest[start.end()..];
            // Only the non-secret END prefix is reconstructed, never key bytes.
            let body = format!("{}{tail}", &end[..pending]);
            if let Some(position) = body.find(&end) {
                text.push_str("[REDACTED:private-key]");
                closed_pem = true;
                rest = &tail[position + end.len() - pending..];
            } else {
                let pending = (0..end.len()).rev().find(|&n| body.ends_with(&end[..n])).unwrap_or(0);
                if closed_pem { text = self.plain(&text); }
                let (label,pending)=if label.len()<=64 {(label,pending)} else {("PRIVATE KEY",0)};
                text.push_str(&format!("[REDACTED:private-key:open:{label}:{pending}]"));
                return text;
            }
        }
        text.push_str(&self.plain(rest));
        // A complete PEM may split a quoted credential assignment across pieces.
        if closed_pem { self.plain(&text) } else { text }
    }
    fn plain(&self, input: &str) -> String {
        let mut text = ASSIGNMENT.replace_all(input, |captures: &Captures<'_>| {
            let value = captures.name("value").expect("assignment value");
            let raw = value.as_str();
            let quote = raw.chars().next().filter(|c| *c == '"' || *c == '\'');
            let closed = captures.name("closed").is_some();
            let inner = quote.map(|q| {
                let inner = raw.strip_prefix(q).expect("opening quote");
                if closed { inner.strip_suffix(q).expect("closing quote") } else { inner }
            }).unwrap_or(raw);
            if REDACTED.find(inner).is_some_and(|m| m.start() == 0 && m.end() == inner.len()) {
                return captures[0].to_string();
            }
            let prefix = &captures[0][..value.start() - captures.get(0).expect("assignment match").start()];
            match quote {
                Some(quote) if closed => format!("{prefix}{quote}[REDACTED:secret]{quote}"),
                Some(quote) => format!("{prefix}{quote}[REDACTED:secret]"),
                None => format!("{prefix}[REDACTED:secret]"),
            }
        }).into_owned();
        for (regex, kind) in PATTERNS.iter() {
            text = regex.replace_all(&text, format!("[REDACTED:{kind}]").as_str()).into_owned();
        }
        for regex in &self.custom {
            // Custom patterns append to built-ins without rewriting their markers.
            let mut redacted = String::with_capacity(text.len());
            let mut start = 0;
            for marker in REDACTED.find_iter(&text) {
                redacted.push_str(&regex.replace_all(&text[start..marker.start()], "[REDACTED:custom]"));
                redacted.push_str(marker.as_str());
                start = marker.end();
            }
            redacted.push_str(&regex.replace_all(&text[start..], "[REDACTED:custom]"));
            text = redacted;
        }
        text
    }
    pub fn value(&self, value: &mut Value, bytes: usize) {
        match value {
            Value::String(text) => {
                let redacted = self.text(text);
                *text=cap_content(&redacted,bytes);
            }
            Value::Array(values) => values.iter_mut().for_each(|v| self.value(v, bytes)),
            Value::Object(values) => {
                let old = std::mem::take(values);
                for (key, mut value) in old {
                    if sensitive_key(&key) { value = Value::String("[REDACTED:secret]".into()); }
                    else { self.value(&mut value, bytes); }
                    values.insert(self.text(&key), value);
                }
            }
            _ => {},
        }
    }
}
fn sensitive_key(key: &str) -> bool { SENSITIVE_KEY.is_match(key) }
/// System-generated open-PEM state is exempt from content caps, including tiny caps.
/// It contains only a bounded public label and END-prefix length, never captured key bytes.
pub(crate) fn cap_content(text:&str,bytes:usize)->String {
    if let Some(marker)=PEM_START.captures(text).filter(|c|c.name("open").is_some()).and_then(|c|c.get(0)) {
        if text.len()>bytes {
            let prefix=if bytes>=marker.len() {cap(&text[..marker.start()],bytes-marker.len())} else {String::new()};
            return format!("{prefix}{}",marker.as_str());
        }
    }
    cap(text,bytes)
}
pub fn cap(text: &str, bytes: usize) -> String {
    if bytes == 0 { return String::new(); }
    if text.len() <= bytes { return text.into(); }
    // Keep the marker within the cap, including for multibyte text and tiny caps.
    let mut end = bytes.min(text.len());
    loop {
        while !text.is_char_boundary(end) { end -= 1; }
        let marker = format!("…[truncated {} bytes]", text.len() - end);
        if end + marker.len() <= bytes { return format!("{}{marker}", &text[..end]); }
        if end == 0 { return marker.chars().scan(0, |n, c| { *n += c.len_utf8(); Some((*n, c)) }).take_while(|(n, _)| *n <= bytes).map(|(_, c)| c).collect(); }
        end -= 1;
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn bearer_case_and_temporary_aws_keys() {
        let r=Redactor::new(&[]).unwrap();
        for input in ["bearer abcdefghijklmnopqrstuvwxyz", "BEARER abcdefghijklmnopqrstuvwxyz", "ASIA1234567890ABCDEF"] {
            assert!(r.text(input).starts_with("[REDACTED:"));
        }
    }
    #[test] fn secrets_and_false_positives() {
        let r = Redactor::new(&["PRIVATEWORD".into()]).unwrap();
        for secret in ["sk-ant-abcdefghijklmnopqrstuvwxyz", "sk-abcdefghijklmnopqrstuvwxyz", "ghp_abcdefghijklmnopqrstuvwxyz123456", "github_pat_abcdefghijklmnopqrstuvwxyz123456", "AKIA1234567890ABCDEF", "xoxb-1234567890abc", "Bearer abcdefghijklmnopqrstuvwxyz", "-----BEGIN RSA PRIVATE KEY-----\ncontent\n-----END RSA PRIVATE KEY-----", "password=hunter2", "API_KEY: hidden", "PRIVATEWORD"] {
            let out = r.text(secret); assert!(out.contains("[REDACTED:"), "{secret}");
        }
        for code in ["let token =", "password.is_empty()", "fn secret() {}", "sk-short", "AKIAexample", "Bearer short", "let api_key;", "ghp_short"] { assert_eq!(r.text(code), code); }
        let mut data = serde_json::json!({"password":"hunter2", "array":[{"text":"sk-abcdefghijklmnopqrstuvwxyz"}]});
        r.value(&mut data, 16384); assert!(!data.to_string().contains("hunter2")); assert!(!data.to_string().contains("abcdefghijklmnopqrstuvwxyz"));
    }
    #[test] fn quoted_json_assignments_remain_valid() {
        let r = Redactor::new(&[]).unwrap();
        let input = r#"{"password":"hunter2","secret":"two words","safe":"visible"}"#;
        let output = r.text(input);
        let parsed: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["password"], "[REDACTED:secret]");
        assert_eq!(parsed["secret"], "[REDACTED:secret]");
        assert_eq!(parsed["safe"], "visible");
        assert_eq!(r.text(&output), output);
    }
    #[test] fn embedded_assignments_and_redaction_markers_do_not_hide_secrets() {
        let r = Redactor::new(&["REDACTED".into()]).unwrap();
        assert_eq!(r.text(r#"{"text":"password=hunter2"}"#), r#"{"text":"password=[REDACTED:secret]"}"#);
        assert_eq!(r.text("secret=\"[REDACTED:secret] still-secret\""), "secret=\"[REDACTED:secret]\"");
        let output = r.text("password=hunter2 REDACTED");
        assert_eq!(output, "password=[REDACTED:secret] [REDACTED:custom]");
        assert_eq!(r.text(&output), output);
    }
    #[test] fn multiword_quoted_assignments() {
        let r = Redactor::new(&[]).unwrap();
        for (input, expected) in [
            ("SECRET=\"two words\" next=visible", "SECRET=\"[REDACTED:secret]\" next=visible"),
            ("secret='two words' next=visible", "secret='[REDACTED:secret]' next=visible"),
        ] { assert_eq!(r.text(input), expected); assert_eq!(r.text(expected), expected); }
    }
    #[test] fn escaped_quotes_and_multiline_secrets() {
        let r = Redactor::new(&[]).unwrap();
        for input in [r#"{"password":"two \"quoted\" words\nnext line","safe":1}"#, r#"{"password":"ends with backslash\\","safe":1}"#] {
            let output = r.text(input);
            let parsed: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(parsed["password"], "[REDACTED:secret]");
            assert_eq!(parsed["safe"], 1);
            assert_eq!(r.text(&output), output);
        }
        for input in ["SECRET=\"first\nsecond\" safe=ok", "secret='first\\' quote\nsecond' safe=ok"] {
            let output = r.text(input);
            assert!(!output.contains("first")); assert!(!output.contains("second"));
            assert!(output.ends_with(" safe=ok")); assert_eq!(r.text(&output), output);
        }
        let output = r.text("SECRET=\"unterminated\nsecret tail");
        assert_eq!(output, "SECRET=\"[REDACTED:secret]");
        assert_eq!(r.text(&output), output);
    }
    #[test] fn credential_and_header_fields_in_text_and_values() {
        let r = Redactor::new(&[]).unwrap();
        for key in ["access_token", "refresh_token", "id_token", "x-api-key", "x-goog-api-key", "client_secret", "client-secret", "private_key", "private-key", "AWS_SECRET_ACCESS_KEY", "AWS_ACCESS_KEY_ID", "OPENAI_API_KEY", "auth_token", "signing_key", "consumer_secret", "authorization", "Proxy-Authorization"] {
            let input = format!(r#"{{"{key}":"synthetic secret"}}"#);
            let parsed: Value = serde_json::from_str(&r.text(&input)).unwrap();
            assert_eq!(parsed[key], "[REDACTED:secret]", "{key}");
            assert_eq!(r.text(&format!("{key}: short-secret")), format!("{key}: [REDACTED:secret]"));
            let mut data = serde_json::json!({key: {"nested":"synthetic secret"}});
            r.value(&mut data, 16384);
            assert_eq!(data[key], "[REDACTED:secret]", "{key}");
        }
        assert_eq!(r.text("Authorization: Bearer short-token\nx-api-key: short-key"), "Authorization: [REDACTED:secret]\nx-api-key: [REDACTED:secret]");
        assert_eq!(r.text("Proxy-Authorization: Basic dXNlcjpwYXNz"), "Proxy-Authorization: [REDACTED:secret]");
    }
    #[test] fn credential_false_positive_corpus() {
        let r = Redactor::new(&[]).unwrap();
        for input in [
            "token_count=42", "input_tokens: 42", "output_tokens=7", "max_tokens=100",
            "tokenizer='cl100k_base'", "tokenization: enabled", "tokens=42",
            "api_key_count=2", "secret_length=8", "password_hash_algorithm: argon2",
            "fn secret() {}", "fn get_api_key() -> String {}", "fn access_token_count() {}",
            "let tokenizer = Tokenizer::new();", "mytoken: ordinary", "notpassword=ordinary",
            r#"{"token_count":42,"tokenizer":"cl100k_base","input_tokens":12}"#,
        ] { assert_eq!(r.text(input), input, "{input}"); }
        let mut counts = serde_json::json!({"token_count":42,"tokenizer":"cl100k_base","input_tokens":12,"api_key_count":2});
        let original = counts.clone(); r.value(&mut counts, 16384); assert_eq!(counts, original);
    }
    #[test] fn unterminated_pem_redacts_through_end() {
        let r = Redactor::new(&[]).unwrap();
        for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY", "ENCRYPTED PRIVATE KEY"] {
            let output = r.text(&format!("safe -----BEGIN {label}-----\nsecret-body\nsecret-tail"));
            assert_eq!(output, format!("safe [REDACTED:private-key:open:{label}:0]"));
            assert_eq!(r.text(&output), output);
        }
    }
    #[test] fn complete_pem_resave_preserves_surrounding_text() {
        let r = Redactor::new(&[]).unwrap();
        let output = r.text("before -----BEGIN RSA PRIVATE KEY-----\nsecret-body\n-----END RSA PRIVATE KEY----- after");
        assert_eq!(output, "before [REDACTED:private-key] after");
        let saved = serde_json::to_string(&output).unwrap();
        let loaded: String = serde_json::from_str(&saved).unwrap();
        assert_eq!(Redactor::new(&[]).unwrap().text(&format!("{loaded} appended")), "before [REDACTED:private-key] after appended");
    }
    #[test] fn complete_pem_in_quoted_json_remains_valid_and_idempotent() {
        let r = Redactor::new(&[]).unwrap();
        for key in ["text", "private_key", "secret"] {
            for prefix in ["", "extra secret "] {
                let input = serde_json::json!({key: format!("{prefix}-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY----- tail"), "safe":"visible"}).to_string();
                let output = r.text(&input);
                let parsed: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(parsed["safe"], "visible");
                if key == "text" { assert_eq!(parsed[key], format!("{prefix}[REDACTED:private-key] tail")); }
                else { assert_eq!(parsed[key], "[REDACTED:secret]"); }
                assert_eq!(r.text(&output), output);
            }
        }
    }
    #[test] fn fragmented_pem_resaves_suppress_tail_until_end() {
        let mut text = "before -----BEGIN RSA PRIVATE KEY-----\nfirst-secret".to_string();
        for chunk in ["", "\nsecond-secret", "\n-----EN", "D RSA PRI", "VATE KEY--", "--- after"] {
            text.push_str(chunk);
            let mut saved = Value::String(text);
            Redactor::new(&[]).unwrap().value(&mut saved, 16384);
            let disk = serde_json::to_vec(&saved).unwrap();
            let loaded: Value = serde_json::from_slice(&disk).unwrap();
            text = loaded.as_str().unwrap().to_owned();
            assert!(!text.contains("first-secret")); assert!(!text.contains("second-secret"));
            assert_eq!(Redactor::new(&[]).unwrap().text(&text), text);
        }
        assert_eq!(text, "before [REDACTED:private-key] after");
    }
    #[test] fn pem_end_fragments_at_every_boundary() {
        let end = "-----END EC PRIVATE KEY-----";
        for boundary in 0..=end.len() {
            let first = Redactor::new(&[]).unwrap().text(&format!("-----BEGIN EC PRIVATE KEY-----\nsecret\n{}", &end[..boundary]));
            let second = Redactor::new(&[]).unwrap().text(&format!("{first}{} visible", &end[boundary..]));
            assert_eq!(second, "[REDACTED:private-key] visible", "boundary {boundary}");
        }
    }
    #[test] fn capped_pem_state_and_custom_patterns_keep_builtin_scrubbing() {
        let r = Redactor::new(&["PRIVATEWORD".into(), "private-key".into()]).unwrap();
        let mut text = Value::String(format!("{} -----BEGIN PRIVATE KEY-----\nsecret", "visible ".repeat(100)));
        r.value(&mut text, 100);
        let saved = text.as_str().unwrap();
        assert!(saved.len() <= 100); assert!(saved.contains("[REDACTED:private-key:open:"));
        let output = r.text(&format!("{saved}\nmore-secret\n-----END PRIVATE KEY----- PRIVATEWORD password=hunter2"));
        assert!(!output.contains("more-secret")); assert!(!output.contains("PRIVATEWORD")); assert!(!output.contains("hunter2"));
        assert!(output.contains("[REDACTED:private-key]")); assert!(output.contains("[REDACTED:custom]"));
        assert_eq!(r.text(&output), output);
        let unrelated = "public next field"; assert_eq!(r.text(unrelated), unrelated);
    }
    #[test] fn utf8_caps() {
        let text = "🦀".repeat(100);
        for n in 0..100 { let result = cap(&text, n); assert!(result.len() <= n); }
        assert!(cap(&text, 70).contains("[truncated")); assert_eq!(cap("hello", 20), "hello");
        assert!(Redactor::new(&["[".into()]).is_err());
    }
    #[test] fn tiny_content_caps_keep_system_pem_state_until_tail_is_closed() {
        for bytes in 1..48 {
            let mut value=Value::String("-----BEGIN PRIVATE KEY-----\nfirst-secret".into());Redactor::new(&[]).unwrap().value(&mut value,bytes);
            for chunk in ["A","B","-----END PRIVATE KEY-----"] {
                let mut text=value.as_str().unwrap().to_string();text.push_str(chunk);value=Value::String(text);Redactor::new(&[]).unwrap().value(&mut value,bytes);
                assert!(!value.as_str().unwrap().contains("first-secret"));assert!(!value.as_str().unwrap().contains("AB"));
            }
        }
        let label="A".repeat(1000);let output=Redactor::new(&[]).unwrap().text(&format!("-----BEGIN {label} PRIVATE KEY-----secret"));assert!(output.len()<128);assert!(!output.contains("secret"));
    }
}
