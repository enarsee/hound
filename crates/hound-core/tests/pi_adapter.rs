mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::pi::PiAdapter;
use hound_core::model::{Harness, Role, SourceFile};
use serde_json::json;

const U: &str = "01a1da2b-0000-7000-8000-000000000001";
const SLUG: &str = "--home-u-p--";

fn session_path(root: &Path) -> PathBuf {
    root.join(SLUG)
        .join(format!("2026-11-14T13-07-09-755Z_{U}.jsonl"))
}

fn session_file(root: &Path) -> SourceFile {
    SourceFile {
        path: session_path(root),
        subagent: false,
    }
}

fn adapter(root: &Path) -> PiAdapter {
    PiAdapter::new(root.to_path_buf())
}

fn texts(p: &hound_core::model::Parsed) -> Vec<(Role, &str)> {
    p.messages
        .iter()
        .map(|m| (m.role, m.text.as_str()))
        .collect()
}

#[test]
fn discovers_and_keys_by_header_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lines(
        &session_path(root),
        &[pi_header("HID", "/home/u/p", T0), pi_user(T0, "hi")],
    );
    write_lines(&root.join(SLUG).join("notes.txt"), &["x".to_string()]);

    let found = adapter(root).discover();
    assert_eq!(found.len(), 1);
    let s = &found[0];
    assert_eq!(s.key.as_string(), "pi:HID");
    assert_eq!(s.key.harness, Harness::Pi);
    assert_eq!(s.files.len(), 1);
    assert_eq!(s.files[0].path, session_path(root));
    assert!(!s.files[0].subagent);
}

#[test]
fn falls_back_to_filename_uuid_without_header() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lines(&session_path(root), &[pi_user(T0, "hi")]);

    let found = adapter(root).discover();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.id, U);
}

#[test]
fn empty_file_falls_back_to_filename_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(SLUG)).unwrap();
    fs::write(session_path(root), "").unwrap();

    let found = adapter(root).discover();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.id, U);
}

#[test]
fn cleans_records() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let f = session_file(dir.path());
    write_lines(
        &f.path,
        &[
            pi_header("HID", "/home/u/p", T0),
            pi_system(T0, "You are an expert"),
            pi_user(T0, "scrape retries"),
            pi_assistant(T1, "Added backoff"),
        ],
    );

    let p = a.parse(&f, 0).unwrap();
    assert_eq!(
        texts(&p),
        vec![
            (Role::User, "scrape retries"),
            (Role::Assistant, "Added backoff")
        ]
    );
    assert_eq!(p.cwds, vec!["/home/u/p"]);
    assert_eq!(p.title, None);
    assert_eq!(p.first_prompt.as_deref(), Some("scrape retries"));
    assert_eq!(p.first_ts, Some(T0_SECS));
    assert_eq!(p.last_ts, Some(T1_SECS));
    assert_eq!(p.new_offset, fs::metadata(&f.path).unwrap().len());
    assert!(p.messages.iter().all(|m| !m.subagent));
    assert_eq!(p.messages[0].ts, Some(T0_SECS));
    assert_eq!(p.messages[1].ts, Some(T1_SECS));
}

#[test]
fn tool_call_becomes_tool_message() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let f = session_file(dir.path());
    write_lines(
        &f.path,
        &[
            pi_header("HID", "/home/u/p", T0),
            pi_tool_call(T0, "bash", json!({"command": "cargo test", "timeout": 60})),
            pi_tool_result(T0, "bash", "HUGE OUTPUT"),
            pi_tool_call(T1, "read", json!({"path": "/etc/hosts", "limit": 10})),
            pi_tool_call(T1, "todo", json!({})),
            pi_tool_call(T1, "bash", json!({"command": "x".repeat(500)})),
        ],
    );

    let p = a.parse(&f, 0).unwrap();
    let long = format!("bash {}", "x".repeat(200));
    assert_eq!(
        texts(&p),
        vec![
            (Role::Tool, "bash cargo test"),
            (Role::Tool, "read /etc/hosts"),
            (Role::Tool, "todo"),
            (Role::Tool, long.as_str()),
        ]
    );
    assert_eq!(p.first_prompt, None);
}

#[test]
fn skips_thinking_and_tool_results() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let f = session_file(dir.path());
    write_lines(
        &f.path,
        &[
            pi_thinking(T0, "secret plan"),
            pi_tool_result(T0, "bash", "HUGE OUTPUT"),
            pi_assistant(T1, "done"),
        ],
    );

    let p = a.parse(&f, 0).unwrap();
    assert_eq!(texts(&p), vec![(Role::Assistant, "done")]);
}

#[test]
fn parse_from_offset_returns_only_new() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let f = session_file(dir.path());
    write_lines(
        &f.path,
        &[pi_header("HID", "/home/u/p", T0), pi_user(T0, "first")],
    );
    let p1 = a.parse(&f, 0).unwrap();
    assert_eq!(p1.first_prompt.as_deref(), Some("first"));

    append_raw(&f.path, &(pi_assistant(T1, "later reply") + "\n"));
    let p2 = a.parse(&f, p1.new_offset).unwrap();
    assert_eq!(texts(&p2), vec![(Role::Assistant, "later reply")]);
    assert_eq!(p2.title, None);
    assert_eq!(p2.first_prompt, None);
    assert_eq!(p2.new_offset, fs::metadata(&f.path).unwrap().len());
}

#[test]
fn invalid_utf8_and_partial_tail() {
    let dir = tempfile::tempdir().unwrap();
    let a = adapter(dir.path());
    let f = session_file(dir.path());
    // Written literally: the builders' key order is serde_json's, not pi's.
    let partial = format!(
        r#"{{"type":"message","timestamp":"{T1}","message":{{"role":"user","content":[{{"type":"text","text":"tail prompt"}}]}}}}"#
    );
    let (head, rest) = partial.split_at(r#"{"type":"message","time"#.len());
    assert_eq!(head, r#"{"type":"message","time"#);

    write_lines(&f.path, &[pi_assistant(T0, "good one")]);
    let mut file = OpenOptions::new().append(true).open(&f.path).unwrap();
    file.write_all(&[0xff, 0xfe, b'\n']).unwrap();
    drop(file);
    append_raw(&f.path, "not json\n");
    append_raw(&f.path, &(pi_assistant(T0, "good two") + "\n"));
    append_raw(&f.path, head);

    let p = a.parse(&f, 0).unwrap();
    assert_eq!(p.messages.len(), 2);
    let len = fs::metadata(&f.path).unwrap().len();
    assert_eq!(p.new_offset, len - head.len() as u64);

    append_raw(&f.path, &(rest.to_string() + "\n"));
    let p2 = a.parse(&f, p.new_offset).unwrap();
    assert_eq!(texts(&p2), vec![(Role::User, "tail prompt")]);
}

#[test]
fn resume_args() {
    let a = PiAdapter::new(PathBuf::from("/nonexistent"));
    assert_eq!(a.resume_args("abc"), vec!["--session", "abc"]);
    assert_eq!(a.harness(), Harness::Pi);
}

#[test]
fn missing_root_discovers_nothing() {
    assert!(
        PiAdapter::new(PathBuf::from("/nonexistent"))
            .discover()
            .is_empty()
    );
}

/// Read-only sanity run over the real `~/.pi/agent/sessions`; prints counts
/// only. Run with `HOUND_LIVE_TESTS=1 cargo test -p hound-core --test
/// pi_adapter -- --ignored --nocapture`.
#[test]
#[ignore]
fn sanity_real_pi_sessions() {
    if !live_tests_enabled() {
        return;
    }
    let home = std::env::var_os("HOME").expect("HOME");
    let root = PathBuf::from(home).join(".pi/agent/sessions");
    let a = PiAdapter::new(root);
    let sessions = a.discover();
    let (mut files, mut msgs, mut errors) = (0, 0, 0);
    let (mut users, mut assistants, mut tools, mut prompted) = (0, 0, 0, 0);
    let start = Instant::now();
    for s in &sessions {
        for f in &s.files {
            files += 1;
            match a.parse(f, 0) {
                Ok(p) => {
                    msgs += p.messages.len();
                    users += p.messages.iter().filter(|m| m.role == Role::User).count();
                    assistants += p
                        .messages
                        .iter()
                        .filter(|m| m.role == Role::Assistant)
                        .count();
                    tools += p.messages.iter().filter(|m| m.role == Role::Tool).count();
                    prompted += usize::from(p.first_prompt.is_some());
                }
                Err(_) => errors += 1,
            }
        }
    }
    println!(
        "sessions={} files={files} messages={msgs} user={users} assistant={assistants} tool={tools} with_first_prompt={prompted} errors={errors} elapsed={:?}",
        sessions.len(),
        start.elapsed()
    );
}

#[test]
fn session_ids_that_could_be_flags_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = |name: &str| root.join(SLUG).join(format!("{name}.jsonl"));
    // Header ids decide; an invalid one is not replaced by the file name.
    write_lines(&file("2026_good"), &[pi_header("-x", "/p", T0)]);
    write_lines(&file("2026_good2"), &[pi_header("a b", "/p", T0)]);
    write_lines(&file("2026_ok"), &[pi_header("HID-1_a", "/p", T0)]);
    // Without a header the file name's id is checked the same way.
    write_lines(&file("2026_--session"), &[pi_user(T0, "hi")]);
    write_lines(&file("2026_fine"), &[pi_user(T0, "hi")]);

    let found = adapter(root).discover();
    let mut ids: Vec<_> = found.iter().map(|s| s.key.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["HID-1_a", "fine"]);
}

#[test]
fn missing_root_is_reported_unavailable() {
    let err = PiAdapter::new(PathBuf::from("/nonexistent"))
        .try_discover()
        .unwrap_err();
    assert_eq!(err.path, PathBuf::from("/nonexistent"));
    assert_eq!(err.harness, Harness::Pi);

    let dir = tempfile::tempdir().unwrap();
    assert!(adapter(dir.path()).try_discover().unwrap().is_empty());
}
