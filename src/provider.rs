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

/// What `/v1/models` says about one provider next to its rows.
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
    /// Row objects without the `provider` key; the handler adds it.
    pub models: Vec<serde_json::Value>,
}

impl ModelListing {
    /// The compiled-in list, advertised as such.
    pub fn bundled(provider: &'static str, models: Vec<String>) -> Self {
        let models = models
            .into_iter()
            .map(|model| {
                serde_json::json!({
                    "type": "model",
                    "object": "model",
                    "id": model,
                    "display_name": format!("{model} ({provider})"),
                })
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
    async fn list_models(&self) -> ModelListing {
        ModelListing::bundled(self.name(), self.supported_models())
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
