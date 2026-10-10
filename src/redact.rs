use crate::Result;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub struct Redactor { custom: Vec<Regex> }
static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| [
        (r"sk-ant-[A-Za-z0-9_-]{20,}", "anthropic"),
        (r"sk-[A-Za-z0-9_-]{20,}", "openai"),
        (r"gh[pousr]_[A-Za-z0-9]{30,}", "github"),
        (r"github_pat_[A-Za-z0-9_]{30,}", "github"),
        (r"AKIA[0-9A-Z]{16}", "aws"),
        (r"xox[abposr]-[A-Za-z0-9-]{10,}", "slack"),
        (r"Bearer\s+[A-Za-z0-9._~+/=-]{20,}", "bearer"),
        (r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", "private-key"),
        (r"(?i)(password|passwd|secret|api[_-]?key|token)(\s*[:=]\s*)\S+", "assignment"),
    ].into_iter().map(|(p, k)| (Regex::new(p).expect("constant regex"), k)).collect());
impl Redactor {
    pub fn new(custom: &[String]) -> Result<Self> { Ok(Self { custom: custom.iter().map(|s| Regex::new(s)).collect::<std::result::Result<_, _>>()? }) }
    pub fn text(&self, input: &str) -> String {
        let mut text = input.to_string();
        for (regex, kind) in PATTERNS.iter() {
            text = if *kind == "assignment" { regex.replace_all(&text, "${1}${2}[REDACTED:secret]").into_owned() }
            else { regex.replace_all(&text, format!("[REDACTED:{kind}]").as_str()).into_owned() };
        }
        for regex in &self.custom { text = regex.replace_all(&text, "[REDACTED:custom]").into_owned(); }
        text
    }
    pub fn value(&self, value: &mut Value, bytes: usize) {
        match value {
            Value::String(text) => *text = cap(&self.text(text), bytes),
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
fn sensitive_key(key: &str) -> bool {
    matches!(key.to_ascii_lowercase().as_str(), "password" | "passwd" | "secret" | "api_key" | "api-key" | "apikey" | "token" | "authorization")
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
    #[test] fn secrets_and_false_positives() {
        let r = Redactor::new(&["PRIVATEWORD".into()]).unwrap();
        for secret in ["sk-ant-abcdefghijklmnopqrstuvwxyz", "sk-abcdefghijklmnopqrstuvwxyz", "ghp_abcdefghijklmnopqrstuvwxyz123456", "github_pat_abcdefghijklmnopqrstuvwxyz123456", "AKIA1234567890ABCDEF", "xoxb-1234567890abc", "Bearer abcdefghijklmnopqrstuvwxyz", "-----BEGIN RSA PRIVATE KEY-----\ncontent\n-----END RSA PRIVATE KEY-----", "password=hunter2", "API_KEY: hidden", "PRIVATEWORD"] {
            let out = r.text(secret); assert!(out.contains("[REDACTED:"), "{secret}");
        }
        for code in ["let token =", "password.is_empty()", "fn secret() {}", "sk-short", "AKIAexample", "Bearer short", "let api_key;", "ghp_short"] { assert_eq!(r.text(code), code); }
        let mut data = serde_json::json!({"password":"hunter2", "array":[{"text":"sk-abcdefghijklmnopqrstuvwxyz"}]});
        r.value(&mut data, 16384); assert!(!data.to_string().contains("hunter2")); assert!(!data.to_string().contains("abcdefghijklmnopqrstuvwxyz"));
    }
    #[test] fn utf8_caps() {
        let text = "🦀".repeat(100);
        for n in 0..100 { let result = cap(&text, n); assert!(result.len() <= n); }
        assert!(cap(&text, 70).contains("[truncated")); assert_eq!(cap("hello", 20), "hello");
        assert!(Redactor::new(&["[".into()]).is_err());
    }
}
