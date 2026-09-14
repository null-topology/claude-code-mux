//! Token usage as responses report it, and prompt-cache misses derived from it.
//!
//! Usage is kept in the Anthropic shape on every route: `input_tokens` is the
//! part of the prompt the backend processed at full price, cache reads and
//! cache writes are separate, and the prompt size is their sum. The Codex
//! translator already maps the Responses API usage into that shape.
//!
//! A cache miss is judged per conversation lane (session, agent, provider,
//! model): the part of the previous request's prompt that the next one still
//! carries should come back as cached tokens, because both backends cache the
//! growing prefix. When enough of it does not, the shortfall is a miss. The
//! time since the previous request is compared with the cache lifetime to tell
//! a prefix that simply expired from one lost while it should still have been
//! alive (a changed prefix, or on Codex a request routed away from the machine
//! holding it). The monitor store decides which requests are compared at all:
//! side calls, overlapping requests and lanes that never cached are not.

use std::time::Duration;

use serde_json::Value;

/// Anthropic's default cache lifetime when a response does not say which one
/// it wrote. Claude Code requests the one-hour lifetime, and responses that
/// write cache report it (`usage.cache_creation.ephemeral_1h_input_tokens`).
pub const ANTHROPIC_DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// OpenAI documents GPT-5.6 and later prefixes as eligible for reuse for at
/// least 30 minutes after their last write or read, possibly longer, and
/// GPT-5.5 prefixes for about 30 minutes. Measured on the ChatGPT Codex backend
/// (2026-09-11): gpt-5.6-sol prefixes were served after 11, 21 and 35 minutes
/// and after 45 minutes when read again at 20, while re-sends after 2, 6, 29
/// and 61 minutes missed; gpt-5.5 hit at every point from 6 to 35 minutes. A
/// miss inside this window is therefore not proof of a changed prefix, and one
/// past it is not proof of expiry.
pub const CODEX_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// A shortfall below this many tokens is rounding and the uncached tail, not a
/// miss. OpenAI caches prefixes from 1024 tokens, Anthropic from 512 to 4096
/// depending on the model.
pub const CACHE_MISS_MIN_TOKENS: u64 = 1024;

/// Above the floor a shortfall counts once it reaches a tenth of the expected
/// prefix, and always once it reaches this many tokens, so a long prompt
/// cannot hide a large miss behind the relative threshold.
pub const CACHE_MISS_ALWAYS_TOKENS: u64 = 20_000;

/// One set of token counts. `input_tokens` excludes cached tokens.
///
/// The two `cache_write_*` lifetime counts are a breakdown of
/// `cache_write_tokens`, not tokens beside it: Anthropic reports how much of
/// the write it made with the five-minute lifetime and how much with the
/// one-hour one. Each is only ever the number the response sent, so a report
/// naming one bucket says nothing about the other and nothing about the total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageFields {
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Part of the cache write made with the five-minute lifetime.
    pub cache_write_5m_tokens: Option<u64>,
    /// Part of the cache write made with the one-hour lifetime.
    pub cache_write_1h_tokens: Option<u64>,
}

impl UsageFields {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_write_5m_tokens.is_none()
            && self.cache_write_1h_tokens.is_none()
    }

    /// Take every count an Anthropic `usage` object carries; absent or null
    /// fields keep the value already held.
    fn merge_anthropic_usage(&mut self, usage: &Value) {
        let read = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(tokens) = read("input_tokens") {
            self.input_tokens = Some(tokens);
        }
        if let Some(tokens) = read("cache_read_input_tokens") {
            self.cache_read_tokens = Some(tokens);
        }
        if let Some(tokens) = read("cache_creation_input_tokens") {
            self.cache_write_tokens = Some(tokens);
        }
        if let Some(tokens) = read("output_tokens") {
            self.output_tokens = Some(tokens);
        }
        self.merge_cache_creation(usage.get("cache_creation"));
    }

    /// Take the lifetime breakdown of the cache write, when the response sent
    /// one. A bucket the object does not name keeps whatever was known about
    /// it: it is not zero, and it is not the rest of the total either.
    fn merge_cache_creation(&mut self, creation: Option<&Value>) {
        let Some(creation) = creation else {
            return;
        };
        let read = |key: &str| creation.get(key).and_then(Value::as_u64);
        if let Some(tokens) = read("ephemeral_5m_input_tokens") {
            self.cache_write_5m_tokens = Some(tokens);
        }
        if let Some(tokens) = read("ephemeral_1h_input_tokens") {
            self.cache_write_1h_tokens = Some(tokens);
        }
    }
}

/// Usage observed on a response.
///
/// `opening` values come from a stream's first event and may be estimates:
/// the Codex translator reports the whole prompt there before the backend has
/// counted it. They only ever raise a count. `closing` values come from the
/// end of a stream or a complete body and replace whatever was there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageReport {
    pub opening: UsageFields,
    pub closing: UsageFields,
    /// Lifetime of the cache entries this response wrote, when it says.
    pub cache_ttl: Option<Duration>,
    /// The whole prompt as the backend counted it, when it reported a total of
    /// its own. It is the same tokens as `input_tokens` plus the cache counts,
    /// never extra ones, and it is kept separately because a backend can report
    /// the total without saying how much of it was served from cache. Only a
    /// number the backend sent belongs here; nothing is derived from an unknown
    /// split.
    pub reported_prompt_tokens: Option<u64>,
}

impl UsageReport {
    pub fn opening(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Self {
        Self {
            opening: UsageFields {
                input_tokens,
                output_tokens,
                ..UsageFields::default()
            },
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.opening.is_empty()
            && self.closing.is_empty()
            && self.cache_ttl.is_none()
            && self.reported_prompt_tokens.is_none()
    }

    /// Add one Anthropic-shaped event: `message_start`, `message_delta`, or a
    /// whole message body. `start_is_exact` says whether a `message_start`
    /// carries the backend's own prompt counts (the Anthropic passthrough)
    /// rather than an estimate (translated routes). Output tokens in a
    /// `message_start` are partial either way.
    pub fn add_event(&mut self, event: &Value, start_is_exact: bool) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let Some(usage) = event.pointer("/message/usage") else {
                    return;
                };
                self.note_cache_ttl(usage);
                if start_is_exact {
                    let mut prompt = UsageFields::default();
                    prompt.merge_anthropic_usage(usage);
                    self.closing.input_tokens = prompt.input_tokens.or(self.closing.input_tokens);
                    self.closing.cache_read_tokens =
                        prompt.cache_read_tokens.or(self.closing.cache_read_tokens);
                    self.closing.cache_write_tokens = prompt
                        .cache_write_tokens
                        .or(self.closing.cache_write_tokens);
                    self.closing.cache_write_5m_tokens = prompt
                        .cache_write_5m_tokens
                        .or(self.closing.cache_write_5m_tokens);
                    self.closing.cache_write_1h_tokens = prompt
                        .cache_write_1h_tokens
                        .or(self.closing.cache_write_1h_tokens);
                    if let Some(tokens) = prompt.output_tokens {
                        self.opening.output_tokens = Some(tokens);
                    }
                } else {
                    self.opening.merge_anthropic_usage(usage);
                }
            }
            Some("message_delta") => {
                if let Some(usage) = event.get("usage") {
                    self.note_cache_ttl(usage);
                    self.closing.merge_anthropic_usage(usage);
                }
            }
            _ => {
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                    self.note_cache_ttl(usage);
                    self.closing.merge_anthropic_usage(usage);
                }
            }
        }
    }

    fn note_cache_ttl(&mut self, usage: &Value) {
        if let Some(ttl) = cache_ttl_from_usage(usage) {
            self.cache_ttl = Some(ttl);
        }
    }
}

/// Usage from Anthropic-shaped SSE bytes produced by a translating provider,
/// whose `message_start` counts are estimates.
pub fn usage_report_from_anthropic_sse(bytes: &[u8]) -> UsageReport {
    let text = String::from_utf8_lossy(bytes);
    let mut report = UsageReport::default();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        report.add_event(&value, false);
    }
    report
}

/// Usage from a complete Anthropic JSON body: a message with `usage`, or a
/// `count_tokens` reply with a bare `input_tokens`.
pub fn usage_report_from_anthropic_body(body: &Value) -> UsageReport {
    let mut report = UsageReport::default();
    if body.get("usage").is_some_and(Value::is_object) {
        report.add_event(body, true);
    } else if let Some(tokens) = body.get("input_tokens").and_then(Value::as_u64) {
        report.closing.input_tokens = Some(tokens);
    }
    report
}

fn cache_ttl_from_usage(usage: &Value) -> Option<Duration> {
    let creation = usage.get("cache_creation")?;
    let written = |key: &str| {
        creation
            .get(key)
            .and_then(Value::as_u64)
            .is_some_and(|tokens| tokens > 0)
    };
    if written("ephemeral_1h_input_tokens") {
        Some(Duration::from_secs(60 * 60))
    } else if written("ephemeral_5m_input_tokens") {
        Some(Duration::from_secs(5 * 60))
    } else {
        None
    }
}

/// The cache lifetime to assume for a provider when no response has said.
pub fn default_cache_ttl(provider: &str) -> Option<Duration> {
    match provider {
        "anthropic" => Some(ANTHROPIC_DEFAULT_CACHE_TTL),
        "codex" => Some(CODEX_CACHE_TTL),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMissCause {
    /// The lane was idle longer than the cache lifetime.
    Expired,
    /// The previous request was recent enough for its prefix to be alive: the
    /// prefix changed, or the backend served the request without it.
    WithinTtl,
    /// The provider's cache lifetime is not known.
    UnknownTtl,
}

impl CacheMissCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::WithinTtl => "within ttl",
            Self::UnknownTtl => "ttl unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMiss {
    /// Tokens of the expected prefix that did not come back from the cache.
    pub missed_tokens: u64,
    /// The prefix this request shares with the previous request of its lane:
    /// the smaller of the two prompts.
    pub expected_tokens: u64,
    /// Start-to-start time since that request.
    pub gap: Duration,
    pub ttl: Option<Duration>,
    pub cause: CacheMissCause,
}

/// Whether the provider caches prompt prefixes without being asked and reports
/// only reads. The ChatGPT Codex backend caches prefixes from 1024 tokens and
/// always reports `cache_write_tokens` as 0.
pub fn caches_implicitly(provider: &str) -> bool {
    provider == "codex"
}

/// Judge one request against the previous request of its lane.
///
/// Only the part of the previous prompt that this request still carries can
/// come back from the cache, so the expected prefix is the smaller of the two
/// prompts. A prompt that shrank past the noise floor was rewritten by the
/// client (compaction, a cleared or rewound conversation): its uncached part is
/// new content, so it starts a new baseline instead of counting as a miss.
pub fn detect_cache_miss(
    previous_prompt_tokens: u64,
    prompt_tokens: u64,
    cache_read_tokens: u64,
    gap: Duration,
    ttl: Option<Duration>,
) -> Option<CacheMiss> {
    if prompt_tokens.saturating_add(CACHE_MISS_MIN_TOKENS) < previous_prompt_tokens {
        return None;
    }
    let expected_tokens = previous_prompt_tokens.min(prompt_tokens);
    let missed_tokens = expected_tokens.saturating_sub(cache_read_tokens);
    let threshold = (expected_tokens / 10).clamp(CACHE_MISS_MIN_TOKENS, CACHE_MISS_ALWAYS_TOKENS);
    if missed_tokens < threshold {
        return None;
    }
    let cause = match ttl {
        Some(ttl) if gap > ttl => CacheMissCause::Expired,
        Some(_) => CacheMissCause::WithinTtl,
        None => CacheMissCause::UnknownTtl,
    };
    Some(CacheMiss {
        missed_tokens,
        expected_tokens,
        gap,
        ttl,
        cause,
    })
}

/// Which counts have received a closing value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ClosedFields {
    pub input: bool,
    pub cache_read: bool,
    pub cache_write: bool,
    pub output: bool,
    pub cache_write_5m: bool,
    pub cache_write_1h: bool,
}

/// How far one count has been pinned down.
///
/// `Missing` is not a zero: the Codex backend never reports cache writes, and a
/// stream that ended before its final event reported no output. `Opening` is an
/// estimate a closing value may still replace, upwards or downwards. `Exact` is
/// the backend's own final count, a final zero included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UsageQuality {
    #[default]
    Missing,
    Opening,
    Exact,
}

impl UsageQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Opening => "opening",
            Self::Exact => "exact",
        }
    }
}

/// Quality of each count of one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualityFields {
    pub input: UsageQuality,
    pub cache_read: UsageQuality,
    pub cache_write: UsageQuality,
    pub output: UsageQuality,
}

/// Quality of the two lifetime buckets one request's cache write is made of.
///
/// Kept apart from [`QualityFields`], which is the quality of the four counts a
/// request costs. A bucket is known when the response named it, and no bucket
/// is ever inferred from the write it belongs to or from the other bucket, so
/// each carries its own evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheWriteQuality {
    pub ephemeral_5m: UsageQuality,
    pub ephemeral_1h: UsageQuality,
}

/// Quality of one count, from the value held and whether a closing observation
/// produced it.
pub(crate) fn quality_of(value: Option<u64>, closed: bool) -> UsageQuality {
    match value {
        Some(_) if closed => UsageQuality::Exact,
        Some(_) => UsageQuality::Opening,
        None => UsageQuality::Missing,
    }
}

/// Signed change of each count caused by one report.
///
/// A report that changes no count at all still changes what is known about one:
/// a missing count that arrives as a reported zero moves its evidence without
/// moving any total, so nothing is skipped on a zero delta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UsageDelta {
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
}

/// An opening observation only raises a count, and only until it is closed.
pub(crate) fn apply_opening(current: &mut Option<u64>, closed: bool, incoming: Option<u64>) -> i64 {
    let Some(incoming) = incoming else {
        return 0;
    };
    if closed {
        return 0;
    }
    let previous = current.unwrap_or(0);
    if current.is_none() || incoming > previous {
        *current = Some(incoming);
        return signed_difference(incoming, previous);
    }
    0
}

/// A closing observation replaces the count.
pub(crate) fn apply_closing(
    current: &mut Option<u64>,
    closed: &mut bool,
    incoming: Option<u64>,
) -> i64 {
    let Some(incoming) = incoming else {
        return 0;
    };
    let previous = current.unwrap_or(0);
    *current = Some(incoming);
    *closed = true;
    signed_difference(incoming, previous)
}

pub(crate) fn add_signed(total: u64, delta: i64) -> u64 {
    if delta >= 0 {
        total.saturating_add(delta.unsigned_abs())
    } else {
        total.saturating_sub(delta.unsigned_abs())
    }
}

fn signed_difference(new: u64, old: u64) -> i64 {
    if new >= old {
        i64::try_from(new - old).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(old - new).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translated_stream_start_is_an_estimate_and_delta_is_final() {
        let sse = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":31066,"output_tokens":0}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":2906,"cache_read_input_tokens":28160,"cache_creation_input_tokens":0,"output_tokens":117}}

"#;
        let report = usage_report_from_anthropic_sse(sse);
        assert_eq!(report.opening.input_tokens, Some(31066));
        assert_eq!(report.opening.output_tokens, Some(0));
        assert_eq!(
            report.closing,
            UsageFields {
                input_tokens: Some(2906),
                cache_read_tokens: Some(28160),
                cache_write_tokens: Some(0),
                output_tokens: Some(117),
                ..UsageFields::default()
            }
        );
    }

    #[test]
    fn anthropic_stream_start_counts_are_exact_and_report_the_ttl() {
        let mut report = UsageReport::default();
        report.add_event(
            &json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 2, "cache_read_input_tokens": 10126,
                "cache_creation_input_tokens": 22405,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 22405},
                "output_tokens": 3
            }}}),
            true,
        );
        assert_eq!(report.closing.input_tokens, Some(2));
        assert_eq!(report.closing.cache_read_tokens, Some(10126));
        assert_eq!(report.closing.cache_write_tokens, Some(22405));
        assert_eq!(report.closing.output_tokens, None);
        assert_eq!(report.opening.output_tokens, Some(3));
        assert_eq!(report.cache_ttl, Some(Duration::from_secs(3600)));

        // A delta carrying only output keeps the prompt counts.
        report.add_event(
            &json!({"type": "message_delta", "usage": {"output_tokens": 120}}),
            true,
        );
        assert_eq!(report.closing.cache_read_tokens, Some(10126));
        assert_eq!(report.closing.output_tokens, Some(120));
    }

    #[test]
    fn body_usage_and_count_tokens_reply() {
        let message = usage_report_from_anthropic_body(&json!({
            "type": "message",
            "usage": {"input_tokens": 5, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 0, "output_tokens": 7}
        }));
        assert_eq!(message.closing.cache_read_tokens, Some(900));
        assert_eq!(message.closing.output_tokens, Some(7));

        let count = usage_report_from_anthropic_body(&json!({"input_tokens": 4242}));
        assert_eq!(count.closing.input_tokens, Some(4242));
        assert!(count.opening.is_empty());
    }

    #[test]
    fn miss_detection_thresholds_and_causes() {
        let ttl = Some(Duration::from_secs(1800));
        let soon = Duration::from_secs(20);
        // A full hit, and a shortfall that is only the uncached tail.
        assert_eq!(detect_cache_miss(30_000, 31_000, 30_000, soon, ttl), None);
        assert_eq!(detect_cache_miss(30_000, 31_000, 29_000, soon, ttl), None);
        // Too small a prefix to judge.
        assert_eq!(detect_cache_miss(900, 1_200, 0, soon, ttl), None);

        let within = detect_cache_miss(30_917, 31_500, 0, soon, ttl).unwrap();
        assert_eq!(within.expected_tokens, 30_917);
        assert_eq!(within.missed_tokens, 30_917);
        assert_eq!(within.cause, CacheMissCause::WithinTtl);

        let expired = detect_cache_miss(30_917, 31_500, 0, Duration::from_secs(3600), ttl).unwrap();
        assert_eq!(expired.cause, CacheMissCause::Expired);

        let unknown =
            detect_cache_miss(30_917, 31_500, 1_000, Duration::from_secs(5), None).unwrap();
        assert_eq!(unknown.cause, CacheMissCause::UnknownTtl);
        assert_eq!(unknown.missed_tokens, 29_917);
    }

    #[test]
    fn a_large_miss_counts_even_below_a_tenth_of_a_long_prompt() {
        let ttl = Some(Duration::from_secs(1800));
        let soon = Duration::from_secs(20);
        // 39k of 400k is under a tenth but far past the absolute threshold.
        let miss = detect_cache_miss(400_000, 402_000, 361_000, soon, ttl).unwrap();
        assert_eq!(miss.missed_tokens, 39_000);
        // 15k of 400k is below both.
        assert_eq!(
            detect_cache_miss(400_000, 402_000, 385_000, soon, ttl),
            None
        );
    }

    #[test]
    fn a_rewritten_prompt_starts_over_instead_of_missing() {
        let ttl = Some(Duration::from_secs(1800));
        let soon = Duration::from_secs(20);
        // Compaction: 300k shrinks to a 40k summary prompt whose tools and
        // system prompt still come from the cache.
        assert_eq!(detect_cache_miss(300_000, 40_000, 25_000, soon, ttl), None);
        // A shrink inside the noise floor is judged against the smaller prompt.
        let miss = detect_cache_miss(50_000, 49_500, 0, soon, ttl).unwrap();
        assert_eq!(miss.expected_tokens, 49_500);
        assert_eq!(miss.missed_tokens, 49_500);
    }

    #[test]
    fn opening_raises_until_closed_and_closing_replaces() {
        let mut value = None;
        let mut closed = false;
        assert_eq!(apply_opening(&mut value, closed, Some(31_066)), 31_066);
        assert_eq!(apply_opening(&mut value, closed, Some(100)), 0);
        assert_eq!(
            apply_closing(&mut value, &mut closed, Some(2_906)),
            2_906 - 31_066
        );
        assert_eq!(value, Some(2_906));
        assert!(closed);
        assert_eq!(apply_opening(&mut value, closed, Some(40_000)), 0);
        assert_eq!(value, Some(2_906));
        assert_eq!(add_signed(31_066, 2_906 - 31_066), 2_906);
        assert_eq!(add_signed(10, -20), 0);
    }

    #[test]
    fn a_count_nothing_reported_is_not_a_reported_zero() {
        assert_eq!(quality_of(None, false), UsageQuality::Missing);
        assert_eq!(quality_of(Some(0), false), UsageQuality::Opening);
        assert_eq!(quality_of(Some(0), true), UsageQuality::Exact);
        assert_eq!(quality_of(Some(341_974), false), UsageQuality::Opening);
        assert_eq!(quality_of(Some(341_974), true), UsageQuality::Exact);

        // A zero a stream opened with is held as a value and stays an estimate
        // until a closing zero confirms it.
        let mut output = None;
        let mut closed = false;
        assert_eq!(apply_opening(&mut output, closed, Some(0)), 0);
        assert_eq!(output, Some(0));
        assert_eq!(quality_of(output, closed), UsageQuality::Opening);
        assert_eq!(apply_closing(&mut output, &mut closed, Some(0)), 0);
        assert_eq!(output, Some(0));
        assert_eq!(quality_of(output, closed), UsageQuality::Exact);

        // A count no report carries keeps no value and is not forced to zero.
        let mut write = None;
        let mut write_closed = false;
        assert_eq!(apply_opening(&mut write, write_closed, None), 0);
        assert_eq!(apply_closing(&mut write, &mut write_closed, None), 0);
        assert_eq!(write, None);
        assert_eq!(quality_of(write, write_closed), UsageQuality::Missing);
    }

    #[test]
    fn the_cache_creation_object_reports_the_two_lifetimes_it_wrote() {
        let mut both = UsageReport::default();
        both.add_event(
            &json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 2, "cache_read_input_tokens": 10_126,
                "cache_creation_input_tokens": 22_405,
                "cache_creation": {"ephemeral_5m_input_tokens": 405, "ephemeral_1h_input_tokens": 22_000}
            }}}),
            true,
        );
        assert_eq!(both.closing.cache_write_tokens, Some(22_405));
        assert_eq!(both.closing.cache_write_5m_tokens, Some(405));
        assert_eq!(both.closing.cache_write_1h_tokens, Some(22_000));
        // The lifetime it wrote is still read the way it always was.
        assert_eq!(both.cache_ttl, Some(Duration::from_secs(3600)));

        // One bucket named, the other absent: nothing is derived from the total
        // or from the bucket that is known.
        let mut partial = UsageReport::default();
        partial.add_event(
            &json!({"type": "message_delta", "usage": {
                "cache_creation_input_tokens": 900,
                "cache_creation": {"ephemeral_1h_input_tokens": 500}
            }}),
            true,
        );
        assert_eq!(partial.closing.cache_write_tokens, Some(900));
        assert_eq!(partial.closing.cache_write_5m_tokens, None);
        assert_eq!(partial.closing.cache_write_1h_tokens, Some(500));

        // A report with no `cache_creation` object at all names no bucket.
        let mut absent = UsageReport::default();
        absent.add_event(
            &json!({"type": "message_delta", "usage": {"cache_creation_input_tokens": 900}}),
            true,
        );
        assert_eq!(absent.closing.cache_write_tokens, Some(900));
        assert_eq!(absent.closing.cache_write_5m_tokens, None);
        assert_eq!(absent.closing.cache_write_1h_tokens, None);

        // A translated stream's estimate keeps the buckets on the opening side.
        let mut translated = UsageReport::default();
        translated.add_event(
            &json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 31_066,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
            }}}),
            false,
        );
        assert_eq!(translated.opening.cache_write_5m_tokens, Some(0));
        assert_eq!(translated.opening.cache_write_1h_tokens, Some(0));
        assert_eq!(translated.closing.cache_write_5m_tokens, None);
        // Two zeros write nothing, so they say nothing about the lifetime.
        assert_eq!(translated.cache_ttl, None);
    }

    #[test]
    fn a_report_carrying_only_a_lifetime_bucket_is_not_empty() {
        let mut report = UsageReport::default();
        report.add_event(
            &json!({"type": "message_delta", "usage": {
                "cache_creation": {"ephemeral_5m_input_tokens": 7}
            }}),
            true,
        );
        assert!(!report.is_empty());
        assert_eq!(report.closing.cache_write_5m_tokens, Some(7));
        assert_eq!(report.closing.cache_write_tokens, None);
    }
}
