use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord, setup::{self, SetupCtx}};
use serde_json::json;

fn fixture(name: &str) -> &'static str {
    match name { "modern" => include_str!("fixtures/modern.jsonl"), "legacy" => include_str!("fixtures/legacy.jsonl"), "child" => include_str!("fixtures/child.jsonl"), _ => unreachable!() }
}
fn parse_fixture(name: &str) -> Batch {
    let lines: Vec<String> = fixture(name).lines().map(str::to_owned).collect(); let mut stream = Stream::default();
    let events = stream.parse(&lines, if name == "child" { Some(("child", "t1")) } else { None }, 0);
    Batch { v: 1, agent: "codex".into(), session_id: "root".into(), session_path: None, cwd: None, agent_version: stream.version, events }
}
fn ctx(tmp: &crate::tests::Temp) -> HookCtx { HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") }, Config::default(), "codex", BTreeMap::new(), None, false).unwrap() }
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|p| std::fs::read_to_string(p).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()).collect()
}
#[test]
fn fixture_bridge_golden() {
    for (name, golden) in [("modern", include_str!("fixtures/modern.batch.json")), ("legacy", include_str!("fixtures/legacy.batch.json")), ("child", include_str!("fixtures/child.batch.json"))] {
        assert_eq!(serde_json::to_value(parse_fixture(name)).unwrap(), serde_json::from_str::<Value>(golden).unwrap(), "{name}");
    }
}
#[test]
fn modern_rollout_spool_and_dedupe() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp); let batch = parse_fixture("modern");
    bridge::ingest(&mut ctx, batch.clone()).unwrap(); bridge::ingest(&mut ctx, batch).unwrap();
    let spans = records(&tmp); assert_eq!(spans.len(), 4);
    let chats: Vec<_> = spans.iter().filter(|s| s.log_type == "chat").collect(); assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].usage.input_tokens, Some(120)); assert_eq!(chats[1].usage.input_tokens, Some(50));
    assert_eq!(chats[0].model.as_deref(), Some("gpt-5-mini")); assert_eq!(chats[0].parent_key.as_deref(), Some("codex:root:t1:turn"));
    assert!(spans.iter().all(|s| s.thread_id.as_deref() == Some("root")));
}
#[test]
fn child_hook_transcript_and_spawn_parent() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp);
    let root = tmp.path.join("rollout-root.jsonl"); let child = tmp.path.join("rollout-child.jsonl");
    let data = fixture("modern").replace("\"exec_command\"", "\"spawn_agent\"").replace("\"/synthetic/repo\"", "\"{\\\"agent_id\\\":\\\"child\\\"}\"");
    std::fs::write(&root, data).unwrap(); std::fs::write(&child, fixture("child")).unwrap();
    let payload = json!({"session_id":"root","transcript_path":root,"cwd":"/synthetic/repo","hook_event_name":"SubagentStop","model":"gpt-5-mini","permission_mode":"default","turn_id":"t1","agent_id":"child","agent_type":"explore","agent_transcript_path":child,"stop_hook_active":false,"last_assistant_message":"Child done"});
    hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap();
    let spans = records(&tmp); assert_eq!(spans.len(), 7);
    let sub = spans.iter().find(|s| s.span_key.ends_with(":sub:child")).unwrap(); assert_eq!(sub.parent_key.as_deref(), Some("codex:root:t1:tool:call-1"));
    let child_chat = spans.iter().find(|s| s.span_key.ends_with(":chat:child-response")).unwrap(); assert_eq!(child_chat.parent_key.as_deref(), Some("codex:root:t1:sub:child")); assert_eq!(child_chat.usage.input_tokens, Some(10));
    assert!(!spans.iter().any(|s| s.span_key.contains("parent-response")));
}
#[test]
fn incremental_tail_and_foreign_rejection() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp); let path = tmp.path.join("rollout-root.jsonl");
    let data = fixture("modern"); let split = data.find("\"type\":\"token_usage_record\"").unwrap(); let split = data[..split].rfind('\n').unwrap() + 1;
    std::fs::write(&path, &data[..split]).unwrap(); let mut state = State::default();
    let (first, _) = tail(&mut state, path.to_str().unwrap(), None, 0, "root").unwrap(); assert!(!first.iter().any(|e| matches!(e, Event::Chat { .. })));
    std::fs::write(&path, data).unwrap(); let (second, _) = tail(&mut state, path.to_str().unwrap(), None, 0, "root").unwrap(); assert_eq!(second.iter().filter(|e| matches!(e, Event::Chat { .. })).count(), 2);
    assert!(tail(&mut state, path.to_str().unwrap(), None, 0, "root").unwrap().0.is_empty());
    let mut payload: Value = serde_json::from_str(include_str!("fixtures/stop.json")).unwrap(); payload["transcript_path"] = json!(tmp.path.join("claude.jsonl"));
    hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); assert!(records(&tmp).is_empty());
    payload["transcript_path"] = json!(path); payload["session_id"] = json!("wrong-session"); hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); assert!(records(&tmp).is_empty());
    assert!(!native(&json!({"session_id":"s","hook_event_name":"Stop","transcript_path":"rollout-fake.jsonl","cwd":"/repo"})));
    assert!(rollout(r"C:\Codex\sessions\rollout-root.jsonl"));
}
#[test]
fn setup_preserves_herdr_and_remove_roundtrip() {
    let tmp = crate::tests::Temp::new(); let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("config") };
    let project = tmp.path.join("project"); let home = project.join(".codex"); std::fs::create_dir_all(&home).unwrap();
    let ctx = SetupCtx::new(&paths, tmp.path.join("home"), Some(project)).unwrap();
    let original = json!({"description":"existing","hooks":{"Stop":[{"hooks":[{"type":"command","command":"herdr-agent-state idle","timeout":10}]}],"Interrupt":[{"hooks":[{"type":"command","command":"herdr-agent-state idle"}]}]}});
    let toml = "# existing config\nmodel = 'gpt-5-mini'\n[features]\nhooks = true\n";
    std::fs::write(home.join("hooks.json"), original.to_string()).unwrap(); std::fs::write(home.join("config.toml"), toml).unwrap();
    let plan = (SPEC.plan)(&ctx).unwrap(); assert!(plan.notices.iter().any(|s| s.contains("/hooks")));
    setup::apply(&ctx, "codex", &plan).unwrap(); let once = std::fs::read(home.join("hooks.json")).unwrap(); setup::apply(&ctx, "codex", &plan).unwrap(); assert_eq!(once, std::fs::read(home.join("hooks.json")).unwrap());
    let installed: Value = serde_json::from_slice(&once).unwrap(); assert_eq!(installed["hooks"]["Stop"][0], original["hooks"]["Stop"][0]); assert_eq!(installed["hooks"]["Stop"].as_array().unwrap().len(), 2);
    setup::remove(&ctx, &["codex".into()]).unwrap(); assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(home.join("hooks.json")).unwrap()).unwrap(), original);
    assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), toml);
    let cmd = crate::agents::common::hookcmd::command(&ctx, "codex", crate::agents::common::hookcmd::Shell::Cmd); assert!(cmd.starts_with('"')); assert!(!cmd.starts_with('&')); assert!(cmd.contains("\"hook\" \"codex\""));
}

#[test]
fn deferred_creation_and_stop_settle() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp); let path = tmp.path.join("rollout-root.jsonl");
    let mut payload: Value = serde_json::from_str(include_str!("fixtures/stop.json")).unwrap(); payload["transcript_path"] = json!(path);
    hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap(); assert!(records(&tmp).is_empty()); assert!(settle(&mut ctx).unwrap());
    let data = fixture("modern"); let last = data[..data.len() - 1].rfind('\n').unwrap() + 1;
    std::fs::write(&path, &data[..last]).unwrap(); assert!(settle(&mut ctx).unwrap()); assert_eq!(records(&tmp).len(), 3);
    std::fs::write(&path, data).unwrap(); assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&tmp).len(), 4);
    assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&tmp).len(), 4);
}

#[test]
fn paginated_completed_tool_and_unknown_usage() {
    let lines: Vec<String> = include_str!("fixtures/paginated.jsonl").lines().map(str::to_owned).collect();
    let mut stream = Stream::default(); let events = stream.parse(&lines, None, 0);
    assert!(events.iter().any(|e| matches!(e, Event::Tool { id, status, output, .. } if id == "paged-call" && status.as_deref() == Some("error") && output.as_ref() == Some(&json!("synthetic failure")))));
    assert!(!events.iter().any(|e| matches!(e, Event::Chat { .. })));
}

#[test]
fn modern_usage_supersedes_legacy_snapshots_before_completion() {
    let mut lines: Vec<String> = fixture("legacy").lines().map(str::to_owned).collect(); let end = lines.pop().unwrap();
    let mut stream = Stream::default(); let early = stream.parse(&lines, None, 0);
    assert!(!early.iter().any(|e| matches!(e, Event::Chat { .. })));
    let modern = json!({"timestamp":"2026-10-09T00:00:05.000Z","type":"token_usage_record","payload":{"thread_id":"root","turn_id":"legacy","response_id":"modern-response","usage":{"input_tokens":7,"output_tokens":3}}}).to_string();
    let late = stream.parse(&[modern, end], None, 0);
    let chats: Vec<_> = late.iter().filter(|e| matches!(e, Event::Chat { .. })).collect(); assert_eq!(chats.len(), 1);
    assert!(matches!(chats[0], Event::Chat { id, usage: Some(usage), .. } if id == "modern-response" && usage.input_tokens == Some(7) && usage.cache_read_input_tokens.is_none()));
}
#[test]
fn model_context_is_turn_specific() {
    let mut stream = Stream::default(); let lines = vec![
        json!({"type":"session_meta","payload":{"id":"root"}}).to_string(),
        json!({"type":"turn_context","payload":{"turn_id":"old","model":"old-model"}}).to_string(),
        json!({"type":"turn_context","payload":{"turn_id":"new","model":"new-model"}}).to_string(),
        json!({"type":"token_usage_record","payload":{"thread_id":"root","turn_id":"old","response_id":"late-old","usage":{"input_tokens":1}}}).to_string(),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"unknown"}}).to_string(),
        json!({"type":"token_usage_record","payload":{"thread_id":"root","turn_id":"unknown","response_id":"unknown-model","usage":{"input_tokens":-1}}}).to_string(),
    ];
    let events = stream.parse(&lines, None, 10);
    assert!(events.iter().any(|e| matches!(e, Event::Chat { id, model, .. } if id == "late-old" && model.as_deref() == Some("old-model"))));
    assert!(events.iter().any(|e| matches!(e, Event::Chat { id, model, usage:Some(usage), start_ns, .. } if id == "unknown-model" && model.is_none() && usage.input_tokens.is_none() && *start_ns == 10)));
}
// Shapes from a live codex-cli 0.162.1 / gpt-6-luna rollout (2026-10-10): the model's code-mode `exec` call runs a
// script whose shell command arrives as an item before the exec output.
#[test]
fn code_mode_inner_command_nests_under_exec_call() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp); let mut stream = Stream::default();
    let lines: Vec<String> = [
        json!({"type":"session_meta","payload":{"id":"root"}}),
        json!({"timestamp":"2026-10-10T21:06:04.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"timestamp":"2026-10-10T21:06:06.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_x","input":"tools.exec_command({cmd:\"echo hra-e2e\"})"}}),
        json!({"timestamp":"2026-10-10T21:06:07.256Z","type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"CommandExecution","id":"exec-1","command":["/bin/bash","-lc","echo hra-e2e"],"aggregated_output":"hra-e2e\n","status":"completed"}}}),
        json!({"timestamp":"2026-10-10T21:06:07.287Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_x","output":"Script completed"}}),
        json!({"timestamp":"2026-10-10T21:06:08.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}),
    ].iter().map(Value::to_string).collect();
    let events = stream.parse(&lines, None, 0);
    bridge::ingest(&mut ctx, Batch { v: 1, agent: "codex".into(), session_id: "root".into(), session_path: None, cwd: None, agent_version: None, events }).unwrap();
    let spans = records(&tmp);
    let tool = |id: &str| spans.iter().find(|s| s.metadata.get("tool_call_id") == Some(&json!(id))).unwrap();
    assert_eq!(tool("call_x").parent_key.as_deref(), Some("codex:root:t1:turn"));
    assert_eq!(tool("exec-1").parent_key.as_deref(), Some(tool("call_x").span_key.as_str()));
    assert!(!tool("exec-1").metadata.contains_key("parent_tool_id") && !tool("exec-1").metadata.contains_key("subagent"));
}

#[test]
fn settle_preserves_proxy_and_pane_context() {
    let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp); let path = tmp.path.join("rollout-root.jsonl");
    ctx.proxy_active = true; ctx.run_id = Some("run-original".into()); ctx.metadata.insert("herdr.pane_id".into(), json!("pane-original"));
    let mut payload: Value = serde_json::from_str(include_str!("fixtures/stop.json")).unwrap(); payload["transcript_path"] = json!(path);
    hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap();
    ctx.proxy_active = false; ctx.run_id = None; ctx.metadata.clear(); std::fs::write(path, fixture("modern")).unwrap();
    assert!(!settle(&mut ctx).unwrap()); let spans = records(&tmp); assert_eq!(spans.len(), 2); assert!(!spans.iter().any(|s| s.log_type == "chat"));
    assert!(spans.iter().all(|s| s.run_id.as_deref() == Some("run-original") && s.metadata.get("herdr.pane_id") == Some(&json!("pane-original"))));
}

#[test]
fn user_provider_base_url_detection_is_read_only() {
    let tmp = crate::tests::Temp::new(); let home = tmp.path.join("codex-home"); let sessions = home.join("sessions/2026/10/09"); std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("rollout-root.jsonl"); let config = home.join("config.toml");
    let text = "openai_base_url = 'https://api.respan.ai/openai/v1'\n"; std::fs::write(&config, text).unwrap(); assert!(respan_provider(&path, Some("openai"))); assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert!(!respan_provider(&path, Some("custom")));
    std::fs::write(&config, "model_provider='custom'\n[model_providers.custom]\nbase_url='https://api.respan.ai/v1'\n").unwrap(); assert!(respan_provider(&path, Some("custom")));
}

#[test]
fn captured_live_hooks_rollouts_exact_usage_and_replay() {
    let hooks: Vec<Value> = include_str!("fixtures/live-hooks.jsonl").lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    for (fixture, prompt, chat_count, tool_count) in [
        (include_str!("fixtures/live-text.jsonl"), "Say the deterministic mock text.", 1, 0),
        (include_str!("fixtures/live-tool.jsonl"), "RUN_TOOL: execute echo hra-e2e then reply.", 2, 1),
    ] {
        let tmp = crate::tests::Temp::new(); let mut ctx = ctx(&tmp);
        let rows: Vec<Value> = fixture.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let session = rows[0]["payload"]["id"].as_str().unwrap();
        let turn = rows.iter().find(|v| v["type"] == "turn_context").unwrap()["payload"]["turn_id"].as_str().unwrap();
        let path = tmp.path.join("rollout-live.jsonl"); std::fs::write(&path, fixture).unwrap();
        let captured: Vec<_> = hooks.iter().filter(|v| v["session_id"] == session).collect();
        assert!(captured.iter().any(|v| v["prompt"] == prompt));
        for _ in 0..2 {
            for payload in &captured {
                let mut payload = (*payload).clone(); payload["transcript_path"] = json!(path);
                hook(&mut ctx, &serde_json::to_vec(&payload).unwrap()).unwrap();
            }
            assert!(!settle(&mut ctx).unwrap());
        }
        let spans = records(&tmp);
        let chats: Vec<_> = spans.iter().filter(|s| s.log_type == "chat").collect();
        let usage: Vec<_> = rows.iter().filter(|v| v["type"] == "token_usage_record").collect();
        assert_eq!(chats.len(), chat_count); assert_eq!(chats.len(), usage.len());
        assert_eq!(spans.iter().filter(|s| s.log_type == "agent").count(), 1);
        let tools: Vec<_> = spans.iter().filter(|s| s.log_type == "tool").collect(); assert_eq!(tools.len(), tool_count);
        let parent = format!("codex:{session}:{turn}:turn");
        for chat in chats {
            assert_eq!(chat.parent_key.as_deref(), Some(parent.as_str()));
            assert_eq!(chat.model.as_deref(), Some("gpt-4.1")); assert_eq!(chat.provider.as_deref(), Some("mock"));
            assert_eq!(chat.usage.input_tokens, Some(11)); assert_eq!(chat.usage.output_tokens, Some(7));
            assert_eq!(chat.usage.cache_read_input_tokens, Some(3)); assert_eq!(chat.usage.reasoning_tokens, Some(0));
            assert_eq!(chat.metadata.get("codex.usage_source"), Some(&json!("token_usage_record")));
            assert!(usage.iter().any(|v| chat.span_key.ends_with(v["payload"]["response_id"].as_str().unwrap())));
        }
        for tool in tools {
            assert_eq!(tool.parent_key.as_deref(), Some(parent.as_str()));
            assert!(tool.input.as_ref().unwrap().to_string().contains("echo hra-e2e"));
            assert!(tool.output.as_ref().unwrap().to_string().contains("hra-e2e"));
        }
    }
}

#[test]
fn metadata_only_scrubs_contended_queue_and_unfinished_rollout_content() {
    let tmp=crate::tests::Temp::new();let mut ctx=ctx(&tmp);ctx.config.content_cap_bytes=0;
    let path=tmp.path.join("rollout-root.jsonl");
    let rows=[json!({"type":"session_meta","payload":{"id":"root"}}),json!({"type":"turn_context","payload":{"turn_id":"t1","model":"mock"}}),json!({"type":"response_item","payload":{"type":"message","role":"user","content":"metadata-private-sentinel"}}),json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"metadata-private-sentinel"}}),json!({"type":"response_item","payload":{"type":"function_call","call_id":"tool","name":"exec","arguments":"metadata-private-sentinel"}})];
    std::fs::write(&path,rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")+"\n").unwrap();
    let p=json!({"session_id":"root","turn_id":"t1","transcript_path":path,"cwd":"/repo","hook_event_name":"UserPromptSubmit","prompt":"metadata-private-sentinel","tool_input":"metadata-private-sentinel","tool_response":"metadata-private-sentinel","last_assistant_message":"metadata-private-sentinel"});
    let lock=ctx.try_lock(&format!("codex-{}",digest("root"))).unwrap().unwrap();hook(&mut ctx,&serde_json::to_vec(&p).unwrap()).unwrap();
    for entry in std::fs::read_dir(ctx.state_dir()).unwrap() {let entry=entry.unwrap();if entry.path().extension().is_some_and(|s|s=="json") {assert!(!std::fs::read_to_string(entry.path()).unwrap().contains("metadata-private-sentinel"));}}
    drop(lock);settle(&mut ctx).unwrap();
    let saved=ctx.state_dir().join(format!("codex-{}.json",digest("root")));assert!(!std::fs::read_to_string(saved).unwrap().contains("metadata-private-sentinel"));
}
