use super::*;
use crate::{config::Config, paths::Paths, record::SpanRecord};
fn hooks() -> Vec<Value> { serde_json::from_str(include_str!("fixtures/hooks.json")).unwrap() }
fn wire_records() -> Vec<Value> { include_str!("fixtures/wire.jsonl").lines().map(|l| serde_json::from_str(l).unwrap()).collect() }
fn context(tmp: &crate::tests::Temp) -> HookCtx {
    HookCtx::new(Paths { state: tmp.path.join("state"), config: tmp.path.join("config") },
        Config::default(), "kimi", BTreeMap::new(), None, false).unwrap()
}
fn records(tmp: &crate::tests::Temp) -> Vec<SpanRecord> {
    crate::spool::ready(&tmp.path.join("state")).unwrap().into_iter().flat_map(|p|
        std::fs::read_to_string(p).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<_>>()
    ).collect()
}
#[test] fn captured_successor_wire_and_hooks_emit_exact_spool_tree() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    let rs = wire_records(); let hs = hooks();
    let mut s = Session { wire: Some(tmp.path.join("wire.jsonl")), ..Default::default() };
    let mut events = vec![];
    for r in &rs {
        let ts = r["time"].as_u64().unwrap() * 1_000_000;
        wire(&mut s, r, ts, 16384, &mut events);
        if r["event"]["type"] == "tool.call" { native(&mut s, &hs[5], ts, &mut events); }
        if r["event"]["type"] == "tool.result" { native(&mut s, &hs[6], ts, &mut events); complete_tools(&mut s, &mut events); }
    }
    assert!(s.ended); assert!(s.steps.is_empty());
    let batch = Batch { v:1, agent:"kimi".into(), session_id:"session_tool".into(), session_path:None, cwd:Some("/synthetic/repo".into()), agent_version:None, events };
    bridge::ingest(&mut ctx, batch.clone()).unwrap(); bridge::ingest(&mut ctx, batch).unwrap();
    let out = records(&tmp); assert_eq!(out.len(), 4);
    let turn = out.iter().find(|r| r.log_type == "agent").unwrap();
    assert_eq!(turn.trace_key.as_deref(), Some("kimi:session_tool:0"));
    for r in out.iter().filter(|r| r.log_type == "chat") {
        assert_eq!(r.parent_key.as_deref(), Some(turn.span_key.as_str()));
        assert_eq!(r.model.as_deref(),Some("mock-model")); assert_eq!(r.provider.as_deref(),Some("openai"));
        assert_eq!(r.usage.input_tokens,Some(11)); assert_eq!(r.usage.output_tokens,Some(7));
        assert_eq!(r.usage.cache_read_input_tokens,Some(3)); assert_eq!(r.usage.cache_creation_input_tokens,Some(0));
        assert!(r.input.is_none());
    }
    let tool = out.iter().find(|r| r.log_type == "tool").unwrap();
    assert_eq!(tool.parent_key.as_deref(),Some(turn.span_key.as_str()));
    assert_eq!(tool.name,"Bash"); assert_eq!(tool.input.as_ref().unwrap()["command"],"echo hra-e2e");
    assert_eq!(tool.output.as_ref().unwrap(),"hra-e2e\n");
}
#[test] fn validates_live_prompts_and_rejects_legacy_and_foreign_payloads() {
    let tmp = crate::tests::Temp::new(); let mut ctx = context(&tmp);
    for h in hooks() { assert!(valid(&h)); }
    let mut legacy=hooks()[1].clone(); legacy.as_object_mut().unwrap().remove("client_type");
    let mut foreign=hooks()[1].clone(); foreign["client_type"]=json!("claude_code");
    let mut malformed=hooks()[1].clone(); malformed["prompt"]=json!(42);
    for h in [legacy,foreign,malformed,json!({"hook_event_name":"Stop","session_id":"s","cwd":"/repo","transcript_path":"/tmp/claude.jsonl"})] {
        assert!(!valid(&h)); hook(&mut ctx,&serde_json::to_vec(&h).unwrap()).unwrap();
    }
    assert_eq!(std::fs::read_dir(ctx.state_dir()).unwrap().count(),0);
}
#[test] fn setup_coexists_with_herdr_and_restores_semantically() {
    let tmp = crate::tests::Temp::new(); let paths=Paths { state:tmp.path.join("state"),config:tmp.path.join("config") };
    let ctx=crate::setup::SetupCtx::new(&paths,tmp.path.join("home"),None).unwrap();
    let plan=setup::make_plan(&ctx,"offline".into()).unwrap();
    let file=match &plan.edits[0] { crate::setup::Edit::TomlInsert { file,.. }=>file.clone(), _=>panic!() };
    assert_eq!(file,ctx.agent_home("KIMI_CODE_HOME",".kimi-code").join("config.toml"));
    let original="# user comment\ndefault_model = 'mock'\n[[hooks]]\nevent = 'SessionStart'\ncommand = 'herdr-agent-state'\nmatcher = ''\ntimeout = 10\n[models.mock]\nmodel = 'example'\n";
    std::fs::create_dir_all(file.parent().unwrap()).unwrap(); std::fs::write(&file,original).unwrap();
    crate::setup::apply(&ctx,"kimi",&plan).unwrap(); let once=std::fs::read(&file).unwrap();
    crate::setup::apply(&ctx,"kimi",&plan).unwrap(); assert_eq!(std::fs::read(&file).unwrap(),once);
    let installed:toml::Value=toml::from_str(std::str::from_utf8(&once).unwrap()).unwrap();
    let hs=installed["hooks"].as_array().unwrap(); assert_eq!(hs.len(),setup::EVENTS.len()+1);
    assert_eq!(hs[0]["command"].as_str(),Some("herdr-agent-state"));
    for h in &hs[1..] {
        assert!(setup::EVENTS.contains(&h["event"].as_str().unwrap()));
        assert!(h.as_table().unwrap().keys().all(|k| ["event","command","timeout","matcher"].contains(&k.as_str())));
    }
    crate::setup::remove(&ctx,&["kimi".into()]).unwrap();
    assert_eq!(toml::from_str::<toml::Value>(&std::fs::read_to_string(&file).unwrap()).unwrap(),toml::from_str::<toml::Value>(original).unwrap());
    assert!(std::fs::read_to_string(file).unwrap().contains("# user comment"));
}
#[test] fn discovers_only_successor_wire_and_rejects_ambiguous_paths() {
    let tmp=crate::tests::Temp::new();
    let legacy=tmp.path.join("sessions/wd_one/session_test/wire.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap(); std::fs::write(&legacy,"").unwrap();
    assert!(locate("session_test",Some(&tmp.path)).unwrap().is_none());
    let modern=legacy.parent().unwrap().join("agents/main/wire.jsonl");
    std::fs::create_dir_all(modern.parent().unwrap()).unwrap(); std::fs::write(&modern,"").unwrap();
    assert_eq!(locate("session_test",Some(&tmp.path)).unwrap(),Some(modern));
    assert!(locate("../escape",Some(&tmp.path)).unwrap().is_none());
    let second=tmp.path.join("sessions/wd_two/session_test/agents/main/wire.jsonl");
    std::fs::create_dir_all(second.parent().unwrap()).unwrap(); std::fs::write(&second,"").unwrap();
    assert!(locate("session_test",Some(&tmp.path)).is_err());
}
#[test] fn late_partial_wire_settles_with_original_metadata_and_proxy_mode() {
    let tmp=crate::tests::Temp::new(); let mut ctx=context(&tmp);
    let path=tmp.path.join("wire.jsonl"); let rs=wire_records();
    let first=rs[0].to_string(); std::fs::write(&path,&first).unwrap();
    let mut s=Session { wire:Some(path.clone()), ..Default::default() }; let mut events=vec![];
    tail_until(&ctx,&mut s,&mut events,u64::MAX).unwrap(); assert!(s.turn.is_none());
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
    tail_until(&ctx,&mut s,&mut events,u64::MAX).unwrap(); assert_eq!(events.len(),1);
    let mut state=State::default(); state.sessions.insert("session_tool".into(),Session { wire:Some(path.clone()), ..Default::default() }); ctx.save("native.json",&state).unwrap();
    for (i,h) in [hooks()[3].clone(),hooks()[5].clone(),hooks()[6].clone(),hooks()[7].clone()].into_iter().enumerate() {
        ctx.save(&format!("native-{i}.json"),&Pending {payload:h,ts:1791597079000000000+i as u64,
            metadata:BTreeMap::from([("herdr.pane_id".into(),json!("original-pane"))]),proxy_active:true,run_id:Some("original-run".into()),..Default::default()}).unwrap();
    }
    assert!(settle(&mut ctx).unwrap());
    std::fs::write(&path,include_str!("fixtures/wire.jsonl")).unwrap();
    // Reset the cursor because this test deliberately replaces the complete file.
    let mut state:State=ctx.load("native.json").unwrap(); state.sessions.get_mut("session_tool").unwrap().cursor=Cursor::default();
    ctx.save("native.json",&state).unwrap();
    assert!(!settle(&mut ctx).unwrap()); let out=records(&tmp); assert_eq!(out.len(),2);
    assert!(out.iter().all(|r|r.metadata["herdr.pane_id"]=="original-pane" && r.run_id.as_deref()==Some("original-run")));
    assert!(!settle(&mut ctx).unwrap()); assert_eq!(records(&tmp).len(),2);
    assert!(!ctx.proxy_active); assert!(ctx.run_id.is_none());
}
#[test] fn missing_usage_stays_unknown_and_turn_errors_are_not_success() {
    let mut s=Session::default(); let mut events=vec![];
    wire(&mut s,&json!({"type":"turn.prompt","agentId":"main","turnId":0,"input":[]}),10,16384,&mut events);
    wire(&mut s,&json!({"type":"context.append_loop_event","agentId":"main","event":{"type":"step.end","uuid":"s","usage":{"inputOther":4,"output":2}}}),20,16384,&mut events);
    if let Event::Chat {usage:Some(u),..}=&events[1] { assert_eq!(u.input_tokens,None); assert_eq!(u.cache_read_input_tokens,None); } else {panic!()}
    wire(&mut s,&json!({"type":"turn.ended","agentId":"main","reason":"interrupted"}),30,16384,&mut events);
    assert!(matches!(&events[2],Event::TurnEnd {status:Some(status),..} if status=="error"));
    wire(&mut s,&json!({"type":"turn.ended","agentId":"main","reason":"completed"}),40,16384,&mut events); assert_eq!(events.len(),3);
}
