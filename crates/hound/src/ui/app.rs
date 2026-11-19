//! The popup's state and how keys and open outcomes change it. No I/O.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound_core::model::SessionKey;
use hound_core::open::OpenOutcome;
use hound_core::search::{Entry, Hit, Scope, merge_hits};

/// What the event loop must do after a key.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    /// The query, scope or subagent setting changed: search again.
    Requery,
    /// Open the entry of `rows()[hit]` (see [`App::entry_at`]); `confirmed`
    /// answers an earlier `NeedsConfirm`.
    Open {
        hit: usize,
        confirmed: bool,
    },
    Close,
}

/// One input for the popup, from a batch of terminal events.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Key(KeyEvent),
    /// Text pasted into the terminal: a bracketed paste, or a burst of key
    /// presses (see [`split_paste_bursts`]).
    Pasted(String),
    /// Any other event (resize, focus, mouse): nothing for the popup to do
    /// beyond redrawing.
    Other(Event),
}

/// Key presses in a burst that make it a paste.
const BURST_MIN: usize = 3;

/// A key press of a character typed without Ctrl or Alt (Shift is fine).
fn printable(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if key.kind == KeyEventKind::Press
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// A key release or repeat: inside a burst it neither counts nor ends it.
fn not_a_press(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Press)
}

/// Interprets a batch of terminal events read without waiting, in order.
/// Some terminals (crossterm on Windows) report pasted text as separate key
/// presses, a line break as Enter. So a run of at least three printable key
/// presses is taken as pasted: the run and every printable character and
/// Enter after it, up to the first other key or event, become one
/// [`Input::Pasted`], each Enter a space between the characters around it
/// (Enters at its end are dropped). Key releases and repeats within such a
/// run are dropped; elsewhere every event passes through, a bracketed paste
/// as [`Input::Pasted`] with its text unchanged.
pub fn split_paste_bursts(batch: &[Event]) -> Vec<Input> {
    let mut inputs = Vec::new();
    let mut i = 0;
    while i < batch.len() {
        let is_printable = |e: &Event| matches!(e, Event::Key(key) if printable(key).is_some());
        let presses = batch[i..]
            .iter()
            .filter(|e| !not_a_press(e))
            .take_while(|e| is_printable(e))
            .count();
        if is_printable(&batch[i]) && presses >= BURST_MIN {
            let mut text = String::new();
            let mut enters = 0;
            while let Some(event) = batch.get(i) {
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(c) = printable(key) {
                            text.extend(std::iter::repeat_n(' ', enters));
                            enters = 0;
                            text.push(c);
                        } else if key.code == KeyCode::Enter {
                            enters += 1;
                        } else {
                            break;
                        }
                    }
                    event if not_a_press(event) => {}
                    _ => break,
                }
                i += 1;
            }
            inputs.push(Input::Pasted(text));
            continue;
        }
        inputs.push(match &batch[i] {
            Event::Key(key) => Input::Key(*key),
            Event::Paste(text) => Input::Pasted(text.clone()),
            other => Input::Other(other.clone()),
        });
        i += 1;
    }
    inputs
}

/// Why the popup ends after an open.
#[derive(Debug, PartialEq)]
pub enum Exit {
    /// A pane was focused or a session resumed elsewhere.
    Done,
    /// Leave the TUI and run `argv` in `cwd` in this terminal.
    RunHere { cwd: String, argv: Vec<String> },
}

/// How long to wait before retrying a refresh that found `writer.lock` held.
const LOCK_RETRY: Duration = Duration::from_millis(500);

/// What the popup does when an index refresh ends without an error.
#[derive(Debug, PartialEq)]
pub enum AfterRefresh {
    /// Reload sessions from the index, then start enrichment if `enrich`
    /// (and it is enabled).
    Reload { enrich: bool },
    /// Another process holds `writer.lock`: refresh again after this long.
    Retry(Duration),
}

/// `ran`: the refresh ran (`false`: another process held `writer.lock`).
/// `retried`: it was already the retry. A lock-busy refresh is retried once;
/// either way the sessions are then reloaded, so what the other process wrote
/// shows up.
pub fn after_refresh(ran: bool, retried: bool) -> AfterRefresh {
    match (ran, retried) {
        (true, _) => AfterRefresh::Reload { enrich: true },
        (false, false) => AfterRefresh::Retry(LOCK_RETRY),
        (false, true) => AfterRefresh::Reload { enrich: false },
    }
}

/// One machine's results: the entries given to the search and the hits it
/// returned.
#[derive(Clone, Debug)]
pub struct Source {
    /// `None` for this machine.
    pub machine: Option<String>,
    pub entries: Vec<Entry>,
    pub hits: Vec<Hit>,
}

pub struct App {
    pub query: String,
    pub scope: Scope,
    pub include_subagents: bool,
    /// Index into [`App::rows`].
    pub selected: usize,
    /// This machine's hits, indexing [`App::entries`].
    pub hits: Vec<Hit>,
    /// This machine's entries.
    pub entries: Vec<Entry>,
    /// Remote machines are configured: rows name their machine.
    pub remotes_configured: bool,
    /// One-line message in the footer.
    pub status: Option<String>,
    /// `(done, total)` of a running index refresh.
    pub progress: Option<(usize, usize)>,
    pub total_sessions: usize,
    /// Each remote's latest results, in the order the remotes first appeared.
    remotes: Vec<Source>,
    /// Footer note per remote, in the order the remotes were first noted; a
    /// cleared note keeps its place.
    notes: Vec<(String, Option<String>)>,
    /// The entry an [`App::ask_confirm`] was for. The next Enter confirms
    /// only while this entry is still the selected one.
    confirm_target: Option<Identity>,
    /// An open is in flight for this entry (`None` inside if it had no
    /// identity); Enter does nothing until its outcome arrives.
    opening: Option<Option<Identity>>,
    /// Esc or Ctrl+C was pressed during an open: close once it finishes.
    close_requested: bool,
}

const OPENING: &str = "opening…";
const CLOSING: &str = "closing when the open finishes (Esc again to quit now)";
const SELECTION_CHANGED: &str = "not opened: the selection changed while it was being checked";

/// What stays the same about an entry across rebuilds: its machine and its
/// session key, or for a session-less pane its pane ID. The same session on
/// two machines is two identities.
#[derive(Clone, Debug, PartialEq)]
struct Identity {
    machine: Option<String>,
    key: Key,
}

#[derive(Clone, Debug, PartialEq)]
enum Key {
    Session(SessionKey),
    Pane(String),
}

fn identity(entry: &Entry) -> Option<Identity> {
    let key = match (&entry.session, &entry.live) {
        (Some(s), _) => Key::Session(s.key.clone()),
        (None, Some(l)) => Key::Pane(l.pane_id.clone()),
        (None, None) => return None,
    };
    Some(Identity {
        machine: entry.machine.clone(),
        key,
    })
}

/// `hits` without any that point past `entries`.
fn valid_hits(entries: &[Entry], mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.retain(|h| h.entry < entries.len());
    hits
}

impl App {
    pub fn new(scope: Scope, include_subagents: bool) -> App {
        App {
            query: String::new(),
            scope,
            include_subagents,
            selected: 0,
            hits: Vec::new(),
            entries: Vec::new(),
            remotes_configured: false,
            status: None,
            progress: None,
            total_sessions: 0,
            remotes: Vec::new(),
            notes: Vec::new(),
            confirm_target: None,
            opening: None,
            close_requested: false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Effect {
        if key.kind == KeyEventKind::Release {
            return Effect::None;
        }
        if key.code != KeyCode::Enter {
            self.confirm_target = None;
            if self.opening.is_none() {
                self.status = None;
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                Effect::Requery
            }
            KeyCode::Char('s') if ctrl => {
                self.include_subagents = !self.include_subagents;
                Effect::Requery
            }
            KeyCode::Char('n') if ctrl => self.move_selection(1),
            KeyCode::Char('p') if ctrl => self.move_selection(-1),
            KeyCode::Char('c') if ctrl => self.close(),
            KeyCode::Char(c) if plain => {
                self.query.push(c);
                Effect::Requery
            }
            KeyCode::Backspace => {
                self.query.pop();
                Effect::Requery
            }
            KeyCode::Tab => {
                self.scope = self.scope.next();
                Effect::Requery
            }
            KeyCode::BackTab => {
                self.scope = self.scope.prev();
                Effect::Requery
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Enter => {
                let count = self.row_count();
                if count == 0 || self.opening.is_some() {
                    return Effect::None;
                }
                let target = self.confirm_target.take();
                let confirmed = target.is_some() && target == self.selected_identity();
                Effect::Open {
                    hit: self.selected.min(count - 1),
                    confirmed,
                }
            }
            KeyCode::Esc => self.close(),
            _ => Effect::None,
        }
    }

    /// One input from [`split_paste_bursts`].
    pub fn on_input(&mut self, input: &Input) -> Effect {
        match input {
            Input::Key(key) => self.on_key(*key),
            Input::Pasted(text) => self.on_paste(text),
            Input::Other(_) => Effect::None,
        }
    }

    /// Text pasted into the terminal (bracketed paste): appended to the query
    /// with every control character, newlines included, removed, so a pasted
    /// line break never acts as Enter.
    pub fn on_paste(&mut self, text: &str) -> Effect {
        let before = self.query.len();
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        if self.query.len() == before {
            return Effect::None;
        }
        self.confirm_target = None;
        if self.opening.is_none() {
            self.status = None;
        }
        Effect::Requery
    }

    /// New results for this machine after a user-driven requery; the
    /// selection goes back to the top. A pending confirmation is dropped,
    /// since the hit it was for may have moved.
    pub fn set_results(&mut self, entries: Vec<Entry>, hits: Vec<Hit>) {
        self.hits = valid_hits(&entries, hits);
        self.entries = entries;
        self.selected = 0;
        self.disarm_confirm();
    }

    /// This machine's results replaced by background work (refresh, host
    /// snapshot, scrollback). See [`App::follow_selection`].
    pub fn update_results(&mut self, entries: Vec<Entry>, hits: Vec<Hit>) {
        self.follow_selection(|app| {
            app.hits = valid_hits(&entries, hits);
            app.entries = entries;
        });
    }

    /// The remote `machine`'s results, built for it (each entry's `machine`
    /// is `machine`), replace its earlier ones. See
    /// [`App::follow_selection`].
    pub fn update_source(&mut self, machine: &str, entries: Vec<Entry>, hits: Vec<Hit>) {
        self.follow_selection(|app| {
            let hits = valid_hits(&entries, hits);
            match app
                .remotes
                .iter_mut()
                .find(|s| s.machine.as_deref() == Some(machine))
            {
                Some(source) => {
                    source.entries = entries;
                    source.hits = hits;
                }
                None => app.remotes.push(Source {
                    machine: Some(machine.to_string()),
                    entries,
                    hits,
                }),
            }
        });
    }

    /// On a user-driven requery: every remote's rows disappear until its
    /// fresh results arrive through [`App::update_source`]. Notes stay. See
    /// [`App::follow_selection`].
    pub fn clear_sources(&mut self) {
        self.follow_selection(|app| {
            for source in &mut app.remotes {
                source.entries.clear();
                source.hits.clear();
            }
        });
    }

    /// Sets or, with `None`, removes the footer note for `machine`.
    pub fn set_note(&mut self, machine: &str, note: Option<String>) {
        match self.notes.iter_mut().find(|(m, _)| m == machine) {
            Some((_, slot)) => *slot = note,
            None if note.is_some() => self.notes.push((machine.to_string(), note)),
            None => {}
        }
    }

    /// The footer notes as `(machine, note)`, in a stable order.
    pub fn notes(&self) -> impl Iterator<Item = (&str, &str)> {
        self.notes
            .iter()
            .filter_map(|(m, n)| Some((m.as_str(), n.as_deref()?)))
    }

    /// Every machine's hits in one ranked list, each with its entry: this
    /// machine's and every remote's, merged as [`merge_hits`] orders them.
    /// [`App::selected`] and `Effect::Open { hit }` index this list.
    pub fn rows(&self) -> Vec<(&Entry, &Hit)> {
        let lists = self.lists();
        self.order(&lists)
            .into_iter()
            .map(|(l, h)| {
                let (entries, hits) = lists[l];
                (&entries[hits[h].entry], &hits[h])
            })
            .collect()
    }

    /// The entry of `rows()[row]`.
    pub fn entry_at(&self, row: usize) -> Option<&Entry> {
        let lists = self.lists();
        let (l, h) = *self.order(&lists).get(row)?;
        let (entries, hits) = lists[l];
        entries.get(hits[h].entry)
    }

    /// The entry of the selected row.
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entry_at(self.selected)
    }

    /// Applies `change` to the results, as background work does. The
    /// selection stays on the same entry (same machine, same session or
    /// pane) wherever it now sits among the rows; if that entry is gone, the
    /// index is clamped to the new rows. A pending confirmation survives only
    /// if its entry stays selected.
    fn follow_selection(&mut self, change: impl FnOnce(&mut App)) {
        let before = self.selected_identity();
        change(self);
        let found = before.as_ref().and_then(|id| {
            self.rows()
                .iter()
                .position(|(e, _)| identity(e).as_ref() == Some(id))
        });
        match found {
            Some(i) => self.selected = i,
            None => self.selected = self.selected.min(self.row_count().saturating_sub(1)),
        }
        if self.confirm_target.is_some() && self.confirm_target != self.selected_identity() {
            self.disarm_confirm();
        }
    }

    /// Each source as (entries, hits): this machine first, then the remotes.
    fn lists(&self) -> Vec<(&[Entry], &[Hit])> {
        std::iter::once((self.entries.as_slice(), self.hits.as_slice()))
            .chain(
                self.remotes
                    .iter()
                    .map(|s| (s.entries.as_slice(), s.hits.as_slice())),
            )
            .collect()
    }

    /// The rows as (list index, hit index) into `lists`. A single list with
    /// hits keeps the order it came in (the search already ranked it), so
    /// rows without remote results are exactly the hits as given.
    fn order(&self, lists: &[(&[Entry], &[Hit])]) -> Vec<(usize, usize)> {
        let valid =
            |(entries, hits): &(&[Entry], &[Hit])| hits.iter().all(|h| h.entry < entries.len());
        let with_hits = lists.iter().filter(|(_, hits)| !hits.is_empty()).count();
        if with_hits > 1 && lists.iter().all(valid) {
            return merge_hits(lists);
        }
        lists
            .iter()
            .enumerate()
            .flat_map(|(l, (entries, hits))| {
                hits.iter()
                    .enumerate()
                    .filter(|(_, h)| h.entry < entries.len())
                    .map(move |(h, _)| (l, h))
            })
            .collect()
    }

    fn row_count(&self) -> usize {
        let lists = self.lists();
        self.order(&lists).len()
    }

    /// Shows `message`; the next Enter yields `Open { confirmed: true }` if
    /// the entry selected now is still selected then. With nothing
    /// selected, only the message is shown.
    pub fn ask_confirm(&mut self, message: String) {
        self.status = Some(message);
        self.confirm_target = self.selected_identity();
    }

    /// An open of the selected entry has been started; Enter is ignored
    /// until [`App::on_outcome`].
    pub fn begin_open(&mut self) {
        self.opening = Some(self.selected_identity());
        self.status = Some(OPENING.to_string());
    }

    /// Applies the outcome of an open; `Some` if the popup should end. A
    /// close requested during the open wins over any outcome. A request to
    /// confirm is armed only if the opened entry is still the selected one.
    pub fn on_outcome(&mut self, outcome: OpenOutcome) -> Option<Exit> {
        let opened = self.opening.take();
        if self.close_requested {
            return Some(Exit::Done);
        }
        if self.status.as_deref() == Some(OPENING) {
            self.status = None;
        }
        match outcome {
            OpenOutcome::Focused | OpenOutcome::Resumed => Some(Exit::Done),
            OpenOutcome::RunHere { cwd, argv } => Some(Exit::RunHere { cwd, argv }),
            OpenOutcome::NeedsConfirm(message) => {
                let still_selected = opened
                    .flatten()
                    .is_some_and(|id| self.selected_identity().as_ref() == Some(&id));
                if still_selected {
                    self.ask_confirm(message);
                } else {
                    self.status = Some(SELECTION_CHANGED.to_string());
                }
                None
            }
            OpenOutcome::Failed(message) => {
                self.status = Some(message);
                None
            }
        }
    }

    /// Esc or Ctrl+C. During an open the close waits for its outcome, so the
    /// host is not left half-done; pressing it again quits at once.
    fn close(&mut self) -> Effect {
        if self.opening.is_none() || self.close_requested {
            return Effect::Close;
        }
        self.close_requested = true;
        self.status = Some(CLOSING.to_string());
        Effect::None
    }

    fn selected_identity(&self) -> Option<Identity> {
        identity(self.selected_entry()?)
    }

    /// Drops a pending confirmation and its message.
    fn disarm_confirm(&mut self) {
        if self.confirm_target.take().is_some() {
            self.status = None;
        }
    }

    fn move_selection(&mut self, delta: isize) -> Effect {
        let last = self.row_count().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        Effect::None
    }
}
