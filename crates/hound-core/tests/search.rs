use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use hound_core::host::LivePane;
use hound_core::index::{FileState, SessionDoc, TermHit, TermMode, TextSearch};
use hound_core::model::{Harness, SessionKey};
use hound_core::search::{Entry, Hit, Scope, Tier, build_entries, search};

const NOW: i64 = 1_000_000;

/// Text matches keyed by lowercase term; records every call.
#[derive(Default)]
struct FakeText(
    HashMap<String, HashMap<SessionKey, TermHit>>,
    RefCell<Vec<(String, TermMode)>>,
);

impl FakeText {
    fn with(mut self, term: &str, key: SessionKey, snippet: &str, from_subagent: bool) -> Self {
        self.0.entry(term.to_lowercase()).or_default().insert(
            key,
            TermHit {
                score: 1.0,
                snippet: snippet.to_string(),
                from_subagent,
            },
        );
        self
    }

    fn calls(&self) -> Vec<(String, TermMode)> {
        self.1.borrow().clone()
    }
}

impl TextSearch for FakeText {
    fn term_matches(
        &self,
        term: &str,
        mode: TermMode,
        include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        self.1.borrow_mut().push((term.to_string(), mode));
        Ok(self
            .0
            .get(&term.to_lowercase())
            .map(|hits| {
                hits.iter()
                    .filter(|(_, h)| include_subagents || !h.from_subagent)
                    .map(|(k, h)| (k.clone(), h.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }
}

struct FailingText;

impl TextSearch for FailingText {
    fn term_matches(
        &self,
        _term: &str,
        _mode: TermMode,
        _include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        anyhow::bail!("index unavailable")
    }
}

fn key(harness: Harness, id: &str) -> SessionKey {
    SessionKey {
        harness,
        id: id.to_string(),
    }
}

fn session(harness: Harness, id: &str, title: &str, updated_at: i64) -> SessionDoc {
    SessionDoc {
        key: key(harness, id),
        title: title.to_string(),
        cwds: vec!["/home/u/proj".to_string()],
        resume_cwd: "/home/u/proj".to_string(),
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

fn claude(id: &str, title: &str, updated_at: i64) -> SessionDoc {
    session(Harness::Claude, id, title, updated_at)
}

fn pane(pane_id: &str, kind: Option<&str>, session_id: Option<&str>) -> LivePane {
    LivePane {
        pane_id: pane_id.to_string(),
        tab_id: "t1".to_string(),
        workspace_id: "w1".to_string(),
        workspace_label: "Main".to_string(),
        tab_label: "work".to_string(),
        agent_name: None,
        kind: kind.map(str::to_string),
        session_id: session_id.map(str::to_string),
        title: String::new(),
        cwd: "/home/u/proj".to_string(),
        status: String::new(),
        focused: false,
        scrollback: None,
    }
}

fn shell(pane_id: &str) -> LivePane {
    pane(pane_id, None, None)
}

fn titles(entries: &[Entry], hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| entries[h.entry].title().to_string())
        .collect()
}

fn run(entries: &[Entry], text: &dyn TextSearch, query: &str) -> Vec<Hit> {
    search(entries, text, query, Scope::All, true, None)
}

#[test]
fn joins_live_pane_to_session() {
    let entries = build_entries(
        vec![claude("S1", "Claude one", 100)],
        vec![pane("p1", Some("claude"), Some("S1"))],
        NOW,
    );
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_live());
    assert_eq!(entries[0].session.as_ref().unwrap().key.id, "S1");

    let entries = build_entries(
        vec![session(Harness::Pi, "S1", "Pi one", 100)],
        vec![pane("p1", Some("claude"), Some("S1"))],
        NOW,
    );
    assert_eq!(entries.len(), 2);
    let pi = entries.iter().find(|e| e.session.is_some()).unwrap();
    assert!(!pi.is_live());
}

#[test]
fn joins_live_pane_by_main_file_path() {
    let entries = build_entries(
        vec![claude("S1", "Claude one", 100)],
        vec![pane("p1", Some("claude"), Some("/data/sessions/S1.jsonl"))],
        NOW,
    );
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_live());

    // The harness must still agree.
    let entries = build_entries(
        vec![claude("S1", "Claude one", 100)],
        vec![pane("p1", Some("pi"), Some("/data/sessions/S1.jsonl"))],
        NOW,
    );
    assert_eq!(entries.len(), 2);
}

#[test]
fn second_pane_for_same_session_gets_its_own_entry() {
    let entries = build_entries(
        vec![claude("S1", "Claude one", 100)],
        vec![
            pane("p1", Some("claude"), Some("S1")),
            pane("p2", Some("claude"), Some("S1")),
        ],
        NOW,
    );
    assert_eq!(entries.len(), 2);
    let joined = entries.iter().find(|e| e.session.is_some()).unwrap();
    assert_eq!(joined.live.as_ref().unwrap().pane_id, "p1");
    let other = entries.iter().find(|e| e.session.is_none()).unwrap();
    assert_eq!(other.live.as_ref().unwrap().pane_id, "p2");
}

#[test]
fn unmatched_become_separate_entries() {
    let entries = build_entries(
        vec![claude("S1", "One", 100), claude("S2", "Two", 200)],
        vec![shell("p1"), pane("p2", Some("claude"), Some("S1"))],
        NOW,
    );
    assert_eq!(entries.len(), 3);
    assert_eq!(entries.iter().filter(|e| e.is_live()).count(), 2);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.session.is_some() && e.live.is_some())
            .count(),
        1
    );
}

#[test]
fn scopes() {
    let entries = build_entries(
        vec![
            claude("S1", "Closed one", 500),
            claude("S2", "Live one", 100),
        ],
        vec![pane("p1", Some("claude"), Some("S2"))],
        NOW,
    );
    let text = FakeText::default();

    let active = search(&entries, &text, "", Scope::Active, true, None);
    assert_eq!(titles(&entries, &active), vec!["Live one"]);

    let old = search(&entries, &text, "", Scope::Old, true, None);
    assert_eq!(titles(&entries, &old), vec!["Closed one"]);

    let all = search(&entries, &text, "", Scope::All, true, None);
    assert_eq!(titles(&entries, &all), vec!["Live one", "Closed one"]);
    assert!(
        all.iter()
            .all(|h| h.tier == Tier::Short && h.snippet.is_none())
    );
    assert!(text.calls().is_empty());
}

#[test]
fn empty_query_orders_by_recency_within_liveness() {
    let entries = build_entries(
        vec![
            claude("A", "Three hundred", 300),
            claude("B", "One hundred", 100),
            claude("C", "Two hundred", 200),
        ],
        vec![],
        NOW,
    );
    let hits = run(&entries, &FakeText::default(), "   ");
    assert_eq!(
        titles(&entries, &hits),
        vec!["Three hundred", "Two hundred", "One hundred"]
    );
}

#[test]
fn title_match_is_short_tier() {
    let entries = build_entries(
        vec![
            claude("S1", "Depot Leap new listing alerts", 100),
            claude("S2", "Unrelated work", 200),
        ],
        vec![],
        NOW,
    );
    let hits = run(&entries, &FakeText::default(), "depot");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        entries[hits[0].entry].title(),
        "Depot Leap new listing alerts"
    );
    assert_eq!(hits[0].tier, Tier::Short);
    assert_eq!(hits[0].snippet, None);
}

#[test]
fn short_terms_need_a_substring() {
    let entries = build_entries(vec![claude("S1", "Quota config", 100)], vec![], NOW);
    let text = FakeText::default();
    // q…a appear in order but not adjacent: no subsequence matching.
    assert!(run(&entries, &text, "qa").is_empty());
    assert!(run(&entries, &text, "qta").is_empty());
    assert_eq!(run(&entries, &text, "qu").len(), 1);
    assert_eq!(run(&entries, &text, "QUO").len(), 1);
}

fn short_matches(title: &str, cwd: &str, query: &str) -> bool {
    let mut s = claude("S1", title, 100);
    s.cwds = vec![cwd.to_string()];
    let entries = build_entries(vec![s], vec![], NOW);
    let text = FakeText::default();
    let hit = run(&entries, &text, query)
        .into_iter()
        .any(|h| h.tier == Tier::Short);
    assert!(
        hit || text.calls().len() == 1,
        "a short miss consults the text index"
    );
    hit
}

#[test]
fn short_fields_match_substrings_or_near_words() {
    let title = "Depot Leap new listing alerts";
    let cwd = "/home/u/dev/tools-dev";
    // Substrings.
    assert!(!short_matches(title, cwd, "sig"));
    assert!(short_matches(title, cwd, "leap"));
    assert!(short_matches(title, cwd, "list"));
    assert!(short_matches(title, cwd, "tools"));
    assert!(short_matches(title, cwd, "dev/tools"));
    // Under 5 characters: no typo tolerance.
    assert!(!short_matches(title, cwd, "dept"));
    // 5+ characters: within one edit of a word, transposition counting as one.
    assert!(short_matches(title, cwd, "lisitng"));
    assert!(short_matches(title, cwd, "alerst"));
    assert!(short_matches(title, cwd, "depoa"));
    assert!(short_matches(title, cwd, "listings"));
    assert!(short_matches(title, cwd, "alets"));
    assert!(!short_matches(title, cwd, "depoxx"));
    assert!(!short_matches(title, cwd, "ilsitng"));
}

#[test]
fn workspace_and_tab_labels_match_live_entries() {
    let mut p = pane("p1", Some("claude"), Some("S1"));
    p.workspace_label = "Billing".to_string();
    p.tab_label = "Budget".to_string();
    let entries = build_entries(
        vec![
            claude("S1", "Quarterly review", 100),
            claude("S2", "Other", 200),
        ],
        vec![p],
        NOW,
    );
    for query in ["billing", "budget"] {
        let hits = run(&entries, &FakeText::default(), query);
        assert_eq!(hits.len(), 1, "query {query}");
        assert_eq!(entries[hits[0].entry].title(), "Quarterly review");
        assert_eq!(hits[0].tier, Tier::Short);
    }
}

#[test]
fn terms_may_split_across_title_and_content() {
    let entries = build_entries(
        vec![claude("S1", "Depot Leap new listing alerts", 100)],
        vec![],
        NOW,
    );
    let text = FakeText::default().with(
        "signal",
        key(Harness::Claude, "S1"),
        "send to the Signal group",
        false,
    );
    let hits = run(&entries, &text, "depot signal");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].tier, Tier::Content);
    assert_eq!(hits[0].snippet.as_deref(), Some("send to the Signal group"));
    assert!(!hits[0].from_subagent);
}

#[test]
fn all_terms_required() {
    let entries = build_entries(
        vec![claude("S1", "Depot Leap new listing alerts", 100)],
        vec![],
        NOW,
    );
    assert!(run(&entries, &FakeText::default(), "depot zebra").is_empty());
}

#[test]
fn last_term_is_prefix() {
    let entries = build_entries(
        vec![claude("S1", "Depot Leap new listing alerts", 100)],
        vec![],
        NOW,
    );

    let text = FakeText::default();
    run(&entries, &text, "depot sig");
    assert_eq!(text.calls(), vec![("sig".to_string(), TermMode::Prefix)]);

    let text = FakeText::default();
    run(&entries, &text, "sig depot");
    assert_eq!(text.calls(), vec![("sig".to_string(), TermMode::Whole)]);

    // When every entry short-matches "sig", no text lookup happens.
    let entries = build_entries(vec![claude("S1", "Depot Signal", 100)], vec![], NOW);
    let text = FakeText::default();
    let hits = run(&entries, &text, "sig depot");
    assert_eq!(hits.len(), 1);
    assert!(text.calls().is_empty());
}

#[test]
fn text_is_queried_at_most_once_per_term() {
    let entries = build_entries(
        (0..20)
            .map(|i| claude(&format!("S{i}"), &format!("Session {i}"), i))
            .collect(),
        vec![],
        NOW,
    );
    let text = FakeText::default();
    run(&entries, &text, "alpha beta gamma");
    // Nothing matched "alpha", so later terms need no lookup at all.
    assert_eq!(text.calls(), vec![("alpha".to_string(), TermMode::Whole)]);

    let mut text = FakeText::default();
    for i in 0..20 {
        text = text.with("alpha", key(Harness::Claude, &format!("S{i}")), "a", false);
    }
    run(&entries, &text, "alpha beta");
    assert_eq!(
        text.calls(),
        vec![
            ("alpha".to_string(), TermMode::Whole),
            ("beta".to_string(), TermMode::Prefix)
        ]
    );
}

#[test]
fn text_errors_mean_no_text_matches() {
    let entries = build_entries(
        vec![
            claude("S1", "Depot Leap", 100),
            claude("S2", "Signal setup", 200),
        ],
        vec![],
        NOW,
    );
    let hits = run(&entries, &FailingText, "signal");
    assert_eq!(titles(&entries, &hits), vec!["Signal setup"]);
}

#[test]
fn tier_ordering_beats_recency() {
    let entries = build_entries(
        vec![
            claude("OLD", "Depot old title", 100),
            claude("NEW", "Something newer", 900),
        ],
        vec![],
        NOW,
    );
    let text =
        FakeText::default().with("depot", key(Harness::Claude, "NEW"), "depot in text", false);
    let hits = run(&entries, &text, "depot");
    assert_eq!(
        titles(&entries, &hits),
        vec!["Depot old title", "Something newer"]
    );
    assert_eq!(hits[0].tier, Tier::Short);
    assert_eq!(hits[1].tier, Tier::Content);
}

#[test]
fn live_beats_closed_within_tier() {
    let entries = build_entries(
        vec![
            claude("CLOSED", "Depot closed", 900),
            claude("LIVE", "Depot live", 100),
        ],
        vec![pane("p1", Some("claude"), Some("LIVE"))],
        NOW,
    );
    let hits = run(&entries, &FakeText::default(), "depot");
    assert_eq!(titles(&entries, &hits), vec!["Depot live", "Depot closed"]);
}

#[test]
fn equal_recency_orders_by_title() {
    let entries = build_entries(
        vec![claude("B", "Bravo", 100), claude("A", "Alpha", 100)],
        vec![],
        NOW,
    );
    let hits = run(&entries, &FakeText::default(), "");
    assert_eq!(titles(&entries, &hits), vec!["Alpha", "Bravo"]);
}

#[test]
fn subagent_only_match_is_last_and_flagged() {
    let entries = build_entries(
        vec![
            claude("SUB", "First session", 900),
            claude("MAIN", "Second session", 100),
        ],
        vec![],
        NOW,
    );
    let text = FakeText::default()
        .with(
            "zebra",
            key(Harness::Claude, "SUB"),
            "zebra in subagent",
            true,
        )
        .with(
            "zebra",
            key(Harness::Claude, "MAIN"),
            "zebra in main",
            false,
        );
    let hits = run(&entries, &text, "zebra");
    assert_eq!(
        titles(&entries, &hits),
        vec!["Second session", "First session"]
    );
    assert_eq!(hits[0].tier, Tier::Content);
    assert!(!hits[0].from_subagent);
    assert_eq!(hits[1].tier, Tier::Subagent);
    assert!(hits[1].from_subagent);
    assert_eq!(hits[1].snippet.as_deref(), Some("zebra in subagent"));
}

#[test]
fn subagent_matches_hidden_when_excluded() {
    let entries = build_entries(
        vec![
            claude("SUB", "First session", 900),
            claude("MAIN", "Second session", 100),
        ],
        vec![],
        NOW,
    );
    let text = FakeText::default()
        .with(
            "zebra",
            key(Harness::Claude, "SUB"),
            "zebra in subagent",
            true,
        )
        .with(
            "zebra",
            key(Harness::Claude, "MAIN"),
            "zebra in main",
            false,
        );
    let hits = search(&entries, &text, "zebra", Scope::All, false, None);
    assert_eq!(titles(&entries, &hits), vec!["Second session"]);
}

#[test]
fn sessionless_pane_matches_scrollback() {
    let mut p = shell("p1");
    p.title = "zsh".to_string();
    p.scrollback = Some("$ cargo build\ncargo build finished\n$ ".to_string());
    let entries = build_entries(vec![], vec![p], NOW);
    let text = FakeText::default();
    let hits = run(&entries, &text, "finished");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].tier, Tier::Content);
    assert_eq!(hits[0].snippet.as_deref(), Some("cargo build finished"));
    assert!(text.calls().is_empty());
    assert!(run(&entries, &text, "absent").is_empty());
}

#[test]
fn maybe_running_elsewhere_window() {
    let entries = build_entries(
        vec![
            claude("RECENT", "Recent", NOW - 60),
            claude("STALE", "Stale", NOW - 300),
            claude("LIVE", "Live", NOW - 60),
        ],
        vec![pane("p1", Some("claude"), Some("LIVE"))],
        NOW,
    );
    let by_title = |t: &str| entries.iter().find(|e| e.title() == t).unwrap();
    assert!(by_title("Recent").maybe_running_elsewhere(NOW));
    assert!(!by_title("Stale").maybe_running_elsewhere(NOW));
    assert!(!by_title("Live").maybe_running_elsewhere(NOW));
}

#[test]
fn entry_title_and_last_active_fallbacks() {
    let mut titled = shell("p1");
    titled.title = "build pane".to_string();
    let untitled = shell("p2");
    let entries = build_entries(vec![], vec![titled, untitled], NOW);
    assert_eq!(entries[0].title(), "build pane");
    assert_eq!(entries[1].title(), "/home/u/proj");
    assert_eq!(entries[0].last_active(), NOW);
}

#[test]
fn scope_cycle() {
    assert_eq!(Scope::Active.next(), Scope::Old);
    assert_eq!(Scope::Old.next(), Scope::All);
    assert_eq!(Scope::All.next(), Scope::Active);
    assert_eq!(Scope::Active.prev(), Scope::All);
    assert_eq!(Scope::All.prev(), Scope::Old);
    assert_eq!(Scope::parse("old"), Some(Scope::Old));
    assert_eq!(Scope::parse("ALL"), Some(Scope::All));
    assert_eq!(Scope::parse("bogus"), None);
}

#[test]
fn home_directory_prefix_does_not_match() {
    let mut s = session(Harness::Pi, "S1", "Pi notes", 100);
    s.cwds = vec!["/home/u/dev/tools-dev".to_string()];
    s.resume_cwd = s.cwds[0].clone();
    let mut p = shell("p1");
    p.cwd = "/home/u/dev/tools-dev".to_string();
    let text = FakeText::default();

    for (entries, what) in [
        (build_entries(vec![s], vec![], NOW), "session cwd"),
        (build_entries(vec![], vec![p], NOW), "pane cwd"),
    ] {
        let hits = |q: &str| search(&entries, &text, q, Scope::All, true, Some("/home/u")).len();
        assert_eq!(hits("home"), 0, "{what}");
        assert_eq!(hits("u"), 0, "{what}");
        assert_eq!(hits("tools"), 1, "{what}");
        assert_eq!(hits("dev/tools"), 1, "{what}");
        // Without a home directory, the whole path is matched as before.
        assert_eq!(
            search(&entries, &text, "home", Scope::All, true, None).len(),
            1,
            "{what}"
        );
    }
}

#[test]
fn home_prefix_is_stripped_only_at_a_component_boundary() {
    let mut s = session(Harness::Pi, "S1", "Pi notes", 100);
    s.cwds = vec!["/home/user2/x".to_string()];
    let entries = build_entries(vec![s], vec![], NOW);
    let text = FakeText::default();
    assert_eq!(
        search(&entries, &text, "user2", Scope::All, true, Some("/home/u")).len(),
        1
    );
    assert_eq!(
        search(
            &entries,
            &text,
            "user2",
            Scope::All,
            true,
            Some("/home/user2/")
        )
        .len(),
        0
    );
}

/// How many entries a session in `cwd` gives for `query` with `home`.
fn cwd_hits(cwd: &str, home: &str, query: &str) -> usize {
    let mut s = session(Harness::Pi, "S1", "Pi notes", 100);
    s.cwds = vec![cwd.to_string()];
    s.resume_cwd = cwd.to_string();
    let entries = build_entries(vec![s], vec![], NOW);
    search(
        &entries,
        &FakeText::default(),
        query,
        Scope::All,
        true,
        Some(home),
    )
    .len()
}

#[test]
fn windows_home_variants() {
    // A trailing separator on home is ignored.
    let (cwd, home) = (r"C:\Users\alice\proj\depot", r"C:\Users\alice\");
    assert_eq!(cwd_hits(cwd, home, "proj"), 1);
    assert_eq!(cwd_hits(cwd, home, "depot"), 1);
    assert_eq!(cwd_hits(cwd, home, "users"), 0);
    assert_eq!(cwd_hits(cwd, home, "alice"), 0);

    // Windows paths compare case-insensitively.
    let (cwd, home) = (r"C:\Users\Alice\depot", r"c:\users\alice");
    assert_eq!(cwd_hits(cwd, home, "alice"), 0);
    assert_eq!(cwd_hits(cwd, home, "depot"), 1);

    // `/` and `\` are the same separator.
    let (cwd, home) = (r"C:\Users\alice\depot", "C:/Users/alice");
    assert_eq!(cwd_hits(cwd, home, "alice"), 0);
    assert_eq!(cwd_hits(cwd, home, "depot"), 1);

    // Stripped only at a component boundary.
    let (cwd, home) = (r"C:\Users\alice\depot", r"C:\Users\al");
    assert_eq!(cwd_hits(cwd, home, "alice"), 1);
    assert_eq!(cwd_hits(cwd, home, "users"), 1);
}

#[test]
fn windows_unc_home_is_stripped() {
    let (cwd, home) = (r"\\server\Home\alice\depot", r"\\SERVER\home\alice");
    assert_eq!(cwd_hits(cwd, home, "alice"), 0);
    assert_eq!(cwd_hits(cwd, home, "server"), 0);
    assert_eq!(cwd_hits(cwd, home, "depot"), 1);
}

#[test]
fn windows_home_strips_cwd_equal_to_home() {
    assert_eq!(cwd_hits(r"C:\Users\Alice", r"c:\users\alice\", "alice"), 0);
}

#[test]
fn unix_home_case_still_matters() {
    assert_eq!(cwd_hits("/home/Alice/x", "/home/alice", "alice"), 1);
    assert_eq!(cwd_hits("/home/Alice/x", "/home/alice", "home"), 1);
    // Unix homes do not treat `\` as a separator.
    assert_eq!(cwd_hits(r"/home/alice\x", "/home/alice", "alice"), 1);
}
