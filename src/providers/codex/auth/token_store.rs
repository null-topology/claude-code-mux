use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::jwt::{TokenResponse, extract_account_id, token_exp_ms};
use crate::auth::AuthStorage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

/// The outcome of a token refresh, to be written back to the login it was made from.
#[derive(Debug, Clone)]
pub struct RefreshedTokens {
    /// The access token the refresh was made from. The write happens only while the
    /// store still holds it, so a refresh whose refresh token did not rotate cannot
    /// overwrite an access token another writer stored meanwhile.
    pub refreshed_access: String,
    /// The refresh token the refresh was made from. The write happens only while the
    /// store still holds it.
    pub refreshed_from: String,
    /// The account id of the login that was refreshed, when known.
    pub expected_account_id: Option<String>,
    pub access_token: String,
    /// A new refresh token, when the response carried one.
    pub refresh_token: Option<String>,
    /// A new id token, when the response carried one the Codex CLI can read back.
    pub id_token: Option<String>,
    /// The account id to record when the store names none.
    pub account_id: Option<String>,
    pub expires: u64,
}

impl RefreshedTokens {
    /// Whether a store holding `access`, `refresh` and `account_id` still holds the
    /// login this refresh was made from.
    fn same_login(
        &self,
        access: Option<&str>,
        refresh: Option<&str>,
        account_id: Option<&str>,
    ) -> bool {
        access == Some(self.refreshed_access.as_str())
            && refresh == Some(self.refreshed_from.as_str())
            && match (account_id, self.expected_account_id.as_deref()) {
                (Some(stored), Some(expected)) => stored == expected,
                _ => true,
            }
    }

    /// The credentials the store holds once this refresh is written over a login whose
    /// account id is `stored_account_id`.
    pub fn into_stored(self, stored_account_id: Option<String>) -> StoredAuth {
        StoredAuth {
            access: self.access_token,
            refresh: self.refresh_token.unwrap_or(self.refreshed_from),
            expires: self.expires,
            account_id: stored_account_id.or(self.account_id),
        }
    }
}

/// Why a refreshed token was not written back. In every case nothing was written.
#[derive(Debug)]
pub enum WritebackError {
    /// The store no longer holds the login the refresh was made from: the file is
    /// gone, or it holds another access token, refresh token or account id.
    LoginChanged,
    /// The store could not be read, is not JSON or holds no token object. It is left
    /// as it is.
    Unusable(anyhow::Error),
    /// Taking `auth.lock` or writing the new file failed. The file keeps what it held.
    Io(anyhow::Error),
}

impl std::fmt::Display for WritebackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoginChanged => f.write_str("Codex login changed during token refresh"),
            Self::Unusable(err) | Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WritebackError {}

pub struct CodexTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
    /// Set when `store` is the Codex CLI's `auth.json`: a refresh is then written back
    /// under the CLI's `auth.lock` instead of through `AuthStorage::save`.
    cli_file: Option<CodexCliAuthStore>,
}

impl CodexTokenStore<CodexCliAuthStore> {
    /// A store over the Codex CLI's `auth.json` whose refresh writeback takes the
    /// CLI's `auth.lock`.
    pub fn codex_cli(store: CodexCliAuthStore) -> Self {
        Self {
            cli_file: Some(store.clone()),
            store,
        }
    }
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            cli_file: None,
        }
    }

    /// Write a refresh back to the login it was made from and return the credentials
    /// the store then holds. Writes nothing when the store holds another login.
    pub async fn save_refreshed(
        &self,
        refreshed: RefreshedTokens,
    ) -> Result<StoredAuth, WritebackError> {
        if let Some(file) = &self.cli_file {
            let file = file.clone();
            // The lock wait and the file I/O block, so they run off the async runtime.
            return tokio::task::spawn_blocking(move || file.save_refreshed(refreshed))
                .await
                .map_err(|_| {
                    WritebackError::Io(anyhow::anyhow!("Codex token writeback task failed"))
                })?;
        }
        let Some(stored) = self.store.load().map_err(WritebackError::Unusable)? else {
            return Err(WritebackError::LoginChanged);
        };
        if !refreshed.same_login(
            Some(&stored.access),
            Some(&stored.refresh),
            stored.account_id.as_deref(),
        ) {
            return Err(WritebackError::LoginChanged);
        }
        let next = refreshed.into_stored(stored.account_id);
        self.store.save(next.clone()).map_err(WritebackError::Io)?;
        Ok(next)
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
    }

    /// `load_auth`, with the Codex CLI's `auth.json` read under its `auth.lock`. The
    /// CLI rewrites the file in place, so a read outside the lock can catch a
    /// partial write.
    pub async fn load_auth_locked(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        let Some(file) = &self.cli_file else {
            return self.store.load();
        };
        let file = file.clone();
        tokio::task::spawn_blocking(move || file.load_locked())
            .await
            .map_err(|_| anyhow::anyhow!("Codex auth read task failed"))?
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

/// Credential source for codex requests: the Codex CLI's `auth.json` (ChatGPT login).
///
/// The proxy never runs its own login. It reads the tokens the Codex CLI already stored
/// and, when it refreshes an expired access token, writes the rotated tokens back so the
/// Codex CLI keeps working. Every write holds the CLI's `auth.lock` next to the file
/// (`codex-rs/login/src/auth/storage.rs` at rust-v0.166.0), re-reads the file under it,
/// and changes only the token fields and `last_refresh`; fields the proxy does not own
/// (auth_mode, OPENAI_API_KEY, any future keys) are preserved untouched. A file that
/// cannot be read or parsed is never replaced.
///
/// The CLI takes `auth.lock` in `$CODEX_HOME` and writes through a symlinked
/// `auth.json` (`lock_for_write` and `FileAuthStorage::save` in
/// `codex-rs/login/src/auth/storage.rs` at rust-v0.166.0). So the lock is taken in the
/// directory the configured path names, which is the CLI's only when that path is
/// `$CODEX_HOME/auth.json`, and a symlinked file is replaced at its target with the
/// link left in place.
#[derive(Debug, Clone)]
pub struct CodexCliAuthStore {
    path: PathBuf,
}

impl CodexCliAuthStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn dir(&self) -> &Path {
        match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        }
    }

    /// The file a write replaces: the configured path with every symlink resolved,
    /// read under `auth.lock` just before the write.
    fn target(&self) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(&self.path)
    }

    /// Take the Codex CLI's exclusive `auth.lock` in the directory of `auth.json`,
    /// waiting as long as another handle holds it, as the CLI's own writes do. It
    /// blocks, so callers run it on the blocking pool. The lock file is created when
    /// missing and never truncated or removed; the lock is released when the returned
    /// handle drops.
    fn lock(&self) -> Result<std::fs::File, WritebackError> {
        let lock_path = self.dir().join("auth.lock");
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            // No directory means no login to write back to; it is not recreated.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(WritebackError::LoginChanged);
            }
            Err(err) => {
                return Err(WritebackError::Io(anyhow::anyhow!(
                    "failed to open {}: {err}",
                    lock_path.display()
                )));
            }
        };
        file.lock().map_err(|err| {
            WritebackError::Io(anyhow::anyhow!(
                "failed to lock {}: {err}",
                lock_path.display()
            ))
        })?;
        Ok(file)
    }

    /// `load` under `auth.lock`. A missing directory means no login; a lock that
    /// cannot be taken otherwise still leaves the file to read.
    fn load_locked(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        let _lock = match self.lock() {
            Ok(lock) => Some(lock),
            Err(WritebackError::LoginChanged) => return Ok(None),
            Err(_) => None,
        };
        self.load()
    }

    /// Read the file at `path` for a rewrite. `Ok(None)` means it does not exist; a
    /// file that cannot be read, is not JSON or whose root is not an object is an error,
    /// never an empty document.
    fn read_document(
        path: &Path,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, anyhow::Error> {
        let raw = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(anyhow::anyhow!("failed to read {}: {err}", path.display()));
            }
        };
        let doc: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|_| anyhow::anyhow!("{} is not valid JSON", path.display()))?;
        match doc {
            serde_json::Value::Object(map) => Ok(Some(map)),
            _ => Err(anyhow::anyhow!(
                "{} does not hold a JSON object",
                path.display()
            )),
        }
    }

    /// The locked read-modify-write of a refresh: reload under `auth.lock`, check that
    /// the file still holds the login the refresh was made from, then change only the
    /// token fields the refresh produced and `last_refresh`.
    fn save_refreshed(&self, refreshed: RefreshedTokens) -> Result<StoredAuth, WritebackError> {
        let _lock = self.lock()?;
        let target = match self.target() {
            Ok(target) => target,
            // No file, or a symlink to none: there is no login to write back to.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(WritebackError::LoginChanged);
            }
            Err(err) => {
                return Err(WritebackError::Unusable(anyhow::anyhow!(
                    "failed to resolve {}: {err}",
                    self.path.display()
                )));
            }
        };
        let Some(mut doc) = Self::read_document(&target).map_err(WritebackError::Unusable)? else {
            return Err(WritebackError::LoginChanged);
        };
        let Some(tokens) = doc.get_mut("tokens").and_then(|v| v.as_object_mut()) else {
            return Err(WritebackError::Unusable(anyhow::anyhow!(
                "{} holds no token object",
                self.path.display()
            )));
        };
        let stored_account_id = tokens
            .get("account_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let stored_access = tokens.get("access_token").and_then(|v| v.as_str());
        let stored_refresh = tokens.get("refresh_token").and_then(|v| v.as_str());
        if !refreshed.same_login(stored_access, stored_refresh, stored_account_id.as_deref()) {
            return Err(WritebackError::LoginChanged);
        }
        let string = |value: &str| serde_json::Value::String(value.to_string());
        tokens.insert("access_token".into(), string(&refreshed.access_token));
        if let Some(refresh) = &refreshed.refresh_token {
            tokens.insert("refresh_token".into(), string(refresh));
        }
        if let Some(id_token) = &refreshed.id_token {
            tokens.insert("id_token".into(), string(id_token));
        }
        if stored_account_id.is_none()
            && let Some(account_id) = &refreshed.account_id
        {
            tokens.insert("account_id".into(), string(account_id));
        }
        doc.insert("last_refresh".into(), string(&now_rfc3339()));
        let bytes = serde_json::to_vec_pretty(&doc)
            .map_err(|e| WritebackError::Io(anyhow::anyhow!("failed to encode auth.json: {e}")))?;
        write_atomic(&target, &bytes).map_err(WritebackError::Io)?;
        Ok(refreshed.into_stored(stored_account_id))
    }
}

impl AuthStorage<StoredAuth> for CodexCliAuthStore {
    fn load(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read {}: {err}",
                    self.path.display()
                ));
            }
        };
        let doc: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", self.path.display()))?;
        let Some(tokens) = doc.get("tokens") else {
            return Ok(None);
        };
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if access.is_empty() {
            return Ok(None);
        }
        let refresh = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let account_id = tokens
            .get("account_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                let probe = TokenResponse {
                    id_token: tokens
                        .get("id_token")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    access_token: access.clone(),
                    refresh_token: None,
                    expires_in: None,
                };
                extract_account_id(&probe)
            });
        // The access-token JWT carries the real expiry. If it cannot be parsed, assume a
        // short validity window so a genuinely stale token surfaces as a 401 (handled by
        // force_refresh) rather than being trusted indefinitely.
        let expires = token_exp_ms(&access).unwrap_or_else(|| now_ms() + 3_600_000);
        Ok(Some(StoredAuth {
            access,
            refresh,
            expires,
            account_id,
        }))
    }

    fn save(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        // An unconditional write of a whole login. A refresh goes through
        // `save_refreshed` instead, which writes only over the login it refreshed.
        let _lock = self.lock().map_err(anyhow::Error::new)?;
        // A symlinked file is replaced at its target; a missing one is created at the
        // configured path. A link whose target is missing is refused and left as it
        // is, as `save_refreshed` treats it as a changed login.
        let target = match self.target() {
            Ok(target) => target,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&self.path)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    anyhow::bail!(
                        "{} is a link to a file that does not exist",
                        self.path.display()
                    );
                }
                self.path.clone()
            }
            Err(err) => anyhow::bail!("failed to resolve {}: {err}", self.path.display()),
        };
        // Re-read so fields the proxy does not own survive the write. A file that
        // cannot be read or parsed is left as it is.
        let mut doc = Self::read_document(&target)?.unwrap_or_default();
        let tokens = doc.entry("tokens").or_insert_with(|| serde_json::json!({}));
        let Some(tokens) = tokens.as_object_mut() else {
            anyhow::bail!("{} holds no token object", self.path.display());
        };
        tokens.insert(
            "access_token".into(),
            serde_json::Value::String(value.access),
        );
        tokens.insert(
            "refresh_token".into(),
            serde_json::Value::String(value.refresh),
        );
        if let Some(account_id) = value.account_id {
            tokens.insert("account_id".into(), serde_json::Value::String(account_id));
        }
        doc.insert(
            "last_refresh".into(),
            serde_json::Value::String(now_rfc3339()),
        );
        write_atomic(&target, &serde_json::to_vec_pretty(&doc)?)
    }

    fn clear(&self) -> Result<(), anyhow::Error> {
        // No-op: auth.json is owned by the Codex CLI. The proxy must never delete it;
        // a failed refresh should leave the file for the user to `codex login` again.
        Ok(())
    }

    fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

pub type DefaultCodexAuthStore = CodexCliAuthStore;

/// Locate the Codex CLI credential file, honoring `$CODEX_HOME` (and a test override).
pub fn codex_auth_file() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CCP_CODEX_AUTH_FILE") {
        return PathBuf::from(explicit);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(home).join("auth.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".codex").join("auth.json");
    }
    PathBuf::from(".codex").join("auth.json")
}

pub fn file_store() -> CodexTokenStore<DefaultCodexAuthStore> {
    CodexTokenStore::codex_cli(CodexCliAuthStore::new(codex_auth_file()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Replace `path` with `bytes`: a new uniquely named temp file in the same directory,
/// created with mode 0600 on unix, synced, then renamed over `path`, followed by a
/// best-effort sync of the directory. `path` is a resolved file, never a symlink, so the
/// rename replaces the file itself. The temp file is removed on any failure after it
/// was created. The directory itself is neither created nor chmodded: it belongs to the
/// Codex CLI.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), anyhow::Error> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());
    let tmp = dir.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| {
        anyhow::anyhow!(
            "failed to create a temp file next to {}: {e}",
            path.display()
        )
    })?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            std::fs::rename(&tmp, path)
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("failed to persist {}: {e}", path.display());
    }
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use serde_json::json;

    #[test]
    fn stored_auth_reads_account_id_alias() {
        let auth: StoredAuth = serde_json::from_value(json!({
            "access": "a",
            "refresh": "r",
            "expires": 123,
            "accountId": "acct"
        }))
        .unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn stored_auth_roundtrip() {
        let store = CodexTokenStore::new(InMemoryAuthStore::new());
        let auth = StoredAuth {
            access: "token".into(),
            refresh: "refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let loaded = store.load_auth().unwrap().unwrap();
        assert_eq!(loaded.access, "token");
        assert_eq!(loaded.account_id.as_deref(), Some("acct_1"));
    }

    fn write_auth_json(dir: &std::path::Path, access: &str) -> PathBuf {
        let path = dir.join("auth.json");
        let doc = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id.tok.sig",
                "access_token": access,
                "refresh_token": "refresh-abc",
                "account_id": "acct_file"
            },
            "last_refresh": "2026-07-13T08:00:00Z"
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        path
    }

    // {"alg":"none"}.{"exp":4102444800}  (exp = year 2100)
    const ACCESS_JWT_2100: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig";

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CodexCliAuthStore::new(dir.path().join("auth.json"));
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn load_maps_tokens_and_account_id_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path);
        let auth = store.load().unwrap().unwrap();
        assert_eq!(auth.access, ACCESS_JWT_2100);
        assert_eq!(auth.refresh, "refresh-abc");
        assert_eq!(auth.account_id.as_deref(), Some("acct_file"));
        assert_eq!(auth.expires, 4102444800 * 1000);
    }

    #[test]
    fn save_preserves_unowned_fields_and_updates_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store
            .save(StoredAuth {
                access: "new-access".into(),
                refresh: "new-refresh".into(),
                expires: 0,
                account_id: Some("acct_new".into()),
            })
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["tokens"]["access_token"], "new-access");
        assert_eq!(doc["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(doc["tokens"]["account_id"], "acct_new");
        // Fields the proxy does not own are preserved.
        assert_eq!(doc["auth_mode"], "chatgpt");
        assert!(doc.as_object().unwrap().contains_key("OPENAI_API_KEY"));
        assert_eq!(doc["tokens"]["id_token"], "id.tok.sig");
        assert_ne!(doc["last_refresh"], "2026-07-13T08:00:00Z");
    }

    #[test]
    fn save_sets_0600_permissions() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store
            .save(StoredAuth {
                access: "a".into(),
                refresh: "r".into(),
                expires: 0,
                account_id: None,
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn clear_is_noop_and_keeps_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store.clear().unwrap();
        assert!(path.exists(), "clear must not delete the Codex CLI file");
    }

    /// A refresh made from the login `write_auth_json(_, ACCESS_JWT_2100)` writes, with
    /// `refresh` as its refresh token.
    fn refreshed_from(refresh: &str) -> RefreshedTokens {
        RefreshedTokens {
            refreshed_access: ACCESS_JWT_2100.into(),
            refreshed_from: refresh.into(),
            expected_account_id: Some("acct_file".into()),
            access_token: "new-access".into(),
            refresh_token: None,
            id_token: None,
            account_id: Some("acct_file".into()),
            expires: 42,
        }
    }

    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn save_refreshed_leaves_unreadable_or_partial_files_untouched() {
        let cases: [&[u8]; 6] = [
            b"not json at all",
            b"{\"tokens\": {\"access_token\": \"a\", \"refresh_token\": \"refresh-abc\"",
            b"[1, 2, 3]",
            b"\"refresh-abc\"",
            b"{\"auth_mode\": \"chatgpt\"}",
            b"{\"tokens\": \"refresh-abc\"}",
        ];
        for bytes in cases {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("auth.json");
            std::fs::write(&path, bytes).unwrap();
            let store = CodexCliAuthStore::new(path.clone());

            let err = store
                .save_refreshed(refreshed_from("refresh-abc"))
                .unwrap_err();
            assert!(matches!(err, WritebackError::Unusable(_)), "{err:?}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);

            // A whole-login write leaves a file it cannot read as a JSON object, or
            // whose `tokens` is not an object, alone as well; only a document that
            // lacks `tokens` entirely gets one.
            if bytes == b"{\"auth_mode\": \"chatgpt\"}" {
                continue;
            }
            assert!(
                store
                    .save(StoredAuth {
                        access: "a".into(),
                        refresh: "r".into(),
                        expires: 0,
                        account_id: None,
                    })
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn save_refreshed_writes_nothing_over_another_login() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let before = std::fs::read(&path).unwrap();
        let store = CodexCliAuthStore::new(path.clone());

        let err = store
            .save_refreshed(refreshed_from("refresh-other"))
            .unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut other_account = refreshed_from("refresh-abc");
        other_account.expected_account_id = Some("acct_other".into());
        let err = store.save_refreshed(other_account).unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // Same refresh token and account, but another writer already stored a newer
        // access token: a refresh whose refresh token did not rotate looks the same.
        let mut other_access = refreshed_from("refresh-abc");
        other_access.refreshed_access = "access-before-the-other-write".into();
        let err = store.save_refreshed(other_access).unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn save_refreshed_does_not_recreate_a_missing_file_or_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let store = CodexCliAuthStore::new(path.clone());
        let err = store
            .save_refreshed(refreshed_from("refresh-abc"))
            .unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert!(!path.exists());

        let gone = dir.path().join("gone");
        let store = CodexCliAuthStore::new(gone.join("auth.json"));
        let err = store
            .save_refreshed(refreshed_from("refresh-abc"))
            .unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert!(!gone.exists());
    }

    #[test]
    fn save_refreshed_changes_only_the_token_fields_and_last_refresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let doc = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": "sk-toy",
            "future_key": {"nested": [1, 2]},
            "tokens": {
                "id_token": "id.tok.sig",
                "access_token": "old-access",
                "refresh_token": "refresh-abc",
                "token_extra": "kept"
            },
            "last_refresh": "2026-07-13T08:00:00Z"
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        let store = CodexCliAuthStore::new(path.clone());

        let mut refreshed = refreshed_from("refresh-abc");
        refreshed.refreshed_access = "old-access".into();
        refreshed.expected_account_id = None;
        let stored = store.save_refreshed(refreshed).unwrap();
        assert_eq!(stored.access, "new-access");
        assert_eq!(stored.refresh, "refresh-abc");
        assert_eq!(stored.account_id.as_deref(), Some("acct_file"));
        assert_eq!(stored.expires, 42);

        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut expected = doc.clone();
        expected["tokens"]["access_token"] = json!("new-access");
        expected["tokens"]["account_id"] = json!("acct_file");
        expected["last_refresh"] = after["last_refresh"].clone();
        assert_eq!(after, expected);
        assert_ne!(after["last_refresh"], "2026-07-13T08:00:00Z");

        // A known account id never replaces one the file already names.
        let mut refreshed = refreshed_from("refresh-abc");
        refreshed.refreshed_access = "new-access".into();
        refreshed.account_id = Some("acct_derived".into());
        refreshed.refresh_token = Some("refresh-new".into());
        refreshed.id_token = Some("new.id.tok".into());
        store.save_refreshed(refreshed).unwrap();
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["tokens"]["account_id"], "acct_file");
        assert_eq!(after["tokens"]["refresh_token"], "refresh-new");
        assert_eq!(after["tokens"]["id_token"], "new.id.tok");
        assert_eq!(after["tokens"]["token_extra"], "kept");
    }

    #[test]
    fn save_refreshed_writes_0600_keeps_auth_lock_and_leaves_no_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let store = CodexCliAuthStore::new(path.clone());
        store.save_refreshed(refreshed_from("refresh-abc")).unwrap();

        let lock_path = dir.path().join("auth.lock");
        assert!(
            lock_path.exists(),
            "the write takes the Codex CLI's auth.lock"
        );
        assert_eq!(dir_entries(dir.path()), ["auth.json", "auth.lock"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // The lock file is never truncated or removed.
        std::fs::write(&lock_path, b"kept").unwrap();
        let mut again = refreshed_from("refresh-abc");
        again.refreshed_access = "new-access".into();
        again.access_token = "newer-access".into();
        store.save_refreshed(again).unwrap();
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"kept");
        assert_eq!(dir_entries(dir.path()), ["auth.json", "auth.lock"]);
    }

    fn hold_auth_lock(dir: &std::path::Path) -> std::fs::File {
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

    #[test]
    fn save_refreshed_waits_for_a_held_auth_lock_then_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let before = std::fs::read(&path).unwrap();
        let holder = hold_auth_lock(dir.path());
        let store = CodexCliAuthStore::new(path.clone());

        let writer =
            std::thread::spawn(move || store.save_refreshed(refreshed_from("refresh-abc")));
        // The write waits as long as the lock is held, here longer than five seconds,
        // and writes nothing meanwhile.
        std::thread::sleep(std::time::Duration::from_millis(5500));
        assert!(!writer.is_finished(), "the write must wait for auth.lock");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        holder.unlock().unwrap();
        let stored = writer.join().unwrap().unwrap();
        assert_eq!(stored.access, "new-access");
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["tokens"]["access_token"], "new-access");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_is_an_io_error_and_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        drop(hold_auth_lock(dir.path()));
        let before = std::fs::read(&path).unwrap();
        // A directory the temp file cannot be created in.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let probe = dir.path().join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            // Permissions are not enforced for this user; nothing to observe.
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let store = CodexCliAuthStore::new(path.clone());

        let err = store
            .save_refreshed(refreshed_from("refresh-abc"))
            .unwrap_err();
        let entries = dir_entries(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(err, WritebackError::Io(_)), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(entries, ["auth.json", "auth.lock"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_replaced_at_its_target_and_the_lock_stays_beside_the_link() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home");
        let real = root.path().join("real");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&real).unwrap();
        let target = write_auth_json(&real, ACCESS_JWT_2100);
        let link = home.join("auth.json");
        std::os::unix::fs::symlink("../real/auth.json", &link).unwrap();
        let store = CodexCliAuthStore::new(link.clone());

        store.save_refreshed(refreshed_from("refresh-abc")).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("../real/auth.json")
        );
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(after["tokens"]["access_token"], "new-access");
        assert_eq!(after["auth_mode"], "chatgpt");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(dir_entries(&home), ["auth.json", "auth.lock"]);
        assert_eq!(dir_entries(&real), ["auth.json"]);

        // A whole-login write goes to the target the same way.
        store
            .save(StoredAuth {
                access: "whole-access".into(),
                refresh: "whole-refresh".into(),
                expires: 0,
                account_id: None,
            })
            .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(after["tokens"]["access_token"], "whole-access");
        assert_eq!(dir_entries(&real), ["auth.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_no_file_is_a_changed_login_and_stays_a_symlink() {
        let root = tempfile::TempDir::new().unwrap();
        let link = root.path().join("auth.json");
        std::os::unix::fs::symlink("missing/auth.json", &link).unwrap();
        let store = CodexCliAuthStore::new(link.clone());

        let err = store
            .save_refreshed(refreshed_from("refresh-abc"))
            .unwrap_err();
        assert!(matches!(err, WritebackError::LoginChanged), "{err:?}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(dir_entries(root.path()), ["auth.json", "auth.lock"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_whole_login_save_refuses_a_symlink_to_no_file_and_leaves_it() {
        let root = tempfile::TempDir::new().unwrap();
        let link = root.path().join("auth.json");
        std::os::unix::fs::symlink("missing/auth.json", &link).unwrap();
        let store = CodexCliAuthStore::new(link.clone());

        let saved = store.save(StoredAuth {
            access: ACCESS_JWT_2100.to_string(),
            refresh: "refresh-abc".to_string(),
            expires: u64::MAX,
            account_id: Some("acct_file".to_string()),
        });
        assert!(saved.is_err());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(dir_entries(root.path()), ["auth.json", "auth.lock"]);
    }

    #[tokio::test]
    async fn token_store_writes_back_through_the_cli_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexTokenStore::codex_cli(CodexCliAuthStore::new(path.clone()));
        let stored = store
            .save_refreshed(refreshed_from("refresh-abc"))
            .await
            .unwrap();
        assert_eq!(stored.access, "new-access");
        assert!(dir.path().join("auth.lock").exists());
        assert!(matches!(
            store.save_refreshed(refreshed_from("refresh-other")).await,
            Err(WritebackError::LoginChanged)
        ));
    }
}
