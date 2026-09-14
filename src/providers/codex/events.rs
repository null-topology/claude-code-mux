use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexFailureKind {
    RateLimit,
    Overloaded,
    Transient,
    Permanent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexEventFailure {
    pub kind: CodexFailureKind,
    pub explicit_status: Option<u16>,
    pub status: u16,
    pub message: String,
    pub retry_after: Option<String>,
}

impl CodexEventFailure {
    pub fn retryable(&self) -> bool {
        !matches!(self.kind, CodexFailureKind::Permanent)
    }
}

pub(crate) fn is_terminal_rate_limit_event(payload: &Value) -> bool {
    payload.get("type").and_then(Value::as_str) == Some("codex.rate_limits")
        && payload
            .pointer("/rate_limits/limit_reached")
            .and_then(Value::as_bool)
            == Some(true)
        && payload
            .pointer("/credits/has_credits")
            .and_then(Value::as_bool)
            != Some(true)
        && payload
            .pointer("/credits/unlimited")
            .and_then(Value::as_bool)
            != Some(true)
}

pub(crate) fn event_error(payload: &Value) -> Option<&Value> {
    payload
        .get("error")
        .or_else(|| payload.pointer("/response/error"))
}

pub(crate) fn classify_event_failure(payload: &Value) -> Option<CodexEventFailure> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    if event_type == "codex.rate_limits" {
        if !is_terminal_rate_limit_event(payload) {
            return None;
        }
        return Some(CodexEventFailure {
            kind: CodexFailureKind::RateLimit,
            explicit_status: Some(429),
            status: 429,
            message: "rate limit reached".to_string(),
            retry_after: scalar_string(payload.pointer("/rate_limits/primary/reset_after_seconds")),
        });
    }
    if !matches!(event_type, "response.failed" | "response.error" | "error") {
        return None;
    }

    let error = event_error(payload);
    let explicit_status = numeric_status(payload)
        .or_else(|| {
            error
                .and_then(|value| value.get("status"))
                .and_then(Value::as_u64)
        })
        .and_then(|status| u16::try_from(status).ok());
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Upstream error")
        .to_string();
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str);
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let lower = message.to_ascii_lowercase();

    let kind = if explicit_status == Some(429) || lower.contains("rate limit") {
        CodexFailureKind::RateLimit
    } else if explicit_status == Some(529)
        || code == Some("overloaded_error")
        || error_type == Some("overloaded_error")
        || lower.contains("overloaded")
    {
        CodexFailureKind::Overloaded
    } else if explicit_status.is_some_and(|status| matches!(status, 500 | 502 | 503 | 504))
        || matches!(
            code,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || matches!(
            error_type,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || retryable_message(&lower)
    {
        CodexFailureKind::Transient
    } else {
        CodexFailureKind::Permanent
    };
    let status = explicit_status.unwrap_or(match kind {
        CodexFailureKind::RateLimit => 429,
        CodexFailureKind::Overloaded => 529,
        CodexFailureKind::Transient => 503,
        CodexFailureKind::Permanent => 500,
    });
    let retry_after = error
        .and_then(|value| value.get("retry_after"))
        .and_then(scalar_string_value)
        .or_else(|| {
            error
                .and_then(|value| value.get("retry_after_seconds"))
                .and_then(scalar_string_value)
        })
        .or_else(|| scalar_string(payload.get("retry_after_seconds")))
        .or_else(|| scalar_string(payload.pointer("/headers/retry-after")))
        .or_else(|| scalar_string(payload.pointer("/headers/Retry-After")));

    Some(CodexEventFailure {
        kind,
        explicit_status,
        status,
        message,
        retry_after,
    })
}

/// Which known upstream quota window ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLimitWindow {
    FiveHour,
    SevenDay,
}

impl CodexLimitWindow {
    pub(crate) fn claim(self) -> &'static str {
        match self {
            CodexLimitWindow::FiveHour => "five_hour",
            CodexLimitWindow::SevenDay => "seven_day",
        }
    }
}

/// Quota exhaustion reported by Codex, together with the reset clock upstream
/// sends alongside it. Unlike a transient rate limit this does not clear on a
/// backoff, so the reset time is the only useful thing to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageLimit {
    pub message: String,
    pub resets_at: Option<u64>,
    pub window: Option<CodexLimitWindow>,
}

/// Recognise the `usage_limit_reached` error Codex emits when a subscription
/// window is spent. Upstream puts the clock both in the error body
/// (`resets_at`, `resets_in_seconds`) and in `X-Codex-*` headers mirrored into
/// the event payload.
pub(crate) fn usage_limit_from_event(payload: &Value) -> Option<CodexUsageLimit> {
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.failed" | "response.error" | "error")
    ) {
        return None;
    }
    usage_limit_from_payload(payload)
}

/// Read quota exhaustion from either a JSON error response or an SSE event
/// body. HTTP response headers are included because Codex does not always
/// mirror its quota clocks into the error payload.
pub(crate) fn usage_limit_from_response(
    body: &[u8],
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let direct = serde_json::from_slice::<Value>(body).ok().into_iter();
    let events = crate::anthropic::sse::parse_sse_events(body)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok());
    direct.chain(events).find_map(|mut payload| {
        attach_response_headers(&mut payload, headers);
        usage_limit_from_payload(&payload)
    })
}

fn usage_limit_from_payload(payload: &Value) -> Option<CodexUsageLimit> {
    let error = event_error(payload)?;
    if error.get("type").and_then(Value::as_str) != Some("usage_limit_reached") {
        return None;
    }

    let resets_at = numeric_value(error.get("resets_at"));
    let resets_in_seconds = numeric_value(error.get("resets_in_seconds"));
    let limiting_prefix = limiting_window_prefix(payload, resets_in_seconds, resets_at);
    let resets_at = resets_at.or_else(|| {
        let prefix = limiting_prefix?;
        header_number(payload, &format!("X-Codex-{prefix}-Reset-At"))
    });
    let window = limiting_prefix.and_then(|prefix| {
        header_number(payload, &format!("X-Codex-{prefix}-Window-Minutes"))
            .and_then(super::rate_limits::claimable_window)
    });

    Some(CodexUsageLimit {
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Usage limit reached")
            .to_string(),
        resets_at,
        window,
    })
}

/// Codex sends the clock for both windows on every limit error, so the one that
/// actually ran out is the one whose countdown or reset epoch matches the
/// error's own clock.
fn limiting_window_prefix(
    payload: &Value,
    resets_in_seconds: Option<u64>,
    resets_at: Option<u64>,
) -> Option<&'static str> {
    let by_countdown = resets_in_seconds.and_then(|actual| {
        closest_window(
            actual,
            header_number(payload, "X-Codex-Primary-Reset-After-Seconds"),
            header_number(payload, "X-Codex-Secondary-Reset-After-Seconds"),
        )
    });
    let by_epoch = resets_at.and_then(|actual| {
        closest_window(
            actual,
            header_number(payload, "X-Codex-Primary-Reset-At"),
            header_number(payload, "X-Codex-Secondary-Reset-At"),
        )
    });
    by_countdown.or(by_epoch).or_else(|| {
        match (
            window_headers_present(payload, "Primary"),
            window_headers_present(payload, "Secondary"),
        ) {
            (true, false) => Some("Primary"),
            (false, true) => Some("Secondary"),
            _ => None,
        }
    })
}

fn closest_window(
    actual: u64,
    primary: Option<u64>,
    secondary: Option<u64>,
) -> Option<&'static str> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => {
            match (actual.abs_diff(primary), actual.abs_diff(secondary)) {
                (primary_distance, secondary_distance) if primary_distance < secondary_distance => {
                    Some("Primary")
                }
                (primary_distance, secondary_distance) if secondary_distance < primary_distance => {
                    Some("Secondary")
                }
                _ => None,
            }
        }
        (Some(_), None) => Some("Primary"),
        (None, Some(_)) => Some("Secondary"),
        (None, None) => None,
    }
}

fn window_headers_present(payload: &Value, prefix: &str) -> bool {
    ["Reset-After-Seconds", "Reset-At", "Window-Minutes"]
        .into_iter()
        .any(|suffix| header_number(payload, &format!("X-Codex-{prefix}-{suffix}")).is_some())
}

fn attach_response_headers(payload: &mut Value, headers: &[(String, String)]) {
    let Some(payload) = payload.as_object_mut() else {
        return;
    };
    let header_values = payload
        .entry("headers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(header_values) = header_values.as_object_mut() else {
        return;
    };
    for (name, value) in headers {
        if !header_values
            .keys()
            .any(|present| present.eq_ignore_ascii_case(name))
        {
            header_values.insert(name.clone(), Value::String(value.clone()));
        }
    }
}

fn header_number(payload: &Value, name: &str) -> Option<u64> {
    payload
        .get("headers")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| numeric_value(Some(value)))
}

fn numeric_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

pub(crate) fn first_retryable_failure(body: &[u8]) -> Option<CodexEventFailure> {
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if let Some(failure) = classify_event_failure(&payload)
            && failure.retryable()
        {
            return Some(failure);
        }
    }
    None
}

pub(crate) fn numeric_status(payload: &Value) -> Option<u64> {
    payload
        .get("status")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("status_code").and_then(Value::as_u64))
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value.and_then(scalar_string_value)
}

fn scalar_string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn retryable_message(message: &str) -> bool {
    [
        "server error",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "temporarily unavailable",
        "you can retry your request",
        "socket connection was closed unexpectedly",
        "connection closed unexpectedly",
        "operation timed out",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "econnreset",
        "epipe",
        "etimedout",
        "und_err_socket",
        "fetch failed",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape recorded from a live `gpt-5.6` turn that exhausted the five hour
    /// window: the clock arrives both in the error body and in the mirrored
    /// `X-Codex-*` headers, and the primary window is the one that ran out.
    fn spent_five_hour_window() -> Value {
        serde_json::json!({
            "type": "error",
            "status_code": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "plus",
                "resets_at": 1788879437u64,
                "resets_in_seconds": 9568u64
            },
            "headers": {
                "X-Codex-Primary-Used-Percent": "100",
                "X-Codex-Primary-Window-Minutes": "300",
                "X-Codex-Primary-Reset-After-Seconds": "9569",
                "X-Codex-Primary-Reset-At": "1788879438",
                "X-Codex-Secondary-Used-Percent": "16",
                "X-Codex-Secondary-Window-Minutes": "10080",
                "X-Codex-Secondary-Reset-After-Seconds": "596369",
                "X-Codex-Secondary-Reset-At": "1789466238"
            }
        })
    }

    #[test]
    fn reads_usage_limit_reset_clock() {
        let limit = usage_limit_from_event(&spent_five_hour_window()).expect("usage limit");
        assert_eq!(limit.message, "The usage limit has been reached");
        assert_eq!(limit.resets_at, Some(1788879437));
        assert_eq!(limit.window, Some(CodexLimitWindow::FiveHour));
        assert_eq!(limit.window.unwrap().claim(), "five_hour");
    }

    #[test]
    fn attributes_the_window_whose_clock_matches() {
        let mut payload = spent_five_hour_window();
        // Same error, but it is the weekly window that ran out.
        payload["error"]["resets_in_seconds"] = serde_json::json!(596_368u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn falls_back_to_header_clock_when_body_omits_it() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879438));
    }

    #[test]
    fn attributes_window_by_body_epoch_when_countdown_is_missing() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        payload["error"]["resets_at"] = serde_json::json!(1789466238u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn preserves_ambiguous_body_epoch_without_claim() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        payload["error"]["resets_at"] = serde_json::json!(150u64);
        payload["headers"]["X-Codex-Primary-Reset-At"] = serde_json::json!(100u64);
        payload["headers"]["X-Codex-Secondary-Reset-At"] = serde_json::json!(200u64);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(150));
        assert_eq!(limit.window, None);
    }

    #[test]
    fn omits_header_reset_and_claim_without_identifying_body_clock() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_in_seconds");
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, None);
        assert_eq!(limit.window, None);
    }

    #[test]
    fn ignores_errors_that_are_not_usage_limits() {
        assert!(
            usage_limit_from_event(&serde_json::json!({
                "type": "error",
                "status_code": 429,
                "error": {
                    "type": "rate_limit_exceeded",
                    "message": "slow down",
                    "resets_at": 1788879437u64,
                    "resets_in_seconds": 60
                }
            }))
            .is_none()
        );
        assert!(
            usage_limit_from_event(&serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }))
            .is_none()
        );
    }

    #[test]
    fn claims_the_five_hour_window_reported_as_299_minutes() {
        let mut payload = spent_five_hour_window();
        payload["headers"]["X-Codex-Primary-Window-Minutes"] = serde_json::json!(299);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.window, Some(CodexLimitWindow::FiveHour));
    }

    #[test]
    fn omits_claim_for_unknown_window_duration() {
        let mut payload = spent_five_hour_window();
        payload["headers"]["X-Codex-Primary-Window-Minutes"] = serde_json::json!(60);
        let limit = usage_limit_from_event(&payload).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879437));
        assert_eq!(limit.window, None);
    }

    #[test]
    fn reads_usage_limit_from_http_body_and_headers() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "weekly limit reached",
                "resets_in_seconds": 90
            }
        }))
        .unwrap();
        let headers = vec![
            (
                "x-codex-secondary-reset-after-seconds".to_string(),
                "90".to_string(),
            ),
            (
                "x-codex-secondary-reset-at".to_string(),
                "1789466238".to_string(),
            ),
            (
                "x-codex-secondary-window-minutes".to_string(),
                "10080".to_string(),
            ),
        ];
        let limit = usage_limit_from_response(&body, &headers).expect("usage limit");
        assert_eq!(limit.message, "weekly limit reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn classifies_retryable_failure_kinds() {
        let rate = classify_event_failure(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true, "primary": {"reset_after_seconds": 1.5}}
        }))
        .unwrap();
        assert_eq!(rate.kind, CodexFailureKind::RateLimit);
        assert_eq!(rate.retry_after.as_deref(), Some("1.5"));

        let overload = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"type": "overloaded_error", "message": "busy"}}
        }))
        .unwrap();
        assert_eq!(overload.status, 529);
        assert!(overload.retryable());
    }

    #[test]
    fn terminal_rate_limit_honors_credits() {
        // No credits field at all: legacy payload stays terminal.
        assert!(is_terminal_rate_limit_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true}
        })));

        // Credits exhausted: terminal.
        assert!(is_terminal_rate_limit_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true},
            "credits": {"has_credits": false, "unlimited": false}
        })));

        // Usable credits remain: informational.
        assert!(!is_terminal_rate_limit_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true},
            "credits": {"has_credits": true, "unlimited": false}
        })));

        // Unlimited plan: informational.
        assert!(!is_terminal_rate_limit_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true},
            "credits": {"has_credits": false, "unlimited": true}
        })));

        // Limit not reached: never terminal, credits irrelevant.
        assert!(!is_terminal_rate_limit_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": false},
            "credits": {"has_credits": false, "unlimited": false}
        })));

        // Wrong event type never matches.
        assert!(!is_terminal_rate_limit_event(&serde_json::json!({
            "type": "response.completed",
            "rate_limits": {"limit_reached": true}
        })));
    }

    #[test]
    fn classifier_skips_credited_rate_limit_snapshots() {
        assert!(
            classify_event_failure(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": true},
                "credits": {"has_credits": true, "unlimited": false}
            }))
            .is_none()
        );
    }

    #[test]
    fn ignores_informational_and_permanent_events() {
        assert!(
            classify_event_failure(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": false}
            }))
            .is_none()
        );
        let failure = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"status": 400, "message": "bad request"}
        }))
        .unwrap();
        assert!(!failure.retryable());
    }
}
