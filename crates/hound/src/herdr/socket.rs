//! A minimal client for herdr's socket API: newline-delimited JSON over a Unix socket, one
//! request line answered by one response line carrying the same `id`. The socket path is the
//! `HERDR_SOCKET_PATH` herdr injects into every pane it manages.

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};

const TIMEOUT: Duration = Duration::from_secs(10);

/// One request as a newline-terminated JSON line.
pub fn request_line(id: &str, method: &str, params: &serde_json::Value) -> String {
    let request = serde_json::json!({ "id": id, "method": method, "params": params });
    format!("{request}\n")
}

/// An open connection to a herdr server's socket.
pub struct SocketConnection {
    stream: UnixStream,
}

/// Connects to `HERDR_SOCKET_PATH`; `None` if it is unset or the connection fails.
pub fn connect_from_env() -> Option<SocketConnection> {
    let path = env::var_os("HERDR_SOCKET_PATH")?;
    SocketConnection::connect(Path::new(&path)).ok()
}

impl SocketConnection {
    pub fn connect(path: &Path) -> anyhow::Result<SocketConnection> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(SocketConnection { stream })
    }

    /// Sends one request line (from [`request_line`]) and returns the response's `result`; an
    /// `error` response, a mismatched `id` or a closed connection is an error.
    pub fn call(&mut self, request: &str) -> anyhow::Result<serde_json::Value> {
        let sent: serde_json::Value =
            serde_json::from_str(request).context("request is not JSON")?;
        self.stream.write_all(request.as_bytes())?;
        self.stream.flush()?;

        let mut line = String::new();
        BufReader::new(&self.stream).read_line(&mut line)?;
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
}
