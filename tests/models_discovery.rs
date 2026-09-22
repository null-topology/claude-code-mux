// `/v1/models` answered from the Codex backend's own model listing, through an
// in-process mock upstream with isolated Codex credentials.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use axum::routing::get;
use claude_code_mux::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_mux::providers::codex::models::clear_discovered_models_for_tests;
use claude_code_mux::registry::clear_listed_models_for_tests;
use claude_code_mux::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    let m = ENV_LOCK.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    /// Clear a variable for the duration of the test and put it back after, so
    /// a developer's shell cannot silently redirect what the assertions read.
    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// A Codex CLI style `auth.json` with toy credentials, pointed at by
/// `CCP_CODEX_AUTH_FILE` so the developer's real login is never read.
fn write_codex_auth(dir: &std::path::Path) -> EnvGuard {
    let path = dir.join("auth.json");
    let auth = json!({
        "tokens": {
            "access_token": "test-access",
            "refresh_token": "test-refresh",
            "account_id": "acct_test"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    EnvGuard::set("CCP_CODEX_AUTH_FILE", path)
}

#[derive(Default)]
struct CapturedTraffic {
    /// Query and headers of the last `GET /models` listing call, and how many
    /// of them arrived.
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    listings: usize,
    /// Every completion request, in arrival order.
    completions: Vec<CapturedCompletion>,
}

/// A completion request as the mock backend received it. Which lane a request
/// ran on is only decidable here: asking a translation helper the same question
/// again re-derives the answer instead of reading what was sent.
#[derive(Clone, Debug)]
struct CapturedCompletion {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

fn header_snapshot(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Mock Codex backend: `GET /models` answers `status` with `body`, recording
/// the query and headers it received; any other request is a completed
/// `/responses` stream, recorded in full, so a model taken from the listing can
/// be exercised on `/v1/messages` against the same server.
async fn spawn_codex_upstream(
    status: StatusCode,
    body: Value,
) -> (String, Arc<Mutex<CapturedTraffic>>) {
    spawn_codex_upstream_with_delay(status, body, Duration::ZERO).await
}

/// [`spawn_codex_upstream`] whose listing answers only after `delay`, counted
/// from its arrival, so requests can pile up behind one listing.
async fn spawn_codex_upstream_with_delay(
    status: StatusCode,
    body: Value,
    delay: Duration,
) -> (String, Arc<Mutex<CapturedTraffic>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(CapturedTraffic::default()));

    let models = {
        let captured = captured.clone();
        move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
              headers: axum::http::HeaderMap| {
            let captured = captured.clone();
            let body = body.clone();
            async move {
                if let Ok(mut guard) = captured.lock() {
                    guard.query = query;
                    guard.headers = header_snapshot(&headers);
                    guard.listings += 1;
                }
                tokio::time::sleep(delay).await;
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            }
        }
    };
    let completion = {
        let captured = captured.clone();
        move |method: Method,
              uri: axum::http::Uri,
              headers: axum::http::HeaderMap,
              body: axum::body::Bytes| {
            let captured = captured.clone();
            async move {
                if let Ok(mut guard) = captured.lock() {
                    guard.completions.push(CapturedCompletion {
                        method: method.to_string(),
                        path: uri.path().to_string(),
                        headers: header_snapshot(&headers),
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    });
                }
                http::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(concat!(
                        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"discovered ok\"}\n\n",
                        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
                    )))
                    .unwrap()
            }
        }
    };
    let router = axum::Router::new()
        .route("/models", get(models))
        .fallback(completion);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (format!("http://{addr}"), captured)
}

async fn get_models(uri: &str) -> (StatusCode, Value) {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    let response = app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn post_messages(model: &str) -> Response {
    let _no_proxy_env = EnvGuard::set("NO_PROXY", "127.0.0.1,localhost");
    let registry = Arc::new(Registry::with_default_alias());
    post_through(&registry, "/v1/messages", model, "models-session").await
}

/// A request through a registry the caller keeps, so the registry's own
/// memory of when it last refreshed carries from one request to the next.
/// The caller sets `NO_PROXY` before it builds the registry.
async fn post_through(
    registry: &Arc<Registry>,
    path: &str,
    model: &str,
    session: &str,
) -> Response {
    app(registry.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", session)
                .body(Body::from(
                    json!({
                        "model": model,
                        "max_tokens": 64,
                        "messages": [{"role":"user","content":"hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn provider_entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["provider"] == name)
        .unwrap_or_else(|| panic!("provider {name} missing from {value}"))
}

fn upstream_inventory() -> Value {
    json!({
        "models": [
            {
                "slug": "gpt-7-test",
                "display_name": "Test 7",
                "description": "Newest model, unknown to this build",
                "visibility": "list",
                "priority": 1,
                "supported_in_api": true,
                "use_responses_lite": true,
                "context_window": 272000,
                "max_context_window": 872000,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "fast"},
                    {"effort": "high", "description": "thorough"}
                ],
                "input_modalities": ["text", "image"],
                "model_messages": {"not": "forwarded"}
            },
            {
                "slug": "gpt-5.5",
                "display_name": "GPT-5.5",
                "visibility": "list",
                "supported_in_api": true,
                "use_responses_lite": false
            },
            {
                "slug": "codex-auto-review",
                "visibility": "hide",
                "supported_in_api": true,
                "use_responses_lite": true
            }
        ]
    })
}

/// The model the lane test drives. It is in the compiled-in list, so it routes
/// before any listing, and the compiled-in lane table calls it a lite model.
const LANE_MODEL: &str = "gpt-5.6-sol";

/// A listing naming only [`LANE_MODEL`], so the single thing that differs
/// between the lane cases is the flag the backend reports for it.
fn lane_inventory(use_responses_lite: bool) -> Value {
    json!({
        "models": [
            {
                "slug": LANE_MODEL,
                "display_name": "Sol",
                "visibility": "list",
                "supported_in_api": true,
                "use_responses_lite": use_responses_lite
            }
        ]
    })
}

fn completions(captured: &Arc<Mutex<CapturedTraffic>>) -> Vec<CapturedCompletion> {
    captured.lock().unwrap().completions.clone()
}

fn listings(captured: &Arc<Mutex<CapturedTraffic>>) -> usize {
    captured.lock().unwrap().listings
}

/// The Responses Lite lane as it appears on the wire: the internal lane header,
/// the Codex CLI originator that goes with it, the `client_metadata` entry the
/// backend reads over WebSocket, and the two body fields the lane forces
/// (`parallel_tool_calls: false`, `reasoning.context: "all_turns"`).
fn assert_lite_lane(call: &CapturedCompletion, case: &str) {
    assert_eq!(call.method, "POST", "{case}");
    assert_eq!(call.path, "/", "{case}");
    assert_eq!(call.body["model"], LANE_MODEL, "{case}: requested model");
    assert_eq!(
        call.headers
            .get("x-openai-internal-codex-responses-lite")
            .map(String::as_str),
        Some("true"),
        "{case}: lite lane header"
    );
    assert_eq!(
        call.headers.get("originator").map(String::as_str),
        Some("codex_cli_rs"),
        "{case}: lite lane originator"
    );
    assert_eq!(
        call.body["client_metadata"],
        json!({"ws_request_header_x_openai_internal_codex_responses_lite": "true"}),
        "{case}: lite lane client_metadata"
    );
    assert_eq!(
        call.body["parallel_tool_calls"],
        json!(false),
        "{case}: the lite lane rejects parallel tool calls"
    );
    assert_eq!(
        call.body["reasoning"]["context"], "all_turns",
        "{case}: lite lane reasoning context"
    );
}

/// The full Responses API: the proxy's own originator, no lane header, no lane
/// marker in the body, and the parallel tool calls the lite lane forbids.
fn assert_full_lane(call: &CapturedCompletion, case: &str) {
    assert_eq!(call.method, "POST", "{case}");
    assert_eq!(call.path, "/", "{case}");
    assert_eq!(call.body["model"], LANE_MODEL, "{case}: requested model");
    assert!(
        !call
            .headers
            .contains_key("x-openai-internal-codex-responses-lite"),
        "{case}: the full lane must not send the lite lane header, saw {:?}",
        call.headers
    );
    assert_eq!(
        call.headers.get("originator").map(String::as_str),
        Some("claude-code-proxy"),
        "{case}: full lane originator"
    );
    assert!(
        call.body.get("client_metadata").is_none(),
        "{case}: the full lane must not carry the lite marker, sent {}",
        call.body
    );
    assert_eq!(
        call.body["parallel_tool_calls"],
        json!(true),
        "{case}: the full lane keeps parallel tool calls"
    );
    assert!(
        call.body.pointer("/reasoning/context").is_none(),
        "{case}: `all_turns` is a lite lane field, sent {}",
        call.body
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_endpoint_lists_codex_from_the_backend_and_routes_what_it_listed() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    let (upstream, captured) = spawn_codex_upstream(StatusCode::OK, upstream_inventory()).await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _version_env = EnvGuard::set("CCP_CODEX_CLIENT_VERSION", "9.9.9");

    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::OK, "{value}");

    // The rows are the backend's answer, one `provider` tag each, hidden
    // models and unknown fields passed through as the backend sent them.
    let data = value["data"].as_array().unwrap();
    let ids: Vec<&str> = data.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["gpt-7-test", "gpt-5.5", "codex-auto-review"]);
    for row in data {
        assert_eq!(row["type"], "model");
        assert_eq!(row["object"], "model");
        assert_eq!(row["provider"], "codex");
    }
    assert_eq!(data[0]["display_name"], "Test 7");
    assert_eq!(
        data[0]["description"],
        "Newest model, unknown to this build"
    );
    assert_eq!(data[0]["visibility"], "list");
    assert_eq!(data[0]["use_responses_lite"], true);
    assert_eq!(data[0]["context_window"], 272000);
    assert_eq!(data[0]["supported_reasoning_levels"][1]["effort"], "high");
    assert_eq!(data[0]["input_modalities"][1], "image");
    assert!(data[0].get("model_messages").is_none());
    assert_eq!(data[2]["display_name"], "codex-auto-review");
    assert_eq!(data[2]["visibility"], "hide");
    assert_eq!(value["has_more"], json!(false));
    assert_eq!(value["first_id"], "gpt-7-test");
    assert_eq!(value["last_id"], "codex-auto-review");

    let providers = value["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1, "filter narrows the provider block too");
    let codex = provider_entry(&value, "codex");
    assert_eq!(codex["auth"], "proxy");
    assert_eq!(codex["source"], "upstream");
    assert_eq!(codex["status"], "ok");
    assert!(codex["detail"].is_null());
    assert!(codex["fetched_at"].as_str().unwrap().contains('T'));

    // The backend saw the proxy's own login and the required client version.
    let seen = captured.lock().unwrap();
    assert_eq!(
        seen.query.get("client_version").map(String::as_str),
        Some("9.9.9")
    );
    assert_eq!(
        seen.headers.get("authorization").map(String::as_str),
        Some("Bearer test-access")
    );
    assert_eq!(
        seen.headers.get("chatgpt-account-id").map(String::as_str),
        Some("acct_test")
    );
    assert_eq!(
        seen.headers.get("originator").map(String::as_str),
        Some("claude-code-proxy")
    );
    drop(seen);

    // What the backend listed now routes to codex, base id and -fast alike.
    // Which lane those requests then run on is asserted on the wire by
    // `codex_lane_on_the_wire_follows_the_policy_then_the_backend_flag`.
    for model in ["gpt-7-test", "gpt-7-test-fast"] {
        let response = post_messages(model).await;
        assert_eq!(response.status(), StatusCode::OK, "{model}");
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["content"][0]["text"], "discovered ok", "{model}");
        assert_eq!(body["model"], model);
    }

    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_endpoint_reports_codex_unauthorized_on_403_without_rows() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    let (upstream, _captured) = spawn_codex_upstream(
        StatusCode::FORBIDDEN,
        json!({"error": {"message": "account not entitled"}}),
    )
    .await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);

    // The aggregate answer stays 200 so Claude Code's discovery never fails
    // because one backend refused; the refusal is in the provider block.
    let (status, value) = get_models("/v1/models?limit=1000").await;
    assert_eq!(status, StatusCode::OK);
    let codex = provider_entry(&value, "codex");
    assert_eq!(codex["status"], "unauthorized");
    assert_eq!(codex["source"], "none");
    assert!(
        codex["detail"]
            .as_str()
            .unwrap()
            .contains("account not entitled")
    );
    assert!(
        !value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["provider"] == "codex")
    );

    // Asked for codex alone, the answer is a hard failure, not an empty list.
    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("codex model listing unavailable (unauthorized)"));
    assert!(message.contains("account not entitled"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_endpoint_reports_codex_unreachable_on_5xx_and_malformed_bodies() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());

    let (upstream, _captured) =
        spawn_codex_upstream(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "down"})).await;
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(unreachable): HTTP 503")
    );
    drop(_base_url_env);

    let (upstream, _captured) =
        spawn_codex_upstream(StatusCode::OK, json!({"unexpected": "shape"})).await;
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let (status, value) = get_models("/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    let codex = provider_entry(&value, "codex");
    // A body without `models` is an empty inventory, not an error: the
    // backend answered and listed nothing.
    assert_eq!(codex["status"], "ok");
    assert_eq!(codex["source"], "upstream");
    assert!(
        !value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["provider"] == "codex")
    );
    drop(_base_url_env);

    // A closed port is unreachable too.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &closed);
    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(unreachable)")
    );
}

/// Which lane a Codex request runs on, judged by the requests the backend
/// received rather than by asking a translation helper the same question again.
/// `CCP_CODEX_LANE_POLICY=full` pins the full Responses API; `inventory`
/// follows `use_responses_lite`, which is the compiled-in table until a listing
/// reports one, and the newest listing from then on. The cases run in sequence
/// and each is judged on its own captured request.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn codex_lane_on_the_wire_follows_the_policy_then_the_backend_flag() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _version_env = EnvGuard::set("CCP_CODEX_CLIENT_VERSION", "9.9.9");
    // A configured model or originator would rewrite the very fields the lane
    // is read from, so the test refuses to inherit either.
    let _model_env = EnvGuard::unset("CCP_CODEX_MODEL");
    let _originator_env = EnvGuard::unset("CCP_CODEX_ORIGINATOR");

    // 1) Policy `inventory` with nothing discovered yet: the compiled-in flag
    //    decides, and it calls gpt-5.6-sol a lite model.
    let inventory_policy_env = EnvGuard::set("CCP_CODEX_LANE_POLICY", "inventory");
    let (upstream, backend) = spawn_codex_upstream(StatusCode::OK, lane_inventory(false)).await;
    let base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);

    assert_answered(post_messages(LANE_MODEL).await, "bundled flag").await;
    let calls = completions(&backend);
    assert_eq!(calls.len(), 1, "one upstream request per message");
    assert_eq!(
        listings(&backend),
        0,
        "a message for a model that routes must not fetch the inventory"
    );
    assert_lite_lane(&calls[0], "bundled flag, before any listing");

    // 2) The backend lists the same model as not lite. That flag, not the
    //    compiled-in one, now decides.
    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["data"][0]["id"], LANE_MODEL);
    assert_eq!(value["data"][0]["use_responses_lite"], false);

    clear_all_continuations_for_tests();
    assert_answered(post_messages(LANE_MODEL).await, "listed as full").await;
    let calls = completions(&backend);
    assert_eq!(calls.len(), 2, "one upstream request per message");
    assert_eq!(
        listings(&backend),
        1,
        "the message reused what the listing remembered"
    );
    assert_full_lane(&calls[1], "backend listed use_responses_lite: false");

    // 3) The next successful listing flips the flag back, and the next request
    //    follows it. A listing is the only thing that changes the answer: a
    //    message for a model that routes never asks the backend for one.
    drop(base_url_env);
    let (upstream, backend) = spawn_codex_upstream(StatusCode::OK, lane_inventory(true)).await;
    let base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let (status, value) = get_models("/v1/models?provider=codex").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["data"][0]["use_responses_lite"], true);

    clear_all_continuations_for_tests();
    assert_answered(post_messages(LANE_MODEL).await, "listed as lite").await;
    let calls = completions(&backend);
    assert_eq!(
        calls.len(),
        1,
        "the listing must not cost an extra completion request"
    );
    assert_eq!(listings(&backend), 1, "one listing, then no further fetch");
    assert_lite_lane(&calls[0], "backend listed use_responses_lite: true");

    // 4) Policy `full` overrides that discovered `true` without a new listing.
    drop(inventory_policy_env);
    let _full_policy_env = EnvGuard::set("CCP_CODEX_LANE_POLICY", "full");
    clear_all_continuations_for_tests();
    assert_answered(post_messages(LANE_MODEL).await, "policy full").await;
    let calls = completions(&backend);
    assert_eq!(calls.len(), 2, "one upstream request per message");
    assert_eq!(
        listings(&backend),
        1,
        "the policy override needs no new listing"
    );
    assert_full_lane(&calls[1], "policy full over a discovered lite flag");

    drop(base_url_env);
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
}

/// Every case must be a working request, so a lane assertion can never pass on
/// a request that failed before it reached the backend.
async fn assert_answered(response: Response, case: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{case}");
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["content"][0]["text"], "discovered ok", "{case}");
    assert_eq!(body["model"], LANE_MODEL, "{case}");
}

/// The mock backend answered the request.
async fn assert_routed(response: Response, case: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{case}");
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["content"][0]["text"], "discovered ok", "{case}");
}

/// The 400 an unknown model gets, byte for byte as before any listing existed.
async fn assert_unknown_model(response: Response, registry: &Registry, model: &str) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{model}");
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body,
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": format!(
                    "Unknown model \"{model}\". {}",
                    registry.unknown_model_message()
                ),
            }
        })
    );
}

/// Toy Codex credentials, a mock backend, and plain HTTP to it: the setup of
/// every routing test below. The returned guards restore the environment.
fn routing_env(config: &TempDir, upstream: &str) -> Vec<EnvGuard> {
    vec![
        write_codex_auth(config.path()),
        EnvGuard::set("CCP_CONFIG_DIR", config.path()),
        EnvGuard::set("CCP_CODEX_BASE_URL", upstream),
        EnvGuard::set("CCP_CODEX_TRANSPORT", "http"),
        EnvGuard::set("CCP_CODEX_CLIENT_VERSION", "9.9.9"),
        EnvGuard::set("NO_PROXY", "127.0.0.1,localhost"),
    ]
}

/// A model that only the backend's listing names routes on its first
/// `/v1/messages` request, with nobody calling `/v1/models` first: the miss
/// refreshes the listing once and routes again. `count_tokens` does the same.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_model_only_the_backend_lists_routes_on_its_first_request() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let (upstream, captured) = spawn_codex_upstream(StatusCode::OK, upstream_inventory()).await;
    let _env = routing_env(&config, &upstream);
    let registry = Arc::new(Registry::with_default_alias());

    let response = post_through(&registry, "/v1/messages", "gpt-7-test", "miss-1").await;
    assert_routed(response, "first request").await;
    assert_eq!(listings(&captured), 1, "one listing for the miss");
    assert_eq!(completions(&captured)[0].body["model"], "gpt-7-test");

    // Its fast tier and the next request route from the remembered listing.
    let response = post_through(&registry, "/v1/messages", "gpt-7-test-fast", "miss-2").await;
    assert_routed(response, "fast tier").await;
    let response = post_through(&registry, "/v1/messages", "gpt-7-test", "miss-3").await;
    assert_routed(response, "second request").await;
    assert_eq!(listings(&captured), 1, "nothing left to refresh");

    // The listing replaced the compiled-in list, so a compiled-in model it
    // does not name is unknown now.
    let response = post_through(&registry, "/v1/messages", "gpt-6-astra", "miss-4").await;
    assert_unknown_model(response, &registry, "gpt-6-astra").await;

    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    let registry = Arc::new(Registry::with_default_alias());
    let response = post_through(
        &registry,
        "/v1/messages/count_tokens",
        "gpt-7-test",
        "miss-count",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        listings(&captured),
        2,
        "count_tokens refreshes the same way"
    );
}

/// An id the refreshed listing does not name gets the same 400 as before;
/// a second miss inside the interval does not ask the backend again; and a
/// refresh that fails changes nothing about the answer.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn an_unknown_model_keeps_its_400_and_refreshes_at_most_once_per_interval() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let (upstream, captured) = spawn_codex_upstream(StatusCode::OK, upstream_inventory()).await;
    let env = routing_env(&config, &upstream);
    let registry = Arc::new(Registry::with_default_alias());

    let response = post_through(&registry, "/v1/messages", "gpt-typo", "typo-1").await;
    assert_unknown_model(response, &registry, "gpt-typo").await;
    assert_eq!(listings(&captured), 1, "the miss refreshed once");

    let response = post_through(&registry, "/v1/messages", "gpt-typo-2", "typo-2").await;
    assert_unknown_model(response, &registry, "gpt-typo-2").await;
    assert_eq!(
        listings(&captured),
        1,
        "no second listing inside the interval"
    );
    assert!(completions(&captured).is_empty());
    drop(env);

    let (failing, failing_captured) =
        spawn_codex_upstream(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "down"})).await;
    let _env = routing_env(&config, &failing);
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    let registry = Arc::new(Registry::with_default_alias());
    let response = post_through(&registry, "/v1/messages", "gpt-7-test", "typo-3").await;
    assert_unknown_model(response, &registry, "gpt-7-test").await;
    assert_eq!(listings(&failing_captured), 1, "the refresh was tried");
    assert!(completions(&failing_captured).is_empty());
}

/// Misses that arrive while a refresh is running wait for it instead of
/// starting their own.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn concurrent_misses_share_one_listing() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_listed_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let (upstream, captured) = spawn_codex_upstream_with_delay(
        StatusCode::OK,
        upstream_inventory(),
        Duration::from_millis(300),
    )
    .await;
    let _env = routing_env(&config, &upstream);
    let registry = Arc::new(Registry::with_default_alias());

    let mut requests = tokio::task::JoinSet::new();
    for n in 0..5 {
        let registry = registry.clone();
        requests.spawn(async move {
            post_through(
                &registry,
                "/v1/messages",
                "gpt-7-test",
                &format!("concurrent-{n}"),
            )
            .await
            .status()
        });
    }
    while let Some(status) = requests.join_next().await {
        assert_eq!(status.unwrap(), StatusCode::OK);
    }
    assert_eq!(listings(&captured), 1);
    assert_eq!(completions(&captured).len(), 5);
}

/// Stops the proxy process when the test ends, whether it passed or not.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `serve` lists the models of every provider holding credentials as it
/// starts, so the first request for a model only the backend names routes
/// without a refresh. The proxy runs as its own process with a cleared
/// environment: toy Codex credentials, the mock backend, every other backend
/// on a closed port, and all of its state in a temp dir.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn serve_lists_models_at_start_so_the_first_request_needs_no_refresh() {
    let _guard = env_lock();
    let home = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(home.path());
    let (upstream, captured) = spawn_codex_upstream(StatusCode::OK, upstream_inventory()).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let closed = "http://127.0.0.1:9";
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-mux"))
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env_clear()
        .env("HOME", home.path())
        .env("CCP_CONFIG_DIR", home.path().join("config"))
        .env("XDG_CONFIG_HOME", home.path().join("xdg-config"))
        .env("XDG_DATA_HOME", home.path().join("xdg-data"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("CCP_CODEX_AUTH_FILE", home.path().join("auth.json"))
        .env("CCP_CODEX_BASE_URL", &upstream)
        .env("CCP_CODEX_TRANSPORT", "http")
        .env("CCP_CODEX_CLIENT_VERSION", "9.9.9")
        .env("CCP_CODEX_IMAGES_BASE_URL", closed)
        .env("CCP_ANTHROPIC_BASE_URL", closed)
        .env("CCP_KIMI_BASE_URL", closed)
        .env("CCP_KIMI_OAUTH_HOST", closed)
        .env("CCP_GROK_BASE_URL", closed)
        .env("CCP_CURSOR_BASE_URL", closed)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _child = ChildGuard(child);

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
    {
        assert!(std::time::Instant::now() < deadline, "proxy never came up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    while listings(&captured) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no listing reached the backend at start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The backend counts a listing when it arrives; give the proxy a moment
    // to read the answer.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let response = client
        .post(format!("{base}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "startup-session")
        .body(
            json!({
                "model": "gpt-7-test",
                "max_tokens": 64,
                "messages": [{"role":"user","content":"hello"}]
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK.as_u16());
    assert_eq!(
        listings(&captured),
        1,
        "routed from the listing made at start"
    );
    assert_eq!(completions(&captured)[0].body["model"], "gpt-7-test");
}
