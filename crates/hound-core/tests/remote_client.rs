//! No test in this binary may start a process. A child forked while another
//! thread holds an index's `writer.lock` shares the lock until it execs, so
//! a concurrent `refresh` here would see the lock as busy and return `None`.
//! Tests that need a child process live in `remote_client_proxy.rs`.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::index::{Index, TermMode, TextSearch};
use hound_core::model::{Harness, SessionKey};
use hound_core::remote::client::{MAX_SESSIONS_BYTES, RemoteClient, RemoteError};
use hound_core::remote::handler::ServeContext;
use hound_core::remote::server::{Server, ServerHandle};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const TOKEN: &str = "t0k-SECRET";
const TIMEOUT: Duration = Duration::from_secs(10);

fn write_sessions(root: &Path, cwd: &str) {
    write_lines(
        &root.join("-home-u-proj").join(format!("{S1}.jsonl")),
        &[
            claude_user(S1, cwd, T0, "Sent to the Signal group"),
            claude_title(S1, "Depot alerts"),
        ],
    );
    write_lines(
        &root.join("-home-u-proj").join(format!("{S2}.jsonl")),
        &[
            claude_user(S2, cwd, T0, "plan the qa pass via Riverbend"),
            claude_title(S2, "QA pass"),
        ],
    );
}

fn build_index(root: &Path, state: &Path) -> Index {
    let index = Index::open(state).unwrap();
    let adapters: Vec<Box<dyn Adapter>> =
        vec![Box::new(ClaudeAdapter::new(root.to_path_buf(), true))];
    index.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
    index
}

/// Two indexed sessions served on `127.0.0.1:0`, and a second index of the
/// same files to compare against.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    local: Index,
    handle: ServerHandle,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let cwd = dir.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        write_sessions(&root, cwd.to_str().unwrap());
        let served = build_index(&root, &dir.path().join("state"));
        let local = build_index(&root, &dir.path().join("state-local"));
        let ctx = ServeContext {
            index: served,
            token: TOKEN.to_string(),
            host: "box".to_string(),
            home: "/home/u".to_string(),
            version: "0.1.0".to_string(),
        };
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), ctx).unwrap();
        Fixture {
            _dir: dir,
            root,
            local,
            handle: server.spawn(),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.handle.local_addr())
    }

    fn client(&self) -> RemoteClient {
        RemoteClient::new(&self.url(), TOKEN.to_string(), TIMEOUT)
    }

    fn session_file(&self, id: &str) -> PathBuf {
        self.root.join("-home-u-proj").join(format!("{id}.jsonl"))
    }
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        id: id.to_string(),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Reads a request head (and gives up quickly on anything else).
fn read_head(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            _ => return,
        }
    }
}

/// A listener that answers each of up to `n` connections with `response`, then
/// closes them. Returns its address.
fn canned(response: Vec<u8>, n: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            read_head(&mut stream);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Write);
            // Let the client read before the socket is dropped.
            thread::sleep(Duration::from_millis(50));
        }
    });
    addr
}

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn client_for(addr: SocketAddr, timeout: Duration) -> RemoteClient {
    RemoteClient::new(&format!("http://{addr}"), TOKEN.to_string(), timeout)
}

#[test]
fn info_and_sessions_roundtrip() {
    let f = Fixture::new();
    let client = f.client();

    let (info, now) = client.info().unwrap();
    assert_eq!(info.sessions, 2);
    assert_eq!(info.host, "box");
    assert!((now - unix_now()).abs() <= 5, "{now}");

    let remote = client.sessions().unwrap();
    assert!((remote.now - unix_now()).abs() <= 5, "{}", remote.now);
    let titles: HashMap<SessionKey, String> = remote
        .sessions
        .iter()
        .map(|d| (d.key.clone(), d.title.clone()))
        .collect();
    let local: HashMap<SessionKey, String> = f
        .local
        .sessions()
        .unwrap()
        .into_iter()
        .map(|d| (d.key, d.title))
        .collect();
    assert_eq!(titles.len(), 2);
    assert!(titles.contains_key(&key(S1)) && titles.contains_key(&key(S2)));
    assert_eq!(titles, local);
    assert_eq!(remote.facts.len(), 2);
    assert!(remote.facts.contains_key(&key(S1)) && remote.facts.contains_key(&key(S2)));
}

#[test]
fn term_matches_equal_the_local_index() {
    let f = Fixture::new();
    let client = f.client();
    for (term, mode, subagents) in [
        ("signal", TermMode::Whole, true),
        ("river", TermMode::Prefix, true),
        ("nginx", TermMode::Whole, false),
    ] {
        let remote = client.term_matches(term, mode, subagents).unwrap();
        let local = f.local.term_matches(term, mode, subagents).unwrap();
        let flatten = |m: &HashMap<SessionKey, hound_core::index::TermHit>| {
            let mut v: Vec<(SessionKey, bool, String)> = m
                .iter()
                .map(|(k, h)| (k.clone(), h.from_subagent, h.snippet.clone()))
                .collect();
            v.sort_by(|a, b| a.0.id.cmp(&b.0.id));
            v
        };
        assert_eq!(flatten(&remote), flatten(&local), "{term}");
    }
    // The fixture really matches something.
    assert_eq!(
        client
            .term_matches("signal", TermMode::Whole, true)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        client
            .term_matches("river", TermMode::Prefix, true)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn session_facts_are_fresh() {
    let f = Fixture::new();
    let client = f.client();
    let (first, now) = client.session_facts(&key(S1)).unwrap();
    assert!((now - unix_now()).abs() <= 5);
    assert!(first.cwd_exists);
    let later = SystemTime::now() + Duration::from_secs(1000);
    fs::File::options()
        .append(true)
        .open(f.session_file(S1))
        .unwrap()
        .set_modified(later)
        .unwrap();
    let (second, _) = client.session_facts(&key(S1)).unwrap();
    assert!(
        second.last_change > first.last_change,
        "{first:?} -> {second:?}"
    );
}

#[test]
fn unknown_session_is_bad() {
    let f = Fixture::new();
    let err = f.client().session_facts(&key("nope & ?=")).unwrap_err();
    assert_eq!(err, RemoteError::Bad("unknown session".to_string()));
}

#[test]
fn valid_unknown_session_is_bad_from_the_server() {
    let f = Fixture::new();
    let err = f.client().session_facts(&key("nope")).unwrap_err();
    assert_eq!(err, RemoteError::Bad("unknown session".to_string()));
}

#[test]
fn wrong_token_is_unauthorised() {
    let f = Fixture::new();
    let client = RemoteClient::new(&f.url(), "wrong".to_string(), TIMEOUT);
    assert_eq!(client.info().unwrap_err(), RemoteError::Unauthorised);
    assert_eq!(client.sessions().unwrap_err(), RemoteError::Unauthorised);
    assert_eq!(
        client.session_facts(&key(S1)).unwrap_err(),
        RemoteError::Unauthorised
    );
    let err = client
        .term_matches("signal", TermMode::Whole, true)
        .unwrap_err();
    assert_eq!(err.to_string(), "unauthorised");
    assert_eq!(RemoteError::Unauthorised.note(), "unauthorised");
}

#[test]
fn refused_connection_is_unreachable() {
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let client = client_for(addr, Duration::from_secs(2));
    let err = client.info().unwrap_err();
    assert!(matches!(err, RemoteError::Unreachable(_)), "{err:?}");
    assert_eq!(err.note(), "unreachable");
    let RemoteError::Unreachable(reason) = err else {
        unreachable!()
    };
    assert!(!reason.contains(TOKEN), "{reason}");
}

#[test]
fn stalled_server_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let held: JoinHandle<()> = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_secs(3));
        drop(stream);
    });
    let client = client_for(addr, Duration::from_millis(300));
    let started = Instant::now();
    let err = client.info().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "{err:?}");
    assert!(matches!(err, RemoteError::Unreachable(_)), "{err:?}");
    drop(held);
}

#[test]
fn server_stalling_mid_body_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream);
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"protocol\":1,",
        );
        let _ = stream.flush();
        thread::sleep(Duration::from_secs(3));
    });
    let client = client_for(addr, Duration::from_millis(300));
    let started = Instant::now();
    let err = client.sessions().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "{err:?}");
    assert!(matches!(err, RemoteError::Unreachable(_)), "{err:?}");
}

#[test]
fn protocol_mismatch_is_reported() {
    let addr = canned(
        http("200 OK", r#"{"protocol":2,"now":0,"whatever":true}"#),
        1,
    );
    let err = client_for(addr, TIMEOUT).info().unwrap_err();
    assert_eq!(err, RemoteError::Protocol { theirs: 2 });
    assert_eq!(err.note(), "needs a newer hound");

    let addr = canned(http("200 OK", r#"{"protocol":0,"now":0}"#), 1);
    let err = client_for(addr, TIMEOUT).sessions().unwrap_err();
    assert_eq!(err, RemoteError::Protocol { theirs: 0 });
    assert_eq!(err.note(), "needs an update there");
}

#[test]
fn non_json_and_error_status_are_bad() {
    let responses = [
        http(
            "500 Internal Server Error",
            r#"{"protocol":1,"now":0,"error":"index error"}"#,
        ),
        http("200 OK", "hello"),
        http("200 OK", r#"{"protocol":1,"now":0}"#),
    ];
    for response in responses {
        let addr = canned(response, 1);
        let err = client_for(addr, TIMEOUT).info().unwrap_err();
        assert!(matches!(err, RemoteError::Bad(_)), "{err:?}");
        assert_eq!(err.note(), "bad response");
        assert!(!format!("{err:?}").contains(TOKEN), "{err:?}");
    }
}

#[test]
fn redirects_are_not_followed() {
    // Where a followed redirect would land: it must never see a request.
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let to = target.local_addr().unwrap();
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{to}/v1/info\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let addr = canned(redirect.into_bytes(), 1);
    let err = client_for(addr, TIMEOUT).info().unwrap_err();
    assert!(matches!(err, RemoteError::Bad(_)), "{err:?}");
    thread::sleep(Duration::from_millis(100));
    assert!(target.accept().is_err(), "the redirect was followed");
}

/// Streams a `/v1/sessions` body without end; returns how many bytes it
/// managed to write before the client went away.
fn endless(content_length: Option<u64>) -> (SocketAddr, JoinHandle<u64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream);
        let length = content_length
            .map(|n| format!("Content-Length: {n}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{length}Connection: close\r\n\r\n{{\"protocol\":1,\"now\":0,\"sessions\":["
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let chunk = vec![b' '; 64 * 1024];
        let mut written = 0u64;
        while written < 256 * 1024 * 1024 {
            match stream.write(&chunk) {
                Ok(n) => written += n as u64,
                Err(_) => break,
            }
        }
        written
    });
    (addr, writer)
}

#[test]
fn oversized_sessions_response_is_too_large() {
    let (addr, writer) = endless(None);
    let started = Instant::now();
    let err = client_for(addr, TIMEOUT).sessions().unwrap_err();
    assert_eq!(err, RemoteError::TooLarge);
    assert_eq!(err.note(), "too many sessions");
    assert!(started.elapsed() < Duration::from_secs(5));
    let written = writer.join().unwrap();
    // The client stopped reading soon after the cap.
    assert!(written < 4 * MAX_SESSIONS_BYTES, "{written}");
}

#[test]
fn declared_oversized_length_is_too_large() {
    let (addr, writer) = endless(Some(MAX_SESSIONS_BYTES + 1));
    let err = client_for(addr, TIMEOUT).sessions().unwrap_err();
    assert_eq!(err, RemoteError::TooLarge);
    let written = writer.join().unwrap();
    assert!(written < 4 * MAX_SESSIONS_BYTES, "{written}");
}

fn wire_session(harness: &str, id: &str) -> serde_json::Value {
    serde_json::json!({
        "harness": harness, "id": id, "title": "t", "cwds": ["/w"], "resume_cwd": "/w",
        "started_at": 1, "updated_at": 2, "summary": null, "category": null, "tags": [],
        "main_file": "/w/f.jsonl", "last_change": 3, "cwd_exists": true, "future_field": 1,
    })
}

#[test]
fn unknown_harness_sessions_are_skipped() {
    let body = serde_json::json!({
        "protocol": 1, "now": 42,
        "sessions": [wire_session("claude", S1), wire_session("codex", S2)],
    });
    let addr = canned(http("200 OK", &body.to_string()), 1);
    let remote = client_for(addr, TIMEOUT).sessions().unwrap();
    assert_eq!(remote.sessions.len(), 1);
    assert_eq!(remote.sessions[0].key, key(S1));
    assert_eq!(remote.facts.len(), 1);
    assert_eq!(remote.facts[&key(S1)].last_change, 3);
    assert_eq!(remote.now, 42);
}

#[test]
fn unknown_harness_matches_are_skipped() {
    let body = serde_json::json!({
        "protocol": 1, "now": 42,
        "terms": [{"term": "x", "matches": [
            {"harness": "claude", "id": S1, "score": 1.0, "snippet": "a x", "from_subagent": false},
            {"harness": "codex", "id": S2, "score": 2.0, "snippet": "b x", "from_subagent": true},
        ]}],
    });
    let addr = canned(http("200 OK", &body.to_string()), 1);
    let hits = client_for(addr, TIMEOUT)
        .term_matches("x", TermMode::Whole, true)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[&key(S1)].snippet, "a x");
}

#[test]
fn trailing_slash_in_url_is_accepted() {
    let f = Fixture::new();
    let client = RemoteClient::new(&format!("{}/", f.url()), TOKEN.to_string(), TIMEOUT);
    assert_eq!(client.info().unwrap().0.sessions, 2);
    assert_eq!(client.sessions().unwrap().sessions.len(), 2);
}

/// Ids a hostile or wrong server might send: flags, shell metacharacters, a
/// path, nothing, and one past the 128-character cap.
fn hostile_ids() -> Vec<String> {
    vec![
        "--dangerously-skip-permissions".to_string(),
        "a;b".to_string(),
        "$(x)".to_string(),
        "../x".to_string(),
        String::new(),
        "a".repeat(300),
        "a".repeat(129),
    ]
}

#[test]
fn sessions_with_invalid_ids_are_dropped() {
    let mut sessions = vec![wire_session("claude", S1)];
    sessions.extend(hostile_ids().iter().map(|id| wire_session("claude", id)));
    // The cap itself is allowed.
    let longest = "b".repeat(128);
    sessions.push(wire_session("claude", &longest));
    let body = serde_json::json!({"protocol": 1, "now": 42, "sessions": sessions});
    let addr = canned(http("200 OK", &body.to_string()), 1);
    let remote = client_for(addr, TIMEOUT).sessions().unwrap();
    let ids: Vec<&str> = remote.sessions.iter().map(|d| d.key.id.as_str()).collect();
    assert_eq!(ids, vec![S1, longest.as_str()]);
    assert_eq!(remote.facts.len(), 2);
}

#[test]
fn wire_session_into_doc_refuses_invalid_ids() {
    use hound_core::remote::wire::WireSession;
    for id in hostile_ids() {
        let wire: WireSession = serde_json::from_value(wire_session("claude", &id)).unwrap();
        assert!(wire.into_doc().is_none(), "{id:?}");
    }
    let wire: WireSession = serde_json::from_value(wire_session("claude", S1)).unwrap();
    assert!(wire.into_doc().is_some());
}

#[test]
fn matches_with_invalid_ids_are_dropped() {
    let mut matches = vec![serde_json::json!(
        {"harness": "claude", "id": S1, "score": 1.0, "snippet": "a x", "from_subagent": false}
    )];
    matches.extend(hostile_ids().iter().map(|id| {
        serde_json::json!(
            {"harness": "claude", "id": id, "score": 2.0, "snippet": "b x", "from_subagent": false}
        )
    }));
    let body = serde_json::json!({
        "protocol": 1, "now": 42, "terms": [{"term": "x", "matches": matches}],
    });
    let addr = canned(http("200 OK", &body.to_string()), 1);
    let hits = client_for(addr, TIMEOUT)
        .term_matches("x", TermMode::Whole, true)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits.contains_key(&key(S1)));
}

#[test]
fn session_facts_refuses_an_invalid_key_without_a_request() {
    // Nothing listens here: a request would come back Unreachable.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = client_for(addr, TIMEOUT);
    for id in hostile_ids() {
        let err = client.session_facts(&key(&id)).unwrap_err();
        assert_eq!(
            err,
            RemoteError::Bad("unknown session".to_string()),
            "{id:?}"
        );
    }
}
