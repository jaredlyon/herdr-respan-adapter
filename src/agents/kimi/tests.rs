use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord};

fn fixtures() -> Vec<Value> { serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap() }
fn fixture_batch() -> Batch {
    let hooks = fixtures();
    let mut session = Session { cwd: "/synthetic/repo".into(), ..Default::default() };
    let mut events = vec![];
    native(&mut session, &hooks[1], 2_000_000_000, &mut events);
    for line in include_str!("fixtures/wire.jsonl").lines() {
        let record: Value = serde_json::from_str(line).unwrap();
        if let Some(ts) = record.get("timestamp").and_then(Value::as_f64) {
            wire(&mut session, &record["message"], (ts * 1e9) as u64, None, 16384, &mut events, 0);
        }
    }
    native(&mut session, &hooks[3], 4_100_000_000, &mut events);
    complete_tools(&mut session, &mut events);
    native(&mut session, &hooks[4], 5_000_000_000, &mut events);
    Batch { v: 1, agent: "kimi".into(), session_id: "synthetic-session".into(), session_path: None,
        cwd: Some(session.cwd), agent_version: None, events }
}
fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
        Config { customer_identifier: "synthetic-host".into(), ..Default::default() }, "kimi", BTreeMap::new(), None, false).unwrap()
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().into_iter().flat_map(|p|
        std::fs::read_to_string(p).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
    ).collect()
}
#[test] fn fixture_matches_batch_golden_and_spool_tree() {
    let batch = fixture_batch();
    let expected: Value = serde_json::from_str(include_str!("fixtures/expected-batch.json")).unwrap();
    assert_eq!(serde_json::to_value(&batch).unwrap(), expected);
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp);
    bridge::ingest(&mut ctx, batch.clone()).unwrap();
    bridge::ingest(&mut ctx, batch).unwrap();
    let records = records(&tmp);
    assert_eq!(records.len(), 6);
    let root = "kimi:synthetic-session:2000000000";
    let sub = records.iter().find(|r| r.span_key.ends_with(":sub:child-1")).unwrap();
    assert_eq!(sub.parent_key, Some(format!("{root}:tool:spawn-1")));
    for record in &records {
        assert_eq!(record.thread_id.as_deref(), Some("synthetic-session"));
        if record.log_type == "chat" { assert!(record.model.is_none()); }
    }
    let child = records.iter().find(|r| r.span_key.ends_with(":chat:child-response")).unwrap();
    assert_eq!(child.parent_key, Some(format!("{root}:sub:child-1")));
    let root_chat = records.iter().find(|r| r.span_key.ends_with(":chat:root-response")).unwrap();
    assert_eq!(root_chat.usage.input_tokens, Some(18));
}
#[test] fn foreign_payloads_do_not_queue_or_emit() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    for payload in [json!({"hook_event_name":"Stop","session_id":"claude","cwd":"/repo","stop_hook_active":false,"transcript_path":"/claude.jsonl"}),
        json!({"hookEventName":"Stop","sessionId":"grok","cwd":"/repo"}),
        json!({"hook_event_name":"SessionStart","session_id":"copilot","cwd":"/repo","source":"startup","timestamp":"2026-01-01T00:00:00Z"}),
        json!({"hook_event_name":"PermissionRequest","session_id":"s","cwd":"/repo"}),
        json!({"type":"turn_start","agent":"pi","session_id":"s"})] {
        assert!(!valid(&payload)); hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap();
    }
    assert_eq!(std::fs::read_dir(ctx.state_dir()).unwrap().count(), 0);
    for hook in fixtures() { assert!(valid(&hook)); }
}
#[test] fn setup_coexists_with_herdr_and_roundtrips() {
    let tmp = crate::tests::Temp::new();
    let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let ctx = crate::setup::SetupCtx::new(&paths, tmp.path.join("home"), None).unwrap();
    let plan = setup::make_plan(&ctx, "offline test".into()).unwrap();
    let file = match &plan.edits[0] { crate::setup::Edit::TomlInsert { file, .. } => file.clone(), _ => panic!("not TOML") };
    let original = "# user comment\ndefault_model = 'synthetic'\n[[hooks]]\nevent = 'SessionStart'\ncommand = 'herdr-agent-state'\nmatcher = ''\ntimeout = 10\n[models.synthetic]\nmodel = 'example'\n";
    std::fs::create_dir_all(file.parent().unwrap()).unwrap(); std::fs::write(&file, original).unwrap();
    crate::setup::apply(&ctx, "kimi", &plan).unwrap();
    let once = std::fs::read(&file).unwrap(); crate::setup::apply(&ctx, "kimi", &plan).unwrap(); assert_eq!(std::fs::read(&file).unwrap(), once);
    let installed: toml::Value = toml::from_str(std::str::from_utf8(&once).unwrap()).unwrap();
    let hooks = installed["hooks"].as_array().unwrap(); assert_eq!(hooks.len(), setup::EVENTS.len() + 1);
    assert_eq!(hooks[0]["command"].as_str(), Some("herdr-agent-state"));
    for hook in &hooks[1..] { assert!(setup::EVENTS.contains(&hook["event"].as_str().unwrap())); assert!(hook["command"].as_str().unwrap().contains("herdr-respan")); }
    crate::setup::remove(&ctx, &["kimi".into()]).unwrap();
    assert_eq!(toml::from_str::<toml::Value>(&std::fs::read_to_string(&file).unwrap()).unwrap(), toml::from_str::<toml::Value>(original).unwrap());
    assert!(std::fs::read_to_string(file).unwrap().contains("# user comment"));
}
#[test] fn incremental_tail_keeps_partial_record_and_defers_final_closure() {
    let tmp = crate::tests::Temp::new(); let ctx = context(&tmp);
    let path = tmp.path.join("wire.jsonl"); std::fs::write(&path, "{\"type\":\"metadata\",\"protocol_version\":\"1.10\"}\n").unwrap();
    let mut session = Session { wire: Some(path.clone()), ..Default::default() }; let mut events = vec![];
    tail(&ctx, &mut session, &mut events).unwrap();
    let hooks = fixtures(); native(&mut session, &hooks[1], 2_000_000_000, &mut events);
    let line = "{\"timestamp\":2.1,\"message\":{\"type\":\"TurnBegin\",\"payload\":{\"user_input\":\"test\"}}}";
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(line.as_bytes()).unwrap();
    tail(&ctx, &mut session, &mut events).unwrap(); assert!(session.awaiting_begin);
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
    tail(&ctx, &mut session, &mut events).unwrap(); assert!(!session.awaiting_begin);
    native(&mut session, &hooks[4], 3_000_000_000, &mut events);
    assert!(!events.iter().any(|e| matches!(e, Event::TurnEnd { .. })));
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"timestamp\":3.1,\"message\":{\"type\":\"TurnEnd\",\"payload\":{}}}\n").unwrap();
    tail(&ctx, &mut session, &mut events).unwrap(); assert_eq!(events.iter().filter(|e| matches!(e, Event::TurnEnd { .. })).count(), 1);
    tail(&ctx, &mut session, &mut events).unwrap(); assert_eq!(events.len(), 2);
}
#[test] fn unknown_usage_does_not_turn_into_zero_and_legacy_parent_alias() {
    let mut s = Session { turn: Some("t".into()), ..Default::default() }; let mut events = vec![];
    wire(&mut s, &json!({"type":"StatusUpdate","payload":{"context_tokens":1000}}), 10, None, 16384, &mut events, 0);
    assert!(events.is_empty());
    wire(&mut s, &json!({"type":"StatusUpdate","payload":{"message_id":"m","token_usage":{"input_other":4,"output":2}}}), 11, None, 16384, &mut events, 0);
    if let Event::Chat { usage: Some(usage), .. } = &events[0] { assert_eq!(usage.input_tokens, None); assert_eq!(usage.cache_read_input_tokens, None); } else { panic!("not chat"); }
    wire(&mut s, &json!({"type":"SubagentEvent","payload":{"agent_id":"child","task_tool_call_id":"spawn","event":{"type":"TurnBegin","payload":{"user_input":"test"}}}}), 12, None, 16384, &mut events, 0);
    assert!(matches!(&events[1], Event::SubagentStart { parent_id: Some(id), .. } if id == "spawn"));
}

#[test] fn queued_hooks_settle_late_wire_into_spool() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    let wire_path = tmp.path.join("wire.jsonl");
    std::fs::write(&wire_path, "{\"type\":\"metadata\",\"protocol_version\":\"1.10\"}\n").unwrap();
    let mut state = State::default();
    state.sessions.insert("synthetic-session".into(), Session { wire: Some(wire_path.clone()), ..Default::default() });
    ctx.save("native.json", &state).unwrap();
    let hooks = fixtures();
    ctx.save("native-1.json", &Pending { payload: hooks[1].clone(), ts: 2_000_000_000, ..Default::default() }).unwrap();
    assert!(settle(&mut ctx).unwrap());
    std::fs::write(&wire_path, include_str!("fixtures/wire.jsonl")).unwrap();
    ctx.save("native-2.json", &Pending { payload: hooks[3].clone(), ts: 4_100_000_000, ..Default::default() }).unwrap();
    ctx.save("native-3.json", &Pending { payload: hooks[4].clone(), ts: 5_000_000_000, ..Default::default() }).unwrap();
    assert!(!settle(&mut ctx).unwrap());
    assert_eq!(records(&tmp).len(), 6);
    assert!(!ctx.state_dir().join("native-1.json").exists());
    assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&tmp).len(), 6);
}

#[test] fn native_only_tools_fail_without_invented_usage() {
    let hooks = fixtures(); let mut s = Session::default(); let mut events = vec![];
    native(&mut s, &hooks[1], 10, &mut events);
    native(&mut s, &hooks[2], 20, &mut events);
    let mut failure = hooks[3].clone();
    failure["hook_event_name"] = json!("PostToolUseFailure"); failure["error"] = json!("synthetic failure");
    native(&mut s, &failure, 30, &mut events); complete_tools(&mut s, &mut events);
    native(&mut s, &hooks[4], 40, &mut events);
    assert!(matches!(&events[1], Event::Tool { start_ns:20, end_ns:30, status:Some(status), .. } if status=="error"));
    assert!(matches!(events[2], Event::TurnEnd { .. }));
    assert!(!events.iter().any(|event| matches!(event, Event::Chat { .. } | Event::SubagentStart { .. })));
}

#[test] fn delayed_initial_drain_keeps_current_turn_and_original_pane() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    let wire_path = tmp.path.join("wire.jsonl"); std::fs::write(&wire_path, include_str!("fixtures/wire.jsonl")).unwrap();
    let mut state = State::default();
    state.sessions.insert("synthetic-session".into(), Session { wire: Some(wire_path), ..Default::default() });
    ctx.save("native.json", &state).unwrap();
    let hooks = fixtures();
    for (index, payload, ts) in [(0, &hooks[0], 1_000_000_000), (1, &hooks[1], 2_000_000_000),
        (2, &hooks[3], 4_100_000_000), (3, &hooks[4], 5_000_000_000)] {
        ctx.save(&format!("native-{index}.json"), &Pending { payload: payload.clone(), ts,
            metadata: BTreeMap::from([("herdr.pane_id".into(), json!("original-pane"))]),
            run_id: Some("original-run".into()), proxy_active: true, ..Default::default() }).unwrap();
    }
    assert!(!settle(&mut ctx).unwrap());
    let output = records(&tmp); assert_eq!(output.len(), 4); // both model-null chats are suppressed under proxy
    assert!(output.iter().all(|r| r.run_id.as_deref()==Some("original-run") && r.metadata["herdr.pane_id"]=="original-pane"));
    assert!(!ctx.proxy_active); assert!(ctx.run_id.is_none()); assert!(ctx.metadata.is_empty());
}
