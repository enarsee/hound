//! A blocking client for another machine's hound service.
//!
//! The client talks only to the configured URL: it never uses a proxy from the
//! environment and never follows a redirect, so the token goes nowhere else.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::PROTOCOL;
use super::wire::{
    Envelope, Info, SessionFacts, Sessions, TermSpec, TermsRequest, TermsResponse, WireMode,
};
use crate::adapters::valid_session_id;
use crate::index::{SessionDoc, TermHit, TermMode, TextSearch};
use crate::model::{Harness, SessionKey};

/// Largest accepted response body, in bytes; a larger `/v1/sessions` is
/// [`RemoteError::TooLarge`].
pub const MAX_SESSIONS_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum RemoteError {
    /// No answer: the connection failed or timed out. A short reason, never
    /// the URL or the token.
    Unreachable(String),
    Unauthorised,
    /// The remote speaks another protocol version.
    Protocol {
        theirs: u32,
    },
    TooLarge,
    /// Anything else unexpected: a short reason, never the token.
    Bad(String),
}

impl RemoteError {
    /// A few words for the user.
    pub fn note(&self) -> &'static str {
        match self {
            RemoteError::Unreachable(_) => "unreachable",
            RemoteError::Unauthorised => "unauthorised",
            RemoteError::Protocol { theirs } if *theirs > PROTOCOL => "needs a newer hound",
            RemoteError::Protocol { .. } => "needs an update there",
            RemoteError::TooLarge => "too many sessions",
            RemoteError::Bad(_) => "bad response",
        }
    }
}

/// A remote machine's sessions, as stand-in documents with their facts.
#[derive(Debug, Clone)]
pub struct RemoteSessions {
    pub sessions: Vec<SessionDoc>,
    pub facts: HashMap<SessionKey, SessionFacts>,
    /// The server's clock, Unix seconds.
    pub now: i64,
}

pub struct RemoteClient {
    agent: ureq::Agent,
    base: String,
    authorization: String,
}

// Shared between popup threads.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RemoteClient>();
};

impl fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Only the protocol number, read before the body is trusted.
#[derive(Deserialize)]
struct ProtocolProbe {
    protocol: u32,
}

impl RemoteClient {
    /// `url` is the service's base URL, with or without a trailing slash.
    /// `timeout` bounds connecting and, separately, each whole request.
    pub fn new(url: &str, token: String, timeout: Duration) -> RemoteClient {
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(timeout))
            .timeout_global(Some(timeout))
            .build()
            .new_agent();
        RemoteClient {
            agent,
            base: url.trim_end_matches('/').to_string(),
            authorization: format!("Bearer {token}"),
        }
    }

    /// The remote's description and its clock (Unix seconds).
    pub fn info(&self) -> Result<(Info, i64), RemoteError> {
        let env: Envelope<Info> = self.get("/v1/info", &[])?;
        Ok((env.body, env.now))
    }

    /// Every remote session; sessions of a harness this build does not know
    /// or with an invalid id are skipped (see [`WireSession::into_doc`]).
    ///
    /// [`WireSession::into_doc`]: super::wire::WireSession::into_doc
    pub fn sessions(&self) -> Result<RemoteSessions, RemoteError> {
        let env: Envelope<Sessions> = self.get("/v1/sessions", &[])?;
        let mut sessions = Vec::with_capacity(env.body.sessions.len());
        let mut facts = HashMap::with_capacity(env.body.sessions.len());
        for (doc, f) in env.body.sessions.into_iter().filter_map(|s| s.into_doc()) {
            facts.insert(doc.key.clone(), f);
            sessions.push(doc);
        }
        Ok(RemoteSessions {
            sessions,
            facts,
            now: env.now,
        })
    }

    /// The session's facts, read fresh on the remote, and its clock. A
    /// session the remote no longer has is `Bad("unknown session")`. So is a
    /// key whose id is not [valid](valid_session_id), which no remote indexes:
    /// it is refused without a request.
    pub fn session_facts(&self, key: &SessionKey) -> Result<(SessionFacts, i64), RemoteError> {
        if !valid_session_id(&key.id) {
            return Err(bad("unknown session"));
        }
        let env: Envelope<SessionFacts> = self
            .get(
                "/v1/session",
                &[("harness", key.harness.as_str()), ("id", &key.id)],
            )
            .map_err(|e| match e {
                RemoteError::Bad(reason) if reason == "status 404" => {
                    RemoteError::Bad("unknown session".to_string())
                }
                e => e,
            })?;
        Ok((env.body, env.now))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Envelope<T>, RemoteError> {
        let mut req = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json");
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        decode(req.call())
    }

    fn post<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<Envelope<T>, RemoteError> {
        let body = serde_json::to_vec(body).map_err(|_| bad("encoding error"))?;
        let resp = self
            .agent
            .post(self.url(path))
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send(&body[..]);
        decode(resp)
    }
}

/// Strings from a remote are untrusted. Match ids are validated here, as they
/// key sessions that may be resumed; snippets must be sanitised before they
/// reach a terminal or a command line.
impl TextSearch for RemoteClient {
    /// One `POST /v1/terms`; matches of a harness this build does not know,
    /// or whose id is not [valid](valid_session_id), are skipped. Errors carry
    /// the [`RemoteError::note`].
    fn term_matches(
        &self,
        term: &str,
        mode: TermMode,
        include_subagents: bool,
    ) -> anyhow::Result<HashMap<SessionKey, TermHit>> {
        let request = TermsRequest {
            terms: vec![TermSpec {
                term: term.to_string(),
                mode: WireMode::from(mode),
            }],
            include_subagents,
        };
        let env: Envelope<TermsResponse> = self
            .post("/v1/terms", &request)
            .map_err(|e| anyhow::anyhow!(e.note()))?;
        let [entry] = <[_; 1]>::try_from(env.body.terms)
            .map_err(|_| anyhow::anyhow!(bad("term count").note()))?;
        Ok(entry
            .matches
            .into_iter()
            .filter_map(|m| {
                let harness = Harness::parse(&m.harness)?;
                if !valid_session_id(&m.id) {
                    return None;
                }
                Some((
                    SessionKey { harness, id: m.id },
                    TermHit {
                        score: m.score,
                        snippet: m.snippet,
                        from_subagent: m.from_subagent,
                    },
                ))
            })
            .collect())
    }
}

fn bad(reason: &str) -> RemoteError {
    RemoteError::Bad(reason.to_string())
}

/// Status, size cap, protocol check, then the body.
fn decode<T: DeserializeOwned>(
    resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Envelope<T>, RemoteError> {
    let resp = resp.map_err(transport_error)?;
    let status = resp.status().as_u16();
    if status == 401 {
        return Err(RemoteError::Unauthorised);
    }
    let declared = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_SESSIONS_BYTES) {
        return Err(RemoteError::TooLarge);
    }
    let bytes = read_capped(resp.into_body())?;
    // A JSON body from another protocol version is reported as such, whatever
    // the status.
    if let Ok(probe) = serde_json::from_slice::<ProtocolProbe>(&bytes)
        && probe.protocol != PROTOCOL
    {
        return Err(RemoteError::Protocol {
            theirs: probe.protocol,
        });
    }
    if status != 200 {
        return Err(RemoteError::Bad(format!("status {status}")));
    }
    serde_json::from_slice::<ProtocolProbe>(&bytes).map_err(|_| bad("not a hound response"))?;
    serde_json::from_slice(&bytes).map_err(|_| bad("unexpected body"))
}

/// The whole body, or `TooLarge` as soon as it passes the cap.
fn read_capped(body: ureq::Body) -> Result<Vec<u8>, RemoteError> {
    let mut bytes = Vec::new();
    body.into_reader()
        .take(MAX_SESSIONS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| transport_error(ureq::Error::from(e)))?;
    if bytes.len() as u64 > MAX_SESSIONS_BYTES {
        return Err(RemoteError::TooLarge);
    }
    Ok(bytes)
}

/// A failure below HTTP status: built from fixed text, so it never carries
/// the URL or the token.
fn transport_error(err: ureq::Error) -> RemoteError {
    use ureq::Error as E;
    let reason = match err {
        E::Timeout(_) => "timed out",
        E::HostNotFound => "host not found",
        E::ConnectionFailed => "connection failed",
        E::Io(e) => io_reason(&e),
        E::BadUri(_) | E::Http(_) => "invalid URL",
        E::BodyExceedsLimit(_) => return RemoteError::TooLarge,
        E::Protocol(_) => return bad("malformed HTTP"),
        _ => return bad("request failed"),
    };
    RemoteError::Unreachable(reason.to_string())
}

fn io_reason(e: &io::Error) -> &'static str {
    use io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => "connection refused",
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => "connection reset",
        K::TimedOut | K::WouldBlock => "timed out",
        K::UnexpectedEof => "connection closed",
        K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => "no route to host",
        K::AddrNotAvailable => "address not available",
        _ => "connection failed",
    }
}
