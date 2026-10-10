use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord, setup::{self as engine, SetupCtx}};

fn context(tmp:&crate::tests::Temp)->HookCtx {
    HookCtx::new(Paths {state:tmp.path.join("state"),config:tmp.path.join("config")},Config {customer_identifier:"fixture".into(),..Default::default()},"claude",BTreeMap::from([("herdr.pane_id".into(),json!("fixture-pane"))]),None,false).unwrap()
}
fn records(tmp:&crate::tests::Temp)->Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().iter().flat_map(|path|std::fs::read_to_string(path).unwrap().lines().map(|line|serde_json::from_str(line).unwrap()).collect::<Vec<_>>()).collect()
}
fn payloads(tmp:&crate::tests::Temp)->Vec<Value> {
    let transcript=serde_json::to_string(&tmp.path.join("main.jsonl").to_string_lossy()).unwrap();
    let child=serde_json::to_string(&tmp.path.join("child.jsonl").to_string_lossy()).unwrap();
    serde_json::from_str(&include_str!("fixtures/hooks.json").replace("\"$TRANSCRIPT\"",&transcript).replace("\"$CHILD\"",&child)).unwrap()
}
fn send(ctx:&mut HookCtx,payload:&Value) {hook(ctx,&serde_json::to_vec(payload).unwrap()).unwrap();}
fn settle_now(ctx:&mut HookCtx) {
    let mut state:Session=ctx.load("session-session-fixture.json").unwrap();
    state.stopped=state.stopped.map(|_|0);for file in state.files.values_mut() {file.stable_since=0;}
    ctx.save("session-session-fixture.json",&state).unwrap();settle_pending(ctx).unwrap();
}

#[test]
fn fixture_batch_golden_and_streamed_final_usage() {
    let tmp=crate::tests::Temp::new();std::fs::write(tmp.path.join("main.jsonl"),"").unwrap();
    let payloads=payloads(&tmp);let mut state=Session {id:"session-fixture".into(),..Default::default()};let mut events=Vec::new();
    process(&mut state,&payloads[0],1,&mut events).unwrap();
    std::fs::write(tmp.path.join("main.jsonl"),include_str!("fixtures/streamed.jsonl")).unwrap();
    process(&mut state,&payloads[5],10,&mut events).unwrap();assert!(!events.iter().any(|event|matches!(event,Event::Chat {..})));
    close_turn(&mut state,1_000_000_000,false,&mut events);
    let batch=Batch {v:1,agent:"claude".into(),session_id:state.id,session_path:Some("$TRANSCRIPT".into()),cwd:state.cwd,agent_version:state.version,events};
    let mut value=serde_json::to_value(batch).unwrap();for event in value["events"].as_array_mut().unwrap() {for key in ["ts_ns","start_ns","end_ns"] {if event.get(key).is_some() {event[key]=json!(0);}}}
    assert_eq!(value,serde_json::from_str::<Value>(include_str!("fixtures/batch.json")).unwrap());
}

#[test]
fn chat_starts_when_the_request_was_sent() {
    // The transcript records only completion rows; latency is measured from the preceding user row (live-check finding).
    let tmp=crate::tests::Temp::new();std::fs::write(tmp.path.join("main.jsonl"),"").unwrap();
    let payloads=payloads(&tmp);let mut state=Session {id:"session-fixture".into(),..Default::default()};let mut events=Vec::new();
    process(&mut state,&payloads[0],1,&mut events).unwrap();
    std::fs::write(tmp.path.join("main.jsonl"),include_str!("fixtures/streamed.jsonl")).unwrap();
    process(&mut state,&payloads[5],10,&mut events).unwrap();close_turn(&mut state,1_000_000_000,false,&mut events);
    let (start,end)=events.iter().find_map(|e|match e {Event::Chat {start_ns,end_ns,..}=>Some((*start_ns,*end_ns)),_=>None}).unwrap();
    assert_eq!((start,end),(timestamp("2026-10-09T12:00:00.000Z").unwrap(),timestamp("2026-10-09T12:00:02.000Z").unwrap()));
}

#[test]
fn hooks_to_spool_tree_and_subagent_stop_before_tool_result() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);
    std::fs::write(tmp.path.join("main.jsonl"),"").unwrap();send(&mut ctx,&payloads[0]);send(&mut ctx,&payloads[1]);send(&mut ctx,&payloads[2]);
    std::fs::write(tmp.path.join("child.jsonl"),include_str!("fixtures/subagent.jsonl")).unwrap();send(&mut ctx,&payloads[3]);
    assert!(records(&tmp).is_empty()); // No guessed parent and no unfinished usage.
    send(&mut ctx,&payloads[4]);
    std::fs::write(tmp.path.join("main.jsonl"),include_str!("fixtures/streamed.jsonl")).unwrap();send(&mut ctx,&payloads[5]);settle_now(&mut ctx);
    let spans=records(&tmp);assert_eq!(spans.len(),5);
    let root="claude:session-fixture:prompt-fixture";
    let parent=|suffix:&str|spans.iter().find(|span|span.span_key==format!("{root}:{suffix}")).unwrap().parent_key.clone();
    assert_eq!(parent("sub:child-fixture"),Some(format!("{root}:tool:tool-spawn")));
    assert_eq!(parent("chat:child-fixture:request-child"),Some(format!("{root}:sub:child-fixture")));
    assert_eq!(parent("chat:request-main"),Some(format!("{root}:turn")));
    let usage:Vec<_>=spans.iter().filter(|span|span.log_type=="chat").map(|span|span.usage.output_tokens.unwrap()).collect();assert_eq!(usage.iter().sum::<u64>(),13);
    assert!(spans.iter().all(|span|span.thread_id.as_deref()==Some("session-fixture") && span.metadata["herdr.pane_id"]=="fixture-pane"));
    send(&mut ctx,&payloads[5]);settle_now(&mut ctx);assert_eq!(records(&tmp).len(),5);
}

#[test]
fn incremental_partial_tail_late_stop_and_replay() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);
    let lines:Vec<_>=include_str!("fixtures/streamed.jsonl").lines().collect();
    std::fs::write(&path,format!("{}\n{}\n{}",lines[0],lines[1],&lines[2][..50])).unwrap();send(&mut ctx,&payloads[5]);
    settle_now(&mut ctx);assert!(records(&tmp).is_empty()); // Null stop_reason is not final.
    std::fs::write(&path,include_str!("fixtures/streamed.jsonl")).unwrap();assert!(settle_pending(&mut ctx).unwrap());settle_now(&mut ctx);
    let spans=records(&tmp);assert_eq!(spans.len(),2);let chat=spans.iter().find(|span|span.log_type=="chat").unwrap();assert_eq!(chat.usage.output_tokens,Some(7));
    let replacement=tmp.path.join("replacement.jsonl");std::fs::write(&replacement,include_str!("fixtures/streamed.jsonl")).unwrap();std::fs::rename(replacement,path).unwrap();settle_pending(&mut ctx).unwrap();assert_eq!(records(&tmp).len(),2);
}

#[test]
fn contested_native_queue_preserves_payloads_and_scrubs_state() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let mut payload=payloads(&tmp).remove(0);payload["prompt"]=json!("password=private-fixture");
    let lock=ctx.try_lock("native").unwrap().unwrap();send(&mut ctx,&payload);assert_eq!(files(ctx.state_dir(),"native-").unwrap().len(),1);
    let queue=files(ctx.state_dir(),"native-").unwrap().remove(0);assert!(!std::fs::read_to_string(queue).unwrap().contains("private-fixture"));drop(lock);
    settle_pending(&mut ctx).unwrap();assert!(files(ctx.state_dir(),"native-").unwrap().is_empty());
    assert!(!std::fs::read_to_string(ctx.state_dir().join("session-session-fixture.json")).unwrap().contains("private-fixture"));
}

#[test]
fn malformed_and_foreign_payloads_are_silent() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);
    for bytes in [b"invalid".as_slice(),b"{}",br#"{"hookEventName":"Stop","workspaceRoot":"/fixture","session_id":"session-fixture","cwd":"/fixture","transcript_path":"/fixture/main.jsonl"}"#,br#"{"session_id":"../foreign","hook_event_name":"Stop","cwd":"/fixture","transcript_path":"/fixture/main.jsonl"}"#,br#"{"conversation_id":"cursor","session_id":"session-fixture","hook_event_name":"Stop","cwd":"/fixture","transcript_path":"/fixture/main.jsonl"}"#] {hook(&mut ctx,bytes).unwrap();}
    assert!(files(ctx.state_dir(),"session-").unwrap().is_empty());assert!(SPEC.reply.is_none());
    for key in ["GROK_HOOK_EVENT","GROK_SESSION_ID","GROK_HOOK_NAME"] {
        let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","agents::claude::tests::foreign_env_child"]).env(key,"grok").env("CLAUDE_FIXTURE_CHILD","1").output().unwrap();assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    }
}
#[test]
fn foreign_env_child() {
    if std::env::var_os("CLAUDE_FIXTURE_CHILD").is_none() {return;}
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);send(&mut ctx,&payloads(&tmp)[0]);assert!(files(ctx.state_dir(),"session-").unwrap().is_empty());
}

#[test]
fn setup_preserves_herdr_and_remove_roundtrip() {
    let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state space"),config:tmp.path.join("config space")};
    let ctx=SetupCtx::new(&paths,tmp.path.join("home"),Some(tmp.path.join("project"))).unwrap();let plan=setup::plan(&ctx).unwrap();
    let file=tmp.path.join("project/.claude/settings.local.json");std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let original=json!({"permissions":{"allow":["Read"]},"hooks":{"SessionStart":[{"matcher":"^(startup|resume|clear|compact|fork)$","hooks":[{"type":"command","command":"bash '/fixture/herdr-agent-state.sh' session","timeout":10}]}],"Stop":[{"hooks":[{"type":"command","command":"echo custom"}]}]}});
    std::fs::write(&file,original.to_string()).unwrap();assert!(engine::preview(&ctx,"claude",&plan).unwrap().contains("herdr-respan"));
    engine::apply(&ctx,"claude",&plan).unwrap();let once=std::fs::read(&file).unwrap();engine::apply(&ctx,"claude",&plan).unwrap();assert_eq!(once,std::fs::read(&file).unwrap());
    let installed:Value=serde_json::from_slice(&once).unwrap();assert_eq!(installed["hooks"]["SessionStart"][0],original["hooks"]["SessionStart"][0]);assert_eq!(installed["hooks"]["SessionStart"].as_array().unwrap().len(),2);
    let command=installed["hooks"]["SessionStart"][1]["hooks"][0]["command"].as_str().unwrap();assert!(command.contains("herdr-respan"));assert!(command.contains("state space"));
    engine::remove(&ctx,&["claude".into()]).unwrap();assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(file).unwrap()).unwrap(),original);
    let windows=crate::agents::common::hookcmd::command(&ctx,"claude",crate::agents::common::hookcmd::Shell::PowerShell);assert!(windows.starts_with("& '"));assert!(windows.contains("'hook' 'claude'"));
}

#[test]
fn timestamps_and_tool_failure() {
    assert_eq!(timestamp("1970-01-01T00:00:00Z"),Some(0));assert_eq!(timestamp("2026-10-09T12:00:00.123Z"),Some(1_791_547_200_123_000_000));assert!(timestamp("2026-02-30T00:00:00Z").is_none());assert!(timestamp("bad").is_none());
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);send(&mut ctx,&payloads[0]);send(&mut ctx,&payloads[1]);
    let mut failure=payloads[4].clone();failure["hook_event_name"]=json!("PostToolUseFailure");failure["error"]=json!("synthetic tool failure");failure.as_object_mut().unwrap().remove("tool_response");send(&mut ctx,&failure);
    let spans=records(&tmp);assert_eq!(spans.len(),1);assert_eq!(spans[0].status,"error");assert_eq!(spans[0].status_message.as_deref(),Some("synthetic tool failure"));
}

#[test]
fn missing_usage_message_identity_and_metadata_are_unknown() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);
    let mut text=String::new();
    for line in include_str!("fixtures/streamed.jsonl").lines() {
        let mut row:Value=serde_json::from_str(line).unwrap();row.as_object_mut().unwrap().remove("requestId");
        if row["type"]=="assistant" {row["message"].as_object_mut().unwrap().remove("usage");}
        text.push_str(&row.to_string());text.push('\n');
    }
    text.push_str(r#"{"type":"progress","message":{"id":"not-a-call","usage":{"output_tokens":999}}}"#);text.push('\n');
    text.push_str(r#"{"type":"user","isCompactSummary":true,"isMeta":true,"message":{"role":"user","content":"Synthetic compact summary"}}"#);text.push('\n');
    std::fs::write(&path,text).unwrap();send(&mut ctx,&payloads[5]);settle_now(&mut ctx);
    let spans=records(&tmp);assert_eq!(spans.len(),2);let chat=spans.iter().find(|s|s.log_type=="chat").unwrap();assert!(chat.span_key.ends_with(":chat:msg-main"));assert_eq!(chat.usage,Usage::default());
}

#[test]
fn relocation_and_empty_config_env() {
    for path in ["~/relocated claude", "", "/tmp/claude-fixture-relocated"] {
        let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","agents::claude::tests::relocation_env_child"]).env("CLAUDE_CONFIG_DIR",path).env("CLAUDE_RELOCATION_CHILD","1").output().unwrap();assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    }
}
#[test]
fn relocation_env_child() {
    if std::env::var_os("CLAUDE_RELOCATION_CHILD").is_none() {return;}
    let tmp=crate::tests::Temp::new();let paths=Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};let ctx=SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
    let value=std::env::var("CLAUDE_CONFIG_DIR").unwrap();let expected=if value.is_empty() {ctx.home.join(".claude/settings.json")} else if let Some(rest)=value.strip_prefix("~/") {ctx.home.join(rest).join("settings.json")} else {PathBuf::from(value).join("settings.json")};
    for edit in setup::plan(&ctx).unwrap().edits {match edit {crate::setup::Edit::JsonInsert {file,..}=>assert_eq!(file,expected),_=>panic!("unexpected setup edit")}}
}

#[test]
fn proxy_keeps_turn_but_not_model_spans() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.proxy_active=true;let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);std::fs::write(&path,include_str!("fixtures/streamed.jsonl")).unwrap();send(&mut ctx,&payloads[5]);settle_now(&mut ctx);
    let spans=records(&tmp);assert_eq!(spans.len(),1);assert_eq!(spans[0].log_type,"agent");
}

#[test]
fn session_start_baselines_existing_history() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,include_str!("fixtures/streamed.jsonl")).unwrap();let mut start=payloads[0].clone();start["hook_event_name"]=json!("SessionStart");start["source"]=json!("resume");send(&mut ctx,&start);
    let state:Session=ctx.load("session-session-fixture.json").unwrap();assert_eq!(state.files[&path.to_string_lossy().into_owned()].cursor.offset,std::fs::metadata(&path).unwrap().len());assert!(records(&tmp).is_empty());
    send(&mut ctx,&payloads[0]);send(&mut ctx,&payloads[5]);settle_now(&mut ctx);assert_eq!(records(&tmp).len(),1); // Historical response was never traced.
}

#[test]
fn failed_or_ended_turn_emits_incomplete_chat_without_final_usage() {
    for event in ["StopFailure","SessionEnd"] {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
        std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);
        let partial=include_str!("fixtures/streamed.jsonl").lines().take(2).collect::<Vec<_>>().join("\n")+"\n";std::fs::write(&path,partial).unwrap();
        let mut failed=payloads[5].clone();failed["hook_event_name"]=json!(event);failed["error"]=json!("synthetic API failure");send(&mut ctx,&failed);settle_now(&mut ctx);
        let spans=records(&tmp);assert_eq!(spans.len(),2);let chat=spans.iter().find(|s|s.log_type=="chat").unwrap();let turn=spans.iter().find(|s|s.log_type=="agent").unwrap();
        assert_eq!(chat.status,"error");assert_eq!(chat.usage,Usage::default());assert!(chat.output.is_some());assert_eq!(chat.parent_key.as_ref(),Some(&turn.span_key));assert_eq!(turn.status,"error");
    }
}

#[test]
fn stop_before_every_assistant_row_keeps_settlement_pending() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);send(&mut ctx,&payloads[5]);
    let mut state:Session=ctx.load("session-session-fixture.json").unwrap();state.stopped=Some(ctx.now_ns().saturating_sub(200_000_000));ctx.save("session-session-fixture.json",&state).unwrap();
    assert!(settle_pending(&mut ctx).unwrap());assert!(records(&tmp).is_empty());
    std::fs::write(&path,include_str!("fixtures/streamed.jsonl")).unwrap();assert!(settle_pending(&mut ctx).unwrap());settle_now(&mut ctx);
    let spans=records(&tmp);assert_eq!(spans.len(),2);let chat=spans.iter().find(|span|span.log_type=="chat").unwrap();assert_eq!(chat.usage.output_tokens,Some(7));
    assert!(!settle_pending(&mut ctx).unwrap());assert_eq!(records(&tmp).len(),2);
}

#[test]
fn failed_or_abandoned_response_never_becomes_partial_success_on_next_prompt() {
    for failed in [false,true] {
        let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);let payloads=payloads(&tmp);let path=tmp.path.join("main.jsonl");
        std::fs::write(&path,"").unwrap();send(&mut ctx,&payloads[0]);
        let partial=include_str!("fixtures/streamed.jsonl").lines().take(2).collect::<Vec<_>>().join("\n")+"\n";
        std::fs::write(&path,partial).unwrap();
        if failed {let mut p=payloads[5].clone();p["hook_event_name"]=json!("StopFailure");send(&mut ctx,&p);settle_now(&mut ctx);}
        let mut next=payloads[0].clone();next["prompt_id"]=json!("next-prompt");send(&mut ctx,&next);
        let spans=records(&tmp);let chat=spans.iter().find(|s|s.log_type=="chat").unwrap();
        assert_eq!(chat.status,"error");assert_eq!(chat.usage,Usage::default());
        let turn=spans.iter().find(|s|s.log_type=="agent").unwrap();assert_eq!(turn.status,"error");
    }
}
#[test]
fn metadata_only_scrubs_native_queue_and_partial_transcript_state() {
    let tmp=crate::tests::Temp::new();let mut ctx=context(&tmp);ctx.config.content_cap_bytes=0;let path=tmp.path.join("main.jsonl");
    std::fs::write(&path,"").unwrap();let mut p=payloads(&tmp).remove(0);
    for key in ["prompt","tool_input","tool_response","last_assistant_message"] {p[key]=json!("metadata-private-sentinel");}
    let lock=ctx.try_lock("native").unwrap().unwrap();send(&mut ctx,&p);
    for file in files(ctx.state_dir(),"native-").unwrap() {assert!(!std::fs::read_to_string(file).unwrap().contains("metadata-private-sentinel"));}
    drop(lock);settle_pending(&mut ctx).unwrap();
    let partial=include_str!("fixtures/streamed.jsonl").lines().take(2).collect::<Vec<_>>().join("\n").replace("Hello ","metadata-private-sentinel")+"\n";
    std::fs::write(&path,partial).unwrap();let stop=payloads(&tmp).remove(5);send(&mut ctx,&stop);
    let state:Session=ctx.load("session-session-fixture.json").unwrap();
    for file in state.files.values() {assert!(file.input.is_empty());if let Some(response)=&file.current {assert!(response.input.is_empty());assert!(response.parts.is_empty());}}
    assert!(!std::fs::read_to_string(ctx.state_dir().join("session-session-fixture.json")).unwrap().contains("metadata-private-sentinel"));
}
