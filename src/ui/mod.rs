//! Terminal popups use the same setup engine and config transport as the CLI.
use crate::{config::Config, paths::Paths, Result};
use std::io::{IsTerminal, Read, Write};
use std::process::Command;
const PLUGIN: &str = "jaredlyon.respan-adapter";
fn line(prompt: &str) -> Result<String> {
    print!("{prompt}"); std::io::stdout().flush()?;
    let mut answer = String::new(); std::io::stdin().read_line(&mut answer)?; Ok(answer.trim().into())
}
fn herdr() -> std::ffi::OsString { std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()) }
fn open(entry: &str, agent: Option<&str>) -> Result<()> {
    let mut command = Command::new(herdr());
    command.args(["plugin", "pane", "open", "--plugin", PLUGIN, "--entrypoint", entry]);
    if let Some(agent) = agent { command.args(["--env", &format!("HERDR_RESPAN_SELECTED_AGENT={agent}")]); }
    if !command.status()?.success() { return Err("herdr could not open the plugin pane".into()); }
    Ok(())
}
fn wait() -> Result<()> { line("\nPress Enter to close. ").map(|_| ()) }
fn context_agent() -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?).ok()?;
    value.get("focused_pane_agent")?.as_str().filter(|a| crate::cli::KINDS.contains(a)).map(str::to_string)
}
fn executable(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| {
        #[cfg(windows)] { ["", ".exe", ".cmd", ".bat"].iter().any(|ext| dir.join(format!("{name}{ext}")).is_file()) }
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; dir.join(name).metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) }
    }))
}
fn detected(kind: &str, home: &std::path::Path) -> bool {
    let (bin, env, relative) = match kind {
        "claude" => ("claude", "CLAUDE_CONFIG_DIR", ".claude"), "codex" => ("codex", "CODEX_HOME", ".codex"),
        "omp" => ("omp", "PI_CODING_AGENT_DIR", ".omp/agent"), "pi" => ("pi", "PI_CODING_AGENT_DIR", ".pi/agent"),
        "gemini" => ("gemini", "GEMINI_CLI_HOME", ".gemini"), "opencode" => ("opencode", "OPENCODE_CONFIG_DIR", ".config/opencode"),
        "cursor" => ("agent", "CURSOR_CONFIG_DIR", ".cursor"), "hermes" => ("hermes", "HERMES_HOME", ".hermes"),
        "copilot" => ("copilot", "COPILOT_HOME", ".copilot"), "kimi" => ("kimi", "KIMI_SHARE_DIR", ".kimi"),
        "qwen" => ("qwen", "QWEN_HOME", ".qwen"), "grok" => ("grok", "GROK_HOME", ".grok"), _ => return false,
    };
    let override_dir = std::env::var_os(env).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    let dir = match (kind, override_dir) {
        ("gemini", Some(root)) => root.join(".gemini"),
        (_, Some(dir)) => dir,
        ("omp", None) => home.join(std::env::var_os("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into())).join("agent"),
        ("opencode", None) => std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| home.join(".config")).join("opencode"),
        ("hermes", None) if cfg!(windows) && (std::env::var_os("HOME").is_none() || std::env::var_os("HOME") == std::env::var_os("USERPROFILE")) => std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|| home.join("AppData/Local")).join("hermes"),
        (_, None) => home.join(relative),
    };
    executable(bin) || (kind == "cursor" && executable("cursor-agent")) || dir.is_dir()
}
fn choose() -> Result<Vec<String>> {
    let home = crate::paths::home()?;
    println!("Respan adapter setup (no changes until the diff is confirmed):");
    for kind in crate::cli::KINDS { println!("  {kind:10} {}", if detected(kind, &home) { "detected on PATH/config" } else { "not detected" }); }
    let answer = line("\nAgents (space separated), 'all', or Enter to cancel: ")?;
    let kinds: Vec<String> = answer.split_whitespace().map(str::to_string).collect();
    if kinds == ["all"] { return Ok(vec!["--all".into()]); }
    if kinds.iter().any(|k| !crate::cli::KINDS.contains(&k.as_str())) { return Err("unknown agent selection".into()); }
    Ok(kinds)
}
fn status(paths: &Paths) -> Result<()> {
    let credentials = crate::config::credentials(&paths.config, &crate::paths::home()?, std::env::var("RESPAN_API_KEY").ok())?;
    let report = crate::status::report(paths, credentials.as_ref())?;
    println!("Respan — {}\n", env!("CARGO_PKG_VERSION"));
    if let Some(agents) = report["agents"].as_array() {
        for agent in agents {
            let kind = agent["kind"].as_str().unwrap_or("unknown");
            let installed = agent["installed"].as_bool().unwrap_or(false);
            let coverage = if kind == "codex" && installed && agent["last_span_ns"].is_null() { "awaiting Codex trust (run /hooks in Codex once)" }
                else if installed { "adapter installed" } else { "lifecycle" };
            println!("{kind:10} {coverage}");
        }
    }
    println!("\nProxy: {}", if report["proxy_active"] == true { "active" } else { "inactive" });
    println!("Key source: {}", report["key_source"].as_str().unwrap_or("not configured"));
    println!("Spool: {} files / {} bytes / {} spans; dropped files: {}", report["spool_files"], report["spool_bytes"], report["spool_spans"], report["dropped_files"]);
    println!("Month {}: {} spans sent", report["month"], report["spans_sent_month"]);
    println!("Last flush: {}", report["flush"]);
    wait()
}
// Disable canonical line buffering and echo so a 1 MiB pasted secret is not truncated or displayed.
struct Terminal {
    #[cfg(unix)] saved: String,
    #[cfg(windows)] saved: u32,
}
#[cfg(windows)] mod console {
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetStdHandle(id: u32) -> *mut std::ffi::c_void;
        pub fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
    }
    pub fn handle() -> *mut std::ffi::c_void { unsafe { GetStdHandle(-10i32 as u32) } }
}
impl Terminal {
    fn paste() -> Result<Self> {
        #[cfg(unix)] {
            let saved = Command::new("stty").arg("-g").stdin(std::process::Stdio::inherit()).output()?;
            if !saved.status.success() { return Err("cannot read terminal settings".into()); }
            let saved = String::from_utf8(saved.stdout)?.trim().to_string();
            if !Command::new("stty").args(["-icanon", "-echo", "-isig", "min", "1", "time", "0"]).status()?.success() { return Err("cannot configure terminal input".into()); }
            let terminal = Self { saved };
            let mut stdout = std::io::stdout();
            stdout.write_all(b"\x1b[?2004h")?; stdout.flush()?;
            Ok(terminal)
        }
        #[cfg(windows)] {
            let mut saved = 0;
            unsafe {
                if console::GetConsoleMode(console::handle(), &mut saved) == 0 || console::SetConsoleMode(console::handle(), saved & !7) == 0 { return Err("cannot configure console input".into()); }
            }
            let terminal = Self { saved };
            let mut stdout = std::io::stdout();
            stdout.write_all(b"\x1b[?2004h")?; stdout.flush()?;
            Ok(terminal)
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x1b[?2004l"); let _ = stdout.flush();
        #[cfg(unix)] { let _ = Command::new("stty").arg(&self.saved).status(); }
        #[cfg(windows)] unsafe { console::SetConsoleMode(console::handle(), self.saved); }
    }
}
fn pasted(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    let mut in_paste = false;
    for byte in std::io::BufReader::new(reader).bytes() {
        let byte = byte?;
        if byte == 3 { return Err("import cancelled".into()); }
        if byte == 4 { break; }
        if !in_paste && [8, 127].contains(&byte) { bytes.pop(); continue; }
        bytes.push(byte);
        if bytes.ends_with(b"\x1b[200~") { bytes.truncate(bytes.len() - 6); in_paste = true; }
        if in_paste && bytes.ends_with(b"\x1b[201~") { bytes.truncate(bytes.len() - 6); break; }
        if !in_paste && (bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\r") || bytes.ends_with(b"\r\n\r\n")) { break; }
        if bytes.len() > crate::remote::MAX_BLOB { return Err("config blob exceeds 1 MiB".into()); }
    }
    String::from_utf8(bytes).map_err(|_| "invalid config blob".into())
}
fn import(paths: &Paths) -> Result<()> {
    if line("Replace this host's plugin config and key? [y/N] ")?.to_lowercase() != "y" { return Ok(()); }
    println!("Paste the secret blob. Bracketed paste finishes automatically; otherwise finish with a blank line or Ctrl-D. Ctrl-C cancels. Input is hidden.");
    let blob = if std::io::stdin().is_terminal() {
        let terminal = Terminal::paste()?;
        let result = pasted(std::io::stdin().lock());
        drop(terminal); println!(); result?
    } else { crate::remote::read_blob(std::io::stdin().lock())? };
    crate::remote::import(paths, &blob)?;
    println!("Config imported. Agent setup is separate on this host."); Ok(())
}
pub fn popup(paths: &Paths, config: &Config, args: &[String]) -> Result<()> {
    let args = args.strip_prefix(&["popup".into()]).unwrap_or(args);
    if let [action, name] = args { if action == "action" {
        return match name.as_str() {
            "run" => if let Some(agent) = context_agent() { open("run", Some(&agent)) } else { open("run-choice", None) },
            "config-export" => open("export", None), "config-push" => open("push", None),
            "config-import" => open("import", None), "setup" => open("setup", None), "status" => open("status", None),
            _ => Err("unknown UI action".into()),
        };
    } }
    match args {
        [name] if name == "status" => status(paths),
        [name] if name == "setup" => {
            let selected = choose()?; if selected.is_empty() { return Ok(()); }
            let status = Command::new(std::env::current_exe()?).arg("setup").args(selected)
                .arg("--state-dir").arg(&paths.state).arg("--config-dir").arg(&paths.config).status()?;
            if !status.success() { return Err("setup failed; no credentials are displayed".into()); }
            wait()
        }
        [name] if name == "import" => import(paths),
        [name] if name == "export" => {
            println!("SECRET: this one-time display includes your API key. Base64 is not encryption. Paste only into the trusted remote import popup; do not share screenshots/logs.");
            let blob = crate::remote::export(paths, config)?;
            for chunk in blob.as_bytes().chunks(76) { println!("{}", std::str::from_utf8(chunk)?); }
            println!(); wait()
        }
        [name] if name == "push" => {
            let machine = line("Saved machine label or id (Enter cancels): ")?;
            if machine.is_empty() { return Ok(()); }
            if line("Transfer this plugin config AND API key over SSH? [y/N] ")?.to_lowercase() == "y" { crate::remote::push(paths, config, &machine)?; }
            wait()
        }
        [name] if name == "run-choice" => {
            let agent = line("Agent to launch in a split (claude codex omp pi gemini opencode cursor hermes copilot kimi qwen grok): ")?;
            if agent.is_empty() { return Ok(()); }
            if !crate::cli::KINDS.contains(&agent.as_str()) { return Err("unknown agent".into()); }
            open("run", Some(&agent))
        }
        [name] if name == "launch" => {
            let agent = std::env::var("HERDR_RESPAN_SELECTED_AGENT")?;
            if !crate::cli::KINDS.contains(&agent.as_str()) { return Err("unknown agent".into()); }
            crate::run::run(paths, config, &[agent])
        }
        _ => Err("expected ui popup status|setup|import|export|push|run-choice".into()),
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn bounded_multiline_and_bracketed_paste() {
        assert_eq!(pasted(b"abc\ndef\n\n".as_slice()).unwrap().trim(), "abc\ndef");
        assert_eq!(pasted(b"\x1b[200~abc\ndef\x1b[201~".as_slice()).unwrap(), "abc\ndef");
        assert!(pasted([3].as_slice()).is_err());
        assert!(pasted(vec![b'a'; crate::remote::MAX_BLOB + 1].as_slice()).is_err());
    }
}