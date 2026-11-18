mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::index::Index;
use hound_core::remote::PROTOCOL;
use hound_core::remote::handler::ServeContext;
use hound_core::remote::server::{Server, ServerHandle, check_bind};
use hound_core::remote::wire::{Envelope, Info, Sessions, TermsResponse};
use serde_json::json;

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const TOKEN: &str = "t0k";

/// Two indexed sessions served on `127.0.0.1:0`.
struct Fixture {
    _dir: tempfile::TempDir,
    handle: ServerHandle,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let cwd = dir.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_str().unwrap();
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
        let index = Index::open(&dir.path().join("state")).unwrap();
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(ClaudeAdapter::new(root, true))];
        index.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
        let ctx = ServeContext {
            index,
            token: TOKEN.to_string(),
            host: "box".to_string(),
            home: "/home/u".to_string(),
            version: "0.1.0".to_string(),
        };
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), ctx).unwrap();
        Fixture {
            _dir: dir,
            handle: server.spawn(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.handle.local_addr()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr())
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent()
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

/// Sends `raw` on a new connection and returns everything the server sends
/// back before closing.
fn raw_exchange(addr: SocketAddr, raw: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _ = stream.write_all(raw);
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn serves_info_over_http() {
    let f = Fixture::new();
    let mut resp = agent()
        .get(&f.url("/v1/info"))
        .header("Authorization", &bearer())
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp.headers().get("content-type").unwrap();
    assert_eq!(content_type, "application/json");
    let body = resp.body_mut().read_to_string().unwrap();
    let env: Envelope<Info> = serde_json::from_str(&body).unwrap();
    assert_eq!(env.protocol, PROTOCOL);
    assert_eq!(env.protocol, 1);
    assert_eq!(env.body.sessions, 2);
    assert_eq!(env.body.host, "box");
}

#[test]
fn unauthorised_over_http() {
    let f = Fixture::new();
    let mut resp = agent().get(&f.url("/v1/info")).call().unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.body_mut().read_to_string().unwrap(), "");
}

#[test]
fn terms_over_http() {
    let f = Fixture::new();
    let body = json!({
        "terms": [{"term": "signal", "mode": "whole"}],
        "include_subagents": false,
    });
    let mut resp = agent()
        .post(&f.url("/v1/terms"))
        .header("Authorization", &bearer())
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(resp.status(), 200);
    let env: Envelope<TermsResponse> =
        serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(env.body.terms.len(), 1);
    let matches = &env.body.terms[0].matches;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].id, S1);
    assert_eq!(matches[0].harness, "claude");
    assert!(matches[0].snippet.contains("Signal"), "{matches:?}");
}

#[test]
fn oversized_body_is_rejected_without_reading_it_all() {
    let f = Fixture::new();
    let body = vec![b'x'; 1024 * 1024];
    let started = Instant::now();
    let mut resp = agent()
        .post(&f.url("/v1/terms"))
        .header("Authorization", &bearer())
        .send(&body[..])
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(started.elapsed() < Duration::from_secs(2));
    let text = resp.body_mut().read_to_string().unwrap();
    assert!(!text.contains("xxxx"), "{text}");
}

#[test]
fn concurrent_requests() {
    let f = Fixture::new();
    let url = f.url("/v1/sessions");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let url = url.clone();
            thread::spawn(move || {
                let agent = agent();
                for _ in 0..5 {
                    let mut resp = agent
                        .get(&url)
                        .header("Authorization", &bearer())
                        .call()
                        .unwrap();
                    assert_eq!(resp.status(), 200);
                    let env: Envelope<Sessions> =
                        serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
                    assert_eq!(env.body.sessions.len(), 2);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn handle_drop_stops_the_server() {
    let f = Fixture::new();
    let addr = f.addr();
    let resp = agent()
        .get(&f.url("/v1/info"))
        .header("Authorization", &bearer())
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let started = Instant::now();
    drop(f);
    assert!(started.elapsed() < Duration::from_secs(5));
    let after = agent()
        .get(&format!("http://{addr}/v1/info"))
        .header("Authorization", &bearer())
        .call();
    assert!(after.is_err(), "server still answering after drop");
}

#[test]
fn check_bind_rules() {
    for addr in ["127.0.0.1:1", "100.64.0.10:7717", "[::1]:1"] {
        let addr: SocketAddr = addr.parse().unwrap();
        assert!(check_bind(&addr, false).is_ok(), "{addr}");
    }
    // Pure checks: nothing here binds these addresses.
    for addr in [
        "0.0.0.0:7717",
        "[::]:7717",
        "[::ffff:0.0.0.0]:7717",
        "[::ffff:0:0]:7717",
    ] {
        let addr: SocketAddr = addr.parse().unwrap();
        let err = check_bind(&addr, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("allow_any_interface"),
            "{err:#}"
        );
        assert!(check_bind(&addr, true).is_ok(), "{addr}");
    }
}

#[test]
fn heartbeat_age_reports_and_is_none_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(&dir.path().join("state")).unwrap();
    assert_eq!(index.heartbeat_age(1_800_000_000), None);

    let empty: PathBuf = dir.path().join("none");
    let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(ClaudeAdapter::new(empty, true))];
    let secs = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    let before = secs();
    index.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
    let after = secs();
    let age = index.heartbeat_age(after + 100).unwrap();
    assert!((100..=100 + after - before).contains(&age), "{age}");
}

#[test]
fn malformed_requests_get_a_bare_error_that_echoes_nothing() {
    let f = Fixture::new();
    for raw in [
        b"NOT A REQUEST MARKER123\r\n\r\n".to_vec(),
        b"GET /v1/info HTTP/1.1\r\nBad Header MARKER123\r\n\r\n".to_vec(),
        format!(
            "GET /v1/info?q={} HTTP/1.1\r\nHost: x\r\n\r\n",
            "MARKER123".repeat(2000)
        )
        .into_bytes(),
        format!(
            "GET /v1/info HTTP/1.1\r\nX-Big: {}\r\n\r\n",
            "MARKER123".repeat(2000)
        )
        .into_bytes(),
    ] {
        let reply = raw_exchange(f.addr(), &raw);
        assert!(!reply.contains("MARKER123"), "{reply}");
        if !reply.is_empty() {
            assert!(reply.starts_with("HTTP/1.1 4"), "{reply}");
            assert!(reply.ends_with("\r\n\r\n"), "body in {reply}");
        }
    }
}

#[test]
fn a_silent_client_does_not_block_the_others() {
    let f = Fixture::new();
    // Hold every worker with connections that send nothing.
    let idle: Vec<TcpStream> = (0..4)
        .map(|_| TcpStream::connect(f.addr()).unwrap())
        .collect();
    let started = Instant::now();
    let resp = agent()
        .get(&f.url("/v1/info"))
        .header("Authorization", &bearer())
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Served once a worker's head budget (2 s) frees it.
    assert!(started.elapsed() < Duration::from_secs(4));
    drop(idle);
}

/// The status line of `reply`.
fn status_line(reply: &str) -> &str {
    reply.lines().next().unwrap_or("")
}

/// Request framing this server refuses: `(what, headers, body)`.
const BAD_FRAMING: [(&str, &str, &str); 4] = [
    (
        "chunked",
        "Transfer-Encoding: chunked\r\n",
        "5\r\nhello\r\n0\r\n\r\n",
    ),
    (
        "duplicate length",
        "Content-Length: 2\r\nContent-Length: 2\r\n",
        "{}",
    ),
    ("non-numeric length", "Content-Length: two\r\n", "{}"),
    ("signed length", "Content-Length: +5\r\n", "hello"),
];

#[test]
fn unsupported_or_malformed_framing_is_a_bare_400() {
    let f = Fixture::new();
    let auth = format!("Authorization: {}\r\n", bearer());
    for (what, headers, body) in BAD_FRAMING {
        let raw = format!("POST /v1/terms HTTP/1.1\r\nHost: x\r\n{auth}{headers}\r\n{body}");
        let reply = raw_exchange(f.addr(), raw.as_bytes());
        assert!(
            status_line(&reply).starts_with("HTTP/1.1 400"),
            "{what}: {reply}"
        );
        assert!(reply.ends_with("\r\n\r\n"), "{what}: body in {reply}");
    }
}

#[test]
fn bad_framing_without_valid_credentials_is_a_bare_401() {
    let f = Fixture::new();
    for auth in ["", "Authorization: Bearer wrong\r\n"] {
        for (what, headers, body) in BAD_FRAMING {
            let raw = format!("POST /v1/terms HTTP/1.1\r\nHost: x\r\n{auth}{headers}\r\n{body}");
            let reply = raw_exchange(f.addr(), raw.as_bytes());
            assert!(
                status_line(&reply).starts_with("HTTP/1.1 401"),
                "{what} {auth:?}: {reply}"
            );
            assert!(reply.ends_with("\r\n\r\n"), "{what}: body in {reply}");
            assert!(!reply.contains("Content-Type"), "{what}: {reply}");
        }
    }
}

#[test]
fn a_slow_body_keeps_the_rest_of_the_read_budget() {
    let f = Fixture::new();
    let body = r#"{"terms":[],"include_subagents":true}"#;
    let mut stream = TcpStream::connect(f.addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let head = format!(
        "POST /v1/terms HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n",
        bearer(),
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    // Past the 2 s head budget, within the 5 s total.
    thread::sleep(Duration::from_millis(3000));
    stream.write_all(body.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    let reply = String::from_utf8_lossy(&out);
    assert!(status_line(&reply).starts_with("HTTP/1.1 200"), "{reply}");
}

#[test]
fn a_trickling_client_is_cut_off() {
    let f = Fixture::new();
    let mut stream = TcpStream::connect(f.addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let started = Instant::now();
    // One byte of a head that never completes, every 300 ms.
    let head = b"GET /v1/info HTTP/1.1\r\nX-Slow: "
        .iter()
        .chain([b'a'; 100].iter());
    let mut closed = false;
    for byte in head {
        if stream.write_all(&[*byte]).is_err() {
            closed = true;
            break;
        }
        let mut buf = [0u8; 64];
        match stream.read(&mut buf) {
            Ok(_) => {
                closed = true;
                break;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
        if started.elapsed() > Duration::from_secs(15) {
            break;
        }
    }
    let elapsed = started.elapsed();
    assert!(closed, "still connected after {elapsed:?}");
    // The head budget is 2 s; allow slack for scheduling.
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
}
