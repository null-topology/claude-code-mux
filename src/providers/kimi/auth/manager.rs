use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::constants::{CLIENT_ID, REFRESH_MARGIN_MS, oauth_host};
use super::headers::common_headers;
use super::jwt::extract_user_id;
use super::login::TokenResponse;
use super::token_store::{KimiTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const MAX_REFRESH_ATTEMPTS: u32 = 3;
const RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 504];
/// The most of a refresh answer read.
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
/// The most of a refusal kept for its message.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// The timeout of the refresh client, and the deadline for reading a body:
/// the 30 s reqwest's blocking client applies by default.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct KimiAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: KimiTokenStore<S>,
    cached: Arc<Mutex<Option<StoredAuth>>>,
}

impl<S: AuthStorage<StoredAuth>> KimiAuthManager<S> {
    pub fn new(store: KimiTokenStore<S>) -> Self {
        Self {
            store,
            cached: Arc::new(Mutex::new(None)),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let cached = {
            let guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            guard.clone()
        };
        let stored = match cached {
            Some(ref auth) => auth.clone(),
            None => {
                let loaded = self.store.load_auth()?;
                match loaded {
                    Some(auth) => {
                        let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                        *guard = Some(auth.clone());
                        auth
                    }
                    None => {
                        anyhow::bail!("Not authenticated. Run: claude-code-mux kimi auth login");
                    }
                }
            }
        };

        if stored.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        self.refresh_now(&stored)
    }

    pub fn force_refresh(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = {
            let guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            guard.clone()
        };
        let stored = match stored {
            Some(auth) => auth,
            None => {
                let loaded = self.store.load_auth()?;
                loaded.ok_or_else(|| anyhow::anyhow!("Not authenticated"))?
            }
        };
        self.refresh_now(&stored)
    }

    fn refresh_now(&self, current: &StoredAuth) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let headers = common_headers()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()?;
        self.refresh_at(&client, &oauth_host(), &headers, current)
    }

    fn refresh_at(
        &self,
        client: &reqwest::blocking::Client,
        oauth_host: &str,
        headers: &std::collections::HashMap<String, String>,
        current: &StoredAuth,
    ) -> Result<StoredAuth, anyhow::Error> {
        for attempt in 0..MAX_REFRESH_ATTEMPTS {
            let form = [
                ("client_id", CLIENT_ID.to_string()),
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", current.refresh.clone()),
            ];

            let resp = match client
                .post(format!("{oauth_host}/api/oauth/token"))
                .headers(build_headers_map(headers))
                .form(&form)
                .send()
            {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                        let ms = 2u64.pow(attempt) * 1000;
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        continue;
                    }
                    let err = crate::upstream_http::error_text(err);
                    anyhow::bail!("refresh network error: {err}");
                }
            };

            if resp.status().as_u16() == 200 {
                let tokens: TokenResponse = crate::upstream_http::read_blocking_json_capped(
                    resp,
                    MAX_TOKEN_RESPONSE_BYTES,
                    HTTP_TIMEOUT,
                )?;
                let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(900) as u64 * 1000);
                let next = StoredAuth {
                    access: tokens.access_token.clone(),
                    refresh: tokens
                        .refresh_token
                        .unwrap_or_else(|| current.refresh.clone()),
                    expires,
                    scope: tokens.scope.clone(),
                    user_id: extract_user_id(&tokens.access_token)
                        .or_else(|| current.user_id.clone()),
                };
                self.store.save_auth(next.clone())?;
                {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    *guard = Some(next.clone());
                }
                return Ok(next);
            }

            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                {
                    let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
                    *guard = None;
                }
                let _ = self.store.clear_auth();
                let err_msg = crate::upstream_http::read_blocking_body_capped(
                    resp,
                    MAX_ERROR_BODY_BYTES,
                    HTTP_TIMEOUT,
                )
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
                anyhow::bail!("{err_msg}");
            }

            if !RETRYABLE_STATUSES.contains(&status) {
                anyhow::bail!("Token refresh failed: {status}");
            }

            if attempt < MAX_REFRESH_ATTEMPTS - 1 {
                let ms = 2u64.pow(attempt) * 1000;
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }

        anyhow::bail!("Token refresh failed after {MAX_REFRESH_ATTEMPTS} attempts");
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(900) * 1000);
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone().unwrap_or_default(),
            expires,
            scope: tokens.scope.clone(),
            user_id: extract_user_id(&tokens.access_token),
        };
        self.store.save_auth(auth.clone())?;
        {
            let mut guard = self.cached.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            *guard = Some(auth.clone());
        }
        Ok(auth)
    }

    pub fn reset_cache(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }
}

fn build_headers_map(
    headers: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
        {
            map.insert(name, value);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    fn test_store() -> KimiTokenStore<InMemoryAuthStore<StoredAuth>> {
        KimiTokenStore::new(InMemoryAuthStore::new())
    }

    #[test]
    fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            scope: Some("openid".into()),
            user_id: Some("user1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = KimiAuthManager::new(store);
        let result = manager.get_auth().unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.user_id.as_deref(), Some("user1"));
    }

    #[test]
    fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = KimiAuthManager::new(store);
        assert!(manager.get_auth().is_err());
    }

    fn stale_auth() -> StoredAuth {
        StoredAuth {
            access: "toy-old-access".into(),
            refresh: "toy-refresh".into(),
            expires: 0,
            scope: None,
            user_id: None,
        }
    }

    fn refresh_from(host: &str) -> Result<StoredAuth, anyhow::Error> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        KimiAuthManager::new(test_store()).refresh_at(
            &client,
            host,
            &std::collections::HashMap::new(),
            &stale_auth(),
        )
    }

    #[test]
    fn a_refresh_that_cannot_connect_carries_no_url() {
        let url = crate::upstream_http::test_support::unanswered_secret_url();
        let error = refresh_from(&url).unwrap_err();
        crate::upstream_http::test_support::assert_no_url(&format!("{error:?}"));
    }

    #[test]
    fn a_refresh_answer_is_read_up_to_the_limit() {
        use crate::upstream_http::test_support::{
            padded_json, serve_in_order, serve_in_order_undeclared,
        };
        let token = r#"{"access_token":"toy-new-access"}"#;

        for serve in [serve_in_order, serve_in_order_undeclared] {
            let host = serve(vec![(200, padded_json(token, MAX_TOKEN_RESPONSE_BYTES))]);
            assert_eq!(refresh_from(&host).unwrap().access, "toy-new-access");

            let host = serve(vec![(
                200,
                padded_json(token, MAX_TOKEN_RESPONSE_BYTES + 1),
            )]);
            let error = refresh_from(&host).unwrap_err().to_string();
            assert_eq!(error, "response body exceeded 1048576 bytes");
        }
    }

    #[test]
    fn an_unauthorized_refresh_body_is_kept_up_to_the_limit() {
        use crate::upstream_http::test_support::serve_in_order;

        let host = serve_in_order(vec![(401, vec![b'e'; MAX_ERROR_BODY_BYTES])]);
        let error = refresh_from(&host).unwrap_err().to_string();
        assert_eq!(error, "e".repeat(MAX_ERROR_BODY_BYTES));

        let host = serve_in_order(vec![(401, vec![b'e'; MAX_ERROR_BODY_BYTES + 1])]);
        let error = refresh_from(&host).unwrap_err().to_string();
        assert_eq!(error, "Token refresh unauthorized");
    }
}
