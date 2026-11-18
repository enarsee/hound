# hound across machines — design

Status: built on branch `remote` 2026-11-17; see "As built" at the end. Date: 2026-11-17. Builds on `docs/DESIGN.md`
(v1, on `main`).

## Intent

The user works from a client machine whose herdr UI shows two groups of
workspaces: the client's own ("Local") and those of a server machine, attached
as a saved herdr machine (labelled `homelab` in the examples below) over
Tailscale. hound v1 runs on one machine and sees only that machine. The user
wants the popup to cover both: go into a running session wherever it is, and
resume an old session on the machine it belongs to.

## What was established first (2026-11-17)

- herdr runs custom commands on the server whose workspace is selected. A
  popup bound in the server's herdr config opens from the client's UI and runs
  on the server. Verified: popup opens, live agents are detected, Enter focuses
  a live agent, Enter resumes a closed session in a new tab.
- From the client, `herdr --machine homelab agent focus <name>` focuses the
  pane on the server **and moves the client's view to it**. Verified.
- herdr's API (forwarded by `--machine`) covers panes, focus, tabs and agent
  start. It has no call that runs a command remotely and returns output, and
  no port forwarding, so conversation search needs hound's own channel.
- The server is on a Tailscale tailnet, reachable by a full tailnet name such
  as `homelab.example-tailnet.ts.net` or an address such as `100.64.0.10`.
  systemd user services are available with lingering on. The client reaches it
  by tailnet name.
- The server has no route back to the client.

## Goals

1. A long-running `hound serve` on a machine that answers search over HTTP and
   keeps that machine's index warm (this is phase C of the v1 design).
2. A popup that, when configured with remotes, shows local and remote sessions
   in one ranked list, each labelled with its machine.
3. Enter on a running remote session takes you into it. Enter on a closed
   remote session resumes it on its own machine and takes you into it.
4. The popup stays instant: remote results arrive asynchronously and a slow or
   unreachable remote never delays local results.

## Non-goals

- The server's popup seeing the client's sessions (no route back).
- Resuming a session on a different machine from the one that holds it.
- TLS in hound (Tailscale encrypts the link), user accounts, or multi-user use.
- Discovering remotes automatically; they are listed in config.
- File watching (the service polls), an SSH transport, opencode.
- Any endpoint that returns whole conversations or changes anything.

## Anti-goals

- Serving on an address the user did not choose. Nothing listens unless
  `hound serve` is run, and it binds only to the configured address.
- Answering an unauthenticated request with anything but 401.
- Conversation text leaving the machine beyond the session title (which, for a
  session whose harness records no title, is the first prompt cut to 80
  characters), the stored summary and tags, and the bounded snippets.
- A remote result ever being resumed locally, or a local one remotely.
- A second copy of a session that is already live on its machine.

## Approaches considered

- **A. Ask the remote hound over SSH per popup.** No open port, reuses SSH
  keys; a connect delay on each popup open; nothing reusable by other clients.
- **B. Copy remote session files and index them locally.** Offline search, but
  private history is duplicated and must be kept in sync.
- **C. Remote titles only.** Cheapest; loses full-text search of the remote.
- **D. HTTP service on the remote, reached directly over Tailscale.** Fast
  from the first keystroke, doubles as the always-warm index service, usable
  by other clients later. Costs a listening port and a token to manage.

**Chosen: D** (the user asked about an HTTP service and confirmed the machines
are on Tailscale, which makes a token-protected port on the tailnet address
acceptable). Live state and actions use herdr's machine forwarding in every
approach.

## Design

### `hound serve`

A foreground process, meant to run as a systemd user unit.

- Config, in `~/.config/hound/config.toml`:

  ```toml
  [serve]
  enabled = true                 # default false; the service refuses to start without it
  bind = "100.64.0.10:7717"      # required to serve; no default address
  refresh_secs = 30
  ```

  Off by default (decided by the user): with no config, nothing listens and the
  popup is exactly v1. `hound serve` exits with an error naming the key when
  `enabled` is not true or `bind` is missing. Remotes are likewise opt-in: the
  popup contacts another machine only for a `[[remote]]` block the user wrote,
  and a block can be switched off with `enabled = false` without deleting it.
  Both live in an "Advanced: several machines" section of the README; `hound
  install` (without `--serve`) does not mention them. Binding to
  an unspecified address (`0.0.0.0`, `::`) is refused unless
  `allow_any_interface = true` is also set.
- Token: `~/.config/hound/serve-token`, created on first run with mode 0600,
  32 random bytes as hex. `hound serve --show-token` prints it. Every request
  must carry `Authorization: Bearer <token>`; comparison is constant-time;
  anything else gets 401 with an empty body.
- Index upkeep: refresh at start, then every `refresh_secs`; run enrichment
  after a refresh when enabled; write the heartbeat in `state.json` after each
  pass. A popup on the same machine skips its own refresh when the heartbeat
  is under 60 seconds old.
- Limits: request body at most 4 KiB, the per-request caps given under the
  protocol, a per-request time budget, a small fixed worker pool. Logs go to stderr (the journal) and
  never include query text, snippets or the token.
- Unit file: `hound install --serve` prints a `hound-serve.service` user unit
  and the commands to enable it. hound does not install it itself.

### Protocol v1 (JSON over HTTP)

Search runs on the client, with the remote supplying data. This keeps one
matching and ranking implementation: the client knows which remote sessions
are live and what their workspace, tab and agent names are, so a query such as
`billing depot` (workspace label plus a word from the conversation) matches a
remote session exactly as it would a local one. The server never ranks.

Every response carries `"protocol": 1` and `"now": <server unix seconds>`.
Fields may be added later without changing the protocol number; clients ignore
fields they do not know.

- `GET /v1/info` → `{ "hound_version": "...", "host": "<hostname>",
  "home": "<server user's home directory>", "sessions": <count> }`
- `GET /v1/sessions` → every indexed session's metadata, no conversation text:

  ```json
  { "sessions": [ {
      "harness": "claude", "id": "...", "title": "...",
      "cwds": ["..."], "resume_cwd": "...", "started_at": 0, "updated_at": 0,
      "summary": null, "category": null, "tags": [],
      "main_file": "/home/.../<id>.jsonl",
      "last_change": 0, "cwd_exists": true } ] }
  ```

  `last_change` is the later of the last indexed record and the session
  file's modification time; `cwd_exists` says whether `resume_cwd` is a
  directory there. The client cannot stat remote files, so these stand in.
  `main_file` lets the client join a live pane that reports a path instead of
  an ID. Not capped; a response larger than 8 MiB is an error the client
  reports as "too many sessions".
- `POST /v1/terms` with
  `{ "terms": [ { "term": "depot", "mode": "whole|prefix" } ], "include_subagents": true }`
  → for each term, the sessions whose conversation contains it:

  ```json
  { "terms": [ { "term": "depot", "matches": [
      { "harness": "claude", "id": "...", "score": 1.0,
        "snippet": "…", "from_subagent": false } ] } ] }
  ```

  This is the existing per-term text match, exposed. At most 8 terms per
  request, each at most 64 characters; snippets keep the 120-character bound.
- `GET /v1/session?harness=<h>&id=<id>` → that one session's current
  `last_change` and `cwd_exists`, read fresh. Used at Enter.
- Unknown paths 404, wrong method 405, bad JSON or over-limit input 400.

### Remotes on the client

```toml
[[remote]]
machine = "homelab"                         # the herdr machine label
url = "http://100.64.0.10:7717"
token_file = "~/.config/hound/remotes/homelab.token"
enabled = true                              # optional; false ignores this block
```

For each enabled remote the popup starts one background worker. At start it fetches,
in parallel: `GET /v1/info` (a protocol mismatch disables the remote), `GET
/v1/sessions`, and one live snapshot through `herdr --machine <label> …` (the
same four list commands and parser as the local host). From the sessions and
the snapshot it builds that machine's entries with the existing join, so a
live remote agent is a session entry with a stable identity, not a bare pane.

On each query change, after a short pause in typing (about 80 ms), the worker
runs the existing search over that machine's entries. Terms that match a
session's short fields (title, working directory with the **remote** home
stripped, summary, tags, and for live ones workspace, tab and agent name) are
decided on the client with no request; only terms that still need conversation
text go to `POST /v1/terms` (the endpoint takes several terms; the client may
send one request per term). Searches are numbered and a result
older than the latest query is dropped. Request timeout about 2 seconds.

A remote that fails is shown in the footer (`homelab: unreachable`, `homelab:
needs a newer hound`, `homelab: unauthorised`, `homelab: herdr machine not
reachable`) and local results are unaffected. If the sessions arrive but the
herdr snapshot fails, the remote's sessions are still searchable and all shown
as closed, and Enter on them fails with that note. If the HTTP side fails but
the snapshot works, its live panes are still listed and focusable, by name
only. Remote search is skipped entirely when hound has no host (standalone),
since remote actions need herdr.

### One list

The popup holds one hit list per machine (local plus each remote) and shows
them merged by the existing order, applied across all machines together: tier,
then live before closed, then most recently active, then title. Scopes apply
across machines. Ages are computed against each server's own `now`, so a
clock difference between machines does not distort them.

When at least one remote is configured, every row names its machine in the
location: `homelab › Billing › t7` and `local › Tools › t1` for live rows,
`3d  homelab · tools-dev` and `3d  local · tools-dev` for closed ones. With no
remote configured, rows are exactly as in v1.

Selection identity, confirmation targets and background-update behaviour
include the machine, so the same session ID on two machines is two entries.
Remote session-less panes are matched by name only (no scrollback).

### Enter on a remote entry

All actions go through `herdr --machine <label>`:

- **Live agent** → `agent focus <agent name, else pane ID>`. The view follows.
- **Live pane that is not an agent** → `workspace focus` then `tab focus`
  (best effort; the pane itself cannot be focused by ID through the CLI).
- **Closed session** → the same sequence as locally, against that machine:
  fresh herdr snapshot; if the session is now live there, focus it; then `GET
  /v1/session` for fresh facts — if that request fails, stop with the reason
  rather than resume blind; if `cwd_exists` is false, fail with the directory
  message; if `last_change` is within two minutes of the server's `now` and
  not confirmed, ask for confirmation; pick the workspace among that machine's
  panes (falling back to its focused pane's workspace); `tab create`; `agent
  start`; then `agent focus` on the new agent so the view follows. Failures
  are reported with the machine name.

A remote entry is never run in the local terminal, and a local entry never
goes through `--machine`.

### Where each popup runs

herdr runs a popup on the server whose workspace is selected. So:

- With a **Local** workspace selected, the client's binding opens the client's
  hound: the merged view.
- With a **homelab** workspace selected, the server's binding opens the
  server's hound: the server's sessions only (it has no route to the client).

Both machines bind the same keys. The merged view is available only from Local
workspaces; this is accepted.

### What changes in the code

- `hound-core`: `Entry` gains an optional machine label; the open logic gets
  its directory and last-change facts through a small trait, implemented by
  the filesystem locally and by `GET /v1/session` remotely; the search
  function takes the home directory to strip per call (already a parameter);
  the serve request handling and the wire types live in core (no herdr
  knowledge needed). A remote implements the existing `TextSearch` trait over
  `POST /v1/terms`.
- `hound`: `hound serve`; an HTTP client for remotes; the herdr host gains a
  machine-prefixed mode (CLI only, focus through `agent focus`); the popup
  gains remote threads, merged results and the footer notes; `hound install
  --serve`.
- The core crate still must not mention herdr or depend on the terminal UI.

### Testing

- Server: started in tests on `127.0.0.1` with an ephemeral port and a
  temporary index; auth, limits, each endpoint, empty query, truncation.
  Loopback connections to a server the test itself started are allowed; no
  test may contact any other address.
- Client: against that same test server — merge, ordering, stale-response
  dropping, timeout, unauthorised, protocol mismatch.
- Machine-prefixed host: argument building as pure functions.
- Open logic: remote facts drive the directory and running-elsewhere checks.
- UI: machine labels in rows, footer notes, selection identity across machines.

### To verify on the real setup (cannot be tested from the server alone)

- `herdr --machine homelab tab create` / `agent start` from the client, and
  that a final `agent focus` moves the view after a resume.
- Whether `workspace focus` / `tab focus` through `--machine` move the view.
- hound building and running on macOS (never compiled there).
- The four list commands through `--machine` return the same shapes as local.
- (Verified 2026-11-17: plain `herdr workspace list` on the client lists only
  the client's own workspaces. Its workspace IDs look like `w0123456789abcde`,
  unlike the server's `w2`; IDs are opaque.) Originally to check: that it lists
  only the client's workspaces, not the attached server's. (herdr's docs say
  IDs are scoped to one server and `--machine` is needed to reach another; if
  the client's list did include server workspaces they would appear twice, so
  check this first.)

## Rollout order

1. Build on the server; run `hound serve` there as a user unit bound to the
   tailnet address.
2. Build hound on the client; add the popup bindings to the client's herdr
   config and the `[[remote]]` block with the token.
3. Check the "to verify" list from the client.

## Calls made by Claude (not decided by the user)

- systemd user unit rather than a herdr plugin startup hook (starts at boot,
  independent of herdr).
- Port 7717; no default bind address; refusal of unspecified addresses.
- Token in a 0600 file, bearer header.
- Polling refresh every 30 s instead of file watching.
- Remotes listed in config rather than read from `herdr machine list`.
- Search runs on the client over remote data (sessions list plus a per-term
  endpoint), not on the server — so remote and local rank identically.
- A fresh per-session check at Enter; resume refuses if it cannot be made.
- Every row names its machine once any remote is configured; `local` for this
  machine.
- Debounce 80 ms, timeout 2 s.
- Remote session-less panes searchable by name only (no scrollback).
- No remote search in standalone mode.

## Left to the builder

- HTTP server and client crates; worker-pool size; exact timeouts.
- Wire type layout in code, as long as the JSON above is what goes on the wire.
- How the popup threads are organised.

## As built (2026-11-17)

Where the build differs from the design above, and what is still open.

- **`url` uses the Tailscale IP or full tailnet name**, not a short host name:
  a short name can resolve elsewhere when Tailscale is down, and the bearer
  token would be sent there in clear text.
- **What a token holder can read.** No single request returns a whole
  conversation, but arbitrary terms return 120-character snippets around each
  match, so repeated searches can recover much of every conversation. The
  token is as sensitive as the session files. Titles of untitled sessions are
  the first 80 characters of the first prompt.
- **The server is a hand-written HTTP/1.1 loop** (4 workers, 2 s to read the
  request head, 5 s for the whole request, 30 s to write a response). It
  refuses chunked bodies and duplicate or non-numeric `Content-Length`.
- **Token file checks.** `hound serve` refuses a token file that is a symlink,
  not a regular file, not owned by the current user, or readable by others.
- **Bind guard** also refuses the IPv4-mapped "all interfaces" address.
- **Session IDs from a remote are validated** (`[A-Za-z0-9_-]`, no leading
  dash, length-capped) before they can reach a harness command line, and again
  when resume arguments are built.
- **Machine labels** may not be empty, start with a dash, be `local`, or
  duplicate another remote's label.
- **Enter routing** goes through one function that selects the host and facts
  by the entry's machine and returns an error, never a local fallback.
- **Notes in the footer:** `loading…`, `unreachable`, `unauthorised`, `needs a
  newer hound`, `herdr machine not reachable`, `needs herdr`, `no token`,
  `invalid machine name`, `duplicate machine name`, `internal error`.
- **Listing a remote's panes is limited to 5 seconds** when the popup loads
  it; after that its sessions show as closed with a note.
- **Per-remote `timeout_secs`** (default 3).
- **Enrichment under `hound serve`:** at most every 10 minutes and only after
  changes, with a per-session failure cooldown; requires `enrich.categories`.
  The popup on that machine no longer spawns enrichment itself.
- **The index writer lock** can appear held for an instant when the process
  spawns a child; a refresh is then skipped once and retried later.
- **Selection** stays on its entry when a better-ranked remote row arrives
  above it (it does not jump back to the top).

Not built or deferred: a time budget for handling a request (only a token
holder can make an expensive one); a timed herdr call at Enter (a second Esc
quits); permission checks on the client's token file; silencing a
background-thread panic message over the popup; `hound serve` has not been
run on macOS.

Not yet verified on the real setup: `tab create` / `agent start` through
`--machine` and the view following after a remote resume; how herdr quotes the
arguments after `--` in `agent start`; that plain `herdr pane list` / `agent
list` on the client show only the client's panes; the list commands through
`--machine` returning the same shapes; building and running on macOS; whether
the 3-second timeout is enough for `/v1/sessions` over a relayed Tailscale
path; the service surviving a server reboot.
