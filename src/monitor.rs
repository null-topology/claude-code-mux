use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

mod accounting;
mod mock;
mod usage;

use accounting::{AbsorbedRequest, Ledger, SessionRecord};
pub use accounting::{
    CacheMissTally, LOCAL_PROVIDER, ModelUsage, QualityCoverage, UnattributedUsage, UsageEvidence,
};
pub use mock::{MockMonitor, mock_state};
pub use usage::{
    CacheMiss, CacheMissCause, CacheWriteQuality, QualityFields, UsageFields, UsageQuality,
    UsageReport, caches_implicitly, default_cache_ttl, detect_cache_miss,
    usage_report_from_anthropic_body, usage_report_from_anthropic_sse,
};
use usage::{ClosedFields, UsageDelta, apply_closing, apply_opening, quality_of};

const DEFAULT_RECENT_LIMIT: usize = 200;
pub const SESSION_TOKEN_BUCKET_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Messages,
    CountTokens,
    Responses,
    ChatCompletions,
    Images,
    Transcriptions,
}

impl EndpointKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Images => "images",
            Self::Transcriptions => "transcriptions",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    Started,
    ProviderSelected,
    Compacting,
    Upstream,
    Streaming,
    Completed,
    Failed,
}

impl RequestStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::ProviderSelected => "selected",
            Self::Compacting => "compacting",
            Self::Upstream => "upstream",
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    RequestStarted {
        request_id: String,
        session_id: Option<String>,
        session_seq: Option<u64>,
        endpoint: EndpointKind,
    },
    ProjectResolved {
        request_id: String,
        project: String,
    },
    SessionSequenceResolved {
        request_id: String,
        session_seq: u64,
    },
    /// The model the client asked for, as its request named it and before the
    /// proxy rewrote anything: the summary and classifier overrides, the
    /// one-hour suffix, and provider aliases all come after this. It is
    /// published on its own so a request that never reached a provider still
    /// says what it asked for.
    ModelRequested {
        request_id: String,
        model: String,
    },
    ProviderSelected {
        request_id: String,
        provider: String,
        model: String,
        effort: Option<String>,
    },
    /// The model a provider put on the wire, as the outgoing request carried
    /// it. Only a producer that saw the request leave publishes this, so a
    /// request that failed before it was built has none.
    ModelResolved {
        request_id: String,
        model: String,
    },
    CompactionStarted {
        request_id: String,
    },
    UpstreamStarted {
        request_id: String,
    },
    GenerationStarted {
        request_id: String,
    },
    TrafficCapturePath {
        request_id: String,
        path: PathBuf,
    },
    /// Which conversation of the session the request belongs to: `main`, or
    /// the Claude Code agent id of a subagent. `parent` is the agent that
    /// spawned it, when Claude Code says so, which nests subagents of
    /// subagents under the one they came from.
    ConversationResolved {
        request_id: String,
        conversation: String,
        parent: Option<String>,
    },
    StreamProgress {
        request_id: String,
        bytes: u64,
        chunks: u64,
        usage: UsageReport,
    },
    UsageUpdated {
        request_id: String,
        usage: UsageReport,
    },
    RequestCompleted {
        request_id: String,
        http_status: u16,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    RequestFailed {
        request_id: String,
        http_status: Option<u16>,
        error: String,
    },
    RequestAbandoned {
        request_id: String,
        error: String,
    },
}

/// Cache usage of one request, next to its input and output counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCache {
    pub read_tokens: Option<u64>,
    pub write_tokens: Option<u64>,
    /// How much of the write was made with the five-minute lifetime, when the
    /// response said. Part of `write_tokens`, never tokens beside it.
    pub write_5m_tokens: Option<u64>,
    /// The same for the one-hour lifetime.
    pub write_1h_tokens: Option<u64>,
    /// Set when the request's final cache read fell well short of the previous
    /// prompt in its conversation lane.
    pub miss: Option<CacheMiss>,
    /// Lifetime of the cache entries the request wrote, when the response said.
    pub ttl: Option<Duration>,
    /// The whole prompt as the backend counted it, when it reported a total of
    /// its own. It holds the same tokens the categories do, so it is the prompt
    /// size rather than anything to add to them, and it is the only number
    /// available when a backend reports a total without a cache split.
    pub reported_prompt_tokens: Option<u64>,
    closed: ClosedFields,
    evaluated: bool,
}

impl RequestCache {
    /// Whether the request has been compared with the previous one of its lane.
    pub fn evaluated(&self) -> bool {
        self.evaluated
    }
}

/// Prompt size of a request: the total the backend reported, else uncached
/// input plus cache reads and writes.
///
/// A reported total is the backend's own measurement of the whole prompt and
/// wins over the sum, which may be carrying an estimate in one of its parts.
fn prompt_tokens(input_tokens: Option<u64>, cache: &RequestCache) -> Option<u64> {
    if let Some(total) = cache.reported_prompt_tokens {
        return Some(total);
    }
    if input_tokens.is_none() && cache.read_tokens.is_none() && cache.write_tokens.is_none() {
        return None;
    }
    Some(
        input_tokens
            .unwrap_or(0)
            .saturating_add(cache.read_tokens.unwrap_or(0))
            .saturating_add(cache.write_tokens.unwrap_or(0)),
    )
}

/// How far each count of a request has been pinned down, from the values held
/// and the closing values that produced them.
fn usage_quality(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache: &RequestCache,
) -> QualityFields {
    QualityFields {
        input: quality_of(input_tokens, cache.closed.input),
        cache_read: quality_of(cache.read_tokens, cache.closed.cache_read),
        cache_write: quality_of(cache.write_tokens, cache.closed.cache_write),
        output: quality_of(output_tokens, cache.closed.output),
    }
}

/// How far each lifetime bucket of a request's cache write has been pinned
/// down, judged on its own report rather than on the write it belongs to.
fn cache_write_quality(cache: &RequestCache) -> CacheWriteQuality {
    CacheWriteQuality {
        ephemeral_5m: quality_of(cache.write_5m_tokens, cache.closed.cache_write_5m),
        ephemeral_1h: quality_of(cache.write_1h_tokens, cache.closed.cache_write_1h),
    }
}

/// Share of the prompt served from cache.
fn cache_hit_ratio(input_tokens: Option<u64>, cache: &RequestCache) -> Option<f64> {
    let prompt = prompt_tokens(input_tokens, cache)?;
    let read = cache.read_tokens?;
    (prompt > 0).then(|| read as f64 / prompt as f64)
}

#[derive(Debug, Clone)]
pub struct ActiveRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub conversation: Option<String>,
    /// The conversation that spawned this one, from Claude Code's lineage
    /// header.
    pub conversation_parent: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    /// What the row shows: the routed model, and the wire model after it when
    /// the two differ. A display projection of the two fields below, kept for
    /// the views that grew up on it; nothing is counted per model by it.
    pub model: Option<String>,
    /// The model the client asked for, before any rewrite.
    pub requested_model: Option<String>,
    /// The model the provider put on the wire, when one was observed leaving.
    pub effective_model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    started_instant: Instant,
    pub generation_started_at: Option<SystemTime>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache: RequestCache,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl ActiveRequest {
    pub fn elapsed(&self) -> Duration {
        self.started_instant.elapsed()
    }

    pub fn prompt_tokens(&self) -> Option<u64> {
        prompt_tokens(self.input_tokens, &self.cache)
    }

    pub fn cache_hit_ratio(&self) -> Option<f64> {
        cache_hit_ratio(self.input_tokens, &self.cache)
    }

    pub fn usage_quality(&self) -> QualityFields {
        usage_quality(self.input_tokens, self.output_tokens, &self.cache)
    }

    pub fn cache_write_quality(&self) -> CacheWriteQuality {
        cache_write_quality(&self.cache)
    }

    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone)]
pub struct CompletedRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub conversation: Option<String>,
    /// The conversation that spawned this one, from Claude Code's lineage
    /// header.
    pub conversation_parent: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    /// The routed model and the wire model, as [`ActiveRequest::model`].
    pub model: Option<String>,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub generation_started_at: Option<SystemTime>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub http_status: Option<u16>,
    pub latency: Duration,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache: RequestCache,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl CompletedRequest {
    pub fn prompt_tokens(&self) -> Option<u64> {
        prompt_tokens(self.input_tokens, &self.cache)
    }

    pub fn cache_hit_ratio(&self) -> Option<f64> {
        cache_hit_ratio(self.input_tokens, &self.cache)
    }

    pub fn usage_quality(&self) -> QualityFields {
        usage_quality(self.input_tokens, self.output_tokens, &self.cache)
    }

    pub fn cache_write_quality(&self) -> CacheWriteQuality {
        cache_write_quality(&self.cache)
    }

    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Throughput {
    TokensPerSecond(f64),
    BytesPerSecond(f64),
    EventsPerSecond(f64),
    None,
}

impl Throughput {
    pub fn label(&self) -> String {
        match self {
            Self::TokensPerSecond(value) => format!("{value:.1} tok/s"),
            Self::BytesPerSecond(value) if *value >= 1024.0 => {
                format!("{:.1} KB/s", value / 1024.0)
            }
            Self::BytesPerSecond(value) => format!("{value:.0} B/s"),
            Self::EventsPerSecond(value) => format!("{value:.1} ev/s"),
            Self::None => "-".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorState {
    pub started_at: SystemTime,
    pub sessions: Vec<SessionSummary>,
    pub active: Vec<ActiveRequest>,
    pub recent: Vec<CompletedRequest>,
}

impl MonitorState {
    /// What every backend and model that ran cost since the proxy started, one
    /// row per pair, added up over every session, largest prompt total first.
    ///
    /// The counts are the session rollups summed, so they outlive the recent
    /// list the way the sessions do. The two timing figures are the exception:
    /// a median is read off the requests still in view, so they cover the
    /// recent window only and say so in the row.
    pub fn model_stats(&self) -> Vec<ModelStats> {
        let mut rows: Vec<ModelStats> = Vec::new();
        // Answers the proxy gave itself ran on no model, so they have no row.
        for usage in self
            .sessions
            .iter()
            .flat_map(|session| &session.models)
            .filter(|usage| usage.provider.as_deref() != Some(LOCAL_PROVIDER))
        {
            let row = match rows
                .iter_mut()
                .find(|row| row.provider == usage.provider && row.model == usage.model)
            {
                Some(row) => row,
                None => {
                    rows.push(ModelStats {
                        provider: usage.provider.clone(),
                        model: usage.model.clone(),
                        ..ModelStats::default()
                    });
                    rows.last_mut().expect("row just pushed")
                }
            };
            row.active_count = row.active_count.saturating_add(usage.active_count);
            row.request_count = row.request_count.saturating_add(usage.request_count);
            row.failure_count = row.failure_count.saturating_add(usage.failure_count);
            row.input_tokens = row.input_tokens.saturating_add(usage.input_tokens);
            row.output_tokens = row.output_tokens.saturating_add(usage.output_tokens);
            row.cache_read_tokens = row
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            row.cache_write_tokens = row
                .cache_write_tokens
                .saturating_add(usage.cache_write_tokens);
            row.evidence.add(&usage.evidence);
            row.misses.add(&usage.misses);
            for (requested, count) in &usage.requested_models {
                match row
                    .requested_models
                    .iter_mut()
                    .find(|(known, _)| known == requested)
                {
                    Some((_, total)) => *total = total.saturating_add(*count),
                    None => row.requested_models.push((requested.clone(), *count)),
                }
            }
        }
        for row in &mut rows {
            let window: Vec<&CompletedRequest> = self
                .recent
                .iter()
                .filter(|request| {
                    request.status == RequestStatus::Completed
                        && request.provider == row.provider
                        && request.effective_model == row.model
                })
                .collect();
            row.recent_requests = window.len();
            row.median_latency = median(
                window.iter().map(|request| request.latency),
                // Half the gap added to the lower value cannot overflow the
                // way a sum of the two can.
                |low, high| low + (high - low) / 2,
            );
            row.median_output_rate = median(
                window.iter().filter_map(|request| match request.rate() {
                    Throughput::TokensPerSecond(rate) => Some(rate),
                    _ => None,
                }),
                |low, high| (low + high) / 2.0,
            );
        }
        rows.sort_by(|left, right| {
            right
                .prompt_tokens()
                .cmp(&left.prompt_tokens())
                .then_with(|| right.request_count.cmp(&left.request_count))
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.model.cmp(&right.model))
        });
        rows
    }
}

/// The middle value of a sample, or the `mean` of the two middle values, lower
/// first, when the sample has an even size.
fn median<T: Copy + PartialOrd>(
    values: impl Iterator<Item = T>,
    mean: impl FnOnce(T, T) -> T,
) -> Option<T> {
    let mut values: Vec<T> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let upper = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        mean(values[upper - 1], values[upper])
    } else {
        values[upper]
    })
}

/// What one backend and one model that ran cost over every session, and how
/// its recent requests performed. Every count means what a session's does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStats {
    pub provider: Option<String>,
    /// The model on the wire, or `None` where none was observed.
    pub model: Option<String>,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub evidence: UsageEvidence,
    pub misses: CacheMissTally,
    /// The ids the clients asked for, with how many requests each fed in,
    /// summed over the session rollups.
    pub requested_models: Vec<(Option<String>, usize)>,
    /// Completed requests of the row still in the recent list: what the two
    /// medians below are read off.
    pub recent_requests: usize,
    pub median_latency: Option<Duration>,
    /// Median output tokens per second over the same requests, counting only
    /// the ones with a measured generation interval.
    pub median_output_rate: Option<f64>,
}

impl ModelStats {
    /// The prompt tokens of every request of the row: uncached input plus the
    /// cache reads and writes.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Share of the row's prompt tokens served from cache.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        totals_cache_hit_ratio(
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        )
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub project: Option<String>,
    /// Where the session sits among the ones seen before it: a stable key for
    /// ordering rows, unaffected by anything that happens later.
    pub first_seen_rank: usize,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    /// Uncached input tokens; `count_tokens` estimates and locally answered
    /// requests are not counted.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// How the cache write splits by the lifetime it was written with. The two
    /// are part of `cache_write_tokens`, so they are never added to a prompt.
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    /// How firmly each of those four counts is known, request by request.
    pub evidence: UsageEvidence,
    pub cache: SessionCacheStats,
    pub output_token_samples: Vec<(SystemTime, u64)>,
    rate_output_tokens: u64,
    pub generation_duration: Duration,
    pub last_status: String,
    /// The conversations this session is made of, in display order: a
    /// depth-first walk of the spawn tree, `main` first and side lanes last
    /// within a level. Their figures plus `unattributed` add up to the
    /// session's.
    pub conversations: Vec<ConversationSummary>,
    /// The requests of this session that named no conversation, counted on
    /// their own rather than left as what the rows do not explain.
    pub unattributed: UnattributedUsage,
    /// What the session cost per backend and per model that actually ran, in
    /// the order the rows were first seen. Their figures add up to the
    /// session's, requests whose model is unknown included as a row of their
    /// own.
    pub models: Vec<ModelUsage>,
}

/// One conversation of a session: the main thread, a subagent, or a side lane.
/// Every figure means what the session's does, counted for this lane alone.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    /// `main`, the agent id of a subagent, or either with the side suffix.
    pub conversation: String,
    /// The conversation this one hangs under, once resolved within the
    /// session: the agent that spawned it, or the conversation a side call was
    /// made from. `None` for a conversation that hangs under the session row
    /// itself, which is also where an unresolvable parent lands.
    pub parent: Option<String>,
    /// The parent Claude Code named, as it named it, whether or not this
    /// session has been seen talking to it.
    pub raw_parent: Option<String>,
    /// Where the conversation sits among the ones of its session seen before it.
    pub first_seen_rank: usize,
    /// Levels below the session row, `0` for a conversation hanging under it.
    pub depth: usize,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// The lifetime split of the cache write, as a session's.
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    /// How firmly each of those four counts is known, request by request.
    pub evidence: UsageEvidence,
    pub cache: SessionCacheStats,
    pub last_status: String,
}

impl ConversationSummary {
    /// Share of this conversation's prompt tokens served from cache.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        totals_cache_hit_ratio(
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        )
    }

    /// Whether the conversation is one of Claude Code's side calls, which do
    /// not extend a transcript.
    pub fn is_side(&self) -> bool {
        self.conversation.ends_with(SIDE_CONVERSATION_SUFFIX)
    }
}

/// Cache behaviour of a session, kept for its whole life rather than the
/// recent-request window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCacheStats {
    pub miss_count: u64,
    pub missed_tokens: u64,
    pub last_miss: Option<(SystemTime, CacheMiss)>,
    /// Prompt size of the latest main-conversation request.
    pub context_tokens: u64,
    pub peak_context_tokens: u64,
    /// When that request started and the cache lifetime of its lane: together
    /// they say how long its prefix should stay readable.
    pub context_started_at: Option<SystemTime>,
    pub context_ttl: Option<Duration>,
}

impl SessionCacheStats {
    /// Time left before the main conversation's cached prefix expires, or how
    /// long ago it expired, as of `now`.
    pub fn context_cache_expiry(&self, now: SystemTime) -> Option<CacheExpiry> {
        let started_at = self.context_started_at?;
        let ttl = self.context_ttl?;
        let idle = now.duration_since(started_at).unwrap_or(Duration::ZERO);
        Some(match ttl.checked_sub(idle) {
            Some(left) if !left.is_zero() => CacheExpiry::WarmFor(left),
            _ => CacheExpiry::ExpiredAgo(idle.saturating_sub(ttl)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheExpiry {
    WarmFor(Duration),
    ExpiredAgo(Duration),
}

/// Share of a set of accumulated prompt tokens that came from cache.
fn totals_cache_hit_ratio(
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> Option<f64> {
    let prompt = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens);
    (prompt > 0 && (cache_read_tokens > 0 || cache_write_tokens > 0))
        .then(|| cache_read_tokens as f64 / prompt as f64)
}

impl SessionSummary {
    /// Share of all prompt tokens served from cache.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        totals_cache_hit_ratio(
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        )
    }

    pub fn rate(&self) -> Throughput {
        throughput(
            Some(self.rate_output_tokens).filter(|tokens| *tokens > 0),
            0,
            0,
            self.generation_duration,
        )
    }

    pub fn label(&self) -> String {
        self.session_id
            .clone()
            .unwrap_or_else(|| "no-session".to_string())
    }
}

#[derive(Debug)]
struct MonitorStore {
    started_at: SystemTime,
    active: HashMap<String, ActiveRequest>,
    recent: VecDeque<CompletedRequest>,
    /// Counting, attribution and token totals for every request served, which
    /// outlive the recent list the detail above is bounded by.
    ledger: Ledger,
    lanes: HashMap<LaneKey, LaneState>,
    recent_limit: usize,
}

/// The four accumulated cost categories of one row, and the lifetime split of
/// the cache write, which is part of the third rather than a fifth.
#[derive(Debug, Clone, Copy, Default)]
struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
}

/// Make a request the latest prompt of the totals it belongs to: the size the
/// context column shows and the window its cached prefix lives in.
fn note_context(
    stats: &mut SessionCacheStats,
    prompt: u64,
    started_at: SystemTime,
    ttl: Option<Duration>,
) {
    stats.context_tokens = prompt;
    stats.peak_context_tokens = stats.peak_context_tokens.max(prompt);
    stats.context_started_at = Some(started_at);
    stats.context_ttl = ttl;
}

fn note_miss(stats: &mut SessionCacheStats, started_at: SystemTime, miss: CacheMiss) {
    stats.miss_count = stats.miss_count.saturating_add(1);
    stats.missed_tokens = stats.missed_tokens.saturating_add(miss.missed_tokens);
    stats.last_miss = Some((started_at, miss));
}

/// Appended to a conversation label for requests without client tools. They
/// are side calls that do not extend the transcript, so consecutive ones share
/// little beyond the system prompt and are not judged for cache misses.
pub const SIDE_CONVERSATION_SUFFIX: &str = "/side";

/// One conversation's stream of requests to one model: the unit a prompt cache
/// builds up in. Subagents and side requests get their own lanes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LaneKey {
    session_id: Option<String>,
    conversation: Option<String>,
    provider: String,
    model: String,
}

#[derive(Debug, Clone, Copy)]
struct LaneState {
    prompt_tokens: u64,
    started_at: SystemTime,
    /// When the request's response began. A cache entry becomes readable only
    /// then, so a request sent earlier could not have used it.
    readable_from: SystemTime,
    ttl: Option<Duration>,
    /// Whether any request of the lane has read or written cache.
    reported_cache: bool,
}

/// Usage of one request as the store tracks it.
struct UsageTarget<'a> {
    input_tokens: &'a mut Option<u64>,
    output_tokens: &'a mut Option<u64>,
    cache: &'a mut RequestCache,
}

impl UsageTarget<'_> {
    fn apply(&mut self, report: &UsageReport) -> UsageDelta {
        let cache = &mut *self.cache;
        let mut delta = UsageDelta::default();
        delta.input += apply_opening(
            self.input_tokens,
            cache.closed.input,
            report.opening.input_tokens,
        );
        delta.cache_read += apply_opening(
            &mut cache.read_tokens,
            cache.closed.cache_read,
            report.opening.cache_read_tokens,
        );
        delta.cache_write += apply_opening(
            &mut cache.write_tokens,
            cache.closed.cache_write,
            report.opening.cache_write_tokens,
        );
        delta.output += apply_opening(
            self.output_tokens,
            cache.closed.output,
            report.opening.output_tokens,
        );
        delta.cache_write_5m += apply_opening(
            &mut cache.write_5m_tokens,
            cache.closed.cache_write_5m,
            report.opening.cache_write_5m_tokens,
        );
        delta.cache_write_1h += apply_opening(
            &mut cache.write_1h_tokens,
            cache.closed.cache_write_1h,
            report.opening.cache_write_1h_tokens,
        );
        delta.input += apply_closing(
            self.input_tokens,
            &mut cache.closed.input,
            report.closing.input_tokens,
        );
        delta.cache_read += apply_closing(
            &mut cache.read_tokens,
            &mut cache.closed.cache_read,
            report.closing.cache_read_tokens,
        );
        delta.cache_write += apply_closing(
            &mut cache.write_tokens,
            &mut cache.closed.cache_write,
            report.closing.cache_write_tokens,
        );
        delta.output += apply_closing(
            self.output_tokens,
            &mut cache.closed.output,
            report.closing.output_tokens,
        );
        // The lifetime buckets are closed on their own reports. A write whose
        // total is final says nothing about a bucket the response never named,
        // and a bucket says nothing about the total.
        delta.cache_write_5m += apply_closing(
            &mut cache.write_5m_tokens,
            &mut cache.closed.cache_write_5m,
            report.closing.cache_write_5m_tokens,
        );
        delta.cache_write_1h += apply_closing(
            &mut cache.write_1h_tokens,
            &mut cache.closed.cache_write_1h,
            report.closing.cache_write_1h_tokens,
        );
        if report.cache_ttl.is_some() {
            cache.ttl = report.cache_ttl;
        }
        // The backend's own prompt total is kept as it came. It is not a fifth
        // category and carries no delta: the categories above already account
        // for the same tokens. A report that does not carry one leaves the
        // number a previous report established.
        if report.reported_prompt_tokens.is_some() {
            cache.reported_prompt_tokens = report.reported_prompt_tokens;
        }
        delta
    }
}

#[derive(Debug, Clone)]
pub struct MonitorHandle {
    store: Arc<Mutex<MonitorStore>>,
}

impl Default for MonitorHandle {
    fn default() -> Self {
        Self::new(DEFAULT_RECENT_LIMIT)
    }
}

impl MonitorHandle {
    pub fn new(recent_limit: usize) -> Self {
        Self {
            store: Arc::new(Mutex::new(MonitorStore {
                started_at: SystemTime::now(),
                active: HashMap::new(),
                recent: VecDeque::new(),
                ledger: Ledger::default(),
                lanes: HashMap::new(),
                recent_limit,
            })),
        }
    }

    pub fn publish(&self, event: MonitorEvent) {
        if let Ok(mut store) = self.store.lock() {
            store.apply(event);
        }
    }

    pub fn snapshot(&self) -> MonitorState {
        match self.store.lock() {
            Ok(store) => store.snapshot(),
            Err(_) => MonitorState {
                started_at: SystemTime::now(),
                sessions: Vec::new(),
                active: Vec::new(),
                recent: Vec::new(),
            },
        }
    }

    pub fn request_started(
        &self,
        request_id: impl Into<String>,
        session_id: Option<String>,
        session_seq: Option<u64>,
        endpoint: EndpointKind,
    ) {
        self.publish(MonitorEvent::RequestStarted {
            request_id: request_id.into(),
            session_id,
            session_seq,
            endpoint,
        });
    }

    pub fn project_resolved(&self, request_id: impl Into<String>, project: impl Into<String>) {
        self.publish(MonitorEvent::ProjectResolved {
            request_id: request_id.into(),
            project: project.into(),
        });
    }

    pub fn session_sequence_resolved(&self, request_id: impl Into<String>, session_seq: u64) {
        self.publish(MonitorEvent::SessionSequenceResolved {
            request_id: request_id.into(),
            session_seq,
        });
    }

    pub fn provider_selected(
        &self,
        request_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: Option<String>,
    ) {
        self.publish(MonitorEvent::ProviderSelected {
            request_id: request_id.into(),
            provider: provider.into(),
            model: model.into(),
            effort,
        });
    }

    /// The model the client's own request named, before the proxy rewrote
    /// anything about it.
    pub fn model_requested(&self, request_id: impl Into<String>, model: impl Into<String>) {
        self.publish(MonitorEvent::ModelRequested {
            request_id: request_id.into(),
            model: model.into(),
        });
    }

    /// The model a provider put on the wire, as its outgoing request carried
    /// it.
    pub fn model_resolved(&self, request_id: impl Into<String>, model: impl Into<String>) {
        self.publish(MonitorEvent::ModelResolved {
            request_id: request_id.into(),
            model: model.into(),
        });
    }

    pub fn compaction_started(&self, request_id: impl Into<String>) {
        self.publish(MonitorEvent::CompactionStarted {
            request_id: request_id.into(),
        });
    }

    pub fn upstream_started(&self, request_id: impl Into<String>) {
        self.publish(MonitorEvent::UpstreamStarted {
            request_id: request_id.into(),
        });
    }

    pub fn generation_started(&self, request_id: impl Into<String>) {
        self.publish(MonitorEvent::GenerationStarted {
            request_id: request_id.into(),
        });
    }

    pub fn traffic_capture_path(&self, request_id: impl Into<String>, path: PathBuf) {
        self.publish(MonitorEvent::TrafficCapturePath {
            request_id: request_id.into(),
            path,
        });
    }

    pub fn conversation_resolved(
        &self,
        request_id: impl Into<String>,
        conversation: impl Into<String>,
        parent: Option<String>,
    ) {
        self.publish(MonitorEvent::ConversationResolved {
            request_id: request_id.into(),
            conversation: conversation.into(),
            parent,
        });
    }

    /// Stream progress with input and output counts that only ever raise the
    /// request's totals.
    pub fn stream_progress(
        &self,
        request_id: impl Into<String>,
        bytes: u64,
        chunks: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.stream_progress_usage(
            request_id,
            bytes,
            chunks,
            UsageReport::opening(input_tokens, output_tokens),
        );
    }

    /// Stream progress with a full usage report, whose closing values replace
    /// earlier estimates.
    pub fn stream_progress_usage(
        &self,
        request_id: impl Into<String>,
        bytes: u64,
        chunks: u64,
        usage: UsageReport,
    ) {
        self.publish(MonitorEvent::StreamProgress {
            request_id: request_id.into(),
            bytes,
            chunks,
            usage,
        });
    }

    /// Input and output counts that only ever raise the request's totals.
    pub fn usage_updated(
        &self,
        request_id: impl Into<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.usage_reported(
            request_id,
            UsageReport::opening(input_tokens, output_tokens),
        );
    }

    pub fn usage_reported(&self, request_id: impl Into<String>, usage: UsageReport) {
        self.publish(MonitorEvent::UsageUpdated {
            request_id: request_id.into(),
            usage,
        });
    }

    pub fn request_completed(
        &self,
        request_id: impl Into<String>,
        http_status: u16,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.publish(MonitorEvent::RequestCompleted {
            request_id: request_id.into(),
            http_status,
            input_tokens,
            output_tokens,
        });
    }

    pub fn request_failed(
        &self,
        request_id: impl Into<String>,
        http_status: Option<u16>,
        error: impl Into<String>,
    ) {
        self.publish(MonitorEvent::RequestFailed {
            request_id: request_id.into(),
            http_status,
            error: error.into(),
        });
    }

    pub fn request_abandoned(&self, request_id: impl Into<String>, error: impl Into<String>) {
        self.publish(MonitorEvent::RequestAbandoned {
            request_id: request_id.into(),
            error: error.into(),
        });
    }
}

impl MonitorStore {
    fn apply(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::RequestStarted {
                request_id,
                session_id,
                session_seq,
                endpoint,
            } => {
                let started_at = SystemTime::now();
                self.ledger
                    .start(&request_id, session_id.clone(), endpoint, started_at);
                self.active.insert(
                    request_id.clone(),
                    ActiveRequest {
                        request_id,
                        session_id,
                        conversation: None,
                        conversation_parent: None,
                        session_seq,
                        project: None,
                        provider: None,
                        model: None,
                        requested_model: None,
                        effective_model: None,
                        effort: None,
                        endpoint,
                        started_at,
                        started_instant: Instant::now(),
                        generation_started_at: None,
                        generation_initial_output_tokens: 0,
                        generation_finished_at: None,
                        generation_duration: None,
                        status: RequestStatus::Started,
                        streamed_bytes: 0,
                        stream_chunks: 0,
                        input_tokens: None,
                        output_tokens: None,
                        cache: RequestCache::default(),
                        error: None,
                        traffic_capture_path: None,
                    },
                );
            }
            MonitorEvent::ProjectResolved {
                request_id,
                project,
            } => {
                self.ledger.note_project(&request_id, project.clone());
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.project = Some(project);
                }
            }
            MonitorEvent::SessionSequenceResolved {
                request_id,
                session_seq,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.session_seq = Some(session_seq);
                }
            }
            MonitorEvent::ProviderSelected {
                request_id,
                provider,
                model,
                effort,
            } => {
                self.ledger.note_selection(
                    &request_id,
                    provider.clone(),
                    model.clone(),
                    effort.clone(),
                );
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.provider = Some(provider);
                    active.model = Some(model);
                    active.effort = effort;
                }
                self.project(&request_id);
            }
            MonitorEvent::ModelRequested { request_id, model } => {
                // The visible row shows what the ledger kept, which is the
                // first naming, not whatever this event carried.
                if let Some(requested) = self.ledger.note_requested_model(&request_id, model) {
                    self.project_requested_model(&request_id, requested);
                }
            }
            MonitorEvent::ModelResolved { request_id, model } => {
                if let Some(display) = self.ledger.note_resolved_model(&request_id, model.clone()) {
                    self.project_resolved_model(&request_id, display, model);
                }
            }
            MonitorEvent::CompactionStarted { request_id } => {
                self.ledger
                    .note_status(&request_id, RequestStatus::Compacting);
                self.project(&request_id);
            }
            MonitorEvent::UpstreamStarted { request_id } => {
                self.ledger
                    .note_status(&request_id, RequestStatus::Upstream);
                self.project(&request_id);
            }
            MonitorEvent::GenerationStarted { request_id } => {
                self.ledger
                    .note_generation_started(&request_id, SystemTime::now());
                self.project(&request_id);
            }
            MonitorEvent::TrafficCapturePath { request_id, path } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.traffic_capture_path = Some(path);
                }
            }
            MonitorEvent::ConversationResolved {
                request_id,
                conversation,
                parent,
            } => {
                self.ledger
                    .note_conversation(&request_id, conversation.clone(), parent.clone());
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.conversation = Some(conversation);
                    active.conversation_parent = parent;
                }
            }
            MonitorEvent::StreamProgress {
                request_id,
                bytes,
                chunks,
                usage,
            } => {
                // The ledger owns the counts and the generation interval; the
                // visible row mirrors them and keeps the stream's own volume.
                if self
                    .ledger
                    .note_stream_progress(&request_id, &usage, SystemTime::now())
                    .is_some()
                {
                    if let Some(active) = self.active.get_mut(&request_id) {
                        active.streamed_bytes = active.streamed_bytes.saturating_add(bytes);
                        active.stream_chunks = active.stream_chunks.saturating_add(chunks);
                    } else if let Some(completed) = self
                        .recent
                        .iter_mut()
                        .find(|request| request.request_id == request_id)
                    {
                        completed.streamed_bytes = completed.streamed_bytes.saturating_add(bytes);
                        completed.stream_chunks = completed.stream_chunks.saturating_add(chunks);
                    }
                    self.project(&request_id);
                }
                self.evaluate_cache(&request_id);
            }
            MonitorEvent::UsageUpdated { request_id, usage } => {
                if self
                    .ledger
                    .note_usage(&request_id, &usage, SystemTime::now())
                    .is_some()
                {
                    self.project(&request_id);
                }
                self.evaluate_cache(&request_id);
            }
            MonitorEvent::RequestCompleted {
                request_id,
                http_status,
                input_tokens,
                output_tokens,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Completed,
                    Some(http_status),
                    input_tokens,
                    output_tokens,
                    None,
                );
            }
            MonitorEvent::RequestFailed {
                request_id,
                http_status,
                error,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Failed,
                    http_status,
                    None,
                    None,
                    Some(error),
                );
            }
            MonitorEvent::RequestAbandoned { request_id, error } => {
                self.finish_active(
                    &request_id,
                    RequestStatus::Failed,
                    None,
                    None,
                    None,
                    Some(error),
                );
            }
        }
    }

    fn finish_active(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        if self.active.contains_key(request_id) {
            self.finish(
                request_id,
                status,
                http_status,
                input_tokens,
                output_tokens,
                error,
            );
        }
    }

    fn finish(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        let report = UsageReport::opening(input_tokens, output_tokens);
        if !self
            .ledger
            .finish(request_id, status, &report, SystemTime::now())
        {
            // A second terminal event for the same request counts nothing
            // twice, and the outcome the first one recorded stands. The counts
            // it carried are still worth taking.
            self.project(request_id);
            self.evaluate_cache(request_id);
            return;
        }
        let active = self
            .active
            .remove(request_id)
            .unwrap_or_else(|| ActiveRequest {
                request_id: request_id.to_string(),
                session_id: None,
                conversation: None,
                conversation_parent: None,
                session_seq: None,
                project: None,
                provider: None,
                model: None,
                requested_model: None,
                effective_model: None,
                effort: None,
                endpoint: EndpointKind::Messages,
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                generation_started_at: None,
                generation_initial_output_tokens: 0,
                generation_finished_at: None,
                generation_duration: None,
                status: RequestStatus::Started,
                streamed_bytes: 0,
                stream_chunks: 0,
                input_tokens: None,
                output_tokens: None,
                cache: RequestCache::default(),
                error: None,
                traffic_capture_path: None,
            });
        let numbers = self.ledger.numbers(request_id);
        let completed = CompletedRequest {
            request_id: active.request_id,
            session_id: active.session_id,
            conversation: active.conversation,
            conversation_parent: active.conversation_parent,
            session_seq: active.session_seq,
            project: active.project,
            provider: active.provider,
            model: active.model,
            requested_model: active.requested_model,
            effective_model: active.effective_model,
            effort: active.effort,
            endpoint: active.endpoint,
            started_at: active.started_at,
            finished_at: SystemTime::now(),
            generation_started_at: active.generation_started_at,
            generation_initial_output_tokens: active.generation_initial_output_tokens,
            generation_finished_at: active.generation_finished_at,
            generation_duration: active.generation_duration,
            status,
            http_status,
            latency: active.started_instant.elapsed(),
            streamed_bytes: active.streamed_bytes,
            stream_chunks: active.stream_chunks,
            input_tokens: active.input_tokens,
            output_tokens: active.output_tokens,
            cache: active.cache,
            error: error.or(active.error),
            traffic_capture_path: active.traffic_capture_path,
        };
        self.recent.push_front(completed);
        while self.recent.len() > self.recent_limit {
            self.recent.pop_back();
        }
        if let Some(numbers) = numbers {
            self.project_numbers(request_id, numbers);
        }
        self.evaluate_cache(request_id);
    }

    /// Apply a streaming report at a chosen moment, for tests that need a
    /// request's output to cross an output-history bucket boundary.
    #[cfg(test)]
    fn stream_progress_at(&mut self, request_id: &str, usage: &UsageReport, now: SystemTime) {
        if self
            .ledger
            .note_stream_progress(request_id, usage, now)
            .is_some()
        {
            self.project(request_id);
        }
    }

    /// Copy the numbers the ledger owns onto the request's visible row, which
    /// mirrors them rather than counting anything of its own.
    fn project(&mut self, request_id: &str) {
        if let Some(numbers) = self.ledger.numbers(request_id) {
            self.project_numbers(request_id, numbers);
        }
    }

    /// Mirror the model the client asked for onto the request's visible row,
    /// wherever it has got to.
    fn project_requested_model(&mut self, request_id: &str, model: String) {
        if let Some(active) = self.active.get_mut(request_id) {
            active.requested_model = Some(model);
        } else if let Some(completed) = self
            .recent
            .iter_mut()
            .find(|request| request.request_id == request_id)
        {
            completed.requested_model = Some(model);
        }
    }

    /// The same for the model that went on the wire, together with the display
    /// the two of them make.
    fn project_resolved_model(&mut self, request_id: &str, display: String, model: String) {
        if let Some(active) = self.active.get_mut(request_id) {
            active.effective_model = Some(model);
            active.model = Some(display);
        } else if let Some(completed) = self
            .recent
            .iter_mut()
            .find(|request| request.request_id == request_id)
        {
            completed.effective_model = Some(model);
            completed.model = Some(display);
        }
    }

    fn project_numbers(&mut self, request_id: &str, numbers: accounting::RequestNumbers) {
        if let Some(active) = self.active.get_mut(request_id) {
            active.status = numbers.status;
            active.input_tokens = numbers.input_tokens;
            active.output_tokens = numbers.output_tokens;
            active.cache = numbers.cache;
            active.generation_started_at = numbers.generation_started_at;
            active.generation_initial_output_tokens = numbers.generation_initial_output_tokens;
            active.generation_finished_at = numbers.generation_finished_at;
            active.generation_duration = numbers.generation_duration;
        } else if let Some(completed) = self
            .recent
            .iter_mut()
            .find(|request| request.request_id == request_id)
        {
            completed.input_tokens = numbers.input_tokens;
            completed.output_tokens = numbers.output_tokens;
            completed.cache = numbers.cache;
            completed.generation_started_at = numbers.generation_started_at;
            completed.generation_initial_output_tokens = numbers.generation_initial_output_tokens;
            completed.generation_finished_at = numbers.generation_finished_at;
            completed.generation_duration = numbers.generation_duration;
        }
    }

    /// Once a request's prompt size is the backend's own, make it the latest
    /// prompt of its lane, and where the cached share is final too, compare it
    /// with the previous request of the lane and record a miss.
    ///
    /// The figures come from the ledger, so a request whose counts arrive after
    /// it left the recent list is still judged.
    fn evaluate_cache(&mut self, request_id: &str) {
        let (key, prompt, cache, started_at, response_started_at, judge) = {
            let Some(record) = self.ledger.record(request_id) else {
                return;
            };
            // A request whose tokens belong to no total belongs to no cache
            // lane either: a `count_tokens` estimate counts a prompt the real
            // request counts again, and a request the proxy answered itself
            // reached no backend cache at all. Its exact zeroes are the truth
            // about what it spent and would be a shrunken context and a miss
            // that never happened if the lane took them.
            if record.cache.evaluated || !record.counts_tokens() {
                return;
            }
            // A report naming only part of the prompt sizes none of it: taking
            // its partial sum for the whole would invent a shrunken context and
            // a miss that never happened.
            if !record.prompt_is_measured() {
                return;
            }
            let (Some(provider), Some(model)) = (record.provider.clone(), record.model.clone())
            else {
                return;
            };
            (
                LaneKey {
                    session_id: record.session_id.clone(),
                    conversation: record.conversation.clone(),
                    provider,
                    model,
                },
                record.prompt_tokens().unwrap_or(0),
                record.cache,
                record.started_at,
                record.generation_started_at,
                // A prompt total the backend reported sizes the context without
                // saying how much of it was cached. Judging a miss needs that
                // share, so a request that never named it is not judged.
                record.cache.closed.cache_read,
            )
        };
        let cache_read = cache.read_tokens.unwrap_or(0);
        let reported_cache = cache_read > 0 || cache.write_tokens.unwrap_or(0) > 0;

        let previous = self.lanes.get(&key).copied();
        // A request that started before the lane's baseline finished late; it
        // neither judges nor replaces the newer baseline.
        let superseded = previous.is_some_and(|lane| started_at < lane.started_at);
        let side = key
            .conversation
            .as_deref()
            .is_some_and(|name| name.ends_with(SIDE_CONVERSATION_SUFFIX));
        let judged = judge && !superseded && !side;
        let miss = previous.filter(|_| judged).and_then(|lane| {
            // It was sent before the previous response began, so the entries
            // that response wrote were not readable yet.
            if started_at < lane.readable_from {
                return None;
            }
            // A lane that has never shown cache activity may be below the
            // provider's minimum cacheable length. Codex reports no writes, so
            // its first miss would look the same; it caches without being asked.
            if !reported_cache && !lane.reported_cache && !caches_implicitly(&key.provider) {
                return None;
            }
            let gap = started_at
                .duration_since(lane.started_at)
                .unwrap_or(Duration::ZERO);
            // The previous request's entries decide whether this one could
            // still read them.
            let ttl = lane.ttl.or(cache.ttl).or(default_cache_ttl(&key.provider));
            detect_cache_miss(lane.prompt_tokens, prompt, cache_read, gap, ttl)
        });
        if !superseded {
            self.lanes.insert(
                key.clone(),
                LaneState {
                    prompt_tokens: prompt,
                    started_at,
                    readable_from: response_started_at.unwrap_or_else(SystemTime::now),
                    ttl: cache.ttl.or(previous.and_then(|lane| lane.ttl)),
                    reported_cache: reported_cache
                        || previous.is_some_and(|lane| lane.reported_cache),
                },
            );
        }

        let context_ttl = self
            .lanes
            .get(&key)
            .and_then(|lane| lane.ttl)
            .or(default_cache_ttl(&key.provider));
        // A session shows the main thread's context; a conversation shows its
        // own, side lanes included.
        let session_context = !superseded
            && key
                .conversation
                .as_deref()
                .is_none_or(|name| name == "main");
        if let Some(stats) = self.ledger.session_cache_mut(&key.session_id) {
            if session_context {
                note_context(stats, prompt, started_at, context_ttl);
            }
            if let Some(miss) = miss {
                note_miss(stats, started_at, miss);
            }
        }
        if let Some(conversation) = key.conversation.as_deref()
            && let Some(stats) = self
                .ledger
                .conversation_cache_mut(&key.session_id, conversation)
        {
            if !superseded {
                note_context(stats, prompt, started_at, context_ttl);
            }
            if let Some(miss) = miss {
                note_miss(stats, started_at, miss);
            }
        }

        self.ledger.note_cache_evaluation(request_id, miss);
        self.project(request_id);
    }

    fn snapshot(&self) -> MonitorState {
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active.sort_by_key(|request| request.started_at);
        let mut sessions = session_summaries(&self.ledger);
        apply_window_rate(&mut sessions, &active, &self.recent);
        MonitorState {
            started_at: self.started_at,
            sessions,
            active,
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

/// Where a conversation belongs among its siblings: the main thread leads,
/// side lanes trail, and the rest keep the order they were first seen in.
fn conversation_rank(conversation: &ConversationSummary) -> u8 {
    match conversation.conversation.as_str() {
        _ if conversation.is_side() => 2,
        "main" => 0,
        _ => 1,
    }
}

/// The conversation a row hangs under: the one a side call was made from, else
/// the agent that spawned it. A parent that this session has not been seen
/// talking to, and a row that would be its own ancestor, leave the row at the
/// top level rather than hiding it.
fn resolve_parent(conversations: &[ConversationSummary], index: usize) -> Option<usize> {
    let position = |label: &str| {
        conversations
            .iter()
            .position(|conversation| conversation.conversation == label)
    };
    let conversation = &conversations[index];
    conversation
        .conversation
        .strip_suffix(SIDE_CONVERSATION_SUFFIX)
        .and_then(position)
        .or_else(|| conversation.raw_parent.as_deref().and_then(position))
        .filter(|parent| *parent != index)
}

/// Arrange a session's conversations into display order: a depth-first walk of
/// the spawn tree, ranked within every level, with each row's depth and
/// resolved parent filled in.
fn order_conversations(conversations: Vec<ConversationSummary>) -> Vec<ConversationSummary> {
    let count = conversations.len();
    let mut parents: Vec<Option<usize>> = (0..count)
        .map(|index| resolve_parent(&conversations, index))
        .collect();
    // A conversation that is its own ancestor, or whose chain never reaches a
    // root, is malformed: it hangs under the session instead, so the walk
    // below cannot loop and still shows every row.
    let detached: Vec<bool> = (0..count)
        .map(|index| {
            let mut ancestor = parents[index];
            let mut steps = 0;
            while let Some(parent) = ancestor {
                if parent == index || steps > count {
                    return true;
                }
                ancestor = parents[parent];
                steps += 1;
            }
            false
        })
        .collect();
    for (parent, detached) in parents.iter_mut().zip(detached) {
        if detached {
            *parent = None;
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut roots: Vec<usize> = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        match parent {
            Some(parent) => children[*parent].push(index),
            None => roots.push(index),
        }
    }
    // A stable sort keeps the order the conversations were first seen in
    // within each rank.
    let rank = |index: &usize| conversation_rank(&conversations[*index]);
    roots.sort_by_key(rank);
    for level in &mut children {
        level.sort_by_key(rank);
    }

    let mut ordered: Vec<(usize, usize)> = Vec::with_capacity(count);
    let mut visited = vec![false; count];
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|index| (*index, 0)).collect();
    while let Some((index, depth)) = stack.pop() {
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }
        ordered.push((index, depth));
        for child in children[index].iter().rev() {
            stack.push((*child, depth + 1));
        }
    }
    // Whatever the walk could not reach still gets a row of its own.
    ordered.extend(
        (0..count)
            .filter(|index| !visited[*index])
            .map(|index| (index, 0)),
    );

    let labels: Vec<String> = conversations
        .iter()
        .map(|conversation| conversation.conversation.clone())
        .collect();
    let mut slots: Vec<Option<ConversationSummary>> = conversations.into_iter().map(Some).collect();
    ordered
        .into_iter()
        .map(|(index, depth)| {
            let mut conversation = slots[index].take().expect("each conversation ordered once");
            conversation.depth = depth;
            conversation.parent = parents[index].map(|parent| labels[parent].clone());
            conversation
        })
        .collect()
}

/// The rows of every session the process has served, built from what the
/// ledger kept rather than from the requests the recent list still holds.
///
/// Newest activity first: a session with a request in flight leads, and the
/// rest follow by the time of their latest request, whichever model or
/// conversation of the session made it. Two sessions last seen in the same
/// instant keep the newer one on top.
fn session_summaries(ledger: &Ledger) -> Vec<SessionSummary> {
    let mut out: Vec<_> = ledger
        .sessions()
        .map(|(session_id, record)| session_summary(session_id.clone(), record))
        .collect();
    out.sort_by_key(|session| {
        (
            session.active_count == 0,
            std::cmp::Reverse(session.last_seen),
            std::cmp::Reverse(session.first_seen_rank),
        )
    });
    out
}

fn session_summary(session_id: Option<String>, record: &SessionRecord) -> SessionSummary {
    // The rows are stored by name; their first-seen rank is what keeps the
    // order stable, as the display order builds on it.
    let mut rows: Vec<_> = record.conversations.iter().collect();
    rows.sort_by_key(|(_, row)| row.rank);
    let conversations = rows
        .into_iter()
        .map(|(conversation, row)| conversation_summary(conversation, row))
        .collect();
    SessionSummary {
        session_id,
        project: record.project.clone(),
        first_seen_rank: count(record.rank),
        active_count: count(record.counts.active_count),
        request_count: count(record.counts.request_count),
        failure_count: count(record.counts.failure_count),
        provider: record.provider.clone(),
        model: record.model.clone(),
        effort: record.effort.clone(),
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        input_tokens: record.counts.usage.input_tokens,
        output_tokens: record.counts.usage.output_tokens,
        cache_read_tokens: record.counts.usage.cache_read_tokens,
        cache_write_tokens: record.counts.usage.cache_write_tokens,
        cache_write_5m_tokens: record.counts.usage.cache_write_5m_tokens,
        cache_write_1h_tokens: record.counts.usage.cache_write_1h_tokens,
        evidence: record.counts.evidence,
        cache: record.cache,
        output_token_samples: record
            .output_buckets
            .iter()
            .map(|(bucket, tokens)| (session_token_bucket_start(*bucket), *tokens))
            .collect(),
        // Filled from the bounded active and recent lists, which are what a
        // throughput can honestly be read off.
        rate_output_tokens: 0,
        generation_duration: Duration::ZERO,
        last_status: status_label(record.last_status),
        conversations: order_conversations(conversations),
        unattributed: record.unattributed(),
        models: record.model_usage(),
    }
}

fn conversation_summary(
    conversation: &str,
    record: &accounting::ConversationRecord,
) -> ConversationSummary {
    ConversationSummary {
        conversation: conversation.to_string(),
        parent: None,
        raw_parent: record.parent.clone(),
        first_seen_rank: count(record.rank),
        depth: 0,
        active_count: count(record.counts.active_count),
        request_count: count(record.counts.request_count),
        failure_count: count(record.counts.failure_count),
        provider: record.provider.clone(),
        model: record.model.clone(),
        effort: record.effort.clone(),
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        input_tokens: record.counts.usage.input_tokens,
        output_tokens: record.counts.usage.output_tokens,
        cache_read_tokens: record.counts.usage.cache_read_tokens,
        cache_write_tokens: record.counts.usage.cache_write_tokens,
        cache_write_5m_tokens: record.counts.usage.cache_write_5m_tokens,
        cache_write_1h_tokens: record.counts.usage.cache_write_1h_tokens,
        evidence: record.counts.evidence,
        cache: record.cache,
        last_status: status_label(record.last_status),
    }
}

/// Add the throughput the active and recent requests show to the sessions they
/// belong to.
///
/// A rate is a live measure of what is in view, not a lifetime average: the
/// requests it is made of are the bounded ones the detail lists still hold, so a
/// session whose requests have all left them shows no rate rather than an old
/// one. The rows themselves, their counts and their token totals come from the
/// ledger and owe nothing to this window.
fn apply_window_rate(
    sessions: &mut [SessionSummary],
    active: &[ActiveRequest],
    recent: &VecDeque<CompletedRequest>,
) {
    let rows: HashMap<Option<String>, usize> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.session_id.clone(), index))
        .collect();
    let samples = recent
        .iter()
        .map(|request| {
            (
                &request.session_id,
                request.output_tokens,
                request.generation_initial_output_tokens,
                request.generation_duration,
            )
        })
        .chain(active.iter().map(|request| {
            (
                &request.session_id,
                request.output_tokens,
                request.generation_initial_output_tokens,
                request.generation_duration,
            )
        }));
    for (session_id, output_tokens, initial_output_tokens, duration) in samples {
        let Some(index) = rows.get(session_id).copied() else {
            continue;
        };
        // Output without a measured interval, and an interval without output,
        // say nothing about throughput.
        let (Some(tokens), Some(duration)) = (
            output_tokens
                .and_then(|tokens| tokens.checked_sub(initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            duration.filter(|duration| !duration.is_zero()),
        ) else {
            continue;
        };
        let session = &mut sessions[index];
        session.rate_output_tokens = session.rate_output_tokens.saturating_add(tokens);
        session.generation_duration = session.generation_duration.saturating_add(duration);
    }
}

fn count(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn status_label(status: Option<RequestStatus>) -> String {
    status
        .map(|status| status.label().to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn session_token_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn session_token_bucket_start(bucket: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(bucket.saturating_mul(SESSION_TOKEN_BUCKET_SECS))
}

pub fn throughput(
    output_tokens: Option<u64>,
    streamed_bytes: u64,
    stream_chunks: u64,
    elapsed: Duration,
) -> Throughput {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return Throughput::None;
    }
    if let Some(tokens) = output_tokens.filter(|tokens| *tokens > 0) {
        return Throughput::TokensPerSecond(tokens as f64 / secs);
    }
    if streamed_bytes > 0 {
        return Throughput::BytesPerSecond(streamed_bytes as f64 / secs);
    }
    if stream_chunks > 0 {
        return Throughput::EventsPerSecond(stream_chunks as f64 / secs);
    }
    Throughput::None
}

pub fn usage_from_anthropic_sse(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
    let text = String::from_utf8_lossy(bytes);
    let mut input_tokens = None;
    let mut output_tokens = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
            continue;
        };
        for usage in [
            value.pointer("/usage"),
            value.pointer("/delta/usage"),
            value.pointer("/message/usage"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(tokens) = usage.get("input_tokens").and_then(|value| value.as_u64()) {
                input_tokens = Some(tokens);
            }
            if let Some(tokens) = usage.get("output_tokens").and_then(|value| value.as_u64()) {
                output_tokens = Some(tokens);
            }
        }
    }
    (input_tokens, output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_requests_appear_active() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("s1".to_string()),
            Some(3),
            EndpointKind::Messages,
        );
        let state = monitor.snapshot();
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].request_id, "r1");
        assert_eq!(state.active[0].session_id.as_deref(), Some("s1"));
        assert_eq!(state.active[0].session_seq, Some(3));
    }

    #[test]
    fn resolved_model_appends_to_incoming_alias() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "claude-sonnet-4-6", None);
        monitor.model_resolved("r1", "gpt-5.4");

        let state = monitor.snapshot();
        assert_eq!(
            state.active[0].model.as_deref(),
            Some("claude-sonnet-4-6 → gpt-5.4")
        );
    }

    #[test]
    fn identical_resolved_model_is_shown_once() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("r1", "gpt-5.6-sol");

        let state = monitor.snapshot();
        assert_eq!(state.active[0].model.as_deref(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn compaction_started_marks_request_compacting() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.compaction_started("r1");

        let state = monitor.snapshot();
        assert_eq!(state.active[0].status, RequestStatus::Compacting);
        assert_eq!(state.sessions[0].last_status, "compacting");
    }

    #[test]
    fn generation_baseline_pairs_total_usage_with_the_full_observed_interval() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.generation_started("r1");
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));
        monitor.request_completed("r1", 200, None, None);

        let request = &monitor.snapshot().recent[0];

        assert!(
            request
                .generation_duration
                .is_some_and(|duration| !duration.is_zero())
        );
        assert!(matches!(request.rate(), Throughput::TokensPerSecond(_)));
    }

    #[test]
    fn first_stream_progress_has_no_rate_without_an_interval() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));

        let state = monitor.snapshot();
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].rate(), Throughput::None);
    }

    #[test]
    fn late_stream_progress_extends_stream_timing_without_extending_request_latency() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.stream_progress("r1", 100, 1, Some(0), Some(0));
        monitor.request_completed("r1", 200, None, None);
        let completed = monitor.snapshot().recent[0].clone();
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));

        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent[0].streamed_bytes, 150);
        assert_eq!(state.recent[0].stream_chunks, 2);
        assert_eq!(state.recent[0].input_tokens, Some(1_225));
        assert_eq!(state.recent[0].output_tokens, Some(141));
        assert_eq!(state.recent[0].finished_at, completed.finished_at);
        assert_eq!(state.recent[0].latency, completed.latency);
        assert!(state.recent[0].generation_duration > completed.generation_duration);
        assert!(matches!(
            state.recent[0].rate(),
            Throughput::TokensPerSecond(_)
        ));
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .sum::<u64>(),
            141
        );
    }

    #[test]
    fn completed_requests_leave_active_and_enter_recent() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.5", Some("high".to_string()));
        monitor.request_completed("r1", 200, Some(10), Some(20));
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
        assert_eq!(state.recent[0].effort.as_deref(), Some("high"));
        assert_eq!(state.recent[0].output_tokens, Some(20));
    }

    #[test]
    fn failed_requests_preserve_error_summary() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_failed("r1", Some(400), "Unknown model");
        let state = monitor.snapshot();
        assert_eq!(state.recent[0].status, RequestStatus::Failed);
        assert_eq!(state.recent[0].http_status, Some(400));
        assert_eq!(state.recent[0].error.as_deref(), Some("Unknown model"));
    }

    #[test]
    fn abandoned_requests_leave_active_once() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_abandoned("r1", "request dropped");
        monitor.request_abandoned("r1", "request dropped again");
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].status, RequestStatus::Failed);
        assert_eq!(state.recent[0].http_status, None);
        assert_eq!(state.recent[0].error.as_deref(), Some("request dropped"));
    }

    #[test]
    fn completed_requests_ignore_late_abandonment() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_completed("r1", 200, None, None);
        monitor.request_abandoned("r1", "request dropped");
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].status, RequestStatus::Completed);
    }

    #[test]
    fn bounded_recent_history_drops_oldest() {
        let monitor = MonitorHandle::new(2);
        for id in ["r1", "r2", "r3"] {
            monitor.request_started(id, None, None, EndpointKind::Messages);
            monitor.request_completed(id, 200, None, None);
        }
        let state = monitor.snapshot();
        let ids: Vec<_> = state
            .recent
            .iter()
            .map(|request| request.request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["r3", "r2"]);
    }

    #[test]
    fn throughput_selects_best_available_signal() {
        let elapsed = Duration::from_secs(2);
        assert_eq!(
            throughput(Some(84), 1024, 10, elapsed),
            Throughput::TokensPerSecond(42.0)
        );
        assert_eq!(
            throughput(None, 2048, 10, elapsed),
            Throughput::BytesPerSecond(1024.0)
        );
        assert_eq!(
            throughput(None, 0, 36, elapsed),
            Throughput::EventsPerSecond(18.0)
        );
    }

    #[test]
    fn sse_usage_extracts_final_message_delta_tokens() {
        let sse = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":48}}

"#;
        assert_eq!(usage_from_anthropic_sse(sse), (Some(12), Some(48)));
    }

    fn completed_request(
        request_id: &str,
        session_id: &str,
        output_tokens: u64,
        latency: Duration,
        generation_duration: Option<Duration>,
    ) -> CompletedRequest {
        CompletedRequest {
            request_id: request_id.to_string(),
            session_id: Some(session_id.to_string()),
            conversation: None,
            conversation_parent: None,
            session_seq: None,
            project: None,
            provider: Some("codex".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            requested_model: Some("gpt-5.6-sol".to_string()),
            effective_model: Some("gpt-5.6-sol".to_string()),
            effort: None,
            endpoint: EndpointKind::Messages,
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: SystemTime::UNIX_EPOCH + latency,
            generation_started_at: generation_duration.map(|_| SystemTime::UNIX_EPOCH),
            generation_initial_output_tokens: 0,
            generation_finished_at: generation_duration
                .map(|duration| SystemTime::UNIX_EPOCH + duration),
            generation_duration,
            status: RequestStatus::Completed,
            http_status: Some(200),
            latency,
            streamed_bytes: 0,
            stream_chunks: 0,
            input_tokens: None,
            output_tokens: Some(output_tokens),
            cache: RequestCache::default(),
            error: None,
            traffic_capture_path: None,
        }
    }

    /// Session rows for requests built directly rather than observed, the way
    /// the demo monitor builds them: the ledger counts them and answers for the
    /// rows, so the figures come from the same place as in live traffic.
    fn session_summaries_for_requests(recent: &VecDeque<CompletedRequest>) -> Vec<SessionSummary> {
        let mut ledger = Ledger::default();
        for request in recent.iter().rev() {
            ledger.absorb(AbsorbedRequest::from_completed(request));
        }
        let mut sessions = session_summaries(&ledger);
        apply_window_rate(&mut sessions, &[], recent);
        sessions
    }

    #[test]
    fn completed_request_rate_uses_stream_interval_instead_of_request_latency() {
        let request = completed_request(
            "r1",
            "s1",
            120,
            Duration::from_secs(30),
            Some(Duration::from_secs(4)),
        );

        assert_eq!(request.rate(), Throughput::TokensPerSecond(30.0));
    }

    #[test]
    fn request_rate_uses_token_delta_from_the_initial_observation() {
        let mut request = completed_request(
            "r1",
            "s1",
            120,
            Duration::from_secs(30),
            Some(Duration::from_secs(4)),
        );
        request.generation_initial_output_tokens = 20;

        assert_eq!(request.rate(), Throughput::TokensPerSecond(25.0));
    }

    #[test]
    fn session_rate_combines_request_tokens_and_generation_intervals() {
        let recent = VecDeque::from([
            completed_request(
                "r2",
                "s1",
                50,
                Duration::from_secs(40),
                Some(Duration::from_secs(1)),
            ),
            completed_request(
                "r1",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        let sessions = session_summaries_for_requests(&recent);

        assert_eq!(sessions[0].output_tokens, 150);
        assert_eq!(sessions[0].generation_duration, Duration::from_secs(5));
        assert_eq!(sessions[0].rate(), Throughput::TokensPerSecond(30.0));
    }

    #[test]
    fn output_without_observed_stream_interval_has_no_output_rate() {
        let request = completed_request("r1", "s1", 120, Duration::from_secs(30), None);
        let recent = VecDeque::from([request.clone()]);

        assert_eq!(request.rate(), Throughput::None);
        assert_eq!(
            session_summaries_for_requests(&recent)[0].rate(),
            Throughput::None
        );
    }

    #[test]
    fn session_rate_excludes_interval_without_output_usage() {
        let mut tokenless = completed_request(
            "tokenless",
            "s1",
            0,
            Duration::from_secs(30),
            Some(Duration::from_secs(100)),
        );
        tokenless.output_tokens = None;
        let recent = VecDeque::from([
            tokenless,
            completed_request(
                "measured",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        assert_eq!(
            session_summaries_for_requests(&recent)[0].rate(),
            Throughput::TokensPerSecond(25.0)
        );
    }

    #[test]
    fn session_rate_excludes_output_without_a_matching_stream_interval() {
        let recent = VecDeque::from([
            completed_request("buffered", "s1", 900, Duration::from_secs(30), None),
            completed_request(
                "streamed",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        let session = &session_summaries_for_requests(&recent)[0];

        assert_eq!(session.output_tokens, 1_000);
        assert_eq!(session.rate(), Throughput::TokensPerSecond(25.0));
    }

    #[test]
    fn session_summaries_group_recent_and_active_requests() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("s1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("r1", "example");
        monitor.provider_selected("r1", "codex", "gpt-5.5", None);
        monitor.request_completed("r1", 200, Some(10), Some(20));
        monitor.request_started(
            "r2",
            Some("s1".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.provider_selected("r2", "codex", "gpt-5.5", Some("xhigh".to_string()));
        let state = monitor.snapshot();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].label(), "s1");
        assert_eq!(state.sessions[0].project.as_deref(), Some("example"));
        assert_eq!(state.sessions[0].request_count, 2);
        assert_eq!(state.sessions[0].active_count, 1);
        assert_eq!(state.sessions[0].effort.as_deref(), Some("xhigh"));
        assert_eq!(state.sessions[0].output_tokens, 20);
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .collect::<Vec<_>>(),
            vec![20]
        );
    }

    #[test]
    fn session_output_history_survives_request_eviction() {
        let monitor = MonitorHandle::new(1);
        for (request_id, tokens) in [("oldest", 20), ("newest", 80)] {
            monitor.request_started(
                request_id,
                Some("s1".to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.request_completed(request_id, 200, Some(tokens * 10), Some(tokens));
        }

        let state = monitor.snapshot();

        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.sessions[0].input_tokens, 1_000);
        assert_eq!(state.sessions[0].output_tokens, 100);
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .sum::<u64>(),
            100
        );
    }

    #[test]
    fn session_usage_ignores_decreasing_request_observations() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("s1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.usage_updated("r1", Some(100), Some(20));
        monitor.usage_updated("r1", Some(90), Some(15));
        monitor.usage_updated("r1", Some(120), Some(25));

        let active = monitor.snapshot();
        assert_eq!(active.active[0].input_tokens, Some(120));
        assert_eq!(active.active[0].output_tokens, Some(25));
        assert_eq!(active.sessions[0].input_tokens, 120);
        assert_eq!(active.sessions[0].output_tokens, 25);

        monitor.request_completed("r1", 200, Some(80), Some(10));
        let completed = monitor.snapshot();
        assert_eq!(completed.recent[0].input_tokens, Some(120));
        assert_eq!(completed.recent[0].output_tokens, Some(25));
        assert_eq!(completed.sessions[0].input_tokens, 120);
        assert_eq!(completed.sessions[0].output_tokens, 25);
    }

    #[test]
    fn compaction_preserves_cumulative_session_usage() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "before",
            Some("s1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_completed("before", 200, Some(100), Some(20));
        monitor.request_started(
            "compact",
            Some("s1".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.compaction_started("compact");
        monitor.request_completed("compact", 200, Some(40), Some(10));

        let state = monitor.snapshot();
        assert_eq!(state.sessions[0].input_tokens, 140);
        assert_eq!(state.sessions[0].output_tokens, 30);
    }

    #[test]
    fn session_sequence_restart_preserves_cumulative_usage() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "before",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.session_sequence_resolved("before", 7);
        monitor.request_completed("before", 200, Some(100), Some(20));

        monitor.request_started(
            "after",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.session_sequence_resolved("after", 1);
        monitor.request_completed("after", 200, Some(25), Some(5));

        let state = monitor.snapshot();
        assert_eq!(state.sessions[0].input_tokens, 125);
        assert_eq!(state.sessions[0].output_tokens, 25);
    }

    fn session_labels(state: &MonitorState) -> Vec<String> {
        state.sessions.iter().map(SessionSummary::label).collect()
    }

    fn finish_request(monitor: &MonitorHandle, request_id: &str, session: &str, model: &str) {
        monitor.request_started(
            request_id,
            Some(session.to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected(request_id, "codex", model, None);
        monitor.request_completed(request_id, 200, Some(100), Some(10));
    }

    /// A session with a request in flight leads; the rest follow by their
    /// latest request, whichever model or conversation made it.
    #[test]
    fn sessions_are_ordered_by_latest_activity_with_active_ones_on_top() {
        let monitor = MonitorHandle::new(10);
        finish_request(&monitor, "r1", "session-a", "gpt-5.6-sol");
        std::thread::sleep(Duration::from_millis(2));
        finish_request(&monitor, "r2", "session-b", "gpt-5.6-sol");
        assert_eq!(
            session_labels(&monitor.snapshot()),
            ["session-b", "session-a"]
        );

        // The older session's subagent, on another model, is its newest
        // activity and takes the whole session to the top; its conversations
        // keep their tree order.
        std::thread::sleep(Duration::from_millis(2));
        monitor.request_started(
            "r3",
            Some("session-a".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("r3", "agent-1", None);
        monitor.provider_selected("r3", "codex", "gpt-5.6-terra", None);
        monitor.request_completed("r3", 200, Some(100), Some(10));
        std::thread::sleep(Duration::from_millis(2));
        monitor.request_started(
            "r4",
            Some("session-a".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("r4", "main", None);
        monitor.provider_selected("r4", "codex", "gpt-5.6-sol", None);
        monitor.request_completed("r4", 200, Some(100), Some(10));
        let state = monitor.snapshot();
        assert_eq!(session_labels(&state), ["session-a", "session-b"]);
        assert_eq!(
            state.sessions[0]
                .conversations
                .iter()
                .map(|conversation| conversation.conversation.as_str())
                .collect::<Vec<_>>(),
            ["main", "agent-1"]
        );

        // A request still running keeps its session above one that finished
        // a request more recently.
        std::thread::sleep(Duration::from_millis(2));
        monitor.request_started(
            "r5",
            Some("session-c".to_string()),
            None,
            EndpointKind::Messages,
        );
        std::thread::sleep(Duration::from_millis(2));
        finish_request(&monitor, "r6", "session-b", "gpt-5.6-sol");
        assert_eq!(
            session_labels(&monitor.snapshot()),
            ["session-c", "session-b", "session-a"]
        );
        monitor.request_completed("r5", 200, None, None);
        assert_eq!(
            session_labels(&monitor.snapshot()),
            ["session-c", "session-b", "session-a"]
        );
    }

    fn stats_row<'a>(rows: &'a [ModelStats], provider: &str, model: &str) -> &'a ModelStats {
        rows.iter()
            .find(|row| {
                row.provider.as_deref() == Some(provider) && row.model.as_deref() == Some(model)
            })
            .unwrap_or_else(|| panic!("row {provider}/{model} in {rows:?}"))
    }

    fn finish_with_usage(
        monitor: &MonitorHandle,
        request_id: &str,
        session: &str,
        provider: &str,
        model: &str,
        usage: UsageReport,
    ) {
        monitor.request_started(
            request_id,
            Some(session.to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved(request_id, "main", None);
        monitor.provider_selected(request_id, provider, model, None);
        monitor.model_resolved(request_id, model);
        monitor.usage_reported(request_id, usage);
        monitor.request_completed(request_id, 200, None, None);
    }

    /// One row per backend and model, summed over every session; the hit rate
    /// is the read share of the prompt, and a count no backend reported stays
    /// missing rather than becoming a zero.
    #[test]
    fn model_stats_sum_the_session_rollups_per_backend_and_model() {
        let monitor = MonitorHandle::new(10);
        finish_with_usage(
            &monitor,
            "a1",
            "s1",
            "anthropic",
            "claude-opus-5",
            closing_usage(1_000, 8_000, 1_000, 100),
        );
        finish_with_usage(
            &monitor,
            "a2",
            "s2",
            "anthropic",
            "claude-opus-5",
            closing_usage(3_000, 6_000, 1_000, 300),
        );
        // Codex reports no cache write at all.
        finish_with_usage(
            &monitor,
            "c1",
            "s2",
            "codex",
            "gpt-5.6-sol",
            UsageReport {
                closing: UsageFields {
                    input_tokens: Some(500),
                    cache_read_tokens: Some(1_500),
                    output_tokens: Some(50),
                    ..UsageFields::default()
                },
                ..UsageReport::default()
            },
        );
        let rows = monitor.snapshot().model_stats();
        assert_eq!(rows.len(), 2);

        let opus = stats_row(&rows, "anthropic", "claude-opus-5");
        assert_eq!(opus.request_count, 2);
        assert_eq!(opus.input_tokens, 4_000);
        assert_eq!(opus.cache_read_tokens, 14_000);
        assert_eq!(opus.cache_write_tokens, 2_000);
        assert_eq!(opus.output_tokens, 400);
        assert_eq!(opus.prompt_tokens(), 20_000);
        assert_eq!(opus.cache_hit_ratio(), Some(0.7));
        assert!(opus.evidence.cache_write.is_exact());
        assert_eq!(opus.evidence.cache_write.exact, 2);

        let sol = stats_row(&rows, "codex", "gpt-5.6-sol");
        assert_eq!(sol.prompt_tokens(), 2_000);
        assert_eq!(sol.cache_hit_ratio(), Some(0.75));
        assert_eq!(sol.evidence.cache_write.missing, 1);
        assert_eq!(sol.evidence.cache_write.exact, 0);

        // Largest prompt total first.
        assert_eq!(rows[0].model.as_deref(), Some("claude-opus-5"));
        assert_eq!(rows[1].model.as_deref(), Some("gpt-5.6-sol"));
    }

    /// Misses are tallied on the row of the model that ran the request, with
    /// the cause the lane evaluation gave them.
    #[test]
    fn model_stats_count_misses_per_model_with_their_cause() {
        let monitor = MonitorHandle::new(10);
        // An expiry on the opus lane: idle longer than the reported lifetime.
        start_anthropic_request(&monitor, "o1", "claude-opus-5");
        monitor.model_resolved("o1", "claude-opus-5");
        backdate(&monitor, "o1", Duration::from_secs(2 * 60 * 60));
        monitor.usage_reported(
            "o1",
            UsageReport {
                cache_ttl: Some(Duration::from_secs(60 * 60)),
                ..closing_usage(2, 0, 40_000, 10)
            },
        );
        monitor.request_completed("o1", 200, None, None);
        start_anthropic_request(&monitor, "o2", "claude-opus-5");
        monitor.model_resolved("o2", "claude-opus-5");
        monitor.usage_reported("o2", closing_usage(3, 0, 40_100, 10));
        monitor.request_completed("o2", 200, None, None);

        // A miss within the lifetime on the codex lane of another session.
        finish_with_usage(
            &monitor,
            "c1",
            "s2",
            "codex",
            "gpt-5.6-sol",
            closing_usage(40_000, 0, 0, 10),
        );
        finish_with_usage(
            &monitor,
            "c2",
            "s2",
            "codex",
            "gpt-5.6-sol",
            closing_usage(40_100, 0, 0, 10),
        );

        let rows = monitor.snapshot().model_stats();
        let opus = stats_row(&rows, "anthropic", "claude-opus-5");
        assert_eq!(opus.misses.expired, 1);
        assert_eq!(opus.misses.within_ttl, 0);
        let sol = stats_row(&rows, "codex", "gpt-5.6-sol");
        assert_eq!(sol.misses.within_ttl, 1);
        assert_eq!(sol.misses.expired, 0);
        assert_eq!(sol.misses.total(), 1);
    }

    /// The timing figures are medians over the completed requests still in the
    /// recent list, per row.
    #[test]
    fn model_stats_take_median_latency_and_rate_from_the_recent_window() {
        let recent: VecDeque<CompletedRequest> = [
            completed_request(
                "r1",
                "s1",
                100,
                Duration::from_secs(1),
                Some(Duration::from_secs(1)),
            ),
            completed_request(
                "r2",
                "s1",
                300,
                Duration::from_secs(5),
                Some(Duration::from_secs(1)),
            ),
            completed_request(
                "r3",
                "s2",
                200,
                Duration::from_secs(3),
                Some(Duration::from_secs(1)),
            ),
            completed_request("r4", "s2", 50, Duration::from_secs(9), None),
        ]
        .into_iter()
        .collect();
        let state = MonitorState {
            started_at: SystemTime::UNIX_EPOCH,
            sessions: session_summaries_for_requests(&recent),
            active: Vec::new(),
            recent: recent.into_iter().collect(),
        };
        let rows = state.model_stats();
        let sol = stats_row(&rows, "codex", "gpt-5.6-sol");
        assert_eq!(sol.request_count, 4);
        assert_eq!(sol.recent_requests, 4);
        // Latencies 1, 3, 5, 9: the mean of the two middle values.
        assert_eq!(sol.median_latency, Some(Duration::from_secs(4)));
        // Rates 100, 300, 200 tok/s; the request without an interval has none.
        assert_eq!(sol.median_output_rate, Some(200.0));
    }

    /// An even-sized sample has two middle values, and its median is their
    /// mean, for the latency and the output rate alike.
    #[test]
    fn model_stats_medians_of_an_even_sample_average_the_two_middle_values() {
        let recent: VecDeque<CompletedRequest> = [
            completed_request(
                "r1",
                "s1",
                100,
                Duration::from_secs(1),
                Some(Duration::from_secs(1)),
            ),
            completed_request(
                "r2",
                "s1",
                300,
                Duration::from_secs(9),
                Some(Duration::from_secs(1)),
            ),
        ]
        .into_iter()
        .collect();
        let state = MonitorState {
            started_at: SystemTime::UNIX_EPOCH,
            sessions: session_summaries_for_requests(&recent),
            active: Vec::new(),
            recent: recent.into_iter().collect(),
        };
        let rows = state.model_stats();
        let sol = stats_row(&rows, "codex", "gpt-5.6-sol");
        assert_eq!(sol.recent_requests, 2);
        assert_eq!(sol.median_latency, Some(Duration::from_secs(5)));
        assert_eq!(sol.median_output_rate, Some(200.0));
    }

    fn closing_usage(input: u64, read: u64, write: u64, output: u64) -> UsageReport {
        UsageReport {
            closing: UsageFields {
                input_tokens: Some(input),
                cache_read_tokens: Some(read),
                cache_write_tokens: Some(write),
                output_tokens: Some(output),
                // A report that does not split the write by lifetime leaves
                // both buckets unknown.
                ..UsageFields::default()
            },
            ..UsageReport::default()
        }
    }

    fn start_codex_request(monitor: &MonitorHandle, request_id: &str, conversation: &str) {
        start_codex_subagent_request(monitor, request_id, conversation, None);
    }

    fn start_codex_subagent_request(
        monitor: &MonitorHandle,
        request_id: &str,
        conversation: &str,
        parent: Option<&str>,
    ) {
        monitor.request_started(
            request_id,
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved(request_id, conversation, parent.map(str::to_string));
        monitor.provider_selected(request_id, "codex", "gpt-5.6-sol", None);
    }

    fn recent_by_id<'a>(state: &'a MonitorState, request_id: &str) -> &'a CompletedRequest {
        state
            .recent
            .iter()
            .find(|request| request.request_id == request_id)
            .expect("request in recent list")
    }

    #[test]
    fn translated_stream_estimate_is_replaced_by_final_usage() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        // The live path hands the response to the client before the stream ends.
        monitor.request_completed("r1", 200, None, None);
        monitor.stream_progress_usage(
            "r1",
            200,
            1,
            usage_report_from_anthropic_sse(
                br#"data: {"type":"message_start","message":{"usage":{"input_tokens":31066,"output_tokens":0}}}
"#,
            ),
        );
        let estimate = monitor.snapshot();
        assert_eq!(estimate.recent[0].input_tokens, Some(31_066));
        assert_eq!(estimate.sessions[0].input_tokens, 31_066);

        monitor.stream_progress_usage(
            "r1",
            300,
            1,
            usage_report_from_anthropic_sse(
                br#"data: {"type":"message_delta","usage":{"input_tokens":2906,"cache_read_input_tokens":28160,"cache_creation_input_tokens":0,"output_tokens":117}}
"#,
            ),
        );
        let state = monitor.snapshot();
        let request = &state.recent[0];
        assert_eq!(request.input_tokens, Some(2_906));
        assert_eq!(request.cache.read_tokens, Some(28_160));
        assert_eq!(request.cache.write_tokens, Some(0));
        assert_eq!(request.output_tokens, Some(117));
        assert_eq!(request.prompt_tokens(), Some(31_066));
        let ratio = request.cache_hit_ratio().unwrap();
        assert!((ratio - 28_160.0 / 31_066.0).abs() < 1e-9);

        let session = &state.sessions[0];
        assert_eq!(session.input_tokens, 2_906);
        assert_eq!(session.cache_read_tokens, 28_160);
        assert_eq!(session.cache_write_tokens, 0);
        assert_eq!(session.output_tokens, 117);
        assert_eq!(session.cache.context_tokens, 31_066);

        // A late opening value cannot undo the final count.
        monitor.stream_progress("r1", 10, 1, Some(40_000), Some(1));
        let late = monitor.snapshot();
        assert_eq!(late.recent[0].input_tokens, Some(2_906));
        assert_eq!(late.sessions[0].input_tokens, 2_906);
    }

    #[test]
    fn a_stream_that_failed_leaves_its_opening_counts_provisional() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.stream_progress_usage(
            "r1",
            200,
            1,
            usage_report_from_anthropic_sse(
                br#"data: {"type":"message_start","message":{"usage":{"input_tokens":341974,"output_tokens":0}}}
"#,
            ),
        );
        monitor.request_failed("r1", Some(502), "stream ended before the final usage");

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.status, RequestStatus::Failed);
        assert_eq!(request.input_tokens, Some(341_974));
        assert_eq!(request.output_tokens, Some(0));
        // The prompt estimate stays an estimate, the output zero is the one the
        // stream opened with rather than a final count, and the cache counts the
        // backend never sent are absent instead of zero.
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Opening,
                cache_read: UsageQuality::Missing,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Opening,
            }
        );
        assert_eq!(request.cache.read_tokens, None);
        assert_eq!(request.cache.write_tokens, None);
        assert_eq!(state.sessions[0].input_tokens, 341_974);
        // The session's total says as much about its evidence as the request
        // does: one provisional prompt and cache counts nobody reported.
        assert_eq!(
            state.sessions[0].evidence.input,
            QualityCoverage {
                missing: 0,
                opening: 1,
                exact: 0
            }
        );
        assert_eq!(state.sessions[0].evidence.cache_read.missing, 1);
        assert_eq!(state.sessions[0].failure_count, 1);
    }

    #[test]
    fn anthropic_prompt_counts_are_exact_while_the_live_output_is_provisional() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        let mut report = UsageReport::default();
        report.add_event(
            &serde_json::json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 2, "cache_read_input_tokens": 10_126,
                "cache_creation_input_tokens": 22_405, "output_tokens": 3
            }}}),
            true,
        );
        monitor.stream_progress_usage("r1", 100, 1, report);

        let streaming = monitor.snapshot();
        assert_eq!(
            streaming.active[0].usage_quality(),
            QualityFields {
                input: UsageQuality::Exact,
                cache_read: UsageQuality::Exact,
                cache_write: UsageQuality::Exact,
                output: UsageQuality::Opening,
            }
        );
        assert_eq!(streaming.active[0].output_tokens, Some(3));

        let mut delta = UsageReport::default();
        delta.add_event(
            &serde_json::json!({"type": "message_delta", "usage": {"output_tokens": 120}}),
            true,
        );
        monitor.stream_progress_usage("r1", 200, 1, delta);
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.output_tokens, Some(120));
        assert_eq!(request.prompt_tokens(), Some(32_533));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Exact,
                cache_read: UsageQuality::Exact,
                cache_write: UsageQuality::Exact,
                output: UsageQuality::Exact,
            }
        );
        assert_eq!(state.sessions[0].output_tokens, 120);
    }

    #[test]
    fn a_correction_below_the_estimate_is_exact_and_a_late_estimate_cannot_reopen_it() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.stream_progress("r1", 100, 1, Some(31_066), Some(0));
        let streaming = monitor.snapshot();
        assert_eq!(
            streaming.active[0].usage_quality(),
            QualityFields {
                input: UsageQuality::Opening,
                cache_read: UsageQuality::Missing,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Opening,
            }
        );

        monitor.stream_progress_usage("r1", 200, 1, closing_usage(2_906, 28_160, 0, 117));
        monitor.request_completed("r1", 200, None, None);
        // A late estimate arriving after the final counts changes neither the
        // numbers nor their quality.
        monitor.stream_progress("r1", 10, 1, Some(40_000), Some(200));

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.input_tokens, Some(2_906));
        assert_eq!(request.cache.write_tokens, Some(0));
        assert_eq!(request.output_tokens, Some(117));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Exact,
                cache_read: UsageQuality::Exact,
                cache_write: UsageQuality::Exact,
                output: UsageQuality::Exact,
            }
        );
        assert_eq!(state.sessions[0].input_tokens, 2_906);
        assert_eq!(state.sessions[0].cache_write_tokens, 0);
    }

    /// A report carrying only the backend's prompt total, the way the Codex
    /// backend answers when it does not name the cached part.
    fn total_only_usage(prompt: u64, output: u64) -> UsageReport {
        UsageReport {
            closing: UsageFields {
                output_tokens: Some(output),
                ..UsageFields::default()
            },
            reported_prompt_tokens: Some(prompt),
            ..UsageReport::default()
        }
    }

    #[test]
    fn a_prompt_total_without_a_cache_split_sizes_the_prompt_but_pins_no_half() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.stream_progress("r1", 100, 1, Some(341_974), Some(0));
        monitor.stream_progress_usage("r1", 200, 1, total_only_usage(5_000, 7));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        // The prompt is the number the backend measured, not the estimate the
        // stream opened with and not a sum of halves nobody reported.
        assert_eq!(request.cache.reported_prompt_tokens, Some(5_000));
        assert_eq!(request.prompt_tokens(), Some(5_000));
        assert_eq!(request.output_tokens, Some(7));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Opening,
                cache_read: UsageQuality::Missing,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Exact,
            }
        );
        // The uncached share is unknown, so there is no hit ratio to show.
        assert_eq!(request.cache.read_tokens, None);
        assert_eq!(request.cache_hit_ratio(), None);

        // A late estimate cannot rewrite a measured total.
        monitor.stream_progress("r1", 10, 1, Some(999_999), Some(9));
        let state = monitor.snapshot();
        assert_eq!(
            recent_by_id(&state, "r1").cache.reported_prompt_tokens,
            Some(5_000)
        );
        assert_eq!(recent_by_id(&state, "r1").prompt_tokens(), Some(5_000));
    }

    #[test]
    fn a_buffered_total_without_a_cache_split_leaves_every_category_missing() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        // The non-streaming path reports once, with no opening estimate at all.
        monitor.usage_reported("r1", total_only_usage(5_000, 7));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.input_tokens, None);
        assert_eq!(request.prompt_tokens(), Some(5_000));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Missing,
                cache_read: UsageQuality::Missing,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Exact,
            }
        );
    }

    #[test]
    fn a_reported_cache_split_and_its_total_count_the_same_tokens_once() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(4_000);
        report.closing.cache_read_tokens = Some(1_000);
        report.closing.output_tokens = Some(7);
        report.reported_prompt_tokens = Some(5_000);
        monitor.stream_progress_usage("r1", 200, 1, report);
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.prompt_tokens(), Some(5_000));
        assert_eq!(request.cache_hit_ratio(), Some(0.2));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Exact,
                cache_read: UsageQuality::Exact,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Exact,
            }
        );
        // The total is not a fifth count: the session sees the two categories.
        assert_eq!(state.sessions[0].input_tokens, 4_000);
        assert_eq!(state.sessions[0].cache_read_tokens, 1_000);
    }

    #[test]
    fn a_completed_status_alone_does_not_make_a_count_exact() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_updated("r1", Some(12_000), Some(30));
        monitor.request_completed("r1", 200, Some(12_500), Some(40));

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.status, RequestStatus::Completed);
        assert_eq!(request.http_status, Some(200));
        assert_eq!(request.input_tokens, Some(12_500));
        assert_eq!(request.output_tokens, Some(40));
        assert_eq!(
            request.usage_quality(),
            QualityFields {
                input: UsageQuality::Opening,
                cache_read: UsageQuality::Missing,
                cache_write: UsageQuality::Missing,
                output: UsageQuality::Opening,
            }
        );
        assert_eq!(state.sessions[0].input_tokens, 12_500);
        assert_eq!(state.sessions[0].output_tokens, 40);
    }

    #[test]
    fn count_tokens_estimates_stay_out_of_session_totals() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "count",
            Some("s1".to_string()),
            None,
            EndpointKind::CountTokens,
        );
        monitor.provider_selected("count", "codex", "gpt-5.6-sol", None);
        monitor.usage_updated("count", Some(40_000), None);
        monitor.request_completed("count", 200, None, None);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(100, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(recent_by_id(&state, "count").input_tokens, Some(40_000));
        assert!(!recent_by_id(&state, "count").cache.evaluated());
        assert_eq!(state.sessions[0].input_tokens, 100);
        assert_eq!(state.sessions[0].output_tokens, 10);
    }

    #[test]
    fn cache_misses_are_judged_within_a_conversation_lane() {
        let monitor = MonitorHandle::new(20);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(30_000, 0, 0, 50));
        monitor.request_completed("r1", 200, None, None);

        start_codex_request(&monitor, "r2", "main");
        monitor.usage_reported("r2", closing_usage(900, 30_000, 0, 50));
        monitor.request_completed("r2", 200, None, None);

        // A subagent in the same session builds its own cache; its first
        // request is not compared with the main thread.
        start_codex_request(&monitor, "a1", "agent-1");
        monitor.usage_reported("a1", closing_usage(5_000, 0, 0, 20));
        monitor.request_completed("a1", 200, None, None);

        // The main thread comes back and nothing of its prefix is cached.
        start_codex_request(&monitor, "r3", "main");
        monitor.usage_reported("r3", closing_usage(31_000, 0, 0, 40));
        monitor.request_completed("r3", 200, None, None);

        let state = monitor.snapshot();
        assert!(recent_by_id(&state, "r1").cache.miss.is_none());
        assert!(recent_by_id(&state, "r1").cache.evaluated());
        assert!(recent_by_id(&state, "r2").cache.miss.is_none());
        assert!(recent_by_id(&state, "a1").cache.miss.is_none());
        let miss = recent_by_id(&state, "r3").cache.miss.expect("miss");
        assert_eq!(miss.expected_tokens, 30_900);
        assert_eq!(miss.missed_tokens, 30_900);
        assert_eq!(miss.cause, CacheMissCause::WithinTtl);
        assert_eq!(miss.ttl, Some(usage::CODEX_CACHE_TTL));

        let session = &state.sessions[0];
        assert_eq!(session.cache.miss_count, 1);
        assert_eq!(session.cache.missed_tokens, 30_900);
        assert_eq!(session.cache.context_tokens, 31_000);
        assert_eq!(session.cache.peak_context_tokens, 31_000);
        assert_eq!(session.cache.last_miss.map(|(_, miss)| miss), Some(miss));

        // The miss belongs to the conversation it happened in, not to the
        // subagent that ran in between.
        let main = conversation(session, "main");
        assert_eq!(main.cache.miss_count, 1);
        assert_eq!(main.cache.missed_tokens, 30_900);
        assert_eq!(main.cache.last_miss.map(|(_, miss)| miss), Some(miss));
        assert_eq!(conversation(session, "agent-1").cache.miss_count, 0);
    }

    fn conversation<'a>(session: &'a SessionSummary, label: &str) -> &'a ConversationSummary {
        session
            .conversations
            .iter()
            .find(|conversation| conversation.conversation == label)
            .expect("conversation row")
    }

    #[test]
    fn conversations_of_a_session_accumulate_separately_and_add_up_to_it() {
        let monitor = MonitorHandle::new(20);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(30_000, 0, 0, 50));
        monitor.request_completed("r1", 200, None, None);

        // A subagent of the same session, on its own model.
        let start_agent_request = |request_id: &str| {
            monitor.request_started(
                request_id,
                Some("s1".to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.conversation_resolved(request_id, "agent-1", None);
            monitor.provider_selected(request_id, "codex", "gpt-5.6-luna", None);
        };
        start_agent_request("a1");
        monitor.usage_reported("a1", closing_usage(5_000, 1_000, 200, 20));
        monitor.request_completed("a1", 200, None, None);
        start_agent_request("a2");
        monitor.usage_reported("a2", closing_usage(900, 6_000, 0, 30));
        monitor.request_completed("a2", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let main = conversation(session, "main");
        let agent = conversation(session, "agent-1");

        assert_eq!(main.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(agent.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(main.request_count, 1);
        assert_eq!(agent.request_count, 2);
        assert_eq!(main.input_tokens, 30_000);
        assert_eq!(agent.input_tokens, 5_900);
        assert_eq!(agent.cache_read_tokens, 7_000);
        assert_eq!(agent.cache_write_tokens, 200);
        assert_eq!(agent.output_tokens, 50);
        // Each conversation keeps its own context, not the session's.
        assert_eq!(main.cache.context_tokens, 30_000);
        assert_eq!(agent.cache.context_tokens, 6_900);
        assert_eq!(session.cache.context_tokens, 30_000);
        assert_eq!(
            agent.cache_hit_ratio(),
            Some(7_000.0 / (5_900.0 + 7_000.0 + 200.0))
        );

        assert_eq!(
            session.request_count,
            main.request_count + agent.request_count
        );
        assert_eq!(session.input_tokens, main.input_tokens + agent.input_tokens);
        assert_eq!(
            session.output_tokens,
            main.output_tokens + agent.output_tokens
        );
        assert_eq!(
            session.cache_read_tokens,
            main.cache_read_tokens + agent.cache_read_tokens
        );
        assert_eq!(
            session.cache_write_tokens,
            main.cache_write_tokens + agent.cache_write_tokens
        );
    }

    fn conversation_shape(session: &SessionSummary) -> Vec<(&str, Option<&str>, usize)> {
        session
            .conversations
            .iter()
            .map(|conversation| {
                (
                    conversation.conversation.as_str(),
                    conversation.parent.as_deref(),
                    conversation.depth,
                )
            })
            .collect()
    }

    #[test]
    fn conversation_rows_lead_with_main_and_trail_with_side_calls() {
        let monitor = MonitorHandle::new(20);
        for (request_id, conversation) in [
            ("w1", "main/side"),
            ("a1", "agent-1"),
            ("r1", "main"),
            ("t1", "agent-1/side"),
        ] {
            start_codex_request(&monitor, request_id, conversation);
            monitor.usage_reported(request_id, closing_usage(1_000, 0, 0, 10));
            monitor.request_completed(request_id, 200, None, None);
        }

        let state = monitor.snapshot();

        // Main leads its level, a side call hangs under the conversation that
        // made it, and later arrivals keep the order they were first seen in.
        assert_eq!(
            conversation_shape(&state.sessions[0]),
            vec![
                ("main", None, 0),
                ("main/side", Some("main"), 1),
                ("agent-1", None, 0),
                ("agent-1/side", Some("agent-1"), 1),
            ]
        );
    }

    #[test]
    fn subagents_of_subagents_nest_under_the_agent_that_spawned_them() {
        let monitor = MonitorHandle::new(20);
        let request = |request_id: &str, conversation: &str, parent: Option<&str>| {
            start_codex_subagent_request(&monitor, request_id, conversation, parent);
            monitor.usage_reported(request_id, closing_usage(1_000, 0, 0, 10));
            monitor.request_completed(request_id, 200, None, None);
        };
        request("d1", "agent-deep", Some("agent-mid"));
        request("m1", "agent-mid", Some("agent-top"));
        request("t1", "agent-top", None);
        request("r1", "main", None);
        request("s1", "agent-deep/side", Some("agent-mid"));

        let state = monitor.snapshot();

        assert_eq!(
            conversation_shape(&state.sessions[0]),
            vec![
                ("main", None, 0),
                ("agent-top", None, 0),
                ("agent-mid", Some("agent-top"), 1),
                ("agent-deep", Some("agent-mid"), 2),
                ("agent-deep/side", Some("agent-deep"), 3),
            ]
        );
    }

    #[test]
    fn an_unknown_or_looping_parent_leaves_the_row_under_the_session() {
        let monitor = MonitorHandle::new(20);
        let request = |request_id: &str, conversation: &str, parent: Option<&str>| {
            start_codex_subagent_request(&monitor, request_id, conversation, parent);
            monitor.usage_reported(request_id, closing_usage(1_000, 0, 0, 10));
            monitor.request_completed(request_id, 200, None, None);
        };
        // A parent this session never talked to, and a pair that claims each
        // other.
        request("o1", "agent-orphan", Some("agent-never-seen"));
        request("l1", "agent-loop-a", Some("agent-loop-b"));
        request("l2", "agent-loop-b", Some("agent-loop-a"));

        let state = monitor.snapshot();

        assert_eq!(
            conversation_shape(&state.sessions[0]),
            vec![
                ("agent-orphan", None, 0),
                ("agent-loop-a", None, 0),
                ("agent-loop-b", None, 0),
            ]
        );
    }

    #[test]
    fn a_request_without_a_conversation_counts_for_its_session_only() {
        let monitor = MonitorHandle::new(20);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.usage_reported("r1", closing_usage(700, 0, 0, 5));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();

        assert_eq!(state.sessions[0].input_tokens, 700);
        assert_eq!(state.sessions[0].request_count, 1);
        assert!(state.sessions[0].conversations.is_empty());
    }

    #[test]
    fn idle_gap_beyond_the_reported_ttl_is_an_expiry() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r1", "anthropic", "claude-opus-5", None);
        backdate(&monitor, "r1", Duration::from_secs(2 * 60 * 60));
        monitor.usage_reported(
            "r1",
            UsageReport {
                cache_ttl: Some(Duration::from_secs(60 * 60)),
                ..closing_usage(2, 0, 40_000, 10)
            },
        );
        monitor.request_completed("r1", 200, None, None);

        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r2", "anthropic", "claude-opus-5", None);
        monitor.usage_reported("r2", closing_usage(3, 0, 40_100, 10));
        monitor.request_completed("r2", 200, None, None);

        let state = monitor.snapshot();
        let miss = recent_by_id(&state, "r2").cache.miss.expect("miss");
        assert_eq!(miss.cause, CacheMissCause::Expired);
        assert_eq!(miss.ttl, Some(Duration::from_secs(60 * 60)));
        assert!(miss.gap >= Duration::from_secs(2 * 60 * 60 - 5));
        assert_eq!(state.sessions[0].cache_write_tokens, 80_100);
        assert_eq!(state.sessions[0].cache_hit_ratio(), Some(0.0));
        assert_eq!(
            state.sessions[0].cache.context_ttl,
            Some(Duration::from_secs(60 * 60))
        );
    }

    fn start_anthropic_request(monitor: &MonitorHandle, request_id: &str, model: &str) {
        monitor.request_started(
            request_id,
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved(request_id, "main", None);
        monitor.provider_selected(request_id, "anthropic", model, None);
    }

    fn backdate(monitor: &MonitorHandle, request_id: &str, ago: Duration) {
        let started_at = SystemTime::now() - ago;
        if let Ok(mut store) = monitor.store.lock() {
            // The ledger holds the start the lane comparison reads; the visible
            // row mirrors it.
            store.ledger.backdate_for_tests(request_id, started_at);
            if let Some(active) = store.active.get_mut(request_id) {
                active.started_at = started_at;
            }
        }
    }

    #[test]
    fn side_calls_are_not_judged_and_leave_the_context_alone() {
        let monitor = MonitorHandle::new(20);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(40_000, 0, 0, 50));
        monitor.request_completed("r1", 200, None, None);

        // Two isolated web search calls on the same model: different queries
        // behind a short shared system prompt.
        for id in ["w1", "w2"] {
            start_codex_request(&monitor, id, "main/side");
            monitor.usage_reported(id, closing_usage(3_000, 0, 0, 20));
            monitor.request_completed(id, 200, None, None);
        }

        let state = monitor.snapshot();
        assert!(recent_by_id(&state, "w2").cache.evaluated());
        assert!(recent_by_id(&state, "w2").cache.miss.is_none());
        assert_eq!(state.sessions[0].cache.miss_count, 0);
        assert_eq!(state.sessions[0].cache.context_tokens, 40_000);
    }

    #[test]
    fn a_request_sent_before_the_previous_response_began_is_not_a_miss() {
        let monitor = MonitorHandle::new(20);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        backdate(&monitor, "r1", Duration::from_secs(20));
        start_anthropic_request(&monitor, "r2", "claude-opus-5");
        backdate(&monitor, "r2", Duration::from_secs(10));
        // r1's response begins after r2 was sent, so r2 could not read what
        // r1 wrote and writes the same prefix again.
        monitor.stream_progress_usage("r1", 100, 1, closing_usage(2, 0, 30_000, 1));
        monitor.request_completed("r1", 200, None, None);
        monitor.stream_progress_usage("r2", 100, 1, closing_usage(2, 0, 30_000, 1));
        monitor.request_completed("r2", 200, None, None);

        let state = monitor.snapshot();
        assert!(recent_by_id(&state, "r2").cache.evaluated());
        assert!(recent_by_id(&state, "r2").cache.miss.is_none());
    }

    #[test]
    fn an_older_request_finishing_late_keeps_the_newer_baseline() {
        let monitor = MonitorHandle::new(20);
        start_codex_request(&monitor, "r1", "main");
        backdate(&monitor, "r1", Duration::from_secs(20));
        start_codex_request(&monitor, "r2", "main");
        backdate(&monitor, "r2", Duration::from_secs(10));
        monitor.usage_reported("r2", closing_usage(40_000, 0, 0, 10));
        monitor.request_completed("r2", 200, None, None);
        monitor.usage_reported("r1", closing_usage(50_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        assert_eq!(monitor.snapshot().sessions[0].cache.context_tokens, 40_000);

        // Against r2's 40k this request read everything it could; against
        // r1's 50k it would have missed 10k.
        start_codex_request(&monitor, "r3", "main");
        monitor.usage_reported("r3", closing_usage(20_000, 40_000, 0, 10));
        monitor.request_completed("r3", 200, None, None);

        let state = monitor.snapshot();
        assert!(recent_by_id(&state, "r1").cache.miss.is_none());
        assert!(recent_by_id(&state, "r3").cache.miss.is_none());
        assert_eq!(state.sessions[0].cache.miss_count, 0);
        assert_eq!(state.sessions[0].cache.context_tokens, 60_000);
    }

    #[test]
    fn a_lane_that_never_cached_is_judged_only_where_caching_is_implicit() {
        let monitor = MonitorHandle::new(20);
        // A short Anthropic prompt below the model's minimum cacheable length.
        for id in ["h1", "h2"] {
            start_anthropic_request(&monitor, id, "claude-haiku-4-5");
            monitor.usage_reported(id, closing_usage(3_000, 0, 0, 10));
            monitor.request_completed(id, 200, None, None);
        }
        // Codex never reports writes, so a zero read after a first request of
        // the same prefix is a miss.
        for id in ["c1", "c2"] {
            start_codex_request(&monitor, id, "main");
            monitor.usage_reported(id, closing_usage(3_000, 0, 0, 10));
            monitor.request_completed(id, 200, None, None);
        }

        let state = monitor.snapshot();
        assert!(recent_by_id(&state, "h2").cache.evaluated());
        assert!(recent_by_id(&state, "h2").cache.miss.is_none());
        assert_eq!(
            recent_by_id(&state, "c2")
                .cache
                .miss
                .map(|miss| miss.missed_tokens),
            Some(3_000)
        );
    }

    #[test]
    fn a_late_closing_report_corrects_a_request_the_recent_list_has_dropped() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "a", "main");
        // The live path hands the response to the client while the stream runs,
        // so only the translator's prompt estimate is in yet.
        monitor.stream_progress("a", 100, 1, Some(31_066), Some(0));
        monitor.request_completed("a", 200, None, None);
        start_codex_request(&monitor, "b", "agent-1");
        monitor.usage_reported("b", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("b", 200, None, None);

        let evicted = monitor.snapshot();
        assert_eq!(evicted.recent.len(), 1);
        assert_eq!(evicted.recent[0].request_id, "b");
        assert_eq!(evicted.sessions[0].input_tokens, 32_066);

        // The backend's own counts arrive after the request lost its row.
        monitor.stream_progress_usage("a", 50, 1, closing_usage(2_906, 28_160, 0, 117));

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.input_tokens, 3_906);
        assert_eq!(session.cache_read_tokens, 28_160);
        assert_eq!(session.cache_write_tokens, 0);
        assert_eq!(session.output_tokens, 127);
        assert_eq!(session.request_count, 2);
        // The correction belongs to the conversation the evicted request ran in.
        let main = conversation(session, "main");
        assert_eq!(main.request_count, 1);
        assert_eq!(main.input_tokens, 2_906);
        assert_eq!(main.cache_read_tokens, 28_160);
        assert_eq!(main.output_tokens, 117);
        assert_eq!(conversation(session, "agent-1").input_tokens, 1_000);
        // Judging the prompt cache also still works on an evicted request.
        assert_eq!(main.cache.context_tokens, 31_066);
        assert_eq!(session.cache.context_tokens, 31_066);
    }

    #[test]
    fn session_and_conversation_rows_outlive_the_recent_window() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "r1", "main");
        monitor.project_resolved("r1", "example");
        monitor.usage_reported("r1", closing_usage(30_000, 0, 0, 50));
        monitor.request_completed("r1", 200, None, None);
        // A subagent of the main thread, whose stream died after the status line.
        start_codex_subagent_request(&monitor, "a1", "agent-1", Some("main"));
        monitor.usage_reported("a1", closing_usage(5_000, 1_000, 0, 20));
        monitor.request_failed("a1", Some(200), "stream ended before its terminal event");
        // Another Claude Code process, whose rows stay its own.
        monitor.request_started("o1", Some("s2".to_string()), None, EndpointKind::Messages);
        monitor.conversation_resolved("o1", "main", None);
        monitor.provider_selected("o1", "anthropic", "claude-opus-5", None);
        monitor.usage_reported("o1", closing_usage(700, 0, 0, 5));
        monitor.request_completed("o1", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.sessions.len(), 2);

        // The other process finished last, so its session sits on top.
        assert_eq!(session_labels(&state), ["s2", "s1"]);
        let first = &state.sessions[1];
        assert_eq!(first.label(), "s1");
        assert_eq!(first.project.as_deref(), Some("example"));
        assert_eq!(first.request_count, 2);
        assert_eq!(first.failure_count, 1);
        assert_eq!(first.active_count, 0);
        assert_eq!(first.input_tokens, 35_000);
        assert_eq!(first.cache_read_tokens, 1_000);
        assert_eq!(first.output_tokens, 70);
        assert_eq!(
            conversation_shape(first),
            vec![("main", None, 0), ("agent-1", Some("main"), 1)]
        );
        let agent = conversation(first, "agent-1");
        assert_eq!(agent.request_count, 1);
        assert_eq!(agent.failure_count, 1);
        assert_eq!(agent.input_tokens, 5_000);
        assert_eq!(agent.cache_read_tokens, 1_000);
        assert_eq!(conversation(first, "main").request_count, 1);

        let second = &state.sessions[0];
        assert_eq!(second.label(), "s2");
        assert_eq!(second.request_count, 1);
        assert_eq!(second.input_tokens, 700);
        assert_eq!(conversation_shape(second), vec![("main", None, 0)]);
    }

    #[test]
    fn a_late_correction_below_the_estimate_takes_tokens_back_off_an_evicted_request() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "a", "main");
        monitor.stream_progress("a", 100, 1, Some(31_066), Some(400));
        monitor.request_completed("a", 200, None, None);
        start_codex_request(&monitor, "b", "main");
        monitor.usage_reported("b", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("b", 200, None, None);

        let evicted = monitor.snapshot();
        assert_eq!(evicted.recent.len(), 1);
        assert_eq!(evicted.sessions[0].input_tokens, 32_066);
        assert_eq!(evicted.sessions[0].output_tokens, 410);
        assert_eq!(output_history(&evicted.sessions[0]), 410);

        // The backend's own counts are lower than the estimate the stream
        // opened with.
        monitor.usage_reported("a", closing_usage(2_906, 28_160, 0, 117));

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.input_tokens, 3_906);
        assert_eq!(session.cache_read_tokens, 28_160);
        assert_eq!(session.output_tokens, 127);
        // The output history gives the correction back too, and only from the
        // tokens the corrected request put there.
        assert_eq!(output_history(session), 127);
        let main = conversation(session, "main");
        assert_eq!(main.request_count, 2);
        assert_eq!(main.input_tokens, 3_906);
        assert_eq!(main.output_tokens, 127);
        assert_eq!(main.evidence.input.exact, 2);
    }

    #[test]
    fn a_reported_zero_changes_the_evidence_behind_a_total_without_changing_it() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");

        // Nothing reported yet: the counts are missing rather than zero.
        let started = monitor.snapshot();
        assert_eq!(started.sessions[0].evidence.output.missing, 1);
        assert_eq!(started.sessions[0].evidence.output.opening, 0);

        monitor.stream_progress("r1", 10, 1, Some(0), Some(0));
        let opening = monitor.snapshot();
        assert_eq!(opening.sessions[0].output_tokens, 0);
        assert_eq!(
            opening.sessions[0].evidence.output,
            QualityCoverage {
                missing: 0,
                opening: 1,
                exact: 0
            }
        );
        assert_eq!(opening.sessions[0].evidence.cache_read.missing, 1);

        monitor.usage_reported("r1", closing_usage(0, 0, 0, 0));
        let closed = monitor.snapshot();
        let session = &closed.sessions[0];
        assert_eq!(session.output_tokens, 0);
        assert_eq!(session.input_tokens, 0);
        assert_eq!(
            session.evidence.output,
            QualityCoverage {
                missing: 0,
                opening: 0,
                exact: 1
            }
        );
        assert!(session.evidence.cache_read.is_exact());
        assert_eq!(conversation(session, "main").evidence.output.exact, 1);
    }

    #[test]
    fn a_side_call_hangs_under_the_conversation_that_made_it_after_eviction() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(10_000, 0, 0, 30));
        monitor.request_completed("r1", 200, None, None);
        start_codex_subagent_request(&monitor, "a1", "agent-1", Some("main"));
        monitor.usage_reported("a1", closing_usage(5_000, 0, 0, 20));
        monitor.request_completed("a1", 200, None, None);
        start_codex_subagent_request(&monitor, "t1", "agent-1/side", Some("main"));
        monitor.usage_reported("t1", closing_usage(800, 0, 0, 4));
        monitor.request_completed("t1", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        let session = &state.sessions[0];
        assert_eq!(
            conversation_shape(session),
            vec![
                ("main", None, 0),
                ("agent-1", Some("main"), 1),
                ("agent-1/side", Some("agent-1"), 2),
            ]
        );
        // The lineage Claude Code sent is kept as it sent it, next to where the
        // row ended up hanging.
        assert_eq!(
            conversation(session, "agent-1/side").raw_parent.as_deref(),
            Some("main")
        );
        assert_eq!(session.request_count, 3);
        assert_eq!(session.input_tokens, 15_800);
        assert_eq!(session.output_tokens, 54);
        // A side call neither judges a lane nor becomes the session's context.
        assert_eq!(session.cache.context_tokens, 10_000);
        assert_eq!(session.cache.miss_count, 0);
    }

    #[test]
    fn a_session_that_switched_model_shows_the_latest_and_counts_both_requests() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        start_anthropic_request(&monitor, "r2", "claude-opus-5");
        monitor.usage_reported("r2", closing_usage(2_000, 0, 0, 20));
        monitor.request_completed("r2", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.provider.as_deref(), Some("anthropic"));
        assert_eq!(session.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(session.request_count, 2);
        assert_eq!(session.input_tokens, 3_000);
        let main = conversation(session, "main");
        assert_eq!(main.provider.as_deref(), Some("anthropic"));
        assert_eq!(main.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(main.request_count, 2);
        assert_eq!(main.input_tokens, 3_000);
        assert_eq!(main.evidence.input.exact, 2);
    }

    #[test]
    fn requests_without_a_conversation_are_counted_in_a_bucket_of_their_own() {
        let monitor = MonitorHandle::new(1);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.usage_reported("r1", closing_usage(700, 0, 0, 5));
        monitor.request_failed("r1", Some(500), "upstream overload");
        start_codex_request(&monitor, "r2", "main");
        monitor.usage_reported("r2", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r2", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.request_count, 2);
        assert_eq!(session.failure_count, 1);
        assert_eq!(session.unattributed.request_count, 1);
        assert_eq!(session.unattributed.failure_count, 1);
        assert_eq!(session.unattributed.input_tokens, 700);
        assert_eq!(session.unattributed.output_tokens, 5);
        assert_eq!(session.unattributed.evidence.input.exact, 1);
        // The rows and that bucket are the session; nothing is a difference
        // between them.
        let main = conversation(session, "main");
        assert_eq!(session.conversations.len(), 1);
        assert_eq!(
            main.request_count + session.unattributed.request_count,
            session.request_count
        );
        assert_eq!(
            main.input_tokens + session.unattributed.input_tokens,
            session.input_tokens
        );
        assert_eq!(
            main.output_tokens + session.unattributed.output_tokens,
            session.output_tokens
        );
    }

    #[test]
    fn locally_answered_and_count_tokens_requests_count_without_their_tokens() {
        let monitor = MonitorHandle::new(10);
        // A subagent progress label the proxy answered from the transcript.
        monitor.request_started(
            "local",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("local", "agent-1", None);
        monitor.provider_selected("local", LOCAL_PROVIDER, "gpt-5.6-sol", None);
        monitor.request_completed("local", 200, Some(0), Some(7));
        // A prompt estimate the real request counts again.
        monitor.request_started(
            "count",
            Some("s1".to_string()),
            None,
            EndpointKind::CountTokens,
        );
        monitor.provider_selected("count", "codex", "gpt-5.6-sol", None);
        monitor.usage_updated("count", Some(40_000), None);
        monitor.request_completed("count", 200, None, None);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(100, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        // Both keep their own numbers on their own rows.
        assert_eq!(recent_by_id(&state, "local").output_tokens, Some(7));
        assert_eq!(recent_by_id(&state, "count").input_tokens, Some(40_000));
        let session = &state.sessions[0];
        assert_eq!(session.request_count, 3);
        assert_eq!(session.input_tokens, 100);
        assert_eq!(session.output_tokens, 10);
        assert_eq!(output_history(session), 10);
        assert_eq!(session.evidence.output.requests(), 1);
        assert_eq!(session.evidence.input.requests(), 1);
        // The locally answered request is a request of its conversation and no
        // tokens of it.
        let agent = conversation(session, "agent-1");
        assert_eq!(agent.request_count, 1);
        assert_eq!(agent.output_tokens, 0);
        assert_eq!(agent.evidence.output.requests(), 0);
    }

    /// A request the proxy answered itself knows its whole cost exactly: no
    /// prompt, no cache, the label it wrote. Exact as those counts are, they are
    /// no evidence about a backend's cache, so the conversation keeps the
    /// context its upstream requests built and nothing is compared.
    #[test]
    fn a_locally_answered_request_leaves_its_conversations_cache_lane_alone() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "agent-1");
        monitor.usage_reported("r1", closing_usage(2_000, 28_000, 0, 40));
        monitor.request_completed("r1", 200, None, None);

        // The progress label of the same subagent, answered from the transcript.
        monitor.request_started(
            "local",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("local", "agent-1", None);
        monitor.provider_selected("local", LOCAL_PROVIDER, "gpt-5.6-sol", None);
        monitor.usage_reported("local", closing_usage(0, 0, 0, 4));
        monitor.request_completed("local", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let agent = conversation(session, "agent-1");
        assert_eq!(agent.cache.context_tokens, 30_000);
        assert_eq!(session.cache.miss_count, 0);
        assert!(!recent_by_id(&state, "local").cache.evaluated());
    }

    #[test]
    fn a_repeated_terminal_event_or_report_counts_nothing_twice() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(1_000, 200, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        monitor.usage_reported("r1", closing_usage(1_000, 200, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        monitor.request_failed("r1", Some(502), "too late to change the outcome");

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.status, RequestStatus::Completed);
        assert_eq!(request.input_tokens, Some(1_000));
        let session = &state.sessions[0];
        assert_eq!(session.request_count, 1);
        assert_eq!(session.failure_count, 0);
        assert_eq!(session.active_count, 0);
        assert_eq!(session.input_tokens, 1_000);
        assert_eq!(session.cache_read_tokens, 200);
        assert_eq!(session.output_tokens, 10);
        assert_eq!(output_history(session), 10);
        assert_eq!(session.evidence.input.exact, 1);
        assert_eq!(conversation(session, "main").request_count, 1);
    }

    #[test]
    fn a_correction_takes_back_only_the_history_buckets_its_own_request_wrote() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "other", "main");
        monitor.usage_reported("other", closing_usage(500, 0, 0, 50));
        monitor.request_completed("other", 200, None, None);
        start_codex_request(&monitor, "r1", "main");
        monitor.stream_progress("r1", 100, 1, Some(1_000), Some(40));
        monitor.request_completed("r1", 200, None, None);
        // Its stream ran on into the next bucket of the output history.
        let next_bucket = SystemTime::now() + Duration::from_secs(SESSION_TOKEN_BUCKET_SECS);
        if let Ok(mut store) = monitor.store.lock() {
            store.stream_progress_at(
                "r1",
                &UsageReport::opening(Some(1_000), Some(140)),
                next_bucket,
            );
        }

        let streaming = monitor.snapshot();
        assert_eq!(streaming.sessions[0].output_tokens, 190);
        assert_eq!(streaming.sessions[0].output_token_samples.len(), 2);
        assert_eq!(output_history(&streaming.sessions[0]), 190);

        // The backend's own count is lower than the stream's estimate: the
        // newest bucket gives its tokens back first, the earlier one the rest.
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 30));

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.output_tokens, 80);
        assert_eq!(output_history(session), 80);
        // The request that shares the first bucket keeps its 50 tokens there.
        assert_eq!(session.output_token_samples.len(), 1);
        assert_eq!(session.output_token_samples[0].1, 80);
        let owned = monitor
            .store
            .lock()
            .map(|store| store.ledger.owned_output_buckets("r1"))
            .expect("store lock");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].1, 30);
    }

    #[test]
    fn a_conversation_named_after_a_failed_request_finished_takes_its_counts_along() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        // A stream the client received with a 200 that stopped before its
        // terminal event: a failure the status does not show.
        monitor.request_failed("r1", Some(200), "stream ended before its terminal event");

        let before = monitor.snapshot();
        assert_eq!(before.sessions[0].unattributed.request_count, 1);
        assert_eq!(before.sessions[0].unattributed.failure_count, 1);
        assert_eq!(before.sessions[0].unattributed.input_tokens, 1_000);

        monitor.conversation_resolved("r1", "agent-7", Some("main".to_string()));

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.request_count, 1);
        assert_eq!(session.failure_count, 1);
        assert_eq!(session.input_tokens, 1_000);
        assert_eq!(session.unattributed, UnattributedUsage::default());
        let agent = conversation(session, "agent-7");
        assert_eq!(agent.request_count, 1);
        assert_eq!(agent.failure_count, 1);
        assert_eq!(agent.input_tokens, 1_000);
        assert_eq!(agent.output_tokens, 10);
        assert_eq!(agent.evidence.input.exact, 1);
        assert_eq!(agent.raw_parent.as_deref(), Some("main"));
        assert_eq!(agent.last_status, "failed");

        // Moving it on again leaves no empty row behind.
        monitor.conversation_resolved("r1", "agent-9", None);
        let moved = monitor.snapshot();
        assert_eq!(
            conversation_shape(&moved.sessions[0]),
            vec![("agent-9", None, 0)]
        );
        assert_eq!(
            conversation(&moved.sessions[0], "agent-9").input_tokens,
            1_000
        );
        assert_eq!(moved.sessions[0].request_count, 1);
        assert_eq!(moved.sessions[0].failure_count, 1);
    }

    #[test]
    fn a_reported_prompt_total_survives_eviction_without_pinning_the_uncached_half() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "a", "main");
        monitor.stream_progress("a", 100, 1, Some(341_974), Some(0));
        monitor.stream_progress_usage("a", 200, 1, total_only_usage(5_000, 7));
        monitor.request_completed("a", 200, None, None);
        // A subagent request evicts it from the recent list.
        start_codex_request(&monitor, "b", "agent-1");
        monitor.usage_reported("b", closing_usage(900, 0, 0, 5));
        monitor.request_completed("b", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        let session = &state.sessions[0];
        // The estimate stays where it was, unconfirmed, and the total the
        // backend measured is kept beside the categories instead of added to
        // them.
        assert_eq!(session.input_tokens, 342_874);
        assert_eq!(session.evidence.input.opening, 1);
        assert_eq!(session.evidence.input.exact, 1);
        assert_eq!(session.evidence.cache_read.missing, 1);
        assert_eq!(session.evidence.reported_prompt_requests, 1);
        assert_eq!(session.evidence.reported_prompt_tokens, 5_000);
        let main = conversation(session, "main");
        assert_eq!(main.evidence.reported_prompt_tokens, 5_000);
        assert_eq!(main.input_tokens, 341_974);
        // The prompt the backend measured is the context of the conversation it
        // ran in, estimate and eviction notwithstanding, and no share of it was
        // named so nothing was judged a miss.
        assert_eq!(main.cache.context_tokens, 5_000);
        assert_eq!(session.cache.context_tokens, 5_000);
        assert_eq!(session.cache.miss_count, 0);
    }

    #[test]
    fn a_cached_only_report_sizes_no_prompt_and_judges_no_lane() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(900, 30_000, 0, 5));
        monitor.request_completed("r1", 200, None, None);
        assert_eq!(monitor.snapshot().sessions[0].cache.context_tokens, 30_900);

        // Only the cached part of the next prompt is reported. Its sum is no
        // prompt size: it must not shrink the context or count as a miss.
        start_codex_request(&monitor, "r2", "main");
        monitor.usage_reported(
            "r2",
            UsageReport {
                closing: UsageFields {
                    cache_read_tokens: Some(200),
                    ..UsageFields::default()
                },
                ..UsageReport::default()
            },
        );
        monitor.request_completed("r2", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r2");
        assert!(!request.cache.evaluated());
        assert_eq!(request.cache.miss, None);
        let session = &state.sessions[0];
        assert_eq!(session.cache.context_tokens, 30_900);
        assert_eq!(session.cache.miss_count, 0);
        assert_eq!(session.cache_read_tokens, 30_200);
        assert_eq!(session.evidence.input.missing, 1);
    }

    #[test]
    fn a_usage_event_for_an_unknown_request_invents_no_history() {
        let monitor = MonitorHandle::new(10);
        monitor.usage_reported("never-started", closing_usage(9_000, 0, 0, 90));
        monitor.stream_progress("never-started", 10, 1, Some(9_000), Some(90));

        let state = monitor.snapshot();
        assert!(state.sessions.is_empty());
        assert!(state.recent.is_empty());
        assert!(state.active.is_empty());
    }

    #[test]
    fn a_terminal_event_without_a_start_counts_one_request_from_there_on() {
        let monitor = MonitorHandle::new(10);
        monitor.request_completed("orphan", 200, Some(100), Some(20));

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].request_id, "orphan");
        let session = &state.sessions[0];
        assert_eq!(session.session_id, None);
        assert_eq!(session.request_count, 1);
        assert_eq!(session.active_count, 0);
        assert_eq!(session.input_tokens, 100);
        assert_eq!(session.output_tokens, 20);
        assert_eq!(session.unattributed.request_count, 1);
    }

    #[test]
    fn a_late_report_from_an_older_request_does_not_take_the_model_back() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        start_anthropic_request(&monitor, "r2", "claude-opus-5");
        monitor.usage_reported("r2", closing_usage(2_000, 0, 0, 20));
        monitor.request_completed("r2", 200, None, None);

        // The older request's stream reports its final counts after the switch.
        monitor.stream_progress_usage("r1", 50, 1, closing_usage(900, 100, 0, 12));

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        // What the session is running is the newer request's model, whatever
        // order the reports arrived in.
        assert_eq!(session.provider.as_deref(), Some("anthropic"));
        assert_eq!(session.model.as_deref(), Some("claude-opus-5"));
        let main = conversation(session, "main");
        assert_eq!(main.provider.as_deref(), Some("anthropic"));
        assert_eq!(main.model.as_deref(), Some("claude-opus-5"));
        // The correction still lands in the totals.
        assert_eq!(session.input_tokens, 2_900);
        assert_eq!(session.cache_read_tokens, 100);
        assert_eq!(session.output_tokens, 32);
        assert_eq!(main.input_tokens, 2_900);
    }

    #[test]
    fn a_project_named_late_by_an_older_request_still_reaches_its_session() {
        let monitor = MonitorHandle::new(10);
        // The main request is seen first, but Claude Code's working directory
        // is only found in its body a moment later.
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        // A side call arrives after it and is routed first. It carries no
        // working directory of its own, so it names no project at all.
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.conversation_resolved("r2", "main/side", None);
        monitor.provider_selected("r2", "codex", "gpt-5.6-luna", None);
        monitor.request_completed("r2", 200, Some(500), Some(10));

        monitor.project_resolved("r1", "example-project");
        monitor.conversation_resolved("r1", "main", None);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        // A request of the session knows the project, so the session shows it,
        // however late it said so.
        assert_eq!(session.project.as_deref(), Some("example-project"));
        // What the session is running is still the newest request's, which is
        // the one that named no project.
        assert_eq!(session.provider.as_deref(), Some("codex"));
        assert_eq!(session.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(session.last_status, "completed");
    }

    #[test]
    fn the_newest_request_that_named_a_project_is_the_one_the_session_shows() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        // The newer request names where it is working, and only then does the
        // older one name the directory it was started in.
        monitor.project_resolved("r2", "current-project");
        monitor.project_resolved("r1", "previous-project");

        let state = monitor.snapshot();
        assert_eq!(
            state.sessions[0].project.as_deref(),
            Some("current-project")
        );
    }

    #[test]
    fn two_requests_stamped_in_the_same_instant_keep_the_later_ones_project() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        // Both were stamped in the same instant, so the order they were seen
        // in is all that tells them apart.
        let same_instant = SystemTime::now() - Duration::from_secs(1);
        if let Ok(mut store) = monitor.store.lock() {
            store.ledger.backdate_for_tests("r1", same_instant);
            store.ledger.backdate_for_tests("r2", same_instant);
        }
        monitor.project_resolved("r2", "current-project");
        monitor.project_resolved("r1", "previous-project");

        let state = monitor.snapshot();
        assert_eq!(
            state.sessions[0].project.as_deref(),
            Some("current-project")
        );
    }

    #[test]
    fn a_parent_named_late_by_an_older_request_still_reaches_its_group() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("old", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("new", Some("s1".to_string()), None, EndpointKind::Messages);
        // The newer request joins the group first, saying nothing about what
        // spawned it.
        monitor.conversation_resolved("new", "agent-7", None);
        monitor.provider_selected("new", "anthropic", "claude-opus-5", None);
        monitor.request_completed("new", 200, Some(2_000), Some(20));
        // The older one joins afterwards and names the parent.
        monitor.conversation_resolved("old", "agent-7", Some("main".to_string()));
        monitor.provider_selected("old", "codex", "gpt-5.6-sol", None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let group = conversation(session, "agent-7");
        // A request of the group knows what spawned it, so the group hangs
        // under it, however late it said so.
        assert_eq!(group.raw_parent.as_deref(), Some("main"));
        // What the group is running is still the newest request's.
        assert_eq!(group.provider.as_deref(), Some("anthropic"));
        assert_eq!(group.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(group.last_status, "completed");
    }

    #[test]
    fn a_parent_named_after_the_group_was_already_joined_still_reaches_it() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("old", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("new", Some("s1".to_string()), None, EndpointKind::Messages);
        // Both requests are in the group before either knows its lineage.
        monitor.conversation_resolved("old", "agent-7", None);
        monitor.conversation_resolved("new", "agent-7", None);
        monitor.provider_selected("new", "anthropic", "claude-opus-5", None);
        // The older request names the parent of the group it is already in.
        monitor.conversation_resolved("old", "agent-7", Some("main".to_string()));

        let state = monitor.snapshot();
        let group = conversation(&state.sessions[0], "agent-7");
        assert_eq!(group.raw_parent.as_deref(), Some("main"));
        assert_eq!(group.model.as_deref(), Some("claude-opus-5"));
        // Naming it again moved nothing: the group still holds both requests.
        assert_eq!(group.request_count, 2);
        assert_eq!(state.sessions[0].conversations.len(), 1);
    }

    #[test]
    fn the_newest_request_that_named_a_parent_is_the_one_the_group_shows() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("old", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("new", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.conversation_resolved("new", "agent-7", Some("agent-3".to_string()));
        // Neither an older request naming another parent nor a later one
        // naming none takes the group off the agent that spawned it.
        monitor.conversation_resolved("old", "agent-7", Some("main".to_string()));
        monitor.request_started(
            "later",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("later", "agent-7", None);

        let state = monitor.snapshot();
        let group = conversation(&state.sessions[0], "agent-7");
        assert_eq!(group.raw_parent.as_deref(), Some("agent-3"));
    }

    #[test]
    fn two_requests_of_a_group_stamped_in_the_same_instant_keep_the_later_ones_parent() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("old", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_started("new", Some("s1".to_string()), None, EndpointKind::Messages);
        // Both were stamped in the same instant, so the order they were seen
        // in is all that tells them apart.
        let same_instant = SystemTime::now() - Duration::from_secs(1);
        if let Ok(mut store) = monitor.store.lock() {
            store.ledger.backdate_for_tests("old", same_instant);
            store.ledger.backdate_for_tests("new", same_instant);
        }
        monitor.conversation_resolved("new", "agent-7", Some("agent-3".to_string()));
        monitor.conversation_resolved("old", "agent-7", Some("main".to_string()));

        let state = monitor.snapshot();
        let group = conversation(&state.sessions[0], "agent-7");
        assert_eq!(group.raw_parent.as_deref(), Some("agent-3"));
    }

    #[test]
    fn moving_the_request_that_named_the_parent_out_of_a_group_restates_it_from_the_rest() {
        let monitor = MonitorHandle::new(10);
        start_codex_subagent_request(&monitor, "r1", "agent-7", Some("main"));
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        // A newer request of the same group, which names the parent too and is
        // the one the row is showing.
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.conversation_resolved("r2", "agent-7", Some("main".to_string()));
        monitor.provider_selected("r2", "anthropic", "claude-opus-5", None);
        monitor.usage_reported("r2", closing_usage(2_000, 0, 0, 20));
        monitor.request_completed("r2", 200, None, None);

        // Claude Code says the newer request is a group of its own after all.
        monitor.conversation_resolved("r2", "agent-9", None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let first = conversation(session, "agent-7");
        // The row it left states its lineage again from the request it still
        // holds, which named the same parent.
        assert_eq!(first.raw_parent.as_deref(), Some("main"));
        assert_eq!(first.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(first.request_count, 1);
        // The group it moved to hangs under the session, having named no
        // parent of its own.
        assert_eq!(conversation(session, "agent-9").raw_parent, None);

        // A late report from the request that stayed leaves the restated
        // lineage as it is.
        monitor.stream_progress_usage("r1", 50, 1, closing_usage(900, 100, 0, 12));
        let late = monitor.snapshot();
        let first = conversation(&late.sessions[0], "agent-7");
        assert_eq!(first.raw_parent.as_deref(), Some("main"));
        assert_eq!(first.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(first.input_tokens, 900);
    }

    #[test]
    fn moving_the_newest_request_out_of_a_group_leaves_the_metadata_of_the_rest() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "agent-1");
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        // A newer request of the same group on another provider, which Claude
        // Code only later says belongs to a group of its own.
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.conversation_resolved("r2", "agent-1", None);
        monitor.provider_selected("r2", "anthropic", "claude-opus-5", None);
        monitor.usage_reported("r2", closing_usage(2_000, 0, 0, 20));
        monitor.request_failed("r2", Some(200), "stream ended before its terminal event");

        let before = monitor.snapshot();
        let group = conversation(&before.sessions[0], "agent-1");
        assert_eq!(group.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(group.first_seen_rank, 0);

        monitor.conversation_resolved("r2", "agent-2", None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let first = conversation(session, "agent-1");
        assert_eq!(first.request_count, 1);
        assert_eq!(first.failure_count, 0);
        assert_eq!(first.input_tokens, 1_000);
        // The row it left shows what its remaining request says, not what the
        // one that moved away was doing.
        assert_eq!(first.provider.as_deref(), Some("codex"));
        assert_eq!(first.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(first.last_status, "completed");
        // And it keeps the identity it was first seen with.
        assert_eq!(first.first_seen_rank, 0);
        let second = conversation(session, "agent-2");
        assert_eq!(second.request_count, 1);
        assert_eq!(second.failure_count, 1);
        assert_eq!(second.input_tokens, 2_000);
        assert_eq!(second.provider.as_deref(), Some("anthropic"));
        assert_eq!(second.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(second.last_status, "failed");
        assert_eq!(session.request_count, 2);
        assert_eq!(session.failure_count, 1);
    }

    #[test]
    fn the_session_rate_comes_from_the_requests_still_in_view() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "measured", "main");
        monitor.generation_started("measured");
        monitor.stream_progress("measured", 100, 1, Some(1_000), Some(120));
        monitor.request_completed("measured", 200, None, None);

        let visible = monitor.snapshot();
        assert!(matches!(
            visible.sessions[0].rate(),
            Throughput::TokensPerSecond(_)
        ));

        // A buffered request with no measured interval evicts it from view.
        start_codex_request(&monitor, "buffered", "main");
        monitor.usage_reported("buffered", closing_usage(500, 0, 0, 30));
        monitor.request_completed("buffered", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(state.recent.len(), 1);
        // Its tokens stay in the lifetime totals; its throughput left with it.
        assert_eq!(state.sessions[0].output_tokens, 150);
        assert_eq!(state.sessions[0].rate(), Throughput::None);
    }

    #[test]
    fn output_history_is_published_at_the_terminal_event_and_corrected_after_it() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "other", "main");
        monitor.usage_reported("other", closing_usage(500, 0, 0, 50));
        monitor.request_completed("other", 200, None, None);

        start_codex_request(&monitor, "r1", "main");
        monitor.stream_progress("r1", 100, 1, Some(1_000), Some(100));
        // While it runs its output is a live count with no place in the
        // history yet.
        let streaming = monitor.snapshot();
        assert_eq!(streaming.active[0].output_tokens, Some(100));
        assert_eq!(streaming.sessions[0].output_tokens, 150);
        assert_eq!(output_history(&streaming.sessions[0]), 50);

        monitor.stream_progress_usage("r1", 50, 1, closing_usage(1_000, 0, 0, 80));
        let corrected = monitor.snapshot();
        assert_eq!(corrected.sessions[0].output_tokens, 130);
        assert_eq!(output_history(&corrected.sessions[0]), 50);

        monitor.request_completed("r1", 200, None, None);
        let finished = monitor.snapshot();
        assert_eq!(output_history(&finished.sessions[0]), 130);
        assert_eq!(finished.sessions[0].output_token_samples.len(), 1);

        // A late chunk lands in the next bucket of the history.
        let next_bucket = SystemTime::now() + Duration::from_secs(SESSION_TOKEN_BUCKET_SECS);
        if let Ok(mut store) = monitor.store.lock() {
            store.stream_progress_at("r1", &closing_usage(1_000, 0, 0, 90), next_bucket);
        }
        let late = monitor.snapshot();
        assert_eq!(late.sessions[0].output_tokens, 140);
        assert_eq!(output_history(&late.sessions[0]), 140);
        assert_eq!(late.sessions[0].output_token_samples.len(), 2);

        // The backend's final count takes its own tokens back, newest bucket
        // first, and leaves the other request's alone.
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 70));
        let state = monitor.snapshot();
        assert_eq!(state.sessions[0].output_tokens, 120);
        assert_eq!(output_history(&state.sessions[0]), 120);
        assert_eq!(state.sessions[0].output_token_samples.len(), 1);
        assert_eq!(state.sessions[0].output_token_samples[0].1, 120);
        let owned = monitor
            .store
            .lock()
            .map(|store| store.ledger.owned_output_buckets("r1"))
            .expect("store lock");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].1, 70);
    }

    /// Every output token a session's history holds.
    fn output_history(session: &SessionSummary) -> u64 {
        session
            .output_token_samples
            .iter()
            .map(|(_, tokens)| *tokens)
            .sum()
    }

    #[test]
    fn context_cache_expiry_counts_down_from_the_latest_main_request() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let stats = SessionCacheStats {
            context_started_at: Some(started),
            context_ttl: Some(Duration::from_secs(3600)),
            ..SessionCacheStats::default()
        };
        assert_eq!(
            stats.context_cache_expiry(started + Duration::from_secs(600)),
            Some(CacheExpiry::WarmFor(Duration::from_secs(3000)))
        );
        assert_eq!(
            stats.context_cache_expiry(started + Duration::from_secs(7200)),
            Some(CacheExpiry::ExpiredAgo(Duration::from_secs(3600)))
        );
        assert_eq!(
            SessionCacheStats::default().context_cache_expiry(started),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Requested versus effective model, and what the two add up to
    // -----------------------------------------------------------------------

    /// The row of one wire model, by provider and model name.
    fn model_row<'a>(
        session: &'a SessionSummary,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> &'a ModelUsage {
        session
            .models
            .iter()
            .find(|row| row.provider.as_deref() == provider && row.model.as_deref() == model)
            .unwrap_or_else(|| {
                panic!(
                    "no row for {provider:?}/{model:?}; rows={:?}",
                    session
                        .models
                        .iter()
                        .map(|row| (row.provider.clone(), row.model.clone()))
                        .collect::<Vec<_>>()
                )
            })
    }

    fn requested_of(row: &ModelUsage) -> Vec<(Option<&str>, usize)> {
        row.requested_models
            .iter()
            .map(|(model, count)| (model.as_deref(), *count))
            .collect()
    }

    #[test]
    fn the_model_a_caller_asked_for_is_kept_apart_from_the_one_that_ran() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        // What the client sent, before the proxy rewrote anything.
        monitor.model_requested("r1", "gpt-5.6-luna");
        // What the proxy routed, then what the provider put on the wire.
        monitor.provider_selected("r1", "codex", "gpt-5.6-luna", None);
        monitor.model_resolved("r1", "gpt-5.6-sol");
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.requested_model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(request.effective_model.as_deref(), Some("gpt-5.6-sol"));
        // The old display stays a projection of the two, and is not what any
        // total is keyed on.
        assert_eq!(request.model.as_deref(), Some("gpt-5.6-luna → gpt-5.6-sol"));

        let session = &state.sessions[0];
        assert_eq!(session.models.len(), 1);
        let row = model_row(session, Some("codex"), Some("gpt-5.6-sol"));
        assert_eq!(row.request_count, 1);
        assert_eq!(row.input_tokens, 1_000);
        assert_eq!(row.output_tokens, 10);
        assert_eq!(requested_of(row), vec![(Some("gpt-5.6-luna"), 1)]);
    }

    #[test]
    fn a_request_that_never_reached_a_provider_keeps_the_model_it_asked_for() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.model_requested("r1", "not-a-model");
        monitor.request_failed("r1", Some(400), "Unknown model");
        // A request whose body named no model at all invents none.
        monitor.request_started("r2", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.request_failed("r2", Some(400), "Missing model");

        let state = monitor.snapshot();
        let unknown = recent_by_id(&state, "r1");
        assert_eq!(unknown.requested_model.as_deref(), Some("not-a-model"));
        assert_eq!(unknown.effective_model, None);
        assert_eq!(unknown.provider, None);
        let missing = recent_by_id(&state, "r2");
        assert_eq!(missing.requested_model, None);
        assert_eq!(missing.effective_model, None);

        // Both are counted where no model ran, rather than on the last model of
        // the session.
        let session = &state.sessions[0];
        let row = model_row(session, None, None);
        assert_eq!(row.request_count, 2);
        assert_eq!(row.failure_count, 2);
        assert_eq!(requested_of(row), vec![(None, 1), (Some("not-a-model"), 1)]);
    }

    #[test]
    fn locally_answered_requests_have_a_requested_model_and_no_executed_one() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "local",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.model_requested("local", "gpt-5.6-sol");
        monitor.provider_selected("local", LOCAL_PROVIDER, "gpt-5.6-sol", None);
        monitor.request_completed("local", 200, Some(0), Some(7));
        monitor.request_started(
            "count",
            Some("s1".to_string()),
            None,
            EndpointKind::CountTokens,
        );
        monitor.model_requested("count", "gpt-5.6-sol");
        monitor.provider_selected("count", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("count", "gpt-5.6-sol");
        monitor.usage_updated("count", Some(40_000), None);
        monitor.request_completed("count", 200, None, None);
        start_codex_request(&monitor, "r1", "main");
        monitor.model_requested("r1", "gpt-5.6-sol");
        monitor.model_resolved("r1", "gpt-5.6-sol");
        monitor.usage_reported("r1", closing_usage(100, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let local = recent_by_id(&state, "local");
        assert_eq!(local.requested_model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(local.effective_model, None);

        let session = &state.sessions[0];
        // The proxy's own answer is a request of the session with no model
        // behind it; its synthetic output stays out of every model's tokens.
        let answered = model_row(session, Some(LOCAL_PROVIDER), None);
        assert_eq!(answered.request_count, 1);
        assert_eq!(answered.output_tokens, 0);
        assert_eq!(answered.evidence.output.requests(), 0);
        // The estimate and the real request share a row; only the real one has
        // tokens on it.
        let ran = model_row(session, Some("codex"), Some("gpt-5.6-sol"));
        assert_eq!(ran.request_count, 2);
        assert_eq!(ran.input_tokens, 100);
        assert_eq!(ran.output_tokens, 10);
        assert_eq!(ran.evidence.input.requests(), 1);
    }

    #[test]
    fn a_wire_model_named_late_carries_the_whole_contribution_to_its_row() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "a", "main");
        monitor.model_requested("a", "gpt-5.6-luna");
        monitor.usage_reported("a", closing_usage(2_000, 500, 0, 20));
        monitor.request_failed("a", Some(502), "stream ended");
        // Another request evicts it from the recent list.
        start_codex_request(&monitor, "b", "main");
        monitor.model_resolved("b", "gpt-5.6-sol");
        monitor.request_completed("b", 200, None, None);

        let before = monitor.snapshot();
        assert_eq!(before.recent.len(), 1);
        let unknown = model_row(&before.sessions[0], Some("codex"), None);
        assert_eq!(unknown.request_count, 1);
        assert_eq!(unknown.failure_count, 1);
        assert_eq!(unknown.input_tokens, 2_000);
        assert_eq!(unknown.cache_read_tokens, 500);
        assert_eq!(unknown.evidence.input.exact, 1);

        // The producer names the wire model after the request left the recent
        // list: everything it contributed moves with it, once.
        monitor.model_resolved("a", "gpt-5.6-terra");

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert!(
            session.models.iter().all(|row| row.model.is_some()),
            "the row with no model must be gone once every request named one"
        );
        let terra = model_row(session, Some("codex"), Some("gpt-5.6-terra"));
        assert_eq!(terra.request_count, 1);
        assert_eq!(terra.failure_count, 1);
        assert_eq!(terra.input_tokens, 2_000);
        assert_eq!(terra.cache_read_tokens, 500);
        assert_eq!(terra.output_tokens, 20);
        assert_eq!(terra.evidence.input.exact, 1);
        assert_eq!(requested_of(terra), vec![(Some("gpt-5.6-luna"), 1)]);
        let sol = model_row(session, Some("codex"), Some("gpt-5.6-sol"));
        assert_eq!(sol.request_count, 1);
        assert_eq!(sol.failure_count, 0);
        assert_eq!(sol.input_tokens, 0);
        // Nothing was counted twice on the way.
        assert_eq!(session.request_count, 2);
        assert_eq!(session.failure_count, 1);
        assert_eq!(session.input_tokens, 2_000);
    }

    #[test]
    fn a_later_requested_model_does_not_replace_the_first_on_any_row() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "r1", "main");
        monitor.model_requested("r1", "opus");
        monitor.model_resolved("r1", "gpt-5.6-luna");
        // What the client asked for is what arrived first; a later naming of it
        // is a second reading of the same thing, not a second request.
        monitor.model_requested("r1", "haiku");

        let live = monitor.snapshot();
        assert_eq!(live.active.len(), 1);
        assert_eq!(live.active[0].requested_model.as_deref(), Some("opus"));

        monitor.usage_reported("r1", closing_usage(100, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);
        monitor.model_requested("r1", "sonnet");

        let done = monitor.snapshot();
        let request = recent_by_id(&done, "r1");
        assert_eq!(request.requested_model.as_deref(), Some("opus"));
        let row = model_row(&done.sessions[0], Some("codex"), Some("gpt-5.6-luna"));
        assert_eq!(requested_of(row), vec![(Some("opus"), 1)]);

        // Another request evicts it from the recent list; a naming that arrives
        // after that still changes nothing the ledger holds.
        start_codex_request(&monitor, "r2", "main");
        monitor.model_resolved("r2", "gpt-5.6-sol");
        monitor.request_completed("r2", 200, None, None);
        monitor.model_requested("r1", "fable");

        let after = monitor.snapshot();
        let row = model_row(&after.sessions[0], Some("codex"), Some("gpt-5.6-luna"));
        assert_eq!(requested_of(row), vec![(Some("opus"), 1)]);
        assert_eq!(row.request_count, 1);
    }

    #[test]
    fn naming_the_same_wire_model_again_is_not_a_second_switch() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.model_requested("r1", "gpt-5.6-sol");
        monitor.model_resolved("r1", "gpt-5.6-terra");

        let once = monitor.snapshot();
        assert_eq!(
            once.active[0].model.as_deref(),
            Some("gpt-5.6-sol → gpt-5.6-terra")
        );

        // The same reading arriving twice must read as it did the first time.
        monitor.model_resolved("r1", "gpt-5.6-terra");
        monitor.usage_reported("r1", closing_usage(100, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(
            request.model.as_deref(),
            Some("gpt-5.6-sol → gpt-5.6-terra")
        );
        assert_eq!(request.effective_model.as_deref(), Some("gpt-5.6-terra"));
        let session = &state.sessions[0];
        assert_eq!(session.models.len(), 1);
        let row = model_row(session, Some("codex"), Some("gpt-5.6-terra"));
        assert_eq!(row.request_count, 1);
        assert_eq!(row.input_tokens, 100);
        assert_eq!(requested_of(row), vec![(Some("gpt-5.6-sol"), 1)]);
        assert_eq!(session.request_count, 1);
        assert_eq!(session.input_tokens, 100);
    }

    #[test]
    fn a_wire_model_named_again_brings_the_whole_contribution_back_to_its_row() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.model_requested("r1", "gpt-5.6-sol");
        monitor.model_resolved("r1", "gpt-5.6-sol");
        monitor.usage_reported("r1", closing_usage(1_000, 400, 0, 20));
        monitor.request_completed("r1", 200, None, None);
        // A second request opens a row after it, so the order can be read.
        start_codex_request(&monitor, "r2", "main");
        monitor.model_resolved("r2", "gpt-5.6-terra");
        monitor.request_completed("r2", 200, None, None);
        let first_rank = model_row(
            &monitor.snapshot().sessions[0],
            Some("codex"),
            Some("gpt-5.6-sol"),
        )
        .first_seen_rank;

        // The wire model is restated, away from the first row and back to it.
        monitor.model_resolved("r1", "gpt-5.6-terra");
        monitor.model_resolved("r1", "gpt-5.6-sol");

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let sol = model_row(session, Some("codex"), Some("gpt-5.6-sol"));
        // The row it came back to is the one it left, not a new one at the end.
        assert_eq!(sol.first_seen_rank, first_rank);
        assert_eq!(sol.request_count, 1);
        assert_eq!(sol.input_tokens, 1_000);
        assert_eq!(sol.cache_read_tokens, 400);
        assert_eq!(sol.output_tokens, 20);
        assert_eq!(requested_of(sol), vec![(Some("gpt-5.6-sol"), 1)]);
        let terra = model_row(session, Some("codex"), Some("gpt-5.6-terra"));
        assert_eq!(terra.request_count, 1);
        assert_eq!(terra.input_tokens, 0);
        assert_eq!(session.request_count, 2);
        assert_eq!(session.input_tokens, 1_000);
    }

    #[test]
    fn a_closing_report_under_the_opening_one_takes_the_difference_off_its_row() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "r1", "main");
        monitor.model_requested("r1", "gpt-5.6-sol");
        monitor.model_resolved("r1", "gpt-5.6-sol");
        // The translated stream opens with an estimate of the whole prompt.
        monitor.stream_progress_usage("r1", 100, 1, UsageReport::opening(Some(5_000), Some(0)));
        assert_eq!(
            model_row(
                &monitor.snapshot().sessions[0],
                Some("codex"),
                Some("gpt-5.6-sol")
            )
            .input_tokens,
            5_000
        );

        // The backend's own count closes it lower.
        monitor.usage_reported("r1", closing_usage(1_000, 0, 0, 10));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let row = model_row(session, Some("codex"), Some("gpt-5.6-sol"));
        assert_eq!(row.input_tokens, 1_000);
        assert_eq!(row.output_tokens, 10);
        assert_eq!(session.input_tokens, 1_000);
        // The session, its conversations and its models still say the same.
        let by_conversation: u64 = session
            .conversations
            .iter()
            .map(|row| row.input_tokens)
            .sum::<u64>()
            + session.unattributed.input_tokens;
        assert_eq!(by_conversation, session.input_tokens);
        let by_model: u64 = session.models.iter().map(|row| row.input_tokens).sum();
        assert_eq!(by_model, session.input_tokens);
        let output: u64 = session.models.iter().map(|row| row.output_tokens).sum();
        assert_eq!(output, session.output_tokens);
    }

    #[test]
    fn two_sessions_keep_their_providers_and_models_apart() {
        let monitor = MonitorHandle::new(10);
        for (request_id, session_id, provider, model) in [
            ("a", "s1", "codex", "gpt-5.6-sol"),
            ("b", "s1", "anthropic", "claude-opus-5"),
            ("c", "s2", "codex", "gpt-5.6-sol"),
        ] {
            monitor.request_started(
                request_id,
                Some(session_id.to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.model_requested(request_id, model);
            monitor.provider_selected(request_id, provider, model, None);
            monitor.model_resolved(request_id, model);
            monitor.usage_reported(request_id, closing_usage(100, 0, 0, 1));
            monitor.request_completed(request_id, 200, None, None);
        }

        let state = monitor.snapshot();
        let first = state
            .sessions
            .iter()
            .find(|session| session.session_id.as_deref() == Some("s1"))
            .expect("first session");
        assert_eq!(first.models.len(), 2);
        assert_eq!(
            model_row(first, Some("codex"), Some("gpt-5.6-sol")).request_count,
            1
        );
        assert_eq!(
            model_row(first, Some("anthropic"), Some("claude-opus-5")).input_tokens,
            100
        );
        let second = state
            .sessions
            .iter()
            .find(|session| session.session_id.as_deref() == Some("s2"))
            .expect("second session");
        assert_eq!(second.models.len(), 1);
        assert_eq!(
            model_row(second, Some("codex"), Some("gpt-5.6-sol")).request_count,
            1
        );
    }

    #[test]
    fn a_model_row_keeps_its_place_when_a_report_passes_through_it() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "first", "main");
        monitor.model_resolved("first", "gpt-5.6-sol");
        monitor.request_completed("first", 200, None, None);
        start_anthropic_request(&monitor, "second", "claude-opus-5");
        monitor.model_resolved("second", "claude-opus-5");
        monitor.request_completed("second", 200, None, None);

        let order = |state: &MonitorState| {
            state.sessions[0]
                .models
                .iter()
                .map(|row| (row.model.clone(), row.first_seen_rank))
                .collect::<Vec<_>>()
        };
        let before = order(&monitor.snapshot());
        // The rows are shown oldest first, and their ranks say so.
        assert_eq!(
            before
                .iter()
                .map(|(model, _)| model.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("gpt-5.6-sol"), Some("claude-opus-5")]
        );
        assert!(before[0].1 < before[1].1);

        // Every change to a request takes its contribution out of its rows and
        // puts it back. The older row holds only this request, so it empties on
        // the way through; it must come back where it was rather than at the
        // end of the order.
        monitor.usage_reported("first", closing_usage(10, 0, 0, 1));
        assert_eq!(order(&monitor.snapshot()), before);
    }

    #[test]
    fn a_sessions_rows_reconcile_with_its_conversations_and_its_models() {
        let monitor = MonitorHandle::new(10);
        start_codex_request(&monitor, "main-1", "main");
        monitor.model_requested("main-1", "gpt-5.6-sol");
        monitor.model_resolved("main-1", "gpt-5.6-sol");
        monitor.usage_reported("main-1", closing_usage(1_000, 4_000, 200, 30));
        monitor.request_completed("main-1", 200, None, None);
        start_codex_subagent_request(&monitor, "sub-1", "agent-1", Some("main"));
        monitor.model_requested("sub-1", "gpt-5.6-luna");
        monitor.model_resolved("sub-1", "gpt-5.6-terra");
        monitor.usage_reported("sub-1", closing_usage(500, 0, 0, 5));
        monitor.request_completed("sub-1", 200, None, None);
        // A request that named no conversation of its own.
        monitor.request_started(
            "loose",
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.model_requested("loose", "gpt-5.6-sol");
        monitor.provider_selected("loose", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("loose", "gpt-5.6-sol");
        monitor.usage_reported("loose", closing_usage(7, 0, 0, 1));
        monitor.request_completed("loose", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        let by_conversation: u64 = session
            .conversations
            .iter()
            .map(|row| row.input_tokens)
            .sum::<u64>()
            + session.unattributed.input_tokens;
        assert_eq!(by_conversation, session.input_tokens);
        let by_model: u64 = session.models.iter().map(|row| row.input_tokens).sum();
        assert_eq!(by_model, session.input_tokens);
        let requests: usize = session.models.iter().map(|row| row.request_count).sum();
        assert_eq!(requests, session.request_count);
        let output: u64 = session.models.iter().map(|row| row.output_tokens).sum();
        assert_eq!(output, session.output_tokens);
        assert_eq!(
            model_row(session, Some("codex"), Some("gpt-5.6-sol")).request_count,
            2
        );
        assert_eq!(
            requested_of(model_row(session, Some("codex"), Some("gpt-5.6-terra"))),
            vec![(Some("gpt-5.6-luna"), 1)]
        );
    }

    #[test]
    fn a_reported_prompt_total_reaches_the_model_row_it_belongs_to() {
        let monitor = MonitorHandle::new(1);
        start_codex_request(&monitor, "a", "main");
        monitor.model_requested("a", "gpt-5.6-sol");
        monitor.model_resolved("a", "gpt-5.6-sol");
        monitor.stream_progress("a", 100, 1, Some(341_974), Some(0));
        monitor.stream_progress_usage("a", 200, 1, total_only_usage(5_000, 7));
        monitor.request_completed("a", 200, None, None);
        // Evict it from the recent list.
        start_codex_request(&monitor, "b", "agent-1");
        monitor.model_resolved("b", "gpt-5.6-sol");
        monitor.request_completed("b", 200, None, None);

        let state = monitor.snapshot();
        let row = model_row(&state.sessions[0], Some("codex"), Some("gpt-5.6-sol"));
        // The backend's own total is held beside the categories, never added to
        // them and never forced to agree with them.
        assert_eq!(row.input_tokens, 341_974);
        assert_eq!(row.evidence.reported_prompt_requests, 1);
        assert_eq!(row.evidence.reported_prompt_tokens, 5_000);
        assert_eq!(row.evidence.input.opening, 1);
        assert_eq!(row.evidence.cache_read.missing, 2);
    }

    // -----------------------------------------------------------------------
    // The two cache-write lifetimes
    // -----------------------------------------------------------------------

    /// An Anthropic report whose cache write is split into its two lifetimes.
    fn cache_creation_usage(
        write: Option<u64>,
        ephemeral_5m: Option<u64>,
        ephemeral_1h: Option<u64>,
    ) -> UsageReport {
        let mut usage = serde_json::Map::new();
        usage.insert("input_tokens".to_string(), serde_json::json!(10));
        usage.insert("cache_read_input_tokens".to_string(), serde_json::json!(20));
        usage.insert("output_tokens".to_string(), serde_json::json!(3));
        if let Some(write) = write {
            usage.insert(
                "cache_creation_input_tokens".to_string(),
                serde_json::json!(write),
            );
        }
        let mut creation = serde_json::Map::new();
        if let Some(tokens) = ephemeral_5m {
            creation.insert(
                "ephemeral_5m_input_tokens".to_string(),
                serde_json::json!(tokens),
            );
        }
        if let Some(tokens) = ephemeral_1h {
            creation.insert(
                "ephemeral_1h_input_tokens".to_string(),
                serde_json::json!(tokens),
            );
        }
        if !creation.is_empty() {
            usage.insert(
                "cache_creation".to_string(),
                serde_json::Value::Object(creation),
            );
        }
        let mut report = UsageReport::default();
        report.add_event(
            &serde_json::json!({
                "type": "message_delta",
                "usage": serde_json::Value::Object(usage),
            }),
            true,
        );
        report
    }

    #[test]
    fn the_two_cache_lifetimes_break_the_write_down_without_adding_to_the_prompt() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        monitor.usage_reported("r1", cache_creation_usage(Some(900), Some(400), Some(500)));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.cache.write_tokens, Some(900));
        assert_eq!(request.cache.write_5m_tokens, Some(400));
        assert_eq!(request.cache.write_1h_tokens, Some(500));
        // The breakdown is the same tokens as the write, so the prompt is the
        // three categories and nothing else.
        assert_eq!(request.prompt_tokens(), Some(930));
        assert_eq!(
            request.cache_write_quality(),
            CacheWriteQuality {
                ephemeral_5m: UsageQuality::Exact,
                ephemeral_1h: UsageQuality::Exact,
            }
        );

        let session = &state.sessions[0];
        assert_eq!(session.cache_write_tokens, 900);
        assert_eq!(session.cache_write_5m_tokens, 400);
        assert_eq!(session.cache_write_1h_tokens, 500);
        assert_eq!(session.evidence.cache_write.exact, 1);
        assert_eq!(session.evidence.cache_write_5m.exact, 1);
        assert_eq!(session.evidence.cache_write_1h.exact, 1);
        // The hit ratio is read off the prompt categories, which the breakdown
        // is not one of.
        let ratio = session.cache_hit_ratio().unwrap();
        assert!((ratio - 20.0 / 930.0).abs() < 1e-9, "ratio was {ratio}");
    }

    #[test]
    fn one_lifetime_reported_leaves_the_other_missing_rather_than_derived() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        // Only the hour bucket is named, and it is smaller than the write: the
        // rest is not evidence for the five-minute bucket.
        monitor.usage_reported("r1", cache_creation_usage(Some(900), None, Some(500)));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.cache.write_tokens, Some(900));
        assert_eq!(request.cache.write_5m_tokens, None);
        assert_eq!(request.cache.write_1h_tokens, Some(500));
        assert_eq!(
            request.cache_write_quality(),
            CacheWriteQuality {
                ephemeral_5m: UsageQuality::Missing,
                ephemeral_1h: UsageQuality::Exact,
            }
        );
        let session = &state.sessions[0];
        assert_eq!(session.cache_write_5m_tokens, 0);
        assert_eq!(session.cache_write_1h_tokens, 500);
        assert_eq!(session.evidence.cache_write_5m.missing, 1);
        assert_eq!(session.evidence.cache_write_1h.exact, 1);
        // Knowing one bucket says nothing about the whole write's quality.
        assert_eq!(session.evidence.cache_write.exact, 1);
    }

    #[test]
    fn a_reported_lifetime_zero_is_evidence_and_an_absent_one_is_not() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "zero", "claude-opus-5");
        monitor.usage_reported("zero", cache_creation_usage(Some(700), Some(0), Some(700)));
        monitor.request_completed("zero", 200, None, None);
        // A response that wrote nothing and said nothing about lifetimes.
        start_anthropic_request(&monitor, "silent", "claude-opus-5");
        monitor.usage_reported("silent", cache_creation_usage(Some(0), None, None));
        monitor.request_completed("silent", 200, None, None);

        let state = monitor.snapshot();
        assert_eq!(recent_by_id(&state, "zero").cache.write_5m_tokens, Some(0));
        assert_eq!(recent_by_id(&state, "silent").cache.write_5m_tokens, None);
        let session = &state.sessions[0];
        assert_eq!(session.cache_write_5m_tokens, 0);
        assert_eq!(session.cache_write_1h_tokens, 700);
        assert_eq!(session.evidence.cache_write_5m.exact, 1);
        assert_eq!(session.evidence.cache_write_5m.missing, 1);
        assert_eq!(session.evidence.cache_write_1h.exact, 1);
        assert_eq!(session.evidence.cache_write_1h.missing, 1);
    }

    #[test]
    fn a_later_report_lowers_the_lifetime_buckets_it_raised_and_counts_once() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        monitor.stream_progress_usage(
            "r1",
            100,
            1,
            cache_creation_usage(Some(900), Some(400), Some(500)),
        );
        assert_eq!(monitor.snapshot().sessions[0].cache_write_5m_tokens, 400);

        // The final event corrects both buckets downwards.
        monitor.usage_reported("r1", cache_creation_usage(Some(300), Some(100), Some(200)));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let session = &state.sessions[0];
        assert_eq!(session.cache_write_tokens, 300);
        assert_eq!(session.cache_write_5m_tokens, 100);
        assert_eq!(session.cache_write_1h_tokens, 200);
        // One request behind each count, not two.
        assert_eq!(session.evidence.cache_write_5m.requests(), 1);
        assert_eq!(session.evidence.cache_write_1h.requests(), 1);
        assert_eq!(
            model_row(session, Some("anthropic"), None).cache_write_5m_tokens,
            100
        );
    }

    #[test]
    fn a_breakdown_alone_does_not_move_the_write_it_belongs_to() {
        let monitor = MonitorHandle::new(10);
        start_anthropic_request(&monitor, "r1", "claude-opus-5");
        // No aggregate in this report: the write itself stays unknown.
        monitor.usage_reported("r1", cache_creation_usage(None, Some(400), Some(500)));
        monitor.request_completed("r1", 200, None, None);

        let state = monitor.snapshot();
        let request = recent_by_id(&state, "r1");
        assert_eq!(request.cache.write_tokens, None);
        assert_eq!(request.cache.write_5m_tokens, Some(400));
        assert_eq!(request.cache.write_1h_tokens, Some(500));
        let session = &state.sessions[0];
        assert_eq!(session.cache_write_tokens, 0);
        assert_eq!(session.evidence.cache_write.missing, 1);
        assert_eq!(session.cache_write_5m_tokens, 400);
        assert_eq!(session.cache_write_1h_tokens, 500);
    }
}
