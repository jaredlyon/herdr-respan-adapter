pub mod common;
pub mod claude;
pub mod codex;
pub mod omp;
pub mod pi;
pub mod gemini;
pub mod opencode;
pub mod cursor;
pub mod hermes;
pub mod copilot;
pub mod kimi;
pub mod qwen;
pub mod grok;
pub struct AgentSpec {
    pub kind: &'static str,
    pub display: &'static str,
    pub hook: fn(&mut crate::record::HookCtx, &[u8]) -> crate::Result<()>,
    pub plan: fn(&crate::setup::SetupCtx) -> crate::Result<crate::setup::Plan>,
    /// Protocol reply printed to stdout for every invocation (gated, failing, or successful) before any work runs,
    /// for agents whose hooks block on missing/invalid stdout (Cursor). Must not fail; `None` = print nothing.
    pub reply: Option<fn(&[u8]) -> String>,
    /// Restore deferred native records before flush; true means another settlement round is needed.
    pub settle: Option<fn(&mut crate::record::HookCtx) -> crate::Result<bool>>,
}
pub static AGENTS: &[AgentSpec] = &[
    claude::SPEC,
    codex::SPEC,
    omp::SPEC,
    pi::SPEC,
    gemini::SPEC,
    opencode::SPEC,
    cursor::SPEC,
    hermes::SPEC,
    copilot::SPEC,
    kimi::SPEC,
    qwen::SPEC,
    grok::SPEC,
];
pub fn find(kind: &str) -> Option<&'static AgentSpec> {
    #[cfg(test)] if kind == "fake" { return Some(&FAKE); }
    AGENTS.iter().find(|agent| agent.kind == kind)
}
#[cfg(test)] static FAKE: AgentSpec = AgentSpec {kind:"fake",display:"Fake agent",hook:fake_hook,plan:fake_plan,reply:None,settle:None};
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
    use crate::setup::{Edit,Offer,Plan};
    Ok(Plan {
        edits:vec![Edit::JsonInsert {file:ctx.home.join("fake/settings.json"),pointer:"/hooks/Stop".into(),value:serde_json::json!({"command":ctx.hook_command("fake")}),id:"herdr-respan-fake".into()},
            Edit::TextArg {file:ctx.home.join("fake/base.txt"),id:"herdr-respan-fake-base-arg".into(),arg:"base".into(),apply:fake_arg_apply,revert:fake_arg_revert}],
        offers:vec![Offer {id:"text",prompt:"Text offer?".into(),edits:vec![Edit::Text {file:ctx.home.join("fake/offer.txt"),id:"herdr-respan-fake-text".into(),apply:fake_text_apply,revert:fake_text_revert}]},
            Offer {id:"textarg",prompt:"Argument offer?".into(),edits:vec![Edit::TextArg {file:ctx.home.join("fake/offerarg.txt"),id:"herdr-respan-fake-offer-arg".into(),arg:"current".into(),apply:fake_arg_apply,revert:fake_arg_revert}]}],
        ..Default::default()
    })
}
#[cfg(test)] fn fake_text_apply(content:&str)->crate::Result<String> {
    let marker="\n# herdr-respan text\n";Ok(if content.contains(marker) {content.into()} else {format!("{content}{marker}")})
}
#[cfg(test)] fn fake_text_revert(content:&str)->crate::Result<String> {Ok(content.replacen("\n# herdr-respan text\n","",1))}
#[cfg(test)] fn fake_arg_apply(content:&str,arg:&str)->crate::Result<String> {
    let marker=format!("\n# herdr-respan arg={arg}\n");Ok(if content.contains(&marker) {content.into()} else {format!("{content}{marker}")})
}
#[cfg(test)] fn fake_arg_revert(content:&str,arg:&str)->crate::Result<String> {Ok(content.replacen(&format!("\n# herdr-respan arg={arg}\n"),"",1))}
