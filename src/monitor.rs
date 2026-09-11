use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

mod mock;
mod usage;

pub use mock::{MockMonitor, mock_state};
pub use usage::{
    CacheMiss, CacheMissCause, UsageFields, UsageReport, caches_implicitly, default_cache_ttl,
    detect_cache_miss, usage_report_from_anthropic_body, usage_report_from_anthropic_sse,
};
use usage::{ClosedFields, UsageDelta, add_signed, apply_closing, apply_opening};

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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    ProviderSelected {
        request_id: String,
        provider: String,
        model: String,
        effort: Option<String>,
    },
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
    /// the Claude Code agent id of a subagent.
    ConversationResolved {
        request_id: String,
        conversation: String,
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
    /// Set when the request's final cache read fell well short of the previous
    /// prompt in its conversation lane.
    pub miss: Option<CacheMiss>,
    /// Lifetime of the cache entries the request wrote, when the response said.
    pub ttl: Option<Duration>,
    closed: ClosedFields,
    evaluated: bool,
}

impl RequestCache {
    /// Whether the request has been compared with the previous one of its lane.
    pub fn evaluated(&self) -> bool {
        self.evaluated
    }
}

/// Prompt size of a request: uncached input plus cache reads and writes.
fn prompt_tokens(input_tokens: Option<u64>, cache: &RequestCache) -> Option<u64> {
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
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    started_instant: Instant,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
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
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
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

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub last_seen: SystemTime,
    /// Uncached input tokens; `count_tokens` estimates are not counted.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache: SessionCacheStats,
    pub output_token_samples: Vec<(SystemTime, u64)>,
    rate_output_tokens: u64,
    pub generation_duration: Duration,
    pub last_status: String,
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

impl SessionSummary {
    /// Share of all prompt tokens served from cache.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let prompt = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens);
        (prompt > 0 && (self.cache_read_tokens > 0 || self.cache_write_tokens > 0))
            .then(|| self.cache_read_tokens as f64 / prompt as f64)
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
    session_usage: HashMap<Option<String>, SessionUsage>,
    session_cache: HashMap<Option<String>, SessionCacheStats>,
    session_output_buckets: HashMap<Option<String>, Vec<(u64, u64)>>,
    lanes: HashMap<LaneKey, LaneState>,
    recent_limit: usize,
}

#[derive(Debug, Default)]
struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
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
        if report.cache_ttl.is_some() {
            cache.ttl = report.cache_ttl;
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
                session_usage: HashMap::new(),
                session_cache: HashMap::new(),
                session_output_buckets: HashMap::new(),
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
    ) {
        self.publish(MonitorEvent::ConversationResolved {
            request_id: request_id.into(),
            conversation: conversation.into(),
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
                self.active.insert(
                    request_id.clone(),
                    ActiveRequest {
                        request_id,
                        session_id,
                        conversation: None,
                        session_seq,
                        project: None,
                        provider: None,
                        model: None,
                        effort: None,
                        endpoint,
                        started_at: SystemTime::now(),
                        started_instant: Instant::now(),
                        generation_started_at: None,
                        generation_started_instant: None,
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
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.provider = Some(provider);
                    active.model = Some(model);
                    active.effort = effort;
                    active.status = RequestStatus::ProviderSelected;
                }
            }
            MonitorEvent::ModelResolved { request_id, model } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.model = Some(match active.model.take() {
                        Some(incoming) if incoming != model => format!("{incoming} → {model}"),
                        Some(incoming) => incoming,
                        None => model,
                    });
                }
            }
            MonitorEvent::CompactionStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Compacting;
                }
            }
            MonitorEvent::UpstreamStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Upstream;
                }
            }
            MonitorEvent::GenerationStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.generation_started_at = Some(SystemTime::now());
                    active.generation_started_instant = Some(Instant::now());
                    active.generation_initial_output_tokens = active.output_tokens.unwrap_or(0);
                    active.generation_finished_at = None;
                    active.generation_duration = None;
                }
            }
            MonitorEvent::TrafficCapturePath { request_id, path } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.traffic_capture_path = Some(path);
                }
            }
            MonitorEvent::ConversationResolved {
                request_id,
                conversation,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.conversation = Some(conversation);
                }
            }
            MonitorEvent::StreamProgress {
                request_id,
                bytes,
                chunks,
                usage,
            } => {
                let output_seen = usage.closing.output_tokens.or(usage.opening.output_tokens);
                let mut usage_update = None;
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Streaming;
                    if active.generation_started_instant.is_none() {
                        active.generation_started_at = Some(SystemTime::now());
                        active.generation_started_instant = Some(Instant::now());
                        active.generation_initial_output_tokens =
                            output_seen.or(active.output_tokens).unwrap_or(0);
                    } else {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = active
                            .generation_started_instant
                            .map(|started| started.elapsed());
                    }
                    active.streamed_bytes = active.streamed_bytes.saturating_add(bytes);
                    active.stream_chunks = active.stream_chunks.saturating_add(chunks);
                    let delta = UsageTarget {
                        input_tokens: &mut active.input_tokens,
                        output_tokens: &mut active.output_tokens,
                        cache: &mut active.cache,
                    }
                    .apply(&usage);
                    usage_update = Some((active.session_id.clone(), active.endpoint, delta));
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    if let Some(started) = completed.generation_started_instant {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    completed.streamed_bytes = completed.streamed_bytes.saturating_add(bytes);
                    completed.stream_chunks = completed.stream_chunks.saturating_add(chunks);
                    let delta = UsageTarget {
                        input_tokens: &mut completed.input_tokens,
                        output_tokens: &mut completed.output_tokens,
                        cache: &mut completed.cache,
                    }
                    .apply(&usage);
                    usage_update = Some((completed.session_id.clone(), completed.endpoint, delta));
                    if delta.output > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            delta.output.unsigned_abs(),
                        ));
                    }
                }
                if let Some((session_id, endpoint, delta)) = usage_update {
                    self.record_session_usage(session_id, endpoint, delta);
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
                }
                self.evaluate_cache(&request_id);
            }
            MonitorEvent::UsageUpdated { request_id, usage } => {
                let output_seen = usage.closing.output_tokens.or(usage.opening.output_tokens);
                let mut usage_update = None;
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    if output_seen.is_some()
                        && let Some(started) = active.generation_started_instant
                    {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = Some(started.elapsed());
                    }
                    let delta = UsageTarget {
                        input_tokens: &mut active.input_tokens,
                        output_tokens: &mut active.output_tokens,
                        cache: &mut active.cache,
                    }
                    .apply(&usage);
                    usage_update = Some((active.session_id.clone(), active.endpoint, delta));
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    if output_seen.is_some()
                        && let Some(started) = completed.generation_started_instant
                    {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    let delta = UsageTarget {
                        input_tokens: &mut completed.input_tokens,
                        output_tokens: &mut completed.output_tokens,
                        cache: &mut completed.cache,
                    }
                    .apply(&usage);
                    usage_update = Some((completed.session_id.clone(), completed.endpoint, delta));
                    if delta.output > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            delta.output.unsigned_abs(),
                        ));
                    }
                }
                if let Some((session_id, endpoint, delta)) = usage_update {
                    self.record_session_usage(session_id, endpoint, delta);
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
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
        let mut active = self
            .active
            .remove(request_id)
            .unwrap_or_else(|| ActiveRequest {
                request_id: request_id.to_string(),
                session_id: None,
                conversation: None,
                session_seq: None,
                project: None,
                provider: None,
                model: None,
                effort: None,
                endpoint: EndpointKind::Messages,
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                generation_started_at: None,
                generation_started_instant: None,
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
        if output_tokens.is_some()
            && let Some(started) = active.generation_started_instant
        {
            active.generation_finished_at = Some(SystemTime::now());
            active.generation_duration = Some(started.elapsed());
        }
        let delta = UsageTarget {
            input_tokens: &mut active.input_tokens,
            output_tokens: &mut active.output_tokens,
            cache: &mut active.cache,
        }
        .apply(&UsageReport::opening(input_tokens, output_tokens));
        self.record_session_usage(active.session_id.clone(), active.endpoint, delta);
        let completed = CompletedRequest {
            request_id: active.request_id,
            session_id: active.session_id,
            conversation: active.conversation,
            session_seq: active.session_seq,
            project: active.project,
            provider: active.provider,
            model: active.model,
            effort: active.effort,
            endpoint: active.endpoint,
            started_at: active.started_at,
            finished_at: SystemTime::now(),
            generation_started_at: active.generation_started_at,
            generation_started_instant: active.generation_started_instant,
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
        if let Some(tokens) = completed.output_tokens.filter(|tokens| *tokens > 0) {
            self.record_session_output(
                completed.session_id.clone(),
                completed
                    .generation_finished_at
                    .unwrap_or(completed.finished_at),
                tokens,
            );
        }
        self.recent.push_front(completed);
        while self.recent.len() > self.recent_limit {
            self.recent.pop_back();
        }
        self.evaluate_cache(request_id);
    }

    /// Add a request's usage change to its session. `count_tokens` requests are
    /// local estimates of a prompt that the real request counts again, so they
    /// stay out of the session totals.
    fn record_session_usage(
        &mut self,
        session_id: Option<String>,
        endpoint: EndpointKind,
        delta: UsageDelta,
    ) {
        if endpoint == EndpointKind::CountTokens || delta.is_zero() {
            return;
        }
        let usage = self.session_usage.entry(session_id).or_default();
        usage.input_tokens = add_signed(usage.input_tokens, delta.input);
        usage.output_tokens = add_signed(usage.output_tokens, delta.output);
        usage.cache_read_tokens = add_signed(usage.cache_read_tokens, delta.cache_read);
        usage.cache_write_tokens = add_signed(usage.cache_write_tokens, delta.cache_write);
    }

    /// Once a request's cache read is final, compare it with the previous
    /// request of its lane, record a miss, and make it the lane's new baseline.
    fn evaluate_cache(&mut self, request_id: &str) {
        let (key, prompt, cache, started_at, response_started_at) = {
            let request = if let Some(active) = self.active.get(request_id) {
                (
                    &active.session_id,
                    &active.conversation,
                    &active.provider,
                    &active.model,
                    active.input_tokens,
                    active.cache,
                    active.started_at,
                    active.generation_started_at,
                    active.endpoint,
                )
            } else if let Some(completed) = self
                .recent
                .iter()
                .find(|request| request.request_id == request_id)
            {
                (
                    &completed.session_id,
                    &completed.conversation,
                    &completed.provider,
                    &completed.model,
                    completed.input_tokens,
                    completed.cache,
                    completed.started_at,
                    completed.generation_started_at,
                    completed.endpoint,
                )
            } else {
                return;
            };
            let (
                session_id,
                conversation,
                provider,
                model,
                input,
                cache,
                started_at,
                response,
                endpoint,
            ) = request;
            if cache.evaluated || !cache.closed.cache_read || endpoint == EndpointKind::CountTokens
            {
                return;
            }
            let (Some(provider), Some(model)) = (provider.clone(), model.clone()) else {
                return;
            };
            (
                LaneKey {
                    session_id: session_id.clone(),
                    conversation: conversation.clone(),
                    provider,
                    model,
                },
                prompt_tokens(input, &cache).unwrap_or(0),
                cache,
                started_at,
                response,
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
        let judged = !superseded && !side;
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

        let stats = self
            .session_cache
            .entry(key.session_id.clone())
            .or_default();
        if !superseded
            && key
                .conversation
                .as_deref()
                .is_none_or(|name| name == "main")
        {
            stats.context_tokens = prompt;
            stats.peak_context_tokens = stats.peak_context_tokens.max(prompt);
            stats.context_started_at = Some(started_at);
            stats.context_ttl = self
                .lanes
                .get(&key)
                .and_then(|lane| lane.ttl)
                .or(default_cache_ttl(&key.provider));
        }
        if let Some(miss) = miss {
            stats.miss_count = stats.miss_count.saturating_add(1);
            stats.missed_tokens = stats.missed_tokens.saturating_add(miss.missed_tokens);
            stats.last_miss = Some((started_at, miss));
        }

        let cache = if let Some(active) = self.active.get_mut(request_id) {
            &mut active.cache
        } else if let Some(completed) = self
            .recent
            .iter_mut()
            .find(|request| request.request_id == request_id)
        {
            &mut completed.cache
        } else {
            return;
        };
        cache.evaluated = true;
        cache.miss = miss;
    }

    fn record_session_output(
        &mut self,
        session_id: Option<String>,
        timestamp: SystemTime,
        tokens: u64,
    ) {
        let bucket = session_token_bucket(timestamp);
        let buckets = self.session_output_buckets.entry(session_id).or_default();
        match buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
            Ok(index) => buckets[index].1 = buckets[index].1.saturating_add(tokens),
            Err(index) => buckets.insert(index, (bucket, tokens)),
        }
    }

    fn snapshot(&self) -> MonitorState {
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active.sort_by_key(|request| request.started_at);
        let sessions = session_summaries(
            &active,
            &self.recent,
            &self.session_usage,
            &self.session_cache,
            &self.session_output_buckets,
        );
        MonitorState {
            started_at: self.started_at,
            sessions,
            active,
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

fn session_summaries(
    active: &[ActiveRequest],
    recent: &VecDeque<CompletedRequest>,
    session_usage: &HashMap<Option<String>, SessionUsage>,
    session_cache: &HashMap<Option<String>, SessionCacheStats>,
    session_output_buckets: &HashMap<Option<String>, Vec<(u64, u64)>>,
) -> Vec<SessionSummary> {
    let mut sessions: HashMap<Option<String>, SessionSummary> = HashMap::new();
    for request in recent.iter().rev() {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.finished_at,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache: SessionCacheStats::default(),
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.request_count += 1;
        if request.status == RequestStatus::Failed {
            entry.failure_count += 1;
        }
        entry.project = request.project.clone().or(entry.project.clone());
        entry.provider = request.provider.clone().or(entry.provider.clone());
        entry.model = request.model.clone().or(entry.model.clone());
        entry.effort = request.effort.clone().or(entry.effort.clone());
        entry.last_seen = max_system_time(entry.last_seen, request.finished_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    for request in active {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.started_at,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache: SessionCacheStats::default(),
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.active_count += 1;
        entry.request_count += 1;
        entry.project = request.project.clone().or(entry.project.clone());
        entry.provider = request.provider.clone().or(entry.provider.clone());
        entry.model = request.model.clone().or(entry.model.clone());
        entry.effort = request.effort.clone().or(entry.effort.clone());
        entry.last_seen = max_system_time(entry.last_seen, request.started_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    for (session_id, session) in &mut sessions {
        if let Some(usage) = session_usage.get(session_id) {
            session.input_tokens = usage.input_tokens;
            session.output_tokens = usage.output_tokens;
            session.cache_read_tokens = usage.cache_read_tokens;
            session.cache_write_tokens = usage.cache_write_tokens;
        }
        if let Some(stats) = session_cache.get(session_id) {
            session.cache = *stats;
        }
        if let Some(buckets) = session_output_buckets.get(session_id) {
            session.output_token_samples = buckets
                .iter()
                .map(|(bucket, tokens)| (session_token_bucket_start(*bucket), *tokens))
                .collect();
        }
    }

    let mut out: Vec<_> = sessions.into_values().collect();
    out.sort_by_key(SessionSummary::label);
    out
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

fn max_system_time(left: SystemTime, right: SystemTime) -> SystemTime {
    if right.duration_since(left).is_ok() {
        right
    } else {
        left
    }
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
            session_seq: None,
            project: None,
            provider: Some("codex".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            effort: None,
            endpoint: EndpointKind::Messages,
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: SystemTime::UNIX_EPOCH + latency,
            generation_started_at: generation_duration.map(|_| SystemTime::UNIX_EPOCH),
            generation_started_instant: None,
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

    fn session_summaries_for_requests(recent: &VecDeque<CompletedRequest>) -> Vec<SessionSummary> {
        let mut usage = HashMap::<Option<String>, SessionUsage>::new();
        for request in recent {
            let entry = usage.entry(request.session_id.clone()).or_default();
            entry.input_tokens = entry
                .input_tokens
                .saturating_add(request.input_tokens.unwrap_or(0));
            entry.output_tokens = entry
                .output_tokens
                .saturating_add(request.output_tokens.unwrap_or(0));
        }
        session_summaries(&[], recent, &usage, &HashMap::new(), &HashMap::new())
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

    #[test]
    fn session_order_is_stable_across_activity() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("session-b".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_started(
            "r2",
            Some("session-a".to_string()),
            Some(1),
            EndpointKind::Messages,
        );

        let first: Vec<_> = monitor
            .snapshot()
            .sessions
            .iter()
            .map(SessionSummary::label)
            .collect();
        monitor.request_completed("r1", 200, None, None);
        monitor.request_started(
            "r3",
            Some("session-b".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        let second: Vec<_> = monitor
            .snapshot()
            .sessions
            .iter()
            .map(SessionSummary::label)
            .collect();

        assert_eq!(first, vec!["session-a", "session-b"]);
        assert_eq!(second, first);
    }

    fn closing_usage(input: u64, read: u64, write: u64, output: u64) -> UsageReport {
        UsageReport {
            closing: UsageFields {
                input_tokens: Some(input),
                cache_read_tokens: Some(read),
                cache_write_tokens: Some(write),
                output_tokens: Some(output),
            },
            ..UsageReport::default()
        }
    }

    fn start_codex_request(monitor: &MonitorHandle, request_id: &str, conversation: &str) {
        monitor.request_started(
            request_id,
            Some("s1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved(request_id, conversation);
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
    }

    #[test]
    fn idle_gap_beyond_the_reported_ttl_is_an_expiry() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", Some("s1".to_string()), None, EndpointKind::Messages);
        monitor.provider_selected("r1", "anthropic", "claude-opus-5", None);
        if let Ok(mut store) = monitor.store.lock()
            && let Some(active) = store.active.get_mut("r1")
        {
            active.started_at = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        }
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
        monitor.conversation_resolved(request_id, "main");
        monitor.provider_selected(request_id, "anthropic", model, None);
    }

    fn backdate(monitor: &MonitorHandle, request_id: &str, ago: Duration) {
        if let Ok(mut store) = monitor.store.lock()
            && let Some(active) = store.active.get_mut(request_id)
        {
            active.started_at = SystemTime::now() - ago;
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
}
