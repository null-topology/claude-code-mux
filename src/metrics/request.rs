//! Per-request metrics: the long-lived families the monitor store feeds at
//! settlement, and the gauge readings it hands to a render.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::{self, Counter};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};

use super::{
    DURATION_BUCKETS_SECONDS, FIRST_TOKEN_BUCKETS_SECONDS, MAX_LABEL_BYTES, MAX_MODEL_SERIES,
    PROMPT_TOKEN_BUCKETS,
};

const UNKNOWN: &str = "unknown";
const OTHER: &str = "other";

type HistogramFamily<L> = Family<L, Histogram, fn() -> Histogram>;

/// A counter value that stops at `u64::MAX` instead of wrapping, so a token
/// total never reads lower after an addition.
#[derive(Debug, Default)]
struct SaturatingU64(AtomicU64);

impl counter::Atomic<u64> for SaturatingU64 {
    fn inc(&self) -> u64 {
        self.inc_by(1)
    }

    fn inc_by(&self, v: u64) -> u64 {
        // The update never declines, so either arm holds the previous value.
        match self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(v))
            }) {
            Ok(previous) | Err(previous) => previous,
        }
    }

    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

type TokenCounter = Counter<u64, SaturatingU64>;

/// Families of the request metrics. A clone shares every counter and the
/// pair set with the original.
#[derive(Clone, Debug)]
pub struct RequestMetrics {
    pairs: Arc<Mutex<HashSet<PairLabels>>>,
    requests: Family<RequestLabels, Counter>,
    duration: HistogramFamily<EndpointLabels>,
    first_token: HistogramFamily<PairLabels>,
    prompt: HistogramFamily<PairLabels>,
    tokens: Family<TokenLabels, TokenCounter>,
    hit_basis: Family<HitBasisLabels, TokenCounter>,
    misses: Family<MissLabels, Counter>,
    rewrites: Family<PairLabels, Counter>,
}

/// The `provider` and `model` labels every request family starts with.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(super) struct PairLabels {
    provider: String,
    model: String,
}

impl PairLabels {
    fn other() -> Self {
        Self {
            provider: OTHER.to_owned(),
            model: OTHER.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    #[prometheus(flatten)]
    pair: PairLabels,
    endpoint: &'static str,
    outcome: &'static str,
    status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EndpointLabels {
    #[prometheus(flatten)]
    pair: PairLabels,
    endpoint: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TokenLabels {
    #[prometheus(flatten)]
    pair: PairLabels,
    kind: &'static str,
    quality: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct HitBasisLabels {
    #[prometheus(flatten)]
    pair: PairLabels,
    kind: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MissLabels {
    #[prometheus(flatten)]
    pair: PairLabels,
    cause: &'static str,
}

fn duration_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS_SECONDS)
}

fn first_token_histogram() -> Histogram {
    Histogram::new(FIRST_TOKEN_BUCKETS_SECONDS)
}

fn prompt_histogram() -> Histogram {
    Histogram::new(PROMPT_TOKEN_BUCKETS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Abandoned,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissCause {
    WithinTtl,
    Expired,
    UnknownTtl,
}

impl MissCause {
    fn label(self) -> &'static str {
        match self {
            Self::WithinTtl => "within_ttl",
            Self::Expired => "expired",
            Self::UnknownTtl => "unknown_ttl",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenCount {
    pub tokens: u64,
    pub exact: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SettledUsage {
    pub input: Option<TokenCount>,
    pub cache_read: Option<TokenCount>,
    pub cache_write: Option<TokenCount>,
    pub output: Option<TokenCount>,
    /// `prompt_tokens()` when `prompt_is_measured()`.
    pub measured_prompt: Option<u64>,
    /// `!without_history`.
    pub carries_history: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SettledRequest<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub endpoint: &'static str,
    pub outcome: Outcome,
    pub http_status: Option<u16>,
    pub duration: Duration,
    pub time_to_first_token: Option<Duration>,
    /// `None` when the request's tokens belong to no total.
    pub usage: Option<SettledUsage>,
}

impl RequestMetrics {
    pub fn new() -> Self {
        Self {
            pairs: Arc::default(),
            requests: Family::default(),
            duration: Family::new_with_constructor(duration_histogram),
            first_token: Family::new_with_constructor(first_token_histogram),
            prompt: Family::new_with_constructor(prompt_histogram),
            tokens: Family::default(),
            hit_basis: Family::default(),
            misses: Family::default(),
            rewrites: Family::default(),
        }
    }

    /// Exports a request once, when it settles.
    pub fn request_settled(&self, request: &SettledRequest<'_>) {
        let pair = self.pair_labels(request.provider, request.model);
        let status = request
            .http_status
            .map_or_else(|| "none".to_owned(), |status| status.to_string());
        self.requests
            .get_or_create(&RequestLabels {
                pair: pair.clone(),
                endpoint: request.endpoint,
                outcome: request.outcome.label(),
                status,
            })
            .inc();
        self.duration
            .get_or_create(&EndpointLabels {
                pair: pair.clone(),
                endpoint: request.endpoint,
            })
            .observe(request.duration.as_secs_f64());
        if let Some(first_token) = request.time_to_first_token {
            self.first_token
                .get_or_create(&pair)
                .observe(first_token.as_secs_f64());
        }

        let Some(usage) = request.usage else {
            return;
        };
        let counts = [
            ("input", usage.input),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
            ("output", usage.output),
        ];
        for (kind, count) in counts {
            let Some(count) = count else { continue };
            let quality = if count.exact { "exact" } else { "estimate" };
            self.tokens
                .get_or_create(&TokenLabels {
                    pair: pair.clone(),
                    kind,
                    quality,
                })
                .inc_by(count.tokens);
        }
        if let Some(prompt) = usage.measured_prompt {
            self.prompt.get_or_create(&pair).observe(prompt as f64);
        }
        if usage.carries_history {
            for &(kind, count) in &counts[..3] {
                let Some(count) = count else { continue };
                self.hit_basis
                    .get_or_create(&HitBasisLabels {
                        pair: pair.clone(),
                        kind,
                    })
                    .inc_by(count.tokens);
            }
        }
    }

    pub fn cache_miss(&self, provider: Option<&str>, model: Option<&str>, cause: MissCause) {
        let pair = self.pair_labels(provider, model);
        self.misses
            .get_or_create(&MissLabels {
                pair,
                cause: cause.label(),
            })
            .inc();
    }

    pub fn context_rewrite(&self, provider: Option<&str>, model: Option<&str>) {
        let pair = self.pair_labels(provider, model);
        self.rewrites.get_or_create(&pair).inc();
    }

    /// The labels a (provider, model) pair is exported under. A value over
    /// `MAX_LABEL_BYTES`, or a new pair once `MAX_MODEL_SERIES` pairs are
    /// admitted, gives (`other`, `other`); an admitted pair keeps its labels.
    pub(super) fn pair_labels(&self, provider: Option<&str>, model: Option<&str>) -> PairLabels {
        let provider = provider.unwrap_or(UNKNOWN);
        let model = model.unwrap_or(UNKNOWN);
        if provider.len() > MAX_LABEL_BYTES || model.len() > MAX_LABEL_BYTES {
            return PairLabels::other();
        }
        let pair = PairLabels {
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        let mut admitted = self.pairs.lock().unwrap_or_else(PoisonError::into_inner);
        if admitted.contains(&pair) {
            return pair;
        }
        if admitted.len() < MAX_MODEL_SERIES {
            admitted.insert(pair.clone());
            return pair;
        }
        PairLabels::other()
    }

    /// Registers clones of the long-lived families.
    pub(super) fn register(&self, registry: &mut Registry) {
        registry.register(
            "requests",
            "Settled requests by outcome and HTTP status",
            self.requests.clone(),
        );
        registry.register_with_unit(
            "request_duration",
            "Time from the start of a request to its settlement",
            Unit::Seconds,
            self.duration.clone(),
        );
        registry.register_with_unit(
            "time_to_first_token",
            "Time from the start of a request to its first generated output",
            Unit::Seconds,
            self.first_token.clone(),
        );
        registry.register(
            "request_prompt_tokens",
            "Prompt size of settled requests whose backend gave the whole prompt size",
            self.prompt.clone(),
        );
        registry.register(
            "tokens",
            "Tokens of settled requests by kind and quality",
            self.tokens.clone(),
        );
        registry.register(
            "cache_hit_basis_tokens",
            "Prompt tokens of settled requests that carried history, the basis of the cache hit ratio",
            self.hit_basis.clone(),
        );
        registry.register(
            "cache_misses",
            "Prompt cache misses by cause",
            self.misses.clone(),
        );
        registry.register(
            "context_rewrites",
            "Client rewrites of a conversation's context",
            self.rewrites.clone(),
        );
    }
}

impl Default for RequestMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GaugeReading {
    pub sessions_active: u64,
    pub sessions_idle: u64,
    pub sessions_started: u64,
    pub in_flight: Vec<InFlight>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InFlight {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub requests: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CodexWindow {
    pub used_ratio: f64,
    pub resets_at: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CodexWindows {
    pub five_hour: Option<CodexWindow>,
    pub seven_day: Option<CodexWindow>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::*;
    use crate::metrics::{Format, MAX_MODEL_SERIES, RenderInput, render};

    const BOTH_FORMATS: [Format; 2] = [Format::OpenMetrics, Format::Text];

    fn samples(metrics: &RequestMetrics, format: Format) -> BTreeSet<String> {
        let input = RenderInput {
            requests: Some(metrics),
            ..RenderInput::default()
        };
        render(&input, format)
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(str::to_owned)
            .collect()
    }

    fn with_prefix(samples: &BTreeSet<String>, prefix: &str) -> BTreeSet<String> {
        samples
            .iter()
            .filter(|line| line.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn settled(model: &str) -> SettledRequest<'_> {
        SettledRequest {
            provider: Some("codex"),
            model: Some(model),
            endpoint: "messages",
            outcome: Outcome::Completed,
            http_status: Some(200),
            duration: Duration::from_millis(1500),
            time_to_first_token: None,
            usage: None,
        }
    }

    fn exact(tokens: u64) -> Option<TokenCount> {
        Some(TokenCount {
            tokens,
            exact: true,
        })
    }

    fn estimate(tokens: u64) -> Option<TokenCount> {
        Some(TokenCount {
            tokens,
            exact: false,
        })
    }

    fn set(lines: &[&str]) -> BTreeSet<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    fn requests_line(provider: &str, model: &str, count: u64) -> String {
        format!(
            "ccmux_requests_total{{provider=\"{provider}\",model=\"{model}\",endpoint=\"messages\",outcome=\"completed\",status=\"200\"}} {count}"
        )
    }

    #[test]
    fn a_settled_request_renders_its_counter_and_histogram_lines() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&settled("gpt-6-sol"));
        metrics.request_settled(&SettledRequest {
            outcome: Outcome::Failed,
            ..settled("gpt-6-sol")
        });
        metrics.request_settled(&SettledRequest {
            outcome: Outcome::Abandoned,
            http_status: None,
            ..settled("gpt-6-sol")
        });

        let counter = set(&[
            "ccmux_requests_total{provider=\"codex\",model=\"gpt-6-sol\",endpoint=\"messages\",outcome=\"completed\",status=\"200\"} 1",
            "ccmux_requests_total{provider=\"codex\",model=\"gpt-6-sol\",endpoint=\"messages\",outcome=\"failed\",status=\"200\"} 1",
            "ccmux_requests_total{provider=\"codex\",model=\"gpt-6-sol\",endpoint=\"messages\",outcome=\"abandoned\",status=\"none\"} 1",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(with_prefix(&samples, "ccmux_requests_total"), counter);
            assert!(with_prefix(&samples, "ccmux_time_to_first_token_seconds").is_empty());
            assert!(with_prefix(&samples, "ccmux_tokens_total").is_empty());
        }

        let open_metrics = samples(&metrics, Format::OpenMetrics);
        let duration = with_prefix(&open_metrics, "ccmux_request_duration_seconds");
        let labels = "provider=\"codex\",model=\"gpt-6-sol\",endpoint=\"messages\"";
        for line in [
            format!("ccmux_request_duration_seconds_sum{{{labels}}} 4.5"),
            format!("ccmux_request_duration_seconds_count{{{labels}}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{le=\"0.1\",{labels}}} 0"),
            format!("ccmux_request_duration_seconds_bucket{{le=\"1.0\",{labels}}} 0"),
            format!("ccmux_request_duration_seconds_bucket{{le=\"2.5\",{labels}}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{le=\"1800.0\",{labels}}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{le=\"+Inf\",{labels}}} 3"),
        ] {
            assert!(duration.contains(&line), "missing {line} in {duration:?}");
        }
        assert_eq!(
            with_prefix(&duration, "ccmux_request_duration_seconds_bucket").len(),
            16
        );

        let text = samples(&metrics, Format::Text);
        let duration = with_prefix(&text, "ccmux_request_duration_seconds");
        for line in [
            format!("ccmux_request_duration_seconds_sum{{{labels}}} 4.5"),
            format!("ccmux_request_duration_seconds_count{{{labels}}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{{labels},le=\"0.1\"}} 0"),
            format!("ccmux_request_duration_seconds_bucket{{{labels},le=\"1\"}} 0"),
            format!("ccmux_request_duration_seconds_bucket{{{labels},le=\"2.5\"}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{{labels},le=\"1800\"}} 3"),
            format!("ccmux_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} 3"),
        ] {
            assert!(duration.contains(&line), "missing {line} in {duration:?}");
        }
        assert_eq!(
            with_prefix(&duration, "ccmux_request_duration_seconds_bucket").len(),
            16
        );
    }

    #[test]
    fn tokens_carry_kind_and_quality() {
        let metrics = RequestMetrics::new();
        let usage = SettledUsage {
            input: exact(100),
            cache_read: estimate(2_000),
            cache_write: exact(0),
            output: None,
            measured_prompt: None,
            carries_history: false,
        };
        metrics.request_settled(&SettledRequest {
            usage: Some(usage),
            ..settled("gpt-6-sol")
        });
        metrics.request_settled(&SettledRequest {
            usage: Some(SettledUsage {
                input: exact(50),
                output: estimate(7),
                ..SettledUsage::default()
            }),
            ..settled("gpt-6-sol")
        });

        let expected = set(&[
            "ccmux_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"input\",quality=\"exact\"} 150",
            "ccmux_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"cache_read\",quality=\"estimate\"} 2000",
            "ccmux_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"cache_write\",quality=\"exact\"} 0",
            "ccmux_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"output\",quality=\"estimate\"} 7",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(with_prefix(&samples, "ccmux_tokens_total"), expected);
        }
    }

    #[test]
    fn a_request_without_usage_exports_no_tokens() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&SettledRequest {
            time_to_first_token: Some(Duration::from_millis(300)),
            ..settled("gpt-6-sol")
        });

        for format in BOTH_FORMATS {
            let text = render(
                &RenderInput {
                    requests: Some(&metrics),
                    ..RenderInput::default()
                },
                format,
            );
            for family in [
                "ccmux_tokens",
                "ccmux_request_prompt_tokens",
                "ccmux_cache_hit_basis_tokens",
            ] {
                assert!(
                    !text.lines().any(|line| line.contains(family)),
                    "{family} rendered in {text}"
                );
            }
            assert_eq!(
                with_prefix(&samples(&metrics, format), "ccmux_requests_total"),
                set(&[&requests_line("codex", "gpt-6-sol", 1)])
            );
        }
    }

    #[test]
    fn first_token_is_observed_only_when_present() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&SettledRequest {
            time_to_first_token: Some(Duration::from_millis(300)),
            ..settled("gpt-6-sol")
        });
        metrics.request_settled(&settled("gpt-6-luna"));

        let labels = "provider=\"codex\",model=\"gpt-6-sol\"";
        let open_metrics = with_prefix(
            &samples(&metrics, Format::OpenMetrics),
            "ccmux_time_to_first_token_seconds",
        );
        for line in [
            format!("ccmux_time_to_first_token_seconds_sum{{{labels}}} 0.3"),
            format!("ccmux_time_to_first_token_seconds_count{{{labels}}} 1"),
            format!("ccmux_time_to_first_token_seconds_bucket{{le=\"0.25\",{labels}}} 0"),
            format!("ccmux_time_to_first_token_seconds_bucket{{le=\"0.5\",{labels}}} 1"),
        ] {
            assert!(
                open_metrics.contains(&line),
                "missing {line} in {open_metrics:?}"
            );
        }
        assert_eq!(open_metrics.len(), 14);
        assert!(!open_metrics.iter().any(|line| line.contains("gpt-6-luna")));

        let text = with_prefix(
            &samples(&metrics, Format::Text),
            "ccmux_time_to_first_token_seconds",
        );
        for line in [
            format!("ccmux_time_to_first_token_seconds_bucket{{{labels},le=\"0.25\"}} 0"),
            format!("ccmux_time_to_first_token_seconds_bucket{{{labels},le=\"0.5\"}} 1"),
            format!("ccmux_time_to_first_token_seconds_count{{{labels}}} 1"),
        ] {
            assert!(text.contains(&line), "missing {line} in {text:?}");
        }
        assert_eq!(text.len(), 14);
    }

    #[test]
    fn a_measured_prompt_is_observed_in_its_bucket() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&SettledRequest {
            usage: Some(SettledUsage {
                measured_prompt: Some(15_000),
                ..SettledUsage::default()
            }),
            ..settled("gpt-6-sol")
        });
        metrics.request_settled(&SettledRequest {
            usage: Some(SettledUsage {
                input: exact(40_000),
                ..SettledUsage::default()
            }),
            ..settled("gpt-6-sol")
        });

        let labels = "provider=\"codex\",model=\"gpt-6-sol\"";
        let open_metrics = with_prefix(
            &samples(&metrics, Format::OpenMetrics),
            "ccmux_request_prompt_tokens",
        );
        for line in [
            format!("ccmux_request_prompt_tokens_sum{{{labels}}} 15000.0"),
            format!("ccmux_request_prompt_tokens_count{{{labels}}} 1"),
            format!("ccmux_request_prompt_tokens_bucket{{le=\"10000.0\",{labels}}} 0"),
            format!("ccmux_request_prompt_tokens_bucket{{le=\"20000.0\",{labels}}} 1"),
        ] {
            assert!(
                open_metrics.contains(&line),
                "missing {line} in {open_metrics:?}"
            );
        }

        let text = with_prefix(
            &samples(&metrics, Format::Text),
            "ccmux_request_prompt_tokens",
        );
        for line in [
            format!("ccmux_request_prompt_tokens_bucket{{{labels},le=\"10000\"}} 0"),
            format!("ccmux_request_prompt_tokens_bucket{{{labels},le=\"20000\"}} 1"),
            format!("ccmux_request_prompt_tokens_count{{{labels}}} 1"),
        ] {
            assert!(text.contains(&line), "missing {line} in {text:?}");
        }
    }

    #[test]
    fn history_adds_its_counts_to_the_hit_basis() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&SettledRequest {
            usage: Some(SettledUsage {
                input: estimate(10),
                cache_read: exact(90),
                cache_write: exact(5),
                output: exact(7),
                measured_prompt: None,
                carries_history: true,
            }),
            ..settled("gpt-6-sol")
        });

        let expected = set(&[
            "ccmux_cache_hit_basis_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"input\"} 10",
            "ccmux_cache_hit_basis_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"cache_read\"} 90",
            "ccmux_cache_hit_basis_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"cache_write\"} 5",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(
                with_prefix(&samples, "ccmux_cache_hit_basis_tokens_total"),
                expected
            );
        }
    }

    #[test]
    fn a_request_without_history_adds_nothing_to_the_hit_basis() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&SettledRequest {
            usage: Some(SettledUsage {
                input: exact(10),
                cache_read: exact(90),
                cache_write: exact(5),
                output: exact(7),
                measured_prompt: None,
                carries_history: false,
            }),
            ..settled("gpt-6-sol")
        });

        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert!(with_prefix(&samples, "ccmux_cache_hit_basis_tokens").is_empty());
            assert_eq!(with_prefix(&samples, "ccmux_tokens_total").len(), 4);
        }
    }

    #[test]
    fn a_token_count_stops_at_the_largest_value() {
        let metrics = RequestMetrics::new();
        for tokens in [u64::MAX, 1] {
            metrics.request_settled(&SettledRequest {
                usage: Some(SettledUsage {
                    input: exact(tokens),
                    ..SettledUsage::default()
                }),
                ..settled("gpt-6-sol")
            });
        }

        let expected = set(&[
            "ccmux_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"input\",quality=\"exact\"} 18446744073709551615",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(with_prefix(&samples, "ccmux_tokens_total"), expected);
        }
    }

    #[test]
    fn the_hit_basis_stops_at_the_largest_value() {
        let metrics = RequestMetrics::new();
        for tokens in [u64::MAX, 1] {
            metrics.request_settled(&SettledRequest {
                usage: Some(SettledUsage {
                    cache_read: exact(tokens),
                    carries_history: true,
                    ..SettledUsage::default()
                }),
                ..settled("gpt-6-sol")
            });
        }

        let expected = set(&[
            "ccmux_cache_hit_basis_tokens_total{provider=\"codex\",model=\"gpt-6-sol\",kind=\"cache_read\"} 18446744073709551615",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(
                with_prefix(&samples, "ccmux_cache_hit_basis_tokens_total"),
                expected
            );
        }
    }

    #[test]
    fn the_pair_budget_falls_back_to_other() {
        let metrics = RequestMetrics::new();
        for index in 0..MAX_MODEL_SERIES {
            metrics.request_settled(&settled(&format!("model-{index}")));
        }
        metrics.request_settled(&settled("one-model-too-many"));
        metrics.cache_miss(Some("codex"), Some("another-one"), MissCause::Expired);
        metrics.context_rewrite(Some("kimi"), Some("model-0"));

        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            let requests = with_prefix(&samples, "ccmux_requests_total");
            assert_eq!(requests.len(), MAX_MODEL_SERIES + 1);
            assert!(requests.contains(&requests_line("codex", "model-0", 1)));
            assert!(requests.contains(&requests_line("codex", "model-199", 1)));
            assert!(requests.contains(&requests_line("other", "other", 1)));
            assert!(
                !requests
                    .iter()
                    .any(|line| line.contains("one-model-too-many"))
            );
            assert_eq!(
                with_prefix(&samples, "ccmux_cache_misses_total"),
                set(&[
                    "ccmux_cache_misses_total{provider=\"other\",model=\"other\",cause=\"expired\"} 1"
                ])
            );
            assert_eq!(
                with_prefix(&samples, "ccmux_context_rewrites_total"),
                set(&["ccmux_context_rewrites_total{provider=\"other\",model=\"other\"} 1"])
            );
        }
    }

    #[test]
    fn an_overlong_label_makes_the_pair_other() {
        let metrics = RequestMetrics::new();
        let longest = "m".repeat(120);
        let overlong_model = "m".repeat(121);
        let overlong_provider = "p".repeat(121);
        metrics.request_settled(&settled(&longest));
        metrics.request_settled(&settled(&overlong_model));
        metrics.request_settled(&SettledRequest {
            provider: Some(&overlong_provider),
            ..settled("gpt-6-sol")
        });

        for format in BOTH_FORMATS {
            let requests = with_prefix(&samples(&metrics, format), "ccmux_requests_total");
            assert_eq!(
                requests,
                set(&[
                    &requests_line("codex", &longest, 1),
                    &requests_line("other", "other", 2),
                ])
            );
        }
    }

    #[test]
    fn an_admitted_pair_keeps_its_labels_past_the_cap() {
        let metrics = RequestMetrics::new();
        metrics.request_settled(&settled("gpt-6-sol"));
        for index in 1..MAX_MODEL_SERIES {
            metrics.request_settled(&settled(&format!("model-{index}")));
        }
        metrics.request_settled(&settled("one-model-too-many"));
        metrics.request_settled(&settled("gpt-6-sol"));
        metrics.cache_miss(Some("codex"), Some("gpt-6-sol"), MissCause::WithinTtl);

        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            let requests = with_prefix(&samples, "ccmux_requests_total");
            assert!(requests.contains(&requests_line("codex", "gpt-6-sol", 2)));
            assert!(requests.contains(&requests_line("other", "other", 1)));
            assert_eq!(
                with_prefix(&samples, "ccmux_cache_misses_total"),
                set(&[
                    "ccmux_cache_misses_total{provider=\"codex\",model=\"gpt-6-sol\",cause=\"within_ttl\"} 1"
                ])
            );
        }
    }

    #[test]
    fn misses_and_rewrites_count_under_their_labels() {
        let metrics = RequestMetrics::new();
        metrics.cache_miss(Some("codex"), Some("gpt-6-sol"), MissCause::WithinTtl);
        metrics.cache_miss(Some("codex"), Some("gpt-6-sol"), MissCause::WithinTtl);
        metrics.cache_miss(Some("codex"), Some("gpt-6-sol"), MissCause::Expired);
        metrics.cache_miss(None, None, MissCause::UnknownTtl);
        metrics.context_rewrite(Some("anthropic"), Some("claude-opus-5-5"));
        metrics.context_rewrite(Some("anthropic"), None);

        let misses = set(&[
            "ccmux_cache_misses_total{provider=\"codex\",model=\"gpt-6-sol\",cause=\"within_ttl\"} 2",
            "ccmux_cache_misses_total{provider=\"codex\",model=\"gpt-6-sol\",cause=\"expired\"} 1",
            "ccmux_cache_misses_total{provider=\"unknown\",model=\"unknown\",cause=\"unknown_ttl\"} 1",
        ]);
        let rewrites = set(&[
            "ccmux_context_rewrites_total{provider=\"anthropic\",model=\"claude-opus-5-5\"} 1",
            "ccmux_context_rewrites_total{provider=\"anthropic\",model=\"unknown\"} 1",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&metrics, format);
            assert_eq!(with_prefix(&samples, "ccmux_cache_misses_total"), misses);
            assert_eq!(
                with_prefix(&samples, "ccmux_context_rewrites_total"),
                rewrites
            );
            assert!(with_prefix(&samples, "ccmux_requests_total").is_empty());
        }
    }
}
