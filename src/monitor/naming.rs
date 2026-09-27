//! What a session is called. Claude Code keeps two names in the session's
//! transcript on this machine: the one `/rename` set and a title of its own.
//! The title is also visible on the wire, in the reply to the side request that
//! asks a model for it, which is all there is for a session whose transcript
//! lives elsewhere.
//!
//! Transcripts are read here, in the background and incrementally, never on a
//! request's path. Names are kept in memory only and go with their session.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::MonitorHandle;

/// Where a session's name came from, in the order the sources win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionNameSource {
    /// Set with `/rename`, read from the transcript.
    Rename,
    /// Claude Code's own title, read from the transcript.
    Auto,
    /// Claude Code's own title, read off the reply to the request for it.
    Wire,
}

impl SessionNameSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Auto => "auto",
            Self::Wire => "wire",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionName {
    pub text: String,
    pub source: SessionNameSource,
}

/// The longest name kept, in characters. Titles are a few words; anything
/// longer is cut rather than carried whole.
const MAX_SESSION_NAME_CHARS: usize = 120;

/// A name as the monitor shows it: on one line, with no control characters,
/// and not empty.
pub(crate) fn clean_session_name(raw: &str) -> Option<String> {
    let text = raw
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(MAX_SESSION_NAME_CHARS).collect())
}

/// The title in the reply to Claude Code's request for one: the reply's text
/// is a JSON document with a `title` string.
pub(crate) fn session_title_from_reply(text: &str) -> Option<String> {
    let reply = serde_json::from_str::<Value>(text.trim()).ok()?;
    clean_session_name(reply.get("title")?.as_str()?)
}

/// The names one transcript holds: the latest line of each kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptNames {
    pub renamed: Option<String>,
    pub auto_title: Option<String>,
}

/// How often the transcripts of the sessions the monitor holds are read.
const TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long a session whose transcript was not found waits before it is looked
/// for again. A missing transcript is normal: the session may run elsewhere.
const TRANSCRIPT_LOOKUP_RETRY: Duration = Duration::from_secs(60);
/// The longest transcript line read for a name. The lines that carry one are
/// short; longer ones hold messages and tool output, and are passed over
/// without being kept.
const MAX_NAME_LINE_BYTES: usize = 16 * 1024;

/// Reads the names out of session transcripts, from where the previous read
/// stopped.
pub(crate) struct TranscriptReader {
    /// Claude Code's `projects` directory, one subdirectory per working
    /// directory, each holding `<session id>.jsonl` files.
    projects: PathBuf,
    sessions: HashMap<String, TranscriptState>,
}

#[derive(Debug, Default)]
struct TranscriptState {
    path: Option<PathBuf>,
    /// Where the next read starts: just past the last complete line read.
    offset: u64,
    /// When a transcript not found so far is looked for again.
    retry_at: Option<Instant>,
    names: TranscriptNames,
}

impl TranscriptReader {
    pub(crate) fn new(projects: PathBuf) -> Self {
        Self {
            projects,
            sessions: HashMap::new(),
        }
    }

    /// The reader for Claude Code's configuration directory: the
    /// `CLAUDE_CONFIG_DIR` it honours, else `~/.claude`.
    fn from_environment() -> Option<Self> {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".claude"))
            })?;
        Some(Self::new(config.join("projects")))
    }

    /// Read what the transcripts of `session_ids` gained since the previous
    /// call, and forget every session not listed. Returns the sessions whose
    /// names changed, with their names now.
    pub(crate) fn poll(
        &mut self,
        session_ids: &[String],
        now: Instant,
    ) -> Vec<(String, TranscriptNames)> {
        let wanted: HashSet<&str> = session_ids
            .iter()
            .map(String::as_str)
            .filter(|id| is_transcript_file_stem(id))
            .collect();
        self.sessions.retain(|id, _| wanted.contains(id.as_str()));
        for id in &wanted {
            if !self.sessions.contains_key(*id) {
                self.sessions
                    .insert((*id).to_string(), TranscriptState::default());
            }
        }
        self.look_up_transcripts(now);

        let mut changed = Vec::new();
        for (id, state) in &mut self.sessions {
            if state.path.is_none() {
                continue;
            }
            let before = state.names.clone();
            if let Err(error) = read_new_lines(state)
                && error.kind() == io::ErrorKind::NotFound
            {
                // The file went away; what it said stays known.
                state.path = None;
                state.offset = 0;
                state.retry_at = Some(now + TRANSCRIPT_LOOKUP_RETRY);
            }
            if state.names != before {
                changed.push((id.clone(), state.names.clone()));
            }
        }
        changed
    }

    /// Find the transcripts not found yet whose retry is due, with one listing
    /// of the projects directory for all of them.
    fn look_up_transcripts(&mut self, now: Instant) {
        let due: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, state)| {
                state.path.is_none() && state.retry_at.is_none_or(|retry_at| now >= retry_at)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        let mut found = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&self.projects) {
            for entry in entries.flatten() {
                let directory = entry.path();
                for id in &due {
                    if found.contains_key(id) {
                        continue;
                    }
                    let candidate = directory.join(format!("{id}.jsonl"));
                    if candidate.is_file() {
                        found.insert(id.clone(), candidate);
                    }
                }
            }
        }
        for id in due {
            let Some(state) = self.sessions.get_mut(&id) else {
                continue;
            };
            match found.remove(&id) {
                Some(path) => {
                    state.path = Some(path);
                    state.offset = 0;
                    state.retry_at = None;
                }
                None => state.retry_at = Some(now + TRANSCRIPT_LOOKUP_RETRY),
            }
        }
    }

    #[cfg(test)]
    fn offset_for_tests(&self, session_id: &str) -> Option<u64> {
        self.sessions.get(session_id).map(|state| state.offset)
    }

    #[cfg(test)]
    fn held_for_tests(&self) -> usize {
        self.sessions.len()
    }
}

/// Whether a session id can name a transcript file: nothing that could leave
/// the directory it is looked for in.
fn is_transcript_file_stem(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Read the complete lines past the remembered offset and note the names they
/// carry. A line still being written is left for the next read.
fn read_new_lines(state: &mut TranscriptState) -> io::Result<()> {
    let Some(path) = state.path.as_ref() else {
        return Ok(());
    };
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < state.offset {
        // Shorter than what was read: the file was replaced. Start over.
        state.offset = 0;
    }
    if len == state.offset {
        return Ok(());
    }
    file.seek(SeekFrom::Start(state.offset))?;
    let mut reader = BufReader::new(file.take(len - state.offset));
    let mut line = Vec::new();
    loop {
        line.clear();
        let Some((consumed, kept)) = read_capped_line(&mut reader, &mut line)? else {
            return Ok(());
        };
        state.offset += consumed;
        if kept {
            note_line(&line, &mut state.names);
        }
    }
}

/// Read one line up to its newline. `None` when the input ends before one;
/// otherwise the bytes consumed and whether the line fit in `line`. A line
/// over [`MAX_NAME_LINE_BYTES`] is consumed without being kept.
fn read_capped_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> io::Result<Option<(u64, bool)>> {
    let mut consumed = 0u64;
    let mut kept = true;
    loop {
        let (used, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(None);
            }
            let (chunk, complete) = match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&available[..=end], true),
                None => (available, false),
            };
            if kept {
                if line.len() + chunk.len() > MAX_NAME_LINE_BYTES {
                    kept = false;
                    line.clear();
                } else {
                    line.extend_from_slice(chunk);
                }
            }
            (chunk.len(), complete)
        };
        reader.consume(used);
        consumed += used as u64;
        if complete {
            return Ok(Some((consumed, kept)));
        }
    }
}

/// Take a name from a transcript line that carries one. Only lines that
/// mention a name's type are parsed; a malformed one is skipped.
fn note_line(line: &[u8], names: &mut TranscriptNames) {
    let mentions = |needle: &[u8]| line.windows(needle.len()).any(|window| window == needle);
    if !mentions(b"\"custom-title\"") && !mentions(b"\"ai-title\"") {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let (field, slot) = match entry.get("type").and_then(Value::as_str) {
        Some("custom-title") => ("customTitle", &mut names.renamed),
        Some("ai-title") => ("aiTitle", &mut names.auto_title),
        _ => return,
    };
    if let Some(name) = entry
        .get(field)
        .and_then(Value::as_str)
        .and_then(clean_session_name)
    {
        *slot = Some(name);
    }
}

/// Read the transcripts of the sessions the monitor holds every few seconds,
/// on the blocking pool, and hand it the names that changed. Runs for the life
/// of the process.
pub fn spawn_transcript_reader(monitor: MonitorHandle) {
    let Some(mut reader) = TranscriptReader::from_environment() else {
        return;
    };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TRANSCRIPT_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let session_ids = monitor.session_ids();
            let polled = tokio::task::spawn_blocking(move || {
                let changed = reader.poll(&session_ids, Instant::now());
                (reader, changed)
            })
            .await;
            let Ok((returned, changed)) = polled else {
                return;
            };
            reader = returned;
            for (session_id, names) in changed {
                monitor.transcript_names_read(session_id, names);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SESSION: &str = "5f0c2a4e-1b7d-4c3a-9e2f-8a6b4d1c0e9f";

    fn transcript_dir() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("projects").join("-home-u-repo");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{SESSION}.jsonl"));
        (root, path)
    }

    fn reader_for(root: &tempfile::TempDir) -> TranscriptReader {
        TranscriptReader::new(root.path().join("projects"))
    }

    fn append(path: &std::path::Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn rename_line(name: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "custom-title", "customTitle": name, "sessionId": SESSION})
        )
    }

    fn auto_line(name: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "ai-title", "aiTitle": name, "sessionId": SESSION})
        )
    }

    fn sessions() -> Vec<String> {
        vec![SESSION.to_string()]
    }

    #[test]
    fn the_latest_line_of_each_kind_names_the_session() {
        let (root, path) = transcript_dir();
        append(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        );
        append(&path, &auto_line("Fix the build"));
        append(&path, &rename_line("first name"));
        append(&path, &rename_line("second name"));
        let mut reader = reader_for(&root);

        let changed = reader.poll(&sessions(), Instant::now());

        assert_eq!(
            changed,
            vec![(
                SESSION.to_string(),
                TranscriptNames {
                    renamed: Some("second name".to_string()),
                    auto_title: Some("Fix the build".to_string()),
                }
            )]
        );
        // Nothing new, nothing reported.
        assert!(reader.poll(&sessions(), Instant::now()).is_empty());
    }

    #[test]
    fn appended_lines_are_read_from_where_the_previous_read_stopped() {
        let (root, path) = transcript_dir();
        append(&path, &rename_line("one"));
        let mut reader = reader_for(&root);
        reader.poll(&sessions(), Instant::now());
        let first_length = std::fs::metadata(&path).unwrap().len();
        assert_eq!(reader.offset_for_tests(SESSION), Some(first_length));

        // Rewrite the line already read, same length, and append another. A
        // read from the start would take the rewritten name.
        let rewritten = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"one\"", "\"two\"");
        std::fs::write(&path, rewritten).unwrap();
        append(&path, &auto_line("Later title"));
        let changed = reader.poll(&sessions(), Instant::now());

        assert_eq!(
            changed[0].1,
            TranscriptNames {
                renamed: Some("one".to_string()),
                auto_title: Some("Later title".to_string()),
            }
        );
    }

    #[test]
    fn a_line_still_being_written_waits_for_its_end() {
        let (root, path) = transcript_dir();
        let line = rename_line("half written");
        let (head, tail) = line.split_at(20);
        append(&path, head);
        let mut reader = reader_for(&root);

        assert!(reader.poll(&sessions(), Instant::now()).is_empty());
        assert_eq!(reader.offset_for_tests(SESSION), Some(0));

        append(&path, tail);
        let changed = reader.poll(&sessions(), Instant::now());
        assert_eq!(changed[0].1.renamed.as_deref(), Some("half written"));
    }

    #[test]
    fn a_missing_transcript_is_looked_for_again_later() {
        let (root, path) = transcript_dir();
        let mut reader = reader_for(&root);
        let start = Instant::now();

        assert!(reader.poll(&sessions(), start).is_empty());

        append(&path, &rename_line("found later"));
        // Not before the retry is due.
        assert!(reader.poll(&sessions(), start).is_empty());
        let changed = reader.poll(&sessions(), start + TRANSCRIPT_LOOKUP_RETRY);
        assert_eq!(changed[0].1.renamed.as_deref(), Some("found later"));
    }

    #[test]
    fn malformed_and_oversized_lines_are_skipped() {
        let (root, path) = transcript_dir();
        append(&path, "{\"type\":\"custom-title\",\"customTitle\":\n");
        append(&path, "not json at all \"ai-title\"\n");
        append(
            &path,
            &format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"{}\"}}\n",
                "x".repeat(MAX_NAME_LINE_BYTES)
            ),
        );
        append(&path, &rename_line("  kept\tname \n"));
        let mut reader = reader_for(&root);

        let changed = reader.poll(&sessions(), Instant::now());

        assert_eq!(changed[0].1.renamed.as_deref(), Some("kept name"));
        assert_eq!(changed[0].1.auto_title, None);
        assert_eq!(
            reader.offset_for_tests(SESSION),
            Some(std::fs::metadata(&path).unwrap().len())
        );
    }

    #[test]
    fn sessions_no_longer_held_are_forgotten_and_unsafe_ids_never_looked_up() {
        let (root, path) = transcript_dir();
        append(&path, &rename_line("named"));
        let mut reader = reader_for(&root);
        reader.poll(&sessions(), Instant::now());
        assert_eq!(reader.held_for_tests(), 1);

        reader.poll(&["../escape".to_string(), String::new()], Instant::now());
        assert_eq!(reader.held_for_tests(), 0);
    }

    #[test]
    fn a_title_reply_names_the_session_and_anything_else_does_not() {
        assert_eq!(
            session_title_from_reply(" {\"title\": \"Wire title\"} "),
            Some("Wire title".to_string())
        );
        assert_eq!(session_title_from_reply("{\"title\": \"  \"}"), None);
        assert_eq!(session_title_from_reply("{\"verdict\": \"ok\"}"), None);
        assert_eq!(session_title_from_reply("Wire title"), None);
    }
}
