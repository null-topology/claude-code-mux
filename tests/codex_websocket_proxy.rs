use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use claude_code_mux::logging::log_file;
use claude_code_mux::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_mux::providers::codex::websocket::{
    clear_codex_websocket_pool_for_tests, set_codex_websocket_request_write_timeout_for_tests,
};
use claude_code_mux::{registry::Registry, server::app};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as UpgradeRequest, Response as UpgradeResponse,
};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
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

/// The `codex_websocket_fallback_to_http` lines of the proxy log, oldest
/// first. Read while `XDG_STATE_HOME` points at the test's own directory.
fn fallback_log_lines() -> Vec<Value> {
    std::fs::read_to_string(log_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["msg"] == "codex_websocket_fallback_to_http")
        .collect()
}

/// The one warning a request leaves when its refused handshake falls back.
fn assert_fallback_logged(logged: &[Value], status: StatusCode, context: &str) {
    assert_eq!(logged.len(), 1, "{context}: {logged:?}");
    assert_eq!(logged[0]["level"], "warn", "{context}");
    let fields = &logged[0]["fields"];
    assert!(
        fields["reqId"].as_str().is_some_and(|id| !id.is_empty()),
        "{context}: {fields}"
    );
    assert_eq!(fields["status"], status.as_u16(), "{context}: {fields}");
    assert_eq!(
        fields.get("class"),
        Some(&Value::Null),
        "{context}: {fields}"
    );
    assert_eq!(fields["usageLimit"], false, "{context}: {fields}");
    assert_eq!(fields["retryAfter"], "7", "{context}: {fields}");
    assert!(
        fields["detail"]
            .as_str()
            .is_some_and(|detail| detail.chars().count() <= 300),
        "{context}: {fields}"
    );
}

/// A Codex origin that refuses every WebSocket upgrade with `status`,
/// `Retry-After: 7` and an empty body, and answers a POST with a complete
/// Responses stream. Records the method of each request.
async fn spawn_rejecting_websocket_origin(status: StatusCode) -> (String, Arc<Mutex<Vec<String>>>) {
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
                        .status(status)
                        .header("retry-after", "7")
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

/// A Codex origin that completes the WebSocket upgrade and then never reads.
/// Its small receive buffer keeps a large request frame from fitting in the
/// socket buffers. Records `GET` for an upgrade and `HTTP` for anything else.
async fn spawn_non_reading_websocket_origin() -> (String, Arc<Mutex<Vec<String>>>) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
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
                let Ok(_websocket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                std::future::pending::<()>().await;
            });
        }
    });
    (format!("http://{addr}/responses"), seen)
}

/// Shortens the request write budget for one test and restores it on drop.
struct RequestWriteTimeoutGuard;

impl RequestWriteTimeoutGuard {
    fn set(timeout: Duration) -> Self {
        set_codex_websocket_request_write_timeout_for_tests(Some(timeout));
        Self
    }
}

impl Drop for RequestWriteTimeoutGuard {
    fn drop(&mut self) {
        set_codex_websocket_request_write_timeout_for_tests(None);
    }
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
        while let Ok((stream, _)) = listener.accept().await {
            let seen = task_seen.clone();
            tokio::spawn(async move {
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    return;
                }
                if first[0] != b'G' {
                    record_http_request(stream, &seen).await;
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
                    let number = record_websocket_request(&seen, &text);
                    for event in websocket_reply_events(number) {
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

/// Reads one HTTP request, records its JSON body as `http` and answers it
/// with `HTTP_FALLBACK_SSE`.
async fn record_http_request(mut stream: TcpStream, seen: &SeenRequests) {
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
}

/// Records a `response.create` frame as `websocket` and returns how many
/// WebSocket requests the origin has seen, this one included.
fn record_websocket_request(seen: &SeenRequests, text: &str) -> usize {
    let mut seen = seen.lock().unwrap();
    seen.push(("websocket", serde_json::from_str(text).unwrap()));
    seen.iter()
        .filter(|(transport, _)| *transport == "websocket")
        .count()
}

/// The events of a complete answer to the Nth WebSocket request: the text
/// `websocket reply N` in response `resp_ws_N`.
fn websocket_reply_events(number: usize) -> [Value; 4] {
    [
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
    ]
}

/// The reason text of every close frame `spawn_size_limited_origin` sends. It
/// must never reach the log.
const CLOSE_REASON: &str = "close-reason-text";

/// What `spawn_size_limited_origin` does with a WebSocket request.
#[derive(Clone, Copy, Debug)]
enum WebSocketReply {
    /// The complete answer of `websocket_reply_events`.
    Complete,
    /// A close frame with this code before any response event.
    CloseBeforeText(u16),
    /// A close with no close frame, so no code, before any response event.
    CloseWithoutFrame,
    /// The first response event, then a close frame with this code.
    CloseAfterText(u16),
}

struct SizeLimitedOrigin {
    url: String,
    seen: SeenRequests,
    upgrades: Arc<AtomicUsize>,
}

/// A Codex origin like `spawn_dual_transport_origin` whose WebSocket upgrade
/// answers `x-codex-websocket-max-message-bytes: <advertised>` when one is
/// given. The Nth WebSocket request gets `replies[N - 1]`, and `Complete`
/// once the list runs out. Counts upgrades, so a test can tell a pooled
/// socket from a new one.
async fn spawn_size_limited_origin(
    advertised: Option<&'static str>,
    replies: Vec<WebSocketReply>,
) -> SizeLimitedOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: SeenRequests = Arc::new(Mutex::new(Vec::new()));
    let upgrades = Arc::new(AtomicUsize::new(0));
    let replies = Arc::new(replies);
    let task_seen = seen.clone();
    let task_upgrades = upgrades.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = task_seen.clone();
            let upgrades = task_upgrades.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    return;
                }
                if first[0] != b'G' {
                    record_http_request(stream, &seen).await;
                    return;
                }
                #[allow(clippy::result_large_err)]
                let callback = move |_: &UpgradeRequest, mut response: UpgradeResponse| {
                    if let Some(value) = advertised {
                        response.headers_mut().insert(
                            "x-codex-websocket-max-message-bytes",
                            http::HeaderValue::from_static(value),
                        );
                    }
                    Ok::<_, ErrorResponse>(response)
                };
                let Ok(mut websocket) = tokio_tungstenite::accept_hdr_async(stream, callback).await
                else {
                    return;
                };
                upgrades.fetch_add(1, Ordering::SeqCst);
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
                    let number = record_websocket_request(&seen, &text);
                    let events = websocket_reply_events(number);
                    let close = |code: u16| {
                        Message::Close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: CLOSE_REASON.into(),
                        }))
                    };
                    let reply = replies
                        .get(number - 1)
                        .copied()
                        .unwrap_or(WebSocketReply::Complete);
                    let frames: Vec<Message> = match reply {
                        WebSocketReply::Complete => events
                            .iter()
                            .map(|event| Message::Text(event.to_string()))
                            .collect(),
                        WebSocketReply::CloseBeforeText(code) => vec![close(code)],
                        WebSocketReply::CloseWithoutFrame => vec![Message::Close(None)],
                        WebSocketReply::CloseAfterText(code) => {
                            vec![Message::Text(events[0].to_string()), close(code)]
                        }
                    };
                    for frame in frames {
                        if websocket.send(frame).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    SizeLimitedOrigin {
        url: format!("http://{addr}/responses"),
        seen,
        upgrades,
    }
}

/// The proxy log lines named `msg`, oldest first. Read while `XDG_STATE_HOME`
/// points at the test's own directory.
fn log_lines_named(msg: &str) -> Vec<Value> {
    std::fs::read_to_string(log_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["msg"] == msg)
        .collect()
}

fn transports(seen: &SeenRequests) -> Vec<&'static str> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(transport, _)| *transport)
        .collect()
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
async fn codex_server_compaction_call_goes_over_http_without_the_summary_prompt() {
    let _env = ENV_LOCK.lock().await;
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
    let (origin_url, seen) = spawn_dual_transport_origin().await;
    let mut guards = clear_proxy_environment();
    // `configure_codex` pins the transport to `websocket`.
    guards.extend(configure_codex(config_dir.path(), &origin_url));
    guards.push(EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "1"));
    let reminder = "<system-reminder>Context.</system-reminder>";
    let system_note = "Injected system note.";

    let (status, body) = call_messages_with_history(
        "server-compaction-transport",
        false,
        json!([
            {"role": "user", "content": "one"},
            {"role": "assistant", "content": "earlier reply"},
            {"role": "user", "content": concat!(
                "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n",
                "Your task is to create a detailed summary of the conversation so far."
            )},
            {"role": "user", "content": reminder},
            {"role": "system", "content": system_note}
        ]),
    )
    .await;
    drop(guards);
    // The origin answers the compaction call with an ordinary message, so the
    // compaction fails and the summary request is sent on its own as before.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("transport fallback ok"), "{body}");

    let seen = seen.lock().unwrap();
    let transports: Vec<_> = seen.iter().map(|(transport, _)| *transport).collect();
    assert_eq!(transports, ["http", "http"]);
    let compaction = &seen[0].1;
    let compaction_input = compaction["input"].as_array().unwrap();
    // The prompt's own message is gone; the reminder and the system message
    // after it stay, in order, before the trigger.
    let tail: Vec<_> = compaction_input
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|item| {
            (
                item["type"].as_str().unwrap_or_default(),
                item["role"].as_str().unwrap_or_default(),
                item["content"][0]["text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            ("message", "assistant", "earlier reply"),
            ("message", "user", reminder),
            ("message", "developer", system_note),
            ("compaction_trigger", "", ""),
        ],
        "{compaction}"
    );
    let compaction_text = compaction.to_string();
    assert!(
        !compaction_text.contains("Your task is to create a detailed summary"),
        "{compaction_text}"
    );
    assert!(
        seen[1]
            .1
            .to_string()
            .contains("Your task is to create a detailed summary")
    );
    drop(seen);
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
}

#[tokio::test]
async fn codex_default_transport_falls_back_to_http_after_a_rate_limited_handshake() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    for stream in [false, true] {
        clear_codex_websocket_pool_for_tests();
        let (origin_url, seen) =
            spawn_rejecting_websocket_origin(StatusCode::TOO_MANY_REQUESTS).await;
        let state_dir = TempDir::new().unwrap();
        let (status, body, logged) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
            let (status, body) =
                call_messages_with_stream("transport-default-rate-limited", stream).await;
            (status, body, fallback_log_lines())
        };
        assert_eq!(status, StatusCode::OK, "stream {stream}: {body}");
        assert!(
            body.contains("transport fallback ok"),
            "stream {stream}: {body}"
        );
        assert_eq!(*seen.lock().unwrap(), ["GET", "POST"], "stream {stream}");
        assert_fallback_logged(
            &logged,
            StatusCode::TOO_MANY_REQUESTS,
            &format!("stream {stream}"),
        );
    }

    // With `websocket` the handshake's 429 reaches the client as it came.
    clear_codex_websocket_pool_for_tests();
    let (origin_url, seen) = spawn_rejecting_websocket_origin(StatusCode::TOO_MANY_REQUESTS).await;
    let state_dir = TempDir::new().unwrap();
    let (status, body, logged) = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &origin_url));
        guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
        let (status, body) = call_messages("transport-explicit-websocket-rate-limited").await;
        (status, body, fallback_log_lines())
    };
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(*seen.lock().unwrap(), ["GET"]);
    assert!(logged.is_empty(), "{logged:?}");
    clear_codex_websocket_pool_for_tests();
}

#[tokio::test]
async fn codex_default_transport_falls_back_to_http_only_before_sending() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    for stream in [false, true] {
        clear_codex_websocket_pool_for_tests();
        let (origin_url, seen) = spawn_rejecting_websocket_origin(StatusCode::FORBIDDEN).await;
        let state_dir = TempDir::new().unwrap();
        let (status, body, logged) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
            let (status, body) =
                call_messages_with_stream("transport-default-fallback", stream).await;
            (status, body, fallback_log_lines())
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
        assert_fallback_logged(&logged, StatusCode::FORBIDDEN, &format!("stream {stream}"));
    }

    clear_codex_websocket_pool_for_tests();
    let (origin_url, seen) = spawn_rejecting_websocket_origin(StatusCode::FORBIDDEN).await;
    let state_dir = TempDir::new().unwrap();
    let (status, logged) = {
        let mut guards = clear_proxy_environment();
        guards.extend(configure_codex(config_dir.path(), &origin_url));
        guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
        let (status, _) = call_messages("transport-explicit-websocket").await;
        (status, fallback_log_lines())
    };
    assert!(!status.is_success());
    assert_eq!(*seen.lock().unwrap(), ["GET", "GET"]);
    assert!(logged.is_empty(), "{logged:?}");

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
async fn codex_default_transport_does_not_replay_a_timed_out_request_write_over_http() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let _write_timeout = RequestWriteTimeoutGuard::set(Duration::from_millis(500));
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());
    // Large enough that the request frame cannot fit in the socket buffers.
    // The response wait keeps its full idle budget, longer than the outer
    // timeout below, so only the write deadline can end the request in time.
    let messages = json!([{"role": "user", "content": "word ".repeat(16 * 1024 * 1024 / 5)}]);

    for stream in [false, true] {
        clear_codex_websocket_pool_for_tests();
        let (origin_url, seen) = spawn_non_reading_websocket_origin().await;
        let state_dir = TempDir::new().unwrap();
        let (status, body, logged) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin_url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
            let (status, body) = tokio::time::timeout(
                Duration::from_secs(60),
                call_messages_with_history(
                    "transport-default-stalled-write",
                    stream,
                    messages.clone(),
                ),
            )
            .await
            .expect("a request write the peer never reads must end within its budget");
            (status, body, fallback_log_lines())
        };
        assert!(!status.is_success(), "stream {stream}: {body}");
        assert_eq!(*seen.lock().unwrap(), ["GET"], "stream {stream}");
        assert!(logged.is_empty(), "stream {stream}: {logged:?}");
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

/// The limit `spawn_size_limited_origin` advertises in the size tests. A
/// request holding `oversized_text` is over it, a short conversation is not.
const ADVERTISED_LIMIT: &str = "100000";

fn oversized_text() -> String {
    "oversized ".repeat(15_000)
}

/// The fallback warning of a request kept off the socket by the limit: its
/// detail names the frame length and the limit, and nothing from the request.
fn assert_oversized_fallback_logged(line: &Value, context: &str) {
    let fields = &line["fields"];
    assert_eq!(line["level"], "warn", "{context}");
    assert!(
        fields["reqId"].as_str().is_some_and(|id| !id.is_empty()),
        "{context}: {fields}"
    );
    assert_eq!(fields["status"], 413, "{context}: {fields}");
    let detail = fields["detail"].as_str().unwrap();
    let frame_bytes = detail
        .strip_prefix("The WebSocket request frame is ")
        .and_then(|rest| {
            rest.strip_suffix(
                " bytes, over the 100000-byte message limit the Codex backend advertised",
            )
        })
        .and_then(|bytes| bytes.parse::<usize>().ok());
    assert!(
        frame_bytes.is_some_and(|bytes| bytes > 100_000),
        "{context}: {detail}"
    );
    assert!(!line.to_string().contains("oversized"), "{context}: {line}");
}

#[tokio::test]
async fn codex_default_transport_sends_a_request_over_the_advertised_limit_over_http() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());
    let big = oversized_text();

    for (stream, continuations) in [(false, false), (true, false), (false, true), (true, true)] {
        let label = format!("stream {stream}, continuations {continuations}");
        clear_codex_websocket_pool_for_tests();
        clear_all_continuations_for_tests();
        let origin = spawn_size_limited_origin(Some(ADVERTISED_LIMIT), vec![]).await;
        let state_dir = TempDir::new().unwrap();
        let (fallbacks, limits) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin.url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            if continuations {
                guards.push(EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1"));
            }
            guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
            let turns = [
                // Over the limit on a new socket.
                (
                    json!([{"role": "user", "content": big}]),
                    "transport fallback ok",
                ),
                // Under the limit, answered over the WebSocket.
                (
                    json!([{"role": "user", "content": "one"}]),
                    "websocket reply 1",
                ),
                // Over the limit on the pooled socket, continuing `resp_ws_1`.
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": big}
                    ]),
                    "transport fallback ok",
                ),
                // The oversized turn rewound: `resp_ws_1` still matches the
                // prefix, but it predates the request answered over HTTP.
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": "three"}
                    ]),
                    "websocket reply 2",
                ),
            ];
            for (messages, reply) in turns {
                let (status, body) = call_messages_with_history(
                    &format!("size-limit-oversized-{stream}-{continuations}"),
                    stream,
                    messages,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{label}: {body}");
                assert!(body.contains(reply), "{label}: {body}");
            }
            (
                fallback_log_lines(),
                log_lines_named("codex_websocket_message_limit"),
            )
        };

        // The WebSocket received no oversized frame. Without continuations no
        // socket is pooled, so each request opens one. With them, the third
        // request takes the second one's pooled socket, which is dropped once
        // its frame is refused, and the other three, having nothing to
        // continue, each start on a new socket.
        assert_eq!(
            transports(&origin.seen),
            ["http", "websocket", "http", "websocket"],
            "{label}"
        );
        let sockets = if continuations { 3 } else { 4 };
        assert_eq!(origin.upgrades.load(Ordering::SeqCst), sockets, "{label}");
        // With continuations the third request continued `resp_ws_1`; over
        // HTTP it went with full context, and the last one continues nothing
        // that predates the HTTP answer.
        let seen = origin.seen.lock().unwrap();
        for (index, (_, request)) in seen.iter().enumerate() {
            assert!(
                request["previous_response_id"].is_null(),
                "{label}: request {index}"
            );
        }
        assert!(seen[0].1["input"].to_string().contains(&big), "{label}");
        let full = seen[2].1["input"].to_string();
        assert!(
            full.contains("websocket reply 1") && full.contains(&big),
            "{label}"
        );
        assert!(seen[3].1["input"].to_string().contains("three"), "{label}");

        assert_eq!(fallbacks.len(), 2, "{label}: {fallbacks:?}");
        for line in &fallbacks {
            assert_oversized_fallback_logged(line, &label);
        }
        assert_eq!(limits.len(), sockets, "{label}: {limits:?}");
        for line in &limits {
            assert_eq!(line["fields"]["maxMessageBytes"], 100_000, "{label}");
            assert!(
                line["fields"]["reqId"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()),
                "{label}"
            );
        }
    }
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
}

#[tokio::test]
async fn codex_default_transport_sends_a_request_closed_with_1009_over_http() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    for stream in [false, true] {
        let label = format!("stream {stream}");
        clear_codex_websocket_pool_for_tests();
        clear_all_continuations_for_tests();
        let origin = spawn_size_limited_origin(
            None,
            vec![
                WebSocketReply::Complete,
                WebSocketReply::CloseBeforeText(1009),
            ],
        )
        .await;
        let state_dir = TempDir::new().unwrap();
        let (fallbacks, closes) = {
            let mut guards = clear_proxy_environment();
            guards.extend(configure_codex(config_dir.path(), &origin.url));
            guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
            guards.push(EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1"));
            guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
            let turns = [
                (
                    json!([{"role": "user", "content": "one"}]),
                    "websocket reply 1",
                ),
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": "two"}
                    ]),
                    "transport fallback ok",
                ),
                (
                    json!([
                        {"role": "user", "content": "one"},
                        {"role": "assistant", "content": "websocket reply 1"},
                        {"role": "user", "content": "three"}
                    ]),
                    "websocket reply 3",
                ),
            ];
            for (messages, reply) in turns {
                let (status, body) = call_messages_with_history(
                    &format!("size-limit-closed-{stream}"),
                    stream,
                    messages,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{label}: {body}");
                assert!(body.contains(reply), "{label}: {body}");
            }
            (
                fallback_log_lines(),
                log_lines_named("codex_websocket_closed"),
            )
        };

        // The refused frame went out once, then the request once over HTTP
        // with its full context, and nothing continues from before it.
        assert_eq!(
            transports(&origin.seen),
            ["websocket", "websocket", "http", "websocket"],
            "{label}"
        );
        let seen = origin.seen.lock().unwrap();
        assert_eq!(seen[1].1["previous_response_id"], "resp_ws_1", "{label}");
        assert!(seen[2].1["previous_response_id"].is_null(), "{label}");
        let full = seen[2].1["input"].to_string();
        assert!(
            full.contains("websocket reply 1") && full.contains("two"),
            "{label}: {full}"
        );
        assert!(seen[3].1["previous_response_id"].is_null(), "{label}");

        assert_eq!(fallbacks.len(), 1, "{label}: {fallbacks:?}");
        let fields = &fallbacks[0]["fields"];
        assert_eq!(fields["status"], 413, "{label}: {fields}");
        assert_eq!(
            fields["detail"],
            "The Codex backend closed the WebSocket with code 1009 (message too big) before any response",
            "{label}"
        );
        assert!(
            !fallbacks[0].to_string().contains("two"),
            "{label}: {fields}"
        );
        assert_eq!(closes.len(), 1, "{label}: {closes:?}");
        assert_eq!(closes[0]["fields"]["closeCode"], 1009, "{label}");
        assert_eq!(closes[0]["fields"]["textStarted"], false, "{label}");
        assert!(!closes[0].to_string().contains(CLOSE_REASON), "{label}");
    }
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
}

#[tokio::test]
async fn codex_websocket_transport_answers_a_size_refusal_once_with_413() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());
    let big = oversized_text();

    for stream in [false, true] {
        for (advertised, reply, messages, expected) in [
            (
                Some(ADVERTISED_LIMIT),
                WebSocketReply::Complete,
                json!([{"role": "user", "content": big}]),
                "-byte message limit the Codex backend advertised",
            ),
            (
                None,
                WebSocketReply::CloseBeforeText(1009),
                json!([{"role": "user", "content": "hello"}]),
                "The Codex backend closed the WebSocket with code 1009 (message too big) before any response",
            ),
        ] {
            let label = format!("stream {stream}, advertised {advertised:?}");
            clear_codex_websocket_pool_for_tests();
            clear_all_continuations_for_tests();
            let origin = spawn_size_limited_origin(advertised, vec![reply]).await;
            let state_dir = TempDir::new().unwrap();
            let (status, body, fallbacks) = {
                let mut guards = clear_proxy_environment();
                guards.extend(configure_codex(config_dir.path(), &origin.url));
                guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
                let (status, body) = call_messages_with_history(
                    &format!("size-limit-websocket-{stream}"),
                    stream,
                    messages,
                )
                .await;
                (status, body, fallback_log_lines())
            };
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{label}: {body}");
            let error: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["error"]["type"], "request_too_large", "{label}");
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.contains(expected), "{label}: {message}");
            assert!(
                message.ends_with("The auto transport sends such a request over HTTP."),
                "{label}: {message}"
            );
            assert!(!message.contains("oversized"), "{label}: {message}");
            // Nothing over HTTP and nothing resent: an oversized frame is never
            // written, and a refused one is written once.
            let expected_transports: &[&str] = match reply {
                WebSocketReply::CloseBeforeText(_) => &["websocket"],
                _ => &[],
            };
            assert_eq!(transports(&origin.seen), expected_transports, "{label}");
            assert_eq!(origin.upgrades.load(Ordering::SeqCst), 1, "{label}");
            assert!(fallbacks.is_empty(), "{label}: {fallbacks:?}");
        }
    }
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();
}

#[tokio::test]
async fn codex_default_transport_keeps_its_handling_of_other_closes() {
    let _env = ENV_LOCK.lock().await;
    let _retry_delay = ZeroRetryDelayGuard::new();
    let config_dir = TempDir::new().unwrap();
    write_codex_auth(config_dir.path());

    for (reply, code, text_started) in [
        (WebSocketReply::CloseAfterText(1009), 1009, true),
        (WebSocketReply::CloseBeforeText(1000), 1000, false),
        (WebSocketReply::CloseBeforeText(1011), 1011, false),
        (WebSocketReply::CloseWithoutFrame, 0, false),
    ] {
        for stream in [false, true] {
            let label = format!("{reply:?}, stream {stream}");
            clear_codex_websocket_pool_for_tests();
            clear_all_continuations_for_tests();
            let origin = spawn_size_limited_origin(None, vec![reply]).await;
            let state_dir = TempDir::new().unwrap();
            let (status, body, fallbacks, closes, started) = {
                let mut guards = clear_proxy_environment();
                guards.extend(configure_codex(config_dir.path(), &origin.url));
                guards.push(EnvGuard::unset("CCP_CODEX_TRANSPORT"));
                guards.push(EnvGuard::set("XDG_STATE_HOME", state_dir.path()));
                let (status, body) = call_messages_with_history(
                    &format!("size-limit-other-close-{code}-{stream}"),
                    stream,
                    json!([{"role": "user", "content": "hello"}]),
                )
                .await;
                (
                    status,
                    body,
                    fallback_log_lines(),
                    log_lines_named("codex_websocket_closed"),
                    log_lines_named("codex_upstream_request_started"),
                )
            };
            // Sent once, not over HTTP, and answered as a response that never
            // finished, as before.
            assert_eq!(transports(&origin.seen), ["websocket"], "{label}: {body}");
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{label}: {body}");
            assert!(body.contains("api_error"), "{label}: {body}");
            assert!(!body.contains("transport fallback ok"), "{label}: {body}");
            assert!(!body.contains("request_too_large"), "{label}: {body}");
            assert!(fallbacks.is_empty(), "{label}: {fallbacks:?}");
            assert!(!closes.is_empty(), "{label}");
            assert_eq!(closes[0]["fields"]["closeCode"], code, "{label}");
            assert_eq!(closes[0]["fields"]["textStarted"], text_started, "{label}");
            // The close names the request it ended.
            assert_eq!(started.len(), 1, "{label}");
            assert!(started[0]["fields"]["reqId"].is_string(), "{label}");
            assert_eq!(
                closes[0]["fields"]["reqId"], started[0]["fields"]["reqId"],
                "{label}"
            );
            for line in &closes {
                assert!(!line.to_string().contains(CLOSE_REASON), "{label}");
            }
        }
    }
    clear_codex_websocket_pool_for_tests();
}
