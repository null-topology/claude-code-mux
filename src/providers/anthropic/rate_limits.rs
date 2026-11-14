//! Reading the Anthropic allowance windows off relayed responses.
//!
//! A relayed response may carry, for each allowance window, an
//! `anthropic-ratelimit-unified-<id>-utilization` and an
//! `anthropic-ratelimit-unified-<id>-reset` header. The relay forwards them
//! untouched; this module only reads them and holds the newest reading of each
//! window, so the metrics endpoint can report how much of each window is spent.
//!
//! Windows belong to one organization's allowance, named by the response's
//! `anthropic-organization-id`. A response of another organization replaces
//! the reading instead of merging into it. The organization id is compared,
//! never shown: it is not logged, not rendered and has no `Debug` output.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

/// One allowance window: how much of it is spent and when it resets.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct Window {
    /// The spent share as sent, 1.0 being the whole window; it may pass 1.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "read only by tests until metrics report the windows"
        )
    )]
    pub(crate) used_ratio: f64,
    /// Unix seconds.
    pub(crate) resets_at: u64,
}

/// The windows held, with the organization of the responses that carried
/// them, `None` when those named none. No `Debug`, so the id cannot reach a
/// log or a panic message.
struct Held {
    organization: Option<String>,
    windows: BTreeMap<String, Window>,
}

static LATEST: Mutex<Option<Held>> = Mutex::new(None);

/// At most this many windows are held.
const MAX_HELD_WINDOWS: usize = 8;

/// A window id is 1 to this many bytes of `[a-z0-9_]`.
const MAX_WINDOW_ID_BYTES: usize = 16;

const ORGANIZATION_HEADER: &str = "anthropic-organization-id";
const WINDOW_HEADER_PREFIX: &str = "anthropic-ratelimit-unified-";
const UTILIZATION_SUFFIX: &str = "-utilization";
const RESET_SUFFIX: &str = "-reset";

/// The usable windows `headers` carry. A window is a pair of
/// `anthropic-ratelimit-unified-<id>-utilization` and `-<id>-reset`, each
/// present once, for an id `is_window_id` admits: utilization a finite
/// number of at least 0, reset a decimal integer of Unix seconds after `now`.
/// Anything else drops that window only. The family's other headers, the bare
/// `anthropic-ratelimit-unified-reset` included, name no window.
fn windows_from_headers(headers: &HeaderMap, now: u64) -> BTreeMap<String, Window> {
    let mut windows = BTreeMap::new();
    for name in headers.keys() {
        let Some(rest) = name.as_str().strip_prefix(WINDOW_HEADER_PREFIX) else {
            continue;
        };
        let Some(id) = rest
            .strip_suffix(UTILIZATION_SUFFIX)
            .or_else(|| rest.strip_suffix(RESET_SUFFIX))
            .filter(|id| is_window_id(id))
        else {
            continue;
        };
        if windows.contains_key(id) {
            continue;
        }
        if let Some(window) = window_from_headers(headers, id, now) {
            windows.insert(id.to_owned(), window);
        }
    }
    windows
}

/// Header names arrive lowercase, so only length and punctuation can fail.
fn is_window_id(id: &str) -> bool {
    (1..=MAX_WINDOW_ID_BYTES).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn window_from_headers(headers: &HeaderMap, id: &str, now: u64) -> Option<Window> {
    let utilization = single_text(
        headers,
        &format!("{WINDOW_HEADER_PREFIX}{id}{UTILIZATION_SUFFIX}"),
    )?;
    let reset = single_text(
        headers,
        &format!("{WINDOW_HEADER_PREFIX}{id}{RESET_SUFFIX}"),
    )?;
    let used_ratio = utilization
        .parse::<f64>()
        .ok()
        .filter(|ratio| ratio.is_finite() && *ratio >= 0.0)?;
    if reset.is_empty() || !reset.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let resets_at = reset.parse::<u64>().ok().filter(|reset| *reset > now)?;
    Some(Window {
        used_ratio,
        resets_at,
    })
}

/// The value of header `name` as text, when it is present exactly once and
/// is UTF-8.
fn single_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    std::str::from_utf8(value.as_bytes()).ok()
}

/// The response's organization id: `anthropic-organization-id` when it is
/// present once, non-empty and UTF-8. It is compared, never parsed.
fn organization_from_headers(headers: &HeaderMap) -> Option<String> {
    single_text(headers, ORGANIZATION_HEADER)
        .filter(|organization| !organization.is_empty())
        .map(str::to_owned)
}

/// The windows of `windows` whose reset is after `now`.
fn running(windows: &BTreeMap<String, Window>, now: u64) -> BTreeMap<String, Window> {
    windows
        .iter()
        .filter(|(_, window)| window.resets_at > now)
        .map(|(id, window)| (id.clone(), *window))
        .collect()
}

/// The reading to hold once a response of `organization` carrying `incoming`
/// arrives.
///
/// A response with no usable window changes nothing unless it names an
/// organization other than the held one (or names one with nothing held),
/// which leaves no reading: the held windows belong to another allowance. A
/// response with usable windows merges into a reading of the same
/// organization, known or unknown alike, and replaces any other whole. In a
/// merge each incoming window replaces the held one of its id, whatever
/// either reset says, and a held window the response does not carry stays.
/// Windows whose reset has passed are dropped before the cap, which keeps the
/// incoming windows first and then the held ones, each in byte order of their
/// ids, up to `MAX_HELD_WINDOWS`.
fn merge(
    held: Option<Held>,
    organization: Option<String>,
    incoming: BTreeMap<String, Window>,
    now: u64,
) -> Option<Held> {
    let incoming = running(&incoming, now);
    let same_organization = held
        .as_ref()
        .is_some_and(|held| held.organization == organization);
    if incoming.is_empty() {
        return if organization.is_none() || same_organization {
            held
        } else {
            None
        };
    }
    let kept = held
        .filter(|_| same_organization)
        .map(|held| running(&held.windows, now))
        .unwrap_or_default();
    let mut windows: BTreeMap<String, Window> =
        incoming.into_iter().take(MAX_HELD_WINDOWS).collect();
    for (id, window) in kept {
        if windows.len() >= MAX_HELD_WINDOWS {
            break;
        }
        windows.entry(id).or_insert(window);
    }
    Some(Held {
        organization,
        windows,
    })
}

/// Read the allowance windows and the organization id off a relayed
/// response's headers and fold them into the held reading (`merge`). The
/// headers are only read.
pub(crate) fn observe_headers(headers: &HeaderMap) {
    let now = now();
    let organization = organization_from_headers(headers);
    let windows = windows_from_headers(headers, now);
    if organization.is_none() && windows.is_empty() {
        return;
    }
    let mut latest = LATEST.lock().unwrap_or_else(PoisonError::into_inner);
    *latest = merge(latest.take(), organization, windows, now);
}

/// The held windows whose reset has not passed, whichever organization they
/// were read under. The organization stays here.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "read only by tests until metrics report the windows"
    )
)]
pub(crate) fn held_windows() -> BTreeMap<String, Window> {
    let latest = LATEST.lock().unwrap_or_else(PoisonError::into_inner);
    latest
        .as_ref()
        .map(|held| running(&held.windows, now()))
        .unwrap_or_default()
}

/// Forget the reading, for a test that leaves one behind.
pub(crate) fn clear_for_tests() {
    *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// Held by every test that writes or reads the reading, which it clears when
/// taken and again when dropped, so a reading one test leaves never reaches
/// another test's assertions.
#[cfg(test)]
pub(crate) struct ReadingLock {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for ReadingLock {
    fn drop(&mut self) {
        clear_for_tests();
    }
}

#[cfg(test)]
pub(crate) fn lock_for_tests() -> ReadingLock {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    clear_for_tests();
    ReadingLock { _guard: guard }
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
    use axum::http::{HeaderName, HeaderValue};

    const NOW: u64 = 1_794_900_000;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const ORG_A: &str = "org-test-1";
    const ORG_B: &str = "org-test-2";

    type Parts = (Option<String>, BTreeMap<String, Window>);

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn window(used_ratio: f64, resets_at: u64) -> Window {
        Window {
            used_ratio,
            resets_at,
        }
    }

    fn windows(entries: &[(&str, Window)]) -> BTreeMap<String, Window> {
        entries
            .iter()
            .map(|(id, window)| ((*id).to_owned(), *window))
            .collect()
    }

    fn held(organization: Option<&str>, entries: &[(&str, Window)]) -> Option<Held> {
        Some(Held {
            organization: organization.map(str::to_owned),
            windows: windows(entries),
        })
    }

    /// What a test compares of a reading, since `Held` has no `Debug`.
    fn parts(held: Option<Held>) -> Option<Parts> {
        held.map(|held| (held.organization, held.windows))
    }

    fn reading(organization: Option<&str>, entries: &[(&str, Window)]) -> Option<Parts> {
        parts(held(organization, entries))
    }

    fn ids(reading: Option<Parts>) -> Vec<String> {
        reading
            .map(|(_, windows)| windows.into_keys().collect())
            .unwrap_or_default()
    }

    fn numbered(prefix: &str, count: usize, resets_at: u64) -> BTreeMap<String, Window> {
        (1..=count)
            .map(|n| (format!("{prefix}{n}"), window(0.1, resets_at)))
            .collect()
    }

    #[test]
    fn unified_headers_with_one_window_yield_only_that_window() {
        let headers = header_map(&[
            ("anthropic-organization-id", ORG_A),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.38"),
            ("anthropic-ratelimit-unified-7d-reset", "1794960000"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "seven_day",
            ),
            ("anthropic-ratelimit-unified-reset", "1794960000"),
            ("anthropic-ratelimit-unified-fallback-percentage", "0.5"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "disabled",
            ),
        ]);

        assert_eq!(
            windows_from_headers(&headers, NOW),
            windows(&[("7d", window(0.38, 1_794_960_000))])
        );
    }

    #[test]
    fn any_admitted_window_id_parses_and_utilization_may_pass_one() {
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.12"),
            ("anthropic-ratelimit-unified-5h-reset", "1794903600"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.25"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "1794960000"),
            ("anthropic-ratelimit-unified-overage-utilization", "0"),
            ("anthropic-ratelimit-unified-overage-reset", "1795000000"),
            // Sixteen bytes, the longest id admitted.
            (
                "anthropic-ratelimit-unified-abcdefghij_12345-utilization",
                "0.5",
            ),
            (
                "anthropic-ratelimit-unified-abcdefghij_12345-reset",
                "1794960000",
            ),
        ]);

        assert_eq!(
            windows_from_headers(&headers, NOW),
            windows(&[
                ("5h", window(0.12, 1_794_903_600)),
                ("7d_oi", window(1.25, 1_794_960_000)),
                ("overage", window(0.0, 1_795_000_000)),
                ("abcdefghij_12345", window(0.5, 1_794_960_000)),
            ])
        );
    }

    #[test]
    fn unusable_windows_are_dropped_one_by_one() {
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-7d-utilization", "0.38"),
            ("anthropic-ratelimit-unified-7d-reset", "1794960000"),
            // Ids with punctuation other than `_`, or longer than sixteen bytes.
            ("anthropic-ratelimit-unified-7d-oi-utilization", "0.1"),
            ("anthropic-ratelimit-unified-7d-oi-reset", "1794960000"),
            ("anthropic-ratelimit-unified-7d.oi-utilization", "0.1"),
            ("anthropic-ratelimit-unified-7d.oi-reset", "1794960000"),
            (
                "anthropic-ratelimit-unified-abcdefghij_123456-utilization",
                "0.1",
            ),
            (
                "anthropic-ratelimit-unified-abcdefghij_123456-reset",
                "1794960000",
            ),
            // A header without its partner.
            ("anthropic-ratelimit-unified-lonely-utilization", "0.1"),
            ("anthropic-ratelimit-unified-orphan-reset", "1794960000"),
            // Utilization that is negative, not finite or not a number.
            ("anthropic-ratelimit-unified-negative-utilization", "-0.1"),
            ("anthropic-ratelimit-unified-negative-reset", "1794960000"),
            ("anthropic-ratelimit-unified-nan-utilization", "NaN"),
            ("anthropic-ratelimit-unified-nan-reset", "1794960000"),
            ("anthropic-ratelimit-unified-infinite-utilization", "inf"),
            ("anthropic-ratelimit-unified-infinite-reset", "1794960000"),
            ("anthropic-ratelimit-unified-word-utilization", "high"),
            ("anthropic-ratelimit-unified-word-reset", "1794960000"),
            // A reset that is not a decimal integer.
            ("anthropic-ratelimit-unified-fraction-utilization", "0.1"),
            ("anthropic-ratelimit-unified-fraction-reset", "1794960000.5"),
            ("anthropic-ratelimit-unified-signed-utilization", "0.1"),
            ("anthropic-ratelimit-unified-signed-reset", "+1794960000"),
            ("anthropic-ratelimit-unified-soon-utilization", "0.1"),
            ("anthropic-ratelimit-unified-soon-reset", "soon"),
            ("anthropic-ratelimit-unified-blank-utilization", "0.1"),
            ("anthropic-ratelimit-unified-blank-reset", ""),
            // A reset that is not after now.
            ("anthropic-ratelimit-unified-due-utilization", "0.1"),
            ("anthropic-ratelimit-unified-due-reset", "1794900000"),
            ("anthropic-ratelimit-unified-past-utilization", "0.1"),
            ("anthropic-ratelimit-unified-past-reset", "1794899999"),
            // A header present twice.
            ("anthropic-ratelimit-unified-twice-utilization", "0.1"),
            ("anthropic-ratelimit-unified-twice-utilization", "0.1"),
            ("anthropic-ratelimit-unified-twice-reset", "1794960000"),
            ("anthropic-ratelimit-unified-again-utilization", "0.1"),
            ("anthropic-ratelimit-unified-again-reset", "1794960000"),
            ("anthropic-ratelimit-unified-again-reset", "1794960000"),
        ]);

        assert_eq!(
            windows_from_headers(&headers, NOW),
            windows(&[("7d", window(0.38, 1_794_960_000))])
        );
    }

    #[test]
    fn only_the_first_eight_incoming_ids_are_kept() {
        let merged = merge(
            None,
            Some(ORG_A.to_owned()),
            numbered("w", 9, NOW + DAY),
            NOW,
        );

        assert_eq!(
            ids(parts(merged)),
            ["w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8"]
        );
    }

    #[test]
    fn a_new_incoming_id_displaces_the_last_held_id_at_the_cap() {
        let held = Some(Held {
            organization: Some(ORG_A.to_owned()),
            windows: numbered("w", 8, NOW + DAY),
        });
        // The new id sorts after every held one and is still kept.
        let incoming = windows(&[("x", window(0.2, NOW + DAY))]);

        let merged = merge(held, Some(ORG_A.to_owned()), incoming, NOW);

        assert_eq!(
            ids(parts(merged)),
            ["w1", "w2", "w3", "w4", "w5", "w6", "w7", "x"]
        );
    }

    #[test]
    fn expired_windows_are_pruned_before_the_cap() {
        let mut held_windows = numbered("w", 8, NOW + DAY);
        held_windows.insert("w1".to_owned(), window(0.1, NOW));
        let held = Some(Held {
            organization: Some(ORG_A.to_owned()),
            windows: held_windows,
        });
        let incoming = windows(&[("x", window(0.2, NOW + DAY))]);

        let merged = merge(held, Some(ORG_A.to_owned()), incoming, NOW);

        assert_eq!(
            ids(parts(merged)),
            ["w2", "w3", "w4", "w5", "w6", "w7", "w8", "x"]
        );
    }

    #[test]
    fn the_same_organization_merges_window_by_window() {
        let held = held(
            Some(ORG_A),
            &[
                ("5h", window(0.1, NOW + HOUR)),
                ("7d", window(0.2, NOW + DAY)),
            ],
        );
        let incoming = windows(&[("7d", window(0.3, NOW + 2 * DAY))]);

        assert_eq!(
            parts(merge(held, Some(ORG_A.to_owned()), incoming, NOW)),
            reading(
                Some(ORG_A),
                &[
                    ("5h", window(0.1, NOW + HOUR)),
                    ("7d", window(0.3, NOW + 2 * DAY)),
                ]
            )
        );
    }

    #[test]
    fn the_latest_window_replaces_the_held_one_whatever_its_reset() {
        let held = held(Some(ORG_A), &[("7d", window(0.9, NOW + 2 * DAY))]);
        let incoming = windows(&[("7d", window(0.1, NOW + HOUR))]);

        assert_eq!(
            parts(merge(held, Some(ORG_A.to_owned()), incoming, NOW)),
            reading(Some(ORG_A), &[("7d", window(0.1, NOW + HOUR))])
        );
    }

    #[test]
    fn another_known_organization_replaces_the_reading_whole() {
        let held_windows = [
            ("5h", window(0.1, NOW + HOUR)),
            ("7d", window(0.2, NOW + DAY)),
        ];
        let incoming = [("7d", window(0.3, NOW + DAY))];

        for held_organization in [Some(ORG_A), None] {
            assert_eq!(
                parts(merge(
                    held(held_organization, &held_windows),
                    Some(ORG_B.to_owned()),
                    windows(&incoming),
                    NOW,
                )),
                reading(Some(ORG_B), &incoming),
                "held under {held_organization:?}"
            );
            assert_eq!(
                parts(merge(
                    held(held_organization, &held_windows),
                    Some(ORG_B.to_owned()),
                    BTreeMap::new(),
                    NOW,
                )),
                None,
                "held under {held_organization:?}, nothing usable incoming"
            );
        }
    }

    #[test]
    fn windows_without_an_organization_merge_only_into_an_unknown_reading() {
        let five_hour = [("5h", window(0.1, NOW + HOUR))];
        let seven_day = [("7d", window(0.2, NOW + DAY))];

        // A known reading is replaced whole.
        assert_eq!(
            parts(merge(
                held(Some(ORG_A), &five_hour),
                None,
                windows(&seven_day),
                NOW
            )),
            reading(None, &seven_day)
        );
        // An unknown one takes the window beside the ones it holds.
        assert_eq!(
            parts(merge(
                held(None, &five_hour),
                None,
                windows(&seven_day),
                NOW
            )),
            reading(None, &[five_hour[0], seven_day[0]])
        );
        // With nothing held, windows start a reading, unknown or under their id.
        assert_eq!(
            parts(merge(None, None, windows(&seven_day), NOW)),
            reading(None, &seven_day)
        );
        assert_eq!(
            parts(merge(
                None,
                Some(ORG_A.to_owned()),
                windows(&seven_day),
                NOW
            )),
            reading(Some(ORG_A), &seven_day)
        );
    }

    #[test]
    fn an_empty_non_utf8_or_repeated_organization_header_counts_as_none() {
        let mut not_utf8 = HeaderMap::new();
        not_utf8.insert(
            "anthropic-organization-id",
            HeaderValue::from_bytes(b"org-\xff").unwrap(),
        );
        let mut utf8 = HeaderMap::new();
        utf8.insert(
            "anthropic-organization-id",
            HeaderValue::from_bytes(b"org-t\xc3\xa9st").unwrap(),
        );

        assert_eq!(organization_from_headers(&HeaderMap::new()), None);
        assert_eq!(
            organization_from_headers(&header_map(&[("anthropic-organization-id", "")])),
            None
        );
        assert_eq!(organization_from_headers(&not_utf8), None);
        assert_eq!(
            organization_from_headers(&header_map(&[
                ("anthropic-organization-id", ORG_A),
                ("anthropic-organization-id", ORG_A),
            ])),
            None
        );
        assert_eq!(
            organization_from_headers(&header_map(&[("anthropic-organization-id", ORG_A)]))
                .as_deref(),
            Some(ORG_A)
        );
        // Any UTF-8 is an id, not only visible ASCII.
        assert_eq!(
            organization_from_headers(&utf8).as_deref(),
            Some("org-t\u{e9}st")
        );
    }

    #[test]
    fn a_response_with_nothing_to_change_leaves_the_reading() {
        let five_hour = [("5h", window(0.1, NOW + HOUR))];
        // A window whose reset has passed is no usable window.
        let spent = windows(&[("7d", window(0.2, NOW))]);

        for held_organization in [Some(ORG_A), None] {
            assert_eq!(
                parts(merge(
                    held(held_organization, &five_hour),
                    None,
                    BTreeMap::new(),
                    NOW
                )),
                reading(held_organization, &five_hour),
                "held under {held_organization:?}"
            );
            assert_eq!(
                parts(merge(
                    held(held_organization, &five_hour),
                    None,
                    spent.clone(),
                    NOW
                )),
                reading(held_organization, &five_hour),
                "held under {held_organization:?}, a passed window incoming"
            );
        }
        assert_eq!(
            parts(merge(
                held(Some(ORG_A), &five_hour),
                Some(ORG_A.to_owned()),
                BTreeMap::new(),
                NOW
            )),
            reading(Some(ORG_A), &five_hour)
        );
        // Nothing held: an organization alone starts no reading.
        assert_eq!(
            parts(merge(None, Some(ORG_A.to_owned()), BTreeMap::new(), NOW)),
            None
        );
        assert_eq!(parts(merge(None, None, BTreeMap::new(), NOW)), None);
    }

    #[test]
    fn running_leaves_out_a_window_whose_reset_passed() {
        let all = windows(&[
            ("a", window(0.1, NOW - 1)),
            ("b", window(0.2, NOW)),
            ("c", window(0.3, NOW + 1)),
        ]);

        assert_eq!(running(&all, NOW), windows(&[("c", window(0.3, NOW + 1))]));
    }

    /// Response headers naming `organization` and carrying the window
    /// `(id, utilization, resets_at)`.
    fn response_headers(
        organization: Option<&str>,
        window: Option<(&str, &str, u64)>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(organization) = organization {
            headers.insert(
                "anthropic-organization-id",
                HeaderValue::from_str(organization).unwrap(),
            );
        }
        if let Some((id, utilization, resets_at)) = window {
            headers.insert(
                HeaderName::from_bytes(
                    format!("anthropic-ratelimit-unified-{id}-utilization").as_bytes(),
                )
                .unwrap(),
                HeaderValue::from_str(utilization).unwrap(),
            );
            headers.insert(
                HeaderName::from_bytes(
                    format!("anthropic-ratelimit-unified-{id}-reset").as_bytes(),
                )
                .unwrap(),
                HeaderValue::from(resets_at),
            );
        }
        headers
    }

    #[test]
    fn observed_headers_are_held_until_their_reset() {
        let _reading = lock_for_tests();
        let reset = now() + DAY;

        observe_headers(&response_headers(Some(ORG_A), Some(("7d", "0.38", reset))));
        assert_eq!(held_windows(), windows(&[("7d", window(0.38, reset))]));

        // A window held past its reset is no longer read.
        *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = held(
            Some(ORG_A),
            &[("5h", window(0.5, now() - 1)), ("7d", window(0.38, reset))],
        );
        assert_eq!(held_windows(), windows(&[("7d", window(0.38, reset))]));
    }

    #[test]
    fn observing_another_organization_drops_the_held_reading() {
        let _reading = lock_for_tests();
        let reset = now() + DAY;
        let seven_day = windows(&[("7d", window(0.38, reset))]);

        observe_headers(&response_headers(Some(ORG_A), Some(("7d", "0.38", reset))));
        assert_eq!(held_windows(), seven_day);
        // Another organization with no usable window leaves nothing held.
        observe_headers(&response_headers(Some(ORG_B), None));
        assert_eq!(held_windows(), BTreeMap::new());

        // A window without an organization replaces a known reading.
        observe_headers(&response_headers(Some(ORG_A), Some(("7d", "0.38", reset))));
        observe_headers(&response_headers(None, Some(("5h", "0.12", reset))));
        assert_eq!(held_windows(), windows(&[("5h", window(0.12, reset))]));

        // A window under an organization replaces an unknown reading.
        observe_headers(&response_headers(Some(ORG_A), Some(("7d", "0.38", reset))));
        assert_eq!(held_windows(), seven_day);

        // A response with neither changes nothing.
        observe_headers(&response_headers(None, None));
        assert_eq!(held_windows(), seven_day);
    }
}
