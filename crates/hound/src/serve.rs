//! `hound serve`: keeps the index up to date and offers it to other machines
//! over HTTP. Off unless the config sets `serve.enabled = true`. Unix only:
//! `main.rs` compiles this module on Unix and a refusing stub elsewhere.

mod schedule;

use std::env;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hound::ui;
use hound_core::config::{self, Config};
use hound_core::enrich::Attempt;
use hound_core::index::Index;
use hound_core::remote::handler::ServeContext;
use hound_core::remote::server::{Server, check_bind};
use hound_core::remote::token::load_or_create_token;

use crate::{Failure, Outcome, enrich_configured, enrich_pass, open_index, printable};
use schedule::{Cooldown, EnrichSchedule};

/// Runs `hound serve`, or with `show_token` prints the token and exits.
pub(crate) fn run(show_token: bool) -> Outcome {
    let config_path = config::config_path();
    let config = Config::load(&config_path)?;
    let serve = &config.serve;
    if !serve.enabled {
        return Err(Failure(format!(
            "set serve.enabled = true in {}",
            config_path.display()
        )));
    }
    let token_path = config::serve_token_path();
    if show_token {
        println!("{}", token(&token_path)?);
        return Ok(());
    }
    let Some(bind) = serve.bind.as_deref() else {
        return Err(Failure(format!(
            "set serve.bind in {}",
            config_path.display()
        )));
    };
    let addr: SocketAddr = bind.trim().parse().map_err(|_| {
        Failure(format!(
            "serve.bind {bind:?} in {} is not an address like 127.0.0.1:7717",
            config_path.display()
        ))
    })?;
    check_bind(&addr, serve.allow_any_interface)?;
    let enrich = enrich_configured(&config)?;
    let token = token(&token_path)?;
    let ctx = ServeContext {
        index: open_index()?,
        token,
        host: hostname(),
        home: env::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
        version: hound::VERSION.to_string(),
    };
    // The upkeep thread writes through its own handle; the server's reader
    // picks up each commit.
    let upkeep_index = open_index()?;
    let server = Server::bind(addr, ctx)?;
    eprintln!("hound serve listening on {}", server.local_addr());
    let enrich = enrich && has_categories(&config);
    thread::Builder::new()
        .name("hound-upkeep".to_string())
        .spawn(move || upkeep(&config, &upkeep_index, enrich))
        .map_err(|e| Failure(format!("starting the index upkeep thread: {e}")))?;
    server.run()?;
    Ok(())
}

/// Whether the configured categories let `hound serve` enrich. Outside a
/// host there are no workspace labels to offer instead, so without
/// `enrich.categories` it does not enrich, and says so once.
fn has_categories(config: &Config) -> bool {
    if config.enrich.categories.is_empty() {
        eprintln!(
            "hound serve: enrichment is off here; set enrich.categories in {} to enable it under hound serve",
            config::config_path().display()
        );
        return false;
    }
    true
}

/// The token in `path`, created if missing. An existing token file must be
/// a regular file (not a symbolic link) owned by this user that group and
/// other users cannot access.
fn token(path: &Path) -> Result<String, Failure> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let me = current_uid();
            let kind = meta.file_type();
            let found = TokenFile {
                symlink: kind.is_symlink(),
                regular: kind.is_file(),
                mode: meta.permissions().mode(),
                owner: meta.uid(),
            };
            if let Some(problem) = token_file_problem(&found, me) {
                return Err(Failure(problem.message(path)));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Failure(format!("reading {}: {e}", path.display()))),
    }
    Ok(load_or_create_token(path)?)
}

/// What [`token`] checks about an existing token file.
struct TokenFile {
    symlink: bool,
    regular: bool,
    mode: u32,
    owner: u32,
}

#[derive(Debug, PartialEq)]
enum TokenFileProblem {
    Symlink,
    NotRegular,
    NotOwned,
    OpenToOthers,
}

impl TokenFileProblem {
    /// One line naming the file and what to do.
    fn message(&self, path: &Path) -> String {
        let p = path.display();
        match self {
            TokenFileProblem::Symlink => {
                format!("token file {p} is a symbolic link; replace it with a regular file")
            }
            TokenFileProblem::NotRegular => format!("token file {p} is not a regular file"),
            TokenFileProblem::NotOwned => {
                format!("token file {p} is not owned by the user running hound")
            }
            TokenFileProblem::OpenToOthers => {
                format!("token file {p} is open to other users; run chmod 600 {p}")
            }
        }
    }
}

/// Why `file` may not be used as the token file of user `me`, if it may not.
fn token_file_problem(file: &TokenFile, me: u32) -> Option<TokenFileProblem> {
    if file.symlink {
        Some(TokenFileProblem::Symlink)
    } else if !file.regular {
        Some(TokenFileProblem::NotRegular)
    } else if file.owner != me {
        Some(TokenFileProblem::NotOwned)
    } else if file.mode & 0o077 != 0 {
        Some(TokenFileProblem::OpenToOthers)
    } else {
        None
    }
}

/// This process's effective user id, the owner of the files it creates.
fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Refreshes the index now and every `serve.refresh_secs`. A refresh writes
/// the index's heartbeat when it completes; one skipped because another
/// process holds the writer lock writes nothing. When `enrich` is set, an
/// enrichment pass follows a refresh once the index has changed (see
/// [`EnrichSchedule`]), skipping sessions that failed recently (see
/// [`Cooldown`]). Never returns.
fn upkeep(config: &Config, index: &Index, enrich: bool) {
    let adapters = ui::adapters(config);
    let every = Duration::from_secs(config.serve.refresh_secs.max(1));
    let mut schedule = EnrichSchedule::new(config.enrich.quiet_secs);
    let mut cooldown = Cooldown::default();
    loop {
        let pass = panic::catch_unwind(AssertUnwindSafe(|| {
            match index.refresh(&adapters, &mut |_, _| {}) {
                Ok(Some(stats)) => {
                    if stats.added + stats.updated > 0 {
                        schedule.changed(unix_now());
                    }
                    if enrich && schedule.due(unix_now()) {
                        enrich_due_sessions(config, index, &mut schedule, &mut cooldown);
                    }
                }
                Ok(None) => {}
                Err(e) => log_error("index refresh failed", &e),
            }
        }));
        if pass.is_err() {
            eprintln!("hound serve: index upkeep panicked");
        }
        thread::sleep(every);
    }
}

/// One enrichment pass over the sessions not cooling down after a failure.
fn enrich_due_sessions(
    config: &Config,
    index: &Index,
    schedule: &mut EnrichSchedule,
    cooldown: &mut Cooldown,
) {
    let started = unix_now();
    let mut outcomes = Vec::new();
    // Categories are configured, so no workspace labels are asked for.
    let pass = enrich_pass(
        config,
        index,
        Vec::new,
        &mut |key| cooldown.may_try(key, started),
        &mut |key, attempt| outcomes.push((key.clone(), matches!(attempt, Attempt::Enriched))),
    );
    for (key, enriched) in outcomes {
        if enriched {
            cooldown.succeeded(&key);
        } else {
            cooldown.failed(&key, unix_now());
        }
    }
    match pass {
        // Another hound is enriching: try again after the next refresh.
        Ok(None) => {}
        Ok(Some(pass)) => {
            schedule.passed(started);
            if pass.stats.failed > 0 {
                eprintln!(
                    "hound serve: enrichment failed for {} sessions",
                    pass.stats.failed
                );
            }
        }
        Err(e) => {
            schedule.passed(started);
            log_error("enrichment failed", &e);
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn log_error(what: &str, e: &anyhow::Error) {
    eprintln!("hound serve: {what}: {}", printable(&format!("{e:#}")));
}

/// This machine's hostname, or `unknown`.
fn hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Prints what `hound install --serve` prints for the executable `exe`.
pub(crate) fn print_unit(exe: &Path) -> Outcome {
    print!("{}", unit_text(exe, &config::config_path()));
    Ok(())
}

/// The text `hound install --serve` prints: a systemd user unit running
/// `<exe> serve`, and how to install it.
fn unit_text(exe: &Path, config_path: &Path) -> String {
    let mut out = String::new();
    out.push_str("# hound serve as a systemd user service. Run\n");
    out.push_str("#   mkdir -p ~/.config/systemd/user\n");
    out.push_str("# save this as\n");
    out.push_str("#   ~/.config/systemd/user/hound-serve.service\n");
    out.push_str("# then run:\n");
    out.push_str("#   systemctl --user daemon-reload\n");
    out.push_str("#   systemctl --user enable --now hound-serve.service\n");
    out.push_str("# To keep it running while you are logged out: loginctl enable-linger\n");
    out.push_str(&format!(
        "# It needs serve.enabled = true and serve.bind in {}.\n",
        config_path.display()
    ));
    out.push_str("# `hound install --serve` only prints this; it changes nothing.\n");
    out.push_str("\n[Unit]\n");
    out.push_str("Description=hound session search service\n");
    // At boot the address to bind may not exist yet: keep retrying rather than
    // hit the start limit and stay failed.
    out.push_str("StartLimitIntervalSec=0\n");
    out.push_str("\n[Service]\n");
    out.push_str(&format!(
        "ExecStart={} serve\n",
        systemd_quote(&exe.to_string_lossy())
    ));
    out.push_str("Restart=on-failure\n");
    out.push_str("RestartSec=5\n");
    out.push_str("NoNewPrivileges=yes\n");
    out.push_str("UMask=0077\n");
    out.push_str("\n[Install]\n");
    out.push_str("WantedBy=default.target\n");
    out
}

/// `s` as one word of a systemd `ExecStart=` line: `%` and `$` escaped, and
/// double-quoted unless it contains only safe characters.
fn systemd_quote(s: &str) -> String {
    let escaped = s.replace('%', "%%").replace('$', "$$");
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+:,@%$".contains(c);
    if !escaped.is_empty() && escaped.chars().all(safe) {
        escaped
    } else {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_rules() {
        let ok = TokenFile {
            symlink: false,
            regular: true,
            mode: 0o100600,
            owner: 1000,
        };
        assert_eq!(token_file_problem(&ok, 1000), None);
        assert_eq!(
            token_file_problem(
                &TokenFile {
                    mode: 0o100400,
                    ..ok
                },
                1000
            ),
            None
        );
        assert_eq!(
            token_file_problem(&ok, 1001),
            Some(TokenFileProblem::NotOwned)
        );
        for mode in [0o100640, 0o100604, 0o100660] {
            assert_eq!(
                token_file_problem(&TokenFile { mode, ..ok }, 1000),
                Some(TokenFileProblem::OpenToOthers)
            );
        }
        assert_eq!(
            token_file_problem(
                &TokenFile {
                    symlink: true,
                    regular: false,
                    ..ok
                },
                1000
            ),
            Some(TokenFileProblem::Symlink)
        );
        assert_eq!(
            token_file_problem(
                &TokenFile {
                    regular: false,
                    mode: 0o040700,
                    ..ok
                },
                1000
            ),
            Some(TokenFileProblem::NotRegular)
        );
        let line = TokenFileProblem::OpenToOthers.message(Path::new("/c/serve-token"));
        assert!(line.contains("chmod 600 /c/serve-token"), "{line}");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn current_uid_owns_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "x").unwrap();
        assert_eq!(current_uid(), fs::metadata(&path).unwrap().uid());
    }

    #[test]
    fn unit_quotes_unusual_paths() {
        assert_eq!(systemd_quote("/usr/bin/hound"), "/usr/bin/hound");
        assert_eq!(systemd_quote("/opt/my hound"), "\"/opt/my hound\"");
        assert_eq!(systemd_quote("/opt/a%b$c"), "/opt/a%%b$$c");
        assert_eq!(systemd_quote("/opt/\"x\""), "\"/opt/\\\"x\\\"\"");
    }

    #[test]
    fn unit_runs_serve_and_restarts_on_failure() {
        let text = unit_text(Path::new("/opt/hound"), Path::new("/c/config.toml"));
        assert!(text.contains("\nExecStart=/opt/hound serve\n"), "{text}");
        assert!(text.contains("\nRestart=on-failure\n"), "{text}");
        assert!(text.contains("\nWantedBy=default.target\n"), "{text}");
        assert!(text.contains("/c/config.toml"), "{text}");
    }

    #[test]
    fn unit_keeps_retrying_and_is_locked_down() {
        let text = unit_text(Path::new("/opt/hound"), Path::new("/c/config.toml"));
        let section = |name: &str| {
            let start = text.find(&format!("\n[{name}]\n")).expect(name);
            let rest = &text[start + name.len() + 4..];
            rest[..rest.find("\n[").unwrap_or(rest.len())].to_string()
        };
        let unit = section("Unit");
        let service = section("Service");
        assert!(unit.contains("\nStartLimitIntervalSec=0\n"), "{text}");
        for line in ["RestartSec=5", "NoNewPrivileges=yes", "UMask=0077"] {
            assert!(service.contains(&format!("\n{line}\n")), "{line}: {text}");
        }
        // The directory is created before the unit is saved into it.
        let mkdir = text.find("mkdir -p ~/.config/systemd/user").expect("mkdir");
        assert!(mkdir < text.find("daemon-reload").unwrap(), "{text}");
    }
}
