//! Process-wide metrics: Codex refusals, log events and the start instant.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

use super::MAX_LOG_EVENT_SERIES;

const OTHER: &str = "other";

/// Families of the process-wide metrics. A clone shares every counter and
/// the event budget with the original.
#[derive(Clone, Debug)]
pub struct ProcessMetrics {
    refusals: Family<ClassLabels, Counter>,
    log_events: Family<LogEventLabels, Counter>,
    admitted_events: Arc<Mutex<HashSet<LogEventLabels>>>,
    max_event_series: usize,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ClassLabels {
    class: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct LogEventLabels {
    service: String,
    level: String,
    event: String,
}

impl ProcessMetrics {
    pub fn new() -> Self {
        Self::with_event_budget(MAX_LOG_EVENT_SERIES)
    }

    fn with_event_budget(max_event_series: usize) -> Self {
        Self {
            refusals: Family::default(),
            log_events: Family::default(),
            admitted_events: Arc::default(),
            max_event_series,
        }
    }

    /// Counts one log line. A new label set past the event budget is counted
    /// under `service="other"`, `event="other"` and its own level.
    pub fn log_event(&self, service: &str, level: &str, event: &str) {
        let labels = LogEventLabels {
            service: service.to_owned(),
            level: level.to_owned(),
            event: event.to_owned(),
        };
        let labels = {
            let mut admitted = self
                .admitted_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if admitted.contains(&labels) {
                labels
            } else if admitted.len() < self.max_event_series {
                admitted.insert(labels.clone());
                labels
            } else {
                LogEventLabels {
                    service: OTHER.to_owned(),
                    level: labels.level,
                    event: OTHER.to_owned(),
                }
            }
        };
        self.log_events.get_or_create(&labels).inc();
    }

    pub fn codex_refusal(&self, class: RefusalClass) {
        self.refusals
            .get_or_create(&ClassLabels {
                class: class.label(),
            })
            .inc();
    }

    /// Registers clones of the long-lived families.
    pub(super) fn register(&self, registry: &mut Registry) {
        registry.register(
            "codex_refusals",
            "Codex refusals answered to the client, by class",
            self.refusals.clone(),
        );
        registry.register(
            "log_events",
            "Log lines written, by service, level and message",
            self.log_events.clone(),
        );
    }
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalClass {
    UsageLimit,
    QuotaExceeded,
    UsageNotIncluded,
    Rejected,
    ContextOverflow,
    Throttled,
    Overloaded,
    ModelNotFound,
    PermissionDenied,
}

impl RefusalClass {
    fn label(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::QuotaExceeded => "quota_exceeded",
            Self::UsageNotIncluded => "usage_not_included",
            Self::Rejected => "rejected",
            Self::ContextOverflow => "context_overflow",
            Self::Throttled => "throttled",
            Self::Overloaded => "overloaded",
            Self::ModelNotFound => "model_not_found",
            Self::PermissionDenied => "permission_denied",
        }
    }
}

static PROCESS_START_SECONDS: OnceLock<u64> = OnceLock::new();

/// Captures the process start instant; later calls keep the first one.
pub fn mark_process_start() {
    process_start_seconds();
}

/// Unix seconds of the process start instant, captured now when no call to
/// `mark_process_start` came first.
pub(super) fn process_start_seconds() -> u64 {
    *PROCESS_START_SECONDS.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_secs())
    })
}
