mod common;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::index::{Index, TermHit, TermMode, TextSearch};
use hound_core::model::{Harness, SessionKey};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const S3: &str = "33333333-3333-3333-3333-333333333333";
const S4: &str = "44444444-4444-4444-4444-444444444444";
const SLUG: &str = "-home-u-proj";

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        id: id.to_string(),
    }
}

fn main_path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG).join(format!("{id}.jsonl"))
}

fn sub_path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG)
        .join(id)
        .join("subagents")
        .join("agent-a.jsonl")
}

/// S1: main "Sent to the Signal group", "retry failure alert"; subagent
/// "investigated nginx config". S2: main "plan the qa pass via Riverbend".
/// S3: the signal match only in its subagent.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ix: Index,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        write_lines(
            &main_path(&root, S1),
            &[
                claude_user(S1, "/home/u/proj", T0, "Sent to the Signal group"),
                claude_text(S1, T1, "retry failure alert"),
            ],
        );
        write_lines(
            &sub_path(&root, S1),
            &[claude_sidechain(S1, T1, "investigated nginx config")],
        );
        write_lines(
            &main_path(&root, S2),
            &[claude_user(
                S2,
                "/home/u/qa",
                T0,
                "plan the qa pass via Riverbend",
            )],
        );
        write_lines(
            &main_path(&root, S3),
            &[claude_user(S3, "/home/u/rt", T0, "look into the runtime")],
        );
        write_lines(
            &sub_path(&root, S3),
            &[claude_sidechain(S3, T1, "signal handling in tokio")],
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

    fn refresh(&self) {
        let adapters: Vec<Box<dyn Adapter>> =
            vec![Box::new(ClaudeAdapter::new(self.root.clone(), true))];
        self.ix.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
    }

    fn search(&self, term: &str, mode: TermMode, subagents: bool) -> HashMap<SessionKey, TermHit> {
        self.ix.term_matches(term, mode, subagents).unwrap()
    }

    fn ids(&self, term: &str, mode: TermMode, subagents: bool) -> BTreeSet<String> {
        self.search(term, mode, subagents)
            .into_keys()
            .map(|k| k.id)
            .collect()
    }
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn whole_term_finds_sessions() {
    let f = Fixture::new();
    let hits = f.search("signal", TermMode::Whole, true);
    let ids: BTreeSet<String> = hits.keys().map(|k| k.id.clone()).collect();
    assert_eq!(ids, set(&[S1, S3]));
    assert!(!hits[&key(S1)].from_subagent);
    assert!(hits[&key(S3)].from_subagent);
    assert!(hits[&key(S1)].snippet.contains("Signal"));
    assert!(hits[&key(S3)].snippet.contains("signal"));
}

#[test]
fn excluding_subagents() {
    let f = Fixture::new();
    assert_eq!(f.ids("signal", TermMode::Whole, false), set(&[S1]));
    assert_eq!(f.ids("nginx", TermMode::Whole, false), set(&[]));
    let hits = f.search("nginx", TermMode::Whole, true);
    assert_eq!(hits.len(), 1);
    assert!(hits[&key(S1)].from_subagent);
    assert!(hits[&key(S1)].snippet.contains("nginx"));
}

#[test]
fn main_match_beats_subagent_match() {
    let f = Fixture::new();
    // The subagent message scores higher for "retry", but a main match wins.
    write_lines(
        &sub_path(&f.root, S1),
        &[claude_sidechain(
            S1,
            T1,
            "alert alert alert retry retry failure failure",
        )],
    );
    f.refresh();
    let hits = f.search("retry", TermMode::Whole, true);
    assert!(!hits[&key(S1)].from_subagent);
    assert_eq!(hits[&key(S1)].snippet, "retry failure alert");
}

#[test]
fn typo_tolerance_on_long_terms() {
    let f = Fixture::new();
    assert!(f.ids("signel", TermMode::Whole, true).contains(S1));
    assert!(f.ids("singal", TermMode::Whole, true).contains(S1));
    assert!(f.ids("riverbnd", TermMode::Whole, true).contains(S2));
    assert!(!f.ids("sgnl", TermMode::Whole, true).contains(S1));
    assert!(!f.ids("qx", TermMode::Whole, true).contains(S2));
    // Snippets show the word actually matched.
    assert!(
        f.search("singal", TermMode::Whole, false)[&key(S1)]
            .snippet
            .contains("Signal")
    );
}

#[test]
fn prefix_mode() {
    let f = Fixture::new();
    assert_eq!(f.ids("river", TermMode::Prefix, true), set(&[S2]));
    assert_eq!(f.ids("river", TermMode::Whole, true), set(&[]));
    assert!(
        f.search("river", TermMode::Prefix, true)[&key(S2)]
            .snippet
            .contains("Riverbend")
    );
    // Only the last token is a prefix; earlier ones are whole words.
    assert_eq!(f.ids("qa pas", TermMode::Prefix, true), set(&[S2]));
    assert_eq!(f.ids("q pass", TermMode::Prefix, true), set(&[]));
}

#[test]
fn prefix_mode_earlier_tokens_follow_whole_rules() {
    let f = Fixture::new();
    // A one-edit typo in an earlier token of 5+ characters is tolerated...
    assert_eq!(f.ids("riverbnd vi", TermMode::Prefix, true), set(&[S2]));
    assert_eq!(f.ids("rivrbend pla", TermMode::Prefix, true), set(&[S2]));
    assert_eq!(f.ids("singal gro", TermMode::Prefix, true), set(&[S1]));
    // ...shorter earlier tokens are exact...
    assert_eq!(f.ids("plam riverb", TermMode::Prefix, true), set(&[]));
    // ...but not in the last (prefix) token.
    assert_eq!(f.ids("signal grp", TermMode::Prefix, true), set(&[]));
    assert_eq!(f.ids("riverbend pln", TermMode::Prefix, true), set(&[]));
}

#[test]
fn case_insensitive() {
    let f = Fixture::new();
    assert_eq!(
        f.ids("SIGNAL", TermMode::Whole, true),
        f.ids("signal", TermMode::Whole, true)
    );
}

#[test]
fn punctuation_in_terms_is_literal() {
    let f = Fixture::new();
    for term in ["\"", "foo:bar", "-qa", "(pass", "*", "a AND b", ""] {
        assert!(
            f.ix.term_matches(term, TermMode::Whole, true).is_ok(),
            "{term:?}"
        );
        assert!(
            f.ix.term_matches(term, TermMode::Prefix, true).is_ok(),
            "{term:?}"
        );
    }
    assert_eq!(f.ids("-qa", TermMode::Whole, true), set(&[S2]));
    assert_eq!(f.ids("(pass", TermMode::Whole, true), set(&[S2]));
    assert_eq!(f.ids("\"", TermMode::Whole, true), set(&[]));
    assert_eq!(f.ids("*", TermMode::Prefix, true), set(&[]));
    assert_eq!(f.ids("", TermMode::Whole, true), set(&[]));
    // Every token is required, in the same message.
    assert_eq!(f.ids("qa:pass", TermMode::Whole, true), set(&[S2]));
    assert_eq!(f.ids("qa:signal", TermMode::Whole, true), set(&[]));
    assert_eq!(f.ids("signal AND alert", TermMode::Whole, true), set(&[]));
}

#[test]
fn snippet_is_bounded_single_line() {
    let f = Fixture::new();
    let mut text = String::new();
    while text.chars().count() < 1000 {
        text.push_str("lorem ipsum\ndolor sit amet\n\n");
    }
    text.push_str("the zebrafish\nswims\n");
    while text.chars().count() < 2000 {
        text.push_str("consectetur\nadipiscing elit ");
    }
    write_lines(
        &main_path(&f.root, S4),
        &[claude_user(S4, "/home/u/x", T0, &text)],
    );
    f.refresh();
    let hits = f.search("zebrafish", TermMode::Whole, true);
    let snippet = &hits[&key(S4)].snippet;
    assert!(snippet.chars().count() <= 120, "{snippet:?}");
    assert!(!snippet.contains('\n'), "{snippet:?}");
    assert!(snippet.contains("zebrafish"), "{snippet:?}");
}

#[test]
fn messages_roundtrip_in_order() {
    let f = Fixture::new();
    let messages = f.ix.messages(&key(S1)).unwrap();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "Sent to the Signal group",
            "retry failure alert",
            "investigated nginx config"
        ]
    );
    let flags: Vec<bool> = messages.iter().map(|m| m.subagent).collect();
    assert_eq!(flags, vec![false, false, true]);
    assert!(f.ix.messages(&key("missing")).unwrap().is_empty());
}

#[test]
fn removed_session_no_longer_matches() {
    let f = Fixture::new();
    assert_eq!(f.ids("riverbend", TermMode::Whole, true), set(&[S2]));
    fs::remove_file(main_path(&f.root, S2)).unwrap();
    f.refresh();
    assert_eq!(f.ids("riverbend", TermMode::Whole, true), set(&[]));
}

/// Times `term_matches` on a synthetic corpus of 40,000 messages over 800
/// sessions. Run with
/// `cargo test --release -p hound-core --test index_query -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing_on_40k_messages() {
    use std::time::Instant;

    const SESSIONS: usize = 800;
    const PER_SESSION: usize = 50;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let mut seed: u64 = 42;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    // A Zipf-ish vocabulary: low indexes are far more common.
    let vocab: Vec<String> = (0..20_000)
        .map(|i| {
            let syllables = ["ka", "lo", "mi", "ne", "ru", "sa", "to", "vi", "zu", "pe"];
            let mut w = String::new();
            let mut n = i + 1;
            while n > 0 {
                w.push_str(syllables[n % 10]);
                n /= 10;
            }
            w
        })
        .collect();
    for s in 0..SESSIONS {
        let sid = format!("{s:08}-0000-0000-0000-000000000000");
        let lines: Vec<String> = (0..PER_SESSION)
            .map(|m| {
                let words: Vec<&str> = (0..40)
                    .map(|_| {
                        let r = next() % 1000;
                        let idx = (r * r * r / 1000 / 1000) * 20 + next() % 20;
                        vocab[idx.min(vocab.len() - 1)].as_str()
                    })
                    .collect();
                let text = words.join(" ");
                if m % 2 == 0 {
                    claude_user(&sid, "/home/u/x", T0, &text)
                } else {
                    claude_text(&sid, T1, &text)
                }
            })
            .collect();
        write_lines(&main_path(&root, &sid), &lines);
    }
    let ix = Index::open(&dir.path().join("state")).unwrap();
    let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(ClaudeAdapter::new(root, true))];
    ix.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();

    for (term, mode) in [
        (vocab[0].as_str(), TermMode::Whole),
        (vocab[5].as_str(), TermMode::Whole),
        ("kalomine", TermMode::Whole),
        (vocab[15_000].as_str(), TermMode::Whole),
        ("ka", TermMode::Prefix),
        ("lo", TermMode::Prefix),
        ("nothingmatches", TermMode::Whole),
    ] {
        let _ = ix.term_matches(term, mode, true).unwrap();
        let runs = 20;
        let t = Instant::now();
        let mut n = 0;
        for _ in 0..runs {
            n = ix.term_matches(term, mode, true).unwrap().len();
        }
        let per = t.elapsed() / runs;
        println!("{term:>16} {mode:?}: {n:>4} sessions, {per:?} per query");
    }
}
