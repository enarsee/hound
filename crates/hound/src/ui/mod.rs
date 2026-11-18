//! The popup: [`app`] holds the state machine, [`render`] draws it, and
//! [`run`] wires both to the terminal, the index and the host. Everything
//! slow (refresh, host calls, scrollback) runs on background threads and
//! reports back over a channel, so the loop never waits on it.

pub mod app;
pub mod remote;
pub mod render;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Stdout};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::adapters::pi::PiAdapter;
use hound_core::config::{self, Config};
use hound_core::host::{Host, LivePane};
use hound_core::index::{Index, SessionDoc};
use hound_core::open::{OpenOutcome, open_entry};
use hound_core::search::{Scope, build_entries, search};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use app::{AfterRefresh, App, Effect, Exit, after_refresh};
pub use render::draw;

use crate::herdr::CliHerdr;
use remote::{Debounce, Reach, RemoteMsg, Routes, SearchRequest, Worker, start_remotes};

/// How long the loop waits for a terminal event before checking the channel.
const TICK: Duration = Duration::from_millis(50);
/// Longest time between redraws when nothing changes.
const REDRAW_EVERY: Duration = Duration::from_secs(1);
/// Scrollback lines read from each session-less pane.
const SCROLLBACK_LINES: u32 = 200;
/// `enrich.log` is emptied when opened if it has grown past this.
const ENRICH_LOG_MAX: u64 = 1024 * 1024;
/// A refresh completed less than this many seconds ago (by `hound serve`,
/// another popup or `hound index`) makes the popup's own refresh unnecessary.
const FRESH_HEARTBEAT_SECS: i64 = 60;

/// A host shared with background threads.
pub type SharedHost = Arc<dyn Host + Send + Sync>;

/// Results from background threads.
enum Msg {
    Progress(usize, usize),
    /// The refresh committed a batch of sessions.
    Committed,
    /// `Ok(true)` refreshed, `Ok(false)` another process holds the writer
    /// lock. `retried`: this refresh was the retry after a lock-busy one.
    Refreshed {
        result: anyhow::Result<bool>,
        retried: bool,
    },
    Live(anyhow::Result<Vec<LivePane>>),
    /// `(pane_id, scrollback)` for each pane that could be read.
    Scrollback(Vec<(String, String)>),
    Opened(OpenOutcome),
}

/// The adapters for every supported harness, as configured.
pub fn adapters(config: &Config) -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(ClaudeAdapter::new(
            config.claude_root.clone(),
            config.index_subagents,
        )),
        Box::new(PiAdapter::new(config.pi_root.clone())),
    ]
}

/// Runs the popup until it closes. `Some((cwd, argv))`: the terminal has been
/// restored and the caller must exec `argv` in `cwd`. `herdr` is the herdr
/// session hound runs in, if any: the local host, and the route to every
/// machine attached to it.
pub fn run(
    scope: Scope,
    config: &Config,
    index: Index,
    herdr: Option<CliHerdr>,
) -> anyhow::Result<Option<(String, Vec<String>)>> {
    let index = Arc::new(index);
    let herdr: Option<Arc<CliHerdr>> = herdr.map(Arc::new);
    let host: Option<SharedHost> = herdr.clone().map(|h| h as SharedHost);
    let (tx, rx) = mpsc::channel();
    let (remote_tx, remote_rx) = mpsc::channel();

    let machine_host = |label: &str| -> anyhow::Result<SharedHost> {
        match &herdr {
            Some(herdr) => Ok(Arc::new(herdr.for_machine(label)?)),
            None => anyhow::bail!("no local herdr"),
        }
    };
    let local_herdr = host
        .clone()
        .map(|local| (local, &machine_host as remote::MachineHosts<'_>));
    let remotes = start_remotes(config, local_herdr, remote_tx);

    let mut app = App::new(scope, config.search_subagents);
    app.remotes_configured = remotes.configured;
    for (machine, note) in &remotes.notes {
        app.set_note(machine, Some(note.clone()));
    }
    let mut ui = Ui {
        app,
        sessions: index.sessions()?,
        live: Vec::new(),
        index: Arc::clone(&index),
        host: host.clone(),
        config: config.clone(),
        home: std::env::var("HOME").ok(),
        tx: tx.clone(),
        routes: remotes.routes,
        workers: remotes.workers,
        debounce: Debounce::new(),
        remote_sessions: Vec::new(),
    };
    ui.requery(Reason::User);

    if !skip_start_refresh(index.heartbeat_age(now())) {
        spawn_refresh(Arc::clone(&index), config.clone(), tx.clone(), None);
    }
    if let Some(host) = host {
        thread::spawn(move || {
            let _ = tx.send(Msg::Live(host.snapshot()));
        });
    }

    let mut terminal = TerminalGuard::enter()?;
    let result = ui.event_loop(&mut terminal.terminal, &rx, &remote_rx);
    drop(terminal);
    result
}

/// Whether the popup can skip its refresh at start: the index's heartbeat
/// (seconds since its last completed refresh) is under a minute old. A
/// heartbeat in the future, or none, means refresh.
pub fn skip_start_refresh(heartbeat_age: Option<i64>) -> bool {
    heartbeat_age.is_some_and(|age| (0..FRESH_HEARTBEAT_SECS).contains(&age))
}

/// Why results are being recomputed.
#[derive(Clone, Copy)]
enum Reason {
    /// The query, scope or subagent setting changed: back to the top.
    User,
    /// New data arrived: keep the selected entry.
    Background,
}

struct Ui {
    app: App,
    sessions: Vec<SessionDoc>,
    live: Vec<LivePane>,
    index: Arc<Index>,
    host: Option<SharedHost>,
    config: Config,
    /// The user's home directory, left out when matching working directories.
    home: Option<String>,
    tx: Sender<Msg>,
    /// The host and facts each machine's entries are opened with.
    routes: Routes,
    /// One per remote being searched.
    workers: Vec<Worker>,
    /// Numbers the searches sent to workers; older results are dropped.
    debounce: Debounce,
    /// `(machine, session count)` of each loaded remote.
    remote_sessions: Vec<(String, usize)>,
}

impl Ui {
    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        rx: &Receiver<Msg>,
        remote_rx: &Receiver<RemoteMsg>,
    ) -> anyhow::Result<Option<(String, Vec<String>)>> {
        let mut dirty = true;
        let mut drawn_at = Instant::now();
        loop {
            // Redraw on changes, and now and then so ages stay current.
            if dirty || drawn_at.elapsed() >= REDRAW_EVERY {
                terminal.draw(|f| draw(f, &self.app, now()))?;
                dirty = false;
                drawn_at = Instant::now();
            }

            let mut wait = TICK;
            while event::poll(wait)? {
                wait = Duration::ZERO;
                dirty = true;
                let effect = match event::read()? {
                    Event::Key(key) => self.app.on_key(key),
                    Event::Paste(text) => self.app.on_paste(&text),
                    _ => continue,
                };
                match effect {
                    Effect::None => {}
                    Effect::Requery => self.requery(Reason::User),
                    Effect::Close => return Ok(None),
                    Effect::Open { hit, confirmed } => self.open(hit, confirmed),
                }
            }

            while let Ok(msg) = rx.try_recv() {
                dirty = true;
                if let Some(exit) = self.on_msg(msg) {
                    return Ok(match exit {
                        Exit::Done => None,
                        Exit::RunHere { cwd, argv } => Some((cwd, argv)),
                    });
                }
            }
            while let Ok(msg) = remote_rx.try_recv() {
                dirty = true;
                self.on_remote(msg);
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) -> Option<Exit> {
        match msg {
            Msg::Progress(done, total) => self.app.progress = Some((done, total)),
            Msg::Committed => self.reload_sessions(),
            Msg::Refreshed { result, retried } => {
                self.app.progress = None;
                match result.map(|ran| after_refresh(ran, retried)) {
                    Ok(AfterRefresh::Reload { enrich }) => {
                        self.reload_sessions();
                        if enrich && self.config.enrich.enabled {
                            spawn_enrich();
                        }
                    }
                    Ok(AfterRefresh::Retry(delay)) => spawn_refresh(
                        Arc::clone(&self.index),
                        self.config.clone(),
                        self.tx.clone(),
                        Some(delay),
                    ),
                    Err(e) => self.app.status = Some(format!("index refresh failed: {e:#}")),
                }
            }
            Msg::Live(Ok(live)) => {
                self.live = live;
                self.requery(Reason::Background);
                self.spawn_scrollback();
            }
            Msg::Live(Err(e)) => self.app.status = Some(format!("listing panes: {e:#}")),
            Msg::Scrollback(read) => {
                for (pane_id, text) in read {
                    if let Some(pane) = self.live.iter_mut().find(|p| p.pane_id == pane_id) {
                        pane.scrollback = Some(text);
                    }
                }
                self.requery(Reason::Background);
            }
            Msg::Opened(outcome) => return self.app.on_outcome(outcome),
        }
        None
    }

    fn on_remote(&mut self, msg: RemoteMsg) {
        match msg {
            RemoteMsg::Loaded {
                machine,
                sessions,
                reach,
            } => {
                self.routes.apply(&machine, &reach);
                self.app
                    .set_note(&machine, reach.note().map(str::to_string));
                self.remote_sessions.push((machine, sessions));
                self.app.total_sessions = self.total_sessions();
            }
            RemoteMsg::Results {
                machine,
                seq,
                entries,
                hits,
                note,
            } => {
                if self.debounce.is_current(seq) {
                    self.app.update_source(&machine, entries, hits);
                    self.app.set_note(&machine, note);
                }
            }
            RemoteMsg::Stopped { machine, note } => {
                self.routes.apply(&machine, &Reach::Failed(note.clone()));
                self.app.update_source(&machine, Vec::new(), Vec::new());
                self.app.set_note(&machine, Some(note));
                self.remote_sessions.retain(|(m, _)| *m != machine);
                self.app.total_sessions = self.total_sessions();
            }
        }
    }

    fn total_sessions(&self) -> usize {
        remote::total_sessions(
            self.sessions.len(),
            self.remote_sessions.iter().map(|(_, n)| *n),
            self.app.remotes_configured,
        )
    }

    /// Asks every remote's worker for the current query; their earlier
    /// results are cleared until the new ones arrive.
    fn search_remotes(&mut self) {
        self.app.clear_sources();
        if self.workers.is_empty() {
            return;
        }
        let request = SearchRequest {
            seq: self.debounce.next(),
            query: self.app.query.clone(),
            scope: self.app.scope,
            include_subagents: self.app.include_subagents,
        };
        for worker in &self.workers {
            // A worker that has stopped (its remote failed) is skipped.
            let _ = worker.requests.send(request.clone());
        }
    }

    /// Reads the sessions from the index again and updates the results.
    fn reload_sessions(&mut self) {
        match self.index.sessions() {
            Ok(sessions) => self.sessions = sessions,
            Err(e) => self.app.status = Some(format!("reading index: {e:#}")),
        }
        self.requery(Reason::Background);
    }

    fn requery(&mut self, reason: Reason) {
        let entries = build_entries(self.sessions.clone(), self.live.clone(), now());
        let hits = search(
            &entries,
            self.index.as_ref(),
            &self.app.query,
            self.app.scope,
            self.app.include_subagents,
            self.home.as_deref(),
        );
        self.app.total_sessions = self.total_sessions();
        match reason {
            Reason::User => {
                self.app.set_results(entries, hits);
                self.search_remotes();
            }
            Reason::Background => self.app.update_results(entries, hits),
        }
    }

    /// Opens the entry of row `hit` on a background thread, with the host
    /// and facts [`Routes::route`] gives for its machine.
    fn open(&mut self, hit: usize, confirmed: bool) {
        let Some(entry) = self.app.entry_at(hit).cloned() else {
            self.app.status = Some("that result is no longer available".to_string());
            return;
        };
        let route = match self.routes.route(entry.machine.as_deref()) {
            Ok(route) => route,
            Err(message) => {
                self.app.status = Some(message);
                return;
            }
        };
        self.app.begin_open();
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                let host = route.host.as_deref().map(|h| h as &dyn Host);
                open_entry(
                    &entry,
                    host,
                    &adapters(&config),
                    &route.facts,
                    confirmed,
                    now(),
                )
            }))
            .unwrap_or_else(|_| OpenOutcome::Failed("opening the session panicked".to_string()));
            let _ = tx.send(Msg::Opened(outcome));
        });
    }

    /// Reads the scrollback of every live pane without a session.
    fn spawn_scrollback(&self) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let panes: Vec<String> = self
            .app
            .entries
            .iter()
            .filter(|e| e.session.is_none())
            .filter_map(|e| e.live.as_ref().map(|l| l.pane_id.clone()))
            .collect();
        if panes.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let read = panes
                .into_iter()
                .filter_map(|id| {
                    let text = host.read_scrollback(&id, SCROLLBACK_LINES).ok()?;
                    Some((id, text))
                })
                .collect();
            let _ = tx.send(Msg::Scrollback(read));
        });
    }
}

/// Refreshes the index on a background thread, reporting progress and each
/// committed batch. `retry_after`: this is the retry of a lock-busy refresh,
/// started after that delay.
fn spawn_refresh(
    index: Arc<Index>,
    config: Config,
    tx: Sender<Msg>,
    retry_after: Option<Duration>,
) {
    thread::spawn(move || {
        if let Some(delay) = retry_after {
            thread::sleep(delay);
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut progress = |done, total| {
                let _ = tx.send(Msg::Progress(done, total));
            };
            let mut committed = || {
                let _ = tx.send(Msg::Committed);
            };
            index.refresh_notifying(&adapters(&config), &mut progress, &mut committed)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("refresh panicked")));
        let _ = tx.send(Msg::Refreshed {
            result: result.map(|stats| stats.is_some()),
            retried: retry_after.is_some(),
        });
    });
}

/// `<state_dir>/enrich.log`, created if missing and opened for appending;
/// emptied first if it is over 1 MiB.
pub fn open_enrich_log(state_dir: &Path) -> io::Result<File> {
    let path = state_dir.join("enrich.log");
    let too_big = fs::metadata(&path).is_ok_and(|m| m.len() > ENRICH_LOG_MAX);
    if too_big {
        OpenOptions::new().write(true).truncate(true).open(&path)?;
    }
    OpenOptions::new().create(true).append(true).open(&path)
}

/// Starts `hound enrich` detached from this process and its terminal, its
/// stderr (failure lines, which carry no conversation text) appended to
/// `enrich.log` in the state directory.
fn spawn_enrich() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let stderr = open_enrich_log(&config::state_dir()).map_or_else(|_| Stdio::null(), Stdio::from);
    let mut command = Command::new(exe);
    command
        .arg("enrich")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Ok(mut child) = command.spawn() {
        // Reap it if it finishes while the popup is still open.
        thread::spawn(move || child.wait());
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Raw mode and the alternate screen, undone on drop and on a panic in the
/// thread that entered them.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// The thread running the popup while the terminal is in raw mode.
static UI_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);
static PANIC_HOOK: Once = Once::new();

impl TerminalGuard {
    fn enter() -> anyhow::Result<TerminalGuard> {
        PANIC_HOOK.call_once(install_panic_hook);
        terminal::enable_raw_mode()?;
        set_ui_thread(Some(thread::current().id()));
        // From here on, a failure must still restore the terminal.
        let terminal = (|| -> anyhow::Result<_> {
            execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            Ok(Terminal::new(CrosstermBackend::new(io::stdout()))?)
        })();
        match terminal {
            Ok(terminal) => Ok(TerminalGuard { terminal }),
            Err(e) => {
                set_ui_thread(None);
                restore_terminal();
                Err(e)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        set_ui_thread(None);
        restore_terminal();
    }
}

fn set_ui_thread(id: Option<ThreadId>) {
    *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = id;
}

/// Restores the terminal before the panic message is printed, when the panic
/// is in the popup's thread. A panic in a background thread leaves the
/// terminal alone; the refresh and open threads, the remote workers and
/// their snapshot helpers catch theirs and report a failure (the panic
/// message itself is still printed by the previous hook).
fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let in_ui = UI_THREAD
            .try_lock()
            .is_ok_and(|id| *id == Some(thread::current().id()));
        if in_ui {
            restore_terminal();
        }
        previous(info);
    }));
}

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
}
