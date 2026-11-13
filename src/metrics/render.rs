//! Rendering: one registry per call, encoded in the format the client asked
//! for.

use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::{EncodeLabelSet, prometheus_text, text};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::{ConstGauge, Gauge};
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::{Registry, Unit};

use super::process::{ProcessMetrics, process_start_seconds};
use super::request::{CodexWindows, GaugeReading, PairLabels, RequestMetrics};

const OPEN_METRICS_MEDIA_TYPE: &[u8] = b"application/openmetrics-text";
const OPEN_METRICS_CONTENT_TYPE: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    OpenMetrics,
    Text,
}

/// OpenMetrics when the `Accept` header names its media type in any ASCII
/// case, else the Prometheus text format.
pub fn format_for_accept(accept: Option<&str>) -> Format {
    let names_open_metrics = accept.is_some_and(|accept| {
        accept
            .as_bytes()
            .windows(OPEN_METRICS_MEDIA_TYPE.len())
            .any(|window| window.eq_ignore_ascii_case(OPEN_METRICS_MEDIA_TYPE))
    });
    if names_open_metrics {
        Format::OpenMetrics
    } else {
        Format::Text
    }
}

pub fn content_type(format: Format) -> &'static str {
    match format {
        Format::OpenMetrics => OPEN_METRICS_CONTENT_TYPE,
        Format::Text => prometheus_text::CONTENT_TYPE,
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StateLabels {
    state: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct WindowLabels {
    window: &'static str,
}

#[derive(Clone, Copy, Default)]
pub struct RenderInput<'a> {
    pub requests: Option<&'a RequestMetrics>,
    pub gauges: Option<&'a GaugeReading>,
    pub codex_windows: Option<&'a CodexWindows>,
    pub process: Option<&'a ProcessMetrics>,
}

/// Encodes the input in one fresh registry. An absent input leaves its
/// families out; build info and the start time are always there.
pub fn render(input: &RenderInput<'_>, format: Format) -> String {
    let mut registry = Registry::with_prefix("ccmux");
    if let Some(requests) = input.requests {
        requests.register(&mut registry);
        if let Some(gauges) = input.gauges {
            registry.register(
                "requests_in_flight",
                "Requests in flight",
                in_flight(requests, gauges),
            );
        }
    }
    if let Some(gauges) = input.gauges {
        register_sessions(&mut registry, gauges);
    }
    if let Some(windows) = input.codex_windows {
        register_codex_windows(&mut registry, windows);
    }
    if let Some(process) = input.process {
        process.register(&mut registry);
    }
    registry.register(
        "build",
        "Version of the running proxy",
        Info::new(BuildLabels {
            version: env!("CARGO_PKG_VERSION"),
        }),
    );
    registry.register_with_unit(
        "process_start_time",
        "Unix time the process started",
        Unit::Seconds,
        ConstGauge::new(process_start_seconds()),
    );

    let mut output = String::new();
    let encoded = match format {
        Format::OpenMetrics => text::encode(&mut output, &registry),
        Format::Text => prometheus_text::encode(&mut output, &registry),
    };
    // Writing into a `String` does not fail, and no family here is of a kind
    // an encoder refuses; should one ever fail, answer nothing rather than
    // half a document.
    if encoded.is_err() {
        output.clear();
    }
    output
}

/// Entries whose labels resolve to the same pair are summed, stopping at
/// `u64::MAX`; an entry with no request in flight adds no sample.
fn in_flight(
    requests: &RequestMetrics,
    gauges: &GaugeReading,
) -> Family<PairLabels, Gauge<u64, AtomicU64>> {
    let family = Family::<PairLabels, Gauge<u64, AtomicU64>>::default();
    for entry in gauges.in_flight.iter().filter(|entry| entry.requests > 0) {
        let labels = requests.pair_labels(entry.provider.as_deref(), entry.model.as_deref());
        // The family is local to this render, so nothing else writes it
        // between the read and the write.
        let gauge = family.get_or_create(&labels);
        gauge.set(gauge.get().saturating_add(entry.requests));
    }
    family
}

fn register_sessions(registry: &mut Registry, gauges: &GaugeReading) {
    let sessions = Family::<StateLabels, Gauge<u64, AtomicU64>>::default();
    sessions
        .get_or_create(&StateLabels { state: "active" })
        .set(gauges.sessions_active);
    sessions
        .get_or_create(&StateLabels { state: "idle" })
        .set(gauges.sessions_idle);
    registry.register("sessions", "Sessions the monitor holds, by state", sessions);
    registry.register(
        "sessions_started",
        "Sessions started since the process started",
        ConstCounter::new(gauges.sessions_started),
    );
}

fn register_codex_windows(registry: &mut Registry, windows: &CodexWindows) {
    let used = Family::<WindowLabels, Gauge<f64, AtomicU64>>::default();
    let resets = Family::<WindowLabels, Gauge<u64, AtomicU64>>::default();
    for (window, reading) in [("5h", windows.five_hour), ("7d", windows.seven_day)] {
        let Some(reading) = reading else { continue };
        let labels = WindowLabels { window };
        used.get_or_create(&labels).set(reading.used_ratio);
        resets.get_or_create(&labels).set(reading.resets_at);
    }
    registry.register(
        "codex_window_used_ratio",
        "Share of the Codex allowance window already used",
        used,
    );
    registry.register_with_unit(
        "codex_window_reset_timestamp",
        "Unix time the Codex allowance window resets",
        Unit::Seconds,
        resets,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::*;
    use crate::metrics::{
        CodexWindow, CodexWindows, GaugeReading, InFlight, Outcome, ProcessMetrics, RefusalClass,
        RequestMetrics, SettledRequest, mark_process_start,
    };

    const BOTH_FORMATS: [Format; 2] = [Format::OpenMetrics, Format::Text];

    fn samples(output: &str) -> BTreeSet<String> {
        output
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

    fn set(lines: &[&str]) -> BTreeSet<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    fn sample_names(output: &str) -> BTreeSet<String> {
        samples(output)
            .iter()
            .map(|line| line.split(['{', ' ']).next().unwrap_or_default().to_owned())
            .collect()
    }

    fn start_time(output: &str) -> u64 {
        let line = samples(output)
            .into_iter()
            .find(|line| line.starts_with("ccmux_process_start_time_seconds "))
            .expect("start time sample");
        line.rsplit(' ')
            .next()
            .and_then(|value| value.parse().ok())
            .expect("integer start time")
    }

    fn build_info_line() -> String {
        format!(
            "ccmux_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn empty_input_renders_only_build_info_and_start_time() {
        let names = set(&["ccmux_build_info", "ccmux_process_start_time_seconds"]);

        let open_metrics = render(&RenderInput::default(), Format::OpenMetrics);
        assert_eq!(sample_names(&open_metrics), names);
        assert!(samples(&open_metrics).contains(&build_info_line()));
        assert!(open_metrics.ends_with("# EOF\n"), "{open_metrics}");

        let text = render(&RenderInput::default(), Format::Text);
        assert_eq!(sample_names(&text), names);
        assert!(samples(&text).contains(&build_info_line()));
        assert!(!text.contains("# EOF"), "{text}");
    }

    #[test]
    fn the_start_time_is_positive_and_stable() {
        let first = start_time(&render(&RenderInput::default(), Format::OpenMetrics));
        let second = start_time(&render(&RenderInput::default(), Format::Text));
        assert!(first > 0);
        assert_eq!(first, second);
        mark_process_start();
        assert_eq!(
            start_time(&render(&RenderInput::default(), Format::OpenMetrics)),
            first
        );
    }

    #[test]
    fn format_follows_the_accept_header() {
        for accept in [
            None,
            Some("*/*"),
            Some("text/plain"),
            Some("text/plain; version=0.0.4"),
            Some(""),
        ] {
            assert_eq!(format_for_accept(accept), Format::Text, "{accept:?}");
        }
        for accept in [
            "application/openmetrics-text",
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
            "Application/OpenMetrics-Text;version=0.0.1",
            "text/plain;q=0.5, APPLICATION/OPENMETRICS-TEXT;q=1",
        ] {
            assert_eq!(
                format_for_accept(Some(accept)),
                Format::OpenMetrics,
                "{accept}"
            );
        }
        assert_eq!(
            content_type(Format::OpenMetrics),
            "application/openmetrics-text; version=1.0.0; charset=utf-8"
        );
        assert_eq!(
            content_type(Format::Text),
            "text/plain; version=0.0.4; charset=utf-8"
        );
    }

    #[test]
    fn gauges_absent_from_the_input_render_no_sample() {
        let requests = RequestMetrics::new();
        let reading = GaugeReading {
            sessions_active: 1,
            sessions_idle: 1,
            sessions_started: 2,
            in_flight: vec![InFlight {
                provider: Some("codex".to_owned()),
                model: Some("gpt-6-sol".to_owned()),
                requests: 1,
            }],
        };
        let windows = CodexWindows::default();
        let inputs = [
            RenderInput {
                requests: Some(&requests),
                ..RenderInput::default()
            },
            RenderInput {
                gauges: Some(&reading),
                ..RenderInput::default()
            },
            RenderInput {
                requests: Some(&requests),
                codex_windows: Some(&windows),
                ..RenderInput::default()
            },
        ];
        for format in BOTH_FORMATS {
            let without_gauges = render(&inputs[0], format);
            for family in [
                "ccmux_requests_in_flight",
                "ccmux_sessions",
                "ccmux_codex_window",
            ] {
                assert!(!without_gauges.contains(family), "{without_gauges}");
            }

            let without_requests = samples(&render(&inputs[1], format));
            assert!(with_prefix(&without_requests, "ccmux_requests_in_flight").is_empty());
            assert_eq!(with_prefix(&without_requests, "ccmux_sessions{").len(), 2);

            let empty_windows = render(&inputs[2], format);
            assert!(
                !empty_windows.contains("ccmux_codex_window"),
                "{empty_windows}"
            );
        }
    }

    #[test]
    fn a_gauge_reading_renders_its_session_values() {
        let reading = GaugeReading {
            sessions_active: 2,
            sessions_idle: 3,
            sessions_started: 7,
            in_flight: Vec::new(),
        };
        let input = RenderInput {
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_sessions{state=\"active\"} 2",
            "ccmux_sessions{state=\"idle\"} 3",
            "ccmux_sessions_started_total 7",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_sessions"), expected);
        }
    }

    #[test]
    fn sessions_gauge_shows_both_states_at_zero() {
        let reading = GaugeReading::default();
        let input = RenderInput {
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_sessions{state=\"active\"} 0",
            "ccmux_sessions{state=\"idle\"} 0",
            "ccmux_sessions_started_total 0",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_sessions"), expected);
        }
    }

    #[test]
    fn codex_windows_render_ratio_and_reset() {
        let windows = CodexWindows {
            five_hour: Some(CodexWindow {
                used_ratio: 0.25,
                resets_at: 1_800_000_000,
            }),
            seven_day: Some(CodexWindow {
                used_ratio: 1.0,
                resets_at: 1_800_500_000,
            }),
        };
        let input = RenderInput {
            codex_windows: Some(&windows),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_codex_window_used_ratio{window=\"5h\"} 0.25",
            "ccmux_codex_window_used_ratio{window=\"7d\"} 1.0",
            "ccmux_codex_window_reset_timestamp_seconds{window=\"5h\"} 1800000000",
            "ccmux_codex_window_reset_timestamp_seconds{window=\"7d\"} 1800500000",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_codex_window"), expected);
        }

        let five_hour_only = CodexWindows {
            seven_day: None,
            ..windows
        };
        let input = RenderInput {
            codex_windows: Some(&five_hour_only),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_codex_window_used_ratio{window=\"5h\"} 0.25",
            "ccmux_codex_window_reset_timestamp_seconds{window=\"5h\"} 1800000000",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_codex_window"), expected);
        }
    }

    #[test]
    fn in_flight_entries_with_the_same_labels_are_summed() {
        let requests = RequestMetrics::new();
        let entry = |provider: Option<&str>, model: Option<String>, count: u64| InFlight {
            provider: provider.map(str::to_owned),
            model,
            requests: count,
        };
        let reading = GaugeReading {
            in_flight: vec![
                entry(Some("codex"), Some("gpt-6-sol".to_owned()), 2),
                entry(Some("codex"), Some("gpt-6-sol".to_owned()), 3),
                entry(None, None, 1),
                entry(Some("codex"), Some("x".repeat(121)), 1),
                entry(Some("codex"), Some("y".repeat(121)), 4),
                entry(Some("kimi"), Some("never-settled".to_owned()), 0),
            ],
            ..GaugeReading::default()
        };
        let input = RenderInput {
            requests: Some(&requests),
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_requests_in_flight{provider=\"codex\",model=\"gpt-6-sol\"} 5",
            "ccmux_requests_in_flight{provider=\"unknown\",model=\"unknown\"} 1",
            "ccmux_requests_in_flight{provider=\"other\",model=\"other\"} 5",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_requests_in_flight"), expected);
        }
    }

    fn one_in_flight(model: &str) -> InFlight {
        InFlight {
            provider: Some("codex".to_owned()),
            model: Some(model.to_owned()),
            requests: 1,
        }
    }

    fn in_flight_line(provider: &str, model: &str) -> String {
        format!("ccmux_requests_in_flight{{provider=\"{provider}\",model=\"{model}\"}} 1")
    }

    fn settled(model: &str) -> SettledRequest<'_> {
        SettledRequest {
            provider: Some("codex"),
            model: Some(model),
            endpoint: "messages",
            outcome: Outcome::Completed,
            http_status: Some(200),
            duration: Duration::from_secs(1),
            time_to_first_token: None,
            usage: None,
        }
    }

    fn requests_line(provider: &str, model: &str) -> String {
        format!(
            "ccmux_requests_total{{provider=\"{provider}\",model=\"{model}\",endpoint=\"messages\",outcome=\"completed\",status=\"200\"}} 1"
        )
    }

    #[test]
    fn pairs_admitted_in_flight_take_the_budget_of_settlements() {
        let requests = RequestMetrics::new();
        let models: Vec<String> = (0..200).map(|index| format!("model-{index}")).collect();
        let reading = GaugeReading {
            in_flight: models.iter().map(|model| one_in_flight(model)).collect(),
            ..GaugeReading::default()
        };
        let with_gauges = RenderInput {
            requests: Some(&requests),
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let admitted: BTreeSet<String> = models
            .iter()
            .map(|model| in_flight_line("codex", model))
            .collect();
        assert_eq!(
            with_prefix(
                &samples(&render(&with_gauges, Format::Text)),
                "ccmux_requests_in_flight"
            ),
            admitted
        );

        requests.request_settled(&settled("model-0"));
        requests.request_settled(&settled("model-200"));
        let settlements = RenderInput {
            requests: Some(&requests),
            ..RenderInput::default()
        };
        let expected = BTreeSet::from([
            requests_line("codex", "model-0"),
            requests_line("other", "other"),
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&settlements, format));
            assert_eq!(with_prefix(&samples, "ccmux_requests_total"), expected);
        }
    }

    #[test]
    fn pairs_admitted_by_settlements_take_the_budget_of_the_in_flight_gauge() {
        let requests = RequestMetrics::new();
        for index in 0..200 {
            requests.request_settled(&settled(&format!("model-{index}")));
        }
        let reading = GaugeReading {
            in_flight: vec![one_in_flight("model-0"), one_in_flight("model-200")],
            ..GaugeReading::default()
        };
        let input = RenderInput {
            requests: Some(&requests),
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let expected = BTreeSet::from([
            in_flight_line("codex", "model-0"),
            in_flight_line("other", "other"),
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_requests_in_flight"), expected);
        }
    }

    #[test]
    fn an_in_flight_sum_stops_at_the_largest_value() {
        let requests = RequestMetrics::new();
        let entry = |count: u64| InFlight {
            provider: Some("codex".to_owned()),
            model: Some("gpt-6-sol".to_owned()),
            requests: count,
        };
        let reading = GaugeReading {
            in_flight: vec![entry(u64::MAX), entry(1)],
            ..GaugeReading::default()
        };
        let input = RenderInput {
            requests: Some(&requests),
            gauges: Some(&reading),
            ..RenderInput::default()
        };
        let expected = set(&[
            "ccmux_requests_in_flight{provider=\"codex\",model=\"gpt-6-sol\"} 18446744073709551615",
        ]);
        for format in BOTH_FORMATS {
            let samples = samples(&render(&input, format));
            assert_eq!(with_prefix(&samples, "ccmux_requests_in_flight"), expected);
        }
    }

    #[test]
    fn process_absent_renders_no_process_family() {
        let process = ProcessMetrics::new();
        process.log_event("server", "info", "listening");
        process.codex_refusal(RefusalClass::Throttled);

        let with_process = RenderInput {
            process: Some(&process),
            ..RenderInput::default()
        };
        for format in BOTH_FORMATS {
            let samples = samples(&render(&with_process, format));
            assert!(samples.contains(
                "ccmux_log_events_total{service=\"server\",level=\"info\",event=\"listening\"} 1"
            ));
            assert!(samples.contains("ccmux_codex_refusals_total{class=\"throttled\"} 1"));

            let without = render(&RenderInput::default(), format);
            assert!(!without.contains("ccmux_log_events"), "{without}");
            assert!(!without.contains("ccmux_codex_refusals"), "{without}");
        }
    }
}
