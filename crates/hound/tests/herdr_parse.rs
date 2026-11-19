use hound::herdr::{
    MachineHerdr, SocketConnection, check_output, create_tab_args, detect_herdr, exchange,
    fallback_focus_args, focus_commands, machine_args, machine_create_tab_args,
    machine_focus_commands, machine_start_agent_commands, one_line, pane_focus_request,
    parse_read_output, parse_snapshot, parse_tab_created, read_args, request_line,
    snapshot_commands, start_agent_args, validate_machine_label, with_time_limit,
};
use hound_core::host::LivePane;
use std::ffi::OsStr;
use std::io::{Cursor, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WORKSPACES: &str = include_str!("fixtures/herdr/workspace_list.json");
const TABS: &str = include_str!("fixtures/herdr/tab_list.json");
const PANES: &str = include_str!("fixtures/herdr/pane_list.json");
const AGENTS: &str = include_str!("fixtures/herdr/agent_list.json");

fn snapshot() -> Vec<LivePane> {
    parse_snapshot(WORKSPACES, TABS, PANES, AGENTS).unwrap()
}

fn pane<'a>(panes: &'a [LivePane], id: &str) -> &'a LivePane {
    panes.iter().find(|p| p.pane_id == id).unwrap()
}

#[test]
fn every_pane_is_returned() {
    let panes = snapshot();
    assert_eq!(panes.len(), 4);
    let ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
    assert_eq!(ids, ["w1:p1", "w1:p2", "w1:p3", "w2:p1"]);
}

#[test]
fn agent_pane_fields() {
    let panes = snapshot();
    let p = pane(&panes, "w1:p1");
    assert_eq!(p.kind.as_deref(), Some("claude"));
    assert_eq!(
        p.session_id.as_deref(),
        Some("0a1b2c3d-1111-4aaa-8bbb-000000000001")
    );
    assert_eq!(p.agent_name.as_deref(), Some("refactor"));
    assert_eq!(p.title, "Refactor the parser");
    assert_eq!(p.status, "idle");
    assert_eq!(p.workspace_id, "w1");
    assert_eq!(p.tab_id, "w1:t1");
    assert_eq!(p.workspace_label, "Alpha Project");
    assert_eq!(p.tab_label, "editor");
    assert_eq!(p.cwd, "/home/dev/alpha");
    assert!(!p.focused);
    assert_eq!(p.scrollback, None);
}

#[test]
fn unnamed_agent_has_no_name() {
    let panes = snapshot();
    let p = pane(&panes, "w1:p3");
    assert_eq!(p.kind.as_deref(), Some("claude"));
    assert_eq!(p.agent_name, None);
    assert_eq!(p.status, "done");
    assert_eq!(p.tab_label, "review");
}

#[test]
fn shell_pane_has_no_agent() {
    let panes = snapshot();
    let p = pane(&panes, "w1:p2");
    assert_eq!(p.kind, None);
    assert_eq!(p.session_id, None);
    assert_eq!(p.agent_name, None);
    assert_eq!(p.status, "");
    assert_eq!(p.title, "dev@box: ~/alpha");
    // Shares a tab with the named agent pane.
    assert_eq!(p.tab_id, "w1:t1");
    assert_eq!(p.tab_label, "editor");
}

#[test]
fn focused_flag() {
    let panes = snapshot();
    let focused: Vec<&str> = panes
        .iter()
        .filter(|p| p.focused)
        .map(|p| p.pane_id.as_str())
        .collect();
    assert_eq!(focused, ["w2:p1"]);
    assert_eq!(pane(&panes, "w2:p1").workspace_label, "Beta Notes");
}

#[test]
fn error_envelope_is_an_error() {
    let err = r#"{"id":"cli:x","error":{"code":"server_unavailable","message":"no server"}}"#;
    assert!(parse_snapshot(err, TABS, PANES, AGENTS).is_err());
    assert!(parse_snapshot(WORKSPACES, err, PANES, AGENTS).is_err());
    assert!(parse_snapshot(WORKSPACES, TABS, err, AGENTS).is_err());
    assert!(parse_snapshot(WORKSPACES, TABS, PANES, err).is_err());
    let msg = format!(
        "{:#}",
        parse_snapshot(WORKSPACES, TABS, err, AGENTS).unwrap_err()
    );
    assert!(msg.contains("no server"), "{msg}");
}

#[test]
fn unknown_extra_fields_ignored() {
    let agents = AGENTS.replacen(
        r#""agent": "claude","#,
        r#""new_field": 1, "agent": "claude","#,
        1,
    );
    assert_ne!(agents, AGENTS);
    let panes = parse_snapshot(WORKSPACES, TABS, PANES, &agents).unwrap();
    assert_eq!(panes.len(), 4);
    assert_eq!(
        pane(&panes, "w1:p1").agent_name.as_deref(),
        Some("refactor")
    );
}

#[test]
fn malformed_json_is_an_error() {
    assert!(parse_snapshot("not json", TABS, PANES, AGENTS).is_err());
}

#[test]
fn focus_switches_workspace_then_tab_for_any_pane() {
    let panes = snapshot();
    for id in ["w1:p1", "w1:p2"] {
        assert_eq!(
            focus_commands(pane(&panes, id)),
            vec![
                vec!["workspace", "focus", "w1"],
                vec!["tab", "focus", "w1:t1"]
            ]
        );
    }
}

#[test]
fn pane_focus_request_is_one_json_line() {
    let line = pane_focus_request("w1:p2");
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"id": "hound:pane.focus", "method": "pane.focus", "params": {"pane_id": "w1:p2"}})
    );
}

#[test]
fn fallback_focuses_agent_pane_through_cli() {
    let panes = snapshot();
    assert_eq!(
        fallback_focus_args(pane(&panes, "w1:p1")).unwrap(),
        ["agent", "focus", "w1:p1"]
    );
}

#[test]
fn fallback_cannot_focus_shell_pane() {
    let panes = snapshot();
    let err = fallback_focus_args(pane(&panes, "w1:p2")).unwrap_err();
    assert!(err.to_string().contains("w1:p2"), "{err}");
}

#[cfg(unix)]
/// A one-shot fake server on a Unix socket: checks the request line, sends `response`.
fn fake_server(
    response: &'static str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::thread::JoinHandle<String>,
) {
    use std::io::{BufRead, BufReader, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        stream.write_all(response.as_bytes()).unwrap();
        line
    });
    (dir, path, handle)
}

#[test]
#[cfg(unix)]
fn socket_call_returns_result() {
    let (_dir, path, server) =
        fake_server("{\"id\":\"hound:pane.focus\",\"result\":{\"type\":\"ok\"}}\n");
    let mut conn = SocketConnection::connect(&path).unwrap();
    let result = conn.call(&pane_focus_request("w1:p2")).unwrap();
    assert_eq!(result, serde_json::json!({"type": "ok"}));
    assert_eq!(server.join().unwrap(), pane_focus_request("w1:p2"));
}

#[test]
#[cfg(unix)]
fn socket_error_response_is_an_error() {
    let (_dir, path, _server) = fake_server(
        "{\"id\":\"hound:pane.focus\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"no pane w9:p9\"}}\n",
    );
    let mut conn = SocketConnection::connect(&path).unwrap();
    let err = conn.call(&pane_focus_request("w9:p9")).unwrap_err();
    assert!(format!("{err:#}").contains("no pane w9:p9"), "{err:#}");
}

#[test]
#[cfg(unix)]
fn socket_mismatched_id_is_an_error() {
    let (_dir, path, _server) = fake_server("{\"id\":\"other\",\"result\":{\"type\":\"ok\"}}\n");
    let mut conn = SocketConnection::connect(&path).unwrap();
    assert!(conn.call(&pane_focus_request("w1:p2")).is_err());
}

#[test]
#[cfg(unix)]
fn socket_closed_without_response_is_an_error() {
    let (_dir, path, _server) = fake_server("");
    let mut conn = SocketConnection::connect(&path).unwrap();
    assert!(conn.call(&pane_focus_request("w1:p2")).is_err());
}

#[test]
#[cfg(unix)]
fn missing_socket_does_not_connect() {
    let dir = tempfile::tempdir().unwrap();
    assert!(SocketConnection::connect(&dir.path().join("absent.sock")).is_err());
}

#[test]
fn checked_output_tolerates_non_envelopes() {
    assert!(check_output("").is_ok());
    assert!(check_output("focused w1:p1\n").is_ok());
    assert!(check_output("[1,2]").is_ok());
    assert!(check_output(r#"{"id":"x","result":{"type":"ok"}}"#).is_ok());
    assert!(
        check_output(r#"{"id":"x","error":{"code":"agent_not_found","message":"gone"}}"#).is_err()
    );
}

#[test]
fn null_error_is_no_error() {
    assert!(check_output(r#"{"id":"x","error":null}"#).is_ok());
    assert!(check_output(r#"{"id":"x","result":{"type":"ok"},"error":null}"#).is_ok());
    let json = r#"{"id":"x","error":null,"result":{"type":"tab_created",
        "tab":{"tab_id":"w1:t3","workspace_id":"w1","number":3,"label":"3","focused":true,"pane_count":1,"agent_status":"unknown"},
        "root_pane":{"pane_id":"w1:p5","tab_id":"w1:t3","workspace_id":"w1","terminal_id":"t","focused":true,"agent_status":"unknown","revision":0}}}"#;
    assert_eq!(parse_tab_created(json, "/p").unwrap().pane_id, "w1:p5");
}

#[test]
#[cfg(unix)]
fn socket_null_error_returns_result() {
    let (_dir, path, _server) =
        fake_server("{\"id\":\"hound:pane.focus\",\"error\":null,\"result\":{\"type\":\"ok\"}}\n");
    let mut conn = SocketConnection::connect(&path).unwrap();
    let result = conn.call(&pane_focus_request("w1:p2")).unwrap();
    assert_eq!(result, serde_json::json!({"type": "ok"}));
}

#[test]
fn time_limit_returns_the_result() {
    let value = with_time_limit(Duration::from_secs(5), || Ok(7)).unwrap();
    assert_eq!(value, 7);
}

#[test]
fn time_limit_gives_up_on_a_hung_call() {
    let started = Instant::now();
    let err = with_time_limit(Duration::from_millis(200), || {
        std::thread::sleep(Duration::from_secs(5));
        Ok(())
    })
    .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
}

#[test]
fn time_limit_passes_errors_through() {
    let err = with_time_limit(Duration::from_secs(5), || -> anyhow::Result<()> {
        anyhow::bail!("boom")
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
}

/// An in-memory stream: reads come from `response`, writes land in `request`.
struct Duplex {
    response: Cursor<Vec<u8>>,
    request: Vec<u8>,
}

impl Duplex {
    fn answering(response: &str) -> Duplex {
        Duplex {
            response: Cursor::new(response.as_bytes().to_vec()),
            request: Vec::new(),
        }
    }
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.response.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.request.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn request_response_logic_is_stream_generic() {
    let request = pane_focus_request("w1:p2");

    let mut ok = Duplex::answering("{\"id\":\"hound:pane.focus\",\"result\":{\"type\":\"ok\"}}\n");
    let result = exchange(&mut ok, &request).unwrap();
    assert_eq!(result, serde_json::json!({"type": "ok"}));
    assert_eq!(String::from_utf8(ok.request).unwrap(), request);

    let mut null_error = Duplex::answering(
        "{\"id\":\"hound:pane.focus\",\"error\":null,\"result\":{\"type\":\"ok\"}}\n",
    );
    let result = exchange(&mut null_error, &request).unwrap();
    assert_eq!(result, serde_json::json!({"type": "ok"}));

    let mut mismatched = Duplex::answering("{\"id\":\"other\",\"result\":{\"type\":\"ok\"}}\n");
    assert!(exchange(&mut mismatched, &request).is_err());

    let mut error = Duplex::answering(
        "{\"id\":\"hound:pane.focus\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"no pane w9:p9\"}}\n",
    );
    let err = exchange(&mut error, &pane_focus_request("w9:p9")).unwrap_err();
    assert!(format!("{err:#}").contains("no pane w9:p9"), "{err:#}");

    let mut empty = Duplex::answering("");
    assert!(exchange(&mut empty, &request).is_err());
}

/// Read-only transport check against the live herdr session: `workspace.list` over the
/// socket must agree with `herdr workspace list`. Run with `HOUND_LIVE_TESTS=1` and
/// `--ignored` inside herdr only.
#[test]
#[ignore]
fn live_socket_workspace_list_matches_cli() {
    if std::env::var("HOUND_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let path = std::env::var_os("HERDR_SOCKET_PATH").expect("run inside herdr");
    let mut conn = SocketConnection::connect(std::path::Path::new(&path)).unwrap();
    let via_socket = conn
        .call(&request_line(
            "hound:check",
            "workspace.list",
            &serde_json::json!({}),
        ))
        .unwrap();
    let cli = std::process::Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .unwrap();
    assert!(cli.status.success());
    let via_cli: serde_json::Value = serde_json::from_slice(&cli.stdout).unwrap();
    let ids_labels = |v: &serde_json::Value| -> Vec<(String, String)> {
        v["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["workspace_id"].as_str().unwrap().into(),
                    w["label"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    assert_eq!(via_socket["type"], "workspace_list");
    assert!(!ids_labels(&via_socket).is_empty());
    assert_eq!(ids_labels(&via_socket), ids_labels(&via_cli["result"]));
}

#[test]
fn create_tab_arguments() {
    assert_eq!(
        create_tab_args("w1", "/home/dev/alpha"),
        [
            "tab",
            "create",
            "--workspace",
            "w1",
            "--cwd",
            "/home/dev/alpha",
            "--focus"
        ]
    );
}

#[test]
fn start_agent_arguments() {
    assert_eq!(
        start_agent_args("resume-1", "claude", "w1:p4", &[]),
        [
            "agent", "start", "resume-1", "--kind", "claude", "--pane", "w1:p4"
        ]
    );
    let args = vec!["--resume".to_string(), "abc".to_string()];
    assert_eq!(
        start_agent_args("resume-1", "claude", "w1:p4", &args),
        [
            "agent", "start", "resume-1", "--kind", "claude", "--pane", "w1:p4", "--", "--resume",
            "abc"
        ]
    );
}

#[test]
fn read_arguments() {
    assert_eq!(
        read_args("w1:p1", 200),
        [
            "pane",
            "read",
            "w1:p1",
            "--source",
            "recent-unwrapped",
            "--lines",
            "200"
        ]
    );
}

#[test]
fn read_output_json_or_plain() {
    let json = r#"{"id":"x","result":{"type":"pane_read","read":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1",
        "source":"recent_unwrapped","format":"text","text":"line one\nline two","revision":3,"truncated":false}}}"#;
    assert_eq!(parse_read_output(json).unwrap(), "line one\nline two");
    assert_eq!(parse_read_output("plain text\n").unwrap(), "plain text\n");
    // JSON that is not a response envelope is pane text, not an error.
    assert_eq!(
        parse_read_output(r#"{"key":"value"}"#).unwrap(),
        r#"{"key":"value"}"#
    );
    assert_eq!(parse_read_output("[1, 2]").unwrap(), "[1, 2]");
    assert!(
        parse_read_output(r#"{"id":"x","error":{"code":"pane_not_found","message":"gone"}}"#)
            .is_err()
    );
}

#[test]
fn tab_created_response_becomes_root_pane() {
    // Shape from the tab_created result in `herdr api schema --json`.
    let json = r#"{"id":"cli:tab:create","result":{"type":"tab_created",
        "tab":{"tab_id":"w1:t3","workspace_id":"w1","number":3,"label":"3","focused":true,"pane_count":1,"agent_status":"unknown"},
        "root_pane":{"pane_id":"w1:p5","tab_id":"w1:t3","workspace_id":"w1","terminal_id":"term_x","focused":true,
            "agent_status":"unknown","cwd":"/home/dev/alpha","revision":0,"terminal_title":null}}}"#;
    let p = parse_tab_created(json, "/fallback").unwrap();
    assert_eq!(
        p,
        LivePane {
            pane_id: "w1:p5".into(),
            tab_id: "w1:t3".into(),
            workspace_id: "w1".into(),
            workspace_label: String::new(),
            tab_label: "3".into(),
            agent_name: None,
            kind: None,
            session_id: None,
            title: String::new(),
            cwd: "/home/dev/alpha".into(),
            status: String::new(),
            focused: true,
            scrollback: None,
        }
    );
}

#[test]
fn tab_created_without_cwd_uses_requested_cwd() {
    let json = r#"{"id":"x","result":{"type":"tab_created",
        "tab":{"tab_id":"w1:t3","workspace_id":"w1","number":3,"label":"3","focused":true,"pane_count":1,"agent_status":"unknown"},
        "root_pane":{"pane_id":"w1:p5","tab_id":"w1:t3","workspace_id":"w1","terminal_id":"t","focused":true,"agent_status":"unknown","revision":0}}}"#;
    assert_eq!(
        parse_tab_created(json, "/fallback").unwrap().cwd,
        "/fallback"
    );
}

fn os(s: &str) -> Option<&OsStr> {
    Some(OsStr::new(s))
}

/// `detect_herdr` with a PATH lookup that finds `/usr/bin/herdr`.
fn detect(env: Option<&OsStr>, socket: Option<&OsStr>, bin: Option<&OsStr>) -> Option<PathBuf> {
    detect_herdr(env, socket, bin, |name| {
        assert_eq!(name, "herdr");
        Some(PathBuf::from("/usr/bin/herdr"))
    })
}

const ON_PATH: &str = "/usr/bin/herdr";

#[test]
fn detect_with_herdr_env_only() {
    assert_eq!(detect(os("1"), None, None), Some(PathBuf::from(ON_PATH)));
}

#[test]
fn detect_with_socket_path_only() {
    assert_eq!(
        detect(None, os("/run/herdr.sock"), None),
        Some(PathBuf::from(ON_PATH))
    );
}

#[test]
fn detect_with_both() {
    assert_eq!(
        detect(os("1"), os("/run/herdr.sock"), None),
        Some(PathBuf::from(ON_PATH))
    );
}

#[test]
fn detect_with_neither() {
    assert_eq!(detect(None, None, os("/opt/herdr")), None);
}

#[test]
fn detect_ignores_empty_and_other_values() {
    assert_eq!(detect(os(""), os(""), None), None);
    assert_eq!(detect(os("0"), None, None), None);
    assert_eq!(detect(os(""), None, None), None);
    assert_eq!(detect(None, os(""), os("/opt/herdr")), None);
}

#[test]
fn detect_prefers_herdr_bin_path() {
    let found = detect_herdr(os("1"), None, os("/opt/herdr/bin/herdr"), |_| {
        panic!("PATH must not be searched when HERDR_BIN_PATH is set")
    });
    assert_eq!(found, Some(PathBuf::from("/opt/herdr/bin/herdr")));
}

#[test]
fn detect_falls_back_to_path_when_bin_path_is_empty() {
    assert_eq!(detect(os("1"), None, os("")), Some(PathBuf::from(ON_PATH)));
}

#[test]
fn detect_needs_a_binary() {
    assert_eq!(
        detect_herdr(os("1"), os("/run/herdr.sock"), None, |_| None),
        None
    );
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// A pane as a remote machine's herdr reports it.
fn remote_pane(kind: Option<&str>, agent_name: Option<&str>) -> LivePane {
    LivePane {
        pane_id: "w2:p1".into(),
        tab_id: "w2:t3".into(),
        workspace_id: "w2".into(),
        workspace_label: "Lab".into(),
        tab_label: "work".into(),
        agent_name: agent_name.map(str::to_string),
        kind: kind.map(str::to_string),
        session_id: None,
        title: String::new(),
        cwd: "/srv/lab".into(),
        status: String::new(),
        focused: false,
        scrollback: None,
    }
}

#[test]
fn machine_args_prefix() {
    assert_eq!(
        machine_args("homelab", &strings(&["agent", "list"])),
        ["--machine", "homelab", "agent", "list"]
    );
}

#[test]
fn machine_focus_prefers_agent_name() {
    let p = remote_pane(Some("claude"), Some("reviewer"));
    assert_eq!(
        machine_focus_commands(&p).unwrap(),
        vec![vec!["agent", "focus", "reviewer"]]
    );
}

#[test]
fn machine_focus_unnamed_agent_uses_pane_id() {
    let p = remote_pane(Some("claude"), None);
    assert_eq!(
        machine_focus_commands(&p).unwrap(),
        vec![vec!["agent", "focus", "w2:p1"]]
    );
}

#[test]
fn machine_focus_shell_pane_uses_workspace_and_tab() {
    let p = remote_pane(None, None);
    assert_eq!(
        machine_focus_commands(&p).unwrap(),
        vec![
            vec!["workspace", "focus", "w2"],
            vec!["tab", "focus", "w2:t3"]
        ]
    );
}

#[test]
fn machine_label_with_spaces_is_one_argument() {
    let args = machine_args("my lab", &strings(&["agent", "list"]));
    assert_eq!(args[1], "my lab");
    assert_eq!(args.len(), 4);
}

#[test]
fn opaque_long_ids_parse() {
    let panes = parse_snapshot(
        include_str!("fixtures/herdr/long_ids/workspace_list.json"),
        include_str!("fixtures/herdr/long_ids/tab_list.json"),
        include_str!("fixtures/herdr/long_ids/pane_list.json"),
        include_str!("fixtures/herdr/long_ids/agent_list.json"),
    )
    .unwrap();
    let ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "w0123456789abcde:p1",
            "w0123456789abcde:p2",
            "w0123456789abcde:p3",
            "w0c3e9a5d27b184:p1"
        ]
    );
    let p = pane(&panes, "w0123456789abcde:p1");
    assert_eq!(p.workspace_id, "w0123456789abcde");
    assert_eq!(p.tab_id, "w0123456789abcde:t1");
    assert_eq!(p.workspace_label, "Alpha Project");
    assert_eq!(p.tab_label, "editor");
    assert_eq!(p.agent_name.as_deref(), Some("refactor"));
    let shell = pane(&panes, "w0123456789abcde:p2");
    assert_eq!(shell.tab_label, "editor");
    assert_eq!(shell.kind, None);
    let unnamed = pane(&panes, "w0123456789abcde:p3");
    assert_eq!(unnamed.tab_label, "review");
    assert_eq!(unnamed.agent_name, None);
    let beta = pane(&panes, "w0c3e9a5d27b184:p1");
    assert_eq!(beta.workspace_label, "Beta Notes");
    assert_eq!(beta.tab_label, "notes");
    assert!(beta.focused);
    // Long-form ids go through the focus builders untouched.
    assert_eq!(
        machine_focus_commands(shell).unwrap(),
        vec![
            vec!["workspace", "focus", "w0123456789abcde"],
            vec!["tab", "focus", "w0123456789abcde:t1"]
        ]
    );
}

#[test]
fn snapshot_runs_the_four_list_commands() {
    assert_eq!(
        snapshot_commands(),
        [
            vec!["workspace", "list"],
            vec!["tab", "list"],
            vec!["pane", "list"],
            vec!["agent", "list"]
        ]
    );
}

#[test]
fn machine_start_agent_starts_then_focuses_by_name() {
    let args = strings(&["--resume", "abc"]);
    assert_eq!(
        machine_start_agent_commands("resume-1", "claude", "w2:p7", &args).unwrap(),
        [
            strings(&[
                "agent", "start", "resume-1", "--kind", "claude", "--pane", "w2:p7", "--",
                "--resume", "abc"
            ]),
            strings(&["agent", "focus", "resume-1"]),
        ]
    );
}

#[test]
fn machine_host_prefixes_every_command() {
    let host = MachineHerdr::new(PathBuf::from("/usr/bin/herdr"), "my lab".into()).unwrap();
    assert_eq!(host.label(), "my lab");
    assert_eq!(
        host.command_args(&create_tab_args("w2", "/srv/lab")),
        [
            "--machine",
            "my lab",
            "tab",
            "create",
            "--workspace",
            "w2",
            "--cwd",
            "/srv/lab",
            "--focus"
        ]
    );
}

#[test]
fn machine_scrollback_is_unavailable() {
    use hound_core::host::Host;
    // `read_scrollback` fails before running anything, so a bogus binary is never spawned.
    let host = MachineHerdr::new(PathBuf::from("/nonexistent/herdr"), "homelab".into()).unwrap();
    let err = host.read_scrollback("w2:p1", 10).unwrap_err();
    assert!(
        err.to_string()
            .contains("not available on a remote machine"),
        "{err}"
    );
}

#[test]
fn one_line_collapses_whitespace() {
    assert_eq!(
        one_line("connection lost\n  while forwarding\r\n"),
        "connection lost while forwarding"
    );
}

#[test]
fn bad_machine_labels_are_rejected() {
    for label in [
        "", " ", "\t", "-x", "--help", " lab", "lab ", "lab\n", "la\u{7}b",
    ] {
        assert!(validate_machine_label(label).is_err(), "{label:?}");
        assert!(
            MachineHerdr::new(PathBuf::from("/nonexistent/herdr"), label.into()).is_err(),
            "{label:?}"
        );
    }
    for label in ["homelab", "my lab", "mac-mini", "lab-"] {
        assert!(validate_machine_label(label).is_ok(), "{label:?}");
    }
}

#[test]
fn bad_remote_targets_are_not_commands() {
    let mut named = remote_pane(Some("claude"), Some("--help"));
    let err = machine_focus_commands(&named).unwrap_err();
    assert!(err.to_string().contains("agent name"), "{err}");
    named.agent_name = Some(String::new());
    assert!(machine_focus_commands(&named).is_err());

    let mut unnamed = remote_pane(Some("claude"), None);
    unnamed.pane_id = "-x".into();
    let err = machine_focus_commands(&unnamed).unwrap_err();
    assert!(err.to_string().contains("pane ID"), "{err}");

    let mut shell = remote_pane(None, None);
    shell.workspace_id = String::new();
    let err = machine_focus_commands(&shell).unwrap_err();
    assert!(err.to_string().contains("workspace ID"), "{err}");
    let mut shell = remote_pane(None, None);
    shell.tab_id = "--focus".into();
    let err = machine_focus_commands(&shell).unwrap_err();
    assert!(err.to_string().contains("tab ID"), "{err}");

    assert!(machine_create_tab_args("-w", "/srv").is_err());
    assert!(machine_create_tab_args("", "/srv").is_err());
    assert!(machine_create_tab_args("w2", "--focus").is_err());
    assert!(machine_start_agent_commands("-n", "claude", "w2:p1", &[]).is_err());
    assert!(machine_start_agent_commands("n", "claude", "", &[]).is_err());
    assert!(machine_start_agent_commands("n", "--kind", "w2:p1", &[]).is_err());
    // Agent arguments follow herdr's own `--`, so a leading '-' there is expected.
    assert!(
        machine_start_agent_commands("n", "claude", "w2:p1", &strings(&["--resume", "x"])).is_ok()
    );
}

// --- A recording stub standing in for the herdr binary (never a real herdr). ---

/// Serialises the stub tests: each writes an executable then spawns it, and a fork from
/// another test while the script is open for writing would make exec fail (ETXTBSY).
static STUB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TAB_CREATED: &str = r#"{"id":"cli:tab:create","result":{"type":"tab_created",
    "tab":{"tab_id":"w2:t9","workspace_id":"w2","number":9,"label":"9","focused":true,"pane_count":1,"agent_status":"unknown"},
    "root_pane":{"pane_id":"w2:p9","tab_id":"w2:t9","workspace_id":"w2","terminal_id":"t","focused":true,"agent_status":"unknown","revision":0}}}"#;

/// A tempdir holding `herdr`, a shell script that appends each invocation's arguments to
/// `calls` (tab-separated, one invocation per line) and prints `<arg3>_<arg4>.json` from the
/// same directory when present, else an `ok` envelope. With `fail_focus`, `agent focus`
/// exits 1 with a message on stderr.
fn stub(fail_focus: bool) -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fail = if fail_focus {
        "if [ \"$3 $4\" = \"agent focus\" ]; then printf 'connection lost\\nto homelab\\n' >&2; exit 1; fi\n"
    } else {
        ""
    };
    let script = format!(
        "#!/bin/sh\n\
         d=$(dirname \"$0\")\n\
         printf '%s\\t' \"$@\" >> \"$d/calls\"\n\
         printf '\\n' >> \"$d/calls\"\n\
         {fail}\
         if [ -f \"$d/$3_$4.json\" ]; then cat \"$d/$3_$4.json\"; \
         else echo '{{\"id\":\"stub\",\"result\":{{\"type\":\"ok\"}}}}'; fi\n"
    );
    let bin = dir.path().join("herdr");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (name, body) in [
        ("workspace_list", WORKSPACES),
        ("tab_list", TABS),
        ("pane_list", PANES),
        ("agent_list", AGENTS),
        ("tab_create", TAB_CREATED),
    ] {
        std::fs::write(dir.path().join(format!("{name}.json")), body).unwrap();
    }
    (dir, bin)
}

/// The argument vectors the stub in `dir` was run with, in order.
fn calls(dir: &tempfile::TempDir) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.path().join("calls"))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let mut args: Vec<String> = line.split('\t').map(str::to_string).collect();
            assert_eq!(args.pop().as_deref(), Some(""), "line ends with a tab");
            args
        })
        .collect()
}

fn prefixed(label: &str, parts: &[&str]) -> Vec<String> {
    machine_args(label, &strings(parts))
}

#[test]
fn stub_records_executed_vectors() {
    use hound_core::host::Host;
    let _guard = STUB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, bin) = stub(false);
    let host = MachineHerdr::new(bin, "my lab".into()).unwrap();

    let panes = host.snapshot().unwrap();
    assert_eq!(panes.len(), 4);
    host.focus(pane(&panes, "w1:p1")).unwrap(); // named agent
    host.focus(pane(&panes, "w1:p3")).unwrap(); // unnamed agent
    host.focus(pane(&panes, "w1:p2")).unwrap(); // shell
    let created = host.create_tab("w2", "/srv/lab").unwrap();
    assert_eq!(created.pane_id, "w2:p9");
    host.start_agent(
        "resume-1",
        "claude",
        "w2:p9",
        &strings(&["--resume", "abc"]),
    )
    .unwrap();

    let l = "my lab";
    assert_eq!(
        calls(&dir),
        [
            prefixed(l, &["workspace", "list"]),
            prefixed(l, &["tab", "list"]),
            prefixed(l, &["pane", "list"]),
            prefixed(l, &["agent", "list"]),
            prefixed(l, &["agent", "focus", "refactor"]),
            prefixed(l, &["agent", "focus", "w1:p3"]),
            prefixed(l, &["workspace", "focus", "w1"]),
            prefixed(l, &["tab", "focus", "w1:t1"]),
            prefixed(
                l,
                &[
                    "tab",
                    "create",
                    "--workspace",
                    "w2",
                    "--cwd",
                    "/srv/lab",
                    "--focus"
                ]
            ),
            prefixed(
                l,
                &[
                    "agent", "start", "resume-1", "--kind", "claude", "--pane", "w2:p9", "--",
                    "--resume", "abc"
                ]
            ),
            prefixed(l, &["agent", "focus", "resume-1"]),
        ]
    );
    for call in calls(&dir) {
        assert_eq!(call[..2], ["--machine", "my lab"], "{call:?}");
    }
}

#[test]
fn stub_start_agent_ok_when_trailing_focus_fails() {
    use hound_core::host::Host;
    let _guard = STUB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, bin) = stub(true);
    let host = MachineHerdr::new(bin, "homelab".into()).unwrap();
    host.start_agent("resume-1", "claude", "w2:p9", &[])
        .unwrap();
    let recorded = calls(&dir);
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        recorded[1],
        prefixed("homelab", &["agent", "focus", "resume-1"])
    );
    // The same failure from `focus` is an error, on one line, carrying herdr's message.
    let err = host
        .focus(&remote_pane(Some("claude"), Some("reviewer")))
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("connection lost to homelab"), "{msg}");
    assert!(!msg.contains('\n'), "{msg}");
    assert!(!msg.to_lowercase().contains("retry"), "{msg}");
}

#[test]
fn stub_bad_remote_target_runs_nothing() {
    use hound_core::host::Host;
    let _guard = STUB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, bin) = stub(false);
    let host = MachineHerdr::new(bin, "homelab".into()).unwrap();
    let err = host
        .focus(&remote_pane(Some("claude"), Some("--help")))
        .unwrap_err();
    assert!(err.to_string().contains("agent name"), "{err}");
    assert!(host.create_tab("-w", "/srv").is_err());
    assert!(host.start_agent("-n", "claude", "w2:p1", &[]).is_err());
    assert!(calls(&dir).is_empty());
}
