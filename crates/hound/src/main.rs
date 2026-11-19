//! The `hound` command line.

mod serve;

use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use hound::herdr::CliHerdr;
use hound::program::resolve_on_this_machine;
use hound::text::printable;
use hound::ui;
use hound_core::config::{self, Config};
use hound_core::enrich::{self, Attempt, EnrichStats, HttpClient};
use hound_core::host::Host;
use hound_core::index::{Index, SessionDoc};
use hound_core::model::{Role, SessionKey};
use hound_core::search::Scope;

#[derive(Parser)]
#[command(
    name = "hound",
    version,
    about = "Search coding-agent sessions and jump to them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open the search popup.
    Ui {
        /// Which entries to search: active, old or all.
        #[arg(long, default_value = "all", value_parser = parse_scope)]
        scope: Scope,
    },
    /// Bring the index up to date and print what changed.
    Index,
    /// Print an indexed session and its messages.
    Debug {
        /// Session id, `<harness>:<id>`, or a unique prefix of either.
        session: String,
    },
    /// Print the herdr config snippet that binds hound to keys. Changes nothing.
    Install {
        /// Print a systemd user unit for `hound serve` instead.
        #[arg(long)]
        serve: bool,
    },
    /// Summarise sessions with the configured LLM endpoint.
    Enrich,
    /// Offer this machine's sessions to other machines over HTTP. Needs
    /// `serve.enabled = true` and `serve.bind` in the config.
    Serve {
        /// Print the access token (creating it if needed) and exit.
        #[arg(long)]
        show_token: bool,
    },
}

fn parse_scope(s: &str) -> Result<Scope, String> {
    Scope::parse(s).ok_or_else(|| format!("expected active, old or all, got {s:?}"))
}

/// A failure to report on stderr as one line before exiting with status 1.
struct Failure(String);

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure(format!("{e:#}"))
    }
}

type Outcome = Result<(), Failure>;

/// Longest a failed `hound ui` waits for a key before closing.
const ERROR_HOLD: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_ui = matches!(cli.command, Command::Ui { .. });
    let result = match cli.command {
        Command::Ui { scope } => run_ui(scope),
        Command::Index => run_index(),
        Command::Debug { session } => run_debug(&session),
        Command::Install { serve } => run_install(serve),
        Command::Enrich => run_enrich(),
        Command::Serve { show_token } => serve::run(show_token),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure(message)) => {
            // One line, whatever the error chain contained.
            eprintln!("hound: {}", printable(&message));
            if is_ui && hold_error_on_screen(io::stdout().is_terminal()) {
                wait_for_key();
            }
            ExitCode::FAILURE
        }
    }
}

/// Whether a failed `hound ui` keeps its error on screen until a key is
/// pressed. In a terminal it may be a popup that closes as soon as hound
/// exits, taking the message with it; otherwise the output is captured.
fn hold_error_on_screen(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
}

/// Prints `press any key to close` and waits for a key press, at most
/// [`ERROR_HOLD`].
fn wait_for_key() {
    use crossterm::event::{self, Event, KeyEventKind};
    use crossterm::terminal;

    eprintln!("press any key to close");
    if terminal::enable_raw_mode().is_err() {
        return;
    }
    let deadline = Instant::now() + ERROR_HOLD;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match event::poll(left) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read()
                    && key.kind != KeyEventKind::Release
                {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = terminal::disable_raw_mode();
}

fn load_config() -> anyhow::Result<Config> {
    Config::load(&config::config_path())
}

fn open_index() -> anyhow::Result<Index> {
    Index::open(&config::state_dir())
}

fn run_ui(scope: Scope) -> Outcome {
    let config = load_config()?;
    let index = open_index()?;
    // The popup's background threads are detached: return or exec straight away.
    match ui::run(scope, &config, index, CliHerdr::detect())? {
        None => Ok(()),
        Some((cwd, argv)) => Err(exec_in(&cwd, &argv)),
    }
}

/// Runs `argv` in `cwd` in place of hound (on Windows, as a child whose exit code hound
/// exits with); returns only on failure. `argv[0]` is looked up with `PATH` (and `PATHEXT`),
/// falling back to the bare name so a failure reports the system's own error.
fn exec_in(cwd: &str, argv: &[String]) -> Failure {
    let Some((program, args)) = argv.split_first() else {
        return Failure("nothing to run".to_string());
    };
    if let Err(e) = env::set_current_dir(cwd) {
        return Failure(format!("changing directory to {cwd}: {e}"));
    }
    let resolved = resolve_on_this_machine(program).unwrap_or_else(|| PathBuf::from(program));
    exec(program, &resolved, args)
}

/// Replaces this process with `path` run with `args`; `program` names it in an error.
#[cfg(unix)]
fn exec(program: &str, path: &Path, args: &[String]) -> Failure {
    use std::os::unix::process::CommandExt;
    let e = std::process::Command::new(path)
        .arg0(program)
        .args(args)
        .exec();
    Failure(format!("running {program}: {e}"))
}

/// Runs `path` with `args` as a child and exits with its exit code; `program` names it in
/// an error. While the child runs hound ignores Ctrl+C and Ctrl+Break, which reach only
/// the child, so hound does not exit underneath it.
#[cfg(windows)]
fn exec(program: &str, path: &Path, args: &[String]) -> Failure {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };

    /// Swallows Ctrl+C and Ctrl+Break in hound; other events get the default handling.
    /// A handler is registered rather than `SetConsoleCtrlHandler(None, TRUE)` because that
    /// ignore flag is inherited by child processes, and the agent must still see Ctrl+C.
    unsafe extern "system" fn ignore_interrupt(ctrl_type: u32) -> windows_sys::core::BOOL {
        (ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT).into()
    }

    // SAFETY: registers a handler that touches no state; it stays registered until exit.
    let registered = unsafe { SetConsoleCtrlHandler(Some(ignore_interrupt), 1) };
    if registered == 0 {
        return Failure(format!("ignoring Ctrl+C: {}", io::Error::last_os_error()));
    }
    match std::process::Command::new(path).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => Failure(format!("running {program}: {e}")),
    }
}

fn run_index() -> Outcome {
    let config = load_config()?;
    let index = open_index()?;
    match index.refresh(&ui::adapters(&config), &mut |_, _| {})? {
        None => println!("another hound is indexing"),
        Some(s) => {
            for root in &s.unavailable {
                eprintln!(
                    "hound: warning: {}; its sessions were left as indexed",
                    printable(&root.to_string())
                );
            }
            println!(
                "added {}, updated {}, removed {}, unchanged {}",
                s.added, s.updated, s.removed, s.unchanged
            );
        }
    }
    Ok(())
}

fn run_enrich() -> Outcome {
    let config = load_config()?;
    if !enrich_configured(&config)? {
        println!("enrichment disabled");
        return Ok(());
    }
    let index = open_index()?;
    let Some(pass) = enrich_pass(
        &config,
        &index,
        host_workspace_labels,
        &mut |_| true,
        &mut |_, _| {},
    )?
    else {
        println!("another hound is enriching");
        return Ok(());
    };
    for line in failure_lines(&pass.failures) {
        eprintln!("{line}");
    }
    let s = pass.stats;
    println!(
        "enriched {}, failed {}, skipped {}",
        s.enriched, s.failed, s.skipped
    );
    Ok(())
}

/// Whether enrichment is enabled; an error if it is enabled without a model.
fn enrich_configured(config: &Config) -> Result<bool, Failure> {
    let cfg = &config.enrich;
    if cfg.enabled && cfg.model.trim().is_empty() {
        return Err(Failure(format!(
            "enrich.model is not set in {}",
            config::config_path().display()
        )));
    }
    Ok(cfg.enabled)
}

/// What one enrichment run did.
struct EnrichPass {
    stats: EnrichStats,
    /// `(session key, reason)` for each session that failed.
    failures: Vec<(String, String)>,
}

/// Enriches the sessions that need it and `select` accepts, holding
/// `enrich.lock` throughout; `None` if another hound is enriching. Without
/// configured categories, the categories offered are `workspace_labels()`.
/// `on_attempt` hears how each attempted session ended.
fn enrich_pass(
    config: &Config,
    index: &Index,
    workspace_labels: impl FnOnce() -> Vec<String>,
    select: &mut dyn FnMut(&SessionKey) -> bool,
    on_attempt: &mut dyn FnMut(&SessionKey, Attempt<'_>),
) -> anyhow::Result<Option<EnrichPass>> {
    let cfg = &config.enrich;
    // Held until this function returns; the OS releases it if we crash.
    let Some(_enrich_lock) = index.try_lock_enrich() else {
        return Ok(None);
    };
    let categories = if cfg.categories.is_empty() {
        workspace_labels()
    } else {
        cfg.categories.clone()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut failures: Vec<(String, String)> = Vec::new();
    let stats = enrich::run_selected(
        index,
        &HttpClient::new(cfg),
        &categories,
        cfg.quiet_secs,
        now,
        select,
        &mut |key, attempt| {
            if let Attempt::Failed(why) = attempt {
                failures.push((key.as_string(), why.to_string()));
            }
            on_attempt(key, attempt);
        },
    )?;
    Ok(Some(EnrichPass { stats, failures }))
}

/// Most this many failures are listed individually.
const SHOWN_FAILURES: usize = 5;

/// `<session key>: <reason>` for the first [`SHOWN_FAILURES`] failures, then
/// `…and N more` if there were others.
fn failure_lines(failures: &[(String, String)]) -> Vec<String> {
    let mut lines: Vec<String> = failures
        .iter()
        .take(SHOWN_FAILURES)
        .map(|(key, why)| format!("{}: {}", printable(key), printable(why)))
        .collect();
    if failures.len() > SHOWN_FAILURES {
        lines.push(format!(
            "\u{2026}and {} more",
            failures.len() - SHOWN_FAILURES
        ));
    }
    lines
}

/// The host's distinct non-empty workspace labels in first-seen order; none
/// without a host or if it cannot be listed.
fn host_workspace_labels() -> Vec<String> {
    let panes = CliHerdr::detect()
        .and_then(|h| h.snapshot().ok())
        .unwrap_or_default();
    let mut labels: Vec<String> = Vec::new();
    for pane in panes {
        let label = pane.workspace_label.trim();
        if !label.is_empty() && !labels.iter().any(|l| l == label) {
            labels.push(label.to_string());
        }
    }
    labels
}

fn run_debug(arg: &str) -> Outcome {
    let index = open_index()?;
    let session = find_session(index.sessions()?, arg)?;
    print_session(&session);
    for m in index.messages(&session.key)? {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let sub = if m.subagent { " sub" } else { "" };
        println!("{role}{sub}: {}", printable(&m.text));
    }
    Ok(())
}

/// The session whose id or `<harness>:<id>` equals `arg`, or else the only one
/// either starts with. An empty `arg` matches nothing.
fn find_session(sessions: Vec<SessionDoc>, arg: &str) -> Result<SessionDoc, Failure> {
    if arg.is_empty() {
        return Err(Failure(format!("no session matching {arg:?}")));
    }
    let exact = |s: &SessionDoc| s.key.id == arg || s.key.as_string() == arg;
    let prefix = |s: &SessionDoc| s.key.id.starts_with(arg) || s.key.as_string().starts_with(arg);

    let mut found: Vec<SessionDoc> = sessions.iter().filter(|s| exact(s)).cloned().collect();
    if found.len() != 1 {
        found = sessions.into_iter().filter(|s| prefix(s)).collect();
    }
    match found.len() {
        0 => Err(Failure(format!("no session matching {arg}"))),
        1 => Ok(found.remove(0)),
        n => {
            let mut keys: Vec<String> = found.iter().map(|s| s.key.as_string()).collect();
            keys.sort();
            Err(Failure(format!(
                "{arg} matches {n} sessions: {}",
                keys.join(", ")
            )))
        }
    }
}

fn print_session(s: &SessionDoc) {
    let p = printable;
    println!("key:         {}", p(&s.key.as_string()));
    println!("harness:     {}", s.key.harness.as_str());
    println!("id:          {}", p(&s.key.id));
    println!("title:       {}", p(&s.title));
    println!("cwds:        {}", p(&s.cwds.join(", ")));
    println!("resume_cwd:  {}", p(&s.resume_cwd));
    println!("started_at:  {}", s.started_at);
    println!("updated_at:  {}", s.updated_at);
    println!("summary:     {}", p(s.summary.as_deref().unwrap_or("-")));
    println!("category:    {}", p(s.category.as_deref().unwrap_or("-")));
    println!("tags:        {}", p(&s.tags.join(", ")));
    println!("enriched_at: {} bytes", s.enriched_at_bytes);
    println!("files:");
    for f in &s.files {
        println!(
            "  {}{} size {} offset {} head {:016x}",
            p(&f.path.to_string_lossy()),
            if f.subagent { " (subagent)" } else { "" },
            f.size,
            f.offset,
            f.head_hash
        );
    }
    println!("messages:");
}

/// herdr popup bindings: `(key, scope, description)`.
const BINDINGS: [(&str, &str, &str); 3] = [
    ("prefix+space", "all", "hound: search all sessions"),
    ("prefix+a", "active", "hound: search live agents"),
    ("prefix+shift+o", "old", "hound: search closed sessions"),
];

fn run_install(serve: bool) -> Outcome {
    let exe = env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Failure(format!("locating the hound executable: {e}")))?;
    if serve {
        print!("{}", serve::unit_text(&exe, &config::config_path()));
    } else {
        print!("{}", install_text(&exe, plugin_dir().as_deref()));
    }
    Ok(())
}

/// The checkout this binary was built from, if it still holds `herdr-plugin.toml`.
fn plugin_dir() -> Option<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .ok()?;
    repo.join("herdr-plugin.toml").is_file().then_some(repo)
}

/// The text `hound install` prints. `plugin_dir` is the directory holding
/// `herdr-plugin.toml`, when known.
fn install_text(exe: &Path, plugin_dir: Option<&Path>) -> String {
    let exe = shell_quote(&exe.to_string_lossy());
    let mut out = String::new();
    out.push_str("# hound key bindings for herdr. Add these to ~/.config/herdr/config.toml,\n");
    out.push_str("# then run `herdr server reload-config` (or restart herdr).\n");
    out.push_str("# `hound install` only prints this; it changes nothing.\n");
    for (key, scope, description) in BINDINGS {
        out.push_str("\n[[keys.command]]\n");
        out.push_str(&format!("key = {}\n", toml_string(key)));
        out.push_str("type = \"popup\"\n");
        out.push_str(&format!(
            "command = {}\n",
            toml_string(&format!("{exe} ui --scope {scope}"))
        ));
        out.push_str(&format!("description = {}\n", toml_string(description)));
        out.push_str("width = \"80%\"\n");
        out.push_str("height = \"80%\"\n");
    }
    out.push_str("\n# Optional, instead of the bindings above: register hound as a herdr plugin\n");
    out.push_str("# (see README.md; needs `hound` on the herdr server's PATH):\n");
    match plugin_dir {
        Some(dir) => out.push_str(&format!(
            "#   herdr plugin link {}\n",
            shell_quote(&dir.to_string_lossy())
        )),
        None => out.push_str(
            "#   herdr plugin link <the hound checkout, the directory containing herdr-plugin.toml>\n",
        ),
    }
    out
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `s` quoted for a POSIX shell (herdr runs popup commands through one on
/// Unix) unless it contains only safe characters.
fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+:,@%".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_lines_are_capped_at_five() {
        let failures: Vec<(String, String)> = (1..=7)
            .map(|i| (format!("claude:s{i}"), format!("error {i}")))
            .collect();
        let lines = failure_lines(&failures);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[0], "claude:s1: error 1");
        assert_eq!(lines[4], "claude:s5: error 5");
        assert_eq!(lines[5], "\u{2026}and 2 more");

        let few = failure_lines(&failures[..5]);
        assert_eq!(few.len(), 5);
        assert!(failure_lines(&[]).is_empty());
    }

    #[test]
    fn ui_errors_are_held_only_in_a_terminal() {
        assert!(hold_error_on_screen(true));
        assert!(!hold_error_on_screen(false));
    }

    #[test]
    fn failure_lines_are_printable() {
        let lines = failure_lines(&[("pi:\u{1b}[2J".to_string(), "bad\nthing".to_string())]);
        assert_eq!(lines, vec!["pi:\u{FFFD}[2J: bad thing"]);
    }

    #[test]
    fn install_without_a_plugin_dir_names_the_manifest() {
        let text = install_text(Path::new("/opt/hound"), None);
        assert!(text.contains("herdr plugin link <"), "{text}");
        assert!(text.contains("herdr-plugin.toml"), "{text}");
        assert_eq!(text.matches("[[keys.command]]").count(), 3);
    }

    #[test]
    fn install_with_a_plugin_dir_links_it() {
        let text = install_text(Path::new("/opt/hound"), Some(Path::new("/src/my hound")));
        assert!(
            text.contains("herdr plugin link '/src/my hound'\n"),
            "{text}"
        );
    }
}
