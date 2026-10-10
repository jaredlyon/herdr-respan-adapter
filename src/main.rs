pub mod agents;
pub mod cli;
pub mod config;
pub mod flush;
pub mod gate;
pub mod lifecycle;
pub mod log;
pub mod meta;
pub mod otlp;
pub mod paths;
pub mod record;
pub mod redact;
pub mod proxy;
pub mod remote;
pub mod run;
pub mod setup;
pub mod spool;
pub mod status;
pub mod ui;
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
use cli::{Cli, Command};
use paths::Paths;
use std::io::{Read, Write};

fn main() { std::process::exit(run_args(std::env::args().skip(1).collect())); }
fn run_args(args: Vec<String>) -> i32 {
    let fail_open = args.first().is_some_and(|s| s == "hook" || s == "lifecycle");
    if fail_open {
        // Silence panic payloads: adapter panics may contain agent content or credentials.
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| cli::parse(args).and_then(dispatch));
        if !matches!(result, Ok(Ok(()))) {
            if let Ok(paths) = Paths::resolve(None, None) { log::error(&paths.state,"hook failed (agent unaffected)"); }
        }
        0
    } else {
        match cli::parse(args).and_then(dispatch) { Ok(()) => 0, Err(e) => { eprintln!("{e}"); 1 } }
    }
}
fn credentials(paths: &Paths) -> Result<Option<config::Credentials>> {
    config::credentials(&paths.config, &paths::home()?, std::env::var("RESPAN_API_KEY").ok())
}
fn dispatch(cli: Cli) -> Result<()> {
    if cli.command == Command::Help {
        println!("herdr-respan hook <agent> | lifecycle | flush [--detach] | setup <agent>... [--all] [--dry-run] [--yes] [--project DIR] | remove <agent>... [--all] | status | pause/resume [--workspace ID] | test | startup | proxy serve | run | config export/import/push | ui <popup>");
        return Ok(());
    }
    if let Command::Hook { agent } = &cli.command {
        const MAX: usize = 32 * 1024 * 1024;
        let mut payload = Vec::new(); std::io::stdin().take(MAX as u64 + 1).read_to_end(&mut payload)?;
        // Some agents (Cursor) block on missing/invalid stdout, so the protocol reply goes out before anything can fail.
        if let Some(reply) = agents::find(agent).and_then(|spec| spec.reply) {
            let mut out = std::io::stdout(); out.write_all(reply(&payload).as_bytes())?; out.flush()?;
        }
        if payload.len() > MAX { return Err("hook payload too large".into()); }
        let paths = Paths::resolve(cli.state_dir, cli.config_dir)?;
        return hook(&paths, agent, &payload);
    }
    let paths = Paths::resolve(cli.state_dir, cli.config_dir)?;
    match cli.command {
        Command::Hook { .. } => unreachable!(),
        Command::Lifecycle => {
            if !gate::allowed(&paths.state,std::env::var("HERDR_ENV").ok().as_deref(),std::env::var("HERDR_RESPAN").ok().as_deref(),None) { return Ok(()); }
            let event = std::env::var("HERDR_PLUGIN_EVENT_JSON")?;
            lifecycle::handle(&paths, config::Config::load(&paths.config)?, &serde_json::from_str(&event)?)
        }
        Command::Flush { detach } => {
            if detach { spool::detach_sender(&paths) } else { flush::run(&paths, &config::Config::load(&paths.config)?, credentials(&paths)?.as_ref()) }
        }
        Command::Setup { agents, all, dry_run, yes, accept, project } => {
            let selected = select(&agents, all)?;
            let ctx = setup::SetupCtx::new(&paths, paths::home()?, project)?;
            let mut plans: Vec<_> = selected.iter().map(|a| (a.plan)(&ctx)).collect::<Result<_>>()?;
            for id in &accept { if !plans.iter().any(|p| p.offers.iter().any(|o| o.id == id)) { return Err(format!("unknown setup offer: {id}").into()); } }
            use std::io::IsTerminal;
            for plan in &mut plans {
                for notice in &plan.notices { println!("{notice}"); }
                setup::accept_offers(plan,&accept,!yes&&!dry_run&&std::io::stdin().is_terminal(),|prompt| {
                    print!("{prompt} [y/N] "); std::io::stdout().flush()?;
                    let mut answer=String::new();std::io::stdin().read_line(&mut answer)?;
                    Ok(["y","yes"].contains(&answer.trim().to_ascii_lowercase().as_str()))
                })?;
            }
            for (agent, plan) in selected.iter().zip(&plans) { print!("{}", setup::preview(&ctx, agent.kind, plan)?); }
            if dry_run { return Ok(()); }
            if !yes {
                print!("Apply these edits? [y/N] "); std::io::stdout().flush()?;
                let mut answer = String::new(); std::io::stdin().read_line(&mut answer)?;
                if !["y", "yes"].contains(&answer.trim().to_ascii_lowercase().as_str()) { return Ok(()); }
            }
            setup::refresh_binary(&paths)?;
            for (agent, plan) in selected.iter().zip(&plans) { setup::apply(&ctx, agent.kind, plan)?; }
            Ok(())
        }
        Command::Remove { agents, all, project } => {
            if !all && agents.is_empty() { return Err("choose agents or --all".into()); }
            let ctx = setup::SetupCtx::new(&paths, paths::home()?, project)?;
            setup::remove(&ctx, if all { &[] } else { &agents })
        }
        Command::Status => { println!("{}",serde_json::to_string_pretty(&status::report(&paths,credentials(&paths)?.as_ref())?)?); Ok(()) }
        Command::Pause { workspace, paused } => {
            let workspace = workspace.or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok()).ok_or("missing workspace")?;
            gate::pause(&paths.state, &workspace, paused)
        }
        Command::Test => {
            let mut ctx = record::HookCtx::new(paths.clone(),config::Config::load(&paths.config)?,"test",meta::collect(&std::env::current_dir()?),None,false)?;
            let key = format!("test:{}",record::random_id()?); let now = ctx.now_ns();
            ctx.emit(record::SpanRecord { trace_key:Some(key.clone()),span_key:key,name:"herdr-respan synthetic test".into(),start_ns:now,end_ns:now,..Default::default() })?; ctx.finish()?;
            flush::run(&paths,&ctx.config,credentials(&paths)?.as_ref())
        }
        Command::Startup => {
            let version = paths.state.join("bin/version");
            if version.exists() && std::fs::read_to_string(version)? != env!("CARGO_PKG_VERSION") {
                let ctx = setup::SetupCtx::new(&paths,paths::home()?,None)?; setup::refresh_owned(&ctx)?; setup::refresh_binary(&paths)?;
            }
            flush::run(&paths,&config::Config::load(&paths.config)?,credentials(&paths)?.as_ref())
        }
        Command::Proxy => proxy::serve(&paths, &config::Config::load(&paths.config)?),
        Command::Run { args } => run::run(&paths, &config::Config::load(&paths.config)?, &args),
        Command::Config { args } => remote::config(&paths, &config::Config::load(&paths.config)?, &args),
        Command::Ui { args } => ui::popup(&paths, &config::Config::load(&paths.config)?, &args),
        Command::Help => unreachable!(),
    }
}
fn hook(paths: &Paths, agent: &str, payload: &[u8]) -> Result<()> {
    if !gate::from_env(&paths.state) || (agent == "omp" && std::env::var("OMPCODE").ok().as_deref() == Some("1")) { return Ok(()); }
    let config = config::Config::load(&paths.config)?;
    let proxy = std::env::var("HERDR_RESPAN_PROXY").ok().as_deref() == Some("1") || ["ANTHROPIC_BASE_URL","OPENAI_BASE_URL","OPENAI_API_BASE","GOOGLE_GEMINI_BASE_URL"].iter().any(|key| std::env::var(key).is_ok_and(|url| url.contains("api.respan.ai")));
    let spec = agents::find(agent).ok_or("adapter not installed")?;
    let mut ctx = record::HookCtx::new(paths.clone(), config, agent, meta::collect(&std::env::current_dir()?), std::env::var("HERDR_RESPAN_RUN_ID").ok(), proxy)?;
    let result = (spec.hook)(&mut ctx, payload);
    ctx.finish()?;
    if !cfg!(test) { spool::spawn_flush(paths)?; }
    result
}
fn select(names: &[String], all: bool) -> Result<Vec<&'static agents::AgentSpec>> {
    if all { if agents::AGENTS.is_empty() { return Err("no adapters built yet".into()); } return Ok(agents::AGENTS.iter().collect()); }
    if names.is_empty() { return Err("choose agents or --all".into()); }
    names.iter().map(|n| agents::find(n).ok_or_else(|| format!("adapter not installed: {n}").into())).collect()
}

#[cfg(test)] mod tests {
    use super::*;
    pub struct Temp { pub path: std::path::PathBuf }
    impl Temp { pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("herdr-respan-test-{}-{}",std::process::id(),record::random_id().unwrap()));
        std::fs::create_dir_all(&path).unwrap(); Self { path }
    } }
    impl Drop for Temp { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); } }
    #[test] fn fake_hook_child() {
        if std::env::var("HERDR_RESPAN_TEST_CHILD").ok().as_deref() != Some("1") { return; }
        let agent = std::env::var("HERDR_RESPAN_TEST_AGENT").unwrap_or_else(|_| "fake".into());
        let args = if agent == "malformed" { vec!["hook".into()] } else if agent == "lifecycle" {
            let mut event=String::new();std::io::stdin().read_to_string(&mut event).unwrap();std::env::set_var("HERDR_PLUGIN_EVENT_JSON",event); vec!["lifecycle".into()]
        } else { vec!["hook".into(),agent] };
        assert_eq!(run_args(args),0);
    }
    fn child(tmp: &Temp, agent: &str, herdr: &str, input: &[u8], config: &str) -> std::time::Duration {
        let cfg = tmp.path.join("config"); std::fs::create_dir_all(&cfg).unwrap(); std::fs::write(cfg.join("config.toml"),config).unwrap();
        let start = std::time::Instant::now();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","tests::fake_hook_child","--nocapture"])
            .env_clear().env("PATH",std::env::var_os("PATH").unwrap_or_default()).env("HOME",tmp.path.join("home")).env("USERPROFILE",tmp.path.join("home"))
            .env("HOSTNAME","test-host").env("HERDR_RESPAN_TEST_CHILD","1").env("HERDR_RESPAN_TEST_AGENT",agent).env("HERDR_ENV",herdr)
            .env("HERDR_PLUGIN_STATE_DIR",tmp.path.join("state")).env("HERDR_PLUGIN_CONFIG_DIR",cfg).env("HERDR_WORKSPACE_ID","w")
            .current_dir(&tmp.path).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap(); assert!(child.wait().unwrap().success()); start.elapsed()
    }
    #[test] fn fake_hook_fail_open_and_wall_time() {
        let tmp = Temp::new(); let mut times = Vec::new();
        for _ in 0..30 { times.push(child(&tmp,"fake","1",br#"{"input":"hi"}"#,"")); }
        times.sort(); let p95 = times[28]; eprintln!("hook wall time: p95={:.3} ms (30 isolated subprocesses; detached sender excluded)",p95.as_secs_f64()*1000.0);
        // Opt-in on a controlled performance host: debug CI runners have variable load.
        // HERDR_RESPAN_PERF=1 cargo test --release fake_hook_fail_open_and_wall_time -- --nocapture
        if std::env::var("HERDR_RESPAN_PERF").ok().as_deref()==Some("1") {assert!(p95 < std::time::Duration::from_millis(50));}
        assert_eq!(spool::ready(&tmp.path.join("state")).unwrap().len(),30);
        for (agent,payload,cfg) in [("fake",b"invalid".as_slice(),""),("fake",b"panic".as_slice(),""),("absent",b"{}".as_slice(),""),("malformed",b"".as_slice(),""),("fake",b"{}".as_slice(),"not toml"),("fake",b"{}".as_slice(),"redact_patterns = ['[']")] { child(&tmp,agent,"1",payload,cfg); }
        assert_eq!(spool::ready(&tmp.path.join("state")).unwrap().len(),30);
        let outside = Temp::new(); child(&outside,"fake","0",b"{}",""); assert!(spool::ready(&outside.path.join("state")).unwrap().is_empty());
        gate::pause(&outside.path.join("state"),"w",true).unwrap(); child(&outside,"fake","1",b"{}",""); assert!(spool::ready(&outside.path.join("state")).unwrap().is_empty());
        let broken = Temp::new(); std::fs::write(broken.path.join("state"), b"not a directory").unwrap(); child(&broken,"fake","1",b"{}","");
    }
    #[test] fn toggle_and_metadata_only() {
        let tmp = Temp::new(); let paths = Paths {state:tmp.path.join("state"),config:tmp.path.join("config")};
        let config = config::Config {tool_spans:false,subagent_spans:false,aux_spans:false,content_cap_bytes:0,..Default::default()};
        let mut ctx = record::HookCtx::new(paths.clone(),config,"fake",Default::default(),None,true).unwrap();
        for rec in [record::SpanRecord {span_key:"tool".into(),log_type:"tool".into(),..Default::default()},record::SpanRecord {span_key:"sub".into(),metadata:std::collections::BTreeMap::from([("subagent".into(),serde_json::json!(true))]),..Default::default()},record::SpanRecord {span_key:"aux".into(),metadata:std::collections::BTreeMap::from([("hermes.aux_task".into(),serde_json::json!("title"))]),..Default::default()},record::SpanRecord {span_key:"model".into(),log_type:"chat".into(),model:Some("m".into()),..Default::default()}] { ctx.emit(rec).unwrap(); }
        ctx.emit(record::SpanRecord {span_key:"turn".into(),input:Some(serde_json::json!("password=private")),..Default::default()}).unwrap(); ctx.finish().unwrap();
        let bytes = std::fs::read(&spool::ready(&paths.state).unwrap()[0]).unwrap(); let rec:record::SpanRecord = serde_json::from_slice(&bytes).unwrap(); assert!(rec.input.is_none()); assert!(!String::from_utf8(bytes).unwrap().contains("private"));
    }
    #[test] fn lifecycle_working_idle_roundtrip() {
        // Real herdr envelope: snake_case `event`, payload under `data` (src/api/schema/events.rs).
        let tmp=Temp::new();
        let status=|s:&str| format!(r#"{{"event":"pane_agent_status_changed","data":{{"type":"pane_agent_status_changed","pane_id":"p","workspace_id":"w","agent":"other","agent_status":"{s}"}}}}"#);
        child(&tmp,"lifecycle","1",status("working").as_bytes(),"");child(&tmp,"lifecycle","1",status("working").as_bytes(),"");assert!(spool::ready(&tmp.path.join("state")).unwrap().is_empty());
        child(&tmp,"lifecycle","1",status("idle").as_bytes(),"");
        let files=spool::ready(&tmp.path.join("state")).unwrap();assert_eq!(files.len(),1);let rec:record::SpanRecord=serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(rec.agent,"other");assert_eq!(rec.source,"lifecycle");assert_eq!(rec.metadata["final_status"],"idle");assert_eq!(rec.metadata["herdr.pane_id"],"p");
        assert_eq!(rec.metadata["herdr.machine"],"test-host");assert!(!rec.metadata.contains_key("cwd"));
        child(&tmp,"lifecycle","1",status("working").as_bytes(),"");
        child(&tmp,"lifecycle","1",br#"{"event":"pane_exited","data":{"type":"pane_exited","pane_id":"p","workspace_id":"w"}}"#,"");assert_eq!(spool::ready(&tmp.path.join("state")).unwrap().len(),2);
    }
}
