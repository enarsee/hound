use std::cell::RefCell;
use std::path::PathBuf;

use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::adapters::pi::PiAdapter;
use hound_core::host::{Host, LivePane};
use hound_core::index::{FileState, SessionDoc};
use hound_core::model::{Harness, SessionKey};
use hound_core::open::{LocalFacts, OpenOutcome, agent_name, open_entry, pick_workspace};
use hound_core::search::{Entry, build_entries};

const NOW: i64 = 1_000_000;
/// Long enough ago that the session cannot be running elsewhere.
const OLD: i64 = NOW - 3600;
const CONFIRM_MSG: &str = "This session changed in the last 2 minutes and may be running elsewhere. Enter again to resume anyway.";

/// Records every call; returns a fixed snapshot.
#[derive(Default)]
struct FakeHost {
    panes: Vec<LivePane>,
    fail_create: bool,
    fail_start: bool,
    calls: RefCell<Vec<String>>,
}

impl FakeHost {
    fn new(panes: Vec<LivePane>) -> Self {
        FakeHost {
            panes,
            ..Default::default()
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn create_tab_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with("create_tab"))
            .count()
    }
}

impl Host for FakeHost {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        self.calls.borrow_mut().push("snapshot".to_string());
        Ok(self.panes.clone())
    }

    fn read_scrollback(&self, pane_id: &str, _lines: u32) -> anyhow::Result<String> {
        self.calls
            .borrow_mut()
            .push(format!("read_scrollback {pane_id}"));
        Ok(String::new())
    }

    fn focus(&self, pane: &LivePane) -> anyhow::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("focus {}:{}", pane.workspace_id, pane.pane_id));
        Ok(())
    }

    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane> {
        self.calls
            .borrow_mut()
            .push(format!("create_tab {workspace_id} {cwd}"));
        if self.fail_create {
            anyhow::bail!("no such workspace");
        }
        let mut p = pane("pnew", workspace_id, cwd);
        p.focused = true;
        Ok(p)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!(
            "start_agent {name} {kind} {pane_id} {}",
            args.join(" ")
        ));
        if self.fail_start {
            Err(anyhow::anyhow!("agent exited").context("start failed"))
        } else {
            Ok(())
        }
    }
}

/// A host whose snapshot always fails.
struct BrokenHost(RefCell<Vec<String>>);

impl Host for BrokenHost {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        self.0.borrow_mut().push("snapshot".to_string());
        anyhow::bail!("socket closed")
    }
    fn read_scrollback(&self, _: &str, _: u32) -> anyhow::Result<String> {
        unreachable!()
    }
    fn focus(&self, _: &LivePane) -> anyhow::Result<()> {
        self.0.borrow_mut().push("focus".to_string());
        Ok(())
    }
    fn create_tab(&self, _: &str, _: &str) -> anyhow::Result<LivePane> {
        self.0.borrow_mut().push("create_tab".to_string());
        anyhow::bail!("unexpected")
    }
    fn start_agent(&self, _: &str, _: &str, _: &str, _: &[String]) -> anyhow::Result<()> {
        self.0.borrow_mut().push("start_agent".to_string());
        Ok(())
    }
}

fn adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(ClaudeAdapter::new(
            PathBuf::from("/nonexistent/claude"),
            true,
        )),
        Box::new(PiAdapter::new(PathBuf::from("/nonexistent/pi"))),
    ]
}

fn session(harness: Harness, id: &str, cwd: &str, updated_at: i64) -> SessionDoc {
    SessionDoc {
        key: SessionKey {
            harness,
            id: id.to_string(),
        },
        title: format!("session {id}"),
        cwds: vec![cwd.to_string()],
        resume_cwd: cwd.to_string(),
        started_at: updated_at - 10,
        updated_at,
        summary: None,
        category: None,
        tags: vec![],
        files: vec![FileState {
            path: PathBuf::from(format!("/data/sessions/{id}.jsonl")),
            subagent: false,
            size: 0,
            offset: 0,
            head_hash: 0,
        }],
        enriched_at_bytes: 0,
    }
}

fn pane(pane_id: &str, workspace_id: &str, cwd: &str) -> LivePane {
    LivePane {
        pane_id: pane_id.to_string(),
        tab_id: format!("{workspace_id}-t1"),
        workspace_id: workspace_id.to_string(),
        workspace_label: String::new(),
        tab_label: String::new(),
        agent_name: None,
        kind: None,
        session_id: None,
        title: String::new(),
        cwd: cwd.to_string(),
        status: String::new(),
        focused: false,
        scrollback: None,
    }
}

fn agent_pane(pane_id: &str, workspace_id: &str, kind: &str, session_id: &str) -> LivePane {
    LivePane {
        kind: Some(kind.to_string()),
        session_id: Some(session_id.to_string()),
        ..pane(pane_id, workspace_id, "/home/u/elsewhere")
    }
}

fn closed(doc: SessionDoc) -> Entry {
    let entry = build_entries(vec![doc], vec![], NOW).remove(0);
    assert!(!entry.is_live());
    entry
}

fn tempdir_str(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().unwrap().to_string()
}

#[test]
fn live_entry_focuses() {
    let live = agent_pane("p8", "w2", "claude", "S1");
    let entry = build_entries(
        vec![session(Harness::Claude, "S1", "/home/u/proj", OLD)],
        vec![live.clone()],
        NOW,
    )
    .remove(0);
    assert!(entry.is_live());
    let host = FakeHost::new(vec![live]);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Focused);
    let calls: Vec<String> = host
        .calls()
        .into_iter()
        .filter(|c| c != "snapshot")
        .collect();
    assert_eq!(calls, vec!["focus w2:p8"]);
    assert_eq!(host.create_tab_calls(), 0);
}

#[test]
fn live_entry_without_host_fails() {
    let entry = build_entries(vec![], vec![agent_pane("p8", "w2", "claude", "S1")], NOW).remove(0);
    assert!(matches!(
        open_entry(&entry, None, &adapters(), &LocalFacts, false, NOW),
        OpenOutcome::Failed(_)
    ));
}

#[test]
fn closed_entry_resumes_in_matching_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![
        pane("p1", "w4", &cwd),
        pane("p2", "w2", &cwd),
        pane("p3", "w2", &cwd),
        LivePane {
            focused: true,
            ..pane("p4", "w9", "/somewhere/else")
        },
    ]);
    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Resumed);
    let calls = host.calls();
    let create = calls
        .iter()
        .position(|c| *c == format!("create_tab w2 {cwd}"))
        .expect("create_tab in w2");
    let start = calls
        .iter()
        .position(|c| c == "start_agent claude-s1 claude pnew --resume S1")
        .expect("start_agent call");
    assert!(create < start);
    assert!(!calls.iter().any(|c| c.starts_with("focus")));
}

#[test]
fn falls_back_to_focused_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![
        pane("p1", "w1", "/srv/other"),
        LivePane {
            focused: true,
            ..pane("p2", "w8", "/srv/x")
        },
    ]);
    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Resumed);
    assert!(host.calls().contains(&format!("create_tab w8 {cwd}")));
}

#[test]
fn no_workspace_fails() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![pane("p1", "w1", "/srv/other")]);
    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert!(matches!(outcome, OpenOutcome::Failed(_)));
    assert_eq!(host.create_tab_calls(), 0);
}

#[test]
fn went_live_since_list_was_built() {
    let entry = closed(session(Harness::Claude, "S1", "/nonexistent/x", OLD));
    let host = FakeHost::new(vec![
        pane("p1", "w1", "/home/u"),
        agent_pane("p5", "w3", "claude", "S1"),
    ]);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Focused);
    assert_eq!(host.calls(), vec!["snapshot", "focus w3:p5"]);
}

#[test]
fn went_live_by_main_file_path() {
    let entry = closed(session(Harness::Claude, "S1", "/nonexistent/x", OLD));
    let host = FakeHost::new(vec![
        agent_pane("p6", "w3", "pi", "S1"),
        agent_pane("p5", "w3", "claude", "/data/sessions/S1.jsonl"),
    ]);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Focused);
    assert_eq!(host.calls(), vec!["snapshot", "focus w3:p5"]);
}

#[test]
fn snapshot_error_fails_with_its_text() {
    let dir = tempfile::tempdir().unwrap();
    let entry = closed(session(Harness::Claude, "S1", &tempdir_str(&dir), OLD));
    let host = BrokenHost(RefCell::new(Vec::new()));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    match outcome {
        OpenOutcome::Failed(msg) => assert!(msg.contains("socket closed"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(*host.0.borrow(), vec!["snapshot"]);
}

#[test]
fn missing_cwd_fails_without_side_effects() {
    let host = FakeHost::new(vec![LivePane {
        focused: true,
        ..pane("p1", "w1", "/home/u")
    }]);
    let entry = closed(session(Harness::Claude, "S1", "/nonexistent/x", OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    match outcome {
        OpenOutcome::Failed(msg) => {
            assert!(msg.contains("/nonexistent/x"), "{msg}");
            assert_eq!(msg, "directory no longer exists: /nonexistent/x");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(host.create_tab_calls(), 0);
    assert!(!host.calls().iter().any(|c| c.starts_with("start_agent")));
}

#[test]
fn empty_cwd_fails() {
    let host = FakeHost::new(vec![LivePane {
        focused: true,
        ..pane("p1", "w1", "/home/u")
    }]);
    let entry = closed(session(Harness::Claude, "S1", "", OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert!(matches!(outcome, OpenOutcome::Failed(_)));
    assert_eq!(host.create_tab_calls(), 0);
}

#[test]
fn running_elsewhere_needs_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![pane("p1", "w1", &cwd)]);
    let entry = closed(session(Harness::Claude, "S1", &cwd, NOW - 30));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);
    assert_eq!(outcome, OpenOutcome::NeedsConfirm(CONFIRM_MSG.to_string()));
    assert_eq!(host.create_tab_calls(), 0);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, true, NOW);
    assert_eq!(outcome, OpenOutcome::Resumed);
    assert_eq!(host.create_tab_calls(), 1);
}

/// Unix seconds now.
fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A session last indexed an hour ago whose main file has just been written.
fn stale_index_fresh_file(dir: &tempfile::TempDir, now: i64) -> Entry {
    let cwd = tempdir_str(dir);
    let file = dir.path().join("S1.jsonl");
    std::fs::write(&file, "{}\n").unwrap();
    let mut doc = session(Harness::Claude, "S1", &cwd, now - 3600);
    doc.files[0].path = file;
    let entry = build_entries(vec![doc], vec![], now).remove(0);
    assert!(!entry.maybe_running_elsewhere(now));
    entry
}

#[test]
fn just_written_file_needs_confirmation_with_host() {
    let now = real_now();
    let dir = tempfile::tempdir().unwrap();
    let entry = stale_index_fresh_file(&dir, now);
    let host = FakeHost::new(vec![pane("p1", "w1", &tempdir_str(&dir))]);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, now);
    assert_eq!(outcome, OpenOutcome::NeedsConfirm(CONFIRM_MSG.to_string()));
    assert_eq!(host.create_tab_calls(), 0);

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, true, now);
    assert_eq!(outcome, OpenOutcome::Resumed);
}

#[test]
fn just_written_file_needs_confirmation_without_host() {
    let now = real_now();
    let dir = tempfile::tempdir().unwrap();
    let entry = stale_index_fresh_file(&dir, now);

    assert_eq!(
        open_entry(&entry, None, &adapters(), &LocalFacts, false, now),
        OpenOutcome::NeedsConfirm(CONFIRM_MSG.to_string())
    );
    assert!(matches!(
        open_entry(&entry, None, &adapters(), &LocalFacts, true, now),
        OpenOutcome::RunHere { .. }
    ));
}

#[test]
fn start_failure_reports_and_does_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost {
        fail_start: true,
        ..FakeHost::new(vec![pane("p1", "w1", &cwd)])
    };
    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    match outcome {
        OpenOutcome::Failed(msg) => assert_eq!(
            msg,
            "tab created but the agent failed to start: start failed: agent exited"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    let calls = host.calls();
    assert_eq!(host.create_tab_calls(), 1);
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("start_agent"))
            .count(),
        1
    );
}

#[test]
fn create_tab_failure_is_reported_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost {
        fail_create: true,
        ..FakeHost::new(vec![pane("p1", "w1", &cwd)])
    };
    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(
        outcome,
        OpenOutcome::Failed("no such workspace".to_string())
    );
    assert!(!host.calls().iter().any(|c| c.starts_with("start_agent")));
}

#[test]
fn agent_name_avoids_names_taken_in_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![LivePane {
        agent_name: Some("claude-abcdef".to_string()),
        ..pane("p1", "w1", &cwd)
    }]);
    let entry = closed(session(Harness::Claude, "ABCDEF12", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Resumed);
    assert!(
        host.calls()
            .contains(&"start_agent claude-abcdef-2 claude pnew --resume ABCDEF12".to_string())
    );
}

#[test]
fn missing_adapter_fails_naming_harness() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![pane("p1", "w1", &cwd)]);
    let entry = closed(session(Harness::Opencode, "S1", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    match outcome {
        OpenOutcome::Failed(msg) => assert!(msg.contains("opencode"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(host.create_tab_calls(), 0);
}

#[test]
fn without_host_runs_here() {
    let dir = tempfile::Builder::new()
        .prefix("with space ")
        .tempdir()
        .unwrap();
    let cwd = tempdir_str(&dir);
    assert!(cwd.contains(' '));

    let entry = closed(session(Harness::Claude, "S1", &cwd, OLD));
    assert_eq!(
        open_entry(&entry, None, &adapters(), &LocalFacts, false, NOW),
        OpenOutcome::RunHere {
            cwd: cwd.clone(),
            argv: vec!["claude".into(), "--resume".into(), "S1".into()],
        }
    );

    let missing = closed(session(Harness::Claude, "S1", "/nonexistent/x", OLD));
    match open_entry(&missing, None, &adapters(), &LocalFacts, false, NOW) {
        OpenOutcome::Failed(msg) => assert_eq!(msg, "directory no longer exists: /nonexistent/x"),
        other => panic!("expected Failed, got {other:?}"),
    }

    let recent = closed(session(Harness::Claude, "S1", &cwd, NOW - 30));
    assert_eq!(
        open_entry(&recent, None, &adapters(), &LocalFacts, false, NOW),
        OpenOutcome::NeedsConfirm(CONFIRM_MSG.to_string())
    );
    assert!(matches!(
        open_entry(&recent, None, &adapters(), &LocalFacts, true, NOW),
        OpenOutcome::RunHere { .. }
    ));
}

#[test]
fn pi_uses_pi_resume_args() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    let host = FakeHost::new(vec![pane("p1", "w1", &cwd)]);
    let entry = closed(session(Harness::Pi, "ID", &cwd, OLD));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, false, NOW);

    assert_eq!(outcome, OpenOutcome::Resumed);
    assert!(
        host.calls()
            .contains(&"start_agent pi-id pi pnew --session ID".to_string())
    );
}

#[test]
fn pick_workspace_exact_beats_prefix() {
    let live = vec![
        pane("p1", "w1", "/home/u/dev/a/b/c"),
        pane("p2", "w1", "/home/u/dev/a/b/d"),
        pane("p3", "w2", "/home/u/dev/a"),
    ];
    assert_eq!(pick_workspace("/home/u/dev/a", &live), Some("w2".into()));
}

#[test]
fn pick_workspace_most_exact_panes_then_lowest_id() {
    let live = vec![
        pane("p1", "w5", "/home/u/p"),
        pane("p2", "w3", "/home/u/p"),
        pane("p3", "w4", "/home/u/p"),
        pane("p4", "w4", "/home/u/p"),
    ];
    assert_eq!(pick_workspace("/home/u/p", &live), Some("w4".into()));
    assert_eq!(pick_workspace("/home/u/p", &live[..2]), Some("w3".into()));
}

#[test]
fn pick_workspace_longest_prefix() {
    let live = vec![
        pane("p1", "w2", "/home/u"),
        pane("p2", "w1", "/home/u/dev/a"),
    ];
    assert_eq!(pick_workspace("/home/u/dev/a/b", &live), Some("w1".into()));
}

#[test]
fn pick_workspace_prefix_needs_two_components() {
    let live = vec![pane("p1", "w1", "/home/v")];
    assert_eq!(pick_workspace("/home/u/x", &live), None);
    assert_eq!(pick_workspace("/home/v/x", &live), Some("w1".into()));
}

#[test]
fn pick_workspace_none_when_unrelated() {
    let live = vec![pane("p1", "w1", "/home/u"), pane("p2", "w2", "/home/u/dev")];
    assert_eq!(pick_workspace("/srv/x", &live), None);
}

#[test]
fn agent_names() {
    assert_eq!(
        agent_name(Harness::Claude, "0A1B2C3d-4e5f", &[]),
        "claude-0a1b2c"
    );
    assert_eq!(
        agent_name(Harness::Claude, "0a1b2c3d", &["claude-0a1b2c".into()]),
        "claude-0a1b2c-2"
    );
    assert_eq!(agent_name(Harness::Pi, "--", &[]), "pi"); // nothing usable in the id
    assert_eq!(
        agent_name(
            Harness::Pi,
            "",
            &["pi".into(), "pi-2".into(), "pi-3".into()]
        ),
        "pi-4"
    );
}

#[test]
fn invalid_session_id_fails_without_a_host_call() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempdir_str(&dir);
    for id in ["--dangerously-skip-permissions", "a;b", "$(x)", "../x", ""] {
        let host = FakeHost::new(vec![LivePane {
            focused: true,
            ..pane("p1", "w1", &cwd)
        }]);
        let entry = closed(session(Harness::Claude, id, &cwd, OLD));

        let outcome = open_entry(&entry, Some(&host), &adapters(), &LocalFacts, true, NOW);
        assert_eq!(
            outcome,
            OpenOutcome::Failed("invalid session id".to_string()),
            "{id:?}"
        );
        assert!(host.calls().is_empty(), "{id:?}: {:?}", host.calls());

        let outcome = open_entry(&entry, None, &adapters(), &LocalFacts, true, NOW);
        assert_eq!(
            outcome,
            OpenOutcome::Failed("invalid session id".to_string()),
            "{id:?}"
        );
    }
}
