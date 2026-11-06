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
    pub class: Option<CodexErrorClass>,
}

impl CodexEventFailure {
    pub fn retryable(&self) -> bool {
        !matches!(self.kind, CodexFailureKind::Permanent)
    }
}

/// What a failure's error code says about it, read the way the Codex CLI reads
/// it. A code settles the failure before any status or message heuristic, and
/// an unknown code leaves the failure to those heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorClass {
    /// Credits or a spend limit ran out. Waiting does not bring them back.
    QuotaExceeded,
    /// The account's plan does not include this usage.
    UsageNotIncluded,
    /// The backend refused the request itself, for its content or its policy.
    Rejected,
    /// The prompt does not fit the model's context window.
    ContextOverflow,
    /// Asked to slow down; the same request can succeed a little later.
    Throttled,
    /// The backend is overloaded; the same request can succeed later.
    Overloaded,
    /// The backend does not serve the model the request named.
    ModelNotFound,
}

impl CodexErrorClass {
    /// The status the client is answered with.
    pub fn status(self) -> u16 {
        match self {
            Self::QuotaExceeded | Self::Throttled => 429,
            Self::UsageNotIncluded => 403,
            Self::Rejected => 400,
            Self::ContextOverflow => 413,
            Self::Overloaded => 529,
            Self::ModelNotFound => 404,
        }
    }

    /// The Anthropic error type the client is answered with.
    pub fn error_type(self) -> &'static str {
        match self {
            Self::QuotaExceeded | Self::Throttled => "rate_limit_error",
            Self::UsageNotIncluded => "permission_error",
            Self::Rejected => "invalid_request_error",
            Self::ContextOverflow => "request_too_large",
            Self::Overloaded => "overloaded_error",
            Self::ModelNotFound => "not_found_error",
        }
    }

    fn kind(self) -> CodexFailureKind {
        match self {
            Self::Throttled => CodexFailureKind::RateLimit,
            Self::Overloaded => CodexFailureKind::Overloaded,
            Self::QuotaExceeded
            | Self::UsageNotIncluded
            | Self::Rejected
            | Self::ContextOverflow
            | Self::ModelNotFound => CodexFailureKind::Permanent,
        }
    }
}

const QUOTA_ERROR_CODES: [&str; 5] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// The class an error object's `code` (or, for quota, plan, capacity and
/// missing model errors, its `type`) names. A spent subscription window keeps its own handling, so an
/// error typed `usage_limit_reached` has no class whatever its code says.
pub(crate) fn error_class(error: &Value) -> Option<CodexErrorClass> {
    let code = error.get("code").and_then(Value::as_str);
    let error_type = error.get("type").and_then(Value::as_str);
    if error_type == Some("usage_limit_reached") {
        return None;
    }
    let names = |value: &str| code == Some(value) || error_type == Some(value);
    if code == Some("context_length_exceeded") {
        Some(CodexErrorClass::ContextOverflow)
    } else if QUOTA_ERROR_CODES.into_iter().any(names) {
        Some(CodexErrorClass::QuotaExceeded)
    } else if names("usage_not_included") {
        Some(CodexErrorClass::UsageNotIncluded)
    } else if matches!(
        code,
        Some("invalid_prompt" | "cyber_policy" | "bio_policy" | "misalignment_policy_violation")
    ) {
        Some(CodexErrorClass::Rejected)
    } else if code == Some("server_is_overloaded") || names("policy_capacity_unavailable") {
        // The CLI waits out both with the server's own retry delay
        // (codex-rs/protocol/src/error.rs at rust-v0.166.0).
        Some(CodexErrorClass::Overloaded)
    } else if matches!(code, Some("rate_limit_exceeded" | "slow_down")) {
        Some(CodexErrorClass::Throttled)
    } else if names("model_not_found") {
        // Only with a status the CLI accepts for it (`class_with_status`).
        Some(CodexErrorClass::ModelNotFound)
    } else {
        None
    }
}

/// `class` as it stands beside `status`. A missing model is one only with no
/// status or a 400, 403 or 404, as the Codex CLI reads it
/// (`codex-rs/codex-api/src/error.rs:83-94` at rust-v0.166.0); under any other
/// status the failure keeps the handling that status gives it.
fn class_with_status(
    class: Option<CodexErrorClass>,
    status: Option<u16>,
) -> Option<CodexErrorClass> {
    match (class, status) {
        (Some(CodexErrorClass::ModelNotFound), Some(status))
            if !matches!(status, 400 | 403 | 404) =>
        {
            None
        }
        _ => class,
    }
}

/// The class an error body names that came with HTTP `status`: a JSON body
/// `{"error": {...}}`, else the first event of an SSE body that names one,
/// read with that event's own status as a stream event is
/// (`classify_event_failure`).
pub(crate) fn error_class_from_body(body: &[u8], status: u16) -> Option<CodexErrorClass> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => class_with_status(error_class(value.get("error")?), Some(status)),
        Err(_) => sse_payloads(body).find_map(|payload| classify_event_failure(&payload)?.class),
    }
}

/// The retry advice an error body gives, read the way a stream event's is
/// (`retry_advice`): a JSON body's own fields, else the first event of an SSE
/// body that names one. Normalised (`normalize_retry_after`).
pub(crate) fn retry_after_from_body(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => {
            retry_advice(&value, event_error(&value)).map(|value| normalize_retry_after(&value))
        }
        Err(_) => {
            sse_payloads(body).find_map(|payload| classify_event_failure(&payload)?.retry_after)
        }
    }
}

fn sse_payloads(body: &[u8]) -> impl Iterator<Item = Value> {
    crate::anthropic::sse::parse_sse_events(body)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
}

/// The delay a throttling message asks for, as in "Please try again in
/// 11.054s": whole seconds, rounded up and never below one. Units are `s`,
/// `ms` and `second(s)`; a message that names none of them gives no delay.
pub(crate) fn retry_after_from_message(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let seconds = lower
        .match_indices("try again in")
        .find_map(|(start, phrase)| delay_seconds(lower[start + phrase.len()..].trim_start()))?;
    Some(whole_seconds(seconds))
}

/// A `Retry-After` value a client can wait on: a number of seconds becomes
/// whole seconds, rounded up and never below one, because a fraction or a zero
/// reads as "retry at once". Any other value, such as an HTTP date, is kept as
/// it came.
pub(crate) fn normalize_retry_after(value: &str) -> String {
    match value.trim().parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => whole_seconds(seconds),
        _ => value.to_string(),
    }
}

fn whole_seconds(seconds: f64) -> String {
    (seconds.ceil() as u64).max(1).to_string()
}

/// The delay a text opens with, as a number and a unit, in seconds.
fn delay_seconds(text: &str) -> Option<f64> {
    let digits = |text: &str| {
        text.find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(text.len())
    };
    let mut end = digits(text);
    if end == 0 {
        return None;
    }
    if let Some(fraction) = text[end..].strip_prefix('.')
        && digits(fraction) > 0
    {
        end += 1 + digits(fraction);
    }
    let (number, unit) = text.split_at(end);
    let value = number.parse::<f64>().ok()?;
    let unit = unit.trim_start();
    if unit.starts_with("ms") {
        Some(value / 1000.0)
    } else if unit.starts_with('s') {
        Some(value)
    } else {
        None
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
    // A `codex.rate_limits` event is no failure here: one that ends the
    // request is a spent window, which the usage limit readers answer once and
    // never as something to retry.
    let event_type = payload.get("type").and_then(Value::as_str)?;
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
    let class = class_with_status(error.and_then(error_class), explicit_status);
    let native = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str);
    let message = match native {
        // A missing model is named by its code when it comes with no message,
        // as the Codex CLI names it.
        _ if class == Some(CodexErrorClass::ModelNotFound)
            && native.is_none_or(|text| text.trim().is_empty()) =>
        {
            "model_not_found"
        }
        Some(text) => text,
        None => "Upstream error",
    }
    .to_string();
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str);
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let lower = message.to_ascii_lowercase();

    let kind = if let Some(class) = class {
        class.kind()
    } else if explicit_status == Some(429) || lower.contains("rate limit") {
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
    let status = class.map(CodexErrorClass::status).unwrap_or_else(|| {
        explicit_status.unwrap_or(match kind {
            CodexFailureKind::RateLimit => 429,
            CodexFailureKind::Overloaded => 529,
            CodexFailureKind::Transient => 503,
            CodexFailureKind::Permanent => 500,
        })
    });
    // Normalised once here, so a failure without a class passes whole seconds
    // as well; normalising a whole number again keeps it.
    let retry_after = retry_advice(payload, error).map(|value| normalize_retry_after(&value));

    Some(CodexEventFailure {
        kind,
        explicit_status,
        status,
        message,
        retry_after,
        class,
    })
}

/// The retry delay a failure payload names, as it came: `error` is the
/// payload's error object (`event_error`).
fn retry_advice(payload: &Value, error: Option<&Value>) -> Option<String> {
    error
        .and_then(|value| value.get("retry_after"))
        .and_then(scalar_string_value)
        .or_else(|| {
            error
                .and_then(|value| value.get("retry_after_seconds"))
                .and_then(scalar_string_value)
        })
        .or_else(|| scalar_string(payload.get("retry_after_seconds")))
        .or_else(|| scalar_string(payload.pointer("/headers/retry-after")))
        .or_else(|| scalar_string(payload.pointer("/headers/Retry-After")))
        // The headers of the failed response, which `response.failed` carries
        // inside its error (codex-rs/codex-api/src/sse/responses_error.rs at
        // rust-v0.162.0-alpha.17). Read after the fields above, so a value
        // they give is kept.
        .or_else(|| {
            error.and_then(|value| {
                scalar_string(value.pointer("/headers/retry-after"))
                    .or_else(|| scalar_string(value.pointer("/headers/Retry-After")))
            })
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
/// the event payload. A `codex.rate_limits` event that ends the request
/// (`is_terminal_rate_limit_event`) is a spent window too.
#[cfg(test)]
pub(crate) fn usage_limit_from_event(payload: &Value) -> Option<CodexUsageLimit> {
    usage_limit_from_event_with_headers(payload, &[])
}

/// Read a quota exhaustion event from a live stream. Codex may send the clock
/// only in the stream's HTTP response headers, so those are consulted for any
/// `X-Codex-*` value the event payload does not carry; a WebSocket stream has
/// none to give.
pub(crate) fn usage_limit_from_event_with_headers(
    payload: &Value,
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.failed" | "response.error" | "error" | "codex.rate_limits")
    ) {
        return None;
    }
    usage_limit_from_payload_with_headers(payload, headers)
}

/// Read quota exhaustion from either a JSON error response or an SSE event
/// body. HTTP response headers are included because Codex does not always
/// mirror its quota clocks into the error payload.
pub(crate) fn usage_limit_from_response(
    body: &[u8],
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let payloads: Vec<Value> = response_payloads(body).collect();
    // An explicit `usage_limit_reached` error names its own clock, so it is
    // read before a terminal snapshot whose windows may not tell which one ran
    // out, wherever each sits in the body.
    let limit_error = |payload: &&Value| {
        payload.get("type").and_then(Value::as_str) != Some("codex.rate_limits")
            && is_usage_limit_payload(payload)
    };
    let snapshot = |payload: &&Value| is_terminal_rate_limit_event(payload);
    let explicit = payloads.iter().filter(limit_error);
    let terminal = payloads.iter().filter(snapshot);
    explicit.chain(terminal).find_map(|payload| {
        // Only an actual quota event consults the headers; they are not copied
        // into every parsed event.
        usage_limit_from_payload_with_headers(payload, headers)
    })
}

/// Hand every `codex.rate_limits` event of a buffered body to the quota gauge
/// (`rate_limits::observe_event`), in body order.
pub(crate) fn observe_rate_limits_in_body(body: &[u8]) {
    for payload in response_payloads(body) {
        super::rate_limits::observe_event(&payload);
    }
}

/// A JSON body as one payload, or the events of an SSE body.
fn response_payloads(body: &[u8]) -> impl Iterator<Item = Value> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .into_iter()
        .chain(sse_payloads(body))
}

fn is_usage_limit_payload(payload: &Value) -> bool {
    is_terminal_rate_limit_event(payload)
        || event_error(payload)
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            == Some("usage_limit_reached")
}

fn usage_limit_from_payload_with_headers(
    payload: &Value,
    response_headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    if payload.get("type").and_then(Value::as_str) == Some("codex.rate_limits") {
        return is_terminal_rate_limit_event(payload).then(|| usage_limit_from_snapshot(payload));
    }
    let error = event_error(payload)?;
    if error.get("type").and_then(Value::as_str) != Some("usage_limit_reached") {
        return None;
    }

    let resets_at = numeric_value(error.get("resets_at"));
    let resets_in_seconds = numeric_value(error.get("resets_in_seconds"));
    let limiting_prefix =
        limiting_window_prefix(payload, response_headers, resets_in_seconds, resets_at);
    let resets_at = resets_at.or_else(|| {
        let prefix = limiting_prefix?;
        header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Reset-At"),
        )
    });
    let window = limiting_prefix.and_then(|prefix| {
        header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Window-Minutes"),
        )
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

/// A terminal `codex.rate_limits` event carries no error object, so its clock
/// and window come from the windows it reports (`rate_limits::limiting_window`).
/// When the window that ran out cannot be told, neither a clock nor a claim is
/// named, as for a limit error whose clock matches no window.
fn usage_limit_from_snapshot(payload: &Value) -> CodexUsageLimit {
    let limiting = payload
        .get("rate_limits")
        .and_then(super::rate_limits::limiting_window);
    CodexUsageLimit {
        message: "Usage limit reached".to_string(),
        resets_at: limiting.map(|(_, resets_at)| resets_at),
        window: limiting.and_then(|(minutes, _)| super::rate_limits::claimable_window(minutes)),
    }
}

/// Codex sends the clock for both windows on every limit error, so the one that
/// actually ran out is the one whose countdown or reset epoch matches the
/// error's own clock.
fn limiting_window_prefix(
    payload: &Value,
    response_headers: &[(String, String)],
    resets_in_seconds: Option<u64>,
    resets_at: Option<u64>,
) -> Option<&'static str> {
    let by_countdown = resets_in_seconds.and_then(|actual| {
        closest_window(
            actual,
            header_number(
                payload,
                response_headers,
                "X-Codex-Primary-Reset-After-Seconds",
            ),
            header_number(
                payload,
                response_headers,
                "X-Codex-Secondary-Reset-After-Seconds",
            ),
        )
    });
    let by_epoch = resets_at.and_then(|actual| {
        closest_window(
            actual,
            header_number(payload, response_headers, "X-Codex-Primary-Reset-At"),
            header_number(payload, response_headers, "X-Codex-Secondary-Reset-At"),
        )
    });
    by_countdown.or(by_epoch).or_else(|| {
        match (
            window_headers_present(payload, response_headers, "Primary"),
            window_headers_present(payload, response_headers, "Secondary"),
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

fn window_headers_present(
    payload: &Value,
    response_headers: &[(String, String)],
    prefix: &str,
) -> bool {
    ["Reset-After-Seconds", "Reset-At", "Window-Minutes"]
        .into_iter()
        .any(|suffix| {
            header_number(
                payload,
                response_headers,
                &format!("X-Codex-{prefix}-{suffix}"),
            )
            .is_some()
        })
}

/// A header mirrored into the event payload wins; the HTTP response headers
/// are the fallback for a value the payload does not carry.
fn header_number(
    payload: &Value,
    response_headers: &[(String, String)],
    name: &str,
) -> Option<u64> {
    payload
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| numeric_value(Some(value)))
        })
        .or_else(|| {
            response_headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.parse().ok())
        })
}

fn numeric_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

/// The first failure in a buffered body that ends the request as an error:
/// one worth retrying, or one whose error code names what went wrong. Other
/// failures are left to the body's own translation.
pub(crate) fn first_reportable_failure(body: &[u8]) -> Option<CodexEventFailure> {
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if let Some(failure) = classify_event_failure(&payload)
            && (failure.retryable() || failure.class.is_some())
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

    fn http_headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// A usage limit event with no mirrored `headers` object, as a live HTTP
    /// stream delivers it when the clock travels in the response headers.
    fn header_less_limit(error: Value) -> Value {
        serde_json::json!({"type": "error", "status_code": 429, "error": error})
    }

    #[test]
    fn parses_header_only_sse_quota_metadata_without_byte_matching() {
        let body = br#"data: {"type":"error","error":{"type":"usage_limit_reached","message":"weekly \"limit\" reached","resets_in_seconds":90}}

"#;
        let headers = http_headers(&[
            ("X-Codex-Secondary-Reset-After-Seconds", "90"),
            ("X-Codex-Secondary-Reset-At", "1789466238"),
            ("X-Codex-Secondary-Window-Minutes", "10080"),
        ]);

        let limit = usage_limit_from_response(body, &headers).expect("usage limit");
        assert_eq!(limit.message, "weekly \"limit\" reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
    }

    #[test]
    fn payload_header_wins_over_response_header() {
        let mut payload = spent_five_hour_window();
        payload["error"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
        let headers = http_headers(&[
            ("x-codex-primary-reset-at", "1"),
            ("x-codex-primary-window-minutes", "60"),
        ]);

        let limit = usage_limit_from_event_with_headers(&payload, &headers).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879438));
        assert_eq!(limit.window, Some(CodexLimitWindow::FiveHour));

        let body = format!("data: {payload}\n\n");
        let limit = usage_limit_from_response(body.as_bytes(), &headers).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1788879438));
        assert_eq!(limit.window, Some(CodexLimitWindow::FiveHour));
    }

    #[test]
    fn ambiguous_response_header_clocks_substitute_nothing() {
        let headers = http_headers(&[
            ("x-codex-primary-reset-after-seconds", "100"),
            ("x-codex-primary-reset-at", "100"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-reset-after-seconds", "200"),
            ("x-codex-secondary-reset-at", "200"),
            ("x-codex-secondary-window-minutes", "10080"),
        ]);

        // The body epoch sits between both header clocks: it is kept, and no
        // window is claimed.
        let payload = header_less_limit(serde_json::json!({
            "type": "usage_limit_reached",
            "message": "The usage limit has been reached",
            "resets_at": 150u64
        }));
        let limit = usage_limit_from_event_with_headers(&payload, &headers).expect("usage limit");
        assert_eq!(limit.resets_at, Some(150));
        assert_eq!(limit.window, None);

        // A countdown that matches neither window gets no header reset either.
        let payload = header_less_limit(serde_json::json!({
            "type": "usage_limit_reached",
            "message": "The usage limit has been reached",
            "resets_in_seconds": 150u64
        }));
        let limit = usage_limit_from_event_with_headers(&payload, &headers).expect("usage limit");
        assert_eq!(limit.resets_at, None);
        assert_eq!(limit.window, None);
    }

    #[test]
    fn response_header_reset_is_kept_for_unknown_window_length() {
        let payload = header_less_limit(serde_json::json!({
            "type": "usage_limit_reached",
            "message": "The usage limit has been reached",
            "resets_in_seconds": 90u64
        }));
        let headers = http_headers(&[
            ("x-codex-secondary-reset-after-seconds", "90"),
            ("x-codex-secondary-reset-at", "1789466238"),
            ("x-codex-secondary-window-minutes", "60"),
        ]);

        let limit = usage_limit_from_event_with_headers(&payload, &headers).expect("usage limit");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, None);
    }

    #[test]
    fn classifies_retryable_failure_kinds() {
        // A snapshot that ends the request is a spent window, answered by the
        // usage limit readers, never a failure to retry after its countdown.
        let spent = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true, "primary": {"reset_after_seconds": 1.5}}
        });
        assert!(is_terminal_rate_limit_event(&spent));
        assert!(classify_event_failure(&spent).is_none());

        let overload = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"type": "overloaded_error", "message": "busy"}}
        }))
        .unwrap();
        assert_eq!(overload.status, 529);
        assert!(overload.retryable());
    }

    #[test]
    fn retry_after_is_read_from_the_headers_of_the_error() {
        let failed = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {
                "code": "rate_limit_exceeded",
                "message": "Rate limit reached. Please try again in 20s.",
                "headers": {"retry-after": "3"}
            }}
        }))
        .unwrap();
        assert_eq!(failed.class, Some(CodexErrorClass::Throttled));
        assert_eq!(failed.retry_after.as_deref(), Some("3"));

        let error = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {
                "code": "server_is_overloaded",
                "message": "busy",
                "headers": {"Retry-After": 4}
            }
        }))
        .unwrap();
        assert_eq!(error.retry_after.as_deref(), Some("4"));

        // A field read before the error's headers keeps its value.
        let both = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {
                "code": "rate_limit_exceeded",
                "message": "busy",
                "retry_after_seconds": 9,
                "headers": {"retry-after": "3"}
            },
            "headers": {"retry-after": "5"}
        }))
        .unwrap();
        assert_eq!(both.retry_after.as_deref(), Some("9"));
        let envelope = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"code": "rate_limit_exceeded", "headers": {"retry-after": "3"}},
            "headers": {"retry-after": "5"}
        }))
        .unwrap();
        assert_eq!(envelope.retry_after.as_deref(), Some("5"));
    }

    #[test]
    fn a_failure_without_a_class_passes_whole_seconds() {
        let overloaded = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"type": "overloaded_error", "message": "busy", "retry_after": 0.1}}
        }))
        .unwrap();
        assert_eq!(overloaded.class, None);
        assert_eq!(overloaded.retry_after.as_deref(), Some("1"));

        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        let dated = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"message": "rate limit", "headers": {"retry-after": date}}
        }))
        .unwrap();
        assert_eq!(dated.class, None);
        assert_eq!(dated.retry_after.as_deref(), Some(date));
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

    #[test]
    fn classifies_known_error_codes_in_both_event_shapes() {
        let cases = [
            (
                "insufficient_quota",
                CodexErrorClass::QuotaExceeded,
                429,
                false,
            ),
            (
                "credit_balance_exhausted",
                CodexErrorClass::QuotaExceeded,
                429,
                false,
            ),
            (
                "organization_spend_limit_exceeded",
                CodexErrorClass::QuotaExceeded,
                429,
                false,
            ),
            (
                "project_spend_limit_exceeded",
                CodexErrorClass::QuotaExceeded,
                429,
                false,
            ),
            (
                "organization_usage_limit_exceeded",
                CodexErrorClass::QuotaExceeded,
                429,
                false,
            ),
            (
                "usage_not_included",
                CodexErrorClass::UsageNotIncluded,
                403,
                false,
            ),
            ("invalid_prompt", CodexErrorClass::Rejected, 400, false),
            ("cyber_policy", CodexErrorClass::Rejected, 400, false),
            ("bio_policy", CodexErrorClass::Rejected, 400, false),
            (
                "misalignment_policy_violation",
                CodexErrorClass::Rejected,
                400,
                false,
            ),
            (
                "context_length_exceeded",
                CodexErrorClass::ContextOverflow,
                413,
                false,
            ),
            ("slow_down", CodexErrorClass::Throttled, 429, true),
            ("rate_limit_exceeded", CodexErrorClass::Throttled, 429, true),
            (
                "server_is_overloaded",
                CodexErrorClass::Overloaded,
                529,
                true,
            ),
            (
                "policy_capacity_unavailable",
                CodexErrorClass::Overloaded,
                529,
                true,
            ),
            (
                "model_not_found",
                CodexErrorClass::ModelNotFound,
                404,
                false,
            ),
        ];
        for (code, class, status, retryable) in cases {
            let error = serde_json::json!({"code": code, "message": "toy message"});
            let payloads = [
                serde_json::json!({"type": "response.failed", "response": {"error": error}}),
                serde_json::json!({"type": "error", "error": error}),
            ];
            for payload in payloads {
                let failure = classify_event_failure(&payload).unwrap();
                assert_eq!(failure.class, Some(class), "{code}: {payload}");
                assert_eq!(failure.status, status, "{code}: {payload}");
                assert_eq!(failure.retryable(), retryable, "{code}: {payload}");
                assert_eq!(failure.message, "toy message");
            }
        }
    }

    #[test]
    fn quota_and_plan_errors_are_also_read_from_the_error_type() {
        for (error_type, class) in [
            ("insufficient_quota", CodexErrorClass::QuotaExceeded),
            ("usage_not_included", CodexErrorClass::UsageNotIncluded),
            ("policy_capacity_unavailable", CodexErrorClass::Overloaded),
            ("model_not_found", CodexErrorClass::ModelNotFound),
        ] {
            let failure = classify_event_failure(&serde_json::json!({
                "type": "error",
                "error": {"type": error_type, "message": "toy message"}
            }))
            .unwrap();
            assert_eq!(failure.class, Some(class), "{error_type}");
        }
        // Only quota, plan, capacity and missing model errors are read from
        // the type.
        let failure = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_prompt", "message": "toy message"}
        }))
        .unwrap();
        assert_eq!(failure.class, None);
    }

    #[test]
    fn a_known_code_decides_over_status_and_message() {
        let throttled = classify_event_failure(&serde_json::json!({
            "type": "error",
            "status": 503,
            "error": {"code": "slow_down", "message": "Please slow down."}
        }))
        .unwrap();
        assert_eq!(throttled.class, Some(CodexErrorClass::Throttled));
        assert_eq!(throttled.kind, CodexFailureKind::RateLimit);
        assert_eq!(throttled.explicit_status, Some(503));
        assert_eq!(throttled.status, 429);

        let rejected = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {
                "code": "invalid_prompt",
                "message": "rate limit words in a refused prompt"
            }}
        }))
        .unwrap();
        assert_eq!(rejected.class, Some(CodexErrorClass::Rejected));
        assert_eq!(rejected.kind, CodexFailureKind::Permanent);
        assert_eq!(rejected.status, 400);
    }

    #[test]
    fn capacity_is_an_overload_with_the_delay_its_payload_names() {
        let cases = [
            serde_json::json!({
                "type": "error",
                "error": {"code": "policy_capacity_unavailable", "message": "toy message"}
            }),
            serde_json::json!({
                "type": "error",
                "status": 503,
                "error": {"code": "policy_capacity_unavailable", "message": "toy message"}
            }),
        ];
        for payload in cases {
            let failure = classify_event_failure(&payload).unwrap();
            assert_eq!(
                failure.class,
                Some(CodexErrorClass::Overloaded),
                "{payload}"
            );
            assert_eq!(failure.kind, CodexFailureKind::Overloaded, "{payload}");
            assert_eq!(failure.status, 529, "{payload}");
            assert_eq!(failure.message, "toy message", "{payload}");
            assert_eq!(failure.retry_after, None, "{payload}");
        }

        let delayed = classify_event_failure(&serde_json::json!({
            "type": "response.failed",
            "response": {"error": {
                "type": "policy_capacity_unavailable",
                "message": "toy message",
                "retry_after": 1.5
            }}
        }))
        .unwrap();
        assert_eq!(delayed.class, Some(CodexErrorClass::Overloaded));
        assert_eq!(delayed.retry_after.as_deref(), Some("2"));
    }

    #[test]
    fn a_missing_model_is_not_found_under_the_statuses_the_cli_accepts() {
        let statusless = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"code": "model_not_found", "message": "toy message"}
        }))
        .unwrap();
        assert_eq!(statusless.class, Some(CodexErrorClass::ModelNotFound));
        assert_eq!(statusless.explicit_status, None);
        assert_eq!(statusless.status, 404);
        assert_eq!(statusless.message, "toy message");
        assert!(!statusless.retryable());

        let bad_request = classify_event_failure(&serde_json::json!({
            "type": "error",
            "status": 400,
            "error": {"code": "model_not_found", "message": "toy message"}
        }))
        .unwrap();
        assert_eq!(bad_request.class, Some(CodexErrorClass::ModelNotFound));
        assert_eq!(bad_request.explicit_status, Some(400));
        assert_eq!(bad_request.status, 404);

        // Any other status keeps the handling that status gives.
        for (status, retryable) in [(500, true), (429, true), (401, false)] {
            let other = classify_event_failure(&serde_json::json!({
                "type": "error",
                "status": status,
                "error": {"code": "model_not_found", "message": "toy message"}
            }))
            .unwrap();
            assert_eq!(other.class, None, "{status}");
            assert_eq!(other.status, status, "{status}");
            assert_eq!(other.retryable(), retryable, "{status}");
        }
        for status in [400, 403, 404] {
            assert_eq!(
                error_class_from_body(br#"{"error":{"code":"model_not_found"}}"#, status),
                Some(CodexErrorClass::ModelNotFound),
                "{status}"
            );
        }
        for status in [401, 429, 500, 503] {
            assert_eq!(
                error_class_from_body(br#"{"error":{"code":"model_not_found"}}"#, status),
                None,
                "{status}"
            );
        }

        // With no message, or an empty one, the code names it.
        for error in [
            serde_json::json!({"code": "model_not_found"}),
            serde_json::json!({"code": "model_not_found", "message": " "}),
        ] {
            let failure =
                classify_event_failure(&serde_json::json!({"type": "error", "error": error}))
                    .unwrap();
            assert_eq!(failure.message, "model_not_found", "{error}");
        }
        assert_eq!(
            error_class_from_body(br#"{"error":{"type":"model_not_found"}}"#, 404),
            Some(CodexErrorClass::ModelNotFound)
        );
        // A spent window keeps precedence.
        let spent_window = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {"type": "usage_limit_reached", "code": "model_not_found"}
        }))
        .unwrap();
        assert_eq!(spent_window.class, None);
    }

    /// A `codex.rate_limits` event that ends the request, with the five hour
    /// window spent and the weekly one not.
    fn terminal_snapshot(credits: Value) -> Value {
        serde_json::json!({
            "type": "codex.rate_limits",
            "credits": credits,
            "rate_limits": {
                "allowed": false,
                "limit_reached": true,
                "primary": {
                    "used_percent": 100.0,
                    "window_minutes": 300,
                    "reset_after_seconds": 9569,
                    "reset_at": 1788879437u64
                },
                "secondary": {
                    "used_percent": 40.0,
                    "window_minutes": 10080,
                    "reset_after_seconds": 596369,
                    "reset_at": 1789466238u64
                }
            }
        })
    }

    #[test]
    fn a_terminal_rate_limit_snapshot_is_a_spent_window() {
        let snapshot =
            terminal_snapshot(serde_json::json!({"has_credits": false, "unlimited": false}));
        let expected = CodexUsageLimit {
            message: "Usage limit reached".to_string(),
            resets_at: Some(1788879437),
            window: Some(CodexLimitWindow::FiveHour),
        };
        assert_eq!(usage_limit_from_event(&snapshot), Some(expected.clone()));
        assert_eq!(
            usage_limit_from_event_with_headers(&snapshot, &[]),
            Some(expected.clone())
        );
        let body = format!("data: {snapshot}\n\n");
        assert_eq!(
            usage_limit_from_response(body.as_bytes(), &[]),
            Some(expected)
        );

        // Credits that still pay keep the stream going, so there is no limit.
        let credited =
            terminal_snapshot(serde_json::json!({"has_credits": true, "unlimited": false}));
        assert_eq!(usage_limit_from_event(&credited), None);
        let body = format!("data: {credited}\n\n");
        assert_eq!(usage_limit_from_response(body.as_bytes(), &[]), None);
        assert_eq!(
            usage_limit_from_event(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": false}
            })),
            None
        );
    }

    #[test]
    fn a_terminal_snapshot_names_a_window_only_when_one_can_be_told() {
        // Both windows spent: which one refused cannot be told.
        let mut both = terminal_snapshot(Value::Null);
        both["rate_limits"]["secondary"]["used_percent"] = serde_json::json!(100.0);
        let limit = usage_limit_from_event(&both).unwrap();
        assert_eq!((limit.resets_at, limit.window), (None, None));

        // A lone window that is not spent names nothing; a spent one does.
        let mut weekly_only = terminal_snapshot(Value::Null);
        weekly_only["rate_limits"]
            .as_object_mut()
            .unwrap()
            .remove("primary");
        let limit = usage_limit_from_event(&weekly_only).unwrap();
        assert_eq!((limit.resets_at, limit.window), (None, None));
        weekly_only["rate_limits"]["secondary"]["used_percent"] = serde_json::json!(100.0);
        let limit = usage_limit_from_event(&weekly_only).unwrap();
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));

        // A spent window beside one whose numbers are unusable names nothing.
        let mut broken = terminal_snapshot(Value::Null);
        broken["rate_limits"]["secondary"]["reset_at"] = serde_json::json!("soon");
        let limit = usage_limit_from_event(&broken).unwrap();
        assert_eq!((limit.resets_at, limit.window), (None, None));

        // No windows at all still ends the request, with nothing to name.
        let bare = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": true}
        });
        let limit = usage_limit_from_event(&bare).unwrap();
        assert_eq!(limit.message, "Usage limit reached");
        assert_eq!((limit.resets_at, limit.window), (None, None));
    }

    #[test]
    fn an_explicit_limit_error_is_read_before_a_terminal_snapshot() {
        // The snapshot comes first and cannot tell which window ran out; the
        // limit error after it can.
        let mut both = terminal_snapshot(Value::Null);
        both["rate_limits"]["secondary"]["used_percent"] = serde_json::json!(100.0);
        let failed = serde_json::json!({
            "type": "response.failed",
            "response": {"error": spent_five_hour_window()["error"].clone()},
            "headers": spent_five_hour_window()["headers"].clone()
        });
        for limit_event in [spent_five_hour_window(), failed] {
            let body = format!("data: {both}\n\ndata: {limit_event}\n\n");
            let limit = usage_limit_from_response(body.as_bytes(), &[]).expect("usage limit");
            assert_eq!(limit.resets_at, Some(1788879437), "{limit_event}");
            assert_eq!(
                limit.window,
                Some(CodexLimitWindow::FiveHour),
                "{limit_event}"
            );
        }

        // A snapshot alone still answers.
        let body = format!("data: {both}\n\n");
        let limit = usage_limit_from_response(body.as_bytes(), &[]).expect("usage limit");
        assert_eq!((limit.resets_at, limit.window), (None, None));
    }

    #[test]
    fn unknown_codes_and_spent_windows_keep_their_old_handling() {
        let unknown = classify_event_failure(&serde_json::json!({
            "type": "error",
            "status": 503,
            "error": {"code": "some_future_code", "message": "toy message"}
        }))
        .unwrap();
        assert_eq!(unknown.class, None);
        assert_eq!(unknown.kind, CodexFailureKind::Transient);
        assert_eq!(unknown.status, 503);

        let spent_window = classify_event_failure(&serde_json::json!({
            "type": "error",
            "error": {
                "type": "usage_limit_reached",
                "code": "insufficient_quota",
                "message": "The usage limit has been reached"
            }
        }))
        .unwrap();
        assert_eq!(spent_window.class, None);
    }

    #[test]
    fn reads_the_class_of_a_json_error_body() {
        assert_eq!(
            error_class_from_body(br#"{"error":{"code":"credit_balance_exhausted"}}"#, 429),
            Some(CodexErrorClass::QuotaExceeded)
        );
        assert_eq!(
            error_class_from_body(br#"{"error":{"type":"usage_not_included"}}"#, 403),
            Some(CodexErrorClass::UsageNotIncluded)
        );
        assert_eq!(
            error_class_from_body(br#"{"error":{"code":"server_is_overloaded"}}"#, 503),
            Some(CodexErrorClass::Overloaded)
        );
        assert_eq!(
            error_class_from_body(br#"{"error":{"code":"some_future_code"}}"#, 500),
            None
        );
        assert_eq!(error_class_from_body(br#"{"detail":"toy"}"#, 500), None);
        assert_eq!(error_class_from_body(b"not json", 500), None);
    }

    #[test]
    fn reads_the_class_and_delay_of_an_sse_error_body() {
        // A buffered WebSocket failure arrives as SSE under the status its
        // event named; the event's own status decides, as on the stream.
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_toy\"}}\n\n",
            "data: {\"type\":\"error\",\"status\":503,\"error\":{\"code\":\"policy_capacity_unavailable\",\"message\":\"toy\",\"retry_after\":2.5}}\n\n",
        );
        assert_eq!(
            error_class_from_body(body.as_bytes(), 503),
            Some(CodexErrorClass::Overloaded)
        );
        assert_eq!(retry_after_from_body(body.as_bytes()).as_deref(), Some("3"));

        // A statusless missing model is one, whatever status the buffered
        // response was given for it.
        let statusless = b"data: {\"type\":\"error\",\"error\":{\"code\":\"model_not_found\"}}\n\n";
        assert_eq!(
            error_class_from_body(statusless, 500),
            Some(CodexErrorClass::ModelNotFound)
        );
    }

    #[test]
    fn reads_the_delay_a_json_error_body_names() {
        for (body, expected) in [
            (r#"{"error":{"code":"toy","retry_after":2.5}}"#, Some("3")),
            (
                r#"{"error":{"code":"toy","retry_after_seconds":"4"}}"#,
                Some("4"),
            ),
            (
                r#"{"error":{"code":"toy"},"retry_after_seconds":0.2}"#,
                Some("1"),
            ),
            (
                r#"{"error":{"code":"toy"},"headers":{"retry-after":"7"}}"#,
                Some("7"),
            ),
            (
                r#"{"error":{"code":"toy","headers":{"Retry-After":"8"}}}"#,
                Some("8"),
            ),
            (r#"{"error":{"code":"toy"}}"#, None),
        ] {
            assert_eq!(
                retry_after_from_body(body.as_bytes()).as_deref(),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn buffered_scan_reports_classified_failures_that_are_not_retryable() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_toy\"}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":",
            "{\"code\":\"invalid_prompt\",\"message\":\"toy refusal\"}}}\n\n",
        );
        let failure = first_reportable_failure(body.as_bytes()).unwrap();
        assert_eq!(failure.class, Some(CodexErrorClass::Rejected));
        assert_eq!(failure.message, "toy refusal");

        let unknown = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":",
            "{\"code\":\"some_future_code\",\"message\":\"toy\"}}}\n\n",
        );
        assert!(first_reportable_failure(unknown.as_bytes()).is_none());
    }

    #[test]
    fn parses_the_delay_a_throttle_message_names() {
        let cases = [
            (
                "Rate limit reached for toy-model. Please try again in 11.054s. Visit the docs.",
                Some("12"),
            ),
            ("Rate limit reached. Please try again in 1s.", Some("1")),
            ("Please try again in 28ms.", Some("1")),
            ("Please try again in 2500ms.", Some("3")),
            ("Slow down. Try again in 2 seconds.", Some("2")),
            ("Please try again in 1 second.", Some("1")),
            ("TRY AGAIN IN 1.898S", Some("2")),
            ("Please try again later.", None),
            ("Please try again in 5 minutes.", None),
            ("Rate limit exceeded.", None),
        ];
        for (message, expected) in cases {
            assert_eq!(
                retry_after_from_message(message).as_deref(),
                expected,
                "{message}"
            );
        }
    }
}
