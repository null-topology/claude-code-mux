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
    fs::rename(path, rotated)?;
    prune_rotated_logs(path, MAX_ROTATED_LOGS);
    Ok(())
}

/// Delete all but the `keep` newest rotated files of the log at `path`.
/// [`rotate_file`] names them `<stem>.<millis>` beside the log, so only a file
/// whose name is the log's stem, a dot and digits counts, ordered by that
/// number. A failed listing or delete is ignored: pruning never fails logging.
fn prune_rotated_logs(path: &Path, keep: usize) {
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
        .filter_map(|entry| {
            let name = entry.file_name();
            let stamp = name.to_str()?.strip_prefix(&prefix)?;
            if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some((stamp.parse().ok()?, entry.path()))
        })
        .collect();
    rotated.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in rotated.into_iter().skip(keep) {
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

fn redact_with_depth(value: Value, depth: u8) -> Value {
    if depth > 6 {
        return Value::String("[depth-limit]".into());
    }

    match value {
        Value::String(s) => {
            if config::log_verbose() {
                Value::String(s)
            } else if s.len() > 4000 {
                // Cut on a character boundary: slicing inside a multibyte
                // character panics.
                let kept = s.floor_char_boundary(4000);
                Value::String(format!("{}…[{} more]", &s[..kept], s.len() - kept))
            } else {
                Value::String(s)
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
        let text = format!("{}é{}", "a".repeat(3999), "b".repeat(100));
        let redacted = redact_value(serde_json::json!({ "message": text }));

        let expected = format!("{}…[{} more]", "a".repeat(3999), text.len() - 3999);
        assert_eq!(redacted["message"], Value::String(expected));
        assert!(
            redacted["message"]
                .as_str()
                .unwrap()
                .ends_with("…[102 more]")
        );
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
}
