//! Text queries: which sessions contain a search term, with the best snippet
//! for each.

use std::collections::HashMap;

use anyhow::Context;
use tantivy::collector::{Collector, DocSetCollector, SegmentCollector};
use tantivy::columnar::{Column, StrColumn};
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TokenStream;
use tantivy::{DocAddress, DocId, Score, SegmentOrdinal, SegmentReader, TantivyDocument, Term};

use super::{Index, KIND_MESSAGE, SESSION_FIELD, SUBAGENT_FIELD};
use crate::model::{Harness, Message, Role, SessionKey};

/// Tokens of at least this many characters match within one edit in
/// [`TermMode::Whole`].
const FUZZY_MIN_CHARS: usize = 5;
/// Maximum length of [`TermHit::snippet`], in characters.
const SNIPPET_CHARS: usize = 120;
/// Context kept before the matched word in a snippet, in characters.
const SNIPPET_LEAD_CHARS: usize = 30;

/// How a search term matches indexed words.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TermMode {
    /// Whole words; words of 5+ characters also within one edit.
    Whole,
    /// Words starting with the term's last token, without typo tolerance;
    /// earlier tokens match as in [`TermMode::Whole`].
    Prefix,
}

/// A session's best-matching message for one term.
#[derive(Clone, Debug)]
pub struct TermHit {
    pub score: f32,
    /// One line of at most 120 characters around the matched word.
    pub snippet: String,
    /// True only if no main-conversation message matched.
    pub from_subagent: bool,
}

pub trait TextSearch {
    /// Every session with a message containing all of `term`'s tokens, mapped
    /// to its best match. `term` is literal text, never query syntax; a term
    /// without word tokens matches nothing.
    fn term_matches(
        &self,
        term: &str,
        mode: TermMode,
        include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>>;
}

impl TextSearch for Index {
    fn term_matches(
        &self,
        term: &str,
        mode: TermMode,
        include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        let matchers = matchers(term, mode);
        if matchers.is_empty() {
            return Ok(HashMap::new());
        }
        let f = self.fields();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = matchers
            .iter()
            .map(|m| (Occur::Must, m.query(f.text)))
            .collect();
        if !include_subagents {
            clauses.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_bool(f.subagent, true),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        let query = BooleanQuery::new(clauses);

        let searcher = self.searcher();
        let mut best = searcher.search(&query, &BestPerSession)?;
        // In storage order, so each compressed block of the doc store is
        // decompressed once rather than once per hit in it.
        best.sort_unstable_by_key(|(_, hit)| hit.doc);
        let mut out = HashMap::with_capacity(best.len());
        for (key, hit) in best {
            let doc: TantivyDocument = searcher.doc(hit.doc)?;
            let text = doc
                .get_first(f.text)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            out.insert(
                parse_key(&key)?,
                TermHit {
                    score: hit.score,
                    snippet: snippet(text, &matchers),
                    from_subagent: hit.subagent,
                },
            );
        }
        Ok(out)
    }
}

impl Index {
    /// The session's messages in file order, main file first; empty if the
    /// session is not indexed.
    pub fn messages(&self, key: &SessionKey) -> anyhow::Result<Vec<Message>> {
        let f = self.fields();
        let query = BooleanQuery::intersection(vec![
            Box::new(TermQuery::new(
                Term::from_field_text(f.kind, KIND_MESSAGE),
                IndexRecordOption::Basic,
            )),
            Box::new(TermQuery::new(
                Term::from_field_text(f.session, &key.as_string()),
                IndexRecordOption::Basic,
            )),
        ]);
        let searcher = self.searcher();
        let mut out = Vec::new();
        for addr in searcher.search(&query, &DocSetCollector)? {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let seq = doc
                .get_first(f.seq)
                .and_then(|v| v.as_u64())
                .context("message without seq")?;
            let role = doc
                .get_first(f.role)
                .and_then(|v| v.as_str())
                .and_then(parse_role)
                .context("message without a known role")?;
            out.push((
                seq,
                Message {
                    role,
                    text: doc
                        .get_first(f.text)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    subagent: doc
                        .get_first(f.subagent)
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    ts: doc.get_first(f.ts).and_then(|v| v.as_i64()),
                },
            ));
        }
        out.sort_by_key(|(seq, _)| *seq);
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" => Some(Role::Tool),
        _ => None,
    }
}

/// Inverse of [`SessionKey::as_string`].
fn parse_key(s: &str) -> anyhow::Result<SessionKey> {
    let (harness, id) = s
        .split_once(':')
        .with_context(|| format!("malformed session key {s:?} in index"))?;
    let harness =
        Harness::parse(harness).with_context(|| format!("unknown harness in session key {s:?}"))?;
    Ok(SessionKey {
        harness,
        id: id.to_string(),
    })
}

/// How one token of the search term matches indexed words.
#[derive(Debug)]
enum Matcher {
    Exact(String),
    /// Within one edit, a transposition of adjacent letters counting as one.
    Fuzzy(String),
    Prefix(String),
}

/// The term's tokens, analysed as message text is, each with how it matches.
fn matchers(term: &str, mode: TermMode) -> Vec<Matcher> {
    let mut analyzer = super::text_analyzer();
    let mut stream = analyzer.token_stream(term);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    let last = tokens.len().saturating_sub(1);
    tokens
        .into_iter()
        .enumerate()
        .map(|(i, t)| match mode {
            TermMode::Prefix if i == last => Matcher::Prefix(t),
            _ if t.chars().count() >= FUZZY_MIN_CHARS => Matcher::Fuzzy(t),
            _ => Matcher::Exact(t),
        })
        .collect()
}

impl Matcher {
    /// Matches documents containing the token. Exact occurrences are scored
    /// by BM25; fuzzy and prefix expansions add a constant score, so a
    /// message with the exact word ranks above one with only a near miss.
    fn query(&self, field: tantivy::schema::Field) -> Box<dyn Query> {
        let exact = |t: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, t),
                IndexRecordOption::WithFreqs,
            ))
        };
        match self {
            Matcher::Exact(t) => exact(t),
            Matcher::Fuzzy(t) => Box::new(BooleanQuery::union(vec![
                exact(t),
                Box::new(FuzzyTermQuery::new(
                    Term::from_field_text(field, t),
                    1,
                    true,
                )),
            ])),
            Matcher::Prefix(t) => Box::new(BooleanQuery::union(vec![
                exact(t),
                Box::new(FuzzyTermQuery::new_prefix(
                    Term::from_field_text(field, t),
                    0,
                    false,
                )),
            ])),
        }
    }

    /// Whether the analysed word `word` is one this matcher's query matches.
    fn matches(&self, word: &str) -> bool {
        match self {
            Matcher::Exact(t) => word == t,
            Matcher::Fuzzy(t) => within_one_edit(t, word),
            Matcher::Prefix(t) => word.starts_with(t.as_str()),
        }
    }
}

/// Whether `a` and `b` differ by at most one insertion, deletion,
/// substitution or transposition of adjacent characters.
fn within_one_edit(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let i = long.iter().zip(&short).take_while(|(x, y)| x == y).count();
    match long.len() - short.len() {
        0 if i == long.len() => true,
        0 => {
            long[i + 1..] == short[i + 1..]
                || (i + 1 < long.len()
                    && long[i] == short[i + 1]
                    && long[i + 1] == short[i]
                    && long[i + 2..] == short[i + 2..])
        }
        1 => long[i + 1..] == short[i..],
        _ => false,
    }
}

/// One line of at most [`SNIPPET_CHARS`] characters of `text`, starting a
/// little before the first word any matcher matches.
fn snippet(text: &str, matchers: &[Matcher]) -> String {
    let mut analyzer = super::text_analyzer();
    let mut stream = analyzer.token_stream(text);
    let mut hit = None;
    while stream.advance() {
        let token = stream.token();
        if matchers.iter().any(|m| m.matches(&token.text)) {
            hit = Some(token.offset_from);
            break;
        }
    }
    let start = hit.map_or(0, |at| snippet_start(text, at));
    one_line_prefix(&text[start..], SNIPPET_CHARS)
}

/// Byte offset up to [`SNIPPET_LEAD_CHARS`] characters before `at`, moved
/// forward past a partial word when that does not pass `at`.
fn snippet_start(text: &str, at: usize) -> usize {
    let start = text[..at]
        .char_indices()
        .rev()
        .take(SNIPPET_LEAD_CHARS)
        .last()
        .map_or(at, |(i, _)| i);
    let mid_word = text[..start]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());
    if mid_word && let Some(ws) = text[start..at].find(char::is_whitespace) {
        return start + ws;
    }
    start
}

/// The first `max` characters of `s` with whitespace runs collapsed to one
/// space, trimmed.
fn one_line_prefix(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_space = count > 0;
            continue;
        }
        let needed = if pending_space { 2 } else { 1 };
        if count + needed > max {
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        count += needed;
    }
    out
}

/// A session's best matching message so far.
#[derive(Clone, Copy, Debug)]
struct Best {
    score: Score,
    subagent: bool,
    doc: DocAddress,
}

impl Best {
    /// Main-conversation matches beat subagent ones, then higher scores win;
    /// on a tie the earlier candidate is kept.
    fn beats(&self, other: &Best) -> bool {
        (!self.subagent, self.score) > (!other.subagent, other.score)
    }
}

/// Collects every matching message, keeping the best one per session.
struct BestPerSession;

/// A segment's best message per session key, or the error resolving the keys.
type SegmentFruit = tantivy::Result<Vec<(String, Best)>>;

impl Collector for BestPerSession {
    /// Session key string and its best message.
    type Fruit = Vec<(String, Best)>;
    type Child = SegmentBest;

    fn for_segment(
        &self,
        segment_ord: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<SegmentBest> {
        let fast = segment.fast_fields();
        let session = fast.str(SESSION_FIELD)?;
        let num_terms = session.as_ref().map_or(0, |c| c.num_terms());
        Ok(SegmentBest {
            segment_ord,
            session,
            best: vec![None; num_terms],
            subagent: fast.bool(SUBAGENT_FIELD)?,
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<SegmentFruit>) -> tantivy::Result<Self::Fruit> {
        let mut merged: HashMap<String, Best> = HashMap::new();
        for fruit in fruits {
            for (key, best) in fruit? {
                match merged.get_mut(&key) {
                    Some(current) if best.beats(current) => *current = best,
                    Some(_) => {}
                    None => {
                        merged.insert(key, best);
                    }
                }
            }
        }
        Ok(merged.into_iter().collect())
    }
}

struct SegmentBest {
    segment_ord: SegmentOrdinal,
    /// `None` if no document in the segment has a session.
    session: Option<StrColumn>,
    /// Indexed by the session's term ordinal in `session`.
    best: Vec<Option<Best>>,
    subagent: Column<bool>,
}

impl SegmentCollector for SegmentBest {
    type Fruit = SegmentFruit;

    fn collect(&mut self, doc: DocId, score: Score) {
        let Some(ord) = self.session.as_ref().and_then(|c| c.ords().first(doc)) else {
            return;
        };
        let Some(slot) = self.best.get_mut(ord as usize) else {
            return;
        };
        let candidate = Best {
            score,
            subagent: self.subagent.first(doc).unwrap_or(false),
            doc: DocAddress::new(self.segment_ord, doc),
        };
        if slot.is_none_or(|current| candidate.beats(&current)) {
            *slot = Some(candidate);
        }
    }

    /// Resolves session keys only for the sessions that matched.
    fn harvest(self) -> SegmentFruit {
        let Some(column) = self.session else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (ord, best) in self.best.into_iter().enumerate() {
            let Some(best) = best else { continue };
            let mut key = String::new();
            if column.ord_to_str(ord as u64, &mut key)? {
                out.push((key, best));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_edit() {
        assert!(within_one_edit("signal", "signal"));
        assert!(within_one_edit("signel", "signal"));
        assert!(within_one_edit("singal", "signal"));
        assert!(within_one_edit("signa", "signal"));
        assert!(within_one_edit("signall", "signal"));
        assert!(!within_one_edit("sgnl", "signal"));
        assert!(!within_one_edit("sinagl", "signal"));
        assert!(!within_one_edit("snigla", "signal"));
    }

    #[test]
    fn one_line_prefix_collapses_and_bounds() {
        assert_eq!(one_line_prefix("  a\n\n b  c \n", 120), "a b c");
        assert_eq!(one_line_prefix("ab cd", 4), "ab c");
        assert_eq!(one_line_prefix("ab cd", 3), "ab");
    }

    #[test]
    fn snippet_starts_on_a_word_before_the_match() {
        let text = format!("{} needle tail", "word ".repeat(20));
        let s = snippet(&text, &[Matcher::Exact("needle".into())]);
        assert!(s.starts_with("word "), "{s:?}");
        assert!(s.contains("needle"), "{s:?}");
        assert!(s.len() < text.len());
    }
}
