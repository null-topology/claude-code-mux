//! Live model inventory from the Codex backend.
//!
//! `GET {codex api root}/models?client_version=<x.y.z>` returns the models the
//! logged-in ChatGPT account may use, on the same bearer the completions use
//! and without a prompt body. It is a metadata call: it answers while the
//! usage windows are spent, so a listing stays checkable under a rate limit.
//!
//! The proxy does not curate this list. Whatever the backend returns is what
//! `/v1/models` advertises for the codex provider, and the last successful
//! answer is remembered so a model the backend just started serving routes
//! to codex without a proxy release.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthStorage;
use crate::config;

use super::auth::constants::{CODEX_CLIENT_VERSION, ORIGINATOR};
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{StoredAuth, codex_auth_file};

/// Upper bound for one listing round-trip; Claude Code's own gateway
/// discovery gives up after five seconds, so the proxy must answer sooner.
pub const MODELS_TIMEOUT: Duration = Duration::from_secs(4);

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// One reasoning effort the backend accepts for a model.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The subset of a backend model entry the proxy forwards. Unknown fields are
/// ignored so a backend schema change never breaks the listing.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UpstreamModel {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_in_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_responses_lite: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
}

impl UpstreamModel {
    /// The row shape `/v1/models` advertises: the Anthropic-style fields first,
    /// then the backend facts a consumer needs to pick a model.
    pub fn to_listing_row(&self) -> Value {
        let mut row = json!({
            "type": "model",
            "object": "model",
            "id": self.slug,
            "display_name": self.display_name.clone().unwrap_or_else(|| self.slug.clone()),
        });
        let extra = serde_json::to_value(self).unwrap_or_default();
        if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                if matches!(key.as_str(), "slug" | "display_name") {
                    continue;
                }
                row.insert(key.clone(), value.clone());
            }
        }
        row
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    models: Vec<Value>,
}

/// One successful listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInventory {
    pub models: Vec<UpstreamModel>,
    /// RFC 3339 timestamp of the answer.
    pub fetched_at: String,
}

/// Why a listing could not be produced. `Unauthorized` covers "no Codex
/// credentials on this machine" as well as a backend 401/403; everything else
/// (transport, timeout, 5xx, malformed body) is `Unreachable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    Unauthorized(String),
    Unreachable(String),
}

impl ModelsError {
    pub fn status(&self) -> &'static str {
        match self {
            ModelsError::Unauthorized(_) => "unauthorized",
            ModelsError::Unreachable(_) => "unreachable",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            ModelsError::Unauthorized(detail) | ModelsError::Unreachable(detail) => detail,
        }
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status(), self.detail())
    }
}

// ---------------------------------------------------------------------------
// Endpoint shaping
// ---------------------------------------------------------------------------

/// `.../codex/responses` (the configured completions URL) becomes
/// `.../codex/models`; a bare root gets `/models` appended.
pub fn models_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    match base_url.strip_suffix("/responses") {
        Some(api_root) => format!("{api_root}/models"),
        None => format!("{base_url}/models"),
    }
}

/// The `client_version` the backend requires on the listing call. The
/// installed Codex CLI's own cache records the version it last used, so
/// that is preferred over the compiled-in default; `CCP_CODEX_CLIENT_VERSION`
/// overrides both.
pub fn client_version() -> String {
    if let Some(explicit) = config::codex_client_version() {
        return explicit;
    }
    if let Some(cached) = client_version_from_codex_cache() {
        return cached;
    }
    CODEX_CLIENT_VERSION.to_string()
}

fn client_version_from_codex_cache() -> Option<String> {
    let path = codex_auth_file().parent()?.join("models_cache.json");
    let raw = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let version = value.get("client_version")?.as_str()?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn parse_inventory(body: &[u8]) -> Result<ModelInventory, ModelsError> {
    let parsed: ModelsResponse = serde_json::from_slice(body)
        .map_err(|error| ModelsError::Unreachable(format!("malformed models body: {error}")))?;
    let models = parsed
        .models
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<UpstreamModel>(entry).ok())
        .collect();
    Ok(ModelInventory {
        models,
        fetched_at: now_rfc3339(),
    })
}

fn body_snippet(body: &[u8]) -> String {
    const MAX: usize = 200;
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.len() <= MAX {
        text.to_string()
    } else {
        let mut end = MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    }
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

fn build_request(
    client: &reqwest::Client,
    url: &str,
    auth: &StoredAuth,
) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .timeout(MODELS_TIMEOUT)
        .header(http::header::ACCEPT, "application/json")
        .bearer_auth(&auth.access)
        .header("originator", config::codex_originator(ORIGINATOR));
    if let Some(account_id) = auth.account_id.as_deref() {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let user_agent =
        config::codex_user_agent(&format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION")));
    if !user_agent.is_empty() {
        request = request.header(http::header::USER_AGENT, user_agent);
    }
    request
}

/// Ask the backend which models this login may use. A 401 triggers one token
/// refresh and one retry, mirroring the completions path. The result of a
/// successful call that names at least one model is remembered for routing
/// (see [`is_discovered_model`]).
pub async fn fetch_models<S: AuthStorage<StoredAuth>>(
    client: &reqwest::Client,
    auth_manager: &CodexAuthManager<S>,
    base_url: &str,
) -> Result<ModelInventory, ModelsError> {
    let url = format!(
        "{}?client_version={}",
        models_endpoint(base_url),
        client_version()
    );
    let mut auth = auth_manager
        .get_auth()
        .await
        .map_err(|error| ModelsError::Unauthorized(error.to_string()))?;
    let mut refresh_attempted = false;
    loop {
        let response = build_request(client, &url, &auth)
            .send()
            .await
            .map_err(|error| ModelsError::Unreachable(error.to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| ModelsError::Unreachable(error.to_string()))?;
        if status == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
            refresh_attempted = true;
            auth = auth_manager
                .force_refresh(&auth.access)
                .await
                .map_err(|error| ModelsError::Unauthorized(error.to_string()))?;
            continue;
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ModelsError::Unauthorized(format!(
                "HTTP {}: {}",
                status.as_u16(),
                body_snippet(&body)
            )));
        }
        if !status.is_success() {
            return Err(ModelsError::Unreachable(format!(
                "HTTP {}: {}",
                status.as_u16(),
                body_snippet(&body)
            )));
        }
        let inventory = parse_inventory(&body)?;
        remember_discovered(&inventory);
        return Ok(inventory);
    }
}

// ---------------------------------------------------------------------------
// Remembered inventory, used by routing and the allowlist
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Discovered {
    /// slug -> `use_responses_lite` as the backend reported it.
    models: HashMap<String, Option<bool>>,
}

static DISCOVERED: Lazy<RwLock<Arc<Discovered>>> = Lazy::new(|| RwLock::new(Arc::default()));

fn remember_discovered(inventory: &ModelInventory) {
    // A listing that names nothing would make every slug unknown; keep the
    // previous one instead.
    if inventory.models.is_empty() {
        return;
    }
    let models = inventory
        .models
        .iter()
        .map(|model| (model.slug.clone(), model.use_responses_lite))
        .collect();
    let mut guard = DISCOVERED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Arc::new(Discovered { models });
}

fn discovered() -> Arc<Discovered> {
    DISCOVERED
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Whether the last successful listing named this slug.
pub fn is_discovered_model(slug: &str) -> bool {
    discovered().models.contains_key(slug)
}

/// `use_responses_lite` as the backend reported it for a discovered slug.
pub fn discovered_uses_responses_lite(slug: &str) -> Option<bool> {
    discovered().models.get(slug).copied().flatten()
}

/// Slugs from the last successful listing, sorted.
pub fn discovered_slugs() -> Vec<String> {
    let mut slugs: Vec<String> = discovered().models.keys().cloned().collect();
    slugs.sort_unstable();
    slugs
}

pub fn clear_discovered_models_for_tests() {
    let mut guard = DISCOVERED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Arc::default();
}

/// Pretend the backend listed these slugs (no lane flag), for routing tests.
pub fn remember_for_tests(slugs: &[&str]) {
    let models = slugs
        .iter()
        .map(|slug| ((*slug).to_string(), None))
        .collect();
    let mut guard = DISCOVERED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Arc::new(Discovered { models });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_endpoint_replaces_responses_suffix_or_appends() {
        assert_eq!(
            models_endpoint("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/models"
        );
        assert_eq!(
            models_endpoint("http://127.0.0.1:1234/"),
            "http://127.0.0.1:1234/models"
        );
    }

    #[test]
    fn inventory_keeps_known_fields_and_skips_entries_without_slug() {
        let body = json!({
            "models": [
                {
                    "slug": "gpt-7-test",
                    "display_name": "Test",
                    "description": "A test model",
                    "visibility": "list",
                    "priority": 1,
                    "supported_in_api": true,
                    "use_responses_lite": true,
                    "context_window": 272000,
                    "max_context_window": 872000,
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [{"effort": "low", "description": "fast"}, {"effort": "high"}],
                    "input_modalities": ["text", "image"],
                    "model_messages": {"ignored": true}
                },
                {"display_name": "no slug"},
                {"slug": "gpt-5.5", "use_responses_lite": false}
            ]
        });
        let inventory = parse_inventory(serde_json::to_vec(&body).unwrap().as_slice()).unwrap();
        assert_eq!(inventory.models.len(), 2);
        let first = &inventory.models[0];
        assert_eq!(first.slug, "gpt-7-test");
        assert_eq!(first.supported_reasoning_levels.len(), 2);
        assert_eq!(first.input_modalities, vec!["text", "image"]);

        let row = first.to_listing_row();
        assert_eq!(row["type"], "model");
        assert_eq!(row["id"], "gpt-7-test");
        assert_eq!(row["display_name"], "Test");
        assert_eq!(row["description"], "A test model");
        assert_eq!(row["visibility"], "list");
        assert_eq!(row["use_responses_lite"], true);
        assert_eq!(row["supported_reasoning_levels"][0]["effort"], "low");
        assert!(row.get("slug").is_none());
        assert!(row.get("model_messages").is_none());

        let bare = inventory.models[1].to_listing_row();
        assert_eq!(bare["display_name"], "gpt-5.5");
        assert!(bare.get("description").is_none());
    }

    #[test]
    fn malformed_body_is_unreachable() {
        let error = parse_inventory(b"not json").unwrap_err();
        assert_eq!(error.status(), "unreachable");
        assert!(error.detail().starts_with("malformed models body"));
    }

    #[test]
    fn remembered_inventory_drives_routing_helpers() {
        clear_discovered_models_for_tests();
        assert!(!is_discovered_model("gpt-7-test"));
        remember_discovered(&ModelInventory {
            models: vec![
                UpstreamModel {
                    slug: "gpt-7-test".into(),
                    display_name: None,
                    description: None,
                    visibility: None,
                    priority: None,
                    supported_in_api: None,
                    use_responses_lite: Some(true),
                    context_window: None,
                    max_context_window: None,
                    default_reasoning_level: None,
                    supported_reasoning_levels: Vec::new(),
                    input_modalities: Vec::new(),
                },
                UpstreamModel {
                    slug: "gpt-5.5".into(),
                    display_name: None,
                    description: None,
                    visibility: None,
                    priority: None,
                    supported_in_api: None,
                    use_responses_lite: None,
                    context_window: None,
                    max_context_window: None,
                    default_reasoning_level: None,
                    supported_reasoning_levels: Vec::new(),
                    input_modalities: Vec::new(),
                },
            ],
            fetched_at: String::new(),
        });
        assert!(is_discovered_model("gpt-7-test"));
        assert_eq!(discovered_uses_responses_lite("gpt-7-test"), Some(true));
        assert_eq!(discovered_uses_responses_lite("gpt-5.5"), None);
        assert_eq!(discovered_slugs(), vec!["gpt-5.5", "gpt-7-test"]);
        clear_discovered_models_for_tests();
        assert!(discovered_slugs().is_empty());
    }

    #[test]
    fn body_snippet_truncates_on_char_boundary() {
        let long = "é".repeat(300);
        let snippet = body_snippet(long.as_bytes());
        assert!(snippet.ends_with('…'));
        assert!(snippet.len() <= 204);
    }
}
