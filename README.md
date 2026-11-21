# hound

hound indexes the session files of AI coding agents and lets you search them
from a popup, then jump to the result: it focuses the pane if the session is
still running, or resumes it if it is closed. It runs as a popup inside
[herdr](https://herdr.dev), or standalone in any terminal.

```text
┌ hound ─  Active  Old [All] ───────────────────── subagents: on ─ 44 sessions ┐
│ > signal                                                                     │
├──────────────────────────────────────────────────────────────────────────────┤
│ ● claude  Depot alerts                                          Billing › t8 │
│     …send to the Ops Alerts Signal group…                                    │
│ ○ claude  nginx setup                                          3d  tools-dev │
│     [subagent] nginx config                                                  │
│ ○ pi      scraper retries                                      2w  ledgerapp │
└─ ⏎ open  tab scope  ^s subagents  esc close ─────────────────────────────────┘
```

`●` is a session running in a live pane, `○` a closed one. The second line of a
row is the text that matched.

## Quick start

```bash
git clone https://github.com/enarsee/hound && cd hound
cargo build --release                 # needs a recent Rust toolchain

./target/release/hound ui             # try it right away, in any terminal
```

Type to search, Enter to open, Esc to close. Outside herdr, Enter resumes the
session in the terminal you are in.

To get the popup on a key inside herdr:

```bash
./target/release/hound install        # prints three key bindings; changes nothing
```

Paste what it prints into `~/.config/herdr/config.toml`, then:

```bash
herdr server reload-config
```

| Key              | Opens                     |
|------------------|---------------------------|
| `prefix+space`   | all sessions              |
| `prefix+a`       | live agents               |
| `prefix+shift+o` | closed sessions           |

The first open indexes your sessions in the background; after that the index
is kept up to date each time the popup opens. Nothing is sent anywhere: hound
reads local files and talks only to herdr, unless you turn on the optional
features described under [Enrichment](#enrichment) and
[Advanced: several machines](#advanced-several-machines).

Supported agents:

| Agent       | Sessions read from          | Status          |
|-------------|-----------------------------|-----------------|
| Claude Code | `~/.claude/projects`        | supported       |
| pi          | `~/.pi/agent/sessions`      | supported       |
| opencode    | —                           | not supported   |

hound only reads session files; it never writes, moves or locks them.

opencode is not supported yet. Since v1.2.0 it keeps sessions in a SQLite
database (`~/.local/share/opencode/opencode.db`) and updates message and part
rows in place, while hound's adapters read append-only files by byte offset.
Supporting it needs an adapter that reads the database and tracks a
per-session version instead of a file offset.

## Building

Requires a Rust toolchain with edition 2024 support.

```bash
cargo build --release          # binary at target/release/hound
# or put it on PATH:
cargo install --path crates/hound
```

## Commands

```text
hound ui [--scope active|old|all]   open the search popup (default scope: all)
hound index                         bring the index up to date, print counts
hound debug <session-id-or-prefix>  print an indexed session and its messages
hound install                       print the herdr key bindings (changes nothing)
hound enrich                        summarise quiet sessions with an LLM (off by default)
```

`hound index` prints `added N, updated N, removed N, unchanged N`, or
`another hound is indexing` if another hound process holds the index's writer
lock. The popup refreshes the index in the background on every open, so
running `hound index` by hand is optional.

## Installing in herdr

### Primary: popup key bindings

Run `hound install`. It prints three `[[keys.command]]` entries with
`type = "popup"` that run hound by the absolute path of the binary you ran it
from. Add them to `~/.config/herdr/config.toml` (create the file if needed),
then run `herdr server reload-config` or restart herdr. (`herdr --help` in
herdr 0.9.3 lists `herdr server reload-config` as "Reload config.toml in the
running server".)

| Key              | Opens                     |
|------------------|---------------------------|
| `prefix+space`   | `hound ui --scope all`    |
| `prefix+a`       | `hound ui --scope active` |
| `prefix+shift+o` | `hound ui --scope old`    |

The popups are 80% of the terminal in each dimension. These keys are free in
herdr 0.9.3's default bindings (`herdr --default-config`); `prefix+o` is not
used because it is herdr's default `open_notification_target`.

hound is developed against herdr 0.9.3; earlier versions are untested.

### Optional: herdr plugin

`herdr-plugin.toml` at the repository root declares hound as a herdr plugin:
three popup panes (`all`, `active`, `old`) and three actions
(`search-all`, `search-active`, `search-old`) that each open the matching
pane. Link it from the checkout (no build step runs for linked plugins):

```bash
herdr plugin link /path/to/hound
```

The pane commands are `hound ui --scope …` as argv arrays run without a shell,
so `hound` must be on the PATH of the herdr server (for example via
`cargo install --path crates/hound`). The plugin is an alternative to the popup
bindings above, not an addition: use one or the other. The example below
reuses `prefix+space`, so remove the popup binding for that key if you switch.
Bind an action to a key with:

```toml
[[keys.command]]
key = "prefix+space"
type = "plugin_action"
command = "hound.search-all"
description = "hound: search all sessions"
```

What herdr's documentation says, and does not say, about this path
(herdr 0.9.5 docs, `plugins.mdx`, `configuration.mdx`, `cli-reference.mdx`):

- **Can a plugin action be bound to a key?** Yes:
  `[[keys.command]] type = "plugin_action"` with the action id
  (Plugins → Keybindings; Configuration → custom commands).
- **Can an action open a plugin pane with arguments?** Not directly. A
  `[[panes]]` entry has a fixed argv; `herdr plugin pane open --plugin ID
  --entrypoint ID` accepts placement, size, `--cwd` and `--env KEY=VALUE`, but
  no extra arguments for the pane command. Nothing documents an action opening
  a pane by itself, so each action here runs
  `$HERDR_BIN_PATH plugin pane open …` through `sh`. This is why there is one
  pane per scope. This two-step chain has not been tried in a live session.
- **Is a double-tap-Shift binding possible?** The documentation does not
  describe one. Key strings are single chords (`ctrl+a`, `prefix+n`, …) and the
  keyboard page says modifier-only presses are not forwarded to panes. No such
  binding is offered here.

Because the plugin path relies on that undocumented action-to-pane chain, the
popup key bindings above stay the primary install path.

## Using the popup

Type to search. Results update as you type.

| Key                       | Does                                            |
|---------------------------|-------------------------------------------------|
| Enter                     | focus the live pane, or resume the closed session |
| Tab / Shift+Tab           | cycle scope: active → old → all                 |
| Up / Down, Ctrl+N / Ctrl+P | move the selection                             |
| Ctrl+S                    | include or exclude subagent messages in matches |
| Ctrl+U                    | clear the query                                 |
| Backspace                 | delete a character                              |
| Esc / Ctrl+C              | close                                           |

Scopes:

- **active** — sessions and agents with a live herdr pane.
- **old** — sessions without a live pane.
- **all** — both.

When Enter needs confirmation (for example a session that may be running
outside herdr), the popup asks, and a second Enter on the same entry confirms.

## Configuration

`~/.config/hound/config.toml` (override the path with `HOUND_CONFIG`). Every
key is optional; a missing file means the defaults.

```toml
index_subagents = true                    # index Claude subagent transcripts
search_subagents = true                   # include subagent matches by default (Ctrl+S toggles)
claude_root = "~/.claude/projects"        # default shown
pi_root = "~/.pi/agent/sessions"          # default shown

[enrich]                                  # LLM enrichment, see below
enabled = false
base_url = "http://localhost:11434/v1"    # OpenAI-compatible endpoint
model = ""
api_key_env = "SOME_ENV_VAR"              # optional: env var holding the API key
categories = []
quiet_secs = 600                          # a session must be quiet this long
```

A leading `~/` (or a bare `~`) in `claude_root` and `pi_root` is your home
directory; other paths are used as written. If a root cannot be read (a
typo, a missing mount), the sessions already indexed from it are kept, and
`hound index` prints a warning naming the path.

## Enrichment

With `enrich.enabled = true` and a `model` set, `hound enrich` asks the
configured OpenAI-compatible endpoint (`POST {base_url}/chat/completions`) for a
one-line summary, a category and up to five tags for each session that has
been quiet for `quiet_secs` and is new or has grown by half since it was last
enriched. The popup shows and searches these fields, and starts
`hound enrich` in the background after each refresh. Run it by hand to see
`enriched N, failed N, skipped N`, with one line on stderr per failed session
(the first five).

- Categories are `categories` from the config or, if that is empty, the
  current herdr workspace names; `Other` is always allowed.
- What is sent: the session title, the name of its project directory (the
  last path component, not the full path) and an excerpt of the conversation
  (the first three prompts and the last ten messages, at most 6000
  characters). Use a local endpoint such as Ollama if that text must not leave
  the machine.
- The API key, if any, is read from the environment variable named by
  `api_key_env` and sent as a bearer token.
- The client honours the `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`
  environment variables (also lowercase; the first set of `ALL_PROXY`,
  `HTTPS_PROXY`, `HTTP_PROXY` is used for every request, whatever the
  endpoint's scheme) and `NO_PROXY`: when a proxy applies, the conversation
  text sent for summarising goes through it.
- Requests time out after 120 seconds. Only one `hound enrich` runs at a
  time; another prints `another hound is enriching` and exits.
- When the popup starts `hound enrich`, its failure lines go to
  `enrich.log` in the state directory (session keys and reasons only, no
  conversation text). The file is emptied when it has grown past 1 MiB.
- On a machine running `hound serve` (see [Advanced: several
  machines](#advanced-several-machines)), the service owns enrichment: the
  popup there finds the index freshly refreshed, skips its own refresh and so
  never starts `hound enrich`. The service enriches only after the index
  changed, at most every 10 minutes, and leaves a session whose enrichment
  failed alone for an hour, doubling with each further failure up to a day.
  It needs `enrich.categories` set (it has no herdr workspace names to fall
  back on); without them it does not enrich and says so once when it starts.

## State

hound keeps its index in `~/.local/state/hound/` (override with
`HOUND_STATE_DIR`):

- `index/` — the search index
- `state.json` — schema version and time of the last refresh
- `writer.lock` — held while the index is being written
- `enrich.lock` — held while `hound enrich` runs
- `enrich.log` — failures of `hound enrich` runs started by the popup

Deleting the directory is safe; the next run rebuilds it.

## Running outside herdr

`hound ui` works in any terminal. hound treats itself as inside herdr when
`HERDR_ENV` is `1` or `HERDR_SOCKET_PATH` is set, and drives the herdr binary
named by `HERDR_BIN_PATH` (or `herdr` on `PATH`). Outside herdr there are
no live panes: every session is listed as closed, and Enter replaces the hound
process with the resumed session (`claude --resume …`, `pi --session …`) in
the session's directory, in the same terminal (on Windows hound runs it as a
child instead; see [Windows](#windows)).

## Windows

**Status:** CI builds hound and runs its tests on Windows (`windows-latest`).
hound has not been run by hand on a Windows machine, and its herdr
integration (the popup, focusing panes over herdr's named pipe, resuming in a
new tab) has not been tried. Expect rough edges.

### Building on Windows

Two dependencies (`ring` and `zstd-sys`) compile C code, so besides Rust from
[rustup](https://rustup.rs) you need the Microsoft C++ build tools (the
"Desktop development with C++" workload of Visual Studio or of the Build
Tools for Visual Studio). In a Developer PowerShell:

```powershell
git clone https://github.com/enarsee/hound; cd hound
cargo build --release          # binary at target\release\hound.exe
```

### Where hound keeps its files on Windows

| What                 | Windows default                    |
|----------------------|------------------------------------|
| config               | `%APPDATA%\hound\config.toml`      |
| index and state      | `%LOCALAPPDATA%\hound`             |
| Claude Code sessions | `%USERPROFILE%\.claude\projects`   |
| pi sessions          | `%USERPROFILE%\.pi\agent\sessions` |

`HOUND_CONFIG` and `HOUND_STATE_DIR` override the first two, as on Unix, and a
leading `~` in `claude_root` or `pi_root` is your profile folder (for example
`C:\Users\alice`). The two session locations are where the agents are
expected to keep their files on Windows; that has not been confirmed yet.

When the index has to be rebuilt while another hound has it open, Windows
does not let the old index be deleted; hound then stops with
`another hound process is using the index; close it and retry`.

### Differences from Linux and macOS

- `hound install` prints the bindings for `%APPDATA%\herdr\config.toml`.
  Each `command` wraps the quoted path to `hound.exe` in one more pair of
  quotes, because herdr runs it through `cmd.exe`, which strips the outer
  pair. A path to `hound.exe` containing `"` or `%` is refused.
- `hound serve` and `hound install --serve` are not available: they exit
  with `hound serve is not supported on Windows`. A Windows machine can
  still search another machine running `hound serve` through a `[[remote]]`
  block.
- Inside herdr, hound talks to herdr over its named pipe, and falls back to
  the `herdr` command when the pipe cannot be connected.
- Outside herdr, Enter starts the agent (found on `PATH` using `PATHEXT`, so
  a `claude.cmd` shim works) as a child of hound in the same window, instead
  of replacing hound. Ctrl+C goes to the agent while hound waits; hound then
  exits with the agent's exit code. hound stays running for the whole agent
  session, keeping the index files open; while it does, `hound.exe` cannot be
  overwritten, so a `cargo build` of hound fails until the agent exits.
- `hound install` prints no `herdr plugin link` line: the herdr plugin is not
  offered on Windows. Use the popup bindings.
- The Windows console does not deliver a paste as one event, so the popup
  treats a burst of key presses as pasted text: it joins the query, a line
  break in it becomes a space, and it never opens anything. A character typed
  with AltGr (which Windows reports as Ctrl+Alt), such as `@` or `{` on a
  German keyboard, is typed as text.

### Trying it on a real Windows machine

These checks would move Windows support out of "tests only", in this order:

1. `cargo build --release` and `cargo test --workspace` succeed.
2. `hound index` finds Claude and pi sessions (confirms the session
   locations above).
3. `hound ui` in a plain PowerShell window: search, then Enter on a closed
   session resumes it in that window.
4. In that resumed agent, press Ctrl+C: the agent handles it (for example
   interrupts or asks to confirm) and the window does not drop back to
   PowerShell. Quit the agent normally; `echo $LASTEXITCODE` then shows the
   agent's exit code.
5. In the popup in a plain PowerShell window, with a keyboard layout that has
   AltGr (German, French or Polish, for example): type `@`, `{` and `\` with
   AltGr, and a non-ASCII letter such as `é`, `ü` or `ł`: each appears in the
   query. Then paste one line (`fix parser`), and a three-line text
   (`alpha`, `beta`, `gamma` on separate lines): each lands in the query, the
   lines joined by spaces, and nothing opens or resumes.
6. Inside herdr: paste `hound install`'s bindings into
   `%APPDATA%\herdr\config.toml`, run `herdr server reload-config`, press
   `prefix+space`: the popup opens and live agents show with workspace and
   tab.
7. Enter on a live agent focuses it; Enter on a live plain shell pane
   focuses it (this one needs the named pipe).
8. Enter on a closed session opens a new tab and resumes it.
9. Repeat step 5 in the popup inside herdr (typing, the one-line paste and
   the three-line paste): the same results, and nothing opens.
10. With `[enrich]` enabled, an endpoint that answers and at least one
    session not yet summarised (quiet for `quiet_secs`), open the popup
    inside herdr and close it with Esc as soon as it has loaded. Task Manager
    still shows a `hound.exe` (the background `hound enrich`) until it
    finishes, and the next popup shows the new summaries; `enrich.log` in
    `%LOCALAPPDATA%\hound` names any failures.
11. Optional: a `[[remote]]` block pointing at a Linux `hound serve`.

## Advanced: several machines

Off by default. With no `[serve]` and no `[[remote]]` in the config, hound
listens on nothing, contacts nothing, and everything above is all there is.
Skip this section unless you run herdr on more than one machine and want one
popup to search them all.

Turning it on has two halves. On a machine whose sessions you want to reach,
`hound serve` offers that machine's index over HTTP to anyone holding its
token. On the machine you sit at, a `[[remote]]` block makes the popup search
that server too, and Enter on one of its results acts on that machine through
`herdr --machine <label>`.

### What `hound serve` exposes, and what it does not

To a client presenting the token, the service answers with:

- the hostname, the server user's home directory, the hound version and the
  number of sessions;
- for every indexed session: its harness and ID, title (the harness's title
  or, for an untitled session, its first prompt cut to 80 characters on one
  line), working directories, resume directory, start and update times, the
  enrichment summary, category and tags if any, the path of its main
  transcript, when it last changed, and whether its directory still exists;
- for a search term, which sessions' conversations contain it, each with a
  snippet of at most 120 characters around the match.

No single answer contains a whole conversation, but a token holder can send
any terms they like and get a snippet around every match, so repeated
searches can recover much of every conversation, subagent transcripts
included. Treat the token as being as sensitive as the session files
themselves. The service has no endpoint that changes, starts or resumes
anything, and never logs query text, snippets or the token.
Every request needs `Authorization: Bearer <token>`; anything else gets an
empty `401`. The traffic is plain HTTP: run it only over an encrypted private
network such as Tailscale.

### The server: `[serve]`, token and systemd unit

```toml
[serve]
enabled = true                 # default false: hound serve refuses to start
bind = "100.64.0.1:7717"       # required; this machine's Tailscale address
refresh_secs = 30              # how often the service refreshes the index
# allow_any_interface = false  # binding 0.0.0.0 or [::] is refused unless true
```

Bind to the machine's tailnet address (`tailscale ip -4` prints it), never to
all interfaces: `0.0.0.0` and `[::]` are refused unless you also set
`allow_any_interface = true`, which you should not.

The token lives in `serve-token` next to the config file
(`~/.config/hound/serve-token`), created with mode 0600 on first start.
`hound serve --show-token` prints it. Copy it to each client into a file only
you can read, for example `~/.config/hound/remotes/homelab.token`.

`hound serve` runs in the foreground, refreshing the index every
`refresh_secs`. With enrichment enabled and `enrich.categories` set it also
enriches, at most every 10 minutes after the index changed and backing off
from sessions that keep failing (see [Enrichment](#enrichment)).
`hound install --serve` prints a systemd user unit for it and the commands to
enable it; it installs nothing itself:

```bash
mkdir -p ~/.config/systemd/user
hound install --serve > ~/.config/systemd/user/hound-serve.service
systemctl --user daemon-reload
systemctl --user enable --now hound-serve.service
loginctl enable-linger         # optional: keep it running while logged out
```

The unit restarts the service 5 seconds after a failure, without giving up,
so a service started at boot before the Tailscale address exists keeps
trying until it can bind.

After upgrading hound on a machine that runs the service, restart it:
`systemctl --user restart hound-serve`. Until then the old process keeps
serving with the old code.

`hound serve` is supported on Linux. On macOS nothing has been compiled or
run yet, neither the service nor the client side (see [Not yet
verified](#not-yet-verified)).

### The client: `[[remote]]`

```toml
[[remote]]
machine = "homelab"                                # herdr's label for that machine
url = "http://100.64.0.1:7717"                     # Tailscale IP or full tailnet name
token_file = "~/.config/hound/remotes/homelab.token"
enabled = true                                     # optional; false ignores the block
timeout_secs = 3                                   # optional; 1 to 30, default 3
```

In `url`, use the machine's Tailscale IP address or its full tailnet name
(`homelab.example-tailnet.ts.net`), not a short host name: when Tailscale is
down, a short name can resolve to a different machine, and the token would be
sent there in clear text.

`timeout_secs` bounds connecting to that remote and, separately, each request
to it; values outside 1 to 30 are taken as the nearest of the two.

`machine` must be the label herdr uses for that machine (the one
`herdr --machine <label>` takes). A block with `enabled = false` is ignored
entirely. A remote named `local` (in any case), one whose label herdr would
not accept, or two remotes sharing a label are not used, and the footer says
`<label>: invalid machine name` or `<label>: duplicate machine name`.

With a remote configured, each popup loads that machine's sessions and live
panes in the background (requests time out after `timeout_secs`) and merges
them with the local results; every row then names its machine, and the
header counts the sessions of every machine loaded. Local results never wait
for a remote. Problems show in the footer and leave local results alone:

| Footer note                         | Meaning                                                    |
|-------------------------------------|------------------------------------------------------------|
| `homelab: loading…`                 | the remote's sessions and panes are still being fetched   |
| `homelab: needs herdr`              | the popup runs outside herdr; remotes are skipped          |
| `homelab: no token`                 | `token_file` is missing, unreadable or empty               |
| `homelab: unreachable`              | no answer from `url` (also shown when a search's request fails after loading; cleared by the next search that succeeds) |
| `homelab: unauthorised`             | the token was refused                                      |
| `homelab: needs a newer hound`      | the server speaks a newer protocol; update this machine    |
| `homelab: needs an update there`    | the server speaks an older protocol; update the server     |
| `homelab: too many sessions`        | the session list is over 8 MiB                             |
| `homelab: herdr machine not reachable` | herdr could not list that machine's panes within 5 seconds; its sessions are listed as closed |
| `homelab: internal error`           | hound failed while handling that remote's data; it is left out until the popup is reopened |

If the server is unreachable but herdr can list the machine's panes, its
live panes are still listed (matched by name only) and can be focused, but
nothing there can be resumed.

Enter on a remote result always acts on that machine through
`herdr --machine <label>`, never in the local terminal: a live agent is
focused with `agent focus`; a closed session is resumed in a new tab there,
after asking that machine's service whether its directory still exists and
whether it changed in the last two minutes. A local result never goes
through `--machine`.

### Which popup runs where

herdr runs a popup on the server whose workspace is selected. With one of
the client's own workspaces selected, its key binding opens the client's
hound, which shows every machine. With a workspace of an attached machine
selected, that machine's hound opens and shows only its own sessions (it has
no route back). Bind the same keys on every machine.

## Development

`cargo test --workspace` uses fixtures only: tests never read `~/.claude`,
`~/.pi` or hound's state, and never talk to a running herdr. A few
`#[ignore]`d tests read real data instead (sanity runs over the real Claude
and pi session directories, which print counts only, and a read-only check
against the live herdr socket). They do nothing unless `HOUND_LIVE_TESTS=1`
is set, so `cargo test -- --include-ignored` alone stays off real data:

```bash
HOUND_LIVE_TESTS=1 cargo test --workspace -- --ignored --nocapture
```

CI (GitHub Actions) builds, tests, lints and checks formatting on Linux,
macOS and Windows for pushes to `main`, version tags and pull requests. Run it
on any other branch by hand with `gh workflow run CI --ref <branch>`.

## Versions and releases

hound follows semantic versioning while it is at 0.x: a minor release
(0.2.0) brings features or breaking changes, a patch release (0.1.1) fixes.
The version lives in `Cargo.toml` and is copied into `herdr-plugin.toml`; a
test fails if the two differ. `hound --version` prints it.

Releases are cut with [cargo-release](https://github.com/crate-ci/cargo-release)
from a `main` whose CI is green:

```bash
cargo install cargo-release
cargo release minor              # dry run: shows the version bump, commit and tag
cargo release minor --execute    # commits "chore: release vX.Y.Z", tags vX.Y.Z, pushes
```

One commit updates both version copies, and one tag `vX.Y.Z` marks it.
Nothing is published to crates.io. Pushing the tag creates a GitHub release
whose notes list the commits since the previous tag; the release is refused if
the tag does not match the version in both files.

A fixed version can be installed through herdr's plugin system by naming the
tag (the plugin path has not yet been tried in a live session; see
[Optional: herdr plugin](#optional-herdr-plugin)):

```bash
herdr plugin install enarsee/hound --ref v0.1.0
```

## Status

hound is young. The core — indexing, search, the popup, focusing and resuming
on one machine — is in daily use. The parts below are marked by how much real
use they have had.

**Tried on a real setup** (a macOS herdr client attached to a Linux herdr
0.9.3 server, with hound running on the server):

- The key bindings open the popup from the attached client, each in its scope.
- herdr is detected inside the popup; live agents show with their workspace
  and tab.
- Enter focuses a live agent, across workspaces.
- Enter resumes a closed session in a new tab.
- `herdr --machine <label> agent focus` moves the client's view to the pane.

**Covered by tests only, not yet tried by hand:**

- Everything under [Advanced: several machines](#advanced-several-machines):
  `hound serve` on a real network, the merged list, and opening a remote
  session. In particular `tab create` and `agent start` through
  `herdr --machine`, whether the client's view follows after a remote resume,
  and whether the list commands through `--machine` return the same shapes as
  locally.
- Running on macOS. CI builds and tests hound on macOS, but it has only been
  run by hand on Linux.
- Everything under [Windows](#windows): CI builds and tests hound on Windows,
  but nothing there, including the herdr integration, has been run by hand.
- The herdr plugin manifest (`herdr-plugin.toml`): an action opening a popup
  pane through `herdr plugin pane open`.
- Running outside herdr: opening, typing and closing were tried; Enter handing
  the terminal to `claude --resume …` / `pi --session …` was not.
- Whether `claude --resume <id>` and `pi --session <id>` keep the same session
  ID and file, or start a new one (which hound would index as a new session).
- Whether the enrichment run started by the popup survives the popup closing.
- The service coming back by itself after the machine reboots.
- opencode is not supported (see the top of this file).

Design notes: [docs/DESIGN.md](docs/DESIGN.md) and
[docs/REMOTE-DESIGN.md](docs/REMOTE-DESIGN.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
