use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::AtomicBool,
};

async fn fixture(
    config: Config,
) -> (
    crate::tests::Temp,
    Arc<State>,
    u16,
    String,
    tokio::task::JoinHandle<()>,
) {
    let temp = crate::tests::Temp::new();
    let paths = Paths {
        state: temp.path.join("state"),
        config: temp.path.join("config"),
    };
    let run = "abcdefabcdefabcdefabcdefabcdefab".to_owned();
    let registration = Registration {
        pid: std::process::id(),
        agent: "fake".into(),
        upstream: BTreeMap::new(),
        metadata: BTreeMap::new(),
    };
    crate::paths::atomic_json(&run_path(&paths, &run), &registration).unwrap();
    let state = State::new(paths, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = server_state.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |r| handle(state.clone(), r));
                hyper::server::conn::http1::Builder::new()
                    .preserve_header_case(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await
                    .unwrap();
            });
        }
    });
    (temp, state, port, run, server)
}
fn request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut buffer).unwrap();
        bytes.push(buffer[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length = headers
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|n| n.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (headers, body)
}
fn chunk(stream: &mut TcpStream, bytes: &[u8]) {
    write!(stream, "{:x}\r\n", bytes.len()).unwrap();
    stream.write_all(bytes).unwrap();
    stream.write_all(b"\r\n").unwrap();
    stream.flush().unwrap();
}
async fn records(state: &State, count: usize) -> Vec<SpanRecord> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let files = crate::spool::ready(&state.paths.state).unwrap();
        if files.len() >= count {
            return files
                .iter()
                .flat_map(|p| {
                    fs::read_to_string(p)
                        .unwrap()
                        .lines()
                        .map(|l| serde_json::from_str(l).unwrap())
                        .collect::<Vec<_>>()
                })
                .collect();
        }
        assert!(Instant::now() < deadline, "spans not spooled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn http_protocols_stream_headers_and_usage() {
    for (protocol, path, first, last) in [
        (
            "anthropic",
            "v1/messages",
            r#"{"type":"message_start","message":{"model":"synthetic","usage":{"input_tokens":7,"cache_read_input_tokens":2}}}"#,
            r#"{"type":"message_delta","delta":{"text":"done"},"usage":{"output_tokens":3}}"#,
        ),
        (
            "openai",
            "v1/chat/completions",
            r#"{"choices":[{"delta":{"content":"hello"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}"#,
        ),
        (
            "openai",
            "v1/responses",
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            r#"{"type":"response.completed","response":{"model":"synthetic","output":[],"usage":{"input_tokens":7,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}}}"#,
        ),
        (
            "gemini",
            "v1beta/models/synthetic:streamGenerateContent?alt=sse",
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}}"#,
            r#"{"response":{"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"cachedContentTokenCount":2}}}"#,
        ),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let mut config = Config::default();
        config
            .proxy
            .upstream
            .insert(protocol.into(), format!("http://127.0.0.1:{upstream_port}"));
        let (_temp, state, port, run, server) = fixture(config).await;
        let done = Arc::new(AtomicBool::new(false));
        let upstream_done = done.clone();
        let body =
            br#"{"model":"synthetic","messages":[{"role":"user","content":"hello"}]}"#.to_vec();
        let expected = body.clone();
        let upstream = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let (headers, body) = request(&mut stream);
            assert_eq!(body, expected);
            for line in [
                "accept-encoding: identity",
                "authorization: Bearer synthetic-token",
                "x-api-key: synthetic-key",
                "chatgpt-account-id: account",
                "anthropic-beta: oauth-2025-04-20",
                "openai-beta: responses_websockets=2026-02-06",
                "xai-grok-cli: synthetic-session",
                "x-hra-exact: MixedCase +/= bytes",
            ] {
                let (name, value) = line.split_once(": ").unwrap();
                let actual = headers.lines().filter_map(|line| line.split_once(": "))
                    .find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value);
                assert_eq!(actual, Some(value), "changed header {line}: {headers}");
            }
            let duplicates: Vec<_> = headers.lines().filter_map(|line| line.split_once(": "))
                .filter(|(key, _)| key.eq_ignore_ascii_case("x-hra-duplicate"))
                .map(|(_, value)| value).collect();
            assert_eq!(duplicates, ["first", "SECOND"]);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nX-Test-Header: unchanged\r\nConnection: close\r\n\r\n").unwrap();
            chunk(&mut stream, format!("data: {first}\n\n").as_bytes());
            std::thread::sleep(Duration::from_millis(250));
            upstream_done.store(true, Ordering::SeqCst);
            chunk(
                &mut stream,
                format!("data: {last}\n\ndata: [DONE]\n\n").as_bytes(),
            );
            stream.write_all(b"0\r\n\r\n").unwrap();
        });
        let downstream = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            write!(stream,"POST /r/{run}/{protocol}/{path} HTTP/1.1\r\nHost: local\r\nContent-Length: {}\r\nAuthorization: Bearer synthetic-token\r\nX-Api-Key: synthetic-key\r\nChatGPT-Account-ID: account\r\nAnthropic-Beta: oauth-2025-04-20\r\nOpenAI-Beta: responses_websockets=2026-02-06\r\nxai-grok-cli: synthetic-session\r\nAccept-Encoding: gzip, br\r\nX-HRA-Exact: MixedCase +/= bytes\r\nX-HRA-Duplicate: first\r\nX-HRA-Duplicate: SECOND\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            stream.write_all(&body).unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..n]);
                if String::from_utf8_lossy(&bytes).contains(first) {
                    assert!(
                        !done.load(Ordering::SeqCst),
                        "response buffered until final chunk"
                    );
                    break;
                }
            }
            stream.read_to_end(&mut bytes).unwrap();
            let response = String::from_utf8(bytes).unwrap();
            assert!(response.contains(last));
            assert!(response
                .to_ascii_lowercase()
                .contains("x-test-header: unchanged"));
        });
        downstream.await.unwrap();
        upstream.join().unwrap();
        let spans = records(&state, 1).await;
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].usage.input_tokens, Some(if protocol == "anthropic" { 9 } else { 7 }));
        assert_eq!(spans[0].usage.output_tokens, Some(3));
        assert_eq!(spans[0].usage.cache_read_input_tokens, Some(2));
        assert_eq!(spans[0].source, "proxy");
        server.abort();
    }
}
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::result_large_err)] // Tungstenite fixes the handshake callback error to an HTTP response.
async fn websocket_frames_auth_warmup_and_multiple_requests() {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let mut config = Config::default();
    config
        .proxy
        .upstream
        .insert("openai".into(), format!("http://127.0.0.1:{upstream_port}"));
    let (_temp, state, port, run, server) = fixture(config).await;
    let upstream = std::thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut ws = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request,
             response: tungstenite::handshake::server::Response| {
                assert_eq!(request.headers()["authorization"], "Bearer synthetic-token");
                assert_eq!(request.headers()["accept-encoding"], "gzip");
                assert_eq!(request.headers()["chatgpt-account-id"], "account");
                assert_eq!(
                    request.headers()["openai-beta"],
                    "responses_websockets=2026-02-06"
                );
                Ok(response)
            },
        )
        .unwrap();
        for n in 0..3 {
            let message = ws.read().unwrap();
            assert!(message.is_text());
            let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(request["type"], "response.create");
            ws.send(Message::Text(
                json!({"type":"response.created","response":{"id":format!("r{n}")}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
            ws.send(Message::Text(json!({"type":"response.completed","response":{"id":format!("r{n}"),"model":"synthetic","usage":{"input_tokens":7,"output_tokens":3},"output":[{"content":[{"text":"done"}]}]}}).to_string().into())).unwrap();
        }
        assert_eq!(ws.read().unwrap(), Message::Binary(vec![1, 2, 3].into()));
        ws.send(Message::Binary(vec![3, 2, 1].into())).unwrap();
        let _ = ws.read();
    });
    let client = tokio::task::spawn_blocking(move || {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = format!("ws://127.0.0.1:{port}/r/{run}/openai/v1/responses")
            .into_client_request()
            .unwrap();
        for (key, value) in [
            ("accept-encoding", "gzip"),
            ("authorization", "Bearer synthetic-token"),
            ("chatgpt-account-id", "account"),
            ("openai-beta", "responses_websockets=2026-02-06"),
        ] {
            request.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let (mut ws, _) = tungstenite::client(request, stream).unwrap();
        for n in 0..3 {
            ws.send(Message::Text(json!({"type":"response.create","model":"synthetic","generate":n!=0,"input":[{"role":"user","content":format!("turn {n}")}]}).to_string().into())).unwrap();
            assert!(ws
                .read()
                .unwrap()
                .to_text()
                .unwrap()
                .contains("response.created"));
            assert!(ws
                .read()
                .unwrap()
                .to_text()
                .unwrap()
                .contains("response.completed"));
        }
        ws.send(Message::Binary(vec![1, 2, 3].into())).unwrap();
        assert_eq!(ws.read().unwrap(), Message::Binary(vec![3, 2, 1].into()));
        ws.close(None).unwrap();
    });
    client.await.unwrap();
    upstream.join().unwrap();
    let spans = records(&state, 2).await;
    assert_eq!(spans.len(), 2);
    assert!(spans.iter().all(|r| r.usage.output_tokens == Some(3)));
    server.abort();
}
#[tokio::test(flavor = "current_thread")]
async fn unknown_run_and_parser_panic_are_fail_open() {
    let (_temp, state, port, _run, server) = fixture(Config::default()).await;
    let client = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"POST /r/00000000000000000000000000000000/openai/v1/responses HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
    });
    client.await.unwrap();
    assert!(crate::spool::ready(&state.paths.state).unwrap().is_empty());
    server.abort();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let mut config = Config::default();
    config
        .proxy
        .upstream
        .insert("openai".into(), format!("http://127.0.0.1:{upstream_port}"));
    let (_temp, state, port, run, server) = fixture(config).await;
    state.panic_parser.store(true, Ordering::Relaxed);
    let payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"untouched\"}}]}\n\n";
    let upstream = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        request(&mut stream);
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",payload.len()).unwrap();
        stream.write_all(payload).unwrap();
    });
    let client = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream,"POST /r/{run}/openai/v1/chat/completions HTTP/1.1\r\nHost: local\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.ends_with(payload));
    });
    client.await.unwrap();
    upstream.join().unwrap();
    assert_eq!(records(&state, 1).await.len(), 1);
    server.abort();
}
