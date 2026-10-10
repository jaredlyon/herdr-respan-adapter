use std::path::PathBuf;
use crate::Result;

#[derive(Debug, PartialEq)]
pub enum Command {
    Hook { agent: String }, Lifecycle, Flush { detach: bool },
    Setup { agents: Vec<String>, all: bool, dry_run: bool, yes: bool, accept: Vec<String>, project: Option<PathBuf> },
    Remove { agents: Vec<String>, all: bool, project: Option<PathBuf> },
    Status, Pause { workspace: Option<String>, paused: bool }, Switch { on: bool }, Test, Startup,
    /// Validated here, implemented by the Proxy (`proxy`, `run`) and HerdrSurface (`config`, `ui`) slices.
    /// `args` are every argument after the command name, unmodified.
    Proxy, Run { args: Vec<String> }, Config { args: Vec<String> }, Ui { args: Vec<String> },
    Help,
}
#[derive(Debug)]
pub struct Cli { pub command: Command, pub state_dir: Option<PathBuf>, pub config_dir: Option<PathBuf> }

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut args = args.into_iter().peekable();
    let name = args.next().unwrap_or_else(|| "help".into());
    let mut rest = Vec::new();
    let (mut state_dir, mut config_dir) = (None, None);
    while let Some(arg) = args.next() {
        if arg == "--" { rest.push(arg); rest.extend(args); break; }
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(args.next().ok_or("missing state directory")?)),
            "--config-dir" => config_dir = Some(PathBuf::from(args.next().ok_or("missing config directory")?)),
            _ => rest.push(arg),
        }
    }
    let raw = rest.clone();
    let mut args = rest.into_iter().peekable();
    let command = match name.as_str() {
        "hook" => Command::Hook { agent: args.next().ok_or("missing agent")? },
        "lifecycle" => Command::Lifecycle,
        "flush" => Command::Flush { detach: args.next_if(|s| s == "--detach").is_some() },
        "setup" | "remove" => {
            let (mut agents, mut all, mut dry_run, mut yes, mut project, mut accept) = (Vec::new(), false, false, false, None, Vec::new());
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--all" => all = true,
                    "--dry-run" if name == "setup" => dry_run = true,
                    "--yes" if name == "setup" => yes = true,
                    "--accept" if name == "setup" => accept.push(args.next().ok_or("missing offer id")?),
                    "--project" => project = Some(PathBuf::from(args.next().ok_or("missing project directory")?)),
                    s if !s.starts_with('-') => agents.push(arg),
                    _ => return Err("unknown setup/remove option".into()),
                }
            }
            if all && !agents.is_empty() { return Err("--all and agent names are mutually exclusive".into()); }
            if name == "setup" { Command::Setup { agents, all, dry_run, yes, accept, project } }
            else { Command::Remove { agents, all, project } }
        }
        "pause" | "resume" => {
            let workspace = if args.next_if(|s| s == "--workspace").is_some() {
                Some(args.next().ok_or("missing workspace")?)
            } else { None };
            Command::Pause { workspace, paused: name == "pause" }
        }
        "status" => Command::Status,
        "on" | "off" => Command::Switch { on: name == "on" },
        "test" => Command::Test,
        "startup" => Command::Startup,
        "proxy" => { if args.next().as_deref() != Some("serve") { return Err("expected proxy serve".into()); } Command::Proxy }
        "run" => {
            let first = args.next().ok_or("missing agent or --protocol")?;
            if first == "--protocol" {
                let protocol = args.next().ok_or("missing protocol")?;
                if !["anthropic", "openai", "gemini"].contains(&protocol.as_str()) { return Err("unknown protocol".into()); }
                if args.next().as_deref() != Some("--") || args.next().is_none() { return Err("expected -- command".into()); }
            } else if !KINDS.contains(&first.as_str()) { return Err("unknown agent".into()); }
            args.by_ref().for_each(drop);
            Command::Run { args: raw }
        }
        "config" => {
            let action = args.next().ok_or("missing config action")?;
            match action.as_str() {
                "export" | "import" => {},
                "push" => { args.next().ok_or("missing machine")?; },
                _ => return Err("unknown config action".into()),
            }
            args.by_ref().for_each(drop);
            Command::Config { args: raw }
        }
        "ui" => { args.next().ok_or("missing popup")?; args.by_ref().for_each(drop); Command::Ui { args: raw } }
        "help" | "--help" | "-h" => Command::Help,
        _ => return Err("unknown command".into()),
    };
    if args.next().is_some() { return Err("unexpected argument".into()); }
    Ok(Cli { command, state_dir, config_dir })
}
pub const KINDS: &[&str] = &["claude", "codex", "omp", "pi", "gemini", "opencode", "cursor", "hermes", "copilot", "kimi", "qwen", "grok"];

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn commands() {
        for args in ["hook claude --state-dir /s --config-dir /c", "flush --detach", "setup --all --dry-run", "remove codex", "pause --workspace w", "resume", "on", "off", "lifecycle", "status", "test", "startup", "proxy serve", "run claude -- -p hi", "run --protocol openai -- echo hi", "config push spark", "ui status"] {
            assert!(parse(args.split_whitespace().map(str::to_string)).is_ok(), "{args}");
        }
        for args in ["hook", "flush nope", "setup --all claude", "run --protocol bad -- x", "config push", "pause --workspace", "off now"] {
            assert!(parse(args.split_whitespace().map(str::to_string)).is_err());
        }
    }
    #[test] fn later_slice_args_preserved() {
        let cli = parse("run claude -- -p hi".split_whitespace().map(str::to_string)).unwrap();
        assert_eq!(cli.command, Command::Run { args: ["claude", "--", "-p", "hi"].map(String::from).to_vec() });
        let cli = parse("config push spark".split_whitespace().map(str::to_string)).unwrap();
        assert_eq!(cli.command, Command::Config { args: ["push", "spark"].map(String::from).to_vec() });
    }
}
