use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::sse::parse_sse_events;
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;
use crate::retry::should_retry_status;
use crate::traffic::TrafficCapture;

use super::auth::constants::{CODEX_API_ENDPOINT, ORIGINATOR, RESPONSES_LITE_ORIGINATOR};
use super::auth::manager::{CodexAuthManager, TransientRefreshError};
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
pub use super::events::{CodexErrorClass, CodexLimitWindow, CodexUsageLimit};
use super::search::{SearchRequest, SearchResponse};
use super::translate::request::ResponsesRequest;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    pub usage_limit: Option<Box<CodexUsageLimit>>,
    /// What the backend's error code says about the failure, when it names a
    /// code the proxy knows. It decides the client's answer over the status.
    pub class: Option<CodexErrorClass>,
    pub origin: CodexErrorOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorOrigin {
    Http,
    WebSocket,
    WebSocketHandshake,
    Auth,
    BufferedHttp,
    BufferedWebSocket,
}

impl CodexError {
    pub fn new(status: u16, message: String) -> Self {
        Self {
            status,
            message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        }
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex error {}: {}", self.status, self.message)
    }
}

#[derive(Debug)]
pub struct CodexHeaderTimeoutError {
    pub timeout_ms: u64,
}

impl std::fmt::Display for CodexHeaderTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Timed out waiting {}ms for Codex response headers",
            self.timeout_ms
        )
    }
}

#[derive(Debug)]
pub struct CodexTransportError {
    pub message: String,
}

impl std::fmt::Display for CodexTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex transport error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Header builder
// ---------------------------------------------------------------------------

fn default_user_agent(use_responses_lite: bool) -> String {
    if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
    }
}

/// A `prompt_cache_key` fit to repeat in a header. The Anthropic route derives
/// the key from the conversation, but `/v1/responses` relays whatever the
/// client sent, which may be empty or carry bytes no header accepts; such a key
/// still rides in the body while the headers fall back to the session.
fn is_routing_id(scope: &str) -> bool {
    !scope.is_empty() && scope.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

/// `cache_scope` is the request's `prompt_cache_key`: the conversation the
/// prompt cache belongs to. It also drives the routing headers below, so a
/// subagent keeps its own cache affinity instead of sharing the main thread's.
/// Callers without a translated body pass `None` and fall back to the session.
pub fn build_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
    cache_scope: Option<&str>,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        header_value("content-type", "application/json")?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "text/event-stream")?,
    );
    let bearer = format!("Bearer {}", auth.access);
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &bearer)?,
    );
    let originator = if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        config::codex_originator(ORIGINATOR)
    };
    headers.insert("originator", header_value("originator", &originator)?);
    headers.insert(
        "openai-beta",
        header_value("openai-beta", "responses=experimental")?,
    );
    headers.insert(
        "x-codex-beta-features",
        header_value("x-codex-beta-features", "remote_compaction_v2")?,
    );
    if use_responses_lite {
        headers.insert(
            "x-openai-internal-codex-responses-lite",
            header_value("x-openai-internal-codex-responses-lite", "true")?,
        );
    }
    if let Some(ref account_id) = auth.account_id {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(session_id) = cache_scope
        .filter(|scope| is_routing_id(scope))
        .or(ctx.session_id.as_deref())
    {
        // Spelled as the Codex CLI spells them; both carry the conversation's
        // routing id.
        headers.insert("session-id", header_value("session-id", session_id)?);
        headers.insert("thread-id", header_value("thread-id", session_id)?);
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
        let window_id = format!("{session_id}:0");
        headers.insert(
            "x-codex-window-id",
            header_value("x-codex-window-id", &window_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(use_responses_lite));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    if let Some(passthrough) = ctx.passthrough.as_ref() {
        forward_client_headers(
            &mut headers,
            &passthrough.headers,
            &config::codex_forward_headers(),
        );
    }
    Ok(headers)
}

/// Copies the client headers named in `names` (`CCP_CODEX_FORWARD_HEADERS`)
/// onto a request for the Codex backend, such as a token a gateway in front
/// of it checks. A header the proxy set itself is never replaced, so the
/// client's own `authorization` cannot stand in for the Codex login, and a
/// header that describes the client's connection to the proxy is never
/// copied. The copies are marked sensitive, which keeps their values out of
/// traffic captures.
pub(crate) fn forward_client_headers(
    headers: &mut http::HeaderMap,
    client: &http::HeaderMap,
    names: &[http::HeaderName],
) {
    for name in names {
        if headers.contains_key(name) || is_connection_header(name) {
            continue;
        }
        for value in client.get_all(name) {
            let mut value = value.clone();
            value.set_sensitive(true);
            headers.append(name.clone(), value);
        }
    }
}

fn is_connection_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "accept-encoding"
    )
}

pub fn build_native_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
    stream: bool,
    cache_scope: Option<&str>,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = build_codex_headers(auth, ctx, use_responses_lite, cache_scope)?;
    headers.insert(
        http::header::ACCEPT,
        header_value(
            "accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )?,
    );
    Ok(headers)
}

pub fn build_codex_search_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = build_codex_headers(auth, ctx, false, None)?;
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    let originator = config::codex_originator(RESPONSES_LITE_ORIGINATOR);
    headers.insert("originator", header_value("originator", &originator)?);
    let user_agent = config::codex_user_agent(RESPONSES_LITE_ORIGINATOR);
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

pub fn build_codex_image_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        header_value("content-type", "application/json")?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &format!("Bearer {}", auth.access))?,
    );
    headers.insert(
        "originator",
        header_value("originator", &config::codex_originator(ORIGINATOR))?,
    );
    if let Some(account_id) = auth.account_id.as_deref() {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(session_id) = ctx.session_id.as_deref() {
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(false));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

pub fn build_codex_transcription_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &format!("Bearer {}", auth.access))?,
    );
    headers.insert("originator", header_value("originator", "Codex Desktop")?);
    if let Some(account_id) = auth.account_id.as_deref() {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(session_id) = ctx.session_id.as_deref() {
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(false));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

fn header_value(name: &str, value: &str) -> Result<http::HeaderValue, CodexError> {
    http::HeaderValue::from_str(value).map_err(|e| CodexError {
        status: 500,
        message: format!("Failed to parse {name} header"),
        detail: Some(e.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::Http,
    })
}

/// The largest search answer read; a longer one fails as a body that could not
/// be read does.
const MAX_SEARCH_BODY_BYTES: usize = 8 * 1024 * 1024;

fn search_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    match base_url.strip_suffix("/responses") {
        Some(api_root) => format!("{api_root}/alpha/search"),
        None => format!("{base_url}/alpha/search"),
    }
}

// ---------------------------------------------------------------------------
// WebSocket request shaping
// ---------------------------------------------------------------------------

pub fn build_websocket_request(
    body: &ResponsesRequest,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> serde_json::Value {
    let mut payload = serde_json::to_value(body).unwrap_or_default();
    let obj = payload.as_object_mut().expect("request must be an object");

    // Omit the stream field for WebSocket transport
    obj.remove("stream");
    obj.insert("type".to_string(), serde_json::json!("response.create"));

    // Apply continuation if available
    if let Some(candidate) = continuation {
        if let Some(ref prev_id) = candidate.previous_response_id {
            obj.insert(
                "previous_response_id".to_string(),
                serde_json::json!(prev_id),
            );
        }
        if let Some(ref delta) = candidate.input_delta {
            obj.insert(
                "input".to_string(),
                serde_json::to_value(delta).unwrap_or_default(),
            );
        }
    }

    payload
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualTransport {
    Http,
    WebSocket,
}

pub struct CodexResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub transport: ActualTransport,
}

pub type CodexHttpEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;

pub(super) const MAX_HTTP_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct HttpSseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

struct DecodedHttpSseEvent {
    event: Option<String>,
    payload: Option<serde_json::Value>,
    /// The length of the frame's joined `data` lines.
    data_bytes: usize,
}

struct HttpEventStreamState {
    resp: reqwest::Response,
    response_headers: Vec<(String, String)>,
    started_at: Instant,
    account_id: Option<String>,
}

impl HttpSseDecoder {
    fn push(&mut self, input: &[u8]) -> Result<Vec<DecodedHttpSseEvent>, CodexError> {
        let mut events = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(events)
    }

    fn finish(&self) -> Result<(), CodexError> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            Err(http_sse_error(
                "Codex SSE stream ended with an incomplete frame",
            ))
        }
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), CodexError> {
        if self.frame.len() >= MAX_HTTP_SSE_FRAME_BYTES {
            return Err(http_sse_error("Codex SSE frame exceeds the size limit"));
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<DecodedHttpSseEvent>) -> Result<(), CodexError> {
        if self.frame.len() == self.line_start {
            if !self.frame.is_empty()
                && let Some(event) = decode_http_sse_frame(&self.frame)?
            {
                events.push(event);
            }
            self.frame.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.push_byte(b'\n')?;
        self.line_start = self.frame.len();
        Ok(())
    }
}

fn decode_http_sse_frame(frame: &[u8]) -> Result<Option<DecodedHttpSseEvent>, CodexError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid UTF-8"))?;
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let data = data.join("\n");
    if data == "[DONE]" {
        return Ok(Some(DecodedHttpSseEvent {
            event,
            payload: None,
            data_bytes: data.len(),
        }));
    }
    let payload = serde_json::from_str(&data)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid JSON"))?;
    Ok(Some(DecodedHttpSseEvent {
        event,
        payload: Some(payload),
        data_bytes: data.len(),
    }))
}

fn http_sse_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some("http_response_sse".to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::Http,
    }
}

pub(crate) struct OwnerAwareCodexResponse {
    response: CodexResponse,
    pub(crate) socket_id: Option<u64>,
}

impl OwnerAwareCodexResponse {
    pub(crate) fn new(response: CodexResponse, socket_id: Option<u64>) -> Self {
        Self {
            response,
            socket_id,
        }
    }

    fn into_response(self) -> CodexResponse {
        self.response
    }
}

impl std::ops::Deref for OwnerAwareCodexResponse {
    type Target = CodexResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

const HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS: u64 = 300_000;
const IMAGE_HEADER_TIMEOUT_MS: u64 = 300_000;
/// How much of a non-success Codex body is read to classify it, over HTTP and
/// on a rejected WebSocket upgrade alike; the rest of the body is not read.
pub(super) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// The most of a buffered Codex answer's SSE body held in memory, over HTTP
/// and WebSocket alike. A live stream is relayed as it arrives; the copies of
/// its events kept beside the relay, the one its continuation state is read
/// from and the one a traffic capture writes, stop at the same size.
pub(super) const MAX_BUFFERED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Marks a buffered answer whose body went past `MAX_BUFFERED_RESPONSE_BYTES`.
/// The request is not sent again, refreshed or replayed over HTTP.
pub(super) const BUFFERED_RESPONSE_TOO_LARGE_DETAIL: &str =
    "Response body exceeded the size limit for a buffered answer";

/// A buffered answer that grew past its limit, on the transport that read it.
pub(super) fn buffered_response_too_large_error(origin: CodexErrorOrigin) -> CodexError {
    CodexError {
        status: 502,
        message: "Codex response body exceeded the size limit for a buffered answer".to_string(),
        detail: Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin,
    }
}

/// Whether the part of a buffered answer held when it crossed its limit
/// names a refusal that `post_codex_with_transport` answers as such from the
/// body: a spent window (an explicit `usage_limit_reached` or a terminal
/// `codex.rate_limits` snapshot), or a failure with an error class. Such an
/// answer goes on to those readers instead of the size error, so it is
/// answered as without the limit. Keep this in step with the readers there.
pub(super) fn capped_body_names_refusal(
    body: &[u8],
    status: u16,
    headers: &[(String, String)],
) -> bool {
    if super::events::usage_limit_from_response(body, headers).is_some() {
        return true;
    }
    if (200..300).contains(&status) {
        super::events::first_reportable_failure(body).is_some_and(|failure| failure.class.is_some())
    } else {
        super::events::error_class_from_body(body, status).is_some()
    }
}

#[derive(Clone)]
struct ProxyEnvironment {
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<reqwest::NoProxy>,
    no_proxy_value: Option<String>,
}

impl ProxyEnvironment {
    fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// The proxy settings from the variables `var` returns. For each proxy URL
    /// the uppercase spelling, when set, is the one consulted, else the
    /// lowercase one, and a value that is not valid Unicode is an invalid
    /// proxy URL: skipping it would connect directly around the proxy. For
    /// `NO_PROXY` a spelling that is not valid Unicode counts as unset, so the
    /// other spelling, if any, applies.
    fn from_lookup(var: impl Fn(&str) -> Option<OsString>) -> Self {
        if var("REQUEST_METHOD").is_some() {
            return Self {
                http_proxy: None,
                https_proxy: None,
                all_proxy: None,
                no_proxy: None,
                no_proxy_value: None,
            };
        }

        let no_proxy_value = unicode_env_value(&var, "NO_PROXY", "no_proxy");
        Self {
            http_proxy: proxy_env_value(&var, "HTTP_PROXY", "http_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            https_proxy: proxy_env_value(&var, "HTTPS_PROXY", "https_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            all_proxy: proxy_env_value(&var, "ALL_PROXY", "all_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            no_proxy: no_proxy_value
                .as_deref()
                .and_then(reqwest::NoProxy::from_string),
            no_proxy_value,
        }
    }

    fn websocket_proxy_config(&self) -> super::websocket::WebSocketProxyConfig {
        super::websocket::WebSocketProxyConfig::new(
            self.http_proxy.as_deref(),
            self.https_proxy.as_deref(),
            self.all_proxy.as_deref(),
            self.no_proxy_value.as_deref(),
        )
    }

    fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder = builder.no_proxy();
        if let Some(proxy) = self.http_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::http(proxy)
                    .expect("validated HTTP_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.https_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::https(proxy)
                    .expect("validated HTTPS_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.all_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .expect("validated ALL_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        builder
    }
}

fn native_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    proxy_environment
        .apply(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none()),
        )
        .build()
        .expect("failed to create native Responses HTTP client")
}

/// A redirect is not followed: on a cross-host redirect reqwest drops only
/// its own list of credentials, so a header forwarded from the client
/// (`CCP_CODEX_FORWARD_HEADERS`) would reach the other host.
fn codex_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    proxy_environment
        .apply(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none()),
        )
        .build()
        .expect("failed to create HTTP client")
}

/// The value of `uppercase`, else of `lowercase`, skipping a spelling that is
/// unset or not valid Unicode.
fn unicode_env_value(
    var: &impl Fn(&str) -> Option<OsString>,
    uppercase: &str,
    lowercase: &str,
) -> Option<String> {
    [uppercase, lowercase]
        .into_iter()
        .find_map(|name| var(name)?.into_string().ok())
}

fn proxy_env_value(
    var: &impl Fn(&str) -> Option<OsString>,
    uppercase: &'static str,
    lowercase: &'static str,
) -> Result<Option<String>, &'static str> {
    let Some(raw) = var(uppercase).or_else(|| var(lowercase)) else {
        return Ok(None);
    };
    let raw = raw.into_string().map_err(|_| uppercase)?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    normalize_proxy_url(raw).map(Some).ok_or(uppercase)
}

fn normalize_proxy_url(raw: &str) -> Option<String> {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
        || matches!(parsed.scheme(), "socks4" | "socks4a")
            && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        return None;
    }
    Some(parsed.to_string())
}

fn websocket_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    let tls_config = super::websocket::websocket_tls_config();
    proxy_environment
        .apply(
            reqwest::Client::builder()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .use_preconfigured_tls((*tls_config).clone()),
        )
        .build()
        .expect("failed to create Codex WebSocket HTTP client")
}

fn custom_client_auto_http_fallback_enabled(
    base_url: &str,
    proxy_config: &super::websocket::WebSocketProxyConfig,
) -> bool {
    let Ok(websocket_url) = super::websocket::to_websocket_url(base_url) else {
        return false;
    };
    !proxy_config.uses_proxy_for(&websocket_url)
}

#[cfg(test)]
fn test_native_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("failed to create test native Responses HTTP client")
}

#[cfg(test)]
fn test_websocket_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("failed to create test WebSocket HTTP client")
}

pub struct CodexHttpClient {
    client: reqwest::Client,
    native_client: reqwest::Client,
    websocket_client: reqwest::Client,
    websocket_proxy_config: super::websocket::WebSocketProxyConfig,
    auto_http_fallback_enabled: bool,
    auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    base_url: String,
    header_timeout_ms: u64,
    body_idle_timeout_ms: u64,
    buffered_response_limit: usize,
    #[allow(dead_code)]
    header_timeout_retries: u32,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHttpClient {
    pub fn new() -> Self {
        let timeout_ms = 60_000;
        let proxy_environment = ProxyEnvironment::from_env();
        Self {
            client: codex_http_client(&proxy_environment),
            native_client: native_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config: proxy_environment.websocket_proxy_config(),
            auto_http_fallback_enabled: true,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
            header_timeout_retries: 1,
        }
    }

    pub fn new_with_client(
        client: reqwest::Client,
        auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
        base_url: String,
    ) -> Self {
        let proxy_environment = ProxyEnvironment::from_env();
        let websocket_proxy_config = proxy_environment.websocket_proxy_config();
        let auto_http_fallback_enabled =
            custom_client_auto_http_fallback_enabled(&base_url, &websocket_proxy_config);
        Self {
            native_client: native_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config,
            auto_http_fallback_enabled,
            client,
            auth_manager,
            base_url,
            header_timeout_ms: 60_000,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
            header_timeout_retries: 1,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        client: reqwest::Client,
        base_url: String,
        header_timeout_ms: u64,
        body_idle_timeout_ms: u64,
        header_timeout_retries: u32,
    ) -> Self {
        Self {
            native_client: test_native_http_client(),
            websocket_client: test_websocket_http_client(),
            websocket_proxy_config: super::websocket::WebSocketProxyConfig::direct(),
            auto_http_fallback_enabled: true,
            client,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url,
            header_timeout_ms,
            body_idle_timeout_ms,
            buffered_response_limit: MAX_BUFFERED_RESPONSE_BYTES,
            header_timeout_retries,
        }
    }

    /// The same client with a smaller buffered answer limit, so a test can
    /// reach it without a body of `MAX_BUFFERED_RESPONSE_BYTES`.
    #[cfg(test)]
    pub(super) fn with_buffered_response_limit(mut self, limit: usize) -> Self {
        self.buffered_response_limit = limit;
        self
    }

    /// The most bytes of upstream events this client holds for one answer:
    /// a buffered body, or a live stream's copies of its events.
    pub(super) fn buffered_response_limit(&self) -> usize {
        self.buffered_response_limit
    }

    pub fn auth_manager(&self) -> &CodexAuthManager<DefaultCodexAuthStore> {
        &self.auth_manager
    }

    /// The models this login may use, straight from the backend.
    pub async fn list_models(
        &self,
        client_headers: Option<&http::HeaderMap>,
    ) -> Result<super::models::ModelInventory, super::models::ModelsError> {
        super::models::fetch_models(
            &self.client,
            &self.auth_manager,
            &self.base_url,
            client_headers,
        )
        .await
    }

    pub fn body_idle_timeout_ms(&self) -> u64 {
        self.body_idle_timeout_ms
    }

    pub(crate) async fn post_transcription(
        &self,
        base_url: &str,
        input: &super::transcription::PreparedTranscription,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let url = format!("{}/transcribe", base_url.trim_end_matches('/'));
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|error| auth_error(error, "Auth error"))?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_transcription_headers(&auth, ctx)?;
            let response = self.attempt_transcription(&url, &headers, input).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_transcription(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        input: &super::transcription::PreparedTranscription,
    ) -> Result<reqwest::Response, CodexError> {
        let part = reqwest::multipart::Part::bytes(input.audio.to_vec())
            .file_name(input.filename.clone())
            .mime_str(&input.content_type)
            .map_err(|error| CodexError {
                status: 400,
                message: "Invalid audio content type".to_string(),
                detail: Some(crate::upstream_http::error_text(error)),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(language) = input.language.as_deref() {
            form = form.text("language", language.to_string());
        }
        let mut request = self.native_client.post(url).multipart(form);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex transcription response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!(
                "Codex transcription transport error: {}",
                crate::upstream_http::error_text(error)
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub(crate) async fn post_image_json(
        &self,
        base_url: &str,
        operation: super::images::ImageOperation,
        body: &serde_json::Value,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let body_json = serde_json::to_vec(body).map_err(|error| CodexError {
            status: 500,
            message: "Failed to serialize image request".to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?;
        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            operation.upstream_path()
        );
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|error| auth_error(error, "Auth error"))?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_image_headers(&auth, ctx)?;
            let response = self
                .attempt_image_json(&url, &headers, body_json.clone())
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_image_json(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        body_json: Vec<u8>,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.body(body_json).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex image response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!(
                "Codex image transport error: {}",
                crate::upstream_http::error_text(error)
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub async fn post_native_responses(
        &self,
        body: &serde_json::Value,
        ctx: &RequestContext,
        use_responses_lite: bool,
        stream: bool,
    ) -> Result<reqwest::Response, CodexError> {
        let body_json = serde_json::to_string(body).map_err(|err| CodexError {
            status: 500,
            message: "Failed to serialize native Responses request".to_string(),
            detail: Some(err.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?;
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|err| auth_error(err, "Auth error"))?;
        let mut refresh_attempted = false;

        loop {
            let started_at = Instant::now();
            // This surface relays a client's own Responses request: its
            // `prompt_cache_key` is whatever that client chose and stays in the
            // body, while the routing headers keep the session, which is the
            // shape the backend expects there.
            let headers = build_native_codex_headers(&auth, ctx, use_responses_lite, stream, None)?;
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_codex_http_request_capture(traffic, &self.base_url, &headers, &body_json);
            }

            let response = self
                .attempt_native_responses(&headers, body_json.clone())
                .await?;
            let status = response.status().as_u16();
            if status == 401 && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .auth_manager
                    .force_refresh(&auth.access)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }

            if let Some(traffic) = ctx.traffic.as_deref() {
                write_live_upstream_response_headers(traffic, &response, started_at.elapsed());
            }
            return Ok(response);
        }
    }

    async fn attempt_native_responses(
        &self,
        headers: &http::HeaderMap,
        body_json: String,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(&self.base_url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }

        tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            request.body(body_json).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex response headers",
                self.header_timeout_ms
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|err| CodexError {
            status: 0,
            message: format!(
                "Native Responses transport error: {}",
                crate::upstream_http::error_text(err)
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub async fn post_codex(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<CodexResponse, CodexError> {
        let reservation =
            continuation.map(super::continuation::ContinuationReservation::from_public_candidate);
        self.post_codex_with_transport(
            body,
            ctx,
            reservation.as_ref(),
            crate::config::codex_transport(),
        )
        .await
        .map(OwnerAwareCodexResponse::into_response)
    }

    pub async fn post_search(
        &self,
        body: &SearchRequest,
        ctx: &RequestContext,
    ) -> Result<SearchResponse, CodexError> {
        self.post_search_capped(body, ctx, MAX_SEARCH_BODY_BYTES)
            .await
    }

    async fn post_search_capped(
        &self,
        body: &SearchRequest,
        ctx: &RequestContext,
        body_limit: usize,
    ) -> Result<SearchResponse, CodexError> {
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|e| auth_error(e, "Auth error"))?;
        let body_json = serde_json::to_string(body).map_err(|e| CodexError {
            status: 500,
            message: "Failed to serialize search request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?;
        let mut auth_refresh_attempted = false;

        loop {
            let response = self
                .attempt_post_search(&auth, &body_json, ctx, body_limit)
                .await?;
            if !(200..300).contains(&response.status) {
                let error = codex_status_error(response, auth.account_id.as_deref());
                if !auth_refresh_attempted && unauthorized_needs_refresh(&error) {
                    auth_refresh_attempted = true;
                    auth = self
                        .auth_manager
                        .force_refresh(&auth.access)
                        .await
                        .map_err(auth_refresh_error)?;
                    continue;
                }
                return Err(error);
            }
            return serde_json::from_slice(&response.body).map_err(|e| CodexError {
                status: 502,
                message: "Failed to decode Codex search response".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            });
        }
    }

    pub async fn stream_codex_http_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
    ) -> Result<CodexHttpEventReceiver, CodexError> {
        self.stream_codex_http_events_with_headers(body, ctx, false, None)
            .await
            .map(|(receiver, _, _)| receiver)
    }

    /// The live HTTP stream together with its response headers, which hold
    /// the `X-Codex-*` quota clocks an event may leave out, and the ChatGPT
    /// account the request went out with. `auth_refresh_spent` says the
    /// client request already refreshed the token (a WebSocket attempt that
    /// fell back), so a 401 here is answered without a second refresh.
    /// `size_fallback` holds a WebSocket size refusal this request replays,
    /// which is not sent under another account.
    async fn stream_codex_http_events_with_headers(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        auth_refresh_spent: bool,
        mut size_fallback: Option<SizeRefusalFallback>,
    ) -> Result<
        (
            CodexHttpEventReceiver,
            Vec<(String, String)>,
            Option<String>,
        ),
        CodexError,
    > {
        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|e| auth_error(e, "Auth error"))?;
        refuse_size_fallback_under_another_account(&mut size_fallback, &auth, ctx)?;
        let body_json = serde_json::to_string(body).map_err(|e| CodexError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?;
        let mut auth_refresh_attempted = auth_refresh_spent;
        let use_responses_lite = body.client_metadata.is_some();
        let (resp, response_headers, started_at) = self
            .start_http_event_attempt(
                &mut auth,
                &body_json,
                ctx,
                use_responses_lite,
                &mut auth_refresh_attempted,
                body.prompt_cache_key.as_deref(),
                &mut size_fallback,
            )
            .await?;

        let headers = response_headers.clone();
        let account_id = auth.account_id.clone();
        let receiver = self.spawn_http_event_stream(
            HttpEventStreamState {
                resp,
                response_headers,
                started_at,
                account_id: account_id.clone(),
            },
            ctx.clone(),
        );
        Ok((receiver, headers, account_id))
    }

    /// The live HTTP stream and its response headers; see
    /// `stream_codex_http_events_with_headers`.
    pub(crate) async fn stream_codex_http_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        auth_refresh_spent: bool,
    ) -> Result<
        (
            super::websocket::CodexWebSocketEventStream,
            Vec<(String, String)>,
        ),
        CodexError,
    > {
        self.stream_codex_http_events_replaying(body, ctx, auth_refresh_spent, None)
            .await
    }

    /// `stream_codex_http_events_for_owner` for a request that may replay a
    /// WebSocket size refusal; see `refuse_size_fallback_under_another_account`.
    async fn stream_codex_http_events_replaying(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        auth_refresh_spent: bool,
        size_fallback: Option<SizeRefusalFallback>,
    ) -> Result<
        (
            super::websocket::CodexWebSocketEventStream,
            Vec<(String, String)>,
        ),
        CodexError,
    > {
        let (receiver, headers, account_id) = self
            .stream_codex_http_events_with_headers(body, ctx, auth_refresh_spent, size_fallback)
            .await?;
        let (stream, publisher) = super::websocket::CodexWebSocketEventStream::pending(receiver);
        publisher.publish_account(account_id.as_deref());
        Ok((stream, headers))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_http_event_attempt(
        &self,
        auth: &mut StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
        use_responses_lite: bool,
        auth_refresh_attempted: &mut bool,
        cache_scope: Option<&str>,
        size_fallback: &mut Option<SizeRefusalFallback>,
    ) -> Result<(reqwest::Response, Vec<(String, String)>, Instant), CodexError> {
        loop {
            let (resp, started_at) = self
                .start_post_http(auth, body_json, ctx, use_responses_lite, cache_scope)
                .await?;

            if !resp.status().is_success() {
                // The body is read first: a 401 that names a spent window or a
                // known class is answered as such, not refreshed.
                let response = self.collect_http_response(resp, started_at, ctx).await?;
                let mut error = codex_status_error(response, auth.account_id.as_deref());
                error.origin = CodexErrorOrigin::Http;
                if !*auth_refresh_attempted && unauthorized_needs_refresh(&error) {
                    *auth_refresh_attempted = true;
                    *auth = self
                        .auth_manager
                        .force_refresh(&auth.access)
                        .await
                        .map_err(auth_refresh_error)?;
                    refuse_size_fallback_under_another_account(size_fallback, auth, ctx)?;
                    continue;
                }
                return Err(error);
            }

            let status = resp.status().as_u16();
            let headers = response_headers(&resp);
            super::turn_state::observe_response_headers(&ctx.req_id, &headers);
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_upstream_response_headers_capture(
                    traffic,
                    status,
                    started_at.elapsed(),
                    &headers,
                );
            }
            return Ok((resp, headers, started_at));
        }
    }

    pub async fn stream_codex_auto_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        let reservation =
            continuation.map(super::continuation::ContinuationReservation::from_public_candidate);
        self.stream_codex_auto_events_for_owner(body, ctx, reservation.as_ref())
            .await
            .map(|(events, _)| events.into_receiver())
    }

    /// The stream and, when it fell back to HTTP, that response's headers; a
    /// WebSocket stream has none.
    pub(crate) async fn stream_codex_auto_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<
        (
            super::websocket::CodexWebSocketEventStream,
            Vec<(String, String)>,
        ),
        CodexError,
    > {
        let mut websocket = self
            .stream_live_websocket_events(body, ctx, continuation, true)
            .await?;
        match websocket.recv().await {
            Some(Err(err)) if should_fallback_to_http(&err) => {
                log_websocket_fallback_to_http(ctx, &err);
                // The account and the access token the refused attempt went
                // out with, which a size refusal's replay must not leave.
                let size_fallback = super::websocket::is_message_size_refusal(&err).then(|| {
                    SizeRefusalFallback::new(err, websocket.account_id(), websocket.access_token())
                });
                // One refresh per client request: when the WebSocket attempt
                // already refreshed the token, the HTTP one does not again.
                self.stream_codex_http_events_replaying(
                    body,
                    ctx,
                    websocket.used_auth_refresh(),
                    size_fallback,
                )
                .await
            }
            Some(item) => {
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                let receiver = websocket.replace_receiver(rx);
                tokio::spawn(async move {
                    if tx.send(item).await.is_ok() {
                        forward_codex_events(receiver, tx).await;
                    }
                });
                Ok((websocket, Vec::new()))
            }
            None => Err(CodexError {
                status: 0,
                message: "WebSocket connection closed before the first Codex event".to_string(),
                detail: Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::WebSocket,
            }),
        }
    }

    fn spawn_http_event_stream(
        self: &Arc<Self>,
        state: HttpEventStreamState,
        ctx: RequestContext,
    ) -> CodexHttpEventReceiver {
        let HttpEventStreamState {
            mut resp,
            response_headers,
            started_at,
            account_id,
        } = state;
        let body_idle_timeout_ms = self.body_idle_timeout_ms;
        let buffered_response_limit = self.buffered_response_limit;
        let req_id = ctx.req_id.clone();
        let traffic = ctx.traffic.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let log = create_logger("codex");
            let mut semantic_output_forwarded = false;

            if tx
                .send(Ok(serde_json::json!({
                    "type": "keepalive",
                    "_ccp_synthetic": true
                })))
                .await
                .is_err()
            {
                return;
            }

            {
                let mut decoder = HttpSseDecoder::default();
                let mut body_bytes = 0_u64;
                let mut body_chunks = 0_u64;
                let mut event_count = 0_u64;
                let mut pending_events = Vec::new();
                // The data length of the events held until output starts.
                let mut pending_bytes = 0_usize;

                let stream_error = 'read_attempt: loop {
                    let chunk = tokio::select! {
                        _ = tx.closed() => {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_dropped",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                        chunk = tokio::time::timeout(
                            Duration::from_millis(body_idle_timeout_ms),
                            resp.chunk(),
                        ) => chunk
                    };

                    let chunk = match chunk {
                        Ok(Ok(Some(chunk))) => chunk,
                        Ok(Ok(None)) => {
                            let error = match decoder.finish() {
                                Ok(()) => http_sse_error(
                                    "Codex SSE stream ended before a terminal response event",
                                ),
                                Err(error) => error,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded && retryable_http_stream_error(&error) {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Ok(Err(err)) => {
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Transport error reading Codex response body: {}",
                                    crate::upstream_http::error_text(err)
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                class: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Err(_) => {
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Timed out waiting {body_idle_timeout_ms}ms for the next Codex response body chunk"
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                class: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    body_bytes = body_bytes.saturating_add(chunk.len() as u64);
                    body_chunks = body_chunks.saturating_add(1);
                    let events = match decoder.push(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded && retryable_http_stream_error(&error) {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    for event in events {
                        let data_bytes = event.data_bytes;
                        let Some(payload) = event.payload else {
                            continue;
                        };
                        event_count = event_count.saturating_add(1);
                        if let Some(traffic) = traffic.as_deref() {
                            write_codex_http_sse_event_capture(
                                traffic,
                                event.event.as_deref(),
                                &payload,
                            );
                        }

                        let failure = super::events::classify_event_failure(&payload);
                        if !semantic_output_forwarded {
                            if let Some(limit) = super::events::usage_limit_from_event_with_headers(
                                &payload,
                                &response_headers,
                            ) {
                                pending_events.clear();
                                // The terminal snapshot is never forwarded, so
                                // the gauge reads its windows here, before the
                                // limit marks the one it names.
                                super::rate_limits::observe_event(account_id.as_deref(), &payload);
                                let _ = tx
                                    .send(Err(codex_usage_limit_error(
                                        limit.for_account(account_id.as_deref()),
                                        CodexErrorOrigin::Http,
                                    )))
                                    .await;
                                return;
                            }
                            if let Some(failure) = failure.as_ref()
                                && failure.retryable()
                            {
                                pending_events.clear();
                                break 'read_attempt codex_event_failure_error(failure.clone());
                            }
                        }

                        let terminal = event_closes_http_stream(&payload);
                        if !semantic_output_forwarded && failure.is_some() {
                            pending_events.clear();
                            pending_bytes = 0;
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        } else if !semantic_output_forwarded
                            && http_event_starts_semantic_output(&payload)
                        {
                            semantic_output_forwarded = true;
                            pending_bytes = 0;
                            for pending in pending_events.drain(..) {
                                if tx.send(Ok(pending)).await.is_err() {
                                    return;
                                }
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        } else if semantic_output_forwarded || http_event_is_control(&payload) {
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        } else {
                            // Nothing of these events has reached the client,
                            // so holding more of them than a buffered answer
                            // may hold ends the stream here, with nothing sent
                            // again.
                            let held = pending_bytes.saturating_add(data_bytes);
                            if held > buffered_response_limit {
                                pending_events.clear();
                                let error =
                                    buffered_response_too_large_error(CodexErrorOrigin::Http);
                                log_http_stream_end(
                                    &log,
                                    "codex_http_stream_failed",
                                    &req_id,
                                    started_at,
                                    body_bytes,
                                    body_chunks,
                                    event_count,
                                    Some(&error.message),
                                );
                                let _ = tx.send(Err(error)).await;
                                return;
                            }
                            pending_bytes = held;
                            pending_events.push(payload);
                        }

                        if terminal {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_completed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                    }
                };

                // The client owns the retry policy, so a failed stream is
                // reported to it instead of being reopened here.
                let _ = tx.send(Err(stream_error)).await;
            }
        });

        rx
    }

    pub(crate) async fn post_codex_with_transport(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        mut transport: crate::config::CodexTransport,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        use crate::config::CodexTransport;

        let mut auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|e| auth_error(e, "Auth error"))?;

        let initial_pool_owner = websocket_pool_owner(continuation).cloned();
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = initial_pool_owner.as_ref()
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(
                owner,
                continuation.and_then(super::continuation::ContinuationReservation::turn_id),
            );
        }

        let mut active_continuation = continuation.cloned();
        let mut auth_refresh_attempted = false;
        let mut size_fallback = None;
        loop {
            let result = match transport {
                CodexTransport::Http => {
                    // A size refusal's replay after a token refresh stays
                    // under the credential of its WebSocket attempt.
                    refuse_size_fallback_under_another_account(&mut size_fallback, &auth, ctx)?;
                    let body_json = serde_json::to_string(body).map_err(|e| CodexError {
                        status: 500,
                        message: "Failed to serialize request".to_string(),
                        detail: Some(e.to_string()),
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    })?;
                    self.attempt_post_http(
                        &auth,
                        &body_json,
                        ctx,
                        body.client_metadata.is_some(),
                        body.prompt_cache_key.as_deref(),
                    )
                    .await
                    .map(|response| OwnerAwareCodexResponse::new(response, None))
                }
                CodexTransport::WebSocket => {
                    let ws_headers = build_codex_headers(
                        &auth,
                        ctx,
                        body.client_metadata.is_some(),
                        body.prompt_cache_key.as_deref(),
                    )?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let mut ws_body = build_websocket_request(
                        body,
                        active_continuation
                            .as_ref()
                            .map(super::continuation::ContinuationReservation::candidate),
                    );
                    super::turn_state::apply_to_websocket_request(&ctx.req_id, &mut ws_body);

                    super::websocket::codex_websocket_request(
                        &self.websocket_client,
                        &self.websocket_proxy_config,
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        ctx.traffic.as_deref(),
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        self.buffered_response_limit,
                        active_continuation.as_ref(),
                        super::websocket::OnConnectFailure::Fail,
                    )
                    .await
                }
                CodexTransport::Auto => {
                    let ws_headers = build_codex_headers(
                        &auth,
                        ctx,
                        body.client_metadata.is_some(),
                        body.prompt_cache_key.as_deref(),
                    )?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let mut ws_body = build_websocket_request(
                        body,
                        active_continuation
                            .as_ref()
                            .map(super::continuation::ContinuationReservation::candidate),
                    );
                    super::turn_state::apply_to_websocket_request(&ctx.req_id, &mut ws_body);

                    // Try WebSocket first
                    let on_connect_failure = if self.auto_http_fallback_enabled {
                        super::websocket::OnConnectFailure::FallBackToHttp {
                            req_id: &ctx.req_id,
                        }
                    } else {
                        super::websocket::OnConnectFailure::Fail
                    };
                    let ws_result = super::websocket::codex_websocket_request(
                        &self.websocket_client,
                        &self.websocket_proxy_config,
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        ctx.traffic.as_deref(),
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        self.buffered_response_limit,
                        active_continuation.as_ref(),
                        on_connect_failure,
                    )
                    .await;

                    match ws_result {
                        Ok(response) => Ok(response),
                        Err(err)
                            if should_retry_without_continuation(
                                &err,
                                active_continuation.as_ref(),
                            ) =>
                        {
                            // Drop stale continuation state before considering a
                            // replacement transport or connection.
                            Err(err)
                        }
                        Err(err)
                            if self.auto_http_fallback_enabled && should_fallback_to_http(&err) =>
                        {
                            // Fall back to HTTP only if WebSocket failed before sending
                            log_websocket_fallback_to_http(ctx, &err);
                            if super::websocket::is_message_size_refusal(&err) {
                                // The same frame would be refused again, so a
                                // resend of this request (after a token refresh
                                // or without its continuation) stays on HTTP.
                                transport = CodexTransport::Http;
                                size_fallback = Some(SizeRefusalFallback::new(
                                    err,
                                    auth.account_id.clone(),
                                    Some(auth.access.clone()),
                                ));
                            }
                            let body_json =
                                serde_json::to_string(body).map_err(|e| CodexError {
                                    status: 500,
                                    message: "Failed to serialize request".to_string(),
                                    detail: Some(e.to_string()),
                                    retry_after: None,
                                    usage_limit: None,
                                    class: None,
                                    origin: CodexErrorOrigin::Http,
                                })?;
                            self.attempt_post_http(
                                &auth,
                                &body_json,
                                ctx,
                                body.client_metadata.is_some(),
                                body.prompt_cache_key.as_deref(),
                            )
                            .await
                            .map(|response| OwnerAwareCodexResponse::new(response, None))
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            // A spent window is answered before a 401 can ask for a refresh.
            if let Ok(response) = &result
                && let Some(limit) =
                    super::events::usage_limit_from_response(&response.body, &response.headers)
            {
                let origin = match response.transport {
                    ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                    ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                };
                // The gauge reads the body's windows before the limit marks
                // the one it names, as on a live stream.
                let account = auth.account_id.as_deref();
                super::events::observe_rate_limits_in_body(account, &response.body);
                return Err(codex_usage_limit_error(limit.for_account(account), origin));
            }

            if should_refresh_after_unauthorized(&result, auth_refresh_attempted, transport) {
                auth_refresh_attempted = true;
                match self.auth_manager.force_refresh(&auth.access).await {
                    Ok(new_auth) => {
                        auth = new_auth;
                        invalidate_live_continuation_pool(active_continuation.as_ref());
                        active_continuation =
                            full_context_continuation(active_continuation.as_ref());
                        continue;
                    }
                    Err(e) => return Err(auth_refresh_error(e)),
                }
            }

            if let Ok(response) = &result
                && (200..300).contains(&response.status)
                && let Some(failure) = super::events::first_reportable_failure(&response.body)
            {
                return Err(CodexError {
                    status: failure.status,
                    message: failure.message.clone(),
                    detail: Some(failure.message),
                    retry_after: failure.retry_after,
                    usage_limit: None,
                    class: failure.class,
                    origin: CodexErrorOrigin::Http,
                });
            }

            match result {
                // An error code in the body says more than the status: read
                // its class and its message rather than the raw body. A
                // buffered WebSocket failure is an SSE body under the status
                // its event named, and is read the same way.
                Ok(response)
                    if !(200..300).contains(&response.status)
                        && super::events::error_class_from_body(
                            &response.body,
                            response.status,
                        )
                        .is_some() =>
                {
                    return Err(codex_status_error(
                        response.into_response(),
                        auth.account_id.as_deref(),
                    ));
                }
                Ok(response) if response.status == 401 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 401,
                        message: "Unauthorized".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 403 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 403,
                        message: "Forbidden".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = response_retry_after(&response.body, &response.headers);
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if !(200..300).contains(&response.status) => {
                    return Err(codex_status_error(
                        response.into_response(),
                        auth.account_id.as_deref(),
                    ));
                }
                Ok(response) => return Ok(response),
                Err(err)
                    if should_retry_without_continuation(&err, active_continuation.as_ref()) =>
                {
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn stream_codex_websocket_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        let reservation =
            continuation.map(super::continuation::ContinuationReservation::from_public_candidate);
        self.stream_codex_websocket_events_for_owner(body, ctx, reservation.as_ref())
            .await
            .map(super::websocket::CodexWebSocketEventStream::into_receiver)
    }

    pub(crate) async fn stream_codex_websocket_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        self.stream_live_websocket_events(body, ctx, continuation, false)
            .await
    }

    /// `http_fallback` says the caller answers a failed connect over HTTP, so
    /// the connect pause applies (`OnConnectFailure::FallBackToHttp`) until
    /// the first item reached the caller; later connects use `Fail`.
    async fn stream_live_websocket_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        http_fallback: bool,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let auth = self
            .auth_manager
            .get_auth()
            .await
            .map_err(|e| auth_error(e, "Auth error"))?;

        let turn_id = continuation.and_then(super::continuation::ContinuationReservation::turn_id);
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = websocket_pool_owner(continuation)
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, turn_id);
        }

        let client = self.clone();
        let body = body.clone();
        let ctx = ctx.clone();
        let continuation = continuation.cloned();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let (mut rx, socket_id_publisher) =
            super::websocket::CodexWebSocketEventStream::pending(rx);
        rx.mark_rate_limits_observed();
        socket_id_publisher.publish_account(auth.account_id.as_deref());
        socket_id_publisher.publish_access_token(&auth.access);
        tokio::spawn(async move {
            client
                .coordinate_live_websocket_events(
                    body,
                    ctx,
                    continuation,
                    auth,
                    tx,
                    socket_id_publisher,
                    http_fallback,
                )
                .await;
        });

        Ok(rx)
    }

    #[allow(clippy::too_many_arguments)]
    async fn coordinate_live_websocket_events(
        &self,
        body: ResponsesRequest,
        ctx: RequestContext,
        mut continuation: Option<super::continuation::ContinuationReservation>,
        mut auth: StoredAuth,
        tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
        socket_id_publisher: super::websocket::CodexWebSocketSocketIdPublisher,
        http_fallback: bool,
    ) {
        let mut auth_refresh_attempted = false;
        let mut continuation_retry_available = continuation
            .as_ref()
            .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
            .is_some();
        let mut forwarded_any = false;
        // The caller falls back to HTTP only on the first item it receives,
        // so once an item has reached it a later connect of this request
        // fails on its own: the pause neither skips it nor starts from it.
        let mut sent_any = false;

        'attempt: loop {
            let on_connect_failure = if http_fallback && !sent_any {
                super::websocket::OnConnectFailure::FallBackToHttp {
                    req_id: &ctx.req_id,
                }
            } else {
                super::websocket::OnConnectFailure::Fail
            };
            socket_id_publisher.publish(None);
            socket_id_publisher.publish_account(auth.account_id.as_deref());
            socket_id_publisher.publish_access_token(&auth.access);
            let ws_headers = match build_codex_headers(
                &auth,
                &ctx,
                body.client_metadata.is_some(),
                body.prompt_cache_key.as_deref(),
            ) {
                Ok(headers) => super::websocket::codex_websocket_headers(&headers),
                Err(err) => {
                    if tx.send(Err(err)).await.is_err() {
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                    }
                    return;
                }
            };
            let mut ws_body = build_websocket_request(
                &body,
                continuation
                    .as_ref()
                    .map(super::continuation::ContinuationReservation::candidate),
            );
            super::turn_state::apply_to_websocket_request(&ctx.req_id, &mut ws_body);
            let start = super::websocket::codex_websocket_event_stream(
                &self.websocket_client,
                &self.websocket_proxy_config,
                &self.base_url,
                &ws_headers,
                &ws_body,
                &ctx,
                ctx.traffic.clone(),
                super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                self.buffered_response_limit,
                continuation.as_ref(),
                on_connect_failure,
            );
            let mut stream = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_abandoned_live_continuation(
                        continuation.as_ref(),
                        &socket_id_publisher,
                    );
                    return;
                }
                result = start => match result {
                    Ok(stream) => stream,
                    Err(err)
                        if unauthorized_needs_refresh(&err)
                            && !auth_refresh_attempted
                            && !forwarded_any =>
                    {
                        auth_refresh_attempted = true;
                        socket_id_publisher.mark_auth_refresh();
                        invalidate_live_continuation_pool(continuation.as_ref());
                        let refresh = self.auth_manager.force_refresh(&auth.access);
                        auth = match refresh.await {
                            Ok(auth) => {
                                if tx.is_closed() {
                                    abort_abandoned_live_continuation(
                                        continuation.as_ref(),
                                        &socket_id_publisher,
                                    );
                                    return;
                                }
                                auth
                            },
                            Err(refresh_err) => {
                                if tx.send(Err(auth_refresh_error(refresh_err))).await.is_err() {
                                    abort_abandoned_live_continuation(
                                        continuation.as_ref(),
                                        &socket_id_publisher,
                                    );
                                }
                                return;
                            }
                        };
                        if continuation_retry_available {
                            socket_id_publisher.mark_full_context_retry();
                        }
                        continuation = full_context_continuation(continuation.as_ref());
                        continuation_retry_available = false;
                        continue 'attempt;
                    }
                    Err(err) if continuation_retry_available && is_continuation_retry_error(&err) => {
                        socket_id_publisher.mark_full_context_retry();
                        continuation = full_context_continuation(continuation.as_ref());
                        continuation_retry_available = false;
                        continue 'attempt;
                    }
                    Err(err) => {
                        if tx.send(Err(err)).await.is_err() {
                            abort_abandoned_live_continuation(
                                continuation.as_ref(),
                                &socket_id_publisher,
                            );
                        }
                        return;
                    }
                }
            };

            loop {
                let item = tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        if let Some(reservation) = continuation.as_ref() {
                            super::websocket::invalidate_codex_websocket_pool_socket(
                                reservation,
                                stream.socket_id(),
                            );
                        }
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                        return;
                    }
                    item = stream.recv() => item,
                };
                if tx.is_closed() {
                    if let Some(reservation) = continuation.as_ref() {
                        super::websocket::invalidate_codex_websocket_pool_socket(
                            reservation,
                            stream.socket_id(),
                        );
                    }
                    abort_abandoned_live_continuation(continuation.as_ref(), &socket_id_publisher);
                    return;
                }
                let Some(item) = item else {
                    return;
                };
                socket_id_publisher.publish(stream.socket_id());

                let unauthorized = match &item {
                    Err(err) => unauthorized_needs_refresh(err),
                    Ok(payload) => unauthorized_event_needs_refresh(payload),
                };
                if unauthorized && !auth_refresh_attempted && !forwarded_any {
                    auth_refresh_attempted = true;
                    socket_id_publisher.mark_auth_refresh();
                    invalidate_live_continuation_pool(continuation.as_ref());
                    let refresh = self.auth_manager.force_refresh(&auth.access);
                    auth = match refresh.await {
                        Ok(auth) => {
                            if tx.is_closed() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_socket(
                                        reservation,
                                        stream.socket_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                            auth
                        }
                        Err(refresh_err) => {
                            if tx.send(Err(auth_refresh_error(refresh_err))).await.is_err() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_socket(
                                        reservation,
                                        stream.socket_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                            }
                            return;
                        }
                    };
                    if continuation_retry_available {
                        socket_id_publisher.mark_full_context_retry();
                    }
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                if let Err(err) = &item
                    && continuation_retry_available
                    && is_continuation_retry_error(err)
                    && !forwarded_any
                {
                    socket_id_publisher.mark_full_context_retry();
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                if item.as_ref().is_ok_and(event_closes_live_retry_window) {
                    forwarded_any = true;
                }
                let terminal = item.as_ref().is_err()
                    || item.as_ref().is_ok_and(super::websocket::is_terminal_event);
                // A refresh can move the stream to another account before the
                // reader takes this item off the queue, so the reading is
                // recorded here, for the account of the attempt that read it.
                if let Ok(payload) = &item {
                    super::rate_limits::observe_event(auth.account_id.as_deref(), payload);
                }
                if tx.send(item).await.is_err() {
                    if let Some(reservation) = continuation.as_ref() {
                        super::websocket::invalidate_codex_websocket_pool_socket(
                            reservation,
                            stream.socket_id(),
                        );
                    }
                    abort_abandoned_live_continuation(continuation.as_ref(), &socket_id_publisher);
                    return;
                }
                sent_any = true;
                if terminal {
                    return;
                }
            }
        }
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
        use_responses_lite: bool,
        cache_scope: Option<&str>,
    ) -> Result<CodexResponse, CodexError> {
        let (resp, started_at) = self
            .start_post_http(auth, body_json, ctx, use_responses_lite, cache_scope)
            .await?;
        self.collect_http_response(resp, started_at, ctx).await
    }

    async fn start_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
        use_responses_lite: bool,
        cache_scope: Option<&str>,
    ) -> Result<(reqwest::Response, Instant), CodexError> {
        let url = &self.base_url;
        let mut headers = build_codex_headers(auth, ctx, use_responses_lite, cache_scope)?;
        if let Some(turn_state) = super::turn_state::outgoing(&ctx.req_id) {
            headers.insert(
                super::turn_state::TURN_STATE_HEADER,
                header_value(super::turn_state::TURN_STATE_HEADER, &turn_state)?,
            );
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_codex_http_request_capture(traffic, url, &headers, body_json);
        }

        // Build headers
        let mut req_builder = self.client.post(url);
        for (key, value) in headers.iter() {
            req_builder = req_builder.header(key.as_str(), value.as_bytes());
        }

        // Apply header timeout
        let started_at = Instant::now();
        let send_fut = req_builder.body(body_json.to_string()).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: None,
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!(
                            "Transport error: {}",
                            crate::upstream_http::error_text(e)
                        ),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {}", crate::upstream_http::error_text(e)),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        class: None,
                        origin: CodexErrorOrigin::Http,
                    }
                }
            })?;

        Ok((resp, started_at))
    }

    async fn collect_http_response(
        &self,
        mut resp: reqwest::Response,
        started_at: Instant,
        ctx: &RequestContext,
    ) -> Result<CodexResponse, CodexError> {
        if !resp.status().is_success() {
            return collect_error_response(
                resp,
                started_at,
                self.body_idle_timeout_ms,
                ctx,
                "Codex",
            )
            .await;
        }
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let mut body_bytes = Vec::new();
        let mut response_started = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                resp.chunk(),
            )
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for the next Codex response body chunk",
                    self.body_idle_timeout_ms
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| CodexError {
                status: 0,
                message: format!(
                    "Transport error reading Codex response body: {}",
                    crate::upstream_http::error_text(e)
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?;

            let Some(chunk) = chunk else {
                break;
            };
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            // Past the limit the bytes held up to it are read for a refusal;
            // whatever comes after them is never read. Dropping the response
            // closes its connection unread.
            let room = self
                .buffered_response_limit
                .saturating_sub(body_bytes.len());
            if chunk.len() > room {
                body_bytes.extend_from_slice(&chunk[..room]);
                if !capped_body_names_refusal(&body_bytes, status, &headers) {
                    return Err(buffered_response_too_large_error(CodexErrorOrigin::Http));
                }
                break;
            }
            body_bytes.extend_from_slice(&chunk);
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(
                traffic,
                status,
                started_at.elapsed(),
                &headers,
                &body_bytes,
            );
        }

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }

    async fn attempt_post_search(
        &self,
        auth: &StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
        body_limit: usize,
    ) -> Result<CodexResponse, CodexError> {
        let url = search_endpoint(&self.base_url);
        let headers = build_codex_search_headers(auth, ctx)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_codex_http_request_capture(traffic, &url, &headers, body_json);
        }

        let mut request = self.client.post(&url);
        for (key, value) in headers.iter() {
            request = request.header(key.as_str(), value.as_bytes());
        }
        let started_at = Instant::now();
        let mut response = tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            request.body(body_json.to_string()).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex search response headers",
                self.header_timeout_ms
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|e| CodexError {
            status: 0,
            message: format!(
                "Codex search network error: {}",
                crate::upstream_http::error_text(e)
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?;

        if !response.status().is_success() {
            return collect_error_response(
                response,
                started_at,
                self.body_idle_timeout_ms,
                ctx,
                "Codex search",
            )
            .await;
        }
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let mut body = Vec::new();
        let mut response_started = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                response.chunk(),
            )
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for the next Codex search response body chunk",
                    self.body_idle_timeout_ms
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| CodexError {
                status: 0,
                message: format!(
                    "Transport error reading Codex search response body: {}",
                    crate::upstream_http::error_text(e)
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.len() > body_limit - body.len() {
                return Err(CodexError {
                    status: 0,
                    message: format!(
                        "Transport error reading Codex search response body: {}",
                        crate::upstream_http::BodyReadError::TooLarge { limit: body_limit }
                    ),
                    detail: Some("http_response_body".to_string()),
                    retry_after: None,
                    usage_limit: None,
                    class: None,
                    origin: CodexErrorOrigin::Http,
                });
            }
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            body.extend_from_slice(&chunk);
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(traffic, status, started_at.elapsed(), &headers, &body);
        }

        Ok(CodexResponse {
            body,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }
}

async fn forward_codex_events(
    mut source: tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
) {
    while let Some(item) = source.recv().await {
        if tx.send(item).await.is_err() {
            return;
        }
    }
}

fn response_headers(resp: &reqwest::Response) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
}

/// A non-success Codex answer with the part of its body `read_error_body`
/// reads, which is also what the traffic capture records.
async fn collect_error_response(
    resp: reqwest::Response,
    started_at: Instant,
    deadline_ms: u64,
    ctx: &RequestContext,
    subject: &str,
) -> Result<CodexResponse, CodexError> {
    let status = resp.status().as_u16();
    let headers = response_headers(&resp);
    let body = read_error_body(resp, deadline_ms, ctx, subject).await?;
    if let Some(traffic) = ctx.traffic.as_deref() {
        write_upstream_response_capture(traffic, status, started_at.elapsed(), &headers, &body);
    }
    Ok(CodexResponse {
        body,
        status,
        headers,
        transport: ActualTransport::Http,
    })
}

/// Reads the body of a non-success answer to classify it: at most
/// `MAX_ERROR_BODY_BYTES`, all of it within `deadline_ms`, so an upstream that
/// keeps sending can neither hold the request nor grow its memory. Nothing past
/// the limit is read; the response is dropped with the rest of its body. A
/// read that outlasts the deadline fails as a transport error of its own.
async fn read_error_body(
    mut resp: reqwest::Response,
    deadline_ms: u64,
    ctx: &RequestContext,
    subject: &str,
) -> Result<Vec<u8>, CodexError> {
    let read = async {
        let mut body = Vec::new();
        let mut response_started = false;
        while body.len() < MAX_ERROR_BODY_BYTES {
            let chunk = resp.chunk().await.map_err(|e| CodexError {
                status: 0,
                message: format!(
                    "Transport error reading {subject} response body: {}",
                    crate::upstream_http::error_text(e)
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: CodexErrorOrigin::Http,
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            let remaining = MAX_ERROR_BODY_BYTES - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        Ok(body)
    };
    tokio::time::timeout(Duration::from_millis(deadline_ms), read)
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {deadline_ms}ms for the {subject} error response body"
            ),
            detail: Some("http_response_body".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        })?
}

fn event_closes_http_stream(payload: &serde_json::Value) -> bool {
    matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some(
            "response.completed"
                | "response.incomplete"
                | "response.done"
                | "response.failed"
                | "response.error"
                | "error"
        )
    )
}

fn http_event_is_control(payload: &serde_json::Value) -> bool {
    matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some(
            "keepalive"
                | "response.created"
                | "response.in_progress"
                | "codex.rate_limits"
                | "response.web_search_call.in_progress"
                | "response.web_search_call.searching"
                | "response.web_search_call.completed"
        )
    )
}

fn http_event_starts_semantic_output(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|value| value.as_str()) {
        Some("response.output_item.added") => matches!(
            payload
                .pointer("/item/type")
                .and_then(|value| value.as_str()),
            Some("message" | "function_call")
        ),
        Some(
            "response.reasoning_summary_text.delta"
            | "response.output_text.delta"
            | "response.function_call_arguments.delta",
        ) => payload
            .get("delta")
            .and_then(|value| value.as_str())
            .is_some_and(|delta| !delta.is_empty()),
        Some("response.output_item.done") => matches!(
            payload
                .pointer("/item/type")
                .and_then(|value| value.as_str()),
            Some("reasoning" | "message" | "function_call")
        ),
        Some("response.completed" | "response.incomplete" | "response.done") => true,
        _ => false,
    }
}

fn codex_event_failure_error(failure: super::events::CodexEventFailure) -> CodexError {
    CodexError {
        status: failure.status,
        message: failure.message.clone(),
        detail: Some(failure.message),
        retry_after: failure.retry_after,
        usage_limit: None,
        class: failure.class,
        origin: CodexErrorOrigin::Http,
    }
}

fn codex_usage_limit_error(limit: CodexUsageLimit, origin: CodexErrorOrigin) -> CodexError {
    CodexError {
        status: 429,
        message: limit.message.clone(),
        detail: Some(limit.message.clone()),
        retry_after: None,
        usage_limit: Some(Box::new(limit)),
        class: None,
        origin,
    }
}

fn retryable_http_stream_error(error: &CodexError) -> bool {
    if error.usage_limit.is_some() {
        return false;
    }
    if should_retry_codex_status(error.status) || is_retryable_transport_error(error) {
        return true;
    }
    if error.status != 0 {
        return false;
    }
    if error.detail.as_deref() == Some("http_response_sse") {
        return error.message != "Codex SSE frame exceeds the size limit";
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("ended before a terminal response event")
        || message.contains("ended with an incomplete frame")
}

#[allow(clippy::too_many_arguments)]
fn log_http_stream_end(
    log: &crate::logging::Logger,
    message: &str,
    req_id: &str,
    started_at: Instant,
    body_bytes: u64,
    body_chunks: u64,
    event_count: u64,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        ("transport".to_string(), serde_json::json!("http")),
        ("bodyBytes".to_string(), serde_json::json!(body_bytes)),
        ("bodyChunks".to_string(), serde_json::json!(body_chunks)),
        ("eventCount".to_string(), serde_json::json!(event_count)),
        (
            "ms".to_string(),
            serde_json::json!(started_at.elapsed().as_millis()),
        ),
    ]);
    if let Some(error) = error {
        fields.insert("error".to_string(), serde_json::json!(error));
    }
    match message {
        "codex_http_stream_completed" => log.info(message, Some(fields)),
        _ => log.warn(message, Some(fields)),
    }
}

fn write_codex_http_request_capture(
    traffic: &TrafficCapture,
    url: &str,
    headers: &http::HeaderMap,
    body_json: &str,
) {
    let body = serde_json::from_str(body_json).unwrap_or_else(|_| {
        serde_json::json!({
            "unparseable": true,
            "bytes": body_json.len(),
        })
    });
    traffic.write_json("020-upstream-request", &body);
    traffic.write_json(
        "021-upstream-request-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "http",
            "url": url,
            "method": "POST",
            "headers": headers_to_json(headers),
            "size": summarize_json_request_size(&body, body_json),
        }),
    );
}

fn write_live_upstream_response_headers(
    traffic: &TrafficCapture,
    response: &reqwest::Response,
    elapsed: Duration,
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": response.status().as_u16(),
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json(response.headers()),
        }),
    );
}

fn write_upstream_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    headers: &[(String, String)],
    body: &[u8],
) {
    write_upstream_response_headers_capture(traffic, status, elapsed, headers);
    if status >= 400 {
        traffic.write_text("031-upstream-error-body", &String::from_utf8_lossy(body));
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", body);
        write_codex_sse_event_capture(traffic, body);
    }
}

fn write_upstream_response_headers_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    headers: &[(String, String)],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json_from_pairs(headers),
        }),
    );
}

fn write_codex_http_sse_event_capture(
    traffic: &TrafficCapture,
    event: Option<&str>,
    payload: &serde_json::Value,
) {
    let mut payload = payload.clone();
    if let Some(event) = event
        && let Some(object) = payload.as_object_mut()
    {
        object
            .entry("_sse_event")
            .or_insert_with(|| serde_json::json!(event));
    }
    traffic.write_json_event("040-upstream-event", &payload);
}

fn write_codex_sse_event_capture(traffic: &TrafficCapture, body: &[u8]) {
    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            traffic.write_json_event(
                "040-upstream-event",
                &serde_json::json!({
                    "event": event.event,
                    "data": "[DONE]",
                }),
            );
            continue;
        }

        match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(mut value) => {
                if let Some(name) = event.event
                    && let Some(obj) = value.as_object_mut()
                {
                    obj.entry("_sse_event").or_insert(serde_json::json!(name));
                }
                traffic.write_json_event("040-upstream-event", &value);
            }
            Err(_) => {
                traffic.write_json_event(
                    "040-upstream-event",
                    &serde_json::json!({
                        "event": event.event,
                        "unparseable": true,
                        "data": event.data,
                    }),
                );
            }
        }
    }
}

/// Headers as a capture records them. A value marked sensitive, such as a
/// forwarded client header, is replaced by its length.
pub(super) fn headers_to_json(headers: &http::HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers.iter() {
        let recorded = if value.is_sensitive() {
            format!("[redacted len={}]", value.len())
        } else {
            value.to_str().unwrap_or("").to_string()
        };
        out.insert(key.to_string(), serde_json::Value::String(recorded));
    }
    serde_json::Value::Object(out)
}

fn headers_to_json_from_pairs(headers: &[(String, String)]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers {
        out.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    serde_json::Value::Object(out)
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

fn auth_refresh_error(err: anyhow::Error) -> CodexError {
    auth_error(err, "Unauthorized")
}

/// What the client is told after a temporary Codex token refresh failure: the
/// token endpoint was unreachable or answered 429 or 5xx, or the refresh did not
/// finish.
pub(crate) const TOKEN_REFRESH_UNAVAILABLE: &str = "Codex token refresh is temporarily unavailable";

/// Maps a failure to obtain a Codex token. A transient refresh failure answers
/// 503 with a fixed message, which the client may retry; any other failure
/// answers 401 with its own text, which asks for a login.
fn auth_error(err: anyhow::Error, message: &str) -> CodexError {
    let transient = err.downcast_ref::<TransientRefreshError>().is_some();
    let (status, message, detail) = if transient {
        (
            503,
            TOKEN_REFRESH_UNAVAILABLE.to_string(),
            TOKEN_REFRESH_UNAVAILABLE.to_string(),
        )
    } else {
        (401, message.to_string(), err.to_string())
    };
    CodexError {
        status,
        message,
        detail: Some(detail),
        retry_after: None,
        usage_limit: None,
        class: None,
        origin: CodexErrorOrigin::Auth,
    }
}

/// The delay a Codex error body names, as `classify_event_failure` reads it
/// from an event, else the response's `Retry-After`, both normalised; a value
/// that is not usable counts as absent.
fn response_retry_after(body: &[u8], headers: &[(String, String)]) -> Option<String> {
    super::events::retry_after_from_body(body).or_else(|| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| super::events::normalize_retry_after(value))
    })
}

/// A non-success answer of the request sent for `account`. Every
/// `codex.rate_limits` event its body holds reaches the quota gauge whole
/// before the body is reduced to a spent window.
pub(super) fn codex_status_error(response: CodexResponse, account: Option<&str>) -> CodexError {
    super::events::observe_rate_limits_in_body(account, &response.body);
    let usage_limit = super::events::usage_limit_from_response(&response.body, &response.headers)
        .map(|limit| Box::new(limit.for_account(account)));
    let retry_after = response_retry_after(&response.body, &response.headers);
    let class = super::events::error_class_from_body(&response.body, response.status);
    // A terminal code with a missing or blank message takes the CLI's text for
    // it, as on a stream event.
    let terminal_text =
        super::events::terminal_code_text_from_body(&response.body, response.status);
    let message = codex_status_error_message(&response.body)
        .filter(|message| terminal_text.is_none() || !message.trim().is_empty())
        .or_else(|| terminal_text.map(str::to_string))
        .unwrap_or_else(|| {
            if class == Some(super::events::CodexErrorClass::ModelNotFound) {
                // The code names a missing model that came with no message.
                return "model_not_found".to_string();
            }
            format!(
                "Upstream Codex request failed with status {}",
                response.status
            )
        });
    CodexError {
        status: response.status,
        message: message.clone(),
        detail: Some(message),
        retry_after,
        usage_limit,
        class,
        origin: match response.transport {
            ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
            ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
        },
    }
}

fn codex_status_error_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            parse_sse_events(body).into_iter().find_map(|event| {
                let payload = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                super::events::classify_event_failure(&payload).map(|failure| failure.message)
            })
        })
}

fn should_retry_codex_status(status: u16) -> bool {
    should_retry_status(status) || status == 529
}

fn is_retryable_transport_error(err: &CodexError) -> bool {
    if err.origin == CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some("websocket_pre_request") {
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    if err.status != 0 {
        return false;
    }

    let message = err.message.to_ascii_lowercase();
    message.contains("timed out waiting")
        || message.contains("transport error")
        || message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("timed out")
        || message.contains("econnreset")
        || message.contains("etimedout")
        || message.contains("broken pipe")
        || message.contains("epipe")
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("econnreset")
        || msg.contains("etimedout")
        || msg.contains("epipe")
}

fn should_refresh_after_unauthorized(
    result: &Result<OwnerAwareCodexResponse, CodexError>,
    auth_refresh_attempted: bool,
    transport: crate::config::CodexTransport,
) -> bool {
    if auth_refresh_attempted {
        return false;
    }
    // A 401 whose body names a spent window or a known error class is answered
    // as such: a fresh token would not change it. The caller has already
    // answered a spent window in a response body.
    match result {
        Ok(response) => {
            response.status == 401
                && super::events::error_class_from_body(&response.body, response.status).is_none()
        }
        Err(err) => {
            unauthorized_needs_refresh(err)
                && (err.origin != CodexErrorOrigin::WebSocketHandshake
                    || transport == crate::config::CodexTransport::WebSocket)
        }
    }
}

/// Whether a 401 asks for a token refresh: only one that names neither a spent
/// window nor a known error class does.
fn unauthorized_needs_refresh(err: &CodexError) -> bool {
    err.status == 401 && err.usage_limit.is_none() && err.class.is_none()
}

/// The same rule for an error event on a live WebSocket stream.
fn unauthorized_event_needs_refresh(payload: &serde_json::Value) -> bool {
    super::events::classify_event_failure(payload)
        .is_some_and(|failure| failure.explicit_status == Some(401) && failure.class.is_none())
        && super::events::usage_limit_from_event_with_headers(payload, &[]).is_none()
}

/// A failed handshake falls back to HTTP, and so does a request the WebSocket
/// refused for its size: one longer than the socket's advertised limit, never
/// written, or one whose socket closed with code 1009 before any text frame of
/// the response. A request already written goes over HTTP in two cases: that
/// 1009 close, as the Codex CLI treats that close and sends it (`codex-rs/codex-api/src/endpoint/responses_websocket.rs`
/// at `ed7ca22bb6`, HTTP fallback at `bc3ccba6e1`), and a resend the proxy
/// makes anyway, such as the full-context one that repairs a continuation,
/// whose frame is over the limit of the socket it would go to.
fn should_fallback_to_http(err: &CodexError) -> bool {
    if super::websocket::is_message_size_refusal(err) {
        return true;
    }
    err.origin == CodexErrorOrigin::WebSocketHandshake
        && err.status != http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
        && !matches!(
            err.detail.as_deref(),
            Some(
                super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL
                    | super::websocket::WEBSOCKET_WRITE_TIMEOUT_DETAIL
            )
        )
}

/// The longest `detail` a fallback log line carries, in characters.
const FALLBACK_LOG_DETAIL_CHARS: usize = 300;

/// Logs a WebSocket failure that `auto` answers over HTTP, so a refused
/// WebSocket stays visible. The line carries only what the handshake itself
/// returned: its status (0 when none came), class, usage-limit flag,
/// `Retry-After` and the rejection text or connect error, cut short. For a
/// request refused for its size the detail is a fixed text naming the frame
/// length and the limit, or the 1009 close. It carries nothing from the
/// request.
fn log_websocket_fallback_to_http(ctx: &RequestContext, err: &CodexError) {
    let detail = websocket_fallback_log_detail(err);
    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
        ("status".to_string(), serde_json::json!(err.status)),
        (
            "class".to_string(),
            serde_json::json!(err.class.map(|class| format!("{class:?}"))),
        ),
        (
            "usageLimit".to_string(),
            serde_json::json!(err.usage_limit.is_some()),
        ),
        ("detail".to_string(), serde_json::json!(detail)),
    ]);
    if let Some(retry_after) = &err.retry_after {
        fields.insert("retryAfter".to_string(), serde_json::json!(retry_after));
    }
    create_logger("codex").warn("codex_websocket_fallback_to_http", Some(fields));
}

/// The `detail` of a WebSocket failure's log line: the rejection text or
/// connect error, or the size refusal's fixed text, cut short.
pub(super) fn websocket_fallback_log_detail(err: &CodexError) -> String {
    let detail = if super::websocket::is_message_size_refusal(err) {
        err.message.as_str()
    } else {
        err.detail.as_deref().unwrap_or(&err.message)
    };
    detail.chars().take(FALLBACK_LOG_DETAIL_CHARS).collect()
}

/// A WebSocket attempt refused for its size whose request goes over HTTP
/// instead, with the ChatGPT account and the access token that attempt went
/// out with. The token is held only for `same_credential`; the type has no
/// `Debug`.
struct SizeRefusalFallback {
    refusal: CodexError,
    account_id: Option<String>,
    access_token: Option<String>,
}

impl SizeRefusalFallback {
    fn new(refusal: CodexError, account_id: Option<String>, access_token: Option<String>) -> Self {
        Self {
            refusal,
            account_id,
            access_token,
        }
    }

    /// Whether a replay with `auth` is shown to go out under the WebSocket
    /// attempt's account: both account ids are known and equal, or, when
    /// either is unknown, the access token is the attempt's own.
    fn same_credential(&self, auth: &StoredAuth) -> bool {
        match (self.account_id.as_deref(), auth.account_id.as_deref()) {
            (Some(before), Some(now)) => before == now,
            _ => self.access_token.as_deref() == Some(auth.access.as_str()),
        }
    }
}

/// Refuses the HTTP replay of a size refusal unless it goes out under the
/// WebSocket attempt's credential: the request is not sent, and the client
/// gets the size refusal itself. A replay passes when both account ids are
/// known and equal, so a renewed token of the same account passes, or when
/// either account id is unknown and the access token is the one the
/// WebSocket attempt went out with. Every other case is refused, a new token
/// under an unknown account included.
fn refuse_size_fallback_under_another_account(
    fallback: &mut Option<SizeRefusalFallback>,
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<(), CodexError> {
    let Some(refused) = fallback.take_if(|pending| !pending.same_credential(auth)) else {
        return Ok(());
    };
    create_logger("codex").warn(
        "codex_websocket_size_fallback_refused",
        Some(size_fallback_refused_fields(&ctx.req_id)),
    );
    Err(refused.refusal)
}

/// The fields of `codex_websocket_size_fallback_refused`: the request id
/// only, nothing of either credential.
fn size_fallback_refused_fields(req_id: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([("reqId".to_string(), serde_json::json!(req_id))])
}

fn should_retry_without_continuation(
    err: &CodexError,
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    if continuation
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    is_continuation_retry_error(err)
}

fn full_context_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<super::continuation::ContinuationReservation> {
    continuation.map(super::continuation::ContinuationReservation::full_context_retry)
}

fn abort_live_continuation(continuation: Option<&super::continuation::ContinuationReservation>) {
    if let Some(continuation) = continuation {
        super::continuation::abort_continuation_for_owner(continuation);
    }
}

fn abort_abandoned_live_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
    socket_id_publisher: &super::websocket::CodexWebSocketSocketIdPublisher,
) {
    if !socket_id_publisher.is_provider_retry_handoff() {
        abort_live_continuation(continuation);
    }
}

pub(super) fn invalidate_live_continuation_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) {
    let Some(continuation) = continuation else {
        return;
    };
    let Some(owner) = websocket_pool_owner(Some(continuation)) else {
        return;
    };
    super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, continuation.turn_id());
}

fn event_closes_live_retry_window(payload: &serde_json::Value) -> bool {
    // A snapshot that reports a spent window ends the request, so a later 401
    // cannot ask for a refresh.
    super::events::is_terminal_rate_limit_event(payload)
        || !matches!(
            payload.get("type").and_then(|value| value.as_str()),
            Some("codex.rate_limits" | "keepalive")
        )
}

/// A write that ran out of time (`WEBSOCKET_WRITE_TIMEOUT_DETAIL`) is left
/// out on purpose: part of the frame may already be on the socket, so the
/// request is not sent again.
pub(super) fn is_continuation_retry_error(err: &CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(super::websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
            | Some(super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
            | Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
            | Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL)
    )
}

fn websocket_pool_owner(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<&ConversationIdentity> {
    let continuation = continuation?;
    if continuation.candidate().disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    continuation.owner()
}

fn should_reset_websocket_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    let Some(reason) =
        continuation.and_then(|continuation| continuation.candidate().disabled_reason.as_deref())
    else {
        return false;
    };
    reason != "disabled"
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_continuation(
        owner: Option<ConversationIdentity>,
        turn_id: Option<u64>,
        previous_response_id: Option<&str>,
        origin_socket_id: Option<u64>,
        disabled_reason: Option<&str>,
    ) -> super::super::continuation::ContinuationReservation {
        super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id,
                previous_response_id: previous_response_id.map(str::to_string),
                input_delta: None,
                input_delta_count: 1,
                disabled_reason: disabled_reason.map(str::to_string),
            },
            owner,
            origin_socket_id,
        )
    }

    #[test]
    fn normalizes_supported_proxy_urls() {
        assert_eq!(
            normalize_proxy_url("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080/")
        );
        assert_eq!(
            normalize_proxy_url("https://user:pass@proxy.example:8443").as_deref(),
            Some("https://user:pass@proxy.example:8443/")
        );
        for scheme in ["socks4", "socks4a"] {
            let proxy = format!("{scheme}://proxy.example:1080");
            assert_eq!(normalize_proxy_url(&proxy), Some(proxy));
        }
        for scheme in ["socks5", "socks5h"] {
            let proxy = format!("{scheme}://user:pass@proxy.example:1080");
            assert_eq!(normalize_proxy_url(&proxy), Some(proxy));
        }
    }

    #[test]
    fn rejects_malformed_or_unsupported_proxy_urls() {
        assert!(normalize_proxy_url("http://").is_none());
        assert!(normalize_proxy_url("ftp://proxy.example:21").is_none());
        assert!(normalize_proxy_url("socks4://user@proxy.example:1080").is_none());
        assert!(normalize_proxy_url("socks4a://user:pass@proxy.example:1080").is_none());
    }

    #[test]
    fn custom_client_auto_fallback_tracks_effective_proxy_route() {
        let proxy = "http://proxy.example:8080";
        let proxied =
            super::super::websocket::WebSocketProxyConfig::new(None, Some(proxy), None, None);
        assert!(!custom_client_auto_http_fallback_enabled(
            "https://codex.invalid/responses",
            &proxied
        ));

        let bypassed = super::super::websocket::WebSocketProxyConfig::new(
            None,
            Some(proxy),
            None,
            Some("codex.invalid"),
        );
        assert!(custom_client_auto_http_fallback_enabled(
            "https://codex.invalid/responses",
            &bypassed
        ));
    }

    fn http_test_auth() -> StoredAuth {
        StoredAuth {
            access: "test".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        }
    }

    fn http_test_context() -> RequestContext {
        RequestContext {
            req_id: "http-body-test".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        }
    }

    fn http_test_client(base_url: String, body_idle_timeout_ms: u64) -> CodexHttpClient {
        CodexHttpClient::new_for_test(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            base_url,
            100,
            body_idle_timeout_ms,
            0,
        )
    }

    fn buffered_test_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-5.6-sol".into(),
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

    fn buffered_request_with_texts(texts: &[&str]) -> ResponsesRequest {
        let mut request = buffered_test_request();
        request.input = texts
            .iter()
            .map(
                |text| super::super::translate::request::ResponsesInputItem::Message {
                    role: "user".to_string(),
                    content: vec![
                        super::super::translate::request::ResponsesContentPart::InputText {
                            text: (*text).to_string(),
                        },
                    ],
                },
            )
            .collect();
        request
    }

    async fn next_websocket_json(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> serde_json::Value {
        loop {
            match websocket.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload))) => {
                    websocket
                        .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                        .await
                        .unwrap();
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    return serde_json::from_str(&text).unwrap();
                }
                other => panic!("unexpected WebSocket request frame: {other:?}"),
            }
        }
    }

    async fn send_completed_websocket_response(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        response_id: &str,
    ) {
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": response_id,
                        "status": "completed",
                        "output": []
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
    }

    async fn send_nested_previous_response_missing(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {
                            "code": "previous_response_not_found",
                            "message": "Previous response not found"
                        }
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
    }

    fn authenticated_http_test_client(base_url: String) -> CodexHttpClient {
        let client = http_test_client(base_url, 100);
        client.auth_manager().set_test_auth(http_test_auth());
        client
    }

    /// How long a one-shot mock server waits for its request, and a test for it.
    const MOCK_DEADLINE: Duration = Duration::from_secs(10);

    /// A mock server task, aborted when the test leaves it, however it leaves.
    struct MockServer(tokio::task::JoinHandle<()>);

    impl MockServer {
        /// Answer one request with `response`. Accepting and reading give up after
        /// `MOCK_DEADLINE`.
        async fn answer_once(response: String) -> (std::net::SocketAddr, Self) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = tokio::time::timeout(MOCK_DEADLINE, listener.accept())
                    .await
                    .expect("mock server: no request arrived")
                    .unwrap();
                tokio::time::timeout(MOCK_DEADLINE, read_http_request(&mut stream))
                    .await
                    .expect("mock server: the request did not finish arriving");
                let _ = tokio::time::timeout(MOCK_DEADLINE, stream.write_all(response.as_bytes()))
                    .await;
            });
            (addr, Self(task))
        }

        /// Wait for the server to have answered, failing the test with its panic.
        async fn join(mut self) {
            tokio::time::timeout(MOCK_DEADLINE, &mut self.0)
                .await
                .expect("mock server did not finish")
                .unwrap();
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// The client's answer to a buffered request the backend rejects with 401, when
    /// the token endpoint answers the forced refresh with `refresh_status`.
    async fn forced_refresh_answer(refresh_status: &'static str) -> axum::response::Response {
        let (backend_addr, backend_server) = MockServer::answer_once(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_string(),
        )
        .await;
        let body = r#"{"error":"invalid_grant"}"#;
        let (token_addr, token_server) = MockServer::answer_once(format!(
            "HTTP/1.1 {refresh_status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let dir = tempfile::tempdir().unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            super::super::auth::token_store::CodexTokenStore::codex_cli(
                super::super::auth::token_store::CodexCliAuthStore::new(
                    dir.path().join("auth.json"),
                ),
            ),
            format!("http://{token_addr}/oauth/token"),
        );
        manager.set_test_auth(StoredAuth {
            refresh: "refresh-test".into(),
            ..http_test_auth()
        });
        let client = CodexHttpClient::new_with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            manager,
            format!("http://{backend_addr}/responses"),
        );
        let (body, context) = (buffered_test_request(), http_test_context());
        let request = client.post_codex_with_transport(
            &body,
            &context,
            None,
            crate::config::CodexTransport::Http,
        );
        let error = match tokio::time::timeout(MOCK_DEADLINE, request)
            .await
            .expect("the request did not finish")
        {
            Ok(_) => panic!("a failed forced refresh must fail the request"),
            Err(error) => error,
        };
        backend_server.join().await;
        token_server.join().await;
        super::super::map_codex_error_to_response(&error)
    }

    async fn error_body(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn a_forced_refresh_outage_after_a_401_answers_503() {
        let response = forced_refresh_answer("503 Service Unavailable").await;
        assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], TOKEN_REFRESH_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_forced_refresh_the_endpoint_refuses_after_a_401_answers_401() {
        let response = forced_refresh_answer("400 Bad Request").await;
        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "request ended before its body was complete");
            request.extend_from_slice(&chunk[..read]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                return request;
            }
        }
    }

    #[tokio::test]
    async fn image_request_refreshes_once_after_unauthorized() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let request = String::from_utf8_lossy(&request);
                if attempt == 0 {
                    assert!(request.contains("authorization: Bearer test"));
                    server_client.auth_manager().set_test_auth(StoredAuth {
                        access: "rotated".into(),
                        refresh: "rotated-refresh".into(),
                        account_id: Some("acct-rotated".into()),
                        expires: u64::MAX,
                    });
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    assert!(request.contains("authorization: Bearer rotated"));
                    assert!(request.contains("chatgpt-account-id: acct-rotated"));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap();
                }
            }
        });

        let response = client
            .post_image_json(
                &format!("http://{addr}"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_request_does_not_retry_server_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_image_json(
                &format!("http://{addr}"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_request_uses_fixed_path_oauth_and_json_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /root/images/generations HTTP/1.1"));
            assert!(headers.contains("authorization: Bearer test"));
            assert!(headers.contains("chatgpt-account-id: acct"));
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end + 4..]).unwrap();
            assert_eq!(body["model"], "gpt-image-2");
            assert_eq!(body["prompt"], "draw a fox");
            let response = br#"{"created":1,"data":[{"b64_json":"aW1n"}]}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(response).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_image_json(
                &format!("http://{addr}/root"),
                super::super::images::ImageOperation::Generation,
                &serde_json::json!({"model":"gpt-image-2","prompt":"draw a fox"}),
                &http_test_context(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    async fn write_http_chunk(stream: &mut tokio::net::TcpStream, body: &[u8]) {
        stream
            .write_all(format!("{:x}\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(body).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();
    }

    #[test]
    fn http_sse_decoder_handles_fragmented_crlf_and_done_marker() {
        let mut decoder = HttpSseDecoder::default();
        assert!(
            decoder
                .push(b"event: response.output_text.delta\r")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .push(b"\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\n\r\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(
            events[0]
                .payload
                .as_ref()
                .and_then(|payload| payload.get("delta"))
                .and_then(|value| value.as_str()),
            Some("ok")
        );

        let done = decoder.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(done.len(), 1);
        assert!(done[0].payload.is_none());
        decoder.finish().unwrap();
    }

    #[test]
    fn http_sse_size_limit_is_not_retryable() {
        assert!(!retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame exceeds the size limit",
        )));
        assert!(retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame contains invalid JSON",
        )));
        assert!(retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame contains invalid UTF-8",
        )));
    }

    #[tokio::test]
    async fn http_stream_forwards_event_before_terminal_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            write_http_chunk(
                &mut stream,
                b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
            )
            .await;
            release_rx.await.unwrap();
            write_http_chunk(
                &mut stream,
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{}}}\n\n",
            )
            .await;
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events(&buffered_test_request(), &http_test_context())
            .await
            .unwrap();

        let synthetic = events.recv().await.unwrap().unwrap();
        assert_eq!(
            synthetic.get("type").and_then(|value| value.as_str()),
            Some("keepalive")
        );
        let first_upstream = tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .expect("first upstream event must arrive before the response completes")
            .unwrap()
            .unwrap();
        assert_eq!(
            first_upstream.get("type").and_then(|value| value.as_str()),
            Some("response.output_item.added")
        );

        release_tx.send(()).unwrap();
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(
            terminal.get("type").and_then(|value| value.as_str()),
            Some("response.completed")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_returns_initial_status_without_retrying() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut attempts = 0_u32;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                let mut request = [0_u8; 16 * 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                attempts += 1;
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
            attempts
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let error = match client
            .stream_codex_http_events(&buffered_test_request(), &http_test_context())
            .await
        {
            Ok(_) => panic!("a failing status must surface as an error"),
            Err(error) => error,
        };

        assert_eq!(error.status, 503);
        assert_eq!(
            server.await.unwrap(),
            1,
            "a failing status goes back to the client without a second attempt"
        );
    }

    #[tokio::test]
    async fn http_stream_header_only_usage_limit_uses_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = br#"data: {"type":"error","status_code":429,"error":{"type":"usage_limit_reached","message":"weekly \"limit\" reached","resets_in_seconds":90}}

"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-codex-secondary-reset-after-seconds: 90\r\nx-codex-secondary-reset-at: 1789466238\r\nx-codex-secondary-window-minutes: 10080\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events(&buffered_test_request(), &http_test_context())
            .await
            .unwrap();

        assert_eq!(
            events.recv().await.unwrap().unwrap().get("type"),
            Some(&serde_json::json!("keepalive"))
        );
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(error.status, 429);
        let limit = error.usage_limit.expect("usage limit");
        assert_eq!(limit.message, "weekly \"limit\" reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_replaces_auth_and_preserves_json_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("authorization: Bearer test"));
            assert!(request.contains("chatgpt-account-id: acct"));
            assert!(request.contains("accept: application/json"));
            assert!(request.contains(r#""extra":{"kept":true}"#));
            let body = br#"{"id":"resp_native","object":"response"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/v1/responses"));
        let response = client
            .post_native_responses(
                &serde_json::json!({
                    "model": "gpt-5.4",
                    "input": "hello",
                    "stream": false,
                    "extra": {"kept": true}
                }),
                &http_test_context(),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.unwrap(),
            br#"{"id":"resp_native","object":"response"}"#.as_slice()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_refreshes_once_before_returning_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/v1/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16 * 1024];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                if attempt == 0 {
                    assert!(request.contains("authorization: Bearer test"));
                    server_client.auth_manager().set_test_auth(StoredAuth {
                        access: "rotated".into(),
                        refresh: "rotated-refresh".into(),
                        account_id: Some("acct-rotated".into()),
                        expires: u64::MAX,
                    });
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    assert!(request.contains("authorization: Bearer rotated"));
                    assert!(request.contains("chatgpt-account-id: acct-rotated"));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap();
                }
            }
        });

        let response = client
            .post_native_responses(
                &serde_json::json!({"model":"gpt-5.4","input":"hello"}),
                &http_test_context(),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap(), b"{}".as_slice());
        server.await.unwrap();
    }

    /// A lookup over a fixed set of variables, so these tests leave the
    /// process environment, which other tests build clients from, untouched.
    fn env_lookup(vars: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        let vars: std::collections::HashMap<&str, OsString> = vars.into_iter().collect();
        move |name| vars.get(name).cloned()
    }

    #[cfg(unix)]
    fn not_unicode() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b"127.0.0.1:\xff".to_vec())
    }

    /// The message `from_lookup` panics with for `vars`, or `None` when it
    /// returns.
    #[cfg(unix)]
    fn proxy_lookup_panic(vars: Vec<(&'static str, OsString)>) -> Option<String> {
        let lookup = env_lookup(vars);
        let payload =
            std::panic::catch_unwind(move || ProxyEnvironment::from_lookup(lookup)).err()?;
        Some(
            *payload
                .downcast::<String>()
                .expect("the panic message is a formatted string"),
        )
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_proxy_url_variables_are_invalid_proxy_urls() {
        for (name, uppercase) in [
            ("HTTP_PROXY", "HTTP_PROXY"),
            ("http_proxy", "HTTP_PROXY"),
            ("HTTPS_PROXY", "HTTPS_PROXY"),
            ("https_proxy", "HTTPS_PROXY"),
            ("ALL_PROXY", "ALL_PROXY"),
            ("all_proxy", "ALL_PROXY"),
        ] {
            assert_eq!(
                proxy_lookup_panic(vec![(name, not_unicode())]),
                Some(format!("invalid {uppercase} proxy URL")),
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_uppercase_proxy_variable_is_not_replaced_by_the_lowercase_one() {
        for (uppercase, lowercase, valid) in [
            ("HTTP_PROXY", "http_proxy", "127.0.0.1:3128"),
            ("HTTPS_PROXY", "https_proxy", "http://127.0.0.1:3129"),
            ("ALL_PROXY", "all_proxy", "socks5h://127.0.0.1:1080"),
        ] {
            assert_eq!(
                proxy_lookup_panic(vec![(uppercase, not_unicode()), (lowercase, valid.into())]),
                Some(format!("invalid {uppercase} proxy URL")),
                "{uppercase}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_no_proxy_spelling_counts_as_unset() {
        for name in ["NO_PROXY", "no_proxy"] {
            let proxies = ProxyEnvironment::from_lookup(env_lookup(vec![
                (name, not_unicode()),
                ("HTTPS_PROXY", "http://127.0.0.1:3129".into()),
            ]));
            assert_eq!(proxies.no_proxy_value, None, "{name}");
            assert!(proxies.no_proxy.is_none(), "{name}");
            assert_eq!(
                proxies.https_proxy,
                normalize_proxy_url("http://127.0.0.1:3129"),
                "{name}"
            );
        }

        let proxies = ProxyEnvironment::from_lookup(env_lookup(vec![
            ("NO_PROXY", not_unicode()),
            ("no_proxy", "localhost".into()),
        ]));
        assert_eq!(proxies.no_proxy_value.as_deref(), Some("localhost"));
        assert!(proxies.no_proxy.is_some());
    }

    #[test]
    fn unicode_proxy_variables_keep_their_precedence() {
        // The uppercase spelling wins, and an empty one still means no proxy
        // rather than falling through to the lowercase spelling.
        let proxies = ProxyEnvironment::from_lookup(env_lookup(vec![
            ("HTTP_PROXY", "127.0.0.1:3128".into()),
            ("http_proxy", "127.0.0.1:3130".into()),
            ("HTTPS_PROXY", "".into()),
            ("https_proxy", "127.0.0.1:3131".into()),
        ]));
        assert_eq!(proxies.http_proxy, normalize_proxy_url("127.0.0.1:3128"));
        assert_eq!(proxies.https_proxy, None);
        assert_eq!(proxies.all_proxy, None);
    }

    #[tokio::test]
    async fn http_client_does_not_follow_redirects() {
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let source_server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{target_addr}/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = codex_http_client(&ProxyEnvironment {
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: None,
            no_proxy_value: None,
        });
        let response = client
            .get(format!("http://{source_addr}/v1/models"))
            .header("x-gateway-token", "gw-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        source_server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_responses_does_not_follow_redirects() {
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let source_server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{target_addr}/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{source_addr}/v1/responses"));
        let response = client
            .post_native_responses(
                &serde_json::json!({"model":"gpt-5.4","input":"hello"}),
                &http_test_context(),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        source_server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_responses_transport_error_leaves_out_the_url() {
        // Nothing listens on port 1, so the send fails before any answer.
        let client = authenticated_http_test_client(
            "http://127.0.0.1:1/toy-secret-path/v1/responses?token=toy-secret-query".into(),
        );
        let error = client
            .post_native_responses(
                &serde_json::json!({"model":"gpt-5.4","input":"hello"}),
                &http_test_context(),
                false,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .starts_with("Native Responses transport error: "),
            "{error:?}"
        );
        let shown = format!("{} {:?}", error.message, error.detail);
        for secret in [
            "toy-secret-path",
            "toy-secret-query",
            "127.0.0.1",
            "for url",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[tokio::test]
    async fn buffered_missing_origin_retries_full_context_and_rebinds_exact_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "buffered-recovery-session".to_string(),
            "buffered-recovery-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_a").await;
            drop(first_websocket);

            let (second_socket, _) = listener.accept().await.unwrap();
            let mut second_websocket = tokio_tungstenite::accept_async(second_socket)
                .await
                .unwrap();
            request_tx
                .send(next_websocket_json(&mut second_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut second_websocket, "resp_b").await;
            request_tx
                .send(next_websocket_json(&mut second_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut second_websocket, "resp_c").await;
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
            .unwrap();
        let first_socket_id = first_response
            .socket_id
            .expect("first socket must be reusable");
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(second_candidate.origin_socket_id(), Some(first_socket_id));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let second_response = client
            .post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
            .unwrap();
        let second_socket_id = second_response
            .socket_id
            .expect("full-context retry socket must be reusable");
        assert_ne!(second_socket_id, first_socket_id);
        super::super::update_continuation_from_upstream(
            None,
            &second_candidate,
            None,
            &second_request,
            &second_response.body,
            second_response.socket_id,
            false,
        );

        let third_request = buffered_request_with_texts(&["one", "two", "three"]);
        let third_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &third_request,
            true,
        );
        assert_eq!(
            third_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_b")
        );
        assert_eq!(third_candidate.origin_socket_id(), Some(second_socket_id));
        let third_response = client
            .post_codex_with_transport(
                &third_request,
                &context,
                Some(&third_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
            .unwrap();
        assert_eq!(third_response.socket_id, Some(second_socket_id));

        let first_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(first_payload["input"].as_array().unwrap().len(), 1);
        assert!(retry_payload.get("previous_response_id").is_none());
        assert_eq!(retry_payload["input"].as_array().unwrap().len(), 2);
        assert_eq!(continued_payload["previous_response_id"], "resp_b");
        assert_eq!(continued_payload["input"].as_array().unwrap().len(), 1);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&third_candidate);
    }

    #[tokio::test]
    async fn buffered_nested_missing_response_retries_once_without_stale_previous_id() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("buffered-nested-recovery-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_nested_origin").await;
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut first_websocket).await;

            let (retry_socket, _) = listener.accept().await.unwrap();
            let mut retry_websocket = tokio_tungstenite::accept_async(retry_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut retry_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut retry_websocket).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(1_200), listener.accept())
                    .await
                    .is_err(),
                "nested missing-response recovery must not open a third socket"
            );
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_nested_origin")
        );
        let error = match client
            .post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
        {
            Ok(_) => {
                panic!("the bounded full-context retry must surface a repeated nested failure")
            }
            Err(error) => error,
        };
        assert_eq!(error.detail.as_deref(), Some("previous_response_not_found"));

        let first_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(
            continued_payload["previous_response_id"],
            "resp_nested_origin"
        );
        assert_eq!(continued_payload["input"].as_array().unwrap().len(), 1);
        assert!(retry_payload.get("previous_response_id").is_none());
        assert_eq!(retry_payload["input"].as_array().unwrap().len(), 2);
        assert!(request_rx.try_recv().is_err());
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    #[tokio::test]
    async fn live_nested_missing_response_retries_once_without_stale_previous_id() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "live-nested-recovery-session".to_string(),
            "live-nested-recovery-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_live_nested_origin")
                .await;
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut first_websocket).await;

            let (retry_socket, _) = listener.accept().await.unwrap();
            let mut retry_websocket = tokio_tungstenite::accept_async(retry_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut retry_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut retry_websocket).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(1_200), listener.accept())
                    .await
                    .is_err(),
                "live nested missing-response recovery must not open a third socket"
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_live_nested_origin")
        );
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &second_request,
                &context,
                Some(&second_candidate),
            )
            .await
            .unwrap();
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(error.detail.as_deref(), Some("previous_response_not_found"));
        assert!(events.used_full_context_retry());
        assert_eq!(events.socket_id(), None);

        let first_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(
            continued_payload["previous_response_id"],
            "resp_live_nested_origin"
        );
        assert_eq!(continued_payload["input"].as_array().unwrap().len(), 1);
        assert!(retry_payload.get("previous_response_id").is_none());
        assert_eq!(retry_payload["input"].as_array().unwrap().len(), 2);
        assert!(request_rx.try_recv().is_err());
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    #[tokio::test]
    async fn live_missing_origin_retries_once_with_full_context_and_actual_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-recovery-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one", "two"]);
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let continuation = super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id: reserved.turn_id(),
                previous_response_id: Some("resp_missing".to_string()),
                input_delta: Some(vec![request.input.last().unwrap().clone()]),
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner.clone()),
            Some(u64::MAX),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut websocket, "resp_live_retry").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.used_full_context_retry());
        let socket_id = events
            .socket_id()
            .expect("successful internal retry must publish its socket");
        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            Some(socket_id)
        );

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    /// A Codex origin that answers one WebSocket request with `resp_a`, then
    /// the next Ping (the pooled-socket validation), and then stops reading.
    /// Its small receive buffer keeps a large frame from fitting in the
    /// socket buffers. Every later connection is recorded by its first bytes
    /// and left unanswered.
    async fn spawn_origin_that_stops_reading_after_one_turn() -> (
        std::net::SocketAddr,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(16).unwrap();
        let addr = listener.local_addr().unwrap();
        let later_connections = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = later_connections.clone();
        let task = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            next_websocket_json(&mut websocket).await;
            send_completed_websocket_response(&mut websocket, "resp_a").await;
            let answer_validation = async {
                while let Some(Ok(message)) = websocket.next().await {
                    if let tokio_tungstenite::tungstenite::Message::Ping(payload) = message {
                        websocket
                            .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                            .await
                            .unwrap();
                        break;
                    }
                }
                futures_util::future::pending::<()>().await;
            };
            let record_later_connections = async {
                let mut held = Vec::new();
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let mut head = [0_u8; 16];
                    let read = stream.read(&mut head).await.unwrap_or(0);
                    recorded
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&head[..read]).into_owned());
                    held.push(stream);
                }
            };
            tokio::join!(answer_validation, record_later_connections);
        });
        (addr, later_connections, task)
    }

    fn assert_no_later_connection(later_connections: &std::sync::Mutex<Vec<String>>) {
        let later = later_connections.lock().unwrap().clone();
        assert!(
            later.is_empty(),
            "the timed-out request reached the origin again: {later:?}"
        );
    }

    // Part of a timed-out frame may already be on the socket, so neither a
    // full-context resend on a new socket nor an HTTP replay may follow.
    #[tokio::test]
    async fn buffered_request_write_timeout_is_neither_resent_nor_replayed_over_http() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("buffered-write-timeout-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let (addr, later_connections, origin) =
            spawn_origin_that_stops_reading_after_one_turn().await;

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let large_text = "x".repeat(16 * 1024 * 1024);
        let second_request = buffered_request_with_texts(&["one", &large_text]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_a")
        );
        let _write_timeout =
            super::super::websocket::RequestWriteTimeoutOverride::set(Duration::from_millis(300));
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            client.post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the request must end within its write budget");
        let error = match result {
            Ok(_) => panic!("expected the request write to time out"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(super::super::websocket::WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 300ms"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_no_later_connection(&later_connections);

        origin.abort();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    #[tokio::test]
    async fn a_buffered_websocket_answer_over_the_limit_is_neither_resent_nor_replayed_over_http() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("buffered-limit-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let server = {
            let (upgrades, posts) = (upgrades.clone(), posts.clone());
            tokio::spawn(async move {
                let mut sockets = Vec::new();
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut first = [0_u8; 1];
                    if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    if first[0] == b'G' {
                        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
                        upgrades.fetch_add(1, Ordering::SeqCst);
                        let _request = next_websocket_json(&mut websocket).await;
                        send_completed_websocket_response(&mut websocket, "resp_limit").await;
                        sockets.push(websocket);
                        continue;
                    }
                    let _request = read_http_request(&mut stream).await;
                    posts.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        let client = authenticated_http_test_client(format!("http://{addr}/responses"))
            .with_buffered_response_limit(1);
        let request = buffered_test_request();
        let candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let result = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &request,
                &http_test_context(),
                Some(&candidate),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the request did not finish");
        let error = match result {
            Ok(_) => panic!("an answer over the limit must fail"),
            Err(error) => error,
        };

        assert_eq!(error.status, 502);
        assert_eq!(
            error.detail.as_deref(),
            Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL)
        );
        assert_eq!(upgrades.load(Ordering::SeqCst), 1);
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            None
        );

        server.abort();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&candidate);
    }

    /// Refusals a buffered answer can end with, by name: a spent window, a
    /// terminal snapshot naming one spent window, and a quota failure.
    fn buffered_refusal_events() -> [(&'static str, String); 3] {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        [
            (
                "spent window",
                serde_json::json!({
                    "type": "error",
                    "status_code": 429,
                    "error": {
                        "type": "usage_limit_reached",
                        "message": "The usage limit has been reached",
                        "resets_at": now + 10_000
                    }
                })
                .to_string(),
            ),
            (
                "terminal snapshot",
                serde_json::json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {
                        "allowed": false,
                        "limit_reached": true,
                        "primary": {"used_percent": 100.0, "window_minutes": 300, "reset_at": now + 10_000},
                        "secondary": {"used_percent": 40.0, "window_minutes": 10080, "reset_at": now + 500_000}
                    }
                })
                .to_string(),
            ),
            (
                "quota",
                serde_json::json!({
                    "type": "error",
                    "status_code": 429,
                    "error": {
                        "type": "insufficient_quota",
                        "code": "insufficient_quota",
                        "message": "You exceeded your current quota."
                    }
                })
                .to_string(),
            ),
        ]
    }

    /// What the client is answered for `error`: status, sorted headers, body.
    async fn answered_parts(
        error: &CodexError,
    ) -> (http::StatusCode, Vec<(String, String)>, serde_json::Value) {
        let response = super::super::map_codex_error_to_response(error);
        let status = response.status();
        let mut headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        headers.sort();
        (status, headers, error_body(response).await)
    }

    /// Asserts that `answer` is the refusal `case` names, not the 502 of an
    /// answer over the limit.
    fn assert_refusal_answer(
        case: &str,
        (status, headers, body): &(http::StatusCode, Vec<(String, String)>, serde_json::Value),
    ) {
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(*status, http::StatusCode::TOO_MANY_REQUESTS, "{case}");
        assert_eq!(body["error"]["type"], "rate_limit_error", "{case}");
        assert_eq!(header("x-should-retry"), Some("false"), "{case}");
        let unified = header("anthropic-ratelimit-unified-status");
        if case == "quota" {
            assert_eq!(unified, None, "{case}");
        } else {
            assert_eq!(unified, Some("rejected"), "{case}");
        }
    }

    /// Sends one buffered request over a WebSocket answered by `events`,
    /// with the client's buffered limit set to `limit` when given. Returns
    /// the error, the upgrades and HTTP requests the origin saw, and whether
    /// a socket stayed pooled. The caller holds the registry and pool locks.
    async fn buffered_websocket_answer(
        events: Vec<String>,
        limit: Option<usize>,
    ) -> (CodexError, usize, usize, bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let owner = ConversationIdentity::Main("buffered-refusal-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let server = {
            let (upgrades, posts) = (upgrades.clone(), posts.clone());
            tokio::spawn(async move {
                let mut sockets = Vec::new();
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut first = [0_u8; 1];
                    if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    if first[0] == b'G' {
                        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
                        upgrades.fetch_add(1, Ordering::SeqCst);
                        let _request = next_websocket_json(&mut websocket).await;
                        for event in &events {
                            websocket
                                .send(tokio_tungstenite::tungstenite::Message::Text(event.clone()))
                                .await
                                .unwrap();
                        }
                        sockets.push(websocket);
                        continue;
                    }
                    let _request = read_http_request(&mut stream).await;
                    posts.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        if let Some(limit) = limit {
            client = client.with_buffered_response_limit(limit);
        }
        let request = buffered_test_request();
        let candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let result = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &request,
                &http_test_context(),
                Some(&candidate),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the request did not finish");
        let pooled = super::super::websocket::pooled_socket_id_for_tests(&owner).is_some();
        server.abort();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&candidate);
        let error = match result {
            Ok(_) => panic!("a refusal must fail the request"),
            Err(error) => error,
        };
        (
            error,
            upgrades.load(Ordering::SeqCst),
            posts.load(Ordering::SeqCst),
            pooled,
        )
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_buffered_websocket_refusal_that_crosses_the_limit_keeps_its_answer() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        let created = r#"{"type":"response.created","response":{"id":"resp_refusal"}}"#;
        // The created event fits; the refusal after it crosses the limit.
        let limit = format!("data: {created}\n\n").len() + 1;
        for (case, refusal) in buffered_refusal_events() {
            let events = vec![created.to_string(), refusal];
            let mut answers = Vec::new();
            for limit in [None, Some(limit)] {
                super::super::rate_limits::clear_for_tests();
                let (error, upgrades, posts, pooled) =
                    buffered_websocket_answer(events.clone(), limit).await;
                assert_eq!((upgrades, posts, pooled), (1, 0, false), "{case}");
                let answer = answered_parts(&error).await;
                answers.push((answer, published_utilizations(Some("acct"))));
            }
            super::super::rate_limits::clear_for_tests();

            assert_eq!(answers[0], answers[1], "{case}");
            let (answer, gauge) = &answers[1];
            assert_refusal_answer(case, answer);
            if case == "terminal snapshot" {
                assert_eq!(gauge[0].as_deref(), Some("1.0000"));
                assert_eq!(gauge[1].as_deref(), Some("0.4000"));
            }
        }
    }

    #[tokio::test]
    async fn live_request_write_timeout_is_neither_resent_nor_replayed_over_http() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-write-timeout-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let (addr, later_connections, origin) =
            spawn_origin_that_stops_reading_after_one_turn().await;

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let large_text = "x".repeat(16 * 1024 * 1024);
        let second_request = buffered_request_with_texts(&["one", &large_text]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_a")
        );
        let _write_timeout =
            super::super::websocket::RequestWriteTimeoutOverride::set(Duration::from_millis(300));
        let error = tokio::time::timeout(Duration::from_secs(30), async {
            match client
                .stream_codex_auto_events_for_owner(
                    &second_request,
                    &context,
                    Some(&second_candidate),
                )
                .await
            {
                Err(error) => error,
                Ok((mut events, _)) => {
                    let item = events.recv().await;
                    assert!(!events.used_full_context_retry());
                    match item {
                        Some(Err(error)) => error,
                        _ => panic!("expected the request write to time out"),
                    }
                }
            }
        })
        .await
        .expect("the request must end within its write budget");

        assert_eq!(
            error.detail.as_deref(),
            Some(super::super::websocket::WEBSOCKET_WRITE_TIMEOUT_DETAIL)
        );
        assert_eq!(
            error.message,
            "WebSocket response start timeout after 300ms"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_no_later_connection(&later_connections);

        origin.abort();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    #[tokio::test]
    async fn public_ownerless_candidate_never_writes_previous_id_to_websocket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let request = buffered_request_with_texts(&["one", "two"]);
        let continuation = super::super::continuation::ContinuationCandidate {
            turn_id: Some(17),
            previous_response_id: Some("resp_unproven".to_string()),
            input_delta: Some(vec![request.input.last().unwrap().clone()]),
            input_delta_count: 1,
            disabled_reason: None,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut websocket, "resp_public_full_context").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "an ownerless stale candidate must use one full-context socket"
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut context = http_test_context();
        context.session_id = Some("must-not-be-derived-as-owner".to_string());
        let mut events = client
            .stream_codex_websocket_events(&request, &context, Some(&continuation))
            .await
            .unwrap();
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");

        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn live_missing_origin_does_not_enter_second_full_context_loop() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-bounded-recovery-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one", "two"]);
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let continuation = super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id: reserved.turn_id(),
                previous_response_id: Some("resp_missing".to_string()),
                input_delta: Some(vec![request.input.last().unwrap().clone()]),
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner.clone()),
            Some(u64::MAX),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            websocket.close(None).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(
            error.detail.as_deref(),
            Some(super::super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
        );
        assert!(events.used_full_context_retry());
        assert_eq!(events.socket_id(), None);
        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn dropping_live_receiver_clears_reserved_turn() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-drop-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut websocket).await;
            request_seen_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = websocket.close(None).await;
        });

        let events = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )))
        .stream_codex_websocket_events_for_owner(
            &request,
            &http_test_context(),
            Some(&continuation),
        )
        .await
        .unwrap();
        request_seen_rx.await.unwrap();
        drop(events);

        tokio::time::timeout(Duration::from_secs(1), async {
            while super::super::continuation::is_current_turn_for_owner(&continuation) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the live receiver must clear its reserved turn");
        server.await.unwrap();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn dropping_retry_handoff_receiver_preserves_reserved_turn() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-retry-handoff-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut websocket).await;
            request_seen_tx.send(()).unwrap();
            while websocket.next().await.is_some() {}
            socket_closed_tx.send(()).unwrap();
        });

        let events = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )))
        .stream_codex_websocket_events_for_owner(
            &request,
            &http_test_context(),
            Some(&continuation),
        )
        .await
        .unwrap();
        request_seen_rx.await.unwrap();
        events.mark_provider_retry_handoff();
        drop(events);

        tokio::time::timeout(Duration::from_secs(1), socket_closed_rx)
            .await
            .expect("marked receiver drop must close only the abandoned attempt socket")
            .expect("socket-close acknowledgement sender dropped");
        assert!(super::super::continuation::is_current_turn_for_owner(
            &continuation
        ));
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&continuation);
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn delayed_retry_handoff_cleanup_preserves_replacement_state_and_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-retry-cleanup-race-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_replacement_tx, release_replacement_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            let _ = next_websocket_json(&mut first_websocket).await;
            send_completed_websocket_response(&mut first_websocket, "resp_attempt_a").await;
            drop(first_websocket);

            let (replacement_socket, _) = listener.accept().await.unwrap();
            let mut replacement_websocket = tokio_tungstenite::accept_async(replacement_socket)
                .await
                .unwrap();
            let _ = next_websocket_json(&mut replacement_websocket).await;
            send_completed_websocket_response(&mut replacement_websocket, "resp_attempt_b").await;
            let _ = release_replacement_rx.await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));

        let mut attempt_a = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let terminal_a = attempt_a.recv().await.unwrap().unwrap();
        assert_eq!(terminal_a["response"]["id"], "resp_attempt_a");
        let socket_a = attempt_a.socket_id().expect("attempt A socket ID");
        super::super::websocket::invalidate_codex_websocket_pool_socket(
            &continuation,
            Some(socket_a),
        );

        let mut attempt_b = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let terminal_b = attempt_b.recv().await.unwrap().unwrap();
        assert_eq!(terminal_b["response"]["id"], "resp_attempt_b");
        let socket_b = attempt_b.socket_id().expect("attempt B socket ID");
        assert_ne!(socket_a, socket_b);
        super::super::continuation::record_continuation_for_owner(
            &continuation,
            &request,
            Some("resp_attempt_b"),
            Some(socket_b),
            &[],
        );

        let (_handoff_tx, handoff_rx) =
            tokio::sync::mpsc::channel::<Result<serde_json::Value, CodexError>>(1);
        let (handoff_stream, handoff_publisher) =
            super::super::websocket::CodexWebSocketEventStream::pending(handoff_rx);
        handoff_stream.mark_provider_retry_handoff();
        let cleanup_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let cleanup_task_barrier = cleanup_barrier.clone();
        let cleanup_continuation = continuation.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_task_barrier.wait().await;
            super::super::websocket::invalidate_codex_websocket_pool_socket(
                &cleanup_continuation,
                Some(socket_a),
            );
            abort_abandoned_live_continuation(Some(&cleanup_continuation), &handoff_publisher);
        });

        cleanup_barrier.wait().await;
        cleanup.await.unwrap();
        assert!(super::super::continuation::is_current_turn_for_owner(
            &continuation
        ));
        assert!(super::super::continuation::has_continuation_for_owner_for_tests(&owner));
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            Some(socket_b)
        );

        let _ = release_replacement_tx.send(());
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&continuation);
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn auto_clears_missing_origin_before_ordinary_http_fallback() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "auto-recovery-session".to_string(),
            "auto-recovery-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one", "two"]);
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let continuation = super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id: reserved.turn_id(),
                previous_response_id: Some("resp_missing".to_string()),
                input_delta: Some(vec![request.input.last().unwrap().clone()]),
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner.clone()),
            Some(u64::MAX),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let websocket_request = read_http_request(&mut websocket).await;
            assert!(String::from_utf8_lossy(&websocket_request).starts_with("GET "));
            websocket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(websocket);

            let (mut http, _) = listener.accept().await.unwrap();
            let http_request = read_http_request(&mut http).await;
            let body_start = http_request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: serde_json::Value =
                serde_json::from_slice(&http_request[body_start..]).unwrap();
            assert!(body.get("previous_response_id").is_none());
            assert_eq!(body["input"].as_array().unwrap().len(), 2);
            let response_body =
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            http.write_all(response.as_bytes()).await.unwrap();
            http.write_all(response_body).await.unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &request,
                &http_test_context(),
                Some(&continuation),
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        assert_eq!(response.transport, ActualTransport::Http);
        assert_eq!(response.socket_id, None);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn buffered_http_returns_retryable_status_without_retrying() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut attempts = 0_u32;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                let mut request = [0_u8; 16 * 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                attempts += 1;
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 5\r\nretry-after: 7\r\nconnection: close\r\n\r\nretry",
                    )
                    .await
                    .unwrap();
            }
            attempts
        });

        let error = match authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Ok(_) => panic!("a failing status must surface as an error"),
            Err(error) => error,
        };
        assert_eq!(error.status, 503);
        assert_eq!(error.retry_after.as_deref(), Some("7"));
        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn standalone_search_posts_json_to_alpha_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /alpha/search HTTP/1.1"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("accept: application/json")
            );
            assert!(headers.contains("authorization: Bearer test"));
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end + 4..]).unwrap();
            assert_eq!(body["model"], "gpt-5.6-luna");
            assert!(body.get("reasoning").is_none());
            assert_eq!(body["commands"]["search_query"][0]["q"], "find Codex");

            let response = serde_json::to_vec(&serde_json::json!({
                "encrypted_output": "opaque",
                "output": "search output",
                "results": [{
                    "type": "text_result",
                    "ref_id": "turn0search0",
                    "url": "https://example.com",
                    "title": "Example"
                }]
            }))
            .unwrap();
            let response_headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(response_headers.as_bytes()).await.unwrap();
            stream.write_all(&response).await.unwrap();
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let request = super::super::search::SearchRequest {
            id: "session".to_string(),
            model: "gpt-5.6-luna".to_string(),
            reasoning: None,
            input: None,
            commands: super::super::search::SearchCommands {
                search_query: vec![super::super::search::SearchQuery {
                    q: "find Codex".to_string(),
                }],
            },
            settings: super::super::search::SearchSettings {
                filters: None,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: 2_500,
        };
        let response = client
            .post_search(&request, &http_test_context())
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.output, "search output");
        assert_eq!(response.results.unwrap().len(), 1);
    }

    /// A search answer of exactly the cap is read; one byte more fails the call
    /// as an unreadable body does, and the call is not sent again.
    #[tokio::test]
    async fn standalone_search_body_is_read_up_to_its_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut answer = serde_json::to_vec(&serde_json::json!({
            "encrypted_output": "opaque",
            "output": "search output",
            "results": []
        }))
        .unwrap();
        answer.resize(answer.len() + 32, b' ');
        let length = answer.len();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let served = attempts.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                served.fetch_add(1, Ordering::SeqCst);
                read_http_request(&mut stream).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
                );
                // Over the cap the client stops reading, so a write may fail.
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&answer).await;
            }
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_search_capped(&search_test_request(), &http_test_context(), length)
            .await
            .unwrap();
        assert_eq!(response.output, "search output");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        let error = client
            .post_search_capped(&search_test_request(), &http_test_context(), length - 1)
            .await
            .unwrap_err();
        assert_eq!(error.status, 0);
        assert_eq!(
            error.message,
            format!(
                "Transport error reading Codex search response body: response body exceeded {} bytes",
                length - 1
            )
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn standalone_search_network_error_leaves_out_the_url() {
        // Nothing listens on port 1, so the send fails before any answer.
        let client = authenticated_http_test_client(
            "http://127.0.0.1:1/toy-secret-path/responses?token=toy-secret-query".into(),
        );
        let error = client
            .post_search(&search_test_request(), &http_test_context())
            .await
            .unwrap_err();
        assert!(
            error.message.starts_with("Codex search network error: "),
            "{error:?}"
        );
        let shown = format!("{} {:?}", error.message, error.detail);
        for secret in [
            "toy-secret-path",
            "toy-secret-query",
            "127.0.0.1",
            "for url",
        ] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[tokio::test]
    async fn auto_falls_back_to_http_after_statusful_websocket_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = websocket.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(
                String::from_utf8_lossy(&request[..read])
                    .to_ascii_lowercase()
                    .contains("upgrade: websocket")
            );
            websocket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 13\r\nconnection: close\r\n\r\npolicy denied",
                )
                .await
                .unwrap();
            drop(websocket);

            let (mut http, _) = listener.accept().await.unwrap();
            let read = http.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
            let body = b"data: keep\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            http.write_all(response.as_bytes()).await.unwrap();
            http.write_all(body).await.unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn over_budget_retry_after_stops_without_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nretry-after: 120\r\nconnection: close\r\n\r\nstop",
                )
                .await
                .unwrap();
        });

        let error = match authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Ok(_) => panic!("over-budget Retry-After should propagate"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(error.status, 503);
        assert_eq!(error.retry_after.as_deref(), Some("120"));
    }

    #[tokio::test]
    async fn buffered_http_rejects_non_retryable_error_status_before_sse_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = br#"{"error":{"message":"Model not found gpt-test"}}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        server.await.unwrap();
        let error = match result {
            Ok(_) => panic!("non-success HTTP status must not reach the SSE reducer"),
            Err(error) => error,
        };

        assert_eq!(error.status, 404);
        assert_eq!(error.detail.as_deref(), Some("Model not found gpt-test"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
    }

    #[test]
    fn status_error_preserves_buffered_websocket_event_message() {
        let error = codex_status_error(
            CodexResponse {
                body: b"data: {\"type\":\"error\",\"error\":{\"status\":400,\"message\":\"bad request\"}}\n\n"
                    .to_vec(),
                status: 400,
                headers: Vec::new(),
                transport: ActualTransport::WebSocket,
            },
            None,
        );

        assert_eq!(error.status, 400);
        assert_eq!(error.detail.as_deref(), Some("bad request"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedWebSocket);
    }

    #[tokio::test]
    async fn http_transport_routes_on_the_conversation_scope() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .unwrap();
            String::from_utf8_lossy(&request[..read]).to_ascii_lowercase()
        });

        let mut ctx = http_test_context();
        ctx.session_id = Some("parent-session".to_string());
        http_test_client(format!("http://{addr}/responses"), 80)
            .attempt_post_http(&http_test_auth(), "{}", &ctx, false, Some("agent-scope"))
            .await
            .expect("request should reach the mock upstream");
        let request = server.await.unwrap();

        assert!(request.contains("session-id: agent-scope"), "{request}");
        assert!(request.contains("thread-id: agent-scope"), "{request}");
        assert!(!request.contains("session_id:"), "{request}");
        assert!(
            request.contains("x-client-request-id: agent-scope"),
            "{request}"
        );
        assert!(
            request.contains("x-codex-window-id: agent-scope:0"),
            "{request}"
        );
        assert!(!request.contains("parent-session"), "{request}");
    }

    #[tokio::test]
    async fn active_http_body_can_exceed_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in [b"a".as_slice(), b"b", b"c"] {
                stream.write_all(b"1\r\n").await.unwrap();
                stream.write_all(chunk).await.unwrap();
                stream.write_all(b"\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(45)).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        });

        let response = http_test_client(format!("http://{addr}/responses"), 80)
            .attempt_post_http(&http_test_auth(), "{}", &http_test_context(), false, None)
            .await
            .expect("active body should not hit a whole-request timeout");
        server.await.unwrap();

        assert_eq!(response.body, b"abc");
    }

    #[tokio::test]
    async fn stalled_http_body_hits_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let result = http_test_client(format!("http://{addr}/responses"), 30)
            .attempt_post_http(&http_test_auth(), "{}", &http_test_context(), false, None)
            .await;
        server.await.unwrap();
        let error = result.err().expect("stalled body should time out");

        assert!(error.message.contains("next Codex response body chunk"));
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
    }

    #[tokio::test]
    async fn reset_http_body_returns_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\npartial")
                .await
                .unwrap();
        });

        let result = http_test_client(format!("http://{addr}/responses"), 100)
            .attempt_post_http(&http_test_auth(), "{}", &http_test_context(), false, None)
            .await;
        server.await.unwrap();
        let error = result.err().expect("truncated body should fail");

        assert!(
            error
                .message
                .contains("Transport error reading Codex response body")
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
    }

    /// A path and query standing in for credentials in the upstream URL.
    const URL_SECRET_TARGET: &str = "/toy-secret-path?token=toy-secret-query";

    /// An origin that closes every connection unanswered, so each request to it
    /// fails while it is sent.
    fn spawn_closing_origin(listener: TcpListener) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        })
    }

    async fn assert_names_no_url(error: &CodexError) {
        for text in [Some(error.message.as_str()), error.detail.as_deref()]
            .into_iter()
            .flatten()
        {
            assert!(!text.contains("toy-secret"), "{text}");
        }
        let body = error_body(super::super::map_codex_error_to_response(error)).await;
        assert!(!body.to_string().contains("toy-secret"), "{body}");
    }

    #[tokio::test]
    async fn a_failed_buffered_http_send_names_no_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_closing_origin(listener);
        let client = authenticated_http_test_client(format!("http://{addr}{URL_SECRET_TARGET}"));
        let (body, context) = (buffered_test_request(), http_test_context());

        let request = client.post_codex_with_transport(
            &body,
            &context,
            None,
            crate::config::CodexTransport::Http,
        );
        let error = match tokio::time::timeout(MOCK_DEADLINE, request)
            .await
            .expect("the request did not finish")
        {
            Ok(_) => panic!("a closed connection must fail the request"),
            Err(error) => error,
        };
        server.abort();

        assert_eq!(error.status, 0);
        assert_names_no_url(&error).await;
    }

    #[tokio::test]
    async fn a_failed_live_http_start_names_no_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn_closing_origin(listener);
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}{URL_SECRET_TARGET}"
        )));

        let (body, context) = (buffered_test_request(), http_test_context());
        let start = client.stream_codex_http_events(&body, &context);
        let error = match tokio::time::timeout(MOCK_DEADLINE, start)
            .await
            .expect("the request did not start")
        {
            Ok(_) => panic!("a closed connection must fail the start"),
            Err(error) => error,
        };
        server.abort();

        assert_eq!(error.status, 0);
        assert_names_no_url(&error).await;
    }

    /// An origin that answers every HTTP request with `body` and counts them.
    fn spawn_fixed_body_origin(
        listener: TcpListener,
        body: String,
        posts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _request = read_http_request(&mut stream).await;
                posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        })
    }

    #[tokio::test]
    async fn a_buffered_http_body_one_byte_over_the_limit_answers_502_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let answer =
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let server = spawn_fixed_body_origin(listener, answer.to_string(), posts.clone());
        let (body, context) = (buffered_test_request(), http_test_context());

        let over = authenticated_http_test_client(url.clone())
            .with_buffered_response_limit(answer.len() - 1);
        let request = over.post_codex_with_transport(
            &body,
            &context,
            None,
            crate::config::CodexTransport::Http,
        );
        let error = match tokio::time::timeout(MOCK_DEADLINE, request)
            .await
            .expect("the request did not finish")
        {
            Ok(_) => panic!("a body over the limit must fail"),
            Err(error) => error,
        };
        assert_eq!(error.status, 502);
        assert_eq!(
            error.detail.as_deref(),
            Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL)
        );
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        let response = super::super::map_codex_error_to_response(&error);
        assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
        let answered = error_body(response).await;
        assert_eq!(answered["error"]["type"], "api_error");
        assert_eq!(
            answered["error"]["message"],
            BUFFERED_RESPONSE_TOO_LARGE_DETAIL
        );

        let at_limit =
            authenticated_http_test_client(url).with_buffered_response_limit(answer.len());
        let request = at_limit.post_codex_with_transport(
            &body,
            &context,
            None,
            crate::config::CodexTransport::Http,
        );
        let response = tokio::time::timeout(MOCK_DEADLINE, request)
            .await
            .expect("the request did not finish")
            .expect("a body at the limit must be answered");
        server.abort();
        assert_eq!(response.status, 200);
        assert_eq!(posts.load(Ordering::SeqCst), 2);
    }

    /// Sends one buffered HTTP request answered by `body`, with the client's
    /// buffered limit set to `limit` when given. Returns the error and the
    /// requests the origin saw.
    async fn buffered_http_answer(body: String, limit: Option<usize>) -> (CodexError, usize) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = spawn_fixed_body_origin(listener, body, posts.clone());
        let mut client = authenticated_http_test_client(url);
        if let Some(limit) = limit {
            client = client.with_buffered_response_limit(limit);
        }
        let (request, context) = (buffered_test_request(), http_test_context());
        let result = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &request,
                &context,
                None,
                crate::config::CodexTransport::Http,
            ),
        )
        .await
        .expect("the request did not finish");
        server.abort();
        let error = match result {
            Ok(_) => panic!("a refusal must fail the request"),
            Err(error) => error,
        };
        (error, posts.load(std::sync::atomic::Ordering::SeqCst))
    }

    const HTTP_LIMIT_CREATED: &str =
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_http\"}}\n\n";

    /// An event that only makes the body larger.
    fn http_padding_event() -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"type": "response.output_text.delta", "delta": "x".repeat(256)})
        )
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_buffered_http_refusal_held_when_the_limit_is_crossed_keeps_its_answer() {
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        for (case, refusal) in buffered_refusal_events() {
            let held = format!("{HTTP_LIMIT_CREATED}data: {refusal}\n\n");
            // The refusal is held whole; the padding after it crosses the limit.
            let limit = held.len() + 8;
            let body = format!("{held}{}", http_padding_event());
            let mut answers = Vec::new();
            for limit in [None, Some(limit)] {
                super::super::rate_limits::clear_for_tests();
                let (error, posts) = buffered_http_answer(body.clone(), limit).await;
                assert_eq!(posts, 1, "{case}");
                let answer = answered_parts(&error).await;
                answers.push((answer, published_utilizations(Some("acct"))));
            }
            super::super::rate_limits::clear_for_tests();

            assert_eq!(answers[0], answers[1], "{case}");
            let (answer, gauge) = &answers[1];
            assert_refusal_answer(case, answer);
            if case == "terminal snapshot" {
                assert_eq!(gauge[0].as_deref(), Some("1.0000"));
                assert_eq!(gauge[1].as_deref(), Some("0.4000"));
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_buffered_http_refusal_wholly_past_the_limit_is_not_seen() {
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        super::super::rate_limits::clear_for_tests();
        let [(_, refusal), ..] = buffered_refusal_events();
        let padding = http_padding_event();
        let body = format!("{HTTP_LIMIT_CREATED}{padding}data: {refusal}\n\n");
        let limit = HTTP_LIMIT_CREATED.len() + padding.len() / 2;

        let (error, posts) = buffered_http_answer(body, Some(limit)).await;
        let gauge = published_utilizations(Some("acct"));
        super::super::rate_limits::clear_for_tests();

        assert_eq!(posts, 1);
        assert_eq!(error.status, 502);
        assert_eq!(
            error.detail.as_deref(),
            Some(BUFFERED_RESPONSE_TOO_LARGE_DETAIL)
        );
        assert!(error.usage_limit.is_none());
        assert_eq!(gauge, [None, None]);
    }

    #[test]
    fn codex_error_display() {
        let err = CodexError {
            status: 429,
            message: "Rate limited".to_string(),
            detail: Some("body".to_string()),
            retry_after: Some("5".to_string()),
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::Http,
        };
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
    }

    #[test]
    fn websocket_pre_request_502_is_retryable() {
        let err = CodexError {
            status: 502,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: Some("3".to_string()),
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn proxy_tunnel_rejection_is_not_retried_or_used_for_http_fallback() {
        let err = CodexError {
            status: 0,
            message: "WebSocket proxy tunnel was rejected".to_string(),
            detail: Some(
                super::super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string(),
            ),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(!is_retryable_transport_error(&err));
        assert!(!should_fallback_to_http(&err));
    }

    #[test]
    fn websocket_pre_request_statusless_error_is_retryable() {
        let err = CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn websocket_pre_request_400_is_not_retryable() {
        let err = CodexError {
            status: 400,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(!is_retryable_transport_error(&err));
    }

    #[test]
    fn statusless_transport_error_matching_is_case_insensitive() {
        let err = CodexError {
            status: 0,
            message: "WebSocket protocol error: Connection reset without closing handshake"
                .to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn keepalive_failure_is_retryable_with_full_context() {
        let err = CodexError {
            status: 0,
            message: "WebSocket keepalive error: test write failed".to_string(),
            detail: Some(super::super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
        assert!(is_continuation_retry_error(&err));
    }

    #[test]
    fn statusless_broken_pipe_is_retryable() {
        let err = CodexError {
            status: 0,
            message: "WebSocket stream error: IO error: Broken pipe (os error 32)".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn image_headers_reuse_oauth_without_responses_beta_headers() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        };
        let headers = build_codex_image_headers(&auth, &http_test_context()).unwrap();

        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok"
        );
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct");
        assert_eq!(
            headers.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            headers.get(http::header::ACCEPT).unwrap(),
            "application/json"
        );
        assert!(headers.get("openai-beta").is_none());
        assert!(headers.get("x-codex-beta-features").is_none());
    }

    #[test]
    fn forwarded_client_headers_are_sensitive_and_never_replace_the_proxys_own() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, "Bearer tok".parse().unwrap());
        let mut client = http::HeaderMap::new();
        client.insert(
            http::header::AUTHORIZATION,
            "Bearer client".parse().unwrap(),
        );
        client.insert("x-gateway-token", "gw-secret".parse().unwrap());
        client.append("x-multi", "one".parse().unwrap());
        client.append("x-multi", "two".parse().unwrap());
        client.insert(http::header::HOST, "127.0.0.1:18765".parse().unwrap());
        client.insert(http::header::CONTENT_LENGTH, "42".parse().unwrap());
        let names = [
            "authorization",
            "x-gateway-token",
            "x-multi",
            "x-absent",
            "host",
            "content-length",
        ]
        .map(http::HeaderName::from_static);

        forward_client_headers(&mut headers, &client, &names);

        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok"
        );
        assert!(headers.get(http::header::HOST).is_none());
        assert!(headers.get(http::header::CONTENT_LENGTH).is_none());
        let token = headers.get("x-gateway-token").unwrap();
        assert_eq!(token, "gw-secret");
        assert!(token.is_sensitive());
        assert_eq!(headers.get_all("x-multi").iter().count(), 2);
        assert!(headers.get("x-absent").is_none());
        assert_eq!(
            headers_to_json(&headers)["x-gateway-token"],
            "[redacted len=9]"
        );
    }

    #[test]
    fn codex_headers_include_session_and_beta() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("s".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let headers = build_codex_headers(&auth, &ctx, false, None).unwrap();
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(headers.get("session-id").unwrap(), "s");
        assert_eq!(headers.get("thread-id").unwrap(), "s");
        assert!(headers.get("session_id").is_none());
        // A conversation scope (a subagent's) wins over the session id, so its
        // prompt cache gets its own routing bucket upstream.
        let scoped = build_codex_headers(&auth, &ctx, false, Some("agent-scope")).unwrap();
        assert_eq!(scoped.get("session-id").unwrap(), "agent-scope");
        assert_eq!(scoped.get("thread-id").unwrap(), "agent-scope");
        assert_eq!(scoped.get("x-client-request-id").unwrap(), "agent-scope");
        assert_eq!(scoped.get("x-codex-window-id").unwrap(), "agent-scope:0");
        // A key from an OpenAI-compatible client is not header material; it
        // stays in the body and the headers keep the session.
        for unusable in ["", "conv\n1", "ключ"] {
            let relayed = build_codex_headers(&auth, &ctx, false, Some(unusable)).unwrap();
            assert_eq!(relayed.get("session-id").unwrap(), "s", "key={unusable:?}");
            assert_eq!(relayed.get("x-codex-window-id").unwrap(), "s:0");
        }
        assert_eq!(
            headers.get("x-codex-beta-features").unwrap(),
            "remote_compaction_v2"
        );
    }

    #[test]
    fn codex_headers_include_responses_lite_when_requested() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let headers = build_codex_headers(&auth, &ctx, true, None).unwrap();
        assert_eq!(
            headers
                .get("x-openai-internal-codex-responses-lite")
                .unwrap(),
            "true"
        );
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(default_user_agent(true), "codex_cli_rs");
    }

    #[test]
    fn codex_headers_omit_session_when_missing() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let headers = build_codex_headers(&auth, &ctx, false, None).unwrap();
        assert!(headers.get("session-id").is_none());
        assert!(headers.get("thread-id").is_none());
        assert!(headers.get("x-client-request-id").is_none());
    }

    #[test]
    fn codex_headers_return_error_for_invalid_session_header() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("bad\nsession".into()),
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let err = build_codex_headers(&auth, &ctx, false, None).unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("session-id"));
    }

    #[test]
    fn build_websocket_request_removes_stream() {
        let input = vec![
            super::super::translate::request::ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "hello".to_string(),
                    },
                ],
            },
        ];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
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
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let payload = build_websocket_request(&req, None);
        assert_eq!(
            payload.get("type").and_then(|v| v.as_str()),
            Some("response.create")
        );
        assert!(payload.get("stream").is_none());
        assert!(payload.get("previous_response_id").is_none());
    }

    #[test]
    fn websocket_pool_owner_tracks_typed_continuation_opt_in() {
        let owner = ConversationIdentity::Agent("session".into(), "agent".into());
        let disabled = test_continuation(Some(owner.clone()), None, None, None, Some("disabled"));
        let first_enabled = test_continuation(
            Some(owner.clone()),
            Some(1),
            None,
            None,
            Some("missing_state"),
        );
        let append = test_continuation(Some(owner.clone()), Some(2), Some("resp_1"), Some(1), None);
        let missing_identity = test_continuation(None, None, None, None, Some("missing_identity"));

        assert_eq!(websocket_pool_owner(Some(&disabled)), None);
        assert_eq!(websocket_pool_owner(Some(&first_enabled)), Some(&owner));
        assert_eq!(websocket_pool_owner(Some(&append)), Some(&owner));
        assert_eq!(websocket_pool_owner(Some(&missing_identity)), None);
    }

    #[test]
    fn websocket_pool_reset_clears_initial_stale_state() {
        let owner = Some(ConversationIdentity::Main("session".into()));
        let missing_state =
            test_continuation(owner.clone(), None, None, None, Some("missing_state"));
        let disabled = test_continuation(owner.clone(), None, None, None, Some("disabled"));
        let prompt_changed = test_continuation(owner, None, None, None, Some("prompt_changed"));

        assert!(should_reset_websocket_pool(Some(&missing_state)));
        assert!(!should_reset_websocket_pool(Some(&disabled)));
        assert!(should_reset_websocket_pool(Some(&prompt_changed)));
    }

    #[test]
    fn build_codex_headers_error_on_empty_access() {
        let auth = StoredAuth {
            access: "".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let result = build_codex_headers(&auth, &ctx, false, None);
        assert!(
            result.is_ok(),
            "empty access should still produce valid Bearer header"
        );
    }

    #[test]
    fn codex_header_timeout_error_display() {
        let err = CodexHeaderTimeoutError { timeout_ms: 60000 };
        let display = format!("{err}");
        assert!(display.contains("60000"));
    }

    #[test]
    fn codex_transport_error_display() {
        let err = CodexTransportError {
            message: "connection reset".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("connection reset"));
    }

    #[test]
    fn unauthorized_retry_distinguishes_auto_and_strict_websocket_handshakes() {
        let http_unauthorized = Ok(OwnerAwareCodexResponse::new(
            CodexResponse {
                body: Vec::new(),
                status: 401,
                headers: Vec::new(),
                transport: ActualTransport::Http,
            },
            None,
        ));
        let websocket_unauthorized = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        });
        let forbidden = Err(CodexError {
            status: 403,
            message: "Forbidden".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        });
        let rejected_handshake = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: Some("policy denied".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
        let rejected_handshake_err = match &rejected_handshake {
            Err(error) => error,
            Ok(_) => panic!("expected rejected handshake"),
        };

        assert!(should_refresh_after_unauthorized(
            &http_unauthorized,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_refresh_after_unauthorized(
            &websocket_unauthorized,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(!should_refresh_after_unauthorized(
            &forbidden,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(!should_refresh_after_unauthorized(
            &rejected_handshake,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_refresh_after_unauthorized(
            &rejected_handshake,
            false,
            crate::config::CodexTransport::WebSocket
        ));
        assert!(!should_refresh_after_unauthorized(
            &http_unauthorized,
            true,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_fallback_to_http(rejected_handshake_err));
    }

    #[tokio::test]
    async fn a_401_naming_a_known_class_is_answered_without_a_token_refresh() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let server_client = client.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            // A refresh would pick up this login and send the request again.
            server_client.auth_manager().set_test_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                account_id: Some("acct-rotated".into()),
                expires: u64::MAX,
            });
            let body = br#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota."}}"#;
            let head = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });

        let err = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("a quota failure must not succeed"),
        };
        assert_eq!(
            err.class,
            Some(super::super::events::CodexErrorClass::QuotaExceeded)
        );
        assert_eq!(err.message, "You exceeded your current quota.");
        server.await.unwrap();

        let classified = Err(CodexError {
            status: 401,
            message: "WebSocket upgrade rejected with status 401".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: Some(super::super::events::CodexErrorClass::QuotaExceeded),
            origin: CodexErrorOrigin::WebSocket,
        });
        assert!(!should_refresh_after_unauthorized(
            &classified,
            false,
            crate::config::CodexTransport::Auto
        ));
    }

    /// The 401 error bodies every path is checked with, and whether each one
    /// asks for a token refresh: a spent window and a known class do not.
    const UNAUTHORIZED_ERRORS: [(&str, bool); 3] = [
        (
            r#"{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":9568}"#,
            false,
        ),
        (
            r#"{"code":"insufficient_quota","message":"You exceeded your current quota."}"#,
            false,
        ),
        (r#"{"message":"Your token has expired."}"#, true),
    ];

    fn rotated_test_auth() -> StoredAuth {
        StoredAuth {
            access: "rotated".into(),
            refresh: "rotated-refresh".into(),
            account_id: Some("acct-rotated".into()),
            expires: u64::MAX,
        }
    }

    /// A rotated login with no refresh token: the first refresh picks it up,
    /// and a second one fails as an `Auth` error without leaving loopback.
    fn last_rotation_test_auth() -> StoredAuth {
        StoredAuth {
            refresh: String::new(),
            ..rotated_test_auth()
        }
    }

    /// A refresh sends the request again, so the attempts tell whether one ran.
    fn expected_unauthorized_attempts(refreshes: bool) -> usize {
        if refreshes { 2 } else { 1 }
    }

    /// The error a 401 that names a spent window or a class must carry.
    fn assert_unauthorized_answer(err: &CodexError, error: &str, refreshes: bool) {
        if refreshes {
            assert_eq!(err.status, 401, "{error}");
            assert!(err.usage_limit.is_none() && err.class.is_none(), "{error}");
        } else if error.contains("usage_limit_reached") {
            assert_eq!(
                err.usage_limit.as_ref().map(|limit| limit.message.as_str()),
                Some("The usage limit has been reached")
            );
        } else {
            assert_eq!(
                err.class,
                Some(super::super::events::CodexErrorClass::QuotaExceeded)
            );
        }
    }

    /// Answers every HTTP request, a WebSocket upgrade included, with a 401
    /// whose body holds `error`, counting them in `attempts`. Each one puts a
    /// rotated login in place, which a token refresh would pick up before
    /// sending the request again.
    async fn serve_unauthorized_http(
        listener: TcpListener,
        client: Arc<CodexHttpClient>,
        error: &'static str,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        serve_unauthorized_http_rotating_to(listener, client, error, attempts, rotated_test_auth())
            .await;
    }

    /// Same, rotating to a login that a second refresh cannot renew.
    async fn serve_unauthorized_http_last_rotation(
        listener: TcpListener,
        client: Arc<CodexHttpClient>,
        error: &'static str,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        serve_unauthorized_http_rotating_to(
            listener,
            client,
            error,
            attempts,
            last_rotation_test_auth(),
        )
        .await;
    }

    async fn serve_unauthorized_http_rotating_to(
        listener: TcpListener,
        client: Arc<CodexHttpClient>,
        error: &'static str,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
        rotated: StoredAuth,
    ) {
        let body = format!(r#"{{"error":{error}}}"#);
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client.auth_manager().set_test_auth(rotated.clone());
            let head = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
        }
    }

    /// Same, for an open WebSocket that answers the request with an `error`
    /// event of status 401 holding `error`.
    async fn serve_unauthorized_websocket_event(
        listener: TcpListener,
        client: Arc<CodexHttpClient>,
        error: &'static str,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let error: serde_json::Value = serde_json::from_str(error).unwrap();
        let mut sockets = Vec::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _request = next_websocket_json(&mut websocket).await;
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client.auth_manager().set_test_auth(rotated_test_auth());
            let event = serde_json::json!({"type": "error", "status": 401, "error": error});
            websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    event.to_string(),
                ))
                .await
                .unwrap();
            // Kept open, so the client reads the event before the close.
            sockets.push(websocket);
        }
    }

    /// A client on a loopback listener served by `serve`, which the caller
    /// stops once the client has answered; the counter then holds every
    /// attempt the client made.
    async fn unauthorized_test_server<F, Fut>(
        serve: F,
        error: &'static str,
    ) -> (
        Arc<CodexHttpClient>,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    )
    where
        F: FnOnce(
            TcpListener,
            Arc<CodexHttpClient>,
            &'static str,
            Arc<std::sync::atomic::AtomicUsize>,
        ) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = tokio::spawn(serve(listener, client.clone(), error, attempts.clone()));
        (client, server, attempts)
    }

    fn unauthorized_attempts(
        server: tokio::task::JoinHandle<()>,
        attempts: &std::sync::atomic::AtomicUsize,
    ) -> usize {
        server.abort();
        attempts.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_401_naming_a_spent_window_or_a_class_is_answered_on_the_live_http_start() {
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http, error).await;
            let err = match client
                .stream_codex_http_events(&buffered_test_request(), &http_test_context())
                .await
            {
                Err(err) => err,
                Ok(_) => panic!("a 401 must not start a stream"),
            };
            assert_unauthorized_answer(&err, error, refreshes);
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                expected_unauthorized_attempts(refreshes),
                "{error}"
            );
        }
    }

    fn search_test_request() -> super::super::search::SearchRequest {
        super::super::search::SearchRequest {
            id: "session".to_string(),
            model: "gpt-5.6-luna".to_string(),
            reasoning: None,
            input: None,
            commands: super::super::search::SearchCommands {
                search_query: vec![super::super::search::SearchQuery {
                    q: "find Codex".to_string(),
                }],
            },
            settings: super::super::search::SearchSettings {
                filters: None,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: 2_500,
        }
    }

    #[tokio::test]
    async fn a_401_naming_a_spent_window_or_a_class_is_answered_on_the_search_call() {
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http, error).await;
            let err = match client
                .post_search(&search_test_request(), &http_test_context())
                .await
            {
                Err(err) => err,
                Ok(_) => panic!("a 401 must not answer the search"),
            };
            assert_unauthorized_answer(&err, error, refreshes);
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                expected_unauthorized_attempts(refreshes),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_401_naming_a_spent_window_or_a_class_is_answered_on_the_buffered_paths() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for transport in [
            crate::config::CodexTransport::Http,
            crate::config::CodexTransport::WebSocket,
        ] {
            for (error, refreshes) in UNAUTHORIZED_ERRORS {
                let (client, server, attempts) =
                    unauthorized_test_server(serve_unauthorized_http, error).await;
                let err = match client
                    .post_codex_with_transport(
                        &buffered_test_request(),
                        &http_test_context(),
                        None,
                        transport,
                    )
                    .await
                {
                    Err(err) => err,
                    Ok(_) => panic!("a 401 must not succeed"),
                };
                assert_unauthorized_answer(&err, error, refreshes);
                assert_eq!(
                    unauthorized_attempts(server, &attempts),
                    expected_unauthorized_attempts(refreshes),
                    "{transport:?} {error}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_401_naming_a_spent_window_or_a_class_is_answered_on_the_live_websocket_start() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http, error).await;
            let mut events = client
                .stream_codex_websocket_events_for_owner(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                )
                .await
                .unwrap();
            let err = match events.recv().await {
                Some(Err(err)) => err,
                other => panic!("a rejected upgrade must end the stream: {other:?}"),
            };
            assert_unauthorized_answer(&err, error, refreshes);
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                expected_unauthorized_attempts(refreshes),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_401_event_naming_a_spent_window_or_a_class_is_answered_on_the_live_websocket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_websocket_event, error).await;
            let mut events = client
                .stream_codex_websocket_events_for_owner(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                )
                .await
                .unwrap();
            let event = match events.recv().await {
                Some(Ok(event)) => event,
                other => panic!("the error event must reach the caller: {other:?}"),
            };
            assert_eq!(event["type"], "error", "{error}");
            assert_eq!(event["status"], 401, "{error}");
            drop(events);
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                expected_unauthorized_attempts(refreshes),
                "{error}"
            );
            assert_eq!(
                unauthorized_event_needs_refresh(&event),
                refreshes,
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_401_event_naming_a_spent_window_or_a_class_is_answered_on_the_buffered_websocket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_websocket_event, error).await;
            let err = match client
                .post_codex_with_transport(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                    crate::config::CodexTransport::WebSocket,
                )
                .await
            {
                Err(err) => err,
                Ok(_) => panic!("a 401 event must not succeed"),
            };
            assert_unauthorized_answer(&err, error, refreshes);
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                expected_unauthorized_attempts(refreshes),
                "{error}"
            );
        }
    }

    /// Under `auto` a refused upgrade falls back to HTTP, which reads its own
    /// answer, and a plain 401 on both is refreshed once per client request:
    /// the HTTP attempt does not refresh again after the WebSocket did. A
    /// second refresh would fail here as an `Auth` error.
    #[tokio::test]
    async fn a_401_on_the_live_auto_fallback_refreshes_at_most_once() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http_last_rotation, error).await;
            let err = match client
                .stream_codex_auto_events_for_owner(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                )
                .await
            {
                Err(err) => err,
                Ok(_) => panic!("a 401 must not start a stream"),
            };
            assert_eq!(
                client.auth_manager().forced_refreshes_for_tests(),
                usize::from(refreshes),
                "{error}"
            );
            assert_unauthorized_answer(&err, error, refreshes);
            assert_eq!(err.origin, CodexErrorOrigin::Http, "{error}");
            // The upgrade, the upgrade again after the refresh, then HTTP;
            // with no refresh, the upgrade and HTTP.
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                if refreshes { 3 } else { 2 },
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_401_on_the_buffered_auto_fallback_refreshes_at_most_once() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        for (error, refreshes) in UNAUTHORIZED_ERRORS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http_last_rotation, error).await;
            let err = match client
                .post_codex_with_transport(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                    crate::config::CodexTransport::Auto,
                )
                .await
            {
                Err(err) => err,
                Ok(_) => panic!("a 401 must not succeed"),
            };
            assert_unauthorized_answer(&err, error, refreshes);
            assert_ne!(err.origin, CodexErrorOrigin::Auth, "{error}");
            // The upgrade and HTTP, and both again after the one refresh.
            assert_eq!(
                unauthorized_attempts(server, &attempts),
                if refreshes { 4 } else { 2 },
                "{error}"
            );
        }
    }

    /// What `spawn_size_refusing_origin` received.
    #[derive(Default)]
    struct SizeRefusalCounts {
        upgrades: std::sync::atomic::AtomicUsize,
        frames: std::sync::atomic::AtomicUsize,
        posts: std::sync::atomic::AtomicUsize,
    }

    impl SizeRefusalCounts {
        /// Upgrades, request frames and HTTP requests, in that order.
        fn read(&self) -> (usize, usize, usize) {
            use std::sync::atomic::Ordering;
            (
                self.upgrades.load(Ordering::SeqCst),
                self.frames.load(Ordering::SeqCst),
                self.posts.load(Ordering::SeqCst),
            )
        }
    }

    /// An origin that closes every WebSocket request frame with 1009 before
    /// any text and completes HTTP requests. With `on_first_upgrade` the first
    /// upgrade is answered with a 401 after switching the login to it. When a
    /// request frame arrives the login switches to `on_frame`, when given; with
    /// `on_first_post` the first HTTP request is answered with a 401 after
    /// switching the login to it.
    fn spawn_size_refusing_origin(
        listener: TcpListener,
        client: Arc<CodexHttpClient>,
        on_first_upgrade: Option<StoredAuth>,
        on_frame: Option<StoredAuth>,
        on_first_post: Option<StoredAuth>,
        counts: Arc<SizeRefusalCounts>,
    ) -> tokio::task::JoinHandle<()> {
        use std::sync::atomic::Ordering;
        use tokio_tungstenite::tungstenite::{
            Message,
            protocol::{CloseFrame, frame::coding::CloseCode},
        };

        tokio::spawn(async move {
            let mut sockets = Vec::new();
            let mut on_first_upgrade = on_first_upgrade;
            let mut on_first_post = on_first_post;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    continue;
                }
                if first[0] == b'G'
                    && let Some(auth) = on_first_upgrade.take()
                {
                    // A 401 the WebSocket task's own refresh answers by
                    // picking up that login before it connects again.
                    let _request = read_http_request(&mut stream).await;
                    counts.upgrades.fetch_add(1, Ordering::SeqCst);
                    client.auth_manager().set_test_auth(auth);
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    continue;
                }
                if first[0] == b'G' {
                    let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    counts.upgrades.fetch_add(1, Ordering::SeqCst);
                    let _request = next_websocket_json(&mut websocket).await;
                    counts.frames.fetch_add(1, Ordering::SeqCst);
                    if let Some(auth) = on_frame.clone() {
                        client.auth_manager().set_test_auth(auth);
                    }
                    websocket
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Size,
                            reason: "".into(),
                        })))
                        .await
                        .unwrap();
                    sockets.push(websocket);
                    continue;
                }
                let _request = read_http_request(&mut stream).await;
                counts.posts.fetch_add(1, Ordering::SeqCst);
                let response = if let Some(auth) = on_first_post.take() {
                    // A 401 the refresh answers by picking up that login.
                    client.auth_manager().set_test_auth(auth);
                    let body = r#"{"error":{"message":"Your token has expired."}}"#;
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        })
    }

    /// One `auto` request, live or buffered, against
    /// `spawn_size_refusing_origin`: the answer, what the origin received and
    /// how many refreshes ran. A buffered answer must be a 200 over HTTP.
    async fn run_size_refusal_fallback(
        live: bool,
        on_frame: Option<StoredAuth>,
        on_first_post: Option<StoredAuth>,
    ) -> (Result<(), CodexError>, (usize, usize, usize), usize) {
        run_size_refusal_fallback_from(http_test_auth(), live, on_frame, on_first_post).await
    }

    /// `run_size_refusal_fallback` starting from the login `initial`.
    async fn run_size_refusal_fallback_from(
        initial: StoredAuth,
        live: bool,
        on_frame: Option<StoredAuth>,
        on_first_post: Option<StoredAuth>,
    ) -> (Result<(), CodexError>, (usize, usize, usize), usize) {
        run_size_refusal_fallback_with(initial, live, None, on_frame, on_first_post).await
    }

    /// `run_size_refusal_fallback_from` whose first upgrade, with
    /// `on_first_upgrade`, is refused with a 401 that switches the login.
    async fn run_size_refusal_fallback_with(
        initial: StoredAuth,
        live: bool,
        on_first_upgrade: Option<StoredAuth>,
        on_frame: Option<StoredAuth>,
        on_first_post: Option<StoredAuth>,
    ) -> (Result<(), CodexError>, (usize, usize, usize), usize) {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 100));
        client.auth_manager().set_test_auth(initial);
        let counts = Arc::new(SizeRefusalCounts::default());
        let server = spawn_size_refusing_origin(
            listener,
            client.clone(),
            on_first_upgrade,
            on_frame,
            on_first_post,
            counts.clone(),
        );

        let (body, context) = (buffered_test_request(), http_test_context());
        let result = if live {
            let request = client.stream_codex_auto_events_for_owner(&body, &context, None);
            tokio::time::timeout(MOCK_DEADLINE, request)
                .await
                .expect("the request did not finish")
                .map(|_| ())
        } else {
            let request = client.post_codex_with_transport(
                &body,
                &context,
                None,
                crate::config::CodexTransport::Auto,
            );
            tokio::time::timeout(MOCK_DEADLINE, request)
                .await
                .expect("the request did not finish")
                .map(|response| {
                    assert_eq!(response.status, 200);
                    assert_eq!(response.transport, ActualTransport::Http);
                })
        };
        server.abort();
        (
            result,
            counts.read(),
            client.auth_manager().forced_refreshes_for_tests(),
        )
    }

    /// The rotated login under the account of `http_test_auth`: a renewed token.
    fn renewed_test_auth() -> StoredAuth {
        StoredAuth {
            account_id: http_test_auth().account_id,
            ..rotated_test_auth()
        }
    }

    /// The rotated login with no account id.
    fn unknown_account_test_auth() -> StoredAuth {
        StoredAuth {
            account_id: None,
            ..rotated_test_auth()
        }
    }

    /// The login of `http_test_auth`, its access token included, with no
    /// account id.
    fn unknown_account_initial_auth() -> StoredAuth {
        StoredAuth {
            account_id: None,
            ..http_test_auth()
        }
    }

    /// The answer a size refusal's replay refused under another account gets:
    /// the WebSocket's own 1009 refusal.
    fn assert_size_refusal_answer(result: Result<(), CodexError>) {
        let err = result.expect_err("the replay must be refused");
        assert!(super::super::websocket::is_message_size_refusal(&err));
        assert_eq!(err.status, 413);
        assert_eq!(
            err.detail.as_deref(),
            Some(super::super::websocket::WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL)
        );
    }

    #[tokio::test]
    async fn a_size_refusal_keeps_the_buffered_auto_request_on_http_after_a_refresh() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(false, None, Some(renewed_test_auth())).await;
        result.unwrap();
        assert_eq!(refreshes, 1);
        // The one request frame the socket refused, then HTTP twice: the
        // resend after the refresh does not go back to the WebSocket.
        assert_eq!(counts, (1, 1, 2));
    }

    /// The buffered replay goes out with the WebSocket attempt's credentials;
    /// a 401 refresh that lands on another account sends nothing further.
    #[tokio::test]
    async fn a_buffered_size_refusal_is_not_replayed_under_another_account_after_a_refresh() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(false, None, Some(rotated_test_auth())).await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (1, 1, 1));
    }

    /// A refreshed login with no account id cannot be shown to be the
    /// WebSocket attempt's account, and its token is new.
    #[tokio::test]
    async fn a_buffered_size_refusal_is_not_replayed_when_the_refreshed_account_is_unknown() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(false, None, Some(unknown_account_test_auth())).await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (1, 1, 1));
    }

    #[tokio::test]
    async fn a_buffered_size_refusal_is_not_replayed_after_a_refresh_when_no_account_is_known() {
        let (result, counts, refreshes) = run_size_refusal_fallback_from(
            unknown_account_initial_auth(),
            false,
            None,
            Some(unknown_account_test_auth()),
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (1, 1, 1));
    }

    #[tokio::test]
    async fn a_live_size_refusal_is_not_replayed_under_another_account() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(true, Some(rotated_test_auth()), None).await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 0);
        assert_eq!(counts, (1, 1, 0));
    }

    #[tokio::test]
    async fn a_live_size_refusal_is_replayed_under_a_renewed_token_of_the_same_account() {
        let (result, counts, _) =
            run_size_refusal_fallback(true, Some(renewed_test_auth()), None).await;
        result.unwrap();
        assert_eq!(counts, (1, 1, 1));
    }

    /// A known account on one side and an unknown one on the other proves
    /// nothing, so a new token is refused in either direction.
    #[tokio::test]
    async fn a_live_size_refusal_is_not_replayed_under_a_new_token_when_an_account_is_unknown() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(true, Some(unknown_account_test_auth()), None).await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 0);
        assert_eq!(counts, (1, 1, 0));

        let (result, counts, refreshes) = run_size_refusal_fallback_from(
            unknown_account_initial_auth(),
            true,
            Some(rotated_test_auth()),
            None,
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 0);
        assert_eq!(counts, (1, 1, 0));
    }

    #[tokio::test]
    async fn a_live_size_refusal_is_not_replayed_under_a_new_token_when_no_account_is_known() {
        let (result, counts, refreshes) = run_size_refusal_fallback_from(
            unknown_account_initial_auth(),
            true,
            Some(unknown_account_test_auth()),
            None,
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 0);
        assert_eq!(counts, (1, 1, 0));
    }

    /// With an account id unknown, the WebSocket attempt's own access token
    /// shows the credential did not change.
    #[tokio::test]
    async fn a_live_size_refusal_is_replayed_under_the_same_token_when_an_account_is_unknown() {
        let (result, counts, _) = run_size_refusal_fallback_from(
            unknown_account_initial_auth(),
            true,
            Some(unknown_account_initial_auth()),
            None,
        )
        .await;
        result.unwrap();
        assert_eq!(counts, (1, 1, 1));

        let (result, counts, _) =
            run_size_refusal_fallback(true, Some(unknown_account_initial_auth()), None).await;
        result.unwrap();
        assert_eq!(counts, (1, 1, 1));
    }

    #[tokio::test]
    async fn a_live_size_refusal_is_not_replayed_under_another_account_after_a_refresh() {
        let (result, counts, refreshes) =
            run_size_refusal_fallback(true, None, Some(rotated_test_auth())).await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (1, 1, 1));
    }

    #[tokio::test]
    async fn a_live_size_refusal_is_not_replayed_after_a_refresh_when_no_account_is_known() {
        let (result, counts, refreshes) = run_size_refusal_fallback_from(
            unknown_account_initial_auth(),
            true,
            None,
            Some(unknown_account_test_auth()),
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (1, 1, 1));
    }

    /// A refresh the WebSocket task makes after a 401 upgrade sends the
    /// request on a socket opened with the refreshed login, so the size
    /// refusal it gets is replayed under that login: by its account, or by its
    /// token when no account is known.
    #[tokio::test]
    async fn a_live_size_refusal_after_a_websocket_refresh_is_replayed_under_the_refreshed_login() {
        let (result, counts, refreshes) = run_size_refusal_fallback_with(
            http_test_auth(),
            true,
            Some(renewed_test_auth()),
            None,
            None,
        )
        .await;
        result.unwrap();
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (2, 1, 1));

        let (result, counts, refreshes) = run_size_refusal_fallback_with(
            unknown_account_initial_auth(),
            true,
            Some(unknown_account_test_auth()),
            None,
            None,
        )
        .await;
        result.unwrap();
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (2, 1, 1));
    }

    /// After the WebSocket task's own refresh, the replay is compared with the
    /// login the refused request went out under, not the one before the
    /// refresh: a change after the refusal sends nothing over HTTP, even back
    /// to the token the first upgrade carried.
    #[tokio::test]
    async fn a_live_size_refusal_after_a_websocket_refresh_is_not_replayed_on_a_changed_login() {
        let (result, counts, refreshes) = run_size_refusal_fallback_with(
            http_test_auth(),
            true,
            Some(renewed_test_auth()),
            Some(rotated_test_auth()),
            None,
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (2, 1, 0));

        let (result, counts, refreshes) = run_size_refusal_fallback_with(
            unknown_account_initial_auth(),
            true,
            Some(unknown_account_test_auth()),
            Some(unknown_account_initial_auth()),
            None,
        )
        .await;
        assert_size_refusal_answer(result);
        assert_eq!(refreshes, 1);
        assert_eq!(counts, (2, 1, 0));
    }

    /// An account that was unknown when the request went out and is known by
    /// the replay, with the same access token, is the same credential.
    #[tokio::test]
    async fn a_size_refusal_is_replayed_under_the_same_token_once_the_account_is_known() {
        for live in [true, false] {
            let (result, counts, refreshes) = run_size_refusal_fallback_from(
                unknown_account_initial_auth(),
                live,
                Some(http_test_auth()),
                None,
            )
            .await;
            result.unwrap();
            assert_eq!(refreshes, 0);
            assert_eq!(counts, (1, 1, 1), "live: {live}");
        }
    }

    /// A conversation's socket pooled under a login with no account id is not
    /// taken by the next turn once the saved login has another token, still
    /// with no account. The turn goes out with its full context on a socket
    /// opened with the new token, so the size refusal it gets and the HTTP
    /// replay that follows are both under that token, and nothing more goes
    /// out on the old socket.
    // tungstenite's own callback type fixes the error response size.
    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn a_size_refusal_after_a_login_change_is_not_sent_on_the_old_pooled_socket() {
        use tokio_tungstenite::tungstenite::{
            Message,
            protocol::{CloseFrame, frame::coding::CloseCode},
        };

        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "size-refusal-pool-session".to_string(),
            "size-refusal-pool-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // What the origin saw, in order: `upgrade <socket> <token>`,
        // `frame <socket> <previous response> <inputs>`, `post <token>`.
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let server = tokio::spawn(async move {
            let mut upgrades = 0_usize;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    continue;
                }
                if first[0] != b'G' {
                    let request = String::from_utf8(read_http_request(&mut stream).await).unwrap();
                    let token = request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().trim_start_matches("Bearer ").to_string())
                        })
                        .unwrap_or_default();
                    seen_tx.send(format!("post {token}")).unwrap();
                    let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    continue;
                }
                let socket = upgrades;
                upgrades += 1;
                let mut token = String::new();
                let mut websocket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &http::Request<()>, response| {
                        token = request
                            .headers()
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.strip_prefix("Bearer "))
                            .unwrap_or_default()
                            .to_string();
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                seen_tx.send(format!("upgrade {socket} {token}")).unwrap();
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let mut frames = 0;
                    while let Some(Ok(message)) = websocket.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        seen_tx
                            .send(format!(
                                "frame {socket} {} {}",
                                request["previous_response_id"].as_str().unwrap_or("-"),
                                request["input"].as_array().map_or(0, Vec::len)
                            ))
                            .unwrap();
                        frames += 1;
                        if socket == 0 && frames == 1 {
                            send_completed_websocket_response(&mut websocket, "resp_a").await;
                        } else {
                            let _ = websocket
                                .send(Message::Close(Some(CloseFrame {
                                    code: CloseCode::Size,
                                    reason: "".into(),
                                })))
                                .await;
                        }
                    }
                });
            }
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 100));
        client
            .auth_manager()
            .set_test_auth(unknown_account_initial_auth());
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the first turn did not finish")
        .unwrap();
        assert_eq!(first_response.transport, ActualTransport::WebSocket);
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        client
            .auth_manager()
            .set_test_auth(unknown_account_test_auth());
        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_a")
        );
        let second_response = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the second turn did not finish")
        .unwrap();
        assert_eq!(second_response.status, 200);
        assert_eq!(second_response.transport, ActualTransport::Http);
        server.abort();

        let mut seen = Vec::new();
        while let Ok(event) = seen_rx.try_recv() {
            seen.push(event);
        }
        assert_eq!(
            seen,
            [
                "upgrade 0 test",
                "frame 0 - 1",
                "upgrade 1 rotated",
                "frame 1 - 2",
                "post rotated",
            ]
        );
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
    }

    #[test]
    fn the_size_fallback_refused_warning_names_only_the_request() {
        assert_eq!(
            serde_json::Value::Object(size_fallback_refused_fields("request-size")),
            serde_json::json!({"reqId": "request-size"})
        );
    }

    /// An origin that refuses every WebSocket upgrade with a 503 and
    /// completes every HTTP request, counting both.
    fn spawn_upgrade_refusing_origin(
        listener: TcpListener,
        upgrades: Arc<std::sync::atomic::AtomicUsize>,
        posts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> tokio::task::JoinHandle<()> {
        use std::sync::atomic::Ordering;

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut first = [0_u8; 1];
                if stream.peek(&mut first).await.unwrap_or(0) == 0 {
                    continue;
                }
                let _request = read_http_request(&mut stream).await;
                let response = if first[0] == b'G' {
                    upgrades.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    posts.fetch_add(1, Ordering::SeqCst);
                    let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            }
        })
    }

    /// A loopback port held without listening, so a connect to it fails until
    /// the test calls `listen`, and no other socket can take it meanwhile.
    /// Linux refuses such a connect; macOS leaves it unanswered until it times
    /// out, which `UNLISTENED_CONNECT_DEADLINE` allows for.
    fn unlistened_loopback_socket() -> (tokio::net::TcpSocket, std::net::SocketAddr) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        (socket, addr)
    }

    /// How long a request to an unlistened port may take: up to the WebSocket
    /// connect timeout, then the HTTP request after it.
    const UNLISTENED_CONNECT_DEADLINE: Duration = Duration::from_millis(
        super::super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS + MOCK_DEADLINE.as_millis() as u64,
    );

    #[tokio::test]
    async fn an_unanswered_connect_sends_the_next_buffered_auto_requests_straight_to_http() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let (socket, addr) = unlistened_loopback_socket();
        let url = format!("http://{addr}/responses");
        let client = Arc::new(authenticated_http_test_client(url.clone()));
        let (body, context) = (buffered_test_request(), http_test_context());
        let send = || {
            let request = client.post_codex_with_transport(
                &body,
                &context,
                None,
                crate::config::CodexTransport::Auto,
            );
            async move {
                tokio::time::timeout(UNLISTENED_CONNECT_DEADLINE, request)
                    .await
                    .expect("the request did not finish")
            }
        };

        // Nothing listens: the connect fails, and so does the HTTP request.
        assert!(send().await.is_err());

        let listener = socket.listen(1024).unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let server = spawn_upgrade_refusing_origin(listener, upgrades.clone(), posts.clone());

        // The target is paused: HTTP at once, no upgrade.
        let response = send().await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.transport, ActualTransport::Http);
        assert_eq!(upgrades.load(Ordering::SeqCst), 0);
        assert_eq!(posts.load(Ordering::SeqCst), 1);

        // Once the pause ended the next request tries the WebSocket again. A
        // refused upgrade falls back to HTTP and starts no pause.
        super::super::websocket::expire_codex_websocket_connect_pause_for_tests(&url);
        for attempt in 1..=2 {
            let response = send().await.unwrap();
            assert_eq!(response.transport, ActualTransport::Http);
            assert_eq!(upgrades.load(Ordering::SeqCst), attempt);
            assert_eq!(posts.load(Ordering::SeqCst), attempt + 1);
        }
        server.abort();
    }

    #[tokio::test]
    async fn an_unanswered_connect_sends_the_next_live_auto_requests_straight_to_http() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let (socket, addr) = unlistened_loopback_socket();
        let url = format!("http://{addr}/responses");
        let client = Arc::new(authenticated_http_test_client(url.clone()));
        let (body, context) = (buffered_test_request(), http_test_context());
        let send = || {
            let request = client.stream_codex_auto_events_for_owner(&body, &context, None);
            async move {
                let started = tokio::time::timeout(UNLISTENED_CONNECT_DEADLINE, request)
                    .await
                    .expect("the request did not start");
                let (mut events, _) = started?;
                match tokio::time::timeout(MOCK_DEADLINE, events.recv())
                    .await
                    .expect("no event arrived")
                {
                    Some(Ok(_)) => Ok(()),
                    Some(Err(error)) => Err(error),
                    None => panic!("the stream ended without an event"),
                }
            }
        };

        // Nothing listens: the connect fails, and so does the HTTP request.
        assert!(send().await.is_err());

        let listener = socket.listen(1024).unwrap();
        let upgrades = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let server = spawn_upgrade_refusing_origin(listener, upgrades.clone(), posts.clone());

        // The target is paused: HTTP at once, no upgrade.
        send().await.unwrap();
        assert_eq!(upgrades.load(Ordering::SeqCst), 0);
        assert_eq!(posts.load(Ordering::SeqCst), 1);

        super::super::websocket::expire_codex_websocket_connect_pause_for_tests(&url);
        for attempt in 1..=2 {
            send().await.unwrap();
            assert_eq!(upgrades.load(Ordering::SeqCst), attempt);
            assert_eq!(posts.load(Ordering::SeqCst), attempt + 1);
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_paused_target_is_still_tried_where_nothing_falls_back_to_http() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let upgrades = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let server = spawn_upgrade_refusing_origin(listener, upgrades.clone(), posts.clone());
        super::super::websocket::pause_codex_websocket_connects_for_tests(&url);
        let client = Arc::new(authenticated_http_test_client(url.clone()));
        let mut without_fallback = authenticated_http_test_client(url.clone());
        without_fallback.auto_http_fallback_enabled = false;
        let (body, context) = (buffered_test_request(), http_test_context());

        // The `websocket` transport, buffered and live.
        let buffered = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &body,
                &context,
                None,
                crate::config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the request did not finish");
        assert_eq!(buffered.err().map(|error| error.status), Some(503));
        assert_eq!(upgrades.load(Ordering::SeqCst), 1);

        let live = tokio::time::timeout(MOCK_DEADLINE, async {
            let mut events = client
                .stream_codex_websocket_events_for_owner(&body, &context, None)
                .await?;
            match events.recv().await {
                Some(Err(error)) => Err(error),
                _ => panic!("a refused upgrade must end the stream with an error"),
            }
        })
        .await
        .expect("the request did not finish");
        assert_eq!(live.map_err(|error| error.status), Err::<(), _>(503));
        assert_eq!(upgrades.load(Ordering::SeqCst), 2);

        // `auto` on a client whose HTTP fallback is off.
        let buffered = tokio::time::timeout(
            MOCK_DEADLINE,
            without_fallback.post_codex_with_transport(
                &body,
                &context,
                None,
                crate::config::CodexTransport::Auto,
            ),
        )
        .await
        .expect("the request did not finish");
        assert_eq!(buffered.err().map(|error| error.status), Some(503));
        assert_eq!(upgrades.load(Ordering::SeqCst), 3);
        assert_eq!(posts.load(Ordering::SeqCst), 0);

        super::super::websocket::expire_codex_websocket_connect_pause_for_tests(&url);
        server.abort();
    }

    #[tokio::test]
    async fn a_live_auto_repair_after_a_forwarded_event_connects_despite_the_pause() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-repair-paused-target-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/responses");
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_repair_origin").await;

            // The continuation goes over the pooled socket: an informational
            // event, then a failure the full-context resend repairs.
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            first_websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::json!({"type": "keepalive"}).to_string(),
                ))
                .await
                .unwrap();
            send_nested_previous_response_missing(&mut first_websocket).await;

            let (repair_socket, _) = listener.accept().await.unwrap();
            let mut repair_websocket = tokio_tungstenite::accept_async(repair_socket)
                .await
                .unwrap();
            request_tx
                .send(next_websocket_json(&mut repair_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut repair_websocket, "resp_repaired").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(500), listener.accept())
                    .await
                    .is_err(),
                "the repaired request must not reach the target again"
            );
        });

        let client = Arc::new(authenticated_http_test_client(url.clone()));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = tokio::time::timeout(
            MOCK_DEADLINE,
            client.post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the first request did not finish")
        .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        super::super::websocket::pause_codex_websocket_connects_for_tests(&url);
        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_repair_origin")
        );
        let items = tokio::time::timeout(MOCK_DEADLINE, async {
            let (mut events, http_headers) = client
                .stream_codex_auto_events_for_owner(
                    &second_request,
                    &context,
                    Some(&second_candidate),
                )
                .await
                .expect("the live request did not start");
            assert!(
                http_headers.is_empty(),
                "the request must stay on the WebSocket"
            );
            let mut items = Vec::new();
            while let Some(item) = events.recv().await {
                items.push(item);
            }
            items
        })
        .await
        .expect("the live request did not finish");

        let errors: Vec<_> = items
            .iter()
            .filter_map(|item| item.as_ref().err())
            .map(|error| error.detail.clone())
            .collect();
        assert!(errors.is_empty(), "the client got errors: {errors:?}");
        assert_eq!(items[0].as_ref().unwrap()["type"], "keepalive");
        assert_eq!(
            items.last().unwrap().as_ref().unwrap()["response"]["id"],
            "resp_repaired"
        );

        let _first_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        let repaired_payload = request_rx.recv().await.unwrap();
        assert_eq!(
            continued_payload["previous_response_id"],
            "resp_repair_origin"
        );
        assert!(repaired_payload.get("previous_response_id").is_none());
        assert_eq!(repaired_payload["input"].as_array().unwrap().len(), 2);
        tokio::time::timeout(MOCK_DEADLINE, server)
            .await
            .expect("the mock server did not finish")
            .unwrap();

        super::super::websocket::expire_codex_websocket_connect_pause_for_tests(&url);
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    /// An error answer whose body never ends, `piece` bytes per chunk,
    /// counting in `pulled` every byte the reader took from it.
    fn endless_error_response(
        status: u16,
        piece: usize,
        pulled: Arc<std::sync::atomic::AtomicUsize>,
    ) -> reqwest::Response {
        let chunks = futures_util::stream::repeat_with(move || {
            pulled.fetch_add(piece, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, std::io::Error>(vec![b'x'; piece])
        });
        http::Response::builder()
            .status(status)
            .body(reqwest::Body::wrap_stream(chunks))
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn an_error_body_is_read_up_to_the_limit_and_no_further() {
        // A piece that does not divide the limit, so the last one is cut.
        let piece = 5_000;
        for status in [401, 500] {
            let pulled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let response = endless_error_response(status, piece, pulled.clone());
            let read = tokio::time::timeout(
                Duration::from_secs(5),
                collect_error_response(
                    response,
                    Instant::now(),
                    30_000,
                    &http_test_context(),
                    "Codex",
                ),
            )
            .await
            .expect("an endless error body must not hold the request")
            .unwrap();
            assert_eq!(read.status, status);
            assert_eq!(read.body.len(), MAX_ERROR_BODY_BYTES, "{status}");
            assert_eq!(
                pulled.load(std::sync::atomic::Ordering::SeqCst),
                MAX_ERROR_BODY_BYTES.div_ceil(piece) * piece,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn an_error_body_slower_than_its_deadline_fails_on_its_own() {
        // Each byte comes well within the deadline, the whole body never.
        let trickle = futures_util::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some((Ok::<_, std::io::Error>(vec![b'x']), ()))
        });
        let response: reqwest::Response = http::Response::builder()
            .status(401)
            .body(reqwest::Body::wrap_stream(trickle))
            .unwrap()
            .into();
        let started = Instant::now();
        let Err(err) =
            collect_error_response(response, Instant::now(), 200, &http_test_context(), "Codex")
                .await
        else {
            panic!("a body past its deadline must fail");
        };
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(err.status, 0);
        assert_eq!(
            err.message,
            "Timed out waiting 200ms for the Codex error response body"
        );
        assert_eq!(err.detail.as_deref(), Some("http_response_body"));
        assert!(!unauthorized_needs_refresh(&err));
    }

    /// The Codex HTTP requests of a Messages call that read an error body to
    /// classify it; a compaction request takes the buffered one.
    #[derive(Clone, Copy, Debug)]
    enum ErrorBodyPath {
        LiveHttp,
        BufferedHttp,
        Search,
    }

    const ERROR_BODY_PATHS: [ErrorBodyPath; 3] = [
        ErrorBodyPath::LiveHttp,
        ErrorBodyPath::BufferedHttp,
        ErrorBodyPath::Search,
    ];

    async fn error_on(client: &Arc<CodexHttpClient>, path: ErrorBodyPath) -> CodexError {
        let result = match path {
            ErrorBodyPath::LiveHttp => client
                .stream_codex_http_events(&buffered_test_request(), &http_test_context())
                .await
                .map(|_| ()),
            ErrorBodyPath::BufferedHttp => client
                .post_codex_with_transport(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                    crate::config::CodexTransport::Http,
                )
                .await
                .map(|_| ()),
            ErrorBodyPath::Search => client
                .post_search(&search_test_request(), &http_test_context())
                .await
                .map(|_| ()),
        };
        match result {
            Err(err) => err,
            Ok(()) => panic!("an error status must not succeed on {path:?}"),
        }
    }

    /// Answers every request with `status_line` and a chunked body that never
    /// ends, `piece` every `pause`, counting the requests in `attempts` and in
    /// `sent` the body bytes of every write that went out, until the client
    /// goes away.
    async fn serve_endless_error_body(
        listener: TcpListener,
        status_line: &'static str,
        piece: Vec<u8>,
        pause: Duration,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
        sent: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let mut frame = format!("{:x}\r\n", piece.len()).into_bytes();
        frame.extend_from_slice(&piece);
        frame.extend_from_slice(b"\r\n");
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let head = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).await.is_err() {
                continue;
            }
            while stream.write_all(&frame).await.is_ok() {
                sent.fetch_add(piece.len(), std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(pause).await;
            }
        }
    }

    #[tokio::test]
    async fn an_endless_error_body_ends_the_request_at_the_limit_on_every_http_path() {
        for (status_line, status) in [
            ("401 Unauthorized", 401),
            ("500 Internal Server Error", 500),
        ] {
            for path in ERROR_BODY_PATHS {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                // The deadline is far off, so only the limit can end the read.
                let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 30_000));
                client.auth_manager().set_test_auth(http_test_auth());
                let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let piece = 4 * 1024;
                let server = tokio::spawn(serve_endless_error_body(
                    listener,
                    status_line,
                    vec![b'x'; piece],
                    Duration::from_millis(1),
                    attempts.clone(),
                    sent.clone(),
                ));
                let err = tokio::time::timeout(Duration::from_secs(5), error_on(&client, path))
                    .await
                    .expect("an endless error body must not hold the request");
                let case = format!("{status_line} {path:?}");
                assert_eq!(err.status, status, "{case}");
                // The plain 401 asks for a refresh, which fails on this login
                // before anything is sent again.
                assert_eq!(
                    attempts.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "{case}"
                );
                // What went out past the limit is at most what was in flight
                // when the client dropped the connection.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let sent = sent.load(std::sync::atomic::Ordering::SeqCst);
                assert!(
                    sent <= MAX_ERROR_BODY_BYTES + 4 * piece,
                    "{case}: {sent} bytes went out"
                );
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn an_error_body_past_its_deadline_is_neither_refreshed_nor_sent_again() {
        for path in ERROR_BODY_PATHS {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 300));
            client.auth_manager().set_test_auth(http_test_auth());
            let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server = tokio::spawn(serve_endless_error_body(
                listener,
                "401 Unauthorized",
                b"x".to_vec(),
                Duration::from_millis(20),
                attempts.clone(),
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            ));
            let err = tokio::time::timeout(Duration::from_secs(5), error_on(&client, path))
                .await
                .expect("a trickling error body must fail at its deadline");
            let subject = match path {
                ErrorBodyPath::Search => "Codex search",
                _ => "Codex",
            };
            assert_eq!(err.status, 0, "{path:?}");
            assert_eq!(
                err.message,
                format!("Timed out waiting 300ms for the {subject} error response body"),
            );
            assert_eq!(err.origin, CodexErrorOrigin::Http, "{path:?}");
            assert_eq!(client.auth_manager().forced_refreshes_for_tests(), 0);
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{path:?}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_classified_error_body_under_the_limit_is_still_classified() {
        let error = serde_json::json!({
            "code": "insufficient_quota",
            "message": "You exceeded your current quota.",
            "padding": "x".repeat(MAX_ERROR_BODY_BYTES - 1024),
        })
        .to_string();
        // The body `serve_unauthorized_http` sends is `{"error":...}`.
        assert!(error.len() + 10 < MAX_ERROR_BODY_BYTES);
        let error: &'static str = Box::leak(error.into_boxed_str());
        for path in ERROR_BODY_PATHS {
            let (client, server, attempts) =
                unauthorized_test_server(serve_unauthorized_http, error).await;
            let err = error_on(&client, path).await;
            assert_eq!(
                err.class,
                Some(super::super::events::CodexErrorClass::QuotaExceeded),
                "{path:?}"
            );
            assert_eq!(client.auth_manager().forced_refreshes_for_tests(), 0);
            assert_eq!(unauthorized_attempts(server, &attempts), 1, "{path:?}");
        }
    }

    #[test]
    fn an_unusable_body_delay_gives_way_to_the_retry_after_header() {
        let body = br#"{"error":{"code":"rate_limit_exceeded","retry_after":"\u0007"}}"#;
        let headers = |value: &str| vec![("retry-after".to_string(), value.to_string())];
        assert_eq!(
            response_retry_after(body, &headers("7")).as_deref(),
            Some("7")
        );
        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        assert_eq!(
            response_retry_after(body, &headers(date)).as_deref(),
            Some(date)
        );
        assert_eq!(response_retry_after(body, &headers(&"x".repeat(65))), None);
        assert_eq!(response_retry_after(body, &headers("")), None);
    }

    /// A delay that is not usable, here one whose 4000th byte falls inside a
    /// character, is dropped where it is read: the `auto` fallback log does
    /// not carry it, and neither does the answer.
    #[tokio::test]
    async fn an_unusable_delay_of_a_rejected_upgrade_is_dropped_on_the_auto_fallback() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let body = serde_json::json!({"error": {
            "code": "rate_limit_exceeded",
            "message": "Slow down.",
            "retry_after": format!("{}é", "a".repeat(3999)),
        }})
        .to_string();
        for live in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            )));
            // The upgrade and then the HTTP request are refused alike.
            let server = tokio::spawn(serve_rate_limited(listener, body.clone()));
            let result = if live {
                client
                    .stream_codex_auto_events_for_owner(
                        &buffered_test_request(),
                        &http_test_context(),
                        None,
                    )
                    .await
                    .map(|_| ())
            } else {
                client
                    .post_codex_with_transport(
                        &buffered_test_request(),
                        &http_test_context(),
                        None,
                        crate::config::CodexTransport::Auto,
                    )
                    .await
                    .map(|_| ())
            };
            server.abort();
            let Err(err) = result else {
                panic!("a 429 must not succeed (live: {live})");
            };
            assert_ne!(err.origin, CodexErrorOrigin::WebSocketHandshake, "{live}");
            assert_eq!(
                err.class,
                Some(super::super::events::CodexErrorClass::Throttled),
                "{live}"
            );
            assert_eq!(err.retry_after, None, "{live}");
            let response = super::super::map_codex_error_to_response(&err);
            assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
            assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        }
    }

    /// A Codex answer that reads both windows spent, each resetting ahead,
    /// so the refusal names neither window.
    fn spent_snapshot_body() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "allowed": false,
                "limit_reached": true,
                "primary": {"used_percent": 100.0, "window_minutes": 300, "reset_at": now + 10_000},
                "secondary": {"used_percent": 100.0, "window_minutes": 10080, "reset_at": now + 500_000}
            }
        })
        .to_string()
    }

    /// Answers every request, a WebSocket upgrade included, with a 429
    /// holding `body`.
    async fn serve_rate_limited(listener: TcpListener, body: String) {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_http_request(&mut stream).await;
            let head = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
        }
    }

    /// The utilisations a healthy response with no telemetry of its own is
    /// given for a request under `account`.
    fn published_utilizations(account: Option<&str>) -> [Option<String>; 2] {
        let response = super::super::with_rate_limit_headers(
            axum::response::IntoResponse::into_response(axum::http::StatusCode::OK),
            account,
        );
        [
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-7d-utilization",
        ]
        .map(|name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_error_answer_feeds_every_window_it_reads_to_the_gauge() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let _rate_limits = super::super::rate_limits::lock_for_tests();
        for path in ["live http", "search", "upgrade"] {
            super::super::rate_limits::clear_for_tests();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            )));
            let server = tokio::spawn(serve_rate_limited(listener, spent_snapshot_body()));
            let err = match path {
                "live http" => client
                    .stream_codex_http_events(&buffered_test_request(), &http_test_context())
                    .await
                    .err(),
                "search" => client
                    .post_search(&search_test_request(), &http_test_context())
                    .await
                    .err(),
                _ => match client
                    .stream_codex_websocket_events_for_owner(
                        &buffered_test_request(),
                        &http_test_context(),
                        None,
                    )
                    .await
                    .unwrap()
                    .recv()
                    .await
                {
                    Some(Err(err)) => Some(err),
                    _ => None,
                },
            }
            .expect(path);
            server.abort();
            let published = published_utilizations(Some("acct"));
            let unkeyed = published_utilizations(None);
            super::super::rate_limits::clear_for_tests();

            // Both windows are spent, so the refusal still names neither.
            assert!(
                err.usage_limit
                    .as_ref()
                    .is_some_and(|limit| limit.window.is_none()),
                "{path}: {err:?}"
            );
            assert_eq!(
                published,
                [Some("1.0000".to_string()), Some("1.0000".to_string())],
                "{path}"
            );
            // The reading is held for the login's account only.
            assert_eq!(unkeyed, [None, None], "{path}");
        }
    }

    #[test]
    fn informational_events_keep_live_continuation_retry_available() {
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": false}
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "response.created"
        })));
        // A snapshot reporting a spent window ends the request.
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true}
        })));
    }

    #[test]
    fn continuation_retry_requires_previous_response_id() {
        let owner = ConversationIdentity::Main("session".into());
        let append =
            test_continuation(Some(owner.clone()), Some(17), Some("resp_1"), Some(1), None);
        let initial =
            test_continuation(Some(owner.clone()), None, None, None, Some("missing_state"));
        let timeout = CodexError {
            status: 0,
            message: "WebSocket response start timeout after 60000ms".to_string(),
            detail: Some(
                super::super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string(),
            ),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let missing = CodexError {
            status: 0,
            message: "Previous response not found".to_string(),
            detail: Some("previous_response_not_found".to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let idle = CodexError {
            status: 0,
            message: "WebSocket idle timeout after 60000ms".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        let full_context = full_context_continuation(Some(&append)).unwrap();
        assert_eq!(full_context.owner(), Some(&owner));
        assert_eq!(full_context.turn_id(), Some(17));
        assert_eq!(full_context.candidate().previous_response_id, None);
        assert_eq!(full_context.origin_socket_id(), None);
        assert_eq!(
            full_context.candidate().disabled_reason.as_deref(),
            Some("full_context_retry")
        );
        assert!(should_retry_without_continuation(&timeout, Some(&append)));
        assert!(should_retry_without_continuation(&missing, Some(&append)));
        assert!(!should_retry_without_continuation(&idle, Some(&append)));
        assert!(!should_retry_without_continuation(&timeout, Some(&initial)));
        assert!(!should_retry_without_continuation(&timeout, None));
    }
}
