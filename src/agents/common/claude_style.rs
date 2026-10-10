use serde_json::{json,Value};
pub fn hook_group(matcher: Option<&str>, command: &str, timeout: Option<u64>) -> Value {
    let mut hook=json!({"type":"command","command":command});if let Some(timeout)=timeout {hook["timeout"]=json!(timeout);}
    let mut group=json!({"hooks":[hook]});if let Some(matcher)=matcher {group["matcher"]=json!(matcher);}group
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn optional_group_fields() {
        assert_eq!(hook_group(None,"herdr-respan hook fake",None),json!({"hooks":[{"type":"command","command":"herdr-respan hook fake"}]}));
        assert_eq!(hook_group(Some("*"),"cmd",Some(10)),json!({"matcher":"*","hooks":[{"type":"command","command":"cmd","timeout":10}]}));
    }
}
