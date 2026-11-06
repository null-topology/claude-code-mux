use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{
    TokenResponse, extract_account_id, is_codex_cli_readable_jwt, validate_token_response,
};
use super::token_store::{CodexTokenStore, RefreshedTokens, StoredAuth, WritebackError};
use crate::auth::AuthStorage;
use crate::logging::create_logger;

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

const NO_CREDENTIALS: &str =
    "No Codex credentials. Log in with the Codex CLI (`codex login`) to create ~/.codex/auth.json";
const LOGIN_CHANGED_EXPIRED: &str = "The Codex login changed during a token refresh and the stored token has expired or was just rejected. Log in with the Codex CLI (`codex login`)";

/// A refresh failure that may pass: the token endpoint could not be reached or
/// answered 429 or 5xx. Its text carries only a fixed message and the HTTP status,
/// never a response body or a token. Requests answer it with 503.
#[derive(Debug)]
pub(crate) struct TransientRefreshError {
    class: &'static str,
    status: u16,
    message: String,
}

impl TransientRefreshError {
    fn error(class: &'static str, status: u16, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            class,
            status,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for TransientRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransientRefreshError {}

/// A refresh the token endpoint answered and that cannot pass by retrying: a 401 or
/// 403, another non-2xx status, or a response that is not a usable token answer. Like
/// `TransientRefreshError` its text never carries a response body or a token.
#[derive(Debug)]
struct PermanentRefreshError {
    class: &'static str,
    status: u16,
    message: String,
}

impl PermanentRefreshError {
    fn error(class: &'static str, status: u16, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            class,
            status,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for PermanentRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PermanentRefreshError {}

/// The class and HTTP status (0 when none) a failed refresh is logged with.
fn refresh_failure_fields(err: &anyhow::Error) -> (&'static str, u16) {
    if let Some(err) = err.downcast_ref::<TransientRefreshError>() {
        (err.class, err.status)
    } else if let Some(err) = err.downcast_ref::<PermanentRefreshError>() {
        (err.class, err.status)
    } else {
        ("auth", 0)
    }
}

/// Whether two readings of the store hold the same login. The expiry is left out: for
/// an access token without a readable `exp` it is derived from the time of the read.
fn same_credentials(a: &StoredAuth, b: &StoredAuth) -> bool {
    a.access == b.access && a.refresh == b.refresh && a.account_id == b.account_id
}

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<StoredAuth>>>,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    pub(crate) fn new_with_token_endpoint(
        store: CodexTokenStore<S>,
        token_endpoint: String,
    ) -> Self {
        Self {
            store,
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = self
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!(NO_CREDENTIALS))?;

        if stored.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        match self.refresh(false, None).await {
            Ok(auth) => Ok(auth),
            Err(err) => {
                // A proactive refresh that failed in any way keeps using the stored
                // token while it is still valid. The store is read again under
                // `auth.lock`; a file that cannot be read keeps the refresh error.
                match self.load_auth_locked().await {
                    Ok(Some(stored)) if stored.expires > Self::now_ms() => {
                        let (class, status) = refresh_failure_fields(&err);
                        let fields = serde_json::Map::from_iter([
                            ("class".to_string(), serde_json::json!(class)),
                            ("status".to_string(), serde_json::json!(status)),
                        ]);
                        create_logger("codex").warn(
                            "codex_token_refresh_failed_using_stored_token",
                            Some(fields),
                        );
                        Ok(stored)
                    }
                    _ => Err(err),
                }
            }
        }
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        self.refresh(true, Some(rejected_access)).await
    }

    fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(auth));
        }

        self.store.load_auth()
    }

    /// `load_auth`, with the file read under the Codex CLI's `auth.lock` so a write in
    /// progress is seen whole.
    async fn load_auth_locked(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(auth));
        }

        self.store.load_auth_locked().await
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let _refresh_guard = self.refresh_lock.lock().await;

        // Reload from durable storage after acquiring the single-flight lock.
        // Another request may have rotated and persisted the token while this
        // caller was waiting.
        let current = self
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;

        if !force && current.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(current);
        }
        if rejected_access.is_some_and(|access| current.access != access) {
            // Another writer replaced the rejected token while this caller waited.
            return Self::usable_after_login_change(Some(current), rejected_access);
        }

        self.refresh_now(&current, rejected_access).await
    }

    /// Exchange the stored refresh token and write the result back to the login it
    /// came from. No error text carries the token endpoint's response body: it is
    /// controlled by the identity provider and may echo credential material.
    async fn refresh_now(
        &self,
        current: &StoredAuth,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = self
            .refresh_client
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                let (class, what) = if e.is_timeout() {
                    ("timeout", "timed out")
                } else {
                    ("network", "could not reach the token endpoint")
                };
                TransientRefreshError::error(class, 0, format!("Codex token refresh {what}"))
            })?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            // Another writer may have replaced the login while this refresh was in
            // flight. The file is read again under `auth.lock`, so a write in
            // progress is seen whole.
            if let Some(latest) = self.store.load_auth_locked().await?
                && !same_credentials(&latest, current)
            {
                return Self::usable_after_login_change(Some(latest), rejected_access);
            }
            self.store.clear_auth()?;
            return Err(PermanentRefreshError::error(
                "rejected",
                status,
                format!(
                    "Codex token refresh was rejected (HTTP {status}): the stored refresh token is no longer valid. Log in with the Codex CLI (`codex login`)"
                ),
            ));
        }

        if status == 429 || resp.status().is_server_error() {
            return Err(TransientRefreshError::error(
                "status",
                status,
                format!("Codex token refresh failed (HTTP {status})"),
            ));
        }

        if !resp.status().is_success() {
            return Err(PermanentRefreshError::error(
                "status",
                status,
                format!("Codex token refresh failed (HTTP {status})"),
            ));
        }

        let body = resp.bytes().await.map_err(|_| {
            TransientRefreshError::error(
                "network",
                status,
                format!("Codex token refresh response could not be read (HTTP {status})"),
            )
        })?;
        let tokens: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
            PermanentRefreshError::error(
                "response",
                status,
                format!("Codex token refresh returned a malformed response (HTTP {status})"),
            )
        })?;
        validate_token_response(&tokens)
            .map_err(|err| PermanentRefreshError::error("response", status, err.to_string()))?;
        let refreshed = RefreshedTokens {
            refreshed_access: current.access.clone(),
            refreshed_from: current.refresh.clone(),
            expected_account_id: current.account_id.clone(),
            account_id: extract_account_id(&tokens).or_else(|| current.account_id.clone()),
            refresh_token: tokens.new_refresh_token().map(str::to_string),
            // An id token the Codex CLI could not decode is not written; the stored one
            // stays.
            id_token: tokens
                .new_id_token()
                .filter(|token| is_codex_cli_readable_jwt(token))
                .map(str::to_string),
            expires: tokens.expires_at_ms(Self::now_ms()),
            access_token: tokens.access_token,
        };
        // What the request uses when the refresh is valid but cannot be written.
        let unwritten = refreshed.clone().into_stored(current.account_id.clone());
        match self.store.save_refreshed(refreshed).await {
            Ok(next) => Ok(next),
            Err(WritebackError::LoginChanged) => {
                Self::usable_after_login_change(self.load_auth()?, rejected_access)
            }
            Err(WritebackError::Io(_)) => {
                // The refresh is spent and its tokens are valid, so the request uses
                // them; the file keeps what it held. The error names a path, so the
                // warning carries no detail.
                create_logger("codex").warn("codex_token_writeback_failed", None);
                Ok(unwritten)
            }
            Err(WritebackError::Unusable(err)) => Err(err),
        }
    }

    /// The one check for a login that changed under a refresh (another login, a
    /// logout, a deleted file, a token another writer stored first): what the store
    /// holds now is used only while it is unexpired and, on the forced path, is not
    /// the access token the backend just rejected. Otherwise the request fails with an
    /// error naming `codex login`. Nothing is written.
    fn usable_after_login_change(
        stored: Option<StoredAuth>,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        match stored {
            Some(stored)
                if stored.expires > Self::now_ms()
                    && rejected_access != Some(stored.access.as_str()) =>
            {
                Ok(stored)
            }
            Some(_) => Err(anyhow::anyhow!(LOGIN_CHANGED_EXPIRED)),
            None => Err(anyhow::anyhow!(NO_CREDENTIALS)),
        }
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        validate_token_response(tokens)?;
        let account_id = extract_account_id(tokens);
        let expires = tokens.expires_at_ms(Self::now_ms());
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.new_refresh_token().unwrap_or_default().to_string(),
            expires,
            account_id,
        };
        self.store.save_auth(auth.clone())?;
        Ok(auth)
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(auth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    /// Accept the one token request a mock endpoint expects. A request that never
    /// arrives, or never finishes arriving, fails the test after ten seconds with a
    /// message saying so instead of hanging it.
    fn accept_within_deadline(listener: &TcpListener) -> std::net::TcpStream {
        const DEADLINE: Duration = Duration::from_secs(10);
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + DEADLINE;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(DEADLINE)).unwrap();
                    return stream;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "mock token endpoint: no token request arrived within {DEADLINE:?}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("mock token endpoint: accept failed: {err}"),
            }
        }
    }

    #[tokio::test]
    async fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().await.unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[tokio::test]
    async fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        assert!(manager.get_auth().await.is_err());
        assert!(
            manager
                .get_auth()
                .await
                .unwrap_err()
                .to_string()
                .contains("No Codex credentials")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_expired_auth_refreshes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = refreshes.clone();
        let server = thread::spawn(move || {
            let mut stream = accept_within_deadline(&listener);
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("refresh_token=stale"));
            server_refreshes.fetch_add(1, Ordering::SeqCst);

            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "expired".into(),
                refresh: "stale".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{addr}/oauth/token"),
        ));
        let (first, second) = tokio::join!(manager.get_auth(), manager.get_auth());
        let results = [first.unwrap(), second.unwrap()];
        server.join().unwrap();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|auth| auth.access == "rotated"));
        assert!(results.iter().all(|auth| auth.refresh == "rotated-refresh"));
    }

    #[tokio::test]
    async fn stale_401_reuses_already_rotated_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let auth = manager.force_refresh("rejected").await.unwrap();
        assert_eq!(auth.access, "rotated");
        assert_eq!(auth.refresh, "rotated-refresh");
    }

    #[tokio::test]
    async fn unauthorized_refresh_preserves_changed_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backing = InMemoryAuthStore::new();
        let server_backing = backing.clone();
        let server = thread::spawn(move || {
            let mut stream = accept_within_deadline(&listener);
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_backing
                .save(StoredAuth {
                    access: "same-access".into(),
                    refresh: "replacement-refresh".into(),
                    expires: u64::MAX,
                    account_id: Some("acct_1".into()),
                })
                .unwrap();
            let body = b"rejected refresh token";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = CodexTokenStore::new(backing);
        store
            .save_auth(StoredAuth {
                access: "same-access".into(),
                refresh: "rejected-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, "same-access");
        assert_eq!(auth.refresh, "replacement-refresh");
        assert_eq!(manager.store.load_auth().unwrap(), Some(auth));
    }

    #[tokio::test]
    async fn durable_rotation_and_logout_are_observed_by_shared_manager() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first".into(),
                refresh: "first-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);
        assert_eq!(manager.get_auth().await.unwrap().access, "first");

        manager
            .store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_2".into()),
            })
            .unwrap();
        let rotated = manager.get_auth().await.unwrap();
        assert_eq!(rotated.access, "rotated");
        assert_eq!(rotated.account_id.as_deref(), Some("acct_2"));

        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().await.is_err());
    }

    use super::super::token_store::CodexCliAuthStore;
    use base64::Engine;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    const MARKER: &str = "MARKER-7f3c-body-echo";
    // {"email":"user@example.com"} as an unpadded URL-safe payload.
    const NEW_ID_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20ifQ.sig";

    /// A toy access token whose `exp` lies `secs` seconds from now (negative: past).
    fn jwt_expiring_in(secs: i64) -> String {
        let now = (super::CodexAuthManager::<CodexCliAuthStore>::now_ms() / 1000) as i64;
        let payload = json!({ "exp": now + secs }).to_string();
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        )
    }

    fn auth_doc(access: &str, refresh: &str) -> serde_json::Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "future_key": {"kept": true},
            "tokens": {
                "id_token": "old.id.tok",
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct_file"
            },
            "last_refresh": "2026-07-13T08:00:00Z"
        })
    }

    fn write_doc(path: &Path, doc: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec_pretty(doc).unwrap()).unwrap();
    }

    fn read_doc(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Answer one token request with `status` and `body`, running `during` after the
    /// request arrived and before the answer goes out.
    fn token_endpoint(
        status: &'static str,
        body: String,
        during: impl FnOnce() + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut stream = accept_within_deadline(&listener);
            // Read the whole request, so the answer never races the client's write.
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream
                    .read(&mut chunk)
                    .expect("mock token endpoint: the token request did not finish arriving");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
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
            assert!(!request.is_empty());
            during();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}/oauth/token"), server)
    }

    fn file_manager(path: &Path, endpoint: String) -> CodexAuthManager<CodexCliAuthStore> {
        file_manager_with(CodexCliAuthStore::new(path.to_path_buf()), endpoint)
    }

    fn file_manager_with(
        store: CodexCliAuthStore,
        endpoint: String,
    ) -> CodexAuthManager<CodexCliAuthStore> {
        CodexAuthManager::new_with_token_endpoint(CodexTokenStore::codex_cli(store), endpoint)
    }

    fn auth_file(dir: &tempfile::TempDir, access: &str, refresh: &str) -> PathBuf {
        let path = dir.path().join("auth.json");
        write_doc(&path, &auth_doc(access, refresh));
        path
    }

    fn rotated_body() -> String {
        json!({
            "access_token": jwt_expiring_in(3600),
            "refresh_token": "refresh-new",
            "expires_in": 3600
        })
        .to_string()
    }

    #[tokio::test]
    async fn refresh_does_not_recreate_a_deleted_auth_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let removed = path.clone();
        let (endpoint, server) = token_endpoint("200 OK", rotated_body(), move || {
            std::fs::remove_file(&removed).unwrap();
        });
        let manager = file_manager(&path, endpoint);

        let err = manager.get_auth().await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("codex login"), "{err}");
        assert!(
            !path.exists(),
            "a logout must not be undone by a late refresh"
        );
    }

    #[tokio::test]
    async fn refresh_leaves_another_login_untouched_and_uses_its_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let newer_access = jwt_expiring_in(7200);
        let newer = auth_doc(&newer_access, "refresh-from-a-new-login");
        let relogin_path = path.clone();
        let (endpoint, server) = token_endpoint("200 OK", rotated_body(), move || {
            write_doc(&relogin_path, &newer);
        });
        let manager = file_manager(&path, endpoint);
        let before_refresh = auth_doc(&newer_access, "refresh-from-a-new-login");

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, newer_access);
        assert_eq!(auth.refresh, "refresh-from-a-new-login");
        assert_eq!(read_doc(&path), before_refresh);
    }

    #[tokio::test]
    async fn refresh_leaves_another_account_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let mut other = auth_doc(&jwt_expiring_in(-60), "refresh-old");
        other["tokens"]["account_id"] = json!("acct_other");
        let expected = other.clone();
        let relogin_path = path.clone();
        let (endpoint, server) = token_endpoint("200 OK", rotated_body(), move || {
            write_doc(&relogin_path, &other);
        });
        let manager = file_manager(&path, endpoint);

        // The other account's token has expired as well, so the request fails.
        let err = manager.get_auth().await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("codex login"), "{err}");
        assert_eq!(read_doc(&path), expected);
    }

    #[tokio::test]
    async fn refresh_without_refresh_token_keeps_the_stored_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let new_access = jwt_expiring_in(3600);
        let body = json!({ "access_token": new_access, "refresh_token": "" }).to_string();
        let (endpoint, server) = token_endpoint("200 OK", body, || {});
        let manager = file_manager(&path, endpoint);

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, new_access);
        assert_eq!(auth.refresh, "refresh-old");
        assert_eq!(auth.account_id.as_deref(), Some("acct_file"));

        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(new_access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-old");
        assert_eq!(doc["tokens"]["id_token"], "old.id.tok");
        assert_eq!(doc["tokens"]["account_id"], "acct_file");
        assert_eq!(doc["auth_mode"], "chatgpt");
        assert_eq!(doc["OPENAI_API_KEY"], serde_json::Value::Null);
        assert!(doc.as_object().unwrap().contains_key("OPENAI_API_KEY"));
        assert_eq!(doc["future_key"], json!({"kept": true}));
        assert_ne!(doc["last_refresh"], "2026-07-13T08:00:00Z");
    }

    #[tokio::test]
    async fn refresh_writes_a_returned_id_token_as_the_raw_jwt() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let body = json!({
            "access_token": jwt_expiring_in(3600),
            "refresh_token": "refresh-new",
            "id_token": NEW_ID_TOKEN
        })
        .to_string();
        let (endpoint, server) = token_endpoint("200 OK", body, || {});
        let manager = file_manager(&path, endpoint);

        manager.get_auth().await.unwrap();
        server.join().unwrap();
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["id_token"], NEW_ID_TOKEN);
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
    }

    /// A toy JWT whose payload is `payload`.
    fn jwt_with_claims(payload: &str) -> String {
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        )
    }

    #[tokio::test]
    async fn refresh_keeps_the_stored_id_token_when_the_new_one_is_unreadable() {
        let unreadable = [
            "not-a-jwt".to_string(),
            jwt_with_claims(r#"{"email":42}"#),
            jwt_with_claims(r#"{"https://api.openai.com/profile":"user@example.com"}"#),
            jwt_with_claims(r#"{"https://api.openai.com/profile":{"email":false}}"#),
            jwt_with_claims(r#"{"https://api.openai.com/auth":"acct"}"#),
            jwt_with_claims(r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":5}}"#),
            jwt_with_claims(r#"{"https://api.openai.com/auth":{"chatgpt_user_id":7}}"#),
            jwt_with_claims(r#"{"https://api.openai.com/auth":{"user_id":[]}}"#),
            jwt_with_claims(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":1}}"#),
            jwt_with_claims(
                r#"{"https://api.openai.com/auth":{"chatgpt_account_is_fedramp":"yes"}}"#,
            ),
        ];
        for id_token in unreadable {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
            let body = json!({
                "access_token": jwt_expiring_in(3600),
                "refresh_token": "refresh-new",
                "id_token": id_token
            })
            .to_string();
            let (endpoint, server) = token_endpoint("200 OK", body, || {});
            let manager = file_manager(&path, endpoint);

            manager.get_auth().await.unwrap();
            server.join().unwrap();
            let doc = read_doc(&path);
            assert_eq!(doc["tokens"]["id_token"], "old.id.tok", "{id_token}");
            assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
        }
    }

    #[tokio::test]
    async fn rejected_refresh_error_never_carries_the_response_body() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
            let body = json!({ "error": "invalid_grant", "echo": MARKER }).to_string();
            let (endpoint, server) = token_endpoint(status, body, || {});
            let manager = file_manager(&path, endpoint);

            let err = manager.get_auth().await.unwrap_err();
            server.join().unwrap();
            let shown = format!("{err} {err:#} {err:?}");
            assert!(!shown.contains(MARKER), "{shown}");
            assert!(shown.contains(&status[..3]), "{shown}");
            assert!(shown.contains("codex login"), "{shown}");
            assert!(path.exists());
        }
    }

    #[tokio::test]
    async fn permanent_refresh_failures_fail_without_the_body() {
        let bodies = [
            (
                "400 Bad Request",
                json!({ "error": "invalid_grant", "echo": MARKER }).to_string(),
            ),
            ("404 Not Found", MARKER.to_string()),
            (
                "200 OK",
                format!("{{\"access_token\": 5, \"echo\": \"{MARKER}\"}}"),
            ),
            ("200 OK", format!("not json {MARKER}")),
        ];
        for (status, body) in bodies {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
            let before = std::fs::read(&path).unwrap();
            let (endpoint, server) = token_endpoint(status, body, || {});
            let manager = file_manager(&path, endpoint);

            let err = manager.get_auth().await.unwrap_err();
            server.join().unwrap();
            let shown = format!("{err} {err:#} {err:?}");
            assert!(!shown.contains(MARKER), "{status}: {shown}");
            assert!(shown.contains(&status[..3]), "{status}: {shown}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn transient_failure_in_refresh_window_keeps_the_valid_stored_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let in_window = jwt_expiring_in(60);
        let path = auth_file(&dir, &in_window, "refresh-old");
        let before = std::fs::read(&path).unwrap();

        for status in ["503 Service Unavailable", "429 Too Many Requests"] {
            let (endpoint, server) = token_endpoint(status, MARKER.to_string(), || {});
            let auth = file_manager(&path, endpoint).get_auth().await.unwrap();
            server.join().unwrap();
            assert_eq!(auth.access, in_window, "{status}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }

        // An unreachable token endpoint is transient as well.
        let auth = file_manager(&path, "http://127.0.0.1:1/oauth/token".into())
            .get_auth()
            .await
            .unwrap();
        assert_eq!(auth.access, in_window);

        // The forced refresh after an upstream 401 fails instead.
        let (endpoint, server) =
            token_endpoint("503 Service Unavailable", MARKER.to_string(), || {});
        let err = file_manager(&path, endpoint)
            .force_refresh(&in_window)
            .await
            .unwrap_err();
        server.join().unwrap();
        let shown = format!("{err:#}");
        assert!(shown.contains("503"), "{shown}");
        assert!(!shown.contains(MARKER), "{shown}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn transient_failure_with_an_expired_token_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let (endpoint, server) = token_endpoint("502 Bad Gateway", MARKER.to_string(), || {});

        let err = file_manager(&path, endpoint).get_auth().await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("502"), "{err}");
    }

    #[tokio::test]
    async fn permanent_failure_in_refresh_window_keeps_the_valid_stored_token() {
        let bodies = [
            (
                "400 Bad Request",
                json!({ "error": "invalid_grant" }).to_string(),
            ),
            (
                "401 Unauthorized",
                json!({ "error": "invalid_grant" }).to_string(),
            ),
            ("200 OK", "not json".to_string()),
        ];
        for (status, body) in bodies {
            let dir = tempfile::TempDir::new().unwrap();
            let in_window = jwt_expiring_in(60);
            let path = auth_file(&dir, &in_window, "refresh-old");
            let before = std::fs::read(&path).unwrap();
            let (endpoint, server) = token_endpoint(status, body.clone(), || {});

            let auth = file_manager(&path, endpoint).get_auth().await.unwrap();
            server.join().unwrap();
            assert_eq!(auth.access, in_window, "{status}");
            assert_eq!(std::fs::read(&path).unwrap(), before);

            // The forced refresh after an upstream 401 never falls back.
            let (endpoint, server) = token_endpoint(status, body, || {});
            let err = file_manager(&path, endpoint)
                .force_refresh(&in_window)
                .await
                .unwrap_err();
            server.join().unwrap();
            assert!(err.to_string().contains(&status[..3]), "{status}: {err}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn forced_refresh_refuses_an_expired_token_another_writer_stored() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "stored-by-another-writer".into(),
                refresh: "other-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let err = manager.force_refresh("rejected").await.unwrap_err();
        assert!(err.to_string().contains("codex login"), "{err}");
    }

    #[tokio::test]
    async fn forced_refresh_after_a_401_refuses_what_another_writer_left_unusable() {
        let rejected = jwt_expiring_in(3600);
        // Another writer stores a new refresh token but keeps the rejected access
        // token, or stores a login whose access token has already expired.
        let others = [
            auth_doc(&rejected, "refresh-other"),
            auth_doc(&jwt_expiring_in(-60), "refresh-other"),
        ];
        for other in others {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &rejected, "refresh-old");
            let relogin_path = path.clone();
            let expected = other.clone();
            let (endpoint, server) =
                token_endpoint("401 Unauthorized", "{}".to_string(), move || {
                    write_doc(&relogin_path, &other);
                });

            let err = file_manager(&path, endpoint)
                .force_refresh(&rejected)
                .await
                .unwrap_err();
            server.join().unwrap();
            assert!(err.to_string().contains("codex login"), "{err}");
            assert_eq!(read_doc(&path), expected);
        }
    }

    fn lock_auth_dir(dir: &Path) -> std::fs::File {
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("auth.lock"))
            .unwrap();
        holder.lock().unwrap();
        holder
    }

    #[tokio::test]
    async fn rejected_refresh_rereads_the_file_under_auth_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let old_access = jwt_expiring_in(-60);
        let path = auth_file(&dir, &old_access, "refresh-old");
        let newer_access = jwt_expiring_in(7200);
        let newer = auth_doc(&newer_access, "refresh-from-a-new-login");
        let (lock_dir, relogin_path) = (dir.path().to_path_buf(), path.clone());
        // Another writer holds auth.lock while the refresh is in flight and stores a
        // new login only after the 401 went out.
        let (endpoint, server) = token_endpoint("401 Unauthorized", "{}".to_string(), move || {
            let holder = lock_auth_dir(&lock_dir);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                write_doc(&relogin_path, &newer);
                drop(holder);
            });
        });

        let auth = file_manager(&path, endpoint)
            .force_refresh(&old_access)
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, newer_access);
        assert_eq!(auth.refresh, "refresh-from-a-new-login");
    }

    #[tokio::test]
    async fn a_non_rotating_refresh_keeps_another_writers_access_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-shared");
        // The other writer refreshed from the same refresh token, which did not rotate,
        // and stored its access token first.
        let other_access = jwt_expiring_in(7200);
        let other = auth_doc(&other_access, "refresh-shared");
        let expected = other.clone();
        let relogin_path = path.clone();
        let body = json!({ "access_token": jwt_expiring_in(3600) }).to_string();
        let (endpoint, server) = token_endpoint("200 OK", body, move || {
            write_doc(&relogin_path, &other);
        });

        let auth = file_manager(&path, endpoint).get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, other_access);
        assert_eq!(read_doc(&path), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refresh_that_cannot_be_written_serves_the_new_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        drop(lock_auth_dir(dir.path()));
        let before = std::fs::read(&path).unwrap();
        // A directory the new file cannot be created in.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let probe = dir.path().join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            // Permissions are not enforced for this user; nothing to observe.
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let new_access = jwt_expiring_in(3600);
        let body =
            json!({ "access_token": new_access, "refresh_token": "refresh-new" }).to_string();
        let (endpoint, server) = token_endpoint("200 OK", body, || {});

        let result = file_manager(&path, endpoint).get_auth().await;
        server.join().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = result.unwrap();
        assert_eq!(auth.access, new_access);
        assert_eq!(auth.refresh, "refresh-new");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn refresh_treats_wrongly_typed_optional_fields_as_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let new_access = jwt_expiring_in(3600);
        let body = json!({
            "access_token": new_access,
            "refresh_token": 5,
            "id_token": {"not": "a token"},
            "expires_in": "3600"
        })
        .to_string();
        let (endpoint, server) = token_endpoint("200 OK", body, || {});

        let auth = file_manager(&path, endpoint).get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, new_access);
        assert_eq!(auth.refresh, "refresh-old");
        assert_eq!(
            Some(auth.expires),
            super::super::jwt::token_exp_ms(&new_access)
        );
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(new_access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-old");
        assert_eq!(doc["tokens"]["id_token"], "old.id.tok");
    }
}
