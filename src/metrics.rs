//! Prometheus metrics of the proxy: the long-lived families, the readings
//! taken at render time, and the text both exposition formats are written in.
//!
//! The module depends on `prometheus-client` and `std` only. Its code never
//! panics, never logs and never calls into the monitor, so it can be called
//! while another module holds a lock.

mod process;
mod render;
mod request;

#[cfg(test)]
pub(crate) use process::scoped_process_for_tests;
pub use process::{
    ProcessMetrics, RefusalClass, codex_refusal, log_event, mark_process_start, process,
};
pub use render::{Format, RenderInput, content_type, format_for_accept, render};
pub use request::{
    CodexWindow, CodexWindows, GaugeReading, InFlight, MissCause, Outcome, RequestMetrics,
    SettledRequest, SettledUsage, TokenCount,
};

/// Upper bounds of `ccmux_request_duration_seconds`, in seconds.
pub const DURATION_BUCKETS_SECONDS: [f64; 15] = [
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0,
];

/// Upper bounds of `ccmux_time_to_first_token_seconds`, in seconds.
pub const FIRST_TOKEN_BUCKETS_SECONDS: [f64; 11] = [
    0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0, 120.0, 300.0,
];

/// Upper bounds of `ccmux_request_prompt_tokens`, in tokens.
pub const PROMPT_TOKEN_BUCKETS: [f64; 13] = [
    1_000.0,
    2_000.0,
    5_000.0,
    10_000.0,
    20_000.0,
    50_000.0,
    100_000.0,
    150_000.0,
    200_000.0,
    300_000.0,
    500_000.0,
    750_000.0,
    1_000_000.0,
];

/// A provider or model label longer than this many bytes puts its pair under
/// (`other`, `other`).
pub const MAX_LABEL_BYTES: usize = 120;

/// Distinct (provider, model) pairs one `RequestMetrics` admits; a new pair
/// past that is counted under (`other`, `other`).
pub const MAX_MODEL_SERIES: usize = 200;

/// Distinct label sets of `ccmux_log_events_total`; a new one past that is
/// counted under `service="other"` and `event="other"`.
pub const MAX_LOG_EVENT_SERIES: usize = 512;
