//! Claude Code sessions: `<root>/<slug>/<session-uuid>.jsonl`, with subagent
//! transcripts in `<root>/<slug>/<session-uuid>/subagents/agent-*.jsonl`.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    Adapter, MAX_LINE, RootUnavailable, block_type, note_cwd, note_ts, push_message,
    read_dir_paths, read_lines_from, read_root, record_ts, tool_text, valid_session_id,
};
use crate::model::{Harness, Parsed, Role, SessionFiles, SessionKey, SourceFile};

pub struct ClaudeAdapter {
    root: PathBuf,
    include_subagents: bool,
}

impl ClaudeAdapter {
    pub fn new(root: PathBuf, include_subagents: bool) -> Self {
        Self {
            root,
            include_subagents,
        }
    }

    fn subagent_files(&self, session_dir: &Path) -> Vec<SourceFile> {
        let mut paths: Vec<PathBuf> = read_dir_paths(&session_dir.join("subagents"))
            .into_iter()
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl"))
            })
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| SourceFile {
                path,
                subagent: true,
            })
            .collect()
    }
}

impl Adapter for ClaudeAdapter {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn try_discover(&self) -> Result<Vec<SessionFiles>, RootUnavailable> {
        let mut slugs = read_root(Harness::Claude, &self.root)?;
        slugs.retain(|p| p.is_dir());
        slugs.sort();

        let mut sessions = Vec::new();
        for slug in slugs {
            let mut mains = read_dir_paths(&slug);
            mains.retain(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl"));
            mains.sort();
            for path in mains {
                let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|id| valid_session_id(id))
                    .map(str::to_owned)
                else {
                    continue;
                };
                let mut files = vec![SourceFile {
                    path: path.clone(),
                    subagent: false,
                }];
                if self.include_subagents {
                    files.extend(self.subagent_files(&slug.join(&id)));
                }
                sessions.push(SessionFiles {
                    key: SessionKey {
                        harness: Harness::Claude,
                        id,
                    },
                    files,
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
        vec!["--resume".to_string(), session_id.to_string()]
    }
}

fn apply_record(parsed: &mut Parsed, record: &Value, subagent: bool) {
    // Sidechain records in a main transcript duplicate subagent work.
    if !subagent && record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }

    if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
        note_cwd(parsed, cwd);
    }

    let ts = record_ts(record);
    note_ts(parsed, ts);

    let mut push = |role: Role, text: String| push_message(parsed, role, text, subagent, ts);

    match record.get("type").and_then(Value::as_str) {
        Some("user") => {
            let flagged = |field| record.get(field).and_then(Value::as_bool) == Some(true);
            if flagged("isMeta") || flagged("isCompactSummary") {
                return;
            }
            match record.pointer("/message/content") {
                Some(Value::String(text)) if !is_harness_wrapper(text) => {
                    push(Role::User, text.clone())
                }
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        if block_type(block) == Some("text")
                            && let Some(text) = block.get("text").and_then(Value::as_str)
                            && !is_harness_wrapper(text)
                        {
                            push(Role::User, text.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            let Some(blocks) = record.pointer("/message/content").and_then(Value::as_array) else {
                return;
            };
            for block in blocks {
                match block_type(block) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            push(Role::Assistant, text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        if let Some(text) = tool_use_text(block) {
                            push(Role::Tool, text);
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("ai-title") => {
            if let Some(title) = record.get("aiTitle").and_then(Value::as_str) {
                parsed.title = Some(title.to_string());
            }
        }
        _ => {}
    }
}

/// Starts of user-role texts that Claude Code writes itself (slash-command
/// echoes, command output, notifications, reminders, interruptions).
const HARNESS_WRAPPERS: [&str; 6] = [
    "<command-name>",
    "<command-message>",
    "<local-command-",
    "<task-notification>",
    "<system-reminder>",
    "[Request interrupted",
];

/// Whether a user-role text was written by the harness, not typed.
fn is_harness_wrapper(text: &str) -> bool {
    let text = text.trim_start();
    HARNESS_WRAPPERS.iter().any(|w| text.starts_with(w))
}

/// The `"<name> <arg>"` text of a `tool_use` block.
fn tool_use_text(block: &Value) -> Option<String> {
    let name = block.get("name").and_then(Value::as_str)?;
    Some(tool_text(name, block.get("input")))
}
