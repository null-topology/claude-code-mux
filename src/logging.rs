use crate::{config, paths};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// How many rotated files of a log are kept; a rotation deletes older ones.
pub const MAX_ROTATED_LOGS: usize = 5;

static STDERR_SUPPRESSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub const REDACT_KEYS: [&str; 15] = [
    "authorization",
    "proxy-authorization",
    "access",
    "access_token",
    "refresh",
    "refresh_token",
    "id_token",
    "code",
    "code_verifier",
    "chatgpt-account-id",
    "cookie",
    "set-cookie",
    "x-api-key",
    "apikey",
    "api_key",
];

pub fn log_file() -> std::path::PathBuf {
    paths::log_file()
}

#[must_use]
pub struct StderrSuppressionGuard;

impl Drop for StderrSuppressionGuard {
    fn drop(&mut self) {
        STDERR_SUPPRESSION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn suppress_stderr() -> StderrSuppressionGuard {
    STDERR_SUPPRESSION_DEPTH.fetch_add(1, Ordering::Relaxed);
    StderrSuppressionGuard
}

fn stderr_suppressed() -> bool {
    STDERR_SUPPRESSION_DEPTH.load(Ordering::Relaxed) > 0
}

fn should_mirror_to_stderr(level: &str) -> bool {
    !stderr_suppressed() && (matches!(level, "warn" | "error") || config::log_stderr())
}

#[derive(Clone)]
pub struct Logger {
    service: String,
    base: serde_json::Map<String, Value>,
}

impl Logger {
    pub fn child(&self, bindings: serde_json::Map<String, Value>) -> Logger {
        let mut merged = self.base.clone();
        merged.extend(bindings);
        Logger {
            service: self.service.clone(),
            base: merged,
        }
    }

    pub fn debug(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("debug", msg, fields)
    }

    pub fn info(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("info", msg, fields)
    }

    pub fn warn(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("warn", msg, fields)
    }

    pub fn error(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("error", msg, fields)
    }

    fn emit(&self, level: &str, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        let mut body = serde_json::Map::new();
        body.insert("t".into(), Value::String(now_iso8601()));
        body.insert("level".into(), Value::String(level.to_string()));
        body.insert("service".into(), Value::String(self.service.clone()));
        body.insert("msg".into(), Value::String(msg.to_string()));

        let mut merged = self.base.clone();
        if let Some(fields) = fields {
            merged.extend(fields);
        }
        if !merged.is_empty() {
            body.insert("fields".into(), redact_value(Value::Object(merged)));
        }

        let line = Value::Object(body).to_string();

        let mirror_to_stderr = should_mirror_to_stderr(level);
        if mirror_to_stderr {
            let _ = writeln!(io::stderr(), "{line}");
        }

        if write_log_line(&line).is_err() && mirror_to_stderr {
            // swallow logging errors intentionally
        }
        // Counted whether or not the write succeeded, by message only: the
        // fields never become a label.
        crate::metrics::log_event(&self.service, level, msg);
    }
}

pub fn create_logger(service: &str) -> Logger {
    Logger {
        service: service.to_string(),
        base: serde_json::Map::new(),
    }
}

fn write_log_line(line: &str) -> io::Result<()> {
    let file = log_file();
    if let Some(dir) = file.parent() {
        create_dir(dir, 0o700)?;
    }

    if fs::metadata(&file).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        rotate_file(&file)?;
    }

    let mut out = OpenOptions::new().create(true).append(true).open(&file)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

fn rotate_file(path: &Path) -> io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rotated = path.with_extension(format!("{ts}"));
    fs::rename(path, &rotated)?;
    prune_rotated_logs(path, &rotated, MAX_ROTATED_LOGS);
    Ok(())
}

/// Keep `keep` rotated files of the log at `path`: `just_rotated`, the file
/// [`rotate_file`] has just created, and the `keep - 1` newest of the others,
/// deleting the rest. Rotated files are named `<stem>.<millis>` beside the
/// log, so only a file whose name is the log's stem, a dot and digits counts,
/// ordered by that number. `just_rotated` is never a candidate, so a clock
/// that went back (or a zero timestamp) cannot make the newest log the one
/// deleted. A failed listing or delete is ignored: pruning never fails
/// logging.
fn prune_rotated_logs(path: &Path, just_rotated: &Path, keep: usize) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{stem}.");
    let mut rotated: Vec<(u128, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| entry.path() != just_rotated)
        .filter_map(|entry| {
            let name = entry.file_name();
            let stamp = name.to_str()?.strip_prefix(&prefix)?;
            if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some((stamp.parse().ok()?, entry.path()))
        })
        .collect();
    rotated.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, old) in rotated.into_iter().skip(keep.saturating_sub(1)) {
        let _ = fs::remove_file(old);
    }
}

fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
}

fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse_borrowed::<3>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
    )
    .unwrap();
    now.format(&format).unwrap_or_else(|_| String::new())
}

pub fn redact_value(value: Value) -> Value {
    redact_with_depth(value, 0)
}

/// `s` cut to at most 4000 bytes, followed by `…[N more]` with N the bytes
/// left out; a shorter string is kept whole. The cut falls on a character
/// boundary, since slicing inside a multibyte character panics.
fn shorten_for_log(s: String) -> String {
    if s.len() <= 4000 {
        return s;
    }
    let kept = s.floor_char_boundary(4000);
    format!("{}…[{} more]", &s[..kept], s.len() - kept)
}

fn redact_with_depth(value: Value, depth: u8) -> Value {
    if depth > 6 {
        return Value::String("[depth-limit]".into());
    }

    match value {
        Value::String(s) => {
            if config::log_verbose() {
                Value::String(s)
            } else {
                Value::String(shorten_for_log(s))
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| redact_with_depth(v, depth + 1))
                .collect(),
        ),
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_key_redaction(value));
                } else {
                    out.insert(key, redact_with_depth(value, depth + 1));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_key_redaction(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

pub fn redacted_keys() -> HashSet<&'static str> {
    REDACT_KEYS.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static STDERR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn stderr_suppression_disables_level_mirroring() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        assert!(should_mirror_to_stderr("warn"));

        {
            let _guard = suppress_stderr();
            assert!(!should_mirror_to_stderr("warn"));
            assert!(!should_mirror_to_stderr("error"));
        }

        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn stderr_suppression_supports_nested_guards() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        let outer = suppress_stderr();
        let inner = suppress_stderr();
        assert!(!should_mirror_to_stderr("warn"));

        drop(inner);
        assert!(!should_mirror_to_stderr("warn"));

        drop(outer);
        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn redacts_proxy_authorization_case_insensitively() {
        let redacted = redact_value(serde_json::json!({
            "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "safe": "kept"
        }));

        assert_eq!(redacted["safe"], "kept");
        assert_eq!(redacted["Proxy-Authorization"], "[redacted len=18]");
    }

    #[test]
    fn long_strings_are_cut_on_a_character_boundary() {
        // `é` takes bytes 3999 and 4000, so a cut at byte 4000 would split it.
        // The helper takes no verbose flag, so a test elsewhere that turns on
        // verbose logging process-wide cannot change this result.
        let text = format!("{}é{}", "a".repeat(3999), "b".repeat(100));
        let shortened = shorten_for_log(text.clone());

        let expected = format!("{}…[{} more]", "a".repeat(3999), text.len() - 3999);
        assert_eq!(shortened, expected);
        assert!(shortened.ends_with("…[102 more]"));

        let whole = "é".repeat(2000);
        assert_eq!(shorten_for_log(whole.clone()), whole);
    }

    #[test]
    fn pruning_keeps_the_just_rotated_log_whatever_its_number() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("proxy.log");
        // A clock at the epoch named the new rotation `proxy.0`, below every
        // earlier one.
        let just_rotated = dir.path().join("proxy.0");
        fs::write(&just_rotated, "newest\n").unwrap();
        let stamps = [10_u64, 20, 30, 40, 50, 60];
        for stamp in stamps {
            fs::write(dir.path().join(format!("proxy.{stamp}")), "old\n").unwrap();
        }

        prune_rotated_logs(&log, &just_rotated, MAX_ROTATED_LOGS);

        assert_eq!(fs::read_to_string(&just_rotated).unwrap(), "newest\n");
        let exists = |stamp: u64| dir.path().join(format!("proxy.{stamp}")).exists();
        for stamp in [30, 40, 50, 60] {
            assert!(exists(stamp), "kept proxy.{stamp}");
        }
        for stamp in [10, 20] {
            assert!(!exists(stamp), "deleted proxy.{stamp}");
        }
    }

    #[test]
    fn rotation_keeps_the_newest_rotated_logs_and_leaves_other_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("proxy.log");
        fs::write(&log, "current\n").unwrap();
        // Eight earlier rotations. As text "900" would sort above "4000000";
        // the stamps are compared as numbers.
        let stamps = [7_u64, 80, 900, 1_000, 1_001, 20_000, 300_000, 4_000_000];
        for stamp in stamps {
            fs::write(dir.path().join(format!("proxy.{stamp}")), "old\n").unwrap();
        }
        let unrelated = [
            "proxy.log.1",
            "proxy.12a",
            "proxy.",
            "proxy-5",
            "proxy.5.gz",
            "other.5",
        ];
        for name in unrelated {
            fs::write(dir.path().join(name), "other\n").unwrap();
        }
        fs::create_dir(dir.path().join("proxy.3")).unwrap();

        rotate_file(&log).unwrap();

        let exists = |name: &str| dir.path().join(name).exists();
        assert!(!exists("proxy.log"));
        for stamp in [1_001, 20_000, 300_000, 4_000_000] {
            assert!(exists(&format!("proxy.{stamp}")), "kept proxy.{stamp}");
        }
        for stamp in [7, 80, 900, 1_000] {
            assert!(!exists(&format!("proxy.{stamp}")), "deleted proxy.{stamp}");
        }
        for name in unrelated {
            assert!(exists(name), "left {name}");
        }
        assert!(exists("proxy.3"));

        let new_rotations: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_str().unwrap();
                name.strip_prefix("proxy.").is_some_and(|stamp| {
                    stamp
                        .parse::<u64>()
                        .is_ok_and(|stamp| !stamps.contains(&stamp))
                }) && path.is_file()
            })
            .collect();
        assert_eq!(new_rotations.len(), 1);
        assert_eq!(fs::read_to_string(&new_rotations[0]).unwrap(), "current\n");
    }

    #[test]
    fn a_logged_line_is_counted_once_and_its_fields_stay_out() {
        use crate::metrics::{Format, ProcessMetrics, RenderInput, render};

        fn rendered(metrics: &ProcessMetrics, format: Format) -> String {
            let input = RenderInput {
                process: Some(metrics),
                ..RenderInput::default()
            };
            render(&input, format)
        }
        let series = "ccmux_log_events_total{service=\"metrics_test\",level=\"info\",event=\"line counted\"}";
        let field_value = "field-value-5c1e8b";
        let scope = crate::metrics::scoped_process_for_tests();
        assert!(!rendered(&scope, Format::OpenMetrics).contains(series));

        create_logger("metrics_test").info(
            "line counted",
            Some(serde_json::Map::from_iter([(
                "detail".to_string(),
                Value::String(field_value.to_string()),
            )])),
        );

        for format in [Format::OpenMetrics, Format::Text] {
            let text = rendered(&scope, format);
            let samples: Vec<&str> = text
                .lines()
                .filter(|line| line.starts_with("ccmux_log_events_total"))
                .collect();
            assert_eq!(samples, [format!("{series} 1")]);
            assert!(!text.contains(field_value));
            assert!(!text.contains("detail"));
        }
    }
}
