//! Anthropic passthrough backend.
//!
//! Unlike the other providers, this one performs no translation. Claude Code already
//! speaks the Anthropic Messages API and, when pointed at a custom base URL, forwards
//! its own subscription credentials (`Authorization: Bearer sk-ant-oat...`) plus the
//! `anthropic-beta` flags that drive prompt caching. So the correct behavior is a
//! transparent reverse proxy: relay the original body bytes and headers to
//! api.anthropic.com and stream the response straight back. The proxy holds zero
//! Anthropic credentials and never touches the cache-keyed request prefix.

use std::task::Poll;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::Stream;
use serde_json::Value;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::MessagesRequest;
use crate::logging::create_logger;
use crate::monitor::{MonitorHandle, UsageReport, usage_report_from_anthropic_body};
use crate::provider::{CliHandlers, ModelListing, Provider, RequestContext, ResponseOutcome};
use crate::providers::translate_shared::wrap_reasoning;
use crate::registry::ANTHROPIC_STYLE_ALIASES;

/// Rewrite an outgoing Anthropic request body so it survives a mid-conversation switch
/// away from the codex backend.
///
/// Claude Code stores the codex backend's reconstructed reasoning as `thinking` blocks
/// carrying an empty signature. Anthropic rejects those on replay (400
/// `Invalid signature in thinking block`), so every post-switch turn would otherwise pay
/// a failed round-trip plus Claude Code's strip-and-retry. A native Anthropic turn does
/// carry prior-turn reasoning forward, so instead of dropping it we convert each
/// signature-less `thinking` block into a tagged `text` block: Anthropic accepts text
/// without a signature and the reasoning stays in context. Genuine Anthropic reasoning
/// (a non-empty signature) is left untouched.
///
/// `rewritten` holds bytes only when something changed; `None` forwards the body
/// verbatim, keeping the byte-identical cache prefix for pure-Anthropic conversations.
///
/// The same parse also reports the model written in the outgoing document. That
/// document is the client's own body, which the proxy does not rewrite the model
/// of, so what it names is what Anthropic will run — even where the proxy pointed
/// its typed copy of the request at another model. Reading it here costs nothing:
/// the parse already happens, and the value is taken before the early return for a
/// body with no `messages`, so a request that needs no rewrite still reports it.
fn sanitize_anthropic_request(raw: &[u8], req_id: &str) -> OutgoingRequest {
    let Ok(mut doc) = serde_json::from_slice::<Value>(raw) else {
        return OutgoingRequest::default();
    };
    let Some(obj) = doc.as_object_mut() else {
        return OutgoingRequest::default();
    };

    detect_hosted_web_search_regression(obj, req_id);
    let model = obj.get("model").and_then(Value::as_str).map(str::to_string);

    let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) else {
        return OutgoingRequest {
            model,
            rewritten: None,
        };
    };
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut() {
            changed |= rehydrate_unsigned_thinking(block);
        }
    }

    OutgoingRequest {
        model,
        rewritten: changed.then(|| serde_json::to_vec(&doc).unwrap_or_else(|_| raw.to_vec())),
    }
}

/// What the relay is about to send: the model the outgoing document names, and
/// the rewritten bytes when the body needed one.
#[derive(Debug, Default)]
struct OutgoingRequest {
    model: Option<String>,
    rewritten: Option<Vec<u8>>,
}

/// Convert one signature-less `thinking` block into a tagged `text` block in place.
/// Returns whether the block was rewritten.
fn rehydrate_unsigned_thinking(block: &mut Value) -> bool {
    let Some(map) = block.as_object() else {
        return false;
    };
    if map.get("type").and_then(Value::as_str) != Some("thinking") {
        return false;
    }
    let signed = map
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|sig| !sig.is_empty());
    if signed {
        return false;
    }
    let reasoning = map.get("thinking").and_then(Value::as_str).unwrap_or("");
    *block = serde_json::json!({
        "type": "text",
        "text": wrap_reasoning(reasoning),
    });
    true
}

/// Regression tripwire. Claude Code drives its `WebSearch` tool through an isolated,
/// history-free inner call, so the hosted `web_search_20250305` tool and its
/// reconstructed `server_tool_use` / `web_search_tool_result` blocks never appear in the
/// outer transcript. If that ever changes (hosted web search reaching a request that
/// already carries assistant history), those blocks would ride the transcript across a
/// model switch and this warning flags it so the assumption can be re-checked.
fn detect_hosted_web_search_regression(obj: &serde_json::Map<String, Value>, req_id: &str) {
    let messages = obj.get("messages").and_then(Value::as_array);
    let has_assistant_history = messages.is_some_and(|ms| {
        ms.iter()
            .any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
    });
    let hosted_tool = obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|ts| {
            ts.iter()
                .any(|t| t.get("type").and_then(Value::as_str) == Some("web_search_20250305"))
        });
    let reconstructed_block = messages.is_some_and(|ms| {
        ms.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("server_tool_use") | Some("web_search_tool_result")
                        )
                    })
                })
        })
    });

    if (hosted_tool && has_assistant_history) || reconstructed_block {
        let mut fields = serde_json::Map::new();
        fields.insert("reqId".into(), Value::String(req_id.to_string()));
        fields.insert("hostedWebSearchTool".into(), Value::Bool(hosted_tool));
        fields.insert(
            "reconstructedSearchBlock".into(),
            Value::Bool(reconstructed_block),
        );
        create_logger("anthropic").warn("hosted_web_search_in_history", Some(fields));
    }
}

/// Request headers that must not be forwarded to the upstream. Hop-by-hop headers are
/// connection-scoped; `content-length` is recomputed by the client from the body; and
/// `accept-encoding` is dropped so the upstream answers with an identity encoding
/// (this build of reqwest does not decompress, so forwarding a compressed body under a
/// stale `content-encoding` would corrupt it).
fn is_stripped_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "accept-encoding"
    )
}

/// Response headers that must not be relayed back to Claude Code. Hop-by-hop and
/// framing headers are re-derived by axum for the streamed body; `content-encoding`
/// is dropped for symmetry with the identity request above.
fn is_stripped_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "content-encoding"
    )
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build anthropic passthrough client");
        Self {
            client,
            base_url: crate::config::anthropic_base_url(),
        }
    }

    async fn relay(&self, ctx: RequestContext) -> Response {
        let RequestContext {
            req_id,
            monitor,
            passthrough,
            ..
        } = ctx;
        let Some(passthrough) = passthrough else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "anthropic passthrough is missing the original request",
            );
        };

        let url = format!("{}{}", self.base_url, passthrough.path_and_query);
        let mut headers = axum::http::HeaderMap::with_capacity(passthrough.headers.len());
        for (name, value) in passthrough.headers.iter() {
            if is_stripped_request_header(name.as_str()) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }

        if let Some(monitor) = monitor.as_ref() {
            monitor.upstream_started(&req_id);
        }

        // Rehydrate signature-less codex `thinking` blocks so a mid-conversation switch
        // to Anthropic does not 400. Unchanged bodies are forwarded verbatim.
        let prepared = sanitize_anthropic_request(&passthrough.raw_body, &req_id);
        // What the relayed bytes ask for is what will run, whatever the proxy
        // decided about its own copy of the request.
        if let (Some(monitor), Some(model)) = (monitor.as_ref(), prepared.model.as_deref()) {
            monitor.model_resolved(&req_id, model);
        }
        let outgoing = match prepared.rewritten {
            Some(bytes) => reqwest::Body::from(bytes),
            None => reqwest::Body::from(passthrough.raw_body),
        };

        let upstream = self
            .client
            .post(&url)
            .headers(headers)
            .body(outgoing)
            .send()
            .await;

        match upstream {
            Ok(upstream) => {
                let status = upstream.status();
                let mut out_headers =
                    axum::http::HeaderMap::with_capacity(upstream.headers().len());
                for (name, value) in upstream.headers() {
                    if is_stripped_response_header(name.as_str()) {
                        continue;
                    }
                    out_headers.append(name.clone(), value.clone());
                }
                let body_kind = ObservedBody::from_content_type(
                    upstream
                        .headers()
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok()),
                );
                // A relayed stream ends with `message_stop`; anything else, an
                // Anthropic `error` event included, did not complete. The
                // observer reads the events as they pass and the bytes reach the
                // client exactly as they arrived. Without a monitor nothing reads
                // either result, so the relay does no work at all.
                let (body, outcome) = match (monitor, body_kind) {
                    (Some(monitor), Some(kind)) => {
                        let outcome = match kind {
                            ObservedBody::EventStream => ResponseOutcome::requiring_terminal(),
                            ObservedBody::Json => ResponseOutcome::default(),
                        };
                        let mut observer =
                            UsageObserver::new(monitor, req_id, kind, outcome.clone());
                        let mut inner = Box::pin(upstream.bytes_stream());
                        let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| {
                            match Stream::poll_next(inner.as_mut(), cx) {
                                Poll::Ready(Some(Ok(bytes))) => {
                                    observer.observe(&bytes);
                                    Poll::Ready(Some(Ok(bytes)))
                                }
                                Poll::Ready(None) => {
                                    observer.finish();
                                    Poll::Ready(None)
                                }
                                other => other,
                            }
                        }));
                        (body, Some(outcome))
                    }
                    _ => (Body::from_stream(upstream.bytes_stream()), None),
                };
                let mut response = Response::new(body);
                *response.status_mut() = status;
                *response.headers_mut() = out_headers;
                if let Some(outcome) = outcome {
                    response.extensions_mut().insert(outcome);
                }
                response
            }
            Err(err) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("anthropic upstream request failed: {err}"),
            ),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Largest JSON body or single SSE line the usage observer keeps in memory.
const MAX_OBSERVED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedBody {
    EventStream,
    Json,
}

impl ObservedBody {
    fn from_content_type(content_type: Option<&str>) -> Option<Self> {
        let content_type = content_type?.to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            Some(Self::EventStream)
        } else if content_type.starts_with("application/json") {
            Some(Self::Json)
        } else {
            None
        }
    }
}

/// Reads a relayed response as it passes: the token usage the monitor records,
/// and whether the stream actually completed. The bytes go to the client
/// untouched; the observer only looks at them. SSE events are parsed line by
/// line as they pass, a JSON body is kept up to a limit and parsed once the
/// stream ends. Anthropic's `message_start` carries the exact prompt counts, so
/// a cache miss is visible as soon as the stream starts.
struct UsageObserver {
    monitor: MonitorHandle,
    req_id: String,
    kind: ObservedBody,
    outcome: ResponseOutcome,
    pending: Vec<u8>,
    overflow: bool,
    finished: bool,
}

impl UsageObserver {
    fn new(
        monitor: MonitorHandle,
        req_id: String,
        kind: ObservedBody,
        outcome: ResponseOutcome,
    ) -> Self {
        Self {
            monitor,
            req_id,
            kind,
            outcome,
            pending: Vec::new(),
            overflow: false,
            finished: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) {
        match self.kind {
            ObservedBody::EventStream => {
                self.pending.extend_from_slice(chunk);
                let mut report = UsageReport::default();
                while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = self.pending.drain(..=end).collect();
                    let line = line.trim_ascii();
                    if let Some(data) = line.strip_prefix(b"data:")
                        && let Ok(event) = serde_json::from_slice::<Value>(data.trim_ascii())
                    {
                        self.note_protocol_event(&event);
                        report.add_event(&event, true);
                    }
                }
                if self.pending.len() > MAX_OBSERVED_BYTES {
                    self.pending = Vec::new();
                }
                self.monitor
                    .stream_progress_usage(&self.req_id, chunk.len() as u64, 1, report);
            }
            ObservedBody::Json => {
                if self.overflow {
                    return;
                }
                if self.pending.len().saturating_add(chunk.len()) > MAX_OBSERVED_BYTES {
                    self.overflow = true;
                    self.pending = Vec::new();
                } else {
                    self.pending.extend_from_slice(chunk);
                }
            }
        }
    }

    /// Judge the event by its own identity, never by the content it carries: a
    /// model that writes the word error, or a whole error document, into a text
    /// delta has not failed. Only a top-level `error` event has, and only
    /// `message_stop` ends the stream.
    fn note_protocol_event(&self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("error") => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the relayed stream reported an error event");
                self.outcome.fail(message);
            }
            Some("message_stop") => self.outcome.mark_terminal(),
            _ => {}
        }
    }

    fn finish(&mut self) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        if self.kind != ObservedBody::Json || self.overflow || self.pending.is_empty() {
            return;
        }
        if let Ok(body) = serde_json::from_slice::<Value>(&self.pending) {
            let report = usage_report_from_anthropic_body(&body);
            if !report.is_empty() {
                self.monitor.usage_reported(&self.req_id, report);
            }
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        ANTHROPIC_STYLE_ALIASES
            .iter()
            .map(|alias| (*alias).to_string())
            .collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &ANTHROPIC_CLI
    }

    /// The proxy holds no Anthropic credential, so it cannot ask what this
    /// login may use, and Claude Code already lists its own models; repeating
    /// them here only duplicated the picker. The provider is reported, its
    /// rows are not.
    async fn list_models(&self) -> ModelListing {
        ModelListing::client_side(
            "anthropic",
            "credentials are forwarded from the client; models are routed, not listed",
        )
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx).await
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx).await
    }
}

pub struct AnthropicCli;
pub static ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "The Claude backend reuses Claude Code's own login; no separate authentication is required"
        )
    }
    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "The Claude backend reuses Claude Code's own login; no separate authentication is required"
        )
    }
    fn status(&self) -> anyhow::Result<()> {
        println!("Claude backend: transparent passthrough to api.anthropic.com");
        println!("Auth: forwarded from Claude Code (no proxy credentials stored)");
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        println!("Claude backend stores no credentials; nothing to remove");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::translate_shared::{REASONING_CLOSE, REASONING_OPEN};

    #[test]
    fn strips_hop_by_hop_and_encoding_from_request() {
        assert!(is_stripped_request_header("host"));
        assert!(is_stripped_request_header("content-length"));
        assert!(is_stripped_request_header("accept-encoding"));
        assert!(is_stripped_request_header("connection"));
        // credentials and cache-relevant headers must survive
        assert!(!is_stripped_request_header("authorization"));
        assert!(!is_stripped_request_header("anthropic-beta"));
        assert!(!is_stripped_request_header("anthropic-version"));
        assert!(!is_stripped_request_header("content-type"));
    }

    #[test]
    fn strips_framing_from_response() {
        assert!(is_stripped_response_header("content-length"));
        assert!(is_stripped_response_header("content-encoding"));
        assert!(is_stripped_response_header("transfer-encoding"));
        // rate-limit and request-id headers must reach Claude Code
        assert!(!is_stripped_response_header("content-type"));
        assert!(!is_stripped_response_header("request-id"));
        assert!(!is_stripped_response_header(
            "anthropic-ratelimit-requests-remaining"
        ));
    }

    #[test]
    fn provider_reports_name_and_models() {
        let provider = AnthropicProvider::new();
        assert_eq!(provider.name(), "anthropic");
        assert!(provider.supported_models().iter().any(|m| m == "opus"));
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn unsigned_thinking_becomes_tagged_text() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "codex reasoning", "signature": ""},
                    {"type": "text", "text": "391"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let out = sanitize_anthropic_request(&raw, "req1")
            .rewritten
            .expect("should rewrite");
        let doc = parse(&out);
        let blocks = doc["messages"][1]["content"].as_array().unwrap();
        // the thinking block is gone, replaced by tagged text; the real answer survives
        assert!(blocks.iter().all(|b| b["type"] != "thinking"));
        let tagged = blocks[0]["text"].as_str().unwrap();
        assert!(tagged.starts_with(REASONING_OPEN), "{tagged}");
        assert!(tagged.contains("codex reasoning"), "{tagged}");
        assert!(tagged.ends_with(REASONING_CLOSE), "{tagged}");
        assert_eq!(blocks[1]["text"], "391");
    }

    #[test]
    fn signed_thinking_is_forwarded_verbatim() {
        // A genuine Anthropic reasoning block (non-empty signature) must not be touched,
        // so a pure-Anthropic conversation keeps its byte-identical cache prefix.
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "opus reasoning", "signature": "abc123"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        assert!(sanitize_anthropic_request(&raw, "req2").rewritten.is_none());
    }

    #[test]
    fn missing_signature_is_treated_as_unsigned() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "r"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let out = sanitize_anthropic_request(&raw, "req3")
            .rewritten
            .expect("should rewrite");
        assert_eq!(parse(&out)["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn plain_request_is_forwarded_verbatim() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        assert!(sanitize_anthropic_request(&raw, "req4").rewritten.is_none());
    }

    #[test]
    fn rewrite_is_deterministic() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "same", "signature": ""}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let a = sanitize_anthropic_request(&raw, "r").rewritten.unwrap();
        let b = sanitize_anthropic_request(&raw, "r").rewritten.unwrap();
        assert_eq!(
            a, b,
            "rewrite must be byte-stable to preserve the cache prefix"
        );
    }

    #[test]
    fn non_json_body_is_forwarded_verbatim() {
        assert!(
            sanitize_anthropic_request(b"not json", "req5")
                .rewritten
                .is_none()
        );
    }

    /// The relay sends the caller's bytes, so the model that reaches Anthropic
    /// is the one written in them. It is read from the document that leaves,
    /// in the parse the rewrite already does, whether or not anything is
    /// rewritten and whatever else the proxy decided about the request.
    #[test]
    fn the_outgoing_model_is_read_from_the_document_that_leaves() {
        let plain = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let raw = serde_json::to_vec(&plain).unwrap();
        let outgoing = sanitize_anthropic_request(&raw, "req6");
        assert_eq!(outgoing.model.as_deref(), Some("claude-opus-5"));
        assert!(
            outgoing.rewritten.is_none(),
            "reading the model must not make the relay reserialize"
        );

        // A body with no `messages` at all still names its model.
        let bare = serde_json::to_vec(&serde_json::json!({"model": "opus"})).unwrap();
        assert_eq!(
            sanitize_anthropic_request(&bare, "req7").model.as_deref(),
            Some("opus")
        );

        // The one-hour suffix is part of what the client sent, so it is
        // reported as the string it is rather than guessed away.
        let suffixed =
            serde_json::to_vec(&serde_json::json!({"model": "claude-opus-5[1m]"})).unwrap();
        assert_eq!(
            sanitize_anthropic_request(&suffixed, "req8")
                .model
                .as_deref(),
            Some("claude-opus-5[1m]")
        );

        // A rewritten body is still the one that leaves, and its model with it.
        let rewritten = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "codex reasoning", "signature": ""}
            ]}]
        });
        let raw = serde_json::to_vec(&rewritten).unwrap();
        let outgoing = sanitize_anthropic_request(&raw, "req9");
        assert_eq!(outgoing.model.as_deref(), Some("claude-sonnet-5"));
        let bytes = outgoing.rewritten.expect("should rewrite");
        assert_eq!(parse(&bytes)["model"], "claude-sonnet-5");

        // Nothing to read from, nothing invented.
        assert_eq!(sanitize_anthropic_request(b"not json", "req10").model, None);
        let modelless = serde_json::to_vec(&serde_json::json!({"messages": []})).unwrap();
        assert_eq!(sanitize_anthropic_request(&modelless, "req11").model, None);
    }

    fn observed_monitor(request_id: &str, endpoint: crate::monitor::EndpointKind) -> MonitorHandle {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(request_id, Some("s1".to_string()), None, endpoint);
        monitor.provider_selected(request_id, "anthropic", "claude-opus-5", None);
        // The response is handed to the client before its body is read.
        monitor.request_completed(request_id, 200, None, None);
        monitor
    }

    #[test]
    fn usage_observer_reads_anthropic_stream_usage_across_split_chunks() {
        let monitor = observed_monitor("r1", crate::monitor::EndpointKind::Messages);
        let outcome = ResponseOutcome::requiring_terminal();
        let mut observer = UsageObserver::new(
            monitor.clone(),
            "r1".to_string(),
            ObservedBody::EventStream,
            outcome.clone(),
        );
        let stream = concat!(
            "event: message_start\r\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,",
            "\"cache_read_input_tokens\":10126,\"cache_creation_input_tokens\":22405,",
            "\"cache_creation\":{\"ephemeral_5m_input_tokens\":4105,\"ephemeral_1h_input_tokens\":18300},",
            "\"output_tokens\":3}}}\r\n\r\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":120}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes();
        let mut relayed = Vec::new();
        for chunk in stream.chunks(37) {
            observer.observe(chunk);
            relayed.extend_from_slice(chunk);
        }
        observer.finish();
        assert_eq!(relayed, stream);
        // A stream that reached its terminal event is a success, and the bytes
        // the client received are the ones that arrived.
        assert_eq!(outcome.failure_at_end(), None);

        let state = monitor.snapshot();
        let request = &state.recent[0];
        assert_eq!(request.input_tokens, Some(2));
        assert_eq!(request.cache.read_tokens, Some(10_126));
        assert_eq!(request.cache.write_tokens, Some(22_405));
        assert_eq!(request.output_tokens, Some(120));
        assert_eq!(
            request.cache.ttl,
            Some(std::time::Duration::from_secs(60 * 60))
        );
        // The lifetime split the response sent reaches the request as it was
        // reported, each bucket on its own evidence.
        assert_eq!(request.cache.write_5m_tokens, Some(4_105));
        assert_eq!(request.cache.write_1h_tokens, Some(18_300));
        assert_eq!(
            request.cache_write_quality(),
            crate::monitor::CacheWriteQuality {
                ephemeral_5m: crate::monitor::UsageQuality::Exact,
                ephemeral_1h: crate::monitor::UsageQuality::Exact,
            }
        );
        // The buckets are a breakdown of the write, not tokens beside it, so
        // the prompt stays input plus read plus write.
        assert_eq!(request.prompt_tokens(), Some(2 + 10_126 + 22_405));
        assert!(request.cache.evaluated());
        assert!(request.stream_chunks > 0);
        let session = &state.sessions[0];
        assert_eq!(session.input_tokens, 2);
        assert_eq!(session.cache_read_tokens, 10_126);
        assert_eq!(session.cache_write_tokens, 22_405);
        assert_eq!(session.output_tokens, 120);
        assert_eq!(session.cache_write_5m_tokens, 4_105);
        assert_eq!(session.cache_write_1h_tokens, 18_300);
        assert_eq!(session.evidence.cache_write_5m.exact, 1);
        assert_eq!(session.evidence.cache_write_1h.exact, 1);
    }

    #[test]
    fn usage_observer_reads_json_bodies_when_the_stream_ends() {
        let monitor = observed_monitor("count", crate::monitor::EndpointKind::CountTokens);
        let outcome = ResponseOutcome::default();
        let mut observer = UsageObserver::new(
            monitor.clone(),
            "count".to_string(),
            ObservedBody::Json,
            outcome.clone(),
        );
        observer.observe(b"{\"input_tok");
        observer.observe(b"ens\": 4242}");
        observer.finish();
        observer.finish();

        let state = monitor.snapshot();
        assert_eq!(state.recent[0].input_tokens, Some(4_242));
        assert_eq!(state.sessions[0].input_tokens, 0);
        // A whole JSON body has no terminal event to wait for.
        assert_eq!(outcome.failure_at_end(), None);
    }

    #[test]
    fn usage_observer_reads_the_cache_creation_split_of_a_buffered_message() {
        let monitor = observed_monitor("r_buffered", crate::monitor::EndpointKind::Messages);
        // A non-streaming Messages reply: one JSON document, read once the body
        // ends. It names the five-minute bucket as a zero and says nothing at
        // all about the one-hour one.
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "msg_boundary",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 7,
                "cache_read_input_tokens": 5_000,
                "cache_creation_input_tokens": 1_536,
                "cache_creation": {"ephemeral_5m_input_tokens": 0},
                "output_tokens": 64
            }
        }))
        .unwrap();
        let outcome = ResponseOutcome::default();
        let mut observer = UsageObserver::new(
            monitor.clone(),
            "r_buffered".to_string(),
            ObservedBody::Json,
            outcome.clone(),
        );
        let mut relayed = Vec::new();
        for chunk in body.chunks(29) {
            observer.observe(chunk);
            relayed.extend_from_slice(chunk);
        }
        observer.finish();

        assert_eq!(relayed, body, "relayed bytes must stay exact");
        assert_eq!(outcome.failure_at_end(), None);

        let state = monitor.snapshot();
        let request = &state.recent[0];
        assert_eq!(request.input_tokens, Some(7));
        assert_eq!(request.cache.read_tokens, Some(5_000));
        assert_eq!(request.cache.write_tokens, Some(1_536));
        assert_eq!(request.output_tokens, Some(64));
        // A reported zero is the backend's own count; a bucket the body never
        // named stays unknown rather than becoming the rest of the write.
        assert_eq!(request.cache.write_5m_tokens, Some(0));
        assert_eq!(request.cache.write_1h_tokens, None);
        assert_eq!(
            request.cache_write_quality(),
            crate::monitor::CacheWriteQuality {
                ephemeral_5m: crate::monitor::UsageQuality::Exact,
                ephemeral_1h: crate::monitor::UsageQuality::Missing,
            }
        );
        assert_eq!(
            request.usage_quality(),
            crate::monitor::QualityFields {
                input: crate::monitor::UsageQuality::Exact,
                cache_read: crate::monitor::UsageQuality::Exact,
                cache_write: crate::monitor::UsageQuality::Exact,
                output: crate::monitor::UsageQuality::Exact,
            }
        );
        // The breakdown neither moves the write it belongs to nor the prompt.
        assert_eq!(request.prompt_tokens(), Some(7 + 5_000 + 1_536));
        // No lifetime was actually written, so none is claimed.
        assert_eq!(request.cache.ttl, None);

        let session = &state.sessions[0];
        assert_eq!(session.cache_write_tokens, 1_536);
        assert_eq!(session.cache_write_5m_tokens, 0);
        assert_eq!(session.cache_write_1h_tokens, 0);
        assert_eq!(session.evidence.cache_write.exact, 1);
        assert_eq!(session.evidence.cache_write_5m.exact, 1);
        assert_eq!(session.evidence.cache_write_1h.missing, 1);
    }

    #[test]
    fn a_top_level_error_event_fails_the_relayed_stream_without_touching_its_bytes() {
        let monitor = observed_monitor("r_err", crate::monitor::EndpointKind::Messages);
        let outcome = ResponseOutcome::requiring_terminal();
        let mut observer = UsageObserver::new(
            monitor.clone(),
            "r_err".to_string(),
            ObservedBody::EventStream,
            outcome.clone(),
        );
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        )
        .as_bytes();
        let mut relayed = Vec::new();
        for chunk in stream.chunks(23) {
            observer.observe(chunk);
            relayed.extend_from_slice(chunk);
        }
        observer.finish();

        assert_eq!(relayed, stream, "relayed bytes must stay exact");
        assert_eq!(outcome.failure().as_deref(), Some("Overloaded"));
        assert_eq!(outcome.failure_at_end().as_deref(), Some("Overloaded"));
        // Anthropic's prompt counts are its own, so they stay exact even though
        // the output never finished.
        let request = &monitor.snapshot().recent[0];
        assert_eq!(request.input_tokens, Some(11));
        assert_eq!(request.cache.read_tokens, Some(900));
        assert_eq!(
            request.usage_quality(),
            crate::monitor::QualityFields {
                input: crate::monitor::UsageQuality::Exact,
                cache_read: crate::monitor::UsageQuality::Exact,
                cache_write: crate::monitor::UsageQuality::Exact,
                output: crate::monitor::UsageQuality::Opening,
            }
        );
    }

    #[test]
    fn a_stream_that_quotes_an_error_in_its_text_still_completes() {
        let monitor = observed_monitor("r_quote", crate::monitor::EndpointKind::Messages);
        let outcome = ResponseOutcome::requiring_terminal();
        let mut observer = UsageObserver::new(
            monitor.clone(),
            "r_quote".to_string(),
            ObservedBody::EventStream,
            outcome.clone(),
        );
        // The model writes the word error and a whole error document into its
        // answer. Only the event's own type decides, so this is a success.
        let quoted = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": "error: {\"type\":\"error\",\"error\":{\"message\":\"quoted, not raised\"}}"
            }
        });
        let stream = format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_delta\n",
                "data: {quoted}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":44}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            quoted = quoted
        );
        let stream = stream.as_bytes();
        let mut relayed = Vec::new();
        for chunk in stream.chunks(19) {
            observer.observe(chunk);
            relayed.extend_from_slice(chunk);
        }
        observer.finish();

        assert_eq!(relayed, stream, "relayed bytes must stay exact");
        assert_eq!(outcome.failure(), None);
        assert_eq!(outcome.failure_at_end(), None);
        assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(44));
    }

    #[test]
    fn a_relayed_stream_that_simply_stopped_is_not_a_success() {
        let monitor = observed_monitor("r_cut", crate::monitor::EndpointKind::Messages);
        let outcome = ResponseOutcome::requiring_terminal();
        let mut observer = UsageObserver::new(
            monitor,
            "r_cut".to_string(),
            ObservedBody::EventStream,
            outcome.clone(),
        );
        observer.observe(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n");
        observer.finish();

        // No error event named a reason, but no `message_stop` arrived either.
        assert_eq!(outcome.failure(), None);
        assert_eq!(
            outcome.failure_at_end().as_deref(),
            Some(crate::provider::MISSING_TERMINAL_FAILURE)
        );
    }

    #[test]
    fn observed_body_kind_follows_content_type() {
        assert_eq!(
            ObservedBody::from_content_type(Some("text/event-stream; charset=utf-8")),
            Some(ObservedBody::EventStream)
        );
        assert_eq!(
            ObservedBody::from_content_type(Some("application/json")),
            Some(ObservedBody::Json)
        );
        assert_eq!(ObservedBody::from_content_type(Some("text/html")), None);
        assert_eq!(ObservedBody::from_content_type(None), None);
    }
}
