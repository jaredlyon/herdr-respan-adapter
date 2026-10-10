use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord, setup::{self as engine, Edit, SetupCtx}};

fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") }, Config { customer_identifier: "synthetic-host".into(), ..Default::default() }, "copilot", BTreeMap::from([("herdr.pane_id".into(), json!("synthetic-pane"))]), None, false).unwrap()
}
fn fixtures() -> Vec<Value> { serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap() }
fn apply_hook(ctx: &mut HookCtx, fixture: &Value) { handle(ctx, &fixture["payload"], fixture["event"].as_str()).unwrap(); }
fn records(ctx: &HookCtx) -> Vec<SpanRecord> {
    crate::spool::ready(ctx.state_dir().parent().unwrap().parent().unwrap()).unwrap().iter().flat_map(|path| std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect::<Vec<_>>()).collect()
}
#[test]
fn documented_otel_attributes_match_golden_batch() {
    let value: Value = serde_json::from_str(include_str!("fixtures/otel.jsonl")).unwrap();
    let batch = Batch { v: 1, agent: "copilot".into(), session_id: "synthetic-session".into(), session_path: None, cwd: None, agent_version: None, events: otel::requests(&value).iter().map(|r| r.event("hook-1700000000000000000")).collect() };
    assert_eq!(serde_json::to_value(batch).unwrap(), serde_json::from_str::<Value>(include_str!("fixtures/chat-batch.json")).unwrap());
}
#[test]
fn native_requests_replace_fallback_and_preserve_tree() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    apply_hook(&mut ctx, &fixtures[0]); apply_hook(&mut ctx, &fixtures[1]);
    std::fs::write(ctx.state_dir().join("otel.jsonl"), include_str!("fixtures/otel.jsonl")).unwrap();
    apply_hook(&mut ctx, &fixtures[2]); assert!(!settle(&mut ctx).unwrap());
    let all = records(&ctx); assert_eq!(all.len(), 4);
    let chats: Vec<_> = all.iter().filter(|r| r.log_type == "chat").collect(); assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].usage.input_tokens, Some(120)); assert_eq!(chats[1].usage.input_tokens, None);
    assert!(chats.iter().all(|r| r.parent_key.as_deref() == Some("copilot:synthetic-session:hook-1700000000000000000:turn")));
    assert!(all.iter().all(|r| r.thread_id.as_deref() == Some("synthetic-session")));
    assert!(!all.iter().any(|r| r.metadata.contains_key("estimated_cost_usd")));
    apply_hook(&mut ctx, &fixtures[2]); assert_eq!(records(&ctx).len(), 4);
}
#[test]
fn late_export_waits_and_settles_once() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    apply_hook(&mut ctx, &fixtures[0]); std::fs::write(ctx.state_dir().join("otel.jsonl"), "").unwrap();
    apply_hook(&mut ctx, &fixtures[2]); assert!(records(&ctx).is_empty()); assert!(settle(&mut ctx).unwrap());
    std::fs::write(ctx.state_dir().join("otel.jsonl"), include_str!("fixtures/otel.jsonl")).unwrap();
    ctx.metadata.clear(); ctx.run_id = None;
    assert!(!settle(&mut ctx).unwrap()); assert!(!settle(&mut ctx).unwrap());
    let all = records(&ctx); assert_eq!(all.len(), 3); assert_eq!(all[0].metadata.get("herdr.pane_id"), Some(&json!("synthetic-pane")));
    assert_eq!(all.iter().filter(|r| r.log_type == "chat").count(), 2);
}
#[test]
fn unavailable_exporter_falls_back_on_third_settle() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    apply_hook(&mut ctx, &fixtures[0]); std::fs::write(ctx.state_dir().join("otel.jsonl"), "").unwrap(); apply_hook(&mut ctx, &fixtures[2]);
    assert!(settle(&mut ctx).unwrap()); assert!(settle(&mut ctx).unwrap()); assert!(!settle(&mut ctx).unwrap());
    let all = records(&ctx); assert_eq!(all.len(), 2); let chat = all.iter().find(|r| r.log_type == "chat").unwrap();
    assert_eq!(chat.usage, crate::record::Usage::default()); assert!(chat.model.is_none());
    std::fs::write(ctx.state_dir().join("otel.jsonl"), include_str!("fixtures/otel.jsonl")).unwrap(); assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&ctx).len(), 2);
}
#[test]
fn compatible_payloads_and_foreign_rejection() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    for fixture in &fixtures[3..] { apply_hook(&mut ctx, fixture); }
    let all = records(&ctx); assert_eq!(all.len(), 3);
    assert_eq!(all.iter().find(|r| r.log_type == "tool").unwrap().status, "error");
    for foreign in [json!({"session_id":"claude","hook_event_name":"UserPromptSubmit","cwd":"/repo","prompt":"foreign"}), json!({"sessionId":"grok","timestamp":1700000000000u64,"cwd":"/repo","workspaceRoot":"/repo","hookEventName":"UserPromptSubmit"}), json!({"conversation_id":"cursor","hook_event_name":"stop"})] {
        handle(&mut ctx, &foreign, None).unwrap();
    }
    assert_eq!(records(&ctx).len(), 3);
}
#[test]
fn metadata_only_never_persists_content() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); ctx.config.content_cap_bytes = 0;
    handle(&mut ctx, &json!({"sessionId":"metadata-session","timestamp":1700000000000u64,"cwd":"/repo","prompt":"private prompt"}), Some("userPromptSubmitted")).unwrap();
    assert!(!std::fs::read_to_string(ctx.state_dir().join(state_key("metadata-session"))).unwrap().contains("private prompt"));
}
#[test]
fn sdk_span_shape_and_incomplete_rows() {
    let value = json!({"span":{"spanContext":{"spanId":"sdk-span"},"startTime":[1700000000u64,100],"endTime":[1700000000u64,200],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"s","github.copilot.turn_id":"t","gen_ai.request.model":"m","gen_ai.usage.input_tokens":5,"gen_ai.input.messages":"never persisted"},"status":{"code":2,"message":"failed"}}});
    let requests = otel::requests(&value); assert_eq!(requests.len(), 1);
    let serialized = serde_json::to_value(requests[0].event("t")).unwrap(); assert_eq!(serialized["usage"]["input_tokens"], 5); assert_eq!(serialized["status"], "error"); assert!(serialized["input"].is_null());
    assert!(!serde_json::to_string(&requests).unwrap().contains("never persisted"));
    assert!(otel::requests(&json!({"attributes":{"gen_ai.operation.name":"chat"}})).is_empty());
    assert_eq!(otel::hook_time(&json!("2023-11-14T22:13:20.123Z")), Some(1700000000123000000));
    assert_eq!(otel::hook_time(&json!("2023-11-14T23:13:20.123+01:00")), Some(1700000000123000000));
    assert!(otel::hook_time(&json!("2023-02-31T22:13:20Z")).is_none());
}
fn setup_context(tmp: &crate::tests::Temp) -> SetupCtx {
    SetupCtx::new(&Paths { state: tmp.path.join("state"), config: tmp.path.join("config") }, tmp.path.join("home"), Some(tmp.path.join("project"))).unwrap()
}
#[test]
fn offer_decline_accept_and_remove_preserve_foreign_settings_profiles() {
    for accepted in [false, true] {
        let tmp = crate::tests::Temp::new(); let ctx = setup_context(&tmp);
        let file = ctx.project.as_ref().unwrap().join(".github/copilot/settings.local.json"); std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = json!({"hooks":{"sessionStart":[{"type":"command","bash":"herdr-agent-state.sh","timeoutSec":10}]},"theme":"dark"}); std::fs::write(&file, original.to_string()).unwrap();
        std::fs::create_dir_all(&ctx.home).unwrap(); let profile = ctx.home.join(".bashrc"); let text = "# existing\nexport EDITOR=vi"; std::fs::write(&profile, text).unwrap();
        let mut plan = setup::plan(&ctx).unwrap();
        // Only fixture paths are eligible for this test; never use inherited real profile relocation.
        for offer in &mut plan.offers { offer.edits.retain(|e| matches!(e, Edit::TextArg {file,..} if file.starts_with(&ctx.home))); }
        assert_eq!(plan.offers[0].id, "copilot-otel-profile"); assert!(!plan.notices.is_empty());
        engine::accept_offers(&mut plan, &[], true, |_| Ok(accepted)).unwrap(); engine::apply(&ctx, "copilot", &plan).unwrap(); engine::apply(&ctx, "copilot", &plan).unwrap();
        let installed: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(installed["hooks"]["sessionStart"][0], original["hooks"]["sessionStart"][0]);
        assert!(installed["hooks"]["sessionStart"][1]["bash"].as_str().unwrap().contains("herdr-respan"));
        assert_eq!(installed["hooks"]["sessionStart"][1]["timeoutSec"], 10);
        let changed = std::fs::read_to_string(&profile).unwrap(); assert_eq!(changed.contains("COPILOT_OTEL_FILE_EXPORTER_PATH"), accepted);
        engine::remove(&ctx, &["copilot".into()]).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&file).unwrap()).unwrap(), original); assert_eq!(std::fs::read_to_string(&profile).unwrap(), text);
    }
}
#[test]
fn exact_profile_revert_all_shells_newlines_and_path_quoting() {
    let tmp = crate::tests::Temp::new(); let ctx = setup_context(&tmp); std::fs::create_dir_all(&ctx.home).unwrap();
    for relative in [".bashrc", ".zshrc", ".config/fish/config.fish", ".config/powershell/Microsoft.PowerShell_profile.ps1"] {
        let path = ctx.home.join(relative); std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(path, "# existing").unwrap();
    }
    let plan = setup::plan(&ctx).unwrap();
    for edit in &plan.offers[0].edits {
        let Edit::TextArg { file, apply, revert, .. } = edit else { panic!("expected profile text edit") };
        if !file.starts_with(&ctx.home) { continue; }
        for text in ["", "# existing", "# existing\n", "# existing\r\n"] {
            let arg = "/synthetic/path with 'quotes'/otel.jsonl";
            let applied = apply(text, arg).unwrap(); assert!(applied.contains("HERDR_ENV")); assert!(applied.contains("COPILOT_OTEL_FILE_EXPORTER_PATH")); assert!(!applied.contains("CAPTURE_MESSAGE_CONTENT"));
            assert_eq!(apply(&applied, arg).unwrap(), applied); assert_eq!(revert(&applied, arg).unwrap(), text);
            let with_foreign = format!("{applied}# later foreign line\n"); assert_eq!(revert(&with_foreign, arg).unwrap(), format!("{text}# later foreign line\n"));
            assert!(revert(&applied.replace("otel.jsonl", "changed.jsonl"), arg).is_err());
        }
    }
}

#[test]
fn contested_hook_queues_without_losing_or_leaking_content() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    let lock = ctx.lock("copilot").unwrap();
    apply_hook(&mut ctx, &fixtures[0]); apply_hook(&mut ctx, &fixtures[1]); apply_hook(&mut ctx, &fixtures[2]);
    assert!(records(&ctx).is_empty()); drop(lock);
    assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&ctx).len(), 3);
    let lock = ctx.lock("copilot").unwrap(); ctx.config.content_cap_bytes = 0;
    handle(&mut ctx, &json!({"sessionId":"scrubbed-queue","timestamp":1700000000000u64,"cwd":"/repo","prompt":"private queued prompt"}), Some("userPromptSubmitted")).unwrap();
    for entry in std::fs::read_dir(ctx.state_dir()).unwrap() { let entry = entry.unwrap(); if entry.file_name().to_string_lossy().starts_with("queue-") { assert!(!std::fs::read_to_string(entry.path()).unwrap().contains("private queued prompt")); } }
    drop(lock); settle(&mut ctx).unwrap();
}
#[test]
fn jsonc_fails_safely_with_exact_manual_entries() {
    let tmp = crate::tests::Temp::new(); let ctx = setup_context(&tmp);
    let path = ctx.project.as_ref().unwrap().join(".github/copilot/settings.local.json"); std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "{ // existing comment\n \"theme\": \"dark\",\n}";
    std::fs::write(&path, original).unwrap();
    let error = setup::plan(&ctx).err().unwrap().to_string();
    assert!(error.contains("No files changed")); assert!(error.contains("HERDR_RESPAN_COPILOT_EVENT")); assert!(error.contains("postToolUseFailure")); assert!(error.contains("timeoutSec"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}
#[test]
fn offer_explicit_accept_without_prompt() {
    let tmp = crate::tests::Temp::new(); let ctx = setup_context(&tmp); std::fs::create_dir_all(&ctx.home).unwrap();
    let path = ctx.home.join(".bashrc"); std::fs::write(&path, "# existing\n").unwrap();
    let mut plan = setup::plan(&ctx).unwrap();
    for offer in &mut plan.offers { offer.edits.retain(|e| matches!(e, Edit::TextArg {file,..} if file.starts_with(&ctx.home))); }
    engine::accept_offers(&mut plan, &["copilot-otel-profile".into()], false, |_| panic!("explicit acceptance must not prompt")).unwrap();
    engine::apply(&ctx, "copilot", &plan).unwrap(); assert!(std::fs::read_to_string(&path).unwrap().contains("COPILOT_OTEL_FILE_EXPORTER_PATH"));
    engine::remove(&ctx, &["copilot".into()]).unwrap(); assert_eq!(std::fs::read_to_string(path).unwrap(), "# existing\n");
}

#[test]
fn sdk_exporter_lines_match_same_golden() {
    let events = include_str!("fixtures/otel-sdk.jsonl").lines().flat_map(|line| otel::requests(&serde_json::from_str::<Value>(line).unwrap())).map(|r| r.event("hook-1700000000000000000")).collect();
    let batch = Batch { v: 1, agent: "copilot".into(), session_id: "synthetic-session".into(), session_path: None, cwd: None, agent_version: None, events };
    assert_eq!(serde_json::to_value(batch).unwrap(), serde_json::from_str::<Value>(include_str!("fixtures/chat-batch.json")).unwrap());
}

#[test]
fn shared_exporter_conversations_have_independent_cursors() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp); let fixtures = fixtures();
    apply_hook(&mut ctx, &fixtures[0]); apply_hook(&mut ctx, &fixtures[3]);
    let original = include_str!("fixtures/otel.jsonl");
    std::fs::write(ctx.state_dir().join("otel.jsonl"), format!("{original}{}", original.replace("synthetic-session", "synthetic-compatible"))).unwrap();
    apply_hook(&mut ctx, &fixtures[2]); apply_hook(&mut ctx, &fixtures[5]);
    assert!(!settle(&mut ctx).unwrap()); let all = records(&ctx); assert_eq!(all.len(), 6);
    for session in ["synthetic-session", "synthetic-compatible"] { assert_eq!(all.iter().filter(|r| r.log_type == "chat" && r.thread_id.as_deref() == Some(session)).count(), 2); }
}
