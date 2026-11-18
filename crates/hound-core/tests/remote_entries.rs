use std::cell::RefCell;
use std::path::PathBuf;

use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::adapters::pi::PiAdapter;
use hound_core::host::{Host, LivePane};
use hound_core::index::{FileState, SessionDoc};
use hound_core::model::{Harness, SessionKey};
use hound_core::open::{Facts, LocalFacts, OpenOutcome, ResumeFacts, open_entry};
use hound_core::search::{Entry, Hit, Tier, build_entries, build_entries_on, merge_hits};

const CONFIRM_MSG: &str = "This session changed in the last 2 minutes and may be running elsewhere. Enter again to resume anyway.";
/// A directory that does not exist on this filesystem.
const REMOTE_CWD: &str = "/nonexistent/remote/proj";

/// Records every call; returns a fixed snapshot.
#[derive(Default)]
struct FakeHost {
    panes: Vec<LivePane>,
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
        Ok(())
    }
}

/// Answers every question with the same facts or error.
struct FixedFacts(Result<Facts, String>);

impl ResumeFacts for FixedFacts {
    fn facts(&self, _session: &SessionDoc, _local_now: i64) -> anyhow::Result<Facts> {
        self.0.clone().map_err(|e| anyhow::anyhow!(e))
    }
}

/// Fails the test if asked anything.
struct PanickingFacts;

impl ResumeFacts for PanickingFacts {
    fn facts(&self, _session: &SessionDoc, _local_now: i64) -> anyhow::Result<Facts> {
        panic!("facts must not be consulted")
    }
}

fn ok_facts(last_change: i64, now: i64) -> FixedFacts {
    FixedFacts(Ok(Facts {
        last_change,
        cwd_exists: true,
        now,
    }))
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

fn session(id: &str, title: &str, cwd: &str, updated_at: i64) -> SessionDoc {
    SessionDoc {
        key: SessionKey {
            harness: Harness::Claude,
            id: id.to_string(),
        },
        title: title.to_string(),
        cwds: vec![cwd.to_string()],
        resume_cwd: cwd.to_string(),
        started_at: updated_at.saturating_sub(10),
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

fn agent_pane(pane_id: &str, workspace_id: &str, session_id: &str) -> LivePane {
    LivePane {
        kind: Some("claude".to_string()),
        session_id: Some(session_id.to_string()),
        ..pane(pane_id, workspace_id, "/home/u/elsewhere")
    }
}

/// A closed session on "homelab", built at local `now` with the server's clock equal to it.
fn remote_closed(doc: SessionDoc, now: i64) -> Entry {
    let entry = build_entries_on("homelab", vec![doc], vec![], now, now).remove(0);
    assert!(!entry.is_live());
    entry
}

fn hit(entry: usize, tier: Tier) -> Hit {
    Hit {
        entry,
        tier,
        snippet: None,
        from_subagent: tier == Tier::Subagent,
    }
}

fn merged_titles(lists: &[(&[Entry], &[Hit])]) -> Vec<String> {
    merge_hits(lists)
        .into_iter()
        .map(|(l, h)| {
            let (entries, hits) = lists[l];
            entries[hits[h].entry].title().to_string()
        })
        .collect()
}

#[test]
fn build_entries_on_sets_machine_and_joins() {
    let entries = build_entries_on(
        "homelab",
        vec![
            session("S1", "Joined", "/srv/a", 100),
            session("S2", "Closed", "/srv/b", 90),
        ],
        vec![agent_pane("p1", "w1", "S1"), pane("p2", "w1", "/srv/c")],
        1000,
        1000,
    );
    assert_eq!(entries.len(), 3);
    for e in &entries {
        assert_eq!(e.machine.as_deref(), Some("homelab"));
    }
    assert_eq!(entries[0].title(), "Joined");
    assert!(entries[0].is_live());
    assert_eq!(entries[0].live.as_ref().unwrap().pane_id, "p1");
    assert!(!entries[1].is_live());
    assert!(entries[2].session.is_none());
    assert_eq!(entries[2].live.as_ref().unwrap().pane_id, "p2");

    let local = build_entries(vec![session("S1", "Joined", "/srv/a", 100)], vec![], 1000);
    assert_eq!(local[0].machine, None);
}

#[test]
fn clock_offset_normalises_recency() {
    let remote = build_entries_on(
        "homelab",
        vec![session("S1", "Remote", "/srv/a", 1540)],
        vec![pane("p2", "w1", "/srv/c")],
        1000,
        1600,
    );
    assert_eq!(remote[0].last_active(), 940);
    // A session-less pane is active at the local `now`, as locally.
    assert_eq!(remote[1].last_active(), 1000);

    let local = build_entries(vec![session("S1", "Local", "/srv/a", 1540)], vec![], 1000);
    assert_eq!(local[0].last_active(), 1540);
}

#[test]
fn maybe_running_elsewhere_uses_shifted_time() {
    // Updated 30 s ago on the server's clock, which is 600 s ahead.
    let remote = build_entries_on(
        "homelab",
        vec![session("S1", "Remote", "/srv/a", 1570)],
        vec![],
        1000,
        1600,
    );
    assert!(remote[0].maybe_running_elsewhere(1000));
    assert!(!remote[0].maybe_running_elsewhere(1200));
}

#[test]
fn extreme_clocks_do_not_overflow() {
    for (updated_at, now, server_now) in [
        (i64::MAX, i64::MIN, i64::MAX),
        (i64::MIN, i64::MAX, i64::MIN),
        (i64::MAX, 0, i64::MIN),
        (i64::MIN, 0, i64::MAX),
        (0, i64::MIN, i64::MIN),
    ] {
        let entries = build_entries_on(
            "homelab",
            vec![session("S1", "Extreme", "/srv/a", updated_at)],
            vec![pane("p2", "w1", "/srv/c")],
            now,
            server_now,
        );
        let _ = entries[0].last_active();
        let _ = entries[0].maybe_running_elsewhere(i64::MIN);
        let _ = entries[0].maybe_running_elsewhere(i64::MAX);
        assert_eq!(entries[1].last_active(), now);

        let hits = [hit(0, Tier::Short), hit(1, Tier::Short)];
        assert_eq!(merge_hits(&[(&entries, &hits)]).len(), 2);
    }

    let host = FakeHost::new(vec![pane("p1", "w1", REMOTE_CWD)]);
    let entry = remote_closed(session("S1", "Extreme", REMOTE_CWD, 0), 0);
    for (last_change, now) in [
        (i64::MAX, i64::MIN),
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MAX),
    ] {
        let _ = open_entry(
            &entry,
            Some(&host),
            &adapters(),
            &ok_facts(last_change, now),
            false,
            i64::MIN,
        );
    }
}

#[test]
fn merge_orders_across_lists() {
    let local = build_entries(
        vec![session("L1", "local short 900", "/srv/a", 900)],
        vec![],
        1000,
    );
    let remote = build_entries_on(
        "homelab",
        vec![
            session("R1", "remote content live", "/srv/r", 990),
            session("R2", "remote short 950", "/srv/r", 950),
            session("R3", "remote short 800", "/srv/r", 800),
        ],
        vec![agent_pane("p1", "w1", "R1")],
        1000,
        1000,
    );
    assert!(remote[0].is_live());
    let local_hits = [hit(0, Tier::Short)];
    let remote_hits = [
        hit(1, Tier::Short),
        hit(2, Tier::Short),
        hit(0, Tier::Content),
    ];

    let order = merge_hits(&[(&local, &local_hits), (&remote, &remote_hits)]);
    assert_eq!(order, vec![(1, 0), (0, 0), (1, 1), (1, 2)]);
    assert_eq!(
        merged_titles(&[(&local, &local_hits), (&remote, &remote_hits)]),
        vec![
            "remote short 950",
            "local short 900",
            "remote short 800",
            "remote content live"
        ]
    );
}

#[test]
fn merge_puts_live_before_closed_within_a_tier() {
    let local = build_entries(
        vec![session("L1", "local closed recent", "/srv/a", 999)],
        vec![],
        1000,
    );
    let remote = build_entries_on(
        "homelab",
        vec![session("R1", "remote live old", "/srv/r", 10)],
        vec![agent_pane("p1", "w1", "R1")],
        1000,
        1000,
    );
    let local_hits = [hit(0, Tier::Content)];
    let remote_hits = [hit(0, Tier::Content)];
    assert_eq!(
        merged_titles(&[(&local, &local_hits), (&remote, &remote_hits)]),
        vec!["remote live old", "local closed recent"]
    );
}

#[test]
fn merge_ties_keep_list_order() {
    let a = build_entries(vec![session("A", "same", "/srv/a", 500)], vec![], 1000);
    let b = build_entries_on(
        "homelab",
        vec![session("B", "same", "/srv/b", 500)],
        vec![],
        1000,
        1000,
    );
    let hits = [hit(0, Tier::Short)];
    assert_eq!(
        merge_hits(&[(&a, &hits), (&b, &hits)]),
        vec![(0, 0), (1, 0)]
    );
    assert_eq!(
        merge_hits(&[(&b, &hits), (&a, &hits)]),
        vec![(0, 0), (1, 0)]
    );
}

#[test]
fn merge_of_empty_and_single_lists() {
    assert!(merge_hits(&[]).is_empty());

    let empty: Vec<Entry> = Vec::new();
    let no_hits: [Hit; 0] = [];
    assert!(merge_hits(&[(&empty, &no_hits)]).is_empty());

    let entries = build_entries(
        vec![
            session("S1", "first", "/srv/a", 300),
            session("S2", "second", "/srv/a", 200),
        ],
        vec![],
        1000,
    );
    let hits = [hit(0, Tier::Short), hit(1, Tier::Content)];
    assert_eq!(merge_hits(&[(&entries, &hits)]), vec![(0, 0), (0, 1)]);
    assert_eq!(
        merge_hits(&[(&empty, &no_hits), (&entries, &hits), (&empty, &no_hits)]),
        vec![(1, 0), (1, 1)]
    );
}

#[test]
fn remote_closed_session_resumes_through_its_host() {
    assert!(!std::path::Path::new(REMOTE_CWD).exists());
    let host = FakeHost::new(vec![LivePane {
        focused: true,
        ..pane("p1", "w1", REMOTE_CWD)
    }]);
    let entry = remote_closed(session("S1", "Remote", REMOTE_CWD, 0), 10_000);

    let outcome = open_entry(
        &entry,
        Some(&host),
        &adapters(),
        &ok_facts(0, 10_000),
        false,
        10_000,
    );

    assert_eq!(outcome, OpenOutcome::Resumed);
    assert_eq!(
        host.calls(),
        vec![
            "snapshot".to_string(),
            format!("create_tab w1 {REMOTE_CWD}"),
            "start_agent claude-s1 claude pnew --resume S1".to_string(),
        ]
    );
}

#[test]
fn remote_missing_directory_fails_from_facts() {
    // The directory exists here; only the remote facts say it is gone.
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_string();
    let host = FakeHost::new(vec![pane("p1", "w1", &cwd)]);
    let entry = remote_closed(session("S1", "Remote", &cwd, 0), 10_000);
    let facts = FixedFacts(Ok(Facts {
        last_change: 0,
        cwd_exists: false,
        now: 10_000,
    }));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &facts, false, 10_000);

    match outcome {
        OpenOutcome::Failed(msg) => {
            assert!(msg.starts_with("homelab: "), "{msg}");
            assert!(msg.contains("directory no longer exists"), "{msg}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(host.create_tab_calls(), 0);
}

#[test]
fn remote_recent_change_uses_server_now() {
    let host = FakeHost::new(vec![pane("p1", "w1", REMOTE_CWD)]);
    let entry = remote_closed(session("S1", "Remote", REMOTE_CWD, 0), 500);
    let recent = ok_facts(9_950, 10_000);

    assert_eq!(
        open_entry(&entry, Some(&host), &adapters(), &recent, false, 500),
        OpenOutcome::NeedsConfirm(CONFIRM_MSG.to_string())
    );
    assert_eq!(host.create_tab_calls(), 0);
    assert_eq!(
        open_entry(&entry, Some(&host), &adapters(), &recent, true, 500),
        OpenOutcome::Resumed
    );

    let host = FakeHost::new(vec![pane("p1", "w1", REMOTE_CWD)]);
    assert_eq!(
        open_entry(
            &entry,
            Some(&host),
            &adapters(),
            &ok_facts(9_000, 10_000),
            false,
            9_010
        ),
        OpenOutcome::Resumed
    );
}

#[test]
fn facts_error_stops_the_resume() {
    let host = FakeHost::new(vec![pane("p1", "w1", REMOTE_CWD)]);
    let entry = remote_closed(session("S1", "Remote", REMOTE_CWD, 0), 10_000);
    let facts = FixedFacts(Err("unreachable".to_string()));

    let outcome = open_entry(&entry, Some(&host), &adapters(), &facts, false, 10_000);

    match outcome {
        OpenOutcome::Failed(msg) => {
            assert!(msg.starts_with("homelab: "), "{msg}");
            assert!(msg.contains("unreachable"), "{msg}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(host.calls(), vec!["snapshot"]);
}

#[test]
fn remote_entry_without_host_fails() {
    let live = build_entries_on(
        "homelab",
        vec![],
        vec![agent_pane("p1", "w1", "S1")],
        10_000,
        10_000,
    )
    .remove(0);
    assert!(live.is_live());
    let closed = remote_closed(session("S1", "Remote", REMOTE_CWD, 0), 10_000);

    for entry in [&live, &closed] {
        match open_entry(entry, None, &adapters(), &ok_facts(0, 10_000), true, 10_000) {
            OpenOutcome::Failed(msg) => assert!(msg.starts_with("homelab: "), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}

#[test]
fn remote_live_entry_focuses_through_its_host() {
    let p = agent_pane("p1", "w1", "S1");
    let entry = build_entries_on(
        "homelab",
        vec![session("S1", "Remote", REMOTE_CWD, 0)],
        vec![p.clone()],
        10_000,
        10_000,
    )
    .remove(0);
    let host = FakeHost::new(vec![p]);

    let outcome = open_entry(
        &entry,
        Some(&host),
        &adapters(),
        &PanickingFacts,
        false,
        10_000,
    );

    assert_eq!(outcome, OpenOutcome::Focused);
    assert_eq!(host.calls(), vec!["focus w1:p1"]);
}

#[test]
fn remote_went_live_is_focused_without_facts() {
    let host = FakeHost::new(vec![agent_pane("p5", "w3", "S1")]);
    let entry = remote_closed(session("S1", "Remote", REMOTE_CWD, 0), 10_000);

    let outcome = open_entry(
        &entry,
        Some(&host),
        &adapters(),
        &PanickingFacts,
        false,
        10_000,
    );

    assert_eq!(outcome, OpenOutcome::Focused);
    assert_eq!(host.calls(), vec!["snapshot", "focus w3:p5"]);
}

#[test]
fn local_entries_unchanged_with_local_facts() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_string();
    let entry = build_entries(vec![session("S1", "Local", &cwd, 0)], vec![], 10_000).remove(0);

    assert_eq!(
        open_entry(&entry, None, &adapters(), &LocalFacts, false, 10_000),
        OpenOutcome::RunHere {
            cwd,
            argv: vec!["claude".into(), "--resume".into(), "S1".into()],
        }
    );
}

#[test]
fn local_facts_read_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_string();
    let facts = LocalFacts
        .facts(&session("S1", "Local", &cwd, 42), 7)
        .unwrap();
    assert_eq!(facts.last_change, 42);
    assert!(facts.cwd_exists);
    assert_eq!(facts.now, 7);

    let facts = LocalFacts
        .facts(&session("S1", "Local", REMOTE_CWD, 42), 7)
        .unwrap();
    assert!(!facts.cwd_exists);
}

#[test]
fn remote_invalid_session_id_fails_without_a_host_call() {
    let host = FakeHost::new(vec![LivePane {
        focused: true,
        ..pane("p1", "w1", REMOTE_CWD)
    }]);
    let entry = remote_closed(
        session("--dangerously-skip-permissions", "Remote", REMOTE_CWD, 0),
        10_000,
    );

    let outcome = open_entry(
        &entry,
        Some(&host),
        &adapters(),
        &ok_facts(0, 10_000),
        true,
        10_000,
    );

    assert_eq!(
        outcome,
        OpenOutcome::Failed("homelab: invalid session id".to_string())
    );
    assert!(host.calls().is_empty(), "{:?}", host.calls());
}
