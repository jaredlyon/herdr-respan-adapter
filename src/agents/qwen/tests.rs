use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord};
use std::{collections::BTreeMap, io::Write};

fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths {state:tmp.path.join("state"),config:tmp.path.join("config")},Config {customer_identifier:"synthetic".into(),..Default::default()},"qwen",BTreeMap::new(),None,false).unwrap()
}
fn records(ctx: &HookCtx) -> Vec<SpanRecord> {
    crate::spool::ready(ctx.state_dir().parent().unwrap().parent().unwrap()).unwrap().iter().flat_map(|p| {
        std::fs::read_to_string(p).unwrap().lines().map(|line|serde_json::from_str(line).unwrap()).collect::<Vec<_>>()
    }).collect()
}
fn project(mut records: Vec<SpanRecord>) -> Value {
    records.sort_by(|a,b|a.span_key.cmp(&b.span_key));
    json!(records.iter().map(|r|json!({"key":r.span_key,"parent":r.parent_key,"trace":r.trace_key,"thread":r.thread_id,"type":r.log_type,"name":r.name,"start":r.start_ns,"end":r.end_ns,"status":r.status,"model":r.model,"usage":r.usage,"input":r.input,"output":r.output})).collect::<Vec<_>>())
}
fn payload(event: &str, ts: &str, path: &Path) -> Value {
    json!({"session_id":"s","transcript_path":path,"cwd":"/synthetic/repo","hook_event_name":event,"timestamp":ts,"prompt_id":"t1","permission_mode":"default"})
}

#[test]
fn fixture_golden_batch_matches_end_to_end_tree_and_dedupes() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
    let path=tmp.path.join("transcript.jsonl");let child=tmp.path.join("agent-Explorer-test.jsonl");
    std::fs::write(&path,"").unwrap();
    std::fs::write(&child,include_str!("fixtures/agent-Explorer-test.jsonl")).unwrap();
    std::fs::write(child.with_extension("meta.json"),include_str!("fixtures/agent-Explorer-test.meta.json")).unwrap();
    let hooks:Vec<Value>=serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap();
    for (i, mut p) in hooks.into_iter().enumerate() {
        p["transcript_path"]=json!(path);
        if p.get("agent_transcript_path").is_some() {p["agent_transcript_path"]=json!(child);}
        if i==2 {std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();}
        process(&mut ctx,&p).unwrap();
    }
    let actual=records(&ctx);assert_eq!(actual.len(),6);
    let root=actual.iter().find(|r|r.span_key.ends_with(":chat:a1")).unwrap();
    assert_eq!(root.usage.reasoning_tokens,Some(2));
    assert_eq!(root.metadata["qwen.reasoning_usage_source"],"provider_or_text_estimate");
    let child_span=actual.iter().find(|r|r.span_key.ends_with(":sub:Explorer-test")).unwrap();
    assert_eq!(child_span.parent_key.as_deref(),Some("qwen:s:t1:tool:spawn"));
    let golden=crate::tests::Temp::new();let mut expected_ctx=context(&golden);
    let batch:Batch=serde_json::from_str(include_str!("fixtures/expected-batch.json")).unwrap();
    bridge::ingest(&mut expected_ctx,batch).unwrap();
    assert_eq!(project(actual.clone()),project(records(&expected_ctx)));
    let mut stop=payload("Stop","1970-01-01T00:00:06.000Z",&path);stop["input_tokens"]=json!(999);
    process(&mut ctx,&stop).unwrap();assert_eq!(records(&ctx).len(),actual.len());
    assert_eq!(actual.iter().filter_map(|r|r.usage.input_tokens).sum::<u64>(),53);
}

#[test]
fn partial_transcript_tail_rotation_and_null_usage() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("transcript.jsonl");
    std::fs::write(&path,"").unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    let first=include_str!("fixtures/transcript.jsonl").lines().nth(1).unwrap();
    std::fs::write(&path,&first.as_bytes()[..first.len()-1]).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    assert_eq!(records(&ctx).len(),1);
    let mut file=std::fs::OpenOptions::new().append(true).open(&path).unwrap();file.write_all(b"}\n").unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    assert_eq!(records(&ctx).len(),2);
    let mut r:Value=serde_json::from_str(first).unwrap();r["uuid"]=json!("unknown");r.as_object_mut().unwrap().remove("usageMetadata");
    let replacement=tmp.path.join("replacement");std::fs::write(&replacement,format!("{r}\n")).unwrap();std::fs::rename(replacement,&path).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    let all=records(&ctx);assert_eq!(all.len(),3);assert_eq!(all.iter().find(|r|r.span_key.ends_with(":chat:unknown")).unwrap().usage,Usage::default());
}

#[test]
fn resume_skips_history_and_foreign_payloads_are_noops() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("transcript.jsonl");
    std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();
    process(&mut ctx,&payload("SessionStart","1970-01-01T00:00:00.000Z",&path)).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    assert!(records(&ctx).is_empty());
    let mut foreign=payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path);
    foreign.as_object_mut().unwrap().remove("timestamp");process(&mut ctx,&foreign).unwrap();
    foreign["timestamp"]=json!("1970-01-01T00:00:01.000Z");foreign["llm_response"]=json!({});process(&mut ctx,&foreign).unwrap();
    foreign.as_object_mut().unwrap().remove("llm_response");foreign["permission_mode"]=json!("bypassPermissions");process(&mut ctx,&foreign).unwrap();
    assert!(records(&ctx).is_empty());
    assert!(!ctx.state_dir().join(key("session","s")).exists());
}

#[test]
fn tool_failure_error_turn_and_sidecar_identity() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("missing.jsonl");
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    let mut p=payload("PostToolUseFailure","1970-01-01T00:00:02.000Z",&path);
    p["tool_use_id"]=json!("failed");p["tool_name"]=json!("shell");p["error"]=json!("synthetic failure");p["duration_ms"]=json!(100);
    process(&mut ctx,&p).unwrap();
    let mut end=payload("StopFailure","1970-01-01T00:00:03.000Z",&path);end["error"]=json!("server_error");
    process(&mut ctx,&end).unwrap();
    let all=records(&ctx);assert_eq!(all.len(),2);assert!(all.iter().all(|r|r.status=="error"));
    assert_eq!(all.iter().find(|r|r.log_type=="tool").unwrap().start_ns,1_900_000_000);
    let child=tmp.path.join("agent-test.jsonl");
    assert_eq!(parent_tool(&child,"s","Explorer-test"),None);
    std::fs::write(child.with_extension("meta.json"),include_str!("fixtures/agent-Explorer-test.meta.json")).unwrap();
    assert_eq!(parent_tool(&child,"s","Explorer-test"),Some("spawn".into()));
    assert_eq!(parent_tool(&child,"foreign","Explorer-test"),None);
    assert_eq!(parent_tool(&child,"s","foreign"),None);
}

#[test]
fn timestamps_validate_calendar_and_preserve_fraction() {
    assert_eq!(timestamp_ns("1970-01-01T00:00:01.123Z"),Some(1_123_000_000));
    assert_eq!(timestamp_ns("2000-03-01T00:00:00Z"),Some(951_868_800_000_000_000));
    for invalid in ["bad","éééééééééééééééééééé","2025-02-29T00:00:00Z","2026-01-01T25:00:00Z","1970-01-01T00:00:00.Z","1970-01-01T00:00:00+00:00"] {assert_eq!(timestamp_ns(invalid),None);}
}

#[test]
fn late_final_record_settles_without_replaying_other_chats() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("transcript.jsonl");
    std::fs::write(&path,"").unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    assert_eq!(records(&ctx).len(),1);
    std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();
    assert!(settle(&mut ctx).unwrap());
    assert_eq!(records(&ctx).len(),3);
    assert!(settle(&mut ctx).unwrap());assert_eq!(records(&ctx).len(),3);
    let tail_key=key("tail",&path.to_string_lossy());let mut tail:Tail=ctx.load(&tail_key).unwrap();tail.until_ns=0;ctx.save(&tail_key,&tail).unwrap();
    assert!(!settle(&mut ctx).unwrap());assert!(!ctx.state_dir().join(tail_key).exists());
}

#[test]
fn proxy_suppresses_even_usage_only_unknown_model_children() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.proxy_active=true;
    let path=tmp.path.join("transcript.jsonl");std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    let child=tmp.path.join("child.jsonl");std::fs::write(&child,include_str!("fixtures/agent-Explorer-test.jsonl")).unwrap();
    let mut batch=Batch {v:1,agent:"qwen".into(),session_id:"s".into(),session_path:None,cwd:None,agent_version:None,events:vec![]};
    transcript(&mut ctx,&child,"s","t1",Some("Explorer-test"),&mut batch).unwrap();
    assert!(!records(&ctx).iter().any(|r|r.log_type=="chat"));
}


#[test]
fn child_without_sidecar_stays_under_turn_and_foreign_child_lines_drop() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("root.jsonl");let child=tmp.path.join("agent.jsonl");
    std::fs::write(&path,"").unwrap();std::fs::write(&child,include_str!("fixtures/agent-Explorer-test.jsonl")).unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    let mut start=payload("SubagentStart","1970-01-01T00:00:03.000Z",&path);start["agent_id"]=json!("Explorer-test");start["agent_type"]=json!("Explorer");
    process(&mut ctx,&start).unwrap();
    let mut end=payload("SubagentStop","1970-01-01T00:00:05.000Z",&path);end["agent_id"]=json!("Explorer-test");end["agent_transcript_path"]=json!(child);
    process(&mut ctx,&end).unwrap();
    let all=records(&ctx);assert_eq!(all.len(),2);
    assert_eq!(all.iter().find(|r|r.span_key.ends_with(":sub:Explorer-test")).unwrap().parent_key.as_deref(),Some("qwen:s:t1:turn"));
    let wrong=tmp.path.join("wrong.jsonl");std::fs::write(&wrong,include_str!("fixtures/transcript.jsonl")).unwrap();
    let mut batch=Batch {v:1,agent:"qwen".into(),session_id:"s".into(),session_path:None,cwd:None,agent_version:None,events:vec![]};
    transcript(&mut ctx,&wrong,"s","t1",Some("Explorer-test"),&mut batch).unwrap();assert_eq!(records(&ctx).len(),2);
}


#[test]
fn background_child_keeps_spawning_turn_after_next_prompt() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("root.jsonl");let child=tmp.path.join("agent-Explorer-test.jsonl");
    std::fs::write(&path,"").unwrap();std::fs::write(&child,include_str!("fixtures/agent-Explorer-test.jsonl")).unwrap();
    std::fs::write(child.with_extension("meta.json"),include_str!("fixtures/agent-Explorer-test.meta.json")).unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    let mut tool=payload("PreToolUse","1970-01-01T00:00:02.000Z",&path);tool["tool_use_id"]=json!("spawn");process(&mut ctx,&tool).unwrap();
    let mut start=payload("SubagentStart","1970-01-01T00:00:03.000Z",&path);start["agent_id"]=json!("Explorer-test");process(&mut ctx,&start).unwrap();
    let mut next=payload("UserPromptSubmit","1970-01-01T00:00:03.500Z",&path);next["prompt_id"]=json!("t2");process(&mut ctx,&next).unwrap();
    let mut end=payload("SubagentStop","1970-01-01T00:00:05.000Z",&path);end["agent_id"]=json!("Explorer-test");end["prompt_id"]=json!("t2");end["agent_transcript_path"]=json!(child);
    process(&mut ctx,&end).unwrap();
    let all=records(&ctx);assert_eq!(all.len(),2);assert!(all.iter().all(|r|r.trace_key.as_deref()==Some("qwen:s:t1")));
}


#[test]
fn fixture_translates_to_exact_golden_events() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("root.jsonl");let child=tmp.path.join("agent-Explorer-test.jsonl");
    std::fs::write(&path,"").unwrap();std::fs::write(&child,include_str!("fixtures/agent-Explorer-test.jsonl")).unwrap();
    std::fs::write(child.with_extension("meta.json"),include_str!("fixtures/agent-Explorer-test.meta.json")).unwrap();
    // Hold only the assembler lock to inspect its scrubbed pending bridge batches.
    let lock=ctx.state_dir().join("bridge.lock");std::fs::write(&lock,b"fixture-lock").unwrap();
    let hooks:Vec<Value>=serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap();
    for (i,mut p) in hooks.into_iter().enumerate() {
        p["transcript_path"]=json!(path);if p.get("agent_transcript_path").is_some() {p["agent_transcript_path"]=json!(child);}
        if i==2 {std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();}
        process(&mut ctx,&p).unwrap();
    }
    let mut pending=std::fs::read_dir(ctx.state_dir()).unwrap().map(|e|e.unwrap().path()).filter(|p|p.file_name().unwrap().to_string_lossy().starts_with("pending-")).collect::<Vec<_>>();pending.sort();
    let events=pending.iter().flat_map(|p| {
        let envelope:Value=serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        let batch:Batch=serde_json::from_value(envelope["batch"].clone()).unwrap();batch.events
    }).collect::<Vec<_>>();
    let expected:Batch=serde_json::from_str(include_str!("fixtures/expected-batch.json")).unwrap();
    assert_eq!(serde_json::to_value(events).unwrap(),serde_json::to_value(expected.events).unwrap());
    std::fs::remove_file(lock).unwrap();
}

#[test]
fn live_qwen_025_mock_text_and_tool_transcripts() {
    let tmp = crate::tests::Temp::new();
    let mut ctx = context(&tmp);
    let hooks: Vec<Value> = serde_json::from_str(include_str!("fixtures/live-hooks.json")).unwrap();
    let transcripts = [include_str!("fixtures/live-transcript-0.jsonl"), include_str!("fixtures/live-transcript-1.jsonl")];
    for mut p in hooks {
        let index = usize::from(p["session_id"] == "live-session-1");
        let path = tmp.path.join(format!("live-{index}.jsonl"));
        if p["hook_event_name"] == "SessionStart" {
            std::fs::write(&path, "").unwrap();
        } else if matches!(p["hook_event_name"].as_str(), Some("PreToolUse" | "Stop")) {
            std::fs::write(&path, transcripts[index]).unwrap();
        }
        p["transcript_path"] = json!(path);
        process(&mut ctx, &p).unwrap();
    }
    let all = records(&ctx);
    assert_eq!(all.len(), 6);
    let turns = all.iter().filter(|r| r.log_type == "agent").collect::<Vec<_>>();
    assert_eq!(turns.len(), 2);
    let chats = all.iter().filter(|r| r.log_type == "chat").collect::<Vec<_>>();
    assert_eq!(chats.len(), 3);
    for chat in chats {
        assert_eq!(chat.model.as_deref(), Some("mock-model"));
        assert_eq!(chat.usage.input_tokens, Some(11));
        assert_eq!(chat.usage.output_tokens, Some(7));
        assert_eq!(chat.usage.cache_read_input_tokens, Some(3));
        assert_eq!(chat.usage.reasoning_tokens, Some(0));
        assert!(turns.iter().any(|turn| Some(&turn.span_key) == chat.parent_key.as_ref() && turn.trace_key == chat.trace_key));
    }
    let tool = all.iter().find(|r| r.log_type == "tool").unwrap();
    assert_eq!(tool.name, "run_shell_command");
    assert!(tool.input.as_ref().unwrap().to_string().contains("echo hra-e2e"));
    assert!(tool.output.as_ref().unwrap().to_string().contains("hra-e2e"));
    assert_eq!(tool.end_ns - tool.start_ns, 10_000_000);
    assert!(turns.iter().any(|turn| Some(&turn.span_key) == tool.parent_key.as_ref() && turn.trace_key == tool.trace_key));
    assert!(all.iter().all(|r| r.status == "ok"));
}

#[test]
fn held_native_lock_queues_without_blocking_and_settle_keeps_context() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("missing.jsonl");
    ctx.metadata.insert("herdr.pane_id".into(),json!("original-pane"));ctx.run_id=Some("original-run".into());
    let held=ctx.try_lock("qwen-native").unwrap().unwrap();
    let start=std::time::Instant::now();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    let mut pre=payload("PreToolUse","1970-01-01T00:00:02.000Z",&path);pre["tool_use_id"]=json!("queued-tool");
    process(&mut ctx,&pre).unwrap();
    let mut post=payload("PostToolUse","1970-01-01T00:00:03.000Z",&path);post["tool_use_id"]=json!("queued-tool");post["tool_name"]=json!("shell");
    process(&mut ctx,&post).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:04.000Z",&path)).unwrap();
    assert!(start.elapsed()<std::time::Duration::from_secs(1));
    assert_eq!(queued(&ctx).unwrap().len(),4);assert!(records(&ctx).is_empty());
    assert!(ctx.state_dir().join("settle-pending").exists());drop(held);
    ctx.metadata.insert("herdr.pane_id".into(),json!("settler-pane"));ctx.run_id=Some("settler-run".into());
    settle(&mut ctx).unwrap();assert!(queued(&ctx).unwrap().is_empty());
    let all=records(&ctx);assert_eq!(all.len(),2);
    let tool=all.iter().find(|r|r.log_type=="tool").unwrap();
    assert_eq!((tool.start_ns,tool.end_ns),(2_000_000_000,3_000_000_000));
    assert_eq!(tool.parent_key.as_deref(),Some("qwen:s:t1:turn"));
    assert!(all.iter().all(|r|r.run_id.as_deref()==Some("original-run")&&r.metadata["herdr.pane_id"]=="original-pane"));
    assert_eq!(ctx.run_id.as_deref(),Some("settler-run"));assert_eq!(ctx.metadata["herdr.pane_id"],"settler-pane");
}

#[test]
fn metadata_only_queue_and_transcript_input_never_persist_content() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.config.content_cap_bytes=0;
    let path=tmp.path.join("transcript.jsonl");std::fs::write(&path,"").unwrap();
    let held=ctx.try_lock("qwen-native").unwrap().unwrap();
    let mut prompt=payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path);
    for name in ["prompt","tool_input","tool_response","last_assistant_message","error","error_details","input","output","message","tool_result"] {prompt[name]=json!("disk-content-sentinel");}
    process(&mut ctx,&prompt).unwrap();
    let files=queued(&ctx).unwrap();assert_eq!(files.len(),1);
    let saved=std::fs::read_to_string(&files[0]).unwrap();assert!(!saved.contains("disk-content-sentinel"));assert!(saved.contains("t1"));
    drop(held);settle(&mut ctx).unwrap();
    std::fs::write(&path,format!("{}\n",json!({"sessionId":"s","promptId":"t1","type":"user","timestamp":"1970-01-01T00:00:02.000Z","message":{"role":"user","content":"disk-content-sentinel"}}))).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:03.000Z",&path)).unwrap();
    let state:Transcript=ctx.load(&key("transcript",&path.to_string_lossy())).unwrap();assert!(state.input.is_empty());
    fn inspect(path:&Path) {for entry in std::fs::read_dir(path).unwrap() {let path=entry.unwrap().path();if path.is_dir() {inspect(&path);} else {assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains("disk-content-sentinel"));}}}
    inspect(&tmp.path.join("state"));
}

#[test]
fn queued_session_start_uses_observed_resume_tail_not_drain_time() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let path=tmp.path.join("transcript.jsonl");std::fs::write(&path,"").unwrap();
    let held=ctx.try_lock("qwen-native").unwrap().unwrap();
    process(&mut ctx,&payload("SessionStart","1970-01-01T00:00:00.000Z",&path)).unwrap();
    process(&mut ctx,&payload("UserPromptSubmit","1970-01-01T00:00:01.000Z",&path)).unwrap();
    process(&mut ctx,&payload("Stop","1970-01-01T00:00:06.000Z",&path)).unwrap();
    std::fs::write(&path,include_str!("fixtures/transcript.jsonl")).unwrap();drop(held);
    settle(&mut ctx).unwrap();assert_eq!(records(&ctx).len(),3);
}
