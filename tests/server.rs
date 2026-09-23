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
    provider::{CliHandlers, Generation, GenerationBody, Provider, ProviderError, RequestContext},
    registry::Registry,
    request_identity::ConversationIdentity,
    server::{
        AppFeatures, app, app_with_features, app_with_monitor, app_with_options,
        bind_proxy_listener,
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::util::ServiceExt;

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
        vec!["gpt-5.5".to_string(), "gpt-5.6-luna".to_string()]
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

/// Point every saved-login lookup and the state dir at an empty temporary
/// directory for the duration of a test, so neither `/v1/models` nor the
/// refresh an unknown model starts can read the developer's real logins,
/// reach the macOS Keychain or call a backend. Codex then reports itself as
/// unauthorized.
struct NoSavedLogins {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _dir: tempfile::TempDir,
}

impl NoSavedLogins {
    fn install() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let vars = [
            ("HOME", dir.path().to_path_buf()),
            ("CCP_CONFIG_DIR", dir.path().join("config")),
            ("CCP_CODEX_AUTH_FILE", dir.path().join("missing-auth.json")),
            ("XDG_STATE_HOME", dir.path().join("state")),
        ];
        let previous = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in vars {
            unsafe { std::env::set_var(key, value) };
        }
        Self {
            previous,
            _dir: dir,
        }
    }
}

impl Drop for NoSavedLogins {
    fn drop(&mut self) {
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

fn provider_entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["provider"] == name)
        .unwrap_or_else(|| panic!("provider {name} missing from {value}"))
}

#[tokio::test]
async fn models_endpoint_tags_rows_and_describes_every_provider() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?limit=1000").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert!(!data.is_empty());
    for entry in data {
        assert_eq!(entry["type"], "model");
        assert!(entry["display_name"].as_str().is_some());
        assert!(entry["provider"].as_str().is_some(), "{entry}");
    }
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(value["first_id"], data[0]["id"]);
    assert_eq!(value["last_id"], data[data.len() - 1]["id"]);

    // Backends without a listing call advertise their compiled-in lists and
    // say so; the rows keep the `<id> (<provider>)` display tag.
    let kimi_row = data
        .iter()
        .find(|m| m["id"] == "kimi-for-coding")
        .expect("kimi advertised");
    assert_eq!(kimi_row["provider"], "kimi");
    assert_eq!(kimi_row["display_name"], "kimi-for-coding (kimi)");
    let kimi = provider_entry(&value, "kimi");
    assert_eq!(kimi["auth"], "proxy");
    assert_eq!(kimi["source"], "bundled");
    assert_eq!(kimi["status"], "ok");
    assert_eq!(provider_entry(&value, "grok")["source"], "bundled");
    assert_eq!(provider_entry(&value, "cursor")["source"], "bundled");

    // Codex lists only what its backend answers; with no credentials it
    // answers nothing and says why.
    let codex = provider_entry(&value, "codex");
    assert_eq!(codex["auth"], "proxy");
    assert_eq!(codex["source"], "none");
    assert_eq!(codex["status"], "unauthorized");
    assert!(
        codex["detail"]
            .as_str()
            .unwrap()
            .contains("No Codex credentials")
    );
    assert!(codex["fetched_at"].is_null());
    assert!(!data.iter().any(|m| m["provider"] == "codex"));

    // The Anthropic passthrough holds no credential and Claude Code lists its
    // own models, so the provider is described and its rows are not emitted.
    // No id may contain "claude" or "anthropic": Claude Code's gateway
    // discovery would turn it into a picker row.
    let anthropic = provider_entry(&value, "anthropic");
    assert_eq!(anthropic["auth"], "client");
    assert_eq!(anthropic["source"], "none");
    assert_eq!(anthropic["status"], "not_listed");
    assert!(anthropic["detail"].as_str().unwrap().contains("client"));
    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&"opus"));
    assert!(
        !ids.iter()
            .any(|id| id.to_ascii_lowercase().contains("claude")
                || id.to_ascii_lowercase().contains("anthropic"))
    );
}

#[tokio::test]
async fn models_endpoint_provider_filter_narrows_rows_and_provider_block() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?provider=kimi").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert!(data.iter().all(|m| m["provider"] == "kimi"));
    assert_eq!(value["providers"].as_array().unwrap().len(), 1);
    assert_eq!(value["providers"][0]["provider"], "kimi");
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
            .contains("codex model listing unavailable (unauthorized)")
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

static AGENT_SUMMARY_LOCK: Mutex<()> = Mutex::new(());

/// Pin the progress-label mode for the duration of a test: set or clear
/// `CCP_AGENT_SUMMARY`, clear `CCP_AGENT_SUMMARY_MODEL` and point
/// `CCP_CONFIG_DIR` at an empty directory, so neither the environment nor a
/// `config.json` on the machine picks the mode or the label model. Tests that
/// pin it run one at a time.
struct PinnedAgentSummary {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _config_dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PinnedAgentSummary {
    fn install(mode: Option<&str>) -> Self {
        let lock = AGENT_SUMMARY_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let config_dir = tempfile::TempDir::new().unwrap();
        let previous = [
            "CCP_AGENT_SUMMARY",
            "CCP_AGENT_SUMMARY_MODEL",
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
    assert_eq!(bodies[0].model.as_deref(), Some("gpt-5.6-luna"));
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
async fn models_endpoint_tolerates_unknown_query_params() {
    let _no_logins = NoSavedLogins::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, _) = get_models(app, "/v1/models?limit=1000&after_id=x").await;
    assert_eq!(status, StatusCode::OK);
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

    // The provider reads its base URL once, when it is built.
    let previous = std::env::var_os("CCP_ANTHROPIC_BASE_URL");
    unsafe {
        std::env::set_var("CCP_ANTHROPIC_BASE_URL", format!("http://{address}"));
    }
    let provider = Arc::new(claude_code_mux::providers::anthropic::AnthropicProvider::new())
        as Arc<dyn Provider>;
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CCP_ANTHROPIC_BASE_URL", value),
            None => std::env::remove_var("CCP_ANTHROPIC_BASE_URL"),
        }
    }
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Anthropic,
        [provider],
    )));

    let response = post_json(app, "/v1/messages", messages_body("claude-opus-5")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(request_id(&response), "req_upstream");
}
