//! Turns one request into one response over the index, with no transport: the
//! HTTP server parses requests into [`Request`] and writes back [`Response`].

use serde::Serialize;

use super::PROTOCOL;
use super::token::tokens_equal;
use super::wire::{
    Envelope, ErrorBody, Info, Sessions, TermMatches, TermsRequest, TermsResponse, WireMatch,
    WireSession, session_facts,
};
use crate::index::{Index, TextSearch};
use crate::model::{Harness, SessionKey};

/// Largest accepted request body, in bytes.
pub const MAX_BODY: usize = 4096;
/// Most terms in one `POST /v1/terms`.
pub const MAX_TERMS: usize = 8;
/// Longest accepted term, in characters.
pub const MAX_TERM_CHARS: usize = 64;

const BAD_JSON: &str = "invalid JSON";
const BODY_TOO_LARGE: &str = "request body too large";
const TOO_MANY_TERMS: &str = "too many terms";
const TERM_TOO_LONG: &str = "term too long";
const MISSING_PARAM: &str = "missing query parameter";
const BAD_QUERY: &str = "malformed query string";
const INDEX_ERROR: &str = "index error";
const ENCODING_ERROR: &str = "encoding error";

pub struct Request<'a> {
    pub method: &'a str,
    /// The path without the query string.
    pub path: &'a str,
    /// The raw query string, without the `?`.
    pub query: &'a str,
    /// The `Authorization` header's value.
    pub authorization: Option<&'a str>,
    pub body: &'a [u8],
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    /// JSON, or empty for 401, 404 and 405.
    pub body: String,
}

pub struct ServeContext {
    pub index: Index,
    pub token: String,
    pub host: String,
    pub home: String,
    pub version: String,
}

enum Route {
    Info,
    Sessions,
    Session,
    Terms,
}

/// Answers `req` at time `now` (Unix seconds). Authorization is checked
/// before anything else, the path included.
pub fn handle(ctx: &ServeContext, req: &Request<'_>, now: i64) -> Response {
    if !authorized(ctx, req.authorization) {
        return empty(401);
    }
    let (route, method) = match req.path {
        "/v1/info" => (Route::Info, "GET"),
        "/v1/sessions" => (Route::Sessions, "GET"),
        "/v1/session" => (Route::Session, "GET"),
        "/v1/terms" => (Route::Terms, "POST"),
        _ => return empty(404),
    };
    if req.method != method {
        return empty(405);
    }
    if req.body.len() > MAX_BODY {
        return error(400, BODY_TOO_LARGE, now);
    }
    match route {
        Route::Info => info(ctx, now),
        Route::Sessions => sessions(ctx, now),
        Route::Session => session(ctx, req.query, now),
        Route::Terms => terms(ctx, req.body, now),
    }
}

/// Whether an `Authorization` header value is `Bearer <token>` (the scheme
/// in any case) with exactly the configured, non-empty token. [`handle`]
/// checks this first; the server also checks it before refusing a request's
/// framing, so a client without the token only ever sees a bare 401.
pub fn authorized(ctx: &ServeContext, authorization: Option<&str>) -> bool {
    let Some((scheme, given)) = authorization.and_then(|a| a.split_once(' ')) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Bearer")
        && !ctx.token.is_empty()
        && tokens_equal(given, &ctx.token)
}

fn info(ctx: &ServeContext, now: i64) -> Response {
    let Ok(docs) = ctx.index.sessions() else {
        return error(500, INDEX_ERROR, now);
    };
    ok(
        Info {
            hound_version: ctx.version.clone(),
            host: ctx.host.clone(),
            home: ctx.home.clone(),
            sessions: docs.len(),
        },
        now,
    )
}

fn sessions(ctx: &ServeContext, now: i64) -> Response {
    let Ok(docs) = ctx.index.sessions() else {
        return error(500, INDEX_ERROR, now);
    };
    let sessions = docs.iter().map(WireSession::from_doc).collect();
    ok(Sessions { sessions }, now)
}

fn session(ctx: &ServeContext, query: &str, now: i64) -> Response {
    let Some(params) = parse_query(query) else {
        return error(400, BAD_QUERY, now);
    };
    let param = |name: &str| {
        params
            .iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| v.as_str())
    };
    let (Some(harness), Some(id)) = (param("harness"), param("id")) else {
        return error(400, MISSING_PARAM, now);
    };
    let Some(harness) = Harness::parse(harness) else {
        return empty(404);
    };
    let key = SessionKey {
        harness,
        id: id.to_string(),
    };
    match ctx.index.session(&key) {
        Ok(Some(doc)) => ok(session_facts(&doc), now),
        Ok(None) => empty(404),
        Err(_) => error(500, INDEX_ERROR, now),
    }
}

fn terms(ctx: &ServeContext, body: &[u8], now: i64) -> Response {
    let Ok(req) = serde_json::from_slice::<TermsRequest>(body) else {
        return error(400, BAD_JSON, now);
    };
    if req.terms.len() > MAX_TERMS {
        return error(400, TOO_MANY_TERMS, now);
    }
    if req
        .terms
        .iter()
        .any(|t| t.term.chars().count() > MAX_TERM_CHARS)
    {
        return error(400, TERM_TOO_LONG, now);
    }
    let mut terms = Vec::with_capacity(req.terms.len());
    for spec in req.terms {
        let Ok(hits) = ctx
            .index
            .term_matches(&spec.term, spec.mode.into(), req.include_subagents)
        else {
            return error(500, INDEX_ERROR, now);
        };
        let mut matches: Vec<WireMatch> = hits
            .into_iter()
            .map(|(key, hit)| WireMatch {
                harness: key.harness.as_str().to_string(),
                id: key.id,
                score: hit.score,
                snippet: hit.snippet,
                from_subagent: hit.from_subagent,
            })
            .collect();
        // Best first; a stable order for equal scores.
        matches.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| (&a.harness, &a.id).cmp(&(&b.harness, &b.id)))
        });
        terms.push(TermMatches {
            term: spec.term,
            matches,
        });
    }
    ok(TermsResponse { terms }, now)
}

/// `name=value` pairs, percent-decoded (`+` is a space); `None` if a pair is
/// not valid percent-encoded UTF-8.
fn parse_query(query: &str) -> Option<Vec<(String, String)>> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect()
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex_digit(*bytes.get(i + 1)?)?;
                let lo = hex_digit(*bytes.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn ok<T: Serialize>(body: T, now: i64) -> Response {
    match serde_json::to_string(&Envelope {
        protocol: PROTOCOL,
        now,
        body,
    }) {
        Ok(body) => Response { status: 200, body },
        Err(_) => error(500, ENCODING_ERROR, now),
    }
}

fn error(status: u16, reason: &str, now: i64) -> Response {
    let body = serde_json::to_string(&Envelope {
        protocol: PROTOCOL,
        now,
        body: ErrorBody {
            error: reason.to_string(),
        },
    })
    .expect("an error envelope always encodes");
    Response { status, body }
}

fn empty(status: u16) -> Response {
    Response {
        status,
        body: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Db").as_deref(), Some("a-b"));
        assert_eq!(percent_decode("a%2db").as_deref(), Some("a-b"));
        assert_eq!(percent_decode("a+b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("%2B").as_deref(), Some("+"));
        assert_eq!(percent_decode("%C3%A9").as_deref(), Some("é"));
        assert_eq!(percent_decode("%+5"), None);
        assert_eq!(percent_decode("%5"), None);
        assert_eq!(percent_decode("%ZZ"), None);
        assert_eq!(percent_decode("%FF"), None);
    }
}
