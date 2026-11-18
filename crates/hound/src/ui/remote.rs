//! Other machines in the popup: loading a remote's sessions and live panes,
//! searching them, the facts used to resume there, and which host and facts
//! Enter uses for an entry.
//!
//! Everything decidable without a terminal is a plain function or a struct
//! with no threads; [`start_remotes`] is the one place that starts worker
//! threads, one per usable remote.
//!
//! Remote text (titles, labels, cwds, snippets, notes) reaches the screen only
//! through the popup's row and footer functions, which flatten control
//! characters.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hound_core::config::{Config, RemoteConfig};
use hound_core::host::{Host, LivePane};
use hound_core::index::{SessionDoc, TermHit, TermMode, TextSearch};
use hound_core::model::SessionKey;
use hound_core::open::{Facts, LocalFacts, ResumeFacts};
use hound_core::remote::client::{RemoteClient, RemoteError};
use hound_core::remote::token::read_token;
use hound_core::remote::wire::SessionFacts;
use hound_core::search::{Entry, Hit, Scope, build_entries_on, search};

use super::SharedHost;
use crate::herdr::validate_machine_label;

/// How long a worker waits for more keystrokes before searching.
pub const DEBOUNCE: Duration = Duration::from_millis(80);
/// Longest wait for a remote machine's herdr snapshot while loading it.
pub const SNAPSHOT_LIMIT: Duration = Duration::from_secs(5);

const NEEDS_HERDR: &str = "needs herdr";
const HOST_UNREACHABLE: &str = "herdr machine not reachable";
const NO_TOKEN: &str = "no token";
const INVALID_NAME: &str = "invalid machine name";
const DUPLICATE_NAME: &str = "duplicate machine name";
const NOT_CONFIGURED: &str = "not configured";
const LOADING: &str = "loading\u{2026}";
const INTERNAL_ERROR: &str = "internal error";

/// A loaded remote: what its service and its herdr host reported.
#[derive(Clone, Debug)]
pub struct RemoteState {
    /// The machine's label, as configured.
    pub machine: String,
    /// The remote's sessions; empty if its service could not be used.
    pub sessions: Vec<SessionDoc>,
    /// Each session's facts as listed; Enter asks again before resuming.
    pub facts: HashMap<SessionKey, SessionFacts>,
    /// The remote's live panes; empty if its host could not be listed.
    pub live: Vec<LivePane>,
    /// How far the remote's clock is ahead of this one, in seconds.
    pub clock_offset: i64,
    /// The remote user's home directory, left out when matching its cwds.
    pub home: Option<String>,
    /// The service answered: sessions and facts come from it.
    pub http_ok: bool,
}

pub enum RemoteLoad {
    Ready(RemoteState),
    /// Usable, with half missing; the note says which.
    Partial(RemoteState, String),
    Failed(String),
}

/// How a load left a remote, as the event loop needs to know it.
#[derive(Clone, Debug, PartialEq)]
pub enum Reach {
    /// Service and host both answered.
    Ready,
    /// The service answered but the host's snapshot failed: the host is
    /// still tried on Enter.
    NoHost,
    /// The host answered but the service did not: live panes can be focused,
    /// nothing can be resumed. Carries the note.
    NoFacts(String),
    /// Nothing usable. Carries the note.
    Failed(String),
}

impl Reach {
    pub fn of(load: &RemoteLoad) -> Reach {
        match load {
            RemoteLoad::Ready(_) => Reach::Ready,
            RemoteLoad::Partial(state, note) if !state.http_ok => Reach::NoFacts(note.clone()),
            RemoteLoad::Partial(..) => Reach::NoHost,
            RemoteLoad::Failed(note) => Reach::Failed(note.clone()),
        }
    }

    /// The footer note this leaves, if any.
    pub fn note(&self) -> Option<&str> {
        match self {
            Reach::Ready => None,
            Reach::NoHost => Some(HOST_UNREACHABLE),
            Reach::NoFacts(note) | Reach::Failed(note) => Some(note),
        }
    }
}

/// Loads `machine`: its description (a protocol mismatch makes the service
/// unusable) and sessions through `client`, and, at the same time, its live
/// panes through `host`, the machine's own herdr host. Without a host
/// nothing is asked.
pub fn load_remote(
    machine: &str,
    client: &RemoteClient,
    host: Option<&dyn Host>,
    now: i64,
) -> RemoteLoad {
    let Some(host) = host else {
        return RemoteLoad::Failed(NEEDS_HERDR.to_string());
    };
    // The service is asked on its own thread while the host is listed here.
    let (http, live) = thread::scope(|scope| {
        let http = scope.spawn(|| {
            client
                .info()
                .and_then(|(info, _)| Ok((info.home, client.sessions()?)))
        });
        let live = host.snapshot();
        let http = http
            .join()
            .unwrap_or_else(|_| Err(RemoteError::Bad("request panicked".to_string())));
        (http, live)
    });
    let mut state = RemoteState {
        machine: machine.to_string(),
        sessions: Vec::new(),
        facts: HashMap::new(),
        live: Vec::new(),
        clock_offset: 0,
        home: None,
        http_ok: false,
    };
    let http_note = match http {
        Ok((home, remote)) => {
            state.sessions = remote.sessions;
            state.facts = remote.facts;
            state.clock_offset = remote.now.saturating_sub(now);
            state.home = Some(home).filter(|h| !h.is_empty());
            state.http_ok = true;
            None
        }
        Err(e) => Some(e.note().to_string()),
    };
    match (http_note, live) {
        (None, Ok(live)) => {
            state.live = live;
            RemoteLoad::Ready(state)
        }
        (None, Err(_)) => RemoteLoad::Partial(state, HOST_UNREACHABLE.to_string()),
        (Some(note), Ok(live)) => {
            state.live = live;
            RemoteLoad::Partial(state, note)
        }
        (Some(note), Err(_)) => RemoteLoad::Failed(note),
    }
}

impl RemoteState {
    /// The remote's entries, built at local time `now`, and those matching
    /// `query`, its text searched through `client`.
    pub fn search(
        &self,
        client: &dyn TextSearch,
        query: &str,
        scope: Scope,
        include_subagents: bool,
        now: i64,
    ) -> (Vec<Entry>, Vec<Hit>) {
        let entries = build_entries_on(
            &self.machine,
            self.sessions.clone(),
            self.live.clone(),
            now,
            now.saturating_add(self.clock_offset),
        );
        let hits = search(
            &entries,
            client,
            query,
            scope,
            include_subagents,
            self.home.as_deref(),
        );
        (entries, hits)
    }
}

/// A remote session's facts, read fresh by its service (`GET /v1/session`).
pub struct RemoteFacts<'a> {
    pub client: &'a RemoteClient,
}

impl ResumeFacts for RemoteFacts<'_> {
    fn facts(&self, session: &SessionDoc, _local_now: i64) -> anyhow::Result<Facts> {
        let (facts, now) = self
            .client
            .session_facts(&session.key)
            .map_err(|e| anyhow::anyhow!(error_text(&e)))?;
        Ok(Facts {
            last_change: facts.last_change,
            cwd_exists: facts.cwd_exists,
            now,
        })
    }
}

/// A few words for the user; a [`RemoteError::Bad`] reason is fixed text.
fn error_text(e: &RemoteError) -> String {
    match e {
        RemoteError::Bad(reason) => reason.clone(),
        e => e.note().to_string(),
    }
}

/// Latest wins: each new query takes the next sequence number, and only
/// results for the newest are current.
#[derive(Debug, Default)]
pub struct Debounce {
    latest: u64,
}

impl Debounce {
    pub fn new() -> Debounce {
        Debounce::default()
    }

    /// The sequence number for a new query. Named as the plan names it;
    /// this is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    /// Whether `seq` is the newest query's.
    pub fn is_current(&self, seq: u64) -> bool {
        seq == self.latest
    }
}

/// The facts an entry is resumed with, owned so they can move to the
/// thread that opens it.
pub enum MachineFacts {
    /// This machine's filesystem.
    Local,
    /// The remote's service.
    Remote(Arc<RemoteClient>),
    /// The remote's service could not be used; resuming fails with this.
    Unavailable(String),
}

impl ResumeFacts for MachineFacts {
    fn facts(&self, session: &SessionDoc, local_now: i64) -> anyhow::Result<Facts> {
        match self {
            MachineFacts::Local => LocalFacts.facts(session, local_now),
            MachineFacts::Remote(client) => RemoteFacts { client }.facts(session, local_now),
            MachineFacts::Unavailable(reason) => Err(anyhow::anyhow!(reason.clone())),
        }
    }
}

/// The host and facts to open an entry with.
pub struct Route {
    pub host: Option<SharedHost>,
    pub facts: MachineFacts,
}

/// One configured remote, as Enter needs it.
struct RemoteRoute {
    machine: String,
    /// Its herdr host and its service's client, or why it cannot be used.
    reach: Result<(SharedHost, Arc<RemoteClient>), String>,
    /// Why its service cannot answer for facts, if it cannot.
    no_facts: Option<String>,
}

/// Which host and facts each machine's entries are opened with.
pub struct Routes {
    local: Option<SharedHost>,
    remotes: Vec<RemoteRoute>,
}

impl Routes {
    pub fn new(local: Option<SharedHost>) -> Routes {
        Routes {
            local,
            remotes: Vec::new(),
        }
    }

    /// Adds the remote `machine`: its host and client, or why it cannot be
    /// used. The first added under a label wins.
    pub fn add(&mut self, machine: &str, reach: Result<(SharedHost, Arc<RemoteClient>), String>) {
        self.remotes.push(RemoteRoute {
            machine: machine.to_string(),
            reach,
            no_facts: None,
        });
    }

    /// Records how loading `machine` went.
    pub fn apply(&mut self, machine: &str, reach: &Reach) {
        let Some(remote) = self.remotes.iter_mut().find(|r| r.machine == machine) else {
            return;
        };
        match reach {
            Reach::Ready | Reach::NoHost => {}
            Reach::NoFacts(note) => remote.no_facts = Some(note.clone()),
            Reach::Failed(note) => remote.reach = Err(note.clone()),
        }
    }

    /// The host and facts for an entry on `machine` (`None`: this machine).
    ///
    /// This machine's entries get the local host (if any) and
    /// [`LocalFacts`]. A remote's entries get that machine's own host and
    /// its service's facts — never the local host or local facts: a remote
    /// that is not configured, was rejected, has no route through herdr, no
    /// token, or failed to load is an error, `"<machine>: <reason>"`. A
    /// remote whose service failed but whose host answered gets its host and
    /// [`MachineFacts::Unavailable`], so its live panes can be focused but
    /// nothing there resumed.
    pub fn route(&self, machine: Option<&str>) -> Result<Route, String> {
        let Some(machine) = machine else {
            return Ok(Route {
                host: self.local.clone(),
                facts: MachineFacts::Local,
            });
        };
        let remote = self
            .remotes
            .iter()
            .find(|r| r.machine == machine)
            .ok_or_else(|| format!("{machine}: {NOT_CONFIGURED}"))?;
        let (host, client) = remote
            .reach
            .as_ref()
            .map_err(|reason| format!("{machine}: {reason}"))?;
        let facts = match &remote.no_facts {
            Some(reason) => MachineFacts::Unavailable(reason.clone()),
            None => MachineFacts::Remote(Arc::clone(client)),
        };
        Ok(Route {
            host: Some(Arc::clone(host)),
            facts,
        })
    }
}

/// Each remote with `Ok` if it can be used, else why not: its label is
/// `local` (any case), would not be accepted by `herdr --machine`, or is
/// shared (ignoring case) with another remote in `remotes`. Pass only the
/// enabled remotes.
pub fn check_remotes<'a>(
    remotes: impl IntoIterator<Item = &'a RemoteConfig>,
) -> Vec<(&'a RemoteConfig, Result<(), &'static str>)> {
    let remotes: Vec<&RemoteConfig> = remotes.into_iter().collect();
    let key = |r: &RemoteConfig| r.machine.to_lowercase();
    remotes
        .iter()
        .map(|r| {
            let check = if r.machine.eq_ignore_ascii_case("local")
                || validate_machine_label(&r.machine).is_err()
            {
                Err(INVALID_NAME)
            } else if remotes.iter().filter(|o| key(o) == key(r)).count() > 1 {
                Err(DUPLICATE_NAME)
            } else {
                Ok(())
            };
            (*r, check)
        })
        .collect()
}

/// A client for `remote`'s service, bounded by its [`RemoteConfig::timeout`];
/// `Err("no token")` if its token file is missing, unreadable or empty.
pub fn client_for(remote: &RemoteConfig) -> Result<RemoteClient, String> {
    let token = read_token(&remote.token_file).map_err(|_| NO_TOKEN.to_string())?;
    Ok(RemoteClient::new(&remote.url, token, remote.timeout()))
}

/// A search for a worker to run.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub seq: u64,
    pub query: String,
    pub scope: Scope,
    pub include_subagents: bool,
}

/// What a worker reports.
pub enum RemoteMsg {
    /// The remote has been loaded; sent once, before any results.
    Loaded {
        machine: String,
        sessions: usize,
        reach: Reach,
    },
    /// Results for search `seq`, and the footer note they leave: the text
    /// search's error if it failed, else the load's note.
    Results {
        machine: String,
        seq: u64,
        entries: Vec<Entry>,
        hits: Vec<Hit>,
        note: Option<String>,
    },
    /// The worker stopped after reporting `Loaded` (it panicked): the remote
    /// is now unusable, its rows go, and the footer shows `note`.
    Stopped { machine: String, note: String },
}

/// A worker's view of one search: its results and, if the remote's text
/// search failed, that error's note.
pub struct RemoteSearch {
    pub entries: Vec<Entry>,
    pub hits: Vec<Hit>,
    pub error: Option<String>,
}

impl RemoteState {
    /// [`RemoteState::search`], also reporting whether a text request to
    /// the remote failed (short-field matches are kept either way).
    pub fn search_watched(
        &self,
        client: &dyn TextSearch,
        request: &SearchRequest,
        now: i64,
    ) -> RemoteSearch {
        let watched = Watched {
            inner: client,
            error: RefCell::new(None),
        };
        let (entries, hits) = self.search(
            &watched,
            &request.query,
            request.scope,
            request.include_subagents,
            now,
        );
        RemoteSearch {
            entries,
            hits,
            error: watched.error.into_inner(),
        }
    }
}

/// A text search that remembers its last error.
struct Watched<'a> {
    inner: &'a dyn TextSearch,
    error: RefCell<Option<String>>,
}

impl TextSearch for Watched<'_> {
    fn term_matches(
        &self,
        term: &str,
        mode: TermMode,
        include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        let result = self.inner.term_matches(term, mode, include_subagents);
        if let Err(e) = &result {
            // The client's errors are already its RemoteError::note().
            *self.error.borrow_mut() = Some(format!("{e:#}"));
        }
        result
    }
}

/// A host whose `snapshot` gives up after `limit`. The snapshot runs on a
/// helper thread; on timeout that thread is abandoned (its herdr child has
/// no stdin and piped output, so it holds nothing of the terminal, and it is
/// reaped when it ends). Every other call goes straight through. Used only
/// for a remote's load, never for the local host.
pub struct TimedHost {
    pub inner: SharedHost,
    pub limit: Duration,
}

impl Host for TimedHost {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        let (tx, rx) = mpsc::channel();
        let inner = Arc::clone(&self.inner);
        thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| inner.snapshot()))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("snapshot panicked")));
            let _ = tx.send(result);
        });
        rx.recv_timeout(self.limit)
            .unwrap_or_else(|_| Err(anyhow::anyhow!("herdr did not answer in time")))
    }

    fn read_scrollback(&self, pane_id: &str, lines: u32) -> anyhow::Result<String> {
        self.inner.read_scrollback(pane_id, lines)
    }

    fn focus(&self, pane: &LivePane) -> anyhow::Result<()> {
        self.inner.focus(pane)
    }

    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane> {
        self.inner.create_tab(workspace_id, cwd)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        self.inner.start_agent(name, kind, pane_id, args)
    }
}

/// A running worker: send it searches.
pub struct Worker {
    pub machine: String,
    pub requests: Sender<SearchRequest>,
}

/// The popup's remotes once set up.
pub struct RemoteSetup {
    pub routes: Routes,
    pub workers: Vec<Worker>,
    /// `(machine, note)` to show at start: why a remote got no worker, or
    /// `loading…` for one whose worker has started.
    pub notes: Vec<(String, String)>,
    /// Some enabled remote is configured.
    pub configured: bool,
}

/// A host for the attached machine with the given label, driven through
/// the local herdr.
pub type MachineHosts<'a> = &'a dyn Fn(&str) -> anyhow::Result<SharedHost>;

/// The local herdr: its own host and how to reach the machines attached to
/// it, taken from one detection so they cannot disagree.
pub type LocalHerdr<'a> = Option<(SharedHost, MachineHosts<'a>)>;

/// [`start_remotes_with`] with the snapshot limit [`SNAPSHOT_LIMIT`].
pub fn start_remotes(
    config: &Config,
    herdr: LocalHerdr<'_>,
    results: Sender<RemoteMsg>,
) -> RemoteSetup {
    start_remotes_with(config, herdr, results, SNAPSHOT_LIMIT)
}

/// Starts a detached worker for each enabled, valid remote with a token,
/// when there is a local herdr to reach its machine through; every other
/// enabled remote gets a note and no worker, and nothing is sent to it.
/// Disabled remotes are ignored entirely. Workers report on `results`; a
/// remote's herdr snapshot gives up after `snapshot_limit`.
pub fn start_remotes_with(
    config: &Config,
    herdr: LocalHerdr<'_>,
    results: Sender<RemoteMsg>,
    snapshot_limit: Duration,
) -> RemoteSetup {
    let (local, machine_host) = match herdr {
        Some((local, machine_host)) => (Some(local), Some(machine_host)),
        None => (None, None),
    };
    let mut setup = RemoteSetup {
        routes: Routes::new(local),
        workers: Vec::new(),
        notes: Vec::new(),
        configured: false,
    };
    for (remote, check) in check_remotes(config.enabled_remotes()) {
        setup.configured = true;
        let machine = remote.machine.as_str();
        let ready = check
            .map_err(str::to_string)
            .and_then(|()| match machine_host {
                // The label already passed herdr's checks; anything else
                // the factory refuses is still the label.
                Some(machine_host) => machine_host(machine).map_err(|_| INVALID_NAME.to_string()),
                None => Err(NEEDS_HERDR.to_string()),
            })
            .and_then(|host| Ok((host, Arc::new(client_for(remote)?))));
        let note = match ready {
            Ok((host, client)) => {
                let (tx, rx) = mpsc::channel();
                let worker_client = Arc::clone(&client);
                let timed = TimedHost {
                    inner: Arc::clone(&host),
                    limit: snapshot_limit,
                };
                let name = machine.to_string();
                let results = results.clone();
                thread::spawn(move || {
                    run_worker(
                        &name,
                        || load_remote(&name, &worker_client, Some(&timed), now()),
                        |state, request| {
                            state.search_watched(worker_client.as_ref(), request, now())
                        },
                        &rx,
                        &results,
                    )
                });
                setup.routes.add(machine, Ok((host, client)));
                setup.workers.push(Worker {
                    machine: machine.to_string(),
                    requests: tx,
                });
                LOADING.to_string()
            }
            Err(note) => {
                setup.routes.add(machine, Err(note.clone()));
                note
            }
        };
        if !setup.notes.iter().any(|(m, _)| m == machine) {
            setup.notes.push((machine.to_string(), note));
        }
    }
    setup
}

/// A worker's whole life, on its own thread: `load` the remote and report
/// `Loaded`, then answer searches until the popup drops its sender, each
/// after [`DEBOUNCE`] and skipping to the newest pending request. A panic
/// in `load` or `search` is caught: it ends the worker after reporting the
/// remote as failed (`internal error`), as `Loaded` if that was not sent
/// yet, else as `Stopped`.
pub fn run_worker(
    machine: &str,
    load: impl FnOnce() -> RemoteLoad,
    mut search: impl FnMut(&RemoteState, &SearchRequest) -> RemoteSearch,
    requests: &Receiver<SearchRequest>,
    results: &Sender<RemoteMsg>,
) {
    let Ok(load) = panic::catch_unwind(AssertUnwindSafe(load)) else {
        let _ = results.send(RemoteMsg::Loaded {
            machine: machine.to_string(),
            sessions: 0,
            reach: Reach::Failed(INTERNAL_ERROR.to_string()),
        });
        return;
    };
    let reach = Reach::of(&load);
    let load_note = reach.note().map(str::to_string);
    let state = match load {
        RemoteLoad::Ready(state) | RemoteLoad::Partial(state, _) => Some(state),
        RemoteLoad::Failed(_) => None,
    };
    let loaded = RemoteMsg::Loaded {
        machine: machine.to_string(),
        sessions: state.as_ref().map_or(0, |s| s.sessions.len()),
        reach,
    };
    if results.send(loaded).is_err() {
        return;
    }
    let Some(state) = state else { return };
    while let Ok(mut request) = requests.recv() {
        thread::sleep(DEBOUNCE);
        while let Ok(newer) = requests.try_recv() {
            request = newer;
        }
        let Ok(found) = panic::catch_unwind(AssertUnwindSafe(|| search(&state, &request))) else {
            let _ = results.send(RemoteMsg::Stopped {
                machine: machine.to_string(),
                note: INTERNAL_ERROR.to_string(),
            });
            return;
        };
        let msg = RemoteMsg::Results {
            machine: machine.to_string(),
            seq: request.seq,
            entries: found.entries,
            hits: found.hits,
            note: found.error.or_else(|| load_note.clone()),
        };
        if results.send(msg).is_err() {
            return;
        }
    }
}

/// The header's session count: the local index's sessions plus, when
/// remotes are configured, every loaded remote's.
pub fn total_sessions(
    local: usize,
    remotes: impl IntoIterator<Item = usize>,
    remotes_configured: bool,
) -> usize {
    if remotes_configured {
        local + remotes.into_iter().sum::<usize>()
    } else {
        local
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
