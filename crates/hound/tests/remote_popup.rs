//! The popup's remote machines: loading one, searching it, the facts used to
//! resume there, and which host and facts Enter uses for an entry.
//!
//! Every server here is started by the test itself on `127.0.0.1:0`. No test
//! in this binary starts a process (a child forked while another thread holds
//! an index's `writer.lock` would keep the lock until it execs), and no test
//! runs herdr: the machine host is always a [`FakeHost`].

#[path = "../../hound-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use hound::hound_core::adapters::Adapter;
use hound::hound_core::adapters::claude::ClaudeAdapter;
use hound::hound_core::config::{Config, RemoteConfig};
use hound::hound_core::host::{Host, LivePane};
use hound::hound_core::index::{Index, SessionDoc};
use hound::hound_core::model::{Harness, SessionKey};
use hound::hound_core::open::{OpenOutcome, ResumeFacts, open_entry};
use hound::hound_core::remote::client::RemoteClient;
use hound::hound_core::remote::handler::{Request, ServeContext, handle};
use hound::hound_core::search::{Entry, Hit, Scope, Tier};
use hound::ui::SharedHost;
use hound::ui::remote::{
    Debounce, MachineFacts, Reach, RemoteFacts, RemoteLoad, RemoteMsg, RemoteSearch, RemoteState,
    Routes, SearchRequest, TimedHost, check_remotes, client_for, load_remote, run_worker,
    start_remotes, start_remotes_with, total_sessions,
};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const TOKEN: &str = "t0k";
const HOMELAB: &str = "homelab";
/// How far the test server's clock is ahead of this one.
const SKEW: i64 = 1000;
/// S2's working directory, inside the server's home `/home/u`.
const S2_CWD: &str = "/home/u/qa";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// A loopback HTTP server that records every request line.

type Responder = dyn Fn(&str, &str, &str, Option<&str>, &[u8]) -> (u16, String) + Send + Sync;

struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    /// Set to close the listener: later connections are refused.
    stop: Arc<AtomicBool>,
}

impl TestServer {
    /// Answers each request (one per connection) with `respond(method, path,
    /// query, authorization, body)`.
    fn start(respond: Arc<Responder>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        listener.set_nonblocking(true).unwrap();
        thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let respond = Arc::clone(&respond);
                let seen = Arc::clone(&seen);
                thread::spawn(move || serve_one(stream, &*respond, &seen));
            }
            // The listener is dropped here: the port refuses connections.
        });
        TestServer {
            addr,
            requests,
            stop,
        }
    }

    /// Closes the listener and waits until connections are refused.
    fn shut_down(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for _ in 0..200 {
            if TcpStream::connect(self.addr).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("server still accepting");
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `"<METHOD> <path>"` of every request so far.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve_one(mut stream: TcpStream, respond: &Responder, seen: &Mutex<Vec<String>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("").to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut authorization = None;
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => authorization = Some(value.trim().to_string()),
                "content-length" => length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    seen.lock().unwrap().push(format!("{method} {path}"));
    let (status, text) = respond(&method, path, query, authorization.as_deref(), &body);
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(reply.as_bytes());
}

/// A hound service with two sessions: S1 ("Depot alerts", "Signal" in its
/// conversation, cwd an existing directory) and S2 ("QA pass", cwd
/// [`S2_CWD`]), home `/home/u`, clock [`SKEW`] seconds ahead.
struct Fixture {
    _dir: tempfile::TempDir,
    s1_cwd: String,
    server: TestServer,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let cwd = dir.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let s1_cwd = cwd.to_str().unwrap().to_string();
        write_lines(
            &root.join("-home-u-proj").join(format!("{S1}.jsonl")),
            &[
                claude_user(S1, &s1_cwd, T0, "Sent to the Signal group"),
                claude_title(S1, "Depot alerts"),
            ],
        );
        write_lines(
            &root.join("-home-u-qa").join(format!("{S2}.jsonl")),
            &[
                claude_user(S2, S2_CWD, T0, "plan the qa pass via Riverbend"),
                claude_title(S2, "QA pass"),
            ],
        );
        let index = Index::open(&dir.path().join("state")).unwrap();
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(ClaudeAdapter::new(root, true))];
        index.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
        let ctx = Arc::new(ServeContext {
            index,
            token: TOKEN.to_string(),
            host: "box".to_string(),
            home: "/home/u".to_string(),
            version: "0.1.0".to_string(),
        });
        let server = TestServer::start(Arc::new(move |method, path, query, auth, body| {
            let req = Request {
                method,
                path,
                query,
                authorization: auth,
                body,
            };
            let resp = handle(&ctx, &req, now() + SKEW);
            (resp.status, resp.body)
        }));
        Fixture {
            _dir: dir,
            s1_cwd,
            server,
        }
    }

    fn client(&self) -> RemoteClient {
        self.client_with(TOKEN)
    }

    fn client_with(&self, token: &str) -> RemoteClient {
        RemoteClient::new(
            &self.server.url(),
            token.to_string(),
            Duration::from_secs(10),
        )
    }
}

/// A client for a loopback port nothing listens on.
fn dead_client() -> RemoteClient {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    RemoteClient::new(
        &format!("http://127.0.0.1:{port}"),
        TOKEN.to_string(),
        Duration::from_secs(2),
    )
}

// ---------------------------------------------------------------------------
// A machine host that records its calls.

struct FakeHost {
    panes: Result<Vec<LivePane>, String>,
    calls: Mutex<Vec<String>>,
    /// `snapshot` sleeps this long first, like a hung herdr.
    stall: Duration,
}

impl FakeHost {
    fn new(panes: Vec<LivePane>) -> FakeHost {
        FakeHost {
            panes: Ok(panes),
            calls: Mutex::new(Vec::new()),
            stall: Duration::ZERO,
        }
    }

    fn failing(message: &str) -> FakeHost {
        FakeHost {
            panes: Err(message.to_string()),
            ..FakeHost::new(Vec::new())
        }
    }

    fn stalling(stall: Duration) -> FakeHost {
        FakeHost {
            stall,
            ..FakeHost::new(Vec::new())
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl Host for FakeHost {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        self.record("snapshot".to_string());
        thread::sleep(self.stall);
        self.panes.clone().map_err(|e| anyhow::anyhow!(e))
    }

    fn read_scrollback(&self, pane_id: &str, _lines: u32) -> anyhow::Result<String> {
        self.record(format!("read_scrollback {pane_id}"));
        anyhow::bail!("no scrollback")
    }

    fn focus(&self, pane: &LivePane) -> anyhow::Result<()> {
        self.record(format!("focus {}", pane.pane_id));
        Ok(())
    }

    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane> {
        self.record(format!("create_tab {workspace_id} {cwd}"));
        let mut p = pane("pnew", workspace_id, "new", None);
        p.cwd = cwd.to_string();
        Ok(p)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        self.record(format!(
            "start_agent {name} {kind} {pane_id} {}",
            args.join(" ")
        ));
        Ok(())
    }
}

/// A pane in workspace `workspace_id`, labelled `label`; running Claude
/// session `session` if given, else a plain shell.
fn pane(pane_id: &str, workspace_id: &str, label: &str, session: Option<&str>) -> LivePane {
    LivePane {
        pane_id: pane_id.to_string(),
        tab_id: "t1".to_string(),
        workspace_id: workspace_id.to_string(),
        workspace_label: label.to_string(),
        tab_label: "main".to_string(),
        agent_name: session.map(|_| "claude-111111".to_string()),
        kind: session.map(|_| "claude".to_string()),
        session_id: session.map(str::to_string),
        title: String::new(),
        cwd: "/srv/other".to_string(),
        status: String::new(),
        focused: false,
        scrollback: None,
    }
}

fn adapters() -> Vec<Box<dyn Adapter>> {
    vec![Box::new(ClaudeAdapter::new(
        PathBuf::from("/nonexistent/claude"),
        true,
    ))]
}

fn ready(load: RemoteLoad) -> RemoteState {
    match load {
        RemoteLoad::Ready(state) => state,
        RemoteLoad::Partial(_, note) => panic!("partial: {note}"),
        RemoteLoad::Failed(note) => panic!("failed: {note}"),
    }
}

fn partial(load: RemoteLoad) -> (RemoteState, String) {
    match load {
        RemoteLoad::Partial(state, note) => (state, note),
        RemoteLoad::Ready(_) => panic!("ready"),
        RemoteLoad::Failed(note) => panic!("failed: {note}"),
    }
}

fn session_id(entry: &Entry) -> Option<&str> {
    entry.session.as_ref().map(|s| s.key.id.as_str())
}

/// The hits' entries.
fn hit_entries<'a>(entries: &'a [Entry], hits: &[Hit]) -> Vec<&'a Entry> {
    hits.iter().map(|h| &entries[h.entry]).collect()
}

// ---------------------------------------------------------------------------
// Loading and searching a remote.

#[test]
fn load_ready_builds_live_and_closed_entries() {
    let f = Fixture::new();
    let client = f.client();
    let host = FakeHost::new(vec![pane("p1", "w1", "Billing", Some(S1))]);
    let state = ready(load_remote(HOMELAB, &client, Some(&host), now()));
    assert_eq!(state.sessions.len(), 2);

    let (entries, hits) = state.search(&client, "", Scope::All, true, now());
    let found = hit_entries(&entries, &hits);
    assert_eq!(found.len(), 2, "{found:?}");
    let s1 = found.iter().find(|e| session_id(e) == Some(S1)).unwrap();
    assert!(s1.is_live());
    assert_eq!(s1.machine.as_deref(), Some(HOMELAB));
    let s2 = found.iter().find(|e| session_id(e) == Some(S2)).unwrap();
    assert!(!s2.is_live());
    assert_eq!(s2.machine.as_deref(), Some(HOMELAB));
    assert_eq!(host.calls(), vec!["snapshot"]);
}

#[test]
fn remote_times_are_shifted_onto_the_local_clock() {
    let f = Fixture::new();
    let client = f.client();
    let host = FakeHost::new(Vec::new());
    let local_now = now();
    let state = ready(load_remote(HOMELAB, &client, Some(&host), local_now));
    let offset = state.clock_offset;
    assert!((SKEW - 2..=SKEW + 2).contains(&offset), "{offset}");
    let (entries, _) = state.search(&client, "", Scope::All, true, local_now);
    let s1 = entries.iter().find(|e| session_id(e) == Some(S1)).unwrap();
    assert_eq!(s1.last_active(), T0_SECS - offset);
}

#[test]
fn mixed_query_matches_label_and_content() {
    let f = Fixture::new();
    let client = f.client();
    let host = FakeHost::new(vec![pane("p1", "w1", "Billing", Some(S1))]);
    let state = ready(load_remote(HOMELAB, &client, Some(&host), now()));
    let (entries, hits) = state.search(&client, "billing signal", Scope::All, true, now());
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(session_id(&entries[hits[0].entry]), Some(S1));
    assert_eq!(hits[0].tier, Tier::Content);
}

#[test]
fn remote_home_prefix_is_stripped() {
    let f = Fixture::new();
    let client = f.client();
    let host = FakeHost::new(Vec::new());
    let mut state = ready(load_remote(HOMELAB, &client, Some(&host), now()));
    assert_eq!(state.home.as_deref(), Some("/home/u"));

    let (entries, hits) = state.search(&client, "home", Scope::All, true, now());
    assert!(
        !hit_entries(&entries, &hits)
            .iter()
            .any(|e| session_id(e) == Some(S2)),
        "{hits:?}"
    );
    // The cwd still matches below the home directory.
    let (entries, hits) = state.search(&client, "qa", Scope::All, true, now());
    assert!(
        hit_entries(&entries, &hits)
            .iter()
            .any(|e| session_id(e) == Some(S2))
    );
    // Control: without the remote's home, its cwd matches "home".
    state.home = None;
    let (entries, hits) = state.search(&client, "home", Scope::All, true, now());
    assert!(
        hit_entries(&entries, &hits)
            .iter()
            .any(|e| session_id(e) == Some(S2))
    );
}

#[test]
fn partial_remote_states() {
    let f = Fixture::new();

    // Snapshot fails, HTTP works: sessions, all closed.
    let client = f.client();
    let host = FakeHost::failing("herdr: no such machine");
    let (state, note) = partial(load_remote(HOMELAB, &client, Some(&host), now()));
    assert_eq!(note, "herdr machine not reachable");
    assert!(state.http_ok);
    let (entries, hits) = state.search(&client, "", Scope::All, true, now());
    assert_eq!(hits.len(), 2);
    assert!(entries.iter().all(|e| !e.is_live()));
    assert!(
        entries
            .iter()
            .all(|e| e.machine.as_deref() == Some(HOMELAB))
    );

    // Snapshot works, wrong token: live panes only, matched by name.
    let wrong = f.client_with("nope");
    let host = FakeHost::new(vec![pane("p1", "w1", "Billing", Some(S1))]);
    let (state, note) = partial(load_remote(HOMELAB, &wrong, Some(&host), now()));
    assert_eq!(note, "unauthorised");
    assert!(!state.http_ok);
    assert!(state.sessions.is_empty());
    let (entries, hits) = state.search(&wrong, "billing", Scope::All, true, now());
    assert_eq!(hits.len(), 1, "{hits:?}");
    let e = &entries[hits[0].entry];
    assert!(e.session.is_none());
    assert_eq!(e.live.as_ref().unwrap().pane_id, "p1");
    assert_eq!(hits[0].tier, Tier::Short);

    // Both fail.
    let host = FakeHost::failing("herdr: no such machine");
    match load_remote(HOMELAB, &dead_client(), Some(&host), now()) {
        RemoteLoad::Failed(note) => assert_eq!(note, "unreachable"),
        _ => panic!("expected Failed"),
    }

    // No host: nothing is asked.
    let quiet = Fixture::new();
    match load_remote(HOMELAB, &quiet.client(), None, now()) {
        RemoteLoad::Failed(note) => assert_eq!(note, "needs herdr"),
        _ => panic!("expected Failed"),
    }
    thread::sleep(Duration::from_millis(100));
    assert!(quiet.server.requests().is_empty());
}

#[test]
fn protocol_mismatch_disables_the_remote() {
    let server = TestServer::start(Arc::new(|_, _, _, _, _| {
        (
            200,
            r#"{"protocol":2,"now":1,"hound_version":"9","host":"x","home":"/","sessions":5}"#
                .to_string(),
        )
    }));
    let client = RemoteClient::new(&server.url(), TOKEN.to_string(), Duration::from_secs(5));
    let host = FakeHost::new(vec![pane("p1", "w1", "Billing", Some(S1))]);
    let load = load_remote(HOMELAB, &client, Some(&host), now());
    let state = match load {
        RemoteLoad::Partial(state, note) => {
            assert_eq!(note, "needs a newer hound");
            state
        }
        RemoteLoad::Failed(note) => {
            assert_eq!(note, "needs a newer hound");
            return;
        }
        RemoteLoad::Ready(_) => panic!("a protocol 2 remote was used"),
    };
    assert!(state.sessions.is_empty());
    assert!(!state.http_ok);
    assert_eq!(Reach::of(&RemoteLoad::Partial(state, "x".into())), {
        Reach::NoFacts("x".to_string())
    });
    assert!(
        !server.requests().iter().any(|r| r == "GET /v1/sessions"),
        "{:?}",
        server.requests()
    );
}

#[test]
fn remote_facts_come_from_the_server() {
    let f = Fixture::new();
    let client = f.client();
    let host = FakeHost::new(Vec::new());
    let state = ready(load_remote(HOMELAB, &client, Some(&host), now()));
    let s1 = state.sessions.iter().find(|s| s.key.id == S1).unwrap();
    let facts = RemoteFacts { client: &client };
    let local = now();
    let got = facts.facts(s1, local).unwrap();
    assert!(got.cwd_exists);
    assert!(
        (local + SKEW - 2..=local + SKEW + 2).contains(&got.now),
        "{} vs {}",
        got.now,
        local + SKEW
    );
    // The later of the last record (T0) and the file's modification time.
    assert!(got.last_change >= T0_SECS, "{}", got.last_change);
    assert!(f.server.requests().contains(&"GET /v1/session".to_string()));

    let s2 = state.sessions.iter().find(|s| s.key.id == S2).unwrap();
    assert!(!facts.facts(s2, local).unwrap().cwd_exists);

    let mut unknown: SessionDoc = s1.clone();
    unknown.key = SessionKey {
        harness: Harness::Claude,
        id: "99999999-9999-9999-9999-999999999999".to_string(),
    };
    let err = facts.facts(&unknown, local).unwrap_err();
    assert!(format!("{err:#}").contains("unknown session"), "{err:#}");
}

#[test]
fn debounce_latest_wins() {
    let mut d = Debounce::new();
    let a = d.next();
    let b = d.next();
    let c = d.next();
    assert!(!d.is_current(a));
    assert!(!d.is_current(b));
    assert!(d.is_current(c));
    assert!(a < b && b < c);
}

#[test]
fn remote_enter_goes_through_machine_host_and_remote_facts() {
    let f = Fixture::new();
    let client = f.client();
    let state = ready(load_remote(
        HOMELAB,
        &client,
        Some(&FakeHost::new(Vec::new())),
        now(),
    ));
    let (entries, _) = state.search(&client, "", Scope::All, true, now());
    let s1 = entries.iter().find(|e| session_id(e) == Some(S1)).unwrap();
    assert!(!s1.is_live());

    let mut shell = pane("p9", "w1", "Billing", None);
    shell.focused = true;
    let host = FakeHost::new(vec![shell]);
    let outcome = open_entry(
        s1,
        Some(&host),
        &adapters(),
        &RemoteFacts { client: &client },
        false,
        now(),
    );
    assert_eq!(outcome, OpenOutcome::Resumed);
    let calls = host.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], "snapshot");
    assert_eq!(calls[1], format!("create_tab w1 {}", f.s1_cwd));
    assert!(
        calls[2].starts_with("start_agent claude-111111 claude pnew"),
        "{calls:?}"
    );
    assert!(f.server.requests().contains(&"GET /v1/session".to_string()));
}

// ---------------------------------------------------------------------------
// Popup setup.

fn remote(machine: &str, url: &str, token_file: &Path, enabled: bool) -> RemoteConfig {
    RemoteConfig {
        machine: machine.to_string(),
        url: url.to_string(),
        token_file: token_file.to_path_buf(),
        enabled,
        timeout_secs: None,
    }
}

fn config_with(remotes: Vec<RemoteConfig>) -> Config {
    Config {
        remotes,
        ..Config::default()
    }
}

fn token_file(dir: &Path) -> PathBuf {
    let path = dir.join("token");
    fs::write(&path, format!("{TOKEN}\n")).unwrap();
    path
}

/// A machine-host factory handing out fresh [`FakeHost`]s.
fn fake_machines(label: &str) -> anyhow::Result<SharedHost> {
    hound::herdr::validate_machine_label(label)?;
    Ok(Arc::new(FakeHost::new(Vec::new())))
}

#[test]
fn disabled_remote_is_never_contacted() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(vec![remote(
        HOMELAB,
        &f.server.url(),
        &token_file(dir.path()),
        false,
    )]);
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let (tx, _rx) = mpsc::channel();
    let setup = start_remotes(&config, Some((local, &fake_machines)), tx);
    assert!(setup.workers.is_empty());
    assert!(setup.notes.is_empty());
    assert!(!setup.configured);
    assert!(setup.routes.route(Some(HOMELAB)).is_err());
    thread::sleep(Duration::from_millis(300));
    assert!(f.server.requests().is_empty(), "{:?}", f.server.requests());
}

#[test]
fn enabled_remote_without_a_local_host_is_never_contacted() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(vec![remote(
        HOMELAB,
        &f.server.url(),
        &token_file(dir.path()),
        true,
    )]);
    let (tx, _rx) = mpsc::channel();
    let setup = start_remotes(&config, None, tx);
    assert!(setup.workers.is_empty());
    assert!(setup.configured);
    assert_eq!(
        setup.notes,
        vec![(HOMELAB.to_string(), "needs herdr".to_string())]
    );
    let err = setup.routes.route(Some(HOMELAB)).err().unwrap();
    assert_eq!(err, "homelab: needs herdr");
    thread::sleep(Duration::from_millis(300));
    assert!(f.server.requests().is_empty());
}

#[test]
fn enabled_remote_gets_a_worker_that_loads_and_searches() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(vec![remote(
        HOMELAB,
        &f.server.url(),
        &token_file(dir.path()),
        true,
    )]);
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let setup = start_remotes(&config, Some((local, &fake_machines)), tx);
    assert!(setup.configured);
    assert_eq!(setup.workers.len(), 1);
    let worker = &setup.workers[0];
    assert_eq!(worker.machine, HOMELAB);
    for (seq, query) in [(1, "depot"), (2, "qa")] {
        worker
            .requests
            .send(SearchRequest {
                seq,
                query: query.to_string(),
                scope: Scope::All,
                include_subagents: true,
            })
            .unwrap();
    }
    let mut loaded = None;
    let mut results = Vec::new();
    while results.is_empty() {
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            RemoteMsg::Loaded {
                machine,
                sessions,
                reach,
            } => loaded = Some((machine, sessions, reach)),
            RemoteMsg::Results {
                machine,
                seq,
                entries,
                hits,
                note,
            } => {
                assert_eq!(note, None);
                results.push((machine, seq, entries, hits))
            }
            RemoteMsg::Stopped { .. } => panic!("stopped"),
        }
    }
    assert_eq!(
        loaded,
        Some((HOMELAB.to_string(), 2, Reach::Ready)),
        "Loaded comes before any result"
    );
    // The older pending request was skipped.
    let (machine, seq, entries, hits) = results.remove(0);
    assert_eq!(machine, HOMELAB);
    assert_eq!(seq, 2);
    assert_eq!(hits.len(), 1);
    assert_eq!(session_id(&entries[hits[0].entry]), Some(S2));
}

#[test]
fn missing_token_file_is_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let missing = remote(
        HOMELAB,
        "http://127.0.0.1:1",
        &dir.path().join("absent"),
        true,
    );
    assert_eq!(client_for(&missing).err().unwrap(), "no token");
    let blank = dir.path().join("blank");
    fs::write(&blank, "\n").unwrap();
    let empty = remote(HOMELAB, "http://127.0.0.1:1", &blank, true);
    assert_eq!(client_for(&empty).err().unwrap(), "no token");
    let ok = remote(HOMELAB, "http://127.0.0.1:1", &token_file(dir.path()), true);
    assert!(client_for(&ok).is_ok());

    // In the popup it is a footer note, not a startup error.
    let config = config_with(vec![missing]);
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let (tx, _rx) = mpsc::channel();
    let setup = start_remotes(&config, Some((local, &fake_machines)), tx);
    assert!(setup.workers.is_empty());
    assert_eq!(
        setup.notes,
        vec![(HOMELAB.to_string(), "no token".to_string())]
    );
    assert_eq!(
        setup.routes.route(Some(HOMELAB)).err().unwrap(),
        "homelab: no token"
    );
}

#[test]
fn remote_labels_are_checked_before_use() {
    let t = PathBuf::from("/nonexistent/token");
    let remotes = vec![
        remote("homelab", "http://a", &t, true),
        remote("Local", "http://b", &t, true),
        remote("mac", "http://c", &t, true),
        remote("MAC", "http://d", &t, true),
        remote("-x", "http://e", &t, true),
        remote(" pad", "http://f", &t, true),
        remote("", "http://g", &t, true),
        remote("bell\u{7}", "http://h", &t, true),
        remote("office", "http://i", &t, true),
    ];
    let checked: Vec<(String, Result<(), &str>)> = check_remotes(remotes.iter())
        .into_iter()
        .map(|(r, c)| (r.machine.clone(), c))
        .collect();
    assert_eq!(
        checked,
        vec![
            ("homelab".to_string(), Ok(())),
            ("Local".to_string(), Err("invalid machine name")),
            ("mac".to_string(), Err("duplicate machine name")),
            ("MAC".to_string(), Err("duplicate machine name")),
            ("-x".to_string(), Err("invalid machine name")),
            (" pad".to_string(), Err("invalid machine name")),
            ("".to_string(), Err("invalid machine name")),
            ("bell\u{7}".to_string(), Err("invalid machine name")),
            ("office".to_string(), Ok(())),
        ]
    );
}

#[test]
fn rejected_remotes_get_a_note_and_no_worker() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let token = token_file(dir.path());
    let url = f.server.url();
    let config = config_with(vec![
        remote("local", &url, &token, true),
        remote("box", &url, &token, true),
        remote("box", &url, &token, true),
        // A disabled duplicate does not count.
        remote("homelab", &url, &token, false),
    ]);
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let (tx, _rx) = mpsc::channel();
    let setup = start_remotes(&config, Some((local, &fake_machines)), tx);
    assert!(setup.configured);
    assert!(setup.workers.is_empty());
    assert_eq!(
        setup.notes,
        vec![
            ("local".to_string(), "invalid machine name".to_string()),
            ("box".to_string(), "duplicate machine name".to_string()),
        ]
    );
    assert_eq!(
        setup.routes.route(Some("box")).err().unwrap(),
        "box: duplicate machine name"
    );
    thread::sleep(Duration::from_millis(300));
    assert!(f.server.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Enter routing: which host and facts an entry is opened with.

fn same_host(a: &Option<SharedHost>, b: &SharedHost) -> bool {
    a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
}

fn closed_doc() -> SessionDoc {
    SessionDoc {
        key: SessionKey {
            harness: Harness::Claude,
            id: S1.to_string(),
        },
        title: "t".to_string(),
        cwds: vec!["/nonexistent".to_string()],
        resume_cwd: "/nonexistent".to_string(),
        started_at: 0,
        updated_at: 0,
        summary: None,
        category: None,
        tags: Vec::new(),
        files: Vec::new(),
        enriched_at_bytes: 0,
    }
}

#[test]
fn local_entry_routes_to_the_local_host_and_local_facts() {
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let routes = Routes::new(Some(Arc::clone(&local)));
    let route = routes.route(None).unwrap();
    assert!(same_host(&route.host, &local));
    assert!(matches!(route.facts, MachineFacts::Local));

    // No local host: still local, with no host.
    let route = Routes::new(None).route(None).unwrap();
    assert!(route.host.is_none());
    assert!(matches!(route.facts, MachineFacts::Local));
}

#[test]
fn remote_entry_routes_to_its_machine_never_the_local_host() {
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let homelab: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let client = Arc::new(dead_client());
    let mut routes = Routes::new(Some(Arc::clone(&local)));
    routes.add(HOMELAB, Ok((Arc::clone(&homelab), Arc::clone(&client))));
    let route = routes.route(Some(HOMELAB)).unwrap();
    assert!(same_host(&route.host, &homelab));
    assert!(!same_host(&route.host, &local));
    match &route.facts {
        MachineFacts::Remote(c) => assert!(Arc::ptr_eq(c, &client)),
        _ => panic!("expected remote facts"),
    }

    // Unconfigured, rejected, and failed remotes: an error naming the machine.
    assert_eq!(
        routes.route(Some("mac")).err().unwrap(),
        "mac: not configured"
    );
    routes.add("box", Err("invalid machine name".to_string()));
    assert_eq!(
        routes.route(Some("box")).err().unwrap(),
        "box: invalid machine name"
    );
    routes.apply(HOMELAB, &Reach::Failed("unreachable".to_string()));
    assert_eq!(
        routes.route(Some(HOMELAB)).err().unwrap(),
        "homelab: unreachable"
    );
    // The local route is unaffected.
    assert!(same_host(&routes.route(None).unwrap().host, &local));
}

#[test]
fn two_remotes_each_route_to_their_own() {
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let a: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let b: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let ca = Arc::new(dead_client());
    let cb = Arc::new(dead_client());
    let mut routes = Routes::new(Some(Arc::clone(&local)));
    routes.add("homelab", Ok((Arc::clone(&a), Arc::clone(&ca))));
    routes.add("mac", Ok((Arc::clone(&b), Arc::clone(&cb))));

    let ra = routes.route(Some("homelab")).unwrap();
    let rb = routes.route(Some("mac")).unwrap();
    assert!(same_host(&ra.host, &a));
    assert!(same_host(&rb.host, &b));
    assert!(matches!(&ra.facts, MachineFacts::Remote(c) if Arc::ptr_eq(c, &ca)));
    assert!(matches!(&rb.facts, MachineFacts::Remote(c) if Arc::ptr_eq(c, &cb)));
}

#[test]
fn a_remote_without_http_focuses_live_panes_but_cannot_resume() {
    let homelab = Arc::new(FakeHost::new(Vec::new()));
    let shared: SharedHost = homelab.clone();
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let mut routes = Routes::new(Some(Arc::clone(&local)));
    routes.add(HOMELAB, Ok((Arc::clone(&shared), Arc::new(dead_client()))));
    routes.apply(HOMELAB, &Reach::NoFacts("unauthorised".to_string()));
    let route = routes.route(Some(HOMELAB)).unwrap();
    assert!(same_host(&route.host, &shared));
    assert!(matches!(route.facts, MachineFacts::Unavailable(_)));
    let err = route.facts.facts(&closed_doc(), now()).unwrap_err();
    assert_eq!(format!("{err:#}"), "unauthorised");

    // A live pane is focused through the remote host.
    let live = hound::hound_core::search::build_entries_on(
        HOMELAB,
        Vec::new(),
        vec![pane("p1", "w1", "Billing", Some(S1))],
        now(),
        now(),
    );
    let outcome = open_entry(
        &live[0],
        route.host.as_deref().map(|h| h as &dyn Host),
        &adapters(),
        &route.facts,
        false,
        now(),
    );
    assert_eq!(outcome, OpenOutcome::Focused);
    assert_eq!(homelab.calls(), vec!["focus p1"]);

    // A closed session there fails, naming the machine.
    let closed =
        hound::hound_core::search::build_entries_on(HOMELAB, vec![closed_doc()], Vec::new(), 0, 0);
    let outcome = open_entry(
        &closed[0],
        route.host.as_deref().map(|h| h as &dyn Host),
        &adapters(),
        &route.facts,
        false,
        now(),
    );
    assert_eq!(
        outcome,
        OpenOutcome::Failed("homelab: unauthorised".to_string())
    );

    // A snapshot failure keeps the host: Enter tries it.
    routes.apply("homelab", &Reach::NoHost);
    assert!(same_host(
        &routes.route(Some(HOMELAB)).unwrap().host,
        &shared
    ));
}

// ---------------------------------------------------------------------------
// Review fixes: panics, loading note, snapshot limit, failing searches,
// header total.

fn request(seq: u64, query: &str) -> SearchRequest {
    SearchRequest {
        seq,
        query: query.to_string(),
        scope: Scope::All,
        include_subagents: true,
    }
}

fn empty_state() -> RemoteState {
    RemoteState {
        machine: HOMELAB.to_string(),
        sessions: Vec::new(),
        facts: Default::default(),
        live: Vec::new(),
        clock_offset: 0,
        home: None,
        http_ok: true,
    }
}

/// Runs `run_worker` on this thread with `requests` already queued and the
/// sender dropped; returns what it reported.
fn worker_messages(
    load: impl FnOnce() -> RemoteLoad,
    search: impl FnMut(&RemoteState, &SearchRequest) -> RemoteSearch,
    requests: Vec<SearchRequest>,
) -> Vec<RemoteMsg> {
    let (req_tx, req_rx) = mpsc::channel();
    for r in requests {
        req_tx.send(r).unwrap();
    }
    drop(req_tx);
    let (tx, rx) = mpsc::channel();
    run_worker(HOMELAB, load, search, &req_rx, &tx);
    drop(tx);
    rx.iter().collect()
}

#[test]
fn a_panic_while_loading_reports_the_remote_failed() {
    let msgs = worker_messages(
        || panic!("bad remote data"),
        |_, _| unreachable!(),
        vec![request(1, "x")],
    );
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        RemoteMsg::Loaded {
            machine,
            sessions,
            reach,
        } => {
            assert_eq!(machine, HOMELAB);
            assert_eq!(*sessions, 0);
            assert_eq!(*reach, Reach::Failed("internal error".to_string()));
        }
        _ => panic!("expected Loaded"),
    }
}

#[test]
fn a_panic_while_searching_stops_the_remote() {
    let mut searches = 0;
    let msgs = worker_messages(
        || RemoteLoad::Ready(empty_state()),
        |_, _| {
            searches += 1;
            panic!("bad snippet")
        },
        vec![request(1, "x")],
    );
    assert_eq!(searches, 1);
    assert_eq!(msgs.len(), 2);
    assert!(matches!(
        &msgs[0],
        RemoteMsg::Loaded {
            reach: Reach::Ready,
            ..
        }
    ));
    match &msgs[1] {
        RemoteMsg::Stopped { machine, note } => {
            assert_eq!(machine, HOMELAB);
            assert_eq!(note, "internal error");
        }
        _ => panic!("expected Stopped"),
    }

    // In the popup, that makes the remote's route fail.
    let mut routes = Routes::new(None);
    let host: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    routes.add(HOMELAB, Ok((host, Arc::new(dead_client()))));
    routes.apply(HOMELAB, &Reach::Failed("internal error".to_string()));
    assert_eq!(
        routes.route(Some(HOMELAB)).err().unwrap(),
        "homelab: internal error"
    );
}

#[test]
fn results_carry_the_search_error_else_the_load_note() {
    // Requests are sent one at a time, so neither is skipped.
    let (req_tx, req_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut n = 0;
        run_worker(
            HOMELAB,
            || RemoteLoad::Partial(empty_state(), "herdr machine not reachable".to_string()),
            move |_, _| {
                n += 1;
                RemoteSearch {
                    entries: Vec::new(),
                    hits: Vec::new(),
                    error: (n == 1).then(|| "unreachable".to_string()),
                }
            },
            &req_rx,
            &tx,
        )
    });
    let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        next(),
        RemoteMsg::Loaded {
            reach: Reach::NoHost,
            ..
        }
    ));
    req_tx.send(request(1, "a")).unwrap();
    match next() {
        RemoteMsg::Results { note, .. } => assert_eq!(note.as_deref(), Some("unreachable")),
        _ => panic!("expected Results"),
    }
    req_tx.send(request(2, "b")).unwrap();
    match next() {
        RemoteMsg::Results { note, .. } => {
            assert_eq!(note.as_deref(), Some("herdr machine not reachable"))
        }
        _ => panic!("expected Results"),
    }
    drop(req_tx);
    worker.join().unwrap();
}

#[test]
fn a_remote_that_stops_answering_after_load_is_noted() {
    let f = Fixture::new();
    let client = f.client_with(TOKEN);
    let host = FakeHost::new(Vec::new());
    let state = ready(load_remote(HOMELAB, &client, Some(&host), now()));

    // While it answers, no error.
    let found = state.search_watched(&client, &request(1, "depot"), now());
    assert_eq!(found.error, None);

    f.server.shut_down();
    // Short-field matches still show; the failed text request is noted.
    let found = state.search_watched(&client, &request(2, "depot"), now());
    assert_eq!(found.error.as_deref(), Some("unreachable"));
    let ids: Vec<_> = hit_entries(&found.entries, &found.hits)
        .into_iter()
        .filter_map(session_id)
        .collect();
    assert_eq!(ids, vec![S1]);
    // A search needing no text request clears it.
    let found = state.search_watched(&client, &request(3, ""), now());
    assert_eq!(found.error, None);
    assert_eq!(found.hits.len(), 2);
}

#[test]
fn timed_host_gives_up_on_a_hung_snapshot() {
    let hung: SharedHost = Arc::new(FakeHost::stalling(Duration::from_secs(30)));
    let timed = TimedHost {
        inner: hung,
        limit: Duration::from_millis(200),
    };
    let started = std::time::Instant::now();
    let err = timed.snapshot().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(format!("{err:#}").contains("did not answer"), "{err:#}");

    let quick = TimedHost {
        inner: Arc::new(FakeHost::new(vec![pane("p1", "w1", "Billing", None)])),
        limit: Duration::from_secs(5),
    };
    assert_eq!(quick.snapshot().unwrap().len(), 1);
}

#[test]
fn a_hung_machine_shows_loading_then_loads_partial() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(vec![remote(
        HOMELAB,
        &f.server.url(),
        &token_file(dir.path()),
        true,
    )]);
    let hung = |label: &str| -> anyhow::Result<SharedHost> {
        hound::herdr::validate_machine_label(label)?;
        Ok(Arc::new(FakeHost::stalling(Duration::from_secs(30))))
    };
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let setup = start_remotes_with(
        &config,
        Some((local, &hung)),
        tx,
        Duration::from_millis(500),
    );
    assert_eq!(setup.workers.len(), 1);
    assert_eq!(
        setup.notes,
        vec![(HOMELAB.to_string(), "loading\u{2026}".to_string())]
    );
    // Nothing until the limit passes.
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        RemoteMsg::Loaded {
            sessions, reach, ..
        } => {
            assert_eq!(sessions, 2);
            assert_eq!(reach, Reach::NoHost);
            assert_eq!(reach.note(), Some("herdr machine not reachable"));
        }
        _ => panic!("expected Loaded"),
    }
}

#[test]
fn no_local_herdr_is_needs_herdr_and_a_refused_label_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(vec![remote(
        HOMELAB,
        "http://127.0.0.1:1",
        &token_file(dir.path()),
        true,
    )]);
    let (tx, _rx) = mpsc::channel();
    let setup = start_remotes(&config, None, tx.clone());
    assert_eq!(
        setup.notes,
        vec![(HOMELAB.to_string(), "needs herdr".to_string())]
    );

    let refuse = |_: &str| -> anyhow::Result<SharedHost> { anyhow::bail!("refused") };
    let local: SharedHost = Arc::new(FakeHost::new(Vec::new()));
    let setup = start_remotes(&config, Some((local, &refuse)), tx);
    assert!(setup.workers.is_empty());
    assert_eq!(
        setup.notes,
        vec![(HOMELAB.to_string(), "invalid machine name".to_string())]
    );
}

#[test]
fn header_total_counts_loaded_remotes_only_when_configured() {
    assert_eq!(total_sessions(5, [], false), 5);
    assert_eq!(total_sessions(5, [3, 4], false), 5);
    assert_eq!(total_sessions(5, [], true), 5);
    assert_eq!(total_sessions(5, [3, 4], true), 12);
}

#[test]
fn client_uses_the_remotes_timeout() {
    // A service that accepts and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&held);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            keep.lock().unwrap().push(stream);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = remote(
        HOMELAB,
        &format!("http://{addr}"),
        &token_file(dir.path()),
        true,
    );
    config.timeout_secs = Some(1);
    let client = client_for(&config).unwrap();
    let started = std::time::Instant::now();
    assert!(client.info().is_err());
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_millis(1900),
        "{took:?}"
    );
}
