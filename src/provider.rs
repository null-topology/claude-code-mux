use crate::anthropic::schema::MessagesRequest;
use crate::monitor::MonitorHandle;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;
use anyhow::Result;
use async_trait::async_trait;
use axum::{body::Body, http::StatusCode, response::Response};
use bytes::Bytes;
use clap::Subcommand;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Sign in using browser-based authentication
    Login,
    /// Sign in using a device code
    Device,
    /// Show the current authentication status
    Status,
    /// Delete stored authentication credentials
    Logout,
}

/// Who holds the credential a provider's models are used with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingAuth {
    /// The proxy stores the login and can ask the backend on its own.
    Proxy,
    /// The caller sends its own credential on every request; the proxy holds
    /// none and cannot verify anything about the backend.
    Client,
}

impl ListingAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            ListingAuth::Proxy => "proxy",
            ListingAuth::Client => "client",
        }
    }
}

/// Where the rows of a [`ModelListing`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingSource {
    /// The backend answered a listing call on the proxy's own login.
    Upstream,
    /// A list compiled into this build; nothing was verified.
    Bundled,
    /// No rows are produced for this provider.
    None,
}

impl ListingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ListingSource::Upstream => "upstream",
            ListingSource::Bundled => "bundled",
            ListingSource::None => "none",
        }
    }
}

/// `created_at` of every row the proxy lists. The Anthropic Models API allows
/// the epoch when a model's release date is unknown, and the proxy knows none.
pub const MODEL_CREATED_AT: &str = "1970-01-01T00:00:00Z";

/// One `/v1/models` row in the Anthropic Models API `ModelInfo` shape. Every
/// row the proxy lists is built here, so the key set cannot drift: `type`,
/// `id`, `display_name`, `created_at`, `capabilities`, `deprecated_at`,
/// `lifecycle`, `line`, `max_input_tokens`, `max_tokens` and `retires_at`.
/// Every row is active and belongs to no Claude line.
pub fn model_info(
    id: &str,
    display_name: &str,
    max_input_tokens: Option<u64>,
    max_tokens: Option<u64>,
    capabilities: Option<&ModelCapabilities>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "model",
        "id": id,
        "display_name": display_name,
        "created_at": MODEL_CREATED_AT,
        "capabilities": capabilities.map(ModelCapabilities::to_value),
        "deprecated_at": null,
        "lifecycle": "active",
        "line": null,
        "max_input_tokens": max_input_tokens,
        "max_tokens": max_tokens,
        "retires_at": null,
    })
}

/// The Anthropic Models API `ModelCapabilities` object, one flag per leaf.
/// [`Self::to_value`] writes the whole key tree whatever the flags say; a
/// group's own `supported` is true when any of its members is, except
/// `thinking`, whose `supported` is a flag of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub batch: bool,
    pub citations: bool,
    pub code_execution: bool,
    pub clear_thinking_20251015: bool,
    pub clear_tool_uses_20250919: bool,
    pub compact_20260112: bool,
    pub effort_low: bool,
    pub effort_medium: bool,
    pub effort_high: bool,
    pub effort_xhigh: bool,
    pub effort_max: bool,
    pub image_input: bool,
    pub pdf_input: bool,
    pub server_code_execution: bool,
    pub server_web_search: bool,
    pub structured_outputs: bool,
    pub thinking: bool,
    pub thinking_adaptive: bool,
    pub thinking_between_tools: bool,
    pub thinking_disabled: bool,
    pub thinking_enabled: bool,
}

impl ModelCapabilities {
    pub fn to_value(&self) -> serde_json::Value {
        fn leaf(supported: bool) -> serde_json::Value {
            serde_json::json!({ "supported": supported })
        }
        serde_json::json!({
            "batch": leaf(self.batch),
            "citations": leaf(self.citations),
            "code_execution": leaf(self.code_execution),
            "context_management": {
                "supported": self.clear_thinking_20251015
                    || self.clear_tool_uses_20250919
                    || self.compact_20260112,
                "clear_thinking_20251015": leaf(self.clear_thinking_20251015),
                "clear_tool_uses_20250919": leaf(self.clear_tool_uses_20250919),
                "compact_20260112": leaf(self.compact_20260112),
            },
            "effort": {
                "supported": self.effort_low
                    || self.effort_medium
                    || self.effort_high
                    || self.effort_xhigh
                    || self.effort_max,
                "low": leaf(self.effort_low),
                "medium": leaf(self.effort_medium),
                "high": leaf(self.effort_high),
                "xhigh": leaf(self.effort_xhigh),
                "max": leaf(self.effort_max),
            },
            "image_input": leaf(self.image_input),
            "pdf_input": leaf(self.pdf_input),
            "server_tools": {
                "supported": self.server_code_execution || self.server_web_search,
                "code_execution": leaf(self.server_code_execution),
                "web_search": leaf(self.server_web_search),
            },
            "structured_outputs": leaf(self.structured_outputs),
            "thinking": {
                "supported": self.thinking,
                "types": {
                    "adaptive": leaf(self.thinking_adaptive),
                    "between_tools": leaf(self.thinking_between_tools),
                    "disabled": leaf(self.thinking_disabled),
                    "enabled": leaf(self.thinking_enabled),
                },
            },
        })
    }
}

/// What a provider lists for `/v1/models`, plus the status of that listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelListing {
    pub provider: &'static str,
    pub auth: ListingAuth,
    pub source: ListingSource,
    /// `ok`, `unauthorized`, `unreachable`, or `not_listed`.
    pub status: &'static str,
    pub detail: Option<String>,
    /// RFC 3339 time of the upstream answer, when `source` is `upstream`.
    pub fetched_at: Option<String>,
    /// Rows built by [`model_info`], in the order the provider gives them.
    pub models: Vec<serde_json::Value>,
}

impl ModelListing {
    /// The compiled-in list, advertised as such, with no token limits.
    pub fn bundled(provider: &'static str, models: Vec<String>) -> Self {
        Self::bundled_with_limits(provider, models, |_| (None, None))
    }

    /// The compiled-in list, with `limits` giving each id's
    /// `(max_input_tokens, max_tokens)`. Bundled rows carry no capabilities.
    pub fn bundled_with_limits(
        provider: &'static str,
        models: Vec<String>,
        limits: impl Fn(&str) -> (Option<u64>, Option<u64>),
    ) -> Self {
        let models = models
            .into_iter()
            .map(|model| {
                let (max_input_tokens, max_tokens) = limits(&model);
                model_info(
                    &model,
                    &format!("{model} ({provider})"),
                    max_input_tokens,
                    max_tokens,
                    None,
                )
            })
            .collect();
        Self {
            provider,
            auth: ListingAuth::Proxy,
            source: ListingSource::Bundled,
            status: "ok",
            detail: None,
            fetched_at: None,
            models,
        }
    }

    /// A provider whose credential is the caller's: nothing to list.
    pub fn client_side(provider: &'static str, detail: impl Into<String>) -> Self {
        Self {
            provider,
            auth: ListingAuth::Client,
            source: ListingSource::None,
            status: "not_listed",
            detail: Some(detail.into()),
            fetched_at: None,
            models: Vec::new(),
        }
    }

    /// A listing the backend did not give: no rows, the reason in `status`.
    pub fn unavailable(provider: &'static str, status: &'static str, detail: String) -> Self {
        Self {
            provider,
            auth: ListingAuth::Proxy,
            source: ListingSource::None,
            status,
            detail: Some(detail),
            fetched_at: None,
            models: Vec::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_models(&self) -> Vec<String>;
    fn cli(&self) -> &'static dyn CliHandlers;
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    /// The models this provider can list right now. The default is the
    /// compiled-in list, marked as such; a provider that holds a login and
    /// whose backend has a listing call overrides this to ask the backend.
    /// `client_headers` are those of the client request that caused the
    /// listing (a `/v1/models` call, or a request naming a model nothing
    /// routes), and `None` when the proxy lists on its own at start.
    async fn list_models(&self, _client_headers: Option<&axum::http::HeaderMap>) -> ModelListing {
        ModelListing::bundled(self.name(), self.supported_models())
    }

    /// Whether a saved login for this provider exists, read locally with no
    /// network call. The registry calls [`Self::list_models`] only on
    /// providers that have one, when `serve` starts and when a request names
    /// a model nothing routes, on the blocking pool, so a check may block.
    /// The default, for a provider that holds no credential of its own, is
    /// `false`.
    fn has_credentials(&self) -> bool {
        false
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let _ = conversation_identity;
        self.handle_messages(body, ctx).await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response;

    async fn generate_anthropic_stream(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        Err(ProviderError::new(
            StatusCode::NOT_IMPLEMENTED,
            ProviderErrorKind::InvalidRequest,
            format!(
                "provider '{}' does not support OpenAI-compatible generation",
                self.name()
            ),
        ))
    }
}

pub enum GenerationBody {
    BufferedSse(Bytes),
    LiveSse(Body),
}

pub struct Generation {
    pub body: GenerationBody,
    pub resolved_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Permission,
    RateLimit,
    InvalidRequest,
    Api,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: StatusCode,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retry_after: Option<String>,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl ProviderError {
    pub fn new(status: StatusCode, kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            param: None,
            code: None,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Authentication => "authentication_error",
            ProviderErrorKind::Permission => "permission_error",
            ProviderErrorKind::RateLimit => "rate_limit_error",
            ProviderErrorKind::InvalidRequest => "invalid_request_error",
            ProviderErrorKind::Api => "api_error",
        }
    }
}

/// Recorded when a stream whose protocol ends with a terminal event stopped
/// before one arrived.
pub const MISSING_TERMINAL_FAILURE: &str = "stream ended before its terminal event";

/// What a streamed response turned out to be, as the code producing it saw.
///
/// The status line leaves before the body does, so a protocol failure found
/// mid-stream can no longer change the HTTP status; the producer records it
/// here and the server reads it when the body ends. That keeps the reported
/// status the one the client actually received and still lets the request be
/// recorded as failed. The first failure wins, so a healthy-looking finish
/// after it cannot erase it.
///
/// A protocol whose responses end with a terminal event — the Anthropic
/// Messages stream ends with `message_stop` — is tracked with
/// [`ResponseOutcome::requiring_terminal`]: a body that simply stopped before
/// one is a failure, not a success. Producers without that guarantee use
/// [`ResponseOutcome::default`] and report only the failures they observed.
#[derive(Clone, Default)]
pub struct ResponseOutcome {
    state: Arc<Mutex<ResponseOutcomeState>>,
}

#[derive(Default)]
struct ResponseOutcomeState {
    failure: Option<String>,
    terminal_required: bool,
    terminal_seen: bool,
}

impl ResponseOutcome {
    /// An outcome for a protocol that ends with a terminal event.
    pub fn requiring_terminal() -> Self {
        Self {
            state: Arc::new(Mutex::new(ResponseOutcomeState {
                failure: None,
                terminal_required: true,
                terminal_seen: false,
            })),
        }
    }

    /// The failure the producer observed, if any.
    pub fn failure(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.failure.clone())
    }

    /// The failure to record now that the body has ended: the first one
    /// observed, or a missing terminal event when the protocol has one.
    pub fn failure_at_end(&self) -> Option<String> {
        let state = self.state.lock().ok()?;
        if let Some(failure) = state.failure.clone() {
            return Some(failure);
        }
        (state.terminal_required && !state.terminal_seen)
            .then(|| MISSING_TERMINAL_FAILURE.to_string())
    }

    pub(crate) fn fail(&self, message: impl Into<String>) {
        if let Ok(mut state) = self.state.lock()
            && state.failure.is_none()
        {
            state.failure = Some(message.into());
        }
    }

    /// Record that the protocol's terminal event reached the client.
    pub(crate) fn mark_terminal(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.terminal_seen = true;
        }
    }
}

pub trait CliHandlers: Send + Sync {
    fn login(&self) -> Result<()>;
    fn device(&self) -> Result<()>;
    fn status(&self) -> Result<()>;
    fn logout(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: String,
    pub traffic: Option<Arc<TrafficCapture>>,
    pub monitor: Option<MonitorHandle>,
    /// Raw request material for byte-passthrough providers (the Anthropic backend).
    /// Present on real HTTP requests; None in unit tests. Forwarding these verbatim
    /// keeps the prompt-cache prefix byte-identical.
    pub passthrough: Option<Passthrough>,
}

/// Untranslated request material needed to relay a request to an upstream verbatim.
#[derive(Debug, Clone)]
pub struct Passthrough {
    /// Original request body bytes, forwarded without reserialization.
    pub raw_body: axum::body::Bytes,
    /// Original client request headers (carry Authorization + anthropic-beta).
    pub headers: axum::http::HeaderMap,
    /// Canonical upstream path (`/v1/messages` or
    /// `/v1/messages/count_tokens`) followed by the original query,
    /// e.g. `/v1/messages?beta=true`.
    pub path_and_query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path from the root to a leaf value, dot-joined and sorted. Each
    /// leaf must be a boolean, so an object in place of a flag shows up too.
    fn bool_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    bool_paths(child, &path, out);
                }
            }
            serde_json::Value::Bool(_) => out.push(prefix.to_string()),
            other => panic!("{prefix} holds {other}, not a flag"),
        }
    }

    #[test]
    fn capabilities_have_exactly_the_anthropic_key_tree() {
        let mut paths = Vec::new();
        bool_paths(&ModelCapabilities::default().to_value(), "", &mut paths);
        paths.sort_unstable();
        let mut expected = vec![
            "batch.supported",
            "citations.supported",
            "code_execution.supported",
            "context_management.supported",
            "context_management.clear_thinking_20251015.supported",
            "context_management.clear_tool_uses_20250919.supported",
            "context_management.compact_20260112.supported",
            "effort.supported",
            "effort.low.supported",
            "effort.medium.supported",
            "effort.high.supported",
            "effort.xhigh.supported",
            "effort.max.supported",
            "image_input.supported",
            "pdf_input.supported",
            "server_tools.supported",
            "server_tools.code_execution.supported",
            "server_tools.web_search.supported",
            "structured_outputs.supported",
            "thinking.supported",
            "thinking.types.adaptive.supported",
            "thinking.types.between_tools.supported",
            "thinking.types.disabled.supported",
            "thinking.types.enabled.supported",
        ];
        expected.sort_unstable();
        assert_eq!(paths, expected);
    }

    #[test]
    fn a_capability_group_is_supported_when_any_member_is() {
        let none = ModelCapabilities::default().to_value();
        for group in ["context_management", "effort", "server_tools", "thinking"] {
            assert_eq!(none[group]["supported"], false, "{group}");
        }
        let some = ModelCapabilities {
            compact_20260112: true,
            effort_max: true,
            server_web_search: true,
            ..ModelCapabilities::default()
        }
        .to_value();
        assert_eq!(some["context_management"]["supported"], true);
        assert_eq!(some["effort"]["supported"], true);
        assert_eq!(some["effort"]["low"]["supported"], false);
        assert_eq!(some["server_tools"]["supported"], true);
        assert_eq!(some["server_tools"]["code_execution"]["supported"], false);
    }

    #[test]
    fn model_info_has_exactly_the_anthropic_keys() {
        let row = model_info("m", "M", Some(10), None, None);
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capabilities",
                "created_at",
                "deprecated_at",
                "display_name",
                "id",
                "lifecycle",
                "line",
                "max_input_tokens",
                "max_tokens",
                "retires_at",
                "type",
            ]
        );
        assert_eq!(row["type"], "model");
        assert_eq!(row["created_at"], MODEL_CREATED_AT);
        assert_eq!(row["lifecycle"], "active");
        assert_eq!(row["max_input_tokens"], 10);
        for key in [
            "capabilities",
            "deprecated_at",
            "line",
            "max_tokens",
            "retires_at",
        ] {
            assert!(row[key].is_null(), "{key}");
        }
        let with_caps = model_info("m", "M", None, None, Some(&ModelCapabilities::default()));
        assert!(with_caps["capabilities"].is_object());
    }

    #[test]
    fn the_first_failure_wins_and_a_later_finish_cannot_erase_it() {
        let outcome = ResponseOutcome::requiring_terminal();
        outcome.fail("reset before the terminal event");
        outcome.fail("a second, later complaint");
        outcome.mark_terminal();

        assert_eq!(
            outcome.failure().as_deref(),
            Some("reset before the terminal event")
        );
        assert_eq!(
            outcome.failure_at_end().as_deref(),
            Some("reset before the terminal event")
        );
    }

    #[test]
    fn a_protocol_with_a_terminal_event_fails_without_one() {
        let missing = ResponseOutcome::requiring_terminal();
        assert_eq!(missing.failure(), None);
        assert_eq!(
            missing.failure_at_end().as_deref(),
            Some(MISSING_TERMINAL_FAILURE)
        );

        let complete = ResponseOutcome::requiring_terminal();
        complete.mark_terminal();
        assert_eq!(complete.failure_at_end(), None);
    }

    #[test]
    fn a_producer_without_the_stronger_contract_keeps_the_old_one() {
        // The native Responses surfaces report the failures they see and say
        // nothing about terminal events; a body that ends is still a success.
        let outcome = ResponseOutcome::default();
        assert_eq!(outcome.failure_at_end(), None);
        outcome.fail("native stream failed");
        assert_eq!(
            outcome.failure_at_end().as_deref(),
            Some("native stream failed")
        );
    }

    #[test]
    fn the_outcome_is_shared_by_every_clone() {
        let producer = ResponseOutcome::requiring_terminal();
        let consumer = producer.clone();
        producer.fail("upstream closed");
        assert_eq!(consumer.failure().as_deref(), Some("upstream closed"));
    }
}
