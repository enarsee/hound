//! The client ignores proxy variables. The variables are set only on a child
//! process (this binary run again), so they never leak into other tests.
//!
//! This is a binary of its own because it starts a process, and no index is
//! opened here: a child forked while another thread holds an index's
//! `writer.lock` shares that lock until it execs.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::Command;
use std::thread;
use std::time::Duration;

use hound_core::remote::client::RemoteClient;

const TOKEN: &str = "t0k-SECRET";

/// Answers every connection with a valid `/v1/info` response.
fn info_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            let body = r#"{"protocol":1,"now":0,"hound_version":"0.1.0","host":"box","home":"/home/u","sessions":2}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    addr
}

/// Run as a child process of [`proxy_environment_is_ignored`], with proxy
/// variables pointing at a dead address.
#[test]
#[ignore = "run by proxy_environment_is_ignored in a child process"]
fn proxy_environment_child() {
    assert!(std::env::var("HTTP_PROXY").is_ok() && std::env::var("ALL_PROXY").is_ok());
    let url = format!("http://{}", info_server());
    // A client that honours the environment does go through the dead proxy.
    let honouring = ureq::get(format!("{url}/v1/info"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .call();
    assert!(honouring.is_err(), "the proxy variables had no effect");
    let client = RemoteClient::new(&url, TOKEN.to_string(), Duration::from_secs(10));
    assert_eq!(client.info().unwrap().0.sessions, 2);
}

#[test]
fn proxy_environment_is_ignored() {
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let proxy = format!("http://{dead}");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "proxy_environment_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("HTTP_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("all_proxy", &proxy)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
}
