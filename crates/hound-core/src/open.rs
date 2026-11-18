//! What Enter does on an entry: focus its live pane, resume a closed session
//! in a new tab, or, with no host, hand back a command to run in this terminal.
//! An entry from a remote machine goes through that machine's host and facts.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::adapters::{Adapter, valid_session_id};
use crate::host::{Host, LivePane};
use crate::index::SessionDoc;
use crate::model::Harness;
use crate::remote::wire::session_facts;
use crate::search::{Entry, RECENT_SECS, pane_runs_session};

const CONFIRM_RUNNING_ELSEWHERE: &str = "This session changed in the last 2 minutes and may be running elsewhere. Enter again to resume anyway.";
/// Characters of the session ID used in an agent name.
const NAME_ID_CHARS: usize = 6;
/// A prefix match must share at least this many path components below `/`.
const MIN_PREFIX_COMPONENTS: usize = 2;

#[derive(Debug, PartialEq)]
pub enum OpenOutcome {
    /// An existing pane was focused.
    Focused,
    /// A new tab was created, the harness started in it, and it is focused.
    Resumed,
    /// A message to show; call again with `confirmed = true` to go ahead.
    NeedsConfirm(String),
    /// A message to show; the popup stays open.
    Failed(String),
    /// No host: the caller leaves the TUI and execs `argv` in `cwd`, in this terminal.
    RunHere { cwd: String, argv: Vec<String> },
}

/// What resuming a session needs to know from the machine it lives on.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// Unix seconds on that machine's clock: the later of the session's
    /// `updated_at` and its main file's modification time.
    pub last_change: i64,
    /// The session's `resume_cwd` is non-empty and a directory there.
    pub cwd_exists: bool,
    /// That machine's clock, Unix seconds.
    pub now: i64,
}

/// Answers [`Facts`] for a session, fresh, just before it is resumed.
pub trait ResumeFacts {
    /// `local_now` is this machine's clock; an `Err` stops the resume.
    fn facts(&self, session: &SessionDoc, local_now: i64) -> anyhow::Result<Facts>;
}

/// Facts read from this machine's filesystem.
pub struct LocalFacts;

impl ResumeFacts for LocalFacts {
    fn facts(&self, session: &SessionDoc, local_now: i64) -> anyhow::Result<Facts> {
        let facts = session_facts(session);
        Ok(Facts {
            last_change: facts.last_change,
            cwd_exists: facts.cwd_exists,
            now: local_now,
        })
    }
}

/// The workspace to resume a session with working directory `cwd` in.
///
/// The workspace with the most panes whose cwd equals `cwd`; failing that, the
/// workspace owning the pane sharing the longest path-component prefix with
/// `cwd`, if it shares at least two components below `/`. Ties go to the
/// lowest workspace ID; `None` if no pane qualifies.
pub fn pick_workspace(cwd: &str, live: &[LivePane]) -> Option<String> {
    let target = Path::new(cwd);

    let mut exact: BTreeMap<&str, usize> = BTreeMap::new();
    for pane in live.iter().filter(|p| Path::new(&p.cwd) == target) {
        *exact.entry(pane.workspace_id.as_str()).or_default() += 1;
    }
    if let Some(best) = best_by_score(exact) {
        return Some(best.to_string());
    }

    let mut prefix: BTreeMap<&str, usize> = BTreeMap::new();
    for pane in live {
        let shared = shared_components(target, Path::new(&pane.cwd));
        if shared >= MIN_PREFIX_COMPONENTS {
            let best = prefix.entry(pane.workspace_id.as_str()).or_default();
            *best = (*best).max(shared);
        }
    }
    best_by_score(prefix).map(str::to_string)
}

/// The key with the highest score, the lowest key among equals.
fn best_by_score(scores: BTreeMap<&str, usize>) -> Option<&str> {
    // BTreeMap iterates keys ascending; keep the first of the highest.
    scores
        .into_iter()
        .fold(
            None,
            |best: Option<(&str, usize)>, (key, score)| match best {
                Some((_, top)) if top >= score => best,
                _ => Some((key, score)),
            },
        )
        .map(|(key, _)| key)
}

/// Named path components the two paths share from the start, both being
/// rooted the same way.
fn shared_components(a: &Path, b: &Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .filter(|(x, _)| matches!(x, Component::Normal(_)))
        .count()
}

/// A name for the agent resuming `session_id`, unique among `taken`:
/// `"<harness>-<first 6 chars of the ID, lowercased, non [a-z0-9] removed>"`
/// (just `"<harness>"` if nothing of the ID is left), then `-2`, `-3`, … until
/// it is free.
pub fn agent_name(harness: Harness, session_id: &str, taken: &[String]) -> String {
    let id: String = session_id
        .chars()
        .take(NAME_ID_CHARS)
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    let base = if id.is_empty() {
        harness.as_str().to_string()
    } else {
        format!("{}-{id}", harness.as_str())
    };
    let is_taken = |name: &str| taken.iter().any(|t| t == name);
    if !is_taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !is_taken(name))
        .expect("some suffix is free")
}

/// Act on Enter for `entry`. A live entry is focused. A closed session is
/// focused if a pane picked it up since the entries were built, else resumed
/// in a new tab, asking first if it changed recently and may be running
/// elsewhere. With no host a closed session becomes [`OpenOutcome::RunHere`].
///
/// `host` and `facts` belong to the entry's machine. For an entry from a
/// remote machine, no host is a failure, live or closed, and every failure
/// message starts with `"<machine>: "`.
pub fn open_entry(
    entry: &Entry,
    host: Option<&dyn Host>,
    adapters: &[Box<dyn Adapter>],
    facts: &dyn ResumeFacts,
    confirmed: bool,
    now: i64,
) -> OpenOutcome {
    let Some(machine) = &entry.machine else {
        return open_on_machine(entry, host, adapters, facts, confirmed, now);
    };
    let outcome = match host {
        Some(host) => open_on_machine(entry, Some(host), adapters, facts, confirmed, now),
        None => OpenOutcome::Failed("no route to that machine's terminal host".to_string()),
    };
    match outcome {
        OpenOutcome::Failed(msg) => OpenOutcome::Failed(format!("{machine}: {msg}")),
        other => other,
    }
}

/// [`open_entry`] without regard to which machine the entry is on.
fn open_on_machine(
    entry: &Entry,
    host: Option<&dyn Host>,
    adapters: &[Box<dyn Adapter>],
    facts: &dyn ResumeFacts,
    confirmed: bool,
    now: i64,
) -> OpenOutcome {
    if let Some(live) = &entry.live {
        return match host {
            Some(host) => match host.focus(live) {
                Ok(()) => OpenOutcome::Focused,
                Err(e) => failed(e),
            },
            None => OpenOutcome::Failed("no terminal host to focus this pane in".to_string()),
        };
    }
    let Some(session) = &entry.session else {
        return OpenOutcome::Failed("nothing to open".to_string());
    };
    // Every resume path ends in the harness's arguments; no unvalidated id
    // reaches them, whatever machine or index the session came from.
    if !valid_session_id(&session.key.id) {
        return OpenOutcome::Failed("invalid session id".to_string());
    }

    let Some(host) = host else {
        return run_here(session, adapters, facts, confirmed, now);
    };

    let live = match host.snapshot() {
        Ok(live) => live,
        Err(e) => return failed(e),
    };
    if let Some(pane) = live.iter().find(|p| pane_runs_session(p, session)) {
        return match host.focus(pane) {
            Ok(()) => OpenOutcome::Focused,
            Err(e) => failed(e),
        };
    }

    if let Some(outcome) = check_resumable(session, facts, confirmed, now) {
        return outcome;
    }
    let adapter = match adapter_for(session, adapters) {
        Ok(adapter) => adapter,
        Err(outcome) => return outcome,
    };
    let cwd = session.resume_cwd.as_str();
    let workspace = pick_workspace(cwd, &live).or_else(|| {
        live.iter()
            .find(|p| p.focused)
            .map(|p| p.workspace_id.clone())
    });
    let Some(workspace) = workspace else {
        return OpenOutcome::Failed("no workspace to open the session in".to_string());
    };

    let harness = session.key.harness;
    let taken: Vec<String> = live.iter().filter_map(|p| p.agent_name.clone()).collect();
    let name = agent_name(harness, &session.key.id, &taken);
    let args = adapter.resume_args(&session.key.id);
    let pane = match host.create_tab(&workspace, cwd) {
        Ok(pane) => pane,
        Err(e) => return failed(e),
    };
    match host.start_agent(&name, harness.as_str(), &pane.pane_id, &args) {
        Ok(()) => OpenOutcome::Resumed,
        // The tab stays behind; say so, as another Enter would open a second one.
        Err(e) => failed(e.context("tab created but the agent failed to start")),
    }
}

/// The no-host path for a closed session.
fn run_here(
    session: &SessionDoc,
    adapters: &[Box<dyn Adapter>],
    facts: &dyn ResumeFacts,
    confirmed: bool,
    now: i64,
) -> OpenOutcome {
    if let Some(outcome) = check_resumable(session, facts, confirmed, now) {
        return outcome;
    }
    let adapter = match adapter_for(session, adapters) {
        Ok(adapter) => adapter,
        Err(outcome) => return outcome,
    };
    let mut argv = vec![session.key.harness.as_str().to_string()];
    argv.extend(adapter.resume_args(&session.key.id));
    OpenOutcome::RunHere {
        cwd: session.resume_cwd.clone(),
        argv,
    }
}

/// `Failed` if the facts cannot be had or the session's directory is gone,
/// `NeedsConfirm` if it changed recently on its machine's clock and the user
/// has not confirmed; `None` to go ahead.
fn check_resumable(
    session: &SessionDoc,
    facts: &dyn ResumeFacts,
    confirmed: bool,
    now: i64,
) -> Option<OpenOutcome> {
    let facts = match facts.facts(session, now) {
        Ok(facts) => facts,
        Err(e) => return Some(failed(e)),
    };
    if !facts.cwd_exists {
        return Some(OpenOutcome::Failed(format!(
            "directory no longer exists: {}",
            session.resume_cwd
        )));
    }
    if !confirmed && facts.now.saturating_sub(facts.last_change) < RECENT_SECS {
        return Some(OpenOutcome::NeedsConfirm(
            CONFIRM_RUNNING_ELSEWHERE.to_string(),
        ));
    }
    None
}

fn adapter_for<'a>(
    session: &SessionDoc,
    adapters: &'a [Box<dyn Adapter>],
) -> Result<&'a dyn Adapter, OpenOutcome> {
    adapters
        .iter()
        .find(|a| a.harness() == session.key.harness)
        .map(|a| a.as_ref())
        .ok_or_else(|| {
            OpenOutcome::Failed(format!(
                "no adapter for harness {}",
                session.key.harness.as_str()
            ))
        })
}

/// The whole error chain on one line.
fn failed(e: anyhow::Error) -> OpenOutcome {
    OpenOutcome::Failed(format!("{e:#}"))
}
