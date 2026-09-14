pub mod auth;
pub mod client;
pub mod connect;
pub mod model;
pub mod proto;
pub mod request;
pub mod response;
pub mod sse;
#[cfg(test)]
pub(crate) mod test_frames;
pub mod tool_bridge;
pub mod tool_use_xml;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::monitor::usage_from_anthropic_sse;
use crate::provider::{
    CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
    RequestContext,
};
use crate::providers::cursor::auth::{
    clear_cursor_auth, expired_auth_message, load_cursor_auth, missing_auth_message,
    run_cursor_login,
};
use crate::providers::cursor::client::CursorHttpClient;
use crate::providers::cursor::model::resolve_cursor_model;
use crate::providers::cursor::request::render_cursor_prompt;
use crate::providers::cursor::response::{
    CursorDecodeError, decode_cursor_upstream, decode_upstream_response,
};
use crate::providers::cursor::tool_bridge::{
    BridgeRegistry, advertised_tool_names, can_bridge_cursor_native_tools, find_tool_result,
    resume_cursor_tool_bridge, start_cursor_tool_bridge,
};

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CursorProvider;

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn supported_models(&self) -> Vec<String> {
        model::cursor_supported_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CURSOR_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("cursor");

        // The client resolves the caller's id to the model id it puts in its
        // frames; the same resolution decides here whether the request is
        // servable at all. Which model ran is published later, where a call to
        // Cursor is actually prepared: the tool bridge answers from state the
        // proxy holds and a missing credential stops the request, and neither
        // runs a model.
        let resolved = match resolve_cursor_model(model) {
            Ok(resolved) => resolved,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("Model \"{model}\" is not supported: {e}"),
                );
            }
        };

        if let Some(ref session_id) = ctx.session_id
            && let Some(pending) = BridgeRegistry::pending_tool(session_id)
            && let Some(result) = find_tool_result(&body, pending.tool_use_id())
        {
            let (_result_messages, sse_bytes) =
                resume_cursor_tool_bridge(session_id, &message_id, model, result, &pending);
            if let Some(monitor) = ctx.monitor.as_ref() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                monitor.stream_progress(
                    &ctx.req_id,
                    sse_bytes.len() as u64,
                    count_sse_events(&sse_bytes),
                    input_tokens,
                    output_tokens,
                );
            }
            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            return (headers, sse_bytes).into_response();
        }

        let auth = match load_cursor_auth() {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    missing_auth_message(),
                );
            }
            Err(err) => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    format!("Cursor auth failed: {err}"),
                );
            }
        };

        if matches!(auth.expires, Some(expires) if expires <= now_ms() + 60_000) {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                expired_auth_message(&auth),
            );
        }

        let token = auth.access_token;

        let prompt = render_cursor_prompt(&body);
        let images = request::cursor_selected_images(&body);

        let client = CursorHttpClient::new();
        // The last common point before the request leaves: the client resolves
        // the caller's id the same way and puts this id in its frames, so it is
        // the model the call was prepared with, whatever the transport then
        // does with it.
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model_id);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match client.run_agent(&token, &prompt, model, &images).await {
            Ok(r) => r,
            Err(e) => {
                return map_cursor_error_to_response(&e);
            }
        };

        if want_stream {
            let session_id = ctx.session_id.as_deref();
            let bridge_eligible = can_bridge_cursor_native_tools(&body, session_id);

            if bridge_eligible {
                let events = match decode_upstream_response(&upstream.body) {
                    Ok(e) => e,
                    Err(e) => return map_cursor_decode_error_to_response(&e),
                };

                let allowed = advertised_tool_names(&body);
                let (sse_bytes, _paused) = start_cursor_tool_bridge(
                    &message_id,
                    model,
                    session_id.unwrap(),
                    &events,
                    allowed,
                    Box::new(|| uuid::Uuid::new_v4().to_string().replace('-', "")),
                );
                if let Some(monitor) = ctx.monitor.as_ref() {
                    let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                    monitor.stream_progress(
                        &ctx.req_id,
                        sse_bytes.len() as u64,
                        count_sse_events(&sse_bytes),
                        input_tokens,
                        output_tokens,
                    );
                }

                let headers = [
                    (http::header::CONTENT_TYPE, "text/event-stream"),
                    (http::header::CACHE_CONTROL, "no-cache"),
                    (http::header::CONNECTION, "keep-alive"),
                ];
                (headers, sse_bytes).into_response()
            } else {
                let sse_bytes = sse::frame_cursor_stream(&upstream, &message_id, model);
                if let Some(monitor) = ctx.monitor.as_ref() {
                    let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                    monitor.stream_progress(
                        &ctx.req_id,
                        sse_bytes.len() as u64,
                        count_sse_events(&sse_bytes),
                        input_tokens,
                        output_tokens,
                    );
                }
                let headers = [
                    (http::header::CONTENT_TYPE, "text/event-stream"),
                    (http::header::CACHE_CONTROL, "no-cache"),
                    (http::header::CONNECTION, "keep-alive"),
                ];
                (headers, sse_bytes).into_response()
            }
        } else {
            match decode_cursor_upstream(&upstream, &message_id, model) {
                Ok(json) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                            json.pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => map_cursor_decode_error_to_response(&e),
            }
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let prompt = render_cursor_prompt(&body);
        let tokens = (prompt.len() / 4) as u64; // rough estimate
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

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let requested = body.model.clone().unwrap_or_else(|| "cursor".to_string());
        let resolved = resolve_cursor_model(&requested).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_REQUEST,
                ProviderErrorKind::InvalidRequest,
                format!("Model \"{requested}\" is not supported: {error}"),
            )
        })?;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        if let Some(session_id) = ctx.session_id.as_deref()
            && let Some(pending) = BridgeRegistry::pending_tool(session_id)
            && let Some(result) = find_tool_result(&body, pending.tool_use_id())
        {
            let (_, bytes) =
                resume_cursor_tool_bridge(session_id, &message_id, &requested, result, &pending);
            return Ok(Generation {
                body: GenerationBody::BufferedSse(bytes.into()),
                resolved_model: resolved.model_id,
            });
        }
        let auth = load_cursor_auth()
            .map_err(|error| {
                ProviderError::new(
                    StatusCode::UNAUTHORIZED,
                    ProviderErrorKind::Authentication,
                    format!("Cursor auth failed: {error}"),
                )
            })?
            .ok_or_else(|| {
                ProviderError::new(
                    StatusCode::UNAUTHORIZED,
                    ProviderErrorKind::Authentication,
                    missing_auth_message(),
                )
            })?;
        if matches!(auth.expires, Some(expires) if expires <= now_ms() + 60_000) {
            return Err(ProviderError::new(
                StatusCode::UNAUTHORIZED,
                ProviderErrorKind::Authentication,
                expired_auth_message(&auth),
            ));
        }
        let prompt = render_cursor_prompt(&body);
        let images = request::cursor_selected_images(&body);
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_json(
                "020-upstream-request",
                &serde_json::json!({
                    "model": requested,
                    "prompt": prompt,
                    "image_count": images.len(),
                }),
            );
        }
        // Same boundary as the Messages route: the model is named where the
        // call to Cursor is prepared, not where the id was resolved, so a
        // bridged answer or a missing credential names none.
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model_id);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = CursorHttpClient::new()
            .run_agent(&auth.access_token, &prompt, &requested, &images)
            .await
            .map_err(cursor_provider_error)?;
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("032-upstream-response-body.bin", &upstream.body);
        }
        let bytes = if can_bridge_cursor_native_tools(&body, ctx.session_id.as_deref()) {
            let events =
                decode_upstream_response(&upstream.body).map_err(cursor_decode_provider_error)?;
            let allowed = advertised_tool_names(&body);
            start_cursor_tool_bridge(
                &message_id,
                &requested,
                ctx.session_id.as_deref().expect("bridge session validated"),
                &events,
                allowed,
                Box::new(|| uuid::Uuid::new_v4().simple().to_string()),
            )
            .0
        } else {
            sse::frame_cursor_stream(&upstream, &message_id, &requested)
        };
        if let Some(traffic) = ctx.traffic.as_ref() {
            traffic.write_bytes("050-anthropic-intermediate.sse", &bytes);
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&bytes);
            monitor.stream_progress(
                &ctx.req_id,
                bytes.len() as u64,
                count_sse_events(&bytes),
                input_tokens,
                output_tokens,
            );
        }
        Ok(Generation {
            body: GenerationBody::BufferedSse(bytes.into()),
            resolved_model: resolved.model_id,
        })
    }
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn cursor_provider_error(err: client::CursorError) -> ProviderError {
    let (status, kind) = match err.status {
        401 | 403 => (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
        429 => (StatusCode::TOO_MANY_REQUESTS, ProviderErrorKind::RateLimit),
        _ => (StatusCode::BAD_GATEWAY, ProviderErrorKind::Api),
    };
    let mut error = ProviderError::new(status, kind, err.detail.unwrap_or(err.message));
    if err.status == 429 {
        error.retry_after = Some(err.retry_after.unwrap_or_else(|| "5".to_string()));
    }
    error
}

fn cursor_decode_provider_error(err: CursorDecodeError) -> ProviderError {
    let (status, kind) = match err.status() {
        Some(401 | 403) => (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
        Some(429) => (StatusCode::TOO_MANY_REQUESTS, ProviderErrorKind::RateLimit),
        _ => (StatusCode::BAD_GATEWAY, ProviderErrorKind::Api),
    };
    ProviderError::new(status, kind, format!("Response decoding error: {err}"))
}

fn map_cursor_error_to_response(err: &client::CursorError) -> Response {
    match err.status {
        401 | 403 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.detail.as_deref().unwrap_or("Authentication failed"),
        ),
        429 => {
            let retry_after = err.retry_after.as_deref().unwrap_or("5");
            let resp = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &err.message,
            );
            let headers = [(http::header::RETRY_AFTER, retry_after)];
            (headers, resp).into_response()
        }
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            err.detail.as_deref().unwrap_or("Upstream error"),
        ),
    }
}

fn map_cursor_decode_error_to_response(err: &CursorDecodeError) -> Response {
    match err.status() {
        Some(401 | 403) => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.to_string(),
        ),
        Some(429) => json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            err.to_string(),
        ),
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("Response decoding error: {err}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CursorCli;

impl CliHandlers for CursorCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        let auth = run_cursor_login()?.ok_or_else(|| anyhow::anyhow!("Cursor login timed out"))?;
        println!("Cursor auth saved in {}", auth.source);
        if let Some(ref user_id) = auth.user_id {
            println!("User: {user_id}");
        }
        if let Some(ref email) = auth.email {
            println!("Email: {email}");
        }
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!("cursor: device login not yet implemented");
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        match load_cursor_auth()? {
            Some(auth) => {
                println!("Auth source: {}", auth.source);
                if let Some(ref user_id) = auth.user_id {
                    println!("User: {user_id}");
                }
                if let Some(ref email) = auth.email {
                    println!("Email: {email}");
                }
                if let Some(expires) = auth.expires {
                    let remaining = expires.saturating_sub(now_ms()) / 1000;
                    println!("Access token expires in: {remaining}s");
                } else {
                    println!("Access token expiry: unknown");
                }
                Ok(())
            }
            None => {
                anyhow::bail!("Not authenticated");
            }
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        clear_cursor_auth()?;
        println!(
            "Cursor persistent auth cleared. Unset CCP_CURSOR_AUTH_TOKEN or CURSOR_AUTH_TOKEN if using env auth."
        );
        Ok(())
    }
}

pub(crate) static CURSOR_CLI: CursorCli = CursorCli;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_models_includes_legacy_and_agent() {
        let provider = CursorProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"cursor".to_string()));
        assert!(models.contains(&"cursor-agent".to_string()));
        assert!(models.contains(&"cursor-plan".to_string()));
        assert!(models.contains(&"cursor-ask".to_string()));
    }

    /// A request that stops at the auth check never prepares a call to Cursor,
    /// so no model ran: the row keeps the id the caller typed and names no
    /// executed model. The resolution the route performed is not evidence that
    /// anything was sent.
    #[tokio::test]
    async fn a_request_that_stops_at_auth_names_no_model_as_having_run() {
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            "cursor-1",
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        monitor.provider_selected("cursor-1", "cursor", "cursor:composer-2.5-fast", None);
        let ctx = RequestContext {
            req_id: "cursor-1".to_string(),
            session_id: None,
            session_seq: None,
            provider: "cursor".to_string(),
            traffic: None,
            monitor: Some(monitor.clone()),
            passthrough: None,
        };
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:composer-2.5-fast",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        // No Cursor credentials in the test environment, so the request stops at
        // the auth check.
        let response = CursorProvider::new().handle_messages(body, ctx).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let state = monitor.snapshot();
        assert_eq!(
            state.active[0].model.as_deref(),
            Some("cursor:composer-2.5-fast")
        );
        assert_eq!(state.active[0].effective_model, None);
    }

    /// The tool bridge answers a resumed tool call from state the proxy holds,
    /// without a request to Cursor. Nothing ran upstream, so nothing may be
    /// named as the model that ran.
    #[tokio::test]
    async fn a_locally_bridged_answer_names_no_model_as_having_run() {
        let session_id = "session-bridge-monitor";
        BridgeRegistry::remove(session_id);
        let events = vec![
            crate::providers::cursor::response::CursorStreamEvent::TextDelta {
                text: r#"<tool_use name="Read">{"file_path":"/tmp/bridge"}</tool_use>"#.to_string(),
            },
        ];
        let allowed: std::collections::BTreeSet<String> =
            ["Read".to_string()].into_iter().collect();
        let (_first, paused) = start_cursor_tool_bridge(
            "msg_bridge",
            "cursor:composer-2.5-fast",
            session_id,
            &events,
            Some(allowed),
            Box::new(|| "call_bridge_1".to_string()),
        );
        assert!(paused, "the bridge must be waiting for the tool result");

        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            "cursor-bridge",
            Some(session_id.to_string()),
            None,
            crate::monitor::EndpointKind::Messages,
        );
        monitor.provider_selected("cursor-bridge", "cursor", "cursor:composer-2.5-fast", None);
        let ctx = RequestContext {
            req_id: "cursor-bridge".to_string(),
            session_id: Some(session_id.to_string()),
            session_seq: None,
            provider: "cursor".to_string(),
            traffic: None,
            monitor: Some(monitor.clone()),
            passthrough: None,
        };
        let body: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "cursor:composer-2.5-fast",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_bridge_1",
                    "content": "file contents"
                }]
            }]
        }))
        .unwrap();

        let response = CursorProvider::new().handle_messages(body, ctx).await;
        assert_eq!(response.status(), StatusCode::OK);

        let state = monitor.snapshot();
        assert_eq!(
            state.active[0].model.as_deref(),
            Some("cursor:composer-2.5-fast")
        );
        assert_eq!(state.active[0].effective_model, None);

        BridgeRegistry::remove(session_id);
    }
}
