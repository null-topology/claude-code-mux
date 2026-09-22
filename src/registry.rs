use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    config::AliasProvider,
    provider::{CliHandlers, ListingSource, ModelListing, Provider, RequestContext},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use once_cell::sync::Lazy;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "fable",
    "claude-fable-5",
];

pub const CURSOR_PREFIXES: &[&str] = &["cursor:", "cursor-plan:", "cursor-ask:"];

const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
];

pub(crate) const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "kimi-k3", "k2.6", "k3"];
pub(crate) const GROK_MODELS: &[&str] = &["grok-composer-2.5-fast", "grok-4.5"];

/// Minimum time between two model listings started by a model id nothing
/// routes. An id that is simply wrong then costs each backend at most one
/// listing per interval, however many requests carry it.
pub const MISS_LISTING_INTERVAL: Duration = Duration::from_secs(30);

/// The model ids each provider's backend named in its last successful
/// listing. For routing they replace that provider's compiled-in list, which
/// only stands in until such a listing arrives. Process-wide like the rest of
/// the proxy's state, so a listing made through any registry counts; a restart
/// forgets it.
static LISTED_MODELS: Lazy<RwLock<BTreeMap<String, HashSet<String>>>> =
    Lazy::new(|| RwLock::new(BTreeMap::new()));

pub struct Registry {
    alias_provider: AliasProvider,
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
    /// When the last refresh started by an unknown id began. Held for the
    /// whole refresh, so concurrent misses wait for it instead of starting
    /// their own.
    miss_listing: tokio::sync::Mutex<Option<Instant>>,
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert(
            "anthropic".into(),
            ANTHROPIC_STYLE_ALIASES
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
        );
        models.insert("codex".into(), expand_codex_models());
        models.insert(
            "kimi".into(),
            KIMI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        models.insert("cursor".into(), build_cursor_models());
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );
        let mut handlers = BTreeMap::new();
        for (name, entries) in &models {
            let handler: Arc<dyn Provider> = match name.as_str() {
                "anthropic" => Arc::new(crate::providers::anthropic::AnthropicProvider::new()),
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                _ => Arc::new(PlaceholderProvider::new(name, entries.clone())),
            };
            handlers.insert(name.clone(), handler);
        }

        Self {
            alias_provider,
            models,
            handlers,
            miss_listing: tokio::sync::Mutex::new(None),
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn from_providers(
        alias_provider: AliasProvider,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        Self {
            alias_provider,
            models,
            handlers,
            miss_listing: tokio::sync::Mutex::new(None),
        }
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        // Claude-shaped models always resolve to the configured alias target (the
        // Anthropic passthrough by default). Session affinity is deliberately NOT
        // consulted here: a codex request earlier in the same session must never drag
        // the opus/haiku slots off the Anthropic backend. This is what lets the opus
        // slot stay on Max while the sonnet slot runs on codex within one session.
        let _ = session_affinity;
        if is_claude_model(&normalized) {
            return self.handlers.get(self.alias_provider.as_str()).cloned();
        }
        if is_cursor_model(&normalized) {
            return self.handlers.get("cursor").cloned();
        }

        // Exact model-name match reaches a specific backend regardless of the alias
        // target: this is how `ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` sends the
        // sonnet slot to codex even while aliases default to the Anthropic passthrough.
        // Once a provider's backend has listed its models, that listing is what
        // it serves; the compiled-in list stands in until then.
        for (name, compiled) in &self.models {
            if name == "anthropic" {
                continue;
            }
            let routes = listed_models_contain(name, &normalized)
                .unwrap_or_else(|| compiled.iter().any(|candidate| candidate == &normalized));
            if routes {
                return self.handlers.get(name).cloned();
            }
        }

        // Codex also serves each model it lists at the fast tier, as `<id>-fast`.
        if let Some(base) = normalized.strip_suffix("-fast")
            && listed_models_contain("codex", base) == Some(true)
        {
            return self.handlers.get("codex").cloned();
        }

        None
    }

    /// [`Self::provider_for_model`], and when nothing routes an id, one fresh
    /// listing from every provider that holds credentials before giving up: a
    /// model a backend serves but this build does not list then routes on its
    /// first request, whether or not anyone asked `/v1/models`. Claude ids,
    /// aliases and cursor ids never cause a listing. Misses share one refresh
    /// at a time and start at most one per [`MISS_LISTING_INTERVAL`]; a listing
    /// that fails changes nothing and the id stays unknown.
    pub async fn provider_for_model_or_refresh(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        if let Some(provider) = self.provider_for_model(raw_model, session_affinity) {
            return Some(provider);
        }
        let normalized = normalize_incoming_model(raw_model);
        if is_claude_model(&normalized) || is_cursor_model(&normalized) {
            return None;
        }
        let mut last_started = self.miss_listing.lock().await;
        // The refresh this request waited for may already have named the id.
        if let Some(provider) = self.provider_for_model(raw_model, session_affinity) {
            return Some(provider);
        }
        if last_started.is_some_and(|started| started.elapsed() < MISS_LISTING_INTERVAL) {
            return None;
        }
        *last_started = Some(Instant::now());
        self.refresh_listings().await;
        self.provider_for_model(raw_model, session_affinity)
    }

    /// Ask every provider that holds credentials for its models, all at once,
    /// and remember each successful answer from a backend for routing. It
    /// never fails: a provider whose listing fails logs that itself and keeps
    /// what it had.
    pub async fn refresh_listings(&self) {
        let listings = self
            .handlers
            .values()
            .filter(|provider| provider.has_credentials())
            .map(|provider| provider.list_models());
        for listing in futures_util::future::join_all(listings).await {
            record_listing(&listing);
        }
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Supported: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

fn is_claude_model(model: &str) -> bool {
    is_anthropic_alias(model) || model.starts_with("claude-")
}

pub fn is_cursor_model(model: &str) -> bool {
    if CURSOR_LEGACY_MODELS.contains(&model) {
        return true;
    }

    CURSOR_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

struct PlaceholderProvider {
    name: &'static str,
    models: Vec<String>,
}

impl PlaceholderProvider {
    fn new(name: &str, models: Vec<String>) -> Self {
        let name = match name {
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "grok" => "grok",
            _ => "codex",
        };
        Self { name, models }
    }
}

#[async_trait]
impl Provider for PlaceholderProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        match self.name {
            "codex" => &CODEX_CLI,
            "kimi" => &KIMI_CLI,
            "cursor" => &CURSOR_CLI,
            "grok" => &GROK_CLI,
            _ => &CODEX_CLI,
        }
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("messages", &ctx.provider)
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("count_tokens", &ctx.provider)
    }
}

fn placeholder_provider_response(route: &str, provider: &str) -> Response {
    let _ = route;
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_provider_error",
        format!("provider '{}' is not yet implemented", provider),
    )
}

#[derive(Clone, Copy)]
struct PlaceholderCli {
    provider: &'static str,
}

impl CliHandlers for PlaceholderCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!("{}: browser login not supported", self.provider))
    }

    fn device(&self) -> Result<()> {
        Err(anyhow!("{}: device login not supported", self.provider))
    }

    fn status(&self) -> Result<()> {
        use serde_json::Value;
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        if crate::auth::load_auth_file_with_legacy::<Value>(&path, &legacy).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> Result<()> {
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        let _ = crate::auth::delete_auth_file(&path, &legacy);
        Ok(())
    }
}

const CODEX_CLI: PlaceholderCli = PlaceholderCli { provider: "codex" };
const KIMI_CLI: PlaceholderCli = PlaceholderCli { provider: "kimi" };
const CURSOR_CLI: PlaceholderCli = PlaceholderCli { provider: "cursor" };
const GROK_CLI: PlaceholderCli = PlaceholderCli { provider: "grok" };
fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    for model in CODEX_MODELS {
        if set.insert((*model).to_string()) {
            out.push((*model).to_string());
        }
        let fast = format!("{model}-fast");
        if set.insert(fast.clone()) {
            out.push(fast);
        }
    }
    out.sort_unstable();
    out
}

/// Remember the ids of a listing that a provider's backend answered as that
/// provider's routing catalog. A failed listing, or one that only repeats the
/// compiled-in list, changes nothing.
pub fn record_listing(listing: &ModelListing) {
    if !listing.is_ok() || listing.source != ListingSource::Upstream {
        return;
    }
    let ids = listing
        .models
        .iter()
        .filter_map(|row| row.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    LISTED_MODELS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(listing.provider.to_string(), ids);
}

/// Whether `provider`'s last listing names `model`; `None` while no listing
/// has arrived for it.
fn listed_models_contain(provider: &str, model: &str) -> Option<bool> {
    LISTED_MODELS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(provider)
        .map(|models| models.contains(model))
}

/// Forget every recorded listing, so routing is back on the compiled-in lists.
pub fn clear_listed_models_for_tests() {
    LISTED_MODELS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

fn build_cursor_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    /// A provider with a listing call of its own that counts how often it was
    /// asked. Each test gives it a name no other test uses, because recorded
    /// listings are process-wide.
    struct ListingProvider {
        name: &'static str,
        credentials: bool,
        compiled: &'static [&'static str],
        listed: &'static [&'static str],
        listings: std::sync::atomic::AtomicUsize,
    }

    impl ListingProvider {
        fn new(
            name: &'static str,
            credentials: bool,
            compiled: &'static [&'static str],
            listed: &'static [&'static str],
        ) -> Arc<Self> {
            Arc::new(Self {
                name,
                credentials,
                compiled,
                listed,
                listings: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn listings(&self) -> usize {
            self.listings.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for ListingProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supported_models(&self) -> Vec<String> {
            self.compiled.iter().map(|m| (*m).to_string()).collect()
        }

        fn cli(&self) -> &'static dyn CliHandlers {
            &CODEX_CLI
        }

        fn has_credentials(&self) -> bool {
            self.credentials
        }

        async fn list_models(&self) -> ModelListing {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ModelListing {
                provider: self.name,
                auth: crate::provider::ListingAuth::Proxy,
                source: ListingSource::Upstream,
                status: "ok",
                detail: None,
                fetched_at: None,
                models: self
                    .listed
                    .iter()
                    .map(|id| serde_json::json!({ "id": id }))
                    .collect(),
            }
        }

        async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
            placeholder_provider_response("messages", &ctx.provider)
        }

        async fn handle_count_tokens(
            &self,
            _body: MessagesRequest,
            ctx: RequestContext,
        ) -> Response {
            placeholder_provider_response("count_tokens", &ctx.provider)
        }
    }

    /// Implementing `list_models` is all a provider needs for a model its
    /// backend lists to route: the first request for it refreshes the
    /// providers holding credentials, and from then on the listing, not the
    /// compiled-in list, is what that provider serves.
    #[tokio::test]
    async fn a_miss_refreshes_providers_with_credentials_and_routes_what_they_listed() {
        let live = ListingProvider::new(
            "unit-live",
            true,
            &["unit-live-compiled"],
            &["unit-live-new"],
        );
        let offline = ListingProvider::new("unit-offline", false, &[], &["unit-offline-new"]);
        let registry = Registry::from_providers(
            AliasProvider::Anthropic,
            [live.clone() as Arc<dyn Provider>, offline.clone()],
        );

        let p = registry.provider_for_model("unit-live-compiled", None);
        assert_eq!(
            p.expect("compiled-in list before a listing").name(),
            "unit-live"
        );
        assert!(registry.provider_for_model("unit-live-new", None).is_none());

        let p = registry
            .provider_for_model_or_refresh("unit-live-new[1m]", None)
            .await;
        assert_eq!(p.expect("routed after the refresh").name(), "unit-live");
        assert_eq!(live.listings(), 1);
        assert_eq!(offline.listings(), 0, "no credentials, never asked");

        let p = registry.provider_for_model("unit-live-new", None);
        assert_eq!(p.expect("the listing is remembered").name(), "unit-live");
        assert!(
            registry
                .provider_for_model("unit-live-compiled", None)
                .is_none(),
            "the listing replaces the compiled-in list"
        );
    }

    #[tokio::test]
    async fn claude_alias_and_cursor_ids_never_start_a_listing() {
        let live = ListingProvider::new("unit-guard", true, &[], &[]);
        let registry = Registry::from_providers(
            AliasProvider::Anthropic,
            [live.clone() as Arc<dyn Provider>],
        );

        for model in [
            "claude-opus-5",
            "claude-unknown-9[1m]",
            "opus",
            "cursor:gpt-5.5",
            "cursor-agent",
        ] {
            assert!(
                registry
                    .provider_for_model_or_refresh(model, None)
                    .await
                    .is_none(),
                "{model}: no provider registered for it"
            );
        }
        assert_eq!(live.listings(), 0);

        // Any other unknown id does refresh, so the zero above is not vacuous.
        assert!(
            registry
                .provider_for_model_or_refresh("unit-guard-typo", None)
                .await
                .is_none()
        );
        assert_eq!(live.listings(), 1);
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Kimi);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "kimi");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "fable",
            "claude-fable-5",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn claude_models_route_to_anthropic_passthrough_by_default() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in [
            "opus",
            "claude-opus-4-8",
            "sonnet",
            "haiku",
            "claude-3-5-haiku-20241022",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route");
            assert_eq!(p.expect("provider").name(), "anthropic", "{model}");
        }
    }

    #[test]
    fn explicit_codex_model_routes_to_codex_while_default_is_anthropic() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("gpt-5.6-terra", None);
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn session_affinity_cannot_hijack_claude_slot() {
        // Even if a prior codex request set Codex affinity, claude aliases stay on anthropic.
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("claude-opus-4-8", Some(&AliasProvider::Codex));
        assert_eq!(p.expect("provider").name(), "anthropic");
    }

    #[test]
    fn cursor_prefix_routes() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("cursor:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-plan:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-ask:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
    }
}
