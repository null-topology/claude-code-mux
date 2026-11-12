use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use hyper_util::client::proxy::matcher::Matcher as ProxyMatcher;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{CloseFrame, Role, frame::coding::CloseCode},
    },
};

use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;
use crate::retry::sleep as retry_sleep;
use crate::traffic::TrafficCapture;

use super::client::{
    ActualTransport, CodexError, CodexErrorOrigin, CodexResponse, MAX_ERROR_BODY_BYTES,
    OwnerAwareCodexResponse, buffered_response_too_large_error, headers_to_json,
};
use super::continuation::ContinuationReservation;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
pub const WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL: &str = "websocket_response_start_timeout";
pub const WEBSOCKET_MISSING_TERMINAL_DETAIL: &str = "websocket_missing_terminal";
pub const WEBSOCKET_KEEPALIVE_FAILURE_DETAIL: &str = "websocket_keepalive_failure";
pub const WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL: &str =
    "websocket_continuation_socket_missing";
pub(super) const WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL: &str = "websocket_proxy_tunnel_rejected";
/// Marks a write that ran out of time. Part of the frame may already be on
/// the socket, so the request is neither resent nor replayed over HTTP.
pub(super) const WEBSOCKET_WRITE_TIMEOUT_DETAIL: &str = "websocket_write_timeout";
/// Marks a new connection that was not attempted because the connect pause of
/// its target runs (`WEBSOCKET_CONNECT_PAUSE`). Nothing was sent, so the
/// request goes over HTTP.
pub(super) const WEBSOCKET_CONNECT_PAUSED_DETAIL: &str = "websocket_connect_paused";
/// The handshake response header naming the largest request frame, in bytes,
/// that the socket accepts. The Codex CLI reads it and keeps a longer request
/// off the socket (`codex-rs/codex-api/src/endpoint/responses_websocket.rs`
/// at `ed7ca22bb6`).
const MAX_MESSAGE_BYTES_HEADER: &str = "x-codex-websocket-max-message-bytes";
/// Marks a request frame longer than the limit its socket's handshake named.
/// Nothing was written, so under `auto` the request goes over HTTP.
pub(super) const WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL: &str = "websocket_message_too_large";
/// Marks a close with code 1009 (message too big) that arrived before any text
/// frame of the response. The proxy treats it as the Codex CLI does, as a
/// request too large for the WebSocket, so under `auto` the request goes over
/// HTTP.
pub(super) const WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL: &str =
    "websocket_closed_message_too_large";

const POOL_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
const MAX_POOL_ENTRIES: usize = 10_000;
const POOL_CONNECT_CLEANUP_THRESHOLD: usize = 50;
const POOL_CONNECT_CLEANUP_TARGET: usize = 40;
const MAX_CONNECT_RESPONSE_HEADER_BYTES: usize = 8 * 1024;
const WEBSOCKET_CONNECT_START_SPACING: Duration = Duration::from_secs(1);
const WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN: Duration = Duration::from_secs(3);
/// How long a request that falls back to HTTP skips new connections to a
/// target after a connect to it got no answer. The Codex CLI stays on HTTP for
/// the rest of its session once it fell back (`codex-rs/core/src/client.rs` at
/// `4460449921`); the proxy outlives a session by far, so it pauses instead.
const WEBSOCKET_CONNECT_PAUSE: Duration = Duration::from_secs(5 * 60);
const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const WEBSOCKET_KEEPALIVE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

// Terminal WebSocket event types that signal the request is done
const TERMINAL_EVENTS: &[&str] = &[
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
];

pub type CodexWebSocketEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;

pub(crate) struct CodexWebSocketEventStream {
    receiver: CodexWebSocketEventReceiver,
    socket_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
    auth_refresh: Arc<AtomicBool>,
    account_id: Arc<std::sync::Mutex<Option<String>>>,
    /// The access token of the attempt, held only to tell whether a replay
    /// goes out with the same credential. The type has no `Debug`.
    access_token: Arc<std::sync::Mutex<Option<String>>>,
    rate_limits_observed: bool,
}

#[derive(Clone)]
pub(crate) struct CodexWebSocketSocketIdPublisher {
    socket_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
    auth_refresh: Arc<AtomicBool>,
    account_id: Arc<std::sync::Mutex<Option<String>>>,
    access_token: Arc<std::sync::Mutex<Option<String>>>,
}

impl CodexWebSocketEventStream {
    pub(crate) fn pending(
        receiver: CodexWebSocketEventReceiver,
    ) -> (Self, CodexWebSocketSocketIdPublisher) {
        let socket_id = Arc::new(AtomicU64::new(0));
        let full_context_retry = Arc::new(AtomicBool::new(false));
        let provider_retry_handoff = Arc::new(AtomicBool::new(false));
        let auth_refresh = Arc::new(AtomicBool::new(false));
        let account_id = Arc::new(std::sync::Mutex::new(None));
        let access_token = Arc::new(std::sync::Mutex::new(None));
        (
            Self {
                receiver,
                socket_id: socket_id.clone(),
                full_context_retry: full_context_retry.clone(),
                provider_retry_handoff: provider_retry_handoff.clone(),
                auth_refresh: auth_refresh.clone(),
                account_id: account_id.clone(),
                access_token: access_token.clone(),
                rate_limits_observed: false,
            },
            CodexWebSocketSocketIdPublisher {
                socket_id,
                full_context_retry,
                provider_retry_handoff,
                auth_refresh,
                account_id,
                access_token,
            },
        )
    }

    /// Whether the task behind the stream already refreshed the token for
    /// this request, so a fallback that follows does not refresh it again.
    pub(crate) fn used_auth_refresh(&self) -> bool {
        self.auth_refresh.load(Ordering::Acquire)
    }

    /// The ChatGPT account the stream's request was last sent with, which
    /// the quota gauge keys its readings by.
    pub(crate) fn account_id(&self) -> Option<String> {
        self.account_reader()()
    }

    /// The access token the stream's request was last sent with, which a
    /// size refusal's replay compares its own with when an account id is
    /// unknown. It is never logged.
    pub(crate) fn access_token(&self) -> Option<String> {
        self.access_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Reads the stream's account after the stream itself has been handed
    /// on, for a response built once the stream has started.
    pub(crate) fn account_reader(&self) -> impl Fn() -> Option<String> + use<> {
        let account_id = self.account_id.clone();
        move || {
            account_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    /// Whether the task behind the stream records each quota reading for the
    /// account of the attempt that read it before queuing it, so the reader
    /// does not record it again under the stream's current account.
    pub(crate) fn rate_limits_observed(&self) -> bool {
        self.rate_limits_observed
    }

    pub(super) fn mark_rate_limits_observed(&mut self) {
        self.rate_limits_observed = true;
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<serde_json::Value, CodexError>> {
        self.receiver.recv().await
    }

    pub(crate) fn socket_id(&self) -> Option<u64> {
        match self.socket_id.load(Ordering::Acquire) {
            0 => None,
            socket_id => Some(socket_id),
        }
    }

    pub(crate) fn used_full_context_retry(&self) -> bool {
        self.full_context_retry.load(Ordering::Acquire)
    }

    pub(crate) fn mark_provider_retry_handoff(&self) {
        self.provider_retry_handoff.store(true, Ordering::Release);
    }

    pub(crate) fn into_receiver(self) -> CodexWebSocketEventReceiver {
        self.receiver
    }

    pub(crate) fn replace_receiver(
        &mut self,
        receiver: CodexWebSocketEventReceiver,
    ) -> CodexWebSocketEventReceiver {
        std::mem::replace(&mut self.receiver, receiver)
    }
}

impl CodexWebSocketSocketIdPublisher {
    pub(super) fn publish(&self, socket_id: Option<u64>) {
        self.socket_id
            .store(socket_id.unwrap_or(0), Ordering::Release);
    }

    pub(super) fn mark_full_context_retry(&self) {
        self.full_context_retry.store(true, Ordering::Release);
    }

    pub(super) fn mark_auth_refresh(&self) {
        self.auth_refresh.store(true, Ordering::Release);
    }

    pub(super) fn publish_account(&self, account_id: Option<&str>) {
        *self
            .account_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = account_id.map(str::to_owned);
    }

    /// Publishes the access token of the attempt about to go out, beside its
    /// account.
    pub(super) fn publish_access_token(&self, access_token: &str) {
        *self
            .access_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(access_token.to_owned());
    }

    pub(super) fn is_provider_retry_handoff(&self) -> bool {
        self.provider_retry_handoff.load(Ordering::Acquire)
    }
}

trait WebSocketIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> WebSocketIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

type BoxedWebSocketIo = Box<dyn WebSocketIo>;
type CodexWebSocketStream = WebSocketStream<BoxedWebSocketIo>;

pub(super) struct WebSocketProxyConfig {
    matcher: ProxyMatcher,
    tls_config: Arc<rustls::ClientConfig>,
}

#[derive(Clone)]
struct WebSocketProxyRoute {
    uri: http::Uri,
    basic_auth: Option<http::HeaderValue>,
}

impl WebSocketProxyConfig {
    pub(super) fn new(
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
        all_proxy: Option<&str>,
        no_proxy: Option<&str>,
    ) -> Self {
        let mut builder = ProxyMatcher::builder();
        if let Some(proxy) = all_proxy {
            builder = builder.all(proxy.to_string());
        }
        if let Some(proxy) = http_proxy {
            builder = builder.http(proxy.to_string());
        }
        if let Some(proxy) = https_proxy {
            builder = builder.https(proxy.to_string());
        }
        if let Some(no_proxy) = no_proxy {
            builder = builder.no(no_proxy.to_string());
        }
        Self {
            matcher: builder.build(),
            tls_config: websocket_tls_config(),
        }
    }

    #[cfg(test)]
    pub(super) fn direct() -> Self {
        Self::new(None, None, None, None)
    }

    pub(super) fn uses_proxy_for(&self, websocket_url: &str) -> bool {
        let Ok(http_url) = to_http_upgrade_url(websocket_url) else {
            return true;
        };
        let Ok(destination) = http_url.parse::<http::Uri>() else {
            return true;
        };
        self.matcher.intercept(&destination).is_some()
    }

    fn http_connect_route(
        &self,
        websocket_url: &str,
    ) -> Result<Option<WebSocketProxyRoute>, CodexError> {
        let http_url = to_http_upgrade_url(websocket_url).map_err(|error| CodexError {
            status: 0,
            message: error.message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
        if !http_url.starts_with("https://") {
            return Ok(None);
        }
        let destination = http_url.parse::<http::Uri>().map_err(|_| {
            websocket_protocol_error("WebSocket destination URL could not be routed")
        })?;
        let Some(proxy) = self.matcher.intercept(&destination) else {
            return Ok(None);
        };
        if !matches!(proxy.uri().scheme_str(), Some("http" | "https")) {
            return Ok(None);
        }
        Ok(Some(WebSocketProxyRoute {
            uri: proxy.uri().clone(),
            basic_auth: proxy.basic_auth().cloned(),
        }))
    }
}

static WEBSOCKET_TLS_CONFIG: once_cell::sync::Lazy<Arc<rustls::ClientConfig>> =
    once_cell::sync::Lazy::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        let load_error_count = native.errors.len();
        let (_, parse_error_count) = roots.add_parsable_certificates(native.certs);
        if load_error_count > 0 || parse_error_count > 0 {
            let mut fields = serde_json::Map::new();
            fields.insert("loadErrorCount".into(), serde_json::json!(load_error_count));
            fields.insert(
                "parseErrorCount".into(),
                serde_json::json!(parse_error_count),
            );
            create_logger("codex").warn("native_certificate_load_errors", Some(fields));
        }
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });

pub(super) fn websocket_tls_config() -> Arc<rustls::ClientConfig> {
    WEBSOCKET_TLS_CONFIG.clone()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CodexWebSocketError {
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub retry_after: Option<String>,
    pub request_sent: bool,
}

impl CodexWebSocketError {
    pub fn new(message: String) -> Self {
        Self {
            message,
            status: None,
            code: None,
            retry_after: None,
            request_sent: false,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_code(mut self, code: String) -> Self {
        self.code = Some(code);
        self
    }
}

impl std::fmt::Display for CodexWebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex WebSocket error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

struct PoolEntry {
    ws: Arc<AsyncMutex<CodexWebSocketStream>>,
    socket_id: u64,
    created_at: u64,
    last_activity: AtomicU64,
    /// The `ChatGPT-Account-Id` of the handshake. The backend answers every
    /// request on the socket under that account, whatever account a later
    /// request is built with.
    account_id: Option<String>,
    /// The access token of the handshake, held in memory only so a socket
    /// whose handshake named no account goes only to a request with the same
    /// token. Never logged; `PoolEntry` has no `Debug`.
    access_token: Option<String>,
    /// The message size limit the handshake response advertised, if any. A
    /// pooled socket keeps the limit of its own handshake.
    max_message_bytes: Option<usize>,
}

impl PoolEntry {
    fn opened_with(connected: ConnectedWebSocket, handshake_headers: &HeaderMap) -> Self {
        Self {
            ws: Arc::new(AsyncMutex::new(connected.stream)),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            created_at: now_ms(),
            last_activity: AtomicU64::new(next_pool_activity()),
            account_id: request_account_id(handshake_headers).map(str::to_owned),
            access_token: request_access_token(handshake_headers).map(str::to_owned),
            max_message_bytes: connected.max_message_bytes,
        }
    }

    /// Whether a request built with `headers` may go out on this socket: both
    /// account ids are known and equal, so a renewed token of the same
    /// account keeps the socket, or neither is known and the request's access
    /// token is the one of the handshake. Every request the client builds
    /// carries a bearer token (`build_codex_headers`), so two tokens that
    /// both cannot be read, which compare equal here, meet only where a test
    /// builds a socket and a request without headers.
    fn serves_request(&self, headers: &HeaderMap) -> bool {
        match (self.account_id.as_deref(), request_account_id(headers)) {
            (Some(socket), Some(request)) => socket == request,
            (None, None) => self.access_token.as_deref() == request_access_token(headers),
            _ => false,
        }
    }

    /// An entry whose handshake named no account and no message size limit.
    #[cfg(test)]
    fn new(ws: CodexWebSocketStream) -> Self {
        Self::opened_with(
            ConnectedWebSocket {
                stream: ws,
                max_message_bytes: None,
            },
            &HeaderMap::new(),
        )
    }

    fn touch(&self) {
        self.last_activity
            .fetch_max(next_pool_activity(), Ordering::Relaxed);
    }

    /// Refuses a request frame longer than the limit this socket's handshake
    /// advertised, before anything is written, as the Codex CLI's
    /// `send_websocket_request` does
    /// (`codex-rs/codex-api/src/endpoint/responses_websocket.rs` at
    /// `ed7ca22bb6`).
    fn check_message_size(&self, frame_bytes: usize) -> Result<(), CodexError> {
        match self.max_message_bytes {
            Some(limit) if frame_bytes > limit => Err(message_too_large_error(frame_bytes, limit)),
            _ => Ok(()),
        }
    }
}

/// A socket whose handshake succeeded, with the message size limit that
/// handshake advertised.
struct ConnectedWebSocket {
    stream: CodexWebSocketStream,
    max_message_bytes: Option<usize>,
}

/// Reads the message size limit a successful handshake advertised: a trimmed
/// positive integer is the limit, and an absent, empty, zero or unparsable
/// value means none.
fn advertised_max_message_bytes(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(MAX_MESSAGE_BYTES_HEADER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|limit| *limit > 0)
}

/// Logs the message size limit a new socket's handshake advertised. The line
/// carries the request id and the number, and no other header value.
fn log_advertised_message_limit(ctx: &RequestContext, entry: &PoolEntry) {
    if let Some(limit) = entry.max_message_bytes {
        let fields = serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
            ("maxMessageBytes".to_string(), serde_json::json!(limit)),
        ]);
        create_logger("codex").info("codex_websocket_message_limit", Some(fields));
    }
}

/// True for the two refusals of a request frame for its size: one kept off the
/// socket by the advertised limit, and a close with code 1009 seen before any
/// text frame of the response, which the Codex CLI also reads as a request too
/// large for the WebSocket.
pub(super) fn is_message_size_refusal(error: &CodexError) -> bool {
    error.origin == CodexErrorOrigin::WebSocket
        && matches!(
            error.detail.as_deref(),
            Some(WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL | WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL)
        )
}

fn message_too_large_error(frame_bytes: usize, limit: usize) -> CodexError {
    CodexError {
        status: http::StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
        message: format!(
            "The WebSocket request frame is {frame_bytes} bytes, over the {limit}-byte message limit the Codex backend advertised"
        ),
        detail: Some(WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn closed_message_too_large_error() -> CodexError {
    CodexError {
        status: http::StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
        message: "The Codex backend closed the WebSocket with code 1009 (message too big) before any response".to_string(),
        detail: Some(WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

/// The numeric code of a close frame, 0 when the frame carries none.
fn close_code(frame: Option<&CloseFrame<'_>>) -> u16 {
    frame.map_or(0, |frame| u16::from(frame.code))
}

/// True for a 1009 close that arrived before any text frame of the response,
/// the case the Codex CLI answers as a message too large for the socket
/// (`codex-rs/codex-api/src/endpoint/responses_websocket.rs` at `ed7ca22bb6`).
fn closed_for_message_size(frame: Option<&CloseFrame<'_>>, text_received: bool) -> bool {
    !text_received && frame.is_some_and(|frame| frame.code == CloseCode::Size)
}

/// Logs a close frame that ended a response: its numeric code and whether
/// response text had arrived. The close reason is never logged.
fn log_response_close(req_id: &str, code: u16, text_received: bool) {
    let fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        ("closeCode".to_string(), serde_json::json!(code)),
        ("textStarted".to_string(), serde_json::json!(text_received)),
    ]);
    create_logger("codex").info("codex_websocket_closed", Some(fields));
}

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
static POOLED_VALIDATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static POOL_ACTIVITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static WS_POOL: once_cell::sync::Lazy<Mutex<HashMap<ConversationIdentity, Arc<PoolEntry>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
static WS_POOL_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
static WS_CONNECT_GATE: once_cell::sync::Lazy<WebSocketConnectGate> =
    once_cell::sync::Lazy::new(|| WebSocketConnectGate::new(WEBSOCKET_CONNECT_START_SPACING));
#[cfg(test)]
tokio::task_local! {
    /// A gate that connects made in this task go through instead of
    /// `WS_CONNECT_GATE`, so a test reads a gate no other test touches.
    static TEST_CONNECT_GATE: Arc<WebSocketConnectGate>;
}
/// When the connect pause of each target ends, keyed by `connect_pause_key`.
static WS_CONNECT_PAUSES: once_cell::sync::Lazy<Mutex<HashMap<String, Instant>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
/// Test override of the request frame's write budget in milliseconds; 0 means
/// the path's idle budget.
static REQUEST_WRITE_TIMEOUT_MS_FOR_TESTS: AtomicU64 = AtomicU64::new(0);

fn next_monotonic_nonzero(sequence: &AtomicU64, label: &str) -> u64 {
    // `fetch_update` is deprecated in favour of `try_update`, which older
    // stable toolchains do not have yet.
    #[allow(deprecated)]
    let previous = sequence
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{label} sequence exhausted"));
    previous + 1
}

fn next_pool_activity() -> u64 {
    POOL_ACTIVITY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn clear_codex_websocket_pool_for_tests() {
    let mut guard = WS_POOL.lock().unwrap();
    guard.clear();
}

/// Ends the connect pause of `url`'s target, if one runs, as if its time had
/// passed: the entry stays until the map is next touched.
#[doc(hidden)]
pub fn expire_codex_websocket_connect_pause_for_tests(url: &str) {
    let now = Instant::now();
    if let Some(ends_at) = lock_connect_pauses().get_mut(&connect_pause_key(url)) {
        *ends_at = now;
    }
}

/// Starts the connect pause of `url`'s target without a failed connect.
#[cfg(test)]
pub(super) fn pause_codex_websocket_connects_for_tests(url: &str) {
    lock_connect_pauses().insert(
        connect_pause_key(url),
        Instant::now() + WEBSOCKET_CONNECT_PAUSE,
    );
}

/// Shortens the deadline of a request frame's write, so a test can wait out a
/// peer that never reads without the full idle budget. `None` restores the
/// idle budget.
#[doc(hidden)]
pub fn set_codex_websocket_request_write_timeout_for_tests(timeout: Option<Duration>) {
    let millis = timeout.map_or(0, |timeout| (timeout.as_millis() as u64).max(1));
    REQUEST_WRITE_TIMEOUT_MS_FOR_TESTS.store(millis, Ordering::Relaxed);
}

/// Holds the request write budget override for one test.
#[cfg(test)]
pub(crate) struct RequestWriteTimeoutOverride;

#[cfg(test)]
impl RequestWriteTimeoutOverride {
    pub(crate) fn set(timeout: Duration) -> Self {
        set_codex_websocket_request_write_timeout_for_tests(Some(timeout));
        Self
    }
}

#[cfg(test)]
impl Drop for RequestWriteTimeoutOverride {
    fn drop(&mut self) {
        set_codex_websocket_request_write_timeout_for_tests(None);
    }
}

#[cfg(test)]
pub(crate) async fn lock_codex_websocket_pool_for_tests() -> tokio::sync::MutexGuard<'static, ()> {
    WS_POOL_TEST_LOCK.lock().await
}

#[cfg(test)]
pub(crate) fn pooled_socket_id_for_tests(owner: &ConversationIdentity) -> Option<u64> {
    WS_POOL
        .lock()
        .unwrap()
        .get(owner)
        .map(|entry| entry.socket_id)
}

pub fn invalidate_codex_websocket_pool_owner(owner: &ConversationIdentity) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.remove(owner);
}

#[deprecated(note = "use typed conversation ownership internally")]
pub fn invalidate_codex_websocket_pool_key(session_id: &str) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.retain(|owner, _| match owner {
        ConversationIdentity::Main(owner_session_id)
        | ConversationIdentity::Agent(owner_session_id, _) => owner_session_id != session_id,
    });
}

pub(crate) fn invalidate_codex_websocket_pool_turn_for_owner(
    owner: &ConversationIdentity,
    turn_id: Option<u64>,
) {
    let reservation = ContinuationReservation::for_owner_turn(Some(owner), turn_id);
    super::continuation::with_current_turn_for_owner(&reservation, || {
        invalidate_codex_websocket_pool_owner(owner)
    });
}

#[deprecated(note = "use typed conversation ownership internally")]
pub fn invalidate_codex_websocket_pool_turn(session_id: &str, turn_id: Option<u64>) {
    let owner = ConversationIdentity::Main(session_id.to_owned());
    invalidate_codex_websocket_pool_turn_for_owner(&owner, turn_id);
}

fn invalidate_pool_entry(owner: &ConversationIdentity, entry: &Arc<PoolEntry>) {
    let mut guard = WS_POOL.lock().unwrap();
    if guard
        .get(owner)
        .is_some_and(|pooled| Arc::ptr_eq(pooled, entry))
    {
        guard.remove(owner);
    }
}

fn invalidate_pool_owner(owner: Option<&ConversationIdentity>, entry: Option<&Arc<PoolEntry>>) {
    let Some(owner) = owner else {
        return;
    };
    match entry {
        Some(entry) => invalidate_pool_entry(owner, entry),
        None => invalidate_codex_websocket_pool_owner(owner),
    }
}

fn reservation_pool_owner(
    reservation: Option<&ContinuationReservation>,
) -> Option<&ConversationIdentity> {
    let reservation = reservation?;
    if reservation.candidate().disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    reservation.owner()
}

fn pool_take_for_turn(reservation: &ContinuationReservation) -> Option<Arc<PoolEntry>> {
    let owner = reservation.owner()?;
    super::continuation::if_current_turn_for_owner(reservation, || {
        WS_POOL.lock().ok()?.remove(owner)
    })
    .flatten()
}

/// The pooled socket a request may go out on. `headers` are the ones the
/// request is built with: a socket that does not serve them
/// (`PoolEntry::serves_request`: another account, an account known on one
/// side only, or, with no account on either side, another access token)
/// leaves the pool and counts as gone, since what the backend answers on it
/// would be read as this login's. A request that needed it for its previous
/// response then fails as one whose socket is gone; any other request opens
/// a connection with its own headers.
fn take_pool_entry_for_request(
    reservation: Option<&ContinuationReservation>,
    headers: &HeaderMap,
) -> Result<Option<Arc<PoolEntry>>, CodexError> {
    let candidate = reservation.map(ContinuationReservation::candidate);
    let requires_origin = candidate
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let expected_socket_id = reservation.and_then(ContinuationReservation::origin_socket_id);
    let pool_owner = reservation_pool_owner(reservation);
    let pooled = reservation
        .filter(|_| pool_owner.is_some())
        .and_then(pool_take_for_turn)
        .filter(|entry| entry.serves_request(headers));

    if requires_origin
        && (pool_owner.is_none()
            || expected_socket_id.is_none()
            || pooled
                .as_ref()
                .is_none_or(|entry| Some(entry.socket_id) != expected_socket_id))
    {
        if let (Some(owner), Some(entry)) = (pool_owner, pooled.as_ref()) {
            pool_insert_if_vacant_or_same(owner.clone(), entry.clone());
        }
        return Err(continuation_socket_missing_error());
    }

    Ok(pooled)
}

fn pool_insert_for_turn(reservation: &ContinuationReservation, entry: Arc<PoolEntry>) -> bool {
    let Some(owner) = reservation_pool_owner(Some(reservation)).cloned() else {
        return false;
    };
    super::continuation::if_current_turn_for_owner(reservation, || {
        pool_insert_if_vacant_or_same(owner, entry)
    })
    .unwrap_or(false)
}

fn pool_remove_entry(owner: &ConversationIdentity, entry: &Arc<PoolEntry>) {
    invalidate_pool_entry(owner, entry);
}

pub(super) fn invalidate_codex_websocket_pool_socket(
    reservation: &ContinuationReservation,
    socket_id: Option<u64>,
) {
    let Some(owner) = reservation_pool_owner(Some(reservation)) else {
        return;
    };
    let Some(socket_id) = socket_id else {
        return;
    };
    let entry = WS_POOL.lock().ok().and_then(|pool| {
        pool.get(owner)
            .filter(|entry| entry.socket_id == socket_id)
            .cloned()
    });
    if let Some(entry) = entry {
        pool_remove_entry(owner, &entry);
    }
}

fn pool_insert_if_vacant_or_same(owner: ConversationIdentity, entry: Arc<PoolEntry>) -> bool {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    if let Some(existing) = guard.get(&owner) {
        return Arc::ptr_eq(existing, &entry);
    }
    if guard.len() >= MAX_POOL_ENTRIES
        && let Some(oldest_owner) = guard.keys().next().cloned()
    {
        guard.remove(&oldest_owner);
    }
    let now = now_ms();
    guard.retain(|_, pooled| now.saturating_sub(pooled.created_at) < POOL_IDLE_TTL_MS);
    guard.insert(owner, entry);
    true
}

#[cfg(test)]
fn pool_insert(owner: ConversationIdentity, entry: Arc<PoolEntry>) {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    // Evict oldest if at capacity
    if guard.len() >= MAX_POOL_ENTRIES
        && let Some(oldest_owner) = guard.keys().next().cloned()
    {
        guard.remove(&oldest_owner);
    }
    // Evict expired entries
    let now = now_ms();
    guard.retain(|_, entry| now.saturating_sub(entry.created_at) < POOL_IDLE_TTL_MS);
    guard.insert(owner, entry);
}

fn cleanup_pool_before_connect() {
    let removed = {
        let mut guard = WS_POOL.lock().unwrap();
        if guard.len() <= POOL_CONNECT_CLEANUP_THRESHOLD {
            return;
        }

        let remove_count = guard.len() - POOL_CONNECT_CLEANUP_TARGET;
        let mut candidates: Vec<_> = guard
            .iter()
            .filter(|(_, entry)| Arc::strong_count(entry) == 1)
            .map(|(owner, entry)| (owner.clone(), entry.last_activity.load(Ordering::Relaxed)))
            .collect();
        candidates.sort_unstable_by_key(|(_, activity)| *activity);
        candidates
            .into_iter()
            .take(remove_count)
            .filter_map(|(owner, _)| guard.remove(&owner))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

struct WebSocketConnectGate {
    last_start: AsyncMutex<Option<tokio::time::Instant>>,
    start_spacing: Duration,
    /// How many calls have entered `wait_to_start`, so a test can tell that a
    /// connect reached the gate.
    #[cfg(test)]
    arrivals: AtomicU64,
}

impl WebSocketConnectGate {
    fn new(start_spacing: Duration) -> Self {
        Self {
            last_start: AsyncMutex::new(None),
            start_spacing,
            #[cfg(test)]
            arrivals: AtomicU64::new(0),
        }
    }

    /// A `before_start` that fails starts nothing and leaves the last start as
    /// it is, so the next start is spaced from that one.
    async fn wait_to_start<E>(
        &self,
        before_start: impl std::future::Future<Output = Result<(), E>>,
    ) -> Result<(), E> {
        #[cfg(test)]
        self.arrivals.fetch_add(1, Ordering::SeqCst);
        let mut last_start = self.last_start.lock().await;
        if let Some(previous) = *last_start {
            tokio::time::sleep_until(previous + self.start_spacing).await;
        }
        before_start.await?;
        *last_start = Some(tokio::time::Instant::now());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// URL conversion
// ---------------------------------------------------------------------------

pub fn to_websocket_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "http" => parsed.set_scheme("ws").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "https" => parsed.set_scheme("wss").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "ws" | "wss" => { /* already a ws scheme */ }
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

fn to_http_upgrade_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "ws" => parsed.set_scheme("http").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "wss" => parsed.set_scheme("https").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

// ---------------------------------------------------------------------------
// Header rewriting
// ---------------------------------------------------------------------------

pub fn codex_websocket_headers(http_headers: &HeaderMap) -> HeaderMap {
    let mut ws = HeaderMap::new();
    for (key, value) in http_headers.iter() {
        let key_str = key.as_str().to_lowercase();
        // Skip hop-by-hop headers
        if matches!(
            key_str.as_str(),
            "content-length"
                | "content-type"
                | "accept"
                | "connection"
                | "upgrade"
                | "proxy-authorization"
        ) {
            continue;
        }
        ws.append(key.clone(), value.clone());
    }
    // Rewrite openai-beta for WebSocket protocol
    ws.insert("openai-beta", WEBSOCKET_PROTOCOL_HEADER.parse().unwrap());
    // Ensure WebSocket key is present
    if !ws.contains_key("sec-websocket-key") {
        ws.insert("sec-websocket-key", generate_key().parse().unwrap());
    }
    ws
}

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

fn encode_sse(text: &str) -> Vec<u8> {
    let mut out = String::new();
    for line in text.lines() {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

/// The length of `encode_sse(text)`, worked out without building it: each
/// line takes `data: ` and a newline, and the event a closing newline.
fn encoded_sse_len(text: &str) -> usize {
    text.lines().fold(1, |len, line| {
        len.saturating_add(line.len().saturating_add(7))
    })
}

// ---------------------------------------------------------------------------
// Terminal event detection
// ---------------------------------------------------------------------------

pub(super) fn is_terminal_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some(t) => TERMINAL_EVENTS.contains(&t),
        None => false,
    }
}

fn is_response_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("error") => true,
        Some(t) => t.starts_with("response."),
        None => false,
    }
}

fn is_previous_response_missing(payload: &serde_json::Value) -> bool {
    let error = super::events::event_error(payload);
    if error
        .and_then(|error| error.get("code"))
        .and_then(|value| value.as_str())
        == Some("previous_response_not_found")
    {
        return true;
    }
    // Case-insensitive message check
    if let Some(msg) = error
        .and_then(|error| error.get("message"))
        .and_then(|value| value.as_str())
    {
        let lower = msg.to_lowercase();
        if lower.contains("previous response") && lower.contains("not found") {
            return true;
        }
    }
    false
}

pub(super) fn event_error_status(payload: &serde_json::Value) -> Option<u16> {
    super::events::classify_event_failure(payload).and_then(|failure| failure.explicit_status)
}

#[allow(dead_code)]
fn extract_retry_after(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|e| e.get("retry_after"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Main request function
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_request(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<&TrafficCapture>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    body_limit: usize,
    reservation: Option<&ContinuationReservation>,
    on_connect_failure: OnConnectFailure<'_>,
) -> Result<OwnerAwareCodexResponse, CodexError> {
    let continuation = reservation.map(ContinuationReservation::candidate);
    let pool_owner = reservation_pool_owner(reservation);
    let ws_url = to_websocket_url(url).map_err(|e| CodexError {
        status: 0,
        message: e.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let body_json = serde_json::to_string(body_value).unwrap_or_default();
    if let Some(tc) = traffic {
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "url": ws_url,
                "method": "GET",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
                "continuation": {
                    "previousResponseId": continuation
                        .and_then(|c| c.previous_response_id.as_deref()),
                    "inputDeltaCount": continuation
                        .and_then(|c| c.input_delta.as_ref())
                        .map(|items| items.len()),
                    "disabledReason": continuation
                        .and_then(|c| c.disabled_reason.as_deref()),
                },
            }),
        );
    }
    let started_at = Instant::now();

    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let pooled = take_pool_entry_for_request(reservation, headers)?;
    let mut used_pooled = pooled.is_some();
    let mut entry = if let Some(entry) = pooled {
        entry
    } else {
        Arc::new(PoolEntry::opened_with(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
                on_connect_failure,
            )
            .await?,
            headers,
        ))
    };
    let mut guard = entry.ws.clone().lock_owned().await;

    if used_pooled
        && validate_pooled_websocket(&mut guard, connect_timeout_ms)
            .await
            .is_err()
    {
        drop(guard);
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        if requires_origin {
            return Err(continuation_socket_missing_error());
        }
        entry = Arc::new(PoolEntry::opened_with(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
                on_connect_failure,
            )
            .await?,
            headers,
        ));
        guard = entry.ws.clone().lock_owned().await;
        used_pooled = false;
    }
    if !used_pooled {
        log_advertised_message_limit(ctx, &entry);
    }

    if let Err(error) = entry.check_message_size(body_json.len()) {
        // The socket goes as on a failed write; the conversation's next
        // request opens a new one.
        drop(guard);
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(error);
    }

    send_request_frame(&mut guard, body_json, idle_timeout_ms)
        .await
        .inspect_err(|_| {
            if let Some(owner) = pool_owner {
                pool_remove_entry(owner, &entry);
            }
        })?;

    let collected = collect_ws_events(
        &mut guard,
        idle_timeout_ms,
        body_limit,
        pool_owner,
        Some(&entry),
        traffic,
        &ctx.req_id,
    )
    .await;
    drop(guard);
    let (sse_body, terminal_event) = match collected {
        Ok(result) => result,
        Err(error) => {
            if let Some(owner) = pool_owner {
                pool_remove_entry(owner, &entry);
            }
            return Err(error);
        }
    };
    let Some(terminal_event) = terminal_event else {
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(missing_terminal_error());
    };

    if is_previous_response_missing(&terminal_event.payload) {
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(CodexError {
            status: 0,
            message: "Previous response not found".to_string(),
            detail: Some("previous_response_not_found".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        });
    }

    let completed = terminal_event.event_type == "response.completed";
    let origin_reinserted = if completed {
        reservation.is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
    } else {
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        false
    };
    let status = buffered_answer_status(&terminal_event);

    if let Some(tc) = traffic {
        write_websocket_metadata_capture(tc, &ws_url, reservation, used_pooled);
        write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
    }

    Ok(OwnerAwareCodexResponse::new(
        CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
            transport: ActualTransport::WebSocket,
        },
        origin_reinserted.then_some(entry.socket_id),
    ))
}

async fn validate_pooled_websocket<S>(
    websocket: &mut WebSocketStream<S>,
    timeout_ms: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let nonce = next_monotonic_nonzero(&POOLED_VALIDATION_SEQUENCE, "pooled validation")
        .to_be_bytes()
        .to_vec();
    // The Ping is written under the validation deadline too, so a socket the
    // peer stopped reading fails validation instead of holding the request.
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        websocket
            .send(Message::Ping(nonce.clone()))
            .await
            .map_err(|error| error.to_string())?;
        loop {
            match websocket.next().await {
                Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                Some(Ok(Message::Ping(payload))) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| error.to_string())?,
                Some(Ok(Message::Pong(_))) => {
                    return Err("unexpected Pong during pooled validation".to_string());
                }
                Some(Ok(_)) => {
                    return Err("unexpected frame during pooled validation".to_string());
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("connection closed during pooled validation".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "validation timeout".to_string())?
}

pub(super) struct ReadyWebSocket {
    ws_url: String,
    guard: OwnedMutexGuard<CodexWebSocketStream>,
    entry: Arc<PoolEntry>,
    used_pooled: bool,
    reservation: Option<ContinuationReservation>,
    traffic: Option<Arc<TrafficCapture>>,
    idle_timeout_ms: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_codex_websocket(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    traffic: Option<Arc<TrafficCapture>>,
    reservation: Option<&ContinuationReservation>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    on_connect_failure: OnConnectFailure<'_>,
) -> Result<ReadyWebSocket, CodexError> {
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    let ws_url = to_websocket_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let pooled = take_pool_entry_for_request(reservation, headers)?;
    let used_pooled = pooled.is_some();
    let entry = if let Some(entry) = pooled {
        entry
    } else {
        let connected = connect_with_timeout(
            websocket_client,
            proxy_config,
            &ws_url,
            headers,
            connect_timeout_ms,
            on_connect_failure,
        )
        .await?;
        Arc::new(PoolEntry::opened_with(connected, headers))
    };
    let mut guard = entry.ws.clone().lock_owned().await;
    if used_pooled
        && let Err(detail) = validate_pooled_websocket(&mut guard, connect_timeout_ms).await
    {
        drop(guard);
        if let Some(owner) = pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(if requires_origin {
            continuation_socket_missing_error()
        } else {
            pooled_validation_error(detail)
        });
    }
    Ok(ReadyWebSocket {
        ws_url,
        guard,
        entry,
        used_pooled,
        reservation: reservation.cloned(),
        traffic,
        idle_timeout_ms,
    })
}

fn pooled_validation_error(detail: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection validation failed: {detail}"),
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn start_codex_websocket_events(
    ready: ReadyWebSocket,
    body_value: &serde_json::Value,
    body_json: String,
    headers: &HeaderMap,
    reservation: Option<&ContinuationReservation>,
    req_id: String,
    capture_limit: usize,
) -> CodexWebSocketEventStream {
    if let Some(tc) = ready.traffic.as_deref() {
        write_websocket_metadata_capture(tc, &ready.ws_url, reservation, ready.used_pooled);
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
            }),
        );
    }
    let (tx, rx) = mpsc::channel(64);
    let (receiver, socket_id_publisher) = CodexWebSocketEventStream::pending(rx);
    tokio::spawn(async move {
        let ReadyWebSocket {
            ws_url: _,
            mut guard,
            entry,
            used_pooled: _,
            reservation,
            traffic,
            idle_timeout_ms,
        } = ready;
        let pool_owner = reservation_pool_owner(reservation.as_ref());
        if let Err(error) = entry.check_message_size(body_json.len()) {
            // The socket goes as on a failed write.
            drop(guard);
            if let Some(owner) = pool_owner {
                pool_remove_entry(owner, &entry);
            }
            socket_id_publisher.publish(None);
            let _ = tx.send(Err(error)).await;
            return;
        }
        if let Err(error) = send_request_frame(&mut guard, body_json, idle_timeout_ms).await {
            drop(guard);
            if let Some(owner) = pool_owner {
                pool_remove_entry(owner, &entry);
            }
            socket_id_publisher.publish(None);
            let _ = tx.send(Err(error)).await;
            return;
        }
        let (reusable, terminal_item) = stream_ws_events(
            &mut guard,
            idle_timeout_ms,
            traffic,
            &tx,
            &req_id,
            capture_limit,
        )
        .await;
        drop(guard);

        let origin_reinserted = if reusable {
            reservation
                .as_ref()
                .is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
        } else {
            if let Some(owner) = pool_owner {
                pool_remove_entry(owner, &entry);
            }
            false
        };
        socket_id_publisher.publish(origin_reinserted.then_some(entry.socket_id));
        if let Some(item) = terminal_item
            && tx.send(item).await.is_err()
            && origin_reinserted
            && let Some(owner) = pool_owner
        {
            pool_remove_entry(owner, &entry);
        }
    });
    receiver
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_event_stream(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<Arc<TrafficCapture>>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    capture_limit: usize,
    reservation: Option<&ContinuationReservation>,
    on_connect_failure: OnConnectFailure<'_>,
) -> Result<CodexWebSocketEventStream, CodexError> {
    let body_json = serde_json::to_string(body_value).map_err(|error| CodexError {
        status: 500,
        message: "Failed to serialize WebSocket request".to_string(),
        detail: Some(error.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let ready = prepare_codex_websocket(
        websocket_client,
        proxy_config,
        url,
        headers,
        traffic,
        reservation,
        connect_timeout_ms,
        idle_timeout_ms,
        on_connect_failure,
    )
    .await?;
    if !ready.used_pooled {
        log_advertised_message_limit(ctx, &ready.entry);
    }
    Ok(start_codex_websocket_events(
        ready,
        body_value,
        body_json,
        headers,
        reservation,
        ctx.req_id.clone(),
        capture_limit,
    ))
}

fn continuation_socket_missing_error() -> CodexError {
    CodexError {
        status: 0,
        message: "Previous response socket is no longer available".to_string(),
        detail: Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn missing_terminal_error() -> CodexError {
    CodexError {
        status: 0,
        message: "WebSocket connection closed before terminal Codex response event".to_string(),
        detail: Some(WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn response_start_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket response start timeout after {timeout_ms}ms"),
        detail: Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

/// The error a response wait answers when its idle deadline passes.
fn response_wait_timeout_error(response_started: bool, idle_timeout_ms: u64) -> CodexError {
    if response_started {
        CodexError {
            status: 0,
            message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        }
    } else {
        response_start_timeout_error(idle_timeout_ms)
    }
}

/// The error a write answers when it runs out of time: the client sees the
/// response wait's timeout, while the detail keeps the request out of the
/// continuation retry and the HTTP fallback.
fn write_timeout_error(response_started: bool, timeout_ms: u64) -> CodexError {
    CodexError {
        detail: Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL.to_string()),
        ..response_wait_timeout_error(response_started, timeout_ms)
    }
}

/// Writes the request frame under the idle budget of the response wait that
/// follows it, as the Codex CLI's `send_websocket_request` does
/// (`codex-rs/codex-api/src/endpoint/responses_websocket.rs` at
/// rust-v0.166.0). A peer that stops reading gets the answer of a peer that
/// stays silent before the first response event, the response start timeout,
/// and the request is neither resent nor replayed over HTTP.
async fn send_request_frame<S>(
    websocket: &mut WebSocketStream<S>,
    body_json: String,
    idle_timeout_ms: u64,
) -> Result<(), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let budget_ms = match REQUEST_WRITE_TIMEOUT_MS_FOR_TESTS.load(Ordering::Relaxed) {
        0 => idle_timeout_ms,
        override_ms => override_ms,
    };
    match tokio::time::timeout(
        Duration::from_millis(budget_ms),
        websocket.send(Message::Text(body_json)),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(CodexError {
            status: 0,
            message: format!("WebSocket send error: {error}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        }),
        Err(_) => Err(write_timeout_error(false, budget_ms)),
    }
}

fn write_websocket_metadata_capture(
    traffic: &TrafficCapture,
    ws_url: &str,
    reservation: Option<&ContinuationReservation>,
    pooled: bool,
) {
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    traffic.write_json(
        "022-upstream-websocket-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "websocket",
            "url": ws_url,
            "poolingEnabled": pool_owner.is_some(),
            "pooled": pooled,
            "continuation": {
                "previousResponseId": continuation
                    .and_then(|c| c.previous_response_id.as_deref()),
                "inputDeltaCount": continuation
                    .and_then(|c| c.input_delta.as_ref())
                    .map(|items| items.len()),
                "disabledReason": continuation
                    .and_then(|c| c.disabled_reason.as_deref()),
            },
        }),
    );
}

fn write_websocket_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    sse_body: &[u8],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": {
                "content-type": "text/event-stream",
            },
        }),
    );
    if status >= 400 {
        traffic.write_text(
            "031-upstream-error-body",
            &String::from_utf8_lossy(sse_body),
        );
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", sse_body);
    }
}

// ---------------------------------------------------------------------------
// Connection helper
// ---------------------------------------------------------------------------

/// A rejection body is kept up to `MAX_ERROR_BODY_BYTES` to read its class
/// and a spent window from; the detail keeps at most this much of it.
const MAX_HANDSHAKE_ERROR_DETAIL_BYTES: usize = 1024;
const GENERIC_HANDSHAKE_ERROR_DETAIL: &str = "WebSocket upgrade was rejected";

fn handshake_error_detail(body: Option<&[u8]>) -> String {
    body.and_then(handshake_error_message)
        .unwrap_or_else(|| GENERIC_HANDSHAKE_ERROR_DETAIL.to_string())
}

/// The message a rejection body names, without control characters and cut to
/// `MAX_HANDSHAKE_ERROR_DETAIL_BYTES`; `None` when the body is not JSON or
/// names no message.
fn handshake_error_message(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(|value| value.as_str())?;
    let sanitized: String = message
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, ' '))
        .collect();
    let mut end = sanitized.len().min(MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    Some(sanitized[..end].to_string())
}

/// The ChatGPT account an upgrade request is sent with (`build_codex_headers`).
fn request_account_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
}

/// The access token of the request's `Authorization: Bearer` header.
fn request_access_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// A rejected upgrade, built the same way on the direct and the tunneled path.
/// `body` is what was read of the rejection's body, `None` for a 407, which is
/// never read. A `Retry-After` header is normalised the way a classified one
/// is (`normalize_retry_after`), and one that is not usable counts as absent;
/// whether it reaches the client is up to the error mapping, which never
/// sends it for a spent usage window or a quota.
/// Every `codex.rate_limits` event the body holds reaches the quota gauge
/// whole, under `account`, the ChatGPT account the upgrade was sent with,
/// before the body is reduced to a spent window.
fn handshake_rejection_error(
    status: u16,
    headers: &HeaderMap,
    body: Option<&[u8]>,
    account: Option<&str>,
) -> CodexError {
    let header_retry_after = || {
        headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(super::events::normalize_retry_after)
    };
    let (detail, class, usage_limit, retry_after) = match body {
        Some(body) => {
            let header_pairs: Vec<(String, String)> = headers
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.to_string(), value.to_str().ok()?.to_string()))
                })
                .collect();
            let class = super::events::error_class_from_body(body, status);
            // Read from the body, since the detail of a body with no message
            // is the generic text, which a body may also send as its message.
            let blank_message =
                handshake_error_message(body).is_none_or(|message| message.trim().is_empty());
            let detail = match handshake_error_detail(Some(body)) {
                // A missing model with no message is named by its code, as
                // on the stream.
                detail
                    if class == Some(super::events::CodexErrorClass::ModelNotFound)
                        && (detail.trim().is_empty()
                            || detail == GENERIC_HANDSHAKE_ERROR_DETAIL) =>
                {
                    "model_not_found".to_string()
                }
                // A terminal code with a missing or blank message takes the
                // CLI's text for it, as on the stream.
                detail if blank_message => {
                    super::events::terminal_code_text_from_body(body, status)
                        .map_or(detail, str::to_string)
                }
                detail => detail,
            };
            super::events::observe_rate_limits_in_body(account, body);
            (
                detail,
                class,
                super::events::usage_limit_from_response(body, &header_pairs)
                    .map(|limit| Box::new(limit.for_account(account))),
                // The delay the body names wins over the header, as on the
                // other paths.
                super::events::retry_after_from_body(body).or_else(header_retry_after),
            )
        }
        None => (
            GENERIC_HANDSHAKE_ERROR_DETAIL.to_string(),
            None,
            None,
            header_retry_after(),
        ),
    };
    CodexError {
        status,
        message: format!("WebSocket upgrade rejected with status {status}"),
        detail: Some(detail),
        retry_after,
        usage_limit,
        class,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn header_has_token(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(expected))
        })
    })
}

fn requested_subprotocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn websocket_protocol_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn validate_websocket_upgrade(
    version: http::Version,
    headers: &HeaderMap,
    websocket_key: &str,
    requested_subprotocols: &[String],
) -> Result<(), CodexError> {
    if version != http::Version::HTTP_11 {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response did not use HTTP/1.1",
        ));
    }
    if !header_has_token(headers, http::header::UPGRADE.as_str(), "websocket") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Upgrade: websocket",
        ));
    }
    if !header_has_token(headers, http::header::CONNECTION.as_str(), "upgrade") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Connection: Upgrade",
        ));
    }

    let expected_accept = derive_accept_key(websocket_key.as_bytes());
    let mut accept_values = headers.get_all(http::header::SEC_WEBSOCKET_ACCEPT).iter();
    let accept = accept_values.next().and_then(|value| value.to_str().ok());
    if accept_values.next().is_some() || accept != Some(expected_accept.as_str()) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response has an invalid Sec-WebSocket-Accept",
        ));
    }
    if headers.contains_key(http::header::SEC_WEBSOCKET_EXTENSIONS) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response selected an unsolicited extension",
        ));
    }

    let mut response_protocols = headers.get_all(http::header::SEC_WEBSOCKET_PROTOCOL).iter();
    let response_protocol = response_protocols
        .next()
        .map(|value| value.to_str().map(str::trim));
    if response_protocols.next().is_some() {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response contains multiple subprotocols",
        ));
    }
    match response_protocol {
        None if requested_subprotocols.is_empty() => {}
        None => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response omitted the requested subprotocol",
            ));
        }
        Some(Err(_)) => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response contains an invalid subprotocol",
            ));
        }
        Some(Ok(_)) if requested_subprotocols.is_empty() => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsolicited subprotocol",
            ));
        }
        Some(Ok(protocol))
            if !requested_subprotocols
                .iter()
                .any(|requested| requested == protocol) =>
        {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsupported subprotocol",
            ));
        }
        Some(Ok(_)) => {}
    }

    Ok(())
}

async fn bounded_handshake_error_body(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < MAX_ERROR_BODY_BYTES {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let remaining = MAX_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

fn error_chain_contains(error: &(dyn std::error::Error + 'static), expected: &str) -> bool {
    let expected = expected.to_ascii_lowercase();
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().to_ascii_lowercase().contains(&expected) {
            return true;
        }
        current = error.source();
    }
    false
}

fn reqwest_handshake_error(error: reqwest::Error) -> ConnectAttemptError {
    let proxy_auth_required =
        error.is_connect() && error_chain_contains(&error, "proxy authorization required");
    let proxy_tunnel_rejected =
        error.is_connect() && error_chain_contains(&error, "tunnel error: unsuccessful");
    let unreachable = (error.is_connect() || error.is_timeout())
        && !proxy_auth_required
        && !proxy_tunnel_rejected;
    let status = if proxy_auth_required {
        http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
    } else {
        error.status().map(|status| status.as_u16()).unwrap_or(0)
    };
    let message = if proxy_auth_required {
        "WebSocket proxy authentication failed"
    } else if proxy_tunnel_rejected {
        "WebSocket proxy tunnel was rejected"
    } else if error.is_timeout() {
        "WebSocket upgrade request timed out"
    } else if error.is_connect() {
        "WebSocket connection failed"
    } else {
        "WebSocket upgrade request failed"
    };
    let error = CodexError {
        status,
        message: message.to_string(),
        detail: proxy_tunnel_rejected.then(|| WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    };
    if unreachable {
        ConnectAttemptError::Unreachable(error)
    } else {
        ConnectAttemptError::Origin(error)
    }
}

struct PrefixedIo {
    prefix: Vec<u8>,
    position: usize,
    inner: BoxedWebSocketIo,
}

impl AsyncRead for PrefixedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let available = &this.prefix[this.position..];
            let len = available.len().min(buffer.remaining());
            buffer.put_slice(&available[..len]);
            this.position += len;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for PrefixedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn skip_websocket_request_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "host"
            | "content-length"
            | "proxy-authorization"
    )
}

fn tunneled_websocket_request(
    url: &str,
    headers: &HeaderMap,
    websocket_key: &str,
) -> Result<http::Request<()>, CodexError> {
    let mut request = url
        .into_client_request()
        .map_err(|_| websocket_protocol_error("WebSocket request URL was invalid"))?;
    *request.version_mut() = http::Version::HTTP_11;
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_KEY,
        http::HeaderValue::from_str(websocket_key)
            .map_err(|_| websocket_protocol_error("WebSocket key was invalid"))?,
    );
    for (name, value) in headers {
        if !skip_websocket_request_header(name) {
            request.headers_mut().append(name.clone(), value.clone());
        }
    }
    Ok(request)
}

fn tunnel_error(status: u16, retry_after: Option<String>) -> CodexError {
    let proxy_auth_required = status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16();
    CodexError {
        status,
        message: if proxy_auth_required {
            "WebSocket proxy authentication failed".to_string()
        } else {
            "WebSocket proxy tunnel was rejected".to_string()
        },
        detail: if proxy_auth_required {
            Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string())
        } else {
            Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string())
        },
        retry_after,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn invalid_tunnel_response(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn connect_response_header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn parse_connect_response_head(response: &[u8]) -> Result<(u16, Option<String>), CodexError> {
    let status_line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let status_line = std::str::from_utf8(&response[..status_line_end])
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_tunnel_response(
            "WebSocket proxy returned an invalid response",
        ));
    }
    let status = status
        .parse::<u16>()
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let retry_after = response[status_line_end + 2..]
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let separator = line.iter().position(|byte| *byte == b':')?;
            let (name, value) = line.split_at(separator);
            if !name.eq_ignore_ascii_case(b"retry-after") {
                return None;
            }
            std::str::from_utf8(&value[1..])
                .ok()
                .and_then(super::events::normalize_retry_after)
        });
    Ok((status, retry_after))
}

/// `response_head` is set once the proxy's status line is complete and
/// cleared again when the tunnel opens, as the destination has not answered
/// yet; it counts as answered once its handshake response is complete.
async fn establish_connect_tunnel(
    mut stream: BoxedWebSocketIo,
    authority: &str,
    basic_auth: Option<&http::HeaderValue>,
    response_head: &AtomicBool,
) -> Result<BoxedWebSocketIo, CodexError> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    if let Some(auth) = basic_auth {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(auth.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    stream.write_all(&request).await.map_err(|_| CodexError {
        status: 0,
        message: "WebSocket proxy tunnel request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;

    let mut response = Vec::new();
    loop {
        if response.starts_with(b"HTTP/") && response.windows(2).any(|pair| pair == b"\r\n") {
            response_head.store(true, Ordering::Relaxed);
        }
        if let Some(header_end) = connect_response_header_end(&response) {
            let (status, retry_after) = parse_connect_response_head(&response[..header_end])?;
            if (100..200).contains(&status) {
                response.drain(..header_end);
                continue;
            }
            if (200..300).contains(&status) {
                response_head.store(false, Ordering::Relaxed);
                let prefix = response.split_off(header_end);
                return Ok(Box::new(PrefixedIo {
                    prefix,
                    position: 0,
                    inner: stream,
                }));
            }
            return Err(tunnel_error(status, retry_after));
        }
        if response.len() == MAX_CONNECT_RESPONSE_HEADER_BYTES {
            return Err(invalid_tunnel_response(
                "WebSocket proxy response headers were too large",
            ));
        }
        let remaining = MAX_CONNECT_RESPONSE_HEADER_BYTES - response.len();
        let mut buffer = [0_u8; 1024];
        let capacity = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..capacity])
            .await
            .map_err(|_| invalid_tunnel_response("WebSocket proxy response could not be read"))?;
        if read == 0 {
            return Err(invalid_tunnel_response(
                "WebSocket proxy closed the tunnel response early",
            ));
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

/// A host name rustls does not accept fails as `invalid_host` builds it, the
/// handshake itself as unreachable.
async fn tls_connect(
    stream: BoxedWebSocketIo,
    host: &str,
    tls_config: Arc<rustls::ClientConfig>,
    peer: &str,
    invalid_host: fn(CodexError) -> ConnectAttemptError,
) -> Result<BoxedWebSocketIo, ConnectAttemptError> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        invalid_host(websocket_protocol_error(
            "WebSocket TLS host name was invalid",
        ))
    })?;
    let stream = tokio_rustls::TlsConnector::from(tls_config)
        .connect(server_name, stream)
        .await
        .map_err(|_| {
            ConnectAttemptError::Unreachable(CodexError {
                status: 0,
                message: format!("WebSocket TLS connection to {peer} failed"),
                detail: None,
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::WebSocketHandshake,
            })
        })?;
    Ok(Box::new(stream))
}

async fn connect_to_http_proxy(
    route: &WebSocketProxyRoute,
    tls_config: Arc<rustls::ClientConfig>,
) -> Result<BoxedWebSocketIo, ConnectAttemptError> {
    let scheme = route.uri.scheme_str().unwrap_or_default();
    let host = route.uri.host().ok_or_else(|| {
        ConnectAttemptError::ProxyTunnel(websocket_protocol_error(
            "WebSocket proxy URL did not contain a host",
        ))
    })?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = route
        .uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let stream = TcpStream::connect((host, port)).await.map_err(|_| {
        ConnectAttemptError::Unreachable(CodexError {
            status: 0,
            message: "WebSocket proxy connection failed".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })
    })?;
    let stream: BoxedWebSocketIo = Box::new(stream);
    if scheme == "https" {
        tls_connect(
            stream,
            host,
            tls_config,
            "proxy",
            ConnectAttemptError::ProxyTunnel,
        )
        .await
    } else {
        Ok(stream)
    }
}

fn websocket_destination(url: &str) -> Result<(String, String), CodexError> {
    let destination = url::Url::parse(url)
        .map_err(|_| websocket_protocol_error("WebSocket destination URL was invalid"))?;
    let host = destination
        .host_str()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no host"))?;
    let port = destination
        .port_or_known_default()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no usable port"))?;
    let authority = match destination.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]:{port}"),
        Some(_) => format!("{host}:{port}"),
        None => {
            return Err(websocket_protocol_error(
                "WebSocket destination URL had no host",
            ));
        }
    };
    Ok((host.to_string(), authority))
}

/// A rejected upgrade on the tunneled path. The rejection carries whatever
/// body bytes arrived with its head, which may be all of a short body, part of
/// it or nothing; they are read the way the direct path reads its body, and a
/// 407 is not read at all.
fn tungstenite_handshake_error(
    error: tokio_tungstenite::tungstenite::Error,
    account: Option<&str>,
) -> CodexError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error {
        let body = response
            .body()
            .as_deref()
            .filter(|_| response.status() != http::StatusCode::PROXY_AUTHENTICATION_REQUIRED)
            .map(|body| &body[..body.len().min(MAX_ERROR_BODY_BYTES)]);
        return handshake_rejection_error(
            response.status().as_u16(),
            response.headers(),
            body,
            account,
        );
    }
    CodexError {
        status: 0,
        message: "WebSocket upgrade request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

enum ConnectAttemptError {
    Origin(CodexError),
    ProxyTunnel(CodexError),
    /// Nothing answered before the connect timeout (the upgrade's response
    /// head, a proxy's status line or the tunneled destination's handshake
    /// response was not complete), or the connection or the TLS handshake
    /// failed. Only this starts a connect pause.
    Unreachable(CodexError),
    /// The target was paused when the attempt was about to start, so nothing
    /// was sent.
    Paused,
}

impl ConnectAttemptError {
    fn is_origin_forbidden(&self) -> bool {
        matches!(
            self,
            Self::Origin(error) if error.status == http::StatusCode::FORBIDDEN.as_u16()
        )
    }

    fn into_error(self) -> CodexError {
        match self {
            Self::Origin(error) | Self::ProxyTunnel(error) | Self::Unreachable(error) => error,
            Self::Paused => connect_paused_error(),
        }
    }
}

async fn connect_via_http_proxy_tunnel(
    proxy_config: &WebSocketProxyConfig,
    route: WebSocketProxyRoute,
    url: &str,
    headers: &HeaderMap,
    response_head: &AtomicBool,
) -> Result<ConnectedWebSocket, ConnectAttemptError> {
    let (host, authority) = websocket_destination(url).map_err(ConnectAttemptError::Origin)?;
    let stream = connect_to_http_proxy(&route, proxy_config.tls_config.clone()).await?;
    let stream =
        establish_connect_tunnel(stream, &authority, route.basic_auth.as_ref(), response_head)
            .await
            .map_err(ConnectAttemptError::ProxyTunnel)?;
    let stream = tls_connect(
        stream,
        &host,
        proxy_config.tls_config.clone(),
        "destination",
        ConnectAttemptError::Origin,
    )
    .await?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let request = tunneled_websocket_request(url, headers, &websocket_key)
        .map_err(ConnectAttemptError::Origin)?;
    let (websocket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|error| tungstenite_handshake_error(error, request_account_id(headers)))
        .map_err(ConnectAttemptError::Origin)?;
    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )
    .map_err(ConnectAttemptError::Origin)?;
    Ok(ConnectedWebSocket {
        stream: websocket,
        max_message_bytes: advertised_max_message_bytes(response.headers()),
    })
}

async fn connect_via_http_upgrade(
    websocket_client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    response_head: &AtomicBool,
) -> Result<ConnectedWebSocket, ConnectAttemptError> {
    let http_url = to_http_upgrade_url(url).map_err(|error| {
        ConnectAttemptError::Origin(CodexError {
            status: 0,
            message: error.message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })
    })?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let mut request = websocket_client
        .get(http_url)
        .version(http::Version::HTTP_11)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::SEC_WEBSOCKET_VERSION, "13")
        .header(http::header::SEC_WEBSOCKET_KEY, &websocket_key);

    for (key, value) in headers {
        if skip_websocket_request_header(key) {
            continue;
        }
        request = request.header(key.clone(), value.clone());
    }

    let response = request.send().await.map_err(reqwest_handshake_error)?;
    response_head.store(true, Ordering::Relaxed);
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        let body = if status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16() {
            None
        } else {
            Some(bounded_handshake_error_body(response).await)
        };
        return Err(ConnectAttemptError::Origin(handshake_rejection_error(
            status,
            &response_headers,
            body.as_deref(),
            request_account_id(headers),
        )));
    }

    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )
    .map_err(ConnectAttemptError::Origin)?;
    let max_message_bytes = advertised_max_message_bytes(response.headers());
    let upgraded = response.upgrade().await.map_err(|_| {
        ConnectAttemptError::Origin(websocket_protocol_error(
            "WebSocket upgrade stream was not available",
        ))
    })?;
    let upgraded: BoxedWebSocketIo = Box::new(upgraded);
    Ok(ConnectedWebSocket {
        stream: WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await,
        max_message_bytes,
    })
}

async fn connect_once(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    response_head: &AtomicBool,
) -> Result<ConnectedWebSocket, ConnectAttemptError> {
    if let Some(route) = proxy_config
        .http_connect_route(url)
        .map_err(ConnectAttemptError::Origin)?
    {
        connect_via_http_proxy_tunnel(proxy_config, route, url, headers, response_head).await
    } else {
        connect_via_http_upgrade(websocket_client, url, headers, response_head).await
    }
}

fn connect_timeout_error(connect_timeout: Duration) -> CodexError {
    websocket_protocol_error(&format!(
        "WebSocket connect timeout after {}ms",
        connect_timeout.as_millis()
    ))
}

/// `connect` sets `response_head` once the upgrade's response head or the
/// proxy's status line is complete. An attempt that runs out of time after
/// that has been answered and is not `Unreachable`.
async fn connect_with_policy<T, P, PFut, F, Fut>(
    gate: &WebSocketConnectGate,
    connect_timeout: Duration,
    forbidden_cooldown: Duration,
    response_head: &AtomicBool,
    mut before_start: P,
    mut connect: F,
) -> Result<T, ConnectAttemptError>
where
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = Result<(), ConnectAttemptError>>,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectAttemptError>>,
{
    for attempt in 0..=1 {
        gate.wait_to_start(before_start()).await?;
        response_head.store(false, Ordering::Relaxed);
        let result = tokio::time::timeout(connect_timeout, connect())
            .await
            .unwrap_or_else(|_| {
                let error = connect_timeout_error(connect_timeout);
                Err(if response_head.load(Ordering::Relaxed) {
                    ConnectAttemptError::Origin(error)
                } else {
                    ConnectAttemptError::Unreachable(error)
                })
            });
        if result
            .as_ref()
            .is_err_and(ConnectAttemptError::is_origin_forbidden)
            && attempt == 0
        {
            retry_sleep(u64::try_from(forbidden_cooldown.as_millis()).unwrap_or(u64::MAX)).await;
            continue;
        }
        return result;
    }
    unreachable!()
}

/// What a new connection that fails does to later ones.
#[derive(Clone, Copy)]
pub(super) enum OnConnectFailure<'a> {
    /// The failure goes to the client: every request tries to connect.
    Fail,
    /// The request falls back to HTTP: a connect that got no answer pauses
    /// new connections to its target for `WEBSOCKET_CONNECT_PAUSE`, and while
    /// that runs none is attempted.
    FallBackToHttp { req_id: &'a str },
}

/// One pause per connect target: the scheme, host and port of the WebSocket
/// URL.
fn connect_pause_key(url: &str) -> String {
    let websocket_url = to_websocket_url(url).unwrap_or_else(|_| url.to_string());
    url::Url::parse(&websocket_url)
        .map(|parsed| parsed.origin().ascii_serialization())
        .unwrap_or(websocket_url)
}

fn lock_connect_pauses() -> std::sync::MutexGuard<'static, HashMap<String, Instant>> {
    WS_CONNECT_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn connect_paused(url: &str) -> bool {
    let key = connect_pause_key(url);
    let now = Instant::now();
    let mut pauses = lock_connect_pauses();
    pauses.retain(|_, ends_at| *ends_at > now);
    pauses.contains_key(&key)
}

/// A target whose pause still runs keeps it: its end does not move and
/// nothing is logged.
fn start_connect_pause(url: &str, req_id: &str, error: &CodexError) {
    let now = Instant::now();
    {
        let mut pauses = lock_connect_pauses();
        pauses.retain(|_, ends_at| *ends_at > now);
        let key = connect_pause_key(url);
        if pauses.contains_key(&key) {
            return;
        }
        pauses.insert(key, now + WEBSOCKET_CONNECT_PAUSE);
    }
    let fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        (
            "seconds".to_string(),
            serde_json::json!(WEBSOCKET_CONNECT_PAUSE.as_secs()),
        ),
        (
            "detail".to_string(),
            serde_json::json!(super::client::websocket_fallback_log_detail(error)),
        ),
    ]);
    create_logger("codex").warn("codex_websocket_connect_paused", Some(fields));
}

fn end_connect_pause(url: &str) {
    lock_connect_pauses().remove(&connect_pause_key(url));
}

fn connect_paused_error() -> CodexError {
    CodexError {
        status: 0,
        message: "WebSocket connect skipped while its target is paused".to_string(),
        detail: Some(WEBSOCKET_CONNECT_PAUSED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

async fn connect_with_timeout(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    connect_timeout_ms: u64,
    on_failure: OnConnectFailure<'_>,
) -> Result<ConnectedWebSocket, CodexError> {
    let falls_back = matches!(on_failure, OnConnectFailure::FallBackToHttp { .. });
    if falls_back && connect_paused(url) {
        return Err(connect_paused_error());
    }
    let response_head = AtomicBool::new(false);
    #[cfg(test)]
    let test_gate = TEST_CONNECT_GATE.try_with(Arc::clone).ok();
    #[cfg(test)]
    let gate: &WebSocketConnectGate = test_gate.as_deref().unwrap_or(&*WS_CONNECT_GATE);
    #[cfg(not(test))]
    let gate: &WebSocketConnectGate = &WS_CONNECT_GATE;
    connect_with_policy(
        gate,
        Duration::from_millis(connect_timeout_ms),
        WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN,
        &response_head,
        // The pause may have started while this request waited at the gate.
        move || async move {
            if falls_back && connect_paused(url) {
                return Err(ConnectAttemptError::Paused);
            }
            cleanup_pool_before_connect();
            Ok(())
        },
        || connect_once(websocket_client, proxy_config, url, headers, &response_head),
    )
    .await
    .inspect(|_| end_connect_pause(url))
    .map_err(|error| {
        if let (
            ConnectAttemptError::Unreachable(error),
            OnConnectFailure::FallBackToHttp { req_id },
        ) = (&error, on_failure)
        {
            start_connect_pause(url, req_id, error);
        }
        error.into_error()
    })
}

// ---------------------------------------------------------------------------
// Event collection
// ---------------------------------------------------------------------------

enum WebSocketRead {
    Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Timeout,
    KeepaliveError(String),
}

async fn read_ws_frame_with_keepalive<S>(
    ws: &mut WebSocketStream<S>,
    read_timeout: Duration,
    keepalive_interval: Duration,
) -> WebSocketRead
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = tokio::time::sleep(read_timeout);
    tokio::pin!(timeout);
    let mut keepalive = tokio::time::interval(keepalive_interval);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;

    loop {
        tokio::select! {
            biased;
            frame = ws.next() => return WebSocketRead::Frame(frame),
            _ = &mut timeout => return WebSocketRead::Timeout,
            _ = keepalive.tick() => {
                let send = ws.send(Message::Ping(Vec::new()));
                let send_timeout = tokio::time::sleep(WEBSOCKET_KEEPALIVE_SEND_TIMEOUT);
                tokio::pin!(send_timeout);
                tokio::select! {
                    biased;
                    result = send => {
                        if let Err(error) = result {
                            return WebSocketRead::KeepaliveError(error.to_string());
                        }
                    }
                    _ = &mut timeout => return WebSocketRead::Timeout,
                    _ = &mut send_timeout => {
                        return WebSocketRead::KeepaliveError(format!(
                            "send timed out after {}ms",
                            WEBSOCKET_KEEPALIVE_SEND_TIMEOUT.as_millis(),
                        ));
                    }
                }
            }
        }
    }
}

struct WsEvent {
    event_type: String,
    payload: serde_json::Value,
}

/// The status a buffered WebSocket answer carries: an `error` event's own,
/// else 500, and 200 for any other terminal event.
fn buffered_answer_status(terminal_event: &WsEvent) -> u16 {
    if terminal_event.event_type == "error" {
        event_error_status(&terminal_event.payload).unwrap_or(500)
    } else {
        200
    }
}

/// The event that took a buffered answer past its limit, when the collector
/// would stop on it as a failure anyway: an `error` or `response.failed`
/// event, or a `codex.rate_limits` snapshot that ends the request, other than
/// a missing previous response, which is sent again. Only the `type` is read
/// from any other event, so no copy of it is built.
fn failure_past_limit(text: &str) -> Option<WsEvent> {
    #[derive(serde::Deserialize)]
    struct EventType<'a> {
        #[serde(borrow, rename = "type")]
        event_type: Option<std::borrow::Cow<'a, str>>,
    }

    let event_type = serde_json::from_str::<EventType<'_>>(text)
        .ok()?
        .event_type?;
    if !matches!(
        event_type.as_ref(),
        "error" | "response.failed" | "codex.rate_limits"
    ) {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_str(text).ok()?;
    let stops_as_failure = (is_terminal_event(&payload)
        || super::events::is_terminal_rate_limit_event(&payload))
        && !is_previous_response_missing(&payload);
    stops_as_failure.then(|| WsEvent {
        event_type: event_type.into_owned(),
        payload,
    })
}

async fn collect_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    body_limit: usize,
    pool_owner: Option<&ConversationIdentity>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
    req_id: &str,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    collect_ws_events_with_keepalive_interval(
        ws,
        idle_timeout_ms,
        body_limit,
        pool_owner,
        pool_entry,
        traffic,
        req_id,
        WEBSOCKET_KEEPALIVE_INTERVAL,
    )
    .await
}

/// `body_limit` bounds the SSE body held for the answer; past it the request
/// fails and the socket is not kept. The one exception is an event that
/// crosses the limit and that the request would stop on as a refusal
/// (`failure_past_limit`, `capped_body_names_refusal`): it is added to the
/// body and ends the collection as its terminal event, and the socket is
/// still not kept.
#[allow(clippy::too_many_arguments)]
async fn collect_ws_events_with_keepalive_interval<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    body_limit: usize,
    pool_owner: Option<&ConversationIdentity>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
    req_id: &str,
    keepalive_interval: Duration,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut sse_body: Vec<u8> = Vec::new();
    let mut terminal_event: Option<WsEvent> = None;
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = tokio::time::Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;
    let mut text_received = false;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout =
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    return Err(response_wait_timeout_error(
                        response_started,
                        idle_timeout_ms,
                    ));
                }
            };

        let frame = match read_ws_frame_with_keepalive(ws, read_timeout, keepalive_interval).await {
            WebSocketRead::Frame(frame) => frame,
            WebSocketRead::Timeout => {
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(response_wait_timeout_error(
                    response_started,
                    idle_timeout_ms,
                ));
            }
            WebSocketRead::KeepaliveError(error) => {
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: format!("WebSocket keepalive error: {error}"),
                    detail: Some(WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                text_received = true;
                if sse_body.len().saturating_add(encoded_sse_len(&text)) > body_limit {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    // A refusal the request stops on is still answered as
                    // one, read from the events held and this one, which is
                    // added in its compact form, a single line.
                    if let Some(failure) = failure_past_limit(&text) {
                        sse_body.extend_from_slice(&encode_sse(&failure.payload.to_string()));
                        if let Some(tc) = traffic {
                            tc.write_json_event("040-upstream-event", &failure.payload);
                        }
                        let status = buffered_answer_status(&failure);
                        if super::client::capped_body_names_refusal(&sse_body, status, &[]) {
                            terminal_event = Some(failure);
                            break;
                        }
                    }
                    return Err(buffered_response_too_large_error(
                        CodexErrorOrigin::WebSocket,
                    ));
                }
                let event = encode_sse(&text);
                // Parse JSON
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(tc) = traffic {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        // Write invalid JSON as-is
                        sse_body.extend_from_slice(&event);
                        continue;
                    }
                };

                // Convert to SSE bytes
                sse_body.extend_from_slice(&event);
                if let Some(tc) = traffic {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = tokio::time::Instant::now();
                }

                // Check for terminal events. A `codex.rate_limits` event that
                // ends the request is one too, so the spent window is answered.
                if is_terminal_event(&parsed)
                    || super::events::is_terminal_rate_limit_event(&parsed)
                {
                    terminal_event = Some(WsEvent {
                        event_type: parsed
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        payload: parsed,
                    });
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                // Reject binary frames
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            Some(Ok(Message::Ping(data))) => {
                // Respond to ping automatically, continue. The reply shares
                // the read deadline, so a peer that stops reading cannot hold
                // the wait past it.
                let remaining =
                    response_event_budget.saturating_sub(response_deadline_started.elapsed());
                if tokio::time::timeout(remaining, ws.send(Message::Pong(data)))
                    .await
                    .is_err()
                {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    return Err(write_timeout_error(response_started, idle_timeout_ms));
                }
                continue;
            }
            Some(Ok(Message::Pong(_))) => {
                continue;
            }
            Some(Ok(Message::Frame(_))) => {
                // Raw frame passthrough - continue
                continue;
            }
            Some(Ok(Message::Close(frame))) => {
                // Connection closed - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                log_response_close(req_id, close_code(frame.as_ref()), text_received);
                if closed_for_message_size(frame.as_ref(), text_received) {
                    return Err(closed_message_too_large_error());
                }
                break;
            }
            Some(Err(e)) => {
                // Stream error - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: format!("WebSocket stream error: {e}"),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            None => {
                // Stream ended - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                break;
            }
        }
    }

    Ok((sse_body, terminal_event))
}

/// The copy of a live stream's events that its traffic capture writes as the
/// response body. It exists only while a capture is on, and holds at most
/// `limit` bytes: the first event that would take it past the limit, and
/// every event after it, stay out of it. The client gets them all the same.
struct LiveCaptureBody {
    bytes: Vec<u8>,
    limit: usize,
    full: bool,
}

impl LiveCaptureBody {
    fn for_capture(traffic: Option<&TrafficCapture>, limit: usize) -> Option<Self> {
        traffic.map(|_| Self {
            bytes: Vec::new(),
            limit,
            full: false,
        })
    }

    fn append(&mut self, text: &str) {
        if self.full {
            return;
        }
        if self.bytes.len().saturating_add(encoded_sse_len(text)) > self.limit {
            self.full = true;
            return;
        }
        self.bytes.extend_from_slice(&encode_sse(text));
    }
}

async fn stream_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    traffic: Option<Arc<TrafficCapture>>,
    tx: &mpsc::Sender<Result<serde_json::Value, CodexError>>,
    req_id: &str,
    capture_limit: usize,
) -> (bool, Option<Result<serde_json::Value, CodexError>>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started_at = Instant::now();
    let mut capture_body = LiveCaptureBody::for_capture(traffic.as_deref(), capture_limit);
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = tokio::time::Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;
    let mut text_received = false;
    let mut status = 200u16;
    let mut reusable = false;
    let mut terminal_item = None;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout =
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    terminal_item = Some(Err(response_wait_timeout_error(
                        response_started,
                        idle_timeout_ms,
                    )));
                    break;
                }
            };

        let frame = tokio::select! {
            biased;
            _ = tx.closed() => break,
            frame = read_ws_frame_with_keepalive(
                ws,
                read_timeout,
                WEBSOCKET_KEEPALIVE_INTERVAL,
            ) => match frame {
                WebSocketRead::Frame(frame) => frame,
                WebSocketRead::Timeout => {
                    terminal_item = Some(Err(response_wait_timeout_error(
                        response_started,
                        idle_timeout_ms,
                    )));
                    break;
                }
                WebSocketRead::KeepaliveError(error) => {
                    terminal_item = Some(Err(CodexError {
                        status: 0,
                        message: format!("WebSocket keepalive error: {error}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }));
                    break;
                }
            },
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                text_received = true;
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(tc) = traffic.as_deref() {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        if let Some(body) = capture_body.as_mut() {
                            body.append(&text);
                        }
                        continue;
                    }
                };

                if let Some(body) = capture_body.as_mut() {
                    body.append(&text);
                }
                if let Some(tc) = traffic.as_deref() {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = tokio::time::Instant::now();
                }
                if parsed.get("type").and_then(|value| value.as_str()) == Some("error") {
                    status = event_error_status(&parsed).unwrap_or(500);
                }

                if is_terminal_event(&parsed) {
                    if is_previous_response_missing(&parsed) {
                        terminal_item = Some(Err(CodexError {
                            status: 0,
                            message: "Previous response not found".to_string(),
                            detail: Some("previous_response_not_found".to_string()),
                            retry_after: None,
                            usage_limit: None,
                            class: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }));
                    } else {
                        reusable = parsed.get("type").and_then(|value| value.as_str())
                            == Some("response.completed");
                        terminal_item = Some(Ok(parsed));
                    }
                    break;
                }

                if tx.send(Ok(parsed)).await.is_err() {
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                terminal_item = Some(Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::WebSocket,
                }));
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                // The reply shares the read deadline, so a peer that stops
                // reading cannot hold the stream past it.
                let remaining =
                    response_event_budget.saturating_sub(response_deadline_started.elapsed());
                if tokio::time::timeout(remaining, ws.send(Message::Pong(data)))
                    .await
                    .is_err()
                {
                    terminal_item =
                        Some(Err(write_timeout_error(response_started, idle_timeout_ms)));
                    break;
                }
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                log_response_close(req_id, close_code(frame.as_ref()), text_received);
                terminal_item = Some(Err(
                    if closed_for_message_size(frame.as_ref(), text_received) {
                        closed_message_too_large_error()
                    } else {
                        missing_terminal_error()
                    },
                ));
                break;
            }
            None => {
                terminal_item = Some(Err(missing_terminal_error()));
                break;
            }
            Some(Err(error)) => {
                terminal_item = Some(Err(CodexError {
                    status: 0,
                    message: format!("WebSocket stream error: {error}"),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::WebSocket,
                }));
                break;
            }
        }
    }

    if let (Some(tc), Some(body)) = (traffic.as_deref(), capture_body) {
        write_websocket_response_capture(tc, status, started_at.elapsed(), &body.bytes);
    }
    (reusable, terminal_item)
}

fn summarize_json_request_size(body: &serde_json::Value, body_json: &str) -> serde_json::Value {
    serde_json::json!({
        "bytes": body_json.len(),
        "inputCount": body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        "toolCount": body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::super::client::{BUFFERED_RESPONSE_TOO_LARGE_DETAIL, MAX_BUFFERED_RESPONSE_BYTES};
    use super::*;

    #[test]
    fn provider_retry_handoff_is_attempt_local() {
        let (_tx_a, rx_a) = mpsc::channel(1);
        let (stream_a, publisher_a) = CodexWebSocketEventStream::pending(rx_a);
        let (_tx_b, rx_b) = mpsc::channel(1);
        let (_stream_b, publisher_b) = CodexWebSocketEventStream::pending(rx_b);

        assert!(!publisher_a.is_provider_retry_handoff());
        assert!(!publisher_b.is_provider_retry_handoff());
        stream_a.mark_provider_retry_handoff();
        assert!(publisher_a.is_provider_retry_handoff());
        assert!(!publisher_b.is_provider_retry_handoff());
    }

    fn main_owner(session_id: &str) -> ConversationIdentity {
        ConversationIdentity::Main(session_id.to_string())
    }

    fn agent_owner(session_id: &str, agent_id: &str) -> ConversationIdentity {
        ConversationIdentity::Agent(session_id.to_string(), agent_id.to_string())
    }

    fn test_continuation(
        owner: Option<ConversationIdentity>,
        turn_id: Option<u64>,
        previous_response_id: Option<&str>,
        origin_socket_id: Option<u64>,
    ) -> ContinuationReservation {
        ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id,
                previous_response_id: previous_response_id.map(str::to_string),
                input_delta: Some(vec![]),
                input_delta_count: 0,
                disabled_reason: None,
            },
            owner,
            origin_socket_id,
        )
    }

    fn continuation_request() -> super::super::translate::request::ResponsesRequest {
        super::super::translate::request::ResponsesRequest {
            model: "gpt-5.6-sol".to_string(),
            instructions: None,
            input: vec![],
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: None,
                format: None,
            },
            reasoning: None,
        }
    }

    fn test_websocket_client() -> reqwest::Client {
        reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap()
    }

    #[test]
    fn advertised_message_limit_is_a_trimmed_positive_integer() {
        let read = |value: &[u8]| {
            let mut headers = HeaderMap::new();
            headers.insert(
                MAX_MESSAGE_BYTES_HEADER,
                http::HeaderValue::from_bytes(value).unwrap(),
            );
            advertised_max_message_bytes(&headers)
        };
        assert_eq!(read(b"4096"), Some(4096));
        assert_eq!(read(b" 4096\t"), Some(4096));
        for unusable in [
            &b""[..],
            b"0",
            b"-1",
            b"4k",
            b"1.5",
            b"99999999999999999999999",
            b"\xff",
        ] {
            assert_eq!(read(unusable), None, "{unusable:?}");
        }
        assert_eq!(advertised_max_message_bytes(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn direct_upgrade_reads_the_advertised_message_limit() {
        use tokio_tungstenite::tungstenite::handshake::server::{
            ErrorResponse, Request as UpgradeRequest, Response as UpgradeResponse,
        };

        for (advertised, expected) in [
            (Some("4096"), Some(4096)),
            (Some(" 2048 "), Some(2048)),
            (Some("0"), None),
            (Some("many"), None),
            (Some(""), None),
            (None, None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                #[allow(clippy::result_large_err)]
                let callback = move |_: &UpgradeRequest, mut response: UpgradeResponse| {
                    if let Some(value) = advertised {
                        response.headers_mut().insert(
                            MAX_MESSAGE_BYTES_HEADER,
                            http::HeaderValue::from_static(value),
                        );
                    }
                    Ok::<_, ErrorResponse>(response)
                };
                let _websocket = tokio_tungstenite::accept_hdr_async(socket, callback).await;
                futures_util::future::pending::<()>().await;
            });
            let connected = connect_with_timeout(
                &test_websocket_client(),
                &WebSocketProxyConfig::direct(),
                &format!("ws://{addr}/"),
                &HeaderMap::new(),
                2_000,
                OnConnectFailure::Fail,
            )
            .await
            .unwrap();
            assert_eq!(connected.max_message_bytes, expected, "{advertised:?}");
        }
    }

    #[tokio::test]
    async fn a_frame_over_the_advertised_limit_is_refused_and_one_at_it_is_not() {
        let mut entry = PoolEntry::new(create_dummy_stream_async().await);
        assert!(entry.check_message_size(usize::MAX).is_ok());

        entry.max_message_bytes = Some(100);
        assert!(entry.check_message_size(100).is_ok());
        let error = entry.check_message_size(101).unwrap_err();
        assert!(is_message_size_refusal(&error));
        assert_eq!(error.status, 413);
        assert_eq!(
            error.message,
            "The WebSocket request frame is 101 bytes, over the 100-byte message limit the Codex backend advertised"
        );
        assert!(!is_message_size_refusal(&missing_terminal_error()));
    }

    #[test]
    fn only_a_1009_close_before_any_text_is_a_size_refusal() {
        let frame = |code: CloseCode| CloseFrame {
            code,
            reason: "reason text".into(),
        };
        let size = frame(CloseCode::Size);
        assert!(closed_for_message_size(Some(&size), false));
        assert!(!closed_for_message_size(Some(&size), true));
        assert!(!closed_for_message_size(
            Some(&frame(CloseCode::Normal)),
            false
        ));
        assert!(!closed_for_message_size(
            Some(&frame(CloseCode::Error)),
            false
        ));
        assert!(!closed_for_message_size(None, false));
        assert_eq!(close_code(Some(&size)), 1009);
        assert_eq!(close_code(None), 0);
        assert!(is_message_size_refusal(&closed_message_too_large_error()));
    }

    #[tokio::test]
    async fn a_request_over_the_limit_drops_the_pooled_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("size-limit-pooled-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);

        // A peer that answers the validation Ping and counts the text frames
        // it receives.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let text_frames = Arc::new(AtomicUsize::new(0));
        let counted = text_frames.clone();
        let origin = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = websocket.next().await {
                if matches!(message, Message::Text(_)) {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let url = format!("http://{addr}/responses");
        let connected = connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/responses"),
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        let entry = Arc::new(PoolEntry::opened_with(
            ConnectedWebSocket {
                stream: connected.stream,
                max_message_bytes: Some(64),
            },
            &HeaderMap::new(),
        ));
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &continuation_request(),
            true,
        );
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_previous"),
            Some(entry.socket_id),
        );
        pool_insert(owner.clone(), entry.clone());
        let body = serde_json::json!({
            "type": "response.create",
            "input": "x".repeat(100),
        });
        let context = RequestContext {
            req_id: "size-limit-pooled-request".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };

        let buffered = match codex_websocket_request(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            &body,
            &context,
            None,
            1_000,
            2_000,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&continuation),
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("a frame over the limit must not be sent"),
            Err(error) => error,
        };
        assert_eq!(
            buffered.detail.as_deref(),
            Some(WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL)
        );
        assert_eq!(pooled_socket_id_for_tests(&owner), None);

        // The same socket pooled again for the live path.
        pool_insert(owner.clone(), entry.clone());
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            None,
            Some(&continuation),
            1_000,
            2_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        assert!(ready.used_pooled);
        let body_json = serde_json::to_string(&body).unwrap();
        let mut events = start_codex_websocket_events(
            ready,
            &body,
            body_json,
            &HeaderMap::new(),
            Some(&continuation),
            "test-request".to_string(),
            MAX_BUFFERED_RESPONSE_BYTES,
        );
        let live = match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the refusal must arrive at once")
        {
            Some(Err(error)) => error,
            _ => panic!("a frame over the limit must not be sent"),
        };
        assert_eq!(
            live.detail.as_deref(),
            Some(WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL)
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(pooled_socket_id_for_tests(&owner), None);

        // Both validation Pings were answered, so the peer has read past
        // anything written before them; no request frame reached it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(text_frames.load(Ordering::SeqCst), 0);
        super::super::continuation::abort_continuation_for_owner(&reserved);
        invalidate_codex_websocket_pool_owner(&owner);
        origin.abort();
    }

    fn handshake_error(status: u16) -> CodexError {
        CodexError {
            status,
            message: format!("handshake failed with status {status}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        }
    }

    fn origin_forbidden_error() -> ConnectAttemptError {
        ConnectAttemptError::Origin(handshake_error(http::StatusCode::FORBIDDEN.as_u16()))
    }

    struct DropProbeIo {
        probe: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
    }

    impl Drop for DropProbeIo {
        fn drop(&mut self) {
            if let Some((dropped, pool_was_unlocked)) = &self.probe {
                pool_was_unlocked.store(WS_POOL.try_lock().is_ok(), Ordering::SeqCst);
                dropped.store(true, Ordering::SeqCst);
            }
        }
    }

    impl AsyncRead for DropProbeIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for DropProbeIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct FailingWriteIo;

    impl AsyncRead for FailingWriteIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for FailingWriteIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test write failed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A peer that stops reading: while `delay` runs it takes every write,
    /// then it hands out `input` once and never takes a written byte again.
    struct StalledWriteIo {
        input: Vec<u8>,
        delay: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl AsyncRead for StalledWriteIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(delay) = self.delay.as_mut()
                && std::future::Future::poll(delay.as_mut(), cx).is_pending()
            {
                return Poll::Pending;
            }
            self.delay = None;
            if self.input.is_empty() {
                return Poll::Pending;
            }
            let len = self.input.len().min(buffer.remaining());
            buffer.put_slice(&self.input[..len]);
            self.input.drain(..len);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for StalledWriteIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.delay.is_some() {
                return Poll::Ready(Ok(buffer.len()));
            }
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.delay.is_some() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    /// An unmasked server Ping with an empty payload.
    const EMPTY_SERVER_PING: [u8; 2] = [0x89, 0x00];

    /// A WebSocket origin that completes the upgrade and then never reads.
    /// Its small receive buffer keeps a large frame from fitting in the
    /// socket buffers.
    async fn spawn_non_reading_origin() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(16).unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            futures_util::future::pending::<()>().await;
        });
        (addr, task)
    }

    /// A request body larger than any socket buffer pair on loopback.
    fn oversized_request_body() -> serde_json::Value {
        serde_json::json!({
            "type": "response.create",
            "input": "x".repeat(16 * 1024 * 1024),
        })
    }

    async fn raw_test_stream(
        probe: Option<(Arc<AtomicBool>, Arc<AtomicBool>)>,
    ) -> CodexWebSocketStream {
        let io: BoxedWebSocketIo = Box::new(DropProbeIo { probe });
        WebSocketStream::from_raw_socket(io, Role::Client, None).await
    }

    fn shared_pool_entry(ws: &Arc<AsyncMutex<CodexWebSocketStream>>) -> Arc<PoolEntry> {
        Arc::new(PoolEntry {
            ws: ws.clone(),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            created_at: now_ms(),
            last_activity: AtomicU64::new(next_pool_activity()),
            account_id: None,
            access_token: None,
            max_message_bytes: None,
        })
    }

    fn login_headers(account: Option<&str>, token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        if let Some(account) = account {
            headers.insert("chatgpt-account-id", account.parse().unwrap());
        }
        headers
    }

    /// A pooled socket goes to a request only under the login of its
    /// handshake: the same known account, whatever the token, or, with no
    /// account on either side, the same token. An account known on one side
    /// only is another login.
    #[tokio::test]
    async fn a_pooled_socket_serves_only_the_login_of_its_handshake() {
        let entry = |headers: HeaderMap| async move {
            PoolEntry::opened_with(
                ConnectedWebSocket {
                    stream: create_dummy_stream_async().await,
                    max_message_bytes: None,
                },
                &headers,
            )
        };
        let known = entry(login_headers(Some("acct-a"), "token-a")).await;
        assert!(known.serves_request(&login_headers(Some("acct-a"), "token-a")));
        assert!(known.serves_request(&login_headers(Some("acct-a"), "token-renewed")));
        assert!(!known.serves_request(&login_headers(Some("acct-b"), "token-a")));
        assert!(!known.serves_request(&login_headers(None, "token-a")));

        let unknown = entry(login_headers(None, "token-a")).await;
        assert!(unknown.serves_request(&login_headers(None, "token-a")));
        assert!(!unknown.serves_request(&login_headers(None, "token-b")));
        assert!(!unknown.serves_request(&login_headers(Some("acct-a"), "token-a")));
    }

    #[tokio::test]
    async fn connect_cleanup_uses_threshold_target_lru_and_skips_leases() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let ws = Arc::new(AsyncMutex::new(raw_test_stream(None).await));
        {
            let mut guard = WS_POOL.lock().unwrap();
            for index in 0..POOL_CONNECT_CLEANUP_THRESHOLD {
                guard.insert(
                    main_owner(&format!("entry-{index:02}")),
                    shared_pool_entry(&ws),
                );
            }
        }

        cleanup_pool_before_connect();
        assert_eq!(
            WS_POOL.lock().unwrap().len(),
            POOL_CONNECT_CLEANUP_THRESHOLD
        );

        WS_POOL
            .lock()
            .unwrap()
            .insert(main_owner("entry-50"), shared_pool_entry(&ws));
        WS_POOL
            .lock()
            .unwrap()
            .get(&main_owner("entry-00"))
            .unwrap()
            .touch();
        let leased = WS_POOL
            .lock()
            .unwrap()
            .get(&main_owner("entry-01"))
            .unwrap()
            .clone();

        cleanup_pool_before_connect();

        let guard = WS_POOL.lock().unwrap();
        assert_eq!(guard.len(), POOL_CONNECT_CLEANUP_TARGET);
        assert!(guard.contains_key(&main_owner("entry-00")));
        assert!(guard.contains_key(&main_owner("entry-01")));
        for index in 2..=12 {
            assert!(!guard.contains_key(&main_owner(&format!("entry-{index:02}"))));
        }
        assert!(guard.contains_key(&main_owner("entry-13")));
        assert!(guard.contains_key(&main_owner("entry-50")));
        drop(guard);
        drop(leased);
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn connect_cleanup_drops_final_socket_owners_after_unlocking_pool() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let dropped = Arc::new(AtomicBool::new(false));
        let pool_was_unlocked = Arc::new(AtomicBool::new(false));
        let shared_ws = Arc::new(AsyncMutex::new(raw_test_stream(None).await));
        let probe_stream =
            raw_test_stream(Some((dropped.clone(), pool_was_unlocked.clone()))).await;
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(
                main_owner("entry-00"),
                Arc::new(PoolEntry::new(probe_stream)),
            );
            for index in 1..=POOL_CONNECT_CLEANUP_THRESHOLD {
                guard.insert(
                    main_owner(&format!("entry-{index:02}")),
                    shared_pool_entry(&shared_ws),
                );
            }
        }

        cleanup_pool_before_connect();

        assert!(dropped.load(Ordering::SeqCst));
        assert!(pool_was_unlocked.load(Ordering::SeqCst));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test(start_paused = true)]
    async fn connect_gate_spaces_starts_without_serializing_handshakes() {
        let gate = Arc::new(WebSocketConnectGate::new(Duration::from_secs(1)));
        let preflight_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();

        let first_gate = gate.clone();
        let first_preflight = preflight_started.clone();
        let first_release = release_first.clone();
        let first_tx = started_tx.clone();
        let first = tokio::spawn(async move {
            first_gate
                .wait_to_start(async move {
                    first_preflight.notify_one();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok::<(), ()>(())
                })
                .await
                .unwrap();
            first_tx.send(tokio::time::Instant::now()).unwrap();
            first_release.notified().await;
        });
        preflight_started.notified().await;

        let second_gate = gate.clone();
        let second = tokio::spawn(async move {
            second_gate
                .wait_to_start(async { Ok::<(), ()>(()) })
                .await
                .unwrap();
            started_tx.send(tokio::time::Instant::now()).unwrap();
        });
        let first_started = started_rx.recv().await.unwrap();
        let second_started = started_rx.recv().await.unwrap();

        assert!(second_started.duration_since(first_started) >= Duration::from_secs(1));
        assert!(!first.is_finished());
        second.await.unwrap();
        release_first.notify_one();
        first.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn connect_gate_spaces_after_non_403_failure_without_retrying() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let starts = Mutex::new(Vec::new());

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            || async { Ok(()) },
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::Origin(handshake_error(
                        http::StatusCode::BAD_GATEWAY.as_u16(),
                    )))
                }
            },
        )
        .await
        .unwrap_err()
        .into_error();
        assert_eq!(error.status, http::StatusCode::BAD_GATEWAY.as_u16());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            || async { Ok(()) },
            || {
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async { Ok::<(), ConnectAttemptError>(()) }
            },
        )
        .await
        .map_err(ConnectAttemptError::into_error)
        .unwrap();

        let starts = starts.lock().unwrap();
        assert!(starts[1].duration_since(starts[0]) >= Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn origin_403_waits_for_cooldown_retries_once_and_returns_second_error() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let starts = Mutex::new(Vec::new());

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            || async { Ok(()) },
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async move {
                    if attempt == 0 {
                        Err::<(), ConnectAttemptError>(origin_forbidden_error())
                    } else {
                        Err::<(), ConnectAttemptError>(ConnectAttemptError::Origin(CodexError {
                            status: http::StatusCode::FORBIDDEN.as_u16(),
                            message: "second forbidden".to_string(),
                            detail: Some("second-detail".to_string()),
                            retry_after: Some("7".to_string()),
                            usage_limit: None,
                            class: None,
                            origin: CodexErrorOrigin::WebSocketHandshake,
                        }))
                    }
                }
            },
        )
        .await
        .unwrap_err()
        .into_error();

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(error.message, "second forbidden");
        assert_eq!(error.detail.as_deref(), Some("second-detail"));
        assert_eq!(error.retry_after.as_deref(), Some("7"));
        assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let starts = starts.lock().unwrap();
        assert!(starts[1].duration_since(starts[0]) >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn proxy_connect_403_is_not_retried() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let attempts = AtomicUsize::new(0);

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            || async { Ok(()) },
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::ProxyTunnel(
                        handshake_error(http::StatusCode::FORBIDDEN.as_u16()),
                    ))
                }
            },
        )
        .await
        .unwrap_err()
        .into_error();

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn explicit_proxy_connect_403_has_private_tunnel_provenance() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while connect_response_header_end(&request).is_none() {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let proxy_url = format!("http://{proxy_addr}");
        let proxy_config = WebSocketProxyConfig::new(None, Some(&proxy_url), None, None);
        let client = test_websocket_client();

        let error = match connect_once(
            &client,
            &proxy_config,
            "wss://codex.invalid/backend-api/codex/responses",
            &HeaderMap::new(),
            &AtomicBool::new(false),
        )
        .await
        {
            Ok(_) => panic!("proxy CONNECT rejection should fail"),
            Err(error) => error,
        };

        match error {
            ConnectAttemptError::ProxyTunnel(error) => {
                assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
                assert_eq!(
                    error.detail.as_deref(),
                    Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
                );
                assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
            }
            ConnectAttemptError::Origin(_) => panic!("proxy rejection was classified as origin"),
            ConnectAttemptError::Unreachable(_) => {
                panic!("proxy rejection was classified as unanswered")
            }
            ConnectAttemptError::Paused => panic!("proxy rejection was classified as paused"),
        }
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn origin_403_with_spoofed_proxy_detail_still_retries_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while connect_response_header_end(&request).is_none() {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = format!(
                    "{{\"error\":{{\"message\":\"{WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL}\"}}}}"
                );
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = test_websocket_client();
        let proxy_config = WebSocketProxyConfig::direct();
        let url = format!("ws://{addr}/backend-api/codex/responses");
        let headers = HeaderMap::new();
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let response_head = AtomicBool::new(false);

        let error = match connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::ZERO,
            &response_head,
            || async { Ok(()) },
            || connect_once(&client, &proxy_config, &url, &headers, &response_head),
        )
        .await
        {
            Ok(_) => panic!("spoofed origin rejection should fail after one retry"),
            Err(error) => error.into_error(),
        };

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
        );
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_applies_to_each_network_attempt_not_waits_or_cooldown() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let started_at = tokio::time::Instant::now();

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(2),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            || async { Ok(()) },
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Err::<(), ConnectAttemptError>(origin_forbidden_error())
                    } else {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap_err();

        // A connect that ran out of time got no answer.
        assert!(matches!(error, ConnectAttemptError::Unreachable(_)));
        let error = error.into_error();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(error.message, "WebSocket connect timeout after 2000ms");
        assert_eq!(started_at.elapsed(), Duration::from_secs(6));
    }

    #[test]
    fn websocket_tls_configuration_is_shared() {
        let first = websocket_tls_config();
        let second = websocket_tls_config();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn event_error_status_requires_error_event_and_checks_numeric_fallbacks() {
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.failed",
                "status": "failed",
                "status_code": 401
            })),
            Some(401)
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.completed",
                "status_code": 401
            })),
            None
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "error",
                "error": {"status": 401}
            })),
            Some(401)
        );
    }

    #[test]
    fn websocket_url_conversion() {
        assert_eq!(
            to_websocket_url("https://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert_eq!(
            to_websocket_url("http://example.test/codex").unwrap(),
            "ws://example.test/codex"
        );
        assert_eq!(
            to_websocket_url("wss://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert!(to_websocket_url("ftp://example.test/codex").is_err());
    }

    #[test]
    fn websocket_headers_keep_every_value_of_a_repeated_header() {
        let mut http_headers = HeaderMap::new();
        http_headers.append("x-multi", "one".parse().unwrap());
        http_headers.append("x-multi", "two".parse().unwrap());

        let ws = codex_websocket_headers(&http_headers);

        let values: Vec<_> = ws.get_all("x-multi").iter().collect();
        assert_eq!(values, ["one", "two"]);
    }

    #[test]
    fn websocket_upgrade_url_preserves_authority_path_and_query() {
        assert_eq!(
            to_http_upgrade_url("wss://chatgpt.com/backend-api/codex/responses?mode=live").unwrap(),
            "https://chatgpt.com/backend-api/codex/responses?mode=live"
        );
        assert_eq!(
            to_http_upgrade_url("ws://127.0.0.1:4141/backend-api/codex/responses").unwrap(),
            "http://127.0.0.1:4141/backend-api/codex/responses"
        );
        assert_eq!(
            to_http_upgrade_url("ws://[::1]:4141/path").unwrap(),
            "http://[::1]:4141/path"
        );
    }

    #[test]
    fn validates_tokenized_websocket_upgrade_headers() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut headers = HeaderMap::new();
        headers.insert(http::header::UPGRADE, "h2c, WebSocket".parse().unwrap());
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(
            http::header::SEC_WEBSOCKET_ACCEPT,
            derive_accept_key(key.as_bytes()).parse().unwrap(),
        );
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            "responses".parse().unwrap(),
        );

        validate_websocket_upgrade(
            http::Version::HTTP_11,
            &headers,
            key,
            &["responses".to_string()],
        )
        .unwrap();
    }

    #[test]
    fn rejects_invalid_websocket_upgrade_headers() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let valid = || {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
            headers.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
            headers.insert(
                http::header::SEC_WEBSOCKET_ACCEPT,
                derive_accept_key(key.as_bytes()).parse().unwrap(),
            );
            headers
        };

        let mut missing_upgrade = valid();
        missing_upgrade.remove(http::header::UPGRADE);
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &missing_upgrade, key, &[]).is_err()
        );

        let mut missing_connection = valid();
        missing_connection.remove(http::header::CONNECTION);
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &missing_connection, key, &[])
                .is_err()
        );

        let mut wrong_accept = valid();
        wrong_accept.insert(http::header::SEC_WEBSOCKET_ACCEPT, "wrong".parse().unwrap());
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &wrong_accept, key, &[]).is_err()
        );

        let unsolicited_extension = {
            let mut headers = valid();
            headers.insert(
                http::header::SEC_WEBSOCKET_EXTENSIONS,
                "permessage-deflate".parse().unwrap(),
            );
            headers
        };
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &unsolicited_extension, key, &[],)
                .is_err()
        );

        assert!(validate_websocket_upgrade(http::Version::HTTP_10, &valid(), key, &[]).is_err());

        let mut unsolicited_protocol = valid();
        unsolicited_protocol.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            "unexpected".parse().unwrap(),
        );
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &unsolicited_protocol, key, &[])
                .is_err()
        );
    }

    #[test]
    fn websocket_headers_rewrite_beta() {
        let mut headers = http::HeaderMap::new();
        headers.insert("openai-beta", "responses=experimental".parse().unwrap());
        headers.insert("content-length", "10".parse().unwrap());
        headers.insert("authorization", "Bearer tok".parse().unwrap());
        headers.insert(
            http::header::PROXY_AUTHORIZATION,
            "Basic dXNlcjpwYXNz".parse().unwrap(),
        );
        let ws = codex_websocket_headers(&headers);
        assert_eq!(ws.get("openai-beta").unwrap(), WEBSOCKET_PROTOCOL_HEADER);
        assert!(!ws.contains_key("content-length"));
        assert!(!ws.contains_key(http::header::PROXY_AUTHORIZATION));
        assert_eq!(ws.get("authorization").unwrap(), "Bearer tok");
    }

    #[test]
    fn websocket_headers_strips_accept() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
        let ws = codex_websocket_headers(&headers);
        assert!(!ws.contains_key(http::header::ACCEPT.as_str()));
    }

    #[test]
    fn websocket_headers_adds_sec_key() {
        let headers = http::HeaderMap::new();
        let ws = codex_websocket_headers(&headers);
        assert!(ws.contains_key("sec-websocket-key"));
    }

    #[test]
    fn encode_sse_single_line() {
        let result = encode_sse(r#"{"type":"test","data":"hello"}"#);
        let expected = b"data: {\"type\":\"test\",\"data\":\"hello\"}\n\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn encode_sse_multi_line() {
        let result = encode_sse("line1\nline2");
        assert_eq!(
            String::from_utf8(result).unwrap(),
            "data: line1\ndata: line2\n\n"
        );
    }

    #[test]
    fn encoded_sse_len_is_the_length_of_the_encoding() {
        for text in [
            r#"{"type":"test","data":"hello"}"#,
            "line1\nline2\nline3",
            "",
            "\n",
            "\n\n\n",
            "line1\n\nline2",
            "line1\n",
            "line1\r\nline2\r\n",
            "\r\n\r\n",
            "line1\rline2",
        ] {
            assert_eq!(encoded_sse_len(text), encode_sse(text).len(), "{text:?}");
        }
    }

    #[test]
    fn is_terminal_event_detection() {
        let completed = serde_json::json!({"type": "response.completed"});
        assert!(is_terminal_event(&completed));

        let delta = serde_json::json!({"type": "response.output_text.delta"});
        assert!(!is_terminal_event(&delta));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_terminal_event(&error));
    }

    #[test]
    fn is_response_event_detection() {
        let rate_limits = serde_json::json!({"type": "codex.rate_limits"});
        assert!(!is_response_event(&rate_limits));

        let output = serde_json::json!({"type": "response.output_text.delta"});
        assert!(is_response_event(&output));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_response_event(&error));
    }

    #[test]
    fn is_previous_response_missing_detection() {
        let by_code = serde_json::json!({
            "type": "error",
            "error": {"code": "previous_response_not_found", "message": "not found"}
        });
        assert!(is_previous_response_missing(&by_code));

        let by_msg = serde_json::json!({
            "type": "error",
            "error": {"message": "The previous response was not found"}
        });
        assert!(is_previous_response_missing(&by_msg));

        let nested = serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": "previous_response_not_found",
                    "message": "Previous response not found"
                }
            }
        });
        assert!(is_previous_response_missing(&nested));

        let unrelated = serde_json::json!({"type": "error", "error": {"message": "rate limited"}});
        assert!(!is_previous_response_missing(&unrelated));
    }

    #[test]
    fn websocket_metadata_does_not_serialize_typed_owner() {
        let temp = tempfile::tempdir().unwrap();
        let traffic = crate::traffic::test_capture(temp.path().join("traffic"));
        let owner = agent_owner("session-secret", "agent-secret");
        let reservation = test_continuation(Some(owner), None, None, None);

        write_websocket_metadata_capture(
            &traffic,
            "wss://example.invalid/responses",
            Some(&reservation),
            false,
        );

        let artifact = std::fs::read_dir(traffic.root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let captured = std::fs::read_to_string(artifact).unwrap();
        assert!(captured.contains("poolingEnabled"));
        assert!(!captured.contains("poolKey"));
        assert!(!captured.contains("session-secret"));
        assert!(!captured.contains("agent-secret"));
    }

    #[tokio::test]
    async fn pool_checkout_is_exclusive_and_removal_is_identity_safe() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let first = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        let owner = main_owner("exclusive");
        pool_insert(owner.clone(), first.clone());
        assert!(Arc::ptr_eq(
            &WS_POOL.lock().unwrap().remove(&owner).unwrap(),
            &first
        ));
        assert!(WS_POOL.lock().unwrap().remove(&owner).is_none());

        let replacement = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        pool_insert(owner.clone(), replacement.clone());
        pool_remove_entry(&owner, &first);
        let reservation = test_continuation(Some(owner.clone()), None, None, None);
        invalidate_codex_websocket_pool_socket(&reservation, Some(first.socket_id));
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&owner).unwrap(),
            &replacement
        ));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn pooled_validation_uses_unique_nonce_and_rejects_stale_pong() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (queue_stale_tx, queue_stale_rx) = tokio::sync::oneshot::channel();
        let (stale_queued_tx, stale_queued_rx) = tokio::sync::oneshot::channel();
        let (release_peer_tx, release_peer_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let prior_nonce = match websocket.next().await {
                Some(Ok(Message::Ping(payload))) => payload,
                other => panic!("unexpected first validation frame: {other:?}"),
            };
            assert_eq!(prior_nonce.len(), std::mem::size_of::<u64>());
            assert_ne!(prior_nonce, 0_u64.to_be_bytes());
            websocket
                .send(Message::Pong(prior_nonce.clone()))
                .await
                .unwrap();

            queue_stale_rx.await.unwrap();
            websocket
                .send(Message::Pong(prior_nonce.clone()))
                .await
                .unwrap();
            stale_queued_tx.send(()).unwrap();

            let current_nonce = match websocket.next().await {
                Some(Ok(Message::Ping(payload))) => payload,
                other => panic!("unexpected second validation frame: {other:?}"),
            };
            assert_ne!(current_nonce, prior_nonce);
            assert_ne!(current_nonce, 0_u64.to_be_bytes());
            release_peer_rx.await.unwrap();
        });
        let (mut websocket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();

        validate_pooled_websocket(&mut websocket, 1_000)
            .await
            .unwrap();
        queue_stale_tx.send(()).unwrap();
        stale_queued_rx.await.unwrap();
        let error = validate_pooled_websocket(&mut websocket, 100)
            .await
            .unwrap_err();
        assert_eq!(error, "unexpected Pong during pooled validation");

        release_peer_tx.send(()).unwrap();
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn pool_entries_receive_monotonic_nonzero_socket_ids() {
        let first = PoolEntry::new(raw_test_stream(None).await);
        let second = PoolEntry::new(raw_test_stream(None).await);

        assert_ne!(first.socket_id, 0);
        assert!(second.socket_id > first.socket_id);
    }

    #[tokio::test]
    async fn continuation_rejects_and_preserves_same_owner_replacement() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = agent_owner("replacement-session", "replacement-agent");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let replacement = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        pool_insert(owner.clone(), replacement.clone());
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_origin"),
            Some(replacement.socket_id.checked_add(1).unwrap()),
        );

        let error = match prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            Some(&continuation),
            50,
            50,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("replacement socket must not satisfy continuation provenance"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
        );
        assert!(!error.message.contains("replacement-session"));
        assert!(!error.message.contains("replacement-agent"));
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&owner).unwrap(),
            &replacement
        ));

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn dead_exact_origin_removes_only_that_arc_and_preserves_replacement() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("dead-origin-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let exact = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        let replacement = Arc::new(PoolEntry::new(raw_test_stream(None).await));
        pool_insert(owner.clone(), exact.clone());
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_exact"),
            Some(exact.socket_id),
        );

        let replacement_owner = owner.clone();
        let replacement_for_task = replacement.clone();
        let mut insert_replacement = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(1), async move {
                loop {
                    if !WS_POOL.lock().unwrap().contains_key(&replacement_owner) {
                        pool_insert(replacement_owner, replacement_for_task);
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
        });
        let error = match prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            "ws://127.0.0.1:9/responses",
            &HeaderMap::new(),
            None,
            Some(&continuation),
            50,
            50,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("dead continuation origin must be rejected before request send"),
            Err(error) => error,
        };
        match tokio::time::timeout(Duration::from_secs(2), &mut insert_replacement).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => panic!(
                "replacement polling timed out; currently pooled socket ID: {:?}",
                pooled_socket_id_for_tests(&owner)
            ),
            Ok(Err(error)) => panic!(
                "replacement polling task failed ({error}); currently pooled socket ID: {:?}",
                pooled_socket_id_for_tests(&owner)
            ),
            Err(_) => {
                insert_replacement.abort();
                let abort_result = insert_replacement.await;
                panic!(
                    "replacement polling join timed out ({abort_result:?}); currently pooled socket ID: {:?}",
                    pooled_socket_id_for_tests(&owner)
                );
            }
        }

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
        );
        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&owner).unwrap(),
            &replacement
        ));
        assert!(!Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&owner).unwrap(),
            &exact
        ));

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn completed_terminal_is_published_after_origin_returns_to_pool() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = agent_owner("terminal-order-session", "terminal-order-agent");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_response_tx, release_response_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = websocket.next().await {
                match message {
                    Message::Ping(payload) => {
                        websocket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) => {
                        release_response_rx.await.unwrap();
                        websocket
                            .send(Message::Text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_terminal_order"}
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        let context = RequestContext {
            req_id: "terminal-order-request".to_string(),
            session_id: Some("header-session-is-not-pool-owner".to_string()),
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let mut events = codex_websocket_event_stream(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            &serde_json::json!({"type":"response.create","input":[]}),
            &context,
            None,
            1_000,
            1_000,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&continuation),
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        assert_eq!(events.socket_id(), None);
        release_response_tx.send(()).unwrap();
        let terminal = events.recv().await.unwrap().unwrap();

        assert_eq!(
            terminal.get("type").and_then(serde_json::Value::as_str),
            Some("response.completed")
        );
        let pooled = WS_POOL
            .lock()
            .unwrap()
            .get(&owner)
            .cloned()
            .expect("origin must be reusable before terminal publication");
        assert_eq!(events.socket_id(), Some(pooled.socket_id));
        server.await.unwrap();

        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&continuation);
    }

    #[tokio::test]
    async fn dropped_receiver_removes_completed_origin_after_terminal_publication_fails() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("dropped-terminal-receiver-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);
        let request = continuation_request();
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (release_response_tx, release_response_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = websocket.next().await {
                match message {
                    Message::Ping(payload) => {
                        websocket.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Text(_) => {
                        request_seen_tx.send(()).unwrap();
                        release_response_rx.await.unwrap();
                        websocket
                            .send(Message::Text(
                                serde_json::json!({
                                    "type": "response.completed",
                                    "response": {"id": "resp_dropped_receiver"}
                                })
                                .to_string(),
                            ))
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        let headers = HeaderMap::new();
        let body = serde_json::json!({"type":"response.create","input":[]});
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &headers,
            None,
            Some(&continuation),
            1_000,
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        let exact = ready.entry.clone();
        let events = start_codex_websocket_events(
            ready,
            &body,
            serde_json::to_string(&body).unwrap(),
            &headers,
            Some(&continuation),
            "test-request".to_string(),
            MAX_BUFFERED_RESPONSE_BYTES,
        );

        request_seen_rx.await.unwrap();
        drop(events);
        release_response_tx.send(()).unwrap();
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&exact) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed terminal publication must release the exact completed pool entry");

        assert!(
            WS_POOL
                .lock()
                .unwrap()
                .get(&owner)
                .is_none_or(|pooled| !Arc::ptr_eq(pooled, &exact)),
            "the exact completed socket must not remain pooled"
        );
        super::super::continuation::abort_continuation_for_owner(&continuation);
    }

    #[tokio::test]
    async fn pool_invalidation() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let first_stream = create_dummy_stream_async().await;
        let second_stream = create_dummy_stream_async().await;
        let first_owner = agent_owner("test-session", "first-agent");
        let sibling_owner = agent_owner("test-session", "sibling-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(first_owner.clone(), Arc::new(PoolEntry::new(first_stream)));
            guard.insert(
                sibling_owner.clone(),
                Arc::new(PoolEntry::new(second_stream)),
            );
        }
        assert!(WS_POOL.lock().unwrap().contains_key(&first_owner));
        assert!(WS_POOL.lock().unwrap().contains_key(&sibling_owner));

        invalidate_codex_websocket_pool_owner(&first_owner);
        assert!(!WS_POOL.lock().unwrap().contains_key(&first_owner));
        assert!(WS_POOL.lock().unwrap().contains_key(&sibling_owner));
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn session_key_invalidation_removes_main_and_agents_only_for_exact_session() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let session_id = "session-key";
        let main = main_owner(session_id);
        let first_agent = agent_owner(session_id, "first-agent");
        let second_agent = agent_owner(session_id, "second-agent");
        let other_session = main_owner("session-key-other");
        for owner in [
            main.clone(),
            first_agent.clone(),
            second_agent.clone(),
            other_session.clone(),
        ] {
            pool_insert(
                owner,
                Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
            );
        }

        invalidate_codex_websocket_pool_key(session_id);

        let pool = WS_POOL.lock().unwrap();
        assert!(!pool.contains_key(&main));
        assert!(!pool.contains_key(&first_agent));
        assert!(!pool.contains_key(&second_agent));
        assert!(pool.contains_key(&other_session));
        drop(pool);
        clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn missing_owner_or_turn_cannot_mutate_pool_state() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("missing-owner-turn-pool");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        clear_codex_websocket_pool_for_tests();
        let request = continuation_request();
        let current = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let pooled = Arc::new(PoolEntry::new(create_dummy_stream_async().await));
        pool_insert(owner.clone(), pooled.clone());
        let missing_owner = test_continuation(None, current.turn_id(), None, None);
        let missing_turn = test_continuation(Some(owner.clone()), None, None, None);

        assert!(pool_take_for_turn(&missing_owner).is_none());
        assert!(pool_take_for_turn(&missing_turn).is_none());
        assert!(!pool_insert_for_turn(
            &missing_owner,
            Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
        ));
        assert!(!pool_insert_for_turn(
            &missing_turn,
            Arc::new(PoolEntry::new(create_dummy_stream_async().await)),
        ));
        invalidate_codex_websocket_pool_turn_for_owner(&owner, None);
        invalidate_codex_websocket_pool_socket(&missing_owner, Some(pooled.socket_id));

        assert!(Arc::ptr_eq(
            WS_POOL.lock().unwrap().get(&owner).unwrap(),
            &pooled
        ));
        clear_codex_websocket_pool_for_tests();
        super::super::continuation::abort_continuation_for_owner(&current);
    }

    #[tokio::test]
    async fn websocket_connect_401_is_pre_request_handshake_error() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\npolicy denied")
                .await
                .unwrap();
        });

        let client = test_websocket_client();
        let err = match connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("expected unauthorized websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 401);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test]
    async fn tunneled_handshake_rejection_is_classified_from_its_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let body = r#"{"error":{"code":"usage_not_included","message":"Your plan does not include this usage."}}"#;
        let (client_io, mut server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = server_io.read(&mut buf).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            server_io.write_all(response.as_bytes()).await.unwrap();
        });

        let request = tunneled_websocket_request(
            "ws://codex.test/backend-api/codex/responses",
            &HeaderMap::new(),
            &generate_key(),
        )
        .unwrap();
        let err = match tokio_tungstenite::client_async(request, client_io).await {
            Ok(_) => panic!("expected the tunneled upgrade to be rejected"),
            Err(error) => tungstenite_handshake_error(error, None),
        };

        assert_eq!(err.status, 403);
        assert_eq!(
            err.class,
            Some(super::super::events::CodexErrorClass::UsageNotIncluded)
        );
        assert_eq!(
            err.detail.as_deref(),
            Some("Your plan does not include this usage.")
        );
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);

        // A proxy's own 407 is never read, whatever its body says.
        let rejected = http::Response::builder()
            .status(http::StatusCode::PROXY_AUTHENTICATION_REQUIRED)
            .body(Some(body.as_bytes().to_vec()))
            .unwrap();
        let err = tungstenite_handshake_error(
            tokio_tungstenite::tungstenite::Error::Http(rejected),
            None,
        );
        assert_eq!(err.status, 407);
        assert_eq!(err.class, None);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
    }

    #[tokio::test]
    async fn websocket_connect_502_preserves_retry_metadata() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nRetry-After: 3\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = test_websocket_client();
        let err = match connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("expected websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 502);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.retry_after.as_deref(), Some("3"));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    fn raw_rejection(status_line: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        format!(
            "HTTP/1.1 {status_line}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    async fn direct_handshake_rejection(response: Vec<u8>) -> CodexError {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket.write_all(&response).await.unwrap();
        });
        match connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("expected websocket handshake to fail"),
            Err(err) => err,
        }
    }

    /// The direct path's answer to one attempt, without the connect policy,
    /// which tries an origin 403 again after a cooldown.
    async fn direct_handshake_rejection_once(response: Vec<u8>) -> CodexError {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket.write_all(&response).await.unwrap();
        });
        match connect_once(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            &AtomicBool::new(false),
        )
        .await
        {
            Ok(_) => panic!("expected websocket handshake to fail"),
            Err(err) => err.into_error(),
        }
    }

    async fn tunneled_handshake_rejection(response: Vec<u8>) -> CodexError {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client_io, mut server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = server_io.read(&mut buf).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
            }
            server_io.write_all(&response).await.unwrap();
        });
        let request = tunneled_websocket_request(
            "ws://codex.test/backend-api/codex/responses",
            &HeaderMap::new(),
            &generate_key(),
        )
        .unwrap();
        match tokio_tungstenite::client_async(request, client_io).await {
            Ok(_) => panic!("expected the tunneled upgrade to be rejected"),
            Err(error) => tungstenite_handshake_error(error, None),
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rejected_upgrade_answers_retry_after_by_the_class_rules() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        let stop: &[(&str, &str)] = &[("x-should-retry", "false")];
        let spent: &[(&str, &str)] = &[
            ("x-should-retry", "false"),
            ("anthropic-ratelimit-unified-status", "rejected"),
        ];
        // Status line, Retry-After sent, body, then the client's status,
        // Retry-After and other headers.
        let cases = [
            (
                "429 Too Many Requests",
                Some("1.5"),
                "",
                429,
                Some("2"),
                &[][..],
            ),
            (
                "503 Service Unavailable",
                Some(date),
                "",
                503,
                Some(date),
                &[],
            ),
            ("429 Too Many Requests", None, "", 429, None, &[]),
            (
                "529 Site Overloaded",
                Some("4"),
                r#"{"error":{"code":"server_is_overloaded","message":"busy"}}"#,
                529,
                Some("4"),
                &[],
            ),
            (
                "429 Too Many Requests",
                Some("0.2"),
                r#"{"error":{"code":"rate_limit_exceeded","message":"Slow down."}}"#,
                429,
                Some("1"),
                &[],
            ),
            (
                "429 Too Many Requests",
                Some("60"),
                r#"{"error":{"code":"insufficient_quota","message":"No credits left."}}"#,
                429,
                None,
                stop,
            ),
            (
                "429 Too Many Requests",
                Some("3600"),
                r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":3600}}"#,
                429,
                None,
                spent,
            ),
        ];
        for (status_line, retry_after, body, status, expected, headers) in cases {
            let sent = retry_after.map(|value| ("Retry-After", value));
            let raw = raw_rejection(status_line, sent.as_slice(), body);
            let direct = direct_handshake_rejection(raw.clone()).await;
            let tunneled = tunneled_handshake_rejection(raw).await;
            for err in [direct, tunneled] {
                let response = super::super::map_codex_error_to_response(&err);
                let case = format!("{status_line} {body}");
                assert_eq!(response.status().as_u16(), status, "{case}");
                assert_eq!(
                    response
                        .headers()
                        .get(http::header::RETRY_AFTER)
                        .map(|value| value.to_str().unwrap()),
                    expected,
                    "{case}"
                );
                for (name, value) in headers {
                    assert_eq!(response.headers()[*name], *value, "{case}");
                }
            }
        }
        super::super::rate_limits::clear_for_tests();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_long_rejection_body_still_reports_a_spent_window() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        let message = format!("The usage limit has been reached. {}", "x".repeat(2048));
        let body = serde_json::json!({"error": {
            "type": "usage_limit_reached",
            "message": message,
            "resets_in_seconds": 3600
        }})
        .to_string();
        assert!(body.len() > 2 * MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
        let raw = raw_rejection("429 Too Many Requests", &[("Retry-After", "3600")], &body);
        let direct = direct_handshake_rejection(raw.clone()).await;
        let tunneled = tunneled_handshake_rejection(raw).await;
        for err in [direct, tunneled] {
            assert!(err.usage_limit.is_some(), "{:?}", err.origin);
            assert!(err.detail.as_ref().unwrap().len() <= MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
            let response = super::super::map_codex_error_to_response(&err);
            assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()["x-should-retry"], "false");
            assert_eq!(
                response.headers()["anthropic-ratelimit-unified-status"],
                "rejected"
            );
            assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        }
        super::super::rate_limits::clear_for_tests();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rejected_upgrade_takes_the_spent_window_from_codex_headers() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        let raw = raw_rejection(
            "429 Too Many Requests",
            &[
                ("Retry-After", "9568"),
                ("X-Codex-Primary-Window-Minutes", "300"),
                ("X-Codex-Primary-Reset-After-Seconds", "9569"),
                ("X-Codex-Primary-Reset-At", "1788879438"),
                ("X-Codex-Secondary-Window-Minutes", "10080"),
                ("X-Codex-Secondary-Reset-After-Seconds", "596369"),
                ("X-Codex-Secondary-Reset-At", "1789466238"),
            ],
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":9568}}"#,
        );
        let direct = direct_handshake_rejection(raw.clone()).await;
        let tunneled = tunneled_handshake_rejection(raw).await;
        for err in [direct, tunneled] {
            let response = super::super::map_codex_error_to_response(&err);
            assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                response.headers()["anthropic-ratelimit-unified-reset"],
                "1788879438"
            );
            assert_eq!(
                response.headers()["anthropic-ratelimit-unified-representative-claim"],
                "five_hour"
            );
            assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        }
        super::super::rate_limits::clear_for_tests();
    }

    #[test]
    fn a_refused_connect_tunnel_normalizes_its_retry_after() {
        let head = b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0.5\r\n\r\n";
        let (status, retry_after) = parse_connect_response_head(head).unwrap();
        assert_eq!(status, 503);
        assert_eq!(retry_after.as_deref(), Some("1"));
        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        let head = format!("HTTP/1.1 502 Bad Gateway\r\nretry-after: {date}\r\n\r\n");
        let (_, retry_after) = parse_connect_response_head(head.as_bytes()).unwrap();
        assert_eq!(retry_after.as_deref(), Some(date));
        for unusable in ["x".repeat(65), "soon \u{1}".to_string(), "é".to_string()] {
            let head =
                format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: {unusable}\r\n\r\n");
            let (_, retry_after) = parse_connect_response_head(head.as_bytes()).unwrap();
            assert_eq!(retry_after, None, "{unusable:?}");
        }
    }

    #[test]
    fn a_rejected_upgrade_takes_its_header_when_the_body_delay_is_unusable() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {
                "code": "policy_capacity_unavailable",
                "message": "busy",
                "retry_after": format!("{}é", "a".repeat(3999))
            }
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "9".parse().unwrap());
        let err = handshake_rejection_error(503, &headers, Some(&body), None);
        assert_eq!(err.retry_after.as_deref(), Some("9"));

        headers.insert(http::header::RETRY_AFTER, "x".repeat(65).parse().unwrap());
        let err = handshake_rejection_error(503, &headers, Some(&body), None);
        assert_eq!(err.retry_after, None);
        let err = handshake_rejection_error(407, &headers, None, None);
        assert_eq!(err.retry_after, None);
    }

    #[tokio::test]
    async fn websocket_connects_through_explicit_http_proxy() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let request_text = String::from_utf8(request).unwrap();
            let key = request_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim().to_string())
                })
                .unwrap();
            let _ = captured_tx.send(request_text);
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                derive_accept_key(key.as_bytes())
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            assert_eq!(
                websocket.next().await.unwrap().unwrap(),
                Message::Text("hello".to_string())
            );
            websocket
                .send(Message::Text("proxy-ok".to_string()))
                .await
                .unwrap();
        });

        let client = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(
                reqwest::Proxy::http(format!("http://proxy-user:proxy-pass@{proxy_addr}")).unwrap(),
            )
            .build()
            .unwrap();
        let mut websocket = connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            "ws://codex.invalid/backend-api/codex/responses",
            &HeaderMap::new(),
            2_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap()
        .stream;
        websocket
            .send(Message::Text("hello".to_string()))
            .await
            .unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::Text("proxy-ok".to_string())
        );

        let captured = captured_rx.await.unwrap();
        assert!(
            captured.starts_with("GET http://codex.invalid/backend-api/codex/responses HTTP/1.1")
        );
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn connect_tunnel_accepts_fragmented_non_200_success() {
        let (client, mut proxy) = tokio::io::duplex(4096);
        let proxy_task = tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = proxy.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(request.starts_with(b"CONNECT codex.invalid:4443 HTTP/1.1\r\n"));
            proxy.write_all(b"HTTP/1.").await.unwrap();
            tokio::task::yield_now().await;
            proxy
                .write_all(b"1 204 No Content\r\nX-Proxy: ok\r\n\r\nprefixed")
                .await
                .unwrap();
        });

        let stream: BoxedWebSocketIo = Box::new(client);
        let response_head = AtomicBool::new(false);
        let mut stream =
            establish_connect_tunnel(stream, "codex.invalid:4443", None, &response_head)
                .await
                .unwrap();
        // The open tunnel waits on the destination, which has not answered.
        assert!(!response_head.load(Ordering::Relaxed));
        let mut prefix = [0_u8; 8];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"prefixed");
        proxy_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_tunnel_classifies_fragmented_proxy_authentication() {
        let (client, mut proxy) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut request = [0_u8; 512];
            let _ = proxy.read(&mut request).await.unwrap();
            proxy.write_all(b"HTTP/1.1 4").await.unwrap();
            tokio::task::yield_now().await;
            proxy
                .write_all(b"07 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });

        let stream: BoxedWebSocketIo = Box::new(client);
        let response_head = AtomicBool::new(false);
        let error = match establish_connect_tunnel(
            stream,
            "codex.invalid:4443",
            None,
            &response_head,
        )
        .await
        {
            Ok(_) => panic!("proxy authentication should be rejected"),
            Err(error) => error,
        };
        assert!(response_head.load(Ordering::Relaxed));
        assert_eq!(
            error.status,
            http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
        );
        assert_eq!(error.message, "WebSocket proxy authentication failed");
    }

    #[test]
    fn websocket_proxy_routing_honors_no_proxy_and_leaves_socks_to_reqwest() {
        let http_proxy = "http://proxy.example:8080";
        let config = WebSocketProxyConfig::new(None, Some(http_proxy), None, None);
        assert!(config.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            config
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_some()
        );

        let bypass = WebSocketProxyConfig::new(None, Some(http_proxy), None, Some("codex.invalid"));
        assert!(!bypass.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            bypass
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_none()
        );

        let socks =
            WebSocketProxyConfig::new(None, Some("socks5h://proxy.example:1080"), None, None);
        assert!(socks.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            socks
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn websocket_wss_uses_http_connect_without_leaking_proxy_credentials() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = captured_tx.send(String::from_utf8(request).unwrap());
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let proxy_url = format!("http://secret-user:secret-pass@{proxy_addr}");
        let client = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
            .build()
            .unwrap();
        let proxy_config = WebSocketProxyConfig::new(None, Some(&proxy_url), None, None);
        let error = match connect_with_timeout(
            &client,
            &proxy_config,
            "wss://codex.invalid:4443/backend-api/codex/responses",
            &HeaderMap::new(),
            2_000,
            OnConnectFailure::Fail,
        )
        .await
        {
            Ok(_) => panic!("proxy rejection should fail the WebSocket connection"),
            Err(error) => error,
        };

        let captured = captured_rx.await.unwrap();
        assert!(captured.starts_with("CONNECT codex.invalid:4443 HTTP/1.1"));
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        assert!(!error.message.contains("secret-user"));
        assert!(!error.message.contains("secret-pass"));
        assert!(
            !error
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("secret")
        );
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn binary_frame_invalidates_pool_owner() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = agent_owner("binary-session", "binary-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = match collect_ws_events(
            &mut ws,
            1_000,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        {
            Ok(_) => panic!("expected binary frame to fail"),
            Err(err) => err,
        };

        assert!(err.message.contains("binary frames"));
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    const LIMIT_CREATED_EVENT: &str =
        r#"{"type":"response.created","response":{"id":"resp_limit"}}"#;
    const LIMIT_COMPLETED_EVENT: &str =
        r#"{"type":"response.completed","response":{"id":"resp_limit","output":[]}}"#;

    /// A socket that answers with `LIMIT_CREATED_EVENT` then
    /// `LIMIT_COMPLETED_EVENT`, as a buffered answer of two events.
    async fn connect_to_two_event_answer()
    -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        connect_to_answer(&[LIMIT_CREATED_EVENT, LIMIT_COMPLETED_EVENT]).await
    }

    /// A socket that answers with `events`, one text frame each.
    async fn connect_to_answer(
        events: &'static [&'static str],
    ) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            for event in events {
                ws.send(Message::Text((*event).into())).await.unwrap();
            }
            let _ = ws.next().await;
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        ws
    }

    /// A spent window sent as indented JSON over several lines.
    const LIMIT_SPENT_WINDOW_EVENT: &str = "{\n  \"type\": \"error\",\n  \"status_code\": 429,\n  \"error\": {\n    \"type\": \"usage_limit_reached\",\n    \"message\": \"The usage limit has been reached\",\n    \"resets_in_seconds\": 90\n  }\n}";
    const LIMIT_SERVER_ERROR_EVENT: &str = r#"{"type":"error","status_code":500,"error":{"type":"server_error","message":"The server had an error"}}"#;

    #[tokio::test]
    async fn a_refusal_that_crosses_the_limit_ends_the_body_and_drops_the_socket() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = agent_owner("limit-refusal-session", "limit-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }
        let created = encode_sse(LIMIT_CREATED_EVENT);
        let limit = created.len() + 1;

        let mut ws = connect_to_answer(&[LIMIT_CREATED_EVENT, LIMIT_SPENT_WINDOW_EVENT]).await;
        let (body, terminal) = collect_ws_events(
            &mut ws,
            1_000,
            limit,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        .expect("a refusal past the limit is kept for its answer");

        let refusal: serde_json::Value = serde_json::from_str(LIMIT_SPENT_WINDOW_EVENT).unwrap();
        let terminal = terminal.expect("the refusal ends the body");
        assert_eq!(terminal.event_type, "error");
        assert_eq!(terminal.payload, refusal);
        assert_eq!(buffered_answer_status(&terminal), 429);
        assert_eq!(body, [created, encode_sse(&refusal.to_string())].concat());
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    #[tokio::test]
    async fn a_failure_past_the_limit_that_names_no_refusal_still_fails() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = agent_owner("limit-failure-session", "limit-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }
        let limit = encode_sse(LIMIT_CREATED_EVENT).len() + 1;

        let mut ws = connect_to_answer(&[LIMIT_CREATED_EVENT, LIMIT_SERVER_ERROR_EVENT]).await;
        let err = match collect_ws_events(
            &mut ws,
            1_000,
            limit,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        {
            Ok(_) => panic!("a failure that names no refusal must not be kept"),
            Err(err) => err,
        };

        assert_eq!(err.status, 502);
        assert_eq!(
            err.detail.as_deref(),
            Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL)
        );
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    #[tokio::test]
    async fn a_buffered_body_one_byte_over_the_limit_fails_and_drops_the_socket() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = agent_owner("limit-session", "limit-agent");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }
        let limit =
            encode_sse(LIMIT_CREATED_EVENT).len() + encode_sse(LIMIT_COMPLETED_EVENT).len() - 1;

        let mut ws = connect_to_two_event_answer().await;
        let err = match collect_ws_events(
            &mut ws,
            1_000,
            limit,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        {
            Ok(_) => panic!("a body over the limit must fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 502);
        assert_eq!(
            err.detail.as_deref(),
            Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL)
        );
        assert!(!super::super::client::is_continuation_retry_error(&err));
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    #[tokio::test]
    async fn a_buffered_body_exactly_at_the_limit_is_collected() {
        let created = encode_sse(LIMIT_CREATED_EVENT);
        let completed = encode_sse(LIMIT_COMPLETED_EVENT);
        let limit = created.len() + completed.len();

        let mut ws = connect_to_two_event_answer().await;
        let (body, terminal) =
            collect_ws_events(&mut ws, 1_000, limit, None, None, None, "test-request")
                .await
                .expect("a body at the limit must be collected");

        assert_eq!(body, [created, completed].concat());
        assert!(terminal.is_some());
    }

    #[test]
    fn no_capture_body_is_made_without_a_traffic_capture() {
        assert!(LiveCaptureBody::for_capture(None, MAX_BUFFERED_RESPONSE_BYTES).is_none());
    }

    #[test]
    fn a_live_capture_event_past_its_room_leaves_the_copy_as_it_was() {
        let created = encode_sse(LIMIT_CREATED_EVENT);
        let completed = encode_sse(LIMIT_COMPLETED_EVENT);
        let mut capture = LiveCaptureBody {
            bytes: Vec::new(),
            limit: created.len() + completed.len(),
            full: false,
        };

        capture.append(LIMIT_CREATED_EVENT);
        capture.append(&"\n".repeat(completed.len()));
        assert!(capture.full);
        assert_eq!(capture.bytes, created);
        capture.append(LIMIT_COMPLETED_EVENT);
        assert_eq!(capture.bytes, created);

        let mut capture = LiveCaptureBody {
            bytes: created.clone(),
            limit: created.len() + completed.len(),
            full: false,
        };
        capture.append(LIMIT_COMPLETED_EVENT);
        assert!(!capture.full);
        assert_eq!(capture.bytes, [created, completed].concat());
    }

    #[tokio::test]
    async fn a_live_capture_over_the_limit_stops_its_copy_and_relays_every_event() {
        let created = encode_sse(LIMIT_CREATED_EVENT);
        let limit = created.len() + encode_sse(LIMIT_COMPLETED_EVENT).len() - 1;
        let temp = tempfile::tempdir().unwrap();
        let traffic = Arc::new(crate::traffic::test_capture(temp.path().join("traffic")));
        let (tx, mut rx) = mpsc::channel(64);

        let mut ws = connect_to_two_event_answer().await;
        let (_, terminal_item) = stream_ws_events(
            &mut ws,
            1_000,
            Some(traffic.clone()),
            &tx,
            "test-request",
            limit,
        )
        .await;
        drop(tx);

        let mut relayed = Vec::new();
        while let Some(item) = rx.recv().await {
            relayed.push(item.expect("a relayed event")["type"].clone());
        }
        assert_eq!(relayed, [serde_json::json!("response.created")]);
        let terminal = terminal_item
            .expect("the terminal event must be returned")
            .expect("the terminal event must not be an error");
        assert_eq!(terminal["type"], "response.completed");

        let body_path = std::fs::read_dir(traffic.root())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .ends_with("032-upstream-response-body.sse")
                })
            })
            .expect("the response body must be captured");
        let captured = std::fs::read(body_path).unwrap();
        assert!(captured.len() <= limit);
        assert_eq!(captured, created);
    }

    #[tokio::test]
    async fn response_start_timeout_ignores_rate_limits_and_pings() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = main_owner("start-timeout-session");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true}}"#.into(),
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = match collect_ws_events(
            &mut ws,
            50,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        {
            Ok(_) => panic!("expected response start timeout"),
            Err(err) => err,
        };

        assert_eq!(
            err.detail.as_deref(),
            Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
        );
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    #[tokio::test]
    async fn response_idle_timeout_ignores_pings_after_response_event() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let pooled_stream = create_dummy_stream_async().await;
        let owner = main_owner("response-idle-session");
        {
            let mut guard = WS_POOL.lock().unwrap();
            guard.insert(owner.clone(), Arc::new(PoolEntry::new(pooled_stream)));
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#
                    .into(),
            ))
            .await
            .unwrap();
            loop {
                if ws.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let err = match collect_ws_events(
            &mut ws,
            50,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&owner),
            None,
            None,
            "test-request",
        )
        .await
        {
            Ok(_) => panic!("expected response idle timeout"),
            Err(err) => err,
        };

        assert!(err.message.contains("idle timeout"));
        assert_eq!(err.detail, None);
        assert!(!WS_POOL.lock().unwrap().contains_key(&owner));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_response_wait_sends_keepalive_ping() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let server_task = tokio::spawn(async move {
            let frame = server.next().await.unwrap().unwrap();
            assert!(matches!(frame, Message::Ping(_)));
            server
                .send(Message::Text(r#"{"type":"response.created"}"#.into()))
                .await
                .unwrap();
        });

        let frame = read_ws_frame_with_keepalive(
            &mut client,
            Duration::from_secs(60),
            WEBSOCKET_KEEPALIVE_INTERVAL,
        )
        .await;

        assert!(matches!(
            frame,
            WebSocketRead::Frame(Some(Ok(Message::Text(text))))
                if text.contains("response.created")
        ));
        server_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_keepalive_write_returns_stable_error() {
        let mut client = WebSocketStream::from_raw_socket(FailingWriteIo, Role::Client, None).await;

        let error = match collect_ws_events_with_keepalive_interval(
            &mut client,
            60_000,
            MAX_BUFFERED_RESPONSE_BYTES,
            None,
            None,
            None,
            "test-request",
            WEBSOCKET_KEEPALIVE_INTERVAL,
        )
        .await
        {
            Ok(_) => panic!("expected keepalive failure"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_KEEPALIVE_FAILURE_DETAIL)
        );
        assert!(error.message.contains("keepalive error"));
    }

    #[tokio::test]
    async fn keepalive_pongs_do_not_extend_response_start_timeout() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let ping_count = Arc::new(AtomicUsize::new(0));
        let server_ping_count = ping_count.clone();
        let server_task = tokio::spawn(async move {
            while let Some(Ok(frame)) = server.next().await {
                if let Message::Ping(payload) = frame {
                    server_ping_count.fetch_add(1, Ordering::SeqCst);
                    if server.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
            }
        });

        let error = match collect_ws_events_with_keepalive_interval(
            &mut client,
            50,
            MAX_BUFFERED_RESPONSE_BYTES,
            None,
            None,
            None,
            "test-request",
            Duration::from_millis(10),
        )
        .await
        {
            Ok(_) => panic!("expected response start timeout"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
        );
        assert!(ping_count.load(Ordering::SeqCst) >= 2);
        server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn request_write_shares_the_idle_budget() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let mut client = WebSocketStream::from_raw_socket(
            StalledWriteIo {
                input: Vec::new(),
                delay: None,
            },
            Role::Client,
            None,
        )
        .await;
        let started = tokio::time::Instant::now();

        let result = tokio::time::timeout(
            Duration::from_secs(600),
            send_request_frame(&mut client, "{}".to_string(), 60_000),
        )
        .await
        .expect("the request write must end within its idle budget");
        let error = match result {
            Ok(()) => panic!("expected the request write to time out"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 60000ms"
        );
        assert!(started.elapsed() >= Duration::from_secs(60));
        assert!(started.elapsed() <= Duration::from_secs(61));
    }

    // The write budget is shorter than the read budget, so a frame the socket
    // buffers absorbed would leave the test waiting on the read and fail it.
    #[tokio::test]
    async fn request_write_to_a_peer_that_stopped_reading_times_out() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _write_timeout = RequestWriteTimeoutOverride::set(Duration::from_millis(200));
        let (addr, origin) = spawn_non_reading_origin().await;
        let context = RequestContext {
            req_id: "stalled-write-request".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let started = Instant::now();

        let result = tokio::time::timeout(
            Duration::from_secs(30),
            codex_websocket_request(
                &test_websocket_client(),
                &WebSocketProxyConfig::direct(),
                &format!("http://{addr}/responses"),
                &HeaderMap::new(),
                &oversized_request_body(),
                &context,
                None,
                1_000,
                60_000,
                MAX_BUFFERED_RESPONSE_BYTES,
                None,
                OnConnectFailure::Fail,
            ),
        )
        .await
        .expect("the request write must end within its budget");
        let error = match result {
            Ok(_) => panic!("expected the request write to time out"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 200ms"
        );
        assert!(matches!(error.origin, CodexErrorOrigin::WebSocket));
        assert!(started.elapsed() < Duration::from_secs(10));
        origin.abort();
    }

    // As above, a frame the socket buffers absorbed would fail the test.
    #[tokio::test]
    async fn live_request_write_to_a_peer_that_stopped_reading_ends_the_socket_task() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _write_timeout = RequestWriteTimeoutOverride::set(Duration::from_millis(200));
        let (addr, origin) = spawn_non_reading_origin().await;
        let headers = HeaderMap::new();
        let body = oversized_request_body();
        let body_json = serde_json::to_string(&body).unwrap();
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &headers,
            None,
            None,
            1_000,
            60_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        let mut events = start_codex_websocket_events(
            ready,
            &body,
            body_json,
            &headers,
            None,
            "test-request".to_string(),
            MAX_BUFFERED_RESPONSE_BYTES,
        );

        let error = match tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .expect("the request write must end within its budget")
        {
            Some(Err(error)) => error,
            _ => panic!("expected the request write to time out"),
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 200ms"
        );
        assert!(matches!(error.origin, CodexErrorOrigin::WebSocket));
        // The socket task ended with the error, which closes the channel.
        let next = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the socket task must end after the write timeout");
        assert!(next.is_none());
        origin.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn pong_to_a_peer_that_stopped_reading_ends_the_response_wait() {
        let mut client = WebSocketStream::from_raw_socket(
            // The Ping arrives 50 s into the 60 s budget, so the Pong gets
            // the 10 s left, not a budget of its own.
            StalledWriteIo {
                input: EMPTY_SERVER_PING.to_vec(),
                delay: Some(Box::pin(tokio::time::sleep(Duration::from_secs(50)))),
            },
            Role::Client,
            None,
        )
        .await;
        let started = tokio::time::Instant::now();

        let result = tokio::time::timeout(
            Duration::from_secs(600),
            collect_ws_events_with_keepalive_interval(
                &mut client,
                60_000,
                MAX_BUFFERED_RESPONSE_BYTES,
                None,
                None,
                None,
                "test-request",
                WEBSOCKET_KEEPALIVE_INTERVAL,
            ),
        )
        .await
        .expect("the Pong write must end within the read deadline");
        let error = match result {
            Ok(_) => panic!("expected the response wait to time out"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 60000ms"
        );
        assert!(started.elapsed() >= Duration::from_secs(60));
        assert!(started.elapsed() <= Duration::from_secs(61));
    }

    #[tokio::test(start_paused = true)]
    async fn live_pong_to_a_peer_that_stopped_reading_ends_the_stream() {
        let mut client = WebSocketStream::from_raw_socket(
            // The Ping arrives 50 s into the 60 s budget, so the Pong gets
            // the 10 s left, not a budget of its own.
            StalledWriteIo {
                input: EMPTY_SERVER_PING.to_vec(),
                delay: Some(Box::pin(tokio::time::sleep(Duration::from_secs(50)))),
            },
            Role::Client,
            None,
        )
        .await;
        let (tx, _rx) = mpsc::channel(64);
        let started = tokio::time::Instant::now();

        let (reusable, terminal_item) = tokio::time::timeout(
            Duration::from_secs(600),
            stream_ws_events(
                &mut client,
                60_000,
                None,
                &tx,
                "test-request",
                MAX_BUFFERED_RESPONSE_BYTES,
            ),
        )
        .await
        .expect("the Pong write must end within the read deadline");
        let error = match terminal_item {
            Some(Err(error)) => error,
            _ => panic!("expected the stream to end with a timeout"),
        };

        assert!(!reusable);
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 60000ms"
        );
        assert!(started.elapsed() >= Duration::from_secs(60));
        assert!(started.elapsed() <= Duration::from_secs(61));
    }

    #[tokio::test(start_paused = true)]
    async fn validation_ping_to_a_peer_that_stopped_reading_fails_validation() {
        let mut client = WebSocketStream::from_raw_socket(
            StalledWriteIo {
                input: Vec::new(),
                delay: None,
            },
            Role::Client,
            None,
        )
        .await;

        let result = tokio::time::timeout(
            Duration::from_secs(600),
            validate_pooled_websocket(&mut client, 15_000),
        )
        .await
        .expect("the validation Ping must end within the validation deadline");

        assert_eq!(result, Err("validation timeout".to_string()));
    }

    async fn create_dummy_stream_async() -> CodexWebSocketStream {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(socket).await;
            futures_util::future::pending::<()>().await;
        });
        let url = format!("ws://{addr}/");
        let client = test_websocket_client();
        connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap()
        .stream
    }

    #[test]
    fn handshake_details_are_structured_sanitized_and_bounded() {
        let message = format!("safe\n{}", "x".repeat(MAX_HANDSHAKE_ERROR_DETAIL_BYTES * 2));
        let body = serde_json::to_vec(&serde_json::json!({
            "error": { "message": message }
        }))
        .unwrap();
        let detail = handshake_error_detail(Some(&body));
        assert!(!detail.contains('\n'));
        assert!(detail.len() <= MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
        assert!(detail.starts_with("safe"));
    }

    #[test]
    fn handshake_details_reject_unstructured_and_binary_bodies() {
        for body in [
            b"<html>denied</html>".to_vec(),
            vec![0xff, 0xfe],
            b"{".to_vec(),
        ] {
            assert_eq!(
                handshake_error_detail(Some(&body)),
                GENERIC_HANDSHAKE_ERROR_DETAIL
            );
        }
    }

    #[test]
    fn a_rejected_upgrade_for_a_missing_model_without_a_message_names_the_code() {
        for error in [
            serde_json::json!({"code": "model_not_found"}),
            serde_json::json!({"code": "model_not_found", "message": "  \t "}),
        ] {
            let body = serde_json::to_vec(&serde_json::json!({ "error": error })).unwrap();
            let err = handshake_rejection_error(404, &HeaderMap::new(), Some(&body), None);
            assert_eq!(
                err.class,
                Some(super::super::events::CodexErrorClass::ModelNotFound)
            );
            assert_eq!(err.detail.as_deref(), Some("model_not_found"), "{error}");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rejected_upgrade_naming_a_terminal_code_without_a_message_gets_the_cli_text() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        // Status line, body, then the client's status, error type and message.
        let cases = [
            (
                "400 Bad Request",
                r#"{"error":{"code":"content_policy_violation"}}"#,
                400,
                "invalid_request_error",
                "The input was rejected by the content policy.",
            ),
            (
                "400 Bad Request",
                r#"{"error":{"code":"content_policy_violation","message":"toy message"}}"#,
                400,
                "invalid_request_error",
                "toy message",
            ),
            (
                "400 Bad Request",
                r#"{"error":{"code":"budget_reached","message":""}}"#,
                429,
                "rate_limit_error",
                "Rate limited",
            ),
            (
                "403 Forbidden",
                r#"{"error":{"code":"inference_hook_denied","message":""}}"#,
                403,
                "permission_error",
                "Your workspace inference hook blocked this request.",
            ),
        ];
        for (status_line, body, status, error_type, message) in cases {
            let raw = raw_rejection(status_line, &[], body);
            let direct = direct_handshake_rejection_once(raw.clone()).await;
            let tunneled = tunneled_handshake_rejection(raw).await;
            for (path, err) in [("direct", direct), ("tunneled", tunneled)] {
                let case = format!("{path} {status_line} {body}");
                let response = super::super::map_codex_error_to_response(&err);
                assert_eq!(response.status().as_u16(), status, "{case}");
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(answer["error"]["type"], error_type, "{case}");
                assert_eq!(answer["error"]["message"], message, "{case}");
            }
        }
        super::super::rate_limits::clear_for_tests();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rejected_upgrade_keeps_a_message_that_reads_like_the_generic_detail() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        let body = serde_json::json!({
            "error": {
                "code": "content_policy_violation",
                "message": GENERIC_HANDSHAKE_ERROR_DETAIL,
            }
        })
        .to_string();
        let err =
            direct_handshake_rejection_once(raw_rejection("400 Bad Request", &[], &body)).await;
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        // The kept message names the transport, so the client gets the generic
        // text of its error type rather than the content policy text.
        let response = super::super::map_codex_error_to_response(&err);
        assert_eq!(response.status().as_u16(), 400);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(answer["error"]["type"], "invalid_request_error");
        assert_eq!(answer["error"]["message"], "Invalid request");
        super::super::rate_limits::clear_for_tests();
    }

    #[test]
    fn a_rejected_upgrade_reads_the_delay_its_body_names_before_the_header() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {"code": "policy_capacity_unavailable", "message": "busy", "retry_after": 2.5}
        }))
        .unwrap();
        let err = handshake_rejection_error(503, &HeaderMap::new(), Some(&body), None);
        assert_eq!(
            err.class,
            Some(super::super::events::CodexErrorClass::Overloaded)
        );
        assert_eq!(err.retry_after.as_deref(), Some("3"));

        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "9".parse().unwrap());
        let err = handshake_rejection_error(503, &headers, Some(&body), None);
        assert_eq!(err.retry_after.as_deref(), Some("3"));
        let without_delay = serde_json::to_vec(&serde_json::json!({
            "error": {"code": "policy_capacity_unavailable", "message": "busy"}
        }))
        .unwrap();
        let err = handshake_rejection_error(503, &headers, Some(&without_delay), None);
        assert_eq!(err.retry_after.as_deref(), Some("9"));
    }

    /// A loopback WebSocket URL nothing listens on, so a connect is refused.
    async fn refused_websocket_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("ws://{addr}/responses")
    }

    async fn connect_error(url: &str, on_failure: OnConnectFailure<'_>) -> CodexError {
        match connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            url,
            &HeaderMap::new(),
            1_000,
            on_failure,
        )
        .await
        {
            Ok(_) => panic!("the connect to {url} must fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn the_connect_pause_is_keyed_by_scheme_host_and_port() {
        assert_eq!(
            connect_pause_key("http://127.0.0.1:1/backend-api/codex/responses"),
            connect_pause_key("ws://127.0.0.1:1/other")
        );
        assert_ne!(
            connect_pause_key("ws://127.0.0.1:1/responses"),
            connect_pause_key("ws://127.0.0.1:2/responses")
        );
        assert_ne!(
            connect_pause_key("ws://127.0.0.1:1/responses"),
            connect_pause_key("wss://127.0.0.1:1/responses")
        );
    }

    #[tokio::test]
    async fn an_unanswered_connect_pauses_its_target_for_the_http_fallback() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let url = refused_websocket_url().await;
        let fall_back = OnConnectFailure::FallBackToHttp {
            req_id: "connect-pause-start",
        };

        let refused = connect_error(&url, fall_back).await;
        assert_eq!(refused.status, 0);
        assert_ne!(
            refused.detail.as_deref(),
            Some(WEBSOCKET_CONNECT_PAUSED_DETAIL)
        );

        let paused = connect_error(&url, fall_back).await;
        assert_eq!(paused.status, 0);
        assert_eq!(paused.origin, CodexErrorOrigin::WebSocketHandshake);
        assert_eq!(
            paused.detail.as_deref(),
            Some(WEBSOCKET_CONNECT_PAUSED_DETAIL)
        );

        // A caller that does not fall back to HTTP still tries.
        let attempted = connect_error(&url, OnConnectFailure::Fail).await;
        assert_ne!(
            attempted.detail.as_deref(),
            Some(WEBSOCKET_CONNECT_PAUSED_DETAIL)
        );

        // Another target is not paused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _websocket = tokio_tungstenite::accept_async(socket).await;
            futures_util::future::pending::<()>().await;
        });
        connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("ws://{other}/responses"),
            &HeaderMap::new(),
            1_000,
            fall_back,
        )
        .await
        .unwrap();

        // Once the pause ended the next request tries again, and another
        // unanswered connect starts a new pause.
        expire_codex_websocket_connect_pause_for_tests(&url);
        let retried = connect_error(&url, fall_back).await;
        assert_ne!(
            retried.detail.as_deref(),
            Some(WEBSOCKET_CONNECT_PAUSED_DETAIL)
        );
        assert!(connect_paused(&url));
        expire_codex_websocket_connect_pause_for_tests(&url);
    }

    #[tokio::test]
    async fn an_unanswered_connect_without_the_http_fallback_starts_no_pause() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let url = refused_websocket_url().await;

        let refused = connect_error(&url, OnConnectFailure::Fail).await;
        assert_eq!(refused.status, 0);
        assert!(!connect_paused(&url));
    }

    #[tokio::test]
    async fn a_rejected_upgrade_starts_no_connect_pause() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let counted = upgrades.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buf[..read]),
                    }
                }
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let url = format!("ws://{addr}/responses");
        let fall_back = OnConnectFailure::FallBackToHttp {
            req_id: "rejected-upgrade",
        };

        for attempt in 1..=2 {
            let error = connect_error(&url, fall_back).await;
            assert_eq!(error.status, 503);
            assert_eq!(upgrades.load(Ordering::SeqCst), attempt);
        }
        assert!(!connect_paused(&url));
    }

    #[tokio::test]
    async fn a_rejection_whose_body_runs_out_of_time_starts_no_connect_pause() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => request.extend_from_slice(&buf[..read]),
                }
            }
            // The head arrives, the body it declares never does.
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 64\r\n\r\n")
                .await;
            futures_util::future::pending::<()>().await;
        });
        let url = format!("ws://{addr}/responses");

        let error = match connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            300,
            OnConnectFailure::FallBackToHttp {
                req_id: "rejection-body-timeout",
            },
        )
        .await
        {
            Ok(_) => panic!("the connect to {url} must fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.message,
            connect_timeout_error(Duration::from_millis(300)).message
        );
        assert!(!connect_paused(&url));
    }

    #[tokio::test]
    async fn a_proxy_status_line_that_runs_out_of_time_starts_no_connect_pause() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = proxy.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            while connect_response_header_end(&request).is_none() {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => request.extend_from_slice(&buf[..read]),
                }
            }
            // The status line arrives, the rest of the head never does.
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\n")
                .await;
            futures_util::future::pending::<()>().await;
        });
        let proxy_url = format!("http://{proxy_addr}");
        let url = "wss://proxied-pause.invalid/backend-api/codex/responses";

        let error = match connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::new(None, Some(&proxy_url), None, None),
            url,
            &HeaderMap::new(),
            300,
            OnConnectFailure::FallBackToHttp {
                req_id: "proxy-status-timeout",
            },
        )
        .await
        {
            Ok(_) => panic!("the connect to {url} must fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.message,
            connect_timeout_error(Duration::from_millis(300)).message
        );
        assert!(!connect_paused(url));
    }

    #[tokio::test]
    async fn a_target_paused_while_its_request_waited_at_the_gate_is_skipped() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counted = accepted.clone();
        tokio::spawn(async move {
            while let Ok((_socket, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        let url = format!("ws://{addr}/responses");

        // The request goes through a gate of its own, so no other test's
        // connect passes or stamps it.
        let gate = Arc::new(WebSocketConnectGate::new(WEBSOCKET_CONNECT_START_SPACING));
        let held = gate.last_start.lock().await;
        let waiting = tokio::spawn(TEST_CONNECT_GATE.scope(gate.clone(), {
            let url = url.clone();
            async move {
                connect_error(
                    &url,
                    OnConnectFailure::FallBackToHttp {
                        req_id: "paused-at-the-gate",
                    },
                )
                .await
            }
        }));
        // The request passed the first check and waits at the gate, which
        // this test holds.
        tokio::time::timeout(Duration::from_secs(10), async {
            while gate.arrivals.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request never reached the gate");
        pause_codex_websocket_connects_for_tests(&url);
        drop(held);

        let error = waiting.await.unwrap();
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_CONNECT_PAUSED_DETAIL)
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
        assert_eq!(gate.arrivals.load(Ordering::SeqCst), 1);
        assert_eq!(*gate.last_start.lock().await, None);
        expire_codex_websocket_connect_pause_for_tests(&url);
    }

    #[tokio::test]
    async fn a_successful_connect_ends_the_pause_of_its_target() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/responses");
        let paused_url = url.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // A pause that starts while the connect is under way, as
                // another request's unanswered connect would start it.
                pause_codex_websocket_connects_for_tests(&paused_url);
                tokio::spawn(async move {
                    let _websocket = tokio_tungstenite::accept_async(socket).await;
                    futures_util::future::pending::<()>().await;
                });
            }
        });

        for on_failure in [
            OnConnectFailure::Fail,
            OnConnectFailure::FallBackToHttp {
                req_id: "connect-ends-pause",
            },
        ] {
            connect_with_timeout(
                &test_websocket_client(),
                &WebSocketProxyConfig::direct(),
                &url,
                &HeaderMap::new(),
                1_000,
                on_failure,
            )
            .await
            .unwrap();
            assert!(!connect_paused(&url));
        }
    }

    #[test]
    fn an_ended_connect_pause_leaves_the_map_when_it_is_next_read() {
        let ended = "ws://ended-pause.invalid/responses";
        pause_codex_websocket_connects_for_tests(ended);
        expire_codex_websocket_connect_pause_for_tests(ended);

        assert!(!connect_paused("ws://another-target.invalid/responses"));
        assert!(!lock_connect_pauses().contains_key(&connect_pause_key(ended)));
    }

    #[test]
    fn a_second_unanswered_connect_leaves_a_running_pause_as_it_is() {
        let url = "ws://running-pause.invalid/responses";
        let error = connect_timeout_error(Duration::from_millis(1));
        start_connect_pause(url, "first-unanswered", &error);
        let ends_at = lock_connect_pauses().get(&connect_pause_key(url)).copied();
        assert!(ends_at.is_some());

        std::thread::sleep(Duration::from_millis(5));
        start_connect_pause(url, "second-unanswered", &error);
        let still_ends_at = lock_connect_pauses().get(&connect_pause_key(url)).copied();
        assert_eq!(still_ends_at, ends_at);
        expire_codex_websocket_connect_pause_for_tests(url);
    }

    #[test]
    fn the_connect_pause_map_is_usable_after_a_holder_panicked() {
        let url = "ws://poisoned-pause.invalid/responses";
        let _ = std::thread::spawn(|| {
            let _pauses = WS_CONNECT_PAUSES.lock().unwrap();
            panic!("poison the connect pause map");
        })
        .join();

        pause_codex_websocket_connects_for_tests(url);
        assert!(connect_paused(url));
        expire_codex_websocket_connect_pause_for_tests(url);
        assert!(!connect_paused(url));
        WS_CONNECT_PAUSES.clear_poison();
    }

    #[tokio::test]
    async fn a_pooled_socket_still_serves_its_conversation_while_its_target_is_paused() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = main_owner("paused-target-pooled-session");
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        invalidate_codex_websocket_pool_owner(&owner);

        // An origin that counts its upgrades and completes every request.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let counted = upgrades.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut websocket) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    while let Some(Ok(message)) = websocket.next().await {
                        if matches!(message, Message::Text(_)) {
                            let completed = serde_json::json!({
                                "type": "response.completed",
                                "response": {"id": "resp_pooled"},
                            })
                            .to_string();
                            if websocket.send(Message::Text(completed)).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        let url = format!("http://{addr}/responses");
        let connected = connect_with_timeout(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/responses"),
            &HeaderMap::new(),
            1_000,
            OnConnectFailure::Fail,
        )
        .await
        .unwrap();
        let entry = Arc::new(PoolEntry::opened_with(connected, &HeaderMap::new()));
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &continuation_request(),
            true,
        );
        let continuation = test_continuation(
            Some(owner.clone()),
            reserved.turn_id(),
            Some("resp_previous"),
            Some(entry.socket_id),
        );
        pause_codex_websocket_connects_for_tests(&url);
        let context = RequestContext {
            req_id: "paused-target-pooled-request".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let fall_back = OnConnectFailure::FallBackToHttp {
            req_id: &context.req_id,
        };

        pool_insert(owner.clone(), entry.clone());
        let buffered = codex_websocket_request(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            &serde_json::json!({"type": "response.create"}),
            &context,
            None,
            1_000,
            2_000,
            MAX_BUFFERED_RESPONSE_BYTES,
            Some(&continuation),
            fall_back,
        )
        .await;
        assert!(buffered.is_ok(), "the pooled socket must carry the request");

        pool_insert(owner.clone(), entry.clone());
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &url,
            &HeaderMap::new(),
            None,
            Some(&continuation),
            1_000,
            2_000,
            fall_back,
        )
        .await
        .unwrap();
        assert!(ready.used_pooled);
        assert_eq!(upgrades.load(Ordering::SeqCst), 1);

        drop(ready);
        expire_codex_websocket_connect_pause_for_tests(&url);
        invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
    }
}
