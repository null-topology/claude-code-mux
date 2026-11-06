//! Publishing Codex quota state in the vocabulary Anthropic clients read.
//!
//! Codex reports how much of each subscription window is spent inside the
//! stream, as a `codex.rate_limits` event, while Anthropic clients expect that
//! state in response headers — which are already on the wire by the time the
//! event arrives. So the newest reading of each window is kept here and rides
//! the next response: a quota gauge a few seconds stale, which is what a gauge
//! fed by the previous turn's telemetry would be anyway.
//!
//! Without this the only quota signal a client ever sees is the refusal, and a
//! limit reached with no warning beforehand is exactly the case where nobody
//! knows why the assistant stopped answering.

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Codex reports `used_percent` on a 0..100 scale; the headers carry a ratio.
const PERCENT: f64 = 100.0;

/// A window is the five hour one when it is no longer than this. Codex reports
/// 300 minutes for it (299 in older payloads), against 10080 for the weekly.
const FIVE_HOUR_MAX_MINUTES: u64 = 360;

/// The other end of the five hour band, and the band around the weekly window.
/// These exist so a duration can be recognised rather than merely compared: a
/// reading shorter than this names some window other than the five hour one.
const FIVE_HOUR_MIN_MINUTES: u64 = 240;
const SEVEN_DAY_MIN_MINUTES: u64 = 9_000;
const SEVEN_DAY_MAX_MINUTES: u64 = 11_000;

/// The window a reported duration names, when it is one this build can name.
///
/// Announcing a window to a client is a claim about which allowance refused a
/// request, so a duration that falls outside both bands is left unnamed rather
/// than rounded into the nearer one — a window Codex adds later would otherwise
/// be published under the wrong name. Binning a healthy reading into a gauge
/// lane is the other question and stays deliberately tolerant: there being no
/// name for a lane is worse than an approximate one for a gauge.
pub(crate) fn claimable_window(minutes: u64) -> Option<super::events::CodexLimitWindow> {
    if (FIVE_HOUR_MIN_MINUTES..=FIVE_HOUR_MAX_MINUTES).contains(&minutes) {
        Some(super::events::CodexLimitWindow::FiveHour)
    } else if (SEVEN_DAY_MIN_MINUTES..=SEVEN_DAY_MAX_MINUTES).contains(&minutes) {
        Some(super::events::CodexLimitWindow::SevenDay)
    } else {
        None
    }
}

/// Where each window starts being worth announcing. These mirror the thresholds
/// the client applies to Anthropic's own windows, so a Codex session warns at
/// the same points a Claude session does.
const FIVE_HOUR_WARN_AT: f64 = 0.9;
const SEVEN_DAY_WARN_AT: f64 = 0.75;

/// Lowers both thresholds, for a consumer that wants the spent fraction earlier
/// than the defaults publish it — the fraction only reaches a caller once a
/// threshold is declared surpassed. Interfaces that draw their own warning have
/// a floor of their own well above zero, so a low setting here feeds a watcher
/// without turning the terminal noisy.
const WARN_AT_ENV: &str = "CCP_CODEX_QUOTA_WARN_AT";

fn warn_at(default: f64) -> f64 {
    static OVERRIDE: OnceLock<Option<f64>> = OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            std::env::var(WARN_AT_ENV)
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| (0.0..=1.0).contains(value))
        })
        .unwrap_or(default)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowState {
    /// Spent fraction of the window. Can exceed 1.0: usage legitimately runs
    /// past a cap before the refusal lands.
    pub utilization: f64,
    pub resets_at: u64,
}

impl WindowState {
    fn surpassed(&self, threshold: f64) -> bool {
        self.utilization >= threshold
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub five_hour: Option<WindowState>,
    pub seven_day: Option<WindowState>,
}

impl Snapshot {
    /// The window a client should name when it mentions one: whichever is
    /// closest to running out, since that is the one that will stop the work.
    fn representative(&self) -> Option<(&'static str, WindowState)> {
        match (self.five_hour, self.seven_day) {
            (Some(five), Some(seven)) if seven.utilization > five.utilization => {
                Some(("seven_day", seven))
            }
            (Some(five), _) => Some(("five_hour", five)),
            (None, Some(seven)) => Some(("seven_day", seven)),
            (None, None) => None,
        }
    }

    /// The headers that carry this snapshot, as name/value pairs.
    pub(crate) fn headers(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let Some((claim, representative)) = self.representative() else {
            return out;
        };

        out.push(("anthropic-ratelimit-unified-status", "allowed".to_string()));
        out.push((
            "anthropic-ratelimit-unified-reset",
            representative.resets_at.to_string(),
        ));
        out.push((
            "anthropic-ratelimit-unified-representative-claim",
            claim.to_string(),
        ));

        for (window, threshold, utilization_header, reset_header, threshold_header) in [
            (
                self.five_hour,
                FIVE_HOUR_WARN_AT,
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-5h-reset",
                "anthropic-ratelimit-unified-5h-surpassed-threshold",
            ),
            (
                self.seven_day,
                SEVEN_DAY_WARN_AT,
                "anthropic-ratelimit-unified-7d-utilization",
                "anthropic-ratelimit-unified-7d-reset",
                "anthropic-ratelimit-unified-7d-surpassed-threshold",
            ),
        ] {
            let Some(state) = window else { continue };
            let threshold = warn_at(threshold);
            out.push((utilization_header, format!("{:.4}", state.utilization)));
            out.push((reset_header, state.resets_at.to_string()));
            // The client reports the spent fraction to its caller only once a
            // threshold is declared surpassed, so a window worth warning about
            // has to say which line it crossed.
            if state.surpassed(threshold) {
                out.push((threshold_header, format!("{threshold:.2}")));
            }
        }

        out
    }
}

/// The snapshot held, with the ChatGPT account of the requests whose readings
/// built it. Windows belong to one account's allowance, so a reading for
/// another account replaces them rather than merging into them, and a request
/// for another account is told nothing from them.
struct Held {
    account: Option<String>,
    snapshot: Snapshot,
}

static LATEST: OnceLock<Mutex<Option<Held>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Held>> {
    LATEST.get_or_init(|| Mutex::new(None))
}

/// The snapshot `held` keeps for `account`: none when it was built for
/// another account.
fn held_for(held: Option<Held>, account: Option<&str>) -> Option<Snapshot> {
    held.filter(|held| held.account.as_deref() == account)
        .map(|held| held.snapshot)
}

/// `snapshot`, held for `account`.
fn hold(snapshot: Option<Snapshot>, account: Option<&str>) -> Option<Held> {
    snapshot.map(|snapshot| Held {
        account: account.map(str::to_owned),
        snapshot,
    })
}

/// Forget the snapshot, for a test that leaves a reading behind.
pub(crate) fn clear_for_tests() {
    if let Ok(mut latest) = cell().lock() {
        *latest = None;
    }
}

/// Taken by every unit test that writes or reads the snapshot, so a reading
/// one test leaves never reaches another test's assertions.
#[cfg(test)]
pub(crate) fn lock_for_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record the quota state carried by a `codex.rate_limits` event, for the
/// ChatGPT account of the request it came with. Any other event is ignored,
/// so this can be handed every frame off the stream, and so is one that reads
/// a metered limit other than `codex` (`reads_codex_limit`).
pub(crate) fn observe_event(account: Option<&str>, payload: &Value) {
    if payload.get("type").and_then(Value::as_str) != Some("codex.rate_limits")
        || !reads_codex_limit(payload)
    {
        return;
    }
    let Some(snapshot) = snapshot_from_event(payload) else {
        return;
    };
    if let Ok(mut latest) = cell().lock() {
        let held = held_for(latest.take(), account);
        *latest = hold(merge_snapshot(held, snapshot, now()), account);
    }
}

/// Whether a `codex.rate_limits` event reads the `codex` metered limit, the
/// allowance the gauge stands for. The event names its limit in
/// `metered_limit_name`, else `limit_name`, and one naming neither reads
/// `codex`, as the Codex CLI files it
/// (`codex-rs/codex-api/src/rate_limits.rs:124-167` at rust-v0.166.0, with
/// the name trimmed, lowercased and `-` read as `_` at :270-272). A name that
/// is not a string names no limit the gauge knows.
pub(crate) fn reads_codex_limit(payload: &Value) -> bool {
    let named = ["metered_limit_name", "limit_name"]
        .into_iter()
        .find_map(|key| payload.get(key).filter(|value| !value.is_null()));
    match named {
        None => true,
        Some(Value::String(name)) => name.trim().to_ascii_lowercase().replace('-', "_") == "codex",
        Some(_) => false,
    }
}

/// Two readings of a window whose resets are at most this many seconds apart
/// name the same window: a reset worked out from a countdown can land a second
/// or two off the one stated.
const SAME_WINDOW_TOLERANCE_SECONDS: u64 = 60;

/// `held` with each window of `incoming` taken in its place
/// (`merge_window`). A window the incoming reading does not give keeps the
/// one held, which `latest` leaves out once its reset passes.
fn merge_snapshot(held: Option<Snapshot>, incoming: Snapshot, now: u64) -> Option<Snapshot> {
    let held = held.unwrap_or_default();
    let merged = Snapshot {
        five_hour: merge_window(held.five_hour, incoming.five_hour, now),
        seven_day: merge_window(held.seven_day, incoming.seven_day, now),
    };
    (merged != Snapshot::default()).then_some(merged)
}

/// The reading to hold for one window once `incoming` arrives. An incoming
/// window whose reset has passed, or one that resets more than
/// `SAME_WINDOW_TOLERANCE_SECONDS` before the held one, belongs to a window
/// that has since rolled over and leaves the held reading as it was.
fn merge_window(
    held: Option<WindowState>,
    incoming: Option<WindowState>,
    now: u64,
) -> Option<WindowState> {
    let Some(incoming) = incoming.filter(|state| state.resets_at > now) else {
        return held;
    };
    let held_is_newer = held.is_some_and(|held| {
        held.resets_at
            > incoming
                .resets_at
                .saturating_add(SAME_WINDOW_TOLERANCE_SECONDS)
    });
    if held_is_newer { held } else { Some(incoming) }
}

/// The most recent snapshot, with any window that has since rolled over left
/// out.
///
/// A reading travels one response behind the event that produced it, so the
/// turn right after a window reopens would otherwise carry the spent figure
/// from before the reset — and announce an allowance as nearly gone at the
/// moment it came back. A window whose reset time has passed says nothing about
/// the one now running. A snapshot held for another account than `account`
/// gives nothing.
pub(crate) fn latest(account: Option<&str>) -> Option<Snapshot> {
    let snapshot = cell().lock().ok().and_then(|latest| {
        latest
            .as_ref()
            .filter(|held| held.account.as_deref() == account)
            .map(|held| held.snapshot.clone())
    })?;
    still_running(snapshot, now())
}

fn still_running(snapshot: Snapshot, now: u64) -> Option<Snapshot> {
    let fresh = |window: Option<WindowState>| window.filter(|state| state.resets_at > now);
    let snapshot = Snapshot {
        five_hour: fresh(snapshot.five_hour),
        seven_day: fresh(snapshot.seven_day),
    };

    (snapshot != Snapshot::default()).then_some(snapshot)
}

fn snapshot_from_event(payload: &Value) -> Option<Snapshot> {
    let limits = payload.get("rate_limits")?;
    let mut snapshot = Snapshot::default();
    // Codex names the windows by rank rather than by length, and which rank
    // holds which length has changed between payload versions — so the window
    // is decided by the length it reports, not by the slot it arrived in.
    for slot in ["primary", "secondary"] {
        let Some(window) = limits.get(slot) else {
            continue;
        };
        let Some((minutes, state)) = window_state(window) else {
            continue;
        };
        if minutes <= FIVE_HOUR_MAX_MINUTES {
            snapshot.five_hour = Some(state);
        } else {
            snapshot.seven_day = Some(state);
        }
    }

    (snapshot != Snapshot::default()).then_some(snapshot)
}

/// The window `rate_limits` of a `codex.rate_limits` event names as the one
/// that refused the request, as its length in minutes and its reset moment:
/// the one window the event gives at or past its allowance. `None` when that
/// cannot be told: no spent window, two of them, or a given window whose
/// numbers are unusable, since that one may be the window that ran out. A
/// slot that is absent or `null` gives no window.
pub(crate) fn limiting_window(limits: &Value) -> Option<(u64, u64)> {
    let mut spent = None;
    for slot in ["primary", "secondary"] {
        let Some(window) = limits.get(slot).filter(|window| !window.is_null()) else {
            continue;
        };
        let (minutes, state) = window_state(window)?;
        if state.surpassed(1.0) {
            if spent.is_some() {
                return None;
            }
            spent = Some((minutes, state.resets_at));
        }
    }
    spent
}

/// Record the window a spent allowance names as used up until the reset the
/// limit gives, keeping the other window's last reading when it was held for
/// the limit's account. A limit that names no window or no reset cannot be
/// placed and changes nothing, and neither does one from a snapshot of a
/// metered limit other than `codex` (`CodexUsageLimit::feeds_gauge`).
pub(crate) fn observe_usage_limit(limit: &super::events::CodexUsageLimit) {
    let (Some(window), Some(resets_at), true) = (limit.window, limit.resets_at, limit.feeds_gauge)
    else {
        return;
    };
    let account = limit.account.as_deref();
    if let Ok(mut latest) = cell().lock() {
        let held = held_for(latest.take(), account);
        *latest = hold(mark_spent(held, window, resets_at, now()), account);
    }
}

/// `snapshot` with `window` used up until `resets_at`. A reset that has
/// passed, or one more than `SAME_WINDOW_TOLERANCE_SECONDS` earlier than the
/// reading already held for that window, belongs to a window that has since
/// rolled over, and leaves the snapshot as it was (`merge_window`).
fn mark_spent(
    snapshot: Option<Snapshot>,
    window: super::events::CodexLimitWindow,
    resets_at: u64,
    now: u64,
) -> Option<Snapshot> {
    if resets_at <= now {
        return snapshot;
    }
    let mut snapshot = snapshot.unwrap_or_default();
    let held = match window {
        super::events::CodexLimitWindow::FiveHour => &mut snapshot.five_hour,
        super::events::CodexLimitWindow::SevenDay => &mut snapshot.seven_day,
    };
    let spent = WindowState {
        utilization: 1.0,
        resets_at,
    };
    *held = merge_window(*held, Some(spent), now);
    Some(snapshot)
}

/// One window of a `codex.rate_limits` event. A window whose length, spent
/// percentage, reset moment or countdown is not a finite, non-negative number
/// in range is left out rather than guessed, and the event's other window still
/// counts. That holds for a reset field the window carries even when another
/// one is usable: the window then contradicts itself.
fn window_state(window: &Value) -> Option<(u64, WindowState)> {
    let minutes = whole(number(window.get("window_minutes"))?)?;
    let used_percent =
        number(window.get("used_percent")).filter(|value| value.is_finite() && *value >= 0.0)?;
    // The stream names the moment `reset_at`; the same window reserialised into
    // a session log calls it `resets_at`. Both spellings are read so a payload
    // from either side parses.
    let moment = carried(window, &["reset_at", "resets_at"])?;
    // Some payloads count down instead of naming the moment.
    let countdown = carried(window, &["reset_after_seconds", "resets_in_seconds"])?;
    let resets_at = match (moment, countdown) {
        (Some(at), _) => at,
        (None, Some(after)) => now().saturating_add(after),
        (None, None) => return None,
    };

    Some((
        minutes,
        WindowState {
            utilization: used_percent / PERCENT,
            resets_at,
        },
    ))
}

/// A count of minutes or seconds: finite, not negative and below `u64::MAX`,
/// truncated to whole units. Anything else has no value.
fn whole(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0 && value < u64::MAX as f64).then_some(value as u64)
}

/// The first of `names` the window carries, as a whole count: `Some(None)`
/// when it carries none of them (absent or `null`), `None` when one it carries
/// is not a usable count.
fn carried(window: &Value, names: &[&str]) -> Option<Option<u64>> {
    let mut first = None;
    for name in names {
        let value = window.get(*name).filter(|value| !value.is_null());
        if value.is_some() {
            let count = whole(number(value)?)?;
            first.get_or_insert(count);
        }
    }
    Some(first)
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape recorded off a live `gpt-5.6` stream, down to the field names.
    fn event(primary_percent: f64, secondary_percent: f64) -> Value {
        serde_json::json!({
            "type": "codex.rate_limits",
            "plan_type": "plus",
            "credits": {"balance": "0", "has_credits": false, "unlimited": false},
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "used_percent": primary_percent,
                    "window_minutes": 300,
                    "reset_after_seconds": 16394,
                    "reset_at": 1788879437u64
                },
                "secondary": {
                    "used_percent": secondary_percent,
                    "window_minutes": 10080,
                    "reset_after_seconds": 584857,
                    "reset_at": 1789466238u64
                }
            }
        })
    }

    fn headers_of(payload: &Value) -> std::collections::HashMap<&'static str, String> {
        snapshot_from_event(payload)
            .expect("snapshot")
            .headers()
            .into_iter()
            .collect()
    }

    #[test]
    fn names_only_the_window_durations_it_recognises() {
        use super::super::events::CodexLimitWindow;

        // 300 is what the stream reports for the five hour window and 299 is
        // what older payloads reported, so both have to reach the same name.
        assert_eq!(claimable_window(300), Some(CodexLimitWindow::FiveHour));
        assert_eq!(claimable_window(299), Some(CodexLimitWindow::FiveHour));
        assert_eq!(claimable_window(10_080), Some(CodexLimitWindow::SevenDay));
        // A daily window is neither, and guessing which of the two it is closer
        // to would publish a name Codex never used.
        assert_eq!(claimable_window(1_440), None);
    }

    #[test]
    fn maps_windows_by_their_length() {
        let headers = headers_of(&event(42.0, 16.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-utilization"],
            "0.4200"
        );
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-reset"],
            "1788879437"
        );
        assert_eq!(
            headers["anthropic-ratelimit-unified-7d-utilization"],
            "0.1600"
        );
        assert_eq!(headers["anthropic-ratelimit-unified-status"], "allowed");
    }

    #[test]
    fn stays_quiet_below_the_thresholds() {
        let headers = headers_of(&event(42.0, 16.0));
        assert!(!headers.contains_key("anthropic-ratelimit-unified-5h-surpassed-threshold"));
        assert!(!headers.contains_key("anthropic-ratelimit-unified-7d-surpassed-threshold"));
    }

    #[test]
    fn declares_the_threshold_a_window_crossed() {
        let headers = headers_of(&event(93.0, 16.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-5h-surpassed-threshold"],
            "0.90"
        );
        assert!(!headers.contains_key("anthropic-ratelimit-unified-7d-surpassed-threshold"));

        let weekly = headers_of(&event(10.0, 80.0));
        assert_eq!(
            weekly["anthropic-ratelimit-unified-7d-surpassed-threshold"],
            "0.75"
        );
    }

    #[test]
    fn names_the_window_closest_to_running_out() {
        let headers = headers_of(&event(10.0, 80.0));
        assert_eq!(
            headers["anthropic-ratelimit-unified-representative-claim"],
            "seven_day"
        );
        assert_eq!(headers["anthropic-ratelimit-unified-reset"], "1789466238");

        let five = headers_of(&event(80.0, 10.0));
        assert_eq!(
            five["anthropic-ratelimit-unified-representative-claim"],
            "five_hour"
        );
    }

    #[test]
    fn accepts_the_spelling_a_session_log_uses() {
        // The same window, reserialised by the Codex CLI into its rollout file.
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": {
                    "used_percent": 5.0,
                    "window_minutes": 299,
                    "resets_at": 1788879437u64
                }
            }
        });
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.expect("five hour").resets_at, 1788879437);
    }

    #[test]
    fn accepts_a_countdown_instead_of_a_moment() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": {
                    "used_percent": 5.0,
                    "window_minutes": 299,
                    "resets_in_seconds": 17940
                }
            }
        });
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        let five = snapshot.five_hour.expect("five hour window");
        assert!(five.resets_at > now());
        assert!(five.resets_at <= now() + 17940);
    }

    #[test]
    fn drops_a_window_that_has_since_rolled_over() {
        let spent = snapshot_from_event(&event(100.0, 80.0)).expect("snapshot");
        // Both windows reset long before this moment.
        assert_eq!(still_running(spent.clone(), 1_800_000_000), None);

        // The weekly window is still running; the five hour one has rolled over.
        let surviving = still_running(spent, 1_789_000_000).expect("weekly window survives");
        assert!(surviving.five_hour.is_none());
        assert!(surviving.seven_day.is_some());
        let headers: std::collections::HashMap<_, _> = surviving.headers().into_iter().collect();
        assert!(!headers.contains_key("anthropic-ratelimit-unified-5h-utilization"));
        assert_eq!(
            headers["anthropic-ratelimit-unified-representative-claim"],
            "seven_day"
        );
    }

    #[test]
    fn drops_only_the_window_whose_numbers_are_unusable() {
        let unusable = [
            ("window_minutes", serde_json::json!("NaN")),
            ("window_minutes", serde_json::json!(-300)),
            ("window_minutes", serde_json::json!(1e300)),
            ("used_percent", serde_json::json!("NaN")),
            ("used_percent", serde_json::json!("inf")),
            ("used_percent", serde_json::json!(-1.0)),
            ("reset_at", serde_json::json!("NaN")),
            ("reset_at", serde_json::json!(-1)),
            ("reset_at", serde_json::json!(1e300)),
        ];
        for (field, value) in unusable {
            let mut payload = event(42.0, 16.0);
            payload["rate_limits"]["primary"][field] = value.clone();
            let snapshot = snapshot_from_event(&payload).expect("the weekly window survives");
            assert_eq!(snapshot.five_hour, None, "{field}: {value}");
            assert_eq!(
                snapshot.seven_day.expect("weekly window").resets_at,
                1789466238,
                "{field}: {value}"
            );
        }

        // A countdown read only when no moment is named.
        for countdown in [
            serde_json::json!("NaN"),
            serde_json::json!(-5),
            serde_json::json!(1e300),
        ] {
            let mut payload = event(42.0, 16.0);
            let primary = payload["rate_limits"]["primary"].as_object_mut().unwrap();
            primary.remove("reset_at");
            primary.insert("reset_after_seconds".to_string(), countdown.clone());
            let snapshot = snapshot_from_event(&payload).expect("the weekly window survives");
            assert_eq!(snapshot.five_hour, None, "{countdown}");
            assert!(snapshot.seven_day.is_some(), "{countdown}");
        }

        // A reset field the window carries counts even beside a usable one.
        let contradictions = [
            ("reset_at", serde_json::json!("soon")),
            ("reset_at", serde_json::json!(true)),
            ("reset_after_seconds", serde_json::json!("NaN")),
            ("reset_after_seconds", serde_json::json!(-5)),
            ("reset_after_seconds", serde_json::json!("soon")),
            ("resets_in_seconds", serde_json::json!("inf")),
        ];
        for (field, value) in contradictions {
            let mut payload = event(42.0, 16.0);
            payload["rate_limits"]["primary"][field] = value.clone();
            let snapshot = snapshot_from_event(&payload).expect("the weekly window survives");
            assert_eq!(snapshot.five_hour, None, "{field}: {value}");
            assert!(snapshot.seven_day.is_some(), "{field}: {value}");
        }

        // An absent or `null` field is not an unusable one.
        let mut payload = event(42.0, 16.0);
        payload["rate_limits"]["primary"]["reset_after_seconds"] = Value::Null;
        payload["rate_limits"]["primary"]["resets_at"] = Value::Null;
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.expect("five hour").resets_at, 1788879437);
    }

    #[test]
    fn a_countdown_past_the_clock_saturates_instead_of_wrapping() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": {
                    "used_percent": "5",
                    "window_minutes": "300",
                    "reset_after_seconds": "18446744073709549568"
                }
            }
        });
        let snapshot = snapshot_from_event(&payload).expect("snapshot");
        assert_eq!(snapshot.five_hour.expect("five hour").resets_at, u64::MAX);
    }

    #[test]
    fn the_limiting_window_is_the_one_spent_window() {
        let mut spent = event(100.0, 40.0);
        assert_eq!(
            limiting_window(&spent["rate_limits"]),
            Some((300, 1788879437))
        );
        spent["rate_limits"]["primary"]["used_percent"] = serde_json::json!(40.0);
        spent["rate_limits"]["secondary"]["used_percent"] = serde_json::json!(100.0);
        assert_eq!(
            limiting_window(&spent["rate_limits"]),
            Some((10080, 1789466238))
        );
        // Two spent windows, or two that are not: no telling which refused.
        assert_eq!(limiting_window(&event(100.0, 100.0)["rate_limits"]), None);
        assert_eq!(limiting_window(&event(40.0, 40.0)["rate_limits"]), None);

        // A lone window names itself only when it is spent; a `null` slot
        // gives no window.
        let mut lone = event(40.0, 0.0);
        lone["rate_limits"]["secondary"] = Value::Null;
        assert_eq!(limiting_window(&lone["rate_limits"]), None);
        lone["rate_limits"]["primary"]["used_percent"] = serde_json::json!(100.0);
        assert_eq!(
            limiting_window(&lone["rate_limits"]),
            Some((300, 1788879437))
        );

        // An unusable window may be the one that ran out, so nothing is named,
        // whether it reads as spent or the other one does.
        let mut broken = event(100.0, 40.0);
        broken["rate_limits"]["primary"]["reset_at"] = serde_json::json!("NaN");
        assert_eq!(limiting_window(&broken["rate_limits"]), None);
        let mut broken = event(100.0, 40.0);
        broken["rate_limits"]["secondary"]["used_percent"] = serde_json::json!("NaN");
        assert_eq!(limiting_window(&broken["rate_limits"]), None);
    }

    #[test]
    fn a_spent_allowance_marks_its_window_used_up() {
        use super::super::events::CodexLimitWindow;

        let now = 1_788_000_000;
        let held = snapshot_from_event(&event(42.0, 16.0));
        let marked = mark_spent(held.clone(), CodexLimitWindow::FiveHour, 1788879999, now)
            .expect("snapshot");
        assert_eq!(
            marked.five_hour,
            Some(WindowState {
                utilization: 1.0,
                resets_at: 1788879999
            })
        );
        assert_eq!(
            marked.seven_day.expect("weekly reading kept").utilization,
            0.16
        );

        // With no reading held, the spent window is the whole snapshot.
        let alone =
            mark_spent(None, CodexLimitWindow::SevenDay, 1789466238, now).expect("snapshot");
        assert_eq!(alone.five_hour, None);
        assert_eq!(alone.seven_day.expect("weekly").utilization, 1.0);
    }

    #[test]
    fn a_stale_allowance_leaves_a_newer_reading_alone() {
        use super::super::events::CodexLimitWindow;

        // A fresh reading of windows that reset later, then a limit for the
        // five hour window that came before them.
        let fresh = snapshot_from_event(&event(3.0, 16.0));
        let now = 1_788_000_000;
        assert_eq!(
            mark_spent(fresh.clone(), CodexLimitWindow::FiveHour, 1788800000, now),
            fresh
        );
        // A reset that has already passed names no window still running.
        assert_eq!(
            mark_spent(
                fresh.clone(),
                CodexLimitWindow::FiveHour,
                1788879999,
                1788879999
            ),
            fresh
        );
        assert_eq!(
            mark_spent(None, CodexLimitWindow::SevenDay, 1789466238, 1789466239),
            None
        );
        // The same reset as the reading held still marks it spent.
        let same =
            mark_spent(fresh, CodexLimitWindow::FiveHour, 1788879437, now).expect("snapshot");
        assert_eq!(same.five_hour.expect("five hour").utilization, 1.0);
    }

    /// `event` with each window resetting at the moment given, with no
    /// countdown beside it.
    fn event_resetting(
        primary_percent: f64,
        primary_reset: u64,
        secondary_percent: f64,
        secondary_reset: u64,
    ) -> Value {
        let mut payload = event(primary_percent, secondary_percent);
        for (slot, reset) in [("primary", primary_reset), ("secondary", secondary_reset)] {
            let window = payload["rate_limits"][slot]
                .as_object_mut()
                .expect("window");
            window.remove("reset_after_seconds");
            window.insert("reset_at".to_string(), reset.into());
        }
        payload
    }

    /// The same reading turned into a snapshot that refuses the request.
    fn refusing(mut payload: Value) -> Value {
        payload["rate_limits"]["allowed"] = false.into();
        payload["rate_limits"]["limit_reached"] = true.into();
        assert!(super::super::events::is_terminal_rate_limit_event(&payload));
        payload
    }

    /// What a live stream records for a snapshot: the reading, then the
    /// spent window the refusal names.
    fn observe_refusing_snapshot(payload: &Value) {
        observe_event(None, payload);
        if let Some(limit) = super::super::events::usage_limit_from_event_with_headers(payload, &[])
        {
            observe_usage_limit(&limit);
        }
    }

    fn latest_headers() -> std::collections::HashMap<&'static str, String> {
        headers_for(None).expect("snapshot")
    }

    fn headers_for(
        account: Option<&str>,
    ) -> Option<std::collections::HashMap<&'static str, String>> {
        latest(account).map(|snapshot| snapshot.headers().into_iter().collect())
    }

    #[test]
    fn a_late_refusing_snapshot_leaves_a_newer_window_alone() {
        let _snapshot = lock_for_tests();
        clear_for_tests();
        let now = now();
        let newer = now + 10_000;
        let older = newer - SAME_WINDOW_TOLERANCE_SECONDS - 1;
        let weekly = now + 500_000;
        observe_event(None, &event_resetting(3.0, newer, 16.0, weekly));

        let late = refusing(event_resetting(100.0, older, 16.0, weekly));
        let limit = super::super::events::usage_limit_from_event_with_headers(&late, &[])
            .expect("the refusal names its window");
        assert_eq!(limit.resets_at, Some(older));
        observe_refusing_snapshot(&late);

        let headers = latest_headers();
        clear_for_tests();
        assert_eq!(
            headers.get("anthropic-ratelimit-unified-5h-reset"),
            Some(&newer.to_string())
        );
        assert_eq!(
            headers.get("anthropic-ratelimit-unified-5h-utilization"),
            Some(&"0.0300".to_string())
        );
    }

    #[test]
    fn an_expired_refusing_snapshot_changes_nothing() {
        let _snapshot = lock_for_tests();
        clear_for_tests();
        let now = now();
        observe_event(
            None,
            &event_resetting(3.0, now + 10_000, 16.0, now + 500_000),
        );
        let before = latest(None);
        assert!(before.is_some());

        observe_refusing_snapshot(&refusing(event_resetting(100.0, now - 10, 100.0, now - 5)));

        let after = latest(None);
        clear_for_tests();
        assert_eq!(after, before);
    }

    #[test]
    fn a_reading_within_the_tolerance_names_the_same_window() {
        use super::super::events::CodexLimitWindow;

        let now = 1_788_000_000;
        // The five hour window held resets at 1788879437.
        let held = snapshot_from_event(&event(42.0, 16.0));

        // A limit whose reset is a second before the held one still marks
        // that window spent, and so does one at the edge of the tolerance.
        for resets_at in [1788879436, 1788879437 - SAME_WINDOW_TOLERANCE_SECONDS] {
            let marked = mark_spent(held.clone(), CodexLimitWindow::FiveHour, resets_at, now)
                .expect("snapshot");
            assert_eq!(
                marked.five_hour,
                Some(WindowState {
                    utilization: 1.0,
                    resets_at
                })
            );
        }
        // One second past the tolerance it names a window that came before.
        assert_eq!(
            mark_spent(
                held.clone(),
                CodexLimitWindow::FiveHour,
                1788879437 - SAME_WINDOW_TOLERANCE_SECONDS - 1,
                now
            ),
            held
        );

        // The same rule for a reading: inside the tolerance it replaces the
        // held window, past it the held window stays, and a window the
        // reading does not give keeps the one held.
        let held = held.expect("snapshot");
        let close = snapshot_from_event(&event_resetting(50.0, 1788879436, 20.0, 1789466238))
            .expect("snapshot");
        assert_eq!(
            merge_snapshot(Some(held.clone()), close.clone(), now),
            Some(close)
        );
        let stale = snapshot_from_event(&event_resetting(
            100.0,
            1788879437 - SAME_WINDOW_TOLERANCE_SECONDS - 1,
            16.0,
            1789466238,
        ))
        .expect("snapshot");
        assert_eq!(
            merge_snapshot(Some(held.clone()), stale, now),
            Some(held.clone())
        );
        let weekly_only = Snapshot {
            five_hour: None,
            seven_day: Some(WindowState {
                utilization: 0.5,
                resets_at: 1789466238,
            }),
        };
        let merged = merge_snapshot(Some(held.clone()), weekly_only, now).expect("snapshot");
        assert_eq!(merged.five_hour, held.five_hour);
        assert_eq!(merged.seven_day.expect("weekly").utilization, 0.5);
    }

    #[test]
    fn ignores_frames_that_are_not_quota_telemetry() {
        let _snapshot = lock_for_tests();
        observe_event(None, &serde_json::json!({"type": "response.completed"}));
        assert!(snapshot_from_event(&serde_json::json!({"type": "codex.rate_limits"})).is_none());
    }

    #[test]
    fn windows_are_held_for_the_account_that_read_them() {
        let _snapshot = lock_for_tests();
        clear_for_tests();
        let now = now();
        let later = now + 10_000;
        let earlier = later - SAME_WINDOW_TOLERANCE_SECONDS - 1;
        let weekly = now + 500_000;
        observe_event(Some("acct-a"), &event_resetting(30.0, later, 40.0, weekly));

        // A request for another account, or for none, is told nothing from
        // the windows another account's requests read.
        let for_b_before = headers_for(Some("acct-b"));
        let for_none = headers_for(None);
        let for_a = headers_for(Some("acct-a")).expect("account a");

        // Under the same account this reading would be stale and leave the
        // held five hour window alone; under another account it replaces
        // the snapshot whole.
        observe_event(
            Some("acct-b"),
            &event_resetting(70.0, earlier, 80.0, weekly),
        );
        let for_b = headers_for(Some("acct-b")).expect("account b");
        let for_a_after = headers_for(Some("acct-a"));

        // A spent window named under another account does not mark the
        // windows held; it starts that account's snapshot in their place.
        observe_usage_limit(&super::super::events::CodexUsageLimit {
            message: "The usage limit has been reached".to_string(),
            resets_at: Some(earlier),
            window: Some(super::super::events::CodexLimitWindow::FiveHour),
            account: Some("acct-a".to_string()),
            feeds_gauge: true,
        });
        let for_b_after_limit = headers_for(Some("acct-b"));
        let for_a_after_limit = headers_for(Some("acct-a")).expect("account a");
        clear_for_tests();

        assert_eq!(for_b_before, None);
        assert_eq!(for_none, None);
        assert_eq!(
            for_a.get("anthropic-ratelimit-unified-5h-reset"),
            Some(&later.to_string())
        );
        assert_eq!(
            for_b.get("anthropic-ratelimit-unified-5h-reset"),
            Some(&earlier.to_string())
        );
        assert_eq!(
            for_b.get("anthropic-ratelimit-unified-5h-utilization"),
            Some(&"0.7000".to_string())
        );
        assert_eq!(
            for_b.get("anthropic-ratelimit-unified-7d-utilization"),
            Some(&"0.8000".to_string())
        );
        assert_eq!(for_a_after, None);
        assert_eq!(for_b_after_limit, None);
        assert_eq!(
            for_a_after_limit.get("anthropic-ratelimit-unified-5h-utilization"),
            Some(&"1.0000".to_string())
        );
        assert_eq!(
            for_a_after_limit.get("anthropic-ratelimit-unified-7d-utilization"),
            None
        );
    }

    #[test]
    fn only_the_default_metered_limit_feeds_the_gauge() {
        let _snapshot = lock_for_tests();
        clear_for_tests();
        let now = now();
        let reset = now + 10_000;
        let weekly = now + 500_000;
        observe_event(None, &event_resetting(3.0, reset, 16.0, weekly));
        let before = latest(None);
        assert!(before.is_some());

        let mut held_after = Vec::new();
        for (key, name) in [
            ("metered_limit_name", "codex_other"),
            ("limit_name", "codex_other"),
        ] {
            let mut foreign = event_resetting(100.0, reset, 16.0, weekly);
            foreign[key] = name.into();
            observe_event(None, &foreign);
            held_after.push(latest(None));
            // A refusal for another limit is still terminal, but its spent
            // window is not this gauge's.
            let foreign = refusing(foreign);
            let limit = super::super::events::usage_limit_from_event_with_headers(&foreign, &[])
                .expect("the refusal names its window");
            assert!(!limit.feeds_gauge);
            observe_refusing_snapshot(&foreign);
            held_after.push(latest(None));
        }

        // The default limit under its own name, spelled as the CLI
        // normalises it, still counts; `metered_limit_name` wins over
        // `limit_name`.
        let mut named = event_resetting(50.0, reset, 60.0, weekly);
        named["metered_limit_name"] = " Codex ".into();
        named["limit_name"] = "codex_other".into();
        observe_event(None, &named);
        let named_headers = latest_headers();
        clear_for_tests();

        for after in held_after {
            assert_eq!(after, before);
        }
        assert_eq!(
            named_headers.get("anthropic-ratelimit-unified-5h-utilization"),
            Some(&"0.5000".to_string())
        );
        assert!(reads_codex_limit(
            &serde_json::json!({"limit_name": "codex"})
        ));
        assert!(reads_codex_limit(
            &serde_json::json!({"metered_limit_name": null})
        ));
        assert!(!reads_codex_limit(
            &serde_json::json!({"metered_limit_name": 7})
        ));
    }
}
