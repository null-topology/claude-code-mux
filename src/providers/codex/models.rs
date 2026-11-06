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
use serde_json::Value;

use crate::auth::AuthStorage;
use crate::config;
use crate::provider::{ModelCapabilities, model_info};

use super::auth::constants::{CODEX_CLIENT_VERSION, ORIGINATOR};
use super::auth::manager::{CodexAuthManager, TransientRefreshError};
use super::auth::token_store::{StoredAuth, codex_auth_file};
use super::client::forward_client_headers;
use super::translate::model_allowlist::full_lane_web_search_model;
use super::translate::request::to_codex_effort;

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

/// One service tier the backend offers for a model.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServiceTierOffer {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The subset of a backend model entry the proxy reads. Unknown fields are
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_tiers: Vec<ServiceTierOffer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_service_tier: Option<String>,
}

impl UpstreamModel {
    /// The `service_tier` the Codex CLI puts in a request body for this model
    /// when the user chose none: the catalog's `default_service_tier`, but only
    /// when the model's own `service_tiers` lists it, and never the `default`
    /// sentinel, which means standard routing.
    fn catalog_default_service_tier(&self) -> Option<String> {
        self.default_service_tier
            .as_deref()
            .filter(|tier| *tier != "default")
            .filter(|tier| self.service_tiers.iter().any(|offer| offer.id == *tier))
            .map(str::to_string)
    }

    /// The `/v1/models` row for this model, in the Anthropic Models API shape.
    /// The backend serves prompts past `context_window` up to
    /// `max_context_window`, so the larger window is the usable input limit.
    /// `max_tokens` is null: the translator sends no output cap
    /// (`translate_request_inner` never sets `max_output_tokens`) and the
    /// listing names none.
    pub fn to_listing_row(&self) -> Value {
        let display_name = self.display_name.as_deref().unwrap_or(&self.slug);
        let max_input_tokens = self.max_context_window.or(self.context_window);
        let capabilities = self.capabilities();
        model_info(
            &self.slug,
            display_name,
            max_input_tokens,
            None,
            Some(&capabilities),
        )
    }

    /// What an Anthropic Messages request for this model can use through the
    /// Codex translator: a feature counts only when the translator carries it
    /// to the backend as the same feature.
    pub fn capabilities(&self) -> ModelCapabilities {
        // `read_effort` accepts the level and `to_codex_effort` sends it as the
        // Codex effort of the same name; the model must list that level.
        let effort = |level: &str| {
            to_codex_effort(Some(level)).is_some_and(|effort| effort.to_string() == level)
                && self
                    .supported_reasoning_levels
                    .iter()
                    .any(|listed| listed.effort == level)
        };
        ModelCapabilities {
            // No batch endpoint, citation mapping or code execution on this route.
            batch: false,
            citations: false,
            code_execution: false,
            // `translate_request_inner` reads no `context_management` edits.
            clear_thinking_20251015: false,
            clear_tool_uses_20250919: false,
            compact_20260112: false,
            // `read_effort` accepts low, medium, high, xhigh and max, and
            // `to_codex_effort` sends each as the Codex effort of the same name.
            // The level sent can still differ from the one requested:
            // `CCP_CODEX_EFFORT` replaces it on every request, and the
            // compaction effort cap replaces it on Claude Code's compaction
            // summary.
            effort_low: effort("low"),
            effort_medium: effort("medium"),
            effort_high: effort("high"),
            effort_xhigh: effort("xhigh"),
            effort_max: effort("max"),
            // `build_input` sends image blocks, and images in tool results, as
            // `input_image` parts; the model must take image input.
            image_input: self.input_modalities.iter().any(|m| m == "image"),
            // `parse_content_block` drops `document` blocks.
            pdf_input: false,
            // `read_tools` turns a code execution tool into a plain function.
            server_code_execution: false,
            // `read_tools` maps the hosted `web_search_20250305` tool onto the
            // backend's `web_search`; any other web search tool version becomes
            // a plain function, so only that version is carried.
            // `apply_model_lane_for_request` runs it on another model when
            // `full_lane_web_search_model` substitutes one.
            server_web_search: full_lane_web_search_model(&self.slug) == self.slug,
            // `read_output_format` sends an `output_config.format` JSON schema
            // as a strict `json_schema` text format. Strict tool use is not
            // carried: `function_tool` sends every tool with `strict: false`.
            structured_outputs: true,
            // Reasoning is driven by the effort alone (`translate_request_inner`);
            // the request's `thinking` field is never read, so `enabled` and
            // `adaptive` do nothing, while `disabled` and `between_tools` are
            // accepted and never rejected.
            thinking: !self.supported_reasoning_levels.is_empty(),
            thinking_adaptive: false,
            thinking_between_tools: true,
            thinking_disabled: true,
            thinking_enabled: false,
        }
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

/// `client_headers` are those of the client request that caused this listing,
/// if one did; the headers `CCP_CODEX_FORWARD_HEADERS` names go on the
/// listing as on every other Codex request.
fn build_request(
    client: &reqwest::Client,
    url: &str,
    auth: &StoredAuth,
    client_headers: Option<&http::HeaderMap>,
) -> reqwest::Result<reqwest::Request> {
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
    let mut request = request.build()?;
    if let Some(client_headers) = client_headers {
        forward_client_headers(
            request.headers_mut(),
            client_headers,
            &config::codex_forward_headers(),
        );
    }
    Ok(request)
}

/// A failure to obtain a Codex token: a refresh that may pass is `Unreachable`,
/// anything else `Unauthorized`.
fn auth_models_error(error: anyhow::Error) -> ModelsError {
    if error.downcast_ref::<TransientRefreshError>().is_some() {
        ModelsError::Unreachable(error.to_string())
    } else {
        ModelsError::Unauthorized(error.to_string())
    }
}

/// Ask the backend which models this login may use. A 401 triggers one token
/// refresh and one retry, mirroring the completions path. The result of a
/// successful call that names at least one model is remembered for routing
/// (see [`is_discovered_model`]).
pub async fn fetch_models<S: AuthStorage<StoredAuth> + 'static>(
    client: &reqwest::Client,
    auth_manager: &CodexAuthManager<S>,
    base_url: &str,
    client_headers: Option<&http::HeaderMap>,
) -> Result<ModelInventory, ModelsError> {
    let url = format!(
        "{}?client_version={}",
        models_endpoint(base_url),
        client_version()
    );
    let mut auth = auth_manager.get_auth().await.map_err(auth_models_error)?;
    let mut refresh_attempted = false;
    loop {
        let request = build_request(client, &url, &auth, client_headers)
            .map_err(|error| ModelsError::Unreachable(error.to_string()))?;
        let response = client
            .execute(request)
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
                .map_err(auth_models_error)?;
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
    models: HashMap<String, DiscoveredModel>,
}

#[derive(Debug, Default)]
struct DiscoveredModel {
    /// `use_responses_lite` as the backend reported it.
    use_responses_lite: Option<bool>,
    /// See `UpstreamModel::catalog_default_service_tier`.
    default_service_tier: Option<String>,
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
        .map(|model| {
            (
                model.slug.clone(),
                DiscoveredModel {
                    use_responses_lite: model.use_responses_lite,
                    default_service_tier: model.catalog_default_service_tier(),
                },
            )
        })
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
    discovered()
        .models
        .get(slug)
        .and_then(|model| model.use_responses_lite)
}

/// The tier id the last successful listing makes this slug's default, if any.
pub fn discovered_default_service_tier(slug: &str) -> Option<String> {
    discovered()
        .models
        .get(slug)
        .and_then(|model| model.default_service_tier.clone())
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
        .map(|slug| ((*slug).to_string(), DiscoveredModel::default()))
        .collect();
    let mut guard = DISCOVERED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Arc::new(Discovered { models });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                    "service_tiers": [{"id": "priority", "name": "Fast", "description": "faster"}],
                    "default_service_tier": null,
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

        assert_eq!(first.service_tiers[0].id, "priority");

        // The row is an Anthropic `ModelInfo`; nothing of the catalog entry
        // beyond it is forwarded.
        let row = first.to_listing_row();
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capabilities",
                "created_at",
                "deprecated_at",
                "display_name",
                "id",
                "lifecycle",
                "line",
                "max_input_tokens",
                "max_tokens",
                "retires_at",
                "type",
            ]
        );
        assert_eq!(row["type"], "model");
        assert_eq!(row["id"], "gpt-7-test");
        assert_eq!(row["display_name"], "Test");
        assert_eq!(row["max_input_tokens"], 872000);
        assert!(row["max_tokens"].is_null());
        assert_eq!(row["capabilities"]["effort"]["low"]["supported"], true);

        let bare = inventory.models[1].to_listing_row();
        assert_eq!(bare["display_name"], "gpt-5.5");
        assert!(bare["max_input_tokens"].is_null());
    }

    fn catalog_model(entry: Value) -> UpstreamModel {
        serde_json::from_value(entry).unwrap()
    }

    #[test]
    fn max_input_tokens_prefers_the_largest_window_the_catalog_names() {
        let both = catalog_model(json!({
            "slug": "a", "context_window": 272000, "max_context_window": 872000
        }));
        let window_only = catalog_model(json!({"slug": "b", "context_window": 500000}));
        let neither = catalog_model(json!({"slug": "c"}));
        assert_eq!(both.to_listing_row()["max_input_tokens"], 872000);
        assert_eq!(window_only.to_listing_row()["max_input_tokens"], 500000);
        assert!(neither.to_listing_row()["max_input_tokens"].is_null());
    }

    #[test]
    fn capabilities_follow_what_the_translator_carries() {
        let model = catalog_model(json!({
            "slug": "gpt-6-sol",
            "supported_reasoning_levels": [{"effort": "low"}, {"effort": "xhigh"}],
            "input_modalities": ["text", "image"]
        }));
        let caps = model.capabilities();
        assert!(caps.effort_low && caps.effort_xhigh);
        assert!(!caps.effort_medium && !caps.effort_high && !caps.effort_max);
        assert!(caps.thinking && caps.image_input && caps.server_web_search);
        assert!(caps.structured_outputs);
        assert!(caps.thinking_disabled && caps.thinking_between_tools);
        assert!(!caps.thinking_enabled && !caps.thinking_adaptive);
        assert!(!caps.pdf_input && !caps.batch && !caps.citations && !caps.code_execution);
        assert!(!caps.server_code_execution);
        assert!(!caps.clear_thinking_20251015 && !caps.clear_tool_uses_20250919);
        assert!(!caps.compact_20260112);

        // A model with no reasoning levels and no image input.
        let plain = catalog_model(json!({"slug": "gpt-plain"})).capabilities();
        assert!(!plain.thinking && !plain.image_input);
        assert!(!plain.effort_low && !plain.effort_max);
        assert!(plain.thinking_disabled);

        // Hosted search on a model the full lane swaps for another runs on
        // that other model, so the listed one does not get it.
        for slug in ["gpt-5.6-luna", "gpt-6-luna"] {
            let caps = catalog_model(json!({"slug": slug})).capabilities();
            assert!(!caps.server_web_search, "{slug}");
        }
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
                    service_tiers: Vec::new(),
                    default_service_tier: None,
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
                    service_tiers: Vec::new(),
                    default_service_tier: None,
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

    fn tiered_model(slug: &str, default: Option<&str>, offered: &[&str]) -> UpstreamModel {
        serde_json::from_value(json!({
            "slug": slug,
            "default_service_tier": default,
            "service_tiers": offered.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn catalog_default_service_tier_needs_a_listed_non_sentinel_tier() {
        let default_of = |default, offered: &[&str]| {
            tiered_model("gpt-7-test", default, offered).catalog_default_service_tier()
        };
        assert_eq!(default_of(None, &["priority"]), None);
        assert_eq!(
            default_of(Some("priority"), &["priority"]).as_deref(),
            Some("priority")
        );
        assert_eq!(default_of(Some("default"), &["default", "priority"]), None);
        assert_eq!(default_of(Some("priority"), &[]), None);
        assert_eq!(default_of(Some("flex"), &["priority"]), None);
    }

    #[test]
    fn remembered_listing_keeps_default_service_tier_per_slug() {
        clear_discovered_models_for_tests();
        remember_discovered(&ModelInventory {
            models: vec![
                tiered_model("gpt-7-test", Some("priority"), &["priority"]),
                tiered_model("gpt-5.5", None, &["priority"]),
            ],
            fetched_at: String::new(),
        });
        assert_eq!(
            discovered_default_service_tier("gpt-7-test").as_deref(),
            Some("priority")
        );
        assert_eq!(discovered_default_service_tier("gpt-5.5"), None);
        assert_eq!(discovered_default_service_tier("gpt-unlisted"), None);

        // A listing that names nothing keeps the previous per-slug data.
        remember_discovered(&ModelInventory {
            models: Vec::new(),
            fetched_at: String::new(),
        });
        assert_eq!(
            discovered_default_service_tier("gpt-7-test").as_deref(),
            Some("priority")
        );

        // Before any listing, no model has a catalog default, including
        // compiled-in ids.
        clear_discovered_models_for_tests();
        assert_eq!(discovered_default_service_tier("gpt-7-test"), None);
        assert_eq!(discovered_default_service_tier("gpt-5.5"), None);
    }

    #[test]
    fn body_snippet_truncates_on_char_boundary() {
        let long = "é".repeat(300);
        let snippet = body_snippet(long.as_bytes());
        assert!(snippet.ends_with('…'));
        assert!(snippet.len() <= 204);
    }

    /// A token refresh that may pass leaves the listing unreachable; a login that
    /// needs `codex login` again leaves it unauthorized.
    #[tokio::test]
    async fn a_token_refresh_outage_is_unreachable_and_a_dead_login_unauthorized() {
        use super::super::auth::token_store::{CodexCliAuthStore, CodexTokenStore};
        // Nothing listens on port 1, so the refresh cannot reach the endpoint; without
        // a refresh token only a new login helps.
        for (refresh, unreachable) in [("refresh-test", true), ("", false)] {
            let dir = tempfile::tempdir().unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(
                CodexTokenStore::codex_cli(CodexCliAuthStore::new(dir.path().join("auth.json"))),
                "http://127.0.0.1:1/oauth/token".to_string(),
            );
            manager.set_test_auth(StoredAuth {
                access: "expired".to_string(),
                refresh: refresh.to_string(),
                expires: 0,
                account_id: Some("acct".to_string()),
            });
            let error = fetch_models(
                &reqwest::Client::new(),
                &manager,
                "http://127.0.0.1:1/responses",
                None,
            )
            .await
            .unwrap_err();
            assert_eq!(
                matches!(error, ModelsError::Unreachable(_)),
                unreachable,
                "refresh token {refresh:?}: {error:?}"
            );
        }
    }

    /// How long a one-shot mock server waits for its request, and a test for it.
    const MOCK_DEADLINE: Duration = Duration::from_secs(10);

    /// A mock server task, aborted when the test leaves it, however it leaves.
    struct MockServer(tokio::task::JoinHandle<()>);

    impl MockServer {
        /// Answer one request with `status` and a JSON `body`. Accepting and reading
        /// give up after `MOCK_DEADLINE`.
        async fn answer_once(status: &'static str, body: &'static str) -> (String, Self) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = tokio::time::timeout(MOCK_DEADLINE, listener.accept())
                    .await
                    .expect("mock server: no request arrived")
                    .unwrap();
                let read = async {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        assert!(read > 0, "request ended before its body was complete");
                        request.extend_from_slice(&chunk[..read]);
                        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            return;
                        }
                    }
                };
                tokio::time::timeout(MOCK_DEADLINE, read)
                    .await
                    .expect("mock server: the request did not finish arriving");
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::time::timeout(MOCK_DEADLINE, stream.write_all(response.as_bytes()))
                    .await;
            });
            (format!("http://{addr}"), Self(task))
        }

        /// Wait for the server to have answered, failing the test with its panic.
        async fn join(mut self) {
            tokio::time::timeout(MOCK_DEADLINE, &mut self.0)
                .await
                .expect("mock server did not finish")
                .unwrap();
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// The listing rejected with 401 forces a token refresh: one the endpoint cannot
    /// serve right now leaves the listing unreachable, one it refuses unauthorized.
    #[tokio::test]
    async fn a_forced_refresh_after_a_401_maps_its_failure() {
        use super::super::auth::token_store::{CodexCliAuthStore, CodexTokenStore};
        for (refresh_status, unreachable) in [
            ("503 Service Unavailable", true),
            ("400 Bad Request", false),
        ] {
            let (backend, backend_server) = MockServer::answer_once("401 Unauthorized", "{}").await;
            let (token, token_server) =
                MockServer::answer_once(refresh_status, r#"{"error":"invalid_grant"}"#).await;
            let dir = tempfile::tempdir().unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(
                CodexTokenStore::codex_cli(CodexCliAuthStore::new(dir.path().join("auth.json"))),
                format!("{token}/oauth/token"),
            );
            manager.set_test_auth(StoredAuth {
                access: "access-test".to_string(),
                refresh: "refresh-test".to_string(),
                expires: u64::MAX,
                account_id: Some("acct".to_string()),
            });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let base_url = format!("{backend}/responses");
            let listing = fetch_models(&client, &manager, &base_url, None);
            let error = tokio::time::timeout(MOCK_DEADLINE, listing)
                .await
                .expect("the listing did not finish")
                .unwrap_err();
            backend_server.join().await;
            token_server.join().await;
            assert_eq!(
                matches!(error, ModelsError::Unreachable(_)),
                unreachable,
                "{refresh_status}: {error:?}"
            );
            assert_eq!(
                matches!(error, ModelsError::Unauthorized(_)),
                !unreachable,
                "{refresh_status}: {error:?}"
            );
        }
    }
}
