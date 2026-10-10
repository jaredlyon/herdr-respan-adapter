use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord, setup::{Edit, SetupCtx}};
use std::{io::Write, process::{Command, Stdio}};

fn fixtures() -> Vec<Value> { serde_json::from_str(include_str!("fixtures/events.json")).unwrap() }
fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
        Config { customer_identifier: "synthetic-host".into(), ..Default::default() }, "cursor", Default::default(), None, false).unwrap()
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|path|
        std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
    ).collect()
}

#[test]
fn replies_for_every_event_and_malformed_payload() {
    let reply = SPEC.reply.unwrap();
    for event in EVENTS {
        assert_eq!(reply(&serde_json::to_vec(&json!({"hook_event_name":event})).unwrap()), "{}");
        assert_eq!(reply(format!("{{\"hook_event_name\":\"{event}\",broken").as_bytes()), "{}");
    }
    for payload in [b"".as_slice(), b"not JSON", b"null", b"[]", b"{}", b"{\"hook_event_name\":5}"] {
        assert_eq!(reply(payload), "{}");
    }
    assert_eq!(reply(br#"{"hook_event_name":"beforeSubmitPrompt"}"#), "{\"continue\":true}");
}

#[test]
fn tool_batch_golden_and_saturating_duration() {
    let mut payload = fixtures().remove(0);
    let expected: bridge::Batch = serde_json::from_str(include_str!("fixtures/tool-batch.json")).unwrap();
    let actual = translate(&payload, 10_000_000, None).unwrap().unwrap();
    assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    payload["duration"] = json!(u64::MAX);
    let value = serde_json::to_value(translate(&payload, 1, None).unwrap().unwrap()).unwrap();
    assert_eq!(value["events"][1]["start_ns"], 0);
    assert_eq!(value["events"][1]["end_ns"], 1);
}

#[test]
fn stop_batch_golden_and_unknown_usage() {
    let mut stop = fixtures().pop().unwrap();
    let batch = translate(&stop, 10_000_000, Some(json!("Synthetic answer"))).unwrap().unwrap();
    let golden: bridge::Batch = serde_json::from_str(include_str!("fixtures/stop-batch.json")).unwrap();
    assert_eq!(serde_json::to_value(batch).unwrap(), serde_json::to_value(golden).unwrap());
    for field in ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"] {
        stop.as_object_mut().unwrap().remove(field);
    }
    stop["input_tokens"] = json!(-1); stop["output_tokens"] = json!("100");
    stop["model"] = json!("Auto");
    let value = serde_json::to_value(translate(&stop, 1, None).unwrap().unwrap()).unwrap();
    assert!(value["events"][1]["model"].is_null());
    assert!(value["events"][1]["provider"].is_null());
    for (_, counter) in value["events"][1]["usage"].as_object().unwrap() { assert!(counter.is_null()); }
    stop["model_id"] = json!("synthetic-resolved-model");
    stop["cache_write_tokens"] = json!(0);
    let value = serde_json::to_value(translate(&stop, 1, None).unwrap().unwrap()).unwrap();
    assert_eq!(value["events"][1]["model"], "synthetic-resolved-model");
    assert_eq!(value["events"][1]["usage"]["cache_creation_input_tokens"], 0);
    for status in ["aborted", "error"] {
        stop["status"] = json!(status);
        let value = serde_json::to_value(translate(&stop, 1, None).unwrap().unwrap()).unwrap();
        assert_eq!(value["events"][1]["status"], "error");
        assert_eq!(value["events"][2]["status"], "error");
    }
}

#[test]
fn fixture_ingestion_produces_correlated_tree_and_deduplicates() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    for payload in fixtures() { hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); }
    ctx.finish().unwrap();
    let spans = records(&tmp); assert_eq!(spans.len(), 4);
    let turn = spans.iter().find(|s| s.log_type == "agent").unwrap();
    assert_eq!(turn.span_key, "cursor:synthetic-conversation:synthetic-generation:turn");
    assert_eq!(turn.thread_id.as_deref(), Some("synthetic-conversation"));
    let chat = spans.iter().find(|s| s.log_type == "chat").unwrap();
    assert_eq!(chat.parent_key.as_deref(), Some(turn.span_key.as_str()));
    assert_eq!(chat.output, Some(json!("Synthetic answer")));
    assert_eq!(chat.usage.input_tokens, Some(10129));
    assert_eq!(chat.metadata["cursor.usage_scope"], "generation");
    assert_eq!(chat.metadata["session_path"], "/synthetic/transcript.jsonl");
    assert!(chat.model.is_none()); assert!(chat.provider.is_none());
    assert_eq!(chat.start_ns, chat.end_ns);
    assert_eq!(spans.iter().filter(|s| s.log_type == "tool" && s.status == "error").count(), 1);
    for payload in fixtures() { hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); }
    assert_eq!(records(&tmp).len(), 4);
    let mut stop = fixtures().pop().unwrap(); stop["generation_id"] = json!("second-generation");
    hook(&mut ctx, &serde_json::to_vec(&stop).unwrap()).unwrap();
    assert_eq!(records(&tmp).len(), 6);
    assert!(records(&tmp).iter().any(|s| s.span_key == "cursor:synthetic-conversation:second-generation:turn"));
}

#[test]
fn foreign_and_incomplete_payloads_do_not_emit() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    for payload in [b"not JSON".as_slice(), b"null", br#"{"hook_event_name":"Stop","session_id":"foreign"}"#,
        br#"{"hookEventName":"stop","workspaceRoot":"/foreign"}"#] {
        hook(&mut ctx, payload).unwrap();
    }
    for field in ["conversation_id", "generation_id", "cursor_version", "tool_use_id"] {
        let mut payload = fixtures().remove(0); payload.as_object_mut().unwrap().remove(field);
        assert!(translate(&payload, 1, None).unwrap().is_none());
    }
    let mut grok = fixtures().remove(0);
    assert!(!identify(&grok, true));
    grok["hookEventName"] = json!("postToolUse"); grok["workspaceRoot"] = json!("/foreign");
    assert!(!identify(&grok, false));
    assert_eq!(reply(&serde_json::to_vec(&grok).unwrap()), "{}");
    assert!(records(&tmp).is_empty());
}

#[test]
fn response_buffer_respects_redaction_and_metadata_only() {
    for cap in [0, 64] {
        let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); ctx.config.content_cap_bytes = cap;
        let mut response = fixtures().remove(2); response["text"] = json!("password=synthetic-secret");
        hook(&mut ctx, &serde_json::to_vec(&response).unwrap()).unwrap();
        let buffer = std::fs::read_to_string(ctx.state_dir().join(response_key(&response))).unwrap();
        assert!(!buffer.contains("synthetic-secret"));
        if cap == 0 { assert!(serde_json::from_str::<Value>(&buffer).unwrap()["output"].is_null()); }
        hook(&mut ctx, &serde_json::to_vec(&fixtures().pop().unwrap()).unwrap()).unwrap();
        assert!(!ctx.state_dir().join(response_key(&response)).exists());
    }
}

fn setup_roundtrip(project: bool) {
    let tmp = crate::tests::Temp::new();
    let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let ctx = SetupCtx::new(&paths, tmp.path.join("home"), project.then(|| tmp.path.join("project"))).unwrap();
    let plan = setup::plan(&ctx).unwrap();
    let file = match &plan.edits[0] { Edit::JsonSetIfAbsent { file, .. } => file.clone(), _ => panic!("missing version edit") };
    let expected_root = if project { ctx.project.as_ref().unwrap().join(".cursor") } else { ctx.agent_home("CURSOR_CONFIG_DIR", ".cursor") };
    assert_eq!(file, expected_root.join("hooks.json"));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for original in [json!({"version":1,"hooks":{"sessionStart":[{"command":"herdr-agent-state cursor"}],"stop":[{"command":"foreign observer"}]},"unrelated":true}),
        json!({"hooks":{"stop":[{"command":"foreign observer"}]}})] {
        std::fs::write(&file, original.to_string()).unwrap();
        let plan = setup::plan(&ctx).unwrap();
        let preview = crate::setup::preview(&ctx, "cursor", &plan).unwrap(); assert!(preview.contains("herdr-respan"));
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&file).unwrap()).unwrap(), original);
        crate::setup::apply(&ctx, "cursor", &plan).unwrap();
        let once = std::fs::read(&file).unwrap();
        crate::setup::apply(&ctx, "cursor", &plan).unwrap(); assert_eq!(std::fs::read(&file).unwrap(), once);
        let installed: Value = serde_json::from_slice(&once).unwrap(); assert_eq!(installed["version"], 1);
        for event in EVENTS {
            let entries = installed["hooks"][event].as_array().unwrap();
            let entry = entries.last().unwrap(); assert_eq!(entry.as_object().unwrap().len(), 1);
            assert!(entry["command"].as_str().unwrap().contains("herdr-respan"));
        }
        assert!(installed["hooks"]["beforeSubmitPrompt"].is_null());
        assert!(installed["hooks"]["preToolUse"].is_null());
        assert_eq!(installed["hooks"]["stop"][0], original["hooks"]["stop"][0]);
        crate::setup::remove(&ctx, &["cursor".into()]).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&file).unwrap()).unwrap(), original);
    }
    std::fs::remove_file(&file).unwrap();
    let plan = setup::plan(&ctx).unwrap(); crate::setup::apply(&ctx, "cursor", &plan).unwrap();
    crate::setup::remove(&ctx, &["cursor".into()]).unwrap(); assert!(!file.exists());
    std::fs::write(&file, r#"{"version":2,"hooks":{}}"#).unwrap(); assert!(setup::plan(&ctx).is_err());
}

#[test]
fn setup_preserves_herdr_and_foreign_entries() { setup_roundtrip(false); setup_roundtrip(true); }

#[test]
fn protocol_child() {
    if std::env::var("CURSOR_TEST_CHILD").ok().as_deref() == Some("1") {
        assert_eq!(crate::run_args(vec!["hook".into(), "cursor".into()]), 0);
    } else if std::env::var("CURSOR_TEST_CHILD").ok().as_deref() == Some("setup") {
        setup_roundtrip(false);
    }
}

#[test]
fn dispatcher_reply_survives_gating_failures_and_grok_imports() {
    for mode in ["outside", "paused", "off", "bad-config", "bad-state", "malformed", "GROK_HOOK_EVENT", "GROK_SESSION_ID", "GROK_HOOK_NAME"] {
        let tmp = crate::tests::Temp::new(); let state = tmp.path.join("state"); let config = tmp.path.join("config");
        std::fs::create_dir_all(&config).unwrap();
        if mode == "paused" { crate::gate::pause(&state, "synthetic-workspace", true).unwrap(); }
        if mode == "bad-config" { std::fs::write(config.join("config.toml"), "not toml").unwrap(); }
        if mode == "bad-state" { std::fs::write(&state, "not a directory").unwrap(); }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "agents::cursor::tests::protocol_child", "--nocapture"])
            .env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("SYSTEMROOT", std::env::var_os("SYSTEMROOT").unwrap_or_default())
            .env("CURSOR_TEST_CHILD", "1").env("HOME", tmp.path.join("home")).env("USERPROFILE", tmp.path.join("home"))
            .env("HERDR_ENV", if mode == "outside" {"0"} else {"1"})
            .env("HERDR_RESPAN", if mode == "off" {"off"} else {"on"})
            .env("HERDR_WORKSPACE_ID", "synthetic-workspace")
            .env("HERDR_PLUGIN_STATE_DIR", &state).env("HERDR_PLUGIN_CONFIG_DIR", &config)
            .current_dir(&tmp.path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if mode.starts_with("GROK_") { command.env(mode, "synthetic"); }
        let mut child = command.spawn().unwrap();
        let payload = if mode == "malformed" { b"not JSON".to_vec() } else { serde_json::to_vec(&fixtures()[0]).unwrap() };
        child.stdin.take().unwrap().write_all(&payload).unwrap();
        let result = child.wait_with_output().unwrap(); assert!(result.status.success(), "{mode}: {:?}", result);
        assert!(String::from_utf8(result.stdout).unwrap().contains("{}"), "missing safe reply: {mode}");
        if mode != "bad-state" { assert!(crate::spool::ready(&state).unwrap().is_empty(), "{mode}"); }
    }
}

#[test]
fn relocated_config_directory() {
    let tmp = crate::tests::Temp::new();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "agents::cursor::tests::protocol_child"])
        .env("CURSOR_TEST_CHILD", "setup").env("CURSOR_CONFIG_DIR", tmp.path.join("relocated"))
        .output().unwrap();
    assert!(result.status.success(), "{:?}", result);
}
