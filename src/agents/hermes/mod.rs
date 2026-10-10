//! Hermes observer plugin: native kwargs are translated in Python to shared bridge batches.
mod setup;
use crate::{agents::{common::bridge, AgentSpec}, record::HookCtx, Result};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "hermes", display: "Hermes Agent", hook, plan: setup::plan, reply: None,
};
fn hook(ctx: &mut HookCtx, stdin: &[u8]) -> Result<()> { bridge::hook(ctx, stdin) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, paths::Paths, record::SpanRecord};
    use std::collections::BTreeMap;

    #[test]
    fn fixture_spool_tree_and_foreign_rejection() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
        let mut ctx = HookCtx::new(paths.clone(), Config::default(), "hermes", BTreeMap::new(), None, false).unwrap();
        let fixture = include_bytes!("fixtures/batch.json");
        hook(&mut ctx, fixture).unwrap();
        let records: Vec<SpanRecord> = crate::spool::ready(&paths.state).unwrap().into_iter().flat_map(|p| {
            std::fs::read_to_string(p).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect::<Vec<_>>()
        }).collect();
        assert_eq!(records.len(), 7);
        let turn = records.iter().find(|r| r.span_key.ends_with(":turn")).unwrap();
        assert_eq!(turn.thread_id.as_deref(), Some("parent-session"));
        let sub = records.iter().find(|r| r.span_key.ends_with(":sub:child-1")).unwrap();
        assert_eq!(sub.parent_key.as_deref(), Some("hermes:parent-session:turn-1:tool:delegate-1"));
        let chat = records.iter().find(|r| r.span_key.ends_with(":chat:child-request")).unwrap();
        assert_eq!(chat.parent_key.as_deref(), Some("hermes:parent-session:turn-1:sub:child-1"));
        let main = records.iter().find(|r| r.span_key.ends_with(":chat:request-1")).unwrap();
        assert_eq!(main.usage.input_tokens, Some(19));
        assert_eq!(main.usage.cache_read_input_tokens, Some(5));
        assert!(records.iter().any(|r| r.metadata.get("hermes.aux_task") == Some(&serde_json::json!("title"))));
        let mut foreign: serde_json::Value = serde_json::from_slice(fixture).unwrap();
        foreign["agent"] = serde_json::json!("other");
        assert!(hook(&mut ctx, &serde_json::to_vec(&foreign).unwrap()).is_err());
        assert!(hook(&mut ctx, b"{\"event\":\"post_api_request\"}").is_err());
    }
}
