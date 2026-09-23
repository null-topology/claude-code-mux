//! Lifetime accounting: what the monitor knows about a request once its detail
//! is gone.
//!
//! The recent list holds the last few hundred requests in full and nothing
//! else. Counting, attribution and token totals outlive it here: one compact
//! numeric record per request id, and the session and conversation rows those
//! records add up to. A report arriving after a request left the recent list
//! still lands on its record and corrects every total it fed, which the live
//! Codex path needs: it hands the response to the client before the stream
//! ends, so the backend's own counts can arrive arbitrarily late.
//!
//! A record holds numbers and small metadata only: no prompt, no request or
//! response body, no output text. It lives until the process restarts, so the
//! map grows with the number of requests served. That is the deliberate price
//! of keeping those late corrections right.
//!
//! Every change to a record happens between taking its contribution out of the
//! rows it feeds and putting it back ([`Ledger::update`]), so a row is always
//! the sum of the records attached to it, corrections and moves between rows
//! included. A change worth nothing numerically still matters: a count moving
//! from missing to a reported zero changes the evidence behind a total without
//! changing the total.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant, SystemTime},
};

use super::usage::{
    CacheMiss, CacheMissCause, CacheWriteQuality, QualityFields, UsageDelta, UsageQuality,
    UsageReport, add_signed, caches_implicitly,
};
use super::{
    ActiveRequest, CompletedRequest, EndpointKind, RequestCache, RequestStatus, SessionCacheStats,
    SessionUsage, UsageTarget, cache_write_quality, note_context, note_miss, prompt_tokens,
    session_token_bucket, usage_quality,
};

/// The provider name of a request the proxy answered itself. Its counts are
/// synthetic: no model produced them, so they stay out of every token total.
/// `server.rs` publishes this name when it answers an agent summary locally.
pub const LOCAL_PROVIDER: &str = "local";

/// How firmly the requests behind one accumulated count know it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualityCoverage {
    /// Requests that never reported this count at all.
    pub missing: u64,
    /// Requests holding an estimate a closing report could still replace.
    pub opening: u64,
    /// Requests whose backend reported the count itself.
    pub exact: u64,
}

impl QualityCoverage {
    /// How many requests the count is made of, however well each is known.
    pub fn requests(&self) -> u64 {
        self.missing
            .saturating_add(self.opening)
            .saturating_add(self.exact)
    }

    /// Whether every request behind the count reported it.
    pub fn is_exact(&self) -> bool {
        self.missing == 0 && self.opening == 0
    }

    fn note(&mut self, quality: UsageQuality, sign: i64) {
        let slot = match quality {
            UsageQuality::Missing => &mut self.missing,
            UsageQuality::Opening => &mut self.opening,
            UsageQuality::Exact => &mut self.exact,
        };
        *slot = add_count(*slot, sign);
    }

    /// Fold another row's coverage of the same count into this one.
    pub(crate) fn add(&mut self, other: &Self) {
        self.missing = self.missing.saturating_add(other.missing);
        self.opening = self.opening.saturating_add(other.opening);
        self.exact = self.exact.saturating_add(other.exact);
    }
}

/// The evidence behind one row's four accumulated counts, and the prompt totals
/// the backends measured themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageEvidence {
    pub input: QualityCoverage,
    pub cache_read: QualityCoverage,
    pub cache_write: QualityCoverage,
    pub output: QualityCoverage,
    /// How well the two lifetime buckets of the cache write are known. They are
    /// a breakdown of `cache_write` and carry their own evidence: a response
    /// can report the write without saying how it split, and a bucket it did
    /// name says nothing about the bucket it did not.
    pub cache_write_5m: QualityCoverage,
    pub cache_write_1h: QualityCoverage,
    /// Requests whose prompt size is a total the backend reported on its own.
    pub reported_prompt_requests: u64,
    /// Those totals added up. They hold the same tokens the four categories do,
    /// so they are kept apart from them and never added to a category.
    pub reported_prompt_tokens: u64,
}

impl UsageEvidence {
    fn note(
        &mut self,
        quality: QualityFields,
        cache_write: CacheWriteQuality,
        reported_prompt: Option<u64>,
        sign: i64,
    ) {
        self.input.note(quality.input, sign);
        self.cache_read.note(quality.cache_read, sign);
        self.cache_write.note(quality.cache_write, sign);
        self.output.note(quality.output, sign);
        self.cache_write_5m.note(cache_write.ephemeral_5m, sign);
        self.cache_write_1h.note(cache_write.ephemeral_1h, sign);
        if let Some(tokens) = reported_prompt {
            self.reported_prompt_requests = add_count(self.reported_prompt_requests, sign);
            self.reported_prompt_tokens = add_tokens(self.reported_prompt_tokens, tokens, sign);
        }
    }

    /// Fold the evidence behind another row into this one, count by count.
    pub(crate) fn add(&mut self, other: &Self) {
        self.input.add(&other.input);
        self.cache_read.add(&other.cache_read);
        self.cache_write.add(&other.cache_write);
        self.output.add(&other.output);
        self.cache_write_5m.add(&other.cache_write_5m);
        self.cache_write_1h.add(&other.cache_write_1h);
        self.reported_prompt_requests = self
            .reported_prompt_requests
            .saturating_add(other.reported_prompt_requests);
        self.reported_prompt_tokens = self
            .reported_prompt_tokens
            .saturating_add(other.reported_prompt_tokens);
    }
}

/// How many requests of a row were judged a cache miss, by cause. A tally of
/// verdicts already passed on each request against the previous one of its
/// lane; it judges nothing itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheMissTally {
    /// The previous request was recent enough for its prefix to be alive.
    pub within_ttl: u64,
    /// The lane was idle longer than the cache lifetime.
    pub expired: u64,
    /// The provider's cache lifetime is not known.
    pub unknown_ttl: u64,
}

impl CacheMissTally {
    fn note(&mut self, cause: CacheMissCause, sign: i64) {
        let slot = match cause {
            CacheMissCause::WithinTtl => &mut self.within_ttl,
            CacheMissCause::Expired => &mut self.expired,
            CacheMissCause::UnknownTtl => &mut self.unknown_ttl,
        };
        *slot = add_count(*slot, sign);
    }

    pub fn total(&self) -> u64 {
        self.within_ttl
            .saturating_add(self.expired)
            .saturating_add(self.unknown_ttl)
    }

    pub(crate) fn add(&mut self, other: &Self) {
        self.within_ttl = self.within_ttl.saturating_add(other.within_ttl);
        self.expired = self.expired.saturating_add(other.expired);
        self.unknown_ttl = self.unknown_ttl.saturating_add(other.unknown_ttl);
    }
}

/// What requests that named no conversation add up to, counted on their own
/// rather than left as the difference between a session and its rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnattributedUsage {
    pub request_count: usize,
    pub failure_count: usize,
    pub active_count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub evidence: UsageEvidence,
}

/// What one session spent on one backend and one model that actually ran.
///
/// The row is keyed on the model a producer saw leave, not on what the caller
/// typed and not on the display that pairs the two: a request whose model was
/// rewritten on the way costs the model that ran it. Requests whose wire model
/// was never observed — the proxy answered them itself, or they failed before a
/// request was built — hold a row with no model rather than being folded into
/// the last model the session used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelUsage {
    pub provider: Option<String>,
    /// The model on the wire, or `None` where none was observed.
    pub model: Option<String>,
    /// Where the row sits among the ones of its session seen before it.
    pub first_seen_rank: usize,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub evidence: UsageEvidence,
    /// How many of the row's requests were judged a cache miss, by cause.
    pub misses: CacheMissTally,
    /// The models the callers asked for to get here, and how many requests
    /// each accounts for, by name. A request that named no model is counted
    /// under `None`.
    pub requested_models: Vec<(Option<String>, usize)>,
}

/// The figures of one row: how many requests it is made of, what they cost, and
/// how well that is known.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RowCounts {
    pub request_count: u64,
    pub failure_count: u64,
    pub active_count: u64,
    pub usage: SessionUsage,
    pub evidence: UsageEvidence,
}

impl RowCounts {
    /// Add one request's whole contribution, or take it back out.
    fn apply(&mut self, contribution: &Contribution, sign: i64) {
        self.request_count = add_count(self.request_count, sign);
        if contribution.failed {
            self.failure_count = add_count(self.failure_count, sign);
        }
        if contribution.active {
            self.active_count = add_count(self.active_count, sign);
        }
        if !contribution.counts_tokens {
            return;
        }
        apply_usage_totals(&mut self.usage, &contribution.usage, sign);
        self.evidence.note(
            contribution.quality,
            contribution.cache_write_quality,
            contribution.reported_prompt_tokens,
            sign,
        );
    }

    /// Whether the row still holds a request. Everything a row shows comes from
    /// the contributions attached to it, so a row no request is attached to
    /// holds nothing at all.
    fn is_empty(&self) -> bool {
        self.request_count == 0
    }

    fn as_unattributed(&self) -> UnattributedUsage {
        UnattributedUsage {
            request_count: as_usize(self.request_count),
            failure_count: as_usize(self.failure_count),
            active_count: as_usize(self.active_count),
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cache_read_tokens: self.usage.cache_read_tokens,
            cache_write_tokens: self.usage.cache_write_tokens,
            cache_write_5m_tokens: self.usage.cache_write_5m_tokens,
            cache_write_1h_tokens: self.usage.cache_write_1h_tokens,
            evidence: self.evidence,
        }
    }
}

/// Everything one request adds to the rows it belongs to, read off its record.
struct Contribution {
    failed: bool,
    active: bool,
    counts_tokens: bool,
    usage: SessionUsage,
    quality: QualityFields,
    cache_write_quality: CacheWriteQuality,
    reported_prompt_tokens: Option<u64>,
    /// The verdict its lane evaluation passed, once one did.
    miss: Option<CacheMissCause>,
}

/// Where a request sits among the ones the process has served: when the client
/// sent it, and, for two sent in the same instant, the order they were seen in.
/// A row shows the metadata of the request with the greatest order, so a report
/// arriving late for an older request cannot take a row's model or status back.
type RequestOrder = (SystemTime, u64);

/// What the monitor keeps about one request until the process restarts.
#[derive(Debug, Clone)]
pub(crate) struct RequestRecord {
    /// Where the request sits among the ones seen before it.
    pub rank: u64,
    pub session_id: Option<String>,
    pub conversation: Option<String>,
    pub conversation_parent: Option<String>,
    pub project: Option<String>,
    pub provider: Option<String>,
    /// The routed model, and the wire model appended once a producer named it:
    /// what the visible row shows. No total is keyed on it.
    pub model: Option<String>,
    /// The model the client asked for, before any rewrite of the proxy's.
    pub requested_model: Option<String>,
    /// The model that went on the wire, when a producer saw the request leave.
    pub effective_model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub status: RequestStatus,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub generation_started_at: Option<SystemTime>,
    pub generation_started_instant: Option<Instant>,
    pub generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache: RequestCache,
    /// The output-history buckets this request put tokens in, oldest first, and
    /// how many of each bucket's tokens are its own. Enough to take a negative
    /// correction back out of the buckets it went into, and nothing more: no
    /// output text is kept.
    output_buckets: Vec<(u64, u64)>,
    /// Whether a terminal event has already been counted for it.
    terminal: bool,
}

impl RequestRecord {
    fn new(
        rank: u64,
        session_id: Option<String>,
        endpoint: EndpointKind,
        started_at: SystemTime,
    ) -> Self {
        Self {
            rank,
            session_id,
            conversation: None,
            conversation_parent: None,
            project: None,
            provider: None,
            model: None,
            requested_model: None,
            effective_model: None,
            effort: None,
            endpoint,
            status: RequestStatus::Started,
            started_at,
            finished_at: None,
            generation_started_at: None,
            generation_started_instant: None,
            generation_initial_output_tokens: 0,
            generation_finished_at: None,
            generation_duration: None,
            input_tokens: None,
            output_tokens: None,
            cache: RequestCache::default(),
            output_buckets: Vec::new(),
            terminal: false,
        }
    }

    /// Whether the request's tokens belong in a session's upstream totals. A
    /// `count_tokens` estimate counts a prompt the real request counts again,
    /// and a request the proxy answered itself never reached a model; both stay
    /// visible as requests, with their own numbers, and out of every total.
    pub(crate) fn counts_tokens(&self) -> bool {
        self.endpoint != EndpointKind::CountTokens
            && self.provider.as_deref() != Some(LOCAL_PROVIDER)
    }

    pub(crate) fn quality(&self) -> QualityFields {
        usage_quality(self.input_tokens, self.output_tokens, &self.cache)
    }

    /// The row this request's cost belongs to: the backend it went to and the
    /// model that actually ran, neither of them invented where the request
    /// never said.
    fn model_key(&self) -> ModelKey {
        ModelKey {
            provider: self.provider.clone(),
            model: self.effective_model.clone(),
        }
    }

    pub(crate) fn prompt_tokens(&self) -> Option<u64> {
        prompt_tokens(self.input_tokens, &self.cache)
    }

    /// Whether the prompt size is the backend's own measurement rather than a
    /// sum with a hole in it. A report naming only the cached part sizes no
    /// prompt: taking its partial sum for the whole would invent a shrunken
    /// context and a cache miss that never happened.
    pub(crate) fn prompt_is_measured(&self) -> bool {
        if self.cache.reported_prompt_tokens.is_some() {
            return true;
        }
        let implicit = self.provider.as_deref().is_some_and(caches_implicitly);
        self.cache.closed.input
            && self.cache.closed.cache_read
            && (self.cache.closed.cache_write || implicit)
    }

    /// When the output tokens a report just added were generated, as far as the
    /// monitor saw: the end of the generation interval, else the moment the
    /// request finished, else now.
    fn output_timestamp(&self, now: SystemTime) -> SystemTime {
        self.generation_finished_at
            .or(self.finished_at)
            .unwrap_or(now)
    }

    /// When the request last showed activity, for the rows it feeds.
    fn seen_at(&self) -> SystemTime {
        self.finished_at.unwrap_or(self.started_at)
    }

    /// Where the request sits in the logical order of its rows.
    fn order(&self) -> RequestOrder {
        (self.started_at, self.rank)
    }

    fn contribution(&self) -> Contribution {
        let counts_tokens = self.counts_tokens();
        Contribution {
            failed: self.status == RequestStatus::Failed,
            active: !self.terminal,
            counts_tokens,
            usage: SessionUsage {
                input_tokens: self.input_tokens.unwrap_or(0),
                output_tokens: self.output_tokens.unwrap_or(0),
                cache_read_tokens: self.cache.read_tokens.unwrap_or(0),
                cache_write_tokens: self.cache.write_tokens.unwrap_or(0),
                cache_write_5m_tokens: self.cache.write_5m_tokens.unwrap_or(0),
                cache_write_1h_tokens: self.cache.write_1h_tokens.unwrap_or(0),
            },
            quality: self.quality(),
            cache_write_quality: cache_write_quality(&self.cache),
            reported_prompt_tokens: self.cache.reported_prompt_tokens,
            miss: self.cache.miss.map(|miss| miss.cause),
        }
    }
}

/// What the monitor keeps about one session for the whole process life.
#[derive(Debug)]
pub(crate) struct SessionRecord {
    /// Where the session sits among the ones seen before it.
    pub rank: u64,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub last_status: Option<RequestStatus>,
    /// The request whose metadata the row shows: the logically newest one it
    /// holds. An older request reporting late says nothing about what the
    /// session is running now.
    metadata_order: Option<RequestOrder>,
    /// The newest request that named a project, ordered on its own. Claude Code
    /// names one in an event of its own, and only in the requests that carry a
    /// working directory, so the request that knows the project is often not
    /// the one the row is showing. Held against `metadata_order`, a project
    /// named after another request moved the row on would be dropped although
    /// a request of the session knows it.
    project_order: Option<RequestOrder>,
    pub counts: RowCounts,
    /// Requests of the session that named no conversation of their own. They
    /// are counted here rather than derived from what the rows do not explain.
    pub unattributed: RowCounts,
    pub cache: SessionCacheStats,
    pub output_buckets: Vec<(u64, u64)>,
    pub conversations: HashMap<String, ConversationRecord>,
    next_conversation_rank: u64,
    /// What the session cost per backend and per model that ran. Kept beside
    /// the conversations rather than derived from them: the same model runs in
    /// several conversations, and a conversation switches model.
    models: HashMap<ModelKey, ModelRecord>,
    next_model_rank: u64,
}

/// The backend and wire model a request's cost belongs to. Either half is
/// absent when nothing said what it was.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModelKey {
    provider: Option<String>,
    model: Option<String>,
}

/// One backend-and-model row of a session.
#[derive(Debug, Default)]
struct ModelRecord {
    rank: u64,
    counts: RowCounts,
    /// The cache misses judged on the row's requests, by cause. Kept here and
    /// not on the session, which tallies its misses as its lanes are judged.
    misses: CacheMissTally,
    /// How many requests of the row each caller model accounts for. A count
    /// rather than a set, so a request leaving the row takes its own entry with
    /// it and nothing else.
    requested: BTreeMap<Option<String>, u64>,
}

impl ModelRecord {
    fn apply(&mut self, contribution: &Contribution, requested: &Option<String>, sign: i64) {
        self.counts.apply(contribution, sign);
        if let Some(cause) = contribution.miss {
            self.misses.note(cause, sign);
        }
        let count = self.requested.entry(requested.clone()).or_insert(0);
        *count = add_count(*count, sign);
        if *count == 0 {
            self.requested.remove(requested);
        }
    }
}

impl SessionRecord {
    fn new(rank: u64, seen_at: SystemTime) -> Self {
        Self {
            rank,
            project: None,
            provider: None,
            model: None,
            effort: None,
            first_seen: seen_at,
            last_seen: seen_at,
            last_status: None,
            metadata_order: None,
            project_order: None,
            counts: RowCounts::default(),
            unattributed: RowCounts::default(),
            cache: SessionCacheStats::default(),
            output_buckets: Vec::new(),
            conversations: HashMap::new(),
            next_conversation_rank: 0,
            models: HashMap::new(),
            next_model_rank: 0,
        }
    }

    /// The figures of the requests that named no conversation.
    pub(crate) fn unattributed(&self) -> UnattributedUsage {
        self.unattributed.as_unattributed()
    }

    /// What the session spent per backend and per model that ran, oldest row
    /// first. A row every request has left holds nothing and is not shown,
    /// though its place is kept in case one comes back to it.
    pub(crate) fn model_usage(&self) -> Vec<ModelUsage> {
        let mut pairs: Vec<(&ModelKey, &ModelRecord)> = self
            .models
            .iter()
            .filter(|(_, record)| !record.counts.is_empty())
            .collect();
        pairs.sort_by_key(|(_, record)| record.rank);
        pairs
            .into_iter()
            .map(|(key, record)| ModelUsage {
                provider: key.provider.clone(),
                model: key.model.clone(),
                first_seen_rank: as_usize(record.rank),
                active_count: as_usize(record.counts.active_count),
                request_count: as_usize(record.counts.request_count),
                failure_count: as_usize(record.counts.failure_count),
                input_tokens: record.counts.usage.input_tokens,
                output_tokens: record.counts.usage.output_tokens,
                cache_read_tokens: record.counts.usage.cache_read_tokens,
                cache_write_tokens: record.counts.usage.cache_write_tokens,
                cache_write_5m_tokens: record.counts.usage.cache_write_5m_tokens,
                cache_write_1h_tokens: record.counts.usage.cache_write_1h_tokens,
                evidence: record.counts.evidence,
                misses: record.misses,
                requested_models: record
                    .requested
                    .iter()
                    .map(|(model, count)| (model.clone(), as_usize(*count)))
                    .collect(),
            })
            .collect()
    }

    fn model_mut(&mut self, key: ModelKey) -> &mut ModelRecord {
        if !self.models.contains_key(&key) {
            let rank = self.next_model_rank;
            self.next_model_rank = rank.saturating_add(1);
            self.models.insert(
                key.clone(),
                ModelRecord {
                    rank,
                    ..ModelRecord::default()
                },
            );
        }
        self.models.get_mut(&key).expect("model row just inserted")
    }

    /// Take a request's metadata if it is the logically newest one the row
    /// holds. A request of its own may keep updating its phase; one that
    /// started earlier cannot take the row's model or status back, however late
    /// its reports arrive.
    ///
    /// The project is the exception, and has an order of its own: it is the one
    /// field the request states separately from being routed, so it is the
    /// newest request that named a project that wins, not the newest request.
    fn note_metadata(&mut self, record: &RequestRecord) {
        self.last_seen = self.last_seen.max(record.seen_at());
        self.first_seen = self.first_seen.min(record.started_at);
        if record.project.is_some() && newest_in_row(self.project_order, record) {
            self.project_order = Some(record.order());
            self.project = record.project.clone();
        }
        if !newest_in_row(self.metadata_order, record) {
            return;
        }
        self.metadata_order = Some(record.order());
        self.provider = record.provider.clone().or(self.provider.take());
        self.model = record.model.clone().or(self.model.take());
        self.effort = record.effort.clone().or(self.effort.take());
        self.last_status = Some(record.status);
    }

    fn conversation_mut(
        &mut self,
        conversation: &str,
        seen_at: SystemTime,
    ) -> &mut ConversationRecord {
        if !self.conversations.contains_key(conversation) {
            let rank = self.next_conversation_rank;
            self.next_conversation_rank = self.next_conversation_rank.saturating_add(1);
            self.conversations.insert(
                conversation.to_string(),
                ConversationRecord::new(rank, seen_at),
            );
        }
        self.conversations
            .get_mut(conversation)
            .expect("conversation just inserted")
    }
}

/// One conversation of one session, for as long as the session lives.
#[derive(Debug)]
pub(crate) struct ConversationRecord {
    /// Where the conversation sits among the ones of its session seen before
    /// it, so rows keep a stable order however they are stored.
    pub rank: u64,
    /// The conversation Claude Code said spawned this one, as it said it. What
    /// a row hangs under is resolved from this when the snapshot is built.
    pub parent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub last_status: Option<RequestStatus>,
    /// The request whose metadata the row shows, by logical order.
    metadata_order: Option<RequestOrder>,
    /// The newest request that named a parent, ordered on its own, for the
    /// reason `SessionRecord::project_order` has one: a request states its
    /// lineage when it joins the row, which can be after another request of
    /// the row has already moved it on, and a request that names none says
    /// nothing about the lineage of the one that does.
    parent_order: Option<RequestOrder>,
    pub counts: RowCounts,
    pub cache: SessionCacheStats,
    /// The requests counted in this row. Ids only: enough to ask the records
    /// again what the row looks like once the request it was showing moves to
    /// another row, and nothing of a request beyond its id.
    members: HashSet<String>,
}

impl ConversationRecord {
    fn new(rank: u64, seen_at: SystemTime) -> Self {
        Self {
            rank,
            parent: None,
            provider: None,
            model: None,
            effort: None,
            first_seen: seen_at,
            last_seen: seen_at,
            last_status: None,
            metadata_order: None,
            parent_order: None,
            counts: RowCounts::default(),
            cache: SessionCacheStats::default(),
            members: HashSet::new(),
        }
    }

    /// As [`SessionRecord::note_metadata`], for one conversation of a session,
    /// with the parent ordered on its own the way a session's project is.
    fn note_metadata(&mut self, record: &RequestRecord) {
        self.last_seen = self.last_seen.max(record.seen_at());
        self.first_seen = self.first_seen.min(record.started_at);
        if record.conversation_parent.is_some() && newest_in_row(self.parent_order, record) {
            self.parent_order = Some(record.order());
            self.parent = record.conversation_parent.clone();
        }
        if !newest_in_row(self.metadata_order, record) {
            return;
        }
        self.metadata_order = Some(record.order());
        self.provider = record.provider.clone().or(self.provider.take());
        self.model = record.model.clone().or(self.model.take());
        self.effort = record.effort.clone().or(self.effort.take());
        self.last_status = Some(record.status);
    }

    /// Forget what the row was showing, to state it again from the requests it
    /// still holds. Its identity — name, rank, first sighting — stays.
    fn forget_metadata(&mut self) {
        self.parent = None;
        self.provider = None;
        self.model = None;
        self.effort = None;
        self.last_status = None;
        self.metadata_order = None;
        self.parent_order = None;
        self.last_seen = self.first_seen;
    }
}

/// Whether a request is at least as recent as the one a row is showing.
fn newest_in_row(current: Option<RequestOrder>, record: &RequestRecord) -> bool {
    current.is_none_or(|current| record.order() >= current)
}

/// A request built outside the event stream: the demo monitor and tests
/// synthesise finished requests directly instead of replaying their events.
pub(crate) struct AbsorbedRequest<'a> {
    pub request_id: &'a str,
    pub session_id: Option<String>,
    pub conversation: Option<String>,
    pub conversation_parent: Option<String>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub status: RequestStatus,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub generation_started_at: Option<SystemTime>,
    pub generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache: RequestCache,
}

impl<'a> AbsorbedRequest<'a> {
    pub(crate) fn from_active(request: &'a ActiveRequest) -> Self {
        Self {
            request_id: &request.request_id,
            session_id: request.session_id.clone(),
            conversation: request.conversation.clone(),
            conversation_parent: request.conversation_parent.clone(),
            project: request.project.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
            requested_model: request.requested_model.clone(),
            effective_model: request.effective_model.clone(),
            effort: request.effort.clone(),
            endpoint: request.endpoint,
            status: request.status,
            started_at: request.started_at,
            finished_at: None,
            generation_started_at: request.generation_started_at,
            generation_initial_output_tokens: request.generation_initial_output_tokens,
            generation_finished_at: request.generation_finished_at,
            generation_duration: request.generation_duration,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cache: request.cache,
        }
    }

    pub(crate) fn from_completed(request: &'a CompletedRequest) -> Self {
        Self {
            request_id: &request.request_id,
            session_id: request.session_id.clone(),
            conversation: request.conversation.clone(),
            conversation_parent: request.conversation_parent.clone(),
            project: request.project.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
            requested_model: request.requested_model.clone(),
            effective_model: request.effective_model.clone(),
            effort: request.effort.clone(),
            endpoint: request.endpoint,
            status: request.status,
            started_at: request.started_at,
            finished_at: Some(request.finished_at),
            generation_started_at: request.generation_started_at,
            generation_initial_output_tokens: request.generation_initial_output_tokens,
            generation_finished_at: request.generation_finished_at,
            generation_duration: request.generation_duration,
            input_tokens: request.input_tokens,
            output_tokens: request.output_tokens,
            cache: request.cache,
        }
    }
}

/// The numbers of one request as the ledger holds them, for its visible row to
/// mirror.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestNumbers {
    pub status: RequestStatus,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache: RequestCache,
    pub generation_started_at: Option<SystemTime>,
    pub generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
}

/// Every request the process has served, and what its sessions and
/// conversations add up to.
#[derive(Debug, Default)]
pub(crate) struct Ledger {
    requests: HashMap<String, RequestRecord>,
    sessions: HashMap<Option<String>, SessionRecord>,
    next_session_rank: u64,
    next_request_rank: u64,
}

impl Ledger {
    /// Start counting a request the store has just seen. A second start for the
    /// same id changes nothing: the request is already counted.
    pub(crate) fn start(
        &mut self,
        request_id: &str,
        session_id: Option<String>,
        endpoint: EndpointKind,
        started_at: SystemTime,
    ) {
        if self.requests.contains_key(request_id) {
            return;
        }
        let rank = self.next_request_rank;
        self.next_request_rank = self.next_request_rank.saturating_add(1);
        self.requests.insert(
            request_id.to_string(),
            RequestRecord::new(rank, session_id, endpoint, started_at),
        );
        self.attach(request_id);
    }

    pub(crate) fn record(&self, request_id: &str) -> Option<&RequestRecord> {
        self.requests.get(request_id)
    }

    /// The numbers of a request, for its visible row to mirror.
    pub(crate) fn numbers(&self, request_id: &str) -> Option<RequestNumbers> {
        let record = self.requests.get(request_id)?;
        Some(RequestNumbers {
            status: record.status,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cache: record.cache,
            generation_started_at: record.generation_started_at,
            generation_initial_output_tokens: record.generation_initial_output_tokens,
            generation_finished_at: record.generation_finished_at,
            generation_duration: record.generation_duration,
        })
    }

    pub(crate) fn sessions(&self) -> impl Iterator<Item = (&Option<String>, &SessionRecord)> {
        self.sessions.iter()
    }

    /// The prompt-cache figures of a session, for the store to judge its lanes.
    pub(crate) fn session_cache_mut(
        &mut self,
        session_id: &Option<String>,
    ) -> Option<&mut SessionCacheStats> {
        self.sessions
            .get_mut(session_id)
            .map(|session| &mut session.cache)
    }

    pub(crate) fn conversation_cache_mut(
        &mut self,
        session_id: &Option<String>,
        conversation: &str,
    ) -> Option<&mut SessionCacheStats> {
        self.sessions
            .get_mut(session_id)?
            .conversations
            .get_mut(conversation)
            .map(|row| &mut row.cache)
    }

    pub(crate) fn note_project(&mut self, request_id: &str, project: String) {
        self.update(request_id, |record| record.project = Some(project));
    }

    /// The provider and model a request was routed to.
    pub(crate) fn note_selection(
        &mut self,
        request_id: &str,
        provider: String,
        model: String,
        effort: Option<String>,
    ) {
        self.update(request_id, |record| {
            record.provider = Some(provider);
            record.model = Some(model);
            record.effort = effort;
            if !record.terminal {
                record.status = RequestStatus::ProviderSelected;
            }
        });
    }

    /// The model the client's own request named. The first capture wins: it is
    /// what arrived, and nothing later can know it better. Returns what the
    /// request's requested model reads as now, which is the one already held
    /// where a second naming arrived, and nothing where the ledger knows no
    /// such request.
    pub(crate) fn note_requested_model(
        &mut self,
        request_id: &str,
        model: String,
    ) -> Option<String> {
        self.update(request_id, |record| {
            record.requested_model.get_or_insert(model).clone()
        })
    }

    /// The model a provider put on the wire, shown next to the one it was
    /// routed as. Returns what the request's model reads as now.
    ///
    /// A producer naming the model it already named is a second reading of the
    /// same thing rather than a switch, and leaves the record as it stands.
    pub(crate) fn note_resolved_model(
        &mut self,
        request_id: &str,
        model: String,
    ) -> Option<String> {
        self.update(request_id, |record| {
            if record.effective_model.as_deref() == Some(model.as_str()) {
                return record.model.clone();
            }
            record.effective_model = Some(model.clone());
            record.model = Some(match record.model.take() {
                Some(incoming) if incoming != model => format!("{incoming} → {model}"),
                Some(incoming) => incoming,
                None => model,
            });
            record.model.clone()
        })
        .flatten()
    }

    /// Which conversation of its session the request belongs to. A request that
    /// names one after it was already counted takes its tokens, its evidence
    /// and its request and failure counts with it to the new row.
    pub(crate) fn note_conversation(
        &mut self,
        request_id: &str,
        conversation: String,
        parent: Option<String>,
    ) {
        let previous = self
            .requests
            .get(request_id)
            .and_then(|record| record.conversation.clone());
        let moved = previous.as_deref().is_some_and(|name| name != conversation);
        let session_id = self
            .requests
            .get(request_id)
            .map(|record| record.session_id.clone());
        self.update(request_id, |record| {
            record.conversation = Some(conversation);
            record.conversation_parent = parent;
        });
        if let (true, Some(previous), Some(session_id)) = (moved, previous, session_id) {
            self.left_conversation(&session_id, &previous, request_id);
        }
    }

    /// Settle the row a request has just left: it is no longer a conversation of
    /// its session if nothing is counted in it any more, and otherwise it shows
    /// what the requests it still holds say rather than what the one that moved
    /// away was doing.
    fn left_conversation(
        &mut self,
        session_id: &Option<String>,
        conversation: &str,
        request_id: &str,
    ) {
        let Self {
            requests, sessions, ..
        } = self;
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        let emptied = {
            let Some(row) = session.conversations.get_mut(conversation) else {
                return;
            };
            row.members.remove(request_id);
            row.counts.request_count == 0 && row.counts.active_count == 0
        };
        if emptied {
            session.conversations.remove(conversation);
            return;
        }
        if let Some(row) = session.conversations.get_mut(conversation) {
            restate_metadata(row, requests);
        }
    }

    pub(crate) fn note_status(&mut self, request_id: &str, status: RequestStatus) {
        self.update(request_id, |record| {
            if !record.terminal {
                record.status = status;
            }
        });
    }

    /// The response began: output tokens from here on are measured against this
    /// moment.
    pub(crate) fn note_generation_started(&mut self, request_id: &str, now: SystemTime) {
        self.update(request_id, |record| {
            record.generation_started_at = Some(now);
            record.generation_started_instant = Some(Instant::now());
            record.generation_initial_output_tokens = record.output_tokens.unwrap_or(0);
            record.generation_finished_at = None;
            record.generation_duration = None;
        });
    }

    /// Streaming progress: the generation interval it extends and the counts it
    /// carried.
    pub(crate) fn note_stream_progress(
        &mut self,
        request_id: &str,
        report: &UsageReport,
        now: SystemTime,
    ) -> Option<UsageDelta> {
        let output_seen = report
            .closing
            .output_tokens
            .or(report.opening.output_tokens);
        self.update(request_id, |record| {
            if !record.terminal {
                record.status = RequestStatus::Streaming;
            }
            match record.generation_started_instant {
                None => {
                    record.generation_started_at = Some(now);
                    record.generation_started_instant = Some(Instant::now());
                    record.generation_initial_output_tokens =
                        output_seen.or(record.output_tokens).unwrap_or(0);
                }
                Some(started) => {
                    record.generation_finished_at = Some(now);
                    record.generation_duration = Some(started.elapsed());
                }
            }
            absorb_report(record, report, now)
        })
    }

    /// A usage report outside a stream's progress.
    pub(crate) fn note_usage(
        &mut self,
        request_id: &str,
        report: &UsageReport,
        now: SystemTime,
    ) -> Option<UsageDelta> {
        let output_seen = report
            .closing
            .output_tokens
            .or(report.opening.output_tokens);
        self.update(request_id, |record| {
            if output_seen.is_some()
                && let Some(started) = record.generation_started_instant
            {
                record.generation_finished_at = Some(now);
                record.generation_duration = Some(started.elapsed());
            }
            absorb_report(record, report, now)
        })
    }

    /// Record a request's terminal event. The first one decides its outcome and
    /// is counted once; a second event for the same request counts nothing
    /// twice, though the usage it carries is still taken.
    pub(crate) fn finish(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        report: &UsageReport,
        now: SystemTime,
    ) -> bool {
        // A terminal event can be the first thing seen about a request; it is
        // counted from here rather than invented as history it never had.
        self.start(request_id, None, EndpointKind::Messages, now);
        if self
            .requests
            .get(request_id)
            .is_some_and(|record| record.terminal)
        {
            self.note_usage(request_id, report, now);
            return false;
        }
        self.update(request_id, |record| {
            if report.opening.output_tokens.is_some()
                && let Some(started) = record.generation_started_instant
            {
                record.generation_finished_at = Some(now);
                record.generation_duration = Some(started.elapsed());
            }
            record.status = status;
            record.finished_at = Some(now);
            absorb_report(record, report, now);
            // The request is finished, so what it produced takes its place in
            // the output history: the whole count at once, in the bucket of the
            // moment the generation ended. Until now it was a live number with
            // nothing published to correct.
            record.terminal = true;
            let generated = record.output_tokens.unwrap_or(0);
            if generated > 0 && record.counts_tokens() {
                let at = record.output_timestamp(now);
                publish_output(record, at, i64::try_from(generated).unwrap_or(i64::MAX));
            }
        });
        true
    }

    /// Mark a request compared with the previous one of its prompt-cache lane.
    pub(crate) fn note_cache_evaluation(&mut self, request_id: &str, miss: Option<CacheMiss>) {
        self.update(request_id, |record| {
            record.cache.evaluated = true;
            record.cache.miss = miss;
        });
    }

    /// Take a request built outside the event stream, counting it the way its
    /// events would have.
    pub(crate) fn absorb(&mut self, request: AbsorbedRequest<'_>) {
        let AbsorbedRequest {
            request_id,
            session_id,
            conversation,
            conversation_parent,
            project,
            provider,
            model,
            requested_model,
            effective_model,
            effort,
            endpoint,
            status,
            started_at,
            finished_at,
            generation_started_at,
            generation_initial_output_tokens,
            generation_finished_at,
            generation_duration,
            input_tokens,
            output_tokens,
            cache,
        } = request;
        self.start(request_id, session_id, endpoint, started_at);
        let previous = self
            .requests
            .get(request_id)
            .and_then(|record| record.conversation.clone());
        let moved = previous
            .as_deref()
            .is_some_and(|name| Some(name) != conversation.as_deref());
        let session = self
            .requests
            .get(request_id)
            .map(|record| record.session_id.clone());
        self.update(request_id, |record| {
            record.conversation = conversation;
            record.conversation_parent = conversation_parent;
            record.project = project;
            record.provider = provider;
            record.model = model;
            record.requested_model = requested_model;
            record.effective_model = effective_model;
            record.effort = effort;
            record.status = status;
            record.finished_at = finished_at;
            record.terminal = finished_at.is_some();
            record.generation_started_at = generation_started_at;
            record.generation_initial_output_tokens = generation_initial_output_tokens;
            record.generation_finished_at = generation_finished_at;
            record.generation_duration = generation_duration;
            record.input_tokens = input_tokens;
            record.output_tokens = output_tokens;
            record.cache = cache;
            // The counts come as they are rather than as a change, so the
            // output history this request owns is stated again from scratch. A
            // request still running has published none of it yet.
            record.output_buckets.clear();
            if let Some(tokens) = output_tokens.filter(|tokens| *tokens > 0)
                && record.terminal
                && record.counts_tokens()
            {
                let at = record.output_timestamp(started_at);
                publish_output(record, at, i64::try_from(tokens).unwrap_or(i64::MAX));
            }
        });
        if let (true, Some(previous), Some(session)) = (moved, previous, session) {
            self.left_conversation(&session, &previous, request_id);
        }
        self.absorb_cache(request_id);
    }

    /// The prompt size and cache miss of an absorbed request, the way its
    /// lane evaluation would have recorded them.
    fn absorb_cache(&mut self, request_id: &str) {
        let Some(record) = self.requests.get(request_id) else {
            return;
        };
        if !record.counts_tokens() {
            return;
        }
        let session_id = record.session_id.clone();
        let conversation = record.conversation.clone();
        let prompt = record.prompt_tokens().unwrap_or(0);
        let started_at = record.started_at;
        let ttl = record.cache.ttl;
        let miss = record.cache.miss;
        let main = conversation.as_deref().is_none_or(|name| name == "main");
        if let Some(stats) = self.session_cache_mut(&session_id) {
            if main {
                note_context(stats, prompt, started_at, ttl);
            }
            if let Some(miss) = miss {
                note_miss(stats, started_at, miss);
            }
        }
        if let Some(conversation) = conversation.as_deref()
            && let Some(stats) = self.conversation_cache_mut(&session_id, conversation)
        {
            note_context(stats, prompt, started_at, ttl);
            if let Some(miss) = miss {
                note_miss(stats, started_at, miss);
            }
        }
    }

    /// Put an output history into a session directly: the demo monitor
    /// synthesises one instead of observing requests.
    pub(crate) fn seed_output_history(
        &mut self,
        session_id: Option<String>,
        samples: &[(u64, u64)],
    ) {
        if !self.sessions.contains_key(&session_id) {
            let rank = self.next_session_rank;
            self.next_session_rank = self.next_session_rank.saturating_add(1);
            self.sessions.insert(
                session_id.clone(),
                SessionRecord::new(rank, SystemTime::now()),
            );
        }
        let session = self
            .sessions
            .get_mut(&session_id)
            .expect("session just inserted");
        for (bucket, tokens) in samples {
            add_bucket(&mut session.output_buckets, *bucket, *tokens);
        }
    }

    /// Change one request's record, with its contribution taken out of the rows
    /// it feeds for the duration so that whatever the change makes of it lands
    /// in the right place.
    fn update<T>(
        &mut self,
        request_id: &str,
        change: impl FnOnce(&mut RequestRecord) -> T,
    ) -> Option<T> {
        if !self.requests.contains_key(request_id) {
            return None;
        }
        self.detach(request_id);
        let outcome = self
            .requests
            .get_mut(request_id)
            .map(change)
            .expect("record present");
        self.attach(request_id);
        Some(outcome)
    }

    fn detach(&mut self, request_id: &str) {
        let Self {
            requests,
            sessions,
            next_session_rank,
            ..
        } = self;
        if let Some(record) = requests.get(request_id) {
            apply_contribution(sessions, next_session_rank, request_id, record, -1);
        }
    }

    fn attach(&mut self, request_id: &str) {
        let Self {
            requests,
            sessions,
            next_session_rank,
            ..
        } = self;
        if let Some(record) = requests.get(request_id) {
            apply_contribution(sessions, next_session_rank, request_id, record, 1);
        }
    }

    /// Move a request's start in time, for tests that need a request older than
    /// the process.
    #[cfg(test)]
    pub(crate) fn backdate_for_tests(&mut self, request_id: &str, started_at: SystemTime) {
        self.update(request_id, |record| record.started_at = started_at);
    }

    /// The output-history buckets one request owns, for tests that check a
    /// correction took tokens back out of the buckets they went into.
    #[cfg(test)]
    pub(crate) fn owned_output_buckets(&self, request_id: &str) -> Vec<(u64, u64)> {
        self.requests
            .get(request_id)
            .map(|record| record.output_buckets.clone())
            .unwrap_or_default()
    }
}

/// Add one request's whole contribution to the rows it belongs to, or take it
/// back out of them.
fn apply_contribution(
    sessions: &mut HashMap<Option<String>, SessionRecord>,
    next_session_rank: &mut u64,
    request_id: &str,
    record: &RequestRecord,
    sign: i64,
) {
    // Only a session seen for the first time needs its key stored; the rest of
    // a request's events just look theirs up.
    if !sessions.contains_key(&record.session_id) {
        let rank = *next_session_rank;
        *next_session_rank = next_session_rank.saturating_add(1);
        sessions.insert(
            record.session_id.clone(),
            SessionRecord::new(rank, record.started_at),
        );
    }
    let session = sessions
        .get_mut(&record.session_id)
        .expect("session just inserted");
    let contribution = record.contribution();
    session.counts.apply(&contribution, sign);
    if contribution.counts_tokens {
        apply_output_buckets(&mut session.output_buckets, &record.output_buckets, sign);
    }
    // The backend-and-model row the request costs, which a late report naming
    // the wire model moves it to, contribution and evidence together. A row the
    // last of its requests just left keeps its place rather than being dropped:
    // every change detaches and reattaches, so a row that emptied for an
    // instant would come back renamed at the end of the order. It is left out
    // of the projection instead, and takes its old place if it fills again.
    session
        .model_mut(record.model_key())
        .apply(&contribution, &record.requested_model, sign);
    match record.conversation.as_deref() {
        Some(conversation) => {
            let row = session.conversation_mut(conversation, record.started_at);
            row.counts.apply(&contribution, sign);
            if sign >= 0 {
                if !row.members.contains(request_id) {
                    row.members.insert(request_id.to_string());
                }
                row.note_metadata(record);
            }
        }
        None => session.unattributed.apply(&contribution, sign),
    }
    if sign >= 0 {
        session.note_metadata(record);
    }
}

/// State a row's metadata again from the requests it holds, newest last, after
/// the one it was showing moved elsewhere. Only the row that lost a request is
/// walked, and only the metadata its records already carry is read.
fn restate_metadata(row: &mut ConversationRecord, requests: &HashMap<String, RequestRecord>) {
    let mut members: Vec<&RequestRecord> = row
        .members
        .iter()
        .filter_map(|request_id| requests.get(request_id))
        .collect();
    members.sort_by_key(|record| record.order());
    row.forget_metadata();
    for record in members {
        row.note_metadata(record);
    }
}

/// Take one report's counts onto a record, and once the request has finished,
/// keep the output history in step with what the report changed.
///
/// Before the terminal event the output is a live number with no place in the
/// history yet, so there is nothing to publish and nothing to take back: a
/// correction then only moves the record's own count.
fn absorb_report(record: &mut RequestRecord, report: &UsageReport, now: SystemTime) -> UsageDelta {
    let at = record.output_timestamp(now);
    let delta = UsageTarget {
        input_tokens: &mut record.input_tokens,
        output_tokens: &mut record.output_tokens,
        cache: &mut record.cache,
    }
    .apply(report);
    if record.terminal && delta.output != 0 && record.counts_tokens() {
        publish_output(record, at, delta.output);
    }
    delta
}

/// Put output tokens in the history bucket of the moment they were generated,
/// or take back tokens a correction removed.
///
/// A correction comes off this request's own newest contribution first and then
/// its earlier ones: never another request's tokens, and never a bucket it
/// wrote nothing to.
fn publish_output(record: &mut RequestRecord, at: SystemTime, delta: i64) {
    if delta > 0 {
        add_bucket(
            &mut record.output_buckets,
            session_token_bucket(at),
            delta.unsigned_abs(),
        );
        return;
    }
    let mut left = delta.unsigned_abs();
    for (_, tokens) in record.output_buckets.iter_mut().rev() {
        let taken = (*tokens).min(left);
        *tokens -= taken;
        left -= taken;
        if left == 0 {
            break;
        }
    }
    record.output_buckets.retain(|(_, tokens)| *tokens > 0);
}

/// Add a request's owned bucket tokens to a session's history, or take them
/// back out. The buckets keep their own moments in time, so a request that
/// moves to another session keeps its place in history.
fn apply_output_buckets(session: &mut Vec<(u64, u64)>, owned: &[(u64, u64)], sign: i64) {
    for (bucket, tokens) in owned {
        if sign >= 0 {
            add_bucket(session, *bucket, *tokens);
        } else {
            remove_bucket(session, *bucket, *tokens);
        }
    }
}

/// Add tokens to a bucket list kept in time order.
fn add_bucket(buckets: &mut Vec<(u64, u64)>, bucket: u64, tokens: u64) {
    match buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
        Ok(index) => buckets[index].1 = buckets[index].1.saturating_add(tokens),
        Err(index) => buckets.insert(index, (bucket, tokens)),
    }
}

/// Take tokens back out of a bucket list, dropping a bucket that empties.
fn remove_bucket(buckets: &mut Vec<(u64, u64)>, bucket: u64, tokens: u64) {
    if let Ok(index) = buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
        buckets[index].1 = buckets[index].1.saturating_sub(tokens);
        if buckets[index].1 == 0 {
            buckets.remove(index);
        }
    }
}

fn apply_usage_totals(total: &mut SessionUsage, usage: &SessionUsage, sign: i64) {
    total.input_tokens = add_tokens(total.input_tokens, usage.input_tokens, sign);
    total.output_tokens = add_tokens(total.output_tokens, usage.output_tokens, sign);
    total.cache_read_tokens = add_tokens(total.cache_read_tokens, usage.cache_read_tokens, sign);
    total.cache_write_tokens = add_tokens(total.cache_write_tokens, usage.cache_write_tokens, sign);
    total.cache_write_5m_tokens = add_tokens(
        total.cache_write_5m_tokens,
        usage.cache_write_5m_tokens,
        sign,
    );
    total.cache_write_1h_tokens = add_tokens(
        total.cache_write_1h_tokens,
        usage.cache_write_1h_tokens,
        sign,
    );
}

fn add_count(count: u64, sign: i64) -> u64 {
    add_tokens(count, 1, sign)
}

/// Raise or lower a total by a number of tokens, through the same signed
/// arithmetic a report's own correction uses.
fn add_tokens(total: u64, tokens: u64, sign: i64) -> u64 {
    let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
    add_signed(total, if sign >= 0 { tokens } else { -tokens })
}

fn as_usize(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}
