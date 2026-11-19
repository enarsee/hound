//! Merges indexed sessions with live panes into entries, and decides which
//! entries match a query and in what order. Pure logic: called on every
//! keystroke of the popup.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use crate::host::LivePane;
use crate::index::{SessionDoc, TermHit, TermMode, TextSearch};
use crate::model::SessionKey;

/// A closed session updated within this many seconds may still be running
/// somewhere hound cannot see.
pub(crate) const RECENT_SECS: i64 = 120;
/// Shortest term that may match a short-field word with one typo.
const TYPO_MIN_CHARS: usize = 5;
/// Longest scrollback snippet, in characters.
const SNIPPET_CHARS: usize = 120;

/// Which entries a search considers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Live entries only.
    Active,
    /// Entries without a live pane.
    Old,
    All,
}

impl Scope {
    /// Active → Old → All → Active.
    pub fn next(self) -> Scope {
        match self {
            Scope::Active => Scope::Old,
            Scope::Old => Scope::All,
            Scope::All => Scope::Active,
        }
    }

    pub fn prev(self) -> Scope {
        match self {
            Scope::Active => Scope::All,
            Scope::Old => Scope::Active,
            Scope::All => Scope::Old,
        }
    }

    /// `"active"`, `"old"` or `"all"`, ignoring case.
    pub fn parse(s: &str) -> Option<Scope> {
        match s.to_ascii_lowercase().as_str() {
            "active" => Some(Scope::Active),
            "old" => Some(Scope::Old),
            "all" => Some(Scope::All),
            _ => None,
        }
    }

    fn contains(self, entry: &Entry) -> bool {
        match self {
            Scope::Active => entry.is_live(),
            Scope::Old => !entry.is_live(),
            Scope::All => true,
        }
    }
}

/// An indexed session, a live pane, or a session running in a live pane.
/// Built only by [`build_entries`] and [`build_entries_on`]; at least one of
/// the two is `Some`.
#[derive(Clone, Debug)]
pub struct Entry {
    pub session: Option<SessionDoc>,
    pub live: Option<LivePane>,
    /// The remote machine the entry belongs to; `None` for this machine.
    pub machine: Option<String>,
    /// The local `now` given when the entries were built.
    built_at: i64,
    /// How far the entry's machine's clock is ahead of the local one, in
    /// seconds (saturated); 0 for this machine.
    clock_offset: i64,
}

impl Entry {
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// The session's `updated_at` expressed on the local clock (for a remote
    /// entry, shifted back by how far that machine's clock is ahead); for a
    /// session-less pane, the local time the entries were built.
    pub fn last_active(&self) -> i64 {
        self.session.as_ref().map_or(self.built_at, |s| {
            s.updated_at.saturating_sub(self.clock_offset)
        })
    }

    /// A closed session updated within the last two minutes, which may still
    /// be running outside the host.
    pub fn maybe_running_elsewhere(&self, now: i64) -> bool {
        !self.is_live()
            && self.session.as_ref().is_some()
            && now.saturating_sub(self.last_active()) < RECENT_SECS
    }

    /// The session title, else the live pane's title, else its cwd. Empty
    /// titles fall through to the next choice.
    pub fn title(&self) -> &str {
        let session = self.session.as_ref().map(|s| s.title.as_str());
        let live_title = self.live.as_ref().map(|l| l.title.as_str());
        let live_cwd = self.live.as_ref().map(|l| l.cwd.as_str());
        [session, live_title, live_cwd]
            .into_iter()
            .flatten()
            .find(|t| !t.is_empty())
            .or(session)
            .unwrap_or("")
    }

    /// Lowercased fields a term may match without the text index. Working
    /// directories lose the `home` prefix, which every one of them shares.
    fn short_fields(&self, home: Option<&str>) -> Vec<String> {
        let cwd = |c: &str| strip_home(c, home).to_lowercase();
        let mut fields = Vec::new();
        if let Some(s) = &self.session {
            fields.push(s.title.to_lowercase());
            fields.extend(s.cwds.iter().map(|c| cwd(c)));
            fields.push(s.key.harness.as_str().to_string());
            fields.extend(s.summary.iter().map(|v| v.to_lowercase()));
            fields.extend(s.category.iter().map(|v| v.to_lowercase()));
            fields.extend(s.tags.iter().map(|t| t.to_lowercase()));
        }
        if let Some(l) = &self.live {
            fields.push(l.workspace_label.to_lowercase());
            fields.push(l.tab_label.to_lowercase());
            fields.extend(l.agent_name.iter().map(|v| v.to_lowercase()));
            fields.extend(l.kind.iter().map(|v| v.to_lowercase()));
            fields.push(l.title.to_lowercase());
            fields.push(cwd(&l.cwd));
        }
        fields.retain(|f| !f.is_empty());
        fields
    }
}

/// One entry per session and one per live pane, with a pane merged into the
/// session it runs. A pane joins a session when its `kind` is the session's
/// harness name and its `session_id` is either the session ID or the path of
/// the session's main transcript. If several panes map to one session, the
/// first joins it and the rest become entries of their own.
///
/// Sessions come first in input order, then the unjoined panes in input order.
pub fn build_entries(sessions: Vec<SessionDoc>, live: Vec<LivePane>, now: i64) -> Vec<Entry> {
    build(None, sessions, live, now, 0)
}

/// [`build_entries`] for the sessions and panes of the remote `machine`, whose
/// clock read `server_now` when the local clock read `now`. The entries' times
/// are shifted onto the local clock.
pub fn build_entries_on(
    machine: &str,
    sessions: Vec<SessionDoc>,
    live: Vec<LivePane>,
    now: i64,
    server_now: i64,
) -> Vec<Entry> {
    build(
        Some(machine),
        sessions,
        live,
        now,
        server_now.saturating_sub(now),
    )
}

fn build(
    machine: Option<&str>,
    sessions: Vec<SessionDoc>,
    live: Vec<LivePane>,
    now: i64,
    clock_offset: i64,
) -> Vec<Entry> {
    let machine = machine.map(str::to_string);
    let mut joined: Vec<Option<LivePane>> = vec![None; sessions.len()];
    let mut loose = Vec::new();
    for pane in live {
        match sessions.iter().position(|s| pane_runs_session(&pane, s)) {
            Some(i) if joined[i].is_none() => joined[i] = Some(pane),
            _ => loose.push(pane),
        }
    }

    sessions
        .into_iter()
        .zip(joined)
        .map(|(session, live)| Entry {
            session: Some(session),
            live,
            machine: machine.clone(),
            built_at: now,
            clock_offset,
        })
        .chain(loose.into_iter().map(|pane| Entry {
            session: None,
            live: Some(pane),
            machine: machine.clone(),
            built_at: now,
            clock_offset,
        }))
        .collect()
}

/// Whether `pane` runs `session`: its `kind` is the session's harness name and
/// its `session_id` is either the session ID or the path of the session's main
/// transcript.
pub(crate) fn pane_runs_session(pane: &LivePane, session: &SessionDoc) -> bool {
    let (Some(kind), Some(id)) = (&pane.kind, &pane.session_id) else {
        return false;
    };
    kind == session.key.harness.as_str()
        && (*id == session.key.id
            || session
                .files
                .first()
                .is_some_and(|main| main.path == Path::new(id)))
}

/// How well an entry matched: the worst over the query's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Every term matched a short field.
    Short,
    /// Some term matched only conversation text (or a pane's scrollback).
    Content,
    /// Some term matched only subagent text.
    Subagent,
}

/// One matching entry.
#[derive(Clone, Debug)]
pub struct Hit {
    /// Index into the entries passed to [`search`].
    pub entry: usize,
    pub tier: Tier,
    /// Text around the match of the first term that needed text; `None` for
    /// [`Tier::Short`].
    pub snippet: Option<String>,
    /// Some term matched only subagent text (`tier == Tier::Subagent`).
    pub from_subagent: bool,
}

/// The entries in `scope` that match every whitespace-separated term of
/// `query`, best first.
///
/// A term matches an entry's short fields (title, cwds, harness, summary,
/// category, tags; for live entries also workspace label, tab label, agent
/// name, pane title and cwd; cwds without the user's `home` directory
/// prefix, so a fragment of it does not match every session) when,
/// case-insensitively, the term is a substring of the field, or the term has 5 or more characters and is within one edit
/// (insertion, deletion, substitution, or transposition of adjacent letters)
/// of some word of the field, words being the field split on
/// non-alphanumeric characters. This mirrors the typo tolerance used for
/// conversation text; subsequence matching is deliberately not used, as it
/// matches almost any long title or path. Failing that,
/// a session entry may match through `text` (the last term as
/// [`TermMode::Prefix`], earlier ones [`TermMode::Whole`]) and a session-less
/// pane through a case-insensitive substring of its scrollback. `text` is
/// queried at most once per term, and only when some remaining session entry
/// missed that term on its short fields; an error counts as no matches.
///
/// Order: tier, then live before closed, then most recently active, then
/// title. An empty query returns every entry in scope as [`Tier::Short`].
pub fn search(
    entries: &[Entry],
    text: &dyn TextSearch,
    query: &str,
    scope: Scope,
    include_subagents: bool,
    home: Option<&str>,
) -> Vec<Hit> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();

    struct Candidate {
        entry: usize,
        fields: Vec<String>,
        tier: Tier,
        snippet: Option<String>,
    }

    let mut candidates: Vec<Candidate> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| scope.contains(e))
        .map(|(i, e)| Candidate {
            entry: i,
            fields: if terms.is_empty() {
                Vec::new()
            } else {
                e.short_fields(home)
            },
            tier: Tier::Short,
            snippet: None,
        })
        .collect();

    for (t, term) in terms.iter().enumerate() {
        let short: Vec<bool> = candidates
            .iter()
            .map(|c| c.fields.iter().any(|f| short_match(term, f)))
            .collect();
        let needs_text = candidates
            .iter()
            .zip(&short)
            .any(|(c, &s)| !s && entries[c.entry].session.is_some());
        let text_hits: HashMap<SessionKey, TermHit> = if needs_text {
            let mode = if t + 1 == terms.len() {
                TermMode::Prefix
            } else {
                TermMode::Whole
            };
            text.term_matches(term, mode, include_subagents)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };

        let mut keep = short.into_iter();
        candidates.retain_mut(|c| {
            if keep.next().unwrap_or(false) {
                return true;
            }
            let entry = &entries[c.entry];
            let found = match &entry.session {
                Some(s) => text_hits
                    .get(&s.key)
                    .filter(|h| include_subagents || !h.from_subagent)
                    .map(|h| {
                        let tier = if h.from_subagent {
                            Tier::Subagent
                        } else {
                            Tier::Content
                        };
                        (tier, h.snippet.clone())
                    }),
                None => entry
                    .live
                    .as_ref()
                    .and_then(|l| l.scrollback.as_deref())
                    .and_then(|sb| scrollback_snippet(sb, term))
                    .map(|snippet| (Tier::Content, snippet)),
            };
            match found {
                Some((tier, snippet)) => {
                    c.tier = c.tier.max(tier);
                    c.snippet.get_or_insert(snippet);
                    true
                }
                None => false,
            }
        });
        if candidates.is_empty() {
            break;
        }
    }

    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .map(|c| Hit {
            entry: c.entry,
            tier: c.tier,
            snippet: c.snippet,
            from_subagent: c.tier == Tier::Subagent,
        })
        .collect();
    hits.sort_by(|a, b| rank(&entries[a.entry], a.tier, &entries[b.entry], b.tier));
    hits
}

/// The hits of several searches in one order, as (list index, hit index): the
/// order [`search`] gives, with ties kept in list order. Each list pairs the
/// entries given to [`search`] with the hits it returned.
pub fn merge_hits(lists: &[(&[Entry], &[Hit])]) -> Vec<(usize, usize)> {
    let mut order: Vec<(usize, usize)> = lists
        .iter()
        .enumerate()
        .flat_map(|(l, (_, hits))| (0..hits.len()).map(move |h| (l, h)))
        .collect();
    let at = |(l, h): (usize, usize)| {
        let (entries, hits) = lists[l];
        let hit = &hits[h];
        (&entries[hit.entry], hit.tier)
    };
    // A stable sort, so equal hits stay in list order, then hit order.
    order.sort_by(|&a, &b| {
        let (a_entry, a_tier) = at(a);
        let (b_entry, b_tier) = at(b);
        rank(a_entry, a_tier, b_entry, b_tier)
    });
    order
}

/// The order of hits: tier, then live before closed, then most recently
/// active, then title.
fn rank(a: &Entry, a_tier: Tier, b: &Entry, b_tier: Tier) -> Ordering {
    a_tier
        .cmp(&b_tier)
        .then_with(|| b.is_live().cmp(&a.is_live()))
        .then_with(|| b.last_active().cmp(&a.last_active()))
        .then_with(|| a.title().cmp(b.title()))
}

/// `cwd` without the leading `home` directory (keeping the separator after
/// it), if it lies inside it; unchanged otherwise, or when `home` is empty or
/// a root (`/`, `C:\`).
///
/// A Windows-style `home` (a drive letter and `:`, or a leading `\\`) matches
/// ASCII-case-insensitively (non-ASCII letters must match exactly) with `/`
/// and `\` as the same separator. The style
/// is read from `home`, not from the running platform, since cwds may come
/// from a remote machine.
fn strip_home<'a>(cwd: &'a str, home: Option<&str>) -> &'a str {
    let Some(home) = home else {
        return cwd;
    };
    if is_windows_path(home) {
        return strip_windows_home(cwd, home);
    }
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return cwd;
    }
    match cwd.strip_prefix(home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => cwd,
    }
}

/// Whether `path` starts with a drive letter and `:`, or with `\\`.
fn is_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || path.starts_with(r"\\")
}

/// [`strip_home`] for a Windows-style `home`.
fn strip_windows_home<'a>(cwd: &'a str, home: &str) -> &'a str {
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let home = home.trim_end_matches(['/', '\\']);
    // A bare drive (`C:`) is a root; a UNC home has at least a server name.
    if home.len() <= 2 {
        return cwd;
    }
    let (h, c) = (home.as_bytes(), cwd.as_bytes());
    if c.len() < h.len() {
        return cwd;
    }
    let same = h
        .iter()
        .zip(c)
        .all(|(&x, &y)| x.eq_ignore_ascii_case(&y) || (is_sep(x) && is_sep(y)));
    // Only ASCII bytes are matched loosely, so `h.len()` is a char boundary
    // of `cwd` whenever the prefixes are the same.
    match cwd.get(h.len()..) {
        Some(rest) if same && rest.bytes().next().is_none_or(is_sep) => rest,
        _ => cwd,
    }
}

/// `term` and `field` are already lowercase. See [`search`] for the rule.
fn short_match(term: &str, field: &str) -> bool {
    if field.contains(term) {
        return true;
    }
    let term: Vec<char> = term.chars().collect();
    if term.len() < TYPO_MIN_CHARS {
        return false;
    }
    let mut word = Vec::new();
    field
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| {
            word.clear();
            word.extend(w.chars());
            within_one_edit(&term, &word)
        })
}

/// Whether `a` and `b` differ by at most one insertion, deletion,
/// substitution or transposition of adjacent characters.
fn within_one_edit(a: &[char], b: &[char]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let Some(i) = short.iter().zip(long).position(|(x, y)| x != y) else {
        return true;
    };
    if short.len() == long.len() {
        short[i + 1..] == long[i + 1..]
            || (i + 1 < short.len()
                && short[i] == long[i + 1]
                && short[i + 1] == long[i]
                && short[i + 2..] == long[i + 2..])
    } else {
        short[i..] == long[i + 1..]
    }
}

/// The first scrollback line containing `term` (lowercase), trimmed and cut
/// to [`SNIPPET_CHARS`].
fn scrollback_snippet(scrollback: &str, term: &str) -> Option<String> {
    scrollback
        .lines()
        .find(|line| line.to_lowercase().contains(term))
        .map(|line| line.trim().chars().take(SNIPPET_CHARS).collect())
}
