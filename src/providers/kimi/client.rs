use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::providers::kimi::auth::constants::api_base_url;
use crate::providers::kimi::auth::headers::common_headers;
use crate::providers::kimi::auth::manager::KimiAuthManager;
use crate::providers::kimi::auth::token_store::{StoredAuth, file_store};
use crate::providers::kimi::translate::request::KimiChatRequest;
use crate::retry::{MAX_RATE_LIMIT_RETRIES, compute_backoff_delay};
use crate::upstream_http::{
    BodyReadError, blocking_error_text as error_body_text, read_blocking_body_capped,
};

#[derive(Debug)]
pub struct KimiError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

pub struct KimiResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub request_start_time: u64,
}

pub struct KimiHttpClient {
    client: reqwest::blocking::Client,
    auth_manager: KimiAuthManager<crate::auth::FileAuthStore<StoredAuth>>,
}

impl Default for KimiHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: KimiAuthManager::new(file_store()),
        }
    }

    pub fn auth_manager(&self) -> &KimiAuthManager<crate::auth::FileAuthStore<StoredAuth>> {
        &self.auth_manager
    }

    pub fn post_kimi(&self, body: &KimiChatRequest) -> Result<KimiResponse, KimiError> {
        let mut auth = self.auth_manager.get_auth().map_err(|e| KimiError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let mut attempt = 0u32;
        loop {
            let result = self.attempt_post(&auth.access, body);

            match result {
                Ok(response) if response.status == 401 && attempt == 0 => {
                    // First 401: try refresh
                    match self.auth_manager.force_refresh() {
                        Ok(new_auth) => {
                            auth = new_auth;
                            attempt += 1;
                            continue;
                        }
                        Err(e) => {
                            return Err(KimiError {
                                status: 401,
                                message: "Unauthorized".to_string(),
                                detail: Some(e.to_string()),
                                retry_after: None,
                            });
                        }
                    }
                }
                Ok(response) => return Ok(response),
                Err(err @ KimiError { status: 429, .. }) => {
                    if attempt < MAX_RATE_LIMIT_RETRIES {
                        let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
                        std::thread::sleep(std::time::Duration::from_millis(delay.wait_ms));
                        attempt += 1;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn attempt_post(
        &self,
        access_token: &str,
        body: &KimiChatRequest,
    ) -> Result<KimiResponse, KimiError> {
        let headers = common_headers().map_err(|e| KimiError {
            status: 500,
            message: "Failed to build headers".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let url = format!("{}/chat/completions", api_base_url());
        let body_json = serde_json::to_string(body).map_err(|e| KimiError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        let request_start_time = now_ms();

        let mut req_builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access_token}"));

        // Add common headers
        for (k, v) in &headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
            {
                req_builder = req_builder.header(name, value);
            }
        }

        let resp = match req_builder.body(body_json).send() {
            Ok(r) => r,
            Err(e) => {
                return Err(KimiError {
                    status: 0,
                    message: "Network error".to_string(),
                    detail: Some(crate::upstream_http::error_text(e)),
                    retry_after: None,
                });
            }
        };

        read_response(
            resp,
            request_start_time,
            MAX_ERROR_BODY_BYTES,
            MAX_RESPONSE_BYTES,
            REQUEST_TIMEOUT,
        )
    }
}

/// The timeout of the Kimi API client, and the deadline for reading a body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The most of an error response kept for its message.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// The most of a successful response held in memory before it is translated.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn read_response(
    resp: reqwest::blocking::Response,
    request_start_time: u64,
    max_error_bytes: usize,
    max_response_bytes: usize,
    deadline: Duration,
) -> Result<KimiResponse, KimiError> {
    let status = resp.status().as_u16();

    if status == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = error_body_text(resp, max_error_bytes, deadline);
        return Err(KimiError {
            status: 429,
            message: "Rate limited".to_string(),
            detail: if text.is_empty() { None } else { Some(text) },
            retry_after,
        });
    }

    if status == 401 || status == 403 {
        let text = error_body_text(resp, max_error_bytes, deadline);
        return Err(KimiError {
            status,
            message: if status == 401 {
                "Unauthorized"
            } else {
                "Forbidden"
            }
            .to_string(),
            detail: if text.is_empty() { None } else { Some(text) },
            retry_after: None,
        });
    }

    if !resp.status().is_success() {
        let text = error_body_text(resp, max_error_bytes, deadline);
        return Err(KimiError {
            status,
            message: "Upstream error".to_string(),
            detail: if text.is_empty() { None } else { Some(text) },
            retry_after: None,
        });
    }

    // An answer over the limit is an upstream failure: an empty body would
    // reach the client as an empty successful message. A failed read keeps
    // its old outcome, an empty body.
    let body_bytes = match read_blocking_body_capped(resp, max_response_bytes, deadline) {
        Ok(body) => body,
        Err(error @ BodyReadError::TooLarge { .. }) => {
            return Err(KimiError {
                status: 502,
                message: "Upstream error".to_string(),
                detail: Some(format!("Kimi {error}")),
                retry_after: None,
            });
        }
        Err(BodyReadError::Transport(_)) => Vec::new(),
    };

    Ok(KimiResponse {
        body: body_bytes,
        status,
        request_start_time,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers one request with `status` and a body of `size` bytes, then
    /// closes the connection.
    fn serve_body(status: u16, size: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            let mut response = format!(
                "HTTP/1.1 {status} STATUS\r\ncontent-length: {size}\r\nconnection: close\r\n\r\n"
            )
            .into_bytes();
            response.resize(response.len() + size, b'a');
            let _ = stream.write_all(&response);
        });
        format!("http://{addr}/")
    }

    fn read_served(status: u16, size: usize) -> Result<KimiResponse, KimiError> {
        let resp = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(serve_body(status, size))
            .send()
            .unwrap();
        read_response(resp, 0, 16, 32, REQUEST_TIMEOUT)
    }

    #[test]
    fn an_error_body_of_exactly_the_limit_is_kept() {
        let error = read_served(500, 16).err().unwrap();
        assert_eq!(error.status, 500);
        assert_eq!(error.detail.as_deref(), Some("a".repeat(16).as_str()));
    }

    #[test]
    fn an_error_body_over_the_limit_is_dropped() {
        for status in [429, 401, 500] {
            let error = read_served(status, 17).err().unwrap();
            assert_eq!(error.status, status);
            assert_eq!(error.detail, None, "status {status}");
        }
    }

    #[test]
    fn a_response_of_exactly_the_limit_is_kept() {
        let response = read_served(200, 32).ok().unwrap();
        assert_eq!(response.body, vec![b'a'; 32]);
    }

    #[test]
    fn a_response_over_the_limit_is_an_upstream_error() {
        assert_over_the_limit(read_served(200, 33).err().unwrap());
    }

    #[test]
    fn an_undeclared_response_over_the_limit_is_an_upstream_error() {
        use crate::upstream_http::test_support::serve_in_order_undeclared;
        let url = serve_in_order_undeclared(vec![(200, vec![b'a'; 32]), (200, vec![b'a'; 33])]);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let read = || {
            let resp = client.get(&url).send().unwrap();
            assert_eq!(resp.content_length(), None);
            read_response(resp, 0, 16, 32, REQUEST_TIMEOUT)
        };
        assert_eq!(read().ok().unwrap().body, vec![b'a'; 32]);
        assert_over_the_limit(read().err().unwrap());
    }

    /// A 502 `api_error` naming the limit, not an empty successful answer.
    fn assert_over_the_limit(error: KimiError) {
        assert_eq!(error.status, 502);
        assert_eq!(
            error.detail.as_deref(),
            Some("Kimi response body exceeded 32 bytes")
        );
        assert_eq!(error.retry_after, None);
        let response = super::super::map_kimi_error_to_response(&error);
        assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
    }
}
