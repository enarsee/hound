//! A minimal client for herdr's socket API: newline-delimited JSON, one request line answered
//! by one response line carrying the same `id`. The socket path is the `HERDR_SOCKET_PATH`
//! herdr injects into every pane it manages. On Unix it is a Unix socket; on Windows herdr
//! listens on a named pipe whose name is that path string.
//!
//! The request/response logic ([`exchange`]) is the same on both platforms; only the stream
//! differs.

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

const TIMEOUT: Duration = Duration::from_secs(10);

/// One request as a newline-terminated JSON line.
pub fn request_line(id: &str, method: &str, params: &serde_json::Value) -> String {
    let request = serde_json::json!({ "id": id, "method": method, "params": params });
    format!("{request}\n")
}

/// Sends one request line (from [`request_line`]) over `stream` and returns the response's
/// `result`; an `error` response, a mismatched `id` or a closed connection is an error.
pub fn exchange<S: Read + Write>(
    mut stream: S,
    request: &str,
) -> anyhow::Result<serde_json::Value> {
    let sent: serde_json::Value = serde_json::from_str(request).context("request is not JSON")?;
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("herdr closed the connection without a response");
    }
    let response: serde_json::Value =
        serde_json::from_str(&line).context("response is not JSON")?;
    let id_matches = response.get("id") == sent.get("id");
    let result = super::result_from_value::<serde_json::Value>(response)?;
    if !id_matches {
        bail!("response id does not match the request");
    }
    Ok(result)
}

/// Runs `f` on a helper thread and waits at most `limit` for it. On timeout the thread is
/// abandoned; a detached thread does not keep the process alive at exit.
pub fn with_time_limit<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is gone if the caller already gave up.
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "herdr did not answer within {}s",
            limit.as_secs()
        ))
    })
}

/// Connects to `HERDR_SOCKET_PATH`; `None` if it is unset or the connection fails.
pub fn connect_from_env() -> Option<SocketConnection> {
    let path = env::var_os("HERDR_SOCKET_PATH")?;
    SocketConnection::connect(Path::new(&path)).ok()
}

/// An open connection to a herdr server's socket.
#[cfg(unix)]
pub struct SocketConnection {
    stream: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl SocketConnection {
    pub fn connect(path: &Path) -> anyhow::Result<SocketConnection> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(SocketConnection { stream })
    }

    /// Sends one request line (from [`request_line`]) and returns the response's `result`; an
    /// `error` response, a mismatched `id` or a closed connection is an error.
    pub fn call(&mut self, request: &str) -> anyhow::Result<serde_json::Value> {
        exchange(&self.stream, request)
    }
}

/// herdr's named pipe. Named pipes have no receive timeout (herdr's own client treats
/// `Unsupported` from `set_recv_timeout` as normal), so each call connects, writes and reads on
/// a helper thread under one overall limit; connecting is therefore deferred to [`Self::call`].
#[cfg(windows)]
pub struct SocketConnection {
    name: String,
}

#[cfg(windows)]
impl SocketConnection {
    pub fn connect(path: &Path) -> anyhow::Result<SocketConnection> {
        Ok(SocketConnection {
            name: path.to_string_lossy().to_string(),
        })
    }

    /// Sends one request line (from [`request_line`]) and returns the response's `result`; an
    /// `error` response, a mismatched `id`, a closed connection or no answer within 10 seconds
    /// is an error.
    pub fn call(&mut self, request: &str) -> anyhow::Result<serde_json::Value> {
        let name = self.name.clone();
        let request = request.to_owned();
        with_time_limit(TIMEOUT, move || {
            use interprocess::local_socket::{GenericNamespaced, Stream, prelude::*};

            // As herdr's `connect_local_stream` does.
            let pipe = name
                .clone()
                .to_ns_name::<GenericNamespaced>()
                .with_context(|| format!("naming the pipe {name}"))?;
            let stream = Stream::connect(pipe).with_context(|| format!("connecting to {name}"))?;
            exchange(&stream, &request)
        })
    }
}
