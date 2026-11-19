mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::adapters::pi::PiAdapter;
use hound_core::enrich::Enrichment;
use hound_core::index::{Index, RefreshStats, SCHEMA_VERSION};
use hound_core::model::{Harness, SessionKey};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const SLUG: &str = "-home-u-proj";

fn adapters(root: &Path) -> Vec<Box<dyn Adapter>> {
    vec![Box::new(ClaudeAdapter::new(root.to_path_buf(), true))]
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        id: id.to_string(),
    }
}

fn path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG).join(format!("{id}.jsonl"))
}

fn s1_lines() -> Vec<String> {
    vec![
        claude_user(S1, "/home/u/proj", T0, "fix the depot alert"),
        claude_text(S1, T1, "Sent to the Signal group"),
        claude_title(S1, "Depot alerts"),
    ]
}

/// Two sessions: S1 with a harness title, S2 with only a user prompt.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let state = dir.path().join("state");
        write_lines(&path(&root, S1), &s1_lines());
        write_lines(
            &path(&root, S2),
            &[claude_user(S2, "/home/u/qa", T0, "plan the qa pass")],
        );
        Fixture {
            _dir: dir,
            root,
            state,
        }
    }

    fn open(&self) -> Index {
        Index::open(&self.state).unwrap()
    }

    fn refresh(&self, ix: &Index) -> RefreshStats {
        ix.refresh(&adapters(&self.root), &mut |_, _| {})
            .unwrap()
            .unwrap()
    }

    /// An index with both sessions refreshed once.
    fn indexed(&self) -> Index {
        let ix = self.open();
        self.refresh(&ix);
        ix
    }
}

#[test]
fn fresh_index_adds_sessions() {
    let f = Fixture::new();
    let ix = f.open();
    let ad = adapters(&f.root);
    let st = ix.refresh(&ad, &mut |_, _| {}).unwrap().unwrap();
    assert_eq!(
        st,
        RefreshStats {
            added: 2,
            ..Default::default()
        }
    );
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(
        (
            d.title.as_str(),
            d.resume_cwd.as_str(),
            d.started_at,
            d.updated_at
        ),
        ("Depot alerts", "/home/u/proj", T0_SECS, T1_SECS)
    );
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.files[0].offset, d.files[0].size);
    assert_eq!(
        ix.session(&key(S2)).unwrap().unwrap().title,
        "plan the qa pass"
    );
}

#[test]
fn second_refresh_is_a_noop() {
    let f = Fixture::new();
    let ix = f.indexed();
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            unchanged: 2,
            ..Default::default()
        }
    );
}

#[test]
fn append_updates_only_that_session() {
    let f = Fixture::new();
    let ix = f.indexed();
    let before = ix.session(&key(S1)).unwrap().unwrap();
    append_raw(
        &path(&f.root, S1),
        &format!(
            "{}\n",
            claude_text(S1, "2026-11-01T11:00:00.000Z", "and one more thing")
        ),
    );
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            updated: 1,
            unchanged: 1,
            ..Default::default()
        }
    );
    let after = ix.session(&key(S1)).unwrap().unwrap();
    assert!(after.updated_at > before.updated_at);
    assert_eq!(after.updated_at, T0_SECS + 3600);
    assert_eq!(after.started_at, T0_SECS);
    let size = fs::metadata(path(&f.root, S1)).unwrap().len();
    assert_eq!(after.files[0].offset, size);
    assert_eq!(after.files[0].size, size);
    assert_eq!(after.title, "Depot alerts");
    assert_eq!(after.resume_cwd, "/home/u/proj");
}

#[test]
fn late_title_replaces_prompt_fallback() {
    let f = Fixture::new();
    let ix = f.indexed();
    append_raw(
        &path(&f.root, S2),
        &format!("{}\n", claude_title(S2, "QA pass")),
    );
    f.refresh(&ix);
    assert_eq!(ix.session(&key(S2)).unwrap().unwrap().title, "QA pass");
}

#[test]
fn rewrite_triggers_full_reindex() {
    let f = Fixture::new();
    let ix = f.indexed();
    let mut lines = s1_lines();
    lines[0] = claude_user(
        S1,
        "/home/u/other",
        T0,
        "a completely different first prompt",
    );
    lines.push(claude_text(S1, T1, "extra"));
    write_lines(&path(&f.root, S1), &lines);
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            updated: 1,
            full_reindex: 1,
            unchanged: 1,
            ..Default::default()
        }
    );
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.resume_cwd, "/home/u/other");
    assert_eq!(d.cwds, vec!["/home/u/other".to_string()]);
    let size = fs::metadata(path(&f.root, S1)).unwrap().len();
    assert_eq!(d.files[0].offset, size);
}

#[test]
fn truncation_triggers_full_reindex() {
    let f = Fixture::new();
    let ix = f.indexed();
    write_lines(&path(&f.root, S1), &s1_lines()[..1]);
    let st = f.refresh(&ix);
    assert_eq!(st.full_reindex, 1);
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.title, "fix the depot alert");
    assert_eq!(d.updated_at, T0_SECS);
}

#[test]
fn deleted_file_removes_session() {
    let f = Fixture::new();
    let ix = f.indexed();
    fs::remove_file(path(&f.root, S2)).unwrap();
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            removed: 1,
            unchanged: 1,
            ..Default::default()
        }
    );
    assert!(ix.session(&key(S2)).unwrap().is_none());
    assert_eq!(ix.sessions().unwrap().len(), 1);
}

#[test]
fn new_subagent_file_joins_parent() {
    let f = Fixture::new();
    let ix = f.indexed();
    let sub = f
        .root
        .join(SLUG)
        .join(S1)
        .join("subagents")
        .join("agent-a.jsonl");
    write_lines(&sub, &[claude_text(S1, T1, "subagent work")]);
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            updated: 1,
            unchanged: 1,
            ..Default::default()
        }
    );
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert!(d.files.len() == 2 && d.files[1].subagent);
    assert!(!d.files[0].subagent);
    assert_eq!(d.files[1].path, sub);
}

#[test]
fn untitled_and_no_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let file = path(&root, S1);
    // An empty timestamp does not parse, so the file records no time at all.
    write_lines(&file, &[claude_tool_result(S1, "", "output")]);
    let mtime = fs::metadata(&file)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let ix = Index::open(&dir.path().join("state")).unwrap();
    ix.refresh(&adapters(&root), &mut |_, _| {})
        .unwrap()
        .unwrap();
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.title, "(untitled)");
    assert_eq!(d.started_at, mtime);
    assert_eq!(d.updated_at, mtime);
    assert_eq!(d.resume_cwd, "");
}

#[test]
fn long_prompt_title_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let prompt = "é".repeat(300);
    write_lines(&path(&root, S1), &[claude_user(S1, "/p", T0, &prompt)]);
    let ix = Index::open(&dir.path().join("state")).unwrap();
    ix.refresh(&adapters(&root), &mut |_, _| {})
        .unwrap()
        .unwrap();
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.title.chars().count(), 80);
    assert_eq!(d.title, "é".repeat(80));
}

#[test]
fn schema_mismatch_rebuilds() {
    let f = Fixture::new();
    drop(f.indexed());
    fs::write(
        f.state.join("state.json"),
        r#"{"schema_version":0,"heartbeat":0}"#,
    )
    .unwrap();
    let ix = f.open();
    assert!(ix.sessions().unwrap().is_empty());
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            added: 2,
            ..Default::default()
        }
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(f.state.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["schema_version"], SCHEMA_VERSION);
}

#[test]
fn corrupt_index_dir_rebuilds() {
    let f = Fixture::new();
    drop(f.indexed());
    fs::write(f.state.join("index").join("meta.json"), "garbage{{").unwrap();
    let ix = f.open();
    assert!(ix.sessions().unwrap().is_empty());
    assert_eq!(f.refresh(&ix).added, 2);
}

#[test]
fn lock_held_returns_none() {
    let f = Fixture::new();
    let holder = f.open();
    let _guard = holder.try_lock().unwrap();
    let ix = f.open();
    assert!(ix.try_lock().is_none());
    assert_eq!(
        ix.refresh(&adapters(&f.root), &mut |_, _| {}).unwrap(),
        None
    );
    assert!(ix.sessions().unwrap().is_empty());
    drop(_guard);
    assert_eq!(f.refresh(&ix).added, 2);
}

#[test]
fn progress_called_per_session() {
    let f = Fixture::new();
    let ix = f.open();
    let mut calls = Vec::new();
    ix.refresh(&adapters(&f.root), &mut |done, total| {
        calls.push((done, total))
    })
    .unwrap()
    .unwrap();
    assert_eq!(calls, vec![(1, 2), (2, 2)]);
}

#[test]
fn index_survives_reopen() {
    let f = Fixture::new();
    drop(f.indexed());
    let ix = f.open();
    assert_eq!(ix.sessions().unwrap().len(), 2);
    assert_eq!(
        f.refresh(&ix),
        RefreshStats {
            unchanged: 2,
            ..Default::default()
        }
    );
}

#[test]
fn pi_session_keeps_resume_cwd_after_append() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pi");
    let id = "01a1da2b-0000-7000-8000-000000000001";
    let file = root
        .join("--home-u-p--")
        .join(format!("2026-11-14T13-07-09-755Z_{id}.jsonl"));
    write_lines(
        &file,
        &[
            pi_header(id, "/home/u/p", T0),
            pi_user(T0, "tidy the pi config"),
        ],
    );
    let ad: Vec<Box<dyn Adapter>> = vec![Box::new(PiAdapter::new(root.clone()))];
    let ix = Index::open(&dir.path().join("state")).unwrap();
    ix.refresh(&ad, &mut |_, _| {}).unwrap().unwrap();
    append_raw(&file, &format!("{}\n", pi_assistant(T1, "done")));
    let st = ix.refresh(&ad, &mut |_, _| {}).unwrap().unwrap();
    assert_eq!(
        st,
        RefreshStats {
            updated: 1,
            ..Default::default()
        }
    );
    let d = ix
        .session(&SessionKey {
            harness: Harness::Pi,
            id: id.to_string(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(d.resume_cwd, "/home/u/p");
    assert_eq!(d.cwds, vec!["/home/u/p".to_string()]);
    assert_eq!(d.title, "tidy the pi config");
    assert_eq!((d.started_at, d.updated_at), (T0_SECS, T1_SECS));
}

#[test]
fn enrichment_and_total_bytes() {
    let f = Fixture::new();
    let ix = f.indexed();
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.summary, None);
    assert!(d.tags.is_empty());
    assert_eq!(d.enriched_at_bytes, 0);
    assert_eq!(
        d.total_bytes(),
        fs::metadata(path(&f.root, S1)).unwrap().len()
    );
}

#[test]
fn concurrent_first_opens_all_succeed() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        Index::open(&state).map(|ix| ix.sessions().unwrap().len())
                    })
                })
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap().unwrap(), 0);
            }
        });
    }
}

/// A rebuild needs `writer.lock`; while another holder has it, `open` waits
/// briefly, then fails and leaves the directory alone.
#[test]
fn schema_mismatch_with_lock_held_fails_without_wiping() {
    let f = Fixture::new();
    let holder = f.indexed();
    let guard = holder.try_lock().unwrap();
    let stale = r#"{"schema_version":0,"heartbeat":0}"#;
    fs::write(f.state.join("state.json"), stale).unwrap();

    assert!(Index::open(&f.state).is_err());
    assert!(f.state.join("index").join("meta.json").exists());
    assert_eq!(
        fs::read_to_string(f.state.join("state.json")).unwrap(),
        stale
    );
    assert_eq!(holder.sessions().unwrap().len(), 2);

    drop(guard);
    let ix = f.open();
    assert!(ix.sessions().unwrap().is_empty());
}

#[test]
fn titles_are_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    write_lines(
        &path(&root, S1),
        &[claude_user(S1, "/p", T0, "  fix the\n\n  depot\talert  \n")],
    );
    write_lines(
        &path(&root, S2),
        &[
            claude_user(S2, "/p", T0, "x"),
            claude_title(S2, "QA\n pass  plan"),
        ],
    );
    let ix = Index::open(&dir.path().join("state")).unwrap();
    ix.refresh(&adapters(&root), &mut |_, _| {})
        .unwrap()
        .unwrap();
    assert_eq!(
        ix.session(&key(S1)).unwrap().unwrap().title,
        "fix the depot alert"
    );
    assert_eq!(ix.session(&key(S2)).unwrap().unwrap().title, "QA pass plan");
}

/// When the on-disk index cannot be reloaded, reads use the generation
/// already loaded instead of failing.
#[test]
fn failed_reload_falls_back_to_loaded_generation() {
    let f = Fixture::new();
    let ix = f.indexed();
    fs::write(f.state.join("index").join("meta.json"), "garbage{{").unwrap();
    assert_eq!(ix.sessions().unwrap().len(), 2);
    assert!(ix.session(&key(S1)).unwrap().is_some());
}

fn enrich_s1(ix: &Index) {
    let e = Enrichment {
        summary: "Fixes the alert".to_string(),
        category: "Ops".to_string(),
        tags: vec!["alerts".to_string()],
    };
    assert!(ix.set_enrichment(&key(S1), &e, 7).unwrap());
}

/// A root that cannot be read says nothing about its sessions: they stay, with
/// their enrichment, and the refresh reports the root.
#[test]
fn unreadable_root_keeps_its_sessions() {
    let f = Fixture::new();
    let ix = f.indexed();
    enrich_s1(&ix);
    let missing = f.root.with_file_name("no-such-root");

    let st = ix
        .refresh(&adapters(&missing), &mut |_, _| {})
        .unwrap()
        .unwrap();

    assert_eq!(st.removed, 0);
    assert_eq!(st.unavailable.len(), 1);
    assert_eq!(st.unavailable[0].path, missing);
    assert_eq!(st.unavailable[0].harness, Harness::Claude);
    assert_eq!(ix.sessions().unwrap().len(), 2);
    let d = ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.summary.as_deref(), Some("Fixes the alert"));
    assert_eq!(d.enriched_at_bytes, 7);
}

/// Only the unreadable harness is kept: the other one is refreshed as usual.
#[test]
fn unreadable_root_leaves_other_harnesses_alone() {
    let f = Fixture::new();
    let ix = f.indexed();
    let pi_root = f.root.with_file_name("pi");
    fs::create_dir_all(&pi_root).unwrap();
    let ad: Vec<Box<dyn Adapter>> = vec![
        Box::new(ClaudeAdapter::new(f.root.with_file_name("gone"), true)),
        Box::new(PiAdapter::new(pi_root)),
    ];
    let st = ix.refresh(&ad, &mut |_, _| {}).unwrap().unwrap();
    assert_eq!((st.removed, st.unavailable.len()), (0, 1));
    assert_eq!(ix.sessions().unwrap().len(), 2);
}

#[test]
fn existing_empty_root_removes_its_sessions() {
    let f = Fixture::new();
    let ix = f.indexed();
    enrich_s1(&ix);
    fs::remove_dir_all(&f.root).unwrap();
    fs::create_dir_all(&f.root).unwrap();

    let st = f.refresh(&ix);

    assert_eq!(
        st,
        RefreshStats {
            removed: 2,
            ..Default::default()
        }
    );
    assert!(ix.sessions().unwrap().is_empty());
}

/// `n` Claude sessions under `root`, with ids `00000000-…-<i>`.
fn many_sessions(root: &Path, n: usize) {
    for i in 0..n {
        let id = format!("00000000-0000-0000-0000-{i:012}");
        write_lines(
            &path(root, &id),
            &[claude_user(&id, "/home/u/proj", T0, &format!("task {i}"))],
        );
    }
}

#[test]
fn large_refresh_commits_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    many_sessions(&root, 45);
    let ix = Index::open(&dir.path().join("state")).unwrap();
    let mut commits = 0;
    let mut progress_calls = 0;

    let st = ix
        .refresh_notifying(
            &adapters(&root),
            &mut |_, _| progress_calls += 1,
            &mut || commits += 1,
        )
        .unwrap()
        .unwrap();

    assert_eq!(commits, 3);
    assert_eq!(progress_calls, 45);
    assert_eq!(
        st,
        RefreshStats {
            added: 45,
            ..Default::default()
        }
    );
    assert_eq!(ix.sessions().unwrap().len(), 45);

    // Nothing changed: nothing to commit.
    let mut commits = 0;
    ix.refresh_notifying(&adapters(&root), &mut |_, _| {}, &mut || commits += 1)
        .unwrap()
        .unwrap();
    assert_eq!(commits, 0);

    // Only removals: one final commit.
    fs::remove_dir_all(&root).unwrap();
    fs::create_dir_all(&root).unwrap();
    let mut commits = 0;
    let st = ix
        .refresh_notifying(&adapters(&root), &mut |_, _| {}, &mut || commits += 1)
        .unwrap()
        .unwrap();
    assert_eq!((st.removed, commits), (45, 1));
}

/// Each committed batch is visible to another handle while the refresh goes on,
/// with every session's messages committed alongside it.
#[test]
fn committed_batch_is_visible_to_another_handle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let state = dir.path().join("state");
    many_sessions(&root, 25);
    let ix = Index::open(&state).unwrap();
    let mut seen = Vec::new();

    ix.refresh_notifying(&adapters(&root), &mut |_, _| {}, &mut || {
        let other = Index::open(&state).unwrap();
        let sessions = other.sessions().unwrap();
        let with_messages = sessions
            .iter()
            .filter(|s| !other.messages(&s.key).unwrap().is_empty())
            .count();
        seen.push((sessions.len(), with_messages));
    })
    .unwrap()
    .unwrap();

    assert_eq!(seen, vec![(20, 20), (25, 25)]);
}

/// A rebuild whose old index cannot be deleted (here: a read-only directory;
/// on Windows: files another process has open) fails with a clear message and
/// leaves no `state.json`, so the next open rebuilds.
#[cfg(unix)]
#[test]
fn rebuild_with_undeletable_index_reports_index_in_use() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    drop(f.indexed());
    fs::write(
        f.state.join("state.json"),
        r#"{"schema_version":0,"heartbeat":0}"#,
    )
    .unwrap();
    let index_dir = f.state.join("index");
    fs::set_permissions(&index_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let result = Index::open(&f.state);
    fs::set_permissions(&index_dir, fs::Permissions::from_mode(0o755)).unwrap();

    let err = format!("{:#}", result.err().expect("open should fail"));
    assert!(
        err.contains("another hound process is using the index"),
        "{err}"
    );
    assert!(err.contains("close it and retry"), "{err}");
    assert!(!f.state.join("state.json").exists());

    let ix = f.open();
    assert!(ix.sessions().unwrap().is_empty());
    assert_eq!(f.refresh(&ix).added, 2);
}
