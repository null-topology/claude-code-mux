//! `GET /metrics` end to end: the route in both exposition formats, what an
//! app serves with and without a monitor, and the binary in plain mode.
//!
//! The log and refusal counters are process-wide and shared by every test of
//! this binary, so each test reads only the sample lines it caused itself,
//! keyed on a model id or a log message no other test uses.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use claude_code_mux::{
    config::AliasProvider,
    logging::create_logger,
    monitor::MonitorHandle,
    provider::Provider,
    registry::Registry,
    server::{AppFeatures, app_with_features},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use tower::util::ServiceExt;

#[path = "support/mock_http.rs"]
mod mock_http;

use mock_http::{MockHttp, MockReply};

const OPEN_METRICS_CONTENT_TYPE: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";
const TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// An `Accept` that lists OpenMetrics before the text format.
const OPEN_METRICS_FIRST: &str =
    "application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4;q=0.5";

fn features(metrics: bool) -> AppFeatures {
    AppFeatures {
        metrics,
        ..AppFeatures::default()
    }
}

/// An app with no provider at all.
fn bare_app(monitor: Option<MonitorHandle>, metrics: bool) -> axum::Router {
    let providers: [Arc<dyn Provider>; 0] = [];
    app_with_features(
        Arc::new(Registry::from_providers(
            AliasProvider::Anthropic,
            providers,
        )),
        monitor,
        features(metrics),
    )
}

async fn scrape(app: axum::Router, accept: Option<&str>) -> Response {
    let mut request = Request::builder().method("GET").uri("/metrics");
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    app.oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn content_type(response: &Response) -> &str {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// The sample lines of the series named `name`, whatever their labels.
fn series(exposition: &str, name: &str) -> BTreeSet<String> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '))
        })
        .map(str::to_owned)
        .collect()
}

fn lines(expected: &[&str]) -> BTreeSet<String> {
    expected.iter().map(|line| (*line).to_owned()).collect()
}

fn build_info_line() -> String {
    format!(
        "ccmux_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    )
}

/// Held while a test points `CCP_ANTHROPIC_BASE_URL` at its own mock to
/// build a provider, so two tests building at once cannot swap mocks or
/// leave the other's URL behind.
static ANTHROPIC_BASE_URL: Mutex<()> = Mutex::new(());

fn lock_anthropic_base_url() -> MutexGuard<'static, ()> {
    ANTHROPIC_BASE_URL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The Anthropic passthrough, sending to `base`.
fn anthropic_provider_at(base: &str) -> Arc<dyn Provider> {
    // The provider reads its base URL once, when it is built.
    let _base_url = lock_anthropic_base_url();
    let previous = std::env::var_os("CCP_ANTHROPIC_BASE_URL");
    unsafe {
        std::env::set_var("CCP_ANTHROPIC_BASE_URL", base);
    }
    let provider = Arc::new(claude_code_mux::providers::anthropic::AnthropicProvider::new())
        as Arc<dyn Provider>;
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CCP_ANTHROPIC_BASE_URL", value),
            None => std::env::remove_var("CCP_ANTHROPIC_BASE_URL"),
        }
    }
    provider
}

/// A mock Anthropic upstream that answers every request with one short
/// message and these token counts: 12 input, 340 cache read, 56 cache write,
/// 7 output.
fn anthropic_upstream() -> MockHttp {
    MockHttp::start(|_| {
        MockReply::json(
            200,
            json!({
                "id": "msg_metrics",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 12,
                    "cache_read_input_tokens": 340,
                    "cache_creation_input_tokens": 56,
                    "output_tokens": 7
                }
            })
            .to_string(),
        )
    })
}

/// An app with a fresh monitor whose only provider is the Anthropic
/// passthrough to `upstream`, once it has served one `/v1/messages` request
/// for `model` and its answer was read to the end, which settles it.
async fn app_after_one_request(upstream: &MockHttp, model: &str) -> (axum::Router, MonitorHandle) {
    let monitor = MonitorHandle::default();
    let app = app_with_features(
        Arc::new(Registry::from_providers(
            AliasProvider::Anthropic,
            [anthropic_provider_at(&upstream.base)],
        )),
        Some(monitor.clone()),
        features(true),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("request-id"));
    body_text(response).await;
    assert_eq!(upstream.count(), 1);
    (app, monitor)
}

/// The served request's line of `ccmux_requests_total`.
fn completed_request_line(model: &str) -> String {
    format!(
        "ccmux_requests_total{{provider=\"anthropic\",model=\"{model}\",endpoint=\"messages\",outcome=\"completed\",status=\"200\"}} 1"
    )
}

/// Each exposition format answers with its own content type, and a sample
/// line outside a histogram reads the same in both.
#[tokio::test]
async fn metrics_route_answers_in_both_formats() {
    let app = bare_app(None, true);

    let text = scrape(app.clone(), None).await;
    assert_eq!(text.status(), StatusCode::OK);
    assert_eq!(content_type(&text), TEXT_CONTENT_TYPE);
    let text = body_text(text).await;
    assert!(!text.contains("# EOF"), "{text}");

    let open = scrape(app, Some(OPEN_METRICS_FIRST)).await;
    assert_eq!(open.status(), StatusCode::OK);
    assert_eq!(content_type(&open), OPEN_METRICS_CONTENT_TYPE);
    let open = body_text(open).await;
    assert!(open.ends_with("# EOF\n"), "{open}");

    let build = lines(&[&build_info_line()]);
    assert_eq!(series(&text, "ccmux_build_info"), build, "{text}");
    assert_eq!(series(&open, "ccmux_build_info"), build, "{open}");
}

/// With the setting off the path is not routed and falls to the fallback.
#[tokio::test]
async fn metrics_route_is_absent_when_the_feature_is_off() {
    let response = scrape(
        bare_app(Some(MonitorHandle::default()), false),
        Some(OPEN_METRICS_FIRST),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["error"]["message"], "No route for GET /metrics");
}

/// Without a monitor there is nothing per request to show: the build info,
/// the start time and the process-wide log counter, and no request family.
#[tokio::test]
async fn an_app_without_a_monitor_serves_process_metrics_only() {
    const SERVICE: &str = "metrics-test-no-monitor";
    const EVENT: &str = "metrics test line without a monitor";
    create_logger(SERVICE).info(EVENT, None);

    let text = body_text(scrape(bare_app(None, true), None).await).await;

    assert_eq!(
        series(&text, "ccmux_build_info"),
        lines(&[&build_info_line()]),
        "{text}"
    );
    let start = series(&text, "ccmux_process_start_time_seconds");
    assert_eq!(start.len(), 1, "{text}");
    let seconds: u64 = start
        .first()
        .and_then(|line| line.rsplit(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(seconds > 0, "{text}");
    let logged: BTreeSet<String> = series(&text, "ccmux_log_events_total")
        .into_iter()
        .filter(|line| line.contains(&format!("service=\"{SERVICE}\"")))
        .collect();
    assert_eq!(
        logged,
        lines(&[&format!(
            "ccmux_log_events_total{{service=\"{SERVICE}\",level=\"info\",event=\"{EVENT}\"}} 1"
        )]),
        "{text}"
    );
    for family in [
        "ccmux_request",
        "ccmux_time_to_first_token",
        "ccmux_tokens",
        "ccmux_cache_",
        "ccmux_context_rewrites",
        "ccmux_sessions",
    ] {
        assert!(!text.contains(family), "{family} in {text}");
    }
}

/// A request served by the upstream shows in the request counter and in the
/// token counter with the upstream's own counts, in both formats.
#[tokio::test]
async fn a_served_request_shows_in_the_request_and_token_counters() {
    const MODEL: &str = "claude-metrics-test-served";
    let upstream = anthropic_upstream();
    let (app, _monitor) = app_after_one_request(&upstream, MODEL).await;

    let tokens = |kind: &str, count: u64| {
        format!(
            "ccmux_tokens_total{{provider=\"anthropic\",model=\"{MODEL}\",kind=\"{kind}\",quality=\"exact\"}} {count}"
        )
    };
    for accept in [None, Some(OPEN_METRICS_FIRST)] {
        let text = body_text(scrape(app.clone(), accept).await).await;
        assert_eq!(
            series(&text, "ccmux_requests_total"),
            lines(&[&completed_request_line(MODEL)]),
            "{text}"
        );
        assert_eq!(
            series(&text, "ccmux_tokens_total"),
            lines(&[
                &tokens("input", 12),
                &tokens("cache_read", 340),
                &tokens("cache_write", 56),
                &tokens("output", 7),
            ]),
            "{text}"
        );
    }
}

/// A scrape is not a request: it carries no `request-id`, and the request
/// counter and the monitor's recent list read the same after two scrapes.
#[tokio::test]
async fn scraping_stamps_no_request_id_and_counts_no_request() {
    const MODEL: &str = "claude-metrics-test-scraped";
    let upstream = anthropic_upstream();
    let (app, monitor) = app_after_one_request(&upstream, MODEL).await;
    let served = lines(&[&completed_request_line(MODEL)]);

    for _ in 0..2 {
        let response = scrape(app.clone(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key("request-id"),
            "{:?}",
            response.headers()
        );
        let text = body_text(response).await;
        assert_eq!(series(&text, "ccmux_requests_total"), served, "{text}");
    }
    assert_eq!(monitor.snapshot().recent.len(), 1);
}

/// Stops the proxy process when the test ends, whether it passed or not.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the binary in plain mode (`serve --no-monitor`) on a free loopback
/// port with a cleared environment: every backend on a closed port, no Codex
/// login, all of its state under `home`, plus `extra_env`. Returns once
/// `/healthz` answers.
async fn plain_proxy(
    home: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> (ChildGuard, reqwest::Client, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let closed = "http://127.0.0.1:9";
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-mux"));
    command
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env_clear()
        .env("HOME", home)
        .env("CCP_CONFIG_DIR", home.join("config"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("CCP_CODEX_AUTH_FILE", home.join("auth.json"))
        .env("CCP_CODEX_BASE_URL", closed)
        .env("CCP_CODEX_TRANSPORT", "http")
        .env("CCP_CODEX_CLIENT_VERSION", "9.9.9")
        .env("CCP_CODEX_IMAGES_BASE_URL", closed)
        .env("CCP_ANTHROPIC_BASE_URL", closed)
        .env(
            "CCP_ANTHROPIC_CATALOG_URL",
            format!("{closed}/model-catalog/v1/catalog.json"),
        )
        .env("CCP_KIMI_BASE_URL", closed)
        .env("CCP_KIMI_OAUTH_HOST", closed)
        .env("CCP_GROK_BASE_URL", closed)
        .env("CCP_CURSOR_BASE_URL", closed)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let child = ChildGuard(command.spawn().unwrap());

    let client = claude_code_mux::http_client::client_builder()
        .no_proxy()
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
    {
        assert!(std::time::Instant::now() < deadline, "proxy never came up");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    (child, client, base)
}

/// Plain mode keeps a monitor store for metrics, so its scrape carries the
/// session gauge and not only the process figures.
#[tokio::test]
async fn the_binary_serves_metrics_in_plain_mode() {
    let home = tempfile::TempDir::new().unwrap();
    let (_child, client, base) = plain_proxy(home.path(), &[]).await;

    let response = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(TEXT_CONTENT_TYPE)
    );
    assert!(!response.headers().contains_key("request-id"));
    let text = response.text().await.unwrap();
    assert_eq!(
        series(&text, "ccmux_sessions"),
        lines(&[
            "ccmux_sessions{state=\"active\"} 0",
            "ccmux_sessions{state=\"idle\"} 0",
        ]),
        "{text}"
    );
    assert_eq!(
        series(&text, "ccmux_build_info"),
        lines(&[&build_info_line()]),
        "{text}"
    );
}

/// `CCP_METRICS=off` leaves the path unrouted in the running binary.
#[tokio::test]
async fn the_binary_hides_metrics_when_switched_off() {
    let home = tempfile::TempDir::new().unwrap();
    let (_child, client, base) = plain_proxy(home.path(), &[("CCP_METRICS", "off")]).await;

    let response = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "No route for GET /metrics");
}
