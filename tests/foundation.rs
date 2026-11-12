use claude_code_mux::anthropic::{
    encode_sse_event, parse_sse_events, parse_sse_events_with_stats, schema::MessagesRequest,
};
use claude_code_mux::auth::{AuthStorage, InMemoryAuthStore};
use claude_code_mux::config::{AliasProvider, load_config_for_env};
use claude_code_mux::logging::{create_logger, redact_value};
use claude_code_mux::paths::{self, DirResolverEnv};
use claude_code_mux::retry::{RETRY_INITIAL_DELAY_MS, RETRY_MAX_DELAY_MS, compute_backoff_delay};
use claude_code_mux::traffic::{
    MAX_SSE_CAPTURE_BYTES, TrafficCaptureOptions, create_traffic_capture, redact_traffic,
    sanitize_path_part, traffic_capture_enabled_for_env,
};
use serde_json::Map;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use tempfile::TempDir;

#[test]
fn messages_fixture_and_sse_parsing() {
    let raw = std::fs::read_to_string("tests/fixtures/anthropic-message.json").unwrap();
    let req: MessagesRequest = serde_json::from_str(&raw).unwrap();
    assert_eq!(req.model.as_deref(), Some("gpt-5.4[1m]"));
    assert!(req.extra.contains_key("extra_field"));

    let raw_sse = std::fs::read_to_string("tests/fixtures/sse-basic.txt").unwrap();
    let events = parse_sse_events(raw_sse.as_bytes());
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event, None);
    assert_eq!(events[0].data, "{\"chunk\":1}");
    assert_eq!(events[1].event.as_deref(), Some("update"));
    assert_eq!(events[1].data, "{\"text\":\"line1\"}\n{\"text\":\"line2\"}");
    assert_eq!(events[2].event.as_deref(), Some("final"));
    assert_eq!(events[2].data, "{\"done\":true}");

    let (events_with_stats, stats) = parse_sse_events_with_stats(raw_sse.as_bytes());
    assert_eq!(events_with_stats.len(), events.len());
    assert!(stats.event_count >= 2);

    let encoded = encode_sse_event(Some("message"), "{\"a\":1}\n{\"b\":2}");
    assert_eq!(
        encoded,
        b"event: message\ndata: {\"a\":1}\ndata: {\"b\":2}\n\n".to_vec()
    );
    let reparsed = parse_sse_events(&encoded);
    assert_eq!(reparsed[0].event.as_deref(), Some("message"));
    assert_eq!(reparsed[0].data, "{\"a\":1}\n{\"b\":2}");
    let first_line: serde_json::Value =
        serde_json::from_str(reparsed[0].data.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["a"], json!(1));
}

fn resolver_env(platform: &str, home: &str, vars: &[(&str, &str)]) -> DirResolverEnv {
    DirResolverEnv {
        platform: platform.to_string(),
        home: home.into(),
        env: vars
            .iter()
            .map(|&(name, value)| (name.to_string(), value.into()))
            .collect(),
    }
}

fn resolved_dirs(deps: &DirResolverEnv) -> [String; 2] {
    [
        paths::resolve_config_dir(deps)
            .to_string_lossy()
            .into_owned(),
        paths::resolve_state_dir(deps)
            .to_string_lossy()
            .into_owned(),
    ]
}

const XDG_VARS: [(&str, &str); 2] = [
    ("XDG_CONFIG_HOME", "/x/config"),
    ("XDG_STATE_HOME", "/x/state"),
];

const ALL_DIR_VARS: [(&str, &str); 4] = [
    ("APPDATA", "D:\\Roaming"),
    ("LOCALAPPDATA", "D:\\Local"),
    XDG_VARS[0],
    XDG_VARS[1],
];

#[test]
fn path_resolvers_cover_platform_rules() {
    // macOS follows the same XDG rule as Linux.
    for platform in ["macos", "linux"] {
        assert_eq!(
            resolved_dirs(&resolver_env(platform, "/home/u", &[])),
            [
                "/home/u/.config/claude-code-proxy",
                "/home/u/.local/state/claude-code-proxy"
            ],
            "{platform}"
        );
        assert_eq!(
            resolved_dirs(&resolver_env(platform, "/home/u", &XDG_VARS)),
            ["/x/config/claude-code-proxy", "/x/state/claude-code-proxy"],
            "{platform}"
        );
    }

    // Windows reads APPDATA and LOCALAPPDATA and ignores the XDG variables.
    assert_eq!(
        resolved_dirs(&resolver_env("windows", "C:\\Users\\u", &ALL_DIR_VARS)),
        [
            "D:\\Roaming/claude-code-proxy",
            "D:\\Local/claude-code-proxy"
        ]
    );
    assert_eq!(
        resolved_dirs(&resolver_env("windows", "C:\\Users\\u", &[])),
        [
            "C:\\Users\\u\\AppData\\Roaming/claude-code-proxy",
            "C:\\Users\\u\\AppData\\Local/claude-code-proxy"
        ]
    );

    // CCP_CONFIG_DIR moves the config directory on every platform, and only it.
    for (platform, state) in [
        ("windows", "D:\\Local/claude-code-proxy"),
        ("macos", "/x/state/claude-code-proxy"),
        ("linux", "/x/state/claude-code-proxy"),
    ] {
        let mut vars = ALL_DIR_VARS.to_vec();
        vars.push(("CCP_CONFIG_DIR", "/tmp/ccp-config"));
        assert_eq!(
            resolved_dirs(&resolver_env(platform, "/home/u", &vars)),
            ["/tmp/ccp-config", state],
            "{platform}"
        );
    }
}

#[test]
fn path_resolvers_key_on_std_env_consts_os() {
    // The resolvers branch on `std::env::consts::OS` values. A branch keyed on a
    // name it never reports would never run in a real build.
    assert_eq!(DirResolverEnv::default().platform, std::env::consts::OS);

    let host = resolved_dirs(&resolver_env(
        std::env::consts::OS,
        "/home/u",
        &ALL_DIR_VARS,
    ));
    if cfg!(target_os = "windows") {
        assert_eq!(
            host,
            [
                "D:\\Roaming/claude-code-proxy",
                "D:\\Local/claude-code-proxy"
            ]
        );
    } else {
        assert_eq!(
            host,
            ["/x/config/claude-code-proxy", "/x/state/claude-code-proxy"]
        );
    }

    // Names `std::env::consts::OS` never reports get no rule of their own.
    for platform in ["win32", "darwin"] {
        assert_eq!(
            resolved_dirs(&resolver_env(platform, "/home/u", &ALL_DIR_VARS)),
            ["/x/config/claude-code-proxy", "/x/state/claude-code-proxy"],
            "{platform}"
        );
    }
}

#[test]
fn logging_redaction_and_truncation() {
    let short =
        json!({"authorization": "abc", "visible": "x", "nested": {"set-cookie": "c", "v": "y"}});
    let redacted = redact_value(short);
    assert_eq!(redacted["visible"], json!("x"));
    assert!(
        redacted["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );

    let long = "a".repeat(5000);
    let long_payload = redact_value(json!({"text": long}));
    let text = long_payload["text"].as_str().unwrap();
    assert!(text.contains("[1000 more]"));
}

#[test]
fn traffic_capture_helpers() {
    let mut env = HashMap::new();
    env.insert("CCP_TRAFFIC_LOG".into(), "1".into());
    assert!(traffic_capture_enabled_for_env(&env));
    assert!(!traffic_capture_enabled_for_env(&HashMap::new()));

    assert_eq!(sanitize_path_part("a*b@c/def"), "a_b_c_def");
    let path = sanitize_path_part(
        "/a\
/path",
    );
    assert!(path.chars().all(|ch| ch != '/'));

    let sample = json!({"access_token":"abc","nested":{"chatgpt-account-id":"x","value":1}});
    let redacted = redact_traffic(&sample);
    assert!(
        redacted["access_token"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    assert_eq!(
        redact_traffic(&json!({"user_id":"person","note":"Bearer secret"}))["user_id"],
        "[redacted len=6]"
    );
    let secrets = json!({
        "access_token":"access-secret",
        "nested":[{"refresh_token":"refresh-secret","oauth_token":"oauth-secret"}],
        "identity":{"email":"person@example.test"},
        "user":{"message":"Bearer text is ordinary message content"}
    });
    let redacted = redact_traffic(&secrets).to_string();
    for secret in [
        "access-secret",
        "refresh-secret",
        "oauth-secret",
        "person@example.test",
    ] {
        assert!(!redacted.contains(secret));
    }
    assert!(redacted.contains("Bearer text is ordinary message content"));

    unsafe {
        env::set_var("CCP_TRAFFIC_LOG", "1");
    }
    let temp = TempDir::new().unwrap();
    let capture = create_traffic_capture(TrafficCaptureOptions {
        req_id: "req-1".into(),
        session_id: None,
        session_seq: Some(12),
        provider: Some("codex".into()),
        state_dir_override: Some(temp.path().to_path_buf()),
    })
    .expect("capture");

    capture.write_text("020-note", "hello");
    capture.write_json("030-req", &json!({"token":"abc"}));
    let event = json!({"type":"response.completed","refresh_token":"secret"});
    let mut stream_capture = capture.stream_capture();
    for _ in 0..(MAX_SSE_CAPTURE_BYTES / 100 + 2) {
        stream_capture.upstream_event(Some("response.completed"), &event);
    }
    stream_capture.finish(&capture, json!({"kind":"test"}));
    let transcript = std::fs::read_to_string(
        std::fs::read_dir(capture.root())
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path();
                path.file_name()?
                    .to_string_lossy()
                    .ends_with("032-upstream-response-body.sse")
                    .then_some(path)
            })
            .unwrap(),
    )
    .unwrap();
    assert!(transcript.len() <= MAX_SSE_CAPTURE_BYTES);
    assert!(!transcript.contains("secret"));
    unsafe {
        env::remove_var("CCP_TRAFFIC_LOG");
    }
}

#[test]
fn retry_backoff_decisions() {
    let outcome = compute_backoff_delay(0, None);
    assert!(outcome.wait_ms >= RETRY_INITIAL_DELAY_MS / 2);
    assert!(outcome.wait_ms <= RETRY_MAX_DELAY_MS);

    let outcome_num = compute_backoff_delay(0, Some("5"));
    assert_eq!(outcome_num.wait_ms, 5000);

    let too_long = compute_backoff_delay(0, Some("120"));
    assert!(too_long.exceeds_budget);
}

#[tokio::test]
async fn auth_store_read_write_and_logout() {
    let store: InMemoryAuthStore<serde_json::Value> = InMemoryAuthStore::new();
    assert!(store.load().unwrap().is_none());
    store.save(json!({"token": "abc"})).unwrap();
    let loaded = store.load().unwrap().expect("value");
    assert_eq!(loaded["token"], json!("abc"));
    store.clear().unwrap();
    assert!(store.load().unwrap().is_none());

    let path = tempfile::tempdir().unwrap().path().join("auth.json");
    assert!(!path.exists());
}

#[test]
fn config_env_precedence_and_defaults() {
    let config = TempDir::new().unwrap();
    let mut env = HashMap::from([(
        "CCP_CONFIG_DIR".to_string(),
        config.path().to_string_lossy().into_owned(),
    )]);
    assert_eq!(load_config_for_env(&env).port, 18765);

    env.insert("PORT".to_string(), "19999".to_string());
    assert_eq!(load_config_for_env(&env).port, 19999);

    env.insert("CCP_ALIAS_PROVIDER".to_string(), "kimi".to_string());
    assert!(matches!(
        load_config_for_env(&env).alias_provider,
        AliasProvider::Kimi
    ));
}

#[test]
fn alias_provider_has_expected_default() {
    let config = TempDir::new().unwrap();
    let env = HashMap::from([(
        "CCP_CONFIG_DIR".to_string(),
        config.path().to_string_lossy().into_owned(),
    )]);
    assert!(matches!(
        load_config_for_env(&env).alias_provider,
        AliasProvider::Anthropic
    ));
}

#[test]
fn logger_factory_builds() {
    let logger = create_logger("server");
    let fields = Map::new();
    logger.debug("ready", Some(fields));
}
