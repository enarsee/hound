//! Runs the built `hound` binary against fixture roots. Every run points
//! `HOUND_STATE_DIR` and `HOUND_CONFIG` into a tempdir and drops the herdr
//! environment, so nothing reaches the real home or a live herdr session.

#[path = "../../hound-core/tests/common/mod.rs"]
mod common;

use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::process::{Command, Output};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use common::*;

const S1: &str = "11111111-1111-1111-1111-111111111111";
/// Shares the prefix `1111` with `S1`.
const P1: &str = "11112222-0000-7000-8000-000000000001";

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    /// Config pointing at empty fixture roots.
    fn empty() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        let pi = dir.path().join("pi");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&pi).unwrap();
        fs::write(
            dir.path().join("config.toml"),
            format!(
                "claude_root = {:?}\npi_root = {:?}\n",
                claude.to_str().unwrap(),
                pi.to_str().unwrap()
            ),
        )
        .unwrap();
        Env { dir }
    }

    /// One Claude session (`S1`) and one pi session (`P1`).
    fn with_sessions() -> Env {
        let env = Env::empty();
        write_lines(
            &env.claude()
                .join("-home-u-proj")
                .join(format!("{S1}.jsonl")),
            &[
                claude_user(S1, "/home/u/proj", T0, "fix the depot alert"),
                claude_text(S1, T1, "Sent to the Signal group"),
                claude_title(S1, "Depot alerts"),
            ],
        );
        write_lines(
            &env.pi()
                .join("--home-u-trip--")
                .join(format!("2026-11-14T13-07-09-755Z_{P1}.jsonl")),
            &[
                pi_header(P1, "/home/u/qa", T0),
                pi_user(T0, "plan the qa pass"),
                pi_assistant(T1, "Here is a plan"),
            ],
        );
        env
    }

    fn claude(&self) -> PathBuf {
        self.dir.path().join("claude")
    }

    fn pi(&self) -> PathBuf {
        self.dir.path().join("pi")
    }

    fn hound(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hound"))
            .args(args)
            .env("HOUND_STATE_DIR", self.dir.path().join("state"))
            .env("HOUND_CONFIG", self.dir.path().join("config.toml"))
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_BIN_PATH")
            .output()
            .unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every path under `dir`, sorted, with its size.
#[cfg(unix)]
fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let meta = e.metadata().unwrap();
            if meta.is_dir() {
                stack.push(e.path());
            }
            out.push((e.path(), meta.len()));
        }
    }
    out.sort();
    out
}

#[test]
fn index_then_reindex_reports_counts() {
    let env = Env::with_sessions();
    let first = env.hound(&["index"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stdout(&first).contains("added 2"), "{}", stdout(&first));

    let second = env.hound(&["index"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("unchanged 2"),
        "{}",
        stdout(&second)
    );
}

#[test]
fn debug_prints_session() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());

    let out = env.hound(&["debug", S1]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Depot alerts"), "{text}");
    assert!(text.contains("claude"), "{text}");
    assert!(text.contains("user: fix the depot alert"), "{text}");
    assert!(
        text.contains("assistant: Sent to the Signal group"),
        "{text}"
    );
}

#[test]
fn debug_prefix_and_errors() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());

    let unique = env.hound(&["debug", &S1[..8]]);
    assert!(unique.status.success(), "{}", stderr(&unique));
    assert!(stdout(&unique).contains("Depot alerts"));

    let unknown = env.hound(&["debug", "deadbeef"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("no session matching"),
        "{}",
        stderr(&unknown)
    );

    let ambiguous = env.hound(&["debug", "1111"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    let err = stderr(&ambiguous);
    assert!(err.contains(S1) && err.contains(P1), "{err}");
}

#[cfg(unix)]
#[test]
fn install_prints_three_bindings_and_changes_nothing() {
    let env = Env::with_sessions();
    let before = listing(env.dir.path());

    let out = env.hound(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for needle in [
        "--scope all",
        "--scope active",
        "--scope old",
        "prefix+space",
        "prefix+a",
        "key = \"prefix+shift+o\"",
        "herdr plugin link",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in {text}");
    }
    // herdr's default open_notification_target.
    assert!(!text.contains("\"prefix+o\""), "{text}");
    assert_eq!(text.matches("[[keys.command]]").count(), 3, "{text}");
    assert_eq!(text.matches("type = \"popup\"").count(), 3, "{text}");
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_hound")).unwrap();
    assert!(text.contains(exe.to_str().unwrap()), "{text}");

    assert_eq!(listing(env.dir.path()), before);
}

#[test]
fn bad_scope_is_a_usage_error() {
    let env = Env::empty();
    let out = env.hound(&["ui", "--scope", "nope"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn config_roots_are_respected() {
    let env = Env::empty();
    let out = env.hound(&["index"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("added 0"), "{}", stdout(&out));
}

/// Appends `[enrich]` settings to the env's config. The base URL has a scheme
/// the client rejects before connecting, so no test opens a socket.
fn enable_enrich(env: &Env, extra: &str) {
    let config = env.dir.path().join("config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "\n[enrich]\nenabled = true\nbase_url = \"bad://127.0.0.1:9/v1\"\n{extra}"
    ));
    fs::write(&config, text).unwrap();
}

#[test]
fn disabled_makes_no_call() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    let out = env.hound(&["enrich"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "enrichment disabled\n");
    assert_eq!(stderr(&out), "");
}

#[test]
fn enabled_without_a_model_is_a_config_error() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    enable_enrich(&env, "model = \"\"\n");
    let out = env.hound(&["enrich"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert_eq!(err.trim_end().lines().count(), 1, "{err}");
    assert!(err.contains("enrich.model"), "{err}");
}

/// Holds `enrich.lock` in the state directory, as a running `hound enrich` would.
fn hold_enrich_lock(env: &Env) -> fs::File {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.dir.path().join("state").join("enrich.lock"))
        .unwrap();
    file.try_lock().unwrap();
    file
}

#[test]
fn enrich_while_another_hound_enriches() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    // Both sessions qualify; a model call would fail on the bad scheme and be counted.
    enable_enrich(&env, "model = \"m\"\nquiet_secs = 0\n");
    let _lock = hold_enrich_lock(&env);
    let out = env.hound(&["enrich"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "another hound is enriching\n");
    assert_eq!(stderr(&out), "");
}

#[test]
fn enrich_failures_go_to_stderr_one_per_line() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    // An unsupported scheme fails inside the client before any socket is opened.
    enable_enrich(&env, "model = \"m\"\nquiet_secs = 0\n");
    let out = env.hound(&["enrich"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "enriched 0, failed 2, skipped 0\n");
    let err = stderr(&out);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 2, "{err}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&format!("claude:{S1}: "))),
        "{err}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with(&format!("pi:{P1}: "))),
        "{err}"
    );
    assert!(!err.contains("depot") && !err.contains("qa pass"), "{err}");
}

#[test]
fn enrich_with_nothing_quiet_reports_counts() {
    // The fixtures' timestamps are in the past, so make the quiet period huge:
    // nothing qualifies and no request is made.
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    enable_enrich(&env, "model = \"m\"\nquiet_secs = 9000000000\n");
    let out = env.hound(&["enrich"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "enriched 0, failed 0, skipped 2\n");
}

#[test]
fn unreadable_index_is_a_one_line_error() {
    let env = Env::empty();
    // A file where the state directory should be: the index cannot open.
    fs::write(env.dir.path().join("state"), "not a directory").unwrap();
    for args in [&["index"][..], &["debug", "x"], &["ui"]] {
        let out = env.hound(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert_eq!(err.trim_end().lines().count(), 1, "{args:?}: {err}");
        assert!(!err.contains("panicked"), "{err}");
    }
}

/// Holds `writer.lock` in the state directory, as another hound would.
fn hold_writer_lock(env: &Env) -> fs::File {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(env.dir.path().join("state").join("writer.lock"))
        .unwrap();
    file.try_lock().unwrap();
    file
}

#[test]
fn index_while_another_hound_indexes() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    let _lock = hold_writer_lock(&env);
    let out = env.hound(&["index"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("another hound is indexing"));
}

#[test]
fn rebuild_blocked_by_another_hound_is_a_one_line_error() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    // An unreadable state file forces a rebuild, which needs the lock.
    fs::write(env.dir.path().join("state").join("state.json"), "{").unwrap();
    let _lock = hold_writer_lock(&env);
    for args in [&["index"][..], &["debug", S1]] {
        let out = env.hound(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert_eq!(err.trim_end().lines().count(), 1, "{args:?}: {err}");
        assert!(err.contains("writer.lock"), "{err}");
    }
}

#[test]
fn debug_empty_argument_matches_nothing() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    let out = env.hound(&["debug", ""]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("no session matching"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn debug_exact_id_wins_over_longer_id() {
    let env = Env::empty();
    let short = "aaaa1111";
    let long = "aaaa1111-2";
    for (id, title) in [(short, "Short one"), (long, "Long one")] {
        write_lines(
            &env.claude()
                .join("-home-u-proj")
                .join(format!("{id}.jsonl")),
            &[
                claude_user(id, "/home/u/proj", T0, "hi"),
                claude_title(id, title),
            ],
        );
    }
    assert!(env.hound(&["index"]).status.success());
    let out = env.hound(&["debug", short]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Short one"), "{}", stdout(&out));
    assert!(!stdout(&out).contains("Long one"), "{}", stdout(&out));
}

#[cfg(unix)]
#[test]
fn install_links_the_checkout_holding_the_manifest() {
    let env = Env::empty();
    let out = env.hound(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let repo = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    assert!(repo.join("herdr-plugin.toml").is_file());
    let line = format!("herdr plugin link {}\n", repo.display());
    assert!(stdout(&out).contains(&line), "{}", stdout(&out));
}

#[test]
fn index_warns_about_an_unreadable_root_and_keeps_its_sessions() {
    let env = Env::with_sessions();
    assert!(env.hound(&["index"]).status.success());
    fs::rename(env.claude(), env.dir.path().join("moved")).unwrap();

    let out = env.hound(&["index"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("warning"), "{err}");
    assert!(err.contains(env.claude().to_str().unwrap()), "{err}");
    assert!(stdout(&out).contains("removed 0"), "{}", stdout(&out));
    assert!(env.hound(&["debug", S1]).status.success());
}

#[test]
fn debug_output_carries_no_terminal_controls() {
    let env = Env::empty();
    let osc = "\u{1b}]0;x\u{7}";
    let id = "aaaa2222";
    write_lines(
        &env.claude()
            .join("-home-u-proj")
            .join(format!("{id}.jsonl")),
        &[
            claude_user(
                id,
                &format!("/home/u/{osc}proj"),
                T0,
                &format!("evil {osc}prompt\nsecond line\u{9b}2J"),
            ),
            claude_text(id, T1, &format!("reply\u{202e}{osc}")),
            claude_title(id, &format!("Title {osc}here")),
        ],
    );
    assert!(env.hound(&["index"]).status.success());

    for args in [&["debug", id][..], &["debug", &format!("nope{osc}")]] {
        let out = env.hound(args);
        for (name, bytes) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
            assert!(
                !bytes.iter().any(|&b| b == 0x1b || b == 0x07),
                "{args:?} {name}: {}",
                String::from_utf8_lossy(bytes)
            );
            let text = String::from_utf8_lossy(bytes);
            assert!(
                !text.contains('\u{9b}') && !text.contains('\u{202e}'),
                "{text}"
            );
        }
    }
    let text = stdout(&env.hound(&["debug", id]));
    assert!(text.contains("evil "), "{text}");
    // One message stays one line.
    let user_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("user: ")).collect();
    assert_eq!(user_lines.len(), 1, "{text}");
    assert!(user_lines[0].contains("second line"), "{text}");
}

#[cfg(unix)]
impl Env {
    /// Appends `extra` to the env's config.
    fn append_config(&self, extra: &str) {
        let config = self.dir.path().join("config.toml");
        let mut text = fs::read_to_string(&config).unwrap();
        text.push('\n');
        text.push_str(extra);
        fs::write(&config, text).unwrap();
    }

    /// Where `hound serve` keeps its token: next to the config.
    fn token_path(&self) -> PathBuf {
        self.dir.path().join("serve-token")
    }

    /// Starts `hound serve` with stderr piped; killed when the guard drops.
    fn spawn_serve(&self) -> KillOnDrop {
        let child = Command::new(env!("CARGO_BIN_EXE_hound"))
            .arg("serve")
            .env("HOUND_STATE_DIR", self.dir.path().join("state"))
            .env("HOUND_CONFIG", self.dir.path().join("config.toml"))
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_BIN_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        KillOnDrop(child)
    }
}

/// Kills and reaps the child when dropped, including when a test fails.
#[cfg(unix)]
struct KillOnDrop(Child);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
const SERVE_ON: &str = "[serve]\nenabled = true\nbind = \"127.0.0.1:0\"\n";

#[cfg(unix)]
#[test]
fn serve_is_off_by_default() {
    for extra in ["", "[serve]\nbind = \"127.0.0.1:0\"\n"] {
        let env = Env::empty();
        env.append_config(extra);
        for args in [&["serve"][..], &["serve", "--show-token"][..]] {
            let out = env.hound(args);
            assert_eq!(out.status.code(), Some(1), "{args:?} {}", stderr(&out));
            assert!(stderr(&out).contains("serve.enabled"), "{}", stderr(&out));
            assert_eq!(stderr(&out).lines().count(), 1, "{}", stderr(&out));
            assert!(stdout(&out).is_empty(), "{}", stdout(&out));
            assert!(!env.token_path().exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn serve_without_bind_is_an_error() {
    let env = Env::empty();
    env.append_config("[serve]\nenabled = true\n");
    let out = env.hound(&["serve"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("serve.bind"), "{}", stderr(&out));
}

#[test]
fn install_without_serve_flag_does_not_mention_remotes() {
    let env = Env::empty();
    let out = env.hound(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out).to_lowercase();
    // "serve" as a word: the herdr bindings legitimately name `herdr server`.
    let mentions_serve = text
        .match_indices("serve")
        .any(|(i, _)| !text[i + "serve".len()..].starts_with(|c: char| c.is_alphanumeric()));
    assert!(!mentions_serve, "{text}");
    assert!(!text.contains("remote"), "{text}");
}

#[cfg(unix)]
#[test]
fn serve_refuses_unspecified_address() {
    let env = Env::empty();
    env.append_config("[serve]\nenabled = true\nbind = \"0.0.0.0:0\"\n");
    let out = env.hound(&["serve"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("allow_any_interface"),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn serve_show_token_prints_and_persists() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::empty();
    env.append_config(SERVE_ON);
    let first = env.hound(&["serve", "--show-token"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let token = stdout(&first).trim().to_string();
    assert_eq!(token.len(), 64, "{token}");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");

    let second = env.hound(&["serve", "--show-token"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&second).trim(), token);

    let meta = fs::metadata(env.token_path()).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn serve_refuses_a_token_file_others_can_read() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::empty();
    env.append_config(SERVE_ON);
    assert!(env.hound(&["serve", "--show-token"]).status.success());
    let path = env.token_path();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    for args in [&["serve"][..], &["serve", "--show-token"][..]] {
        let out = env.hound(args);
        assert_eq!(out.status.code(), Some(1), "{args:?} {}", stderr(&out));
        let err = stderr(&out);
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(
            err.contains(&format!("chmod 600 {}", path.display())),
            "{err}"
        );
        assert!(stdout(&out).is_empty(), "token printed: {}", stdout(&out));
    }
}

#[cfg(unix)]
#[test]
fn serve_answers_then_dies_with_the_test() {
    let env = Env::with_sessions();
    env.append_config(SERVE_ON);
    let shown = env.hound(&["serve", "--show-token"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let token = stdout(&shown).trim().to_string();

    let mut serve = env.spawn_serve();
    let stderr_pipe = serve.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut lines = BufReader::new(stderr_pipe).lines();
        let _ = tx.send(lines.next().and_then(|l| l.ok()).unwrap_or_default());
        // Keep draining so the server never blocks on a full pipe.
        for _ in lines {}
    });
    let line = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let addr = line
        .split("listening on ")
        .nth(1)
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .trim()
        .to_string();
    assert!(!line.contains(&token), "{line}");

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    // The upkeep thread indexes in the background: wait for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let sessions = loop {
        let mut resp = agent
            .get(&format!("http://{addr}/v1/info"))
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let info: serde_json::Value =
            serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
        assert_eq!(info["protocol"], 1);
        let n = info["sessions"].as_u64().unwrap();
        if n == 2 || Instant::now() > deadline {
            break n;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(sessions, 2);
    drop(serve);
}

#[cfg(unix)]
#[test]
fn install_serve_prints_a_unit_and_changes_nothing() {
    let env = Env::with_sessions();
    let before = listing(env.dir.path());
    let out = env.hound(&["install", "--serve"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for needle in [
        "[Service]",
        "ExecStart=",
        " serve",
        "systemctl --user",
        "Restart=on-failure",
        "WantedBy=default.target",
        "hound-serve.service",
        "\nStartLimitIntervalSec=0\n",
        "\nRestartSec=5\n",
        "\nNoNewPrivileges=yes\n",
        "\nUMask=0077\n",
        "mkdir -p ~/.config/systemd/user",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in {text}");
    }
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_hound")).unwrap();
    assert!(
        text.contains(&format!("ExecStart={} serve\n", exe.display())),
        "{text}"
    );
    assert_eq!(listing(env.dir.path()), before);
}

#[test]
fn popup_skips_refresh_when_heartbeat_is_fresh() {
    use hound::ui::skip_start_refresh;
    assert!(skip_start_refresh(Some(10)));
    assert!(skip_start_refresh(Some(0)));
    assert!(!skip_start_refresh(Some(60)));
    assert!(!skip_start_refresh(Some(-5)));
    assert!(!skip_start_refresh(None));
}

#[cfg(unix)]
#[test]
fn serve_refuses_a_token_file_that_is_a_link_or_not_a_file() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let env = Env::empty();
    env.append_config(SERVE_ON);
    let real = env.dir.path().join("elsewhere");
    fs::write(&real, format!("{}\n", "ab".repeat(32))).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    let path = env.token_path();
    symlink(&real, &path).unwrap();
    for args in [&["serve"][..], &["serve", "--show-token"][..]] {
        let out = env.hound(args);
        assert_eq!(out.status.code(), Some(1), "{args:?} {}", stderr(&out));
        let err = stderr(&out);
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("symbolic link"), "{err}");
        assert!(stdout(&out).is_empty(), "token printed: {}", stdout(&out));
    }

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let out = env.hound(&["serve", "--show-token"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a regular file"),
        "{}",
        stderr(&out)
    );
}

/// Starts `hound serve` on a fixture with two quiet sessions and enrichment
/// against a URL the client refuses before connecting. Waits until both
/// sessions are served, then `settle` longer, and returns everything the
/// service wrote to stderr.
#[cfg(unix)]
fn serve_stderr_with_enrichment(enrich_extra: &str, settle: Duration) -> String {
    let env = Env::with_sessions();
    enable_enrich(
        &env,
        &format!("model = \"m\"\nquiet_secs = 0\n{enrich_extra}"),
    );
    env.append_config(SERVE_ON);
    let shown = env.hound(&["serve", "--show-token"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let token = stdout(&shown).trim().to_string();

    let mut serve = env.spawn_serve();
    let stderr_pipe = serve.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr_pipe).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let addr = first
        .split("listening on ")
        .nth(1)
        .unwrap_or_else(|| panic!("no address in {first:?}"))
        .trim()
        .to_string();
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut resp = agent
            .get(&format!("http://{addr}/v1/info"))
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .unwrap();
        let info: serde_json::Value =
            serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
        if info["sessions"] == 2 || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(settle);
    drop(serve);
    let mut lines = vec![first];
    lines.extend(rx.try_iter());
    lines.join("\n")
}

#[cfg(unix)]
#[test]
fn serve_with_enrichment_and_categories_enriches() {
    // The control for the test below: the same setup with categories does run
    // enrichment, which fails on the refused URL and says so.
    let err =
        serve_stderr_with_enrichment("categories = [\"Work\"]\n", Duration::from_millis(1500));
    assert!(err.contains("enrichment failed for 2 sessions"), "{err}");
    assert!(!err.contains("enrich.categories"), "{err}");
}

#[cfg(unix)]
#[test]
fn serve_with_enrichment_and_no_categories_does_not_enrich() {
    let err = serve_stderr_with_enrichment("", Duration::from_millis(1500));
    let notes: Vec<&str> = err
        .lines()
        .filter(|l| l.contains("enrich.categories"))
        .collect();
    assert_eq!(notes.len(), 1, "{err}");
    assert!(!err.contains("enrichment failed"), "{err}");
}

#[cfg(windows)]
#[test]
fn serve_is_refused_on_windows() {
    let env = Env::empty();
    for args in [
        &["serve"][..],
        &["serve", "--show-token"][..],
        &["install", "--serve"][..],
    ] {
        let out = env.hound(args);
        assert_eq!(out.status.code(), Some(1), "{args:?} {}", stderr(&out));
        assert!(
            stderr(&out).contains("not supported on Windows"),
            "{args:?} {}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "{args:?} {}", stdout(&out));
    }
}

#[cfg(windows)]
#[test]
fn install_prints_cmd_bindings_on_windows() {
    let env = Env::empty();
    let out = env.hound(&["install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(r"%APPDATA%\herdr\config.toml"), "{text}");
    assert_eq!(text.matches(r#"command = "\"\""#).count(), 3, "{text}");
    // No verbatim `\\?\` prefix, raw or TOML-escaped.
    assert!(!text.contains(r"\\?\"), "{text}");
}
