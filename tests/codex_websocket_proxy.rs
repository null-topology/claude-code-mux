use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_code_mux::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_mux::providers::codex::websocket::clear_codex_websocket_pool_for_tests;
use claude_code_mux::{registry::Registry, server::app};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tower::util::ServiceExt;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct ZeroRetryDelayGuard;

impl ZeroRetryDelayGuard {
    fn new() -> Self {
        claude_code_mux::retry::set_zero_retry_delay_for_tests(true);
        Self
    }
}

impl Drop for ZeroRetryDelayGuard {
    fn drop(&mut self) {
        claude_code_mux::retry::set_zero_retry_delay_for_tests(false);
    }
}

fn clear_proxy_environment() -> Vec<EnvGuard> {
    [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "REQUEST_METHOD",
    ]
    .into_iter()
    .map(EnvGuard::unset)
    .collect()
}

fn configure_codex(config_dir: &Path, base_url: &str) -> Vec<EnvGuard> {
    vec![
        EnvGuard::set("CCP_CONFIG_DIR", config_dir),
        EnvGuard::set("CCP_CODEX_AUTH_FILE", config_dir.join("codex/auth.json")),
        EnvGuard::set("CCP_ALIAS_PROVIDER", "codex"),
        EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket"),
        EnvGuard::set("CCP_CODEX_BASE_URL", base_url),
        EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "0"),
        EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "0"),
    ]
}

fn write_codex_auth(config_dir: &Path) {
    let dir = config_dir.join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&json!({
            "tokens": {
                "access_token": "test-access",
                "refresh_token": "test-refresh",
                "account_id": "acct_test"
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn call_messages(session_id: &str) -> (StatusCode, String) {
    call_messages_with_stream(session_id, false).await
}

async fn call_messages_with_stream(session_id: &str, stream: bool) -> (StatusCode, String) {
    call_messages_with_history(
        session_id,
        stream,
        json!([{"role": "user", "content": "hello"}]),
    )
    .await
}

async fn call_messages_with_history(
    session_id: &str,
    stream: bool,
    messages: Value,
) -> (StatusCode, String) {
    let response = app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", session_id)
                .body(Body::from(
                    json!({
                        "model": "gpt-5.6-sol",
                        "max_tokens": 64,
                        "stream": stream,
                        "messages": messages
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn send_codex_response(websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>) {
    let request = websocket.next().await.unwrap().unwrap();
    assert!(matches!(request, Message::Text(_)));
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_proxy"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"proxy env ok"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_proxy","usage":{"input_tokens":5,"output_tokens":2}}}"#,
    ] {
        websocket
            .send(Message::Text(event.to_string()))
            .await
            .unwrap();
    }
}

#[derive(Debug)]
struct CapturedProxyRequest {
    target: String,
    has_proxy_authorization: bool,
}

#[allow(clippy::result_large_err)]
async fn spawn_websocket_proxy() -> (
    String,
    oneshot::Receiver<CapturedProxyRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let websocket = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response| {
                let _ = captured_tx.send(CapturedProxyRequest {
                    target: request.uri().to_string(),
                    has_proxy_authorization: request
                        .headers()
                        .contains_key(http::header::PROXY_AUTHORIZATION),
                });
                Ok(response)
            },
        )
        .await
        .unwrap();
        let mut websocket = websocket;
        send_codex_response(&mut websocket).await;
    });
    (
        format!("http://proxy-user:proxy-pass@{addr}"),
        captured_rx,
        task,
    )
}

async fn spawn_direct_websocket() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        send_codex_response(&mut websocket).await;
    });
    (format!("http://{addr}/responses"), task)
}

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        assert!(request.len() <= 16 * 1024);
    }
    String::from_utf8(request).unwrap()
}

async fn spawn_rejecting_proxy(
    response: &'static [u8],
) -> (
    String,
    Arc<Mutex<Vec<CapturedProxyRequest>>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let task_captured = captured.clone();
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                break;
            };
            let request = read_http_head(&mut stream).await;
            let mut lines = request.lines();
            let target = lines.next().unwrap_or_default().to_string();
            let has_proxy_authorization = lines.any(|line| {
                line.split_once(':')
                    .is_some_and(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
            });
            task_captured.lock().unwrap().push(CapturedProxyRequest {
                target,
                has_proxy_authorization,
            });
            stream.write_all(response).await.unwrap();
        }
    });
    (
        format!("http://secret-user:secret-pass@{addr}"),
        captured,
        stop_tx,
        task,
    )
}

const HTTP_FALLBACK_SSE: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_http\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"transport fallback ok\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
);

/// A Codex origin that refuses every WebSocket upgrade with 403 and answers a
/// POST with a complete Responses stream. Records the method of each request.
async fn spawn_rejecting_websocket_origin() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new().fallback({
        let seen = seen.clone();
        move |request: Request<Body>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(request.method().to_string());
                if request.headers().contains_key(http::header::UPGRADE) {
                    return http::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::empty())
                        .unwrap();
                }
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(HTTP_FALLBACK_SSE))
                    .unwrap()
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/responses"), seen)
}

/// A Codex origin that accepts the WebSocket, reads `response.create` and
/// drops the connection before any event. Records `GET` for an upgrade,
/// `response.create` once the request arrived, and `HTTP` for anything else.
async fn spawn_closing_websocket_origin() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let task_seen = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = task_seen.clone();
            tokio::spawn(async move {
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    return;
                }
                if first[0] != b'G' {
                    seen.lock().unwrap().push("HTTP".to_string());
                    return;
                }
                seen.lock().unwrap().push("GET".to_string());
                let Ok(mut websocket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                if let Some(Ok(Message::Text(_))) = websocket.next().await {
                    seen.lock().unwrap().push("response.create".to_string());
                }
            });
        }
    });
    (format!("http://{addr}/responses"), seen)
}

type SeenRequests = Arc<Mutex<Vec<(&'static str, Value)>>>;

/// A Codex origin that serves both transports and records each request as its
/// transport and JSON body. The Nth `response.create` on any WebSocket is
/// answered as `resp_ws_N` with the text `websocket reply N`; an HTTP POST
/// gets `HTTP_FALLBACK_SSE`.
async fn spawn_dual_transport_origin() -> (String, SeenRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: SeenRequests = Arc::new(Mutex::new(Vec::new()));
    let task_seen = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = task_seen.clone();
            tokio::spawn(async move {
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    return;
                }
                if first[0] != b'G' {
                    let request = read_http_head(&mut stream).await;
                    let (head, partial) = request.split_once("\r\n\r\n").unwrap();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let mut body = partial.as_bytes().to_vec();
                    let mut buffer = [0_u8; 8192];
                    while body.len() < length {
                        let read = stream.read(&mut buffer).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        body.extend_from_slice(&buffer[..read]);
                    }
                    let body = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push(("http", body));
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{HTTP_FALLBACK_SSE}",
                        HTTP_FALLBACK_SSE.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    return;
                }
                let Ok(mut websocket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = websocket.next().await {
                    let text = match message {
                        Message::Text(text) => text,
                        Message::Ping(data) => {
                            let _ = websocket.send(Message::Pong(data)).await;
                            continue;
                        }
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let number = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(("websocket", serde_json::from_str(&text).unwrap()));
                        seen.iter()
                            .filter(|(transport, _)| *transport == "websocket")
                            .count()
                    };
                    for event in [
                        json!({
                            "type": "response.output_item.added",
                            "output_index": 0,
                            "item": {"type": "message", "id": format!("msg_ws_{number}")}
                        }),
                        json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "delta": format!("websocket reply {number}")
                        }),
                        json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {"type": "message"}
                        }),
                        json!({
                            "type": "response.completed",
                            "response": {
                                "id": format!("resp_ws_{number}"),
                                "usage": {"input_tokens": 5, "output_tokens": 2}
                            }
                        }),
                    ] {
                        if websocket
                            .send(Message::Text(event.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });
    (format!("http://{addr}/responses"), seen)
}

#[tokio::test]
async fn codex_compaction_summary_goes_over_http_and_the_next_request_stays_on_websocket() {
    let _env = ENV_LOCK.lock().await;
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());
    let summary_prompt = concat!(
        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n",
        "Your task is to create a detailed summary of the conversation so far."
    );

    for transport in [None, Some("websocket")] {
        for stream in [false, true] {
            let label = format!("transport {transport:?}, stream {stream}");
            clear_codex_websocket_pool_for_tests();
            clear_all_continuations_for_tests();
            let (origin_url, seen) = spawn_dual_transport_origin().await;
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(match transport {
                Some(transport) => EnvGuard::set("CCP_CODEX_TRANSPORT", transport),
                None => EnvGuard::unset("CCP_CODEX_TRANSPORT"),
            });
            guards.push(EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1"));

            let turns = [
                (
                    json!([{"role": "user", "content": "one"}]),
                    "websocket reply 1",
                ),
                (
                    // Claude Code can send context after its summary prompt.
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": summary_prompt},
                        {"role": "user", "content": "<system-reminder>Context.</system-reminder>"}
                    ]),
                    "transport fallback ok",
                ),
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": "two"}
                    ]),
                    "websocket reply 2",
                ),
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": "two"},
                        {"role": "assistant", "content": "websocket reply 2"},
                        {"role": "user", "content": "three"}
                    ]),
                    "websocket reply 3",
                ),
            ];
            for (messages, reply) in turns {
                let (status, body) =
                    call_messages_with_history("compaction-transport", stream, messages).await;
                assert_eq!(status, StatusCode::OK, "{label}: {body}");
                assert!(body.contains(reply), "{label}: {body}");
            }
            drop(guards);

            let seen = seen.lock().unwrap();
            let transports: Vec<_> = seen.iter().map(|(transport, _)| *transport).collect();
            assert_eq!(
                transports,
                ["websocket", "http", "websocket", "websocket"],
                "{label}"
            );
            // The summary goes out whole, with no continuation.
            assert!(seen[1].1["previous_response_id"].is_null(), "{label}");
            // It leaves no continuation behind, so the next request sends the
            // full context, and the one after continues from that request.
            assert!(seen[2].1["previous_response_id"].is_null(), "{label}");
            let full = seen[2].1["input"].to_string();
            assert!(full.contains("websocket reply 1"), "{label}: {full}");
            assert_eq!(seen[3].1["previous_response_id"], "resp_ws_2", "{label}");
            let delta = seen[3].1["input"].to_string();
            assert!(
                delta.contains("three") && !delta.contains("websocket reply"),
                "{label}: {delta}"
            );
        }
    }
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
}

#[tokio::test]
async fn codex_default_transport_falls_back_to_http_only_before_sending() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    for stream in [false, true] {
        clear_codex_websocket_pool_for_tests();
        let (origin_url, seen) = spawn_rejecting_websocket_origin().await;
        let (status, body) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            call_messages_with_stream("transport-default-fallback", stream).await
        };
        assert_eq!(status, StatusCode::OK, "stream {stream}: {body}");
        assert!(
            body.contains("transport fallback ok"),
            "stream {stream}: {body}"
        );
        // The refused handshake is tried once more before HTTP takes over.
        assert_eq!(
            *seen.lock().unwrap(),
            ["GET", "GET", "POST"],
            "stream {stream}"
        );
    }

    clear_codex_websocket_pool_for_tests();
    let (origin_url, seen) = spawn_rejecting_websocket_origin().await;
    let (status, _) = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &origin_url));
        call_messages("transport-explicit-websocket").await
    };
    assert!(!status.is_success());
    assert_eq!(*seen.lock().unwrap(), ["GET", "GET"]);

    for stream in [false, true] {
        clear_codex_websocket_pool_for_tests();
        let (origin_url, seen) = spawn_closing_websocket_origin().await;
        let (status, body) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            call_messages_with_stream("transport-default-in-flight", stream).await
        };
        assert!(
            !body.contains("transport fallback ok"),
            "stream {stream}: {body}"
        );
        if !stream {
            assert!(!status.is_success());
        }
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|event| event == "response.create"),
            "stream {stream}: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| event == "HTTP"),
            "stream {stream}: {seen:?}"
        );
    }
    clear_codex_websocket_pool_for_tests();
}

#[tokio::test]
async fn codex_websocket_inherits_environment_proxy_configuration() {
    let _env = ENV_LOCK.lock().await;
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    clear_codex_websocket_pool_for_tests();
    let (proxy_url, captured, proxy_task) = spawn_websocket_proxy().await;
    let all_proxy_trap = TcpListener::bind("127.0.0.1:0").await.unwrap();
    {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(
            config_dir.path(),
            "http://codex.invalid/backend-api/codex/responses",
        ));
        guards.push(EnvGuard::set("HTTP_PROXY", &proxy_url));
        guards.push(EnvGuard::set(
            "ALL_PROXY",
            format!("http://{}", all_proxy_trap.local_addr().unwrap()),
        ));
        let (status, body) = call_messages("proxy-env-http").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("proxy env ok"));
    }
    let captured = captured.await.unwrap();
    assert_eq!(
        captured.target,
        "http://codex.invalid/backend-api/codex/responses"
    );
    assert!(captured.has_proxy_authorization);
    proxy_task.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), all_proxy_trap.accept())
            .await
            .is_err()
    );

    clear_codex_websocket_pool_for_tests();
    let (all_proxy_url, captured, proxy_task) = spawn_websocket_proxy().await;
    {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(
            config_dir.path(),
            "http://all-proxy.invalid/backend-api/codex/responses",
        ));
        guards.push(EnvGuard::set("ALL_PROXY", &all_proxy_url));
        let (status, body) = call_messages("proxy-env-all").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("proxy env ok"));
    }
    let captured = captured.await.unwrap();
    assert_eq!(
        captured.target,
        "http://all-proxy.invalid/backend-api/codex/responses"
    );
    proxy_task.await.unwrap();

    clear_codex_websocket_pool_for_tests();
    let (direct_url, direct_task) = spawn_direct_websocket().await;
    let cgi_proxy_trap = TcpListener::bind("127.0.0.1:0").await.unwrap();
    {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &direct_url));
        guards.push(EnvGuard::set(
            "HTTP_PROXY",
            format!("http://{}", cgi_proxy_trap.local_addr().unwrap()),
        ));
        guards.push(EnvGuard::set("REQUEST_METHOD", "GET"));
        let (status, body) = call_messages("proxy-env-cgi-safe").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("proxy env ok"));
    }
    direct_task.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), cgi_proxy_trap.accept())
            .await
            .is_err()
    );

    clear_codex_websocket_pool_for_tests();
    let (direct_url, direct_task) = spawn_direct_websocket().await;
    let trap_proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &direct_url));
        guards.push(EnvGuard::set(
            "HTTP_PROXY",
            format!("http://{}", trap_proxy.local_addr().unwrap()),
        ));
        guards.push(EnvGuard::set("NO_PROXY", "127.0.0.1"));
        let (status, body) = call_messages("proxy-env-no-proxy").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("proxy env ok"));
    }
    direct_task.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), trap_proxy.accept())
            .await
            .is_err()
    );

    clear_codex_websocket_pool_for_tests();
    let (https_proxy_url, captured, stop, proxy_task) = spawn_rejecting_proxy(
        b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let (status, body) = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(
            config_dir.path(),
            "https://codex.invalid:4443/backend-api/codex/responses",
        ));
        guards.push(EnvGuard::set("HTTPS_PROXY", &https_proxy_url));
        call_messages("proxy-env-https").await
    };
    let _ = stop.send(());
    proxy_task.await.unwrap();
    assert!(!status.is_success());
    assert!(!body.contains("secret-user"));
    assert!(!body.contains("secret-pass"));
    {
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(
            captured
                .iter()
                .all(|request| request.target.starts_with("CONNECT codex.invalid:4443 "))
        );
        assert!(
            captured
                .iter()
                .all(|request| request.has_proxy_authorization)
        );
    }

    clear_codex_websocket_pool_for_tests();
    let (https_proxy_url, captured, stop, proxy_task) =
        spawn_rejecting_proxy(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
    let status = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(
            config_dir.path(),
            "https://codex.invalid:4443/backend-api/codex/responses",
        ));
        guards.push(EnvGuard::set("HTTPS_PROXY", &https_proxy_url));
        call_messages("proxy-env-connect-rejected").await.0
    };
    let _ = stop.send(());
    proxy_task.await.unwrap();
    assert!(!status.is_success());
    assert_eq!(captured.lock().unwrap().len(), 1);

    clear_codex_websocket_pool_for_tests();
    let (https_proxy_url, captured, stop, proxy_task) =
        spawn_rejecting_proxy(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
    {
        let _retry_delay = ZeroRetryDelayGuard::new();
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(
            config_dir.path(),
            "https://codex.invalid:4443/backend-api/codex/responses",
        ));
        guards.push(EnvGuard::set("HTTPS_PROXY", &https_proxy_url));
        let _ = call_messages_with_stream("proxy-env-live-connect-rejected", true).await;
    }
    let _ = stop.send(());
    proxy_task.await.unwrap();
    assert_eq!(captured.lock().unwrap().len(), 1);

    clear_codex_websocket_pool_for_tests();
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_url = format!("http://{}/responses", origin.local_addr().unwrap());
    let (failing_proxy_url, captured, stop, proxy_task) = spawn_rejecting_proxy(
        b"HTTP/1.1 502 Bad Gateway\r\nRetry-After: 120\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let (status, _) = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &origin_url));
        guards.push(EnvGuard::set("HTTP_PROXY", &failing_proxy_url));
        guards.push(EnvGuard::set("CCP_CODEX_TRANSPORT", "auto"));
        call_messages("proxy-env-no-direct-fallback").await
    };
    let _ = stop.send(());
    proxy_task.await.unwrap();
    assert!(!status.is_success());
    {
        let captured = captured.lock().unwrap();
        assert!(
            captured
                .iter()
                .any(|request| request.target.starts_with("GET http://"))
        );
        assert!(
            captured
                .iter()
                .any(|request| request.target.starts_with("POST http://"))
        );
        assert!(
            captured
                .iter()
                .all(|request| request.has_proxy_authorization)
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), origin.accept())
            .await
            .is_err()
    );

    clear_codex_websocket_pool_for_tests();
}
