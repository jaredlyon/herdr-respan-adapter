pub mod common;
pub struct AgentSpec {
    pub kind: &'static str,
    pub display: &'static str,
    pub hook: fn(&mut crate::record::HookCtx, &[u8]) -> crate::Result<()>,
    pub plan: fn(&crate::setup::SetupCtx) -> crate::Result<crate::setup::Plan>,
}
pub static AGENTS: &[AgentSpec] = &[];
pub fn find(kind: &str) -> Option<&'static AgentSpec> {
    #[cfg(test)] if kind == "fake" { return Some(&FAKE); }
    AGENTS.iter().find(|agent| agent.kind == kind)
}
#[cfg(test)] static FAKE: AgentSpec = AgentSpec {kind:"fake",display:"Fake agent",hook:fake_hook,plan:fake_plan};
#[cfg(test)] fn fake_hook(ctx: &mut crate::record::HookCtx, payload: &[u8]) -> crate::Result<()> {
    if payload == b"panic" { panic!("fake adapter failure"); }
    let input: serde_json::Value = serde_json::from_slice(payload)?;
    let now = ctx.now_ns();
    let batch=serde_json::json!({"v":1,"agent":"fake","session_id":"fake-session","events":[
        {"type":"turn_start","turn_id":now.to_string(),"ts_ns":now,"prompt":input.get("input")},
        {"type":"turn_end","turn_id":now.to_string(),"ts_ns":now,"status":"ok"}
    ]});
    common::bridge::hook(ctx,&serde_json::to_vec(&batch)?)
}
#[cfg(test)] fn fake_plan(ctx: &crate::setup::SetupCtx) -> crate::Result<crate::setup::Plan> {
    Ok(crate::setup::Plan {edits:vec![crate::setup::Edit::JsonInsert {file:ctx.home.join("fake/settings.json"),pointer:"/hooks/Stop".into(),value:serde_json::json!({"command":ctx.hook_command("fake")}),id:"herdr-respan-fake".into()}], ..Default::default()})
}
