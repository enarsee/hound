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
/// abandoned; a detached thread does not keep the process alive at exit. A helper that ends
/// without a result (it panicked) is reported separately from a timeout.
pub fn with_time_limit<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is gone if the caller already gave up.
        let _ = tx.send(f());
    });
    match rx.recv_timeout(limit) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("herdr did not answer within {}", describe_limit(limit))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("herdr call failed unexpectedly"),
    }
}

/// `10s` for whole seconds, otherwise the `Duration`'s own form such as `50ms`.
fn describe_limit(limit: Duration) -> String {
    if limit.subsec_nanos() == 0 {
        format!("{}s", limit.as_secs())
    } else {
        format!("{limit:?}")
    }
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

/// herdr's named pipe, connected as herdr's `connect_local_stream` does. Named pipes have no
/// receive timeout (herdr's own client treats `Unsupported` from `set_recv_timeout` as normal),
/// so connecting and each call run on a helper thread, each under the 10-second limit.
#[cfg(windows)]
pub struct SocketConnection {
    stream: std::sync::Arc<interprocess::local_socket::Stream>,
}

#[cfg(windows)]
impl SocketConnection {
    pub fn connect(path: &Path) -> anyhow::Result<SocketConnection> {
        let name = path.to_string_lossy().to_string();
        let stream = with_time_limit(TIMEOUT, move || {
            use interprocess::local_socket::{GenericNamespaced, Stream, prelude::*};

            let pipe = name
                .clone()
                .to_ns_name::<GenericNamespaced>()
                .with_context(|| format!("naming the pipe {name}"))?;
            Stream::connect(pipe).with_context(|| format!("connecting to {name}"))
        })?;
        Ok(SocketConnection {
            stream: std::sync::Arc::new(stream),
        })
    }

    /// Sends one request line (from [`request_line`]) and returns the response's `result`; an
    /// `error` response, a mismatched `id`, a closed connection or no answer within 10 seconds
    /// is an error.
    pub fn call(&mut self, request: &str) -> anyhow::Result<serde_json::Value> {
        let stream = std::sync::Arc::clone(&self.stream);
        let request = request.to_owned();
        with_time_limit(TIMEOUT, move || exchange(&*stream, &request))
    }
}
