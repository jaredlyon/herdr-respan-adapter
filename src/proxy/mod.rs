//! Opt-in loopback daemon. This module alone builds a Tokio runtime.
mod parse;
#[cfg(test)]
mod tests;

use crate::{
    config::Config,
    paths::Paths,
    record::{HookCtx, SpanRecord},
    Result,
};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{
    body::{Body, Bytes, Frame, Incoming, SizeHint},
    Request, Response, StatusCode,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    convert::Infallible,
    fs,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Tee>;
type HttpBody = BoxBody<Bytes, hyper::Error>;
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Info {
    pub pid: u32,
    pub port: u16,
    pub started_ns: u64,
    pub version: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Registration {
    pub pid: u32,
    pub agent: String,
    pub upstream: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, Value>,
}
pub fn run_path(paths: &Paths, run: &str) -> std::path::PathBuf {
    paths.state.join("proxy/runs").join(format!("{run}.json"))
}
fn valid_run(run: &str) -> bool {
    run.len() == 32 && run.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn live(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // SAFETY: signal zero probes a PID and sends no signal.
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: opened handles are checked, queried, and closed locally.
        unsafe {
            let h = OpenProcess(0x1000, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0;
            let ok = GetExitCodeProcess(h, &mut code) != 0 && code == 259;
            CloseHandle(h);
            ok
        }
    }
}
fn registration(paths: &Paths, run: &str) -> Result<Registration> {
    if !valid_run(run) {
        return Err("invalid run".into());
    }
    let reg: Registration = serde_json::from_slice(&fs::read(run_path(paths, run))?)?;
    if !live(reg.pid) {
        return Err("expired run".into());
    }
    Ok(reg)
}
struct State {
    paths: Paths,
    config: Config,
    client: HttpClient,
    previous: Mutex<HashMap<String, Value>>,
    active: AtomicUsize,
    traffic: Mutex<Instant>,
    #[cfg(test)]
    panic_parser: std::sync::atomic::AtomicBool,
}
impl State {
    fn new(paths: Paths, config: Config) -> Arc<Self> {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Arc::new(Self {
            paths,
            config,
            client: Client::builder(TokioExecutor::new())
                .http1_preserve_header_case(true)
                .build(connector),
            previous: Mutex::new(HashMap::new()),
            active: AtomicUsize::new(0),
            traffic: Mutex::new(Instant::now()),
            #[cfg(test)]
            panic_parser: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn touch(&self) {
        if let Ok(mut time) = self.traffic.lock() {
            *time = Instant::now();
        }
    }
    fn parser(
        &self,
        run: &str,
        reg: &Registration,
        protocol: &str,
        path: &str,
        start: u64,
        request: &Value,
    ) -> parse::Parser {
        let input = if request.get("generate") == Some(&Value::Bool(false)) {
            request.clone()
        } else if let Ok(mut previous) = self.previous.lock() {
            let input = parse::delta(request, previous.get(run));
            previous.insert(run.into(), request.clone());
            input
        } else {
            request.clone()
        };
        parse::Parser::new((run, &reg.agent), protocol, path, start, request, input)
    }
    fn emit(&self, reg: Registration, record: SpanRecord) {
        let paths = self.paths.clone();
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<()> {
                // Explicit run is allowed outside herdr; opt-out/pause still suppress capture.
                if std::env::var("HERDR_RESPAN").ok().as_deref() == Some("off") {
                    return Ok(());
                }
                let workspace = reg
                    .metadata
                    .get("herdr.workspace_id")
                    .and_then(Value::as_str);
                if !crate::gate::allowed(&paths.state, Some("1"), None, workspace) {
                    return Ok(());
                }
                let mut ctx = HookCtx::new(
                    paths.clone(),
                    config,
                    &reg.agent,
                    reg.metadata,
                    record.run_id.clone(),
                    false,
                )?;
                ctx.emit(record)?;
                ctx.finish()?;
                if !cfg!(test) {
                    crate::spool::spawn_flush(&paths)?;
                }
                Ok(())
            })();
            if result.is_err() {
                crate::log::error(&paths.state, "proxy capture failed (traffic unaffected)");
            }
        });
    }
    fn has_runs(&self) -> bool {
        let mut live_runs = std::collections::HashSet::new();
        if let Ok(entries) = fs::read_dir(self.paths.state.join("proxy/runs")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|s| s != "json") {
                    continue;
                }
                if let Some(run) = path.file_stem().and_then(|s| s.to_str()) {
                    if registration(&self.paths, run).is_ok() {
                        live_runs.insert(run.to_owned());
                    } else {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        if let Ok(mut previous) = self.previous.lock() {
            previous.retain(|run, _| live_runs.contains(run));
        }
        !live_runs.is_empty()
    }
}
struct Active(Arc<State>);
impl Active {
    fn new(state: Arc<State>) -> Self {
        state.active.fetch_add(1, Ordering::Relaxed);
        state.touch();
        Self(state)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        self.0.touch();
    }
}

pub fn serve(paths: &Paths, config: &Config) -> Result<()> {
    // Spawned directly with null stdio. Detach this fresh process, never pre_exec.
    #[cfg(unix)]
    if std::env::var("HERDR_RESPAN_PROXY_DAEMON").ok().as_deref() == Some("1") {
        extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: no pointers; affects only the daemon process's session.
        if unsafe { setsid() } == -1 {
            return Err("cannot detach proxy session".into());
        }
    }
    let address: SocketAddr = config.proxy.listen.parse()?;
    if address.ip() != Ipv4Addr::LOCALHOST {
        return Err("proxy must listen on 127.0.0.1".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener=tokio::net::TcpListener::bind(address).await?;
        let info=Info {pid:std::process::id(),port:listener.local_addr()?.port(),started_ns:crate::record::now_ns(),version:env!("CARGO_PKG_VERSION").into()};
        let info_path=paths.state.join("proxy.json");crate::paths::atomic_json(&info_path,&info)?;
        struct RemoveInfo(std::path::PathBuf,u64);
        impl Drop for RemoveInfo { fn drop(&mut self) { if fs::read(&self.0).ok().and_then(|b|serde_json::from_slice::<Info>(&b).ok()).is_some_and(|i|i.started_ns==self.1){let _=fs::remove_file(&self.0);} } }
        let _remove=RemoveInfo(info_path,info.started_ns);
        let state=State::new(paths.clone(),config.clone());
        let mut tick=tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                accepted=listener.accept()=>{let (stream,_)=accepted?;let state=state.clone();tokio::spawn(async move {
                    let service=hyper::service::service_fn(move|request|handle(state.clone(),request));
                    let _=hyper::server::conn::http1::Builder::new().preserve_header_case(true).serve_connection(TokioIo::new(stream),service).with_upgrades().await;
                });},
                _=tick.tick()=> {
                    if state.active.load(Ordering::Relaxed)==0 && !state.has_runs() && state.traffic.lock().map(|t|t.elapsed()>=Duration::from_secs(config.proxy.idle_exit_secs)).unwrap_or(false) { break; }
                }
            }
        } Ok(())
    })
}
fn reply(status: StatusCode, text: impl Into<Bytes>) -> Response<HttpBody> {
    Response::builder()
        .status(status)
        .body(
            Full::new(text.into())
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("constant response")
}
async fn handle(
    state: Arc<State>,
    mut request: Request<Incoming>,
) -> std::result::Result<Response<HttpBody>, Infallible> {
    if request.uri().path() == "/healthz" && request.method() == hyper::Method::GET {
        return Ok(reply(StatusCode::OK,json!({"service":"herdr-respan","pid":std::process::id(),"version":env!("CARGO_PKG_VERSION")}).to_string()));
    }
    let route = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("")
        .to_owned();
    let fields: Vec<_> = route.splitn(5, '/').collect();
    if fields.len() < 5 || fields[1] != "r" {
        return Ok(reply(StatusCode::NOT_FOUND, "unknown route"));
    }
    let run = fields[2].to_owned();
    let protocol = fields[3].to_owned();
    let mut rest = format!("/{}", fields[4]);
    let reg = match registration(&state.paths, &run) {
        Ok(r) => r,
        Err(_) => return Ok(reply(StatusCode::FORBIDDEN, "unknown run")),
    };
    if !["anthropic", "openai", "gemini"].contains(&protocol.as_str()) {
        return Ok(reply(StatusCode::NOT_FOUND, "unknown protocol"));
    }
    let upstream_key = if let Some(selector) = rest.strip_prefix("/__provider/") {
        if let Some((provider, path)) = selector.split_once('/') {
            let key = format!("{protocol}/{provider}");
            rest = format!("/{path}");
            key
        } else {
            return Ok(reply(StatusCode::NOT_FOUND, "missing provider path"));
        }
    } else {
        protocol.clone()
    };
    let upstream = match reg
        .upstream
        .get(&upstream_key)
        .or_else(|| state.config.proxy.upstream.get(&upstream_key))
    {
        Some(u) => u.trim_end_matches('/'),
        None => return Ok(reply(StatusCode::BAD_GATEWAY, "missing upstream")),
    };
    let uri = match format!("{upstream}{rest}").parse::<hyper::Uri>() {
        Ok(uri) => uri,
        Err(_) => return Ok(reply(StatusCode::BAD_GATEWAY, "invalid upstream")),
    };
    if ![Some("http"), Some("https")].contains(&uri.scheme_str()) || uri.authority().is_none() {
        return Ok(reply(StatusCode::BAD_GATEWAY, "invalid upstream"));
    }
    let websocket = request
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let downstream_upgrade = if websocket {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };
    let start = crate::record::now_ns();
    let active = Active::new(state.clone());
    let capture = Arc::new(Mutex::new(Some(Vec::new())));
    let (mut parts, body) = request.into_parts();
    parts.uri = uri;
    // Host names the upstream; HTTP compression is disabled for plaintext observation.
    if let Some(authority) = parts.uri.authority() {
        if let Ok(host) = authority.as_str().parse() {
            parts.headers.insert(hyper::header::HOST, host);
        }
    }
    if !websocket {
        parts.headers.insert(
            hyper::header::ACCEPT_ENCODING,
            hyper::header::HeaderValue::from_static("identity"),
        );
    }
    let outgoing = Request::from_parts(
        parts,
        Tee {
            inner: body,
            request: Some(capture.clone()),
            observe: None,
            active: None,
            complete: false,
        },
    );
    let mut response = match state.client.request(outgoing).await {
        Ok(response) => response,
        Err(_) => {
            if parse::inference(&protocol, &rest) {
                let request = capture
                    .lock()
                    .ok()
                    .and_then(|b| {
                        b.as_deref()
                            .and_then(|bytes| serde_json::from_slice(bytes).ok())
                    })
                    .unwrap_or(Value::Null);
                let parser = state.parser(&run, &reg, &protocol, &rest, start, &request);
                if let Some(rec) = parser.finish(502, false) {
                    state.emit(reg, rec);
                }
            }
            return Ok(reply(StatusCode::BAD_GATEWAY, "upstream connection failed"));
        }
    };
    if websocket && response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let downstream_upgrade = downstream_upgrade.expect("websocket upgrade requested");
        let ws = Arc::new(Mutex::new(WsTrace {
            state: state.clone(),
            reg,
            run,
            protocol,
            path: rest,
            requests: VecDeque::new(),
            current: None,
        }));
        tokio::spawn(async move {
            if let (Ok(down), Ok(up)) = tokio::join!(downstream_upgrade, upstream_upgrade) {
                let mut down = ReadTee {
                    inner: TokioIo::new(down),
                    frames: parse::Frames::default(),
                    trace: ws.clone(),
                    client: true,
                };
                let mut up = ReadTee {
                    inner: TokioIo::new(up),
                    frames: parse::Frames::default(),
                    trace: ws.clone(),
                    client: false,
                };
                let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
            }
            if let Ok(mut trace) = ws.lock() {
                trace.close();
            }
            drop(active);
        });
        let (parts, body) = response.into_parts();
        return Ok(Response::from_parts(parts, body.boxed()));
    }
    let request = capture
        .lock()
        .ok()
        .and_then(|b| {
            b.as_deref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok())
        })
        .unwrap_or(Value::Null);
    let observe = if parse::inference(&protocol, &rest) {
        let mut parser = state.parser(&run, &reg, &protocol, &rest, start, &request);
        parser.sse(
            response
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream")),
        );
        Some(Observation {
            parser: Some(parser),
            state: state.clone(),
            reg,
            status: response.status().as_u16(),
        })
    } else {
        None
    };
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(
        parts,
        Tee {
            inner: body,
            request: None,
            observe,
            active: Some(active),
            complete: false,
        }
        .boxed(),
    ))
}
struct Observation {
    parser: Option<parse::Parser>,
    state: Arc<State>,
    reg: Registration,
    status: u16,
}
fn safe(call: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call));
}
impl Observation {
    fn finish(&mut self, complete: bool) {
        if let Some(parser) = self.parser.take() {
            safe(|| {
                if let Some(record) = parser.finish(self.status, complete) {
                    self.state.emit(self.reg.clone(), record);
                }
            });
        }
    }
}
struct Tee {
    inner: Incoming,
    request: Option<Arc<Mutex<Option<Vec<u8>>>>>,
    observe: Option<Observation>,
    active: Option<Active>,
    complete: bool,
}
impl Body for Tee {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, hyper::Error>>> {
        let frame = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &frame {
            if let Some(bytes) = frame.data_ref() {
                if let Some(capture) = &self.request {
                    if let Ok(mut capture) = capture.lock() {
                        if let Some(copy) = capture.as_mut() {
                            if copy.len() + bytes.len() <= parse::MAX_CAPTURE {
                                copy.extend_from_slice(bytes);
                            } else {
                                *capture = None; // Never reinterpret a later tail as a new JSON request.
                            }
                        }
                    }
                }
                if let Some(active) = &self.active {
                    active.0.touch();
                }
                if let Some(observation) = &mut self.observe {
                    if let Some(parser) = &mut observation.parser {
                        safe(|| {
                            #[cfg(test)]
                            assert!(
                                !observation.state.panic_parser.load(Ordering::Relaxed),
                                "synthetic parser failure"
                            );
                            parser.bytes(bytes);
                        });
                    }
                }
            }
        }
        if matches!(&frame, Poll::Ready(None)) {
            self.complete = true;
            if let Some(observe) = &mut self.observe {
                observe.finish(true);
            }
            self.active.take();
        }
        if matches!(&frame, Poll::Ready(Some(Err(_)))) {
            if let Some(observe) = &mut self.observe {
                observe.finish(false);
            }
            self.active.take();
        }
        frame
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
impl Drop for Tee {
    fn drop(&mut self) {
        if let Some(observe) = &mut self.observe {
            observe.finish(self.complete || self.inner.is_end_stream());
        }
    }
}
struct WsTrace {
    state: Arc<State>,
    reg: Registration,
    run: String,
    protocol: String,
    path: String,
    requests: VecDeque<parse::Parser>,
    current: Option<parse::Parser>,
}
impl WsTrace {
    fn event(&mut self, client: bool, value: Value) {
        self.state.touch();
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if client && kind == "response.create" {
            self.requests.push_back(self.state.parser(
                &self.run,
                &self.reg,
                &self.protocol,
                &self.path,
                crate::record::now_ns(),
                &value,
            ));
        } else if !client {
            if self.current.is_none() {
                self.current = self.requests.pop_front();
            }
            if let Some(parser) = &mut self.current {
                parser.event(&value);
            }
            if matches!(
                kind,
                "response.completed" | "response.failed" | "response.incomplete" | "error"
            ) {
                if let Some(parser) = self.current.take() {
                    if let Some(rec) = parser.finish(101, kind == "response.completed") {
                        self.state.emit(self.reg.clone(), rec);
                    }
                }
            }
        }
    }
    fn close(&mut self) {
        if let Some(parser) = self.current.take() {
            self.requests.push_front(parser);
        }
        while let Some(parser) = self.requests.pop_front() {
            if let Some(rec) = parser.finish(101, false) {
                self.state.emit(self.reg.clone(), rec);
            }
        }
    }
}
struct ReadTee<T> {
    inner: T,
    frames: parse::Frames,
    trace: Arc<Mutex<WsTrace>>,
    client: bool,
}
impl<T: AsyncRead + Unpin> AsyncRead for ReadTee<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if let Poll::Ready(Ok(())) = &result {
            let client = self.client;
            let trace = self.trace.clone();
            safe(|| {
                for value in self.frames.bytes(&buffer.filled()[before..]) {
                    if let Ok(mut trace) = trace.lock() {
                        trace.event(client, value);
                    }
                }
            });
        }
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for ReadTee<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
