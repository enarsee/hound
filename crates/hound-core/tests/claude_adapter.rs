mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::model::{Harness, Role, SourceFile};
use serde_json::json;

const S: &str = "11111111-1111-1111-1111-111111111111";
const SLUG: &str = "-home-u-proj";

fn main_path(root: &Path) -> PathBuf {
    root.join(SLUG).join(format!("{S}.jsonl"))
}

fn subagent_path(root: &Path) -> PathBuf {
    root.join(SLUG)
        .join(S)
        .join("subagents")
        .join("agent-a1.jsonl")
}

fn main_file(root: &Path) -> SourceFile {
    SourceFile {
        path: main_path(root),
        subagent: false,
    }
}

fn adapter(root: &Path) -> ClaudeAdapter {
    ClaudeAdapter::new(root.to_path_buf(), true)
}

/// The full mix of records used by `cleans_records` and `subagent_file_marks_messages`.
fn mixed_lines() -> Vec<String> {
    vec![
        claude_user(S, "/home/u/proj", T0, "fix the depot alert"),
        claude_thinking(S, T0, "secret plan"),
        claude_text(S, T1, "Sent to the Signal group"),
        claude_tool_use(S, T1, "Bash", json!({"command": "systemctl status depot"})),
        claude_tool_use(S, T1, "Read", json!({"file_path": "/etc/hosts"})),
        claude_tool_use(S, T1, "TodoWrite", json!({"todos": []})),
        claude_tool_result(S, T1, "HUGE OUTPUT"),
        claude_meta_user(S, T1, "skill text"),
        claude_sidechain(S, T1, "side"),
        claude_title(S, "Old title"),
        claude_title(S, "Depot alerts"),
    ]
}

#[test]
fn discovers_sessions_and_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lines(&main_path(root), &[claude_text(S, T0, "hi")]);
    write_lines(&subagent_path(root), &[claude_text(S, T0, "sub")]);
    write_lines(
        &root
            .join(SLUG)
            .join(S)
            .join("subagents")
            .join("agent-a1.meta.json"),
        &["{}".to_string()],
    );

    let found = ClaudeAdapter::new(root.to_path_buf(), true).discover();
    assert_eq!(found.len(), 1);
    let s = &found[0];
    assert_eq!(s.key.as_string(), format!("claude:{S}"));
    assert_eq!(s.files.len(), 2);
    assert!(s.files[0].path.ends_with(format!("{S}.jsonl")));
    assert!(!s.files[0].subagent);
    assert!(s.files[1].subagent);

    let found = ClaudeAdapter::new(root.to_path_buf(), false).discover();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].files.len(), 1);
}

#[test]
fn cleans_records() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    write_lines(&main.path, &mixed_lines());

    let p = a.parse(&main, 0).unwrap();
    let texts: Vec<_> = p
        .messages
        .iter()
        .map(|m| (m.role, m.text.as_str()))
        .collect();
    assert_eq!(
        texts,
        vec![
            (Role::User, "fix the depot alert"),
            (Role::Assistant, "Sent to the Signal group"),
            (Role::Tool, "Bash systemctl status depot"),
            (Role::Tool, "Read /etc/hosts"),
            (Role::Tool, "TodoWrite"),
        ]
    );
    assert!(p.messages.iter().all(|m| !m.subagent));
    assert_eq!(p.title.as_deref(), Some("Depot alerts"));
    assert_eq!(p.first_prompt.as_deref(), Some("fix the depot alert"));
    assert_eq!(p.cwds, vec!["/home/u/proj"]);
    assert_eq!((p.first_ts, p.last_ts), (Some(T0_SECS), Some(T1_SECS)));
    assert_eq!(p.new_offset, fs::metadata(&main.path).unwrap().len());
    assert_eq!(p.messages[0].ts, Some(T0_SECS));
    assert_eq!(p.messages[1].ts, Some(T1_SECS));
}

#[test]
fn subagent_file_marks_messages() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let sub = SourceFile {
        path: subagent_path(dir.path()),
        subagent: true,
    };
    write_lines(&sub.path, &mixed_lines());

    let p = a.parse(&sub, 0).unwrap();
    assert!(!p.messages.is_empty());
    assert!(p.messages.iter().all(|m| m.subagent));
    let texts: Vec<_> = p.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "fix the depot alert",
            "Sent to the Signal group",
            "Bash systemctl status depot",
            "Read /etc/hosts",
            "TodoWrite",
            "side",
        ]
    );
}

#[test]
fn tool_arg_truncated_to_200_chars() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    let long = "x".repeat(500);
    write_lines(
        &main.path,
        &[claude_tool_use(S, T0, "Bash", json!({"command": long}))],
    );

    let p = a.parse(&main, 0).unwrap();
    assert_eq!(p.messages.len(), 1);
    assert_eq!(p.messages[0].text, format!("Bash {}", "x".repeat(200)));
}

#[test]
fn multiple_cwds_first_seen_order() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    write_lines(
        &main.path,
        &[
            claude_user(S, "/a", T0, "one"),
            claude_user(S, "/b", T0, "two"),
            claude_user(S, "/a", T1, "three"),
        ],
    );

    let p = a.parse(&main, 0).unwrap();
    assert_eq!(p.cwds, vec!["/a", "/b"]);
}

#[test]
fn parse_from_offset_returns_only_new() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    write_lines(
        &main.path,
        &[
            claude_user(S, "/home/u/proj", T0, "first"),
            claude_title(S, "A title"),
        ],
    );
    let p1 = a.parse(&main, 0).unwrap();
    assert_eq!(p1.title.as_deref(), Some("A title"));

    append_raw(&main.path, &(claude_text(S, T1, "later reply") + "\n"));
    let p2 = a.parse(&main, p1.new_offset).unwrap();
    assert_eq!(p2.messages.len(), 1);
    assert_eq!(p2.messages[0].text, "later reply");
    assert_eq!(p2.title, None);
    assert_eq!(p2.first_prompt, None);
}

#[test]
fn invalid_utf8_and_partial_tail() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    // Written literally: the builders' key order is serde_json's, not Claude's.
    let partial = format!(
        r#"{{"type":"user","message":{{"role":"user","content":"tail prompt"}},"timestamp":"{T1}","cwd":"/p","sessionId":"{S}"}}"#
    );
    let (head, rest) = partial.split_at(r#"{"type":"user","mess"#.len());
    assert_eq!(head, r#"{"type":"user","mess"#);

    write_lines(&main.path, &[claude_text(S, T0, "good one")]);
    let mut f = OpenOptions::new().append(true).open(&main.path).unwrap();
    f.write_all(&[0xff, 0xfe, b'\n']).unwrap();
    drop(f);
    append_raw(&main.path, "not json\n");
    append_raw(&main.path, &(claude_text(S, T0, "good two") + "\n"));
    append_raw(&main.path, head);

    let p = a.parse(&main, 0).unwrap();
    assert_eq!(p.messages.len(), 2);
    let len = fs::metadata(&main.path).unwrap().len();
    assert_eq!(p.new_offset, len - head.len() as u64);

    append_raw(&main.path, &(rest.to_string() + "\n"));
    let p2 = a.parse(&main, p.new_offset).unwrap();
    assert_eq!(p2.messages.len(), 1);
    assert_eq!(p2.messages[0].text, "tail prompt");
}

#[test]
fn huge_line_is_skipped_cheaply() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    let blob = format!(
        r#"{{"type":"api-request-blob","blob":"{}"}}"#,
        "a".repeat(8 * 1024 * 1024)
    );
    write_lines(
        &main.path,
        &[
            claude_text(S, T0, "before"),
            blob,
            claude_text(S, T1, "after"),
        ],
    );

    let start = Instant::now();
    let p = a.parse(&main, 0).unwrap();
    let took = start.elapsed();
    assert_eq!(p.messages.len(), 2);
    assert!(took < Duration::from_millis(500), "took {took:?}");
}

#[test]
fn resume_args() {
    let a = ClaudeAdapter::new(PathBuf::from("/nonexistent"), true);
    assert_eq!(a.resume_args("abc"), vec!["--resume", "abc"]);
    assert_eq!(a.harness(), Harness::Claude);
}

#[test]
fn missing_root_discovers_nothing() {
    assert!(
        ClaudeAdapter::new(PathBuf::from("/nonexistent"), true)
            .discover()
            .is_empty()
    );
}

/// Read-only sanity run over the real `~/.claude/projects`; prints counts
/// only. Run with `HOUND_LIVE_TESTS=1 cargo test -p hound-core --test
/// claude_adapter -- --ignored --nocapture`.
#[test]
#[ignore]
fn sanity_real_claude_projects() {
    if !live_tests_enabled() {
        return;
    }
    let home = std::env::var_os("HOME").expect("HOME");
    let root = PathBuf::from(home).join(".claude/projects");
    let a = ClaudeAdapter::new(root, true);
    let sessions = a.discover();
    let (mut files, mut msgs, mut sub_msgs, mut titled, mut errors) = (0, 0, 0, 0, 0);
    let start = Instant::now();
    for s in &sessions {
        for f in &s.files {
            files += 1;
            match a.parse(f, 0) {
                Ok(p) => {
                    msgs += p.messages.len();
                    sub_msgs += p.messages.iter().filter(|m| m.subagent).count();
                    if !f.subagent && p.title.is_some() {
                        titled += 1;
                    }
                }
                Err(_) => errors += 1,
            }
        }
    }
    println!(
        "sessions={} files={files} messages={msgs} subagent_messages={sub_msgs} titled={titled} errors={errors} elapsed={:?}",
        sessions.len(),
        start.elapsed()
    );
}

/// A user record with `extra` fields merged in.
fn user_with(text: serde_json::Value, ts: &str, cwd: &str, extra: serde_json::Value) -> String {
    let mut record = json!({
        "type": "user",
        "isSidechain": false,
        "message": {"role": "user", "content": text},
        "timestamp": ts,
        "cwd": cwd,
        "sessionId": S,
    });
    for (k, v) in extra.as_object().unwrap() {
        record[k] = v.clone();
    }
    record.to_string()
}

#[test]
fn compact_summary_is_not_a_prompt_but_keeps_cwd_and_time() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    write_lines(
        &main.path,
        &[
            user_with(
                json!("This session is being continued from a previous conversation."),
                T0,
                "/home/u/compacted",
                json!({"isCompactSummary": true}),
            ),
            claude_user(S, "/home/u/proj", T1, "now fix the timer"),
        ],
    );

    let p = a.parse(&main, 0).unwrap();
    let texts: Vec<_> = p.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, vec!["now fix the timer"]);
    assert_eq!(p.first_prompt.as_deref(), Some("now fix the timer"));
    assert_eq!(p.cwds, vec!["/home/u/compacted", "/home/u/proj"]);
    assert_eq!((p.first_ts, p.last_ts), (Some(T0_SECS), Some(T1_SECS)));
}

#[test]
fn harness_wrapper_texts_are_not_prompts() {
    let wrappers = [
        "<command-name>/clear</command-name>",
        "<command-message>clear</command-message>",
        "<local-command-stdout>ok</local-command-stdout>",
        "<task-notification>done</task-notification>",
        "<system-reminder>be brief</system-reminder>",
        "[Request interrupted by user]",
    ];
    for wrapper in wrappers {
        let padded = format!("  \n{wrapper}");
        for content in [json!(padded), json!([{"type": "text", "text": padded}])] {
            let dir = tempfile::tempdir().unwrap();
            let a = adapter(dir.path());
            let main = main_file(dir.path());
            write_lines(
                &main.path,
                &[
                    user_with(content, T0, "/home/u/wrapped", json!({})),
                    claude_user(S, "/home/u/proj", T1, "the real prompt"),
                ],
            );

            let p = a.parse(&main, 0).unwrap();
            let texts: Vec<_> = p.messages.iter().map(|m| m.text.as_str()).collect();
            assert_eq!(texts, vec!["the real prompt"], "{wrapper}");
            assert_eq!(
                p.first_prompt.as_deref(),
                Some("the real prompt"),
                "{wrapper}"
            );
            assert_eq!(p.cwds, vec!["/home/u/wrapped", "/home/u/proj"], "{wrapper}");
            assert_eq!(p.first_ts, Some(T0_SECS), "{wrapper}");
        }
    }
}

#[test]
fn text_mentioning_a_wrapper_later_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let main = main_file(dir.path());
    write_lines(
        &main.path,
        &[claude_user(
            S,
            "/p",
            T0,
            "why does <system-reminder> show up?",
        )],
    );
    let p = a.parse(&main, 0).unwrap();
    assert_eq!(p.messages.len(), 1);
}

#[test]
fn session_ids_that_could_be_flags_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for id in ["-rf", "--resume", "has space", "semi;colon", "ok_id-1"] {
        write_lines(
            &root.join(SLUG).join(format!("{id}.jsonl")),
            &[claude_text(id, T0, "hi")],
        );
    }
    let found = adapter(root).discover();
    let ids: Vec<_> = found.iter().map(|s| s.key.id.as_str()).collect();
    assert_eq!(ids, vec!["ok_id-1"]);
}

#[test]
fn missing_root_is_reported_unavailable() {
    let err = ClaudeAdapter::new(PathBuf::from("/nonexistent"), true)
        .try_discover()
        .unwrap_err();
    assert_eq!(err.path, PathBuf::from("/nonexistent"));
    assert_eq!(err.harness, Harness::Claude);
    assert!(err.to_string().contains("/nonexistent"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    assert!(adapter(dir.path()).try_discover().unwrap().is_empty());
}
