// `/v1/models` answered from the Codex backend's own model listing, through an
// in-process mock upstream with isolated Codex credentials.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use axum::routing::get;
use claude_code_mux::providers::codex::continuation::clear_all_continuations_for_tests;
use claude_code_mux::providers::codex::models::clear_discovered_models_for_tests;
use claude_code_mux::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
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
struct CapturedListing {
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
}

/// Mock Codex backend: `GET /models` answers `status` with `body`, recording
/// the query and headers it received; any other request is a completed
/// `/responses` stream, so a model taken from the listing can be exercised
/// on `/v1/messages` against the same server.
async fn spawn_codex_upstream(
    status: StatusCode,
    body: Value,
) -> (String, Arc<Mutex<CapturedListing>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(CapturedListing::default()));

    let models = {
        let captured = captured.clone();
        move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
              headers: axum::http::HeaderMap| {
            let captured = captured.clone();
            let body = body.clone();
            async move {
                if let Ok(mut guard) = captured.lock() {
                    guard.query = query;
                    guard.headers = headers
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.as_str().to_ascii_lowercase(),
                                value.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect();
                }
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            }
        }
    };
    let completion = || async {
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
    app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "models-session")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_endpoint_lists_codex_from_the_backend_and_routes_what_it_listed() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
    clear_all_continuations_for_tests();
    let config = TempDir::new().unwrap();
    let _codex_auth = write_codex_auth(config.path());
    let (upstream, captured) = spawn_codex_upstream(StatusCode::OK, upstream_inventory()).await;
    let _config_env = EnvGuard::set("CCP_CONFIG_DIR", config.path());
    let _base_url_env = EnvGuard::set("CCP_CODEX_BASE_URL", &upstream);
    let _transport_env = EnvGuard::set("CCP_CODEX_TRANSPORT", "http");
    let _version_env = EnvGuard::set("CCP_CODEX_CLIENT_VERSION", "9.9.9");

    // Before any listing, a model this build does not know is unroutable.
    let response = post_messages("gpt-7-test").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

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

    // What the backend listed now routes to codex, base id and -fast alike,
    // and the lane follows the backend's flag rather than the compiled-in one.
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
    clear_all_continuations_for_tests();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn models_endpoint_reports_codex_unauthorized_on_403_without_rows() {
    let _guard = env_lock();
    clear_discovered_models_for_tests();
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
