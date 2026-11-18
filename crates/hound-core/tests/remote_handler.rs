mod common;

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use common::*;
use hound_core::adapters::Adapter;
use hound_core::adapters::claude::ClaudeAdapter;
use hound_core::index::{FileState, Index, SessionDoc};
use hound_core::model::{Harness, SessionKey};
use hound_core::remote::PROTOCOL;
use hound_core::remote::handler::{
    MAX_BODY, MAX_TERM_CHARS, MAX_TERMS, Request, Response, ServeContext, handle,
};
use hound_core::remote::token::{load_or_create_token, read_token, tokens_equal};
use hound_core::remote::wire::{
    Envelope, ErrorBody, Info, SessionFacts, Sessions, TermsResponse, WireSession,
};
use serde_json::{Value, json};

const S1: &str = "11111111-1111-1111-1111-111111111111";
const S2: &str = "22222222-2222-2222-2222-222222222222";
const S3: &str = "33333333-3333-3333-3333-333333333333";
/// S3's first prompt: over 80 characters, with a line break inside the first 80.
const S3_PROMPT: &str = "Help me port the herd tracker\nto the new cattle registry API before the auction closes on Friday, keeping BARNOWLTAIL intact";
/// S3's later messages.
const S3_LATER: &[&str] = &["the KESTRELREPLY drafted", "MARMOTTOOL output here"];
const SLUG: &str = "-home-u-proj";
const NOW: i64 = 1_800_000_000;
/// Every piece of conversation text in the fixture; none may leave the service
/// except as a snippet.
const CONVERSATION: &[&str] = &[
    "Sent to the Signal group",
    "retry failure alert",
    "investigated nginx config",
    "plan the qa pass via Riverbend",
];

fn main_path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG).join(format!("{id}.jsonl"))
}

fn sub_path(root: &Path, id: &str) -> PathBuf {
    root.join(SLUG)
        .join(id)
        .join("subagents")
        .join("agent-a.jsonl")
}

/// S1 "Depot alerts" in an existing directory, with a subagent that mentions
/// nginx; S2 "QA pass" in a directory that does not exist.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    s1_cwd: String,
    s2_cwd: String,
    ctx: ServeContext,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        let s1_cwd = dir.path().join("work");
        fs::create_dir_all(&s1_cwd).unwrap();
        let s1_cwd = s1_cwd.to_str().unwrap().to_string();
        let s2_cwd = dir.path().join("gone").to_str().unwrap().to_string();
        write_lines(
            &main_path(&root, S1),
            &[
                claude_user(S1, &s1_cwd, T0, "Sent to the Signal group"),
                claude_text(S1, T1, "retry failure alert"),
                claude_title(S1, "Depot alerts"),
            ],
        );
        write_lines(
            &sub_path(&root, S1),
            &[claude_sidechain(S1, T1, "investigated nginx config")],
        );
        write_lines(
            &main_path(&root, S2),
            &[
                claude_user(S2, &s2_cwd, T0, "plan the qa pass via Riverbend"),
                claude_title(S2, "QA pass"),
            ],
        );
        let index = Index::open(&dir.path().join("state")).unwrap();
        let adapters: Vec<Box<dyn Adapter>> =
            vec![Box::new(ClaudeAdapter::new(root.clone(), true))];
        index.refresh(&adapters, &mut |_, _| {}).unwrap().unwrap();
        let ctx = ServeContext {
            index,
            token: "t0k".to_string(),
            host: "box".to_string(),
            home: "/home/u".to_string(),
            version: "0.1.0".to_string(),
        };
        Fixture {
            _dir: dir,
            root,
            s1_cwd,
            s2_cwd,
            ctx,
        }
    }

    /// Adds S3, a session with no harness-recorded title, and re-indexes.
    fn add_untitled(&self) {
        let cwd = self._dir.path().join("work").to_str().unwrap().to_string();
        write_lines(
            &main_path(&self.root, S3),
            &[
                claude_user(S3, &cwd, T0, S3_PROMPT),
                claude_text(S3, T1, S3_LATER[0]),
                claude_tool_result(S3, T1, S3_LATER[1]),
            ],
        );
        let adapters: Vec<Box<dyn Adapter>> =
            vec![Box::new(ClaudeAdapter::new(self.root.clone(), true))];
        self.ctx
            .index
            .refresh(&adapters, &mut |_, _| {})
            .unwrap()
            .unwrap();
    }

    fn get(&self, path: &str) -> Response {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        self.send(Request {
            method: "GET",
            path,
            query,
            authorization: Some("Bearer t0k"),
            body: b"",
        })
    }

    fn post(&self, path: &str, body: &str) -> Response {
        self.send(Request {
            method: "POST",
            path,
            query: "",
            authorization: Some("Bearer t0k"),
            body: body.as_bytes(),
        })
    }

    fn send(&self, req: Request<'_>) -> Response {
        handle(&self.ctx, &req, NOW)
    }

    fn sessions(&self) -> Vec<WireSession> {
        let resp = self.get("/v1/sessions");
        assert_eq!(resp.status, 200);
        let env: Envelope<Sessions> = serde_json::from_str(&resp.body).unwrap();
        env.body.sessions
    }

    fn terms(&self, body: &Value) -> TermsResponse {
        let resp = self.post("/v1/terms", &body.to_string());
        assert_eq!(resp.status, 200, "{}", resp.body);
        let env: Envelope<TermsResponse> = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(env.protocol, PROTOCOL);
        env.body
    }
}

/// Asserts a 400 whose body is an error envelope that does not contain `input`.
fn assert_bad_request(resp: &Response, input: &str) {
    assert_eq!(resp.status, 400, "{}", resp.body);
    let env: Envelope<ErrorBody> = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(env.protocol, 1);
    assert_eq!(env.now, NOW);
    assert!(!env.body.error.is_empty());
    assert!(
        !resp.body.contains(input),
        "error echoes input: {}",
        resp.body
    );
}

#[test]
fn rejects_missing_wrong_and_malformed_auth() {
    let f = Fixture::new();
    for auth in [
        None,
        Some("Bearer nope"),
        Some("t0k"),
        Some("Basic t0k"),
        Some("Bearer "),
    ] {
        let resp = f.send(Request {
            method: "GET",
            path: "/v1/info",
            query: "",
            authorization: auth,
            body: b"",
        });
        assert_eq!(resp.status, 401, "{auth:?}");
        assert_eq!(resp.body, "", "{auth:?}");
    }
    let resp = f.send(Request {
        method: "GET",
        path: "/nope",
        query: "",
        authorization: None,
        body: b"",
    });
    assert_eq!(resp.status, 401);
    assert_eq!(resp.body, "");

    // Auth is decided before the body size and the method.
    let big = vec![b'x'; 5000];
    let resp = f.send(Request {
        method: "POST",
        path: "/v1/terms",
        query: "",
        authorization: None,
        body: &big,
    });
    assert_eq!((resp.status, resp.body.as_str()), (401, ""));
    let resp = f.send(Request {
        method: "POST",
        path: "/v1/info",
        query: "",
        authorization: Some("Bearer wrong"),
        body: b"",
    });
    assert_eq!((resp.status, resp.body.as_str()), (401, ""));
}

#[test]
fn bearer_scheme_is_case_insensitive_token_is_not() {
    let f = Fixture::new();
    for (auth, status) in [
        ("bearer t0k", 200),
        ("BEARER t0k", 200),
        ("BeArEr t0k", 200),
        ("Bearer T0K", 401),
        ("Bearert0k", 401),
    ] {
        let resp = f.send(Request {
            method: "GET",
            path: "/v1/info",
            query: "",
            authorization: Some(auth),
            body: b"",
        });
        assert_eq!(resp.status, status, "{auth:?}");
    }
}

#[test]
fn info_reports_host_home_and_count() {
    let f = Fixture::new();
    let resp = f.get("/v1/info");
    assert_eq!(resp.status, 200);
    let raw: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(raw["protocol"], json!(1));
    let env: Envelope<Info> = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(env.protocol, 1);
    assert_eq!(env.now, NOW);
    assert_eq!(env.body.host, "box");
    assert_eq!(env.body.home, "/home/u");
    assert_eq!(env.body.sessions, 2);
    assert!(!env.body.hound_version.is_empty());
}

#[test]
fn sessions_lists_metadata_without_text() {
    let f = Fixture::new();
    let resp = f.get("/v1/sessions");
    assert_eq!(resp.status, 200);
    for text in CONVERSATION {
        assert!(!resp.body.contains(text), "body leaks {text:?}");
    }
    let env: Envelope<Sessions> = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(env.protocol, 1);
    assert_eq!(env.now, NOW);
    let sessions = env.body.sessions;
    assert_eq!(sessions.len(), 2);
    let s1 = sessions.iter().find(|s| s.title == "Depot alerts").unwrap();
    assert_eq!(s1.harness, "claude");
    assert_eq!(s1.id, S1);
    assert_eq!(s1.resume_cwd, f.s1_cwd);
    assert!(
        s1.main_file
            .as_deref()
            .unwrap()
            .ends_with(&format!("{S1}.jsonl"))
    );
    assert!(s1.cwd_exists);
    assert!(s1.last_change >= s1.updated_at);
}

/// An untitled session's title is its first prompt, collapsed to one line and
/// cut to 80 characters; nothing else of its conversation is returned.
#[test]
fn sessions_untitled_title_is_bounded_first_prompt() {
    let f = Fixture::new();
    f.add_untitled();
    let resp = f.get("/v1/sessions");
    assert_eq!(resp.status, 200);
    let one_line = S3_PROMPT.split_whitespace().collect::<Vec<_>>().join(" ");
    let expected: String = one_line.chars().take(80).collect();
    let rest: String = one_line.chars().skip(80).collect();
    assert!(rest.contains("BARNOWLTAIL"));
    let env: Envelope<Sessions> = serde_json::from_str(&resp.body).unwrap();
    let s3 = env.body.sessions.iter().find(|s| s.id == S3).unwrap();
    assert_eq!(s3.title, expected);
    assert!(!s3.title.contains('\n'));
    for text in S3_LATER {
        assert!(!resp.body.contains(text), "body leaks {text:?}");
    }
    assert!(!resp.body.contains("BARNOWLTAIL"));
    assert!(!resp.body.contains(rest.trim()));
}

#[test]
fn sessions_cwd_exists_false_for_missing_directory() {
    let f = Fixture::new();
    let sessions = f.sessions();
    let s2 = sessions.iter().find(|s| s.id == S2).unwrap();
    assert_eq!(s2.resume_cwd, f.s2_cwd);
    assert!(!s2.cwd_exists);
}

#[test]
fn terms_returns_matches_per_term_in_order() {
    let f = Fixture::new();
    let resp = f.terms(&json!({
        "terms": [
            {"term": "signal", "mode": "whole"},
            {"term": "river", "mode": "prefix"},
            {"term": "zzzz", "mode": "whole"},
        ],
        "include_subagents": true,
    }));
    let terms: Vec<&str> = resp.terms.iter().map(|t| t.term.as_str()).collect();
    assert_eq!(terms, ["signal", "river", "zzzz"]);
    let s1 = resp.terms[0].matches.iter().find(|m| m.id == S1).unwrap();
    assert_eq!(s1.harness, "claude");
    assert!(s1.snippet.contains("Signal"));
    assert!(!s1.from_subagent);
    assert!(resp.terms[1].matches.iter().any(|m| m.id == S2));
    assert!(resp.terms[2].matches.is_empty());
}

#[test]
fn terms_respects_include_subagents() {
    let f = Fixture::new();
    let with = f.terms(&json!({
        "terms": [{"term": "nginx", "mode": "whole"}],
        "include_subagents": true,
    }));
    let hit = with.terms[0].matches.iter().find(|m| m.id == S1).unwrap();
    assert!(hit.from_subagent);
    let without = f.terms(&json!({
        "terms": [{"term": "nginx", "mode": "whole"}],
        "include_subagents": false,
    }));
    assert!(without.terms[0].matches.is_empty());
}

#[test]
fn terms_limits() {
    let f = Fixture::new();
    assert_eq!(MAX_TERMS, 8);
    assert_eq!(MAX_TERM_CHARS, 64);
    assert_eq!(MAX_BODY, 4096);

    let nine: Vec<Value> = (0..9)
        .map(|i| json!({"term": format!("ninthterm{i}"), "mode": "whole"}))
        .collect();
    let resp = f.post(
        "/v1/terms",
        &json!({"terms": nine, "include_subagents": true}).to_string(),
    );
    assert_bad_request(&resp, "ninthterm");

    // 64 characters (multi-byte) is fine; 65 is not.
    let ok = "é".repeat(64);
    f.terms(&json!({"terms": [{"term": ok, "mode": "whole"}], "include_subagents": true}));
    let long = "q".repeat(65);
    let resp = f.post(
        "/v1/terms",
        &json!({"terms": [{"term": long, "mode": "whole"}], "include_subagents": true}).to_string(),
    );
    assert_bad_request(&resp, &long);

    let mut big = json!({"terms": [{"term": "signal", "mode": "whole"}], "include_subagents": true, "pad": ""})
        .to_string();
    let pad = 4097 - big.len();
    big = big.replace(r#""pad":"""#, &format!(r#""pad":"{}""#, "p".repeat(pad)));
    assert_eq!(big.len(), 4097);
    let resp = f.post("/v1/terms", &big);
    assert_bad_request(&resp, "pppppppp");

    let resp = f.post("/v1/terms", "{not json SECRETWORD");
    assert_bad_request(&resp, "SECRETWORD");

    let resp = f.post(
        "/v1/terms",
        r#"{"terms":[{"term":"x","mode":"fuzzyWORD"}],"include_subagents":true}"#,
    );
    assert_bad_request(&resp, "fuzzyWORD");
}

#[test]
fn session_returns_fresh_facts() {
    let f = Fixture::new();
    let main = main_path(&f.root, S1);
    // Back-date the file so the append below moves its mtime forward.
    let old = UNIX_EPOCH + Duration::from_secs((T1_SECS + 600) as u64);
    File::options()
        .write(true)
        .open(&main)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let path = format!("/v1/session?harness=claude&id={S1}");
    let resp = f.get(&path);
    assert_eq!(resp.status, 200, "{}", resp.body);
    let before: Envelope<SessionFacts> = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(before.now, NOW);
    assert_eq!(before.body.last_change, T1_SECS + 600);
    assert!(before.body.cwd_exists);

    append_raw(&main, "\n");
    let resp = f.get(&path);
    assert_eq!(resp.status, 200);
    let after: Envelope<SessionFacts> = serde_json::from_str(&resp.body).unwrap();
    assert!(after.body.last_change > before.body.last_change);

    // Percent-encoded IDs decode.
    let encoded = S1.replace('-', "%2D");
    let resp = f.get(&format!("/v1/session?harness=claude&id={encoded}"));
    assert_eq!(resp.status, 200, "{}", resp.body);

    let resp = f.get(&format!("/v1/session?harness=claude&id={S2}"));
    let facts: Envelope<SessionFacts> = serde_json::from_str(&resp.body).unwrap();
    assert!(!facts.body.cwd_exists);

    let resp = f.get("/v1/session?harness=claude&id=nosuchid");
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body, "");
    assert_bad_request(&f.get("/v1/session?harness=claude"), "claude");
    assert_bad_request(&f.get(&format!("/v1/session?id={S1}")), S1);
    assert_bad_request(&f.get("/v1/session?harness=claude&id=%ZZbad"), "%ZZbad");
    assert_bad_request(&f.get("/v1/session?harness=claude&id=%+5bad"), "%+5bad");
    assert_bad_request(&f.get("/v1/session?harness=claude&id=bad%2"), "bad%2");
    let resp = f.get(&format!("/v1/session?harness=nope&id={S1}"));
    assert_eq!(resp.status, 404);
}

#[test]
fn routing_statuses() {
    let f = Fixture::new();
    let resp = f.post("/v1/info", "");
    assert_eq!((resp.status, resp.body.as_str()), (405, ""));
    let resp = f.get("/v1/terms");
    assert_eq!((resp.status, resp.body.as_str()), (405, ""));
    let resp = f.get("/v1/other");
    assert_eq!((resp.status, resp.body.as_str()), (404, ""));
}

fn sample_doc(harness: Harness, main: &Path) -> SessionDoc {
    SessionDoc {
        key: SessionKey {
            harness,
            id: "abc-123".to_string(),
        },
        title: "Depot alerts".to_string(),
        cwds: vec!["/home/u/a".to_string(), "/home/u/b".to_string()],
        resume_cwd: "/home/u/a".to_string(),
        started_at: 100,
        updated_at: 200,
        summary: Some("sum".to_string()),
        category: Some("dev".to_string()),
        tags: vec!["rust".to_string()],
        files: vec![
            FileState {
                path: main.to_path_buf(),
                subagent: false,
                size: 10,
                offset: 10,
                head_hash: 7,
            },
            FileState {
                path: main.with_extension("sub"),
                subagent: true,
                size: 5,
                offset: 5,
                head_hash: 9,
            },
        ],
        enriched_at_bytes: 15,
    }
}

#[test]
fn wire_session_roundtrip() {
    let main = PathBuf::from("/nonexistent/hound/abc-123.jsonl");
    let doc = sample_doc(Harness::Claude, &main);
    let wire = WireSession::from_doc(&doc);
    assert_eq!(wire.harness, "claude");
    assert_eq!(wire.main_file.as_deref(), main.to_str());
    assert_eq!(wire.last_change, 200);
    assert!(!wire.cwd_exists);
    let (back, facts) = wire.clone().into_doc().unwrap();
    assert_eq!(back.key, doc.key);
    assert_eq!(back.title, doc.title);
    assert_eq!(back.cwds, doc.cwds);
    assert_eq!(back.resume_cwd, doc.resume_cwd);
    assert_eq!(back.started_at, doc.started_at);
    assert_eq!(back.updated_at, doc.updated_at);
    assert_eq!(back.summary, doc.summary);
    assert_eq!(back.category, doc.category);
    assert_eq!(back.tags, doc.tags);
    let paths: Vec<&Path> = back.files.iter().map(|f| f.path.as_path()).collect();
    assert_eq!(paths, [main.as_path()]);
    assert_eq!(
        (
            back.files[0].size,
            back.files[0].offset,
            back.files[0].head_hash
        ),
        (0, 0, 0)
    );
    assert_eq!(back.enriched_at_bytes, 0);
    assert_eq!(
        facts,
        SessionFacts {
            last_change: 200,
            cwd_exists: false
        }
    );

    let none = WireSession {
        main_file: None,
        ..wire.clone()
    };
    assert!(none.into_doc().unwrap().0.files.is_empty());

    let codex = WireSession {
        harness: "codex".to_string(),
        ..wire
    };
    assert!(codex.into_doc().is_none());
}

#[test]
fn unknown_fields_are_ignored_on_deserialise() {
    let raw = r#"{"protocol":1,"now":5,"hound_version":"0.1.0","host":"h","home":"/home/u","sessions":3,"extra":{"x":1}}"#;
    let env: Envelope<Info> = serde_json::from_str(raw).unwrap();
    assert_eq!(
        env,
        Envelope {
            protocol: 1,
            now: 5,
            body: Info {
                hound_version: "0.1.0".to_string(),
                host: "h".to_string(),
                home: "/home/u".to_string(),
                sessions: 3,
            },
        }
    );
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn token_created_once_with_private_mode() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("a/b/token");
    let token = load_or_create_token(&path).unwrap();
    assert_eq!(token.len(), 64);
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir.path().join("a")), 0o700);
    assert_eq!(mode(&dir.path().join("a/b")), 0o700);
    assert_eq!(load_or_create_token(&path).unwrap(), token);
    assert_eq!(read_token(&path).unwrap(), token);

    // A second token differs (random, not fixed).
    let other = load_or_create_token(&dir.path().join("other")).unwrap();
    assert_ne!(other, token);
    // An existing parent directory keeps its mode.
    assert_eq!(mode(dir.path()), 0o755);
}

#[test]
fn token_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tok");
    fs::write(&path, "abc\n").unwrap();
    assert_eq!(read_token(&path).unwrap(), "abc");
    for content in ["  ", ""] {
        fs::write(&path, content).unwrap();
        let err = format!("{:#}", read_token(&path).unwrap_err());
        assert!(err.contains(path.to_str().unwrap()), "{err}");
    }
    let missing = dir.path().join("missing");
    let err = format!("{:#}", read_token(&missing).unwrap_err());
    assert!(err.contains(missing.to_str().unwrap()), "{err}");
}

#[test]
fn tokens_equal_is_exact() {
    assert!(tokens_equal("t0k", "t0k"));
    assert!(!tokens_equal("t0k", "t0kk"));
    assert!(!tokens_equal("t0kk", "t0k"));
    assert!(!tokens_equal("t0k", "t0x"));
    assert!(tokens_equal("", ""));
    assert!(!tokens_equal("", "a"));
}

#[test]
fn index_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ServeContext>();
}
