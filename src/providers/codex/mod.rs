pub mod auth;
pub mod chat_completions;
pub mod client;
pub mod compaction;
pub mod continuation;
pub mod count_tokens;
pub(crate) mod events;
pub mod images;
pub mod models;
pub mod native;
pub(crate) mod rate_limits;
pub mod request_summary;
pub mod search;
pub mod transcription;
pub mod translate;
pub(crate) mod turn_state;
pub mod websocket;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::anthropic::sse::parse_sse_events;
use crate::config;
use crate::logging::create_logger;
use crate::provider::{
    CliHandlers, ListingAuth, ListingSource, ModelListing, Provider, RequestContext,
    ResponseOutcome,
};
use crate::registry;
use crate::request_identity::ConversationIdentity;

use self::auth::token_store::file_store;
use self::client::CodexHttpClient;
use self::compaction::{
    CompactionAttempt, CompactionError, abort_compaction_attempt, activate_compaction,
    apply_compaction_replay, begin_compaction, request_compaction, store_compaction,
};
use self::continuation::{
    ContinuationReservation, abort_continuation_for_owner, continuation_candidate_for_owner,
    record_continuation_for_owner,
};
use self::count_tokens::count_translated_tokens;
use self::translate::accumulate::accumulate_response_parts;
use self::translate::live_stream::LiveStreamTranslator;
use self::translate::model_allowlist::{
    assert_allowed_model, full_lane_web_search_model, resolve_model_request_with_config_override,
    uses_responses_lite,
};
use self::translate::reducer::{
    finish_metadata_from_upstream, reported_usage_report_from_upstream,
};
use self::translate::request::{
    TranslateOptions, compact_prompt_message_index, has_hosted_web_search,
    is_compact_messages_request, resolve_service_tier, translate_request,
};

const LIVE_STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
use self::translate::stream::translate_stream_bytes_with_traffic;

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub(crate) fn clear_session_compaction(session_id: &str) {
    compaction::clear_compaction(session_id);
}

/// Forget the newest Codex quota reading, for a test that leaves one behind.
pub fn clear_codex_rate_limits_for_tests() {
    rate_limits::clear_for_tests();
}

pub struct CodexProvider {
    client: Arc<CodexHttpClient>,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(CodexHttpClient::new()),
        }
    }
}

impl CodexProvider {
    async fn handle_messages_inner(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("gpt-6-sol");

        let mut resolved =
            resolve_model_request_with_config_override(model, !body.bypass_provider_model_override);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        if search::is_standalone_search_request(&body) {
            let (search_request, query) = match search::build_search_request(
                &body,
                &resolved.model,
                ctx.session_id.as_deref(),
            ) {
                Ok(request) => request,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        error.to_string(),
                    );
                }
            };
            // This branch builds a request of its own and keeps the lite-only
            // model the ordinary path would upgrade, so the model is read off
            // the request that will be sent, once it exists and before anything
            // is counted against it.
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.model_resolved(&ctx.req_id, &search_request.model);
            }
            let log = create_logger("codex");
            let started_at = Instant::now();
            log.info(
                "codex_standalone_search_started",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                    ("model".to_string(), serde_json::json!(&resolved.model)),
                    ("stream".to_string(), serde_json::json!(want_stream)),
                ])),
            );
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.upstream_started(&ctx.req_id);
            }
            let search_response = match self.client.post_search(&search_request, &ctx).await {
                Ok(response) => response,
                Err(error) => {
                    log.warn(
                        "codex_standalone_search_failed",
                        Some(serde_json::Map::from_iter([
                            ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                            ("model".to_string(), serde_json::json!(&resolved.model)),
                            ("status".to_string(), serde_json::json!(error.status)),
                            (
                                "ms".to_string(),
                                serde_json::json!(started_at.elapsed().as_millis()),
                            ),
                        ])),
                    );
                    return map_codex_error_to_response(&error);
                }
            };
            log.info(
                "codex_standalone_search_completed",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                    ("model".to_string(), serde_json::json!(&resolved.model)),
                    (
                        "resultCount".to_string(),
                        serde_json::json!(search_response.results.as_ref().map(Vec::len)),
                    ),
                    (
                        "ms".to_string(),
                        serde_json::json!(started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            let input_tokens = search::search_request_input_tokens(&search_request);
            let output_tokens = search::search_response_output_tokens(&search_response);
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.usage_updated(&ctx.req_id, Some(input_tokens), Some(output_tokens));
            }
            return search::anthropic_search_response(
                &search_response,
                &query,
                &message_id,
                model,
                want_stream,
                input_tokens,
                ctx.traffic.as_deref(),
            );
        }
        let use_responses_lite = apply_model_lane_for_request(&mut resolved.model, &body);

        // Subagents arrive with their parent's session id and their own agent
        // header; give each conversation its own cache scope so they do not
        // share one prompt cache key and one routing bucket upstream.
        let cache_scope = conversation_identity
            .as_ref()
            .map(ConversationIdentity::cache_scope)
            .or_else(|| ctx.session_id.clone());

        // Decided on the model that goes on the wire, after the lane upgrade.
        let service_tier = match resolve_service_tier(
            &resolved.model,
            resolved.service_tier.clone(),
            !body.bypass_catalog_service_tier,
        ) {
            Ok(choice) => choice,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        let mut translated = match translate_request(
            &body,
            TranslateOptions {
                session_id: cache_scope.clone(),
                service_tier: service_tier.tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        // The translated request is the document that goes to the backend, so
        // the model it carries is the one that will run: after the alias, the
        // configured override and the lane upgrade, and before compaction or
        // any transport touches it. A request that failed to translate never
        // reached a model and names none.
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &translated.model);
        }

        let compact_boundary = is_compact_messages_request(&body);
        let server_compaction_enabled = config::codex_server_compaction();
        let mut compaction_attempt = None;
        if !server_compaction_enabled && let Some(session_id) = ctx.session_id.as_deref() {
            compaction::clear_compaction(session_id);
        }
        if server_compaction_enabled
            && compact_boundary
            && let Some(session_id) = ctx.session_id.as_deref()
        {
            let attempt = begin_compaction(session_id, &translated.model);
            compaction_attempt = Some(attempt);
            log_compaction_event(
                "server_compaction_triggered",
                &ctx,
                translated.input.len(),
                None,
            );
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.compaction_started(&ctx.req_id);
            }
            let mut compaction_ctx = ctx.clone();
            compaction_ctx.monitor = None;
            let messages_after_prompt = compact_prompt_message_index(&body)
                .map_or(0, |index| body.messages.len() - index - 1);
            match request_compaction(
                self.client.as_ref(),
                &translated,
                &compaction_ctx,
                messages_after_prompt,
            )
            .await
            {
                Ok(native_history) => {
                    if store_compaction(session_id, attempt, native_history) {
                        log_compaction_event(
                            "server_compaction_completed",
                            &ctx,
                            translated.input.len(),
                            None,
                        );
                    } else {
                        log_compaction_event(
                            "server_compaction_failed",
                            &ctx,
                            translated.input.len(),
                            Some("compaction state was superseded or exceeded the in-memory limit"),
                        );
                    }
                }
                // Spent allowance, credits, plan access or a workspace that may
                // not make the request refuse the normal request the same way,
                // so it is not sent after them.
                Err(CompactionError::Upstream(error))
                    if error.usage_limit.is_some()
                        || matches!(
                            error.class,
                            Some(
                                events::CodexErrorClass::QuotaExceeded
                                    | events::CodexErrorClass::UsageNotIncluded
                                    | events::CodexErrorClass::PermissionDenied
                            )
                        ) =>
                {
                    abort_compaction_attempt(Some(session_id), Some(attempt));
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                    return map_codex_error_to_response(&error);
                }
                Err(error) => {
                    abort_compaction_attempt(Some(session_id), Some(attempt));
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                }
            }
        } else if server_compaction_enabled
            && !compact_boundary
            && let Some(replay) = apply_compaction_replay(ctx.session_id.as_deref(), &translated)
        {
            translated = replay.request;
            compaction_attempt = Some(replay.attempt);
        }

        // Check continuation
        let previous_response_id_enabled = config::codex_previous_response_id();
        let continuation = continuation_candidate_for_owner(
            conversation_identity.as_ref(),
            &translated,
            previous_response_id_enabled,
        );
        let turn_id = continuation.turn_id();
        let turn_state =
            turn_state::plan_request(&ctx.req_id, conversation_identity.as_ref(), &body);
        let configured_transport =
            transport_for_request(config::codex_transport(), compact_boundary);
        let transport = configured_transport.as_str();
        let upstream_started_at = Instant::now();
        let log = create_logger("codex");
        let req_id = ctx.req_id.clone();
        log.info(
            "codex_upstream_request_started",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                ("transport".to_string(), serde_json::json!(transport)),
                ("model".to_string(), serde_json::json!(&resolved.model)),
                ("stream".to_string(), serde_json::json!(want_stream)),
                (
                    "responsesLite".to_string(),
                    serde_json::json!(use_responses_lite),
                ),
                (
                    "previousResponseIdEnabled".to_string(),
                    serde_json::json!(previous_response_id_enabled),
                ),
                // The conversation the prompt cache belongs to; a subagent's
                // differs from its session, so captures need the mapping.
                ("cacheScope".to_string(), serde_json::json!(&cache_scope)),
                (
                    "hasPreviousResponseId".to_string(),
                    serde_json::json!(continuation.candidate().previous_response_id.is_some()),
                ),
                (
                    "inputDeltaCount".to_string(),
                    serde_json::json!(continuation.candidate().input_delta.as_ref().map(Vec::len)),
                ),
                ("turnId".to_string(), serde_json::json!(turn_id)),
                (
                    "newTurn".to_string(),
                    serde_json::json!(turn_state.new_turn),
                ),
                (
                    "turnStateSent".to_string(),
                    serde_json::json!(turn_state.sent),
                ),
                (
                    "serviceTier".to_string(),
                    serde_json::json!(service_tier.tier),
                ),
                (
                    "serviceTierSource".to_string(),
                    serde_json::json!(service_tier.source),
                ),
            ])),
        );

        // Post to upstream with continuation
        let client = self.client.clone();
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        if want_stream {
            let stream_request = translated.clone();
            let response = live_stream_response(
                client,
                message_id,
                model,
                ctx,
                stream_request,
                continuation,
                LiveStreamCompaction {
                    compact_boundary,
                    attempt: compaction_attempt,
                },
                configured_transport,
            )
            .await;
            log.info(
                "codex_upstream_response_ready",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&req_id)),
                    ("transport".to_string(), serde_json::json!(transport)),
                    (
                        "status".to_string(),
                        serde_json::json!(response.status().as_u16()),
                    ),
                    (
                        "ms".to_string(),
                        serde_json::json!(upstream_started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            return response;
        }

        let request_continuation = continuation.clone();
        let upstream = match client
            .post_codex_with_transport(&translated, &ctx, Some(&continuation), configured_transport)
            .await
        {
            Ok(r) => {
                turn_state::observe_buffered_response(&ctx.req_id, &r.headers, &r.body);
                r
            }
            Err(e) => {
                log.warn(
                    "codex_upstream_request_failed",
                    Some(serde_json::Map::from_iter([
                        ("reqId".to_string(), serde_json::json!(&req_id)),
                        ("transport".to_string(), serde_json::json!(transport)),
                        ("status".to_string(), serde_json::json!(e.status)),
                        (
                            "origin".to_string(),
                            serde_json::json!(format!("{:?}", e.origin)),
                        ),
                        ("error".to_string(), serde_json::json!(&e.message)),
                        (
                            "detail".to_string(),
                            serde_json::json!(failed_request_log_detail(&e)),
                        ),
                        (
                            "ms".to_string(),
                            serde_json::json!(upstream_started_at.elapsed().as_millis()),
                        ),
                    ])),
                );
                abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                return map_codex_transport_error_to_response(&e, configured_transport);
            }
        };
        log.info(
            "codex_upstream_response_received",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                ("transport".to_string(), serde_json::json!(transport)),
                ("status".to_string(), serde_json::json!(upstream.status)),
                (
                    "bodyBytes".to_string(),
                    serde_json::json!(upstream.body.len()),
                ),
                (
                    "ms".to_string(),
                    serde_json::json!(upstream_started_at.elapsed().as_millis()),
                ),
            ])),
        );

        if want_stream {
            let estimated_input_tokens = count_translated_tokens(&translated);
            let sse_bytes = match translate_stream_bytes_with_traffic(
                &upstream.body,
                &message_id,
                model,
                estimated_input_tokens,
                ctx.traffic.as_deref(),
            ) {
                Ok(b) => b,
                Err(e) => {
                    abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    return map_codex_failure_to_response(&format!(
                        "Stream translation error: {e}"
                    ));
                }
            };
            if let Some(monitor) = ctx.monitor.as_ref() {
                // The usage comes from the upstream body, not from the stream
                // just written: its terminal `usage` object is zero-filled
                // whenever the backend reported nothing.
                monitor.stream_progress_usage(
                    &ctx.req_id,
                    sse_bytes.len() as u64,
                    count_sse_events(&sse_bytes),
                    reported_usage_report_from_upstream(&upstream.body),
                );
            }
            update_continuation_from_upstream(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction_attempt,
                &translated,
                &upstream.body,
                upstream.socket_id,
                compact_boundary,
            );

            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            (headers, sse_bytes).into_response()
        } else {
            match accumulate_response_parts(
                &upstream.body,
                &message_id,
                model,
                ctx.traffic.as_deref(),
            ) {
                Ok((json, reported_usage)) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_reported(&ctx.req_id, reported_usage);
                    }
                    update_continuation_from_upstream(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction_attempt,
                        &translated,
                        &upstream.body,
                        upstream.socket_id,
                        compact_boundary,
                    );
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => {
                    abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    map_codex_failure_to_response(&format!("Accumulation error: {e}"))
                }
            }
        }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        let mut models: Vec<String> = registry::CODEX_MODELS
            .iter()
            .map(|m| m.to_string())
            .collect();
        for m in registry::CODEX_MODELS {
            models.push(format!("{m}-fast"));
        }
        models.sort_unstable();
        models.dedup();
        models
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CODEX_CLI
    }

    fn has_credentials(&self) -> bool {
        matches!(file_store().load_auth(), Ok(Some(_)))
    }

    async fn list_models(&self, client_headers: Option<&axum::http::HeaderMap>) -> ModelListing {
        match self.client.list_models(client_headers).await {
            Ok(inventory) => ModelListing {
                provider: "codex",
                auth: ListingAuth::Proxy,
                source: ListingSource::Upstream,
                status: "ok",
                detail: None,
                fetched_at: Some(inventory.fetched_at),
                models: inventory
                    .models
                    .iter()
                    .map(models::UpstreamModel::to_listing_row)
                    .collect(),
            },
            Err(error) => {
                create_logger("codex").warn(
                    "codex model listing unavailable",
                    Some(serde_json::Map::from_iter([
                        ("status".to_string(), serde_json::json!(error.status())),
                        ("detail".to_string(), serde_json::json!(error.detail())),
                    ])),
                );
                ModelListing::unavailable("codex", error.status(), error.detail().to_string())
            }
        }
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        self.handle_messages_inner(body, ctx, None).await
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        self.handle_messages_inner(body, ctx, conversation_identity)
            .await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.as_deref().unwrap_or("gpt-6-sol");
        let mut resolved =
            resolve_model_request_with_config_override(model, !body.bypass_provider_model_override);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        let use_responses_lite = apply_model_lane_for_request(&mut resolved.model, &body);

        let service_tier = match resolve_service_tier(
            &resolved.model,
            resolved.service_tier.clone(),
            !body.bypass_catalog_service_tier,
        ) {
            Ok(choice) => choice,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        // The estimate below is computed here, against the translated request;
        // nothing is sent, so no model runs and none is named as having run.
        let translated = match translate_request(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: service_tier.tier,
                model: resolved.model.clone(),
                use_responses_lite,
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        let tokens = count_translated_tokens(&translated);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

/// Picks the upstream model and lane for a request. Hosted web_search must
/// run on the full Responses API (the lite lane rejects hosted tools), and
/// lite-only models like gpt-5.6-luna don't exist there, so such requests
/// are upgraded to a full-lane model. Returns whether to use the lite lane.
fn apply_model_lane_for_request(model: &mut String, body: &MessagesRequest) -> bool {
    if has_hosted_web_search(body) {
        *model = full_lane_web_search_model(model).to_string();
        return false;
    }
    uses_responses_lite(model)
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn log_compaction_event(
    event: &str,
    ctx: &RequestContext,
    input_items: usize,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("inputItems".into(), serde_json::json!(input_items));
    if let Some(error) = error {
        fields.insert("error".into(), serde_json::json!(error));
        create_logger("codex").warn(event, Some(fields));
    } else {
        create_logger("codex").info(event, Some(fields));
    }
}

fn abort_request_state(
    session_id: Option<&str>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
) {
    abort_compaction_attempt(session_id, compaction_attempt);
    abort_continuation_for_owner(continuation);
}

/// Ends a live stream the proxy refused to go on with: the stream first, then
/// the socket the turn may already have pooled, then the request state. The
/// stream task pools the socket when the backend finished its answer before
/// the refusal; a socket pooled after the removal goes again when the answer
/// it carries can no longer be handed on.
fn abort_refused_live_stream(
    upstream_events: websocket::CodexWebSocketEventStream,
    session_id: Option<&str>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
) {
    drop(upstream_events);
    client::invalidate_live_continuation_pool(Some(continuation));
    abort_request_state(session_id, continuation, compaction_attempt);
}

struct LiveRequestStateCleanup {
    continuation: ContinuationReservation,
    session_id: Option<String>,
    compaction_attempt: Option<CompactionAttempt>,
    armed: bool,
}

impl LiveRequestStateCleanup {
    fn new(
        continuation: ContinuationReservation,
        session_id: Option<String>,
        compaction_attempt: Option<CompactionAttempt>,
    ) -> Self {
        Self {
            continuation,
            session_id,
            compaction_attempt,
            armed: true,
        }
    }

    fn abort(&mut self) {
        if self.armed {
            abort_request_state(
                self.session_id.as_deref(),
                &self.continuation,
                self.compaction_attempt,
            );
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiveRequestStateCleanup {
    fn drop(&mut self) {
        if self.armed {
            abort_request_state(
                self.session_id.as_deref(),
                &self.continuation,
                self.compaction_attempt,
            );
        }
    }
}

enum LiveStreamStart {
    Response(Response),
    Retry {
        error: client::CodexError,
        full_context_retry_attempted: bool,
    },
}

#[derive(Clone, Copy)]
struct LiveStreamCompaction {
    compact_boundary: bool,
    attempt: Option<CompactionAttempt>,
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_body: translate::request::ResponsesRequest,
    continuation: ContinuationReservation,
    compaction: LiveStreamCompaction,
    transport: config::CodexTransport,
) -> Response {
    let model = model.to_string();
    let request_continuation = continuation.clone();
    let mut cleanup = LiveRequestStateCleanup::new(
        request_continuation.clone(),
        ctx.session_id.clone(),
        compaction.attempt,
    );
    let mut continuation = Some(continuation);

    loop {
        // The stream, and the HTTP response headers when it is an HTTP one:
        // they hold the quota clocks a spent window's event may leave out.
        let upstream_events = match transport {
            config::CodexTransport::Http => {
                client
                    .stream_codex_http_events_for_owner(&request_body, &ctx, false)
                    .await
            }
            config::CodexTransport::WebSocket => client
                .stream_codex_websocket_events_for_owner(&request_body, &ctx, continuation.as_ref())
                .await
                .map(|events| (events, Vec::new())),
            config::CodexTransport::Auto => {
                client
                    .stream_codex_auto_events_for_owner(&request_body, &ctx, continuation.as_ref())
                    .await
            }
        };
        let (upstream_events, response_headers) = match upstream_events {
            Ok(events) => events,
            // The continuation is the proxy's own state, so a request the
            // backend no longer remembers is resent once with full context.
            // Every other failure goes to the client, which owns the retries.
            Err(err)
                if err.origin != client::CodexErrorOrigin::Http
                    && is_missing_previous_response_error(&err)
                    && drop_live_continuation_for_retry(&mut continuation) =>
            {
                continue;
            }
            Err(err) => {
                cleanup.abort();
                return map_codex_transport_error_to_response(&err, transport);
            }
        };

        let account_id = upstream_events.account_reader();
        match live_stream_response_once(
            upstream_events,
            response_headers,
            message_id.clone(),
            &model,
            ctx.clone(),
            request_continuation.clone(),
            request_body.clone(),
            compaction,
            transport,
            client.buffered_response_limit(),
        )
        .await
        {
            LiveStreamStart::Response(response) => {
                cleanup.disarm();
                return with_rate_limit_headers(response, account_id().as_deref());
            }
            LiveStreamStart::Retry {
                error,
                full_context_retry_attempted,
            } => {
                if error.origin != client::CodexErrorOrigin::Http
                    && !full_context_retry_attempted
                    && is_missing_previous_response_error(&error)
                    && drop_live_continuation_for_retry(&mut continuation)
                {
                    continue;
                }
                cleanup.abort();
                return map_codex_transport_error_to_response(&error, transport);
            }
        }
    }
}

fn provider_retry(
    upstream_events: &websocket::CodexWebSocketEventStream,
    error: client::CodexError,
) -> LiveStreamStart {
    let full_context_retry_attempted = upstream_events.used_full_context_retry();
    upstream_events.mark_provider_retry_handoff();
    LiveStreamStart::Retry {
        error,
        full_context_retry_attempted,
    }
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once(
    mut upstream_events: websocket::CodexWebSocketEventStream,
    response_headers: Vec<(String, String)>,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: translate::request::ResponsesRequest,
    compaction: LiveStreamCompaction,
    transport: config::CodexTransport,
    upstream_copy_limit: usize,
) -> LiveStreamStart {
    let estimated_input_tokens = count_translated_tokens(&request_body);
    // The strings the translator keeps of the answer share the client's limit
    // for a buffered one; past it the stream ends with an error.
    let mut translator = LiveStreamTranslator::with_estimated_input_tokens(
        message_id,
        model.to_string(),
        estimated_input_tokens,
    )
    .with_retained_limit(upstream_copy_limit);
    let mut upstream_copy = LiveUpstreamCopy::new(upstream_copy_limit);
    // Keep protocol framing private until real output makes a transparent retry unsafe.
    // Every branch that consumes pending_chunk returns, so it is never flushed twice.
    let mut pending_chunk = Vec::new();
    let mut generation_started = false;

    while let Some(item) = upstream_events.recv().await {
        let payload = match item {
            Ok(payload) => payload,
            Err(err) => {
                if retryable_live_start_codex_error(&err) {
                    return provider_retry(&upstream_events, err);
                }
                abort_request_state(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(map_codex_transport_error_to_response(
                    &err, transport,
                ));
            }
        };
        if !generation_started && codex_generation_event(&payload) {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.generation_started(&ctx.req_id);
            }
            generation_started = true;
        }
        if !upstream_events.rate_limits_observed() {
            rate_limits::observe_event(upstream_events.account_id().as_deref(), &payload);
        }
        turn_state::observe_event(&ctx.req_id, &payload);
        upstream_copy.append(&ctx.req_id, &payload);
        let (chunk, terminal) = match translate_live_stream_payload(&mut translator, &payload, None)
        {
            Ok(result) => result,
            Err(message) => {
                // A spent subscription window reopens hours from now: answer
                // it once, with the reset clock upstream supplied, and never
                // as something to retry.
                if let Some(limit) =
                    events::usage_limit_from_event_with_headers(&payload, &response_headers)
                {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    let limit = limit.for_account(upstream_events.account_id().as_deref());
                    return LiveStreamStart::Response(usage_limit_response(&limit));
                }
                let failure = events::classify_event_failure(&payload);
                let class = failure.as_ref().and_then(|failure| failure.class);
                if retryable_live_start_payload(&payload, &message) {
                    let lower_message = message.to_ascii_lowercase();
                    let status = class
                        .map(events::CodexErrorClass::status)
                        .or_else(|| websocket::event_error_status(&payload))
                        .unwrap_or_else(|| {
                            let error = payload.get("error").or_else(|| {
                                payload.get("response").and_then(|value| value.get("error"))
                            });
                            let overloaded = error.is_some_and(|error| {
                                error.get("code").and_then(|value| value.as_str())
                                    == Some("overloaded_error")
                                    || error.get("type").and_then(|value| value.as_str())
                                        == Some("overloaded_error")
                            });
                            if lower_message.contains("rate limit") {
                                429
                            } else if overloaded || lower_message.contains("overloaded") {
                                529
                            } else {
                                503
                            }
                        });
                    return provider_retry(
                        &upstream_events,
                        client::CodexError {
                            status,
                            message: message.clone(),
                            detail: Some(message),
                            retry_after: retry_after_from_live_payload(&payload),
                            usage_limit: None,
                            class,
                            origin: client::CodexErrorOrigin::WebSocket,
                        },
                    );
                }
                abort_refused_live_stream(
                    upstream_events,
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                if let Some(class) = class {
                    // The classifier's message, as the buffered paths answer
                    // with, so a code that stands in for a missing message
                    // does so here too.
                    let native = failure
                        .as_ref()
                        .map_or(message.as_str(), |failure| failure.message.as_str());
                    return LiveStreamStart::Response(classified_error_response(
                        class, native, None,
                    ));
                }
                return LiveStreamStart::Response(map_codex_failure_to_response(&message));
            }
        };
        // Until output starts the answer is held here, and only the message
        // start and keepalive pings add to it, so a backend that keeps a
        // response open with no output cannot grow it past the limit.
        if !translator.has_semantic_output()
            && pending_chunk.len().saturating_add(chunk.len()) > upstream_copy_limit
        {
            abort_refused_live_stream(
                upstream_events,
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction.attempt,
            );
            return LiveStreamStart::Response(map_codex_error_to_response(
                &client::buffered_response_too_large_error(client::CodexErrorOrigin::WebSocket),
            ));
        }
        pending_chunk.extend_from_slice(&chunk);
        if translator.has_semantic_output() && !pending_chunk.is_empty() {
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &mut translator, &pending_chunk);
            if terminal {
                upstream_copy.update_continuation(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction,
                    &request_body,
                    upstream_events.socket_id(),
                );
                let outcome = live_stream_outcome(&translator);
                return LiveStreamStart::Response(single_live_stream_response(
                    pending_chunk,
                    outcome,
                ));
            }
            return LiveStreamStart::Response(remaining_live_stream_response(
                upstream_events,
                response_headers,
                translator,
                pending_chunk,
                ctx,
                request_continuation,
                request_body,
                upstream_copy,
                compaction,
            ));
        }
        if terminal {
            upstream_copy.update_continuation(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction,
                &request_body,
                upstream_events.socket_id(),
            );
            let outcome = live_stream_outcome(&translator);
            if pending_chunk.is_empty() {
                return LiveStreamStart::Response(empty_live_stream_response(outcome));
            }
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &mut translator, &pending_chunk);
            return LiveStreamStart::Response(single_live_stream_response(pending_chunk, outcome));
        }
    }

    provider_retry(
        &upstream_events,
        client::CodexError {
            status: 0,
            message: "WebSocket connection closed before terminal Codex response event".to_string(),
            detail: Some(websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocket,
        },
    )
}

fn codex_generation_event(payload: &serde_json::Value) -> bool {
    !matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("codex.rate_limits" | "keepalive") | None
    )
}

fn translate_live_stream_payload(
    translator: &mut LiveStreamTranslator,
    payload: &serde_json::Value,
    traffic: Option<&crate::traffic::TrafficCapture>,
) -> Result<(Vec<u8>, bool), String> {
    let chunk = translator.accept(payload, traffic)?;
    let terminal = is_codex_terminal_event(payload) || translator.is_finished();
    Ok((chunk, terminal))
}

fn record_live_stream_downstream_capture(ctx: &RequestContext, chunk: &[u8]) {
    let Some(traffic) = ctx.traffic.as_ref() else {
        return;
    };
    for event in parse_sse_events(chunk) {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event.event.as_deref().unwrap_or("message"),
                "data": data,
            }),
        );
    }
}

/// Report one outgoing chunk to the monitor.
///
/// The usage comes from the translator, not from the bytes: every finish
/// carries a `usage` object because the Messages shape requires one, and the
/// salvage paths fill it with zeros the backend never sent. Reading it back
/// would turn those zeros into the backend's final count.
fn record_live_stream_progress(
    ctx: &RequestContext,
    translator: &mut LiveStreamTranslator,
    chunk: &[u8],
) {
    let usage = translator.take_usage_report();
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.stream_progress_usage(
            &ctx.req_id,
            chunk.len() as u64,
            count_sse_events(chunk),
            usage,
        );
    }
}

/// The outcome of a Messages stream: it ends with `message_stop` or it did not
/// complete, whatever the 200 that already left says.
fn live_stream_outcome(translator: &LiveStreamTranslator) -> ResponseOutcome {
    let outcome = ResponseOutcome::requiring_terminal();
    note_live_stream_outcome(&outcome, translator);
    outcome
}

fn note_live_stream_outcome(outcome: &ResponseOutcome, translator: &LiveStreamTranslator) {
    if translator.emitted_terminal() {
        outcome.mark_terminal();
    }
}

fn single_live_stream_response(chunk: Vec<u8>, outcome: ResponseOutcome) -> Response {
    let mut response = event_stream_response(futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(chunk))
    }));
    response.extensions_mut().insert(outcome);
    response
}

fn empty_live_stream_response(outcome: ResponseOutcome) -> Response {
    let mut response =
        event_stream_response(futures_util::stream::empty::<Result<Bytes, std::io::Error>>());
    response.extensions_mut().insert(outcome);
    response
}

#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response(
    mut upstream_events: websocket::CodexWebSocketEventStream,
    response_headers: Vec<(String, String)>,
    mut translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: translate::request::ResponsesRequest,
    mut upstream_copy: LiveUpstreamCopy,
    compaction: LiveStreamCompaction,
) -> Response {
    let outcome = live_stream_outcome(&translator);
    let response_outcome = outcome.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    // Output has started, so the request is not sent again under another
    // account; the gauge keys this stream's readings by the one it went with.
    let account_id = upstream_events.account_id();
    let rate_limits_observed = upstream_events.rate_limits_observed();
    tokio::spawn(async move {
        if tx.send(Ok(Bytes::from(first_chunk))).await.is_err() {
            abort_request_state(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction.attempt,
            );
            return;
        }
        let mut heartbeat = tokio::time::interval(LIVE_STREAM_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        loop {
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return;
                }
                item = upstream_events.recv() => item,
                _ = heartbeat.tick() => {
                    let chunk = translator.ping_chunk(ctx.traffic.as_deref());
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &mut translator, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    continue;
                }
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(payload) => {
                    if !rate_limits_observed {
                        rate_limits::observe_event(account_id.as_deref(), &payload);
                    }
                    turn_state::observe_event(&ctx.req_id, &payload);
                    upstream_copy.append(&ctx.req_id, &payload);
                    let (chunk, terminal) = match translate_live_stream_payload(
                        &mut translator,
                        &payload,
                        ctx.traffic.as_deref(),
                    ) {
                        Ok(result) => result,
                        Err(message) => {
                            abort_refused_live_stream(
                                upstream_events,
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            // A spent window after output started is still a
                            // rate limit, told the way the status line would
                            // have told it, and the gauge learns of it.
                            let limit = events::usage_limit_from_event_with_headers(
                                &payload,
                                &response_headers,
                            )
                            .map(|limit| limit.for_account(account_id.as_deref()));
                            if let Some(limit) = limit.as_ref() {
                                rate_limits::observe_usage_limit(limit);
                            }
                            // A classified failure is told in the classifier's
                            // words, which name a missing model that came with
                            // no message by its code.
                            let classified = events::classify_event_failure(&payload)
                                .and_then(|failure| Some((failure.class?, failure.message)));
                            let (error_type, text) = match (limit.as_ref(), classified.as_ref()) {
                                (Some(limit), _) => ("rate_limit_error", limit.message.as_str()),
                                (None, Some((class, native))) => {
                                    let error_type = class.error_type();
                                    (error_type, client_error_text(native, error_type))
                                }
                                (None, None) => {
                                    ("api_error", client_error_text(&message, "api_error"))
                                }
                            };
                            let chunk =
                                translator.error_chunk(text, error_type, ctx.traffic.as_deref());
                            // The status left with the first chunk, so the
                            // failure travels beside the body instead.
                            outcome.fail(message);
                            if !chunk.is_empty() {
                                record_live_stream_progress(&ctx, &mut translator, &chunk);
                                let _ = tx.send(Ok(Bytes::from(chunk))).await;
                            }
                            return;
                        }
                    };
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &mut translator, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    note_live_stream_outcome(&outcome, &translator);
                    if terminal {
                        upstream_copy.update_continuation(
                            ctx.session_id.as_deref(),
                            &request_continuation,
                            compaction,
                            &request_body,
                            upstream_events.socket_id(),
                        );
                        return;
                    }
                }
                Err(err) => {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    let chunk =
                        translator.finish_after_closed_completed_tool_call(ctx.traffic.as_deref());
                    if !chunk.is_empty() {
                        // A tool call the backend had already closed is a
                        // complete turn: the salvaged finish is a real terminal.
                        note_live_stream_outcome(&outcome, &translator);
                        record_live_stream_progress(&ctx, &mut translator, &chunk);
                        let _ = tx.send(Ok(Bytes::from(chunk))).await;
                        return;
                    }
                    let error_type = codex_stream_error_type(&err);
                    let chunk = translator.error_chunk(
                        client_error_text(codex_error_message(&err), error_type),
                        error_type,
                        ctx.traffic.as_deref(),
                    );
                    outcome.fail(codex_error_message(&err));
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &mut translator, &chunk);
                        let _ = tx.send(Ok(Bytes::from(chunk))).await;
                    }
                    return;
                }
            }
        }

        abort_request_state(
            ctx.session_id.as_deref(),
            &request_continuation,
            compaction.attempt,
        );
        let chunk = translator.finish_after_closed_completed_tool_call(ctx.traffic.as_deref());
        if !chunk.is_empty() {
            note_live_stream_outcome(&outcome, &translator);
            record_live_stream_progress(&ctx, &mut translator, &chunk);
            let _ = tx.send(Ok(Bytes::from(chunk))).await;
            return;
        }
        let message = "Upstream event stream closed before terminal Codex response event";
        let chunk = translator.error_chunk(
            client_error_text(message, "api_error"),
            "api_error",
            ctx.traffic.as_deref(),
        );
        outcome.fail(message);
        if !chunk.is_empty() {
            record_live_stream_progress(&ctx, &mut translator, &chunk);
            let _ = tx.send(Ok(Bytes::from(chunk))).await;
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut response = event_stream_response(stream);
    response.extensions_mut().insert(response_outcome);
    response
}

/// The copy of a live stream's upstream events that its continuation state is
/// read from at the terminal event; the relay to the client does not use it.
/// It holds at most `limit` bytes: the event that would take it past the
/// limit releases it for good, and the stream then leaves no continuation
/// state.
struct LiveUpstreamCopy {
    body: Option<Vec<u8>>,
    limit: usize,
}

impl LiveUpstreamCopy {
    fn new(limit: usize) -> Self {
        Self {
            body: Some(Vec::new()),
            limit,
        }
    }

    /// Adds one event. Returns true for the event that released the copy,
    /// the only one that logs `codex_live_stream_copy_dropped`.
    fn append(&mut self, req_id: &str, payload: &serde_json::Value) -> bool {
        let Some(body) = self.body.as_mut() else {
            return false;
        };
        let room = self.limit.saturating_sub(body.len());
        if append_upstream_sse_payload(body, payload, room) {
            return false;
        }
        self.body = None;
        create_logger("codex").warn(
            "codex_live_stream_copy_dropped",
            Some(live_stream_copy_dropped_fields(req_id, self.limit)),
        );
        true
    }

    /// Updates continuation state at the terminal event. Without the copy the
    /// request's state is dropped, as for a response that leaves none usable,
    /// so the conversation's next request sends its full context.
    fn update_continuation(
        &self,
        session_id: Option<&str>,
        continuation: &ContinuationReservation,
        compaction: LiveStreamCompaction,
        request_body: &translate::request::ResponsesRequest,
        socket_id: Option<u64>,
    ) {
        match &self.body {
            Some(body) => update_continuation_from_upstream(
                session_id,
                continuation,
                compaction.attempt,
                request_body,
                body,
                socket_id,
                compaction.compact_boundary,
            ),
            None => abort_request_state(session_id, continuation, compaction.attempt),
        }
    }
}

/// The fields of `codex_live_stream_copy_dropped`: the request id and the
/// limit in bytes, nothing from the stream.
fn live_stream_copy_dropped_fields(
    req_id: &str,
    limit: usize,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        ("limitBytes".to_string(), serde_json::json!(limit)),
    ])
}

/// Adds `payload` to `buffer` as one SSE event, `data: ` and its compact JSON
/// on a single line (compact JSON holds no line break), when the event takes
/// at most `room` bytes, and returns whether it did. The payload is
/// serialised into the buffer and stops at `room`, so an event that does not
/// fit is never built whole; the buffer is then left as it was.
fn append_upstream_sse_payload(
    buffer: &mut Vec<u8>,
    payload: &serde_json::Value,
    room: usize,
) -> bool {
    use std::io::Write as _;

    let start = buffer.len();
    let mut writer = RoomLimitedWriter { buffer, room };
    let written = writer.write_all(b"data: ").is_ok()
        && serde_json::to_writer(&mut writer, payload).is_ok()
        && writer.write_all(b"\n\n").is_ok();
    if !written {
        buffer.truncate(start);
    }
    written
}

/// Appends to a buffer and fails a write that would take it past `room`
/// more bytes, so a serialiser stops there.
struct RoomLimitedWriter<'a> {
    buffer: &'a mut Vec<u8>,
    room: usize,
}

impl std::io::Write for RoomLimitedWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.room {
            return Err(std::io::Error::other("event past the copy limit"));
        }
        self.room -= data.len();
        self.buffer.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn event_stream_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, Body::from_stream(stream)).into_response()
}

fn is_codex_terminal_event(payload: &serde_json::Value) -> bool {
    matches!(
        payload.get("type").and_then(|v| v.as_str()),
        Some("response.completed")
            | Some("response.incomplete")
            | Some("response.done")
            | Some("response.failed")
            | Some("response.error")
            | Some("error")
    )
}

/// How many characters of an error's detail `codex_upstream_request_failed`
/// logs, as many as `codex_websocket_fallback_to_http` keeps of its own.
const FAILED_REQUEST_LOG_DETAIL_CHARS: usize = 300;

/// The `detail` field of `codex_upstream_request_failed`: the error's detail cut
/// to `FAILED_REQUEST_LOG_DETAIL_CHARS` characters, or `None` when it has none.
fn failed_request_log_detail(err: &client::CodexError) -> Option<String> {
    err.detail.as_deref().map(|detail| {
        detail
            .chars()
            .take(FAILED_REQUEST_LOG_DETAIL_CHARS)
            .collect()
    })
}

fn retryable_live_start_codex_error(err: &client::CodexError) -> bool {
    if err.origin == client::CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529);
    }
    if err.detail.as_deref() == Some(websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529)
        || (err.status == 0 && retryable_live_message(codex_error_message(err)))
}

fn is_missing_previous_response_error(err: &client::CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
    )
}

fn drop_live_continuation_for_retry(continuation: &mut Option<ContinuationReservation>) -> bool {
    if continuation
        .as_ref()
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    if let Some(reservation) = continuation.as_ref() {
        *continuation = Some(reservation.full_context_retry());
    }
    true
}

fn retryable_live_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overloaded",
        "rate limit",
        "you can retry your request",
        "temporarily unavailable",
        "timed out",
        "connection closed",
        "connection reset",
        "broken pipe",
        "epipe",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn retryable_live_start_payload(payload: &serde_json::Value, _message: &str) -> bool {
    events::classify_event_failure(payload).is_some_and(|failure| failure.retryable())
}

fn retry_after_from_live_payload(payload: &serde_json::Value) -> Option<String> {
    events::classify_event_failure(payload).and_then(|failure| failure.retry_after)
}

fn codex_stream_error_type(err: &client::CodexError) -> &'static str {
    if let Some(class) = err.class {
        return class.error_type();
    }
    match err.status {
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ if codex_error_message(err)
            .to_lowercase()
            .contains("overloaded") =>
        {
            "overloaded_error"
        }
        _ => "api_error",
    }
}

/// Claude Code's compaction summary goes over HTTP whatever the configured
/// transport: it is a large request whose WebSocket connection can close
/// before the terminal event. Every other request keeps the configured one.
fn transport_for_request(
    configured: config::CodexTransport,
    compact_boundary: bool,
) -> config::CodexTransport {
    if compact_boundary {
        config::CodexTransport::Http
    } else {
        configured
    }
}

#[allow(clippy::too_many_arguments)]
fn update_continuation_from_upstream(
    session_id: Option<&str>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
    request_body: &translate::request::ResponsesRequest,
    upstream_body: &[u8],
    socket_id: Option<u64>,
    compact_boundary: bool,
) {
    match finish_metadata_from_upstream(upstream_body) {
        Ok(Some(finish)) if finish.continuation_eligible => {
            if compact_boundary {
                activate_compaction(
                    session_id,
                    compaction_attempt,
                    &request_body.model,
                    &finish.output_items,
                );
            }
            record_continuation_for_owner(
                continuation,
                request_body,
                finish.response_id.as_deref(),
                socket_id,
                &finish.output_items,
            );
        }
        _ => {
            abort_compaction_attempt(session_id, compaction_attempt);
            abort_continuation_for_owner(continuation);
        }
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Attach the newest Codex quota reading to a response, so a client can warn
/// its user before the allowance runs out rather than only when it has.
///
/// A response that already states a rate limit status is left alone: a refusal
/// carries the exact state of the window that refused it, which is better than
/// a snapshot taken a turn earlier. So is one that tells the client not to
/// retry: a spent credit balance is not a window, and a snapshot saying the
/// window is allowed would contradict the refusal. The snapshot is the one
/// held for `account`, the ChatGPT account the request went out with; one
/// held for another account says nothing about this request.
fn with_rate_limit_headers(mut response: Response, account: Option<&str>) -> Response {
    let headers = response.headers();
    if headers.contains_key("anthropic-ratelimit-unified-status")
        || headers.contains_key("x-should-retry")
    {
        return response;
    }
    let Some(snapshot) = rate_limits::latest(account) else {
        return response;
    };

    let headers = response.headers_mut();
    for (name, value) in snapshot.headers() {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert(name, value);
        }
    }
    response
}

/// Answer a spent subscription window with the rate limit headers the client
/// reads, so it can name the exhausted window and show when it reopens.
///
/// `Retry-After` is deliberately absent: clients sleep for its full value, and
/// here that is hours. `x-should-retry: false` stops the retry loop instead,
/// which is the honest signal — a spent window does not reopen on a backoff.
///
/// The window the limit names is recorded as spent until its reset
/// (`rate_limits::observe_usage_limit`), so later responses carry it.
fn usage_limit_response(limit: &events::CodexUsageLimit) -> Response {
    rate_limits::observe_usage_limit(limit);
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-should-retry"),
        HeaderValue::from_static("false"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-ratelimit-unified-status"),
        HeaderValue::from_static("rejected"),
    );
    if let Some(resets_at) = limit.resets_at
        && let Ok(value) = HeaderValue::from_str(&resets_at.to_string())
    {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-reset"),
            value,
        );
    }
    if let Some(window) = limit.window {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
            HeaderValue::from_static(window.claim()),
        );
    }

    (
        headers,
        json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &limit.message,
        ),
    )
        .into_response()
}

/// `map_codex_error_to_response` for a request sent on `transport`. Only the
/// `websocket` transport's answer to a size refusal names another transport:
/// under `auto` with its HTTP fallback disabled no other one is available.
fn map_codex_transport_error_to_response(
    err: &client::CodexError,
    transport: config::CodexTransport,
) -> Response {
    if transport == config::CodexTransport::WebSocket && websocket::is_message_size_refusal(err) {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            format!(
                "{}. The auto transport sends such a request over HTTP.",
                err.message
            ),
        );
    }
    map_codex_error_to_response(err)
}

fn map_codex_error_to_response(err: &client::CodexError) -> Response {
    if let Some(limit) = err.usage_limit.as_ref() {
        return usage_limit_response(limit);
    }
    // A request the WebSocket refused for its size: the message says what
    // happened, with the request's size and the limit when one was named.
    if websocket::is_message_size_refusal(err) {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            err.message.clone(),
        );
    }
    if let Some(class) = err.class {
        return classified_error_response(
            class,
            codex_error_message(err),
            err.retry_after.as_deref(),
        );
    }
    let message = codex_error_message(err);
    if is_context_window_overflow(message) {
        return map_codex_failure_to_response(message);
    }

    match err.status {
        401 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.detail.as_deref().unwrap_or("Authentication failed"),
        ),
        // A token refresh that may succeed later; its fixed message names
        // Codex, so it bypasses the generic text `client_error` would choose.
        503 if err.origin == client::CodexErrorOrigin::Auth => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            client::TOKEN_REFRESH_UNAVAILABLE,
        ),
        403 => json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            err.detail.as_deref().unwrap_or("Permission denied"),
        ),
        429 => {
            let response = client_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &err.message,
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        // A 4xx the backend answered with describes the request, and its
        // text is what lets the user fix it.
        status @ (400..=499) if err.origin != client::CodexErrorOrigin::WebSocketHandshake => {
            json_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                "api_error",
                codex_error_message(err),
            )
        }
        status @ (400..=599) => {
            let response = client_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                if status == 529 {
                    "overloaded_error"
                } else {
                    "api_error"
                },
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        _ => client_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            codex_error_message(err),
        ),
    }
}

/// Answer a failure whose error code the proxy knows. The code decides over
/// the status it arrived with: a request the backend refused, or credits that
/// ran out, fail the same way on every retry, while a throttle or an overload
/// clears with time.
///
/// Credits or a spend limit that ran out are answered with
/// `x-should-retry: false` and no `Retry-After`, like a spent window, but
/// without the unified rate limit headers: no subscription window refused the
/// request, so none can be named. A `Retry-After` in seconds sent with any
/// other class is rounded up to whole seconds (`normalize_retry_after`).
fn classified_error_response(
    class: events::CodexErrorClass,
    native: &str,
    retry_after: Option<&str>,
) -> Response {
    let error_type = class.error_type();
    let native = if !native.trim().is_empty() {
        native
    } else if class == events::CodexErrorClass::ModelNotFound {
        // As the Codex CLI does, the code stands in for an empty message.
        "model_not_found"
    } else {
        generic_error_text(error_type)
    };
    if class == events::CodexErrorClass::ContextOverflow {
        return context_overflow_response(native);
    }
    let response = client_error(
        StatusCode::from_u16(class.status()).unwrap_or(StatusCode::BAD_GATEWAY),
        error_type,
        native,
    );
    let retry_after = match class {
        events::CodexErrorClass::QuotaExceeded => {
            return (
                [(
                    HeaderName::from_static("x-should-retry"),
                    HeaderValue::from_static("false"),
                )],
                response,
            )
                .into_response();
        }
        events::CodexErrorClass::Throttled => retry_after
            .and_then(events::normalize_retry_after)
            .or_else(|| events::retry_after_from_message(native)),
        events::CodexErrorClass::Overloaded => retry_after.and_then(events::normalize_retry_after),
        events::CodexErrorClass::UsageNotIncluded
        | events::CodexErrorClass::Rejected
        | events::CodexErrorClass::ContextOverflow
        | events::CodexErrorClass::ModelNotFound
        | events::CodexErrorClass::PermissionDenied => None,
    };
    match retry_after {
        Some(retry_after) => ([(http::header::RETRY_AFTER, retry_after)], response).into_response(),
        None => response,
    }
}

fn map_codex_failure_to_response(message: &str) -> Response {
    if is_context_window_overflow(message) {
        context_overflow_response(message)
    } else {
        client_error(StatusCode::BAD_GATEWAY, "api_error", message)
    }
}

/// A prompt that does not fit the context window, whether its code or its
/// message says so. The native text is kept as it came, never replaced by
/// `client_error`.
fn context_overflow_response(message: &str) -> Response {
    json_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", message)
}

/// An error for `/v1/messages`. The client expects Anthropic's API, so a text
/// that names this proxy's transport or its backend is replaced with
/// Anthropic's own wording for the error type. The native text travels beside
/// the body, where the request log and the monitor read it.
fn client_error(status: StatusCode, error_type: &'static str, native: &str) -> Response {
    let text = client_error_text(native, error_type);
    let mut response = json_error(status, error_type, text);
    if text != native {
        let outcome = ResponseOutcome::default();
        outcome.fail(native);
        response.extensions_mut().insert(outcome);
    }
    response
}

fn client_error_text<'a>(native: &'a str, error_type: &str) -> &'a str {
    let lower = native.to_ascii_lowercase();
    if !["codex", "websocket", "upstream"]
        .iter()
        .any(|name| lower.contains(name))
    {
        return native;
    }
    generic_error_text(error_type)
}

fn generic_error_text(error_type: &str) -> &'static str {
    match error_type {
        "overloaded_error" => "Overloaded",
        "rate_limit_error" => "Rate limited",
        "invalid_request_error" => "Invalid request",
        "permission_error" => "Permission denied",
        "request_too_large" => "Request too large",
        "not_found_error" => "Not found",
        _ => "Internal server error",
    }
}

fn is_context_window_overflow(message: &str) -> bool {
    message.to_ascii_lowercase().contains("context window")
}

fn codex_error_message(err: &client::CodexError) -> &str {
    if err.status == 0 {
        err.message.as_str()
    } else {
        err.detail.as_deref().unwrap_or("Upstream error")
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CodexCli;

impl CliHandlers for CodexCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` to create ~/.codex/auth.json; this proxy reads it directly."
        )
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` to create ~/.codex/auth.json; this proxy reads it directly."
        )
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let stored = store.load_auth()?;
        match stored {
            Some(auth) => {
                println!(
                    "Account: {}",
                    auth.account_id.as_deref().unwrap_or("(none)")
                );
                println!("{}", format_expiry(auth.expires, now_ms()));
                println!("Storage: {} (Codex CLI)", store.auth_path());
                Ok(())
            }
            None => anyhow::bail!(
                "No Codex credentials. Run `codex login` to create {}",
                store.auth_path()
            ),
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        println!(
            "Codex credentials are managed by the Codex CLI; run `codex logout` to remove them"
        );
        Ok(())
    }
}

pub(crate) static CODEX_CLI: CodexCli = CodexCli;

// ---------------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_expiry(expires: u64, now: u64) -> String {
    let remaining = (i128::from(expires) - i128::from(now)).div_euclid(1000);
    let iso = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires) * 1_000_000)
        .ok()
        .and_then(|dt| {
            let fmt = time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
            )
            .ok()?;
            dt.format(&fmt).ok()
        })
        .unwrap_or_else(|| "invalid".to_string());
    format!("Expires: {iso} (in {remaining}s)")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use http_body_util::BodyExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[test]
    fn compact_boundary_uses_http_without_rerouting_ordinary_requests() {
        use config::CodexTransport;

        for configured in [
            CodexTransport::WebSocket,
            CodexTransport::Auto,
            CodexTransport::Http,
        ] {
            assert_eq!(
                transport_for_request(configured, true),
                CodexTransport::Http
            );
            assert_eq!(transport_for_request(configured, false), configured);
        }
    }

    #[test]
    fn a_refusal_keeps_the_state_of_the_window_that_refused_it() {
        let _rate_limits = rate_limits::lock_for_tests();
        // The snapshot from an earlier turn says the allowance was fine.
        rate_limits::observe_event(
            None,
            &serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {
                    "primary": {"used_percent": 5.0, "window_minutes": 300, "resets_at": 1u64},
                    "secondary": {"used_percent": 5.0, "window_minutes": 10080, "resets_at": 2u64}
                }
            }),
        );

        let refusal = with_rate_limit_headers(
            usage_limit_response(&events::CodexUsageLimit {
                message: "The usage limit has been reached".to_string(),
                resets_at: Some(1788879437),
                window: Some(events::CodexLimitWindow::FiveHour),
                account: None,
                feeds_gauge: true,
            }),
            None,
        );

        let headers = refusal.headers();
        assert_eq!(
            headers
                .get("anthropic-ratelimit-unified-status")
                .and_then(|value| value.to_str().ok()),
            Some("rejected"),
        );
        assert_eq!(
            headers
                .get("anthropic-ratelimit-unified-reset")
                .and_then(|value| value.to_str().ok()),
            Some("1788879437"),
        );
    }

    fn classified_codex_error(
        class: events::CodexErrorClass,
        status: u16,
        message: &str,
        retry_after: Option<&str>,
    ) -> client::CodexError {
        client::CodexError {
            status,
            message: message.to_string(),
            detail: Some(message.to_string()),
            retry_after: retry_after.map(str::to_string),
            usage_limit: None,
            class: Some(class),
            origin: client::CodexErrorOrigin::BufferedHttp,
        }
    }

    async fn error_body(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn a_known_error_code_decides_the_status_and_type() {
        let cases = [
            (
                events::CodexErrorClass::QuotaExceeded,
                503,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
            ),
            (
                events::CodexErrorClass::UsageNotIncluded,
                429,
                StatusCode::FORBIDDEN,
                "permission_error",
            ),
            (
                events::CodexErrorClass::Rejected,
                500,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
            ),
            (
                events::CodexErrorClass::ContextOverflow,
                400,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
            ),
            (
                events::CodexErrorClass::Throttled,
                503,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
            ),
            (
                events::CodexErrorClass::Overloaded,
                503,
                StatusCode::from_u16(529).unwrap(),
                "overloaded_error",
            ),
            (
                events::CodexErrorClass::ModelNotFound,
                400,
                StatusCode::NOT_FOUND,
                "not_found_error",
            ),
            (
                events::CodexErrorClass::PermissionDenied,
                500,
                StatusCode::FORBIDDEN,
                "permission_error",
            ),
        ];
        for (class, backend_status, status, error_type) in cases {
            let response = map_codex_error_to_response(&classified_codex_error(
                class,
                backend_status,
                "toy backend message",
                None,
            ));
            assert_eq!(response.status(), status, "{class:?}");
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], error_type, "{class:?}");
            assert_eq!(body["error"]["message"], "toy backend message", "{class:?}");
        }
    }

    /// What a client gets for a failure event that reached it before any
    /// output, through the same classification the buffered paths use.
    fn response_to_failure_event(payload: serde_json::Value) -> Response {
        let failure = events::classify_event_failure(&payload).expect("a failure event");
        map_codex_error_to_response(&client::CodexError {
            status: failure.status,
            message: failure.message.clone(),
            detail: Some(failure.message),
            retry_after: failure.retry_after,
            usage_limit: None,
            class: failure.class,
            origin: client::CodexErrorOrigin::BufferedHttp,
        })
    }

    /// What a client gets for a non-success answer whose body is `body`, read
    /// the way the HTTP and buffered paths read it.
    fn response_to_error_body(status: u16, body: serde_json::Value) -> Response {
        map_codex_error_to_response(&client::codex_status_error(
            client::CodexResponse {
                body: serde_json::to_vec(&body).unwrap(),
                status,
                headers: Vec::new(),
                transport: client::ActualTransport::Http,
            },
            None,
        ))
    }

    #[tokio::test]
    async fn no_capacity_is_an_overload_with_only_the_delay_it_names() {
        let cases = [
            (serde_json::json!({}), None),
            (serde_json::json!({"status": 503}), None),
            (serde_json::json!({"retry_after": 1.5}), Some("2")),
            (
                serde_json::json!({"status": 503, "retry_after": 7}),
                Some("7"),
            ),
        ];
        for (extra, retry_after) in cases {
            let mut payload = serde_json::json!({
                "type": "error",
                "error": {"code": "policy_capacity_unavailable", "message": "toy capacity message"}
            });
            if let Some(status) = extra.get("status") {
                payload["status"] = status.clone();
            }
            if let Some(delay) = extra.get("retry_after") {
                payload["error"]["retry_after"] = delay.clone();
            }
            let response = response_to_failure_event(payload);
            assert_eq!(response.status().as_u16(), 529, "{extra}");
            assert_eq!(
                response
                    .headers()
                    .get(http::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()),
                retry_after,
                "{extra}"
            );
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], "overloaded_error", "{extra}");
            assert_eq!(body["error"]["message"], "toy capacity message", "{extra}");
        }
    }

    #[tokio::test]
    async fn a_missing_model_is_answered_as_not_found() {
        let cases = [
            (
                serde_json::json!({"type": "error", "error": {
                    "code": "model_not_found", "message": "toy model message"
                }}),
                "toy model message",
            ),
            (
                serde_json::json!({"type": "error", "status": 400, "error": {
                    "code": "model_not_found", "message": "toy model message"
                }}),
                "toy model message",
            ),
            (
                serde_json::json!({"type": "response.failed", "response": {"error": {
                    "type": "model_not_found"
                }}}),
                "model_not_found",
            ),
        ];
        for (payload, message) in cases {
            let response = response_to_failure_event(payload.clone());
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{payload}");
            assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], "not_found_error", "{payload}");
            assert_eq!(body["error"]["message"], message, "{payload}");
        }
        // An empty message from any other source is named by the code too.
        let response = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::ModelNotFound,
            404,
            "",
            None,
        ));
        assert_eq!(
            error_body(response).await["error"]["message"],
            "model_not_found"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn spent_credits_stop_the_retry_loop_without_naming_a_window() {
        let _rate_limits = rate_limits::lock_for_tests();
        // An earlier turn left a quota snapshot behind.
        rate_limits::observe_event(
            None,
            &serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {
                    "primary": {"used_percent": 5.0, "window_minutes": 300, "resets_at": 1u64}
                }
            }),
        );
        let response = with_rate_limit_headers(
            map_codex_error_to_response(&classified_codex_error(
                events::CodexErrorClass::QuotaExceeded,
                429,
                "You exceeded your current quota, please check your plan and billing details.",
                Some("30"),
            )),
            None,
        );

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = response.headers();
        assert_eq!(headers["x-should-retry"], "false");
        assert!(!headers.contains_key(http::header::RETRY_AFTER));
        for name in [
            "anthropic-ratelimit-unified-status",
            "anthropic-ratelimit-unified-reset",
            "anthropic-ratelimit-unified-representative-claim",
        ] {
            assert!(!headers.contains_key(name), "{name}");
        }
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(
            body["error"]["message"],
            "You exceeded your current quota, please check your plan and billing details."
        );
    }

    #[tokio::test]
    async fn a_failed_response_header_delay_wins_over_its_message() {
        let failure = events::classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {
                "code": "rate_limit_exceeded",
                "message": "Rate limit reached. Please try again in 20s.",
                "headers": {"retry-after": "2.5"}
            }}
        }))
        .unwrap();
        let response = map_codex_error_to_response(&classified_codex_error(
            failure.class.unwrap(),
            failure.status,
            &failure.message,
            failure.retry_after.as_deref(),
        ));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "3");
    }

    #[tokio::test]
    async fn a_failed_response_with_an_unusable_delay_takes_its_message_delay() {
        for retry_after in [
            format!("{}é", "a".repeat(3999)),
            "x".repeat(65),
            "\t".to_string(),
        ] {
            let failure = events::classify_event_failure(&serde_json::json!({
                "type": "response.failed",
                "response": {"error": {
                    "code": "rate_limit_exceeded",
                    "message": "Rate limit reached. Please try again in 20s.",
                    "retry_after": retry_after
                }}
            }))
            .unwrap();
            let response = map_codex_error_to_response(&classified_codex_error(
                failure.class.unwrap(),
                failure.status,
                &failure.message,
                failure.retry_after.as_deref(),
            ));
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()[http::header::RETRY_AFTER], "20");
        }
        // A value that reaches the mapping some other way is checked again.
        let response = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Overloaded,
            529,
            "busy",
            Some("é"),
        ));
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn a_throttle_names_its_delay_and_other_classes_do_not() {
        let from_field = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Throttled,
            429,
            "Rate limit exceeded.",
            Some("7"),
        ));
        assert_eq!(from_field.headers()[http::header::RETRY_AFTER], "7");
        assert!(!from_field.headers().contains_key("x-should-retry"));

        let from_message = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Throttled,
            503,
            "Rate limit reached. Please try again in 1.898s.",
            None,
        ));
        assert_eq!(from_message.headers()[http::header::RETRY_AFTER], "2");

        let without_delay = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Throttled,
            429,
            "Please slow down.",
            None,
        ));
        assert!(
            !without_delay
                .headers()
                .contains_key(http::header::RETRY_AFTER)
        );

        let overloaded = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Overloaded,
            503,
            "Please try again in 5s.",
            Some("1"),
        ));
        assert_eq!(overloaded.headers()[http::header::RETRY_AFTER], "1");

        for class in [
            events::CodexErrorClass::UsageNotIncluded,
            events::CodexErrorClass::Rejected,
            events::CodexErrorClass::ContextOverflow,
            events::CodexErrorClass::PermissionDenied,
        ] {
            let response = map_codex_error_to_response(&classified_codex_error(
                class,
                400,
                "Please try again in 5s.",
                Some("5"),
            ));
            assert!(
                !response.headers().contains_key(http::header::RETRY_AFTER),
                "{class:?}"
            );
            assert!(
                !response.headers().contains_key("x-should-retry"),
                "{class:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_prompt_without_text_gets_a_short_message() {
        let response = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Rejected,
            400,
            "",
            None,
        ));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = error_body(response).await;
        assert_eq!(body["error"]["message"], "Invalid request");

        // The live translator's placeholder names the backend, so it is
        // replaced in the same way.
        let response = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::Rejected,
            400,
            "Upstream error",
            None,
        ));
        let body = error_body(response).await;
        assert_eq!(body["error"]["message"], "Invalid request");
    }

    #[tokio::test]
    async fn a_terminal_code_is_answered_once_with_its_class() {
        let cases = [
            (
                "content_policy_violation",
                400,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The input was rejected by the content policy.",
            ),
            (
                "invalid_access_program",
                400,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The requested access program cannot be used with this model.",
            ),
            (
                "unsupported_access_program",
                400,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The requested access program cannot be used with this model.",
            ),
            (
                "inference_hook_denied",
                403,
                StatusCode::FORBIDDEN,
                "permission_error",
                "Your workspace inference hook blocked this request.",
            ),
            (
                "access_program_not_enabled",
                403,
                StatusCode::FORBIDDEN,
                "permission_error",
                "The requested access program is not enabled.",
            ),
        ];
        for (code, held, status, error_type, fallback) in cases {
            for event in [
                serde_json::json!({"type": "error", "error": {"code": code, "message": "toy message"}}),
                serde_json::json!({
                    "type": "error",
                    "status": held,
                    "error": {"code": code, "message": "toy message", "retry_after": 5}
                }),
            ] {
                let response = response_to_failure_event(event);
                assert_eq!(response.status(), status, "{code}");
                assert!(
                    !response.headers().contains_key(http::header::RETRY_AFTER),
                    "{code}"
                );
                assert!(!response.headers().contains_key("x-should-retry"), "{code}");
                let body = error_body(response).await;
                assert_eq!(body["error"]["type"], error_type, "{code}");
                assert_eq!(body["error"]["message"], "toy message", "{code}");
            }
            for message in ["", " "] {
                let response = response_to_failure_event(serde_json::json!({
                    "type": "error",
                    "error": {"code": code, "message": message}
                }));
                let body = error_body(response).await;
                assert_eq!(body["error"]["message"], fallback, "{code}");
            }
        }

        // Spent credits are answered like any other: never to be retried,
        // with no delay and no subscription window named.
        for event in [
            serde_json::json!({"type": "error", "error": {"code": "budget_reached", "message": "toy message"}}),
            serde_json::json!({
                "type": "error",
                "status": 400,
                "error": {"code": "budget_reached", "message": "toy message", "retry_after": 5}
            }),
        ] {
            let response = response_to_failure_event(event);
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()["x-should-retry"], "false");
            assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
            assert!(
                !response
                    .headers()
                    .keys()
                    .any(|name| name.as_str().starts_with("anthropic-ratelimit-unified"))
            );
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], "rate_limit_error");
            assert_eq!(body["error"]["message"], "toy message");
        }

        // An error body with a blank message takes the CLI's text for the
        // code as well.
        let response = response_to_error_body(
            403,
            serde_json::json!({"error": {"code": "inference_hook_denied", "message": ""}}),
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "permission_error");
        assert_eq!(
            body["error"]["message"],
            "Your workspace inference hook blocked this request."
        );
    }

    #[tokio::test]
    async fn a_terminal_code_in_an_error_body_without_a_message_gets_the_cli_text() {
        for error in [
            serde_json::json!({"code": "content_policy_violation", "message": ""}),
            serde_json::json!({"code": "content_policy_violation", "message": " "}),
            serde_json::json!({"code": "content_policy_violation"}),
        ] {
            let response = response_to_error_body(400, serde_json::json!({ "error": error }));
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{error}");
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], "invalid_request_error", "{error}");
            assert_eq!(
                body["error"]["message"], "The input was rejected by the content policy.",
                "{error}"
            );
        }

        // A message that is not blank is kept as it came.
        let response = response_to_error_body(
            400,
            serde_json::json!({"error": {"code": "content_policy_violation", "message": "toy message"}}),
        );
        let body = error_body(response).await;
        assert_eq!(body["error"]["message"], "toy message");

        // Spent credits have no text of their own and keep the quota text.
        for error in [
            serde_json::json!({"code": "budget_reached", "message": ""}),
            serde_json::json!({"code": "budget_reached"}),
        ] {
            let response = response_to_error_body(400, serde_json::json!({ "error": error }));
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{error}");
            assert_eq!(response.headers()["x-should-retry"], "false", "{error}");
            let body = error_body(response).await;
            assert_eq!(body["error"]["type"], "rate_limit_error", "{error}");
            assert_eq!(body["error"]["message"], "Rate limited", "{error}");
        }
    }

    #[test]
    fn a_classified_retry_after_is_whole_seconds() {
        let http_date = "Wed, 21 Oct 2026 07:28:00 GMT";
        let cases = [
            (events::CodexErrorClass::Throttled, "0.25", "1"),
            (events::CodexErrorClass::Throttled, "0", "1"),
            (events::CodexErrorClass::Throttled, "7", "7"),
            (events::CodexErrorClass::Throttled, http_date, http_date),
            (events::CodexErrorClass::Overloaded, "1.5", "2"),
            (events::CodexErrorClass::Overloaded, "0", "1"),
            (events::CodexErrorClass::Overloaded, http_date, http_date),
        ];
        for (class, sent, expected) in cases {
            let response = map_codex_error_to_response(&classified_codex_error(
                class,
                429,
                "toy backend message",
                Some(sent),
            ));
            assert_eq!(
                response.headers()[http::header::RETRY_AFTER],
                expected,
                "{class:?} {sent}"
            );
        }
    }

    #[tokio::test]
    async fn a_context_overflow_code_answers_like_the_context_window_message() {
        // The text names the backend, which `client_error` would replace.
        let native = "Codex error: your input exceeds the context window of this model.";
        let by_code = map_codex_error_to_response(&classified_codex_error(
            events::CodexErrorClass::ContextOverflow,
            400,
            native,
            None,
        ));
        let by_message = map_codex_failure_to_response(native);

        assert_eq!(by_code.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(by_code.status(), by_message.status());
        let by_code = error_body(by_code).await;
        assert_eq!(by_code["error"]["type"], "request_too_large");
        assert_eq!(by_code["error"]["message"], native);
        assert_eq!(by_code, error_body(by_message).await);
    }

    #[test]
    fn a_classified_stream_error_takes_the_type_of_its_class() {
        let err =
            classified_codex_error(events::CodexErrorClass::UsageNotIncluded, 429, "toy", None);
        assert_eq!(codex_stream_error_type(&err), "permission_error");
        let err = classified_codex_error(events::CodexErrorClass::Rejected, 529, "toy", None);
        assert_eq!(codex_stream_error_type(&err), "invalid_request_error");
    }

    fn live_test_request(text: &str) -> translate::request::ResponsesRequest {
        translate::request::ResponsesRequest {
            model: "gpt-5.6-sol".to_string(),
            instructions: None,
            input: vec![translate::request::ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![translate::request::ResponsesContentPart::InputText {
                    text: text.to_string(),
                }],
            }],
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: translate::request::ResponsesText {
                verbosity: None,
                format: None,
            },
            reasoning: None,
        }
    }

    fn live_test_context(session_id: &str) -> RequestContext {
        RequestContext {
            req_id: format!("request-{session_id}"),
            session_id: Some(session_id.to_string()),
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        }
    }

    fn authenticated_live_test_client(base_url: String) -> Arc<CodexHttpClient> {
        let client = CodexHttpClient::new_for_test(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            base_url,
            1_000,
            1_000,
            0,
        );
        client
            .auth_manager()
            .set_test_auth(auth::token_store::StoredAuth {
                access: "test".to_string(),
                refresh: String::new(),
                expires: u64::MAX,
                account_id: Some("acct".to_string()),
            });
        Arc::new(client)
    }

    #[tokio::test]
    async fn a_token_refresh_outage_answers_503_and_a_dead_login_401() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let dir = tempfile::tempdir().unwrap();
        for (refresh, status, error_type) in [
            // Nothing listens on port 1, so the refresh cannot reach the endpoint.
            ("refresh-test", StatusCode::SERVICE_UNAVAILABLE, "api_error"),
            // Without a refresh token only a new login helps.
            ("", StatusCode::UNAUTHORIZED, "authentication_error"),
        ] {
            let manager = auth::manager::CodexAuthManager::new_with_token_endpoint(
                auth::token_store::CodexTokenStore::codex_cli(
                    auth::token_store::CodexCliAuthStore::new(dir.path().join("auth.json")),
                ),
                "http://127.0.0.1:1/oauth/token".to_string(),
            );
            let client = CodexHttpClient::new_with_client(
                reqwest::Client::builder().no_proxy().build().unwrap(),
                manager,
                "http://127.0.0.1:1/responses".to_string(),
            );
            client
                .auth_manager()
                .set_test_auth(auth::token_store::StoredAuth {
                    access: "expired".to_string(),
                    refresh: refresh.to_string(),
                    expires: 0,
                    account_id: Some("acct".to_string()),
                });
            let provider = CodexProvider {
                client: Arc::new(client),
            };
            let body: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model": "gpt-5.6-luna",
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap();
            let response = provider
                .handle_messages_inner(body, live_test_context("token-refresh-outage"), None)
                .await;
            assert_eq!(response.status(), status, "refresh token {refresh:?}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], error_type);
            if status == StatusCode::SERVICE_UNAVAILABLE {
                assert_eq!(body["error"]["message"], client::TOKEN_REFRESH_UNAVAILABLE);
            }
        }
    }

    async fn next_live_websocket_request(
        websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    ) -> serde_json::Value {
        loop {
            match websocket.next().await {
                Some(Ok(Message::Ping(payload))) => {
                    websocket.send(Message::Pong(payload)).await.unwrap();
                }
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                other => panic!("unexpected WebSocket request frame: {other:?}"),
            }
        }
    }

    async fn emit_live_event(
        websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
        event: &serde_json::Value,
    ) {
        websocket
            .send(Message::Text(event.to_string()))
            .await
            .unwrap();
    }

    fn request_with_tools(tools: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": tools
        }))
        .unwrap()
    }

    #[test]
    fn web_search_requests_leave_lite_lane_and_upgrade_luna() {
        let body = request_with_tools(serde_json::json!([
            {"type":"web_search_20250305", "name":"web_search"}
        ]));
        for (resolved, expected) in [
            ("gpt-5.6-luna", "gpt-5.6-sol"),
            ("gpt-5.6-sol", "gpt-5.6-sol"),
            ("gpt-5.6-terra", "gpt-5.6-terra"),
            ("gpt-6-luna", "gpt-6-sol"),
            ("gpt-6-sol", "gpt-6-sol"),
            ("gpt-5.4", "gpt-5.4"),
        ] {
            let mut model = resolved.to_string();
            let lite = apply_model_lane_for_request(&mut model, &body);
            assert!(!lite, "{resolved} with web_search must use the full lane");
            assert_eq!(model, expected);
        }
    }

    const LANE_POLICY_VAR: &str = "CCP_CODEX_LANE_POLICY";

    /// Pins the lane policy for as long as it is held and restores the previous
    /// value on drop, unwinding included. `CCP_CODEX_LANE_POLICY` is the first
    /// source the resolver consults, so pinning it also overrides an inherited
    /// legacy `CCP_CODEX_FULL_LANE` and both config-file spellings.
    struct LanePolicyGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl LanePolicyGuard {
        fn pin(policy: &str) -> Self {
            let previous = std::env::var_os(LANE_POLICY_VAR);
            unsafe {
                std::env::set_var(LANE_POLICY_VAR, policy);
            }
            Self { previous }
        }
    }

    impl Drop for LanePolicyGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(LANE_POLICY_VAR, value),
                    None => std::env::remove_var(LANE_POLICY_VAR),
                }
            }
        }
    }

    #[test]
    fn requests_without_web_search_keep_model_and_lane() {
        let body = request_with_tools(serde_json::json!([
            {"name":"Bash", "input_schema":{}}
        ]));
        // With no listing remembered, the compiled-in lane table is what
        // `inventory` follows, so the expectations below are fixed.
        super::models::clear_discovered_models_for_tests();

        for (policy, expected) in [
            (
                "full",
                [
                    ("gpt-5.6-luna", false),
                    ("gpt-5.6-sol", false),
                    ("gpt-5.4", false),
                ],
            ),
            (
                "inventory",
                [
                    ("gpt-5.6-luna", true),
                    ("gpt-5.6-sol", true),
                    // Not every model is lite under `inventory`: the policy
                    // follows the inventory, it does not force the lite lane.
                    ("gpt-5.4", false),
                ],
            ),
        ] {
            let _lane = LanePolicyGuard::pin(policy);
            for (resolved, expected_lite) in expected {
                let mut model = resolved.to_string();
                let lite = apply_model_lane_for_request(&mut model, &body);
                assert_eq!(model, resolved, "model must not change without web_search");
                assert_eq!(
                    lite, expected_lite,
                    "{resolved} without web_search on the {policy} lane policy"
                );
            }
        }
    }

    #[test]
    fn generation_timing_ignores_control_events() {
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "codex.rate_limits"
        })));
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(codex_generation_event(&serde_json::json!({
            "type": "response.created"
        })));
    }

    fn monitored_live_context(req_id: &str) -> (crate::monitor::MonitorHandle, RequestContext) {
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(req_id, None, None, crate::monitor::EndpointKind::Messages);
        let ctx = RequestContext {
            req_id: req_id.to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: Some(monitor.clone()),
            passthrough: None,
        };
        (monitor, ctx)
    }

    /// Feed the translator one upstream event and report the chunk it produced,
    /// the way the live stream does.
    fn relay_live_event(
        ctx: &RequestContext,
        translator: &mut LiveStreamTranslator,
        payload: serde_json::Value,
    ) {
        let chunk = translator.accept(&payload, None).unwrap();
        if !chunk.is_empty() {
            record_live_stream_progress(ctx, translator, &chunk);
        }
    }

    #[test]
    fn live_stream_progress_reports_the_counts_the_backend_sent() {
        let (monitor, ctx) = monitored_live_context("request");
        let mut translator =
            LiveStreamTranslator::with_estimated_input_tokens("msg_1", "gpt-5.6-sol", 31_066);

        relay_live_event(
            &ctx,
            &mut translator,
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
        );
        // While the stream runs, the prompt count is this proxy's estimate.
        let streaming = monitor.snapshot();
        assert_eq!(streaming.active[0].input_tokens, Some(31_066));
        assert_eq!(
            streaming.active[0].usage_quality(),
            crate::monitor::QualityFields {
                input: crate::monitor::UsageQuality::Opening,
                cache_read: crate::monitor::UsageQuality::Missing,
                cache_write: crate::monitor::UsageQuality::Missing,
                output: crate::monitor::UsageQuality::Opening,
            }
        );

        relay_live_event(
            &ctx,
            &mut translator,
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "hi"
            }),
        );
        relay_live_event(
            &ctx,
            &mut translator,
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "usage": {
                        "input_tokens": 31_000,
                        "output_tokens": 117,
                        "input_tokens_details": {"cached_tokens": 28_160}
                    }
                }
            }),
        );

        let state = monitor.snapshot();
        let request = &state.active[0];
        assert_eq!(request.input_tokens, Some(2_840));
        assert_eq!(request.cache.read_tokens, Some(28_160));
        assert_eq!(request.output_tokens, Some(117));
        // The prompt is the total the backend measured, which is the two halves
        // above and nothing more.
        assert_eq!(request.cache.reported_prompt_tokens, Some(31_000));
        assert_eq!(request.prompt_tokens(), Some(31_000));
        // The backend reports no cache writes, so that count stays absent
        // instead of being measured as zero.
        assert_eq!(request.cache.write_tokens, None);
        assert_eq!(
            request.usage_quality(),
            crate::monitor::QualityFields {
                input: crate::monitor::UsageQuality::Exact,
                cache_read: crate::monitor::UsageQuality::Exact,
                cache_write: crate::monitor::UsageQuality::Missing,
                output: crate::monitor::UsageQuality::Exact,
            }
        );
        assert!(translator.emitted_terminal());
    }

    #[test]
    fn a_salvaged_tool_call_finish_reports_no_backend_counts() {
        let (monitor, ctx) = monitored_live_context("salvage");
        let mut translator =
            LiveStreamTranslator::with_estimated_input_tokens("msg_2", "gpt-5.6-sol", 12_345);

        for payload in [
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"command\":\"ls\"}"
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
            }),
        ] {
            relay_live_event(&ctx, &mut translator, payload);
        }

        // The socket dropped after a closed tool call: the finish is this
        // proxy's, and the zeros its `usage` object carries are not counts.
        let chunk = translator.finish_after_closed_completed_tool_call(None);
        assert!(String::from_utf8_lossy(&chunk).contains("\"input_tokens\":0"));
        record_live_stream_progress(&ctx, &mut translator, &chunk);

        let state = monitor.snapshot();
        let request = &state.active[0];
        assert_eq!(request.input_tokens, Some(12_345));
        assert_eq!(request.output_tokens, Some(0));
        assert_eq!(request.cache.read_tokens, None);
        assert_eq!(request.cache.write_tokens, None);
        assert_eq!(
            request.usage_quality(),
            crate::monitor::QualityFields {
                input: crate::monitor::UsageQuality::Opening,
                cache_read: crate::monitor::UsageQuality::Missing,
                cache_write: crate::monitor::UsageQuality::Missing,
                output: crate::monitor::UsageQuality::Opening,
            }
        );
        // The wire keeps its terminal finish, so the turn still completed.
        assert!(translator.emitted_terminal());
        assert_eq!(live_stream_outcome(&translator).failure_at_end(), None);
    }

    #[tokio::test]
    async fn live_stream_response_emits_downstream_frames_before_terminal_event() {
        use http_body_util::BodyExt as _;

        let body = request_with_tools(serde_json::json!([]));
        let request_body = translate_request(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
            },
        )
        .unwrap();
        let ctx = RequestContext {
            req_id: "incremental-http".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(Ok(serde_json::json!({"type": "keepalive"})))
            .await
            .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_up"}
        })))
        .await
        .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "first"
        })))
        .await
        .unwrap();

        let (rx, _) = websocket::CodexWebSocketEventStream::pending(rx);
        let continuation = ContinuationReservation::for_owner_turn(None, None);
        let response = match live_stream_response_once(
            rx,
            Vec::new(),
            "msg_test".to_string(),
            "claude-opus-4-8",
            ctx,
            continuation,
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            client::MAX_BUFFERED_RESPONSE_BYTES,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { error, .. } => panic!("unexpected retry: {error}"),
        };
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_millis(200), body.frame())
            .await
            .expect("initial downstream frame must be available immediately")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains("event: message_start"));
        assert!(first.contains("event: ping"));
        assert!(first.contains("event: content_block_start"));
        assert!(first.contains("event: content_block_delta"));

        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "second"
        })))
        .await
        .unwrap();
        let second = tokio::time::timeout(Duration::from_millis(200), body.frame())
            .await
            .expect("text delta must arrive before the terminal event")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert!(
            String::from_utf8(second.to_vec())
                .unwrap()
                .contains("event: content_block_delta")
        );

        for payload in [
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
        ] {
            tx.send(Ok(payload)).await.unwrap();
        }
        drop(tx);
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    }

    #[test]
    fn supported_models_includes_fast_variants() {
        let provider = CodexProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"gpt-5.6-sol".to_string()));
        assert!(models.contains(&"gpt-5.6-sol-fast".to_string()));
        assert!(models.contains(&"gpt-5.6-terra".to_string()));
        assert!(models.contains(&"gpt-5.6-luna".to_string()));
        assert!(models.contains(&"gpt-6-sol".to_string()));
        assert!(models.contains(&"gpt-6-sol-fast".to_string()));
        assert!(models.contains(&"gpt-6-luna".to_string()));
        assert!(models.contains(&"gpt-5.4".to_string()));
        assert!(models.contains(&"gpt-5.4-mini".to_string()));
    }

    #[test]
    fn format_expiry_with_future_expiry() {
        // 2100-01-01T00:00:00Z in ms
        let expires = 4102444800000;
        let now = 4102444790000; // 10s before
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2100-01-01T00:00:00.000Z (in "));
        assert!(output.ends_with("s)"));
    }

    #[test]
    fn format_expiry_with_past_expiry() {
        // 2000-01-01T00:00:00Z in ms
        let expires = 946684800000;
        let now = 946684810000; // 10s after
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2000-01-01T00:00:00.000Z (in -"));
    }

    #[tokio::test]
    async fn live_upstream_status_and_retry_after_are_preserved() {
        let err = client::CodexError {
            status: 422,
            message: "invalid request".to_string(),
            detail: Some("invalid request".to_string()),
            retry_after: Some("7".to_string()),
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };
        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response.headers().get(http::header::RETRY_AFTER).unwrap(),
            "7"
        );
    }

    #[tokio::test]
    async fn a_size_refusal_names_another_transport_only_under_websocket() {
        for detail in [
            websocket::WEBSOCKET_MESSAGE_TOO_LARGE_DETAIL,
            websocket::WEBSOCKET_CLOSED_MESSAGE_TOO_LARGE_DETAIL,
        ] {
            let err = client::CodexError {
                status: 413,
                message: "The WebSocket request was refused for its size".to_string(),
                detail: Some(detail.to_string()),
                retry_after: None,
                usage_limit: None,
                class: None,
                origin: client::CodexErrorOrigin::WebSocket,
            };
            for (transport, expected) in [
                (
                    Some(config::CodexTransport::WebSocket),
                    "The WebSocket request was refused for its size. The auto transport sends such a request over HTTP.",
                ),
                (
                    Some(config::CodexTransport::Auto),
                    "The WebSocket request was refused for its size",
                ),
                (None, "The WebSocket request was refused for its size"),
            ] {
                let response = match transport {
                    Some(transport) => map_codex_transport_error_to_response(&err, transport),
                    None => map_codex_error_to_response(&err),
                };
                assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{detail}");
                let body = error_body(response).await;
                assert_eq!(body["error"]["type"], "request_too_large", "{detail}");
                assert_eq!(
                    body["error"]["message"], expected,
                    "{detail}, {transport:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn statusless_codex_error_is_worded_for_the_client_and_keeps_its_source() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect error: HTTP error: 502 Bad Gateway".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .extensions()
                .get::<ResponseOutcome>()
                .and_then(ResponseOutcome::failure)
                .as_deref(),
            Some("WebSocket connect error: HTTP error: 502 Bad Gateway")
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body.pointer("/error/type").and_then(|v| v.as_str()),
            Some("api_error")
        );
        assert_eq!(
            body.pointer("/error/message").and_then(|v| v.as_str()),
            Some("Internal server error")
        );
    }

    #[tokio::test]
    async fn backend_request_error_keeps_its_own_text() {
        let detail = "Invalid tool name: codex_search";
        let err = client::CodexError {
            status: 400,
            message: detail.to_string(),
            detail: Some(detail.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::BufferedHttp,
        };

        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body.pointer("/error/message").and_then(|v| v.as_str()),
            Some(detail)
        );
    }

    #[test]
    fn live_start_statusless_websocket_handshake_error_is_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(retryable_live_start_codex_error(&err));
    }

    #[test]
    fn live_start_proxy_tunnel_rejection_is_not_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket proxy tunnel was rejected".to_string(),
            detail: Some(websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(!retryable_live_start_codex_error(&err));
    }

    #[test]
    fn live_start_keepalive_failure_is_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket keepalive error: test write failed".to_string(),
            detail: Some(websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        assert!(retryable_live_start_codex_error(&err));
    }

    #[test]
    fn failed_request_log_detail_is_cut_to_300_characters() {
        let err = |detail: Option<String>| client::CodexError {
            status: 502,
            message: "Upstream Codex request failed".to_string(),
            detail,
            retry_after: None,
            usage_limit: None,
            class: None,
            origin: client::CodexErrorOrigin::Http,
        };

        assert_eq!(failed_request_log_detail(&err(None)), None);
        assert_eq!(
            failed_request_log_detail(&err(Some("http_response_body".to_string()))).as_deref(),
            Some("http_response_body")
        );
        // Two bytes per character, so a cut by bytes would land mid-character.
        let long = "é".repeat(FAILED_REQUEST_LOG_DETAIL_CHARS + 1);
        let logged = failed_request_log_detail(&err(Some(long.clone()))).unwrap();
        assert_eq!(logged.chars().count(), 300);
        assert!(long.starts_with(&logged));
        let exact = "é".repeat(FAILED_REQUEST_LOG_DETAIL_CHARS);
        assert_eq!(
            failed_request_log_detail(&err(Some(exact.clone()))),
            Some(exact)
        );
        assert_eq!(
            serde_json::json!(failed_request_log_detail(&err(None))),
            serde_json::Value::Null
        );
    }

    #[test]
    fn live_start_payload_retry_detection_covers_rate_limit_and_overload() {
        // A terminal snapshot is a spent window, answered once and never retried.
        assert!(!retryable_live_start_payload(
            &serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": true}
            }),
            "rate limit reached",
        ));
        assert!(retryable_live_start_payload(
            &serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"type": "overloaded_error", "message": "overloaded"}}
            }),
            "overloaded",
        ));
        assert!(!retryable_live_start_payload(
            &serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"message": "bad request"}}
            }),
            "bad request",
        ));
    }

    async fn run_live_failure_case(
        session_id: &str,
        event: serde_json::Value,
        expected_attempts: usize,
    ) -> Response {
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request.model);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..expected_attempts {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_live_websocket_request(&mut websocket).await;
                emit_live_event(&mut websocket, &event).await;
                drop(websocket);
            }
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                continuation.clone(),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("live failure case timed out");
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("live failure server timed out")
            .expect("live failure server failed");

        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        response
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn usage_limit_fast_fails_websocket_and_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        let response = run_live_failure_case(
            "live-usage-limit-cleanup",
            serde_json::json!({
                "type": "error",
                "status_code": 429,
                "error": {
                    "type": "usage_limit_reached",
                    "message": "The usage limit has been reached",
                    "resets_at": 1788879437u64,
                    "resets_in_seconds": 9568
                },
                "headers": {
                    "X-Codex-Primary-Window-Minutes": "300",
                    "X-Codex-Primary-Reset-After-Seconds": "9569",
                    "X-Codex-Secondary-Window-Minutes": "10080",
                    "X-Codex-Secondary-Reset-After-Seconds": "596369"
                }
            }),
            1,
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-status"],
            "rejected"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-reset"],
            "1788879437"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-representative-claim"],
            "five_hour"
        );
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "The usage limit has been reached");
    }

    #[tokio::test]
    async fn spent_credits_fail_a_live_start_once_and_abort_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let response = run_live_failure_case(
            "live-quota-cleanup",
            serde_json::json!({
                "type": "response.failed",
                "response": {"error": {
                    "code": "credit_balance_exhausted",
                    "message": "Your credit balance is exhausted."
                }}
            }),
            1,
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert!(
            !response
                .headers()
                .contains_key("anthropic-ratelimit-unified-status")
        );
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(
            body["error"]["message"],
            "Your credit balance is exhausted."
        );
    }

    #[tokio::test]
    async fn a_throttled_live_start_is_answered_with_its_delay() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let response = run_live_failure_case(
            "live-throttle-cleanup",
            serde_json::json!({
                "type": "error",
                "status": 503,
                "error": {
                    "code": "slow_down",
                    "message": "Please slow down. Try again in 3 seconds."
                }
            }),
            1,
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "3");
        assert!(!response.headers().contains_key("x-should-retry"));
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn failed_live_attempt_is_reported_without_backoff() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        let session_id = "live-retry-backoff-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request.model);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (event_sent_tx, event_sent_rx) = tokio::sync::oneshot::channel();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {"allowed": false, "limit_reached": true}
                }),
            )
            .await;
            event_sent_tx.send(()).unwrap();
            drop(websocket);
            socket_closed_tx.send(()).unwrap();
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        let task_request = request.clone();
        let task_continuation = continuation.clone();
        let response_task = tokio::spawn(async move {
            let model = task_request.model.clone();
            live_stream_response(
                client,
                "message".to_string(),
                &model,
                live_test_context(session_id),
                task_request,
                task_continuation,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
            )
            .await
        });

        event_sent_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), socket_closed_rx)
            .await
            .expect("the failed attempt socket was not closed")
            .expect("socket-close sender dropped");
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), response_task)
            .await
            .expect("a failed attempt must be reported without waiting in backoff")
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        server.await.unwrap();
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn dropping_live_response_body_after_first_chunk_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let session_id = "live-response-body-drop-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request.model);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg-partial"}
                }),
            )
            .await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "partial"
                }),
            )
            .await;
            while websocket.next().await.is_some() {}
            socket_closed_tx.send(()).unwrap();
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                continuation.clone(),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("live response did not publish the first chunk");
        let mut body = response.into_body();
        tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .expect("first downstream chunk timed out")
            .expect("live response body ended before the first chunk")
            .expect("first downstream chunk failed");
        drop(body);

        tokio::time::timeout(std::time::Duration::from_secs(1), socket_closed_rx)
            .await
            .expect("dropping the downstream body did not close the upstream socket")
            .expect("socket-close acknowledgement sender dropped");
        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        server.await.unwrap();
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    /// A plain 401 on the live WebSocket refreshes the login, and the one it
    /// reloads may belong to another account. A reading the first attempt
    /// queued before the refresh stays with the account that read it, so the
    /// response, sent with the reloaded account, publishes none of its windows.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_queued_reading_stays_with_the_account_that_read_it() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();
        let session_id = "live-queued-reading-account";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let login = |access: &str, account: &str| auth::token_store::StoredAuth {
            access: access.to_string(),
            refresh: String::new(),
            expires: u64::MAX,
            account_id: Some(account.to_string()),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        client
            .auth_manager()
            .set_test_auth(login("token-a", "acct-a"));
        let rotating_client = client.clone();
        let rotated = login("token-b", "acct-b");
        let (resent_tx, resent_rx) = tokio::sync::oneshot::channel();
        let (answer_tx, answer_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary": {
                            "used_percent": 40.0,
                            "window_minutes": 300,
                            "reset_at": now + 3_600
                        },
                        "secondary": {
                            "used_percent": 20.0,
                            "window_minutes": 10080,
                            "reset_at": now + 86_400
                        }
                    }
                }),
            )
            .await;
            rotating_client.auth_manager().set_test_auth(rotated);
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "error",
                    "status": 401,
                    "error": {"message": "Your token has expired."}
                }),
            )
            .await;
            drop(websocket);

            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            resent_tx.send(()).unwrap();
            answer_rx.await.unwrap();
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg-reloaded"}
                }),
            )
            .await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "answer"
                }),
            )
            .await;
            while websocket.next().await.is_some() {}
        });

        let ctx = live_test_context(session_id);
        let events = client
            .stream_codex_websocket_events_for_owner(&request, &ctx, Some(&continuation))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), resent_rx)
            .await
            .expect("the request was not sent again after the refresh")
            .unwrap();
        // Nothing has read the queue yet: the first attempt's reading waits in
        // it while the stream already stands for the reloaded account.
        assert_eq!(events.account_id().as_deref(), Some("acct-b"));
        answer_tx.send(()).unwrap();
        let account_id = events.account_reader();
        let start = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            live_stream_response_once(
                events,
                Vec::new(),
                "message".to_string(),
                &request.model,
                ctx,
                continuation.clone(),
                request.clone(),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                client::MAX_BUFFERED_RESPONSE_BYTES,
            ),
        )
        .await
        .expect("the live response did not start");
        let LiveStreamStart::Response(response) = start else {
            panic!("the reloaded account's answer must start the response");
        };
        let response = with_rate_limit_headers(response, account_id().as_deref());

        assert!(
            response
                .headers()
                .keys()
                .all(|name| !name.as_str().starts_with("anthropic-ratelimit-unified")),
            "{:?}",
            response.headers()
        );
        let held = rate_limits::latest(Some("acct-a")).expect("the reading keeps its account");
        assert_eq!(
            held.five_hour.map(|window| window.resets_at),
            Some(now + 3_600)
        );
        assert_eq!(
            held.seven_day.map(|window| window.resets_at),
            Some(now + 86_400)
        );
        assert!(rate_limits::latest(Some("acct-b")).is_none());
        drop(response);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("dropping the response did not close the upstream socket")
            .unwrap();
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        continuation::clear_continuation_for_owner(Some(&owner));
        rate_limits::clear_for_tests();
    }

    fn test_login(access: &str, account: &str) -> auth::token_store::StoredAuth {
        auth::token_store::StoredAuth {
            access: access.to_string(),
            refresh: String::new(),
            expires: u64::MAX,
            account_id: Some(account.to_string()),
        }
    }

    /// A login whose account id is unknown.
    fn test_login_without_account(access: &str) -> auth::token_store::StoredAuth {
        auth::token_store::StoredAuth {
            account_id: None,
            ..test_login(access, "")
        }
    }

    /// What the mock backend of `second_turn_after_saving_login` saw: the
    /// `ChatGPT-Account-Id` and the bearer token of each WebSocket handshake,
    /// and each request with the index of the socket it came on.
    struct SeenUpstream {
        handshakes: Vec<Option<String>>,
        handshake_tokens: Vec<Option<String>>,
        requests: Vec<(usize, serde_json::Value)>,
    }

    /// Two turns of one conversation on the live WebSocket: the first under
    /// `first_login`, the second after `second_login` is saved. The mock
    /// backend answers a request under the account of the socket it came on,
    /// with a reading of that account's windows for `acct-a` and none for
    /// another. Returns the second turn's response and what the backend saw.
    // tungstenite's own callback type fixes the error response size.
    #[allow(clippy::result_large_err)]
    async fn second_turn_after_saving_login(
        session_id: &str,
        first_login: auth::token_store::StoredAuth,
        second_login: auth::token_store::StoredAuth,
    ) -> (Response, SeenUpstream) {
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reading = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "used_percent": 40.0,
                    "window_minutes": 300,
                    "reset_at": now + 3_600
                },
                "secondary": {
                    "used_percent": 20.0,
                    "window_minutes": 10080,
                    "reset_at": now + 86_400
                }
            }
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (handshake_tx, mut handshake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for socket_index in 0_usize..usize::MAX {
                let (socket, _) = listener.accept().await.unwrap();
                let mut account: Option<String> = None;
                let mut token: Option<String> = None;
                let mut websocket = tokio_tungstenite::accept_hdr_async(
                    socket,
                    |request: &http::Request<()>, response| {
                        account = request
                            .headers()
                            .get("chatgpt-account-id")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        token = request
                            .headers()
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.strip_prefix("Bearer "))
                            .map(str::to_owned);
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                handshake_tx.send((account.clone(), token)).unwrap();
                let request_tx = request_tx.clone();
                let reading = reading.clone();
                tokio::spawn(async move {
                    let mut answered = 0;
                    while let Some(Ok(message)) = websocket.next().await {
                        match message {
                            Message::Ping(payload) => {
                                let _ = websocket.send(Message::Pong(payload)).await;
                            }
                            Message::Text(text) => {
                                let request: serde_json::Value =
                                    serde_json::from_str(&text).unwrap();
                                request_tx.send((socket_index, request)).unwrap();
                                answered += 1;
                                if account.as_deref() == Some("acct-a") {
                                    emit_live_event(&mut websocket, &reading).await;
                                }
                                emit_live_event(
                                    &mut websocket,
                                    &serde_json::json!({
                                        "type": "response.completed",
                                        "response": {
                                            "id": format!("resp-{socket_index}-{answered}"),
                                            "status": "completed",
                                            "output": []
                                        }
                                    }),
                                )
                                .await;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        client.auth_manager().set_test_auth(first_login);
        let compaction = LiveStreamCompaction {
            compact_boundary: false,
            attempt: None,
        };
        let first = live_test_request("one");
        let first_turn = continuation_candidate_for_owner(Some(&owner), &first, true);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client.clone(),
                "message".to_string(),
                &first.model,
                live_test_context(session_id),
                first.clone(),
                first_turn,
                compaction,
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the first turn did not finish");
        assert_eq!(response.status(), StatusCode::OK);

        client.auth_manager().set_test_auth(second_login);
        let mut second = live_test_request("one");
        second.input.extend(live_test_request("two").input);
        let second_turn = continuation_candidate_for_owner(Some(&owner), &second, true);
        // The second turn only appends, so its continuation names the first response.
        assert_eq!(
            second_turn.candidate().previous_response_id.as_deref(),
            Some("resp-0-1")
        );
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client,
                "message".to_string(),
                &second.model,
                live_test_context(session_id),
                second.clone(),
                second_turn,
                compaction,
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the second turn did not finish");
        server.abort();

        let mut seen = SeenUpstream {
            handshakes: Vec::new(),
            handshake_tokens: Vec::new(),
            requests: Vec::new(),
        };
        while let Ok((account, token)) = handshake_rx.try_recv() {
            seen.handshakes.push(account);
            seen.handshake_tokens.push(token);
        }
        while let Ok(request) = request_rx.try_recv() {
            seen.requests.push(request);
        }
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        continuation::clear_continuation_for_owner(Some(&owner));
        (response, seen)
    }

    /// A pooled socket answers under the account of its handshake. Once the
    /// saved login belongs to another account, the conversation's next turn
    /// does not go out on it: a socket of its own is opened with the new
    /// account, the turn is sent with its full context, and the response
    /// carries none of the old account's windows.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_pooled_socket_is_not_reused_under_another_account() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();

        let (response, seen) = second_turn_after_saving_login(
            "live-pooled-socket-other-account",
            test_login("token-a", "acct-a"),
            test_login("token-b", "acct-b"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().keys().all(|name| {
                !name.as_str().starts_with("anthropic-ratelimit-unified-5h-")
                    && !name.as_str().starts_with("anthropic-ratelimit-unified-7d-")
            }),
            "{:?}",
            response.headers()
        );
        assert_eq!(
            seen.handshakes,
            [Some("acct-a".to_string()), Some("acct-b".to_string())]
        );
        let sockets: Vec<usize> = seen.requests.iter().map(|(socket, _)| *socket).collect();
        assert_eq!(
            sockets,
            [0, 1],
            "nothing more may go out on acct-a's socket"
        );
        let resent = &seen.requests[1].1;
        assert!(resent.get("previous_response_id").is_none());
        assert_eq!(resent["input"].as_array().unwrap().len(), 2);
        assert!(rate_limits::latest(Some("acct-b")).is_none());
        rate_limits::clear_for_tests();
    }

    /// A refreshed token of the same account keeps the conversation on its
    /// pooled socket: the next turn sends only what it adds, and no second
    /// handshake is made.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_pooled_socket_is_reused_under_the_same_account() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();

        let (response, seen) = second_turn_after_saving_login(
            "live-pooled-socket-same-account",
            test_login("token-a", "acct-a"),
            test_login("token-a-refreshed", "acct-a"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key("anthropic-ratelimit-unified-5h-utilization"),
            "{:?}",
            response.headers()
        );
        assert_eq!(seen.handshakes, [Some("acct-a".to_string())]);
        let sockets: Vec<usize> = seen.requests.iter().map(|(socket, _)| *socket).collect();
        assert_eq!(sockets, [0, 0]);
        let continued = &seen.requests[1].1;
        assert_eq!(continued["previous_response_id"], "resp-0-1");
        assert_eq!(continued["input"].as_array().unwrap().len(), 1);
        rate_limits::clear_for_tests();
    }

    /// A socket whose handshake named no account answers under the login its
    /// token belongs to. Once the saved login has another token, still with
    /// no account, the next turn does not go out on it: a socket of its own
    /// is opened with the new token and the turn is sent with its full
    /// context.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_pooled_socket_without_an_account_is_not_reused_under_another_token() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();

        let (response, seen) = second_turn_after_saving_login(
            "live-pooled-socket-no-account-other-token",
            test_login_without_account("token-a"),
            test_login_without_account("token-b"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.handshakes, [None, None]);
        assert_eq!(
            seen.handshake_tokens,
            [Some("token-a".to_string()), Some("token-b".to_string())]
        );
        let sockets: Vec<usize> = seen.requests.iter().map(|(socket, _)| *socket).collect();
        assert_eq!(
            sockets,
            [0, 1],
            "nothing more may go out on token-a's socket"
        );
        let resent = &seen.requests[1].1;
        assert!(resent.get("previous_response_id").is_none());
        assert_eq!(resent["input"].as_array().unwrap().len(), 2);
        rate_limits::clear_for_tests();
    }

    /// With no account on either side, the same token keeps the conversation
    /// on its pooled socket.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_pooled_socket_without_an_account_is_reused_under_the_same_token() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();

        let (response, seen) = second_turn_after_saving_login(
            "live-pooled-socket-no-account-same-token",
            test_login_without_account("token-a"),
            test_login_without_account("token-a"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.handshakes, [None]);
        assert_eq!(seen.handshake_tokens, [Some("token-a".to_string())]);
        let sockets: Vec<usize> = seen.requests.iter().map(|(socket, _)| *socket).collect();
        assert_eq!(sockets, [0, 0]);
        let continued = &seen.requests[1].1;
        assert_eq!(continued["previous_response_id"], "resp-0-1");
        assert_eq!(continued["input"].as_array().unwrap().len(), 1);
        rate_limits::clear_for_tests();
    }

    #[test]
    fn a_live_upstream_copy_is_released_once_by_the_event_that_passes_its_limit() {
        let event = serde_json::json!({"type": "response.created", "response": {"id": "resp"}});
        let encoded = format!("data: {event}\n\n").into_bytes();
        let mut copy = LiveUpstreamCopy::new(encoded.len() * 2);

        assert!(!copy.append("request-copy", &event));
        assert!(!copy.append("request-copy", &event));
        assert_eq!(copy.body, Some([encoded.clone(), encoded].concat()));
        assert!(copy.append("request-copy", &event));
        assert!(copy.body.is_none());
        assert!(!copy.append("request-copy", &event));
        assert!(copy.body.is_none());
    }

    /// Compact JSON holds no line break, so an event takes one `data:` line
    /// followed by a blank one.
    #[test]
    fn an_upstream_event_is_framed_as_one_compact_data_line() {
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "one\ntwo\r\nthree\n\n"
        });
        let text = event.to_string();
        let mut by_lines = Vec::new();
        for line in text.lines() {
            by_lines.extend_from_slice(b"data: ");
            by_lines.extend_from_slice(line.as_bytes());
            by_lines.push(b'\n');
        }
        by_lines.push(b'\n');

        let mut framed = Vec::new();
        assert!(append_upstream_sse_payload(&mut framed, &event, usize::MAX));
        assert_eq!(framed, by_lines);
        assert_eq!(framed, format!("data: {text}\n\n").into_bytes());
    }

    #[test]
    fn an_upstream_event_past_the_room_leaves_the_copy_as_it_was() {
        let held = b"data: {\"type\":\"response.created\"}\n\n".to_vec();
        let event = serde_json::json!({"type": "response.output_text.delta", "delta": "abc"});
        let size = format!("data: {event}\n\n").len();

        let mut copy = held.clone();
        assert!(!append_upstream_sse_payload(&mut copy, &event, size - 1));
        assert_eq!(copy, held);
        assert!(append_upstream_sse_payload(&mut copy, &event, size));
        assert_eq!(copy.len(), held.len() + size);

        let large = serde_json::json!({"delta": "x".repeat(1024 * 1024)});
        let mut copy = held.clone();
        assert!(!append_upstream_sse_payload(&mut copy, &large, 64));
        assert_eq!(copy, held);
        assert!(copy.capacity() < 1024, "{}", copy.capacity());
    }

    #[test]
    fn the_copy_dropped_warning_names_only_the_request_and_the_limit() {
        assert_eq!(
            serde_json::Value::Object(live_stream_copy_dropped_fields("request-copy", 4096)),
            serde_json::json!({"reqId": "request-copy", "limitBytes": 4096})
        );
    }

    /// A live stream whose copy went past the limit still reaches the client
    /// whole, but leaves no continuation state: the next turn sends its full
    /// context without `previous_response_id`.
    #[tokio::test]
    async fn a_live_stream_over_the_copy_limit_relays_whole_and_leaves_no_continuation() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let session_id = "live-copy-over-limit";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);

        fn copy_limit_text() -> String {
            format!("relayed in full {}", "y".repeat(1024))
        }

        fn copy_limit_events(id: &str) -> Vec<serde_json::Value> {
            let text = copy_limit_text();
            vec![
                serde_json::json!({
                    "type": "response.created",
                    "response": {"id": id}
                }),
                serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg-copy"}
                }),
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": text
                }),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg-copy"}
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": id,
                        "status": "completed",
                        "output": []
                    }
                }),
            ]
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let request_tx = request_tx.clone();
                tokio::spawn(async move {
                    let mut answered = 0;
                    while let Some(Ok(message)) = websocket.next().await {
                        match message {
                            Message::Ping(payload) => {
                                let _ = websocket.send(Message::Pong(payload)).await;
                            }
                            Message::Text(text) => {
                                let request: serde_json::Value =
                                    serde_json::from_str(&text).unwrap();
                                request_tx.send(request).unwrap();
                                answered += 1;
                                let id = format!("resp-copy-{answered}");
                                for event in copy_limit_events(&id) {
                                    emit_live_event(&mut websocket, &event).await;
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        // One byte short of the answer's JSON, so the copy, which adds SSE
        // framing to it, is dropped. The translator holds only the message id
        // and the text, and before output only the message start, all well
        // within it, so the stream itself is relayed whole.
        let answer_bytes: usize = copy_limit_events("resp-copy-1")
            .iter()
            .map(|event| serde_json::to_vec(event).unwrap().len())
            .sum();
        let client = Arc::new(
            Arc::into_inner(authenticated_live_test_client(format!(
                "http://{addr}/responses"
            )))
            .unwrap()
            .with_buffered_response_limit(answer_bytes - 1),
        );
        let compaction = LiveStreamCompaction {
            compact_boundary: false,
            attempt: None,
        };
        let first = live_test_request("one");
        let first_turn = continuation_candidate_for_owner(Some(&owner), &first, true);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client.clone(),
                "message".to_string(),
                &first.model,
                live_test_context(session_id),
                first.clone(),
                first_turn,
                compaction,
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the first turn did not start");
        assert_eq!(response.status(), StatusCode::OK);
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("the first turn did not finish")
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let relayed_text: String = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
            })
            .map(|event| event["delta"]["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(relayed_text, copy_limit_text(), "{body}");
        assert!(body.contains("message_stop"), "{body}");
        assert!(!continuation::has_continuation_for_owner_for_tests(&owner));

        let mut second = live_test_request("one");
        second.input.extend(live_test_request("two").input);
        let second_turn = continuation_candidate_for_owner(Some(&owner), &second, true);
        assert!(second_turn.candidate().previous_response_id.is_none());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client,
                "message".to_string(),
                &second.model,
                live_test_context(session_id),
                second.clone(),
                second_turn,
                compaction,
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the second turn did not start");
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("the second turn did not finish")
        .unwrap();
        server.abort();

        let _first_sent = request_rx.recv().await.unwrap();
        let second_sent = request_rx.recv().await.unwrap();
        assert!(second_sent.get("previous_response_id").is_none());
        assert_eq!(second_sent["input"].as_array().unwrap().len(), 2);
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    /// The size limit the refused-stream tests give the client: above the
    /// message start held before output, below what each test sends.
    const HELD_LIMIT: usize = 4096;

    /// What a live turn the proxy refused left behind.
    struct RefusedLiveStream {
        status: StatusCode,
        body: String,
        requests: usize,
        turn_current: bool,
        continuation_kept: bool,
        socket_pooled: bool,
    }

    /// Runs one live WebSocket turn under `HELD_LIMIT` against a backend that
    /// answers with `response.created`, then `events`, then a completed
    /// response, and returns what the turn left once its socket has closed.
    /// A socket that stays open was pooled, and fails the wait.
    async fn refused_live_websocket_turn(
        session_id: &str,
        events: Vec<serde_json::Value>,
    ) -> RefusedLiveStream {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let request_tx = request_tx.clone();
                let closed_tx = closed_tx.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    while let Some(Ok(message)) = websocket.next().await {
                        match message {
                            Message::Ping(payload) => {
                                let _ = websocket.send(Message::Pong(payload)).await;
                            }
                            Message::Text(_) => {
                                request_tx.send(()).unwrap();
                                let created = serde_json::json!({
                                    "type": "response.created",
                                    "response": {"id": "resp-refused"}
                                });
                                let completed = serde_json::json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": "resp-refused",
                                        "status": "completed",
                                        "output": []
                                    }
                                });
                                for event in std::iter::once(&created)
                                    .chain(&events)
                                    .chain(std::iter::once(&completed))
                                {
                                    // The proxy may close the socket while the
                                    // answer is still being written.
                                    let _ = websocket.send(Message::Text(event.to_string())).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    let _ = closed_tx.send(());
                });
            }
        });

        let client = Arc::new(
            Arc::into_inner(authenticated_live_test_client(format!(
                "http://{addr}/responses"
            )))
            .unwrap()
            .with_buffered_response_limit(HELD_LIMIT),
        );
        let request = live_test_request("refused");
        let turn = continuation_candidate_for_owner(Some(&owner), &request, true);
        let turn_after = turn.clone();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                turn,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
            ),
        )
        .await
        .expect("the turn did not start");
        let status = response.status();
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("the turn did not finish")
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), closed_rx.recv())
            .await
            .expect("the refused turn's socket stayed open");
        server.abort();
        let mut requests = 0;
        while request_rx.try_recv().is_ok() {
            requests += 1;
        }

        let refused = RefusedLiveStream {
            status,
            body: String::from_utf8(body.to_vec()).unwrap(),
            requests,
            turn_current: continuation::is_current_turn_for_owner(&turn_after),
            continuation_kept: continuation::has_continuation_for_owner_for_tests(&owner),
            socket_pooled: websocket::pooled_socket_id_for_tests(&owner).is_some(),
        };
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        refused
    }

    /// The events that start output before each refused-after-output case.
    fn output_before_the_limit() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg-held"}
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "relayed before the limit"
            }),
        ]
    }

    /// Output that started is ended with an SSE error event, before the
    /// string that would pass the limit is kept or relayed, and the turn
    /// leaves nothing behind: no continuation, no pooled socket, no resend.
    fn assert_refused_after_output(refused: &RefusedLiveStream, refused_marker: &str) {
        let body = &refused.body;
        assert_eq!(refused.status, StatusCode::OK, "{body}");
        assert!(body.contains("relayed before the limit"), "{body}");
        assert!(body.contains("event: error"), "{body}");
        assert!(
            body.contains(translate::live_stream::RETAINED_OUTPUT_TOO_LARGE),
            "{body}"
        );
        assert!(!body.contains("message_stop"), "{body}");
        assert!(!body.contains(refused_marker), "{body}");
        assert_eq!(refused.requests, 1, "nothing is sent again");
        assert!(!refused.turn_current, "the request state is dropped");
        assert!(!refused.continuation_kept);
        assert!(!refused.socket_pooled);
    }

    #[tokio::test]
    async fn a_live_text_delta_past_the_held_limit_ends_the_stream_and_its_state() {
        let mut events = output_before_the_limit();
        events.push(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "x".repeat(HELD_LIMIT)
        }));
        let refused = refused_live_websocket_turn("live-held-text", events).await;
        assert_refused_after_output(&refused, &"x".repeat(64));
    }

    /// Streamed arguments, which are relayed as they arrive rather than held
    /// until the call is done, count toward the same limit.
    #[tokio::test]
    async fn live_tool_arguments_past_the_held_limit_end_the_stream_and_its_state() {
        let mut events = output_before_the_limit();
        events.extend([
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_held", "name": "Bash"}
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "{\"command\":"
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "z".repeat(HELD_LIMIT)
            }),
        ]);
        let refused = refused_live_websocket_turn("live-held-tool", events).await;
        assert!(
            refused.body.contains("input_json_delta"),
            "{}",
            refused.body
        );
        assert_refused_after_output(&refused, &"z".repeat(64));
    }

    /// Reasoning kept for the next turn counts toward the limit like text.
    #[tokio::test]
    async fn live_reasoning_past_the_held_limit_ends_the_stream_and_its_state() {
        let mut events = output_before_the_limit();
        events.push(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "reasoning",
                "id": "rs_held",
                "encrypted_content": "r".repeat(HELD_LIMIT)
            }
        }));
        let refused = refused_live_websocket_turn("live-held-reasoning", events).await;
        assert_refused_after_output(&refused, &"r".repeat(64));
    }

    /// Before output starts the translated answer is held, and only the
    /// message start and keepalive pings add to it: a response kept open with
    /// progress events alone is answered 502 once it would pass the limit,
    /// and leaves nothing behind.
    #[tokio::test]
    async fn a_live_answer_held_past_the_limit_before_output_answers_502_once() {
        let progress = serde_json::json!({
            "type": "response.in_progress",
            "response": {"id": "resp-refused"}
        });
        let refused =
            refused_live_websocket_turn("live-held-before-output", vec![progress; HELD_LIMIT / 16])
                .await;
        let body = &refused.body;
        assert_eq!(refused.status, StatusCode::BAD_GATEWAY, "{body}");
        let error: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(error["error"]["type"], "api_error", "{body}");
        assert_eq!(refused.requests, 1, "nothing is sent again");
        assert!(!refused.turn_current, "the request state is dropped");
        assert!(!refused.continuation_kept);
        assert!(!refused.socket_pooled);
    }

    /// Over HTTP, the events held until output starts count toward the same
    /// limit: past it the request is answered 502 `api_error` once, and none
    /// of what was held reaches the client. The held events are ones the
    /// translator keeps nothing of, so only the reader's own count of what it
    /// holds can refuse them.
    #[tokio::test]
    async fn a_live_http_answer_held_past_the_limit_answers_502_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let session_id = "live-http-held";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));

        let held = |part: &str| {
            serde_json::json!({
                "type": "response.content_part.added",
                "output_index": 0,
                "item_id": part,
                "part": {"type": "output_text", "text": "h".repeat(HELD_LIMIT / 2)}
            })
        };
        let mut body = String::new();
        for event in [
            serde_json::json!({"type": "response.created", "response": {"id": "resp-held"}}),
            held("part_held_1"),
            held("part_held_2"),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "after the limit"
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp-held", "status": "completed", "output": []}
            }),
        ] {
            body.push_str(&format!("data: {event}\n\n"));
        }

        // The listener serves until the test ends it, so a request sent again
        // at any point before the answer is read is counted.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                served.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut buf = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut buf).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            }
        });

        let client = Arc::new(
            Arc::into_inner(authenticated_live_test_client(format!(
                "http://{addr}/responses"
            )))
            .unwrap()
            .with_buffered_response_limit(HELD_LIMIT),
        );
        let request = live_test_request("held over http");
        let turn = continuation_candidate_for_owner(Some(&owner), &request, true);
        let turn_after = turn.clone();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                turn,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::Http,
            ),
        )
        .await
        .expect("the turn did not start");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let answer = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let answer = String::from_utf8(answer.to_vec()).unwrap();
        let error: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(error["error"]["type"], "api_error", "{answer}");
        assert!(!answer.contains("after the limit"), "{answer}");
        assert!(!answer.contains("hhhhhhhh"), "{answer}");
        server.abort();
        assert_eq!(requests.load(Ordering::SeqCst), 1, "nothing is sent again");
        assert!(!continuation::is_current_turn_for_owner(&turn_after));
        assert!(!continuation::has_continuation_for_owner_for_tests(&owner));
        continuation::clear_continuation_for_owner(Some(&owner));
    }

    /// Once the response has started, the reader does not record again a
    /// reading the stream task recorded before queuing it: replaying a queued
    /// reading would take back a later one the task already recorded.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_reading_the_stream_task_recorded_is_not_recorded_again_after_output() {
        let _rate_limits = rate_limits::lock_for_tests();
        rate_limits::clear_for_tests();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reading = |used_percent: f64| {
            serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary": {
                        "used_percent": used_percent,
                        "window_minutes": 300,
                        "reset_at": now + 3_600
                    }
                }
            })
        };
        // The task recorded the queued reading, then a later one of the same
        // window that is not queued yet.
        rate_limits::observe_event(Some("acct"), &reading(10.0));
        rate_limits::observe_event(Some("acct"), &reading(50.0));

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        for event in [
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg-recorded"}
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "answer"
            }),
            reading(10.0),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp-recorded", "status": "completed", "output": []}
            }),
        ] {
            tx.send(Ok(event)).await.unwrap();
        }
        drop(tx);
        let (mut events, publisher) = websocket::CodexWebSocketEventStream::pending(rx);
        events.mark_rate_limits_observed();
        publisher.publish_account(Some("acct"));

        let request = live_test_request("one");
        let mut ctx = live_test_context("live-recorded-reading-after-output");
        ctx.session_id = None;
        let start = live_stream_response_once(
            events,
            Vec::new(),
            "message".to_string(),
            &request.model,
            ctx,
            ContinuationReservation::for_owner_turn(None, None),
            request.clone(),
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            client::MAX_BUFFERED_RESPONSE_BYTES,
        )
        .await;
        let LiveStreamStart::Response(response) = start else {
            panic!("the output must start the response");
        };
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            response.into_body().collect(),
        )
        .await
        .expect("the response did not end")
        .unwrap()
        .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("message_stop"));

        let held = rate_limits::latest(Some("acct")).expect("the reading is held");
        assert_eq!(held.five_hour.map(|window| window.utilization), Some(0.5));
        rate_limits::clear_for_tests();
    }

    #[tokio::test]
    async fn stale_request_cleanup_preserves_newer_turn_and_compaction_attempt() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let session_id = "stale-live-request-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        let request = live_test_request("one");
        let stale_continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let stale_compaction = begin_compaction(session_id, &request.model);
        let stale_cleanup = LiveRequestStateCleanup::new(
            stale_continuation,
            Some(session_id.to_string()),
            Some(stale_compaction),
        );

        let newer_continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let newer_compaction = begin_compaction(session_id, &request.model);
        drop(stale_cleanup);

        assert!(continuation::is_current_turn_for_owner(&newer_continuation));
        assert!(store_compaction(session_id, newer_compaction, Vec::new()));
        abort_request_state(
            Some(session_id),
            &newer_continuation,
            Some(newer_compaction),
        );
    }

    #[tokio::test]
    async fn retryable_live_failure_aborts_request_state_after_one_attempt() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let response = run_live_failure_case(
            "live-retryable-failure-cleanup",
            serde_json::json!({
                "type": "error",
                "error": {"code": "rate_limit_exceeded", "message": "Please slow down."}
            }),
            1,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key("x-should-retry"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn terminal_rate_limit_snapshot_fails_a_live_start_as_a_spent_window() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let _rate_limits = rate_limits::lock_for_tests();
        let response = run_live_failure_case(
            "live-terminal-snapshot-cleanup",
            serde_json::json!({
                "type": "codex.rate_limits",
                "credits": {"has_credits": false, "unlimited": false},
                "rate_limits": {
                    "allowed": false,
                    "limit_reached": true,
                    "primary": {
                        "used_percent": 100.0,
                        "window_minutes": 300,
                        "reset_after_seconds": 9569,
                        "reset_at": 1788879437u64
                    },
                    "secondary": {
                        "used_percent": 40.0,
                        "window_minutes": 10080,
                        "reset_after_seconds": 596369,
                        "reset_at": 1789466238u64
                    }
                }
            }),
            1,
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-status"],
            "rejected"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-reset"],
            "1788879437"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-representative-claim"],
            "five_hour"
        );
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "Usage limit reached");
    }

    #[tokio::test]
    async fn excessive_retry_after_aborts_live_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let response = run_live_failure_case(
            "live-excessive-retry-after-cleanup",
            serde_json::json!({
                "type": "error",
                "error": {
                    "code": "rate_limit_exceeded",
                    "message": "Please slow down.",
                    "retry_after": 31
                }
            }),
            1,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "31");
    }

    #[tokio::test]
    async fn nonretryable_live_error_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let status = run_live_failure_case(
            "live-nonretryable-cleanup",
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {"message": "invalid request"}
                }
            }),
            1,
        )
        .await
        .status();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
}
