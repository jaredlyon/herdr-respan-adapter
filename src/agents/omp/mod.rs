//! OMP's Bun extension sends completed turn batches through the shared assembler.
mod setup;
use crate::agents::{common::bridge, AgentSpec};
pub const SPEC: AgentSpec = AgentSpec {
    kind: "omp", display: "omp", hook: bridge::hook, plan: setup::plan, reply: None,
};

#[cfg(test)]
mod tests {
    use crate::{agents::common::bridge::Batch, config::Config, paths::Paths, record::{HookCtx, SpanRecord}};
    use std::collections::BTreeMap;
    const FIXTURE: &[u8] = include_bytes!("fixtures/turn.json");
    #[test]
    fn batch_shape_span_tree_and_foreign_rejection() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths {state: tmp.path.join("state"), config: tmp.path.join("config")};
        let mut ctx = HookCtx::new(paths.clone(), Config { customer_identifier: "synthetic".into(), ..Default::default() }, "omp", BTreeMap::new(), None, false).unwrap();
        let batch: Batch = serde_json::from_slice(FIXTURE).unwrap();
        assert_eq!(batch.events.len(), 8);
        (super::SPEC.hook)(&mut ctx, FIXTURE).unwrap();
        ctx.finish().unwrap();
        let records: Vec<SpanRecord> = crate::spool::ready(&paths.state).unwrap().iter().flat_map(|p| {
            std::fs::read_to_string(p).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect::<Vec<_>>()
        }).collect();
        assert_eq!(records.len(), 6);
        let trace = "omp:synthetic-session:t1";
        for record in &records {
            assert_eq!(record.trace_key.as_deref(), Some(trace));
            assert_eq!(record.thread_id.as_deref(), Some("synthetic-session"));
            assert_eq!(record.agent, "omp");
        }
        let chat = records.iter().find(|s| s.span_key.ends_with(":chat:response-one")).unwrap();
        assert_eq!(chat.usage.input_tokens, Some(11));
        assert_eq!(chat.usage.cache_read_input_tokens, Some(0));
        assert_eq!(chat.usage.reasoning_tokens, Some(1));
        assert_eq!(chat.model.as_deref(), Some("tiny"));
        let child = records.iter().find(|s| s.span_key.ends_with(":sub:child")).unwrap();
        assert_eq!(child.parent_key.as_deref(), Some("omp:synthetic-session:t1:tool:spawn"));
        let child_chat = records.iter().find(|s| s.span_key.ends_with(":chat:response-child")).unwrap();
        assert_eq!(child_chat.parent_key.as_deref(), Some("omp:synthetic-session:t1:sub:child"));
        assert_eq!(child_chat.usage.input_tokens, None);
        let mut foreign: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        foreign["agent"] = serde_json::json!("pi");
        assert!((super::SPEC.hook)(&mut ctx, &serde_json::to_vec(&foreign).unwrap()).is_err());
        assert!((super::SPEC.hook)(&mut ctx, br#"{"session_id":"foreign","hook_event_name":"Stop"}"#).is_err());
        (super::SPEC.hook)(&mut ctx, FIXTURE).unwrap();
        assert_eq!(crate::spool::ready(&paths.state).unwrap().len(), 1);
    }
}
