mod common;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::enrich::{
    Attempt, EnrichStats, Enrichment, LlmClient, build_digest, needs_enrichment, parse_response,
    run, run_selected, run_with, user_prompt,
};
use hound_core::index::{FileState, Index, SessionDoc, TermHit, TermMode, TextSearch};
use hound_core::model::{Harness, Message, Role, SessionKey};
use hound_core::search::{Scope, Tier, build_entries, search};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const SLUG: &str = "-home-u-proj";
/// Long enough after the fixtures' last timestamp for both to be quiet.
const NOW: i64 = T1_SECS + 700;
const QUIET: i64 = 600;

const GOOD: &str =
    r#"{"summary":"Depot alert bot","category":"Billing","tags":["Signal","signal","depot"]}"#;

/// Returns queued responses in order and records every prompt it was given.
struct FakeLlm {
    responses: RefCell<VecDeque<anyhow::Result<String>>>,
    calls: RefCell<Vec<(String, String)>>,
}

impl FakeLlm {
    fn new(responses: Vec<anyhow::Result<String>>) -> FakeLlm {
        FakeLlm {
            responses: RefCell::new(responses.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn good(n: usize) -> FakeLlm {
        FakeLlm::new((0..n).map(|_| Ok(GOOD.to_string())).collect())
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }
}

impl LlmClient for FakeLlm {
    fn complete(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.calls
            .borrow_mut()
            .push((system.to_string(), user.to_string()));
        self.responses
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no response queued")))
    }
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        id: id.to_string(),
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn categories() -> Vec<String> {
    strings(&["Billing", "Travel", "Other"])
}

// ---- needs_enrichment ----

fn doc(updated_at: i64, total: u64, enriched_at_bytes: u64) -> SessionDoc {
    SessionDoc {
        key: key(S1),
        title: "t".to_string(),
        cwds: Vec::new(),
        resume_cwd: String::new(),
        started_at: 0,
        updated_at,
        summary: None,
        category: None,
        tags: Vec::new(),
        files: vec![FileState {
            path: PathBuf::from("/x.jsonl"),
            subagent: false,
            size: total,
            offset: total,
            head_hash: 0,
        }],
        enriched_at_bytes,
    }
}

#[test]
fn needs_enrichment_rules() {
    let now = 10_000;
    assert!(needs_enrichment(&doc(now - 700, 500, 0), now, QUIET));
    assert!(!needs_enrichment(&doc(now - 100, 500, 0), now, QUIET));
    assert!(!needs_enrichment(&doc(now - 700, 1400, 1000), now, QUIET));
    assert!(needs_enrichment(&doc(now - 700, 1500, 1000), now, QUIET));
    // Growth alone is not enough while the session is still active.
    assert!(!needs_enrichment(&doc(now - 100, 1500, 1000), now, QUIET));
}

// ---- build_digest ----

fn msg(role: Role, text: &str, subagent: bool) -> Message {
    Message {
        role,
        text: text.to_string(),
        subagent,
        ts: None,
    }
}

#[test]
fn digest_takes_head_and_tail() {
    let mut messages = Vec::new();
    for i in 1..=30 {
        let role = if i % 2 == 1 {
            Role::User
        } else {
            Role::Assistant
        };
        messages.push(msg(role, &format!("msg-{i:02}"), false));
        if i == 25 {
            messages.push(msg(Role::Assistant, "sidechain chatter", true));
            messages.push(msg(Role::Tool, "tool output here", false));
        }
    }
    messages.push(msg(Role::Tool, "trailing tool output", false));
    messages.push(msg(Role::User, "trailing subagent prompt", true));

    let digest = build_digest(&messages, 6000);
    // User messages 1-3 are messages 1, 3 and 5.
    for i in [1, 3, 5] {
        assert!(digest.contains(&format!("user: msg-{i:02}")), "{digest}");
    }
    for i in 21..=30 {
        assert!(digest.contains(&format!("msg-{i:02}")), "{digest}");
    }
    assert!(digest.contains("assistant: msg-30"), "{digest}");
    assert!(!digest.contains("msg-15"), "{digest}");
    assert!(!digest.contains("msg-07"), "{digest}");
    assert!(!digest.contains("sidechain"), "{digest}");
    assert!(!digest.contains("tool output"), "{digest}");
    assert!(!digest.contains("subagent prompt"), "{digest}");
    // In order, each once.
    let first = digest.find("msg-01").unwrap();
    let fifth = digest.find("msg-05").unwrap();
    let last = digest.find("msg-30").unwrap();
    assert!(first < fifth && fifth < last, "{digest}");
    assert_eq!(digest.matches("msg-21").count(), 1, "{digest}");
}

#[test]
fn digest_deduplicates_overlap() {
    let messages = vec![
        msg(Role::User, "only-user", false),
        msg(Role::Assistant, "only-reply", false),
    ];
    let digest = build_digest(&messages, 6000);
    assert_eq!(digest.matches("only-user").count(), 1, "{digest}");
    assert_eq!(digest.matches("only-reply").count(), 1, "{digest}");
}

#[test]
fn digest_is_bounded() {
    let messages: Vec<Message> = (0..50)
        .map(|i| {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            msg(role, &"x".repeat(5000), false)
        })
        .collect();
    let digest = build_digest(&messages, 6000);
    assert!(digest.chars().count() <= 6000, "{}", digest.chars().count());
    for line in digest.lines() {
        let text = line.split_once(": ").map_or(line, |(_, t)| t);
        assert!(text.chars().count() <= 500, "{}", text.chars().count());
    }
    // Multibyte text is cut on character boundaries.
    let wide = vec![msg(Role::User, &"é".repeat(5000), false)];
    let digest = build_digest(&wide, 300);
    assert!(digest.chars().count() <= 300);
}

// ---- parse_response ----

fn expected() -> Enrichment {
    Enrichment {
        summary: "Depot alert bot".to_string(),
        category: "Billing".to_string(),
        tags: strings(&["signal", "depot"]),
    }
}

#[test]
fn parses_plain_fenced_and_wrapped_json() {
    let cats = categories();
    let fenced = format!("```json\n{GOOD}\n```");
    let wrapped = format!("Sure! Here you go: {GOOD} Hope that helps");
    for body in [GOOD.to_string(), fenced, wrapped] {
        assert_eq!(parse_response(&body, &cats).unwrap(), expected(), "{body}");
    }
}

#[test]
fn parses_after_a_thinking_block() {
    let body = format!("<think>Maybe {{\"summary\": \"wrong\"}}?</think>\n{GOOD}");
    assert_eq!(parse_response(&body, &categories()).unwrap(), expected());
}

#[test]
fn unknown_category_becomes_other() {
    let body = r#"{"summary":"Plan a trip","category":"Gardening","tags":[]}"#;
    let e = parse_response(body, &categories()).unwrap();
    assert_eq!(e.category, "Other");
    // Missing category, and one not in a list without "Other".
    let body = r#"{"summary":"Plan a trip"}"#;
    assert_eq!(
        parse_response(body, &strings(&["Billing"]))
            .unwrap()
            .category,
        "Other"
    );
    // A known category matches regardless of case and keeps the configured spelling.
    let body = r#"{"summary":"Plan a trip","category":"travel"}"#;
    assert_eq!(
        parse_response(body, &categories()).unwrap().category,
        "Travel"
    );
}

#[test]
fn tags_capped_at_five_and_summary_at_160() {
    let long = "word ".repeat(100);
    let body = serde_json::json!({
        "summary": format!("  {long}\n\t end "),
        "category": "Billing",
        "tags": ["A", "b", "a", "C", "d", "E", "f", "g"],
    })
    .to_string();
    let e = parse_response(&body, &categories()).unwrap();
    assert!(e.summary.chars().count() <= 160, "{}", e.summary);
    assert!(!e.summary.contains("  ") && !e.summary.contains('\n'));
    assert!(e.summary.starts_with("word word"));
    assert_eq!(e.tags, strings(&["a", "b", "c", "d", "e"]));

    let body = r#"{"summary":"No tags here","category":"Billing"}"#;
    assert!(parse_response(body, &categories()).unwrap().tags.is_empty());
}

#[test]
fn tags_given_as_one_string_are_split_on_commas() {
    // Small models often return the list as one comma-separated string.
    let body =
        r#"{"summary":"Budget script","category":"Billing","tags":"CSV, pandas,,csv , budgeting"}"#;
    assert_eq!(
        parse_response(body, &categories()).unwrap().tags,
        strings(&["csv", "pandas", "budgeting"])
    );
}

#[test]
fn garbage_response_is_an_error() {
    let cats = categories();
    assert!(parse_response("I cannot help", &cats).is_err());
    assert!(parse_response("{}", &cats).is_err());
    assert!(parse_response(r#"{"summary": 3}"#, &cats).is_err());
    assert!(parse_response(r#"{"summary": "   "}"#, &cats).is_err());
    assert!(parse_response("} backwards {", &cats).is_err());
}

// ---- run ----

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ix: Index,
}

fn path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG).join(format!("{id}.jsonl"))
}

fn adapters(root: &Path) -> Vec<Box<dyn Adapter>> {
    vec![Box::new(ClaudeAdapter::new(root.to_path_buf(), true))]
}

impl Fixture {
    /// Two quiet, indexed sessions.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        write_lines(
            &path(&root, S1),
            &[
                claude_user(S1, "/home/u/proj", T0, "fix the depot alert"),
                claude_text(S1, T1, "Sent to the Signal group"),
            ],
        );
        write_lines(
            &path(&root, S2),
            &[
                claude_user(S2, "/home/u/qa", T0, "plan the qa pass"),
                claude_text(S2, T1, "Here is a checklist"),
            ],
        );
        let ix = Index::open(&dir.path().join("state")).unwrap();
        let f = Fixture {
            _dir: dir,
            root,
            ix,
        };
        f.refresh();
        f
    }

    /// A second handle on the same index, as another process would have.
    fn open_again(&self) -> Index {
        Index::open(&self._dir.path().join("state")).unwrap()
    }

    fn refresh(&self) {
        self.ix
            .refresh(&adapters(&self.root), &mut |_, _| {})
            .unwrap()
            .unwrap();
    }
}

struct NoText;

impl TextSearch for NoText {
    fn term_matches(
        &self,
        _term: &str,
        _mode: TermMode,
        _include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        Ok(HashMap::new())
    }
}

#[test]
fn run_stores_results_and_is_idempotent() {
    let f = Fixture::new();
    let llm = FakeLlm::good(2);
    let stats = run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            enriched: 2,
            ..Default::default()
        }
    );
    for id in [S1, S2] {
        let d = f.ix.session(&key(id)).unwrap().unwrap();
        assert_eq!(d.summary.as_deref(), Some("Depot alert bot"));
        assert_eq!(d.category.as_deref(), Some("Billing"));
        assert_eq!(d.tags, strings(&["signal", "depot"]));
        assert_eq!(d.enriched_at_bytes, d.total_bytes());
    }

    // Search-visible: "bot" appears only in the summary, matched as a short field.
    let entries = build_entries(f.ix.sessions().unwrap(), Vec::new(), NOW);
    let hits = search(&entries, &NoText, "bot", Scope::All, true, None);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.tier == Tier::Short));

    let again = run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    assert_eq!(
        again,
        EnrichStats {
            skipped: 2,
            ..Default::default()
        }
    );
    assert_eq!(llm.call_count(), 2);
}

#[test]
fn run_survives_failures() {
    let f = Fixture::new();
    let llm = FakeLlm::new(vec![
        Err(anyhow::anyhow!("connection refused")),
        Ok(GOOD.to_string()),
    ]);
    let stats = run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            enriched: 1,
            failed: 1,
            ..Default::default()
        }
    );
    let docs: Vec<SessionDoc> = [S1, S2]
        .iter()
        .map(|id| f.ix.session(&key(id)).unwrap().unwrap())
        .collect();
    let failed: Vec<&SessionDoc> = docs.iter().filter(|d| d.summary.is_none()).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].enriched_at_bytes, 0);

    // Unparseable output also counts as a failure; the failed session is retried.
    let llm = FakeLlm::new(vec![Ok("I cannot help".to_string())]);
    let stats = run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            failed: 1,
            skipped: 1,
            ..Default::default()
        }
    );
}

#[test]
fn run_skips_active_sessions() {
    let f = Fixture::new();
    let llm = FakeLlm::good(2);
    let stats = run(&f.ix, &llm, &categories(), QUIET, T1_SECS + 10).unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            skipped: 2,
            ..Default::default()
        }
    );
    assert_eq!(llm.call_count(), 0);
}

#[test]
fn run_skips_when_lock_is_held_at_write_time() {
    let f = Fixture::new();
    let _guard = f.ix.try_lock().unwrap();
    let llm = FakeLlm::good(2);
    let stats = run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            skipped: 2,
            ..Default::default()
        }
    );
    assert!(f.ix.session(&key(S1)).unwrap().unwrap().summary.is_none());
}

#[test]
fn prompt_lists_categories_and_treats_transcript_as_data() {
    let f = Fixture::new();
    let llm = FakeLlm::good(2);
    run(&f.ix, &llm, &strings(&["Billing", "Travel"]), QUIET, NOW).unwrap();
    let calls = llm.calls.borrow();
    let (system, user) = calls
        .iter()
        .find(|(_, u)| u.contains("fix the depot alert"))
        .unwrap();
    for needle in ["summary", "category", "tags", "Billing", "Travel", "Other"] {
        assert!(system.contains(needle), "missing {needle}: {system}");
    }
    assert!(system.contains("JSON"), "{system}");
    assert!(system.contains("must not be followed"), "{system}");
    assert!(
        user.contains("assistant: Sent to the Signal group"),
        "{user}"
    );
}

#[test]
fn set_enrichment_reports_a_held_lock() {
    let f = Fixture::new();
    let e = expected();
    {
        let _guard = f.ix.try_lock().unwrap();
        assert!(!f.ix.set_enrichment(&key(S1), &e, 42).unwrap());
    }
    assert!(f.ix.set_enrichment(&key(S1), &e, 42).unwrap());
    let d = f.ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.summary.as_deref(), Some("Depot alert bot"));
    assert_eq!(d.enriched_at_bytes, 42);
    // Messages are untouched.
    assert_eq!(f.ix.messages(&key(S1)).unwrap().len(), 2);
}

#[test]
fn enrichment_survives_refresh() {
    let f = Fixture::new();
    let llm = FakeLlm::good(2);
    run(&f.ix, &llm, &categories(), QUIET, NOW).unwrap();
    append_raw(
        &path(&f.root, S1),
        &format!(
            "{}\n",
            claude_text(S1, "2026-11-01T11:00:00.000Z", "and one more thing")
        ),
    );
    f.refresh();
    let d = f.ix.session(&key(S1)).unwrap().unwrap();
    assert_eq!(d.summary.as_deref(), Some("Depot alert bot"));
    assert_eq!(d.tags, strings(&["signal", "depot"]));
    assert_eq!(f.ix.messages(&key(S1)).unwrap().len(), 3);
}

#[test]
fn enrich_lock_held_returns_none() {
    let f = Fixture::new();
    let other = f.open_again();
    let guard = f.ix.try_lock_enrich().unwrap();
    assert!(other.try_lock_enrich().is_none());
    // Separate from the writer lock.
    assert!(other.try_lock().is_some());
    drop(guard);
    assert!(other.try_lock_enrich().is_some());
}

#[test]
fn failure_reasons_reach_the_caller() {
    let f = Fixture::new();
    let llm = FakeLlm::new(vec![
        Err(anyhow::anyhow!("connection refused")),
        Ok(r#"{"summary": 3, "note": "fix the depot alert"}"#.to_string()),
    ]);
    let mut failures: Vec<(SessionKey, String)> = Vec::new();
    let stats = run_with(&f.ix, &llm, &categories(), QUIET, NOW, &mut |k, why| {
        failures.push((k.clone(), why.to_string()))
    })
    .unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            failed: 2,
            ..Default::default()
        }
    );
    assert_eq!(failures.len(), 2);
    let reasons: Vec<&str> = failures.iter().map(|(_, r)| r.as_str()).collect();
    assert!(reasons.contains(&"connection refused"), "{reasons:?}");
    assert!(reasons.iter().any(|r| r.contains("summary")), "{reasons:?}");
    for (k, r) in &failures {
        assert!([key(S1), key(S2)].contains(k));
        assert!(!r.contains('\n'), "{r}");
        // Neither the transcript nor the reply's text leaks into the reason.
        for leaked in ["depot", "qa pass", "Signal", "checklist"] {
            assert!(!r.contains(leaked), "{r}");
        }
    }
}

#[test]
fn control_and_format_characters_are_removed() {
    let huge = "y".repeat(100_000);
    let body = serde_json::json!({
        "summary": "Fix\u{1b}[2J the\u{7} build\u{0}\u{7f} \u{202e}now\u{2066}\u{feff}",
        "category": "Billing",
        "tags": ["x\u{1b}[2Jy", "two\nwords", huge, "\u{200e}\u{200f}", "ok\u{202a}tag\u{2069}"],
    })
    .to_string();
    let e = parse_response(&body, &categories()).unwrap();
    assert_eq!(e.summary, "Fix[2J the build now");
    assert_eq!(
        e.tags,
        strings(&["x[2jy", "two words", &"y".repeat(32), "oktag"])
    );
    for t in &e.tags {
        assert!(t.chars().count() <= 32, "{t}");
        assert!(!t.chars().any(char::is_control), "{t:?}");
    }

    // A summary made only of control characters is no summary.
    let body = serde_json::json!({"summary": "\u{1b}\u{7}\u{202e}"}).to_string();
    assert!(parse_response(&body, &categories()).is_err());
}

#[test]
fn user_prompt_sends_only_the_project_directory_name() {
    let mut d = doc(0, 10, 0);
    d.title = "Depot alerts".to_string();
    d.resume_cwd = "/home/alice/dev/depot-leap".to_string();
    d.cwds = vec![d.resume_cwd.clone()];
    let prompt = user_prompt(&d, "user: hello");
    assert!(prompt.contains("depot-leap"), "{prompt}");
    assert!(!prompt.contains("/home/alice"), "{prompt}");
    assert!(!prompt.contains("alice"), "{prompt}");
    assert!(prompt.contains("Depot alerts"), "{prompt}");
    assert!(prompt.ends_with("user: hello"), "{prompt}");

    // No usable directory name: the line is left out.
    d.resume_cwd = "/".to_string();
    let prompt = user_prompt(&d, "user: hello");
    assert!(!prompt.contains("directory"), "{prompt}");
}

#[test]
fn run_selected_attempts_only_selected_sessions() {
    let f = Fixture::new();
    let llm = FakeLlm::good(2);
    let mut asked: Vec<SessionKey> = Vec::new();
    let mut attempts: Vec<(SessionKey, String)> = Vec::new();
    let stats = run_selected(
        &f.ix,
        &llm,
        &categories(),
        QUIET,
        NOW,
        &mut |k| {
            asked.push(k.clone());
            k.id == S1
        },
        &mut |k, a| attempts.push((k.clone(), format!("{a:?}"))),
    )
    .unwrap();
    assert_eq!(
        stats,
        EnrichStats {
            enriched: 1,
            skipped: 1,
            ..Default::default()
        }
    );
    asked.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(asked, vec![key(S1), key(S2)]);
    assert_eq!(llm.call_count(), 1);
    assert_eq!(attempts, vec![(key(S1), "Enriched".to_string())]);
    assert!(f.ix.session(&key(S2)).unwrap().unwrap().summary.is_none());
}

#[test]
fn run_selected_reports_failures_and_is_not_asked_about_sessions_not_due() {
    let f = Fixture::new();
    let llm = FakeLlm::new(vec![Ok("I cannot help".to_string())]);
    let mut attempts: Vec<(SessionKey, Attempt<'static>)> = Vec::new();
    let mut failed_with = String::new();
    let stats = run_selected(
        &f.ix,
        &llm,
        &categories(),
        QUIET,
        NOW,
        &mut |k| k.id == S2,
        &mut |k, a| match a {
            Attempt::Failed(why) => {
                failed_with = why.to_string();
                attempts.push((k.clone(), Attempt::Failed("")));
            }
            Attempt::Enriched => attempts.push((k.clone(), Attempt::Enriched)),
        },
    )
    .unwrap();
    assert_eq!(stats.failed, 1);
    assert_eq!(attempts, vec![(key(S2), Attempt::Failed(""))]);
    assert!(!failed_with.is_empty());

    // Nothing is quiet: the predicate is never consulted.
    let mut asked = 0;
    let stats = run_selected(
        &f.ix,
        &llm,
        &categories(),
        QUIET,
        T1_SECS + 10,
        &mut |_| {
            asked += 1;
            true
        },
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(stats.skipped, 2);
    assert_eq!(asked, 0);
}
