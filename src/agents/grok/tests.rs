use super::*;
use crate::{config::Config, paths::Paths, record::{SpanRecord, Usage}, setup::{self as engine, SetupCtx}};

fn fixtures() -> Vec<Value> { serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap() }
fn context(tmp: &crate::tests::Temp, proxy: bool) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
        Config { customer_identifier: "synthetic-host".into(), ..Default::default() }, "grok",
        BTreeMap::new(), Some("synthetic-run".into()), proxy).unwrap()
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|p|
        std::fs::read_to_string(p).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect::<Vec<_>>()
    ).collect()
}

#[test]
fn fixture_to_golden_batch_and_delayed_spawn_join() {
    let mut state = State::default();
    let hooks = fixtures();
    for (i, payload) in hooks.iter().take(5).enumerate() {
        let batch = normalize(payload, &mut state, i as u64 + 1, false).unwrap();
        if i > 0 { assert!(batch.events.is_empty()); }
    }
    let batch = normalize(&hooks[5], &mut state, 6, false).unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/spawn-batch.json")).unwrap();
    assert_eq!(serde_json::to_value(batch).unwrap(), expected);
    assert!(state.subs.is_empty());
}

#[test]
fn fixture_bridge_produces_exact_parent_tree_and_no_usage() {
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp, false);
    let mut state = State::default();
    for (i, payload) in fixtures().iter().enumerate() {
        let batch = normalize(payload, &mut state, i as u64 + 1, false).unwrap();
        bridge::ingest(&mut ctx, batch.clone()).unwrap();
        bridge::ingest(&mut ctx, batch).unwrap();
    }
    let all = records(&tmp);
    assert_eq!(all.len(), 4);
    let trace = "grok:synthetic-session:prompt-1";
    let tool = all.iter().find(|r| r.log_type == "tool").unwrap();
    assert_eq!(tool.parent_key.as_deref(), Some(format!("{trace}:turn").as_str()));
    let sub = all.iter().find(|r| r.metadata.get("subagent") == Some(&json!(true))).unwrap();
    assert_eq!(sub.parent_key.as_deref(), Some(format!("{trace}:tool:spawn-1").as_str()));
    assert_eq!(sub.start_ns, 3);
    assert_eq!(sub.end_ns, 5);
    assert!(all.iter().all(|r| r.agent == "grok" && r.thread_id.as_deref() == Some("synthetic-session") && r.usage == Usage::default()));
}

#[test]
fn transcript_incremental_partial_rotation_and_proxy_suppression() {
    let tmp = crate::tests::Temp::new();
    let raw = tmp.path.join("chat_history.jsonl");
    let update = tmp.path.join("updates.jsonl");
    std::fs::write(&raw, b"").unwrap();
    let mut prompt = fixtures().remove(0);
    prompt["transcriptPath"] = json!(update);
    let mut state = State::default();
    normalize(&prompt, &mut state, 1, false).unwrap();
    std::fs::write(&raw, include_bytes!("fixtures/chat_history.jsonl")).unwrap();
    let mut stop = fixtures().pop().unwrap();
    stop["transcriptPath"] = json!(update);
    let batch = normalize(&stop, &mut state, 10, false).unwrap();
    let chats: Vec<_> = batch.events.iter().filter(|e| matches!(e, Event::Chat { .. })).collect();
    assert_eq!(chats.len(), 2);
    assert!(chats.iter().all(|e| matches!(e, Event::Chat { usage: None, model: Some(model), .. } if model == "grok-build")));
    assert!(normalize(&stop, &mut state, 11, false).unwrap().events.is_empty());
    let mut events = Vec::new();
    std::fs::write(&raw, b"{\"type\":\"assistant\",\"content\":\"late").unwrap();
    transcript(&mut state, 12, false, &mut events).unwrap();
    assert!(events.is_empty());
    std::fs::write(&raw, b"{\"type\":\"assistant\",\"content\":\"late\"}\n").unwrap();
    transcript(&mut state, 13, false, &mut events).unwrap();
    assert_eq!(events.len(), 1);
    std::fs::write(&raw, include_bytes!("fixtures/chat_history.jsonl")).unwrap();
    state.cursor = Cursor::default();
    events.clear();
    transcript(&mut state, 14, true, &mut events).unwrap();
    assert!(events.is_empty());
}

#[test]
fn settle_picks_up_late_final_assistant_and_scrubs_state() {
    let tmp = crate::tests::Temp::new();
    let raw = tmp.path.join("chat_history.jsonl");
    std::fs::write(&raw, b"").unwrap();
    let mut ctx = context(&tmp, false);
    let mut state = State::default();
    let mut prompt = fixtures().remove(0);
    prompt["prompt"] = json!("password=synthetic-secret");
    prompt["transcriptPath"] = json!(tmp.path.join("updates.jsonl"));
    bridge::ingest(&mut ctx, normalize(&prompt, &mut state, 1, false).unwrap()).unwrap();
    let mut stop = fixtures().pop().unwrap();
    stop["transcriptPath"] = prompt["transcriptPath"].clone();
    bridge::ingest(&mut ctx, normalize(&stop, &mut state, 2, false).unwrap()).unwrap();
    ctx.save("native-test.json", &state).unwrap();
    assert!(!std::fs::read_to_string(ctx.state_dir().join("native-test.json")).unwrap().contains("synthetic-secret"));
    std::fs::write(&raw, b"{\"type\":\"assistant\",\"content\":\"late\",\"model_id\":\"grok-build\"}\n").unwrap();
    assert!(settle(&mut ctx).unwrap());
    let all = records(&tmp);
    assert_eq!(all.iter().filter(|r| r.log_type == "chat").count(), 1);
    assert!(all.iter().all(|r| r.usage == Usage::default()));
    settle(&mut ctx).unwrap();
    assert!(!settle(&mut ctx).unwrap());
    assert_eq!(records(&tmp).len(), all.len());
}

#[test]
fn foreign_identity_rejected_and_error_and_clipping_preserved() {
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp, false);
    let claude = json!({"session_id":"foreign","hook_event_name":"Stop","transcript_path":"/tmp/nope"});
    hook(&mut ctx, &serde_json::to_vec(&claude).unwrap()).unwrap();
    assert!(records(&tmp).is_empty());
    let payload = fixtures().remove(0);
    assert!(identity(&payload, Some("synthetic-session"), Some("user_prompt_submit")));
    assert!(!identity(&payload, Some("other"), None));
    assert!(!identity(&payload, None, Some("stop")));
    let mut state = State::default();
    normalize(&payload, &mut state, 1, false).unwrap();
    let failure = json!({"sessionId":"synthetic-session","hookEventName":"post_tool_use_failure","promptId":"prompt-1","toolName":"bash","toolUseId":"failed","toolInput":{"command":"false"},"toolInputTruncated":true,"error":"exit 1","durationMs":1});
    let batch = normalize(&failure, &mut state, 2_000_000, false).unwrap();
    assert!(matches!(&batch.events[0], Event::Tool { status: Some(status), error: Some(error), input: Some(input), .. } if status == "error" && error == "exit 1" && input["truncated"] == true));
}

#[test]
fn setup_is_nested_non_destructive_idempotent_and_removable() {
    let tmp = crate::tests::Temp::new();
    let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(tmp.path.join("project"))).unwrap();
    let plan = setup::plan(&ctx).unwrap();
    let file = tmp.path.join("project/.grok/hooks/herdr-respan.json");
    let herdr = tmp.path.join("project/.grok/hooks/herdr.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let original = json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"herdr-agent-state"}]}]},"other":true});
    std::fs::write(&file, original.to_string()).unwrap();
    std::fs::write(&herdr, b"herdr untouched").unwrap();
    assert_eq!(plan.edits.len(), setup::EVENTS.len());
    assert!(engine::preview(&ctx, "grok", &plan).unwrap().contains("herdr-respan"));
    engine::apply(&ctx, "grok", &plan).unwrap();
    let once = std::fs::read(&file).unwrap();
    engine::apply(&ctx, "grok", &plan).unwrap();
    assert_eq!(once, std::fs::read(&file).unwrap());
    let installed: Value = serde_json::from_slice(&once).unwrap();
    assert_eq!(installed["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(installed["hooks"]["SessionStart"][1]["hooks"][0]["type"], "command");
    engine::remove(&ctx, &["grok".into()]).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(file).unwrap()).unwrap(), original);
    assert_eq!(std::fs::read(herdr).unwrap(), b"herdr untouched");
}

#[test]
fn utc_timestamp_fraction_and_malformed_fallback() {
    assert_eq!(timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(timestamp("1970-01-01T00:00:01.123456789+00:00"), Some(1_123_456_789));
    assert_eq!(timestamp("2000-02-29T00:00:00Z"), Some(951782400000000000));
    assert_eq!(timestamp("invalid"), None);
    assert_eq!(timestamp("1970-13-01T00:00:00Z"), None);
}

#[test]
fn hook_only_chat_and_proxy_are_disjoint() {
    for proxy in [false, true] {
        let mut state = State::default();
        let hooks = fixtures();
        normalize(&hooks[0], &mut state, 1, proxy).unwrap();
        let batch = normalize(&hooks[6], &mut state, 2, proxy).unwrap();
        let chats: Vec<_> = batch.events.iter().filter(|e| matches!(e, Event::Chat { .. })).collect();
        assert_eq!(chats.len(), usize::from(!proxy));
        assert!(chats.iter().all(|event| matches!(event, Event::Chat { model: None, usage: None, .. })));
        assert!(normalize(&hooks[6], &mut state, 3, proxy).unwrap().events.is_empty());
    }
}

#[test]
fn native_hook_subprocess_honors_gate_pause_and_fail_open() {
    use std::io::Write;
    fn invoke(tmp: &crate::tests::Temp, herdr: &str, input: &[u8]) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::fake_hook_child", "--nocapture"])
            .env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", tmp.path.join("home")).env("USERPROFILE", tmp.path.join("home"))
            .env("HOSTNAME", "synthetic-host").env("HERDR_RESPAN_TEST_CHILD", "1")
            .env("HERDR_RESPAN_TEST_AGENT", "grok").env("HERDR_ENV", herdr)
            .env("HERDR_PLUGIN_STATE_DIR", tmp.path.join("state"))
            .env("HERDR_PLUGIN_CONFIG_DIR", tmp.path.join("config"))
            .env("HERDR_WORKSPACE_ID", "synthetic-workspace")
            .current_dir(&tmp.path).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        assert!(child.wait().unwrap().success());
    }
    let tmp = crate::tests::Temp::new();
    let hooks = fixtures();
    for payload in [&hooks[0], &hooks[6]] { invoke(&tmp, "1", &serde_json::to_vec(payload).unwrap()); }
    let all = records(&tmp);
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|r| r.agent == "grok"));
    invoke(&tmp, "1", b"malformed");
    assert_eq!(records(&tmp).len(), 2);
    let outside = crate::tests::Temp::new();
    invoke(&outside, "0", &serde_json::to_vec(&hooks[0]).unwrap());
    assert!(records(&outside).is_empty());
    crate::gate::pause(&outside.path.join("state"), "synthetic-workspace", true).unwrap();
    invoke(&outside, "1", &serde_json::to_vec(&hooks[0]).unwrap());
    assert!(records(&outside).is_empty());
}

#[test]
fn metadata_only_native_state_omits_all_content() {
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp, false);
    ctx.config.content_cap_bytes = 0;
    let mut state = State { input: vec![json!({"content":"private-input"})],
        final_output: Some(json!("private-final")), ..Default::default() };
    state.subs.insert("child".into(), Sub { output: Some(json!("private-child")), ..Default::default() });
    save_state(&ctx, "native-metadata.json", &mut state).unwrap();
    let saved = std::fs::read_to_string(ctx.state_dir().join("native-metadata.json")).unwrap();
    assert!(!saved.contains("private-"));
}
