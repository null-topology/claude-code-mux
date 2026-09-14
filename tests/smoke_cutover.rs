// End-to-end tests for local server health, provider routing, Kimi, Codex HTTP,
// and Codex WebSocket through in-process mock upstreams with isolated auth.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use claude_code_mux::providers::codex::compaction::clear_all_compactions_for_tests;
use claude_code_mux::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_mux::providers::codex::websocket::clear_codex_websocket_pool_for_tests;
use claude_code_mux::{
    config::AliasProvider,
    monitor::{MonitorHandle, QualityFields, RequestStatus, UsageQuality},
    registry::Registry,
    server::{app, app_with_monitor, app_with_options},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialize all env-var-mutating tests so they never run concurrently.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    // Recover from a poisoned mutex so a failing test doesn't cascade
    let m = ENV_LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Write a valid Kimi auth.json under `config_dir`.
fn write_kimi_auth(config_dir: &std::path::Path) {
    let dir = config_dir.join("kimi");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = json!({
        "access": "test-access",
        "refresh": "test-refresh",
        "expires": 4102444800000_i64,
        "scope": "openid",
        "userId": "user_test"
    });
    std::fs::write(dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
}

/// Write a Codex CLI style `auth.json` and point the proxy at it for the test's
/// lifetime. The proxy reads Codex credentials from the Codex CLI file, not the
/// proxy config dir, so tests must set `CCP_CODEX_AUTH_FILE` to stay hermetic
/// instead of falling back to the developer's real `~/.codex/auth.json`.
fn write_codex_auth(config_dir: &std::path::Path) -> EnvGuard {
    let dir = config_dir.join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    let auth = json!({
        "tokens": {
            "access_token": "test-access",
            "refresh_token": "test-refresh",
            "account_id": "acct_test"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    EnvGuard::set("CCP_CODEX_AUTH_FILE", path)
}

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

    /// Remove a variable for the test's lifetime, restoring it on drop, so a
    /// value in the developer's shell cannot steer what the proxy sends.
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

/// Send a minimal `POST /v1/messages` through the in-process app.
async fn call_messages(model: &str) -> Response {
    call_messages_body(json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await
}

async fn call_messages_body(body: Value) -> Response {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "smoke-session")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Same request, with a monitor watching, so a test can read what the proxy
/// recorded about the request next to what the client received.
async fn call_messages_body_with_monitor(monitor: MonitorHandle, body: Value) -> Response {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    app_with_monitor(Arc::new(Registry::with_default_alias()), Some(monitor))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "smoke-session")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// The message of the `error` event an SSE body ends with, if it has one.
fn sse_error_message(body: &str) -> Option<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|event| event["type"] == "error")
        .and_then(|event| {
            event["error"]["message"]
                .as_str()
                .map(std::string::ToString::to_string)
        })
}

/// Read a streamed response until it ends or its body breaks: the bytes that
/// arrived, and whether the transport failed.
async fn drain_stream_allowing_error(response: Response) -> (String, bool) {
    let mut body = response.into_body();
    let mut collected = Vec::new();
    let mut failed = false;
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    collected.extend_from_slice(&data);
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    (String::from_utf8_lossy(&collected).into_owned(), failed)
}

/// Read a streamed response to the end, which is what finishes the request in
/// the monitor.
async fn drain_stream(response: Response) -> String {
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("stream must terminate")
    .unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

async fn call_responses_body(body: Value) -> Response {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    app_with_options(Arc::new(Registry::with_default_alias()), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "smoke-session")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn traffic_files(state_dir: &Path) -> Vec<PathBuf> {
    collect_files(
        &state_dir
            .join("claude-code-proxy")
            .join("traffic")
            .join("smoke-session"),
    )
}

fn traffic_file<'a>(files: &'a [PathBuf], suffix: &str) -> &'a Path {
    files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        })
        .map(PathBuf::as_path)
        .unwrap_or_else(|| panic!("missing traffic artifact ending in {suffix}; files={files:?}"))
}

fn traffic_json(files: &[PathBuf], suffix: &str) -> Value {
    serde_json::from_slice(&std::fs::read(traffic_file(files, suffix)).unwrap()).unwrap()
}

/// Spawn a mock axum HTTP server that accepts requests at any path, calls
/// `handler(request_json)` and returns the handler's response body as a 200
/// with `content-type: text/event-stream`.
async fn spawn_http_upstream<F>(handler: F) -> String
where
    F: Fn(Value) -> Vec<u8> + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    let app = axum::Router::new().fallback({
        let handler = handler.clone();
        move |body: String| {
            let handler = handler.clone();
            async move {
                let json: Value = serde_json::from_str(&body).unwrap_or_default();
                let response_bytes = handler(json);
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(response_bytes))
                    .unwrap()
            }
        }
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    addr_str
}

/// Same, but keeping the request body exactly as it arrived, so a test can
/// compare the bytes the proxy relayed with the ones the client sent.
async fn spawn_capturing_http_upstream(
    captured: Arc<Mutex<Option<Vec<u8>>>>,
    response: &'static str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let captured = captured.clone();
        async move {
            *captured.lock().unwrap() = Some(body.to_vec());
            http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(response.as_bytes()))
                .unwrap()
        }
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    addr_str
}

async fn spawn_truncated_http_upstream(body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).await;
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len() + 4096
        );
        let _ = stream.write_all(headers.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
    });

    format!("http://{addr}")
}

#[allow(clippy::await_holding_lock)]
async fn assert_codex_http_presemantic_retry(first_response: Vec<u8>) {
    let _guard = env_lock();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(AtomicUsize::new(0));
    let first_response = Arc::new(first_response);
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        let first_response = first_response.clone();
        move |_body: Value| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                first_response.as_ref().clone()
            } else {
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_retry\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_retry\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"retry succeeded\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_retry\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
                )
                .as_bytes()
                .to_vec()
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("retried stream must finish")
    .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "stream body: {text}");
    assert!(text.contains("retry succeeded"), "stream body: {text}");
    assert!(!text.contains("event: error"), "stream body: {text}");
    assert_eq!(text.matches("event: message_start").count(), 1);
    assert_eq!(text.matches("event: message_stop").count(), 1);
}

/// Spawn a mock WebSocket server that accepts one connection, captures the
/// first text message, and responds with Codex WebSocket events that
/// accumulate to `"codex websocket ok"`.
async fn spawn_websocket_upstream(captured: Arc<Mutex<Option<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(ws) = tokio_tungstenite::accept_async(stream).await
        {
            let (mut sender, mut receiver) = ws.split();

            // Read the incoming response.create message
            if let Some(Ok(Message::Text(text))) = receiver.next().await
                && let Ok(json) = serde_json::from_str::<Value>(&text)
            {
                let _ = captured.lock().map(|mut g| *g = Some(json));
            }

            // Send Codex Responses events as WebSocket text messages
            let events = [
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"codex websocket ok"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];

            for event in &events {
                let _ = sender.send(Message::Text(event.to_string())).await;
            }
        }
    });

    addr_str
}

/// Reproduce the Codex subscription-credit response observed in production:
/// the included window is exhausted, but usable credits remain and the model
/// still completes the response after the rate-limit snapshot.
async fn spawn_websocket_credited_rate_limit_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await
        {
            let _ = ws.next().await;
            let events = [
                r#"{"type":"codex.rate_limits","rate_limits":{"allowed":false,"limit_reached":true,"primary":{"used_percent":100,"window_minutes":10080,"reset_after_seconds":509821}},"credits":{"has_credits":true,"unlimited":false,"balance":null}}"#,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"credited codex ok"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];

            for event in &events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_delayed_terminal_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await
        {
            let _ = ws.next().await;
            let early_events = [
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"early chunk"}"#,
            ];
            for event in &early_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }

            tokio::time::sleep(Duration::from_secs(2)).await;

            let terminal_events = [
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ];
            for event in &terminal_events {
                let _ = ws.send(Message::Text(event.to_string())).await;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_error_upstream(message: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await
        {
            let _ = ws.next().await;
            let event = json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "param": "input",
                    "message": message
                }
            });
            let _ = ws.send(Message::Text(event.to_string())).await;
        }
    });

    addr_str
}

/// A socket that starts a response, reasons, and then drops without ever
/// sending a terminal event. Every accepted connection is counted, so a test can
/// see whether the proxy retried after the client already had output.
async fn spawn_websocket_reset_upstream(
    attempts: Arc<AtomicUsize>,
    events: &'static [&'static str],
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            attempts.fetch_add(1, Ordering::SeqCst);
            let _ = ws.next().await;
            for event in events {
                let _ = ws.send(Message::Text((*event).to_string())).await;
            }
            // No response.completed, no close frame: the socket simply goes.
            drop(ws);
        }
    });

    format!("http://{addr}")
}

const WEBSOCKET_RESET_AFTER_REASONING: &[&str] = &[
    r#"{"type":"response.created","response":{"id":"resp_reset"}}"#,
    r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
    r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"weighing the options"}"#,
];

const WEBSOCKET_FAILED_AFTER_TEXT: &[&str] = &[
    r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}"#,
    r#"{"type":"response.output_text.delta","output_index":0,"delta":"partial answer"}"#,
    r#"{"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"message":"generation failed midway"}}}"#,
];

const WEBSOCKET_RESET_AFTER_CLOSED_TOOL_CALL: &[&str] = &[
    r#"{"type":"response.created","response":{"id":"resp_tool"}}"#,
    r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"Bash"}}"#,
    r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"command\":\"ls\"}"}"#,
    r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"command\":\"ls\"}"}"#,
    r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{\"command\":\"ls\"}"}}"#,
];

async fn spawn_websocket_sequence_upstream(captured: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let texts = ["first", "second", "third"];
        let mut handled = 0usize;
        while handled < texts.len() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < texts.len() {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                let idx = handled;
                let response_text = texts[idx];
                let response_id = format!("resp_{}", idx + 1);
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_up_{idx}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_previous_missing_then_retry_upstream(
    captured: Arc<Mutex<Vec<Value>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 3 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 3 {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                if handled == 1 {
                    let rate_limits = json!({
                        "type": "codex.rate_limits",
                        "rate_limits": {"limit_reached": false}
                    });
                    let _ = sender.send(Message::Text(rate_limits.to_string())).await;
                    let event = json!({
                        "type": "error",
                        "error": {
                            "code": "previous_response_not_found",
                            "message": "previous response not found",
                            "status": 400
                        }
                    });
                    let _ = sender.send(Message::Text(event.to_string())).await;
                    handled += 1;
                    break;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_up_{handled}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_close_then_retry_upstream(captured: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 3 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 3 {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                if handled == 1 {
                    handled += 1;
                    let _ = sender.close().await;
                    break;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_close_{handled}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

async fn spawn_websocket_empty_completion_then_retry_upstream(
    captured: Arc<Mutex<Vec<Value>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        let mut handled = 0usize;
        while handled < 3 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();

            while handled < 3 {
                let Some(text) = (loop {
                    match receiver.next().await {
                        Some(Ok(Message::Text(text))) => break Some(text),
                        Some(Ok(Message::Ping(data))) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break None,
                    }
                }) else {
                    break;
                };
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    let _ = captured.lock().map(|mut g| g.push(json));
                }

                if handled == 1 {
                    let event = json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_empty",
                            "status": "completed",
                            "incomplete_details": null,
                            "usage": {"input_tokens": 5, "output_tokens": 0}
                        }
                    });
                    let _ = sender.send(Message::Text(event.to_string())).await;
                    handled += 1;
                    continue;
                }

                let response_text = if handled == 0 { "first" } else { "retry" };
                let response_id = if handled == 0 { "resp_1" } else { "resp_retry" };
                let events = [
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{"type":"message","id":format!("msg_empty_{handled}")}
                    }),
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "delta":response_text
                    }),
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":{"type":"message"}
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":response_id,"usage":{"input_tokens":5,"output_tokens":2}}
                    }),
                ];

                for event in &events {
                    let _ = sender.send(Message::Text(event.to_string())).await;
                }
                handled += 1;
            }
        }
    });

    addr_str
}

/// Upstream that answers every request with a terminal-only completion,
/// so the proxy's bounded retry loop always exhausts.
async fn spawn_websocket_always_empty_completion_upstream(
    request_count: Arc<std::sync::atomic::AtomicUsize>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = format!("http://{addr}");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sender, mut receiver) = ws.split();
            let request_count = request_count.clone();

            tokio::spawn(async move {
                while let Some(message) = receiver.next().await {
                    match message {
                        Ok(Message::Text(_)) => {
                            request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let event = json!({
                                "type": "response.completed",
                                "response": {
                                    "id": "resp_empty",
                                    "status": "completed",
                                    "incomplete_details": null,
                                    "usage": {"input_tokens": 5, "output_tokens": 0}
                                }
                            });
                            if sender.send(Message::Text(event.to_string())).await.is_err() {
                                return;
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = sender.send(Message::Pong(data)).await;
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            });
        }
    });

    addr_str
}

// ---------------------------------------------------------------------------
// Health and routing smoke tests (no env var mutation needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_healthz_returns_ok() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body, json!({"ok": true}));
}

#[test]
fn smoke_codex_model_is_registered() {
    let registry = Registry::new(AliasProvider::Anthropic);
    let provider = registry.provider_for_model("gpt-5.5", None);
    assert!(
        provider.is_some(),
        "gpt-5.5 must resolve to a registered provider"
    );
    assert_eq!(
        provider.unwrap().name(),
        "codex",
        "gpt-5.5 must route to the codex provider"
    );
}

#[test]
fn smoke_kimi_model_is_registered() {
    // Kimi uses reqwest::blocking::Client internally, which panics when
    // dropped from an async context (it joins a dedicated runtime thread).
    // Test routing at the Registry level instead of through the HTTP stack.
    let registry = Registry::with_default_alias();
    let provider = registry.provider_for_model("kimi-for-coding", None);
    assert!(
        provider.is_some(),
        "kimi-for-coding must resolve to a registered provider"
    );
    assert_eq!(
        provider.unwrap().name(),
        "kimi",
        "kimi-for-coding must route to the kimi provider"
    );
}

// ---------------------------------------------------------------------------
// Kimi smoke: mock upstream verifies request shape and returns a valid
// streaming response. Uses multi-thread runtime because KimiHttpClient uses
// reqwest::blocking::Client internally.
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_kimi_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    write_kimi_auth(config.path());

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut g| *g = Some(body));
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"kimi ok\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_KIMI_BASE_URL", &upstream);
    let _compaction_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "1");
    let response = call_messages("kimi-for-coding").await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "kimi ok");

    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "kimi-for-coding");
    assert_eq!(sent["stream"], true);
    assert!(sent.get("input").is_none());
    assert!(!sent.to_string().contains("compaction_trigger"));
}

// ---------------------------------------------------------------------------
// Codex HTTP smoke: mock upstream verifies request shape and returns
// Responses SSE events.
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut g| *g = Some(body));
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex http ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "codex http ok");

    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "gpt-5.5");
    assert_eq!(sent["stream"], true);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_native_responses_preserves_parallel_tool_calls() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let _ = captured.lock().map(|mut guard| *guard = Some(body));
            br#"{"id":"resp_1","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let response = call_responses_body(json!({
        "model":"gpt-5.4",
        "input":"hello",
        "parallel_tool_calls":false
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let sent = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sent["parallel_tool_calls"], false);
}

/// Resets the retry-delay override even when the test panics, so later tests
/// in this process keep real backoff behavior.
struct ZeroRetryDelayGuard;

impl ZeroRetryDelayGuard {
    fn enable() -> Self {
        claude_code_mux::retry::set_zero_retry_delay_for_tests(true);
        ZeroRetryDelayGuard
    }
}

impl Drop for ZeroRetryDelayGuard {
    fn drop(&mut self) {
        claude_code_mux::retry::set_zero_retry_delay_for_tests(false);
    }
}

fn empty_completion_sse() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_empty\",",
        "\"status\":\"completed\",\"incomplete_details\":null,",
        "\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn empty_message_completion_sse() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
        "\"item\":{\"type\":\"message\",\"id\":\"msg_empty\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_empty\",",
        "\"status\":\"completed\",\"incomplete_details\":null,",
        "\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn buffered_success_sse(text: &str) -> Vec<u8> {
    format!(
        concat!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"msg_up\"}}}}\n\n",
            "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{text}\"}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\"}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}}}\n\n"
        ),
        text = text
    )
    .into_bytes()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_empty_completion() {
    let _guard = env_lock();
    let _delay_guard = ZeroRetryDelayGuard::enable();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                empty_completion_sse()
            } else {
                buffered_success_sse("buffered retry ok")
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);

    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["content"][0]["text"], "buffered retry ok");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "empty completion must trigger one retry"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_empty_message_completion() {
    let _guard = env_lock();
    let _delay_guard = ZeroRetryDelayGuard::enable();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                empty_message_completion_sse()
            } else {
                buffered_success_sse("empty message retry ok")
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);

    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["content"][0]["text"], "empty message retry ok");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_stream_retries_empty_completion() {
    let _guard = env_lock();
    let _delay_guard = ZeroRetryDelayGuard::enable();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                empty_completion_sse()
            } else {
                buffered_success_sse("buffered stream retry ok")
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);

    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    assert!(
        body_text.contains("buffered stream retry ok"),
        "expected retried text in SSE body: {body_text}"
    );
    assert!(
        !body_text.contains(r#""input_tokens":0"#),
        "message_start should expose the request token estimate: {body_text}"
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_empty_completions_exhaust_to_service_unavailable() {
    let _guard = env_lock();
    let _delay_guard = ZeroRetryDelayGuard::enable();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            empty_completion_sse()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "exhausted empty completions must surface an explicit error: {body_text}"
    );
    assert!(
        body_text.contains("Codex completed without producing output"),
        "unexpected exhaustion body: {body_text}"
    );
    // Initial attempt plus MAX_EMPTY_COMPLETION_RETRIES retries.
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        11,
        "retry loop must stay bounded"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_auto_review_uses_codex_default_and_configured_override() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            captured.lock().unwrap().push(body);
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"review ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _codex_model_env = EnvGuard::set("CCP_CODEX_MODEL", "gpt-5.6-sol");
    let classifier_body = || {
        json!({
            "model": "gpt-5.6-sol",
            "max_tokens": 64,
            "stream": false,
            "system": [{
                "type": "text",
                "text": "You are a security monitor for autonomous AI coding agents.\n\n## Context"
            }],
            "messages": [{"role":"user","content":"review this Bash command"}],
            "tools": []
        })
    };

    let classifier = call_messages_body(classifier_body()).await;
    assert_eq!(classifier.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(classifier.into_body(), usize::MAX)
        .await
        .unwrap();

    {
        let _review_model_env = EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "gpt-5.6-terra");
        let classifier = call_messages_body(classifier_body()).await;
        assert_eq!(classifier.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(classifier.into_body(), usize::MAX)
            .await
            .unwrap();
    }

    let normal = call_messages("gpt-5.6-sol").await;
    assert_eq!(normal.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(normal.into_body(), usize::MAX)
        .await
        .unwrap();

    let sent = captured.lock().unwrap();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0]["model"], "gpt-5.6-luna");
    assert_eq!(sent[1]["model"], "gpt-5.6-terra");
    assert_eq!(sent[2]["model"], "gpt-5.6-sol");
}

/// Assert that one captured upstream body is on the Responses Lite lane:
/// the `client_metadata` marker the backend reads, plus the two fields the
/// lane forces on every request built for it.
fn assert_responses_lite_lane(request: &Value, label: &str) {
    assert_eq!(
        request.get("client_metadata"),
        Some(&json!({
            "ws_request_header_x_openai_internal_codex_responses_lite": "true"
        })),
        "{label} must carry the lite lane marker"
    );
    assert_eq!(
        request["parallel_tool_calls"], false,
        "{label} must keep the lite lane's parallel_tool_calls"
    );
    assert_eq!(
        request["reasoning"]["context"], "all_turns",
        "{label} must keep the lite lane's reasoning context"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_server_compaction_replays_native_history() {
    let _guard = env_lock();
    clear_all_compactions_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let is_compaction = body["input"].as_array().is_some_and(|input| {
                input.last().and_then(|item| item["type"].as_str())
                    == Some("compaction_trigger")
            });
            let request_number = {
                let mut requests = captured.lock().unwrap();
                requests.push(body);
                requests.len()
            };
            if is_compaction {
                concat!(
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque-history\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_compact\",\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\n"
                )
                .as_bytes()
                .to_vec()
            } else {
                let text = if request_number == 2 {
                    "portable summary with enough detail to anchor the compacted conversation"
                } else {
                    "compacted ok"
                };
                format!(
                    "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"msg_up\"}}}}\n\n\
                     data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{text}\"}}\n\n\
                     data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\"}}}}\n\n\
                     data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{request_number}\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}}}\n\n"
                )
                .into_bytes()
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _compaction_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "1");
    // Pin the lane instead of inheriting it: under the default `full` policy no
    // request carries `client_metadata` at all, so the lane assertions below
    // would hold vacuously. `inventory` follows the backend's own flag, which
    // for `gpt-5.6-sol` is lite from the compiled-in table while no model
    // listing has been fetched (this binary never fetches one).
    let _lane_policy_env = EnvGuard::set("CCP_CODEX_LANE_POLICY", "inventory");
    // Neither of these may decide which lane the request lands on.
    let _codex_model_env = EnvGuard::unset("CCP_CODEX_MODEL");
    let _originator_env = EnvGuard::unset("CCP_CODEX_ORIGINATOR");
    let compact_response = call_messages_body(json!({
        "model": "gpt-5.6-sol",
        "max_tokens": 64,
        "system": "You are Claude Code.",
        "messages": [
            {"role":"user","content":"old conversation"},
            {"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tool-1","content":"result"},
                {"type":"text","text":"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\nYour task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests."}
            ]}
        ]
    }))
    .await;
    assert_eq!(compact_response.status(), StatusCode::OK);

    let response = call_messages_body(json!({
        "model": "gpt-5.6-sol",
        "max_tokens": 64,
        "system": "current instructions",
        "messages": [
            {"role":"user","content":"<summary>portable summary with enough detail to anchor the compacted conversation</summary>"},
            {"role":"user","content":"continue"}
        ]
    }))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "compacted ok");

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["model"], "gpt-5.6-sol");
    // The compaction call must run on the same Responses lane as the
    // conversation it compacts, and the replay after it must stay on that lane
    // too. Assert the marker on each request rather than comparing two of them:
    // an equality check passes when both sides are absent.
    assert_responses_lite_lane(&requests[0], "the compaction request");
    assert_responses_lite_lane(&requests[1], "the summary request");
    assert_eq!(
        requests[0]["input"].as_array().unwrap().last().unwrap()["type"],
        "compaction_trigger"
    );
    assert!(
        !requests[0]
            .to_string()
            .contains("Your task is to create a detailed summary")
    );
    assert!(
        requests[1]
            .to_string()
            .contains("Your task is to create a detailed summary")
    );
    assert!(!requests[1].to_string().contains("opaque-history"));
    let replay = requests[2]["input"].as_array().unwrap();
    assert_responses_lite_lane(&requests[2], "the replay request");
    assert!(requests[2].to_string().contains("current instructions"));
    let compaction = replay
        .iter()
        .find(|item| item["type"] == "compaction")
        .unwrap();
    assert_eq!(compaction["encrypted_content"], "opaque-history");
    assert!(replay.iter().any(|item| item["role"] == "user"));
    clear_all_compactions_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_compaction_failure_preserves_portable_summary() {
    let _guard = env_lock();
    clear_all_compactions_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            let is_compaction = body["input"].as_array().is_some_and(|input| {
                input.last().and_then(|item| item["type"].as_str())
                    == Some("compaction_trigger")
            });
            captured.lock().unwrap().push(body);
            if is_compaction {
                b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_vec()
            } else {
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"portable fallback\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
                )
                .as_bytes()
                .to_vec()
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _compaction_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "1");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "system": "You are a helpful AI assistant tasked with summarizing conversations.",
        "messages": [{"role":"user","content":"old conversation"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["content"][0]["text"], "portable fallback");

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_string().contains("old conversation"));
    assert!(!requests[1].to_string().contains("opaque-history"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_context_window_error_requests_compaction() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let upstream = spawn_http_upstream(|_body: Value| {
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"input exceeds context window\"}}}\n\n"
            .as_bytes()
            .to_vec()
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "request_too_large");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("input exceeds context window")),
        "response body: {}",
        String::from_utf8_lossy(&body)
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex http ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let files = traffic_files(state.path());
    let request = traffic_json(&files, "020-upstream-request.json");
    assert_eq!(request["model"], "gpt-5.5");

    let metadata = traffic_json(&files, "021-upstream-request-metadata.json");
    assert_eq!(metadata["transport"], "http");
    assert!(
        metadata["headers"]["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    assert_eq!(
        traffic_json(&files, "030-upstream-response-headers.json")["status"],
        200
    );
    traffic_file(&files, "032-upstream-response-body.sse");
    traffic_file(&files, "040-upstream-event.json");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_stream_traffic_captures_downstream_events() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"codex stream ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("message_stop"), "stream body: {text}");

    let files = traffic_files(state.path());
    let downstream = traffic_json(&files, "050-downstream-event.json");
    assert!(downstream.get("event").is_some());
    assert!(downstream.get("data").is_some());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_stream_returns_before_upstream_completion() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let release = Arc::new(tokio::sync::Notify::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = format!("http://{addr}");
    let mock = axum::Router::new().fallback({
        let release = release.clone();
        move |_body: String| {
            let release = release.clone();
            async move {
                let stream = futures_util::stream::unfold(0_u8, move |state| {
                    let release = release.clone();
                    async move {
                        match state {
                            0 => Some((
                                Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                                    concat!(
                                        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"incremental ok\"}\n\n"
                                    )
                                    .as_bytes(),
                                )),
                                1,
                            )),
                            1 => {
                                release.notified().await;
                                Some((
                                    Ok(bytes::Bytes::from_static(concat!(
                                        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
                                    ).as_bytes())),
                                    2,
                                ))
                            }
                            _ => None,
                        }
                    }
                });
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, mock).await.ok();
    });

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = tokio::time::timeout(
        Duration::from_millis(500),
        call_messages_body(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        })),
    )
    .await
    .expect("CCP must return streaming headers before upstream completion");
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_millis(200), body.frame())
        .await
        .expect("initial Anthropic heartbeat must arrive before upstream completion")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(first.contains("event: message_start"));
    assert!(first.contains("event: ping"));
    assert!(first.contains("incremental ok"));

    release.notify_one();
    let rest = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let rest = String::from_utf8(rest.to_vec()).unwrap();
    assert!(rest.contains("event: message_stop"), "stream body: {rest}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_overload_after_control_events() {
    assert_codex_http_presemantic_retry(
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_failed\"}}\n\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_failed\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"Our servers are currently overloaded. Please try again later.\",\"retry_after\":0}}}\n\n"
        )
        .as_bytes()
        .to_vec(),
    )
    .await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_rate_limit_after_control_events() {
    assert_codex_http_presemantic_retry(
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_limited\"}}\n\n",
            "data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"limit_reached\":true,\"primary\":{\"reset_after_seconds\":0}}}\n\n"
        )
        .as_bytes()
        .to_vec(),
    )
    .await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_transient_failure_after_control_events() {
    assert_codex_http_presemantic_retry(
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_transient\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"status\":503,\"message\":\"temporarily unavailable\",\"retry_after\":0}}}\n\n"
        )
        .as_bytes()
        .to_vec(),
    )
    .await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_presemantic_eof() {
    assert_codex_http_presemantic_retry(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_truncated\"}}\n\n"
            .as_bytes()
            .to_vec(),
    )
    .await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_bounds_initial_status_retries() {
    let _guard = env_lock();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = format!("http://{addr}");
    let mock = axum::Router::new().fallback({
        let attempts = attempts.clone();
        move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                http::Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("retry-after", "0")
                    .body(Body::empty())
                    .unwrap()
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, mock).await.ok();
    });

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_presemantic_invalid_json() {
    assert_codex_http_presemantic_retry(b"data: not-json\n\n".to_vec()).await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_retries_presemantic_invalid_utf8() {
    assert_codex_http_presemantic_retry(b"data: \xff\n\n".to_vec()).await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_does_not_retry_overload_after_semantic_output() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_partial\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_partial\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial output\"}\n\n",
                    "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded after output\",\"retry_after\":0}}}\n\n"
                )
                .as_bytes()
                .to_vec()
            } else {
                panic!("semantic output must close the full-request retry window");
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("failed semantic stream must terminate")
    .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "stream body: {text}");
    assert!(text.contains("partial output"), "stream body: {text}");
    assert!(
        text.contains("overloaded after output"),
        "stream body: {text}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_stops_after_retry_limit() {
    let _guard = env_lock();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            attempts.fetch_add(1, Ordering::SeqCst);
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_exhausted\"}}\n\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded until retry limit\",\"retry_after\":0}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status().as_u16(), 529);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("exhausted stream must terminate")
    .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert_eq!(attempts.load(Ordering::SeqCst), 4, "stream body: {text}");
    assert!(
        text.contains("overloaded until retry limit"),
        "stream body: {text}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_cancels_retry_backoff_when_request_drops() {
    let _guard = env_lock();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_http_upstream({
        let attempts = attempts.clone();
        move |_body: Value| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_cancel\"}}\n\n",
                    "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"overloaded_error\",\"message\":\"cancel during retry backoff\",\"retry_after\":0.1}}}\n\n"
                )
                .as_bytes()
                .to_vec()
            } else {
                panic!("request cancellation must prevent another upstream attempt");
            }
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let request = tokio::spawn(call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    })));
    tokio::time::timeout(Duration::from_millis(200), async {
        while attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first upstream attempt must start");
    request.abort();
    let _ = request.await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_body_error_after_semantic_output_preserves_message() {
    let _guard = env_lock();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let upstream = spawn_truncated_http_upstream(concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_partial\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_partial\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial before body error\"}\n\n"
    ).as_bytes())
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("failed semantic stream must terminate")
    .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("partial before body error"),
        "stream body: {text}"
    );
    assert!(text.contains("event: error"), "stream body: {text}");
    assert!(
        text.contains("Transport error reading Codex response body"),
        "stream body: {text}"
    );
    assert!(
        !text.contains("\"message\":\"http_response_body\""),
        "stream body: {text}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_http_truncated_upstream_writes_reducer_diagnostic() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let upstream = spawn_http_upstream(|_body: Value| {
        concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n"
        )
        .as_bytes()
        .to_vec()
    })
    .await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let files = traffic_files(state.path());
    let diagnostic = traffic_json(&files, "060-codex-reducer-error.json");
    assert_eq!(diagnostic["kind"], "Transient");
    assert_eq!(
        diagnostic["diagnostics"]["saw_terminal_event"],
        Value::Bool(false)
    );
}

// ---------------------------------------------------------------------------
// Codex WebSocket smoke: mock upstream verifies request shape and returns
// Responses events over WebSocket.
// ---------------------------------------------------------------------------

// Multi-threaded runtime so the spawned accept task runs independently and
// the listener is registered with the I/O driver before connect_async starts.
// A single-threaded runtime risks the root task (connect_async) outpacing the
// spawned accept task, causing connection-refused races.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_messages_uses_mock_upstream() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = call_messages("gpt-5.5").await;

    let ws_status = response.status();
    let ws_body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if ws_status != StatusCode::OK {
        panic!(
            "WS: expected 200, got {}: {}",
            ws_status,
            String::from_utf8_lossy(&ws_body_bytes)
        );
    }
    let value: Value = serde_json::from_slice(&ws_body_bytes).unwrap();
    assert_eq!(value["content"][0]["text"], "codex websocket ok");

    let guard = captured.lock().unwrap();
    let sent = guard.clone().unwrap_or_else(|| {
        panic!(
            "WS mock did not capture a request. Response body: {}",
            String::from_utf8_lossy(&ws_body_bytes)
        );
    });
    assert_eq!(sent["type"], "response.create");
    assert_eq!(sent["model"], "gpt-5.5");
    assert!(sent.get("max_output_tokens").is_none());
    assert!(sent.get("stream").is_none());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_uses_credits_after_included_limit() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_credited_rate_limit_upstream().await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = call_messages("gpt-5.5").await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(
        status,
        StatusCode::OK,
        "credited Codex response must not be discarded: {}",
        String::from_utf8_lossy(&body)
    );
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["content"][0]["text"], "credited codex ok");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_uses_credits_after_included_limit() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_credited_rate_limit_upstream().await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(
        status,
        StatusCode::OK,
        "credited Codex stream must not be discarded: {}",
        String::from_utf8_lossy(&body)
    );
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("credited codex ok"), "stream body: {text}");
    assert!(text.contains("message_stop"), "stream body: {text}");
    assert!(!text.contains("event: error"), "stream body: {text}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_returns_delta_before_terminal() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_delayed_terminal_upstream().await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = tokio::time::timeout(
        Duration::from_millis(1_500),
        call_messages_body(json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        })),
    )
    .await
    .expect("streaming response should start before terminal upstream event");
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let mut collected = Vec::new();
    let read = tokio::time::timeout(Duration::from_millis(500), async {
        while !String::from_utf8_lossy(&collected).contains("text_delta") {
            let Some(frame) = body.frame().await else {
                break;
            };
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                collected.extend_from_slice(&data);
            }
        }
    })
    .await;
    assert!(read.is_ok(), "stream did not yield an early text delta");
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("early chunk"), "stream body: {text}");
    assert!(
        !text.contains(r#""input_tokens":0"#),
        "message_start should expose the request token estimate: {text}"
    );
    assert!(
        !text.contains("message_stop"),
        "stream finished too early: {text}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_context_window_error_requests_compaction() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let upstream = spawn_websocket_error_upstream("input exceeds context window").await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hello"}]
    }))
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "response body: {}",
        String::from_utf8_lossy(&body)
    );
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "request_too_large");
    assert_eq!(value["error"]["message"], "input exceeds context window");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_uses_previous_response_id() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_sequence_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("second"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 2, "expected two upstream websocket requests");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert_eq!(
        guard[1]["input"].as_array().map(Vec::len),
        Some(1),
        "second request should send only the appended input delta"
    );
    assert_eq!(guard[1]["input"][0]["role"], "user");
    assert_eq!(guard[1]["input"][0]["content"][0]["text"], "two");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_missing_previous_response_id() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_previous_missing_then_retry_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("retry"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected retry websocket request");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert!(guard[2].get("previous_response_id").is_none());
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(3),
        "retry request should send the full input"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_empty_close_with_full_context() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_close_then_retry_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("retry"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected full-context retry request");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert!(guard[2].get("previous_response_id").is_none());
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(3),
        "retry request should send the full input"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_stream_retries_terminal_only_completion_with_full_context() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_empty_completion_then_retry_upstream(captured.clone()).await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&second_body).contains("retry"),
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );

    let downstream_end_turns = traffic_files(state.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("050-downstream-event.json"))
        })
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|event| event["data"]["delta"]["stop_reason"] == "end_turn")
        .count();
    assert_eq!(
        downstream_end_turns, 2,
        "discarded empty attempts must not be captured as downstream events"
    );

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected full-context retry request");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert!(guard[2].get("previous_response_id").is_none());
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(3),
        "retry request should send the full input"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_empty_completions_exhaust_to_service_unavailable() {
    let _guard = env_lock();
    let _delay_guard = ZeroRetryDelayGuard::enable();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = spawn_websocket_always_empty_completion_upstream(request_count.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let response = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "exhausted empty completions must surface an explicit error: {body_text}"
    );
    assert!(
        body_text.contains("Codex completed without producing output"),
        "unexpected exhaustion body: {body_text}"
    );
    // Initial attempt plus MAX_RETRYABLE_LIVE_STREAM_RETRIES full-context retries.
    assert_eq!(
        request_count.load(std::sync::atomic::Ordering::SeqCst),
        11,
        "retry loop must stay bounded"
    );

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_previous_response_id_sends_delta_on_second_turn() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_websocket_sequence_upstream(captured.clone()).await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let _previous_response_env = EnvGuard::set("CCP_CODEX_PREVIOUS_RESPONSE_ID", "1");

    let first = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [{"role":"user","content":"one"}]
    }))
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"}
        ]
    }))
    .await;
    let second_status = second.status();
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        second_status,
        StatusCode::OK,
        "second response body: {}",
        String::from_utf8_lossy(&second_body)
    );
    let value: Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(value["content"][0]["text"], "second");

    let third = call_messages_body(json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"one"},
            {"role":"assistant","content":"first"},
            {"role":"user","content":"two"},
            {"role":"assistant","content":"second"},
            {"role":"user","content":"three"}
        ]
    }))
    .await;
    let third_status = third.status();
    let third_body = axum::body::to_bytes(third.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        third_status,
        StatusCode::OK,
        "third response body: {}",
        String::from_utf8_lossy(&third_body)
    );
    let value: Value = serde_json::from_slice(&third_body).unwrap();
    assert_eq!(value["content"][0]["text"], "third");

    let guard = captured.lock().unwrap();
    assert_eq!(guard.len(), 3, "expected three upstream websocket requests");
    assert!(guard[0].get("previous_response_id").is_none());
    assert_eq!(guard[1]["previous_response_id"], "resp_1");
    assert_eq!(
        guard[1]["input"].as_array().map(Vec::len),
        Some(1),
        "second request should send only the appended input delta"
    );
    assert_eq!(guard[1]["input"][0]["role"], "user");
    assert_eq!(guard[1]["input"][0]["content"][0]["text"], "two");
    assert_eq!(guard[2]["previous_response_id"], "resp_2");
    assert_eq!(
        guard[2]["input"].as_array().map(Vec::len),
        Some(1),
        "third request should keep reusing the pooled websocket continuation"
    );
    assert_eq!(guard[2]["input"][0]["role"], "user");
    assert_eq!(guard[2]["input"][0]["content"][0]["text"], "three");

    clear_all_continuations_for_tests();
    clear_codex_websocket_pool_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_traffic_capture_writes_upstream_artifacts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_upstream(captured.clone()).await;

    let _traffic_env = EnvGuard::set("CCP_TRAFFIC_LOG", "1");
    let _state_env = EnvGuard::set("XDG_STATE_HOME", state.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
    let response = call_messages("gpt-5.5").await;

    assert_eq!(response.status(), StatusCode::OK);
    let files = traffic_files(state.path());
    let request = traffic_json(&files, "020-upstream-request.json");
    assert_eq!(request["type"], "response.create");
    assert!(request.get("stream").is_none());

    let metadata = traffic_json(&files, "021-upstream-request-metadata.json");
    assert_eq!(metadata["transport"], "websocket");
    assert!(
        metadata["headers"]["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    traffic_file(&files, "022-upstream-websocket-metadata.json");
    assert_eq!(
        traffic_json(&files, "030-upstream-response-headers.json")["status"],
        200
    );
    traffic_file(&files, "032-upstream-response-body.sse");
    traffic_file(&files, "040-upstream-event.json");
}

// ---------------------------------------------------------------------------
// What the proxy records about a stream that went wrong after its 200 left, and
// which token counts it treats as the backend's own.
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_reset_after_reasoning_is_recorded_as_failed() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream =
        spawn_websocket_reset_upstream(attempts.clone(), WEBSOCKET_RESET_AFTER_REASONING).await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    // The status and the body are what the client really received.
    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert!(text.contains("thinking_delta"), "stream body: {text}");
    assert!(text.contains("event: error"), "stream body: {text}");
    assert!(!text.contains("message_stop"), "stream body: {text}");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(
        request.status,
        RequestStatus::Failed,
        "a stream that never reached message_stop did not complete: {:?}",
        request.error
    );
    assert_eq!(request.http_status, Some(200));
    // The reason recorded is the one the client was given, not a guess.
    let reported = sse_error_message(&text).expect("the client received an error event");
    assert!(!reported.is_empty(), "stream body: {text}");
    assert_eq!(request.error.as_deref(), Some(reported.as_str()));
    // Output had already reached the client, so the request is not retried.
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    // Nothing closed these counts: the prompt is still this proxy's estimate and
    // the zero output is the one the stream opened with, not a final count.
    assert_eq!(
        request.usage_quality(),
        QualityFields {
            input: UsageQuality::Opening,
            cache_read: UsageQuality::Missing,
            cache_write: UsageQuality::Missing,
            output: UsageQuality::Opening,
        }
    );
    assert!(request.input_tokens.unwrap_or(0) > 0);
    assert_eq!(request.output_tokens, Some(0));
    assert_eq!(request.cache.read_tokens, None);
    assert_eq!(request.cache.write_tokens, None);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_reset_after_closed_tool_call_completes_without_backend_counts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream =
        spawn_websocket_reset_upstream(attempts.clone(), WEBSOCKET_RESET_AFTER_CLOSED_TOOL_CALL)
            .await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    // A tool call the backend had already closed is a complete turn: the client
    // keeps it and runs the tool once.
    assert!(text.contains("tool_use"), "stream body: {text}");
    assert_eq!(text.matches(r#""stop_reason":"tool_use""#).count(), 1);
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(!text.contains("event: error"), "stream body: {text}");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Completed);
    assert_eq!(request.http_status, Some(200));
    // The salvaged finish carries a zero `usage` object because the wire format
    // needs one. It is not the backend's accounting, so the estimate stands and
    // the counts stay provisional.
    assert!(request.input_tokens.unwrap_or(0) > 0);
    assert_eq!(request.output_tokens, Some(0));
    assert_eq!(request.cache.read_tokens, None);
    assert_eq!(request.cache.write_tokens, None);
    assert_eq!(
        request.usage_quality(),
        QualityFields {
            input: UsageQuality::Opening,
            cache_read: UsageQuality::Missing,
            cache_write: UsageQuality::Missing,
            output: UsageQuality::Opening,
        }
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_failure_after_text_is_recorded_with_its_reason() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream =
        spawn_websocket_reset_upstream(attempts.clone(), WEBSOCKET_FAILED_AFTER_TEXT).await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert!(text.contains("partial answer"), "stream body: {text}");
    assert_eq!(
        sse_error_message(&text).as_deref(),
        Some("generation failed midway")
    );
    // The answer had already started, so the turn is not sent again.
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Failed);
    assert_eq!(request.http_status, Some(200));
    assert_eq!(request.error.as_deref(), Some("generation failed midway"));
}

/// A client that stops reading has abandoned its request; that is not the
/// stream's protocol failing.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_client_drop_stays_an_abandoned_request() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let upstream = spawn_websocket_delayed_terminal_upstream().await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(
        request.error.as_deref(),
        Some("Request future ended before completion")
    );
    assert_eq!(request.http_status, None);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_codex_websocket_completed_stream_records_the_backend_counts() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_codex_websocket_pool_for_tests();
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_websocket_upstream(captured.clone()).await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert!(text.contains("codex websocket ok"), "stream body: {text}");
    assert!(text.contains("event: message_stop"), "stream body: {text}");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Completed);
    assert_eq!(request.http_status, Some(200));
    // The mock reports a five-token prompt and two output tokens, and says
    // nothing about caching. The prompt size is therefore known exactly, while
    // how much of it was cached is not, so neither half of it is pinned down.
    assert_eq!(request.cache.reported_prompt_tokens, Some(5));
    assert_eq!(request.prompt_tokens(), Some(5));
    assert_eq!(request.output_tokens, Some(2));
    assert_eq!(request.cache.read_tokens, None);
    assert_eq!(request.cache_hit_ratio(), None);
    assert_eq!(
        request.usage_quality(),
        QualityFields {
            input: UsageQuality::Opening,
            cache_read: UsageQuality::Missing,
            cache_write: UsageQuality::Missing,
            output: UsageQuality::Exact,
        }
    );
}

/// The Anthropic relay: an SSE error after a 200 is a failure, the bytes reach
/// the client untouched, and the prompt counts Anthropic sent stay exact.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_anthropic_stream_error_event_fails_the_request_with_exact_bytes() {
    let _guard = env_lock();
    const UPSTREAM_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":1000,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let upstream = spawn_http_upstream(|_body: Value| UPSTREAM_SSE.as_bytes().to_vec()).await;
    let _base_url_env = EnvGuard::set("CCP_ANTHROPIC_BASE_URL", &upstream);

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert_eq!(text, UPSTREAM_SSE, "the relay must stay byte-exact");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.provider.as_deref(), Some("anthropic"));
    assert_eq!(request.status, RequestStatus::Failed);
    assert_eq!(request.http_status, Some(200));
    assert_eq!(request.error.as_deref(), Some("Overloaded"));
    // Anthropic counts its own prompt, so those counts are final even though the
    // output never finished.
    assert_eq!(request.input_tokens, Some(12));
    assert_eq!(request.cache.read_tokens, Some(1_000));
    assert_eq!(
        request.usage_quality(),
        QualityFields {
            input: UsageQuality::Exact,
            cache_read: UsageQuality::Exact,
            cache_write: UsageQuality::Exact,
            output: UsageQuality::Opening,
        }
    );
}

/// An error event names the reason, and a broken body afterwards cannot rename
/// it: the first failure wins.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_anthropic_error_event_survives_a_broken_body() {
    let _guard = env_lock();
    const UPSTREAM_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let upstream = spawn_truncated_http_upstream(UPSTREAM_SSE.as_bytes()).await;
    let _base_url_env = EnvGuard::set("CCP_ANTHROPIC_BASE_URL", &upstream);

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let (text, failed) = drain_stream_allowing_error(response).await;
    assert!(failed, "the relayed body must break: {text}");
    assert_eq!(text, UPSTREAM_SSE, "the relay must stay byte-exact");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Failed);
    assert_eq!(request.http_status, Some(200));
    assert_eq!(
        request.error.as_deref(),
        Some("Overloaded"),
        "the reason upstream gave must not be replaced by the transport's"
    );
}

/// With no failure named before it, the transport error is the reason; the
/// missing terminal event must not stand in for it.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_anthropic_broken_body_keeps_the_transport_reason() {
    let _guard = env_lock();
    const UPSTREAM_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    );
    let upstream = spawn_truncated_http_upstream(UPSTREAM_SSE.as_bytes()).await;
    let _base_url_env = EnvGuard::set("CCP_ANTHROPIC_BASE_URL", &upstream);

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let (text, failed) = drain_stream_allowing_error(response).await;
    assert!(failed, "the relayed body must break: {text}");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Failed);
    assert_eq!(request.http_status, Some(200));
    let error = request.error.as_deref().unwrap_or_default();
    assert!(!error.is_empty());
    assert_ne!(
        error,
        claude_code_mux::provider::MISSING_TERMINAL_FAILURE,
        "a real transport error must not be reported as a missing terminal event"
    );
}

/// The same relay, with the word error and a whole error document quoted inside
/// the answer: a healthy stream stays a success.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_anthropic_stream_quoting_an_error_completes_with_exact_bytes() {
    let _guard = env_lock();
    const UPSTREAM_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"error: {\\\"type\\\":\\\"error\\\",\\\"error\\\":{\\\"message\\\":\\\"quoted, not raised\\\"}}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":31}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let upstream = spawn_http_upstream(|_body: Value| UPSTREAM_SSE.as_bytes().to_vec()).await;
    let _base_url_env = EnvGuard::set("CCP_ANTHROPIC_BASE_URL", &upstream);

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert_eq!(text, UPSTREAM_SSE, "the relay must stay byte-exact");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.status, RequestStatus::Completed);
    assert_eq!(request.http_status, Some(200));
    assert_eq!(request.error, None);
    assert_eq!(request.output_tokens, Some(31));
    assert_eq!(
        request.usage_quality(),
        QualityFields {
            input: UsageQuality::Exact,
            cache_read: UsageQuality::Exact,
            cache_write: UsageQuality::Exact,
            output: UsageQuality::Exact,
        }
    );
}

/// A hosted web_search request that does not force the tool goes down the
/// ordinary path, where the lite-only model is upgraded to a full-lane one. The
/// monitor must report the model that reached the backend, with the one the
/// caller asked for still named beside it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_web_search_reports_the_full_lane_model_it_ran_on() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            *captured.lock().unwrap() = Some(body);
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"searched\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":9,\"output_tokens\":3}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.6-luna",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"find it"}],
            "tools": [{"type":"web_search_20250305","name":"web_search"}]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = drain_stream(response).await;

    // What the backend was actually asked to run.
    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    assert_eq!(sent["model"], "gpt-5.6-sol");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.requested_model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(request.effective_model.as_deref(), Some("gpt-5.6-sol"));
    let row = state.sessions[0]
        .models
        .iter()
        .find(|row| row.model.as_deref() == Some("gpt-5.6-sol"))
        .expect("a row for the model that ran");
    assert_eq!(row.provider.as_deref(), Some("codex"));
    assert_eq!(row.request_count, 1);
    assert_eq!(
        row.requested_models
            .iter()
            .map(|(model, count)| (model.as_deref(), *count))
            .collect::<Vec<_>>(),
        vec![(Some("gpt-5.6-luna"), 1)]
    );
}

/// A `-fast` id and the configured model override both change what leaves for
/// the backend without changing what the caller asked for. The pair is read off
/// the outgoing request, never off the display that joins them.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_fast_alias_and_model_override_report_what_left() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            captured.lock().unwrap().push(body);
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let monitor = MonitorHandle::new(10);
    let fast = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.6-sol-fast",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role":"user","content":"hello"}]
        }),
    )
    .await;
    assert_eq!(fast.status(), StatusCode::OK);
    let _ = drain_stream(fast).await;

    {
        let _model_env = EnvGuard::set("CCP_CODEX_MODEL", "gpt-5.6-terra");
        let overridden = call_messages_body_with_monitor(
            monitor.clone(),
            json!({
                "model": "gpt-5.6-sol",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role":"user","content":"hello again"}]
            }),
        )
        .await;
        assert_eq!(overridden.status(), StatusCode::OK);
        let _ = drain_stream(overridden).await;
    }

    let sent = captured.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    // The `-fast` id is a service tier, not a model of its own.
    assert_eq!(sent[0]["model"], "gpt-5.6-sol");
    assert_eq!(sent[1]["model"], "gpt-5.6-terra");

    let state = monitor.snapshot();
    let by_requested = |requested: &str| {
        state
            .recent
            .iter()
            .find(|request| request.requested_model.as_deref() == Some(requested))
            .unwrap_or_else(|| panic!("no request asked for {requested}"))
    };
    assert_eq!(
        by_requested("gpt-5.6-sol-fast").effective_model.as_deref(),
        Some("gpt-5.6-sol")
    );
    assert_eq!(
        by_requested("gpt-5.6-sol").effective_model.as_deref(),
        Some("gpt-5.6-terra")
    );
    let session = &state.sessions[0];
    let row = |model: &str| {
        session
            .models
            .iter()
            .find(|row| row.model.as_deref() == Some(model))
            .unwrap_or_else(|| panic!("no row for {model}"))
    };
    assert_eq!(
        row("gpt-5.6-sol")
            .requested_models
            .iter()
            .map(|(model, count)| (model.as_deref(), *count))
            .collect::<Vec<_>>(),
        vec![(Some("gpt-5.6-sol-fast"), 1)]
    );
    assert_eq!(
        row("gpt-5.6-terra")
            .requested_models
            .iter()
            .map(|(model, count)| (model.as_deref(), *count))
            .collect::<Vec<_>>(),
        vec![(Some("gpt-5.6-sol"), 1)]
    );
}

/// Forcing the hosted tool takes the standalone search branch instead, which
/// posts to a different endpoint and keeps the lite-only model. The two paths
/// are observed separately, so this one must report what its own request
/// carried rather than what the ordinary path would have chosen.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_codex_forced_search_reports_the_model_its_own_request_carried() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            *captured.lock().unwrap() = Some(body);
            serde_json::to_vec(&json!({
                "encrypted_output": "opaque",
                "output": "search output",
                "results": [{
                    "type": "text_result",
                    "ref_id": "turn0search0",
                    "url": "https://example.com",
                    "title": "Example"
                }]
            }))
            .unwrap()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.6-luna",
            "max_tokens": 64,
            "stream": false,
            "messages": [{"role":"user","content":"find Codex"}],
            "tools": [{"type":"web_search_20250305","name":"web_search"}],
            "tool_choice": {"type":"tool","name":"web_search"}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    assert_eq!(sent["model"], "gpt-5.6-luna");
    assert_eq!(sent["commands"]["search_query"][0]["q"], "find Codex");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.requested_model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(request.effective_model.as_deref(), Some("gpt-5.6-luna"));
}

/// The security classifier is rerouted to another model. What the caller asked
/// for is kept as it arrived, and what ran is read off the request that left.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn smoke_auto_review_keeps_the_requested_model_beside_the_one_that_ran() {
    let _guard = env_lock();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    clear_all_continuations_for_tests();

    let captured = Arc::new(Mutex::new(None));
    let upstream = spawn_http_upstream({
        let captured = captured.clone();
        move |body: Value| {
            *captured.lock().unwrap() = Some(body);
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"review ok\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
            )
            .as_bytes()
            .to_vec()
        }
    })
    .await;

    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");

    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(
        monitor.clone(),
        json!({
            "model": "gpt-5.6-sol",
            "max_tokens": 64,
            "stream": false,
            "system": [{
                "type": "text",
                "text": "You are a security monitor for autonomous AI coding agents.\n\n## Context"
            }],
            "messages": [{"role":"user","content":"review this Bash command"}],
            "tools": []
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    assert_eq!(sent["model"], "gpt-5.6-luna");

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.requested_model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(request.effective_model.as_deref(), Some("gpt-5.6-luna"));
    let row = state.sessions[0]
        .models
        .iter()
        .find(|row| row.model.as_deref() == Some("gpt-5.6-luna"))
        .expect("a row for the model that ran");
    assert_eq!(
        row.requested_models
            .iter()
            .map(|(model, count)| (model.as_deref(), *count))
            .collect::<Vec<_>>(),
        vec![(Some("gpt-5.6-sol"), 1)]
    );
}

/// The relay forwards the caller's own bytes, so the model that reaches
/// Anthropic is the one written in them — not the one the proxy pointed the
/// typed request at. The agent-summary override rewrites the typed model and
/// leaves the bytes alone, so the two differ, and the monitor has to show the
/// difference rather than either half of it.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn smoke_anthropic_wire_model_is_the_one_in_the_relayed_bytes() {
    let _guard = env_lock();
    const UPSTREAM_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let upstream = spawn_capturing_http_upstream(captured.clone(), UPSTREAM_SSE).await;
    let _base_url_env = EnvGuard::set("CCP_ANTHROPIC_BASE_URL", &upstream);
    // Send the label request to a model instead of answering it locally, which
    // is what makes the typed model and the relayed bytes disagree.
    let _summary_env = EnvGuard::set("CCP_AGENT_SUMMARY", "upstream");

    let body = json!({
        "model": "claude-opus-5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"Describe your most recent action in 3-5 words"}]
    });
    let monitor = MonitorHandle::new(10);
    let response = call_messages_body_with_monitor(monitor.clone(), body.clone()).await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = drain_stream(response).await;
    assert_eq!(text, UPSTREAM_SSE, "the relay must stay byte-exact");

    let relayed = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    assert_eq!(
        relayed,
        body.to_string().into_bytes(),
        "the relay must forward the client's bytes verbatim"
    );

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.provider.as_deref(), Some("anthropic"));
    // The override chose claude-sonnet-5 for the typed request; the bytes on the
    // wire still asked for claude-opus-5, and that is what answered.
    assert_eq!(
        request.model.as_deref(),
        Some("claude-sonnet-5 → claude-opus-5")
    );
    assert_eq!(request.requested_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(request.effective_model.as_deref(), Some("claude-opus-5"));
    let row = state.sessions[0]
        .models
        .iter()
        .find(|row| row.model.as_deref() == Some("claude-opus-5"))
        .expect("a row for the model that answered");
    assert_eq!(row.provider.as_deref(), Some("anthropic"));
    assert_eq!(row.request_count, 1);
    assert_eq!(row.input_tokens, 11);
    assert_eq!(row.output_tokens, 4);
}
