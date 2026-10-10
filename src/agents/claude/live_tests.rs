use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord};

#[test]
fn captured_real_claude_text_and_echo_replay() {
    for (hooks, transcript, chats, tools) in [
        (include_str!("fixtures/live-text-hooks.json"), include_str!("fixtures/live-text-transcript.jsonl"), 1, 0),
        (include_str!("fixtures/live-tool-hooks.json"), include_str!("fixtures/live-tool-transcript.jsonl"), 2, 1),
    ] {
        let tmp = crate::tests::Temp::new();
        let path = tmp.path.join("live.jsonl");
        let mut payloads: Vec<Value> = serde_json::from_str(hooks).unwrap();
        for payload in &mut payloads { payload["transcript_path"] = json!(path); }
        std::fs::write(&path, "").unwrap();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
        let mut ctx = HookCtx::new(paths.clone(), Config::default(), "claude", BTreeMap::from([("herdr.pane_id".into(), json!("live-pane"))]), None, false).unwrap();
        for payload in &payloads {
            // Real hook ordering retained; disk rows may lag until Stop.
            if payload["hook_event_name"] == "Stop" { std::fs::write(&path, transcript).unwrap(); }
            hook(&mut ctx, &serde_json::to_vec(payload).unwrap()).unwrap();
        }
        let key = format!("session-{}.json", payloads[0]["session_id"].as_str().unwrap());
        let mut state: Session = ctx.load(&key).unwrap();
        state.stopped = state.stopped.map(|_| 0);
        for file in state.files.values_mut() { file.stable_since = 0; }
        ctx.save(&key, &state).unwrap();
        assert!(!settle_pending(&mut ctx).unwrap());
        ctx.finish().unwrap();
        let records = || -> Vec<SpanRecord> {
            crate::spool::ready(&paths.state).unwrap().iter().flat_map(|file| std::fs::read_to_string(file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()).collect()
        };
        let spans = records();
        assert_eq!(spans.len(), 1 + chats + tools);
        let turn = spans.iter().find(|s| s.log_type == "agent").unwrap();
        assert!(turn.parent_key.is_none());
        assert_eq!(spans.iter().filter(|s| s.log_type == "chat").count(), chats);
        assert_eq!(spans.iter().filter(|s| s.log_type == "tool").count(), tools);
        for span in &spans {
            assert_eq!(span.trace_key, turn.trace_key);
            assert_eq!(span.thread_id, turn.thread_id);
            assert_eq!(span.metadata["herdr.pane_id"], "live-pane");
            assert_eq!(span.status, "ok");
            if span.log_type != "agent" { assert_eq!(span.parent_key.as_ref(), Some(&turn.span_key)); }
            if span.log_type == "chat" {
                assert_eq!(span.usage.input_tokens, Some(11));
                assert_eq!(span.usage.output_tokens, Some(7));
                assert_eq!(span.usage.cache_read_input_tokens, Some(3));
                assert_eq!(span.model.as_deref(), Some("claude-haiku-4-5-20251001"));
            } else if span.log_type == "tool" {
                assert_eq!(span.name, "Bash");
                assert_eq!(span.input.as_ref().unwrap()["command"], "echo hra-e2e");
                assert_eq!(span.output.as_ref().unwrap()["stdout"], "hra-e2e");
            }
        }
        // Observational Stop/SessionEnd replays must not duplicate any child/root.
        for payload in payloads.iter().filter(|p| matches!(p["hook_event_name"].as_str(), Some("Stop" | "SessionEnd"))) {
            hook(&mut ctx, &serde_json::to_vec(payload).unwrap()).unwrap();
        }
        settle_pending(&mut ctx).unwrap();
        ctx.finish().unwrap();
        assert_eq!(records().len(), spans.len());
    }
}
