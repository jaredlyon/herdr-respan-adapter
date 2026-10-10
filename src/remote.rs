//! Explicit credential transfer; blobs are secrets, never action-log output.
use crate::{config::Config, paths::Paths, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
pub const MAX_BLOB: usize = 1024 * 1024;
const PLUGIN: &str = "jaredlyon.respan-adapter";
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].iter().enumerate() {
            out.push(if i > chunk.len() { '=' } else { ALPHABET[((n >> shift) & 63) as usize] as char });
        }
    }
    out
}
fn decode(text: &str) -> Result<Vec<u8>> {
    if text.len() > MAX_BLOB { return Err("config blob exceeds 1 MiB".into()); }
    let bytes: Vec<_> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() || bytes.len() % 4 != 0 { return Err("invalid config blob".into()); }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let padding = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if padding > 2 || (padding > 0 && index + 1 != bytes.len() / 4) { return Err("invalid config blob".into()); }
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let value = if i >= 4 - padding { 0 } else { ALPHABET.iter().position(|&v| v == b).ok_or("invalid config blob")? as u32 };
            n = (n << 6) | value;
        }
        if (padding == 1 && n & 255 != 0) || (padding == 2 && n & 65535 != 0) { return Err("invalid config blob".into()); }
        out.push((n >> 16) as u8);
        if padding < 2 { out.push((n >> 8) as u8); }
        if padding == 0 { out.push(n as u8); }
    }
    Ok(out)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer { v: u8, config: String, dotenv: String }
fn unpack(blob: &str) -> Result<Transfer> {
    let transfer: Transfer = serde_json::from_slice(&decode(blob)?).map_err(|_| "invalid config blob")?;
    if transfer.v != 1 { return Err("unsupported config blob version".into()); }
    let _: Config = toml::from_str(&transfer.config).map_err(|_| "invalid transferred config.toml")?;
    if transfer.dotenv.contains('\0') { return Err("invalid transferred .env".into()); }
    Ok(transfer)
}
pub fn export(paths: &Paths, config: &Config) -> Result<String> {
    let text = match std::fs::read_to_string(paths.config.join("config.toml")) {
        Ok(text) => text, Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::to_string_pretty(config)?, Err(e) => return Err(e.into()),
    };
    let dotenv = match std::fs::read_to_string(paths.config.join(".env")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let credentials = crate::config::credentials(&paths.config, &crate::paths::home()?, std::env::var("RESPAN_API_KEY").ok())?;
            match credentials {
                Some(c) if !c.key.contains(['\n', '\r', '\0', '\'', '"']) => format!("RESPAN_API_KEY={}\n", c.key),
                Some(_) => return Err("API key cannot be transferred as .env".into()), None => String::new(),
            }
        }
        Err(e) => return Err(e.into()),
    };
    let blob = encode(&serde_json::to_vec(&Transfer { v: 1, config: text, dotenv })?);
    unpack(&blob)?;
    Ok(blob)
}
fn private_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("transfer-{}", crate::record::random_id()?));
    let result = (|| -> Result<()> {
        if let Some(parent) = temp.parent() { std::fs::create_dir_all(parent)?; }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temp)?;
        #[cfg(windows)] {
            let user = Command::new("whoami").output()?;
            if !user.status.success() { return Err("cannot resolve Windows owner".into()); }
            let grant = format!("{}:F", String::from_utf8(user.stdout)?.trim());
            if !Command::new("icacls").arg(&temp).args(["/inheritance:r", "/grant:r", &grant]).stdout(Stdio::null()).stderr(Stdio::null()).status()?.success() { return Err("cannot secure imported config ACL".into()); }
        }
        file.write_all(bytes)?;
        drop(file);
        // Replace the directory entry instead of following an existing config-file symlink.
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temp); }
    result
}
pub fn import(paths: &Paths, blob: &str) -> Result<()> {
    let transfer = unpack(blob)?;
    private_write(&paths.config.join("config.toml"), transfer.config.as_bytes())?;
    private_write(&paths.config.join(".env"), transfer.dotenv.as_bytes())
}
pub fn read_blob(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new(); reader.take(MAX_BLOB as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BLOB { return Err("config blob exceeds 1 MiB".into()); }
    String::from_utf8(bytes).map_err(|_| "invalid config blob".into())
}
fn herdr() -> std::ffi::OsString { std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()) }
#[derive(Deserialize)]
struct Machine { id: String, label: String, target: String, enabled: bool }
fn target(bytes: &[u8], name: &str) -> Result<String> {
    let machines: Vec<Machine> = serde_json::from_slice(bytes)?;
    let matches: Vec<_> = machines.iter().filter(|m| m.id == name).collect();
    let matches = if matches.is_empty() { machines.iter().filter(|m| m.label == name).collect() } else { matches };
    if matches.len() != 1 { return Err("machine not found or label ambiguous".into()); }
    let m = matches[0];
    if !m.enabled || m.target.is_empty() || m.target.starts_with('-') || m.target.contains(['\n', '\r', '\0']) { return Err("machine disabled or invalid SSH target".into()); }
    Ok(m.target.clone())
}
fn quote(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }
fn ssh(target: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args(["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=10", target]);
    command
}
pub fn push(paths: &Paths, config: &Config, machine: &str) -> Result<()> {
    let list = Command::new(herdr()).args(["machine", "list", "--json"]).output()?;
    if !list.status.success() { return Err("herdr machine list failed".into()); }
    let target = target(&list.stdout, machine)?;
    let transfer = unpack(&export(paths, config)?)?;
    let probe = ssh(&target).arg("uname -s").output()?;
    let windows = !probe.status.success();
    let script = if windows {
        // No transferred secret is in argv; the script reads two base64 lines from stdin.
        format!("$ErrorActionPreference='Stop'; $d = (& herdr plugin config-dir {PLUGIN} | Out-String).Trim(); if ($LASTEXITCODE -ne 0 -or !$d) {{ throw 'config-dir failed' }}; $a=[Console]::ReadLine(); $b=[Console]::ReadLine(); $u=[Security.Principal.WindowsIdentity]::GetCurrent().Name; New-Item -ItemType Directory -Force -Path $d | Out-Null; & icacls $d /inheritance:r /grant:r ($u+':(OI)(CI)F') | Out-Null; if ($LASTEXITCODE) {{ throw 'ACL failed' }}; foreach ($entry in @(@('config.toml',$a),@('.env',$b))) {{ $p=Join-Path $d $entry[0]; [IO.File]::WriteAllBytes($p,[Convert]::FromBase64String($entry[1])); & icacls $p /inheritance:r /grant:r ($u+':F') | Out-Null; if ($LASTEXITCODE) {{ throw 'ACL failed' }} }}")
    } else {
        let flag = if String::from_utf8_lossy(&probe.stdout).trim() == "Darwin" { "-D" } else { "-d" };
        format!("set -eu; umask 077; d=$(herdr plugin config-dir {PLUGIN}); test -n \"$d\"; IFS= read -r a; IFS= read -r b; mkdir -p \"$d\"; t=$(mktemp -d \"$d/.transfer.XXXXXX\"); trap 'rm -rf \"$t\"' EXIT HUP INT TERM; printf '%s' \"$a\" | base64 {flag} > \"$t/config.toml\"; printf '%s' \"$b\" | base64 {flag} > \"$t/.env\"; chmod 600 \"$t/config.toml\" \"$t/.env\"; mv -f \"$t/config.toml\" \"$d/config.toml\"; mv -f \"$t/.env\" \"$d/.env\"")
    };
    let remote_command = if windows {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!("powershell -NoProfile -NonInteractive -EncodedCommand {}", encode(&utf16))
    } else { format!("sh -c {}", quote(&script)) };
    let mut child = ssh(&target).arg(remote_command).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    let result = (|| -> Result<()> {
        let mut input = child.stdin.take().ok_or("missing SSH stdin")?;
        writeln!(input, "{}", encode(transfer.config.as_bytes()))?;
        writeln!(input, "{}", encode(transfer.dotenv.as_bytes()))?;
        Ok(())
    })();
    let status = child.wait()?;
    result?;
    if !status.success() { return Err("SSH config transfer failed; check authentication, remote herdr PATH and permissions".into()); }
    println!("Config transferred. Run setup on that host separately.");
    Ok(())
}
pub fn config(paths: &Paths, config: &Config, args: &[String]) -> Result<()> {
    match args {
        [action] if action == "export" => { println!("{}", export(paths, config)?); Ok(()) }
        [action] if action == "import" => { import(paths, &read_blob(std::io::stdin().lock())?)?; println!("Config imported."); Ok(()) }
        [action, machine] if action == "push" => push(paths, config, machine),
        _ => Err("expected config export | import | push <machine>".into()),
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn export_import_roundtrip_and_limits() {
        let tmp = crate::tests::Temp::new();
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("source") };
        private_write(&paths.config.join("config.toml"), b"# preserve comment\ncontent_cap_bytes=0\ntool_spans=false\n").unwrap();
        private_write(&paths.config.join(".env"), b"RESPAN_API_KEY=synthetic-only\n").unwrap();
        let dest = Paths { state: tmp.path.join("state2"), config: tmp.path.join("dest") };
        let blob = export(&paths, &Config::default()).unwrap();
        import(&dest, &blob).unwrap();
        for file in ["config.toml", ".env"] { assert_eq!(std::fs::read(paths.config.join(file)).unwrap(), std::fs::read(dest.config.join(file)).unwrap()); }
        assert!(!Config::load(&dest.config).unwrap().tool_spans);
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; assert_eq!(std::fs::metadata(dest.config.join(".env")).unwrap().permissions().mode() & 0o777, 0o600); }
        for bad in ["!bad", "AAAA=", "Zh==", "Zm9=", "===="] { assert!(import(&dest, bad).is_err()); }
        assert!(read_blob(vec![b'a'; MAX_BLOB + 1].as_slice()).is_err());
        assert_eq!(decode(&encode(b"a\0bcde")).unwrap(), b"a\0bcde");
    }
    #[test] fn machine_resolution() {
        let list = br#"[{"id":"a","label":"spark","target":"user@host","enabled":true},{"id":"b","label":"off","target":"other","enabled":false}]"#;
        assert_eq!(target(list, "spark").unwrap(), "user@host");
        assert_eq!(target(list, "a").unwrap(), "user@host");
        assert!(target(list, "off").is_err()); assert!(target(list, "missing").is_err());
    }

    #[cfg(unix)]
    #[test] fn push_child() {
        if std::env::var_os("HERDR_RESPAN_PUSH_TEST").is_none() { return; }
        let paths = Paths::resolve(None, None).unwrap();
        push(&paths, &Config::default(), "synthetic").unwrap();
    }
    #[cfg(unix)]
    #[test] fn push_uses_stdin_and_remote_config_dir_without_remote_binary() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::tests::Temp::new();
        let bin = tmp.path.join("fake-bin"); std::fs::create_dir_all(&bin).unwrap();
        let herdr = bin.join("herdr"); let ssh = bin.join("ssh");
        std::fs::write(&herdr, "#!/bin/sh\ncase \"$1 $2\" in\n'machine list') printf '%s\\n' '[{\"id\":\"a\",\"label\":\"synthetic\",\"target\":\"fake-target\",\"enabled\":true}]' ;;\n'plugin config-dir') printf '%s\\n' \"$REMOTE_CONFIG\" ;;\n*) exit 1 ;;\nesac\n").unwrap();
        std::fs::write(&ssh, "#!/bin/sh\nfor last do :; done\nprintf '%s\\n' \"$last\" >> \"$SSH_ARGS_LOG\"\nexec sh -c \"$last\"\n").unwrap();
        for file in [&herdr, &ssh] { std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap(); }
        let paths = Paths { state: tmp.path.join("state"), config: tmp.path.join("source") };
        private_write(&paths.config.join("config.toml"), b"content_cap_bytes=0\n").unwrap();
        private_write(&paths.config.join(".env"), b"RESPAN_API_KEY=synthetic-transfer-only\n").unwrap();
        let remote = tmp.path.join("remote config with 'quotes'"); let log = tmp.path.join("ssh-args");
        let mut search = vec![bin]; search.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let result = Command::new(std::env::current_exe().unwrap()).args(["--exact", "remote::tests::push_child", "--nocapture"])
            .env("HERDR_RESPAN_PUSH_TEST", "1").env("HERDR_BIN_PATH", herdr).env("PATH", std::env::join_paths(search).unwrap())
            .env("HERDR_PLUGIN_CONFIG_DIR", &paths.config).env("HERDR_PLUGIN_STATE_DIR", &paths.state)
            .env("REMOTE_CONFIG", &remote).env("SSH_ARGS_LOG", &log).env("HOME", tmp.path.join("home"))
            .output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        for file in ["config.toml", ".env"] {
            assert_eq!(std::fs::read(paths.config.join(file)).unwrap(), std::fs::read(remote.join(file)).unwrap());
            assert_eq!(std::fs::metadata(remote.join(file)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let args = std::fs::read_to_string(log).unwrap();
        assert!(!args.contains("synthetic-transfer-only"));
        assert!(!args.contains(&encode(b"RESPAN_API_KEY=synthetic-transfer-only\n")));
        assert!(args.contains("herdr plugin config-dir jaredlyon.respan-adapter"));
        assert!(!args.contains("herdr-respan config"));
    }
}
