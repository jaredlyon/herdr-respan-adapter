use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord, setup::{Edit, SetupCtx}};

fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") }, Config { customer_identifier: "test".into(), ..Default::default() }, "gemini", BTreeMap::new(), None, false).unwrap()
}
fn fixture(path: &Path) -> Vec<Value> {
    let mut values: Vec<Value> = serde_json::from_str(include_str!("fixtures/stream.json")).unwrap();
    for value in &mut values { value["transcript_path"] = json!(path.to_string_lossy()); }
    values
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|p| {
        std::fs::read_to_string(p).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
    }).collect()
}
#[test]
fn multichunk_golden_batch_and_real_bridge_tree() {
    let tmp = crate::tests::Temp::new(); let ctx = context(&tmp);
    let transcript_path = tmp.path.join("session.jsonl"); let payloads = fixture(&transcript_path);
    let mut state = State::default(); let mut events = Vec::new();
    for (index, payload) in payloads.iter().enumerate() {
        if index == 5 { std::fs::write(&transcript_path, include_str!("fixtures/session.jsonl")).unwrap(); }
        translate(&ctx, &mut state, payload, index as u64 + 1, &mut events).unwrap();
        if index < 5 { assert!(!events.iter().any(|e| matches!(e, Event::Chat { .. }))); }
    }
    let batch = Batch { v: 1, agent: "gemini".into(), session_id: "synthetic-gemini".into(), session_path: None, cwd: Some("/synthetic/repo".into()), agent_version: None, events };
    let expected: Batch = serde_json::from_str(include_str!("fixtures/expected-batch.json")).unwrap();
    assert_eq!(serde_json::to_value(&batch).unwrap(), serde_json::to_value(expected).unwrap());
    let mut ctx = ctx; bridge::ingest(&mut ctx, batch.clone()).unwrap(); bridge::ingest(&mut ctx, batch).unwrap();
    let all = records(&tmp); assert_eq!(all.len(), 2);
    let chat = all.iter().find(|r| r.log_type == "chat").unwrap();
    let turn = all.iter().find(|r| r.log_type == "agent").unwrap();
    assert_eq!(chat.parent_key.as_deref(), Some(turn.span_key.as_str()));
    assert_eq!(chat.thread_id.as_deref(), Some("synthetic-gemini"));
    assert_eq!(chat.usage.input_tokens, Some(20)); assert_eq!(chat.usage.output_tokens, Some(5));
    assert_eq!(chat.usage.cache_read_input_tokens, Some(7)); assert_eq!(chat.usage.reasoning_tokens, Some(3));
    assert!(transcript(&mut state, transcript_path.to_str().unwrap()).unwrap().is_empty());
}
#[test]
fn hook_state_survives_processes_and_replayed_chunks() {
    let tmp = crate::tests::Temp::new(); let path = tmp.path.join("session.jsonl");
    for (index, payload) in fixture(&path).iter().enumerate() {
        if index == 5 { std::fs::write(&path, include_str!("fixtures/session.jsonl")).unwrap(); }
        let mut ctx = context(&tmp); let bytes = serde_json::to_vec(payload).unwrap();
        hook(&mut ctx, &bytes).unwrap(); hook(&mut ctx, &bytes).unwrap();
        if index < 5 { assert!(records(&tmp).is_empty()); }
    }
    let all = records(&tmp); assert_eq!(all.iter().filter(|r| r.log_type == "chat").count(), 1);
    assert_eq!(all.iter().find(|r| r.log_type == "chat").unwrap().output, Some(json!([{"role":"assistant","content":"Hello world"}])));
    assert_eq!(all.len(), 2);
}
#[test]
fn tool_boundary_and_second_request_stay_under_one_turn() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let values = fixture(&tmp.path.join("absent.jsonl"));
    for payload in &values[..5] { hook(&mut ctx, &serde_json::to_vec(payload).unwrap()).unwrap(); }
    let make_tool = |event: &str, stamp: &str, input: Value| {
        let mut p = values[0].clone(); p["hook_event_name"] = json!(event); p["timestamp"] = json!(stamp); p["tool_name"] = json!("read_file"); p["tool_input"] = input; p["tool_response"] = json!({"llmContent":"file", "returnDisplay":"file"}); p
    };
    let before = make_tool("BeforeTool", "2026-10-09T12:00:01.000Z", json!({"path":"a"}));
    hook(&mut ctx, &serde_json::to_vec(&before).unwrap()).unwrap();
    assert_eq!(records(&tmp).iter().filter(|r| r.log_type == "chat").count(), 1);
    let before_b = make_tool("BeforeTool", "2026-10-09T12:00:01.001Z", json!({"path":"b"}));
    hook(&mut ctx, &serde_json::to_vec(&before_b).unwrap()).unwrap();
    for (stamp, input) in [("2026-10-09T12:00:01.002Z", json!({"path":"b"})), ("2026-10-09T12:00:01.003Z", json!({"path":"a"}))] {
        hook(&mut ctx, &serde_json::to_vec(&make_tool("AfterTool", stamp, input)).unwrap()).unwrap();
    }
    let mut request = values[1].clone(); request["timestamp"] = json!("2026-10-09T12:00:02.000Z");
    hook(&mut ctx, &serde_json::to_vec(&request).unwrap()).unwrap();
    let mut chunk = values[3].clone(); chunk["timestamp"] = json!("2026-10-09T12:00:02.001Z");
    hook(&mut ctx, &serde_json::to_vec(&chunk).unwrap()).unwrap();
    hook(&mut ctx, &serde_json::to_vec(&values[5]).unwrap()).unwrap();
    let all = records(&tmp); assert_eq!(all.len(), 5);
    let turn = all.iter().find(|r| r.log_type == "agent").unwrap();
    for child in all.iter().filter(|r| r.log_type != "agent") { assert_eq!(child.parent_key.as_ref(), Some(&turn.span_key)); }
    assert!(all.iter().any(|r| r.span_key.ends_with("tool:tool-2026-10-09T12:00:01.000Z-1")));
}
#[test]
fn foreign_payloads_and_unknown_events_are_silent() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    for foreign in [json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"x"}), json!({"hook_event_name":"AfterModel","session_id":"s","llm_response":{}}), json!({"session_id":"s","hook_event_name":"SessionStart","transcript_path":"/tmp/qwen/session.jsonl","cwd":"/repo","timestamp":"now","source":"startup","permission_mode":"auto"})] {
        hook(&mut ctx, &serde_json::to_vec(&foreign).unwrap()).unwrap();
    }
    assert!(records(&tmp).is_empty());
    assert!(hook(&mut ctx, b"not json").is_err());
    assert!(SPEC.reply.is_none());
}
#[test]
fn metadata_only_and_secrets_do_not_persist_content() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); ctx.config.content_cap_bytes = 0;
    let mut values = fixture(&tmp.path.join("absent.jsonl")); values[0]["prompt"] = json!("password=private");
    values[1]["llm_request"]["messages"][0]["content"] = json!("password=private");
    for p in &values[..3] { hook(&mut ctx, &serde_json::to_vec(p).unwrap()).unwrap(); }
    let key = format!("session-{}.json", digest(b"synthetic-gemini"));
    let saved = std::fs::read_to_string(ctx.state_dir().join(key)).unwrap();
    assert!(!saved.contains("private")); assert!(!saved.contains("Say hello")); assert!(!saved.contains("Hello "));
    hook(&mut ctx, &serde_json::to_vec(&values[5]).unwrap()).unwrap();
    assert!(records(&tmp).iter().all(|r| r.input.is_none() && r.output.is_none()));
}
#[test]
fn setup_preserves_telemetry_herdr_and_remove_roundtrips() {
    let tmp = crate::tests::Temp::new(); let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let project = tmp.path.join("project"); let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(project.clone())).unwrap();
    let file = project.join(".gemini/settings.json"); std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let original = json!({"telemetry":{"enabled":true,"target":"gcp","otlpEndpoint":"http://localhost:4317"},"hooksConfig":{"enabled":false,"disabled":["foreign"]},"hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"herdr-agent-session","timeout":10000}]}]}});
    std::fs::write(&file, original.to_string()).unwrap();
    let plan = setup::plan(&ctx).unwrap(); assert_eq!(plan.edits.len(), 8);
    for edit in &plan.edits {
        let Edit::JsonInsert { file: edited, pointer, value, .. } = edit else { panic!("only JSON hook inserts"); };
        assert_eq!(edited, &file); assert!(pointer.starts_with("/hooks/"));
        assert_eq!(value["hooks"][0]["timeout"], json!(5000));
        assert_eq!(value["hooks"][0]["env"]["HERDR_RESPAN_ADAPTER"], json!("gemini"));
    }
    crate::setup::apply(&ctx, "gemini", &plan).unwrap(); let once = std::fs::read(&file).unwrap();
    crate::setup::apply(&ctx, "gemini", &plan).unwrap(); assert_eq!(once, std::fs::read(&file).unwrap());
    let changed: Value = serde_json::from_slice(&once).unwrap();
    assert_eq!(changed["telemetry"], original["telemetry"]); assert_eq!(changed["hooksConfig"], original["hooksConfig"]);
    assert_eq!(changed["hooks"]["SessionStart"][0], original["hooks"]["SessionStart"][0]);
    crate::setup::remove(&ctx, &["gemini".into()]).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(file).unwrap()).unwrap(), original);
}

#[test]
fn concurrent_tools_with_identical_millisecond_timestamps_keep_distinct_spans() {
    let tmp = crate::tests::Temp::new();
    let mut payloads: Vec<Value> = serde_json::from_str(include_str!("fixtures/tools-same-timestamp.json")).unwrap();
    for payload in &mut payloads { payload["transcript_path"] = json!(tmp.path.join("absent.jsonl").to_string_lossy()); }
    assert_eq!(payloads[0]["timestamp"], payloads[1]["timestamp"]);
    for payload in &payloads {
        // Each native hook gets a new context, just like the actual one-shot process.
        let mut ctx = context(&tmp);
        hook(&mut ctx, &serde_json::to_vec(payload).unwrap()).unwrap();
    }
    let all = records(&tmp);
    let tools: Vec<_> = all.iter().filter(|r| r.log_type == "tool").collect();
    assert_eq!(tools.len(), 2);
    assert_ne!(tools[0].span_key, tools[1].span_key);
    assert_eq!(tools[0].parent_key, tools[1].parent_key);
    assert!(tools.iter().any(|r| r.name == "read_file" && r.input == Some(json!({"path":"a"}))));
    assert!(tools.iter().any(|r| r.name == "list_directory" && r.input == Some(json!({"path":"b"}))));
}
