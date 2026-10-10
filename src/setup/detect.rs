//! Is an agent present on this machine? Binary on PATH, or its (possibly relocated) config directory exists.
//! Used by `setup --all` and the setup popup so "all" never creates configs for agents you don't have.
use std::path::{Path, PathBuf};

fn executable(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| {
        #[cfg(windows)] { ["", ".exe", ".cmd", ".bat"].iter().any(|ext| dir.join(format!("{name}{ext}")).is_file()) }
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; dir.join(name).metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) }
    }))
}
pub fn detected(kind: &str, home: &Path) -> bool {
    let (bin, env, relative) = match kind {
        "claude" => ("claude", "CLAUDE_CONFIG_DIR", ".claude"), "codex" => ("codex", "CODEX_HOME", ".codex"),
        "omp" => ("omp", "PI_CODING_AGENT_DIR", ".omp/agent"), "pi" => ("pi", "PI_CODING_AGENT_DIR", ".pi/agent"),
        "gemini" => ("gemini", "GEMINI_CLI_HOME", ".gemini"), "opencode" => ("opencode", "OPENCODE_CONFIG_DIR", ".config/opencode"),
        "cursor" => ("agent", "CURSOR_CONFIG_DIR", ".cursor"), "hermes" => ("hermes", "HERMES_HOME", ".hermes"),
        "copilot" => ("copilot", "COPILOT_HOME", ".copilot"), "kimi" => ("kimi", "KIMI_CODE_HOME", ".kimi-code"),
        "qwen" => ("qwen", "QWEN_HOME", ".qwen"), "grok" => ("grok", "GROK_HOME", ".grok"), _ => return false,
    };
    let override_dir = std::env::var_os(env).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = match (kind, override_dir) {
        ("gemini", Some(root)) => root.join(".gemini"),
        (_, Some(dir)) => dir,
        ("omp", None) => home.join(std::env::var_os("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into())).join("agent"),
        ("opencode", None) => std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("opencode"),
        ("hermes", None) if cfg!(windows) && (std::env::var_os("HOME").is_none() || std::env::var_os("HOME") == std::env::var_os("USERPROFILE")) => std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Local")).join("hermes"),
        (_, None) => home.join(relative),
    };
    executable(bin) || (kind == "cursor" && executable("cursor-agent")) || dir.is_dir()
}
