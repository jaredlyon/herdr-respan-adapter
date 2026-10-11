use crate::{config::Config, paths::Paths, record::{HookCtx, SpanRecord}};
use std::collections::BTreeMap;

#[test]
fn captured_interactive_omp_text_and_echo_replay() {
    for (payload, chats, tools) in [
        (include_bytes!("fixtures/live-text.json").as_slice(), 1, 0),
        (include_bytes!("fixtures/live-tool.json").as_slice(), 2, 1),
    ] {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
        let mut ctx = HookCtx::new(paths.clone(), Config::default(), "omp", BTreeMap::from([
            ("herdr.pane_id".into(), serde_json::json!("captured-pane")),
        ]), None, false).unwrap();
        (super::SPEC.hook)(&mut ctx, payload).unwrap();
        ctx.finish().unwrap();
        let records = || -> Vec<SpanRecord> {
            crate::spool::ready(&paths.state).unwrap().iter().flat_map(|file| {
                std::fs::read_to_string(file).unwrap().lines()
                    .map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
            }).collect()
        };
        let spans = records();
        assert_eq!(spans.len(), 1 + chats + tools);
        let turn = spans.iter().find(|s| s.log_type == "agent").unwrap();
        assert!(turn.parent_key.is_none());
        assert_eq!(spans.iter().filter(|s| s.log_type == "chat").count(), chats);
        assert_eq!(spans.iter().filter(|s| s.log_type == "tool").count(), tools);
        for span in &spans {
            assert_eq!(span.trace_key, turn.trace_key);
            assert_eq!(span.thread_id.as_deref(), Some("captured-omp-session"));
            assert_eq!(span.metadata["herdr.pane_id"], "captured-pane");
            assert_eq!(span.status, "ok");
            if span.log_type != "agent" { assert_eq!(span.parent_key.as_ref(), Some(&turn.span_key)); }
            if span.log_type == "chat" {
                // Adapter input includes cached tokens: OpenAI prompt=11, cached=3.
                assert_eq!(span.usage.input_tokens, Some(11));
                assert_eq!(span.usage.output_tokens, Some(7));
                assert_eq!(span.usage.cache_read_input_tokens, Some(3));
                assert_eq!(span.usage.cache_creation_input_tokens, Some(0));
                assert_eq!(span.usage.reasoning_tokens, None);
                assert_eq!(span.model.as_deref(), Some("mock-model"));
                assert_eq!(span.provider.as_deref(), Some("mock"));
            } else if span.log_type == "tool" {
                assert_eq!(span.name, "bash");
                assert_eq!(span.input.as_ref().unwrap()["command"], "echo hra-e2e");
                assert!(span.output.as_ref().unwrap()["content"][0]["text"].as_str().unwrap().starts_with("hra-e2e\n"));
            }
        }
        (super::SPEC.hook)(&mut ctx, payload).unwrap();
        ctx.finish().unwrap();
        assert_eq!(records().len(), spans.len());
    }
}
