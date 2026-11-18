//! pi sessions: `<root>/<slug>/<timestamp>_<uuid>.jsonl`, whose first line is a
//! `{"type":"session","id",..,"cwd",..}` header.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    Adapter, MAX_LINE, RootUnavailable, block_type, note_cwd, note_ts, push_message,
    read_dir_paths, read_first_record, read_lines_from, read_root, record_ts, tool_text,
    valid_session_id,
};
use crate::model::{Harness, Parsed, Role, SessionFiles, SessionKey, SourceFile};

pub struct PiAdapter {
    root: PathBuf,
}

impl PiAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Adapter for PiAdapter {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn try_discover(&self) -> Result<Vec<SessionFiles>, RootUnavailable> {
        let mut slugs = read_root(Harness::Pi, &self.root)?;
        slugs.retain(|p| p.is_dir());
        slugs.sort();

        let mut sessions = Vec::new();
        for slug in slugs {
            let mut paths = read_dir_paths(&slug);
            paths.retain(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl"));
            paths.sort();
            for path in paths {
                let Some(id) = session_id(&path) else {
                    continue;
                };
                sessions.push(SessionFiles {
                    key: SessionKey {
                        harness: Harness::Pi,
                        id,
                    },
                    files: vec![SourceFile {
                        path,
                        subagent: false,
                    }],
                });
            }
        }
        Ok(sessions)
    }

    fn parse(&self, file: &SourceFile, from_offset: u64) -> io::Result<Parsed> {
        let mut parsed = Parsed::default();
        parsed.new_offset = read_lines_from(&file.path, from_offset, MAX_LINE, |line| {
            if let Ok(record) = serde_json::from_slice::<Value>(line) {
                apply_record(&mut parsed, &record, file.subagent);
            }
        })?;
        Ok(parsed)
    }

    fn resume_args(&self, session_id: &str) -> Vec<String> {
        vec!["--session".to_string(), session_id.to_string()]
    }
}

/// The header's `id`, else the `<uuid>` after the first `'_'` of the file stem
/// (the whole stem if it has none); `None` if that id is not
/// [valid](valid_session_id).
fn session_id(path: &Path) -> Option<String> {
    if let Some(header) = read_first_record(path)
        && header.get("type").and_then(Value::as_str) == Some("session")
        && let Some(id) = header.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        return valid_session_id(id).then(|| id.to_string());
    }
    let stem = path.file_stem()?.to_str()?;
    let id = stem.split_once('_').map_or(stem, |(_, uuid)| uuid);
    valid_session_id(id).then(|| id.to_string())
}

fn apply_record(parsed: &mut Parsed, record: &Value, subagent: bool) {
    let ts = record_ts(record);
    note_ts(parsed, ts);

    match record.get("type").and_then(Value::as_str) {
        Some("session") => {
            if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
                note_cwd(parsed, cwd);
            }
        }
        Some("message") => {
            let role = match record.pointer("/message/role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                // system prompts and tool results are not conversation text
                _ => return,
            };
            let Some(blocks) = record.pointer("/message/content").and_then(Value::as_array) else {
                return;
            };
            for block in blocks {
                match block_type(block) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            push_message(parsed, role, text.to_string(), subagent, ts);
                        }
                    }
                    Some("toolCall") => {
                        if let Some(name) = block.get("name").and_then(Value::as_str) {
                            let text = tool_text(name, block.get("arguments"));
                            push_message(parsed, Role::Tool, text, subagent, ts);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}
