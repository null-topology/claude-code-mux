//! The Claude rows of `/v1/models`.
//!
//! When the client request carries an Anthropic credential (`x-api-key` or
//! `authorization`), the listing is relayed to Anthropic's own
//! `GET /v1/models` with the client's headers, and the rows are Anthropic's,
//! untouched. Without one, or when that relay fails, the rows come from the
//! public Claude model catalog, mapped onto `ModelInfo` with
//! `capabilities: null`.
//!
//! The catalog is signed, but the proxy has no key to verify the signature
//! with and does not try: its rows are informational. Routing never reads
//! them; `claude-*` ids and the aliases route by prefix and name whatever any
//! listing says.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use once_cell::sync::Lazy;
use serde_json::{Map, Value};

use crate::logging::create_logger;
use crate::provider::{
    ListingAuth, ListingRequest, ListingSource, ModelListing, model_info_in_line,
};

/// The public Claude model catalog, read when no credential is at hand.
pub const DEFAULT_CATALOG_URL: &str = "https://downloads.claude.ai/model-catalog/v1/catalog.json";

/// How long a fetched catalog is served before the next call fetches again.
pub const CATALOG_TTL: Duration = Duration::from_secs(10 * 60);

/// Budget for one listing: the whole relay with every page it follows, or
/// one catalog fetch, including the wait for a fetch another call started.
/// The fetch has a budget of its own of the same length, so it runs to its
/// end even when the calls waiting for it stop waiting first.
pub const LISTING_TIMEOUT: Duration = Duration::from_secs(4);

/// Largest relay page or catalog body read. A longer one is a `body` failure.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// `anthropic-version` added to a relayed listing whose client sent none.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Page size asked of Anthropic, the largest its Models API accepts.
const RELAY_PAGE_LIMIT: usize = 1000;

/// Pages followed through `has_more` before the relay keeps what it has.
const RELAY_MAX_PAGES: usize = 10;

/// `runtime.family` values that name a Claude line.
const MODEL_LINES: [&str; 5] = ["haiku", "sonnet", "opus", "fable", "mythos"];

const LOGGER: &str = "anthropic";

static LISTING_TIMEOUT_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

fn listing_timeout() -> Duration {
    match LISTING_TIMEOUT_OVERRIDE_MS.load(Ordering::Relaxed) {
        0 => LISTING_TIMEOUT,
        ms => Duration::from_millis(ms),
    }
}

/// Replace [`LISTING_TIMEOUT`] for the tests that reach it, so they need not
/// wait four seconds; `None` restores it. It shortens the relay and the wait
/// for a catalog fetch, not the fetch's own budget.
pub fn set_anthropic_listing_timeout_for_tests(timeout: Option<Duration>) {
    let ms = timeout.map_or(0, |timeout| {
        u64::try_from(timeout.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    });
    LISTING_TIMEOUT_OVERRIDE_MS.store(ms, Ordering::Relaxed);
}

/// The Claude listing for one `/v1/models` call. The relay goes out on the
/// passthrough `client`; the catalog is read with `catalog_client`.
pub(super) async fn list(
    client: &reqwest::Client,
    catalog_client: &reqwest::Client,
    base_url: &str,
    request: ListingRequest<'_>,
) -> ModelListing {
    if let Some(headers) = request
        .headers
        .filter(|headers| carries_credential(headers))
    {
        let relayed = tokio::time::timeout(
            listing_timeout(),
            relay_pages(
                client,
                base_url,
                request.lifecycle_pairs,
                relay_headers(headers),
            ),
        )
        .await
        .unwrap_or(Err(ListingFailure::new(0, FailureClass::Timeout)));
        match relayed {
            Ok(rows) => return listing(ListingSource::Relayed, now_rfc3339(), rows),
            Err(failure) => create_logger(LOGGER).warn(
                "anthropic model listing relay failed; using the public catalog",
                Some(failure.fields()),
            ),
        }
    }
    match catalog(catalog_client, &crate::config::anthropic_catalog_url()).await {
        Ok((fetched_at, rows)) => listing(ListingSource::Catalog, fetched_at, rows.to_vec()),
        Err(failure) => {
            let mut listing =
                ModelListing::unavailable("anthropic", "unreachable", failure.catalog_detail());
            listing.auth = ListingAuth::Client;
            listing
        }
    }
}

fn listing(source: ListingSource, fetched_at: String, models: Vec<Value>) -> ModelListing {
    ModelListing {
        provider: "anthropic",
        auth: ListingAuth::Client,
        source,
        status: "ok",
        detail: None,
        fetched_at: Some(fetched_at),
        models,
    }
}

/// A non-empty `x-api-key` or `authorization` header.
fn carries_credential(headers: &HeaderMap) -> bool {
    ["x-api-key", "authorization"].into_iter().any(|name| {
        headers
            .get_all(name)
            .iter()
            .any(|value| !value.as_bytes().trim_ascii().is_empty())
    })
}

/// The client's headers as the messages relay forwards them, without
/// `content-type`, with `anthropic-version` added when the client sent none.
/// The credential goes out as the client sent it.
fn relay_headers(client: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(client.len() + 1);
    for (name, value) in client {
        if super::is_stripped_request_header(name.as_str()) || name == CONTENT_TYPE {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if !headers.contains_key("anthropic-version") {
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
    }
    headers
}

/// Anthropic's `GET /v1/models`, page after page while `has_more` holds, up
/// to [`RELAY_MAX_PAGES`]. Rows without a string `id` are skipped; the
/// others are kept as they came. Any page that fails fails the whole relay,
/// so rows of earlier pages are never served alone. A relay that stops with
/// `has_more` still true, at the page cap or for want of a cursor that moves
/// on (none, or the one the page was asked with), keeps what it has and logs
/// `anthropic model listing truncated`.
async fn relay_pages(
    client: &reqwest::Client,
    base_url: &str,
    lifecycle_pairs: &[String],
    headers: HeaderMap,
) -> Result<Vec<Value>, ListingFailure> {
    let mut rows = Vec::new();
    let mut after_id: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut target = format!("{base_url}/v1/models?limit={RELAY_PAGE_LIMIT}");
        for pair in lifecycle_pairs {
            target.push('&');
            target.push_str(pair);
        }
        if let Some(after_id) = &after_id {
            target.push_str("&after_id=");
            target.extend(url::form_urlencoded::byte_serialize(after_id.as_bytes()));
        }
        let response = client
            .get(&target)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| ListingFailure::transport(0, &error))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(ListingFailure::new(status, FailureClass::Status));
        }
        let body = read_capped(response, status).await?;
        let page: Value = serde_json::from_slice(&body)
            .map_err(|_| ListingFailure::new(status, FailureClass::Body))?;
        let Some(data) = page.get("data").and_then(Value::as_array) else {
            return Err(ListingFailure::new(status, FailureClass::Body));
        };
        pages += 1;
        rows.extend(
            data.iter()
                .filter(|row| row.get("id").and_then(Value::as_str).is_some())
                .cloned(),
        );
        if page.get("has_more").and_then(Value::as_bool) != Some(true) {
            break;
        }
        let next = page.get("last_id").and_then(Value::as_str).or_else(|| {
            data.last()
                .and_then(|row| row.get("id"))
                .and_then(Value::as_str)
        });
        match next.filter(|id| after_id.as_deref() != Some(*id)) {
            Some(id) if pages < RELAY_MAX_PAGES => after_id = Some(id.to_string()),
            _ => {
                let mut fields = Map::new();
                fields.insert("pages".into(), Value::from(pages));
                create_logger(LOGGER).warn("anthropic model listing truncated", Some(fields));
                break;
            }
        }
    }
    Ok(rows)
}

/// The body of `response`, read chunk by chunk up to [`MAX_BODY_BYTES`]. A
/// longer body, declared or read, is a `body` failure.
async fn read_capped(
    mut response: reqwest::Response,
    status: u16,
) -> Result<Vec<u8>, ListingFailure> {
    let too_long = ListingFailure::new(status, FailureClass::Body);
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(too_long);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ListingFailure::transport(status, &error))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(too_long);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

struct CachedCatalog {
    expires_at: Instant,
    fetched_at: String,
    rows: Arc<Vec<Value>>,
}

/// Fetched catalogs by URL. A failed fetch is never stored, so there is no
/// stale fallback: once an entry expires, the next call fetches or fails.
static CATALOG_CACHE: Lazy<Mutex<HashMap<String, CachedCatalog>>> = Lazy::new(Default::default);

type CatalogResult = Result<(String, Arc<Vec<Value>>), ListingFailure>;

/// One catalog fetch in progress. It runs as its own task, so it ends the
/// same way whether or not the calls waiting for it are still there.
struct CatalogFlight {
    id: u64,
    result: Shared<BoxFuture<'static, CatalogResult>>,
}

/// The fetch in progress for each catalog URL. A call that finds no fresh
/// entry joins the flight of its URL, or starts one when there is none, and
/// gets that flight's result, a failure included. A flight leaves the map
/// when it ends, after a success has been stored, so a failure reaches only
/// the calls that waited for it and the next call fetches again. A task
/// dropped before its first poll (a runtime shutting down) never runs the
/// guard that takes its flight out, so the first call that sees it end takes
/// it out instead, and a call never joins a flight whose result is already
/// in: it starts a new one in its place.
static CATALOG_FLIGHTS: Lazy<Mutex<HashMap<String, CatalogFlight>>> = Lazy::new(Default::default);

static NEXT_CATALOG_FLIGHT: AtomicU64 = AtomicU64::new(0);

fn catalog_cache() -> MutexGuard<'static, HashMap<String, CachedCatalog>> {
    CATALOG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn catalog_flights() -> MutexGuard<'static, HashMap<String, CatalogFlight>> {
    CATALOG_FLIGHTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn clear_anthropic_catalog_cache_for_tests() {
    catalog_cache().clear();
}

/// Expire every cached catalog now, as if [`CATALOG_TTL`] had passed.
pub fn expire_anthropic_catalog_cache_for_tests() {
    let now = Instant::now();
    for entry in catalog_cache().values_mut() {
        entry.expires_at = now;
    }
}

fn fresh_catalog(url: &str) -> Option<(String, Arc<Vec<Value>>)> {
    let cache = catalog_cache();
    let entry = cache.get(url)?;
    (Instant::now() < entry.expires_at).then(|| (entry.fetched_at.clone(), Arc::clone(&entry.rows)))
}

/// The catalog rows and when they were fetched, from the cache while fresh,
/// else from the one fetch of this URL in progress. The wait is bounded by
/// the listing timeout.
async fn catalog(client: &reqwest::Client, url: &str) -> CatalogResult {
    if let Some(fresh) = fresh_catalog(url) {
        return Ok(fresh);
    }
    let flight = {
        let mut flights = catalog_flights();
        match flights
            .get(url)
            .filter(|flight| flight.result.peek().is_none())
        {
            Some(flight) => flight.result.clone(),
            // No flight, or one that ended without leaving the map, which the
            // new flight replaces.
            None => {
                // A flight that ended since the check above stored its rows
                // before it left the map.
                if let Some(fresh) = fresh_catalog(url) {
                    return Ok(fresh);
                }
                let flight = start_catalog_flight(client.clone(), url.to_string());
                let result = flight.result.clone();
                flights.insert(url.to_string(), flight);
                result
            }
        }
    };
    tokio::time::timeout(listing_timeout(), flight)
        .await
        .unwrap_or(Err(ListingFailure::new(0, FailureClass::Timeout)))
}

/// Spawn the fetch of `url`. The caller holds the flight map while it
/// inserts the flight, so the task cannot leave the map before it is in.
/// A task that panics or is dropped before it runs is a `task` failure,
/// logged as a failed fetch, and its flight leaves the map.
fn start_catalog_flight(client: reqwest::Client, url: String) -> CatalogFlight {
    let id = NEXT_CATALOG_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let task = {
        let url = url.clone();
        tokio::spawn(async move {
            let _leave = LeaveFlight {
                url: url.clone(),
                id,
            };
            fetch_and_store(&client, &url).await
        })
    };
    let result = async move {
        task.await.unwrap_or_else(|_| {
            leave_flight(&url, id);
            let failure = ListingFailure::new(0, FailureClass::Task);
            create_logger(LOGGER).warn(
                "anthropic model catalog fetch failed",
                Some(failure.fields()),
            );
            Err(failure)
        })
    }
    .boxed()
    .shared();
    CatalogFlight { id, result }
}

/// Takes a flight out of the map when its task ends, however it ends, once
/// the task has run at all.
struct LeaveFlight {
    url: String,
    id: u64,
}

impl Drop for LeaveFlight {
    fn drop(&mut self) {
        leave_flight(&self.url, self.id);
    }
}

/// Take flight `id` of `url` out of the map, unless another flight has
/// taken its place.
fn leave_flight(url: &str, id: u64) {
    let mut flights = catalog_flights();
    if flights.get(url).is_some_and(|flight| flight.id == id) {
        flights.remove(url);
    }
}

/// Put a flight for `url` into the flight map that has already failed, as a
/// flight that ended without leaving the map would be.
pub fn insert_ended_anthropic_catalog_flight_for_tests(url: &str) {
    let id = NEXT_CATALOG_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let result: Shared<BoxFuture<'static, CatalogResult>> =
        async { Err(ListingFailure::new(0, FailureClass::Task)) }
            .boxed()
            .shared();
    let _ = result.clone().now_or_never();
    catalog_flights().insert(url.to_string(), CatalogFlight { id, result });
}

/// Put a flight for `url` into the flight map whose fetch task was dropped
/// before its first poll, as a runtime that shuts down leaves one. The task
/// is spawned on a runtime of its own, on a thread of its own, and that
/// runtime is dropped without ever running it.
pub fn insert_unstarted_anthropic_catalog_flight_for_tests(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build a runtime");
        let flight = {
            let _entered = runtime.enter();
            start_catalog_flight(reqwest::Client::new(), url.clone())
        };
        catalog_flights().insert(url, flight);
        drop(runtime);
    })
    .join()
    .expect("failed to leave an unstarted flight");
}

/// Whether the flight map holds a flight for `url`.
pub fn anthropic_catalog_flight_pending_for_tests(url: &str) -> bool {
    catalog_flights().contains_key(url)
}

/// One fetch, logged, and stored in the cache when it succeeds.
async fn fetch_and_store(client: &reqwest::Client, url: &str) -> CatalogResult {
    let logger = create_logger(LOGGER);
    match fetch_catalog(client, url).await {
        Ok((version, rows)) => {
            let mut fields = Map::new();
            fields.insert("version".into(), version);
            fields.insert("models".into(), Value::from(rows.len()));
            logger.info("anthropic model catalog fetched", Some(fields));
            let rows = Arc::new(rows);
            let fetched_at = now_rfc3339();
            catalog_cache().insert(
                url.to_string(),
                CachedCatalog {
                    expires_at: Instant::now() + CATALOG_TTL,
                    fetched_at: fetched_at.clone(),
                    rows: Arc::clone(&rows),
                },
            );
            Ok((fetched_at, rows))
        }
        Err(failure) => {
            logger.warn(
                "anthropic model catalog fetch failed",
                Some(failure.fields()),
            );
            Err(failure)
        }
    }
}

/// One plain GET of the catalog, no client header and no credential. Gives
/// the catalog's numeric `version` (null when it has none) and its rows; a
/// catalog with no row counts as a failure.
async fn fetch_catalog(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Value, Vec<Value>), ListingFailure> {
    let fetched = async {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| ListingFailure::transport(0, &error))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(ListingFailure::new(status, FailureClass::Status));
        }
        let body = read_capped(response, status).await?;
        Ok((status, body))
    };
    let (status, body) = tokio::time::timeout(LISTING_TIMEOUT, fetched)
        .await
        .unwrap_or(Err(ListingFailure::new(0, FailureClass::Timeout)))?;
    let catalog: Value = serde_json::from_slice(&body)
        .map_err(|_| ListingFailure::new(status, FailureClass::Body))?;
    let rows = catalog_rows(&catalog);
    if rows.is_empty() {
        return Err(ListingFailure::new(status, FailureClass::Empty));
    }
    let version = catalog
        .get("version")
        .filter(|version| version.is_number())
        .cloned()
        .unwrap_or(Value::Null);
    Ok((version, rows))
}

/// `ModelInfo` rows from `surfaces.cc.model_selector_config[*].models[*]`:
/// models offered on Anthropic's own API (an `offered_on` that is absent or
/// names `first_party`), the first occurrence of each id.
fn catalog_rows(catalog: &Value) -> Vec<Value> {
    let configs = catalog
        .pointer("/surfaces/cc/model_selector_config")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for model in configs
        .iter()
        .filter_map(|config| config.get("models").and_then(Value::as_array))
        .flatten()
    {
        let Some(id) = model
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if !offered_first_party(model) || !seen.insert(id) {
            continue;
        }
        let display_name = model
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(id);
        let runtime = model.get("runtime");
        let limit = |key: &str| {
            runtime
                .and_then(|runtime| runtime.get(key))
                .and_then(Value::as_u64)
        };
        let line = runtime
            .and_then(|runtime| runtime.get("family"))
            .and_then(Value::as_str)
            .filter(|family| MODEL_LINES.contains(family));
        rows.push(model_info_in_line(
            id,
            display_name,
            limit("max_input_tokens"),
            limit("max_output_tokens"),
            None,
            line,
        ));
    }
    rows
}

/// Only an absent `offered_on` leaves a model unrestricted. `null`, an empty
/// array, any other value and an array without `first_party` drop it.
fn offered_first_party(model: &Value) -> bool {
    match model.get("offered_on") {
        None => true,
        Some(Value::Array(surfaces)) => surfaces.iter().any(|surface| surface == "first_party"),
        Some(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    Status,
    Timeout,
    Network,
    Body,
    Empty,
    /// The catalog fetch task ended without a result: it panicked, or was
    /// dropped before it ran.
    Task,
}

impl FailureClass {
    fn as_str(self) -> &'static str {
        match self {
            FailureClass::Status => "status",
            FailureClass::Timeout => "timeout",
            FailureClass::Network => "network",
            FailureClass::Body => "body",
            FailureClass::Empty => "empty",
            FailureClass::Task => "task",
        }
    }
}

/// Why a relay or a catalog fetch gave no rows: the HTTP status (0 when none
/// came) and a short class. Nothing from the request or the body.
#[derive(Debug, Clone, Copy)]
struct ListingFailure {
    status: u16,
    class: FailureClass,
}

impl ListingFailure {
    fn new(status: u16, class: FailureClass) -> Self {
        Self { status, class }
    }

    fn transport(status: u16, error: &reqwest::Error) -> Self {
        let class = if error.is_timeout() {
            FailureClass::Timeout
        } else {
            FailureClass::Network
        };
        Self::new(status, class)
    }

    fn fields(self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("status".into(), Value::from(self.status));
        fields.insert("class".into(), Value::from(self.class.as_str()));
        fields
    }

    fn catalog_detail(self) -> String {
        match self.class {
            FailureClass::Status => format!("public model catalog answered {}", self.status),
            FailureClass::Timeout => "public model catalog timed out".to_string(),
            FailureClass::Network => "public model catalog unreachable".to_string(),
            FailureClass::Body => "public model catalog body is not a catalog".to_string(),
            FailureClass::Empty => "public model catalog names no model".to_string(),
            FailureClass::Task => "public model catalog fetch did not complete".to_string(),
        }
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_rows_map_first_party_models_once() {
        let catalog = json!({
            "version": 3,
            "surfaces": {"cc": {"model_selector_config": [
                {"models": [
                    {"id": "claude-a-1", "name": "A 1", "offered_on": ["first_party", "vertex"],
                     "runtime": {"max_input_tokens": 1000, "max_output_tokens": 100, "family": "opus"}},
                    {"id": "claude-b-1", "offered_on": ["bedrock"]},
                    {"id": "claude-c-1", "runtime": {"family": "unknown"}},
                    {"name": "no id"},
                ]},
                {"models": [{"id": "claude-a-1", "name": "Again"}]},
            ]}}
        });
        let rows = catalog_rows(&catalog);
        let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["claude-a-1", "claude-c-1"]);
        assert_eq!(rows[0]["display_name"], "A 1");
        assert_eq!(rows[0]["max_input_tokens"], 1000);
        assert_eq!(rows[0]["max_tokens"], 100);
        assert_eq!(rows[0]["line"], "opus");
        assert!(rows[0]["capabilities"].is_null());
        assert_eq!(rows[1]["display_name"], "claude-c-1");
        assert!(rows[1]["line"].is_null());
        assert!(rows[1]["max_input_tokens"].is_null());
        assert_eq!(rows[1].as_object().unwrap().len(), 11);
    }

    #[test]
    fn catalog_without_the_selector_has_no_rows() {
        assert!(catalog_rows(&json!({"surfaces": {}})).is_empty());
        assert!(catalog_rows(&json!([])).is_empty());
    }

    #[test]
    fn only_an_absent_offered_on_or_one_naming_first_party_keeps_a_row() {
        let cases = [
            ("claude-absent", None, true),
            (
                "claude-listed",
                Some(json!(["vertex", "first_party"])),
                true,
            ),
            ("claude-null", Some(Value::Null), false),
            ("claude-empty", Some(json!([])), false),
            ("claude-string", Some(json!("first_party")), false),
            ("claude-object", Some(json!({"first_party": true})), false),
            (
                "claude-elsewhere",
                Some(json!(["vertex", "bedrock"])),
                false,
            ),
        ];
        for (id, offered_on, kept) in cases {
            let mut model = json!({"id": id});
            if let Some(offered_on) = offered_on {
                model["offered_on"] = offered_on;
            }
            let catalog = json!({"surfaces": {"cc": {"model_selector_config": [
                {"models": [model]}
            ]}}});
            let ids: Vec<Value> = catalog_rows(&catalog)
                .into_iter()
                .map(|row| row["id"].clone())
                .collect();
            let expected: Vec<Value> = if kept { vec![json!(id)] } else { vec![] };
            assert_eq!(ids, expected, "{id}");
        }
    }

    #[test]
    fn only_a_non_empty_key_or_authorization_is_a_credential() {
        let mut headers = HeaderMap::new();
        assert!(!carries_credential(&headers));
        headers.insert("x-api-key", HeaderValue::from_static(" "));
        headers.insert("anthropic-beta", HeaderValue::from_static("x"));
        assert!(!carries_credential(&headers));
        headers.insert("authorization", HeaderValue::from_static("Bearer t"));
        assert!(carries_credential(&headers));
    }

    #[test]
    fn relay_headers_keep_the_client_version_and_drop_framing() {
        let mut client = HeaderMap::new();
        client.insert("x-api-key", HeaderValue::from_static("toy-key"));
        client.insert("content-type", HeaderValue::from_static("application/json"));
        client.insert("content-length", HeaderValue::from_static("0"));
        client.insert("host", HeaderValue::from_static("127.0.0.1"));
        let headers = relay_headers(&client);
        assert_eq!(headers["x-api-key"], "toy-key");
        assert_eq!(headers["anthropic-version"], ANTHROPIC_VERSION);
        for dropped in ["content-type", "content-length", "host"] {
            assert!(!headers.contains_key(dropped), "{dropped}");
        }
        client.insert("anthropic-version", HeaderValue::from_static("2099-01-01"));
        let headers = relay_headers(&client);
        assert_eq!(headers.get_all("anthropic-version").iter().count(), 1);
        assert_eq!(headers["anthropic-version"], "2099-01-01");
    }
}
