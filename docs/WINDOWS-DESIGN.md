# hound on Windows — design

Status: design, awaiting approval (builder questions resolved). Builds on `docs/DESIGN.md` and
`docs/REMOTE-DESIGN.md`.

## Intent

herdr runs on Windows (beta). A Windows user should be able to build hound,
bind the popup in herdr, search their Claude Code and pi sessions, focus a
live agent or shell pane, and resume a closed session — the same single-machine
experience Linux and macOS have. A Windows tester will run builds and report;
nobody on the development side has a Windows machine, so the code is written
from herdr's source and documentation, checked by CI on a Windows runner, and
verified by hand afterwards.

## What herdr does on Windows (herdr v0.9.5 source and docs)

- **Transport.** The API is newline-delimited JSON, as on Unix, over a Windows
  named pipe. herdr connects with the `interprocess` crate:
  `path.to_string_lossy().to_ns_name::<GenericNamespaced>()` then
  `local_socket::Stream::connect` (`src/ipc.rs`, `connect_local_stream`). The
  pipe's name is the socket path string itself.
- **`herdr.sock` is a marker file** holding `<server pid>:<timestamp in ns>`,
  used only to detect a stale socket. It is not a port or a token. No
  authentication beyond Windows' per-account pipe access is involved.
- **Environment.** Panes get `HERDR_SOCKET_PATH`, `HERDR_ENV=1`,
  `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_PANE_ID`. Custom-command popups
  get `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`, `HERDR_ACTIVE_*`. No Windows
  differences are documented.
- **Custom commands** (`[[keys.command]]`) run through `cmd.exe /d /c`.
  Plugin commands are argv arrays and resolve `PATHEXT` shims.
- **Config file:** `%APPDATA%\herdr\config.toml` (override `HERDR_CONFIG_PATH`).
- **Default pane shell:** `pwsh.exe`, else `powershell.exe`.
- **Agents with Windows integration:** Pi, OMP, Claude Code, Codex, Copilot,
  Devin, OpenCode, Kilo, Droid, Kimi, Qoder, Antigravity.
- **CLI commands** hound uses (`workspace/tab/pane/agent list`, `pane read`,
  `workspace/tab/agent focus`, `tab create`, `agent start`) have no documented
  Windows differences.

## Goals

1. hound builds and its test suite passes on Windows (x86_64, MSVC toolchain).
2. `hound ui`, `hound index`, `hound debug`, `hound install`, `hound enrich` work
   on Windows inside herdr and in a plain terminal.
3. Focusing any pane by ID works over herdr's named pipe, with the existing CLI
   fallback when the pipe is unavailable.
4. Resuming a closed session works whether the agent is installed as an `.exe`
   or an npm `.cmd` shim.
5. A Windows client can use `[[remote]]` blocks to search a Linux or macOS
   machine running `hound serve`.
6. CI builds and tests Linux, macOS and Windows on every push.

## Non-goals

- `hound serve` on Windows. It exits with a clear message.
- Windows on ARM, 32-bit Windows, the GNU toolchain.
- WSL as a special case (inside WSL hound is a Linux program).
- Prebuilt Windows binaries (the release project).
- The herdr plugin manifest on Windows (herdr marks plugins as preview).

## Anti-goals

- Any change in behaviour on Linux or macOS.
- A Windows code path that silently degrades: when something cannot work on
  Windows, hound says so in one line.
- Guessing the pipe name: hound derives it exactly as herdr does.

## Approaches considered

- **A. Use `interprocess`, as herdr does, for the Windows pipe.** Exactly
  herdr's naming; one Windows-only dependency.
- **B. Open `\\.\pipe\<name>` as a file.** No dependency; hound must reproduce
  the library's naming rule by hand and an error would only show on the
  tester's machine.
- **C. No pipe on Windows; CLI only.** No new code; shell panes cannot be
  focused.

**Chosen: A** (by the user).

## Design

### 1. herdr socket client (`crates/hound/src/herdr/socket.rs`)

The request/response logic stays exactly as it is today and is shared: one
JSON line out, one line back, a response whose `id` differs is an error, error
envelopes are errors, 10-second read and write timeouts. Only the stream
differs:

- Unix: `UnixStream` as today.
- Windows: `interprocess::local_socket::Stream` connected with
  `GenericNamespaced` to the `HERDR_SOCKET_PATH` string, exactly as herdr's
  `connect_local_stream` does. `interprocess` is a
  `[target.'cfg(windows)'.dependencies]` entry of the `hound` crate, at the
  version herdr uses (2.4.x).

herdr's own client treats `Unsupported` from the pipe's receive timeout as
normal on Windows, so on Windows the whole call (connect, write, read) runs on
a helper thread and the caller waits at most 10 seconds, abandoning the thread
on timeout, as the remote snapshot limit already does. The Unix path is
unchanged.

### 2. Finding programs (`herdr.rs`, `main.rs`)

One pure function, `resolve_program(name, path_var, pathext, is_file) ->
Option<PathBuf>`:

- A name containing a path separator, or (on Windows) already ending in an
  extension listed in `PATHEXT`, is used as is.
- Otherwise each `PATH` directory is tried in order; on Windows, for each
  directory, the bare name is skipped and each `PATHEXT` extension (default
  `.COM;.EXE;.BAT;.CMD` when unset) is tried in order; on Unix, the bare name
  with the executable bit, as today.

It is used for finding `herdr` (when `HERDR_BIN_PATH` is unset) and for the
standalone resume (`RunHere`), which now runs the resolved path.

On Windows the standalone resume cannot replace hound's process, so hound runs
the agent as a child and waits. While it waits, hound registers a console
control handler that swallows Ctrl+C and Ctrl+Break for hound itself, so hound
does not exit underneath the agent. (The simpler `SetConsoleCtrlHandler(NULL,
TRUE)` is not used: the "ignore Ctrl+C" flag it sets is inherited by child
processes, so the agent would ignore Ctrl+C too. A handler is not inherited.) hound then exits with
the agent's exit code. This uses `windows-sys`, which `interprocess` already
depends on, as a direct Windows-only dependency; no new crate enters the
build. The harness
name passed to herdr's `agent start --kind` is unchanged. Standard library
`Command` runs `.cmd`/`.bat` files through `cmd.exe` with argument escaping on
Windows; session IDs are already restricted to `[A-Za-z0-9_-]`, so no argument
needs quoting.

### 3. Default locations (`hound-core/src/config.rs`)

| | Unix | Windows |
|---|---|---|
| config | `~/.config/hound/config.toml` | `%APPDATA%\hound\config.toml` |
| state | `~/.local/state/hound` | `%LOCALAPPDATA%\hound` |
| Claude sessions | `~/.claude/projects` | `%USERPROFILE%\.claude\projects` |
| pi sessions | `~/.pi/agent/sessions` | `%USERPROFILE%\.pi\agent\sessions` |

`HOUND_CONFIG` / `HOUND_STATE_DIR` overrides and `~` expansion behave as today.
The defaults come from a pure function given the platform and the environment
values, tested on every platform. The Claude and pi locations on Windows are
assumptions to verify (§ Verification).

### 4. `hound install` on Windows (`main.rs`)

The printed bindings name the config file `%APPDATA%\herdr\config.toml`.
herdr passes the command to `cmd.exe /d /c` unescaped, and `cmd.exe` strips the
first and last quote of the command line unless the quoting is trivially
simple, so a quoted path containing spaces and `(`, `&`, `^` or `@` would
otherwise lose its quotes. hound therefore prints the command as
`""<exe>" ui --scope all"`: the path quoted, and the whole command wrapped in
one more pair of quotes that `cmd.exe` removes. A path containing `"` or `%`
(which `cmd.exe` expands even inside quotes) is refused with a one-line error.
The executable path is printed without the `\\?\` prefix that
`canonicalize` adds on Windows (and without canonicalising a UNC path into a
form `cmd.exe` cannot run). `install_text` takes the platform as a parameter so both forms are
tested everywhere.

### 5. Path matching (`hound-core/src/search.rs`)

`strip_home` treats `\` and `/` as separators and compares Windows-style
paths (drive letter, or `\\` prefix) case-insensitively. A Linux remote's cwds
shown on a Windows client still strip against the remote's own home, as today.

### 6. Background enrichment (`ui/mod.rs`)

On Windows the spawned `hound enrich` gets creation flags
`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`, the
counterpart of `process_group(0)` on Unix.

### 7. Index rebuild (`hound-core/src/index/mod.rs`)

Windows refuses to delete index files another process has open. If removing
the index directory during a rebuild fails, `Index::open` returns an error
saying another hound process is using the index and to close it and retry; no
partial deletion is left behind that the next run cannot rebuild from.

### 8. Terminal and paste (`ui/mod.rs`, `ui/app.rs`)

crossterm's Windows input reader never reports a paste as one event: pasted
text arrives as individual key presses, and a newline in it arrives as Enter.
Enter opens the selected entry, so a pasted newline could focus or resume
something the user did not choose. On every platform, hound treats key
presses that arrive as a burst as pasted text: within one batch of events
already waiting, a run of three or more printable presses, and any Enter that
comes before the batch's last printable press (whatever keys lie between), is
pasted text — printable characters
join the query and those Enters do nothing. A lone Enter, or one or two
characters followed by Enter, is delivered as typed; two Enters in one batch
are both delivered, so a confirming second Enter still works. Accepted costs,
both failing safe (nothing opens): a fast typist whose three characters and
Enter land in one batch while the popup is busy has that Enter dropped, and a
paste split across two batches can deliver the Enter of a short final line;
and once a batch has contained pasted text, an Enter later in that same batch
(for example after an arrow key) is not delivered either. The burst rule
is a pure function over a batch of events, tested everywhere. Failure to
enable bracketed paste is ignored; failure to enter the alternate screen or
raw mode remains an error.

### 9. `hound serve` and the token file

`hound serve`, `hound serve --show-token` and `hound install --serve` exit 1 on
Windows with `hound serve is not supported on Windows`. The Unix-only token
creation, ownership and permission code is compiled on Unix only. Reading a
token file (the client side) is portable.

### 10. Tests

Tests that use shell-script stand-ins for herdr, Unix file modes, or
`geteuid` are marked `#[cfg(unix)]`. Every new pure function (program
resolution, default locations, install text, path stripping) is tested on all
platforms with both Unix and Windows inputs.

### 11. CI (`.github/workflows/ci.yml`)

On push and pull request: `cargo test --workspace` and `cargo clippy
--workspace --all-targets -- -D warnings` on `ubuntu-latest`, `macos-latest`
and `windows-latest`; `cargo fmt --check` once. The toolchain comes from
`rust-toolchain.toml`. Ignored live tests do not run.

### 12. README

A "Windows" section: status (built and tested in CI, herdr integration
unverified), building, the binding snippet location, `hound serve` not
available, and the tester's checklist below.

## Verification on a real Windows machine

Run by the tester after CI is green, in this order:

1. `cargo build --release` and `cargo test --workspace` succeed.
2. `hound index` finds Claude and pi sessions (confirms the session
   locations in § 3).
3. `hound ui` in a plain PowerShell window: search, then Enter on a closed
   session resumes it in that window (confirms program resolution).
4. Inside herdr: paste `hound install`'s bindings into
   `%APPDATA%\herdr\config.toml`, `herdr server reload-config`, press
   `prefix+space`: the popup opens and live agents show with workspace and tab.
5. Enter on a live agent focuses it; Enter on a live plain shell pane focuses
   it (this one needs the named pipe).
6. Enter on a closed session opens a new tab and resumes it.
7. Optional: a `[[remote]]` block pointing at a Linux `hound serve`.

## Calls made by Claude

- `hound serve` excluded on Windows.
- `%APPDATA%` / `%LOCALAPPDATA%` for hound's own config and state.
- Program lookup hand-written rather than a crate.
- CI added in this project, without releases.
- Bracketed paste failure ignored; on every platform an Enter that arrives in
  a burst of key presses is treated as pasted and does nothing.
- On Windows the standalone resume swallows Ctrl+C in hound (through a
  console control handler, not the inherited ignore flag) while the agent
  runs, using `windows-sys` directly.
- `install` wraps the whole Windows command in an extra pair of quotes and
  strips the `\\?\` prefix.
- The socket client keeps today's one-response behaviour; Windows adds an
  overall 10-second limit through a helper thread.

## Left to the builder

- Exact `interprocess` API calls and how timeouts are applied.
- The CI workflow's caching and job layout.
