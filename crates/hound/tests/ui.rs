use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::hound_core::host::LivePane;
use hound::hound_core::index::SessionDoc;
use hound::hound_core::model::{Harness, SessionKey};
use hound::hound_core::open::OpenOutcome;
use hound::hound_core::search::{Entry, Hit, Scope, Tier, build_entries};
use hound::ui::render::{display_width, relative_age, row, row_lines, truncate};
use hound::ui::{App, Effect, Exit, Input, draw, split_paste_bursts};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 24 * 3600;

fn k(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn session(id: &str, title: &str, resume_cwd: &str, updated_at: i64) -> SessionDoc {
    SessionDoc {
        key: SessionKey {
            harness: Harness::Claude,
            id: id.to_string(),
        },
        title: title.to_string(),
        cwds: vec![resume_cwd.to_string()],
        resume_cwd: resume_cwd.to_string(),
        started_at: updated_at - 3600,
        updated_at,
        summary: None,
        category: None,
        tags: Vec::new(),
        files: Vec::new(),
        enriched_at_bytes: 0,
    }
}

fn pane(session_id: &str, workspace: &str, tab: &str) -> LivePane {
    LivePane {
        pane_id: "p1".to_string(),
        tab_id: "t1".to_string(),
        workspace_id: "w1".to_string(),
        workspace_label: workspace.to_string(),
        tab_label: tab.to_string(),
        agent_name: Some("claude-abc".to_string()),
        kind: Some("claude".to_string()),
        session_id: Some(session_id.to_string()),
        title: String::new(),
        cwd: "/home/u/depot".to_string(),
        status: "working".to_string(),
        focused: false,
        scrollback: None,
    }
}

fn hit(entry: usize, snippet: Option<&str>, from_subagent: bool) -> Hit {
    Hit {
        entry,
        tier: if snippet.is_none() {
            Tier::Short
        } else if from_subagent {
            Tier::Subagent
        } else {
            Tier::Content
        },
        snippet: snippet.map(str::to_string),
        from_subagent,
    }
}

/// `n` closed sessions with one hit each.
fn closed_results(n: usize) -> (Vec<Entry>, Vec<Hit>) {
    let sessions = (0..n)
        .map(|i| {
            session(
                &format!("s{i}"),
                &format!("title {i}"),
                "/home/u/p",
                NOW - DAY,
            )
        })
        .collect();
    let entries = build_entries(sessions, Vec::new(), NOW);
    let hits = (0..n).map(|i| hit(i, None, false)).collect();
    (entries, hits)
}

fn app_with_hits(n: usize) -> App {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = closed_results(n);
    app.set_results(entries, hits);
    app
}

/// The buffer as lines, skipping the cells hidden behind wide characters.
fn text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut lines = Vec::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buf[(x, y)].symbol();
            skip = display_width(symbol).saturating_sub(1);
            line.push_str(symbol);
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn render(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| draw(f, app, NOW)).unwrap();
    text(terminal.backend().buffer())
}

/// The live "Depot alerts" entry and the closed tools-dev entry of
/// `rows_show_marker_title_location_snippet`.
fn two_rows(title: &str) -> App {
    let sessions = vec![
        session("live1", title, "/home/u/depot", NOW - 60),
        session("old1", "nginx setup", "/home/u/tools-dev", NOW - 3 * DAY),
    ];
    let entries = build_entries(sessions, vec![pane("live1", "Billing", "t8")], NOW);
    assert!(entries[0].is_live());
    assert!(!entries[1].is_live());
    let hits = vec![
        hit(0, Some("…send to the Ops Alerts Signal group…"), false),
        hit(1, Some("nginx config"), true),
    ];
    let mut app = App::new(Scope::All, true);
    app.query = "signal".to_string();
    app.set_results(entries, hits);
    app
}

#[test]
fn typing_edits_query_and_requeries() {
    let mut app = App::new(Scope::All, true);
    assert_eq!(app.on_key(k(KeyCode::Char('c'))), Effect::Requery);
    assert_eq!(app.on_key(k(KeyCode::Char('a'))), Effect::Requery);
    assert_eq!(app.query, "ca");
    assert_eq!(app.on_key(k(KeyCode::Backspace)), Effect::Requery);
    assert_eq!(app.query, "c");
    assert_eq!(app.on_key(ctrl('u')), Effect::Requery);
    assert_eq!(app.query, "");
}

#[test]
fn shifted_chars_are_typed() {
    let mut app = App::new(Scope::All, true);
    let upper = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);
    assert_eq!(app.on_key(upper), Effect::Requery);
    assert_eq!(app.query, "C");
}

#[test]
fn key_releases_are_ignored() {
    let mut app = App::new(Scope::All, true);
    let mut release = k(KeyCode::Char('x'));
    release.kind = KeyEventKind::Release;
    assert_eq!(app.on_key(release), Effect::None);
    assert_eq!(app.query, "");
}

#[test]
fn tab_cycles_scope_both_ways() {
    let mut app = App::new(Scope::Active, true);
    assert_eq!(app.on_key(k(KeyCode::Tab)), Effect::Requery);
    assert_eq!(app.scope, Scope::Old);
    assert_eq!(app.on_key(k(KeyCode::Tab)), Effect::Requery);
    assert_eq!(app.scope, Scope::All);
    assert_eq!(app.on_key(k(KeyCode::Tab)), Effect::Requery);
    assert_eq!(app.scope, Scope::Active);
    assert_eq!(app.on_key(k(KeyCode::BackTab)), Effect::Requery);
    assert_eq!(app.scope, Scope::All);
    assert_eq!(app.on_key(k(KeyCode::BackTab)), Effect::Requery);
    assert_eq!(app.scope, Scope::Old);
    assert_eq!(app.on_key(k(KeyCode::BackTab)), Effect::Requery);
    assert_eq!(app.scope, Scope::Active);
}

#[test]
fn ctrl_s_toggles_subagents() {
    let mut app = App::new(Scope::All, true);
    assert_eq!(app.on_key(ctrl('s')), Effect::Requery);
    assert!(!app.include_subagents);
    assert_eq!(app.on_key(ctrl('s')), Effect::Requery);
    assert!(app.include_subagents);
    assert_eq!(app.query, "");
}

#[test]
fn selection_moves_and_clamps() {
    let mut app = app_with_hits(3);
    for _ in 0..3 {
        assert_eq!(app.on_key(k(KeyCode::Down)), Effect::None);
    }
    assert_eq!(app.selected, 2);
    for _ in 0..5 {
        assert_eq!(app.on_key(k(KeyCode::Up)), Effect::None);
    }
    assert_eq!(app.selected, 0);

    for _ in 0..3 {
        app.on_key(ctrl('n'));
    }
    assert_eq!(app.selected, 2);
    for _ in 0..5 {
        app.on_key(ctrl('p'));
    }
    assert_eq!(app.selected, 0);
}

#[test]
fn selection_stays_zero_without_hits() {
    let mut app = App::new(Scope::All, true);
    app.on_key(k(KeyCode::Down));
    assert_eq!(app.selected, 0);
}

#[test]
fn set_results_resets_selection() {
    let mut app = app_with_hits(3);
    app.on_key(k(KeyCode::Down));
    app.on_key(k(KeyCode::Down));
    assert_eq!(app.selected, 2);
    let (entries, hits) = closed_results(3);
    app.set_results(entries, hits);
    assert_eq!(app.selected, 0);
    assert_eq!(app.hits.len(), 3);
    assert_eq!(app.entries.len(), 3);
}

#[test]
fn enter_opens_selected() {
    let mut app = app_with_hits(3);
    app.on_key(k(KeyCode::Down));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );
}

#[test]
fn enter_with_no_hits_does_nothing() {
    let mut app = App::new(Scope::All, true);
    assert_eq!(app.on_key(k(KeyCode::Enter)), Effect::None);
}

#[test]
fn confirm_flow() {
    let mut app = app_with_hits(2);
    app.ask_confirm("m".to_string());
    assert_eq!(app.status.as_deref(), Some("m"));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: true
        }
    );
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn other_key_cancels_confirm() {
    let mut app = app_with_hits(2);
    app.ask_confirm("m".to_string());
    app.on_key(k(KeyCode::Down));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );
    assert_eq!(app.status, None);
}

#[test]
fn new_results_cancel_confirm() {
    // The confirmation was for a hit that may no longer be at the same place.
    let mut app = app_with_hits(2);
    app.ask_confirm("m".to_string());
    let (entries, hits) = closed_results(2);
    app.set_results(entries, hits);
    assert_eq!(app.status, None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn esc_and_ctrl_c_close() {
    let mut app = App::new(Scope::All, true);
    assert_eq!(app.on_key(k(KeyCode::Esc)), Effect::Close);
    assert_eq!(app.on_key(ctrl('c')), Effect::Close);
}

#[test]
fn enter_is_ignored_while_opening() {
    let mut app = app_with_hits(2);
    app.begin_open();
    assert_eq!(app.on_key(k(KeyCode::Enter)), Effect::None);
    assert_eq!(app.on_outcome(OpenOutcome::Failed("boom".into())), None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn outcomes_exit_confirm_or_report() {
    let mut app = app_with_hits(2);
    app.begin_open();
    assert_eq!(app.on_outcome(OpenOutcome::Focused), Some(Exit::Done));
    app.begin_open();
    assert_eq!(app.on_outcome(OpenOutcome::Resumed), Some(Exit::Done));

    app.begin_open();
    assert_eq!(
        app.on_outcome(OpenOutcome::NeedsConfirm("sure?".into())),
        None
    );
    assert_eq!(app.status.as_deref(), Some("sure?"));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: true
        }
    );

    app.begin_open();
    assert_eq!(app.on_outcome(OpenOutcome::Failed("no dir".into())), None);
    assert_eq!(app.status.as_deref(), Some("no dir"));

    app.begin_open();
    let run_here = OpenOutcome::RunHere {
        cwd: "/home/u/p".into(),
        argv: vec!["claude".into(), "--resume".into(), "s0".into()],
    };
    assert_eq!(
        app.on_outcome(run_here),
        Some(Exit::RunHere {
            cwd: "/home/u/p".into(),
            argv: vec!["claude".into(), "--resume".into(), "s0".into()],
        })
    );
}

#[test]
fn header_shows_scope_and_subagent_state() {
    let app = App::new(Scope::Old, false);
    let out = render(&app, 80, 12);
    assert!(out.contains("[Old]"), "{out}");
    assert!(out.contains("subagents: off"), "{out}");
    assert!(!out.contains("[Active]"), "{out}");

    let app = App::new(Scope::Active, true);
    let out = render(&app, 80, 12);
    assert!(out.contains("[Active]"), "{out}");
    assert!(out.contains("subagents: on"), "{out}");
}

#[test]
fn header_shows_session_count() {
    let mut app = App::new(Scope::All, true);
    app.total_sessions = 44;
    let out = render(&app, 80, 12);
    assert!(out.lines().next().unwrap().contains("44 sessions"), "{out}");
}

#[test]
fn rows_show_marker_title_location_snippet() {
    let app = two_rows("Depot alerts");
    let out = render(&app, 80, 12);
    for needle in [
        "●",
        "Depot alerts",
        "Billing › t8",
        "Signal group",
        "○",
        "3d",
        "tools-dev",
        "[subagent] nginx config",
    ] {
        assert!(out.contains(needle), "missing {needle:?} in\n{out}");
    }
}

#[test]
fn query_is_shown() {
    let app = two_rows("Depot alerts");
    let out = render(&app, 80, 12);
    assert!(out.contains("> signal"), "{out}");
}

#[test]
fn running_elsewhere_marker() {
    let entries = build_entries(
        vec![session("s1", "busy", "/home/u/p", NOW - 30)],
        Vec::new(),
        NOW,
    );
    let mut app = App::new(Scope::All, true);
    app.set_results(entries, vec![hit(0, None, false)]);
    let out = render(&app, 80, 12);
    assert!(out.contains("may be running elsewhere"), "{out}");
}

#[test]
fn footer_shows_progress_then_status() {
    let mut app = App::new(Scope::All, true);
    app.progress = Some((3, 40));
    let out = render(&app, 80, 12);
    assert!(
        out.lines().last().unwrap().contains("indexing 3/40"),
        "{out}"
    );

    app.status = Some("x".to_string());
    let out = render(&app, 80, 12);
    let footer = out.lines().last().unwrap();
    assert!(footer.contains(" x "), "{out}");
    assert!(!out.contains("indexing"), "{out}");
}

#[test]
fn long_status_stays_on_one_line() {
    let mut app = app_with_hits(1);
    app.status = Some(format!("failed: {}\nsecond line", "y".repeat(200)));
    let out = render(&app, 80, 12);
    let footer = out.lines().last().unwrap();
    assert!(footer.contains("failed: yyy"), "{out}");
    assert!(footer.contains('…'), "{out}");
    assert_eq!(display_width(footer), 80);
    assert!(!out.contains("second line"), "{out}");
}

#[test]
fn long_title_is_truncated_not_wrapped() {
    let title = "t".repeat(300);
    let short = render(&two_rows("Depot alerts"), 60, 12);
    let long = render(&two_rows(&title), 60, 12);
    for line in long.lines() {
        assert!(display_width(line) <= 60, "{line:?}");
    }
    // Same rows on the same lines.
    let row_lines = |out: &str| -> Vec<usize> {
        out.lines()
            .enumerate()
            .filter(|(_, l)| l.contains('●') || l.contains('○') || l.contains("nginx config"))
            .map(|(i, _)| i)
            .collect()
    };
    assert_eq!(row_lines(&short), row_lines(&long));
    let first = long.lines().find(|l| l.contains('●')).unwrap();
    assert!(first.contains("tttt…"), "{long}");
    assert!(first.contains("Billing › t8"), "{long}");
}

#[test]
fn wide_title_is_truncated_by_display_width() {
    let title = "漢字のタイトル🦀と絵文字👍🏽".repeat(10);
    let out = render(&two_rows(&title), 40, 12);
    for line in out.lines() {
        assert_eq!(display_width(line), 40, "{line:?}");
    }
    let first = out.lines().find(|l| l.contains('●')).unwrap();
    assert!(first.contains('…'), "{out}");
    assert!(first.contains("漢字"), "{out}");
    assert!(first.contains("Billing › t8"), "{out}");
}

#[test]
fn renders_empty_and_tiny() {
    let app = App::new(Scope::All, true);
    let out = render(&app, 80, 12);
    assert!(out.contains("No sessions"), "{out}");

    let full = two_rows("Depot alerts");
    for (w, h) in [(20, 5), (5, 2), (1, 1)] {
        render(&app, w, h);
        render(&full, w, h);
    }
}

#[test]
fn selected_row_scrolls_into_view() {
    let mut app = app_with_hits(10);
    for _ in 0..9 {
        app.on_key(k(KeyCode::Down));
    }
    let out = render(&app, 80, 12);
    assert!(out.contains("title 9"), "{out}");
}

#[test]
fn relative_ages() {
    assert_eq!(relative_age(0), "now");
    assert_eq!(relative_age(59), "now");
    assert_eq!(relative_age(60), "1m");
    assert_eq!(relative_age(59 * 60), "59m");
    assert_eq!(relative_age(3600), "1h");
    assert_eq!(relative_age(23 * 3600 + 3599), "23h");
    assert_eq!(relative_age(DAY), "1d");
    assert_eq!(relative_age(13 * DAY), "13d");
    assert_eq!(relative_age(14 * DAY), "2w");
    assert_eq!(relative_age(-5), "now");
}

#[test]
fn truncate_by_display_width() {
    assert_eq!(truncate("hello", 5), "hello");
    assert_eq!(truncate("hello!", 5), "hell…");
    assert_eq!(truncate("hello", 0), "");
    // Each CJK character is two cells; never split one.
    assert_eq!(truncate("漢字漢字", 5), "漢字…");
    assert_eq!(truncate("漢字漢字", 4), "漢…");
    // Combining marks stay with their base character.
    assert_eq!(truncate("e\u{301}e\u{301}e\u{301}", 2), "e\u{301}…");
    assert_eq!(display_width("e\u{301}"), 1);
    assert_eq!(display_width("👍🏽"), 2);
}

#[test]
fn row_text_for_live_and_closed() {
    let app = two_rows("Depot alerts");
    let live = row(&app.hits[0], &app.entries[0], NOW);
    assert!(live.live);
    assert_eq!(live.harness, "claude");
    assert_eq!(live.title, "Depot alerts");
    assert_eq!(live.location, "Billing › t8");
    assert!(!live.warning);
    assert_eq!(live.detail, "…send to the Ops Alerts Signal group…");

    let closed = row(&app.hits[1], &app.entries[1], NOW);
    assert!(!closed.live);
    assert_eq!(closed.location, "3d  tools-dev");
    assert_eq!(closed.detail, "[subagent] nginx config");
}

#[test]
fn row_detail_falls_back_to_summary() {
    let mut doc = session("s1", "t", "/home/u/p", NOW - DAY);
    doc.summary = Some("Set up alerts".to_string());
    let entries = build_entries(vec![doc], Vec::new(), NOW);
    let r = row(&hit(0, None, false), &entries[0], NOW);
    assert_eq!(r.detail, "Set up alerts");
    let r = row(&hit(0, Some("snip"), false), &entries[0], NOW);
    assert_eq!(r.detail, "snip");
}

#[test]
fn row_for_sessionless_pane_and_control_chars() {
    let mut p = pane("", "Work", "shell");
    p.kind = None;
    p.session_id = None;
    p.title = "make\ttest".to_string();
    let entries = build_entries(Vec::new(), vec![p], NOW);
    let r = row(&hit(0, Some("line\x1b[31m red"), false), &entries[0], NOW);
    assert_eq!(r.harness, "shell");
    assert_eq!(r.title, "make test");
    assert_eq!(r.location, "Work › shell");
    assert!(!r.detail.contains('\x1b'));
}

#[test]
fn row_lines_fit_width() {
    let app = two_rows("Depot alerts");
    let r = row(&app.hits[0], &app.entries[0], NOW);
    let [one, two] = row_lines(&r, 50);
    let one = one.to_string();
    assert_eq!(display_width(&one), 50, "{one:?}");
    assert!(one.starts_with("● claude  Depot alerts"), "{one:?}");
    assert!(one.ends_with("Billing › t8"), "{one:?}");
    assert!(display_width(&two.to_string()) <= 50);

    let elsewhere = build_entries(
        vec![session("s1", "busy", "/home/u/p", NOW - 30)],
        Vec::new(),
        NOW,
    );
    let r = row(&hit(0, None, false), &elsewhere[0], NOW);
    assert!(r.warning);
    let [one, _] = row_lines(&r, 80);
    let one = one.to_string();
    assert!(one.contains("busy ⚠ may be running elsewhere"), "{one:?}");
    assert!(one.ends_with("now  p"), "{one:?}");

    // Too narrow for everything: never wider than asked.
    for width in 0..30 {
        let [one, two] = row_lines(&r, width);
        assert!(display_width(&one.to_string()) <= width, "{width}");
        assert!(display_width(&two.to_string()) <= width, "{width}");
    }
}

/// Closed sessions with the given IDs, one hit each, in that order.
fn results_for(ids: &[&str]) -> (Vec<Entry>, Vec<Hit>) {
    let sessions = ids
        .iter()
        .map(|id| session(id, &format!("title {id}"), "/home/u/p", NOW - DAY))
        .collect();
    let entries = build_entries(sessions, Vec::new(), NOW);
    let hits = (0..ids.len()).map(|i| hit(i, None, false)).collect();
    (entries, hits)
}

fn selected_id(app: &App) -> String {
    let entry = &app.entries[app.hits[app.selected].entry];
    entry.session.as_ref().unwrap().key.id.clone()
}

#[test]
fn background_update_keeps_selected_entry_when_hits_are_inserted_above() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b", "c"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.on_key(k(KeyCode::Down));
    assert_eq!(selected_id(&app), "c");

    let (entries, hits) = results_for(&["new", "a", "b", "c"]);
    app.update_results(entries, hits);
    assert_eq!(app.selected, 3);
    assert_eq!(selected_id(&app), "c");
}

#[test]
fn background_update_tracks_entries_not_hit_positions() {
    // Entries rebuilt in a different order than hits: identity is the session key.
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    let (entries, _) = results_for(&["b", "x", "a"]);
    let hits = vec![
        hit(2, None, false),
        hit(1, None, false),
        hit(0, None, false),
    ];
    app.update_results(entries, hits);
    assert_eq!(app.selected, 2);
    assert_eq!(selected_id(&app), "b");
}

#[test]
fn background_update_tracks_sessionless_panes_by_pane_id() {
    let mut a = pane("", "W", "t");
    a.kind = None;
    a.session_id = None;
    a.pane_id = "pa".to_string();
    let mut b = a.clone();
    b.pane_id = "pb".to_string();
    let mut app = App::new(Scope::All, true);
    let entries = build_entries(Vec::new(), vec![a.clone(), b.clone()], NOW);
    app.set_results(entries, vec![hit(0, None, false), hit(1, None, false)]);
    app.on_key(k(KeyCode::Down));

    let mut c = a.clone();
    c.pane_id = "pc".to_string();
    let entries = build_entries(Vec::new(), vec![c, a, b], NOW);
    let hits = (0..3).map(|i| hit(i, None, false)).collect();
    app.update_results(entries, hits);
    assert_eq!(app.selected, 2);
    let entry = &app.entries[app.hits[app.selected].entry];
    assert_eq!(entry.live.as_ref().unwrap().pane_id, "pb");
}

#[test]
fn background_update_removing_selected_entry_clamps_and_cancels_confirm() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b", "c"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.on_key(k(KeyCode::Down));
    app.ask_confirm("m".to_string());

    let (entries, hits) = results_for(&["a", "b"]);
    app.update_results(entries, hits);
    assert_eq!(app.selected, 1);
    assert_eq!(app.status, None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );

    let (entries, hits) = results_for(&[]);
    app.update_results(entries, hits);
    assert_eq!(app.selected, 0);
    assert_eq!(app.on_key(k(KeyCode::Enter)), Effect::None);
}

#[test]
fn background_update_keeping_selected_entry_keeps_confirm() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.ask_confirm("m".to_string());

    let (entries, hits) = results_for(&["new", "a", "b"]);
    app.update_results(entries, hits);
    assert_eq!(app.status.as_deref(), Some("m"));
    assert_eq!(selected_id(&app), "b");
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 2,
            confirmed: true
        }
    );
}

#[test]
fn set_results_still_resets_after_background_updates() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b", "c"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    let (entries, hits) = results_for(&["new", "a", "b", "c"]);
    app.update_results(entries, hits);
    assert_eq!(app.selected, 2);
    let (entries, hits) = results_for(&["new", "a", "b", "c"]);
    app.set_results(entries, hits);
    assert_eq!(app.selected, 0);
}

fn needs_confirm() -> OpenOutcome {
    OpenOutcome::NeedsConfirm("may be running elsewhere".into())
}

#[test]
fn confirm_not_armed_when_selection_moved_during_open() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.begin_open();
    app.on_key(k(KeyCode::Down));
    assert_eq!(app.on_outcome(needs_confirm()), None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );
}

#[test]
fn confirm_not_armed_when_query_changed_during_open() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.begin_open();
    assert_eq!(app.on_key(k(KeyCode::Char('x'))), Effect::Requery);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    assert_eq!(app.on_outcome(needs_confirm()), None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn confirm_not_armed_when_entry_removed_during_open() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.begin_open();
    let (entries, hits) = results_for(&["a"]);
    app.update_results(entries, hits);
    assert_eq!(app.on_outcome(needs_confirm()), None);
    assert_ne!(app.status.as_deref(), Some("may be running elsewhere"));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn confirm_armed_when_entry_kept_selected_during_open() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.begin_open();
    let (entries, hits) = results_for(&["new", "a", "b"]);
    app.update_results(entries, hits);
    app.on_outcome(needs_confirm());
    assert_eq!(app.status.as_deref(), Some("may be running elsewhere"));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 2,
            confirmed: true
        }
    );
}

#[test]
fn background_update_moving_selection_off_confirm_target_disarms() {
    let mut app = App::new(Scope::All, true);
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.ask_confirm("m".to_string());
    // "b" is gone; the clamp lands on "a", which was never confirmed.
    let (entries, hits) = results_for(&["a"]);
    app.update_results(entries, hits);
    assert_eq!(app.status, None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn ask_confirm_with_nothing_selected_arms_nothing() {
    let mut app = App::new(Scope::All, true);
    app.ask_confirm("m".to_string());
    assert_eq!(app.status.as_deref(), Some("m"));
    let (entries, hits) = results_for(&["a"]);
    app.update_results(entries, hits);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn close_during_open_waits_for_outcome() {
    for outcome in [
        OpenOutcome::Focused,
        OpenOutcome::Failed("x".into()),
        needs_confirm(),
        OpenOutcome::RunHere {
            cwd: "/".into(),
            argv: vec!["pi".into()],
        },
    ] {
        let mut app = app_with_hits(2);
        app.begin_open();
        assert_eq!(app.on_key(k(KeyCode::Esc)), Effect::None);
        assert!(app.status.is_some());
        assert_eq!(app.on_key(k(KeyCode::Down)), Effect::None);
        assert_eq!(app.on_outcome(outcome), Some(Exit::Done));
    }

    let mut app = app_with_hits(2);
    app.begin_open();
    assert_eq!(app.on_key(ctrl('c')), Effect::None);
    assert_eq!(app.on_outcome(needs_confirm()), Some(Exit::Done));
}

#[test]
fn second_close_during_open_quits_at_once() {
    let mut app = app_with_hits(2);
    app.begin_open();
    assert_eq!(app.on_key(k(KeyCode::Esc)), Effect::None);
    assert_eq!(app.on_key(k(KeyCode::Esc)), Effect::Close);
}

#[test]
fn lock_busy_refresh_is_retried_once_then_reloads() {
    use hound::ui::{AfterRefresh, after_refresh};
    use std::time::Duration;

    assert_eq!(
        after_refresh(true, false),
        AfterRefresh::Reload { enrich: true }
    );
    assert_eq!(
        after_refresh(true, true),
        AfterRefresh::Reload { enrich: true }
    );
    assert_eq!(
        after_refresh(false, false),
        AfterRefresh::Retry(Duration::from_millis(500))
    );
    // The retry also found the lock held: show what the other process wrote.
    assert_eq!(
        after_refresh(false, true),
        AfterRefresh::Reload { enrich: false }
    );
}

#[test]
fn pasted_text_joins_the_query_without_controls() {
    let mut app = App::new(Scope::All, true);
    app.on_key(k(KeyCode::Char('a')));
    assert_eq!(app.on_paste("depot\r\nal\u{1b}ert\t\n"), Effect::Requery);
    assert_eq!(app.query, "adepotalert");
    // Nothing usable pasted: nothing to do.
    assert_eq!(app.on_paste("\n\u{7}"), Effect::None);
    assert_eq!(app.query, "adepotalert");
}

#[test]
fn enrich_log_is_created_appended_and_capped() {
    use hound::ui::open_enrich_log;
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enrich.log");
    open_enrich_log(dir.path())
        .unwrap()
        .write_all(b"one\n")
        .unwrap();
    open_enrich_log(dir.path())
        .unwrap()
        .write_all(b"two\n")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");

    std::fs::write(&path, vec![b'x'; 1024 * 1024]).unwrap();
    open_enrich_log(dir.path())
        .unwrap()
        .write_all(b"three\n")
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024 + 6);

    std::fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    open_enrich_log(dir.path())
        .unwrap()
        .write_all(b"four\n")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "four\n");
}

// ---- Several machines ----

const HOMELAB: &str = "homelab";

/// Entries of the remote `machine`, on a clock in step with the local one.
fn remote_entries(machine: &str, sessions: Vec<SessionDoc>, live: Vec<LivePane>) -> Vec<Entry> {
    hound::hound_core::search::build_entries_on(machine, sessions, live, NOW, NOW)
}

fn machine_of(entry: &Entry) -> Option<&str> {
    entry.machine.as_deref()
}

fn row_id(entry: &Entry) -> String {
    entry.session.as_ref().unwrap().key.id.clone()
}

/// The selected row as (machine, session ID).
fn selected_row(app: &App) -> (Option<String>, String) {
    let entry = app.selected_entry().unwrap();
    (entry.machine.clone(), row_id(entry))
}

/// Remote closed sessions with the given IDs and age, one Short hit each.
fn remote_results_for(machine: &str, ids: &[&str], updated_at: i64) -> (Vec<Entry>, Vec<Hit>) {
    let sessions = ids
        .iter()
        .map(|id| session(id, &format!("remote {id}"), "/srv/p", updated_at))
        .collect();
    let entries = remote_entries(machine, sessions, Vec::new());
    let hits = (0..ids.len()).map(|i| hit(i, None, false)).collect();
    (entries, hits)
}

#[test]
fn rows_merge_local_and_remote_by_rank() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let local = build_entries(
        vec![session("loc", "local closed", "/home/u/p", NOW - 300)],
        Vec::new(),
        NOW,
    );
    app.set_results(local, vec![hit(0, None, false)]);

    let remote = remote_entries(
        HOMELAB,
        vec![
            session("rlive", "remote live", "/srv/a", NOW - DAY),
            session("rtext", "remote text", "/srv/b", NOW - 60),
        ],
        vec![pane("rlive", "Billing", "t7")],
    );
    assert!(remote[0].is_live());
    app.update_source(
        HOMELAB,
        remote,
        vec![hit(0, None, false), hit(1, Some("matched text"), false)],
    );

    let rows: Vec<(Option<&str>, String)> = app
        .rows()
        .into_iter()
        .map(|(e, _)| (machine_of(e), row_id(e)))
        .collect();
    assert_eq!(
        rows,
        vec![
            (Some(HOMELAB), "rlive".to_string()),
            (None, "loc".to_string()),
            (Some(HOMELAB), "rtext".to_string()),
        ]
    );
}

#[test]
fn same_id_on_two_machines_is_two_identities() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let local = build_entries(
        vec![session("same", "on local", "/home/u/p", NOW - DAY)],
        Vec::new(),
        NOW,
    );
    app.set_results(local, vec![hit(0, None, false)]);
    // More recent, so it ranks first.
    let remote = remote_entries(
        HOMELAB,
        vec![session("same", "on homelab", "/srv/p", NOW - 60)],
        Vec::new(),
    );
    app.update_source(HOMELAB, remote, vec![hit(0, None, false)]);
    assert_eq!(app.rows().len(), 2);
    // The selection followed the local entry below the new remote row.
    assert_eq!(app.selected, 1);
    app.on_key(k(KeyCode::Up));
    assert_eq!(
        selected_row(&app),
        (Some(HOMELAB.to_string()), "same".to_string())
    );

    app.ask_confirm("may be running elsewhere".to_string());
    app.on_key(k(KeyCode::Down));
    assert_eq!(selected_row(&app), (None, "same".to_string()));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );
    app.on_key(k(KeyCode::Up));
    assert_eq!(
        selected_row(&app),
        (Some(HOMELAB.to_string()), "same".to_string())
    );
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn confirm_stays_with_its_machine_entry() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a"]);
    app.set_results(entries, hits);
    // Two days old: below the local row.
    let (entries, hits) = remote_results_for(HOMELAB, &["r"], NOW - 2 * DAY);
    app.update_source(HOMELAB, entries, hits);
    app.on_key(k(KeyCode::Down));
    assert_eq!(
        selected_row(&app),
        (Some(HOMELAB.to_string()), "r".to_string())
    );
    app.ask_confirm("m".to_string());

    // Re-delivered with a newer row above it.
    let sessions = vec![
        session("newer", "remote newer", "/srv/p", NOW - 60),
        session("r", "remote r", "/srv/p", NOW - 2 * DAY),
    ];
    let entries = remote_entries(HOMELAB, sessions, Vec::new());
    app.update_source(
        HOMELAB,
        entries,
        vec![hit(0, None, false), hit(1, None, false)],
    );
    assert_eq!(app.selected, 2);
    assert_eq!(app.status.as_deref(), Some("m"));
    assert_eq!(
        selected_row(&app),
        (Some(HOMELAB.to_string()), "r".to_string())
    );
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 2,
            confirmed: true
        }
    );
}

#[test]
fn remote_results_arriving_keep_the_selection() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a", "b", "c"]);
    app.set_results(entries, hits);
    app.on_key(k(KeyCode::Down));
    app.on_key(k(KeyCode::Down));
    assert_eq!(selected_row(&app), (None, "c".to_string()));

    // More recent than every local row: inserted above them.
    let (entries, hits) = remote_results_for(HOMELAB, &["r1", "r2"], NOW - 60);
    app.update_source(HOMELAB, entries, hits);
    assert_eq!(app.rows().len(), 5);
    assert_eq!(app.selected, 4);
    assert_eq!(selected_row(&app), (None, "c".to_string()));
}

#[test]
fn user_requery_drops_stale_remote_rows() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a"]);
    app.set_results(entries, hits);
    let (entries, hits) = remote_results_for(HOMELAB, &["r"], NOW - 60);
    app.update_source(HOMELAB, entries, hits);
    assert_eq!(app.rows().len(), 2);

    app.clear_sources();
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    let machines: Vec<Option<&str>> = app.rows().into_iter().map(|(e, _)| machine_of(e)).collect();
    assert_eq!(machines, vec![None, None]);
    assert_eq!(app.selected, 0);

    let (entries, hits) = remote_results_for(HOMELAB, &["r"], NOW - 60);
    app.update_source(HOMELAB, entries, hits);
    assert_eq!(app.rows().len(), 3);
}

#[test]
fn clear_sources_on_a_remote_selection_clamps_and_disarms() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a"]);
    app.set_results(entries, hits);
    let (entries, hits) = remote_results_for(HOMELAB, &["r"], NOW - 2 * DAY);
    app.update_source(HOMELAB, entries, hits);
    app.on_key(k(KeyCode::Down));
    app.ask_confirm("m".to_string());
    app.clear_sources();
    assert_eq!(app.selected, 0);
    assert_eq!(app.status, None);
    assert_eq!(selected_row(&app), (None, "a".to_string()));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}

#[test]
fn removing_a_remote_entry_clamps_and_disarms() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a"]);
    app.set_results(entries, hits);
    let (entries, hits) = remote_results_for(HOMELAB, &["r1", "r2"], NOW - 2 * DAY);
    app.update_source(HOMELAB, entries, hits);
    app.on_key(k(KeyCode::Down));
    app.on_key(k(KeyCode::Down));
    assert_eq!(
        selected_row(&app),
        (Some(HOMELAB.to_string()), "r2".to_string())
    );
    app.ask_confirm("m".to_string());

    let (entries, hits) = remote_results_for(HOMELAB, &["r1"], NOW - 2 * DAY);
    app.update_source(HOMELAB, entries, hits);
    assert_eq!(app.selected, 1);
    assert_eq!(app.status, None);
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );

    // The remote's results vanish entirely.
    app.update_source(HOMELAB, Vec::new(), Vec::new());
    assert_eq!(app.selected, 0);
    assert_eq!(selected_row(&app), (None, "a".to_string()));
}

#[test]
fn outcome_for_the_same_id_on_another_machine_does_not_arm() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let local = build_entries(
        vec![session("same", "on local", "/home/u/p", NOW - DAY)],
        Vec::new(),
        NOW,
    );
    app.set_results(local, vec![hit(0, None, false)]);
    let remote = remote_entries(
        HOMELAB,
        vec![session("same", "on homelab", "/srv/p", NOW - 60)],
        Vec::new(),
    );
    app.update_source(HOMELAB, remote, vec![hit(0, None, false)]);
    app.on_key(k(KeyCode::Up));
    app.begin_open();
    app.on_key(k(KeyCode::Down));
    assert_eq!(app.on_outcome(needs_confirm()), None);
    assert_ne!(app.status.as_deref(), Some("may be running elsewhere"));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 1,
            confirmed: false
        }
    );
}

#[test]
fn open_effect_indexes_merged_rows() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let (entries, hits) = results_for(&["a", "b"]);
    app.set_results(entries, hits);
    let (entries, hits) = remote_results_for(HOMELAB, &["r"], NOW - 60);
    app.update_source(HOMELAB, entries, hits);
    assert_eq!(app.selected, 1);
    app.on_key(k(KeyCode::Up));
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
    let entry = app.selected_entry().unwrap();
    assert_eq!(entry.machine.as_deref(), Some(HOMELAB));
    assert_eq!(row_id(entry), "r");
    assert_eq!(
        app.entry_at(0).map(|e| (e.machine.clone(), row_id(e))),
        Some((Some(HOMELAB.to_string()), "r".to_string()))
    );
    assert_eq!(app.entry_at(2).map(row_id), Some("b".to_string()));
    assert!(app.entry_at(3).is_none());
}

/// Live and closed rows on this machine and on "homelab".
fn machine_rows() -> App {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let local = build_entries(
        vec![
            session("l1", "Local live", "/home/u/tools", NOW - 60),
            session("l2", "Local closed", "/home/u/tools-dev", NOW - 3 * DAY),
        ],
        vec![pane("l1", "Tools", "t1")],
        NOW,
    );
    app.set_results(local, vec![hit(0, None, false), hit(1, None, false)]);
    let remote = remote_entries(
        HOMELAB,
        vec![
            session("r1", "Remote live", "/srv/billing", NOW - 30),
            session("r2", "Remote closed", "/srv/tools-dev", NOW - 3 * DAY - 60),
        ],
        vec![pane("r1", "Billing", "t7")],
    );
    app.update_source(
        HOMELAB,
        remote,
        vec![hit(0, None, false), hit(1, None, false)],
    );
    app
}

#[test]
fn rows_name_machines_when_remotes_configured() {
    let mut app = machine_rows();
    app.set_note(HOMELAB, Some("unreachable".to_string()));
    let out = render(&app, 80, 12);
    for needle in [
        "homelab › Billing › t7",
        "local › Tools › t1",
        "3d  homelab · tools-dev",
        "3d  local · tools-dev",
    ] {
        assert!(out.contains(needle), "missing {needle:?} in\n{out}");
    }
    assert!(
        out.lines().last().unwrap().contains("homelab: unreachable"),
        "{out}"
    );
}

#[test]
fn rows_unchanged_without_remotes() {
    let app = two_rows("Depot alerts");
    assert!(!app.remotes_configured);
    let out = render(&app, 80, 12);
    assert!(out.contains("Billing › t8"), "{out}");
    assert!(!out.contains("local"), "{out}");
}

#[test]
fn footer_shows_machine_notes() {
    let mut app = machine_rows();
    app.set_note(HOMELAB, Some("unreachable".to_string()));
    let footer = |app: &App| render(app, 80, 12).lines().last().unwrap().to_string();
    assert!(
        footer(&app).contains("homelab: unreachable"),
        "{}",
        footer(&app)
    );

    // Stable order, joined with two spaces, before progress.
    app.set_note("box", Some("timed out".to_string()));
    app.set_note(HOMELAB, Some("unreachable".to_string()));
    app.progress = Some((3, 40));
    assert!(
        footer(&app).contains("homelab: unreachable  box: timed out  indexing 3/40"),
        "{}",
        footer(&app)
    );

    app.set_note(HOMELAB, None);
    app.set_note("box", None);
    assert!(!footer(&app).contains("homelab:"), "{}", footer(&app));
    assert!(!footer(&app).contains("box:"), "{}", footer(&app));

    app.set_note(HOMELAB, Some("unreachable".to_string()));
    app.status = Some("x".to_string());
    assert!(!footer(&app).contains("unreachable"), "{}", footer(&app));
    assert!(footer(&app).contains(" x "), "{}", footer(&app));

    // clear_sources keeps notes.
    app.status = None;
    app.clear_sources();
    assert!(
        footer(&app).contains("homelab: unreachable"),
        "{}",
        footer(&app)
    );
}

#[test]
fn machine_label_and_note_control_chars_are_flattened() {
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let label = "evil\x1b[31m\nbox";
    let (entries, hits) = remote_results_for(label, &["r"], NOW - 3 * DAY);
    app.update_source(label, entries, hits);
    app.set_note(label, Some("down\r\nnow\x07".to_string()));
    let out = render(&app, 80, 12);
    assert!(!out.contains('\x1b'), "{out:?}");
    assert!(!out.contains('\x07'), "{out:?}");
    assert!(out.contains("evil [31m box"), "{out}");
    assert!(
        out.lines()
            .last()
            .unwrap()
            .contains("evil [31m box: down  now"),
        "{out}"
    );
    assert_eq!(out.lines().count(), 12);
}

#[test]
fn long_machine_label_is_truncated_not_wrapped() {
    let label = "m".repeat(40);
    let mut app = App::new(Scope::All, true);
    app.remotes_configured = true;
    let remote = remote_entries(
        &label,
        vec![
            session("r1", "Remote live", "/srv/billing", NOW - 30),
            session("r2", "Remote closed", "/srv/tools-dev", NOW - 3 * DAY),
        ],
        vec![pane("r1", "Billing", "t7")],
    );
    app.update_source(
        &label,
        remote,
        vec![hit(0, None, false), hit(1, None, false)],
    );
    app.set_note(&label, Some("unreachable".to_string()));
    let out = render(&app, 60, 12);
    assert_eq!(out.lines().count(), 12);
    for line in out.lines() {
        assert!(display_width(line) <= 60, "{line:?}");
    }
    // Cut to 16 cells: 15 characters and an ellipsis.
    let cut = format!("{}…", "m".repeat(15));
    let live = out.lines().find(|l| l.contains('●')).unwrap();
    assert!(live.contains(&format!(" {cut} › Billing › t7")), "{out}");
    let closed = out.lines().find(|l| l.contains('○')).unwrap();
    assert!(closed.contains(&format!("3d  {cut} · tools-dev")), "{out}");
    let footer = out.lines().last().unwrap();
    assert!(footer.contains(&format!(" {cut}: unreachable")), "{out}");
    assert!(!out.contains(&label), "{out}");
}

#[test]
fn format_controls_do_not_reach_the_screen() {
    // Bidi overrides and isolates, zero-width characters, the BOM and the
    // other format controls `printable` neutralises.
    let controls = [
        '\u{202E}', '\u{200B}', '\u{200D}', '\u{2066}', '\u{FEFF}', '\u{00AD}', '\u{061C}',
        '\u{180E}', '\u{2060}', '\u{FFF9}',
    ];
    let title: String = controls.iter().map(|c| format!("x{c}")).collect();
    let sessions = vec![session(
        "old1",
        &format!("evil{title}"),
        "/home/u/\u{202E}proj",
        NOW - 3 * DAY,
    )];
    let entries = build_entries(sessions, vec![], NOW);
    let mut app = App::new(Scope::All, true);
    app.query = "evil\u{202E}".to_string();
    app.set_results(
        entries,
        vec![hit(0, Some("snip\u{202E}pet\u{200D}"), false)],
    );
    let out = render(&app, 120, 12);
    assert!(out.contains("evil"), "{out}");
    let found: Vec<String> = controls
        .iter()
        .filter(|c| out.contains(**c))
        .map(|c| format!("U+{:04X}", *c as u32))
        .collect();
    assert!(found.is_empty(), "{found:?} in {out}");
}

// Pasted input as a burst of key presses.

fn ch(c: char) -> Event {
    Event::Key(k(KeyCode::Char(c)))
}

fn enter() -> Event {
    Event::Key(k(KeyCode::Enter))
}

fn released(event: &Event) -> Event {
    let Event::Key(mut key) = event.clone() else {
        panic!("not a key: {event:?}");
    };
    key.kind = KeyEventKind::Release;
    Event::Key(key)
}

fn key_input(event: &Event) -> Input {
    let Event::Key(key) = event else {
        panic!("not a key: {event:?}");
    };
    Input::Key(*key)
}

#[test]
fn burst_with_enter_is_pasted() {
    let batch = [ch('a'), ch('b'), ch('c'), enter(), ch('d')];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc d".to_string())]
    );
}

#[test]
fn fast_typing_is_not_a_paste() {
    let batch = [ch('x'), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        batch.iter().map(key_input).collect::<Vec<_>>()
    );
    let batch = [ch('x'), ch('y'), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        batch.iter().map(key_input).collect::<Vec<_>>()
    );
}

#[test]
fn single_enter_is_a_key() {
    assert_eq!(
        split_paste_bursts(&[enter()]),
        vec![Input::Key(k(KeyCode::Enter))]
    );
}

#[test]
fn ctrl_keys_end_a_burst() {
    let batch = [ch('a'), ch('b'), ch('c'), Event::Key(ctrl('u'))];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc".to_string()), Input::Key(ctrl('u'))]
    );
}

#[test]
fn non_key_events_pass_through() {
    let batch = [Event::Resize(80, 24), ch('a')];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![
            Input::Other(Event::Resize(80, 24)),
            Input::Key(k(KeyCode::Char('a')))
        ]
    );
}

#[test]
fn pasted_burst_reaches_on_paste() {
    let mut app = app_with_hits(3);
    let effects: Vec<Effect> = split_paste_bursts(&[ch('a'), ch('b'), ch('c'), enter()])
        .iter()
        .map(|input| app.on_input(input))
        .collect();
    assert_eq!(app.query, "abc");
    assert_eq!(effects, vec![Effect::Requery]);
}

#[test]
fn key_releases_neither_break_nor_count_in_a_burst() {
    // Windows reports a release after every press.
    let presses = [ch('a'), ch('b'), ch('c'), enter(), ch('d')];
    let batch: Vec<Event> = presses
        .iter()
        .flat_map(|e| [e.clone(), released(e)])
        .collect();
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc d".to_string())]
    );
    // Releases do not make two presses a burst.
    let batch = [
        ch('x'),
        released(&ch('x')),
        ch('y'),
        released(&ch('y')),
        enter(),
    ];
    assert_eq!(
        split_paste_bursts(&batch),
        batch.iter().map(key_input).collect::<Vec<_>>()
    );
}

#[test]
fn repeats_are_not_presses_in_a_burst() {
    let mut repeat = k(KeyCode::Char('z'));
    repeat.kind = KeyEventKind::Repeat;
    let batch = [ch('a'), Event::Key(repeat), ch('b'), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        batch.iter().map(key_input).collect::<Vec<_>>()
    );
    let batch = [ch('a'), ch('b'), Event::Key(repeat), ch('c'), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc".to_string())]
    );
}

#[test]
fn shifted_and_unicode_chars_join_a_burst() {
    let upper = Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
    let batch = [upper, ch('é'), ch('-'), ch(' '), ch('漢')];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("Aé- 漢".to_string())]
    );
}

#[test]
fn alt_chars_and_other_keys_end_a_burst_in_order() {
    let alt = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
    let batch = [
        ch('a'),
        ch('b'),
        ch('c'),
        enter(),
        enter(),
        Event::Key(alt),
        enter(),
        ch('d'),
        ch('e'),
        ch('f'),
        ch('g'),
        Event::Key(k(KeyCode::Down)),
        ch('h'),
        Event::Resize(1, 1),
    ];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![
            Input::Pasted("abc".to_string()),
            Input::Key(alt),
            Input::Pasted("defg".to_string()),
            Input::Key(k(KeyCode::Down)),
            // After a pasted segment, the rest of the batch's text is pasted.
            Input::Pasted("h".to_string()),
            Input::Other(Event::Resize(1, 1)),
        ]
    );
}

#[test]
fn enters_between_lines_become_spaces() {
    let batch = [
        ch('a'),
        ch('b'),
        ch('c'),
        enter(),
        enter(),
        ch('d'),
        enter(),
    ];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc  d".to_string())]
    );
}

#[test]
fn a_non_key_event_ends_a_burst() {
    let batch = [
        ch('a'),
        ch('b'),
        ch('c'),
        Event::FocusLost,
        ch('d'),
        enter(),
    ];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![
            Input::Pasted("abc".to_string()),
            Input::Other(Event::FocusLost),
            Input::Pasted("d".to_string()),
        ]
    );
}

#[test]
fn bracketed_paste_is_pasted_unchanged() {
    let batch = [Event::Paste("a\nb".to_string()), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![
            Input::Pasted("a\nb".to_string()),
            Input::Key(k(KeyCode::Enter))
        ]
    );
}

#[test]
fn inputs_reach_the_app_in_order() {
    let mut app = app_with_hits(3);
    app.on_key(k(KeyCode::Down));
    let inputs = [
        Input::Other(Event::FocusGained),
        Input::Pasted("ab".to_string()),
        Input::Key(k(KeyCode::Enter)),
    ];
    let effects: Vec<Effect> = inputs.iter().map(|i| app.on_input(i)).collect();
    assert_eq!(
        effects,
        vec![
            Effect::None,
            Effect::Requery,
            Effect::Open {
                hit: 1,
                confirmed: false
            }
        ]
    );
    assert_eq!(app.query, "ab");
}

fn events(text: &str) -> Vec<Event> {
    text.chars()
        .map(|c| match c {
            '\n' => enter(),
            '\t' => Event::Key(k(KeyCode::Tab)),
            c => ch(c),
        })
        .collect()
}

fn feed(app: &mut App, batch: &[Event]) -> Vec<Effect> {
    split_paste_bursts(batch)
        .iter()
        .map(|input| app.on_input(input))
        .collect()
}

fn no_open(effects: &[Effect]) -> bool {
    !effects.iter().any(|e| matches!(e, Effect::Open { .. }))
}

#[test]
fn paste_starting_with_a_newline_does_not_open() {
    let batch = [enter(), ch('a'), ch('b'), ch('c')];
    // The leading Enter is dropped, not turned into a space.
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("abc".to_string())]
    );
    let mut app = app_with_hits(3);
    let effects = feed(&mut app, &batch);
    assert!(no_open(&effects), "{effects:?}");
    assert_eq!(app.query, "abc");
}

#[test]
fn paste_with_a_short_first_line_does_not_open() {
    let batch = events("ab\ncdef");
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Pasted("ab cdef".to_string())]
    );
    let mut app = app_with_hits(3);
    let effects = feed(&mut app, &batch);
    assert!(no_open(&effects), "{effects:?}");
    assert_eq!(app.query, "ab cdef");
}

#[test]
fn short_line_after_a_pasted_tab_does_not_open() {
    let batch = events("abc\tde\n");
    assert_eq!(
        split_paste_bursts(&batch),
        vec![
            Input::Pasted("abc".to_string()),
            Input::Key(k(KeyCode::Tab)),
            Input::Pasted("de".to_string()),
        ]
    );
    let mut app = app_with_hits(3);
    let effects = feed(&mut app, &batch);
    assert!(no_open(&effects), "{effects:?}");
    assert_eq!(app.scope, Scope::All.next());
    assert_eq!(app.query, "abcde");
}

#[test]
fn double_enter_in_one_batch_is_two_keys() {
    let batch = [enter(), enter()];
    assert_eq!(
        split_paste_bursts(&batch),
        vec![Input::Key(k(KeyCode::Enter)), Input::Key(k(KeyCode::Enter))]
    );
    // The first Enter opens and the open asks for confirmation; the second
    // Enter confirms.
    let inputs = split_paste_bursts(&batch);
    let mut app = app_with_hits(3);
    assert_eq!(
        app.on_input(&inputs[0]),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
    app.begin_open();
    assert_eq!(
        app.on_outcome(OpenOutcome::NeedsConfirm("sure?".to_string())),
        None
    );
    assert_eq!(
        app.on_input(&inputs[1]),
        Effect::Open {
            hit: 0,
            confirmed: true
        }
    );
}

#[test]
fn enter_press_and_release_open_once() {
    let batch = [enter(), released(&enter())];
    let mut app = app_with_hits(3);
    let effects = feed(&mut app, &batch);
    assert_eq!(
        effects,
        vec![
            Effect::Open {
                hit: 0,
                confirmed: false
            },
            Effect::None
        ]
    );
}

#[test]
fn a_burst_does_not_complete_a_confirmation() {
    let mut app = app_with_hits(3);
    app.ask_confirm("sure?".to_string());
    let effects = feed(&mut app, &events("ab\ncd\n"));
    assert!(no_open(&effects), "{effects:?}");
    // The paste disarmed the confirmation: a later Enter starts over.
    assert_eq!(
        app.on_key(k(KeyCode::Enter)),
        Effect::Open {
            hit: 0,
            confirmed: false
        }
    );
}
