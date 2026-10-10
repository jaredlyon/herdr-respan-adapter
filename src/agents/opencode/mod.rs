//! OpenCode plugins translate native events; the shared bridge owns ingestion.
mod setup;

use crate::agents::{common::bridge, AgentSpec};

pub const SPEC: AgentSpec = AgentSpec {
    kind: "opencode",
    display: "OpenCode",
    hook: bridge::hook,
    plan: setup::plan,
    reply: None,
    settle: None,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, paths::Paths, record::{HookCtx, SpanRecord}};
    use std::collections::BTreeMap;

    fn context(tmp: &crate::tests::Temp, proxy: bool) -> HookCtx {
        HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
            Config { customer_identifier: "synthetic".into(), ..Default::default() }, "opencode",
            BTreeMap::from([("herdr.pane_id".into(), serde_json::json!("pane1"))]), None, proxy).unwrap()
    }
    fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
        crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|path| {
            std::fs::read_to_string(path).unwrap().lines()
                .map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
        }).collect()
    }
    const BATCH: &[u8] = include_bytes!("fixtures/batch.json");

    #[test]
    fn asset_batch_ingests_expected_tree_and_usage_once() {
        let tmp = crate::tests::Temp::new();
        let mut ctx = context(&tmp, false);
        (SPEC.hook)(&mut ctx, BATCH).unwrap();
        (SPEC.hook)(&mut ctx, BATCH).unwrap();
        let spans = records(&tmp);
        assert_eq!(spans.len(), 3);
        let turn = spans.iter().find(|s| s.log_type == "agent").unwrap();
        assert_eq!(turn.span_key, "opencode:s1:p1:turn");
        assert_eq!(turn.thread_id.as_deref(), Some("s1"));
        assert_eq!(turn.start_ns, 1_000_000_000);
        assert_eq!(turn.end_ns, 1_200_000_000);
        assert_eq!(turn.metadata["herdr.pane_id"], "pane1");
        let chat = spans.iter().find(|s| s.log_type == "chat").unwrap();
        assert_eq!(chat.parent_key.as_deref(), Some(turn.span_key.as_str()));
        assert_eq!(chat.span_key, "opencode:s1:p1:chat:s1:a1");
        assert_eq!(chat.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(chat.provider.as_deref(), Some("anthropic"));
        assert_eq!(chat.usage.input_tokens, Some(12));
        assert_eq!(chat.usage.output_tokens, Some(5));
        assert_eq!(chat.usage.cache_read_input_tokens, Some(3));
        assert_eq!(chat.usage.cache_creation_input_tokens, Some(4));
        assert_eq!(chat.usage.reasoning_tokens, Some(2));
        assert_eq!(chat.metadata["estimated_cost_usd"], 0.01);
        let tool = spans.iter().find(|s| s.log_type == "tool").unwrap();
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.parent_key.as_deref(), Some(turn.span_key.as_str()));
        assert_eq!(tool.output, Some(serde_json::json!("/repo")));
    }

    #[test]
    fn foreign_payloads_are_rejected_and_proxy_suppresses_model_spans() {
        let tmp = crate::tests::Temp::new();
        let mut ctx = context(&tmp, true);
        for payload in [br#"{"hook_event_name":"Stop","session_id":"s1"}"#.as_slice(), br#"{"v":1,"agent":"pi","session_id":"s1","events":[]}"#.as_slice()] {
            assert!((SPEC.hook)(&mut ctx, payload).is_err());
        }
        assert!(records(&tmp).is_empty());
        (SPEC.hook)(&mut ctx, BATCH).unwrap();
        let spans = records(&tmp);
        assert_eq!(spans.len(), 2);
        assert!(!spans.iter().any(|s| s.log_type == "chat"));
    }
}
