use serde::Deserialize;

use super::constants::{CLIENT_ID, oauth_host};
use super::headers::common_headers;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DeviceAuthResponse {
    user_code: String,
    device_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const POLL_SAFETY_MARGIN_MS: u64 = 500;
/// The most of a device or token answer read.
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
/// The most of an error answer kept for its message.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

pub fn run_device_login() -> Result<TokenResponse, anyhow::Error> {
    let headers = common_headers()?;
    let client = reqwest::blocking::Client::new();
    device_login_at(&client, &oauth_host(), &headers)
}

fn device_login_at(
    client: &reqwest::blocking::Client,
    oauth_host: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Result<TokenResponse, anyhow::Error> {
    use crate::upstream_http::{
        blocking_error_text, read_blocking_body_capped, read_blocking_json_capped, url_free_error,
    };

    let init_resp = client
        .post(format!("{oauth_host}/api/oauth/device_authorization"))
        .headers(build_header_map(headers))
        .form(&[("client_id", CLIENT_ID)])
        .send()
        .map_err(url_free_error)?;

    if !init_resp.status().is_success() {
        let status = init_resp.status();
        let text = blocking_error_text(init_resp, MAX_ERROR_BODY_BYTES);
        anyhow::bail!("Device authorization failed: {status} {text}");
    }

    let auth: DeviceAuthResponse = read_blocking_json_capped(init_resp, MAX_TOKEN_RESPONSE_BYTES)?;
    let interval_ms = (auth.interval.unwrap_or(5).max(1)) * 1000;

    eprintln!();
    eprintln!("Visit: {}", auth.verification_uri_complete);
    eprintln!("Code:  {}", auth.user_code);
    eprintln!();

    loop {
        let resp = client
            .post(format!("{oauth_host}/api/oauth/token"))
            .headers(build_header_map(headers))
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", &auth.device_code),
                ("grant_type", GRANT_DEVICE_CODE),
            ])
            .send()
            .map_err(url_free_error)?;

        let status = resp.status();
        if status.as_u16() == 200 {
            return read_blocking_json_capped::<TokenResponse>(resp, MAX_TOKEN_RESPONSE_BYTES);
        }

        let body: serde_json::Value = read_blocking_body_capped(resp, MAX_ERROR_BODY_BYTES)
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or(serde_json::json!({}));
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match error {
            "expired_token" => {
                anyhow::bail!("Device code expired. Run login again.");
            }
            "authorization_pending" | "slow_down" => {
                std::thread::sleep(std::time::Duration::from_millis(
                    interval_ms + POLL_SAFETY_MARGIN_MS,
                ));
                continue;
            }
            _ => {
                let desc = body
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                anyhow::bail!(
                    "Device token poll failed ({}): {}{}",
                    status,
                    error,
                    if desc.is_empty() {
                        "".to_string()
                    } else {
                        format!(" - {desc}")
                    }
                );
            }
        }
    }
}

fn build_header_map(
    headers: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if let Ok(name) = HeaderName::from_bytes(k.as_bytes())
            && let Ok(value) = HeaderValue::from_str(v)
        {
            map.insert(name, value);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream_http::test_support::{
        assert_no_url, padded_json, refused_secret_url, serve_in_order,
    };

    const DEVICE: &str = r#"{"user_code":"ABCD","device_code":"dev","verification_uri_complete":"https://example.test/device","interval":1}"#;
    const TOKEN: &str = r#"{"access_token":"toy-access"}"#;

    fn login(host: &str) -> anyhow::Result<TokenResponse> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        device_login_at(&client, host, &std::collections::HashMap::new())
    }

    #[test]
    fn a_failed_device_authorization_send_carries_no_url() {
        let error = login(&refused_secret_url()).unwrap_err();
        assert_no_url(&format!("{error:?}"));
    }

    #[test]
    fn a_failed_token_poll_send_carries_no_url() {
        let host = serve_in_order(vec![(200, DEVICE.as_bytes().to_vec())]);
        let error = login(&host).unwrap_err();
        assert_no_url(&format!("{error:?}"));
    }

    #[test]
    fn device_and_token_answers_of_exactly_the_limit_are_read() {
        let host = serve_in_order(vec![
            (200, padded_json(DEVICE, MAX_TOKEN_RESPONSE_BYTES)),
            (200, padded_json(TOKEN, MAX_TOKEN_RESPONSE_BYTES)),
        ]);
        assert_eq!(login(&host).unwrap().access_token, "toy-access");
    }

    #[test]
    fn a_device_answer_over_the_limit_is_refused() {
        let host = serve_in_order(vec![(
            200,
            padded_json(DEVICE, MAX_TOKEN_RESPONSE_BYTES + 1),
        )]);
        let error = login(&host).unwrap_err().to_string();
        assert_eq!(error, "response body exceeded 1048576 bytes");
    }

    #[test]
    fn a_token_answer_over_the_limit_is_refused() {
        let host = serve_in_order(vec![
            (200, DEVICE.as_bytes().to_vec()),
            (200, padded_json(TOKEN, MAX_TOKEN_RESPONSE_BYTES + 1)),
        ]);
        let error = login(&host).unwrap_err().to_string();
        assert_eq!(error, "response body exceeded 1048576 bytes");
    }

    #[test]
    fn a_device_authorization_error_body_is_kept_up_to_the_limit() {
        let host = serve_in_order(vec![(500, vec![b'e'; MAX_ERROR_BODY_BYTES])]);
        let error = login(&host).unwrap_err().to_string();
        assert!(error.ends_with(&"e".repeat(MAX_ERROR_BODY_BYTES)));

        let host = serve_in_order(vec![(500, vec![b'e'; MAX_ERROR_BODY_BYTES + 1])]);
        let error = login(&host).unwrap_err().to_string();
        assert_eq!(
            error,
            "Device authorization failed: 500 Internal Server Error "
        );
    }

    #[test]
    fn a_token_poll_error_body_is_read_up_to_the_limit() {
        let denied = r#"{"error":"access_denied"}"#;
        let host = serve_in_order(vec![
            (200, DEVICE.as_bytes().to_vec()),
            (400, padded_json(denied, MAX_ERROR_BODY_BYTES)),
        ]);
        let error = login(&host).unwrap_err().to_string();
        assert_eq!(
            error,
            "Device token poll failed (400 Bad Request): access_denied"
        );

        let host = serve_in_order(vec![
            (200, DEVICE.as_bytes().to_vec()),
            (400, padded_json(denied, MAX_ERROR_BODY_BYTES + 1)),
        ]);
        let error = login(&host).unwrap_err().to_string();
        assert_eq!(error, "Device token poll failed (400 Bad Request): unknown");
    }
}
