use base64::Engine;
use bytes::Bytes;
use futures_util::StreamExt;
use prost::Message;
use tokio::sync::mpsc;

use crate::config;
use crate::providers::cursor::connect::{
    ConnectFrame, ConnectFrameDecoder, FLAG_END, FLAG_GZIP, encode_connect_frame,
    parse_connect_error,
};
use crate::providers::cursor::model::CursorModelResolution;
use crate::providers::cursor::proto;
use crate::providers::cursor::request::CursorSelectedImage;

/// Upstream response from the Cursor API.
///
/// Contains the raw response bytes (or body bytes for streaming) and the
/// HTTP status.
pub struct CursorUpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub error_detail: Option<String>,
}

impl CursorUpstreamResponse {
    pub fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// HTTP/2 client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for CursorHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorHttpClient {
    pub fn new() -> Self {
        // Use HTTP/2 prior knowledge for cleartext URLs (mock testing) and
        // standard TLS for https URLs.
        let base_url = config::cursor_base_url();
        let is_cleartext = base_url.starts_with("http://");

        let mut builder = crate::http_client::client_builder()
            .http2_keep_alive_timeout(std::time::Duration::from_secs(30))
            .http2_keep_alive_while_idle(true);

        if is_cleartext {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build().expect("CursorHttpClient: reqwest client");

        Self { client, base_url }
    }

    /// Run the Cursor agent with the given prompt and token.
    ///
    /// Opens a bidirectional Connect stream, sends the agent request frames,
    /// and keeps the request body alive while collecting the response frames.
    pub async fn run_agent(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<CursorUpstreamResponse, CursorError> {
        let resolved = super::model::resolve_cursor_model(model)
            .map_err(|e| CursorError::internal(format!("model resolution: {e}")))?;

        let request_id = uuid::Uuid::new_v4().to_string();
        let frames = build_run_frames(prompt, &resolved, images, &request_id);
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let sender = tokio::spawn(async move {
            for (index, frame) in frames.into_iter().enumerate() {
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
                let delay = match index {
                    0 => std::time::Duration::from_millis(1500),
                    1 => std::time::Duration::from_millis(800),
                    _ => std::time::Duration::from_millis(400),
                };
                tokio::time::sleep(delay).await;
            }

            let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
            heartbeat.tick().await;
            loop {
                heartbeat.tick().await;
                if tx.send(Ok(heartbeat_frame())).await.is_err() {
                    return;
                }
            }
        });
        let body =
            reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));

        let url = format!(
            "{}/agent.v1.AgentService/Run",
            self.base_url.trim_end_matches('/')
        );
        let client_version = config::cursor_client_version();

        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip,br")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .header("x-cursor-streaming", "true")
            .header("te", "trailers")
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)?;

        let result = read_run_response(response).await;
        sender.abort();
        result
    }
}

/// Reads a Run response. An error status comes from the response head, so a
/// body over the limit or one that cannot be read costs only its message.
async fn read_run_response(
    response: reqwest::Response,
) -> Result<CursorUpstreamResponse, CursorError> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let error_detail = response
        .headers()
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let limit = if status >= 400 {
        MAX_ERROR_BODY_BYTES
    } else {
        MAX_RESPONSE_BYTES
    };
    let body_bytes = read_run_body(response.bytes_stream(), limit).await;

    if status >= 400 {
        let detail = body_bytes
            .ok()
            .and_then(|body| parse_error_body(&body, &headers));
        return Err(CursorError::new(status, "Cursor upstream error", detail));
    }

    Ok(CursorUpstreamResponse {
        status,
        body: body_bytes?,
        error_detail,
    })
}

/// The most of a successful Run response held in memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The most of an error response kept to read its message from.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Collects the Run response frames until the end frame, the end of the body
/// or the idle timeout, refusing a body that grows past `limit` bytes.
async fn read_run_body<S>(mut stream: S, limit: usize) -> Result<Vec<u8>, CursorError>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let mut body_bytes = Vec::new();
    let mut received_data = false;

    loop {
        let timeout = if received_data {
            std::time::Duration::from_secs(5)
        } else {
            std::time::Duration::from_secs(60)
        };
        match tokio::time::timeout(timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                received_data = true;
                if chunk.len() > limit - body_bytes.len() {
                    return Err(CursorError::internal(format!(
                        "read body: {}",
                        crate::upstream_http::BodyReadError::TooLarge { limit }
                    )));
                }
                body_bytes.extend_from_slice(&chunk);
                if contains_end_frame(&body_bytes) {
                    break;
                }
            }
            Ok(Some(Err(error))) => {
                return Err(CursorError::internal(format!(
                    "read body: {}",
                    crate::upstream_http::error_text(error)
                )));
            }
            Ok(None) => break,
            Err(_) if received_data => break,
            Err(_) => {
                return Err(CursorError::internal(
                    "Cursor upstream timed out before sending a response",
                ));
            }
        }
    }
    Ok(body_bytes)
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    encode_varint((field << 3) | 2, &mut out);
    encode_varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn field_string(field: u64, value: &str) -> Vec<u8> {
    field_bytes(field, value.as_bytes())
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(field << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn model_message(model: &str, fast: bool) -> Vec<u8> {
    let mut out = field_string(1, model);
    let mut parameter = field_string(1, "fast");
    parameter.extend(field_string(2, if fast { "true" } else { "false" }));
    out.extend(field_bytes(3, &parameter));
    out
}

fn mode_value(resolved: &CursorModelResolution) -> u64 {
    match resolved.mode {
        super::model::CursorAgentMode::Agent => 1,
        super::model::CursorAgentMode::Ask => 2,
        super::model::CursorAgentMode::Plan => 3,
    }
}

fn selected_context(images: &[CursorSelectedImage]) -> Option<Vec<u8>> {
    if images.is_empty() {
        return None;
    }

    let mut context = Vec::new();
    for image in images {
        let data = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap_or_default();
        let mut selected = field_string(2, &image.uuid);
        selected.extend(field_string(3, &image.path));
        selected.extend(field_string(7, &image.mime_type));
        selected.extend(field_bytes(8, &data));
        context.extend(field_bytes(1, &selected));
    }
    Some(context)
}

fn build_run_frames(
    prompt: &str,
    resolved: &CursorModelResolution,
    images: &[CursorSelectedImage],
    request_id: &str,
) -> Vec<Bytes> {
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let mut user_message = field_string(1, prompt);
    user_message.extend(field_string(2, request_id));
    if let Some(context) = selected_context(images) {
        user_message.extend(field_bytes(3, &context));
    } else {
        user_message.extend(field_bytes(3, &[]));
    }
    user_message.extend(field_varint(4, mode_value(resolved)));

    let action = field_bytes(1, &field_bytes(1, &user_message));
    let mut request = field_bytes(1, &[]);
    request.extend(field_bytes(2, &action));
    request.extend(field_bytes(4, &[]));
    request.extend(field_string(5, &conversation_id));
    request.extend(field_bytes(
        9,
        &model_message(&resolved.model_id, resolved.fast),
    ));
    request.extend(field_varint(12, 0));
    request.extend(field_bytes(14, &field_string(1, "default")));
    request.extend(field_bytes(
        14,
        &model_message(&resolved.model_id, resolved.fast),
    ));
    request.extend(field_string(16, &conversation_id));
    let first = encode_connect_frame(field_bytes(1, &request), 0);

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default();
    let mut environment = field_string(1, std::env::consts::OS);
    environment.extend(field_string(2, &cwd));
    environment.extend(field_string(
        3,
        if cfg!(windows) { "powershell" } else { "bash" },
    ));
    environment.extend(field_string(10, "UTC"));
    environment.extend(field_string(11, &cwd));
    environment.extend(field_varint(14, 1));
    environment.extend(field_varint(16, 1));
    environment.extend(field_varint(19, 0));
    environment.extend(field_varint(20, 0));
    environment.extend(field_string(21, &cwd));
    environment.extend(field_varint(22, 0));
    let context = field_bytes(
        2,
        &field_bytes(
            10,
            &field_bytes(1, &field_bytes(1, &field_bytes(4, &environment))),
        ),
    );

    let mut frames = vec![first, encode_connect_frame(context, 0)];
    frames.push(encode_connect_frame(
        field_bytes(5, &field_string(1, "")),
        0,
    ));
    frames.push(encode_connect_frame(
        field_bytes(3, &field_string(3, "")),
        0,
    ));
    for sequence in 1..=8 {
        let mut marker = field_varint(1, sequence);
        marker.extend(field_string(3, ""));
        frames.push(encode_connect_frame(field_bytes(3, &marker), 0));
    }
    frames
}

fn heartbeat_frame() -> Bytes {
    encode_connect_frame(field_bytes(7, &[]), 0)
}

fn contains_end_frame(body: &[u8]) -> bool {
    let mut offset = 0;
    while body.len().saturating_sub(offset) >= 5 {
        let length = u32::from_be_bytes([
            body[offset + 1],
            body[offset + 2],
            body[offset + 3],
            body[offset + 4],
        ]) as usize;
        if body.len().saturating_sub(offset) < 5 + length {
            return false;
        }
        if body[offset] & FLAG_END != 0 {
            return true;
        }
        offset += 5 + length;
    }
    false
}

fn parse_error_body(body_bytes: &[u8], _headers: &reqwest::header::HeaderMap) -> Option<String> {
    if body_bytes.len() < 5 {
        return None;
    }
    // Try to parse as Connect end frame with JSON error
    if body_bytes.len() >= 5 {
        let flags = body_bytes[0];
        let len = u32::from_be_bytes([body_bytes[1], body_bytes[2], body_bytes[3], body_bytes[4]])
            as usize;
        if flags & FLAG_END != 0 && body_bytes.len() >= 5 + len {
            let payload = &body_bytes[5..5 + len];
            let err = parse_connect_error(payload);
            if err.is_some() {
                return err.map(|e| e.detail);
            }
        }
    }

    // Try plain text error
    if let Ok(text) = String::from_utf8(body_bytes.to_vec())
        && !text.is_empty()
    {
        return Some(text);
    }
    None
}

/// Decode upstream response bytes into Connect frames containing
/// AgentServerMessage values.
pub fn decode_upstream_frames(body: &[u8]) -> Result<Vec<ConnectFrame>, CursorError> {
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder
        .push(body)
        .map_err(|e| CursorError::internal(format!("frame decode: {e}")))?;
    Ok(frames)
}

/// Decode a single Connect frame payload into an AgentServerMessage.
/// Handles gzip decompression if the FLAG_GZIP bit is set.
pub fn decode_frame_payload(
    frame: &ConnectFrame,
) -> Result<proto::AgentServerMessage, CursorError> {
    let payload = if frame.flags & FLAG_GZIP != 0 {
        super::connect::decode_gzip_frame(&frame.payload)
            .map_err(|e| CursorError::internal(format!("gzip decompress: {e}")))?
    } else {
        frame.payload.to_vec()
    };

    proto::AgentServerMessage::decode(&payload[..])
        .map_err(|e| CursorError::internal(format!("prost decode: {e}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CursorError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: crate::upstream_http::error_text(e),
            detail: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn assert_no_url(text: &str) {
        for leaked in [
            "toy-secret-path",
            "toy-secret-query",
            "127.0.0.1",
            "for url",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text:?}");
        }
    }

    /// Answers one HTTP/1.1 request with a body of `size` bytes and no end
    /// frame, then closes the connection.
    async fn serve_body(size: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            let mut response =
                format!("HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {size}\r\n\r\n")
                    .into_bytes();
            response.resize(response.len() + size, b'a');
            stream.write_all(&response).await.unwrap();
        });
        format!("http://{addr}/")
    }

    async fn read_served_body(size: usize, limit: usize) -> Result<Vec<u8>, CursorError> {
        let url = serve_body(size).await;
        let response = crate::http_client::client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        read_run_body(response.bytes_stream(), limit).await
    }

    #[tokio::test]
    async fn a_failed_send_carries_no_url() {
        let client = CursorHttpClient {
            client: crate::http_client::client_builder()
                .no_proxy()
                .build()
                .unwrap(),
            base_url: crate::upstream_http::test_support::unanswered_secret_url(),
        };
        let error = match client
            .run_agent("toy-token", "hi", "cursor:auto", &[])
            .await
        {
            Ok(_) => panic!("the send must fail"),
            Err(error) => error,
        };
        assert_eq!(error.status, 502);
        assert!(!error.message.is_empty());
        assert_no_url(&error.message);
        assert_no_url(&error.to_string());
    }

    #[tokio::test]
    async fn a_run_body_of_exactly_the_limit_is_kept() {
        let body = read_served_body(16, 16).await.unwrap();
        assert_eq!(body, vec![b'a'; 16]);
    }

    #[tokio::test]
    async fn a_run_body_over_the_limit_is_refused() {
        let error = read_served_body(17, 16).await.unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.message, "read body: response body exceeded 16 bytes");
    }

    /// The error a Run response with `status` and `body` turns into, served
    /// with and without a declared length.
    async fn run_errors(status: u16, body: Vec<u8>) -> Vec<CursorError> {
        use crate::upstream_http::test_support::{serve_in_order, serve_in_order_undeclared};
        let mut errors = Vec::new();
        for serve in [serve_in_order, serve_in_order_undeclared] {
            let url = serve(vec![(status, body.clone())]);
            let response = crate::http_client::client_builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .send()
                .await
                .unwrap();
            match read_run_response(response).await {
                Ok(_) => panic!("status {status} must fail"),
                Err(error) => errors.push(error),
            }
        }
        errors
    }

    async fn error_type(response: axum::response::Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        value["error"]["type"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn an_error_body_of_exactly_the_limit_keeps_its_message() {
        for error in run_errors(401, vec![b'e'; MAX_ERROR_BODY_BYTES]).await {
            assert_eq!(error.status, 401);
            assert_eq!(error.detail, Some("e".repeat(MAX_ERROR_BODY_BYTES)));
        }
    }

    #[tokio::test]
    async fn an_unauthorized_answer_over_the_limit_keeps_its_status() {
        for error in run_errors(401, vec![b'e'; MAX_ERROR_BODY_BYTES + 1]).await {
            assert_eq!(error.status, 401);
            assert_eq!(error.detail, None);
            let response = super::super::map_cursor_error_to_response(&error);
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
            assert_eq!(error_type(response).await, "authentication_error");
        }
    }

    #[tokio::test]
    async fn a_rate_limited_answer_over_the_limit_keeps_its_status() {
        for error in run_errors(429, vec![b'e'; MAX_ERROR_BODY_BYTES + 1]).await {
            assert_eq!(error.status, 429);
            assert_eq!(error.detail, None);
            let response = super::super::map_cursor_error_to_response(&error);
            assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()[http::header::RETRY_AFTER], "5");
            assert_eq!(error_type(response).await, "rate_limit_error");
        }
    }
}
