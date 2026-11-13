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

static PROCESS: OnceLock<ProcessMetrics> = OnceLock::new();

/// The instance the free functions below feed.
pub fn process() -> ProcessMetrics {
    with_current(ProcessMetrics::clone)
}

pub fn log_event(service: &str, level: &str, event: &str) {
    with_current(|metrics| metrics.log_event(service, level, event));
}

pub fn codex_refusal(class: RefusalClass) {
    with_current(|metrics| metrics.codex_refusal(class));
}

/// Calls `f` with the instance the free functions feed: in tests the
/// innermost scope of the calling thread when one is open, else the
/// process-wide instance.
fn with_current<R>(f: impl FnOnce(&ProcessMetrics) -> R) -> R {
    #[cfg(test)]
    let scoped = innermost_scope();
    #[cfg(not(test))]
    let scoped: Option<ProcessMetrics> = None;
    match scoped {
        Some(metrics) => f(&metrics),
        None => f(PROCESS.get_or_init(ProcessMetrics::new)),
    }
}

#[cfg(test)]
thread_local! {
    /// Open scopes of this thread, innermost last.
    static SCOPES: std::cell::RefCell<Vec<ProcessMetrics>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn innermost_scope() -> Option<ProcessMetrics> {
    SCOPES
        .try_with(|scopes| {
            scopes
                .try_borrow()
                .ok()
                .and_then(|scopes| scopes.last().cloned())
        })
        .ok()
        .flatten()
}

/// Installs a fresh instance that the free functions called on this thread
/// feed until the returned guard drops. Scopes nest: dropping an inner guard
/// hands the thread back to the scope that was open before it.
#[cfg(test)]
pub(crate) fn scoped_process_for_tests() -> ScopedProcess {
    let metrics = ProcessMetrics::new();
    let depth = SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        scopes.push(metrics.clone());
        scopes.len() - 1
    });
    ScopedProcess {
        metrics,
        depth,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Guard of one scope. It is not `Send`: the scope belongs to the thread that
/// opened it.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ScopedProcess {
    metrics: ProcessMetrics,
    depth: usize,
    _thread_bound: std::marker::PhantomData<*const ()>,
}

#[cfg(test)]
impl std::ops::Deref for ScopedProcess {
    type Target = ProcessMetrics;

    fn deref(&self) -> &ProcessMetrics {
        &self.metrics
    }
}

#[cfg(test)]
impl Drop for ScopedProcess {
    fn drop(&mut self) {
        let _ = SCOPES.try_with(|scopes| {
            if let Ok(mut scopes) = scopes.try_borrow_mut() {
                scopes.truncate(self.depth);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ProcessMetrics, RefusalClass};
    use crate::metrics::{
        Format, RenderInput, codex_refusal, log_event, render, scoped_process_for_tests,
    };

    const BOTH_FORMATS: [Format; 2] = [Format::OpenMetrics, Format::Text];

    /// Sample lines of the two process families in one render of `metrics`.
    fn samples(metrics: &ProcessMetrics, format: Format) -> BTreeSet<String> {
        let input = RenderInput {
            process: Some(metrics),
            ..RenderInput::default()
        };
        render(&input, format)
            .lines()
            .filter(|line| {
                line.starts_with("ccmux_log_events") || line.starts_with("ccmux_codex_refusals")
            })
            .map(str::to_owned)
            .collect()
    }

    fn event_line(service: &str, level: &str, event: &str, count: u64) -> String {
        format!(
            "ccmux_log_events_total{{service=\"{service}\",level=\"{level}\",event=\"{event}\"}} {count}"
        )
    }

    fn refusal_line(class: &str, count: u64) -> String {
        format!("ccmux_codex_refusals_total{{class=\"{class}\"}} {count}")
    }

    #[test]
    fn a_log_event_gets_a_series() {
        let metrics = ProcessMetrics::new();
        metrics.log_event("server", "info", "request completed");
        metrics.log_event("server", "info", "request completed");
        metrics.log_event("server", "warn", "request completed");

        let expected = BTreeSet::from([
            event_line("server", "info", "request completed", 2),
            event_line("server", "warn", "request completed", 1),
        ]);
        for format in BOTH_FORMATS {
            assert_eq!(samples(&metrics, format), expected);
        }
    }

    #[test]
    fn the_log_event_budget_falls_back_to_other() {
        let metrics = ProcessMetrics::with_event_budget(2);
        metrics.log_event("server", "info", "first");
        metrics.log_event("codex", "warn", "second");
        metrics.log_event("server", "error", "third");
        metrics.log_event("monitor", "error", "fourth");
        metrics.log_event("codex", "info", "fifth");
        metrics.log_event("server", "info", "first");

        let expected = BTreeSet::from([
            event_line("server", "info", "first", 2),
            event_line("codex", "warn", "second", 1),
            event_line("other", "error", "other", 2),
            event_line("other", "info", "other", 1),
        ]);
        for format in BOTH_FORMATS {
            assert_eq!(samples(&metrics, format), expected);
        }
    }

    #[test]
    fn a_refusal_counts_under_its_class() {
        let classes = [
            (RefusalClass::UsageLimit, "usage_limit"),
            (RefusalClass::QuotaExceeded, "quota_exceeded"),
            (RefusalClass::UsageNotIncluded, "usage_not_included"),
            (RefusalClass::Rejected, "rejected"),
            (RefusalClass::ContextOverflow, "context_overflow"),
            (RefusalClass::Throttled, "throttled"),
            (RefusalClass::Overloaded, "overloaded"),
            (RefusalClass::ModelNotFound, "model_not_found"),
            (RefusalClass::PermissionDenied, "permission_denied"),
        ];
        let metrics = ProcessMetrics::new();
        for (class, _) in classes {
            metrics.codex_refusal(class);
        }
        metrics.codex_refusal(RefusalClass::UsageLimit);

        let expected: BTreeSet<String> = classes
            .iter()
            .map(|(class, label)| {
                let count = if *class == RefusalClass::UsageLimit {
                    2
                } else {
                    1
                };
                refusal_line(label, count)
            })
            .collect();
        for format in BOTH_FORMATS {
            assert_eq!(samples(&metrics, format), expected);
        }
    }

    #[test]
    fn a_scoped_instance_takes_the_free_functions() {
        let scope = scoped_process_for_tests();
        log_event("server", "warn", "request failed");
        codex_refusal(RefusalClass::Throttled);
        codex_refusal(RefusalClass::Throttled);

        let expected = BTreeSet::from([
            event_line("server", "warn", "request failed", 1),
            refusal_line("throttled", 2),
        ]);
        for format in BOTH_FORMATS {
            assert_eq!(samples(&scope, format), expected);
        }
    }

    #[test]
    fn a_nested_scope_hands_back_to_the_outer_one() {
        let outer = scoped_process_for_tests();
        log_event("server", "info", "before");

        let inner = scoped_process_for_tests();
        log_event("server", "info", "inside");
        codex_refusal(RefusalClass::Overloaded);
        for format in BOTH_FORMATS {
            assert_eq!(
                samples(&outer, format),
                BTreeSet::from([event_line("server", "info", "before", 1)])
            );
        }
        let inner_metrics = (*inner).clone();
        drop(inner);

        log_event("server", "info", "after");
        for format in BOTH_FORMATS {
            assert_eq!(
                samples(&outer, format),
                BTreeSet::from([
                    event_line("server", "info", "before", 1),
                    event_line("server", "info", "after", 1),
                ])
            );
            assert_eq!(
                samples(&inner_metrics, format),
                BTreeSet::from([
                    event_line("server", "info", "inside", 1),
                    refusal_line("overloaded", 1),
                ])
            );
        }
    }
}
