//! The popup's state and how keys and open outcomes change it. No I/O.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound_core::model::SessionKey;
use hound_core::open::OpenOutcome;
use hound_core::platform::Platform;
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

/// Consecutive printable key presses that make a segment a paste.
const BURST_MIN: usize = 3;

/// The character a key types into the query, if it types one: a character
/// without Ctrl or Alt (Shift is fine), or on Windows also one with both
/// Ctrl and Alt. Windows reports AltGr as Left Ctrl + Right Alt, and
/// crossterm passes that on as CONTROL | ALT, so `@`, `\`, `{` on many
/// European layouts and every Polish diacritic arrive that way. A control
/// character is never text.
fn typed_char(key: &KeyEvent, platform: Platform) -> Option<char> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    let text = match platform {
        Platform::Unix => !key.modifiers.intersects(ctrl_alt),
        Platform::Windows => {
            !key.modifiers.intersects(ctrl_alt)
                || (key.modifiers.contains(ctrl_alt) && !c.is_control())
        }
    };
    text.then_some(c)
}

/// A key press of a character it types (see [`typed_char`]).
fn printable(key: &KeyEvent, platform: Platform) -> Option<char> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    typed_char(key, platform)
}

/// A key release or repeat: inside a segment it neither counts nor ends it.
fn not_a_press(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Press)
}

/// A segment's key presses: `Some(c)` a printable character, `None` Enter.
/// `None` for an event that cannot be part of a segment.
fn segment_press(event: &Event, platform: Platform) -> Option<Option<char>> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match printable(key, platform) {
        Some(c) => Some(Some(c)),
        None if key.code == KeyCode::Enter => Some(None),
        None => None,
    }
}

/// How many events from the start of `events` form a segment: printable
/// and Enter presses, with the releases and repeats among and after them. A
/// segment starts at a press; 0 if `events` does not start with one.
fn segment_len(events: &[Event], platform: Platform) -> usize {
    if events
        .first()
        .and_then(|event| segment_press(event, platform))
        .is_none()
    {
        return 0;
    }
    events
        .iter()
        .take_while(|event| not_a_press(event) || segment_press(event, platform).is_some())
        .count()
}

/// The segment's presses, releases and repeats left out.
fn presses(segment: &[Event], platform: Platform) -> Vec<Option<char>> {
    segment
        .iter()
        .filter_map(|event| segment_press(event, platform))
        .collect()
}

/// Whether a segment's presses hold at least [`BURST_MIN`] printable
/// characters in a row.
fn has_burst(presses: &[Option<char>]) -> bool {
    presses
        .split(Option::is_none)
        .any(|chars| chars.len() >= BURST_MIN)
}

/// The text of pasted presses: each Enter a space between the characters
/// around it; Enters at the start or end are dropped.
fn pasted_text(presses: &[Option<char>]) -> String {
    let mut text = String::new();
    let mut enters = 0;
    for press in presses {
        match press {
            Some(c) => {
                if !text.is_empty() {
                    text.extend(std::iter::repeat_n(' ', enters));
                }
                enters = 0;
                text.push(*c);
            }
            None => enters += 1,
        }
    }
    text
}

/// Interprets a batch of terminal events read without waiting, in order.
/// Some terminals (crossterm on Windows) report pasted text as separate key
/// presses, a line break as Enter, and Enter opens the selected entry.
///
/// The batch is cut into segments: runs of printable presses (characters
/// without Ctrl or Alt, and on Windows AltGr characters) and Enter presses, with any key releases and
/// repeats among them; every other key or event ends a segment and passes
/// through on its own. A segment is pasted if it has at least three
/// printable presses in a row, or an Enter press anywhere before the
/// batch's last printable press (so an Enter followed by text, even past a
/// pasted Tab, never acts), or if an earlier segment of the batch was pasted
/// (so a short line after a pasted Tab is still text). A pasted segment becomes one
/// [`Input::Pasted`] (none if it holds only Enters): each Enter a space
/// between the characters around it, Enters at its start or end dropped, its
/// releases and repeats dropped. Other segments pass through as keys. A
/// bracketed paste becomes [`Input::Pasted`] with its text unchanged.
///
/// Accepted costs, all failing safe (nothing opens): a fast typist whose
/// three characters and Enter, or Enter and any later character, land in
/// one batch (only when the popup's thread stalls) has that Enter dropped;
/// an Enter typed after another key in a paste's batch (`a b c Down Enter`)
/// is dropped; a paste split across two batches can still deliver the Enter
/// of a short last line.
pub fn split_paste_bursts(batch: &[Event]) -> Vec<Input> {
    split_paste_bursts_on(batch, Platform::current())
}

/// [`split_paste_bursts`] for the keys of `platform`.
pub fn split_paste_bursts_on(batch: &[Event], platform: Platform) -> Vec<Input> {
    let mut inputs = Vec::new();
    let mut pasting = false;
    let last_text = batch
        .iter()
        .rposition(|event| matches!(segment_press(event, platform), Some(Some(_))));
    let text_follows = |at: usize| last_text.is_some_and(|last| at < last);
    let mut i = 0;
    while i < batch.len() {
        let len = segment_len(&batch[i..], platform);
        if len == 0 {
            inputs.push(match &batch[i] {
                Event::Key(key) => Input::Key(*key),
                Event::Paste(text) => Input::Pasted(text.clone()),
                other => Input::Other(other.clone()),
            });
            i += 1;
            continue;
        }
        let segment = &batch[i..i + len];
        let presses = presses(segment, platform);
        let early_enter = segment
            .iter()
            .enumerate()
            .any(|(j, event)| segment_press(event, platform) == Some(None) && text_follows(i + j));
        if pasting || early_enter || has_burst(&presses) {
            pasting = true;
            let text = pasted_text(&presses);
            if !text.is_empty() {
                inputs.push(Input::Pasted(text));
            }
        } else {
            // A segment holds only key events.
            inputs.extend(segment.iter().filter_map(|event| match event {
                Event::Key(key) => Some(Input::Key(*key)),
                _ => None,
            }));
        }
        i += len;
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
    /// Whose keys `on_key` reads (see [`typed_char`]).
    platform: Platform,
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
        App::new_on(scope, include_subagents, Platform::current())
    }

    /// [`App::new`] reading keys as `platform` reports them.
    pub fn new_on(scope: Scope, include_subagents: bool, platform: Platform) -> App {
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
            platform,
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
        // Before the Ctrl bindings: on Windows a Ctrl+Alt character is AltGr
        // text, not a binding.
        if let Some(c) = typed_char(&key, self.platform) {
            self.query.push(c);
            return Effect::Requery;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
