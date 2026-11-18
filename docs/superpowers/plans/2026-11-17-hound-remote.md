# hound across machines Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Implementer subagents run on Opus 5.5 (`model: "opus"`). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `hound serve` HTTP service that exposes one machine's sessions and per-term text matches, and a popup that merges configured remote machines into its list and focuses or resumes remote sessions through herdr's machine forwarding.

**Architecture:** The remote supplies data; the client searches. `hound-core` gains a `remote` module: wire types, a transport-free request handler over the index, a small HTTP server around it, and a client that implements the existing `TextSearch` trait. `Entry` gains a machine label; the open logic gets a session's resume facts through a trait (filesystem locally, HTTP remotely). The frontend gains a machine-prefixed herdr host, the `serve` subcommand, and remote workers in the popup that build a second hit list per machine and merge by the existing order.

**Tech Stack:** as v1 (Rust stable, tantivy, ratatui, clap, serde, ureq). New: a small blocking HTTP server crate (tiny_http or equivalent, builder's choice), a source of random bytes for the token. No async runtime.

**Spec:** `docs/REMOTE-DESIGN.md` — read it before any task. Background: `docs/DESIGN.md`.

**Test-first rule:** as in the v1 plan. Each task's tests below are the acceptance criteria. Write them first, see them fail, implement. Do not weaken a given assertion; if one is wrong against the real code, stop and report. Tests given as comments must be written out with every stated value. Extra tests are welcome.

## Global Constraints

- Everything in the v1 plan's Global Constraints still holds: session files read-only; tests never touch `~/.claude`, `~/.pi`, `~/.local/state/hound`, `~/.config/hound` or the running herdr session; no mutating herdr command during development; the popup never blocks; `hound-core` has no ratatui/crossterm dependency and no file under `crates/hound-core/` mentions herdr.
- Network in tests: only loopback connections to a server the test itself started on `127.0.0.1` with port 0. No test, and no manual run during development, may bind a non-loopback address or contact any other host. Do not start `hound serve` on the Tailscale address.
- Protocol number is `1`. Every response body is a JSON object carrying `"protocol": 1` and `"now"`. Clients ignore unknown fields.
- Every request needs `Authorization: Bearer <token>`; anything else is `401` with an empty body, decided before the path is looked at.
- Limits: request body ≤ 4096 bytes; `/v1/terms` ≤ 8 terms, each ≤ 64 characters; snippets ≤ 120 characters; `/v1/sessions` responses over 8 MiB are a client-side error.
- The service never returns conversation text beyond the session title (which may be the first prompt, at most 80 characters, one line), the stored summary and tags, and the bounded snippets; it has no endpoint that changes anything, and never logs query text, snippets or the token.
- A remote entry is never run in the local terminal; a local entry never goes through `--machine`.
- Off by default: with no `[serve]` and no `[[remote]]` in the config, nothing listens, nothing is contacted, and `hound install` (without `--serve`) prints nothing about either. `hound serve` requires `serve.enabled = true` and `serve.bind`. A `[[remote]]` block with `enabled = false` is ignored entirely.
- With no enabled `[[remote]]` configured, popup behaviour and row text are exactly as in v1; every existing test keeps passing unchanged unless a task says which assertion changes.
- Commit after every task with a conventional-commit message ending with the implementing model's `Co-Authored-By` line.

## Review Focus

1. A remote that accepts the connection and then never answers: the popup must stay responsive and the remote's worker must time out. → Task 3, `stalled_server_times_out`.
2. The same session ID on two machines: two entries, two identities; a confirm armed for one must not apply to the other. → Task 6, `same_id_on_two_machines_is_two_identities`.
3. Clock difference between machines: ages and the two-minute "running elsewhere" window use the server's `now`. → Task 4, `clock_offset_normalises_recency`, `remote_recent_change_uses_server_now`.
4. A token file with a trailing newline, wrong permissions, or empty: accepted with whitespace trimmed; empty is an error naming the file. → Task 1, `token_edge_cases`.
5. A remote whose HTTP side works while `herdr --machine` fails, and the reverse: each half still contributes what it can, and Enter on a closed remote session without a herdr route fails with a clear message instead of running locally. → Task 4, `remote_entry_without_host_fails`; Task 7, `partial_remote_states`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/hound-core/src/remote/mod.rs` | module declarations, `PROTOCOL` |
| `crates/hound-core/src/remote/wire.rs` | JSON types; `SessionDoc` ↔ wire conversion; `session_facts` |
| `crates/hound-core/src/remote/token.rs` | create/read the token file; constant-time compare |
| `crates/hound-core/src/remote/handler.rs` | `handle(ctx, request, now) -> Response`; auth, limits, routing — no sockets |
| `crates/hound-core/src/remote/server.rs` | HTTP listener around `handle`; bind-address check |
| `crates/hound-core/src/remote/client.rs` | `RemoteClient`, `RemoteError`, `TextSearch` over `/v1/terms` |
| `crates/hound-core/src/search.rs` | `Entry.machine`, clock offset, `build_entries_on`, `merge_hits` (modify) |
| `crates/hound-core/src/open.rs` | `ResumeFacts`, `LocalFacts`; `open_entry` takes facts (modify) |
| `crates/hound-core/src/config.rs` | `[serve]`, `[[remote]]` (modify) |
| `crates/hound/src/herdr.rs` | `MachineHerdr` and its pure argument builders (modify) |
| `crates/hound/src/serve.rs` | `hound serve`: upkeep loop, wiring |
| `crates/hound/src/ui/remote.rs` | per-remote worker: fetch, build entries, search, messages |
| `crates/hound/src/ui/app.rs`, `render.rs`, `mod.rs` | machine-aware identity, merged rows, footer notes, Enter routing (modify) |
| `crates/hound/src/main.rs` | `serve` subcommand, `install --serve` (modify) |
| `crates/hound-core/tests/remote_*.rs`, `crates/hound/tests/*.rs` | one test file per task |

---

### Task 1: Wire types, token, request handler

**Files:** Create `crates/hound-core/src/remote/{mod,wire,token,handler}.rs`, `crates/hound-core/tests/remote_handler.rs`. Modify `crates/hound-core/src/lib.rs`, and `open.rs` only to reuse `session_facts` for its existing last-change/directory logic (behaviour unchanged).

**Interfaces — Produces:**

```rust
// remote/mod.rs
pub const PROTOCOL: u32 = 1;
// remote/wire.rs — all Serialize + Deserialize + Clone + Debug + PartialEq
pub struct Envelope<T> { pub protocol: u32, pub now: i64, #[serde(flatten)] pub body: T }
pub struct Info { pub hound_version: String, pub host: String, pub home: String, pub sessions: usize }
pub struct WireSession { pub harness: String, pub id: String, pub title: String, pub cwds: Vec<String>,
    pub resume_cwd: String, pub started_at: i64, pub updated_at: i64, pub summary: Option<String>,
    pub category: Option<String>, pub tags: Vec<String>, pub main_file: Option<String>,
    pub last_change: i64, pub cwd_exists: bool }
pub struct Sessions { pub sessions: Vec<WireSession> }
#[serde(rename_all = "lowercase")] pub enum WireMode { Whole, Prefix }
pub struct TermSpec { pub term: String, pub mode: WireMode }
pub struct TermsRequest { pub terms: Vec<TermSpec>, pub include_subagents: bool }
pub struct WireMatch { pub harness: String, pub id: String, pub score: f32, pub snippet: String, pub from_subagent: bool }
pub struct TermMatches { pub term: String, pub matches: Vec<WireMatch> }
pub struct TermsResponse { pub terms: Vec<TermMatches> }
#[derive(Copy)] pub struct SessionFacts { pub last_change: i64, pub cwd_exists: bool }
pub fn session_facts(doc: &SessionDoc) -> SessionFacts;   // filesystem: max(updated_at, mtime of files[0]); resume_cwd non-empty and a directory
impl WireSession { pub fn from_doc(doc: &SessionDoc) -> WireSession;      // calls session_facts
                   pub fn into_doc(self) -> Option<(SessionDoc, SessionFacts)>; } // None for an unknown harness name;
                   // the SessionDoc has files = [main_file] (size/offset/hash zero) or empty, enriched_at_bytes 0
// remote/token.rs
pub fn load_or_create_token(path: &Path) -> anyhow::Result<String>; // creates parent dirs; 32 random bytes as 64 lowercase hex; file mode 0600
pub fn read_token(path: &Path) -> anyhow::Result<String>;           // trimmed; empty => Err naming the path
pub fn tokens_equal(a: &str, b: &str) -> bool;                      // constant-time over the longer length
// remote/handler.rs
pub struct Request<'a> { pub method: &'a str, pub path: &'a str, pub query: &'a str,
                         pub authorization: Option<&'a str>, pub body: &'a [u8] }
pub struct Response { pub status: u16, pub body: String }          // body "" for 401/404/405
pub struct ServeContext { pub index: Index, pub token: String, pub host: String, pub home: String, pub version: String }
pub fn handle(ctx: &ServeContext, req: &Request<'_>, now: i64) -> Response;
pub const MAX_BODY: usize = 4096; pub const MAX_TERMS: usize = 8; pub const MAX_TERM_CHARS: usize = 64;
```

Routing: auth first (401). `GET /v1/info`, `GET /v1/sessions`, `GET /v1/session?harness=&id=` (unknown session → 404), `POST /v1/terms`. Known path with wrong method → 405. Unknown path → 404. Body over `MAX_BODY`, invalid JSON, too many or too long terms, missing query parameters → 400 with `{"protocol":1,"now":..,"error":"<short reason>"}`; the reason never echoes input. Index errors → 500 with a fixed reason. `/v1/terms` returns one `TermMatches` per requested term, in order, from `Index::term_matches`.

- [ ] **Step 1: Write failing tests** in `tests/remote_handler.rs` (index built from fixture session files with the builders in `tests/common/mod.rs`, as `tests/index_query.rs` does; `ctx` with token `"t0k"`; helper `get(path)`, `post(path, body)` building `Request` with `authorization: Some("Bearer t0k")`):

```rust
#[test] fn rejects_missing_wrong_and_malformed_auth() {
    // None, Some("Bearer nope"), Some("t0k"), Some("Basic t0k"), Some("Bearer ") => status 401, body ""
    // and 401 even for an unknown path: GET /nope without auth => 401, not 404
}
#[test] fn info_reports_host_home_and_count() { /* 200; protocol 1; now == given now; host, home, sessions == 2, hound_version non-empty */ }
#[test] fn sessions_lists_metadata_without_text() {
    // 200; 2 sessions; one has title "Depot alerts", harness "claude", resume_cwd == fixture cwd,
    // main_file ends with "<id>.jsonl", cwd_exists true (tempdir), last_change >= updated_at;
    // the serialised body contains none of the fixture conversation strings
}
#[test] fn sessions_cwd_exists_false_for_missing_directory() {}
#[test] fn terms_returns_matches_per_term_in_order() {
    // body {"terms":[{"term":"signal","mode":"whole"},{"term":"river","mode":"prefix"},{"term":"zzzz","mode":"whole"}],"include_subagents":true}
    // => 3 TermMatches in that order; first has the S1 session with a snippet containing "Signal"; third has no matches
}
#[test] fn terms_respects_include_subagents() { /* a subagent-only term: present with true (from_subagent true), absent with false */ }
#[test] fn terms_limits() { /* 9 terms => 400; a 65-char term => 400; body of 4097 bytes => 400; invalid JSON => 400; none echo the input */ }
#[test] fn session_returns_fresh_facts() {
    // GET /v1/session?harness=claude&id=<S1> => last_change, cwd_exists; then append to the file (mtime now) => last_change increases
    // unknown id => 404; missing id parameter => 400; harness "nope" => 404
}
#[test] fn routing_statuses() { /* POST /v1/info => 405; GET /v1/terms => 405; GET /v1/other => 404 */ }
#[test] fn wire_session_roundtrip() { /* from_doc(doc).into_doc() => key, title, cwds, resume_cwd, times, summary/category/tags equal; files == [main file path]; harness "codex" => None */ }
#[test] fn unknown_fields_are_ignored_on_deserialise() { /* Envelope<Info> JSON with an extra field parses */ }
#[test] fn token_created_once_with_private_mode() {
    // load_or_create_token(tmp/"a/b/token") => 64 hex chars; file mode & 0o777 == 0o600; second call returns the same token
}
#[test] fn token_edge_cases() { /* read_token: "abc\n" => "abc"; "  " or empty file => Err containing the path; missing file => Err */ }
#[test] fn tokens_equal_is_exact() { /* equal strings true; different length false; same length different false; "" vs "" true */ }
```

- [ ] **Steps 2–4:** fail, implement, pass; `cargo test --workspace`, clippy, fmt clean.
- [ ] **Step 5:** Commit `feat: remote wire types, token and request handler`.

---

### Task 2: HTTP server and `hound serve`

**Files:** Create `crates/hound-core/src/remote/server.rs`, `crates/hound/src/serve.rs`, `crates/hound-core/tests/remote_server.rs`. Modify `crates/hound-core/src/config.rs`, `crates/hound-core/src/index/mod.rs` (heartbeat accessor), `crates/hound/src/main.rs`, `crates/hound/src/ui/mod.rs` (heartbeat skip), `crates/hound/tests/cli.rs`.

**Interfaces — Consumes:** Task 1. **Produces:**

```rust
// remote/server.rs
pub fn check_bind(addr: &SocketAddr, allow_any_interface: bool) -> anyhow::Result<()>; // unspecified address refused unless allowed
pub struct Server; pub struct ServerHandle;                       // handle stops the server on drop
impl Server { pub fn bind(addr: SocketAddr, ctx: ServeContext) -> anyhow::Result<Server>;
              pub fn local_addr(&self) -> SocketAddr;
              pub fn run(self) -> anyhow::Result<()>;             // blocks, serving on a small fixed pool of threads
              pub fn spawn(self) -> ServerHandle; }
// config.rs
pub struct ServeConfig { pub enabled: bool /* false */, pub bind: Option<String>, pub refresh_secs: u64 /* 30 */, pub allow_any_interface: bool /* false */ }
pub struct RemoteConfig { pub machine: String, pub url: String, pub token_file: PathBuf /* ~ expanded */, pub enabled: bool /* true */ }
impl Config { pub fn enabled_remotes(&self) -> impl Iterator<Item = &RemoteConfig>; }
Config { .., pub serve: ServeConfig, #[serde(rename = "remote")] pub remotes: Vec<RemoteConfig> }
pub fn serve_token_path() -> PathBuf;   // <config dir>/serve-token, config dir = parent of config_path()
// index/mod.rs
impl Index { pub fn heartbeat_age(&self, now: i64) -> Option<i64>; }  // None when never written
```

The server reads at most `MAX_BODY + 1` bytes of a body, applies read and write timeouts (5 s), and passes `Authorization`, method, path and query string to `handle`. Responses are `application/json` except empty bodies.

`hound serve`: load config; `serve.enabled` not true → exit 1 `set serve.enabled = true in <config path>`; `serve.bind` missing → exit 1 `set serve.bind in <config path>`; parse and `check_bind`; `load_or_create_token(serve_token_path())`; open the index; start an upkeep thread that refreshes at start and every `refresh_secs`, runs enrichment after a refresh when enabled, and writes the heartbeat after each pass in which its own refresh ran (not when the writer lock was busy); then serve. Prints one line to stderr at start: `hound serve listening on <addr>`. `hound serve --show-token` prints the token (creating it if needed) and exits 0 without binding; it too requires `serve.enabled = true`, so no token file appears on a machine that never opted in. `hound install --serve` prints a systemd user unit (`ExecStart=<absolute exe> serve`, `Restart=on-failure`) and the `systemctl --user` commands to enable it; it changes nothing.

Popup: skip the background refresh at start when `heartbeat_age(now)` is `Some(age)` with `0 <= age < 60`; still load sessions from the index. Keep this decision in a pure function with a test.

- [ ] **Step 1: Write failing tests.** `tests/remote_server.rs` (server on `127.0.0.1:0` via `spawn`, requests with `ureq`):

```rust
#[test] fn serves_info_over_http() { /* GET /v1/info with bearer => 200, protocol 1, content-type application/json */ }
#[test] fn unauthorised_over_http() { /* no header => 401, empty body */ }
#[test] fn terms_over_http() { /* POST /v1/terms => 200 with the expected match */ }
#[test] fn oversized_body_is_rejected_without_reading_it_all() { /* 1 MiB body => 400, returns promptly (< 2 s) */ }
#[test] fn concurrent_requests() { /* 8 threads x 5 GET /v1/sessions => all 200 */ }
#[test] fn handle_drop_stops_the_server() { /* after drop, a new connection is refused or fails */ }
#[test] fn check_bind_rules() {
    // 127.0.0.1:1, 100.64.0.10:7717, [::1]:1 => Ok with allow false
    // 0.0.0.0:7717 and [::]:7717 => Err mentioning allow_any_interface with allow false; Ok with allow true
}
#[test] fn heartbeat_age_reports_and_is_none_when_unset() {}
```

`crates/hound-core/tests/config.rs` additions:

```rust
#[test] fn serve_and_remote_config() {
    // "[serve]\nbind = \"127.0.0.1:7717\"\n[[remote]]\nmachine = \"homelab\"\nurl = \"http://homelab.example-tailnet.ts.net:7717\"\ntoken_file = \"~/t\"\n"
    // => serve.enabled false (not set), serve.bind Some, refresh_secs 30, allow_any_interface false;
    //    remotes.len()==1, machine "homelab", token_file == <home>/t, enabled true; enabled_remotes().count()==1
    // with "enabled = false" in the remote block => enabled_remotes().count()==0
    // defaults: serve.enabled false, serve.bind None, remotes empty
}
```

`crates/hound/tests/cli.rs` additions (tempdir config/state, herdr variables removed, as existing tests):

```rust
#[test] fn serve_is_off_by_default() { /* no [serve] at all, and [serve] with bind but no enabled => exit 1; stderr contains "serve.enabled"; no token file created; `--show-token` likewise */ }
#[test] fn serve_without_bind_is_an_error() { /* enabled = true, no bind => exit 1; stderr contains "serve.bind" */ }
#[test] fn install_without_serve_flag_does_not_mention_remotes() { /* `hound install` stdout contains neither "serve" nor "remote" */ }
#[test] fn serve_refuses_unspecified_address() { /* bind "0.0.0.0:0" => exit 1; stderr contains "allow_any_interface" */ }
#[test] fn serve_show_token_prints_and_persists() { /* 64 hex chars; second run prints the same; file exists under the temp config dir with mode 0600 */ }
#[test] fn serve_answers_then_dies_with_the_test() {
    // bind "127.0.0.1:0": spawn `hound serve`, read the "listening on <addr>" line from stderr, GET /v1/info with the
    // token from --show-token => 200 and sessions == number of fixture sessions (the upkeep thread indexed them);
    // kill the child
}
#[test] fn install_serve_prints_a_unit_and_changes_nothing() { /* stdout contains "[Service]", "ExecStart=", " serve", "systemctl --user"; directory listing unchanged */ }
#[test] fn popup_skips_refresh_when_heartbeat_is_fresh() { /* the pure decision function: Some(10) => skip; Some(60), Some(-5), None => refresh */ }
```

- [ ] **Steps 2–4:** fail, implement, pass; workspace suite, clippy, fmt clean.
- [ ] **Step 5:** Commit `feat: hound serve`.

---

### Task 3: Remote client

**Files:** Create `crates/hound-core/src/remote/client.rs`, `crates/hound-core/tests/remote_client.rs`.

**Interfaces — Consumes:** Tasks 1–2 (`Server::spawn` in tests). **Produces:**

```rust
#[derive(Debug, Clone, PartialEq)] pub enum RemoteError {
    Unreachable(String), Unauthorised, Protocol { theirs: u32 }, TooLarge, Bad(String) }
impl RemoteError { pub fn note(&self) -> &'static str; }
    // "unreachable" | "unauthorised" | "needs a newer hound" (theirs > PROTOCOL) or "needs an update there" (theirs < PROTOCOL)
    // | "too many sessions" | "bad response"
pub struct RemoteSessions { pub sessions: Vec<SessionDoc>, pub facts: HashMap<SessionKey, SessionFacts>, pub now: i64 }
pub struct RemoteClient;
impl RemoteClient {
    pub fn new(url: &str, token: String, timeout: Duration) -> RemoteClient;   // url without trailing slash handling: accept both
    pub fn info(&self) -> Result<(Info, i64), RemoteError>;                     // i64 = server now
    pub fn sessions(&self) -> Result<RemoteSessions, RemoteError>;              // sessions with an unknown harness are skipped
    pub fn session_facts(&self, key: &SessionKey) -> Result<(SessionFacts, i64), RemoteError>;
}
impl TextSearch for RemoteClient { .. }   // one POST /v1/terms per call; an error => Err (search treats it as no matches)
pub const MAX_SESSIONS_BYTES: u64 = 8 * 1024 * 1024;
```

Every call checks `protocol` first. Timeouts apply to connect and to the whole request. The client never follows redirects to another host and sends the token only to the configured URL. Error text never includes the token.

- [ ] **Step 1: Write failing tests** (test server from Task 2 on loopback, fixture index):

```rust
#[test] fn info_and_sessions_roundtrip() { /* info.sessions == 2; sessions().sessions has both keys, titles equal the index's; facts has both keys; now within 5 s of local */ }
#[test] fn term_matches_equal_the_local_index() {
    // for ("signal", Whole, true), ("river", Prefix, true), ("nginx", Whole, false):
    // client.term_matches(..) keys and from_subagent flags == index.term_matches(..); snippets equal
}
#[test] fn session_facts_are_fresh() { /* touch the file between two calls => last_change increases */ }
#[test] fn wrong_token_is_unauthorised() { /* every method => Err(Unauthorised) */ }
#[test] fn refused_connection_is_unreachable() { /* a port with nothing listening => Err(Unreachable(_)), note "unreachable" */ }
#[test] fn stalled_server_times_out() {
    // a TcpListener that accepts and never replies; client timeout 300 ms => Err(Unreachable(_)) in under 2 s
}
#[test] fn protocol_mismatch_is_reported() {
    // a tiny one-shot listener answering {"protocol":2,"now":0,...} => Err(Protocol{theirs:2}), note "needs a newer hound"
    // protocol 0 => note "needs an update there"
}
#[test] fn non_json_and_error_status_are_bad() { /* 500 with a body, and 200 with "hello" => Err(Bad(_)); the message does not contain the token */ }
#[test] fn oversized_sessions_response_is_too_large() { /* listener streaming > 8 MiB => Err(TooLarge) without buffering it all */ }
#[test] fn unknown_harness_sessions_are_skipped() { /* listener returning one "claude" and one "codex" session => 1 session */ }
#[test] fn trailing_slash_in_url_is_accepted() {}
```

- [ ] **Steps 2–4:** fail, implement, pass; workspace suite, clippy, fmt clean.
- [ ] **Step 5:** Commit `feat: remote client`.

---

### Task 4: Machine-aware entries and resume facts

**Files:** Modify `crates/hound-core/src/search.rs`, `crates/hound-core/src/open.rs`; create `crates/hound-core/tests/remote_entries.rs`; adjust call sites of `open_entry` in `crates/hound/src/ui/mod.rs` and tests in `crates/hound-core/tests/open.rs` (they pass `&LocalFacts`).

**Interfaces — Produces:**

```rust
// search.rs
pub struct Entry { pub session: Option<SessionDoc>, pub live: Option<LivePane>, pub machine: Option<String>, /* private: built_at, clock_offset */ }
pub fn build_entries(sessions, live, now) -> Vec<Entry>;                       // unchanged signature; machine None, offset 0
pub fn build_entries_on(machine: &str, sessions: Vec<SessionDoc>, live: Vec<LivePane>, now: i64, server_now: i64) -> Vec<Entry>;
impl Entry { pub fn last_active(&self) -> i64; }   // session.updated_at minus (server_now - now): expressed on the local clock
pub fn merge_hits(lists: &[(&[Entry], &[Hit])]) -> Vec<(usize, usize)>;        // (list index, hit index), in the existing order across all lists
// open.rs
#[derive(Clone, Copy, Debug)] pub struct Facts { pub last_change: i64, pub cwd_exists: bool, pub now: i64 }
pub trait ResumeFacts { fn facts(&self, session: &SessionDoc, local_now: i64) -> anyhow::Result<Facts>; }
pub struct LocalFacts;                               // filesystem via remote::wire::session_facts; now = local_now
pub fn open_entry(entry: &Entry, host: Option<&dyn Host>, adapters: &[Box<dyn Adapter>],
                  facts: &dyn ResumeFacts, confirmed: bool, now: i64) -> OpenOutcome;
```

`merge_hits` uses the same comparison `search` uses (tier, live before closed, `last_active` descending, title); ties across lists keep list order. `open_entry` for a closed session: after the went-live check, call `facts.facts(..)`; an `Err` → `Failed("<reason>")` and nothing else is attempted; `!cwd_exists` → the existing directory message; `!confirmed && facts.now - facts.last_change < 120` → the existing confirm. An entry with `machine: Some(m)` and `host: None` → `Failed("<m>: no route to that machine's herdr")` for live and closed alike — never `RunHere`. Failure messages for an entry with a machine are prefixed `"<m>: "`. For `machine: None` behaviour is exactly as before (all existing `tests/open.rs` assertions hold with `&LocalFacts`).

- [ ] **Step 1: Write failing tests** in `tests/remote_entries.rs` (`FakeHost` as in `tests/open.rs`; `FixedFacts(Result<Facts, String>)` implementing `ResumeFacts`):

```rust
#[test] fn build_entries_on_sets_machine_and_joins() { /* machine Some("homelab") on every entry; a pane joins its session as locally */ }
#[test] fn clock_offset_normalises_recency() {
    // local now 1000, server_now 1600 (server clock 600 s ahead); session updated_at 1540 => last_active() == 940
    // build_entries (local) last_active() == updated_at
}
#[test] fn merge_orders_across_lists() {
    // local: closed Short hit active at 900; remote: live Content hit, closed Short hit active at 950, closed Short hit active at 800
    // => order: remote Short 950, local Short 900, remote Short 800, remote Content   (tier first, then liveness, then recency)
}
#[test] fn merge_puts_live_before_closed_within_a_tier() {}
#[test] fn merge_of_empty_and_single_lists() {}
#[test] fn remote_closed_session_resumes_through_its_host() {
    // entry machine "homelab", FakeHost, FixedFacts(Ok{last_change: 0, cwd_exists: true, now: 10_000}) with a resume_cwd that
    // does NOT exist on this filesystem => Resumed; calls: snapshot, create_tab, start_agent (the local filesystem is not consulted)
}
#[test] fn remote_missing_directory_fails_from_facts() { /* cwd_exists false => Failed containing "homelab" and "directory no longer exists"; no create_tab */ }
#[test] fn remote_recent_change_uses_server_now() {
    // facts {last_change: 9_950, now: 10_000}, local now 500 => NeedsConfirm when !confirmed; Resumed when confirmed
    // facts {last_change: 9_000, now: 10_000}, local now 9_010 => Resumed (not recent on the server's clock)
}
#[test] fn facts_error_stops_the_resume() { /* FixedFacts(Err("unreachable")) => Failed containing "homelab" and "unreachable"; calls == ["snapshot"] */ }
#[test] fn remote_entry_without_host_fails() { /* live and closed remote entries with host None => Failed containing "homelab"; never RunHere */ }
#[test] fn remote_went_live_is_focused_without_facts() { /* snapshot now has the session => Focused; facts not called (a FixedFacts that panics) */ }
#[test] fn local_entries_unchanged_with_local_facts() { /* a local closed session with no host => RunHere as before */ }
```

- [ ] **Steps 2–4:** fail, implement, update call sites, pass; workspace suite, clippy, fmt clean.
- [ ] **Step 5:** Commit `feat: machine-aware entries and resume facts`.

---

### Task 5: Machine-prefixed herdr host

**Files:** Modify `crates/hound/src/herdr.rs`, `crates/hound/tests/herdr_parse.rs`.

**Interfaces — Produces:**

```rust
pub fn machine_args(label: &str, args: &[String]) -> Vec<String>;      // ["--machine", label, ..args]
pub fn machine_focus_commands(pane: &LivePane) -> Vec<Vec<String>>;
    // agent pane (kind is Some): [["agent","focus", <agent_name if Some, else pane_id>]]
    // other pane: [["workspace","focus",<workspace_id>], ["tab","focus",<tab_id>]]
pub struct MachineHerdr; impl MachineHerdr { pub fn new(bin: PathBuf, label: String) -> MachineHerdr; pub fn label(&self) -> &str; }
impl Host for MachineHerdr { .. }
```

`MachineHerdr` runs every command as `<bin> --machine <label> …` with the existing argument builders and parsers: `snapshot` (the four list commands), `focus` (`machine_focus_commands`), `create_tab`, `start_agent` (`agent start …`, then `agent focus <name>` so the client's view follows; a failure of that trailing focus is not an error). `read_scrollback` returns an error (`not available on a remote machine`). It never uses the socket. Errors carry herdr's stderr on one line. Its herdr binary is the same one the local host resolved.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn machine_args_prefix() { assert_eq!(machine_args("homelab", &strings(&["agent","list"])), ["--machine","homelab","agent","list"]); }
#[test] fn machine_focus_prefers_agent_name() { /* named agent => [["agent","focus","migration"]] */ }
#[test] fn machine_focus_unnamed_agent_uses_pane_id() { /* [["agent","focus","w2:p1"]] */ }
#[test] fn machine_focus_shell_pane_uses_workspace_and_tab() { /* [["workspace","focus","w2"],["tab","focus","w2:t3"]] */ }
#[test] fn machine_label_with_spaces_is_one_argument() { /* machine_args("my lab", ..)[1] == "my lab" */ }
#[test] fn opaque_long_ids_parse() {
    // a snapshot fixture variant whose ids look like "w0123456789abcde", "w0123456789abcde:t1", "w0123456789abcde:p1"
    // (the client's herdr uses this form) parses with labels resolved, same as the short-id fixtures
}
```

- [ ] **Steps 2–4:** fail, implement, pass; workspace suite, clippy, fmt clean. Do not execute any `--machine` command.
- [ ] **Step 5:** Commit `feat: herdr host for a saved machine`.

---

### Task 6: Popup state for several machines

**Files:** Modify `crates/hound/src/ui/app.rs`, `crates/hound/src/ui/render.rs`, `crates/hound/tests/ui.rs`.

**Interfaces — Produces:** the popup's pure state handles one result list per machine.

```rust
pub struct Source { pub machine: Option<String>, pub entries: Vec<Entry>, pub hits: Vec<Hit> }
impl App {
    pub fn set_results(&mut self, entries: Vec<Entry>, hits: Vec<Hit>);        // unchanged: replaces the local source, selection 0
    pub fn update_results(&mut self, entries: Vec<Entry>, hits: Vec<Hit>);     // unchanged: local source, selection follows identity
    pub fn update_source(&mut self, machine: &str, entries: Vec<Entry>, hits: Vec<Hit>); // a remote's results; selection follows identity
    pub fn clear_sources(&mut self);                                           // on a user requery: drop remote hits until they re-arrive
    pub fn set_note(&mut self, machine: &str, note: Option<String>);           // footer note per machine
    pub fn rows(&self) -> Vec<(&Entry, &Hit)>;                                 // merged across sources via merge_hits
    pub fn selected_entry(&self) -> Option<&Entry>;
    pub remotes_configured: bool,                                              // rows name their machine when true
}
```

`Effect::Open { hit, confirmed }` keeps its shape; `hit` indexes `rows()`. Identity is (machine, session key) or (machine, pane ID). On a user-driven requery the local results arrive immediately and remote ones later: when a remote's results arrive, the selection stays on the same identity if it is still present, exactly as `update_results` does today. Rendering: when `remotes_configured`, a live row's location is `<machine or "local"> › <workspace> › <tab>` and a closed row's is `<age>  <machine or "local"> · <dir>`; otherwise rows are exactly as today. Footer: notes joined as `homelab: unreachable`, shown after the key hints and before progress; a `status` still takes precedence.

- [ ] **Step 1: Write failing tests** in `tests/ui.rs`:

```rust
#[test] fn rows_merge_local_and_remote_by_rank() { /* local closed Short (recent), remote live Short, remote Content => remote live, local closed, remote Content */ }
#[test] fn same_id_on_two_machines_is_two_identities() {
    // local and "homelab" sessions with the same harness+id: two rows; select the remote one, ask_confirm;
    // move to the local one, Enter => confirmed: false; move back, Enter => confirmed: false (disarmed by moving)
}
#[test] fn confirm_stays_with_its_machine_entry() { /* armed on the remote row; update_source re-delivers it at another index => Enter confirmed: true for that row */ }
#[test] fn remote_results_arriving_keep_the_selection() { /* select local row 2; update_source inserts remote rows above => same local entry selected */ }
#[test] fn user_requery_drops_stale_remote_rows() { /* clear_sources then set_results => only local rows until update_source */ }
#[test] fn removing_a_remote_entry_clamps_and_disarms() {}
#[test] fn open_effect_indexes_merged_rows() { /* Enter on merged row 0 (a remote entry) => Open{hit:0}; selected_entry().machine == Some("homelab") */ }
#[test] fn rows_name_machines_when_remotes_configured() {
    // text contains "homelab › Billing › t7", "local › Tools › t1", "3d  homelab · tools-dev", "3d  local · tools-dev"
}
#[test] fn rows_unchanged_without_remotes() { /* remotes_configured false: text contains "Billing › t8" and not "local" */ }
#[test] fn footer_shows_machine_notes() { /* set_note("homelab", Some("unreachable")) => "homelab: unreachable"; cleared with None; a status hides it */ }
#[test] fn long_machine_label_is_truncated_not_wrapped() { /* 40-char label at width 60: no line wider than 60 */ }
```

All existing `tests/ui.rs` tests must pass unchanged.

- [ ] **Steps 2–4:** fail, implement, pass; workspace suite, clippy, fmt clean.
- [ ] **Step 5:** Commit `feat: popup state for several machines`.

---

### Task 7: Remote workers and Enter routing in the popup

**Files:** Create `crates/hound/src/ui/remote.rs`, `crates/hound/tests/remote_popup.rs`. Modify `crates/hound/src/ui/mod.rs`, `crates/hound/src/main.rs`, `README.md`.

**Interfaces — Consumes:** Tasks 3–6. **Produces:**

```rust
// ui/remote.rs — everything decidable without a terminal is a plain function or a struct with no threads
pub struct RemoteState { /* machine label, client, sessions+facts, live panes, server clock offset, home */ }
pub enum RemoteLoad { Ready(RemoteState), Partial(RemoteState, String /* note */), Failed(String /* note */) }
pub fn load_remote(machine: &str, client: &RemoteClient, host: Option<&dyn Host>, now: i64) -> RemoteLoad;
impl RemoteState {
    pub fn search(&self, client: &dyn TextSearch, query: &str, scope: Scope, include_subagents: bool, now: i64) -> (Vec<Entry>, Vec<Hit>);
}
pub struct RemoteFacts<'a> { pub client: &'a RemoteClient }       // impl ResumeFacts via GET /v1/session
pub struct Debounce;                                              // latest-wins: next(query_seq) / is_current(seq)
```

`load_remote`: `info` (protocol), `sessions`, and a snapshot through the machine host. All good → `Ready`. Sessions good but snapshot failed → `Partial(state with no live panes, "herdr machine not reachable")`. Snapshot good but HTTP failed → `Partial(state with live panes only, <RemoteError::note()>)`. Both failed → `Failed(<note>)`. No host at all (standalone) → `Failed("needs herdr")` without any request. `search` runs `hound_core::search::search` over `build_entries_on(..)` with the remote's home stripped.

Popup wiring (`ui/mod.rs`): for each enabled `[[remote]]` in config (`Config::enabled_remotes`), when a local host exists, start a worker thread that loads the remote, then serves search requests sent over a channel — each carrying a sequence number — sleeping ~80 ms and skipping to the newest pending request before searching; results come back as messages and go to `App::update_source`, dropped if their sequence is not the latest. Notes go to `App::set_note`. `App::remotes_configured` is true when any enabled remote is configured. On a user-driven requery call `clear_sources`. Enter: an entry with `machine: Some(m)` is opened on a background thread with that machine's `MachineHerdr` and `RemoteFacts`; a local entry as today with `LocalFacts`. A missing or unreadable token file is a note (`homelab: no token`), not a startup error.

README: an "Advanced: several machines" section, placed after everything a single-machine user needs and stating first that it is off by default and what turning it on exposes — what `hound serve` exposes and does not, the `[serve]` and `[[remote]]` config, the token, the systemd unit, Tailscale address guidance (bind to the tailnet address, never all interfaces), which popup runs where, and what is verified versus not. Move to "verified on a real setup (2026-11-17, a macOS client attached to a Linux server running herdr 0.9.3)": a popup bound on the server opens from the attached client; herdr is detected inside the popup; Enter focuses a live agent; Enter resumes a closed session in a new tab; `herdr --machine <label> agent focus` moves the client's view. Add to "not yet verified": `tab create` / `agent start` through `--machine`; view following after a remote resume; `workspace focus` / `tab focus` through `--machine`; building and running on macOS.

- [ ] **Step 1: Write failing tests** in `tests/remote_popup.rs` (loopback test server with a fixture index; `FakeHost` for the machine host):

```rust
#[test] fn load_ready_builds_live_and_closed_entries() { /* FakeHost snapshot has a pane running fixture session S1 => search("") gives S1 live with machine "homelab", S2 closed */ }
#[test] fn mixed_query_matches_label_and_content() {
    // S1 live in workspace "Billing"; query "billing signal" ("signal" only in S1's conversation) => S1 matches, tier Content
}
#[test] fn remote_home_prefix_is_stripped() { /* server home "/home/u": query "home" does not match a session whose only link is its cwd */ }
#[test] fn partial_remote_states() {
    // snapshot Err + HTTP ok => Partial, note "herdr machine not reachable", search finds sessions, all closed
    // snapshot ok + wrong token => Partial, note "unauthorised", search("billing") finds the live pane by name only
    // both fail => Failed("unreachable")
    // host None => Failed("needs herdr") and the test server saw no request
}
#[test] fn protocol_mismatch_disables_the_remote() { /* Failed or Partial with note "needs a newer hound"; no sessions used */ }
#[test] fn remote_facts_come_from_the_server() { /* RemoteFacts.facts(S1) => cwd_exists true, now from the server; unknown session => Err */ }
#[test] fn debounce_latest_wins() { /* next() three times => only the last sequence is_current */ }
#[test] fn remote_enter_goes_through_machine_host_and_remote_facts() {
    // open_entry(remote closed entry, Some(&FakeHost), adapters, &RemoteFacts{..}, false, now) => Resumed;
    // FakeHost calls: snapshot, create_tab, start_agent; the test server saw GET /v1/session
}
#[test] fn disabled_remote_is_never_contacted() { /* a [[remote]] with enabled = false pointing at the test server: popup setup builds no worker for it and the server sees no request */ }
#[test] fn missing_token_file_is_a_note() { /* the function that builds a client from RemoteConfig => Err/None with note "no token" */ }
```

- [ ] **Steps 2–4:** fail, implement, pass; workspace suite, clippy, fmt clean.
- [ ] **Step 5: Manual check, loopback only.** In a scratch directory: machine A = a state dir + config with `serve.bind = "127.0.0.1:<free port>"` and fixture roots; run `hound serve` for A. Machine B = another state dir + config with a `[[remote]]` pointing at A and A's token. Run `hound ui` for B **outside herdr** in a pty and confirm the footer shows `homelab: needs herdr` and local results work. Report what was seen. Do not run it inside herdr, and do not bind any non-loopback address.
- [ ] **Step 6:** Commit `feat: remote machines in the popup`.

---

## Out of scope for this plan

Installing the service on the server and hound on the client (done with the user afterwards); the server's popup seeing the client; cross-machine resume; TLS; file watching; an SSH transport; opencode.
