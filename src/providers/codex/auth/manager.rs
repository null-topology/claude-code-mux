use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{
    TokenResponse, extract_account_id, is_codex_cli_readable_jwt, validate_token_response,
};
use super::token_store::{CodexTokenStore, RefreshedTokens, StoredAuth, WritebackError};
use crate::auth::AuthStorage;
use crate::logging::create_logger;
use crate::upstream_http::read_body_capped;

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

/// Tokens a refresh obtained but could not write back, per store path. They stand in
/// for the login they replaced (`refreshed_access`, `refreshed_from` and, when the
/// store names one too, `expected_account_id`) while the store still holds it, and
/// are dropped once it holds anything else, a document they cannot be matched to
/// included, or once the token endpoint refused their refresh token and their access
/// token has expired. They live in memory only and are never logged.
static UNWRITTEN_REFRESHES: LazyLock<Mutex<HashMap<String, Unwritten>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The last failed proactive refresh, per store path.
static PROACTIVE_REFRESH_FAILURES: LazyLock<Mutex<HashMap<String, RefreshFailure>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per store path, a channel the next `refresh` signals once it has read the file
/// without `auth.lock`, so a test can change the file only after that read.
#[cfg(test)]
static UNLOCKED_RELOAD_SIGNALS: LazyLock<Mutex<HashMap<String, std::sync::mpsc::Sender<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long the proactive refresh is not tried again after a transient failure.
const TRANSIENT_REFRESH_BACKOFF: Duration = Duration::from_secs(60);

/// How long tokens that could not be written back wait before the next write.
const WRITEBACK_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The largest token endpoint answer read; a longer one fails the refresh as
/// an unreadable answer does.
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;

struct RefreshFailure {
    access: String,
    at: Instant,
    transient: bool,
}

/// An entry of `UNWRITTEN_REFRESHES`: the tokens, and when they are written again.
struct Unwritten {
    tokens: RefreshedTokens,
    retry_at: Instant,
}

fn unwritten_refreshes() -> MutexGuard<'static, HashMap<String, Unwritten>> {
    UNWRITTEN_REFRESHES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn proactive_refresh_failures() -> MutexGuard<'static, HashMap<String, RefreshFailure>> {
    PROACTIVE_REFRESH_FAILURES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// What the store holds, and what requests use in its place.
struct Loaded {
    /// The store's own reading.
    stored: StoredAuth,
    /// `stored`, or the tokens of a refresh that could not write them back.
    auth: StoredAuth,
    /// Those tokens, when `auth` comes from them.
    unwritten: Option<RefreshedTokens>,
    /// Whether those tokens are due to be written again.
    retry_writeback: bool,
}

impl Loaded {
    fn stored(stored: StoredAuth) -> Self {
        Self {
            auth: stored.clone(),
            stored,
            unwritten: None,
            retry_writeback: false,
        }
    }
}

const NO_CREDENTIALS: &str =
    "No Codex credentials. Log in with the Codex CLI (`codex login`) to create ~/.codex/auth.json";
const LOGIN_CHANGED_EXPIRED: &str = "The Codex login changed during a token refresh and the stored token has expired or was just rejected. Log in with the Codex CLI (`codex login`)";

/// A temporary refresh failure: the token endpoint could not be reached or
/// answered 429 or 5xx, or the refresh task did not finish. Its text carries only a
/// fixed message and the HTTP status, never a response body or a token. Requests
/// answer it with 503.
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

/// A refresh failure that retrying cannot fix: the token endpoint answered 401 or
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
    pub store: Arc<CodexTokenStore<S>>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<StoredAuth>>>,
    #[cfg(test)]
    forced_refreshes: std::sync::atomic::AtomicUsize,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
}

impl<S: AuthStorage<StoredAuth> + 'static> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    pub(crate) fn new_with_token_endpoint(
        store: CodexTokenStore<S>,
        token_endpoint: String,
    ) -> Self {
        Self {
            store: Arc::new(store),
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            forced_refreshes: std::sync::atomic::AtomicUsize::new(0),
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    /// A handle on the same store, lock and endpoint, for the task a refresh runs in.
    fn shared(&self) -> Self {
        Self {
            store: self.store.clone(),
            #[cfg(test)]
            test_auth: self.test_auth.clone(),
            #[cfg(test)]
            forced_refreshes: std::sync::atomic::AtomicUsize::new(0),
            refresh_lock: self.refresh_lock.clone(),
            refresh_client: self.refresh_client.clone(),
            token_endpoint: self.token_endpoint.clone(),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let loaded = self
            .load()?
            .ok_or_else(|| anyhow::anyhow!(NO_CREDENTIALS))?;

        // Tokens that could not be written back are served from here between two
        // writes, without the single-flight lock or `auth.lock`.
        if !loaded.retry_writeback && loaded.auth.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(loaded.auth);
        }

        self.refresh(false, None).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        #[cfg(test)]
        self.forced_refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.refresh(true, Some(rejected_access)).await
    }

    fn load(&self) -> Result<Option<Loaded>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(Loaded::stored(auth)));
        }

        Ok(self.with_unwritten(self.store.load_auth()?))
    }

    /// `load`, with the file read under the Codex CLI's `auth.lock` so a write in
    /// progress is seen whole.
    async fn load_locked(&self) -> Result<Option<Loaded>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(Loaded::stored(auth)));
        }

        let stored = self.store.load_auth_locked().await?;
        Ok(self.with_unwritten(stored))
    }

    /// The store's reading, with the tokens of a refresh that could not write them
    /// back in place of the login they replaced while the store still holds it, as
    /// the writeback decides it (`RefreshedTokens::same_login`). Once it holds
    /// anything else, another account id or a missing file included, those tokens
    /// are dropped.
    fn with_unwritten(&self, stored: Option<StoredAuth>) -> Option<Loaded> {
        let mut slots = unwritten_refreshes();
        let path = self.store.auth_path();
        let Some(entry) = slots.get(&path) else {
            return stored.map(Loaded::stored);
        };
        match stored {
            Some(stored)
                if entry.tokens.same_login(
                    Some(&stored.access),
                    Some(&stored.refresh),
                    stored.account_id.as_deref(),
                ) =>
            {
                let unwritten = entry.tokens.clone();
                Some(Loaded {
                    auth: unwritten.clone().into_stored(stored.account_id.clone()),
                    stored,
                    unwritten: Some(unwritten),
                    retry_writeback: Instant::now() >= entry.retry_at,
                })
            }
            stored => {
                slots.remove(&path);
                stored.map(Loaded::stored)
            }
        }
    }

    /// Keep tokens that could not be written back, to be written again once
    /// `WRITEBACK_RETRY_INTERVAL` has passed.
    fn keep_unwritten(&self, refreshed: RefreshedTokens) {
        unwritten_refreshes().insert(
            self.store.auth_path(),
            Unwritten {
                tokens: refreshed,
                retry_at: Instant::now() + WRITEBACK_RETRY_INTERVAL,
            },
        );
    }

    fn forget_unwritten(&self) {
        unwritten_refreshes().remove(&self.store.auth_path());
    }

    /// After the token endpoint refused a refresh, tokens not yet written back keep
    /// serving requests while their access token is unexpired, as a stored token
    /// does, and are dropped once it has expired. Returns whether they are kept.
    fn keep_unwritten_after_refusal(&self, loaded: &Loaded) -> bool {
        let keep = loaded.unwritten.is_some() && loaded.auth.expires > Self::now_ms();
        if !keep {
            self.forget_unwritten();
        }
        keep
    }

    /// Whether the proactive refresh of `current` failed recently enough to serve it
    /// as it is: within `TRANSIENT_REFRESH_BACKOFF` of a transient failure, and after
    /// any other failure until the access token changes. Only an unexpired token is
    /// served this way.
    fn proactive_refresh_backing_off(&self, current: &StoredAuth) -> bool {
        current.expires > Self::now_ms()
            && proactive_refresh_failures()
                .get(&self.store.auth_path())
                .is_some_and(|failure| {
                    failure.access == current.access
                        && (!failure.transient || failure.at.elapsed() < TRANSIENT_REFRESH_BACKOFF)
                })
    }

    fn note_proactive_refresh(&self, access: &str, result: &Result<StoredAuth, anyhow::Error>) {
        let mut failures = proactive_refresh_failures();
        match result {
            Ok(_) => {
                failures.remove(&self.store.auth_path());
            }
            Err(err) => {
                failures.insert(
                    self.store.auth_path(),
                    RefreshFailure {
                        access: access.to_string(),
                        at: Instant::now(),
                        transient: err.downcast_ref::<TransientRefreshError>().is_some(),
                    },
                );
            }
        }
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let refresh_guard = self.refresh_lock.clone().lock_owned().await;

        // Reload from durable storage after acquiring the single-flight lock.
        // Another request may have rotated and persisted the token while this
        // caller was waiting. A proactive refresh that cannot read the file here (a
        // write caught halfway, for one) reads it again under `auth.lock`
        // (`after_failed_unlocked_reload`).
        let reloaded = self.load();
        #[cfg(test)]
        self.signal_unlocked_reload();
        let loaded = match reloaded {
            Ok(loaded) => loaded.ok_or_else(|| anyhow::anyhow!("Not authenticated"))?,
            Err(err) if !force => return self.after_failed_unlocked_reload(err).await,
            Err(err) => return Err(err),
        };
        if !force && !loaded.retry_writeback && !Self::refresh_due(&loaded.auth) {
            return Ok(loaded.auth);
        }

        // The writeback, the exchange and the fallback run in a task that owns the
        // single-flight lock: a caller that goes away mid-request neither frees the
        // lock early nor loses tokens the endpoint already issued, and the outcome
        // is decided and logged once whether or not anyone still waits for it.
        let worker = self.shared();
        let rejected_access = rejected_access.map(str::to_string);
        let task = tokio::spawn(async move {
            let _refresh_guard: OwnedMutexGuard<()> = refresh_guard;
            worker
                .refresh_holding_lock(loaded, force, rejected_access.as_deref())
                .await
        });
        self.refresh_task_answer(task.await, force).await
    }

    /// The answer of a refresh task. The task decided and logged its own outcome; one
    /// that did not finish is a transient failure, which on a proactive refresh falls
    /// back to the stored token as any other proactive failure does.
    async fn refresh_task_answer(
        &self,
        joined: Result<Result<StoredAuth, anyhow::Error>, tokio::task::JoinError>,
        force: bool,
    ) -> Result<StoredAuth, anyhow::Error> {
        let finished = joined.is_ok();
        match refresh_task_outcome(joined) {
            Err(err) if !finished && !force => self.after_failed_proactive_refresh(err).await,
            answer => answer,
        }
    }

    fn refresh_due(auth: &StoredAuth) -> bool {
        auth.expires <= Self::now_ms() + REFRESH_MARGIN_MS
    }

    /// The part of `refresh` that runs under the single-flight lock.
    async fn refresh_holding_lock(
        &self,
        mut loaded: Loaded,
        force: bool,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        // Tokens not yet written back are written again before anything else, due
        // or not and backing off or not. What follows starts from the store as it
        // is afterwards, so a further exchange writes over the login it now holds.
        if loaded.retry_writeback
            && let Some(unwritten) = loaded.unwritten.take()
        {
            self.retry_write_back(unwritten).await;
            loaded = self
                .load_locked()
                .await?
                .ok_or_else(|| anyhow::anyhow!(NO_CREDENTIALS))?;
        }
        let current = &loaded.auth;

        if !force && !Self::refresh_due(current) {
            return Ok(loaded.auth);
        }
        if rejected_access.is_some_and(|access| current.access != access) {
            // Another writer replaced the rejected token while this caller waited.
            return Self::usable_after_login_change(Some(loaded.auth), rejected_access);
        }
        if !force && self.proactive_refresh_backing_off(current) {
            return Ok(loaded.auth);
        }

        let result = self.refresh_now(&loaded, rejected_access).await;
        if force {
            return result;
        }
        self.note_proactive_refresh(&loaded.auth.access, &result);
        match result {
            Ok(auth) => Ok(auth),
            Err(err) => self.after_failed_proactive_refresh(err).await,
        }
    }

    /// A proactive refresh whose read without `auth.lock` failed reads the store
    /// again under the lock. A token there that is not due is served as it is, with
    /// no warning: no refresh was tried, so none failed. Any other outcome is that of
    /// a failed proactive refresh.
    async fn after_failed_unlocked_reload(
        &self,
        err: anyhow::Error,
    ) -> Result<StoredAuth, anyhow::Error> {
        match self.load_locked().await {
            Ok(Some(reread)) if !Self::refresh_due(&reread.auth) => Ok(reread.auth),
            reread => Self::stored_after_failed_refresh(err, reread),
        }
    }

    /// A proactive refresh that failed in any way keeps using the token in use while
    /// it is still valid, with one warning. The store is read again under
    /// `auth.lock`; a file that cannot be read keeps the refresh error.
    async fn after_failed_proactive_refresh(
        &self,
        err: anyhow::Error,
    ) -> Result<StoredAuth, anyhow::Error> {
        Self::stored_after_failed_refresh(err, self.load_locked().await)
    }

    /// `after_failed_proactive_refresh` once the store has been read again.
    fn stored_after_failed_refresh(
        err: anyhow::Error,
        reread: Result<Option<Loaded>, anyhow::Error>,
    ) -> Result<StoredAuth, anyhow::Error> {
        match reread.map(|loaded| loaded.map(|l| l.auth)) {
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

    /// Write tokens that could not be written back once more. An I/O failure keeps
    /// them for the next interval and logs nothing; once the store holds another
    /// login, or a document that cannot be read and so can no longer be matched to
    /// the login they replaced, they are dropped.
    async fn retry_write_back(&self, unwritten: RefreshedTokens) {
        match self.store.save_refreshed(unwritten.clone()).await {
            Ok(_) | Err(WritebackError::LoginChanged) | Err(WritebackError::Unusable(_)) => {
                self.forget_unwritten();
            }
            Err(WritebackError::Io(_)) => self.keep_unwritten(unwritten),
        }
    }

    /// Exchange the refresh token in use (the stored one, or that of tokens not yet
    /// written back) and write the result back to the login the store holds. No error
    /// text carries the token endpoint's response body: it is controlled by the
    /// identity provider and may echo credential material.
    async fn refresh_now(
        &self,
        loaded: &Loaded,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let (stored, current) = (&loaded.stored, &loaded.auth);
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
                && !same_credentials(&latest, stored)
            {
                self.forget_unwritten();
                return Self::usable_after_login_change(Some(latest), rejected_access);
            }
            // Tokens not yet written back stand in for the stored login, which the
            // store still holds, until their access token expires.
            if !self.keep_unwritten_after_refusal(loaded) {
                self.store.clear_auth()?;
            }
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
            self.keep_unwritten_after_refusal(loaded);
            return Err(PermanentRefreshError::error(
                "status",
                status,
                format!("Codex token refresh failed (HTTP {status})"),
            ));
        }

        let body = read_body_capped(resp, MAX_TOKEN_RESPONSE_BYTES)
            .await
            .map_err(|_| {
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
        // Tokens not yet written back fill in what the answer leaves out, as the
        // stored ones would.
        let unwritten = loaded.unwritten.as_ref();
        let refreshed = RefreshedTokens {
            refreshed_access: stored.access.clone(),
            refreshed_from: stored.refresh.clone(),
            expected_account_id: stored.account_id.clone(),
            account_id: extract_account_id(&tokens).or_else(|| current.account_id.clone()),
            refresh_token: tokens
                .new_refresh_token()
                .map(str::to_string)
                .or_else(|| unwritten.map(|_| current.refresh.clone())),
            // An id token the Codex CLI could not decode is not written; the stored one
            // stays.
            id_token: tokens
                .new_id_token()
                .filter(|token| is_codex_cli_readable_jwt(token))
                .map(str::to_string)
                .or_else(|| unwritten.and_then(|tokens| tokens.id_token.clone())),
            expires: tokens.expires_at_ms(Self::now_ms()),
            access_token: tokens.access_token,
        };
        self.write_back(refreshed, rejected_access).await
    }

    /// Write refreshed tokens back to the login they came from. Tokens that cannot be
    /// written for an I/O reason, `auth.lock` included, are what the request uses and
    /// are kept in memory, to be written again by a later request
    /// (`retry_write_back`).
    async fn write_back(
        &self,
        refreshed: RefreshedTokens,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        // What the request uses when the refresh is valid but cannot be written.
        let unwritten = refreshed
            .clone()
            .into_stored(refreshed.expected_account_id.clone());
        match self.store.save_refreshed(refreshed.clone()).await {
            Ok(next) => {
                self.forget_unwritten();
                Ok(next)
            }
            Err(WritebackError::LoginChanged) => {
                self.forget_unwritten();
                let latest = self.load_locked().await?.map(|loaded| loaded.auth);
                Self::usable_after_login_change(latest, rejected_access)
            }
            Err(WritebackError::Io(_)) => {
                // The refresh is spent and its tokens are valid, so the request uses
                // them; the file keeps what it held. The error names a path, so the
                // warning carries no detail.
                create_logger("codex").warn("codex_token_writeback_failed", None);
                self.keep_unwritten(refreshed);
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

    /// Whether tokens a refresh could not write back are kept for this store.
    #[cfg(test)]
    fn holds_unwritten_refresh(&self) -> bool {
        unwritten_refreshes().contains_key(&self.store.auth_path())
    }

    /// Let the next request write the kept tokens again without waiting out
    /// `WRITEBACK_RETRY_INTERVAL`.
    #[cfg(test)]
    fn allow_writeback_retry(&self) {
        if let Some(entry) = unwritten_refreshes().get_mut(&self.store.auth_path()) {
            entry.retry_at = Instant::now();
        }
    }

    /// Let the access token of the kept tokens expire now instead of waiting out its
    /// lifetime.
    #[cfg(test)]
    fn expire_unwritten_refresh(&self) {
        if let Some(entry) = unwritten_refreshes().get_mut(&self.store.auth_path()) {
            entry.tokens.expires = Self::now_ms().saturating_sub(60_000);
        }
    }

    /// A receiver that gets one message once the next `refresh` of this store has
    /// read the file without `auth.lock`.
    #[cfg(test)]
    fn on_unlocked_reload(&self) -> std::sync::mpsc::Receiver<()> {
        let (signal, reloaded) = std::sync::mpsc::channel();
        UNLOCKED_RELOAD_SIGNALS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.store.auth_path(), signal);
        reloaded
    }

    #[cfg(test)]
    fn signal_unlocked_reload(&self) {
        let signal = UNLOCKED_RELOAD_SIGNALS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.store.auth_path());
        if let Some(signal) = signal {
            let _ = signal.send(());
        }
    }

    /// How many times `force_refresh` was called after a rejected login,
    /// whatever came of it: a token exchange, a newer saved login picked up
    /// without one, or a failure.
    #[cfg(test)]
    pub fn forced_refreshes_for_tests(&self) -> usize {
        self.forced_refreshes
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The result of a spawned refresh task. A task that did not finish (it panicked, or
/// the runtime shut down) is a transient failure: nothing says the login is gone.
fn refresh_task_outcome(
    joined: Result<Result<StoredAuth, anyhow::Error>, tokio::task::JoinError>,
) -> Result<StoredAuth, anyhow::Error> {
    joined.unwrap_or_else(|_| {
        Err(TransientRefreshError::error(
            "task",
            0,
            "Codex token refresh did not finish",
        ))
    })
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
        token_endpoint_framed(status, body, during, false)
    }

    /// [`token_endpoint`] that, when `chunked`, sends `body` with no declared
    /// length, as two chunks of chunked transfer encoding.
    fn token_endpoint_framed(
        status: &'static str,
        body: String,
        during: impl FnOnce() + Send + 'static,
        chunked: bool,
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
            let response = if chunked {
                let (first, second) = body.split_at(body.len() / 2);
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
                    first.len(),
                    second.len()
                )
            } else {
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
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

    /// A token answer of exactly the cap is read; one byte more fails the
    /// refresh as an answer that could not be read does. Sent chunked, the
    /// answer declares no length and each chunk fits on its own, so only the
    /// count of the bytes read refuses it.
    #[tokio::test]
    async fn a_token_answer_is_read_up_to_its_cap() {
        for (length, fits, chunked) in [
            (MAX_TOKEN_RESPONSE_BYTES, true, false),
            (MAX_TOKEN_RESPONSE_BYTES + 1, false, false),
            (MAX_TOKEN_RESPONSE_BYTES, true, true),
            (MAX_TOKEN_RESPONSE_BYTES + 1, false, true),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
            let before = std::fs::read(&path).unwrap();
            let mut body = rotated_body();
            body.push_str(&" ".repeat(length - body.len()));
            let (endpoint, server) = token_endpoint_framed("200 OK", body, || {}, chunked);
            let manager = file_manager(&path, endpoint);

            let result = manager.get_auth().await;
            // Over the cap the client stops reading, so the endpoint may fail
            // to finish its write.
            let _ = server.join();
            if fits {
                assert_eq!(result.unwrap().refresh, "refresh-new");
                continue;
            }
            let err = result.unwrap_err();
            assert!(
                err.downcast_ref::<TransientRefreshError>().is_some(),
                "{err:?}"
            );
            assert_eq!(
                err.to_string(),
                "Codex token refresh response could not be read (HTTP 200)"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn transient_failure_in_refresh_window_keeps_the_valid_stored_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let in_window = jwt_expiring_in(60);
        let path = auth_file(&dir, &in_window, "refresh-old");
        let before = std::fs::read(&path).unwrap();

        // Each case on a store of its own: a failed proactive refresh of a token is not
        // tried again at once.
        for status in ["503 Service Unavailable", "429 Too Many Requests"] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = auth_file(&dir, &in_window, "refresh-old");
            let (endpoint, server) = token_endpoint(status, MARKER.to_string(), || {});
            let auth = file_manager(&path, endpoint).get_auth().await.unwrap();
            server.join().unwrap();
            assert_eq!(auth.access, in_window, "{status}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }

        // An unreachable token endpoint is transient as well.
        let unreachable_dir = tempfile::TempDir::new().unwrap();
        let unreachable_path = auth_file(&unreachable_dir, &in_window, "refresh-old");
        let auth = file_manager(&unreachable_path, "http://127.0.0.1:1/oauth/token".into())
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
        let _log = LOG_LOCK.lock().await;
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

    /// Held by every test that points the proxy log at a directory of its own and by
    /// every test that logs a writeback failure, whose warning has no field to tell
    /// the tests apart by.
    static LOG_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

    /// The proxy log in a directory of its own while it lives.
    struct CapturedLog {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _dir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
    }

    impl CapturedLog {
        async fn start() -> Self {
            let lock = LOG_LOCK.lock().await;
            let dir = tempfile::TempDir::new().unwrap();
            let mut previous = Vec::new();
            for key in ["XDG_STATE_HOME", "LOCALAPPDATA"] {
                previous.push((key, std::env::var_os(key)));
                unsafe {
                    std::env::set_var(key, dir.path());
                }
            }
            Self {
                previous,
                _dir: dir,
                _lock: lock,
            }
        }

        /// The logged events with this message, oldest first.
        fn events(&self, msg: &str) -> Vec<serde_json::Value> {
            std::fs::read_to_string(crate::logging::log_file())
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|event| event["msg"] == msg)
                .collect()
        }
    }

    impl Drop for CapturedLog {
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

    /// A mock token endpoint that answers every request until it is dropped, with the
    /// answers in order and the last one repeated, and keeps each request's body.
    /// With `hold`, each answer waits for one message on it (ten seconds at most).
    struct CountingEndpoint {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CountingEndpoint {
        fn start(
            answers: Vec<(&'static str, String)>,
            hold: Option<std::sync::mpsc::Receiver<()>>,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (seen, stopped) = (requests.clone(), stop.clone());
            thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(err) => panic!("mock token endpoint: accept failed: {err}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let body = read_request_body(&mut stream);
                    let index = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(body);
                        seen.len() - 1
                    };
                    if let Some(hold) = &hold {
                        let _ = hold.recv_timeout(Duration::from_secs(10));
                    }
                    let (status, body) = &answers[index.min(answers.len() - 1)];
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    // A client that went away is not the endpoint's failure.
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self {
                url,
                requests,
                stop,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for CountingEndpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    fn read_request_body(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
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
                    return String::from_utf8_lossy(&request[end + 4..]).into_owned();
                }
            }
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    /// A directory nothing can be created in while this lives, with `auth.lock`
    /// already there; `None` when permissions are not enforced for this user.
    #[cfg(unix)]
    struct ReadOnlyDir(PathBuf);

    #[cfg(unix)]
    impl ReadOnlyDir {
        fn new(dir: &Path) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            drop(lock_auth_dir(dir));
            let read_only = Self(dir.to_path_buf());
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let probe = dir.join("probe");
            if std::fs::write(&probe, b"").is_ok() {
                let _ = std::fs::remove_file(&probe);
                return None;
            }
            Some(read_only)
        }
    }

    #[cfg(unix)]
    impl Drop for ReadOnlyDir {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritten_refresh_serves_later_requests_without_another_exchange() {
        let log = CapturedLog::start().await;
        let dir = tempfile::TempDir::new().unwrap();
        let in_window = jwt_expiring_in(60);
        let path = auth_file(&dir, &in_window, "refresh-old");
        let before = std::fs::read(&path).unwrap();
        let Some(_read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());

        let first = manager.get_auth().await.unwrap();
        // The second request is served from memory until the next write is due;
        // another manager of the process sees the same tokens.
        let second = manager.get_auth().await.unwrap();
        let third = file_manager(&path, endpoint.url.clone())
            .get_auth()
            .await
            .unwrap();
        assert_eq!(endpoint.requests().len(), 1);
        assert_ne!(first.access, in_window);
        assert_eq!(first.refresh, "refresh-new");
        assert_eq!(second, first);
        assert_eq!(third, first);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(manager.holds_unwritten_refresh());

        let warnings = log.events("codex_token_writeback_failed");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].get("fields").is_none(), "{}", warnings[0]);
        let line = warnings[0].to_string();
        for secret in [
            first.access.as_str(),
            in_window.as_str(),
            "refresh-new",
            "refresh-old",
            "acct_file",
            dir.path().to_str().unwrap(),
        ] {
            assert!(!line.contains(secret), "{line}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_later_successful_write_drops_the_unwritten_refresh() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(60), "refresh-old");
        let Some(read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());
        let unwritten = manager.get_auth().await.unwrap();
        assert!(manager.holds_unwritten_refresh());

        drop(read_only);
        manager.allow_writeback_retry();
        let written = manager.get_auth().await.unwrap();
        assert_eq!(written, unwritten);
        assert_eq!(endpoint.requests().len(), 1);
        assert!(!manager.holds_unwritten_refresh());
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(unwritten.access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
        assert_eq!(doc["tokens"]["account_id"], "acct_file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_changed_file_drops_the_unwritten_refresh() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(60), "refresh-old");
        let Some(read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());
        manager.get_auth().await.unwrap();
        assert!(manager.holds_unwritten_refresh());

        drop(read_only);
        let relogin = jwt_expiring_in(7200);
        write_doc(&path, &auth_doc(&relogin, "refresh-relogin"));
        let auth = manager.get_auth().await.unwrap();
        assert_eq!(auth.access, relogin);
        assert_eq!(auth.refresh, "refresh-relogin");
        assert!(!manager.holds_unwritten_refresh());
        assert_eq!(endpoint.requests().len(), 1);
    }

    /// Tokens not yet written back are used only while the file holds the login their
    /// writeback would write over: the same access and refresh token and, when both
    /// name one, the same account id.
    #[tokio::test]
    async fn a_file_naming_another_account_drops_the_unwritten_refresh() {
        // The account id the file names, the one the kept tokens replaced, and
        // whether they are still used.
        let cases = [
            (Some("acct_other"), Some("acct_file"), false),
            (Some("acct_file"), Some("acct_file"), true),
            (None, Some("acct_file"), true),
            (Some("acct_file"), None, true),
        ];
        for (file_account, replaced_account, kept) in cases {
            let dir = tempfile::TempDir::new().unwrap();
            let stored_access = jwt_expiring_in(7200);
            let mut doc = auth_doc(&stored_access, "refresh-old");
            let tokens = doc["tokens"].as_object_mut().unwrap();
            match file_account {
                Some(account) => tokens.insert("account_id".into(), json!(account)),
                None => tokens.remove("account_id"),
            };
            let path = dir.path().join("auth.json");
            write_doc(&path, &doc);
            let manager = file_manager(&path, "http://127.0.0.1:1/should-not-be-called".into());
            let unwritten_access = jwt_expiring_in(3600);
            manager.keep_unwritten(RefreshedTokens {
                refreshed_access: stored_access,
                refreshed_from: "refresh-old".into(),
                expected_account_id: replaced_account.map(str::to_string),
                access_token: unwritten_access.clone(),
                refresh_token: Some("refresh-new".into()),
                id_token: None,
                account_id: replaced_account.map(str::to_string),
                expires: super::super::jwt::token_exp_ms(&unwritten_access).unwrap(),
            });

            let auth = manager.get_auth().await.unwrap();
            let case = format!("file {file_account:?}, replaced {replaced_account:?}");
            if kept {
                assert_eq!(auth.access, unwritten_access, "{case}");
                assert_eq!(auth.refresh, "refresh-new", "{case}");
                assert_eq!(auth.account_id.as_deref(), Some("acct_file"), "{case}");
                assert!(manager.holds_unwritten_refresh(), "{case}");
            } else {
                assert_eq!(Some(auth), manager.store.load_auth().unwrap(), "{case}");
                assert!(!manager.holds_unwritten_refresh(), "{case}");
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritten_refresh_that_is_due_exchanges_its_own_refresh_token() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(60), "refresh-old");
        let Some(read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        // The first answer is itself inside the refresh margin.
        let short = json!({
            "access_token": jwt_expiring_in(120),
            "refresh_token": "refresh-mid",
            "id_token": NEW_ID_TOKEN
        })
        .to_string();
        let endpoint =
            CountingEndpoint::start(vec![("200 OK", short), ("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());
        assert_eq!(manager.get_auth().await.unwrap().refresh, "refresh-mid");

        drop(read_only);
        let auth = manager.get_auth().await.unwrap();
        let requests = endpoint.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].contains("refresh_token=refresh-old"),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].contains("refresh_token=refresh-mid"),
            "{}",
            requests[1]
        );
        assert_eq!(auth.refresh, "refresh-new");
        assert!(!manager.holds_unwritten_refresh());
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(auth.access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
        // The id token of the unwritten refresh is written with the next one.
        assert_eq!(doc["tokens"]["id_token"], NEW_ID_TOKEN);
    }

    #[tokio::test]
    async fn a_caller_that_goes_away_neither_frees_the_lock_nor_loses_the_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let (release, hold) = std::sync::mpsc::channel();
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], Some(hold));
        let manager = file_manager(&path, endpoint.url.clone());

        // The first caller goes away once its request reached the endpoint, which
        // holds the answer.
        let deadline = Instant::now() + Duration::from_secs(10);
        tokio::select! {
            _ = manager.get_auth() => panic!("the first caller finished while its answer was held"),
            () = async {
                while endpoint.requests().is_empty() {
                    assert!(Instant::now() < deadline, "no token request arrived");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => {}
        }

        // A second caller queues behind the exchange still in flight.
        let (second, ()) = tokio::join!(manager.get_auth(), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            release.send(()).unwrap();
        });
        let second = second.unwrap();
        assert_eq!(endpoint.requests().len(), 1);
        assert_eq!(second.refresh, "refresh-new");
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(second.access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
    }

    #[tokio::test]
    async fn a_failed_proactive_refresh_is_not_tried_again_at_once() {
        let log = CapturedLog::start().await;
        for status in ["504 Gateway Timeout", "400 Bad Request"] {
            let dir = tempfile::TempDir::new().unwrap();
            let in_window = jwt_expiring_in(60);
            let path = auth_file(&dir, &in_window, "refresh-old");
            let endpoint = CountingEndpoint::start(vec![(status, MARKER.to_string())], None);
            let manager = file_manager(&path, endpoint.url.clone());

            for _ in 0..2 {
                assert_eq!(
                    manager.get_auth().await.unwrap().access,
                    in_window,
                    "{status}"
                );
            }
            assert_eq!(endpoint.requests().len(), 1, "{status}");
            // The forced refresh after an upstream 401 still asks the endpoint.
            assert!(manager.force_refresh(&in_window).await.is_err(), "{status}");
            assert_eq!(endpoint.requests().len(), 2, "{status}");
        }

        let warnings: Vec<_> = log
            .events("codex_token_refresh_failed_using_stored_token")
            .into_iter()
            .filter(|event| event["fields"]["status"] == 504)
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let fields = warnings[0]["fields"].as_object().unwrap();
        let mut keys: Vec<_> = fields.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["class", "status"]);
        assert_eq!(fields["class"], "status");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_refresh_keeps_unwritten_tokens_until_they_expire() {
        let _log = CapturedLog::start().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let Some(_read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        // The first answer is inside the refresh margin and far from expiry; the
        // endpoint then refuses its refresh token.
        let short = json!({ "access_token": jwt_expiring_in(120), "refresh_token": "refresh-mid" })
            .to_string();
        let endpoint = CountingEndpoint::start(
            vec![("200 OK", short), ("401 Unauthorized", MARKER.to_string())],
            None,
        );
        let manager = file_manager(&path, endpoint.url.clone());
        let unwritten = manager.get_auth().await.unwrap();
        assert_eq!(unwritten.refresh, "refresh-mid");

        let served = manager.get_auth().await.unwrap();
        assert_eq!(served, unwritten);
        let requests = endpoint.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].contains("refresh_token=refresh-mid"),
            "{}",
            requests[1]
        );
        assert!(manager.holds_unwritten_refresh());

        manager.expire_unwritten_refresh();
        let err = manager.get_auth().await.unwrap_err();
        assert!(err.to_string().contains("rejected (HTTP 401)"), "{err}");
        assert_eq!(endpoint.requests().len(), 3);
        assert!(!manager.holds_unwritten_refresh());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwritten_tokens_are_written_again_once_per_interval_without_auth_lock() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let before = std::fs::read(&path).unwrap();
        let Some(read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());
        let unwritten = manager.get_auth().await.unwrap();
        assert!(manager.holds_unwritten_refresh());

        // Until the next write is due, a request neither writes nor waits: not for
        // `auth.lock`, which another process holds here, nor for the single-flight
        // lock, which this test holds.
        drop(read_only);
        let holder = lock_auth_dir(dir.path());
        let single_flight = manager.refresh_lock.lock().await;
        let served = futures_util::FutureExt::now_or_never(manager.get_auth())
            .expect("a request between two writes waited for a lock")
            .unwrap();
        drop(single_flight);
        drop(holder);
        assert_eq!(served, unwritten);
        assert_eq!(manager.get_auth().await.unwrap(), unwritten);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(manager.holds_unwritten_refresh());

        manager.allow_writeback_retry();
        assert_eq!(manager.get_auth().await.unwrap(), unwritten);
        assert_eq!(endpoint.requests().len(), 1);
        assert!(!manager.holds_unwritten_refresh());
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(unwritten.access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwritten_tokens_are_written_again_during_a_refresh_backoff() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let Some(read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        // The first answer is inside the refresh margin, and refreshing it fails.
        let short = json!({ "access_token": jwt_expiring_in(120), "refresh_token": "refresh-mid" })
            .to_string();
        let endpoint = CountingEndpoint::start(
            vec![
                ("200 OK", short),
                ("503 Service Unavailable", MARKER.to_string()),
            ],
            None,
        );
        let manager = file_manager(&path, endpoint.url.clone());
        let unwritten = manager.get_auth().await.unwrap();
        assert_eq!(manager.get_auth().await.unwrap(), unwritten);
        assert_eq!(endpoint.requests().len(), 2);

        // The directory is writable again while the refresh backs off.
        drop(read_only);
        manager.allow_writeback_retry();
        assert_eq!(manager.get_auth().await.unwrap(), unwritten);
        assert_eq!(endpoint.requests().len(), 2);
        assert!(!manager.holds_unwritten_refresh());
        let doc = read_doc(&path);
        assert_eq!(doc["tokens"]["access_token"], json!(unwritten.access));
        assert_eq!(doc["tokens"]["refresh_token"], "refresh-mid");
        assert_eq!(doc["tokens"]["account_id"], "acct_file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_document_the_writeback_retry_cannot_use_fails_and_drops_unwritten_tokens() {
        let _log = LOG_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(-60), "refresh-old");
        let Some(_read_only) = ReadOnlyDir::new(dir.path()) else {
            return;
        };
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let manager = file_manager(&path, endpoint.url.clone());
        manager.get_auth().await.unwrap();
        manager.allow_writeback_retry();

        // Another writer holds auth.lock when the retry starts and leaves a document
        // that is not JSON, once the retry has read the file without the lock.
        let holder = lock_auth_dir(dir.path());
        let reloaded = manager.on_unlocked_reload();
        let broken = path.clone();
        let writer = thread::spawn(move || {
            reloaded
                .recv_timeout(Duration::from_secs(10))
                .expect("the retry read the file");
            std::fs::write(&broken, b"{ not json").unwrap();
            drop(holder);
        });
        let err = manager.get_auth().await.unwrap_err();
        writer.join().unwrap();
        let without_tokens = file_manager(&path, endpoint.url.clone())
            .get_auth()
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), without_tokens.to_string());
        assert!(err.to_string().contains("is not valid JSON"), "{err}");
        assert!(!manager.holds_unwritten_refresh());
        assert_eq!(endpoint.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_caller_still_logs_the_fallback_once() {
        let log = CapturedLog::start().await;
        let dir = tempfile::TempDir::new().unwrap();
        let in_window = jwt_expiring_in(60);
        let path = auth_file(&dir, &in_window, "refresh-old");
        let (release, hold) = std::sync::mpsc::channel();
        let endpoint = CountingEndpoint::start(
            vec![("507 Insufficient Storage", MARKER.to_string())],
            Some(hold),
        );
        let manager = file_manager(&path, endpoint.url.clone());

        // The caller that started the refresh goes away while the failing answer is
        // held.
        let deadline = Instant::now() + Duration::from_secs(10);
        tokio::select! {
            _ = manager.get_auth() => panic!("the first caller finished while its answer was held"),
            () = async {
                while endpoint.requests().is_empty() {
                    assert!(Instant::now() < deadline, "no token request arrived");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => {}
        }
        let (successor, ()) = tokio::join!(manager.get_auth(), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            release.send(()).unwrap();
        });
        assert_eq!(successor.unwrap().access, in_window);
        assert_eq!(endpoint.requests().len(), 1);

        let warnings: Vec<_> = log
            .events("codex_token_refresh_failed_using_stored_token")
            .into_iter()
            .filter(|event| event["fields"]["status"] == 507)
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[tokio::test]
    async fn a_refresh_task_that_did_not_finish_is_transient() {
        let task = tokio::spawn(std::future::pending::<Result<StoredAuth, anyhow::Error>>());
        task.abort();
        let err = refresh_task_outcome(task.await).unwrap_err();
        let transient = err
            .downcast_ref::<TransientRefreshError>()
            .expect("a transient refresh error");
        assert_eq!((transient.class, transient.status), ("task", 0));
        assert_eq!(err.to_string(), "Codex token refresh did not finish");
    }

    #[tokio::test]
    async fn a_proactive_refresh_task_that_did_not_finish_uses_the_stored_token() {
        let log = CapturedLog::start().await;
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        // Without an unexpired stored token, or on a forced refresh, the answer stays
        // the transient failure.
        for (expiring_in, force, served) in
            [(60, false, true), (-60, false, false), (60, true, false)]
        {
            let dir = tempfile::TempDir::new().unwrap();
            let access = jwt_expiring_in(expiring_in);
            let path = auth_file(&dir, &access, "refresh-old");
            let manager = file_manager(&path, endpoint.url.clone());
            let task = tokio::spawn(std::future::pending::<Result<StoredAuth, anyhow::Error>>());
            task.abort();
            let answer = manager.refresh_task_answer(task.await, force).await;
            if served {
                assert_eq!(answer.unwrap().access, access);
            } else {
                let err = answer.unwrap_err();
                let transient = err
                    .downcast_ref::<TransientRefreshError>()
                    .expect("a transient refresh error");
                assert_eq!((transient.class, transient.status), ("task", 0));
            }
        }
        assert!(endpoint.requests().is_empty());

        let warnings: Vec<_> = log
            .events("codex_token_refresh_failed_using_stored_token")
            .into_iter()
            .filter(|event| event["fields"]["class"] == "task")
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["fields"]["status"], 0);
    }

    #[tokio::test]
    async fn a_proactive_refresh_that_cannot_read_the_file_uses_the_stored_token() {
        let log = CapturedLog::start().await;
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        // Without an unexpired stored token, or on a forced refresh, the answer stays
        // the read error.
        for (expiring_in, force, served) in
            [(60, false, true), (-60, false, false), (60, true, false)]
        {
            let dir = tempfile::TempDir::new().unwrap();
            let access = jwt_expiring_in(expiring_in);
            let path = auth_file(&dir, &access, "refresh-old");
            let whole = std::fs::read(&path).unwrap();
            let manager = file_manager(&path, endpoint.url.clone());

            // The refresh reads the file while another writer, holding auth.lock, is
            // halfway through it; the writer finishes once that read is done.
            let holder = lock_auth_dir(dir.path());
            std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();
            let reloaded = manager.on_unlocked_reload();
            let partial = path.clone();
            let writer = thread::spawn(move || {
                reloaded
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the refresh read the file");
                std::fs::write(&partial, whole).unwrap();
                drop(holder);
            });
            let answer = manager
                .refresh(force, force.then_some(access.as_str()))
                .await;
            writer.join().unwrap();
            if served {
                assert_eq!(answer.unwrap().access, access);
            } else {
                let err = answer.unwrap_err();
                assert!(err.to_string().contains("is not valid JSON"), "{err}");
            }
        }
        assert!(endpoint.requests().is_empty());

        let warnings: Vec<_> = log
            .events("codex_token_refresh_failed_using_stored_token")
            .into_iter()
            .filter(|event| event["fields"]["class"] == "auth")
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["fields"]["status"], 0);
    }

    #[tokio::test]
    async fn a_torn_read_of_tokens_another_writer_refreshed_serves_them_without_a_warning() {
        let log = CapturedLog::start().await;
        let endpoint = CountingEndpoint::start(vec![("200 OK", rotated_body())], None);
        let dir = tempfile::TempDir::new().unwrap();
        let path = auth_file(&dir, &jwt_expiring_in(60), "refresh-old");
        let manager = file_manager(&path, endpoint.url.clone());

        // The refresh reads the file while another writer, holding auth.lock, is
        // halfway through storing tokens it refreshed; the writer finishes once that
        // read is done, and the read under auth.lock finds tokens that are not due.
        let fresh_access = jwt_expiring_in(7200);
        let fresh = serde_json::to_vec_pretty(&auth_doc(&fresh_access, "refresh-new")).unwrap();
        let holder = lock_auth_dir(dir.path());
        std::fs::write(&path, &fresh[..fresh.len() / 2]).unwrap();
        let reloaded = manager.on_unlocked_reload();
        let partial = path.clone();
        let writer = thread::spawn(move || {
            reloaded
                .recv_timeout(Duration::from_secs(10))
                .expect("the refresh read the file");
            std::fs::write(&partial, fresh).unwrap();
            drop(holder);
        });
        let auth = manager.refresh(false, None).await.unwrap();
        writer.join().unwrap();
        assert_eq!(auth.access, fresh_access);
        assert_eq!(auth.refresh, "refresh-new");
        assert!(endpoint.requests().is_empty());

        // No refresh failed, so nothing is logged as one.
        let warnings: Vec<_> = log
            .events("codex_token_refresh_failed_using_stored_token")
            .into_iter()
            .filter(|event| event["fields"]["class"] == "auth")
            .collect();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test]
    async fn in_memory_stores_keep_their_refresh_state_apart() {
        let memory = InMemoryAuthStore::<StoredAuth>::new();
        assert_eq!(memory.clone().path(), memory.path());
        assert_ne!(InMemoryAuthStore::<StoredAuth>::new().path(), memory.path());

        // The same token in two stores: the failure one of them records does not keep
        // the other from refreshing.
        let in_window = jwt_expiring_in(60);
        let endpoint = CountingEndpoint::start(vec![("400 Bad Request", MARKER.to_string())], None);
        for _ in 0..2 {
            let store = test_store();
            store
                .save_auth(StoredAuth {
                    access: in_window.clone(),
                    refresh: "refresh-old".into(),
                    expires: CodexAuthManager::<CodexCliAuthStore>::now_ms() + 60_000,
                    account_id: None,
                })
                .unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(store, endpoint.url.clone());
            assert_eq!(manager.get_auth().await.unwrap().access, in_window);
        }
        assert_eq!(endpoint.requests().len(), 2);
    }
}
