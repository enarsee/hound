# hound — search everywhere for agent sessions in herdr

Status: v1 (phase A) built on branch `v1`; see "As built" at the end for where
the build differs from this design. Date: 2026-11-17. `hound` is a working name.

## Intent

With many agents across several herdr workspaces, plus weeks of closed sessions,
finding "the one where we did X" means clicking through tabs or remembering which
workspace it was in. hound is the IntelliJ Shift+Shift equivalent: one hotkey,
one box, type what you remember, Enter lands you in that session — whether it is
open in herdr right now, closed, or was never started from herdr.

## Goals

1. Search across every agent session on this machine by workspace / tab / agent
   name, session title, working directory, and full conversation text.
2. Multi-harness: Claude Code and pi from day one, opencode by adapter, others
   addable without touching the core.
3. Read the harnesses' session files directly (not just what herdr shows).
4. Enter on an open session focuses its pane. Enter on a closed or non-herdr
   session resumes it in a new herdr tab immediately.
5. Three scopes — Active, Old, All — each with its own hotkey, and cyclable
   inside the popup.
6. Optional LLM enrichment at index time: summary, category, tags.
7. A common core that is not tied to herdr. herdr is the first "host"; the same
   TUI also runs standalone in a plain terminal for people who do not use herdr,
   and other hosts or frontends can be added later without changing the core.
8. Starts as index-on-open (phase A); grows into an always-warm background
   service (phase C) without rewriting the UI or index.

## Non-goals (v1)

- Remote herdr machines (`--machine`), other hosts' session files.
- Semantic / embedding search.
- Editing, deleting or exporting sessions.
- Harnesses beyond Claude Code, pi, opencode.
- macOS / Windows.
- A message preview pane (snippets only in v1).

## Anti-goals

- Opening a second copy of a session that is already live in herdr. It must
  jump to the existing pane instead.
- The popup ever waiting on indexing or on the LLM. Stale results now beat
  fresh results later.
- Writing to, moving or locking any harness session file. Read-only, always.
- Sending conversation content off this machine unless explicitly configured.

## Constraints and facts found on a real setup

- herdr 0.9.3 (the version this design was written against). Socket API
  (also reachable via the `herdr` CLI, JSON out) provides: `agent.list`,
  `pane.list`, `tab.list`, `workspace.list`,
  `pane.read`, `pane.focus`, `agent.focus`, `tab.create`, `agent.start`,
  `plugin.pane.open`, `events.subscribe`. `herdr api schema --json` is the
  authority.
- `agent.list` rows carry `agent` (harness kind), `agent_session.value`
  (harness session ID), `pane_id`, `tab_id`, `workspace_id`, `name`,
  `terminal_title_stripped`, `cwd`, `agent_status`. The session ID is the join
  key between a live pane and a session file.
- herdr `config.toml` supports `[[keys.command]]` with `type = "popup"`,
  `width`, `height`. Plugins (`herdr-plugin.toml`) support `[[actions]]`,
  `[[panes]] placement = "popup"`, `[[startup]]`, `[[events]]`.
- Claude Code: `~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl` for
  top-level sessions, with subagent transcripts at
  `<session-uuid>/subagents/agent-*.jsonl`. A session can record more than one
  `cwd`. One JSON record per line; relevant `type`s are `user`, `assistant`,
  `ai-title` (field `aiTitle`); records carry `cwd`, `timestamp`, `sessionId`,
  `isSidechain`. Much of the bulk is noise (`api-request-blob`, `attachment`,
  `file-history-snapshot`, tool results). Resume: `claude --resume <id>`.
- pi: `~/.pi/agent/sessions/<cwd-slug>/<timestamp>_<uuid>.jsonl`.
  First line is `{"type":"session","id","cwd","timestamp"}`; then `message`
  records with `message.role`. Resume: `pi --session <id>`.
- opencode: not available to test against. Adapter is written from its
  documented storage layout and is unverified until it is installed.
- A local Ollama endpoint (`localhost:11434`) is the expected LLM for
  enrichment.
- A Rust toolchain (rustup) is a prerequisite.
- A true double-tap-Shift binding is probably not expressible (herdr bindings
  are chords; terminals rarely report bare modifier presses).

## Approaches considered

- **A. Plugin/popup with an on-disk index, refreshed incrementally on open.**
  One binary, readable snippets, real ranking. First run indexes for a few
  seconds; later opens are instant. Cost: a parser per harness and an index to
  maintain.
- **B. No index: ripgrep + a fuzzy picker in a script.** An afternoon of work
  and fast enough for a modest history, but it matches raw JSONL (tool output, blobs), has
  no title-over-content ranking, and degrades as history grows.
- **C. Background service with an always-warm index; popup is a thin client.**
  Always current, fastest to open. Cost: a second long-running process to
  install and debug; no benefit over A at today's size.

**Chosen (by the user): A, built so it transitions to C.** Plus optional LLM
enrichment at index time.

## Design

### Shape

A Cargo workspace with two crates (decided by the user: keep a common core so
a standalone TUI can exist for non-herdr users):

- `hound-core` — adapters, index, search and ranking, the decision of what
  Enter does, enrichment, and a `Host` trait describing the terminal
  environment (list live panes, focus one, open a new one). It contains no
  herdr code and no terminal UI.
- `hound` — the TUI, the command line, and the herdr implementation of `Host`.

With no host detected, the same TUI runs standalone: there are no live panes,
so every session is "old", and Enter leaves the TUI and resumes the session in
the current terminal. Wherever this document says "herdr" for listing, focusing
or opening panes, read "the host"; herdr is the only host in v1.

One binary, `hound`, with subcommands:

| Command | Does |
|---|---|
| `hound ui --scope active\|old\|all` | The popup TUI. Phase A: kicks off an incremental index refresh in a background thread on start. |
| `hound index` | Incremental index refresh, then exit. |
| `hound enrich` | Run pending LLM enrichment, then exit. |
| `hound serve` | Phase C: watch + index + enrich continuously. |

The A→C seam: indexing and enrichment are library functions over the same
index directory. In phase A the UI calls them itself; in phase C `serve` does and
the UI skips its own refresh when the service's heartbeat is fresh. The UI
and schema do not change between phases.

### Harness adapters

One trait, one implementation per harness:

- `discover()` → session files on disk.
- `parse(path, from_offset)` → session metadata (id, cwd, title, started,
  last-active) and a stream of cleaned messages (role, text, sequence).
- `resume_args(session)` → argv to resume it (`--resume <id>` etc.).
- `herdr_kind()` → the `--kind` value herdr uses (`claude`, `pi`, `opencode`).

Cleaning rules (what gets indexed): user-typed text, assistant prose, tool
names, and short tool inputs (shell commands, file paths). Not indexed: tool
outputs, thinking, system prompts, attachments, request blobs, sidechain
(subagent) records. Title: the harness's own title if it records one (Claude's
last `ai-title`), else the first user prompt truncated.

A session that records several cwds: all of them are searchable; the resume
cwd is the first one the session recorded (the directory it was started in),
since harnesses look sessions up by launch directory.

Subagent transcripts (decided by the user: findable, but filterable and
configurable): a Claude session's `subagents/*.jsonl` files are indexed as
part of the parent session, cleaned by the same rules, stored with a
`subagent` marker, and ranked below matches in the main conversation. They
never appear as results of their own; a snippet that comes from a subagent is
labelled as such. Incremental refresh therefore tracks an offset per file,
with several files per session.

- Config: `index_subagents` (default true) controls whether they are indexed
  at all; `search_subagents` (default true) sets the popup's starting state.
- Popup: a toggle key (proposed `ctrl+s`) includes or excludes subagent
  matches for the current search, with the state shown in the header.

### Index

A Tantivy index directory at `~/.local/state/hound/index/` (decided by
the user: Tantivy alone, chosen over SQLite FTS5 for typo-tolerant matching).
No second store. Two kinds of document:

- Session document, one per session: harness, session_id, cwd(s), title,
  started_at, updated_at, summary, category, tags, plus stored bookkeeping
  (per-file path, size, indexed offset, head hash, enriched-at offset).
  Replaced (delete by key, then add) whenever any of it changes.
- Message document, one per cleaned message: session key, role, seq, subagent
  marker, text. Ranking via BM25, snippets via Tantivy's snippet generator,
  typo tolerance via fuzzy term queries on longer terms.

A small `state.json` beside the index holds schema version and the service
heartbeat. One writer at a time (the UI's refresh thread in phase A, `serve`
in phase C; a lock file decides who), any number of readers.

`hound debug <session>` dumps what is indexed for a session, since there is
no generic shell for inspecting the index.

Incremental refresh: session files are append-only, so for each file compare
size to `indexed_offset` and parse only the tail. If the file shrank or
`head_hash` (hash of the first bytes) changed, reindex that file from scratch.
Files that disappeared are dropped. A schema-version bump rebuilds everything.

### Live state

On open (v1: once; a refresh timer while open is deferred) hound fetches `workspace.list`,
`tab.list` and `agent.list`/`pane.list` from herdr and joins agents to indexed
sessions on (harness, session ID).

- **Active** = sessions with a live herdr pane, plus live panes with no session
  file (plain shells, unsupported agents). The latter are searchable by name,
  title, cwd and their current scrollback (`pane.read`, recent-unwrapped,
  fetched at open, never stored).
- **Old** = indexed sessions with no live herdr pane. This includes sessions
  started outside herdr. A session whose file changed in the last couple of
  minutes but has no herdr pane is flagged "may be running elsewhere".
- **All** = both.

### Search and ranking

One query box. Whitespace-separated terms, all must match, last term is a
prefix match. Matching runs two ways and is merged:

1. Match over the short fields (a term matches a field if it appears in it,
   or, for terms of 5+ characters, is within one typo of a word in it): workspace label, tab label, agent name,
   title, cwd, harness, category, tags, summary.
2. Full-text search over conversation text.

"All terms must match" is judged per session, across everything: one term may
hit the title and another a message, or two different messages. So the query
layer intersects sessions per term rather than running one full-text match per
message row. The snippet shown is the best-ranked matching message.

Order: short-field matches before content-only matches; within a tier, live
before closed, then most recently active. Empty query lists the scope by
recency (live first) — so the popup is also a plain switcher.

### The popup

```
┌ hound ─ [Active] Old  All ───────────────────── 44 sessions ┐
│ > depot signal                                              │
├─────────────────────────────────────────────────────────────┤
│ ● claude  Depot Leap new listing alerts     Billing › t8    │
│     …send to the Ops Alerts Signal group…                   │
│ ○ claude  depot-leap systemd timer          3d  tools-dev   │
│     …retry failure alert when Signal send hangs…            │
│ ○ pi      scraper retries                   2w  ledgerapp   │
└─ ⏎ open   tab scope   esc close ───────────── indexing 3/40 ┘
```

- `●` live in herdr (with agent status colour), `○` closed.
- Second line: best matching snippet; with no query, the LLM summary if one
  exists.
- Tab / Shift+Tab cycles scope. Up/Down or Ctrl+N/P moves. Esc closes.
- While the first index or a refresh is running, results appear as they are
  ready and the footer shows progress.

### Enter

- **Live session** → focus its workspace, tab and pane; close the popup.
- **Closed session** →
  1. Re-check herdr that it has not gone live since the list was built; if it
     has, focus it instead.
  2. If the session's cwd no longer exists, show the error in the popup and do
     nothing.
  3. If the session is flagged "may be running elsewhere", the first Enter
     shows a one-line warning in the footer (two processes would append to the
     same file) and a second Enter resumes anyway. Detection is the file's
     modification time only; no process inspection.
  4. Pick the workspace: the one with the most panes whose cwd equals the
     session's cwd, else longest shared path prefix, else the focused workspace.
  5. Create a tab there with the session's cwd, start the harness in its root
     pane with the adapter's resume args (`agent.start --kind K -- <args>`),
     focus the tab, close the popup.

### Hotkeys

Three bindings, one per scope, each opening the popup at ~80% × 80%. Proposed
defaults: `prefix+space` (All), `prefix+a` (Active), `prefix+shift+o` (Old; `prefix+o`
is taken by a herdr default) — subject
to not colliding with existing bindings. Installed either as plugin actions (if
plugin actions can be key-bound) or as `[[keys.command]] type = "popup"`
entries in herdr's `config.toml`. hound ships `hound install` that prints the
snippet; it does not edit herdr's config itself.

### Packaging

hound is a herdr plugin and also a plain binary.

- Layout: one Cargo project with a `herdr-plugin.toml` at its root. The
  manifest declares three actions (search All / Active / Old), one popup pane
  that runs `hound ui`, a `[[build]]` step, and in phase C a `[[startup]]`
  entry for `hound serve`.
- Local install (v1): `cargo build --release`, then `herdr plugin link
  <absolute path to the repo>`. herdr runs the binary from the checkout;
  rebuilding is the whole upgrade.
- Other machines (later, only if wanted): `herdr plugin install
  <owner>/<repo>[/subdir]` clones the repo and runs the `[[build]]` step. So
  that a Rust toolchain is not needed there, the build step is a script that
  downloads a prebuilt static binary for the platform from the repo's releases
  and falls back to `cargo build --release`. That needs a CI job producing
  release binaries; not built in v1.
- Standalone: the same binary works outside herdr (`cargo install --path .`)
  for searching and printing resume commands.
- Output is a single static binary with no runtime dependencies. State lives
  in `~/.local/state/hound/`, config in `~/.config/hound/config.toml`; neither
  is inside the plugin directory, so reinstalling keeps the index.

### LLM enrichment (optional, off by default)

- Config at `~/.config/hound/config.toml`: `enabled`, an OpenAI-compatible
  `base_url`, `model`, optional API key env var name, and the category list.
  The shipped example points at local Ollama.
- For each session that is new or has grown substantially since last
  enrichment and has been quiet for 10+ minutes, send a bounded digest (first
  few user prompts + last several turns, capped to a token budget) and ask for
  JSON: one-line summary, one category from the configured list, up to 5 tags.
- Results are stored on the session row, shown in the popup, and searchable as
  short fields.
- Runs detached from the UI: phase A spawns `hound enrich` in the background
  after a refresh; phase C runs it inside `serve`. Failures (endpoint down, bad
  JSON) are logged and retried later; search works identically without it.
- Default categories are seeded from current herdr workspace labels plus
  "Other"; editable in config.

### Phase C

`hound serve` runs as a herdr plugin `[[startup]]` process (or a systemd user
unit): file-watches the session directories, subscribes to herdr events to keep
live state current, refreshes the index and runs enrichment as changes arrive,
and writes a heartbeat. Not built until phase A is in daily use.

### Error handling

- Corrupt or half-written JSONL line: skip the line, keep going; a trailing
  partial line is retried on the next refresh.
- No host (hound run outside herdr): everything is searchable as "old";
  Enter resumes the session in the current terminal.
- Unknown record types and future harness format changes: ignored, never fatal.
- Index missing or unreadable: rebuild.

### Testing

- Adapter tests on small fixture session files per harness (sanitised).
- Indexer tests: fresh, append, truncate/rewrite, deleted file.
- Ranking tests on a fixed corpus.
- herdr access behind a trait with a fake for the join, scope and Enter logic.
- End-to-end by hand in an isolated `herdr --session hound-test`.

### Targets

Popup first paint under 100 ms with a warm index; keystroke to results under
30 ms at 10× today's corpus; cold index of today's corpus under 10 s.

## Calls made by Claude (not decided by the user)

- In-memory fuzzy matcher for short fields. (Rust and Tantivy were the user's
  choices.)
- Packaging: local `herdr plugin link` for v1; release binaries deferred.
- Working name `hound`; lives in its own repo at `~/dev/hound`.
- What is indexed vs skipped (cleaning rules above).
- Ranking order; empty query acts as a recency switcher.
- Workspace selection rule for resumed sessions.
- Default hotkeys; `hound install` prints config rather than editing it.
- Enrichment: off by default, local endpoint in the example, categories seeded
  from workspace labels, 10-minute quiet rule.
- No preview pane in v1.
- Subagent defaults (both on) and the `ctrl+s` toggle key.
- Terms match per session across all fields and messages.
- "May be running elsewhere" needs a second Enter; mtime-only detection.
- Resume cwd for multi-cwd sessions is the first recorded one.
- opencode adapter ships unverified.

## Left to the builder

- Exact crates (TUI, fuzzy matcher, file watcher).
- Socket vs shelling out to the `herdr` CLI.
- Whether plugin actions can be key-bound; pick the install path accordingly.
- Whether double-tap Shift is achievable; if so, offer it as an extra binding.
- opencode's storage layout and resume flag, from its docs.
- Popup layout details at narrow widths.

## As built (v1, 2026-11-17)

Where the build differs from the design above, and what is still open.

- **opencode is not supported.** From v1.2.0 opencode stores sessions in a
  SQLite database (`~/.local/share/opencode/opencode.db`) and updates rows in
  place, which the append-only, one-file-per-session reader cannot read.
  Supporting it needs an adapter that reads the database read-only and tracks
  a per-session version instead of a byte offset. Resume flag:
  `opencode --session <id>`.
- **Short-field matching** is substring, or within one typo of a word for
  terms of 5+ characters. Scattered-letter (subsequence) matching was dropped
  because it matched unrelated titles; it could return with scoring.
- **Hotkeys:** `prefix+space` / `prefix+a` / `prefix+shift+o`. Double-tap
  Shift is not documented by herdr. Plugin actions can be key-bound, but a
  plugin pane cannot take arguments, so the plugin route uses one pane per
  scope; the `[[keys.command]]` popup snippet is the primary install path.
- **Host detection** accepts `HERDR_ENV=1` or `HERDR_SOCKET_PATH`.
- **Focusing a pane** goes through herdr's socket API (`pane.focus`), because
  the herdr CLI cannot focus a pane by ID.
- **Standalone:** with no host, Enter replaces hound with the resumed agent in
  the current terminal (the design originally printed the command).
- **Popup behaviour added:** background result updates keep the selection on
  the same entry; a "may be running elsewhere" confirmation is tied to the
  entry it was raised for; Esc during an in-flight open waits for it, and a
  second Esc quits at once.
- **Enrichment:** sends only the project directory name, not the full path;
  one run at a time (`enrich.lock`); model output is stripped of control
  characters and capped. A model that does not fit in memory fails cleanly
  (the session is retried later); a small local model takes a few seconds per
  session.
- **Live state** is snapshotted once when the popup opens, not on a timer.
- **Not yet verified against a live herdr session:** see the README's list.
  Nothing that creates tabs, focuses panes or starts agents has been run for
  real.
- **Deferred:** phase C (`hound serve`), release binaries, preview pane,
  lines over 1 MiB in a session file are skipped, two files claiming the same
  session ID, proxy bypass for loopback enrichment endpoints.
