//! Launch-local endpoint overrides; no agent config or credential writes.
use crate::{
    config::Config,
    paths::Paths,
    proxy::{self, Info, Registration},
    Result,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug)]
struct Launch {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    upstream: BTreeMap<String, String>,
    agent: String,
    proxy: bool,
    extension: bool,
}
fn endpoint(config: &Config, protocol: &str) -> String {
    let base = config
        .proxy
        .upstream
        .get(protocol)
        .map(String::as_str)
        .unwrap_or(match protocol {
            "anthropic" => "https://api.anthropic.com",
            "gemini" => "https://generativelanguage.googleapis.com",
            _ => "https://api.openai.com",
        });
    if protocol == "openai" && !base.trim_end_matches('/').ends_with("/v1") {
        format!("{}/v1", base.trim_end_matches('/'))
    } else {
        base.trim_end_matches('/').into()
    }
}
fn build_launch(
    paths: &Paths,
    config: &Config,
    args: &[String],
    base: &str,
    env: &BTreeMap<String, String>,
) -> Result<Launch> {
    let first = args
        .first()
        .ok_or("usage: run <agent> [-- args] or run --protocol P -- command")?;
    let mut launch = Launch {
        command: first.clone(),
        args: args[1..]
            .iter()
            .skip_while(|s| s.as_str() == "--")
            .cloned()
            .collect(),
        env: BTreeMap::new(),
        upstream: BTreeMap::from([
            ("anthropic".into(), endpoint(config, "anthropic")),
            ("openai".into(), endpoint(config, "openai")),
            ("gemini".into(), endpoint(config, "gemini")),
        ]),
        agent: first.clone(),
        proxy: true,
        extension: false,
    };
    let url = |protocol: &str| format!("{base}/{protocol}");
    let upstream = |key: &str, fallback: String| {
        env.get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or(fallback)
    };
    match first.as_str() {
        "--protocol"=>{
            let protocol=args.get(1).ok_or("missing protocol")?;
            if !["anthropic","openai","gemini"].contains(&protocol.as_str()){return Err("protocol must be anthropic, openai, or gemini".into());}
            if args.get(2).map(String::as_str)!=Some("--"){return Err("generic run needs -- before command".into());}
            launch.command=args.get(3).ok_or("missing command")?.clone();launch.args=args[4..].to_vec();launch.agent="generic".into();
            set_protocol(&mut launch.env,protocol,&url(protocol));
            let key=match protocol.as_str(){"anthropic"=>"ANTHROPIC_BASE_URL","gemini"=>"GOOGLE_GEMINI_BASE_URL",_=>"OPENAI_BASE_URL"};
            launch.upstream.insert(protocol.clone(),upstream(key,endpoint(config,protocol)));
        },
        "claude"=>{launch.env.insert("ANTHROPIC_BASE_URL".into(),url("anthropic"));launch.upstream.insert("anthropic".into(),upstream("ANTHROPIC_BASE_URL",endpoint(config,"anthropic")));},
        "codex"=>{
            launch.args.splice(0..0,["-c".into(),format!("openai_base_url={}",serde_json::to_string(&url("openai"))?)]);
            // Only mode is inspected; credentials never enter registration, logs, or spans.
            let home=env.get("CODEX_HOME").map(std::path::PathBuf::from).unwrap_or(crate::paths::home()?.join(".codex"));
            let chatgpt=fs::read(home.join("auth.json")).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v|v.get("auth_mode").and_then(Value::as_str)==Some("chatgpt")||v.get("tokens").is_some());
            if chatgpt&&!env.get("OPENAI_API_KEY").is_some_and(|v|!v.is_empty()) {
                launch.upstream.insert("openai".into(),"https://chatgpt.com/backend-api/codex".into());
            }else{launch.upstream.insert("openai".into(),upstream("OPENAI_BASE_URL",endpoint(config,"openai")));}
        },
        "gemini"=>{
            launch.env.insert("GOOGLE_GEMINI_BASE_URL".into(),url("gemini"));
            // Code Assist has distinct RPC envelopes and a different fixed upstream.
            launch.env.insert("CODE_ASSIST_ENDPOINT".into(),format!("{base}/gemini/__provider/code-assist"));
            launch.upstream.insert("gemini".into(),upstream("GOOGLE_GEMINI_BASE_URL",endpoint(config,"gemini")));
            launch.upstream.insert("gemini/code-assist".into(),upstream("CODE_ASSIST_ENDPOINT","https://cloudcode-pa.googleapis.com".into()));
        },
        "pi"|"omp"=>{launch.extension=true;},
        "opencode"=>{
            let mut value=env.get("OPENCODE_CONFIG_CONTENT").map(|s|serde_json::from_str::<Value>(s)).transpose()?.unwrap_or(json!({}));
            if !value.is_object(){return Err("OPENCODE_CONFIG_CONTENT must be an object".into());}
            for (provider,protocol) in [("anthropic","anthropic"),("openai","openai"),("google","gemini")] {
                // V1 and V2 honor different config keys; additive invocation-only overrides.
                let old=value.pointer(&format!("/providers/{provider}/settings/baseURL")).or_else(||value.pointer(&format!("/provider/{provider}/options/baseURL"))).and_then(Value::as_str).map(str::to_owned);
                if let Some(old)=old{launch.upstream.insert(protocol.into(),old);}
                value["provider"][provider]["options"]["baseURL"]=json!(url(protocol));
                value["providers"][provider]["settings"]["baseURL"]=json!(url(protocol));
            }
            launch.env.insert("OPENCODE_CONFIG_CONTENT".into(),serde_json::to_string(&value)?);
        },
        "qwen"=>{set_protocol(&mut launch.env,"openai",&url("openai"));launch.upstream.insert("openai".into(),upstream("OPENAI_BASE_URL","https://dashscope.aliyuncs.com/compatible-mode/v1".into()));},
        "kimi"=>{
            launch.env.insert("KIMI_BASE_URL".into(),url("openai"));set_protocol(&mut launch.env,"openai",&url("openai"));
            launch.upstream.insert("openai".into(),upstream("KIMI_BASE_URL",upstream("OPENAI_BASE_URL","https://api.kimi.com/coding/v1".into())));
        },
        "grok"=>{
            for key in ["GROK_MODELS_BASE_URL","GROK_XAI_API_BASE_URL"]{launch.env.insert(key.into(),url("openai"));}
            launch.upstream.insert("openai".into(),upstream("GROK_MODELS_BASE_URL",upstream("GROK_XAI_API_BASE_URL","https://api.x.ai/v1".into())));
        },
        "copilot"=>{
            launch.env.insert("COPILOT_OTEL_FILE_EXPORTER_PATH".into(),paths.state.join("agents/copilot/otel.jsonl").to_string_lossy().into());
            launch.proxy=false;
            if let Some(provider)=env.get("COPILOT_PROVIDER_TYPE").filter(|v|["anthropic","openai"].contains(&v.as_str())) {
                // Registry-based BYOK has precedence; don't claim an ineffective env override.
                let root=env.get("COPILOT_HOME").map(std::path::PathBuf::from).unwrap_or(crate::paths::home()?.join(".copilot"));
                let registry=env.get("COPILOT_PROVIDERS_CONFIG").map(std::path::PathBuf::from).unwrap_or(root.join("providers.json"));
                let registry_active=fs::read(&registry).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v|v.get("providers").is_some()||v.get("models").is_some());
                if !registry_active&&env.get("COPILOT_PROVIDER_BASE_URL").is_some() {
                    launch.env.insert("COPILOT_PROVIDER_BASE_URL".into(),url(provider));launch.upstream.insert(provider.clone(),env["COPILOT_PROVIDER_BASE_URL"].clone());launch.proxy=true;
                }
            }
        },
        "hermes"=>{launch.proxy=false;},
        "cursor"=>return Err("run cursor cannot proxy server-side Cursor model calls; use its native adapter instead".into()),
        _=>return Err(format!("unknown agent {first}; use run --protocol P -- command for generic mode").into()),
    }
    for upstream in launch.upstream.values() {
        validate_upstream(upstream)?;
    }
    Ok(launch)
}
fn validate_upstream(url: &str) -> Result<()> {
    let uri: hyper::Uri = url.parse()?;
    if ![Some("http"), Some("https")].contains(&uri.scheme_str())
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.query().is_some()
        || url.contains('#')
    {
        return Err(
            "provider base URL must be HTTP(S) and contain no credentials, query, or fragment"
                .into(),
        );
    }
    Ok(())
}
fn set_protocol(env: &mut BTreeMap<String, String>, protocol: &str, url: &str) {
    let keys: &[&str] = match protocol {
        "anthropic" => &["ANTHROPIC_BASE_URL"],
        "gemini" => &["GOOGLE_GEMINI_BASE_URL"],
        _ => &["OPENAI_BASE_URL", "OPENAI_API_BASE"],
    };
    for key in keys {
        env.insert((*key).into(), url.into());
    }
}
fn health(paths: &Paths) -> Option<Info> {
    let info: Info =
        serde_json::from_slice(&fs::read(paths.state.join("proxy.json")).ok()?).ok()?;
    if !proxy::live(info.pid) || info.version != env!("CARGO_PKG_VERSION") {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(300))
        .redirects(0)
        .build();
    let response = agent
        .get(&format!("http://127.0.0.1:{}/healthz", info.port))
        .call()
        .ok()?;
    let mut text = String::new();
    response
        .into_reader()
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    (value["service"] == "herdr-respan"
        && value["pid"] == info.pid
        && value["version"] == info.version)
        .then_some(info)
}
fn ensure(paths: &Paths) -> Result<Info> {
    if let Some(info) = health(paths) {
        return Ok(info);
    }
    fs::create_dir_all(paths.state.join("proxy"))?;
    let lock = paths.state.join("proxy/start.lock");
    let start = Instant::now();
    loop {
        if let Some(info) = health(paths) {
            return Ok(info);
        }
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        match options.open(&lock) {
            Ok(file) => {
                struct Remove(std::path::PathBuf);
                impl Drop for Remove {
                    fn drop(&mut self) {
                        let _ = fs::remove_file(&self.0);
                    }
                }
                let remove = Remove(lock.clone());
                drop(file);
                let mut cmd = Command::new(std::env::current_exe()?);
                cmd.args(["proxy", "serve", "--state-dir"])
                    .arg(&paths.state)
                    .arg("--config-dir")
                    .arg(&paths.config)
                    .env("HERDR_RESPAN_PROXY_DAEMON", "1")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000 | 0x00000008);
                }
                let mut daemon = cmd.spawn()?;
                while start.elapsed() < Duration::from_secs(10) {
                    if let Some(info) = health(paths) {
                        drop(remove);
                        return Ok(info);
                    }
                    if daemon.try_wait()?.is_some() {
                        return Err("proxy daemon exited before readiness".into());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                return Err("proxy readiness timed out".into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(15))
                {
                    let _ = fs::remove_file(&lock);
                }
                if start.elapsed() > Duration::from_secs(10) {
                    return Err("proxy startup is locked".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn command(launch: &Launch) -> Command {
    let mut command = Command::new(&launch.command);
    command
        .args(&launch.args)
        .env_remove("HERDR_RESPAN_PROXY")
        .envs(&launch.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(1)
    }
    #[cfg(windows)]
    {
        1
    }
}
fn wait(command: &mut Command) -> Result<i32> {
    let mut child = command.spawn()?;
    // The child inherits the foreground process group / console. Ctrl-C reaches
    // it directly; keep the wrapper alive to unregister and return its status.
    #[cfg(unix)]
    {
        extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        // SAFETY: installs SIG_IGN in this single-threaded wrapper only, after spawn.
        unsafe {
            signal(2, 1);
        }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<unsafe extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }
        unsafe extern "system" fn keep_wrapper(event: u32) -> i32 {
            if event == 0 || event == 1 {
                1
            } else {
                0
            }
        }
        // SAFETY: static handler and valid BOOL; child already inherited the console.
        unsafe {
            SetConsoleCtrlHandler(Some(keep_wrapper), 1);
        }
    }
    Ok(exit_code(child.wait()?))
}
pub fn run(paths: &Paths, config: &Config, args: &[String]) -> Result<()> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    // Validate the requested agent before touching daemon state.
    let mut launch = build_launch(paths, config, args, "http://127.0.0.1:0/r/validation", &env)?;
    if env.get("HERDR_RESPAN").is_some_and(|v| v == "off") {
        launch.env.clear();
        launch.args = if args.first().map(String::as_str) == Some("--protocol") {
            args[4..].to_vec()
        } else {
            args[1..]
                .iter()
                .skip_while(|s| s.as_str() == "--")
                .cloned()
                .collect()
        };
        std::process::exit(wait(&mut command(&launch))?);
    }
    let run = crate::record::random_id()?;
    let mut cleanup = None;
    if launch.proxy {
        let prepared = (|| -> Result<(Launch, RunFiles)> {
            let info = ensure(paths)?;
            let mut routed = build_launch(
                paths,
                config,
                args,
                &format!("http://127.0.0.1:{}/r/{run}", info.port),
                &env,
            )?;
            let registration = Registration {
                pid: std::process::id(),
                agent: routed.agent.clone(),
                upstream: routed.upstream.clone(),
                metadata: crate::meta::collect(&std::env::current_dir()?),
            };
            let files = RunFiles {
                path: proxy::run_path(paths, &run),
                asset_dir: paths.state.join("proxy/runs").join(&run),
            };
            crate::paths::atomic_json(&files.path, &registration)?;
            if routed.extension {
                let asset = files.asset_dir.join("provider.js");
                let contents = include_str!("proxy/provider.js")
                    .replace(
                        "__BASE__",
                        &serde_json::to_string(&format!("http://127.0.0.1:{}/r/{run}", info.port))?,
                    )
                    .replace(
                        "__REGISTRATION__",
                        &serde_json::to_string(&files.path.to_string_lossy())?,
                    );
                crate::paths::write_private(&asset, contents.as_bytes())?;
                routed
                    .args
                    .splice(0..0, ["-e".into(), asset.to_string_lossy().into()]);
            }
            routed.env.insert("HERDR_RESPAN_PROXY".into(), "1".into());
            Ok((routed, files))
        })();
        match prepared {
            Ok((routed, files)) => {
                launch = routed;
                cleanup = Some(files);
            }
            Err(_) => {
                eprintln!("herdr-respan: proxy unavailable; launching without interception");
                launch.env.clear();
                launch.args = args[if args.first().map(String::as_str) == Some("--protocol") {
                    4
                } else {
                    1
                }..]
                    .iter()
                    .skip_while(|s| s.as_str() == "--")
                    .cloned()
                    .collect();
            }
        }
    }
    if launch.agent == "copilot" && fs::create_dir_all(paths.state.join("agents/copilot")).is_err()
    {
        launch.env.remove("COPILOT_OTEL_FILE_EXPORTER_PATH");
        eprintln!("herdr-respan: native exporter unavailable; launching without it");
    }
    launch.env.insert("HERDR_RESPAN_RUN_ID".into(), run);
    let result = wait(&mut command(&launch));
    drop(cleanup);
    std::process::exit(result?);
}
struct RunFiles {
    path: std::path::PathBuf,
    asset_dir: std::path::PathBuf,
}
impl Drop for RunFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.asset_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn documented_env_and_auth_preservation() {
        let temp = crate::tests::Temp::new();
        let paths = Paths {
            state: temp.path.clone(),
            config: temp.path.join("config"),
        };
        let base = "http://127.0.0.1:123/r/abc";
        let config = Config::default();
        let env = BTreeMap::from([
            (
                "CODEX_HOME".into(),
                temp.path.join("codex").to_string_lossy().into_owned(),
            ),
            (
                "COPILOT_HOME".into(),
                temp.path.join("copilot").to_string_lossy().into_owned(),
            ),
            (
                "OPENCODE_CONFIG_CONTENT".into(),
                r#"{"provider":{"openai":{"options":{"apiKey":"keep"}}},"model":"openai/m"}"#
                    .into(),
            ),
        ]);
        for kind in [
            "claude", "codex", "gemini", "pi", "omp", "opencode", "qwen", "kimi", "grok",
            "copilot", "hermes",
        ] {
            let launch = build_launch(
                &paths,
                &config,
                &[kind.into(), "--".into(), "user-arg".into()],
                base,
                &env,
            )
            .unwrap();
            assert!(launch
                .env
                .keys()
                .all(|k| !k.ends_with("API_KEY") && !k.contains("TOKEN")));
            assert_eq!(launch.args.last().map(String::as_str), Some("user-arg"));
            match kind {
                "claude" => assert_eq!(
                    launch.env["ANTHROPIC_BASE_URL"],
                    format!("{base}/anthropic")
                ),
                "codex" => assert_eq!(
                    &launch.args[..2],
                    &["-c", &format!("openai_base_url=\"{base}/openai\"")]
                ),
                "gemini" => {
                    assert_eq!(
                        launch.env["GOOGLE_GEMINI_BASE_URL"],
                        format!("{base}/gemini")
                    );
                    assert!(launch.env.contains_key("CODE_ASSIST_ENDPOINT"));
                }
                "pi" | "omp" => assert!(launch.extension),
                "opencode" => {
                    let value: Value =
                        serde_json::from_str(&launch.env["OPENCODE_CONFIG_CONTENT"]).unwrap();
                    assert_eq!(value["provider"]["openai"]["options"]["apiKey"], "keep");
                    assert_eq!(value["model"], "openai/m");
                }
                "qwen" => assert_eq!(launch.env["OPENAI_BASE_URL"], format!("{base}/openai")),
                "kimi" => assert_eq!(launch.env["KIMI_BASE_URL"], format!("{base}/openai")),
                "grok" => assert_eq!(launch.env["GROK_MODELS_BASE_URL"], format!("{base}/openai")),
                "copilot" => {
                    assert!(!launch.proxy);
                    assert!(launch.env.contains_key("COPILOT_OTEL_FILE_EXPORTER_PATH"));
                }
                "hermes" => assert!(!launch.proxy),
                _ => unreachable!(),
            }
        }
        for protocol in ["anthropic", "openai", "gemini"] {
            let launch = build_launch(
                &paths,
                &config,
                &[
                    "--protocol".into(),
                    protocol.into(),
                    "--".into(),
                    "any".into(),
                    "arg".into(),
                ],
                base,
                &env,
            )
            .unwrap();
            assert_eq!(launch.command, "any");
            assert_eq!(launch.args, vec!["arg"]);
            assert_eq!(launch.env.len(), if protocol == "openai" { 2 } else { 1 });
        }
        assert!(
            build_launch(&paths, &config, &["cursor".into()], base, &env)
                .unwrap_err()
                .to_string()
                .contains("server-side")
        );
    }
    #[test]
    fn upstream_modes_and_byok_registry_precedence() {
        let temp = crate::tests::Temp::new();
        let paths = Paths {
            state: temp.path.join("state"),
            config: temp.path.join("config"),
        };
        let codex = temp.path.join("codex");
        let copilot = temp.path.join("copilot");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&copilot).unwrap();
        fs::write(
            codex.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{}}"#,
        )
        .unwrap();
        let mut env = BTreeMap::from([
            ("CODEX_HOME".into(), codex.to_string_lossy().into_owned()),
            (
                "COPILOT_HOME".into(),
                copilot.to_string_lossy().into_owned(),
            ),
            ("COPILOT_PROVIDER_TYPE".into(), "openai".into()),
            (
                "COPILOT_PROVIDER_BASE_URL".into(),
                "https://example.invalid/v1".into(),
            ),
        ]);
        let config = Config::default();
        let base = "http://127.0.0.1:123/r/synthetic";
        let subscription = build_launch(&paths, &config, &["codex".into()], base, &env).unwrap();
        assert_eq!(
            subscription.upstream["openai"],
            "https://chatgpt.com/backend-api/codex"
        );
        env.insert("OPENAI_API_KEY".into(), "synthetic-existing-key".into());
        let api_key = build_launch(&paths, &config, &["codex".into()], base, &env).unwrap();
        assert_eq!(api_key.upstream["openai"], "https://api.openai.com/v1");
        assert!(!api_key.env.contains_key("OPENAI_API_KEY"));
        let legacy = build_launch(&paths, &config, &["copilot".into()], base, &env).unwrap();
        assert!(legacy.proxy);
        assert_eq!(
            legacy.env["COPILOT_PROVIDER_BASE_URL"],
            format!("{base}/openai")
        );
        assert_eq!(legacy.upstream["openai"], "https://example.invalid/v1");
        fs::write(
            copilot.join("providers.json"),
            r#"{"providers":{"example":{}}}"#,
        )
        .unwrap();
        let registry = build_launch(&paths, &config, &["copilot".into()], base, &env).unwrap();
        assert!(!registry.proxy);
        assert!(!registry.env.contains_key("COPILOT_PROVIDER_BASE_URL"));
        assert!(validate_upstream("https://user:secret@example.invalid").is_err());
        assert!(validate_upstream("https://example.invalid?key=secret").is_err());
    }
}
