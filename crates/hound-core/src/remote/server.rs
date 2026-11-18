//! A small blocking HTTP/1.1 server around [`handle`]: one request per
//! connection, on a fixed pool of worker threads.
//!
//! It only turns bytes into a [`Request`] and a [`Response`] back into bytes;
//! authorisation, routing and limits are decided by [`handle`]. A request it
//! cannot parse gets a bare status line and no body, so nothing the client
//! sent is ever echoed: a head too large or unparseable a bare 431 or 400,
//! and one whose head parsed but whose body framing it refuses a bare 401
//! without valid credentials (checked by [`authorized`]) or 400 with them.
//! It logs nothing.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use super::handler::{MAX_BODY, Request, Response, ServeContext, authorized, handle};

/// Worker threads; further connections wait in the listen backlog.
const WORKERS: usize = 4;
/// Time allowed to read a request's line and headers, so idle connections
/// cannot hold every worker for long.
const HEAD_BUDGET: Duration = Duration::from_secs(2);
/// Time allowed to read a whole request, head and body.
const READ_BUDGET: Duration = Duration::from_secs(5);
/// Time allowed to write a whole response; enough for a multi-MiB
/// `/v1/sessions` to reach a healthy client, short enough that a client
/// reading slowly cannot hold a worker for long.
const WRITE_BUDGET: Duration = Duration::from_secs(30);
/// Largest request line plus headers, in bytes.
const MAX_HEAD: usize = 8 * 1024;
/// Most headers in one request.
const MAX_HEADERS: usize = 32;
/// After answering a request whose body was not read to the end, how long
/// the rest is read and discarded so the client sees the answer instead of
/// a reset connection.
const LINGER: Duration = Duration::from_secs(1);
/// How long dropping a [`ServerHandle`] waits for its workers to finish.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// Refuses an unspecified address (`0.0.0.0`, `[::]`, or the IPv4-mapped
/// `[::ffff:0.0.0.0]`), which listens on every interface, unless
/// `allow_any_interface`.
pub fn check_bind(addr: &SocketAddr, allow_any_interface: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        allow_any_interface || !addr.ip().to_canonical().is_unspecified(),
        "refusing to listen on {addr}, which is every network interface; bind one address, \
         or set serve.allow_any_interface = true"
    );
    Ok(())
}

pub struct Server {
    listener: TcpListener,
    addr: SocketAddr,
    ctx: Arc<ServeContext>,
    stop: Arc<AtomicBool>,
}

/// A server running on background threads; dropping it stops the server.
pub struct ServerHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn bind(addr: SocketAddr, ctx: ServeContext) -> anyhow::Result<Server> {
        let listener = TcpListener::bind(addr).with_context(|| format!("listening on {addr}"))?;
        let addr = listener.local_addr()?;
        Ok(Server {
            listener,
            addr,
            ctx: Arc::new(ctx),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The bound address, with the port chosen when binding port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serves on the worker pool, blocking until the process ends.
    pub fn run(self) -> anyhow::Result<()> {
        for worker in self.start()? {
            let _ = worker.join();
        }
        Ok(())
    }

    /// Serves on background threads until the handle is dropped.
    pub fn spawn(self) -> ServerHandle {
        let addr = self.addr;
        let stop = Arc::clone(&self.stop);
        // Cloning the listener only fails when out of file descriptors; serve
        // on as many workers as could be started.
        let workers = self.start_some();
        ServerHandle {
            addr,
            stop,
            workers,
        }
    }

    fn start(self) -> anyhow::Result<Vec<JoinHandle<()>>> {
        let workers = self.start_some();
        anyhow::ensure!(!workers.is_empty(), "starting the server's threads");
        Ok(workers)
    }

    /// Up to [`WORKERS`] threads, each accepting on its own handle to the
    /// listener.
    fn start_some(self) -> Vec<JoinHandle<()>> {
        let mut workers = Vec::with_capacity(WORKERS);
        for _ in 0..WORKERS {
            let Ok(listener) = self.listener.try_clone() else {
                break;
            };
            let ctx = Arc::clone(&self.ctx);
            let stop = Arc::clone(&self.stop);
            let spawned = thread::Builder::new()
                .name("hound-serve".to_string())
                .spawn(move || worker(&listener, &ctx, &stop));
            if let Ok(handle) = spawned {
                workers.push(handle);
            }
        }
        workers
    }
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake each worker blocked in accept; it sees the flag and leaves.
        let wake = wake_addr(self.addr);
        for _ in 0..self.workers.len() {
            let _ = TcpStream::connect_timeout(&wake, Duration::from_millis(200));
        }
        // Join the workers that finish in time; leave any still busy with a
        // slow client to end on their own (their timeouts bound that).
        let deadline = Instant::now() + STOP_WAIT;
        while self.workers.iter().any(|w| !w.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        for worker in self.workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

/// Where to connect to reach a server bound to `addr`.
fn wake_addr(addr: SocketAddr) -> SocketAddr {
    let ip = match addr.ip().to_canonical() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, addr.port())
}

fn worker(listener: &TcpListener, ctx: &ServeContext, stop: &AtomicBool) {
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match accepted {
            Ok((stream, _)) => {
                // A panic loses this connection, not the worker.
                let _ = panic::catch_unwind(AssertUnwindSafe(|| serve_connection(stream, ctx)));
            }
            // Out of file descriptors and the like: back off a little.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// What a connection's request turned out to be.
enum Parsed {
    Request(ParsedRequest),
    /// Unparseable or over a limit: answer with this bare status, then
    /// discard unread input first if `linger` (the client is likely still
    /// sending).
    Bad {
        status: u16,
        linger: bool,
    },
    /// The client went away or ran out of time: just close.
    Gone,
}

struct ParsedRequest {
    method: String,
    path: String,
    query: String,
    authorization: Option<String>,
    /// At most `MAX_BODY + 1` bytes.
    body: Vec<u8>,
    /// The client said it would send more than was read.
    unread_body: bool,
}

fn serve_connection(mut stream: TcpStream, ctx: &ServeContext) -> io::Result<()> {
    let started = Instant::now();
    let deadlines = Deadlines {
        head: started + HEAD_BUDGET,
        body: started + READ_BUDGET,
    };
    let (response, linger) = match read_request(&mut stream, deadlines, ctx) {
        Parsed::Request(req) => {
            let response = handle(
                ctx,
                &Request {
                    method: &req.method,
                    path: &req.path,
                    query: &req.query,
                    authorization: req.authorization.as_deref(),
                    body: &req.body,
                },
                now(),
            );
            (response, req.unread_body)
        }
        Parsed::Bad { status, linger } => (
            Response {
                status,
                body: String::new(),
            },
            linger,
        ),
        Parsed::Gone => return Ok(()),
    };
    write_before(
        &mut stream,
        &encode(&response),
        Instant::now() + WRITE_BUDGET,
    )?;
    if linger {
        discard_rest(&mut stream);
    }
    Ok(())
}

/// When reading a request must be done by.
#[derive(Clone, Copy)]
struct Deadlines {
    head: Instant,
    body: Instant,
}

/// Reads the request line and headers before `deadlines.head` and up to
/// `MAX_BODY + 1` bytes of body before `deadlines.body`. Refused body framing
/// is a 401 unless the request carries valid credentials for `ctx`.
fn read_request(stream: &mut TcpStream, deadlines: Deadlines, ctx: &ServeContext) -> Parsed {
    let mut buf = vec![0u8; MAX_HEAD];
    let mut filled = 0;
    let head_len = loop {
        if filled == buf.len() {
            return Parsed::Bad {
                status: 431,
                linger: true,
            };
        }
        match read_before(stream, &mut buf[filled..], deadlines.head) {
            Ok(0) | Err(_) => return Parsed::Gone,
            Ok(n) => filled += n,
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        match httparse::Request::new(&mut headers).parse(&buf[..filled]) {
            Ok(httparse::Status::Complete(len)) => break len,
            Ok(httparse::Status::Partial) => {}
            Err(httparse::Error::TooManyHeaders) => {
                return Parsed::Bad {
                    status: 431,
                    linger: true,
                };
            }
            Err(_) => {
                return Parsed::Bad {
                    status: 400,
                    linger: false,
                };
            }
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    let bad_head = Parsed::Bad {
        status: 400,
        linger: false,
    };
    if req.parse(&buf[..filled]).is_err() {
        return bad_head;
    }
    let (Some(method), Some(target)) = (req.method, req.path) else {
        return bad_head;
    };
    let header = |name: &str| -> Vec<&[u8]> {
        req.headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value)
            .collect()
    };
    // Exactly one Authorization header in UTF-8; anything else is no
    // credentials, which `handle` answers with 401.
    let authorization = match header("authorization").as_slice() {
        [value] => std::str::from_utf8(value).ok().map(str::to_string),
        _ => None,
    };
    // A body this server will not read: a bare 401 without credentials, else
    // a bare 400; then discard it.
    let bad_body = Parsed::Bad {
        status: if authorized(ctx, authorization.as_deref()) {
            400
        } else {
            401
        },
        linger: true,
    };
    if !header("transfer-encoding").is_empty() {
        return bad_body;
    }
    let content_length = match header("content-length").as_slice() {
        [] => 0,
        [value] => match parse_content_length(value) {
            Some(n) => n,
            None => return bad_body,
        },
        _ => return bad_body,
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let wanted = content_length.min(MAX_BODY as u64 + 1) as usize;
    let mut body = buf[head_len..filled].to_vec();
    body.truncate(wanted);
    if body.len() < wanted {
        let start = body.len();
        body.resize(wanted, 0);
        let mut got = start;
        while got < wanted {
            match read_before(stream, &mut body[got..], deadlines.body) {
                Ok(0) | Err(_) => return Parsed::Gone,
                Ok(n) => got += n,
            }
        }
    }
    let unread_body = content_length > wanted as u64;
    Parsed::Request(ParsedRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        authorization,
        body,
        unread_body,
    })
}

/// A `Content-Length` value: ASCII digits only, no sign or whitespace.
fn parse_content_length(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Writes all of `bytes`, failing if that is not done by `deadline`.
fn write_before(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        stream.set_write_timeout(Some(left))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One read that must finish before `deadline`.
fn read_before(stream: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(io::ErrorKind::TimedOut)?;
    stream.set_read_timeout(Some(left))?;
    stream.read(buf)
}

/// Signals the end of the response, then reads and drops whatever the client
/// is still sending, for at most [`LINGER`], so closing does not reset the
/// connection before the client has read the response.
fn discard_rest(stream: &mut TcpStream) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = Instant::now() + LINGER;
    let mut sink = [0u8; 16 * 1024];
    while let Ok(n) = read_before(stream, &mut sink, deadline) {
        if n == 0 {
            break;
        }
    }
}

/// The status line and headers, then the body. JSON bodies are labelled as
/// such; empty bodies carry no content type.
fn encode(response: &Response) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    if !response.body.is_empty() {
        out.push_str("Content-Type: application/json\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    out.push_str(&response.body);
    out.into_bytes()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "",
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
