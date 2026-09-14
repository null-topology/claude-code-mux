use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::IntoResponse;
use claude_code_mux::{
    MessagesRequest,
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus, UsageQuality},
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
        _body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> axum::response::Response {
        self.captured
            .lock()
            .unwrap()
            .push((conversation_identity, ctx.session_id));
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

/// Point the codex provider at a credential file that does not exist for the
/// duration of a test, so `/v1/models` reports codex as unauthorized instead
/// of reading the developer's real Codex login and calling the backend.
struct NoCodexAuth {
    previous: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl NoCodexAuth {
    fn install() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("CCP_CODEX_AUTH_FILE");
        unsafe {
            std::env::set_var("CCP_CODEX_AUTH_FILE", dir.path().join("missing-auth.json"));
        }
        Self {
            previous,
            _dir: dir,
        }
    }
}

impl Drop for NoCodexAuth {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("CCP_CODEX_AUTH_FILE", value),
                None => std::env::remove_var("CCP_CODEX_AUTH_FILE"),
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
    let _no_codex = NoCodexAuth::install();
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
    let _no_codex = NoCodexAuth::install();
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
    let _no_codex = NoCodexAuth::install();
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
    let _no_codex = NoCodexAuth::install();
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
    let _no_codex = NoCodexAuth::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, value) = get_models(app, "/v1/models?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    let data = value["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(value["has_more"], json!(true));
    assert_eq!(value["last_id"], data[1]["id"]);
}

#[tokio::test]
async fn agent_progress_label_is_answered_without_a_provider() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: captured.clone(),
    }) as Arc<dyn Provider>;
    let app = app(Arc::new(Registry::from_providers(
        AliasProvider::Codex,
        [provider],
    )));
    let body = json!({
        "model": "gpt-5.5",
        "max_tokens": 64000,
        "stream": false,
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
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], json!("Reading server.rs"));
    assert_eq!(value["usage"]["input_tokens"], json!(0));
    // The label never reaches a backend, which is the whole point: upstream it
    // would cost the subagent's full context.
    assert!(captured.lock().unwrap().is_empty());
}

/// Send a classifier-shaped request through the proxy and report what the
/// backend saw and how the monitor recorded it.
async fn classifier_route(user_text: &str) -> (StatusCode, String, usize, Option<String>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(IdentityCaptureProvider {
        captured: captured.clone(),
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
    let _no_codex = NoCodexAuth::install();
    let app = app(Arc::new(Registry::with_default_alias()));
    let (status, _) = get_models(app, "/v1/models?limit=1000&after_id=x").await;
    assert_eq!(status, StatusCode::OK);
}
