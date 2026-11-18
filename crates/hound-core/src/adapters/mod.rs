//! Harness adapters: find a harness's session files and turn them into cleaned messages.

pub mod claude;
pub mod pi;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::DateTime;
use serde_json::Value;

use crate::model::{Harness, Message, Parsed, Role, SessionFiles, SourceFile};

/// Lines longer than this are skipped unparsed.
pub(crate) const MAX_LINE: usize = 1024 * 1024;
/// Tool arguments are truncated to this many characters.
const MAX_TOOL_ARG_CHARS: usize = 200;
/// Tool input fields tried, in order, for the argument shown after the tool name.
const TOOL_ARG_FIELDS: [&str; 4] = ["command", "file_path", "path", "pattern"];

/// An adapter's root directory could not be read, so nothing is known about
/// its sessions (as opposed to a readable root holding none).
#[derive(Clone, Debug, PartialEq)]
pub struct RootUnavailable {
    pub harness: Harness,
    pub path: PathBuf,
    pub reason: String,
}

impl fmt::Display for RootUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot read {} sessions in {}: {}",
            self.harness.as_str(),
            self.path.display(),
            self.reason
        )
    }
}

/// Reads one harness's on-disk sessions.
pub trait Adapter {
    fn harness(&self) -> Harness;

    /// All sessions under the adapter's root, or why the root cannot be read.
    fn try_discover(&self) -> Result<Vec<SessionFiles>, RootUnavailable>;

    /// All sessions under the adapter's root. A missing root yields none.
    fn discover(&self) -> Vec<SessionFiles> {
        self.try_discover().unwrap_or_default()
    }

    /// Parses the complete lines of `file` starting at byte `from_offset`.
    fn parse(&self, file: &SourceFile, from_offset: u64) -> io::Result<Parsed>;

    /// Arguments after the harness executable that resume `session_id`.
    fn resume_args(&self, session_id: &str) -> Vec<String>;
}

/// Calls `f` with each complete line (without its `'\n'`) of `path` starting at
/// byte `offset`. Lines longer than `max_line` bytes are skipped without being
/// buffered. A final line with no trailing newline is not consumed. Returns the
/// offset just past the last complete line.
pub(crate) fn read_lines_from(
    path: &Path,
    offset: u64,
    max_line: usize,
    mut f: impl FnMut(&[u8]),
) -> io::Result<u64> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let mut consumed = offset;
    let mut line: Vec<u8> = Vec::new();
    let mut line_len: usize = 0;
    let mut oversize = false;

    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(consumed);
        }
        let (chunk, newline) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        line_len += chunk.len();
        if line_len > max_line {
            oversize = true;
            line.clear();
        } else {
            line.extend_from_slice(chunk);
        }
        let step = chunk.len() + usize::from(newline);
        reader.consume(step);
        if newline {
            if !oversize {
                f(&line);
            }
            consumed += (line_len + 1) as u64;
            line.clear();
            line_len = 0;
            oversize = false;
        }
    }
}

/// The first line of `path` parsed as JSON, if it is complete, at most
/// `MAX_LINE` bytes and valid.
pub(crate) fn read_first_record(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(MAX_LINE as u64 + 1));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    if line.pop() != Some(b'\n') {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

/// Longest accepted session ID, in characters.
pub const MAX_SESSION_ID_LEN: usize = 128;

/// Whether `id` is safe to pass to a harness as an argument: only
/// `[A-Za-z0-9_-]`, one to [`MAX_SESSION_ID_LEN`] characters, and not starting
/// with `-`, so it cannot be taken for a flag.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LEN
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Entries of an adapter's root `dir`; an error if it cannot be read.
pub(crate) fn read_root(harness: Harness, dir: &Path) -> Result<Vec<PathBuf>, RootUnavailable> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(entries.filter_map(|e| e.ok().map(|e| e.path())).collect()),
        Err(e) => Err(RootUnavailable {
            harness,
            path: dir.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

/// Entries of `dir`, or none if it cannot be read.
pub(crate) fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    }
}

/// The record's RFC 3339 `"timestamp"` as Unix seconds.
pub(crate) fn record_ts(record: &Value) -> Option<i64> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp())
}

/// Widens `parsed`'s time span to include `ts`.
pub(crate) fn note_ts(parsed: &mut Parsed, ts: Option<i64>) {
    if let Some(t) = ts {
        parsed.first_ts = Some(parsed.first_ts.map_or(t, |f| f.min(t)));
        parsed.last_ts = Some(parsed.last_ts.map_or(t, |l| l.max(t)));
    }
}

/// Records `cwd` unless already seen.
pub(crate) fn note_cwd(parsed: &mut Parsed, cwd: &str) {
    if !parsed.cwds.iter().any(|c| c == cwd) {
        parsed.cwds.push(cwd.to_string());
    }
}

/// Appends a message; the first main-transcript user message becomes `first_prompt`.
pub(crate) fn push_message(
    parsed: &mut Parsed,
    role: Role,
    text: String,
    subagent: bool,
    ts: Option<i64>,
) {
    if role == Role::User && !subagent && parsed.first_prompt.is_none() {
        parsed.first_prompt = Some(text.clone());
    }
    parsed.messages.push(Message {
        role,
        text,
        subagent,
        ts,
    });
}

/// A content block's `"type"`.
pub(crate) fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// `"<name> <arg>"`, where arg is the first string field of `TOOL_ARG_FIELDS`
/// present in `input`, truncated; the name alone if none is present.
pub(crate) fn tool_text(name: &str, input: Option<&Value>) -> String {
    let arg = TOOL_ARG_FIELDS
        .iter()
        .find_map(|k| input.and_then(|i| i.get(k)).and_then(Value::as_str));
    match arg {
        Some(arg) => {
            let arg: String = arg.chars().take(MAX_TOOL_ARG_CHARS).collect();
            format!("{name} {arg}")
        }
        None => name.to_string(),
    }
}
