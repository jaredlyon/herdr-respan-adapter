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

#[test]
fn live_mock_hooks_replay_native_and_proxy_without_duplicate_shutdown_chats() {
    for proxy in [false, true] {
        let tmp = crate::tests::Temp::new();
        let mut ctx = context(&tmp, proxy);
        let mut state = State::default();
        let hooks: Vec<Value> = serde_json::from_str(include_str!("fixtures/live-hooks.json")).unwrap();
        let rows: Vec<_> = include_str!("fixtures/live-chat_history.jsonl").lines().collect();
        let raw = tmp.path.join("chat_history.jsonl");
        std::fs::write(&raw, b"").unwrap();
        for mut payload in hooks {
            assert!(identity(&payload, Some("live-tool-session"), None));
            if payload.get("transcriptPath").is_some() {
                payload["transcriptPath"] = json!(tmp.path.join("updates.jsonl"));
            }
            match text(&payload, "hookEventName") {
                Some("pre_tool_use") => std::fs::write(&raw, format!("{}\n", rows[..2].join("\n"))).unwrap(),
                Some("stop") if text(&payload, "reason") == Some("end_turn") =>
                    std::fs::write(&raw, include_bytes!("fixtures/live-chat_history.jsonl")).unwrap(),
                _ => {}
            }
            let now = timestamp(text(&payload, "timestamp").unwrap()).unwrap();
            let batch = normalize(&payload, &mut state, now, proxy).unwrap();
            bridge::ingest(&mut ctx, batch.clone()).unwrap();
            bridge::ingest(&mut ctx, batch).unwrap();
        }
        let all = records(&tmp);
        assert_eq!(all.len(), if proxy { 2 } else { 4 });
        assert!(all.iter().all(|r| r.usage == Usage::default()));
        let turn = all.iter().find(|r| r.log_type == "agent").unwrap();
        let tool = all.iter().find(|r| r.log_type == "tool").unwrap();
        assert_eq!(tool.name, "run_terminal_command");
        assert_eq!(tool.parent_key.as_deref(), Some(turn.span_key.as_str()));
        assert!(tool.end_ns > tool.start_ns);
        assert_eq!(tool.input.as_ref().unwrap()["value"]["command"], "echo hra-e2e");
        assert_eq!(tool.output.as_ref().unwrap()["value"]["exit_code"], 0);
        assert!(tool.output.as_ref().unwrap()["value"]["output_for_prompt"].as_str().unwrap().contains("hra-e2e"));
        let chats: Vec<_> = all.iter().filter(|r| r.log_type == "chat").collect();
        assert_eq!(chats.len(), if proxy { 0 } else { 2 });
        for chat in chats {
            assert_eq!(chat.parent_key.as_deref(), Some(turn.span_key.as_str()));
            assert_eq!(chat.model.as_deref(), Some("mock-model"));
        }
        let mut late = Vec::new();
        transcript(&mut state, turn.end_ns, proxy, &mut late).unwrap();
        assert!(late.is_empty());
    }
}

#[test]
fn held_native_lock_queues_without_blocking_and_settle_keeps_native_order() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp,false);
    ctx.metadata.insert("herdr.pane_id".into(),json!("original-pane"));ctx.run_id=Some("original-run".into());
    let key=format!("native-{}.json",digest(b"synthetic-session"));let held=ctx.try_lock(&format!("{key}.lock")).unwrap().unwrap();
    let start=std::time::Instant::now();
    for (i,mut payload) in fixtures().into_iter().enumerate() {
        payload["timestamp"]=json!(format!("1970-01-01T00:00:0{i}Z"));
        hook(&mut ctx,&serde_json::to_vec(&payload).unwrap()).unwrap();
    }
    assert!(start.elapsed()<std::time::Duration::from_secs(1));assert_eq!(queued(&ctx,Some(&key)).unwrap().len(),7);
    assert!(ctx.state_dir().join("settle-pending").exists());assert!(records(&tmp).is_empty());
    drop(held);ctx.metadata.insert("herdr.pane_id".into(),json!("settler-pane"));ctx.run_id=Some("settler-run".into());ctx.proxy_active=true;
    assert!(!settle(&mut ctx).unwrap());assert!(queued(&ctx,None).unwrap().is_empty());
    let all=records(&tmp);assert_eq!(all.len(),4);
    let tool=all.iter().find(|r|r.log_type=="tool").unwrap();assert_eq!((tool.start_ns,tool.end_ns),(1_000_000_000,5_000_000_000));
    let sub=all.iter().find(|r|r.span_key.ends_with(":sub:child-1")).unwrap();assert_eq!((sub.start_ns,sub.end_ns),(2_000_000_000,4_000_000_000));
    assert_eq!(sub.parent_key.as_deref(),Some("grok:synthetic-session:prompt-1:tool:spawn-1"));
    assert!(all.iter().all(|r|r.run_id.as_deref()==Some("original-run")&&r.metadata["herdr.pane_id"]=="original-pane"));
    assert_eq!(ctx.run_id.as_deref(),Some("settler-run"));assert!(ctx.proxy_active);assert_eq!(ctx.metadata["herdr.pane_id"],"settler-pane");
}

#[test]
fn metadata_only_contended_queue_preserves_spawn_ids_not_content_aliases() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp,false);ctx.config.content_cap_bytes=0;
    let key=format!("native-{}.json",digest(b"synthetic-session"));let held=ctx.try_lock(&format!("{key}.lock")).unwrap().unwrap();
    for mut payload in fixtures() {
        for name in ["prompt","lastAssistantMessage","error","errorDetails","tool_input","tool_response","tool_result","last_assistant_message","input","output","text"] {payload[name]=json!("disk-content-sentinel");}
        if payload.get("toolInput").is_some() {payload["toolInput"]=json!({"task_id":"child-1","prompt":"disk-content-sentinel"});}
        if payload.get("toolResult").is_some() {payload["toolResult"]=json!({"output":{"subagent_id":"child-1","output":"disk-content-sentinel"}});}
        hook(&mut ctx,&serde_json::to_vec(&payload).unwrap()).unwrap();
    }
    let files=queued(&ctx,Some(&key)).unwrap();assert_eq!(files.len(),7);
    for file in files {let saved=std::fs::read_to_string(file).unwrap();assert!(!saved.contains("disk-content-sentinel"));assert!(saved.contains("prompt-1"));}
    drop(held);assert!(!settle(&mut ctx).unwrap());
    let all=records(&tmp);assert_eq!(all.len(),4);assert!(all.iter().all(|r|r.input.is_none()&&r.output.is_none()));
    assert_eq!(all.iter().find(|r|r.span_key.ends_with(":sub:child-1")).unwrap().parent_key.as_deref(),Some("grok:synthetic-session:prompt-1:tool:spawn-1"));
    fn inspect(path:&Path) {for entry in std::fs::read_dir(path).unwrap() {let path=entry.unwrap().path();if path.is_dir() {inspect(&path);} else {assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("disk-content-sentinel"));}}}
    inspect(&tmp.path.join("state"));
}

#[test]
fn queued_tail_settlement_restores_original_context_and_proxy_mode() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp,false);let raw=tmp.path.join("chat_history.jsonl");std::fs::write(&raw,"").unwrap();
    ctx.metadata.insert("herdr.pane_id".into(),json!("original-pane"));ctx.run_id=Some("original-run".into());
    let key=format!("native-{}.json",digest(b"synthetic-session"));let held=ctx.try_lock(&format!("{key}.lock")).unwrap().unwrap();
    let mut prompt=fixtures().remove(0);prompt["transcriptPath"]=json!(tmp.path.join("updates.jsonl"));
    let mut stop=fixtures().pop().unwrap();stop["transcriptPath"]=prompt["transcriptPath"].clone();
    for payload in [prompt,stop] {hook(&mut ctx,&serde_json::to_vec(&payload).unwrap()).unwrap();}
    drop(held);ctx.metadata.clear();ctx.run_id=None;ctx.proxy_active=true;
    assert!(settle(&mut ctx).unwrap());
    std::fs::write(&raw,format!("{}\n",json!({"type":"assistant","content":"late","model_id":"grok-build"}))).unwrap();
    settle(&mut ctx).unwrap();
    let all=records(&tmp);let chat=all.iter().find(|r|r.log_type=="chat").unwrap();
    assert_eq!(chat.run_id.as_deref(),Some("original-run"));assert_eq!(chat.metadata["herdr.pane_id"],"original-pane");
    assert!(ctx.run_id.is_none());assert!(ctx.proxy_active);assert!(ctx.metadata.is_empty());
}

#[test]
fn full_content_queue_caps_before_save_without_losing_spawn_identity() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp,false);ctx.config.content_cap_bytes=32;
    let key=format!("native-{}.json",digest(b"synthetic-session"));let held=ctx.try_lock(&format!("{key}.lock")).unwrap().unwrap();
    for mut payload in fixtures() {
        if payload.get("prompt").is_some() {payload["prompt"]=json!("password=queue-secret");}
        if payload.get("toolResult").is_some() {payload["toolResult"]["output"]["output"]=json!("password=queue-secret ".repeat(100));}
        hook(&mut ctx,&serde_json::to_vec(&payload).unwrap()).unwrap();
    }
    for file in queued(&ctx,Some(&key)).unwrap() {assert!(!std::fs::read_to_string(file).unwrap().contains("queue-secret"));}
    drop(held);settle(&mut ctx).unwrap();
    let all=records(&tmp);assert_eq!(all.iter().find(|r|r.span_key.ends_with(":sub:child-1")).unwrap().parent_key.as_deref(),Some("grok:synthetic-session:prompt-1:tool:spawn-1"));
}
