use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::IntoResponse;
use claude_code_mux::{
    MessagesRequest,
    anthropic::MAX_ANTHROPIC_REQUEST_BYTES,
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus, UsageQuality},
    openai_compat::MAX_OPENAI_REQUEST_BYTES,
    provider::{
        CliHandlers, Generation, GenerationBody, ListingSource, ModelListing, Provider,
        ProviderError, RequestContext,
    },
    providers::anthropic::models::{
        MAX_BODY_BYTES, anthropic_catalog_flight_pending_for_tests,
        clear_anthropic_catalog_cache_for_tests, expire_anthropic_catalog_cache_for_tests,
        insert_ended_anthropic_catalog_flight_for_tests,
        insert_unstarted_anthropic_catalog_flight_for_tests,
        set_anthropic_listing_timeout_for_tests,
    },
    registry::{Registry, clear_listed_models_for_tests},
    request_identity::ConversationIdentity,
    server::{
        AppFeatures, app, app_with_features, app_with_monitor, app_with_options,
        bind_proxy_listener,
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tower::util::ServiceExt;

#[path = "support/mock_http.rs"]
mod mock_http;

use mock_http::{Framing, MockHttp, MockReply};

fn body_string(json: &str) -> Body {
    Body::from(json.to_string())
}

struct FakeCli;

impl CliHandlers for FakeCli {
    fn login(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn device(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn logout(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

static FAKE_CLI: FakeCli = FakeCli;

struct FakeProvider {
    name: &'static str,
    models: Vec<String>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let model = body.model.unwrap_or_default();
        let sse = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_fake\",\"model\":{model:?},\"usage\":{{\"input_tokens\":2}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{name}\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            name = self.name,
        );
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: model,
        })
    }
}

struct TranslatingProvider {
    name: &'static str,
    model: &'static str,
    captured: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Provider for TranslatingProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        vec![self.model.to_string()]
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        body: MessagesRequest,
        _ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        let translated = match self.name {
            "kimi" => serde_json::to_value(
                claude_code_mux::providers::kimi::translate::request::translate_request(
                    &body,
                    claude_code_mux::providers::kimi::translate::request::TranslateOptions {
                        session_id: None,
                    },
                )
                .unwrap(),
            )
            .unwrap(),
            "grok" => serde_json::to_value(
                claude_code_mux::providers::grok::translate::request::translate_request(
                    &body,
                    self.model.to_string(),
                )
                .unwrap(),
            )
            .unwrap(),
            _ => unreachable!(),
        };
        *self.captured.lock().unwrap() = Some(translated);
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fake\",\"model\":\"test\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        Ok(Generation {
            body: GenerationBody::BufferedSse(sse.into()),
            resolved_model: self.model.to_string(),
        })
    }
}

fn translating_registry(
    name: &'static str,
    model: &'static str,
    captured: Arc<Mutex<Option<Value>>>,
) -> Arc<Registry> {
    Arc::new(Registry::from_providers(
        AliasProvider::Kimi,
        vec![Arc::new(TranslatingProvider {
            name,
            model,
            captured,
        }) as Arc<dyn Provider>],
    ))
}

type CapturedIdentity = (Option<ConversationIdentity>, Option<String>);

struct IdentityCaptureProvider {
    captured: Arc<Mutex<Vec<CapturedIdentity>>>,
    bodies: Arc<Mutex<Vec<MessagesRequest>>>,
}

#[async_trait]
impl Provider for IdentityCaptureProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["gpt-5.5".to_string(), "gpt-6-luna".to_string()]
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "legacy path").into_response()
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> axum::response::Response {
        self.captured
            .lock()
            .unwrap()
            .push((conversation_identity, ctx.session_id));
        self.bodies.lock().unwrap().push(body);
        (StatusCode::OK, "captured").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::OK, "counted").into_response()
    }
}

fn routed_registry() -> Arc<Registry> {
    Arc::new(Registry::from_providers(
        AliasProvider::Kimi,
        vec![
            Arc::new(FakeProvider {
                name: "kimi",
                models: vec!["kimi-k2.6".to_string()],
            }) as Arc<dyn Provider>,
            Arc::new(FakeProvider {
                name: "grok",
                models: vec!["grok-4.5".to_string()],
            }),
            Arc::new(FakeProvider {
                name: "cursor",
                models: vec!["cursor".to_string()],
            }),
        ],
    ))
}

async fn call_identity_ingress(
    app: &axum::Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> StatusCode {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn messages_ingress_forwards_only_strict_conversation_identity() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: captured.clone(),
        bodies: Default::default(),
    }) as Arc<dyn Provider>;
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Codex,
        [provider],
    )));
    let normal_body = || {
        json!({
            "model": "gpt-5.5",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hello"}]
        })
    };

    let cases = [
        (
            vec![("x-claude-code-session-id", "session-main")],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-agent"),
                ("x-claude-code-agent-id", "agent-direct"),
            ],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-nested"),
                ("x-claude-code-agent-id", "agent-child"),
                ("x-claude-code-parent-agent-id", "agent-parent"),
            ],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-malformed-agent"),
                ("x-claude-code-agent-id", "malformed agent"),
            ],
            normal_body(),
        ),
        (vec![], normal_body()),
        (
            vec![("x-claude-code-session-id", " \tsession-trimmed\t ")],
            normal_body(),
        ),
        (
            vec![
                ("x-claude-code-session-id", "session-auto-review"),
                ("x-claude-code-agent-id", "agent-auto-review"),
            ],
            json!({
                "model": "gpt-5.5",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "review"}],
                "system": [{
                    "type": "text",
                    "text": "You are a security monitor for autonomous AI coding agents. Review this turn."
                }]
            }),
        ),
    ];

    for (headers, body) in cases {
        assert_eq!(
            call_identity_ingress(&app, "/v1/messages", &headers, body).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        call_identity_ingress(
            &app,
            "/v1/messages/count_tokens",
            &[("x-claude-code-session-id", "session-count")],
            normal_body(),
        )
        .await,
        StatusCode::OK
    );

    assert_eq!(
        *captured.lock().unwrap(),
        vec![
            (
                Some(ConversationIdentity::Main("session-main".to_string())),
                Some("session-main".to_string()),
            ),
            (
                Some(ConversationIdentity::Agent(
                    "session-agent".to_string(),
                    "agent-direct".to_string(),
                )),
                Some("session-agent".to_string()),
            ),
            (
                Some(ConversationIdentity::Agent(
                    "session-nested".to_string(),
                    "agent-child".to_string(),
                )),
                Some("session-nested".to_string()),
            ),
            (None, Some("session-malformed-agent".to_string())),
            (None, None),
            (
                Some(ConversationIdentity::Main("session-trimmed".to_string())),
                Some(" \tsession-trimmed\t ".to_string()),
            ),
            (None, Some("session-auto-review".to_string())),
        ]
    );
}

#[tokio::test]
async fn bind_error_names_address_and_port() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();

    let err = bind_proxy_listener("127.0.0.1", port)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains(&format!("127.0.0.1:{port}")));
    assert!(err.contains("failed to bind proxy listener"));
}

#[tokio::test]
async fn configurable_bind_address_accepts_all_interfaces() {
    let listener = bind_proxy_listener("0.0.0.0", 0).await.unwrap();
    assert_eq!(listener.local_addr().unwrap().ip().to_string(), "0.0.0.0");
}

#[tokio::test]
async fn invalid_bind_address_is_actionable() {
    let err = bind_proxy_listener("not-an-ip", 18765)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid proxy bind address"));
    assert!(err.contains("not-an-ip"));
}

#[tokio::test]
async fn healthz_returns_ok() {
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

#[tokio::test]
async fn invalid_json_request_is_json_error() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = value["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

#[tokio::test]
async fn empty_body_is_invalid_json() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_model_returns_400_with_summary() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("Unknown model \"not-a-model\""));
    assert!(message.contains("Supported:"));
}

#[tokio::test]
async fn missing_model_returns_400() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    let error_type = body["error"]["type"].as_str().unwrap_or("");
    assert_eq!(error_type, "invalid_request_error");
}

/// A well-formed Anthropic request body of exactly `bytes` bytes, grown to
/// length by a padding field. It names no model on purpose: a body that clears
/// the size gate is answered by model validation, which is how a test tells the
/// two apart.
fn padded_messages_body(bytes: usize) -> Body {
    const PREFIX: &str = r#"{"messages":[{"role":"user","content":"hello"}],"padding":""#;
    const SUFFIX: &str = r#""}"#;
    let padding = bytes
        .checked_sub(PREFIX.len() + SUFFIX.len())
        .expect("a body long enough to hold the padding field");
    let text = format!("{PREFIX}{}{SUFFIX}", "a".repeat(padding));
    assert_eq!(text.len(), bytes);
    Body::from(text)
}

async fn post_anthropic(uri: &str, body: Body) -> (StatusCode, Value) {
    let response = app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    (status, value)
}

/// An image-heavy Claude Code history runs past the limit the
/// OpenAI-compatible routes use. A body over it reaches model validation, and
/// that answer is the proof it was read whole rather than cut short.
#[tokio::test]
async fn an_anthropic_body_over_the_openai_limit_is_read_whole() {
    let (status, body) = post_anthropic(
        "/v1/messages",
        padded_messages_body(MAX_OPENAI_REQUEST_BYTES + 1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(message.starts_with("Missing \"model\""), "{message}");
}

#[tokio::test]
async fn an_anthropic_body_at_the_limit_is_read_whole() {
    let (status, body) = post_anthropic(
        "/v1/messages",
        padded_messages_body(MAX_ANTHROPIC_REQUEST_BYTES),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(message.starts_with("Missing \"model\""), "{message}");
}

/// Past the limit the body is refused for its size, on both Anthropic routes,
/// and says so in the shape an Anthropic client parses. Reporting it as invalid
/// JSON would send the caller looking at the wrong thing.
#[tokio::test]
async fn an_oversized_anthropic_body_is_refused_as_too_large() {
    for uri in ["/v1/messages", "/v1/messages/count_tokens"] {
        let (status, body) =
            post_anthropic(uri, padded_messages_body(MAX_ANTHROPIC_REQUEST_BYTES + 1)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
        assert_eq!(body["type"].as_str(), Some("error"), "{uri}");
        assert_eq!(
            body["error"]["type"].as_str(),
            Some("request_too_large"),
            "{uri}"
        );
        assert_eq!(
            body["error"]["message"].as_str(),
            Some("Request body exceeded the size limit"),
            "{uri}"
        );
    }
}

/// The OpenAI-compatible surfaces keep the limit they had; only the Anthropic
/// routes were raised.
#[tokio::test]
async fn the_openai_compatible_route_keeps_its_own_body_limit() {
    let response = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: true,
            images_api: false,
            transcriptions_api: false,
        },
    )
    .oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(padded_messages_body(MAX_OPENAI_REQUEST_BYTES + 1))
            .unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body["error"]["code"].as_str(), Some("request_too_large"));
}

#[test]
fn known_codex_model_resolves_to_codex_provider() {
    let registry = Registry::new(AliasProvider::Anthropic);
    let provider = registry
        .provider_for_model("gpt-5.4", None)
        .expect("gpt-5.4 must resolve to a registered provider");
    assert_eq!(provider.name(), "codex");
}

#[tokio::test]
async fn count_tokens_routes_to_provider() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Codex provider is now concrete, so count_tokens should succeed
    let status = response.status();
    assert!(
        status != StatusCode::NOT_IMPLEMENTED,
        "count_tokens should no longer return 501 for codex models"
    );
}

#[tokio::test]
async fn context_window_hint_is_removed_before_provider_dispatch() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-luna[1m]","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn opus_5_alias_routes_to_provider() {
    let app = app(Arc::new(Registry::new(AliasProvider::Codex)));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn image_routes_reject_variations_wrong_media_and_oversized_generation() {
    let features = AppFeatures {
        responses_api: false,
        images_api: true,
        transcriptions_api: false,
    };
    let variation = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/variations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(variation.status(), StatusCode::NOT_FOUND);

    let wrong_media = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from("--x--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_media.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let oversized = app_with_features(Arc::new(Registry::with_default_alias()), None, features)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(vec![b'x'; 256 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn monitor_tracks_image_endpoint_without_session_affinity() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].endpoint.label(), "images");
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert!(state.recent[0].session_seq.is_none());
    assert!(state.recent[0].traffic_capture_path.is_none());
}

#[tokio::test]
async fn image_edit_accepts_multipart_and_validates_fields() {
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let boundary = "ccp-image-test";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"input.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/edits")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "prompt");
}

#[tokio::test]
async fn image_routes_are_independently_opt_in() {
    let disabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: true,
            transcriptions_api: false,
        },
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transcription_route_is_independently_opt_in_and_validates_multipart() {
    let disabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: false,
        },
    );
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-4o-mini-transcribe\r\n--{boundary}--\r\n"
    );
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "file");
}

#[tokio::test]
async fn transcription_route_rejects_non_audio_uploads() {
    let app = app_with_features(
        Arc::new(Registry::with_default_alias()),
        None,
        AppFeatures {
            responses_api: false,
            images_api: false,
            transcriptions_api: true,
        },
    );
    let boundary = "ccp-transcription-type-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nnot audio\r\n--{boundary}--\r\n"
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn native_responses_route_is_disabled_by_default_option() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, false);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(r#"{"model":"gpt-5.4","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabled_native_responses_route_uses_openai_errors() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert!(body.get("type").is_none());
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn chat_completions_route_uses_responses_api_gate() {
    let disabled = app_with_options(Arc::new(Registry::with_default_alias()), None, false);
    let response = disabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let enabled = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = enabled
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_json");
}

#[tokio::test]
async fn openai_routes_select_non_codex_providers_and_aliases() {
    for (uri, request, expected) in [
        (
            "/v1/chat/completions",
            json!({"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}]}),
            "kimi",
        ),
        (
            "/v1/responses",
            json!({"model":"grok-4.5","input":"hello"}),
            "grok",
        ),
        (
            "/v1/chat/completions",
            json!({"model":"cursor:gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
            "cursor",
        ),
        (
            "/v1/responses",
            json!({"model":"sonnet","input":"hello"}),
            "kimi",
        ),
    ] {
        let response = app_with_options(routed_registry(), None, true)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(body_string(&request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri} {request}");
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let text = if uri.ends_with("responses") {
            value["output"][0]["content"][0]["text"].as_str()
        } else {
            value["choices"][0]["message"]["content"].as_str()
        };
        assert_eq!(text, Some(expected));
    }
}

#[tokio::test]
async fn openai_routes_preserve_serial_tool_calls_upstream() {
    for (provider, model, uri, body, expected_choice) in [
        (
            "kimi",
            "kimi-k2.6",
            "/v1/chat/completions",
            json!({
                "model":"kimi-k2.6",
                "messages":[{"role":"user","content":"look up x"}],
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],
                "tool_choice":{"type":"function","function":{"name":"lookup"}},
                "parallel_tool_calls":false
            }),
            json!({"type":"function","function":{"name":"lookup"}}),
        ),
        (
            "grok",
            "grok-4.5",
            "/v1/responses",
            json!({
                "model":"grok-4.5",
                "input":"look up x",
                "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":"none",
                "parallel_tool_calls":false
            }),
            json!("none"),
        ),
    ] {
        let captured = Arc::new(Mutex::new(None));
        let response = app_with_options(
            translating_registry(provider, model, captured.clone()),
            None,
            true,
        )
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body_string(&body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let translated = captured.lock().unwrap().clone().unwrap();
        assert_eq!(translated["parallel_tool_calls"], false);
        assert_eq!(translated["tool_choice"], expected_choice);
    }
}

#[tokio::test]
async fn routed_openai_streams_use_surface_specific_events() {
    let chat = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let chat = String::from_utf8(
        axum::body::to_bytes(chat.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(chat.contains("chat.completion.chunk"));
    assert!(chat.contains("\"total_tokens\":3"));
    assert!(chat.ends_with("data: [DONE]\n\n"));

    let responses = app_with_options(routed_registry(), None, true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"grok-4.5","stream":true,"input":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let responses = String::from_utf8(
        axum::body::to_bytes(responses.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(responses.contains("event: response.created"));
    assert!(responses.contains("event: response.completed"));
    assert!(responses.contains("\"sequence_number\":0"));
}

#[tokio::test]
async fn non_codex_validation_uses_openai_errors_before_generation() {
    let monitor = MonitorHandle::new(10);
    let response = app_with_options(routed_registry(), Some(monitor.clone()), true)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-client-request-id", "invalid-routed-request")
                .body(body_string(
                    r#"{"model":"kimi-k2.6","messages":[{"role":"user","content":"hello"}],"temperature":0.5}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["error"]["param"], "temperature");
    assert_eq!(value["error"]["code"], "unsupported_parameter");
    assert!(
        claude_code_mux::session::existing_session_now(Some("invalid-routed-request")).is_none()
    );
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].session_seq, None);
    assert_eq!(snapshot.recent[0].provider, None);
}

#[tokio::test]
async fn chat_completions_validation_returns_openai_parameter_errors() {
    let app = app_with_options(Arc::new(Registry::with_default_alias()), None, true);
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}],"max_tokens":100}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "max_tokens");
    assert_eq!(body["error"]["code"], "unsupported_parameter");
}

#[tokio::test]
async fn unknown_routes_use_anthropic_not_found_error() {
    let app = app(Arc::new(Registry::with_default_alias()));
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap();
    assert_eq!(body["type"].as_str().unwrap_or(""), "error");
}

#[tokio::test]
async fn monitor_records_successful_request_events() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "project-session")
                .body(body_string(
                    r##"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.177.45c"},{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK.","cache_control":{"type":"ephemeral"}},{"type":"text","text":"\nYou are an interactive agent.\n\n# Environment\nYou have been invoked in the following environment: \n - Primary working directory: /projects/example\n - Is a git repository: true","cache_control":{"type":"ephemeral"}}],"output_config":{"effort":"high"}}"##,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let state = monitor.snapshot();
    assert_eq!(state.active.len(), 1);
    assert!(state.recent.is_empty());

    let _body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.recent[0].status, RequestStatus::Completed);
    assert_eq!(state.recent[0].http_status, Some(200));
    assert_eq!(
        state.recent[0].session_id.as_deref(),
        Some("project-session")
    );
    assert!(state.recent[0].session_seq.is_some());
    assert_eq!(state.recent[0].project.as_deref(), Some("example"));
    assert_eq!(state.sessions[0].project.as_deref(), Some("example"));
    assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
    assert_eq!(state.recent[0].model.as_deref(), Some("gpt-5.4"));
    assert_eq!(state.recent[0].effort.as_deref(), Some("high"));
    assert!(state.recent[0].input_tokens.is_some());
}

/// A token estimate is answered from a local tokenizer: the request never
/// reaches the backend, so it names no model as having run and contributes no
/// tokens to what the session actually spent. It is still one request the
/// caller made.
#[tokio::test]
async fn monitor_names_no_wire_model_for_a_local_token_estimate() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "estimate-session")
                .body(body_string(
                    r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 1);
    let request = &state.recent[0];
    assert_eq!(request.provider.as_deref(), Some("codex"));
    assert_eq!(request.requested_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(request.effective_model, None);
    assert_eq!(request.model.as_deref(), Some("gpt-5.4"));

    let session = &state.sessions[0];
    assert_eq!(session.request_count, 1);
    assert_eq!(session.input_tokens, 0);
    assert_eq!(session.output_tokens, 0);
    let row = session
        .models
        .iter()
        .find(|row| row.provider.as_deref() == Some("codex"))
        .expect("the estimate is counted on its provider");
    assert_eq!(row.model, None);
    assert_eq!(row.request_count, 1);
    assert_eq!(row.input_tokens, 0);
    assert_eq!(row.output_tokens, 0);
}

/// Every backend answers a token estimate locally, so none of them may name a
/// model as having run for one.
#[tokio::test]
async fn monitor_names_no_wire_model_for_a_local_kimi_token_estimate() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "kimi-estimate-session")
                .body(body_string(
                    r#"{"model":"kimi-for-coding","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    // The estimate itself is unchanged: a positive local count is still returned.
    assert!(body["input_tokens"].as_u64().unwrap_or(0) > 0);

    let state = monitor.snapshot();
    assert_eq!(state.recent.len(), 1);
    let request = &state.recent[0];
    assert_eq!(request.provider.as_deref(), Some("kimi"));
    assert_eq!(request.requested_model.as_deref(), Some("kimi-for-coding"));
    assert_eq!(request.effective_model, None);
    assert_eq!(request.model.as_deref(), Some("kimi-for-coding"));

    let session = &state.sessions[0];
    assert_eq!(session.request_count, 1);
    assert_eq!(session.input_tokens, 0);
    assert_eq!(session.output_tokens, 0);
    let row = session
        .models
        .iter()
        .find(|row| row.provider.as_deref() == Some("kimi"))
        .expect("the estimate is counted on its provider");
    assert_eq!(row.model, None);
    assert_eq!(row.request_count, 1);
    assert_eq!(row.input_tokens, 0);
    assert_eq!(row.output_tokens, 0);
}

/// A request the translator rejects never becomes an upstream request, so the
/// model it would have run on is not named. The rejection itself is unchanged.
#[tokio::test]
async fn monitor_names_no_wire_model_for_a_request_kimi_refuses_to_translate() {
    for (case, body) in [
        (
            "effort",
            r#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"output_config":{"effort":"ultra"}}"#,
        ),
        (
            "role",
            r#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"tool","content":"hi"}]}"#,
        ),
    ] {
        let monitor = MonitorHandle::new(10);
        let app = app_with_monitor(
            Arc::new(Registry::with_default_alias()),
            Some(monitor.clone()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(body_string(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}");
        let error: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error["type"], "error", "{case}");

        let state = monitor.snapshot();
        let request = &state.recent[0];
        assert_eq!(request.status, RequestStatus::Failed, "{case}");
        assert_eq!(
            request.requested_model.as_deref(),
            Some("kimi-for-coding"),
            "{case}"
        );
        assert_eq!(request.effective_model, None, "{case}");
    }
}

/// The counterpart: once the translator has produced the upstream request, the
/// model it was built for is named, and a failure after that point does not
/// take the name back.
#[tokio::test]
async fn monitor_names_the_wire_model_once_kimi_has_built_the_request() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // No Kimi credentials in the test environment, so the prepared request
    // stops at the auth check inside the client; that is after the boundary.
    assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(
        request.effective_model.as_deref(),
        Some("kimi-for-coding"),
        "the model the upstream request was built for stays named"
    );
}

#[tokio::test]
async fn monitor_records_invalid_json_failure() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .body(body_string("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Invalid JSON:"));
}

#[tokio::test]
async fn monitor_records_unknown_model_failure() {
    let _no_logins = NoSavedLogins::install();
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}],"model":"not-a-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert!(state.active.is_empty());
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    assert_eq!(state.recent[0].http_status, Some(400));
    let error = state.recent[0].error.as_deref().unwrap_or("");
    assert!(error.starts_with("Unknown model \"not-a-model\""));
    assert!(error.contains("Supported:"));
    // The model the caller asked for is kept even though nothing was routed:
    // a request that named a model the proxy does not serve is still a request
    // for that model, and no provider or wire model is invented for it.
    assert_eq!(
        state.recent[0].requested_model.as_deref(),
        Some("not-a-model")
    );
    assert_eq!(state.recent[0].provider, None);
    assert_eq!(state.recent[0].effective_model, None);
}

#[tokio::test]
async fn monitor_records_a_request_that_named_no_model() {
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(body_string(
                    r#"{"messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let state = monitor.snapshot();
    assert_eq!(state.recent[0].status, RequestStatus::Failed);
    // Nothing named a model, so none is invented from anywhere else.
    assert_eq!(state.recent[0].requested_model, None);
    assert_eq!(state.recent[0].effective_model, None);
    assert_eq!(state.recent[0].provider, None);
}

/// A label request the proxy answers itself never reaches a model. It is still
/// a request the caller made for a model, and it is counted as one.
#[tokio::test]
async fn monitor_records_a_locally_answered_request_without_a_wire_model() {
    let _mode = PinnedAgentSummary::install(Some("local"));
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::with_default_alias()),
        Some(monitor.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "local-session")
                .body(body_string(
                    r#"{"model":"gpt-5.4","max_tokens":64,"messages":[{"role":"user","content":"Describe your most recent action in 3-5 words"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let state = monitor.snapshot();
    let request = &state.recent[0];
    assert_eq!(request.provider.as_deref(), Some("local"));
    assert_eq!(request.requested_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(request.effective_model, None);
    // Nothing went upstream, so every count of the request is the proxy's own
    // and final: a prompt of nothing, no cache on either side, and the label it
    // wrote. None of them is an estimate a backend may still correct.
    assert_eq!(request.input_tokens, Some(0));
    assert_eq!(request.cache.read_tokens, Some(0));
    assert_eq!(request.cache.write_tokens, Some(0));
    assert_eq!(request.output_tokens, Some(4));
    let quality = request.usage_quality();
    assert_eq!(quality.input, UsageQuality::Exact);
    assert_eq!(quality.cache_read, UsageQuality::Exact);
    assert_eq!(quality.cache_write, UsageQuality::Exact);
    assert_eq!(quality.output, UsageQuality::Exact);
    // The lifetime split of a cache write is Anthropic's own evidence. A
    // request that wrote no cache at all reports neither bucket rather than
    // claiming two zeroes.
    assert_eq!(request.cache.write_5m_tokens, None);
    assert_eq!(request.cache.write_1h_tokens, None);
    let session = &state.sessions[0];
    assert_eq!(session.request_count, 1);
    assert_eq!(session.output_tokens, 0);
    let row = &session.models[0];
    assert_eq!(row.provider.as_deref(), Some("local"));
    assert_eq!(row.model, None);
    assert_eq!(row.request_count, 1);
}

async fn get_models(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

/// Held by every test that touches state the whole process shares: the
/// environment, the Anthropic catalog cache and its fetch flights, the
/// Anthropic listing timeout, the routing catalog of recorded listings, and
/// the `proxy.log` that `XDG_STATE_HOME` points at. The test runner runs
/// tests in parallel threads, so without it one test's variables, cached
/// catalog or log lines reach another. A test that failed while holding it
/// does not fail the ones after it.
static SHARED_STATE: Mutex<()> = Mutex::new(());

fn lock_shared_state() -> MutexGuard<'static, ()> {
    SHARED_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A loopback address nothing listens on. A port freed by dropping a
/// listener does not do: a test running beside this one can bind it next,
/// while no listener bound to port 0 is ever given a port this low.
const CLOSED_LOOPBACK: &str = "http://127.0.0.1:9";

/// Point every saved-login lookup and the state dir at an empty temporary
/// directory for the duration of a test, so neither `/v1/models` nor the
/// refresh an unknown model starts can read the developer's real logins,
/// reach the macOS Keychain or call a backend. No provider then holds a
/// login. The Claude model catalog is served by a loopback mock, its cache
/// starts empty and the listing timeout is the default one. Loopback is
/// exempt from any proxy, so a mock is always reached directly, while a
/// proxy the environment names stays in place for anything else. The guard
/// holds [`SHARED_STATE`] until it is dropped, so tests that install it run
/// one at a time, and a test must drop one before it installs the next.
struct NoSavedLogins {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    catalog: MockHttp,
    dir: tempfile::TempDir,
    _shared: MutexGuard<'static, ()>,
}

impl NoSavedLogins {
    fn install() -> Self {
        Self::with_catalog(MockHttp::catalog())
    }

    /// The same, with the catalog URL pointing at `catalog`.
    fn with_catalog(catalog: MockHttp) -> Self {
        Self::with_catalog_url(catalog.catalog_url(), catalog)
    }

    fn with_catalog_url(url: String, catalog: MockHttp) -> Self {
        let shared = lock_shared_state();
        let dir = tempfile::TempDir::new().unwrap();
        let vars: [(&'static str, Option<std::ffi::OsString>); 7] = [
            ("HOME", Some(dir.path().into())),
            ("CCP_CONFIG_DIR", Some(dir.path().join("config").into())),
            (
                "CCP_CODEX_AUTH_FILE",
                Some(dir.path().join("missing-auth.json").into()),
            ),
            ("XDG_STATE_HOME", Some(dir.path().join("state").into())),
            ("CCP_ANTHROPIC_CATALOG_URL", Some(url.into())),
            ("NO_PROXY", Some("127.0.0.1,localhost".into())),
            ("no_proxy", Some("127.0.0.1,localhost".into())),
        ];
        let previous = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in vars {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        clear_anthropic_catalog_cache_for_tests();
        set_anthropic_listing_timeout_for_tests(None);
        Self {
            previous,
            catalog,
            dir,
            _shared: shared,
        }
    }

    /// Shorten the Anthropic listing timeout until the returned guard is
    /// dropped. It borrows this guard, so the timeout can only change while
    /// the shared state lock is held.
    fn listing_timeout(&self, timeout: Duration) -> ListingTimeout<'_> {
        set_anthropic_listing_timeout_for_tests(Some(timeout));
        ListingTimeout(std::marker::PhantomData)
    }

    /// Save a toy kimi login, so kimi counts as logged in.
    fn save_kimi_login(&self) {
        let dir = self.dir.path().join("config").join("kimi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"access":"a","refresh":"r","expires":4102444800000,"scope":"openid","userId":"u"}"#,
        )
        .unwrap();
    }

    /// The fields of every `proxy.log` event with this message. Tests that
    /// do not hold the lock log to the same file meanwhile, and the logger
    /// writes an event and its newline separately, so another event can land
    /// between the two. A line is therefore read as a run of events.
    fn logged(&self, msg: &str) -> Vec<Value> {
        let text = self.log_text();
        text.lines()
            .flat_map(|line| {
                serde_json::Deserializer::from_str(line)
                    .into_iter::<Value>()
                    .map_while(Result::ok)
            })
            .filter(|event| event["msg"] == msg)
            .map(|event| event["fields"].clone())
            .collect()
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(claude_code_mux::logging::log_file()).unwrap_or_default()
    }
}

impl Drop for NoSavedLogins {
    fn drop(&mut self) {
        // The lock is a field, released only after this body has run.
        clear_anthropic_catalog_cache_for_tests();
        set_anthropic_listing_timeout_for_tests(None);
        for (key, value) in self.previous.drain(..) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// The keys of an Anthropic `ModelInfo` row, sorted.
const MODEL_INFO_KEYS: [&str; 11] = [
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
];

fn sorted_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn row_ids(value: &Value) -> Vec<&str> {
    value["data"]
        .as_array()
        .unwrap_or_else(|| panic!("no data in {value}"))
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect()
}

/// A provider whose listing and saved-login check the test fixes. `models`
/// is its compiled-in list, which routing reads; `listed` is what it lists,
/// or `None` for a listing that fails. `upstream` marks the listing as one
/// from a backend, which the routing catalog records; a bundled one it
/// ignores. `delay` is how long the listing takes to answer, `check_delay`
/// how long the saved-login check takes.
struct ListingStub {
    name: &'static str,
    models: Vec<&'static str>,
    listed: Option<Vec<&'static str>>,
    logged_in: bool,
    upstream: bool,
    delay: Duration,
    check_delay: Duration,
}

impl ListingStub {
    fn listing(name: &'static str, listed: &[&'static str]) -> Self {
        Self {
            name,
            models: Vec::new(),
            listed: Some(listed.to_vec()),
            logged_in: true,
            upstream: false,
            delay: Duration::ZERO,
            check_delay: Duration::ZERO,
        }
    }

    fn delayed(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }

    fn checked_slowly(self, check_delay: Duration) -> Self {
        Self {
            check_delay,
            ..self
        }
    }

    fn upstream_listing(name: &'static str, listed: &[&'static str]) -> Self {
        Self {
            upstream: true,
            ..Self::listing(name, listed)
        }
    }

    fn failing(name: &'static str, logged_in: bool) -> Self {
        Self {
            name,
            models: Vec::new(),
            listed: None,
            logged_in,
            upstream: false,
            delay: Duration::ZERO,
            check_delay: Duration::ZERO,
        }
    }
}

#[async_trait]
impl Provider for ListingStub {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.iter().map(|id| id.to_string()).collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn list_models(&self, _client_headers: Option<&axum::http::HeaderMap>) -> ModelListing {
        tokio::time::sleep(self.delay).await;
        match &self.listed {
            Some(listed) => {
                let mut listing = ModelListing::bundled(
                    self.name,
                    listed.iter().map(|id| id.to_string()).collect(),
                );
                if self.upstream {
                    listing.source = ListingSource::Upstream;
                }
                listing
            }
            None => ModelListing::unavailable(self.name, "unreachable", "stub down".to_string()),
        }
    }

    fn has_credentials(&self) -> bool {
        // The server runs this check on the blocking pool.
        std::thread::sleep(self.check_delay);
        self.logged_in
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }
}

fn listing_registry(stubs: Vec<ListingStub>) -> Arc<Registry> {
    Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        stubs
            .into_iter()
            .map(|stub| Arc::new(stub) as Arc<dyn Provider>),
    ))
}

fn listing_app(stubs: Vec<ListingStub>) -> axum::Router {
    app(listing_registry(stubs))
}

fn five_model_app() -> axum::Router {
    listing_app(vec![ListingStub::listing(
        "stub",
        &["m1", "m2", "m3", "m4", "m5"],
    )])
}

/// Like `get_models`, with request headers, and the raw response kept so a
/// test can read its headers.
async fn get_models_response(
    app: axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (axum::response::Response, Value) {
    let mut request = Request::builder().method(Method::GET).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("{uri}: not JSON: {}", String::from_utf8_lossy(&bytes)));
    (
        axum::response::Response::from_parts(parts, Body::empty()),
        value,
    )
}

#[tokio::test]
async fn models_list_has_exactly_the_anthropic_keys() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        sorted_keys(&value),
        ["data", "first_id", "has_more", "last_id"]
    );
    let data = value["data"].as_array().unwrap();
    // No provider holds a login and the request carries no credential, so
    // the only rows are the first-party models of the public Claude catalog,
    // each once: the bedrock-only model and the repeat from the second
    // selector are left out.
    assert_eq!(
        row_ids(&value),
        ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
    );
    for row in data {
        assert_eq!(sorted_keys(row), MODEL_INFO_KEYS, "{row}");
        assert_eq!(row["type"], "model");
        assert_eq!(row["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(row["lifecycle"], "active");
        for key in ["deprecated_at", "retires_at"] {
            assert!(row[key].is_null(), "{key} in {row}");
        }
        // The catalog describes no capabilities.
        assert!(row["capabilities"].is_null(), "{row}");
    }
    // Without `limit` every row comes back.
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(value["first_id"], data[0]["id"]);
    assert_eq!(value["last_id"], data[data.len() - 1]["id"]);

    let opus = &data[0];
    assert_eq!(opus["display_name"], "Opus 5.5");
    assert_eq!(opus["max_input_tokens"], json!(1_000_000));
    assert_eq!(opus["max_tokens"], json!(128_000));
    assert_eq!(opus["line"], "opus");
    assert_eq!(data[1]["line"], "sonnet");
    assert_eq!(data[1]["max_tokens"], json!(64_000));
    // A catalog model without a name or runtime keeps its id and no limits.
    let haiku = &data[2];
    assert_eq!(haiku["display_name"], "claude-haiku-5-5");
    for key in ["max_input_tokens", "max_tokens", "line"] {
        assert!(haiku[key].is_null(), "{key} in {haiku}");
    }
}

/// A provider without a saved login is not asked and adds no rows, Codex
/// included; one that holds a login lists as before.
#[tokio::test]
async fn models_list_only_providers_that_hold_a_login() {
    let no_logins = NoSavedLogins::install();
    no_logins.save_kimi_login();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models").await;

    assert_eq!(status, StatusCode::OK, "{value}");
    let data = value["data"].as_array().unwrap();
    let row = |id: &str| {
        data.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from {value}"))
    };
    assert_eq!(
        row("kimi-for-coding")["display_name"],
        "kimi-for-coding (kimi)"
    );
    // Kimi clamps the output of every model but k3.
    assert_eq!(row("kimi-for-coding")["max_tokens"], json!(32000));
    assert_eq!(row("k2.6")["max_tokens"], json!(32000));
    assert!(row("kimi-k3")["max_tokens"].is_null());
    assert!(row("kimi-k3")["line"].is_null());
    assert_eq!(row("claude-opus-5-5")["display_name"], "Opus 5.5");

    let ids = row_ids(&value);
    assert!(!ids.iter().any(|id| id.starts_with("gpt-")), "{ids:?}");
    assert!(!ids.iter().any(|id| id.starts_with("grok-")), "{ids:?}");
    assert!(!ids.iter().any(|id| id.starts_with("cursor")), "{ids:?}");
}

#[tokio::test]
async fn models_keep_provider_order_then_listing_order() {
    let app = listing_app(vec![
        ListingStub::listing("kimi", &["k-late"]),
        ListingStub::listing("grok", &["g-z", "g-a"]),
    ]);
    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row_ids(&value), ["g-z", "g-a", "k-late"]);
}

#[tokio::test]
async fn models_list_keeps_ids_naming_a_claude_model() {
    let app = listing_app(vec![ListingStub::listing(
        "stub",
        &["claude-sonnet-5", "Anthropic-mirror", "plain-model"],
    )]);
    let (status, value) = get_models(app.clone(), "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        row_ids(&value),
        ["claude-sonnet-5", "Anthropic-mirror", "plain-model"]
    );
    let (status, row) = get_models(app, "/v1/models/claude-sonnet-5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row["id"], "claude-sonnet-5");
}

#[tokio::test]
async fn models_list_keeps_one_row_per_id_from_the_routed_provider() {
    // "dedupe-shared" is on kimi's compiled-in list, so routing sends it to
    // kimi although grok lists it first; nothing routes "dedupe-loose", so
    // the first row wins.
    let mut kimi = ListingStub::listing("kimi", &["dedupe-shared", "dedupe-loose"]);
    kimi.models = vec!["dedupe-shared"];
    let app = listing_app(vec![
        ListingStub::listing("grok", &["dedupe-shared", "dedupe-loose"]),
        kimi,
    ]);
    let (status, value) = get_models(app, "/v1/models").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(row_ids(&value), ["dedupe-loose", "dedupe-shared"]);
    assert_eq!(value["data"][0]["display_name"], "dedupe-loose (grok)");
    assert_eq!(value["data"][1]["display_name"], "dedupe-shared (kimi)");
}

#[tokio::test]
async fn models_pages_forward_with_after_id_and_limit() {
    let cases: [(&str, &[&str], bool); 7] = [
        ("/v1/models?limit=2", &["m1", "m2"], true),
        ("/v1/models?limit=1", &["m1"], true),
        (
            "/v1/models?limit=1000",
            &["m1", "m2", "m3", "m4", "m5"],
            false,
        ),
        ("/v1/models?after_id=m2&limit=2", &["m3", "m4"], true),
        ("/v1/models?after_id=m3&limit=2", &["m4", "m5"], false),
        ("/v1/models?after_id=m2", &["m3", "m4", "m5"], false),
        ("/v1/models?after_id=m5", &[], false),
    ];
    for (uri, expected, has_more) in cases {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        assert_eq!(value["has_more"], json!(has_more), "{uri}");
        assert_eq!(value["first_id"], json!(expected.first()), "{uri}");
        assert_eq!(value["last_id"], json!(expected.last()), "{uri}");
    }
}

#[tokio::test]
async fn models_pages_backward_with_before_id_and_limit() {
    let cases: [(&str, &[&str], bool); 4] = [
        ("/v1/models?before_id=m5&limit=2", &["m3", "m4"], true),
        ("/v1/models?before_id=m3&limit=5", &["m1", "m2"], false),
        ("/v1/models?before_id=m4", &["m1", "m2", "m3"], false),
        ("/v1/models?before_id=m1", &[], false),
    ];
    for (uri, expected, has_more) in cases {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        assert_eq!(value["has_more"], json!(has_more), "{uri}");
        assert_eq!(value["first_id"], json!(expected.first()), "{uri}");
        assert_eq!(value["last_id"], json!(expected.last()), "{uri}");
    }
}

#[tokio::test]
async fn models_reject_bad_paging_parameters_with_a_json_error() {
    let cases = [
        ("/v1/models?limit=0", "limit"),
        ("/v1/models?limit=1001", "limit"),
        ("/v1/models?limit=abc", "limit"),
        ("/v1/models?limit=-1", "limit"),
        ("/v1/models?limit=1.5", "limit"),
        ("/v1/models?limit=", "limit"),
        (
            "/v1/models?after_id=m1&before_id=m3",
            "after_id and before_id",
        ),
        ("/v1/models?after_id=nope", "after_id"),
        ("/v1/models?before_id=nope", "before_id"),
    ];
    for (uri, named) in cases {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {value}");
        assert_eq!(value["type"], "error", "{uri}");
        assert_eq!(value["error"]["type"], "invalid_request_error", "{uri}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains(named), "{uri}: {message}");
    }
}

#[tokio::test]
async fn models_lifecycle_filter_accepts_every_array_form() {
    let all = ["m1", "m2", "m3", "m4", "m5"];
    let none: [&str; 0] = [];
    let cases: [(&str, &[&str]); 7] = [
        ("/v1/models?lifecycle=active", &all),
        ("/v1/models?lifecycle=active,retired", &all),
        ("/v1/models?lifecycle[]=retired&lifecycle[]=active", &all),
        ("/v1/models?lifecycle%5B%5D=active", &all),
        ("/v1/models?lifecycle=deprecated&lifecycle=retired", &none),
        ("/v1/models?lifecycle=deprecated,retired,deprecated", &none),
        ("/v1/models?lifecycle[]=retired", &none),
    ];
    for (uri, expected) in cases {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        if expected.is_empty() {
            assert!(value["first_id"].is_null(), "{uri}");
            assert!(value["last_id"].is_null(), "{uri}");
            assert_eq!(value["has_more"], json!(false), "{uri}");
        }
    }
}

#[tokio::test]
async fn models_lifecycle_filter_rejects_unknown_values() {
    for uri in [
        "/v1/models?lifecycle=bogus",
        "/v1/models?lifecycle=active,bogus",
        "/v1/models?lifecycle[]=active&lifecycle[]=Active",
        "/v1/models?lifecycle=",
    ] {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {value}");
        assert_eq!(value["error"]["type"], "invalid_request_error", "{uri}");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("lifecycle"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn models_retrieve_answers_the_listed_row() {
    let app = listing_app(vec![ListingStub::listing("stub", &["m1", "gpt-6.1-sol"])]);
    let (_, list) = get_models(app.clone(), "/v1/models").await;
    let (status, row) = get_models(app, "/v1/models/gpt-6.1-sol").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(sorted_keys(&row), MODEL_INFO_KEYS);
    assert_eq!(row, list["data"][1]);
    assert_eq!(row["id"], "gpt-6.1-sol");
}

#[tokio::test]
async fn models_retrieve_answers_404_for_an_unlisted_id() {
    let app = listing_app(vec![ListingStub::listing("stub", &["m1"])]);
    let (status, value) = get_models(app, "/v1/models/gpt-unknown.2").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "not_found_error");
    assert_eq!(value["error"]["message"], "model: gpt-unknown.2");
}

#[tokio::test]
async fn models_retrieve_finds_a_bundled_row() {
    let no_logins = NoSavedLogins::install();
    no_logins.save_kimi_login();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, row) = get_models(app.clone(), "/v1/models/kimi-for-coding").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row["display_name"], "kimi-for-coding (kimi)");
    let (status, _) = get_models(app, "/v1/models/opus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn models_retrieve_finds_a_claude_catalog_row() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (_, list) = get_models(app.clone(), "/v1/models").await;
    let (status, row) = get_models(app.clone(), "/v1/models/claude-sonnet-5-5").await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(sorted_keys(&row), MODEL_INFO_KEYS);
    assert_eq!(row, list["data"][1]);
    // A model the catalog offers elsewhere only is not a row.
    let (status, _) = get_models(app, "/v1/models/claude-bedrock-only-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn models_fail_when_a_provider_with_a_login_cannot_list() {
    for uri in ["/v1/models", "/v1/models/a1"] {
        let app = listing_app(vec![
            ListingStub::listing("alpha", &["a1"]),
            ListingStub::failing("beta", true),
        ]);
        let (status, value) = get_models(app, uri).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}: {value}");
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "api_error");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("beta model listing unavailable (unreachable)"),
            "{uri}: {message}"
        );
    }
}

/// The 502 names the first failing provider in registry order, not the one
/// that failed first.
#[tokio::test]
async fn models_name_the_earlier_provider_when_a_later_one_fails_first() {
    for uri in ["/v1/models", "/v1/models/a1"] {
        let app = listing_app(vec![
            ListingStub::failing("alpha", true).delayed(Duration::from_millis(200)),
            ListingStub::failing("beta", true),
        ]);
        let (status, value) = get_models(app, uri).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}: {value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("alpha model listing unavailable (unreachable)"),
            "{uri}: {message}"
        );
        assert!(!message.contains("beta"), "{uri}: {message}");
    }
}

/// Providers are asked at once, and each one's login check leads straight
/// into its own listing: two slow listings take about as long as one, and so
/// does a slow login check beside another provider's slow listing.
#[tokio::test]
async fn models_ask_providers_concurrently() {
    let delay = Duration::from_millis(1000);
    // Run one after the other, either case takes at least twice the delay.
    // The bound sits just under that, leaving a loaded machine most of a
    // delay of slack.
    let bound = Duration::from_millis(1900);
    let cases = [
        (
            "slow listings",
            ListingStub::listing("alpha", &["a1"]).delayed(delay),
            ListingStub::listing("beta", &["b1"]).delayed(delay),
        ),
        (
            "slow check beside a slow listing",
            ListingStub::listing("alpha", &["a1"]).checked_slowly(delay),
            ListingStub::listing("beta", &["b1"]).delayed(delay),
        ),
    ];
    for (case, alpha, beta) in cases {
        let app = listing_app(vec![alpha, beta]);
        let started = std::time::Instant::now();
        let (status, value) = get_models(app, "/v1/models").await;
        let elapsed = started.elapsed();
        assert_eq!(status, StatusCode::OK, "{case}: {value}");
        assert_eq!(row_ids(&value), ["a1", "b1"], "{case}");
        assert!(elapsed >= delay, "{case}: {elapsed:?}");
        assert!(elapsed < bound, "{case}: {elapsed:?}");
    }
}

#[tokio::test]
async fn models_omit_a_provider_without_a_login_that_cannot_list() {
    let app = listing_app(vec![
        ListingStub::listing("alpha", &["a1"]),
        ListingStub::failing("beta", false),
    ]);
    let (status, value) = get_models(app.clone(), "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), ["a1"]);
    let (status, _) = get_models(app.clone(), "/v1/models/a1").await;
    assert_eq!(status, StatusCode::OK);
    // Naming a provider without a login fails hard, and says why.
    let (status, value) = get_models(app, "/v1/models?provider=beta").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    assert_eq!(value["error"]["type"], "api_error");
    assert_eq!(
        value["error"]["message"],
        "beta model listing unavailable (no saved login)"
    );
}

#[tokio::test]
async fn models_responses_carry_a_fresh_request_id() {
    let app = listing_app(vec![ListingStub::listing("stub", &["m1"])]);
    let failing = listing_app(vec![ListingStub::failing("stub", true)]);
    let cases = [
        (app.clone(), "/v1/models", StatusCode::OK),
        (app.clone(), "/v1/models?limit=0", StatusCode::BAD_REQUEST),
        (
            app.clone(),
            "/v1/models?provider=nope",
            StatusCode::BAD_REQUEST,
        ),
        (app.clone(), "/v1/models/m1", StatusCode::OK),
        (app.clone(), "/v1/models/nope", StatusCode::NOT_FOUND),
        (failing.clone(), "/v1/models", StatusCode::BAD_GATEWAY),
        (failing, "/v1/models/m1", StatusCode::BAD_GATEWAY),
    ];
    let mut seen = std::collections::HashSet::new();
    for (app, uri, expected) in cases {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{uri}");
        let id = request_id(&response).to_string();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{uri}: {id:?}");
        assert!(seen.insert(id), "{uri} reused a request id");
    }
}

/// The aggregate list fails when an earlier provider with a login cannot
/// list, and the backend listing of a later provider is still recorded, so
/// an id only that listing names routes to it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_record_a_later_listing_when_an_earlier_provider_fails() {
    // The routing catalog is process-wide.
    let _shared = lock_shared_state();
    clear_listed_models_for_tests();
    let registry = listing_registry(vec![
        ListingStub::failing("alpha", true),
        ListingStub::upstream_listing("beta", &["beta-listed-only"]),
    ]);
    assert!(
        registry
            .provider_for_model("beta-listed-only", None)
            .is_none()
    );

    let (status, value) = get_models(app(registry.clone()), "/v1/models").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("alpha model listing unavailable (unreachable)"),
        "{message}"
    );
    assert_eq!(
        registry
            .provider_for_model("beta-listed-only", None)
            .map(|provider| provider.name()),
        Some("beta")
    );
    clear_listed_models_for_tests();
}

/// `anthropic-version` and `anthropic-workspace-id` change nothing.
#[tokio::test]
async fn models_ignore_the_anthropic_request_headers() {
    let headers = [
        ("anthropic-version", "2023-06-01"),
        ("anthropic-workspace-id", "wrkspc_test"),
    ];
    for uri in ["/v1/models", "/v1/models/m2"] {
        let (plain, plain_body) = get_models_response(five_model_app(), uri, &[]).await;
        let (with, with_body) = get_models_response(five_model_app(), uri, &headers).await;
        assert_eq!(plain.status(), StatusCode::OK, "{uri}: {plain_body}");
        assert_eq!(with.status(), StatusCode::OK, "{uri}: {with_body}");
        assert_eq!(with_body, plain_body, "{uri}");
    }
}

/// Every path under `/v1/models/` gets the JSON envelope and a request id:
/// an empty id and a path that does not decode to UTF-8 answer 404 with the
/// remainder as sent, and an id holding `/` is looked up as sent or as `%2F`.
#[tokio::test]
async fn models_retrieve_answers_every_path_under_the_route() {
    // A listing pass would answer 502 here, so the 404s also show none ran.
    let failing = listing_app(vec![ListingStub::failing("stub", true)]);
    for (uri, message) in [
        ("/v1/models/", "model: "),
        ("/v1/models/%FF", "model: %FF"),
        ("/v1/models/org/%FF", "model: org/%FF"),
    ] {
        let (response, value) = get_models_response(failing.clone(), uri, &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}: {value}");
        assert_eq!(value["type"], "error", "{uri}");
        assert_eq!(value["error"]["type"], "not_found_error", "{uri}");
        assert_eq!(value["error"]["message"], message, "{uri}");
        let id = request_id(&response);
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{uri}: {id:?}");
    }

    let app = listing_app(vec![ListingStub::listing("stub", &["m1", "org/model-1"])]);
    for uri in ["/v1/models/org/model-1", "/v1/models/org%2Fmodel-1"] {
        let (response, row) = get_models_response(app.clone(), uri, &[]).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}: {row}");
        assert_eq!(row["id"], "org/model-1", "{uri}");
        let id = request_id(&response);
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{uri}: {id:?}");
    }
    let (response, value) = get_models_response(app, "/v1/models/org/nope", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{value}");
    assert_eq!(value["error"]["message"], "model: org/nope");
}

/// A cursor is resolved over every row, whatever the `lifecycle` filter
/// keeps: one naming a row the filter drops is valid, one naming no row is
/// not. The parameter checks still come first.
#[tokio::test]
async fn models_lifecycle_filter_resolves_cursors_over_every_row() {
    for uri in [
        "/v1/models?lifecycle=deprecated&after_id=m2",
        "/v1/models?lifecycle=retired&before_id=m4&limit=1",
    ] {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        assert_eq!(
            sorted_keys(&value),
            ["data", "first_id", "has_more", "last_id"]
        );
        assert_eq!(value["data"], json!([]), "{uri}");
        assert!(value["first_id"].is_null(), "{uri}");
        assert!(value["last_id"].is_null(), "{uri}");
        assert_eq!(value["has_more"], json!(false), "{uri}");
    }
    let (status, value) = get_models(
        five_model_app(),
        "/v1/models?lifecycle=deprecated&after_id=nope",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("after_id:"), "{message}");
    let (status, value) = get_models(
        five_model_app(),
        "/v1/models?lifecycle=deprecated&after_id=m1&before_id=m3",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("after_id and before_id"),
        "{value}"
    );
}

/// An empty `after_id=` or `before_id=` counts as absent, the check for both
/// cursors included; the other cursor is still resolved.
#[tokio::test]
async fn models_treat_an_empty_cursor_as_absent() {
    let all = ["m1", "m2", "m3", "m4", "m5"];
    let cases: [(&str, &[&str], bool); 6] = [
        ("/v1/models?after_id=", &all, false),
        ("/v1/models?before_id=", &all, false),
        ("/v1/models?after_id=&limit=2", &["m1", "m2"], true),
        ("/v1/models?before_id=&limit=2", &["m1", "m2"], true),
        ("/v1/models?after_id=&before_id=m3", &["m1", "m2"], false),
        ("/v1/models?before_id=&after_id=m3", &["m4", "m5"], false),
    ];
    for (uri, expected, has_more) in cases {
        let (status, value) = get_models(five_model_app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        assert_eq!(value["has_more"], json!(has_more), "{uri}");
    }
    let (status, value) = get_models(five_model_app(), "/v1/models?after_id=&before_id=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("before_id:"), "{message}");
}

#[tokio::test]
async fn models_endpoint_provider_filter_narrows_rows() {
    let no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    // Without a login, naming kimi fails rather than answering no rows.
    let (status, value) = get_models(app.clone(), "/v1/models?provider=kimi").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    assert_eq!(value["error"]["type"], "api_error");
    assert_eq!(
        value["error"]["message"],
        "kimi model listing unavailable (no saved login)"
    );

    no_logins.save_kimi_login();
    let (status, value) = get_models(app.clone(), "/v1/models?provider=kimi").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        row_ids(&value),
        ["kimi-for-coding", "kimi-k2.6", "kimi-k3", "k2.6", "k3"]
    );
    // Naming anthropic asks the catalog alone.
    let (status, value) = get_models(app, "/v1/models?provider=anthropic").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        row_ids(&value),
        ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
    );
}

#[tokio::test]
async fn models_endpoint_provider_filter_rejects_unknown_provider() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?provider=nope").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(value["error"]["type"], "invalid_request_error");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("Unknown provider \"nope\""));
    assert!(message.contains("codex"));
}

#[tokio::test]
async fn models_endpoint_provider_filter_fails_hard_when_listing_unavailable() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?provider=codex").await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("codex model listing unavailable (no saved login)")
    );
}

#[tokio::test]
async fn models_endpoint_respects_limit() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(value["has_more"], json!(true));
    assert_eq!(value["last_id"], data[1]["id"]);
}

/// The Claude rows the catalog fixture yields.
const CATALOG_IDS: [&str; 3] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];

/// A toy API key; it must never reach a log line.
const TOY_API_KEY: &str = "sk-ant-toy-key-0123456789";

/// Rows a mock of Anthropic's own listing answers, one per lifecycle and
/// one with a field the proxy does not know, which must come back as is.
fn relayed_rows() -> [Value; 4] {
    let row = |id: &str, lifecycle: &str| {
        json!({
            "type": "model",
            "id": id,
            "display_name": format!("Relayed {id}"),
            "created_at": "2026-05-01T00:00:00Z",
            "capabilities": {
                "effort": {"supported": true, "max": {"supported": false}},
                "thinking": {"supported": true, "types": {"adaptive": {"supported": true}}}
            },
            "deprecated_at": null,
            "lifecycle": lifecycle,
            "line": "opus",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "retires_at": null
        })
    };
    let mut deprecated = row("claude-relayed-b", "deprecated");
    deprecated["deprecated_at"] = json!("2026-10-01T00:00:00Z");
    deprecated["retires_at"] = json!("2027-01-01T00:00:00Z");
    let mut unknown_field = row("claude-relayed-c", "active");
    unknown_field["future_field"] = json!({"kept": [1, 2]});
    [
        row("claude-relayed-a", "active"),
        deprecated,
        unknown_field,
        row("claude-relayed-r", "retired"),
    ]
}

/// A mock of Anthropic's `GET /v1/models` that pages: the first two rows,
/// then the other two with a row without an id between them.
fn relay_upstream() -> MockHttp {
    MockHttp::start(|request| {
        let [a, b, c, r] = relayed_rows();
        let body = match request.query("after_id").as_deref() {
            None => json!({
                "data": [a, b],
                "first_id": "claude-relayed-a",
                "has_more": true,
                "last_id": "claude-relayed-b",
            }),
            Some("claude-relayed-b") => json!({
                "data": [c, {"type": "model"}, r],
                "first_id": "claude-relayed-c",
                "has_more": false,
                "last_id": "claude-relayed-r",
            }),
            Some(_) => json!({"data": [], "has_more": false}),
        };
        MockReply::json(200, body.to_string())
    })
}

/// An app whose only provider is the Anthropic passthrough, sending to
/// `base`.
fn anthropic_listing_app(base: &str) -> axum::Router {
    app(Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        [anthropic_provider_at(base)],
    )))
}

/// The Anthropic listing timeout that [`NoSavedLogins::listing_timeout`]
/// shortened, restored when dropped. The timeout is process-wide, so the
/// guard lives no longer than the `NoSavedLogins` that holds the lock.
struct ListingTimeout<'a>(std::marker::PhantomData<&'a NoSavedLogins>);

impl Drop for ListingTimeout<'_> {
    fn drop(&mut self) {
        set_anthropic_listing_timeout_for_tests(None);
    }
}

/// The catalog is fetched once and then served from memory, until the
/// cached copy expires. The fetch carries nothing of the client request.
#[tokio::test]
async fn claude_catalog_is_cached_until_it_expires() {
    let no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let client_headers = [
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "toy-beta"),
    ];
    for uri in [
        "/v1/models",
        "/v1/models/claude-opus-5-5",
        "/v1/models?limit=1",
    ] {
        let (response, value) = get_models_response(app.clone(), uri, &client_headers).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}: {value}");
    }
    assert_eq!(no_logins.catalog.count(), 1);
    let request = &no_logins.catalog.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, mock_http::CATALOG_PATH);
    for header in ["anthropic-version", "anthropic-beta", "x-api-key"] {
        assert!(request.header(header).is_none(), "{header}");
    }
    let fetched = no_logins.logged("anthropic model catalog fetched");
    assert_eq!(fetched.len(), 1, "{fetched:?}");
    assert_eq!(fetched[0]["version"], json!(7));
    assert_eq!(fetched[0]["models"], json!(3));

    expire_anthropic_catalog_cache_for_tests();
    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(no_logins.catalog.count(), 2);
}

/// Listings that arrive while the catalog is being fetched wait for that
/// fetch instead of starting their own.
#[tokio::test]
async fn concurrent_listings_share_one_catalog_fetch() {
    let catalog = MockHttp::start(|_| {
        MockReply::json(200, mock_http::CATALOG_FIXTURE).delayed(Duration::from_millis(300))
    });
    let no_logins = NoSavedLogins::with_catalog(catalog);
    let app = app(Arc::new(Registry::with_default_alias()));
    let answers =
        futures_util::future::join_all((0..5).map(|_| get_models(app.clone(), "/v1/models"))).await;
    for (status, value) in answers {
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(row_ids(&value), CATALOG_IDS);
    }
    assert_eq!(no_logins.catalog.count(), 1);
}

/// A call that stops waiting for the catalog does not stop the fetch: it
/// runs to its end and stores the catalog, and a later call is served from
/// that copy without fetching again.
#[tokio::test]
async fn a_catalog_fetch_outlives_a_call_that_stopped_waiting() {
    // The catalog answers well after the call stops waiting and well before
    // the fetch's own budget of four seconds runs out.
    let delay = Duration::from_millis(2000);
    let catalog =
        MockHttp::start(move |_| MockReply::json(200, mock_http::CATALOG_FIXTURE).delayed(delay));
    let no_logins = NoSavedLogins::with_catalog(catalog);
    let _timeout = no_logins.listing_timeout(Duration::from_millis(300));
    let app = app(Arc::new(Registry::with_default_alias()));

    let (status, value) = get_models(app.clone(), "/v1/models").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("public model catalog timed out"),
        "{message}"
    );

    // The fetch leaves the flight map once it has stored the catalog. Wait
    // for that rather than for a fixed time, which a loaded machine can
    // outlast.
    let url = no_logins.catalog.catalog_url();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while anthropic_catalog_flight_pending_for_tests(&url) {
        assert!(
            std::time::Instant::now() < deadline,
            "the catalog fetch never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(no_logins.catalog.count(), 1);
    assert_eq!(no_logins.logged("anthropic model catalog fetched").len(), 1);
    assert!(
        no_logins
            .logged("anthropic model catalog fetch failed")
            .is_empty()
    );
}

/// A flight whose result is already in but that never left the flight map
/// is not joined: the next call starts a fetch of its own in its place.
#[tokio::test]
async fn an_ended_catalog_flight_left_behind_is_replaced() {
    let no_logins = NoSavedLogins::install();
    let url = no_logins.catalog.catalog_url();
    insert_ended_anthropic_catalog_flight_for_tests(&url);
    assert!(anthropic_catalog_flight_pending_for_tests(&url));
    let app = app(Arc::new(Registry::with_default_alias()));

    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(no_logins.catalog.count(), 1);
    assert!(!anthropic_catalog_flight_pending_for_tests(&url));
}

/// A flight whose fetch task was dropped before it ran fails the calls that
/// joined it with a logged `task` failure and leaves the flight map, so the
/// next call fetches again.
#[tokio::test]
async fn an_unstarted_catalog_flight_fails_once_and_leaves() {
    let no_logins = NoSavedLogins::install();
    let url = no_logins.catalog.catalog_url();
    insert_unstarted_anthropic_catalog_flight_for_tests(&url);
    assert!(anthropic_catalog_flight_pending_for_tests(&url));
    let app = app(Arc::new(Registry::with_default_alias()));

    let (status, value) = get_models(app.clone(), "/v1/models").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("public model catalog fetch did not complete"),
        "{message}"
    );
    assert_eq!(
        no_logins.logged("anthropic model catalog fetch failed"),
        [json!({"status": 0, "class": "task"})]
    );
    assert!(!anthropic_catalog_flight_pending_for_tests(&url));
    assert_eq!(no_logins.catalog.count(), 0);

    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(no_logins.catalog.count(), 1);
}

/// Without a credential the catalog is the only source of Claude rows, so a
/// catalog that cannot be read fails both routes. A failure is not cached.
#[tokio::test]
async fn models_fail_when_the_claude_catalog_is_unavailable() {
    let refused = format!("{CLOSED_LOOPBACK}{}", mock_http::CATALOG_PATH);
    let no_first_party = json!({
        "version": 8,
        "surfaces": {"cc": {"model_selector_config": [
            {"models": [{"id": "claude-elsewhere", "offered_on": ["vertex"]}]}
        ]}}
    })
    .to_string();
    // Class, the catalog's answer (none: the port is closed), the logged
    // status and the text the 502 message carries.
    type CatalogDown = (&'static str, Option<(u16, String)>, u64, &'static str);
    let cases: [CatalogDown; 4] = [
        ("network", None, 0, "catalog unreachable"),
        (
            "status",
            Some((500, r#"{"error":"down"}"#.to_string())),
            500,
            "answered 500",
        ),
        (
            "body",
            Some((200, "<html>".to_string())),
            200,
            "is not a catalog",
        ),
        ("empty", Some((200, no_first_party)), 200, "names no model"),
    ];
    for (class, reply, status, detail) in cases {
        let no_logins = match reply {
            Some((code, body)) => {
                NoSavedLogins::with_catalog(MockHttp::start(move |_| MockReply::json(code, &*body)))
            }
            None => NoSavedLogins::with_catalog_url(refused.clone(), MockHttp::catalog()),
        };
        let app = app(Arc::new(Registry::with_default_alias()));
        for uri in ["/v1/models", "/v1/models/claude-opus-5-5"] {
            let (code, value) = get_models(app.clone(), uri).await;
            assert_eq!(code, StatusCode::BAD_GATEWAY, "{class} {uri}: {value}");
            assert_eq!(value["error"]["type"], "api_error");
            let message = value["error"]["message"].as_str().unwrap();
            assert!(
                message.starts_with("anthropic model listing unavailable (unreachable): ")
                    && message.contains(detail),
                "{class}: {message}"
            );
        }
        if class != "network" {
            assert_eq!(no_logins.catalog.count(), 2, "{class}");
        }
        let warned = no_logins.logged("anthropic model catalog fetch failed");
        assert_eq!(warned.len(), 2, "{class}: {warned:?}");
        assert_eq!(warned[0]["class"], class);
        assert_eq!(warned[0]["status"], json!(status));
        assert!(!no_logins.log_text().contains("<html>"), "{class}");
    }
}

/// A request that carries an API key gets Anthropic's own listing for that
/// key: every page, each row as Anthropic wrote it. The relay keeps the key
/// byte for byte, adds `anthropic-version` only when the client sent none,
/// and passes the client's `lifecycle` pairs on as written.
#[tokio::test]
async fn an_api_key_lists_anthropic_rows_as_relayed() {
    let no_logins = NoSavedLogins::install();
    let upstream = relay_upstream();
    let app = anthropic_listing_app(&upstream.base);
    let (response, value) = get_models_response(
        app.clone(),
        "/v1/models?lifecycle[]=active&lifecycle[]=deprecated",
        &[
            ("x-api-key", TOY_API_KEY),
            ("content-type", "application/json"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK, "{value}");
    let [a, b, c, _] = relayed_rows();
    assert_eq!(value["data"], json!([a, b, c]));
    assert_eq!(value["first_id"], "claude-relayed-a");
    assert_eq!(value["last_id"], "claude-relayed-c");
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(no_logins.catalog.count(), 0);

    let requests = upstream.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    for (request, after_id) in requests.iter().zip([None, Some("claude-relayed-b")]) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path(), "/v1/models");
        assert_eq!(request.header_values("x-api-key"), [TOY_API_KEY]);
        assert!(request.header("authorization").is_none());
        assert!(request.header("content-type").is_none());
        assert_eq!(request.header_values("anthropic-version"), ["2023-06-01"]);
        assert_eq!(request.query("limit").as_deref(), Some("1000"));
        assert!(
            request
                .raw_query()
                .contains("lifecycle[]=active&lifecycle[]=deprecated"),
            "{}",
            request.target
        );
        assert_eq!(request.query("after_id").as_deref(), after_id);
    }

    let (response, _) = get_models_response(
        app,
        "/v1/models",
        &[
            ("x-api-key", TOY_API_KEY),
            ("anthropic-version", "2099-01-01"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request = &upstream.requests()[2];
    assert_eq!(request.header_values("anthropic-version"), ["2099-01-01"]);
    assert!(
        !request.raw_query().contains("lifecycle"),
        "{}",
        request.target
    );
}

#[tokio::test]
async fn a_bearer_token_and_beta_header_are_relayed_as_sent() {
    let no_logins = NoSavedLogins::install();
    let upstream = relay_upstream();
    let app = anthropic_listing_app(&upstream.base);
    let (response, value) = get_models_response(
        app,
        "/v1/models",
        &[
            ("authorization", "Bearer toy-oauth-token"),
            ("anthropic-beta", "oauth-2025-04-20,toy-beta"),
            ("anthropic-version", "2023-06-01"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert_eq!(
        row_ids(&value),
        ["claude-relayed-a", "claude-relayed-b", "claude-relayed-c"]
    );
    assert_eq!(no_logins.catalog.count(), 0);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.header_values("authorization"),
            ["Bearer toy-oauth-token"]
        );
        assert_eq!(
            request.header_values("anthropic-beta"),
            ["oauth-2025-04-20,toy-beta"]
        );
        assert_eq!(request.header_values("anthropic-version"), ["2023-06-01"]);
        assert!(request.header("x-api-key").is_none());
    }
}

/// A relay that is refused, too slow or answers something other than a
/// listing falls back to the catalog, with one warning that names neither
/// the credential nor anything of the answer.
#[tokio::test]
async fn a_failed_relay_serves_the_catalog_and_logs_no_credential() {
    // Only the slow relay runs on a shortened listing timeout, which also
    // bounds the wait for the catalog after it, so the timeout leaves the
    // catalog plenty and the relay answers long after it.
    let timeout = Duration::from_millis(1500);
    let cases: [(&str, u64, u16, &str, u64); 4] = [
        (
            "status",
            401,
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"secret-body-text"}}"#,
            0,
        ),
        ("body", 200, 200, "secret-body-text", 0),
        ("body", 200, 200, r#"{"rows":"secret-body-text"}"#, 0),
        ("timeout", 0, 200, r#"{"data":[]}"#, 15_000),
    ];
    for (class, status, code, body, delay) in cases {
        let no_logins = NoSavedLogins::install();
        let _timeout = (class == "timeout").then(|| no_logins.listing_timeout(timeout));
        let upstream = MockHttp::start(move |_| {
            MockReply::json(code, body).delayed(Duration::from_millis(delay))
        });
        let app = anthropic_listing_app(&upstream.base);
        let (response, value) = get_models_response(
            app,
            "/v1/models?lifecycle=active",
            &[("x-api-key", TOY_API_KEY)],
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "{class}: {value}");
        assert_eq!(row_ids(&value), CATALOG_IDS, "{class}");
        assert_eq!(upstream.count(), 1, "{class}");
        assert_eq!(no_logins.catalog.count(), 1, "{class}");
        let warned =
            no_logins.logged("anthropic model listing relay failed; using the public catalog");
        assert_eq!(warned.len(), 1, "{class}: {warned:?}");
        assert_eq!(warned[0]["class"], class);
        assert_eq!(warned[0]["status"], json!(status));
        let log = no_logins.log_text();
        for secret in [TOY_API_KEY, "secret-body-text", "lifecycle"] {
            assert!(!log.contains(secret), "{class}: {secret} in {log}");
        }
    }
}

/// Relayed rows keep their own lifecycle and are filtered by it; by default
/// a deprecated row is listed and a retired one is not. A cursor naming a row
/// the filter drops is still valid.
#[tokio::test]
async fn relayed_rows_are_filtered_by_their_own_lifecycle() {
    let _no_logins = NoSavedLogins::install();
    let upstream = relay_upstream();
    let app = anthropic_listing_app(&upstream.base);
    let cases: [(&str, &[&str], bool); 9] = [
        (
            "/v1/models",
            &["claude-relayed-a", "claude-relayed-b", "claude-relayed-c"],
            false,
        ),
        (
            "/v1/models?lifecycle=active",
            &["claude-relayed-a", "claude-relayed-c"],
            false,
        ),
        (
            "/v1/models?lifecycle=deprecated",
            &["claude-relayed-b"],
            false,
        ),
        ("/v1/models?lifecycle=retired", &["claude-relayed-r"], false),
        (
            "/v1/models?lifecycle=active&after_id=claude-relayed-b",
            &["claude-relayed-c"],
            false,
        ),
        (
            "/v1/models?lifecycle=active&before_id=claude-relayed-b",
            &["claude-relayed-a"],
            false,
        ),
        (
            "/v1/models?lifecycle=active&limit=1",
            &["claude-relayed-a"],
            true,
        ),
        (
            "/v1/models?lifecycle=active&after_id=claude-relayed-a&limit=1",
            &["claude-relayed-c"],
            false,
        ),
        (
            "/v1/models?lifecycle=active&before_id=claude-relayed-r&limit=1",
            &["claude-relayed-c"],
            true,
        ),
    ];
    for (uri, expected, has_more) in cases {
        let (response, value) =
            get_models_response(app.clone(), uri, &[("x-api-key", TOY_API_KEY)]).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        assert_eq!(value["has_more"], json!(has_more), "{uri}");
    }
    let (response, value) = get_models_response(
        app,
        "/v1/models?lifecycle=active&after_id=claude-relayed-zz",
        &[("x-api-key", TOY_API_KEY)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{value}");
}

/// Retrieve relays the credential the same way and applies no lifecycle
/// filter, so a retired relayed row is found.
#[tokio::test]
async fn retrieve_finds_a_relayed_row_with_a_credential() {
    let no_logins = NoSavedLogins::install();
    let upstream = relay_upstream();
    let app = anthropic_listing_app(&upstream.base);
    let (response, row) = get_models_response(
        app.clone(),
        "/v1/models/claude-relayed-r",
        &[("x-api-key", TOY_API_KEY)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{row}");
    assert_eq!(row, relayed_rows()[3]);
    for request in upstream.requests() {
        assert_eq!(request.header_values("x-api-key"), [TOY_API_KEY]);
        assert!(
            !request.raw_query().contains("lifecycle"),
            "{}",
            request.target
        );
    }
    // Without a credential only the catalog answers, and it has no such id.
    let (response, _) = get_models_response(app, "/v1/models/claude-relayed-r", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(no_logins.catalog.count(), 1);
}

/// Listings that arrive while a catalog fetch is failing wait for that fetch
/// and all get its failure. The failure is not kept: the next listing
/// fetches again.
#[tokio::test]
async fn concurrent_listings_share_one_failed_catalog_fetch() {
    // A failure is not cached, so a listing that reached the catalog only
    // after the fetch ended would fetch again. The fetch stays open long
    // enough for a loaded machine to start all five, and well inside the
    // listing timeout.
    let catalog = MockHttp::start(|_| {
        MockReply::json(503, r#"{"error":"down"}"#).delayed(Duration::from_millis(1500))
    });
    let no_logins = NoSavedLogins::with_catalog(catalog);
    let app = app(Arc::new(Registry::with_default_alias()));
    let answers =
        futures_util::future::join_all((0..5).map(|_| get_models(app.clone(), "/v1/models"))).await;
    for (status, value) in answers {
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("public model catalog answered 503"),
            "{message}"
        );
    }
    assert_eq!(no_logins.catalog.count(), 1);
    assert_eq!(
        no_logins.logged("anthropic model catalog fetch failed"),
        [json!({"status": 503, "class": "status"})]
    );

    let (status, value) = get_models(app, "/v1/models").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    assert_eq!(no_logins.catalog.count(), 2);
}

/// A listing that still says `has_more` after ten pages keeps those pages
/// and logs that it stopped.
#[tokio::test]
async fn a_relay_stops_at_the_page_cap_and_says_so() {
    let no_logins = NoSavedLogins::install();
    let upstream = MockHttp::start(|request| {
        let page = request.query("after_id").map_or(0, |id| {
            id.trim_start_matches("claude-page-")
                .parse::<usize>()
                .unwrap()
                + 1
        });
        let id = format!("claude-page-{page}");
        let body = json!({
            "data": [{"type": "model", "id": id}],
            "first_id": id,
            "has_more": true,
            "last_id": id,
        });
        MockReply::json(200, body.to_string())
    });
    let app = anthropic_listing_app(&upstream.base);
    let (response, value) =
        get_models_response(app, "/v1/models", &[("x-api-key", TOY_API_KEY)]).await;

    assert_eq!(response.status(), StatusCode::OK, "{value}");
    let expected: Vec<String> = (0..10).map(|page| format!("claude-page-{page}")).collect();
    assert_eq!(row_ids(&value), expected);
    assert_eq!(upstream.count(), 10);
    assert_eq!(no_logins.catalog.count(), 0);
    assert_eq!(
        no_logins.logged("anthropic model listing truncated"),
        [json!({"pages": 10})]
    );
    assert!(
        no_logins
            .logged("anthropic model listing relay failed; using the public catalog")
            .is_empty()
    );
}

/// A page that says `has_more` without a cursor that moves on (none at all,
/// or the one it was asked with) ends the listing with the rows read so far,
/// and logs that it stopped.
#[tokio::test]
async fn a_relay_page_without_a_usable_cursor_ends_the_listing_and_says_so() {
    let second_pages = [
        json!({"data": [{"type": "model"}], "has_more": true}),
        json!({"data": [], "has_more": true, "last_id": "claude-relayed-b"}),
    ];
    for second_page in second_pages {
        let no_logins = NoSavedLogins::install();
        let second = second_page.to_string();
        let upstream = MockHttp::start(move |request| match request.query("after_id") {
            None => {
                let [a, b, ..] = relayed_rows();
                let body = json!({"data": [a, b], "has_more": true, "last_id": "claude-relayed-b"});
                MockReply::json(200, body.to_string())
            }
            Some(_) => MockReply::json(200, second.clone()),
        });
        let app = anthropic_listing_app(&upstream.base);
        let (response, value) =
            get_models_response(app, "/v1/models", &[("x-api-key", TOY_API_KEY)]).await;

        assert_eq!(response.status(), StatusCode::OK, "{second_page}: {value}");
        assert_eq!(
            row_ids(&value),
            ["claude-relayed-a", "claude-relayed-b"],
            "{second_page}"
        );
        assert_eq!(upstream.count(), 2, "{second_page}");
        assert_eq!(no_logins.catalog.count(), 0, "{second_page}");
        assert_eq!(
            no_logins.logged("anthropic model listing truncated"),
            [json!({"pages": 2})],
            "{second_page}"
        );
    }
}

/// A lifecycle string the proxy does not know counts as `active`, as a
/// missing or non-string one does. The row keeps what Anthropic wrote.
#[tokio::test]
async fn an_unknown_lifecycle_counts_as_active() {
    let _no_logins = NoSavedLogins::install();
    let upstream = MockHttp::start(|_| {
        let [template, ..] = relayed_rows();
        let rows: Vec<Value> = [
            ("claude-preview", json!("preview")),
            ("claude-number", json!(3)),
            ("claude-null", Value::Null),
        ]
        .into_iter()
        .map(|(id, lifecycle)| {
            let mut row = template.clone();
            row["id"] = json!(id);
            row["lifecycle"] = lifecycle;
            row
        })
        .collect();
        MockReply::json(200, json!({"data": rows, "has_more": false}).to_string())
    });
    let app = anthropic_listing_app(&upstream.base);
    let all = ["claude-preview", "claude-number", "claude-null"];
    let cases: [(&str, &[&str]); 4] = [
        ("/v1/models", &all),
        ("/v1/models?lifecycle=active", &all),
        ("/v1/models?lifecycle=deprecated", &[]),
        ("/v1/models?lifecycle=retired", &[]),
    ];
    for (uri, expected) in cases {
        let (response, value) =
            get_models_response(app.clone(), uri, &[("x-api-key", TOY_API_KEY)]).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}: {value}");
        assert_eq!(row_ids(&value), expected, "{uri}");
        if !expected.is_empty() {
            assert_eq!(value["data"][0]["lifecycle"], "preview", "{uri}");
        }
    }
}

/// A relay page or a catalog longer than the body cap is a `body` failure,
/// even when it is JSON that would otherwise be read: the relay falls back
/// to the catalog, and a catalog that long fails the listing.
#[tokio::test]
async fn a_body_over_the_cap_is_a_body_failure() {
    let padding = "x".repeat(MAX_BODY_BYTES + 1);

    let no_logins = NoSavedLogins::install();
    let page =
        json!({"data": [relayed_rows()[0]], "has_more": false, "padding": padding}).to_string();
    let upstream = MockHttp::start(move |_| MockReply::json(200, page.clone()));
    let relay_app = anthropic_listing_app(&upstream.base);
    let (response, value) =
        get_models_response(relay_app, "/v1/models", &[("x-api-key", TOY_API_KEY)]).await;
    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(
        no_logins.logged("anthropic model listing relay failed; using the public catalog"),
        [json!({"status": 200, "class": "body"})]
    );
    drop(no_logins);

    let mut catalog: Value = serde_json::from_str(mock_http::CATALOG_FIXTURE).unwrap();
    catalog["padding"] = json!(padding);
    let catalog = catalog.to_string();
    let no_logins = NoSavedLogins::with_catalog(MockHttp::start(move |_| {
        MockReply::json(200, catalog.clone())
    }));
    let catalog_app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(catalog_app, "/v1/models").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("public model catalog body is not a catalog"),
        "{message}"
    );
    assert_eq!(
        no_logins.logged("anthropic model catalog fetch failed"),
        [json!({"status": 200, "class": "body"})]
    );
}

/// A relay whose connection is refused falls back to the catalog, as any
/// other relay failure does.
#[tokio::test]
async fn a_refused_relay_serves_the_catalog() {
    let no_logins = NoSavedLogins::install();
    let app = anthropic_listing_app(CLOSED_LOOPBACK);
    let (response, value) =
        get_models_response(app, "/v1/models", &[("x-api-key", TOY_API_KEY)]).await;

    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(no_logins.catalog.count(), 1);
    assert_eq!(
        no_logins.logged("anthropic model listing relay failed; using the public catalog"),
        [json!({"status": 0, "class": "network"})]
    );
    assert!(!no_logins.log_text().contains(TOY_API_KEY));
}

/// The body cap holds for a body that carries no `content-length`, chunked
/// or ended by the connection closing, on the relay and on the catalog
/// alike.
#[tokio::test]
async fn a_body_over_the_cap_without_a_length_is_a_body_failure() {
    let padding = "x".repeat(MAX_BODY_BYTES + 1);
    let page =
        json!({"data": [relayed_rows()[0]], "has_more": false, "padding": padding}).to_string();
    let mut catalog: Value = serde_json::from_str(mock_http::CATALOG_FIXTURE).unwrap();
    catalog["padding"] = json!(padding);
    let catalog = catalog.to_string();

    for framing in [Framing::Chunked, Framing::Close] {
        let no_logins = NoSavedLogins::install();
        let relayed = page.clone();
        let upstream =
            MockHttp::start(move |_| MockReply::json(200, relayed.clone()).framed(framing));
        let (response, value) = get_models_response(
            anthropic_listing_app(&upstream.base),
            "/v1/models",
            &[("x-api-key", TOY_API_KEY)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{framing:?}: {value}");
        assert_eq!(row_ids(&value), CATALOG_IDS, "{framing:?}");
        assert_eq!(
            no_logins.logged("anthropic model listing relay failed; using the public catalog"),
            [json!({"status": 200, "class": "body"})],
            "{framing:?}"
        );
        drop(no_logins);

        let served = catalog.clone();
        let no_logins = NoSavedLogins::with_catalog(MockHttp::start(move |_| {
            MockReply::json(200, served.clone()).framed(framing)
        }));
        let catalog_app = app(Arc::new(Registry::with_default_alias()));
        let (status, value) = get_models(catalog_app, "/v1/models").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{framing:?}: {value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("public model catalog body is not a catalog"),
            "{framing:?}: {message}"
        );
        assert_eq!(
            no_logins.logged("anthropic model catalog fetch failed"),
            [json!({"status": 200, "class": "body"})],
            "{framing:?}"
        );
    }
}

/// A catalog of exactly the cap is read, whatever its framing.
#[tokio::test]
async fn a_catalog_of_exactly_the_cap_is_read() {
    let mut catalog: Value = serde_json::from_str(mock_http::CATALOG_FIXTURE).unwrap();
    catalog["padding"] = json!("");
    let unpadded = catalog.to_string().len();
    catalog["padding"] = json!("x".repeat(MAX_BODY_BYTES - unpadded));
    let catalog = catalog.to_string();
    assert_eq!(catalog.len(), MAX_BODY_BYTES);

    for framing in [Framing::Length, Framing::Chunked, Framing::Close] {
        let served = catalog.clone();
        let no_logins = NoSavedLogins::with_catalog(MockHttp::start(move |_| {
            MockReply::json(200, served.clone()).framed(framing)
        }));
        let catalog_app = app(Arc::new(Registry::with_default_alias()));
        let (status, value) = get_models(catalog_app, "/v1/models").await;
        assert_eq!(status, StatusCode::OK, "{framing:?}: {value}");
        assert_eq!(row_ids(&value), CATALOG_IDS, "{framing:?}");
        assert!(
            no_logins
                .logged("anthropic model catalog fetch failed")
                .is_empty(),
            "{framing:?}"
        );
    }
}

/// The catalog fetch follows a redirect. The relay does not: a relay that
/// answers with one is a failed relay, and the catalog answers instead.
#[tokio::test]
async fn the_catalog_follows_a_redirect_and_the_relay_does_not() {
    const MOVED: &str = "/moved/catalog.json";
    let catalog = MockHttp::start(|request| {
        if request.path() == mock_http::CATALOG_PATH {
            MockReply::redirect(MOVED)
        } else {
            MockReply::json(200, mock_http::CATALOG_FIXTURE)
        }
    });
    let no_logins = NoSavedLogins::with_catalog(catalog);
    let catalog_app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(catalog_app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    let paths: Vec<String> = no_logins
        .catalog
        .requests()
        .iter()
        .map(|request| request.path().to_string())
        .collect();
    assert_eq!(paths, [mock_http::CATALOG_PATH, MOVED]);
    drop(no_logins);

    let no_logins = NoSavedLogins::install();
    let upstream = MockHttp::start(|_| MockReply::redirect("/v1/models/elsewhere"));
    let (response, value) = get_models_response(
        anthropic_listing_app(&upstream.base),
        "/v1/models",
        &[("x-api-key", TOY_API_KEY)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(upstream.count(), 1);
    assert_eq!(no_logins.catalog.count(), 1);
    assert_eq!(
        no_logins.logged("anthropic model listing relay failed; using the public catalog"),
        [json!({"status": 302, "class": "status"})]
    );
}

/// A later page that fails fails the whole relay: the rows of the pages
/// before it are dropped and the catalog answers instead.
#[tokio::test]
async fn a_failing_later_page_drops_the_relayed_rows() {
    let no_logins = NoSavedLogins::install();
    let upstream = MockHttp::start(|request| match request.query("after_id") {
        None => {
            let [a, b, ..] = relayed_rows();
            let body = json!({"data": [a, b], "has_more": true, "last_id": "claude-relayed-b"});
            MockReply::json(200, body.to_string())
        }
        Some(_) => MockReply::json(500, r#"{"type":"error"}"#),
    });
    let app = anthropic_listing_app(&upstream.base);
    let (response, value) =
        get_models_response(app, "/v1/models", &[("x-api-key", TOY_API_KEY)]).await;

    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);
    assert_eq!(upstream.count(), 2);
    assert_eq!(no_logins.catalog.count(), 1);
    assert_eq!(
        no_logins.logged("anthropic model listing relay failed; using the public catalog"),
        [json!({"status": 500, "class": "status"})]
    );
}

/// The default registry with its Anthropic passthrough sending to `base`.
fn default_registry_with_anthropic_at(base: &str) -> Arc<Registry> {
    let _base_url = lock_anthropic_base_url();
    let previous = std::env::var_os("CCP_ANTHROPIC_BASE_URL");
    unsafe {
        std::env::set_var("CCP_ANTHROPIC_BASE_URL", base);
    }
    let registry = Arc::new(Registry::with_default_alias());
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CCP_ANTHROPIC_BASE_URL", value),
            None => std::env::remove_var("CCP_ANTHROPIC_BASE_URL"),
        }
    }
    registry
}

/// Neither a relayed nor a catalog listing feeds routing: Claude ids and
/// aliases stay on the alias provider, Codex ids on Codex, and an id only
/// the Anthropic listing names stays unroutable.
#[tokio::test]
async fn an_anthropic_listing_leaves_routing_as_it_was() {
    let _no_logins = NoSavedLogins::install();
    clear_listed_models_for_tests();
    let upstream = MockHttp::start(|_| {
        MockReply::json(
            200,
            json!({
                "data": [
                    {"type": "model", "id": "claude-relayed-a"},
                    {"type": "model", "id": "zz-relayed-only"},
                    {"type": "model", "id": "gpt-6-sol"}
                ],
                "has_more": false
            })
            .to_string(),
        )
    });
    let registry = default_registry_with_anthropic_at(&upstream.base);
    let ids = [
        "claude-opus-5-5",
        "claude-relayed-a",
        "opus",
        "sonnet",
        "haiku",
        "gpt-6-sol",
        "zz-relayed-only",
    ];
    let routed = |registry: &Registry| {
        ids.map(|id| {
            registry
                .provider_for_model(id, None)
                .map(|provider| provider.name())
        })
    };
    let before = routed(&registry);
    assert_eq!(before[0], Some("anthropic"));
    assert_eq!(before[5], Some("codex"));
    assert_eq!(before[6], None);

    let (response, value) = get_models_response(
        app(registry.clone()),
        "/v1/models",
        &[("x-api-key", TOY_API_KEY)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{value}");
    assert!(row_ids(&value).contains(&"zz-relayed-only"));
    let (status, value) = get_models(app(registry.clone()), "/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(row_ids(&value), CATALOG_IDS);

    assert_eq!(routed(&registry), before);
    clear_listed_models_for_tests();
}

/// Pin the progress-label mode for the duration of a test: set or clear
/// `CCP_AGENT_SUMMARY`, clear `CCP_AGENT_SUMMARY_MODEL` and
/// `CCP_AUTO_REVIEW_MODEL`, and point `CCP_CONFIG_DIR` at an empty directory,
/// so neither the environment nor a `config.json` on the machine picks the
/// mode, the label model or the auto-review model. A test may set the cleared
/// keys after installing; they are restored on drop. The guard holds
/// [`SHARED_STATE`], so it runs neither beside another test that pins it nor
/// beside one that installs [`NoSavedLogins`], which sets `CCP_CONFIG_DIR`
/// too.
struct PinnedAgentSummary {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _config_dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PinnedAgentSummary {
    fn install(mode: Option<&str>) -> Self {
        let lock = lock_shared_state();
        let config_dir = tempfile::TempDir::new().unwrap();
        let previous = [
            "CCP_AGENT_SUMMARY",
            "CCP_AGENT_SUMMARY_MODEL",
            "CCP_AUTO_REVIEW_MODEL",
            "CCP_CONFIG_DIR",
        ]
        .map(|key| (key, std::env::var_os(key)))
        .to_vec();
        unsafe {
            match mode {
                Some(mode) => std::env::set_var("CCP_AGENT_SUMMARY", mode),
                None => std::env::remove_var("CCP_AGENT_SUMMARY"),
            }
            std::env::remove_var("CCP_AGENT_SUMMARY_MODEL");
            std::env::remove_var("CCP_AUTO_REVIEW_MODEL");
            std::env::set_var("CCP_CONFIG_DIR", config_dir.path());
        }
        Self {
            previous,
            _config_dir: config_dir,
            _lock: lock,
        }
    }
}

impl Drop for PinnedAgentSummary {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// A subagent turn that ends with Claude Code's progress-label prompt, after
/// the result of its last tool call. The subagent's tools come along, as they
/// do on the wire; the prose is the only restriction the client states.
fn progress_label_body() -> Value {
    json!({
        "model": "gpt-5.5",
        "max_tokens": 64000,
        "stream": false,
        "tools": [
            {"name": "Read", "description": "Read a file",
             "input_schema": {"type": "object",
                              "properties": {"file_path": {"type": "string"}}}},
            {"name": "Bash", "description": "Run a command",
             "input_schema": {"type": "object",
                              "properties": {"command": {"type": "string"}}}}
        ],
        "messages": [
            {"role": "user", "content": "do the work"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read",
                 "input": {"file_path": "/repo/src/server.rs"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "..."},
                {"type": "text", "text": "Describe your most recent action in 3-5 words using present tense (-ing)."}
            ]}
        ]
    })
}

/// Post a label request to a registry holding only an
/// `IdentityCaptureProvider`, and return the response with what that provider
/// received. `request_class` is sent as `x-claude-code-request-class` when
/// given; older clients send none.
async fn post_progress_label(
    body: &Value,
    request_class: Option<&str>,
    monitor: Option<MonitorHandle>,
) -> (axum::response::Response, Arc<Mutex<Vec<MessagesRequest>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: Default::default(),
        bodies: bodies.clone(),
    }) as Arc<dyn Provider>;
    let app = app_with_monitor(
        Arc::new(Registry::from_providers(AliasProvider::Codex, [provider])),
        monitor,
    );
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "label-session");
    if let Some(class) = request_class {
        request = request.header("x-claude-code-request-class", class);
    }
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    (response, bodies)
}

#[tokio::test]
async fn agent_progress_label_is_answered_without_a_provider() {
    let _mode = PinnedAgentSummary::install(Some("local"));
    let (response, bodies) = post_progress_label(&progress_label_body(), None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    // A local answer is still a Messages response and carries its id.
    assert!(!request_id(&response).is_empty());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], json!("Reading server.rs"));
    assert_eq!(value["usage"]["input_tokens"], json!(0));
    // In the local mode the label never reaches a backend.
    assert!(bodies.lock().unwrap().is_empty());
}

/// Claude Code may follow the label prompt with a `role: "system"` message that
/// carries reminders. It is still a label request.
#[tokio::test]
async fn agent_progress_label_followed_by_a_system_message_is_answered_locally() {
    let _mode = PinnedAgentSummary::install(Some("local"));
    let mut body = progress_label_body();
    body["messages"].as_array_mut().unwrap().push(json!({
        "role": "system",
        "content": [{"type": "text", "text": "Reminder: the task list changed."}]
    }));
    let (response, bodies) = post_progress_label(&body, None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], json!("Reading server.rs"));
    assert!(bodies.lock().unwrap().is_empty());
}

/// With no mode set, a label request goes to the subagent's own provider once,
/// with the model, effort and tools the client asked for, and the monitor
/// names that provider rather than a local answer. On the codex route the one
/// rewrite is the tool choice: a label must come back as text, so `none`
/// replaces whatever the client sent, tools kept.
#[tokio::test]
async fn agent_progress_label_is_forwarded_with_tool_calls_forbidden_by_default() {
    let _mode = PinnedAgentSummary::install(None);
    let monitor = MonitorHandle::new(10);
    let mut body = progress_label_body();
    body["output_config"] = json!({"effort": "high"});
    body["tool_choice"] = json!({"type": "auto"});
    let (response, bodies) = post_progress_label(&body, None, Some(monitor.clone())).await;

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"captured");
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        bodies[0].extra.get("output_config"),
        Some(&json!({"effort": "high"}))
    );
    assert_eq!(bodies[0].extra.get("tools"), Some(&body["tools"]));
    let mut expected = body.clone();
    expected["tool_choice"] = json!({"type": "none"});
    assert_eq!(serde_json::to_value(&bodies[0]).unwrap(), expected);
    let state = monitor.snapshot();
    assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
}

/// In the `upstream` mode the label goes to the provider's junior model at the
/// lowest effort, and the provider is told to keep that model, so a model
/// override configured for the provider (`CCP_CODEX_MODEL`) cannot replace it.
/// The junior model is forbidden tool calls like the subagent's own would be.
#[tokio::test]
async fn agent_progress_label_upstream_route_keeps_the_junior_model() {
    let _mode = PinnedAgentSummary::install(Some("upstream"));
    let body = progress_label_body();
    let (response, bodies) = post_progress_label(&body, None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(
        bodies[0].extra.get("output_config"),
        Some(&json!({"effort": "low"}))
    );
    assert!(bodies[0].bypass_provider_model_override);
    assert_eq!(
        bodies[0].extra.get("tool_choice"),
        Some(&json!({"type": "none"}))
    );
    assert_eq!(bodies[0].extra.get("tools"), Some(&body["tools"]));
}

/// Claude Code classes every side request `auxiliary`, the label included,
/// and older clients send no class at all. Both are labels.
#[tokio::test]
async fn agent_progress_label_classed_auxiliary_or_unclassed_is_a_label() {
    let _mode = PinnedAgentSummary::install(None);
    for class in [Some("auxiliary"), None] {
        let mut body = progress_label_body();
        body["tool_choice"] = json!({"type": "auto"});
        let (response, bodies) = post_progress_label(&body, class, None).await;

        assert_eq!(response.status(), StatusCode::OK, "class {class:?}");
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "class {class:?}");
        assert_eq!(
            bodies[0].extra.get("tool_choice"),
            Some(&json!({"type": "none"})),
            "class {class:?}"
        );
    }
}

/// A request Claude Code classes as anything else is not a label, however its
/// prompt reads: it keeps the tool choice it named on the codex route, and in
/// the local mode it is not answered from the transcript.
#[tokio::test]
async fn a_label_prompt_under_another_request_class_is_not_a_label() {
    {
        let _mode = PinnedAgentSummary::install(None);
        for class in ["subagent", "main", "compaction", "Auxiliary"] {
            let mut body = progress_label_body();
            body["tool_choice"] = json!({"type": "auto"});
            let (response, bodies) = post_progress_label(&body, Some(class), None).await;

            assert_eq!(response.status(), StatusCode::OK, "class {class}");
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1, "class {class}");
            assert_eq!(
                bodies[0].extra.get("tool_choice"),
                Some(&json!({"type": "auto"})),
                "class {class}"
            );
        }
    }

    let _mode = PinnedAgentSummary::install(Some("local"));
    for class in ["subagent", "main"] {
        let (response, bodies) =
            post_progress_label(&progress_label_body(), Some(class), None).await;

        assert_eq!(response.status(), StatusCode::OK, "class {class}");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"captured", "class {class}");
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "class {class}");
        assert_eq!(bodies[0].extra.get("tool_choice"), None, "class {class}");
    }
}

/// The class alone is not the detector: an `auxiliary` request whose prompt
/// asks for something else, a title here, keeps the tool choice it named.
#[tokio::test]
async fn an_auxiliary_request_that_is_not_a_label_keeps_its_tool_choice() {
    let _mode = PinnedAgentSummary::install(None);
    let mut body = progress_label_body();
    body["messages"].as_array_mut().unwrap().pop();
    body["messages"].as_array_mut().unwrap().push(json!({
        "role": "user",
        "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "..."},
            {"type": "text", "text": "Write a short title for this conversation."}
        ]
    }));
    body["tool_choice"] = json!({"type": "auto"});
    let (response, bodies) = post_progress_label(&body, Some("auxiliary"), None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0].extra.get("tool_choice"),
        Some(&json!({"type": "auto"}))
    );
    assert_eq!(bodies[0].extra.get("tools"), Some(&body["tools"]));
}

/// Send a classifier-shaped request through the proxy and report what the
/// backend saw and how the monitor recorded it. The local mode is pinned, so
/// the classifier guard is what keeps it off the local answer.
async fn classifier_route(user_text: &str) -> (StatusCode, String, usize, Option<String>) {
    let _mode = PinnedAgentSummary::install(Some("local"));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: captured.clone(),
        bodies: Default::default(),
    }) as Arc<dyn Provider>;
    let monitor = MonitorHandle::new(10);
    let app = app_with_monitor(
        Arc::new(Registry::from_providers(AliasProvider::Codex, [provider])),
        Some(monitor.clone()),
    );
    let body = json!({
        "model": "gpt-5.5",
        "max_tokens": 512,
        "stream": false,
        "system": [{"type": "text", "text":
            "You are a security monitor for autonomous AI coding agents.\n\n## Context"}],
        "tools": [],
        "messages": [{"role": "user", "content": [{"type": "text", "text": user_text}]}]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "classifier-session")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let calls = captured.lock().unwrap().len();
    let provider_name = monitor.snapshot().recent[0].provider.clone();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        calls,
        provider_name,
    )
}

/// Claude Code's auto-mode permission classifier quotes the agent's transcript
/// into the message it asks a verdict on, so the progress-label prompt turns up
/// verbatim inside it. Answering that with a three-word label leaves the
/// classifier with nothing to parse: it retries ten times a second and then
/// reports that it could not evaluate the action. It must reach a model.
#[tokio::test]
async fn the_permission_classifier_is_never_answered_as_a_progress_label() {
    let (status, bytes, calls, provider) = classifier_route(
        "Evaluate the following transcript:\n\
         user: Describe your most recent action in 3-5 words using present tense (-ing).\n\
         assistant: Reading server.rs\n\
         Answer with a verdict.",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, "captured");
    assert_eq!(calls, 1);
    // Routed to a backend, not answered locally, which the monitor spells
    // "local".
    assert_eq!(provider.as_deref(), Some("codex"));
}

/// The same guard, with the marker where the tightened prompt match would still
/// accept it. The classifier's shape alone is enough to keep it off the local
/// answer, so neither half of the fix carries this on its own.
#[tokio::test]
async fn the_permission_classifier_is_routed_even_when_it_opens_with_the_marker() {
    let (status, bytes, calls, provider) = classifier_route(
        "Describe your most recent action in 3-5 words is the prompt under review; \
         decide whether answering it is safe.",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, "captured");
    assert_eq!(calls, 1);
    assert_eq!(provider.as_deref(), Some("codex"));
}

#[tokio::test]
async fn models_endpoint_ignores_unknown_query_params() {
    let (_, plain) = get_models(five_model_app(), "/v1/models").await;
    let (status, value) = get_models(
        five_model_app(),
        "/v1/models?beta=true&foo%5B%5D=bar&page=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(
        sorted_keys(&value),
        ["data", "first_id", "has_more", "last_id"]
    );
    assert_eq!(value, plain);
}

/// The `request-id` header a response carries, or "" when it has none.
fn request_id(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get("request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

async fn post_json(app: axum::Router, uri: &str, body: Body) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .unwrap(),
    )
    .await
    .unwrap()
}

fn capture_app(monitor: Option<MonitorHandle>) -> axum::Router {
    let provider = Arc::new(IdentityCaptureProvider {
        captured: Arc::new(Mutex::new(Vec::new())),
        bodies: Default::default(),
    }) as Arc<dyn Provider>;
    app_with_monitor(
        Arc::new(Registry::from_providers(AliasProvider::Codex, [provider])),
        monitor,
    )
}

fn messages_body(model: &str) -> Body {
    Body::from(
        json!({
            "model": model,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string(),
    )
}

/// Claude Code records this header as the transcript's `requestId`, and
/// transcript readers de-duplicate on it. Each response names the request the
/// proxy logged it under, and two requests never share one.
#[tokio::test]
async fn successful_messages_responses_carry_their_own_request_id() {
    let monitor = MonitorHandle::new(10);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = post_json(
            capture_app(Some(monitor.clone())),
            "/v1/messages",
            messages_body("gpt-5.5"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let id = request_id(&response).to_string();
        assert!(!id.is_empty());
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    let logged: Vec<String> = monitor
        .snapshot()
        .recent
        .iter()
        .map(|request| request.request_id.clone())
        .collect();
    for id in &ids {
        assert!(logged.contains(id), "{id} not in {logged:?}");
    }
}

/// Every Anthropic route goes through the same dispatch, so the token counter
/// and the bare `/messages` aliases carry the header as well.
#[tokio::test]
async fn every_messages_route_carries_a_request_id() {
    for uri in [
        "/v1/messages/count_tokens",
        "/messages",
        "/messages/count_tokens",
    ] {
        let response = post_json(capture_app(None), uri, messages_body("gpt-5.5")).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert!(!request_id(&response).is_empty(), "{uri}");
    }
}

/// Requests refused before any provider is involved still get an id.
#[tokio::test]
async fn rejected_messages_requests_carry_a_request_id() {
    let _no_logins = NoSavedLogins::install();
    let cases = [
        (
            "malformed json",
            Body::from("{not json"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "oversized body",
            padded_messages_body(MAX_ANTHROPIC_REQUEST_BYTES + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "missing model",
            body_string(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown model",
            messages_body("not-a-model"),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (case, body, status) in cases {
        let response = post_json(capture_app(None), "/v1/messages", body).await;
        assert_eq!(response.status(), status, "{case}");
        assert!(!request_id(&response).is_empty(), "{case}");
    }
}

#[tokio::test]
async fn provider_failure_response_carries_a_request_id() {
    let response = post_json(
        app(routed_registry()),
        "/v1/messages",
        messages_body("kimi-k2.6"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert!(!request_id(&response).is_empty());
}

/// Answers with an event stream that sends one event and then stays open.
struct OpenStreamProvider;

#[async_trait]
impl Provider for OpenStreamProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["gpt-5.5".to_string()]
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &FAKE_CLI
    }

    async fn handle_messages(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        let first = futures_util::stream::once(async {
            Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            ))
        });
        let open = futures_util::StreamExt::chain(first, futures_util::stream::pending());
        axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(open))
            .unwrap()
    }

    async fn handle_count_tokens(
        &self,
        _body: MessagesRequest,
        _ctx: RequestContext,
    ) -> axum::response::Response {
        (StatusCode::NOT_IMPLEMENTED, "unused").into_response()
    }
}

/// A streamed response carries the id in its head, which the client reads
/// while the body is still open.
#[tokio::test]
async fn a_streaming_response_carries_the_request_id_before_the_body_ends() {
    use http_body_util::BodyExt;

    let provider = Arc::new(OpenStreamProvider) as Arc<dyn Provider>;
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Codex,
        [provider],
    )));
    let body = Body::from(
        json!({
            "model": "gpt-5.5",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string(),
    );
    let within = std::time::Duration::from_secs(5);
    let response = tokio::time::timeout(within, post_json(app, "/v1/messages", body))
        .await
        .expect("the head arrives while the stream is open");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!request_id(&response).is_empty());

    let mut body = response.into_body();
    let frame = tokio::time::timeout(within, body.frame())
        .await
        .expect("the first event arrives")
        .expect("the stream has a first frame")
        .unwrap();
    let data = frame.into_data().unwrap();
    assert!(data.starts_with(b"event: message_start"));
}

/// Anthropic sends its own `request-id`, and that is the id Claude Code must
/// record for a passthrough request, not one the proxy made up.
#[tokio::test]
async fn an_upstream_request_id_is_kept_on_the_anthropic_route() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = axum::Router::new().fallback(|| async {
        (
            [
                ("request-id", "req_upstream"),
                ("content-type", "application/json"),
            ],
            r#"{"type":"message","content":[]}"#,
        )
    });
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let response = post_json(
        anthropic_app(address),
        "/v1/messages",
        messages_body("claude-opus-5"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(request_id(&response), "req_upstream");
}

/// The Anthropic passthrough, sending to `address`.
fn anthropic_provider(address: std::net::SocketAddr) -> Arc<dyn Provider> {
    anthropic_provider_at(&format!("http://{address}"))
}

/// Held while a test points `CCP_ANTHROPIC_BASE_URL` at its own mock to
/// build a provider, so two tests building at once cannot swap mocks or
/// leave the other's URL behind. It is separate from [`SHARED_STATE`]
/// because tests holding that build providers too, and it is only ever
/// taken after it, never before.
static ANTHROPIC_BASE_URL: Mutex<()> = Mutex::new(());

fn lock_anthropic_base_url() -> MutexGuard<'static, ()> {
    ANTHROPIC_BASE_URL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The Anthropic passthrough, sending to `base`.
fn anthropic_provider_at(base: &str) -> Arc<dyn Provider> {
    // The provider reads its base URL once, when it is built.
    let _base_url = lock_anthropic_base_url();
    let previous = std::env::var_os("CCP_ANTHROPIC_BASE_URL");
    unsafe {
        std::env::set_var("CCP_ANTHROPIC_BASE_URL", base);
    }
    let provider = Arc::new(claude_code_mux::providers::anthropic::AnthropicProvider::new())
        as Arc<dyn Provider>;
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CCP_ANTHROPIC_BASE_URL", value),
            None => std::env::remove_var("CCP_ANTHROPIC_BASE_URL"),
        }
    }
    provider
}

/// An app whose only provider is the Anthropic passthrough, sending to
/// `address`.
fn anthropic_app(address: std::net::SocketAddr) -> axum::Router {
    app(Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        [anthropic_provider(address)],
    )))
}

/// A mock Anthropic upstream that keeps the body of the last request it got,
/// byte for byte, and answers with an empty message.
async fn recording_upstream() -> (std::net::SocketAddr, Arc<Mutex<Option<axum::body::Bytes>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Option<axum::body::Bytes>>> = Default::default();
    let upstream = axum::Router::new().fallback({
        let seen = seen.clone();
        move |body: axum::body::Bytes| {
            let seen = seen.clone();
            async move {
                *seen.lock().unwrap() = Some(body);
                (
                    [("content-type", "application/json")],
                    r#"{"type":"message","content":[]}"#,
                )
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    (address, seen)
}

/// Send `sent` to an app holding the Anthropic passthrough, pointed at a
/// recording upstream, and a Codex provider that serves `gpt-6-luna`. Returns
/// the status, the bytes Anthropic received and the models Codex was asked
/// for.
async fn send_to_anthropic_or_codex(
    sent: String,
) -> (StatusCode, Option<axum::body::Bytes>, Vec<Option<String>>) {
    let (address, seen) = recording_upstream().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let codex = Arc::new(IdentityCaptureProvider {
        captured: Default::default(),
        bodies: bodies.clone(),
    }) as Arc<dyn Provider>;
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        [anthropic_provider(address), codex],
    )));
    let status = post_json(app, "/v1/messages", Body::from(sent))
        .await
        .status();
    let models = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| body.model.clone())
        .collect();
    (status, seen.lock().unwrap().clone(), models)
}

/// The error message of a JSON error response.
async fn error_message(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["error"]["message"].as_str().unwrap().to_string()
}

/// The start of the message a refused threaded request carries. Claude Code
/// matches the token up to a character outside `[A-Za-z0-9_:.-]`, hence the
/// trailing space.
const THREAD_REJECTED: &str = "capability_rejected: beta_header:message-threads-2026-08-12 ";

/// A message threads request whose new messages answer a tool call made
/// earlier in the thread.
fn thread_body(model: &str, thread_type: &str) -> Body {
    Body::from(
        json!({
            "model": model,
            "max_tokens": 16,
            "thread": {"type": thread_type, "previous_message_id": "msg_01"},
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
            ]}]
        })
        .to_string(),
    )
}

/// A continuation holds only what came after the message it names, and a
/// creation served off Anthropic would anchor the thread on an id Anthropic
/// never issued. Either is refused before it reaches the provider, in the
/// shape that makes the client drop threading and resend the full history,
/// and the monitor row still names the route.
#[tokio::test]
async fn a_threaded_request_is_refused_on_a_translated_route() {
    for thread_type in ["continue", "create"] {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(IdentityCaptureProvider {
            captured: captured.clone(),
            bodies: Default::default(),
        }) as Arc<dyn Provider>;
        let monitor = MonitorHandle::new(10);
        let app = app_with_monitor(
            Arc::new(Registry::from_providers(AliasProvider::Codex, [provider])),
            Some(monitor.clone()),
        );

        let response = post_json(app, "/v1/messages", thread_body("gpt-5.5", thread_type)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{thread_type}");
        assert!(!request_id(&response).is_empty(), "{thread_type}");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "error", "{thread_type}");
        assert_eq!(
            body["error"]["type"], "invalid_request_error",
            "{thread_type}"
        );
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with(THREAD_REJECTED),
            "{thread_type}: {message}"
        );
        assert!(captured.lock().unwrap().is_empty(), "{thread_type}");

        let state = monitor.snapshot();
        assert!(state.active.is_empty(), "{thread_type}");
        let request = &state.recent[0];
        assert_eq!(request.status, RequestStatus::Failed, "{thread_type}");
        assert_eq!(request.http_status, Some(400), "{thread_type}");
        assert_eq!(request.error.as_deref(), Some(message), "{thread_type}");
        assert_eq!(request.provider.as_deref(), Some("codex"), "{thread_type}");
        assert_eq!(request.model.as_deref(), Some("gpt-5.5"), "{thread_type}");
    }
}

/// The guard keys on the route, not on Codex: every translator would lose the
/// thread alike. A request that reached these providers would get their 501.
#[tokio::test]
async fn a_threaded_request_is_refused_on_every_translated_provider() {
    for model in ["kimi-k2.6", "grok-4.5", "cursor:gpt-5.5"] {
        let response = post_json(
            app(routed_registry()),
            "/v1/messages",
            thread_body(model, "continue"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{model}");
        let message = error_message(response).await;
        assert!(message.starts_with(THREAD_REJECTED), "{model}: {message}");
    }
}

/// Through the real Grok provider. Its translator refuses a top-level field it
/// does not know with a 400 of its own, which carries no capability token and
/// would leave the client threading; the guard answers before it runs.
#[tokio::test]
async fn a_thread_creation_is_refused_before_the_grok_translator() {
    let response = post_json(
        app(Arc::new(Registry::with_default_alias())),
        "/v1/messages",
        thread_body("grok-4.5", "create"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let message = error_message(response).await;
    assert!(message.starts_with(THREAD_REJECTED), "{message}");
}

/// Anthropic holds the thread, so a continuation goes there as the client
/// sent it, byte for byte.
#[tokio::test]
async fn a_thread_continuation_is_relayed_untouched_on_the_anthropic_route() {
    let (address, seen) = recording_upstream().await;

    // Spacing and key order no serializer would produce, so a rewrite shows.
    let sent = r#"{ "thread" : {"type":"continue","previous_message_id":"msg_01"},
  "model":"claude-opus-5", "max_tokens":16,
  "messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"done"}]}] }"#;
    let response = post_json(anthropic_app(address), "/v1/messages", Body::from(sent)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(seen.lock().unwrap().as_deref(), Some(sent.as_bytes()));
}

/// A classifier-shaped request for a Claude model, which a configured
/// `CCP_AUTO_REVIEW_MODEL` reroutes. Spacing no serializer would produce, so a
/// rewrite shows.
const CLAUDE_CLASSIFIER: &str = r#"{ "model":"claude-opus-5", "max_tokens":512, "stream":false, "tools":[],
  "system":[{"type":"text","text":"You are a security monitor for autonomous AI coding agents.\n\n## Context"}],
  "messages":[{"role":"user","content":[{"type":"text","text":"Is this action safe?"}]}] }"#;

/// The same classifier request, continuing a message thread.
const THREADED_CLAUDE_CLASSIFIER: &str = r#"{ "thread" : {"type":"continue","previous_message_id":"msg_01"},
  "model":"claude-opus-5", "max_tokens":512, "stream":false, "tools":[],
  "system":[{"type":"text","text":"You are a security monitor for autonomous AI coding agents.\n\n## Context"}],
  "messages":[{"role":"user","content":[{"type":"text","text":"Is this action safe?"}]}] }"#;

/// The auto-review route would take a threaded request off Anthropic, where it
/// would be refused and the client would drop threading for the whole
/// session. It stays on Anthropic as the client sent it.
#[tokio::test]
async fn a_threaded_classifier_request_is_not_rerouted_off_anthropic() {
    let _pinned = PinnedAgentSummary::install(None);
    unsafe { std::env::set_var("CCP_AUTO_REVIEW_MODEL", "gpt-6-luna") };
    let (status, seen, codex_models) =
        send_to_anthropic_or_codex(THREADED_CLAUDE_CLASSIFIER.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen.as_deref(), Some(THREADED_CLAUDE_CLASSIFIER.as_bytes()));
    assert!(codex_models.is_empty());
}

/// Without a thread the auto-review route applies as before.
#[tokio::test]
async fn an_unthreaded_classifier_request_is_still_rerouted() {
    let _pinned = PinnedAgentSummary::install(None);
    unsafe { std::env::set_var("CCP_AUTO_REVIEW_MODEL", "gpt-6-luna") };
    let (status, seen, codex_models) =
        send_to_anthropic_or_codex(CLAUDE_CLASSIFIER.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(seen.is_none());
    assert_eq!(codex_models, [Some("gpt-6-luna".to_string())]);
}

/// The `upstream` label route keeps a threaded label on Anthropic too, with the
/// label model pointed at Codex.
#[tokio::test]
async fn a_threaded_label_is_not_rerouted_off_anthropic() {
    let _pinned = PinnedAgentSummary::install(Some("upstream"));
    unsafe { std::env::set_var("CCP_AGENT_SUMMARY_MODEL", "gpt-6-luna") };
    let mut body = progress_label_body();
    body["model"] = json!("claude-opus-5");
    body["thread"] = json!({"type": "continue", "previous_message_id": "msg_01"});
    let sent = body.to_string();
    let (status, seen, codex_models) = send_to_anthropic_or_codex(sent.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen.as_deref(), Some(sent.as_bytes()));
    assert!(codex_models.is_empty());
}
