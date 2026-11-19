//! The herdr host: live panes from the `herdr` CLI, and focus / new tab / agent start through it.
//!
//! Every command's argument vector and socket request is built by a pure function so it can be
//! tested without running herdr; `CliHerdr` only spawns the process (or writes to the socket)
//! and parses the JSON. Focusing a pane by ID needs the socket API's `pane.focus`, which the
//! CLI does not expose.
//!
//! `MachineHerdr` drives another computer the local herdr is attached to, running the same
//! commands as `herdr --machine <label> …`. It never uses the socket.

use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use hound_core::host::{Host, LivePane};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::program::resolve_on_this_machine;

mod socket;

pub use socket::{SocketConnection, exchange, request_line, with_time_limit};

/// Joins the JSON output of `herdr workspace list`, `herdr tab list`, `herdr pane list` and
/// `herdr agent list` (all workspaces) into one `LivePane` per pane, in pane-list order.
pub fn parse_snapshot(
    workspaces: &str,
    tabs: &str,
    panes: &str,
    agents: &str,
) -> anyhow::Result<Vec<LivePane>> {
    let workspaces: WorkspaceList = parse_result(workspaces).context("herdr workspace list")?;
    let tabs: TabList = parse_result(tabs).context("herdr tab list")?;
    let panes: PaneList = parse_result(panes).context("herdr pane list")?;
    let agents: AgentList = parse_result(agents).context("herdr agent list")?;

    let workspace_labels: HashMap<String, String> = workspaces
        .workspaces
        .into_iter()
        .map(|w| (w.workspace_id, w.label))
        .collect();
    let tab_labels: HashMap<String, String> =
        tabs.tabs.into_iter().map(|t| (t.tab_id, t.label)).collect();
    let agent_names: HashMap<String, String> = agents
        .agents
        .into_iter()
        .filter_map(|a| a.name.map(|n| (a.pane_id, n)))
        .collect();

    Ok(panes
        .panes
        .into_iter()
        .map(|p| {
            let workspace_label = workspace_labels
                .get(&p.workspace_id)
                .cloned()
                .unwrap_or_default();
            let tab_label = tab_labels.get(&p.tab_id).cloned().unwrap_or_default();
            let agent_name = agent_names.get(&p.pane_id).cloned();
            p.into_live_pane(workspace_label, tab_label, agent_name)
        })
        .collect())
}

/// Parses the JSON output of `herdr tab create` into the new tab's root pane. `requested_cwd`
/// fills `cwd` when herdr does not report one. herdr does not return the workspace label.
pub fn parse_tab_created(json: &str, requested_cwd: &str) -> anyhow::Result<LivePane> {
    let created: TabCreated = parse_result(json).context("herdr tab create")?;
    let mut pane = created
        .root_pane
        .into_live_pane(String::new(), created.tab.label, None);
    if pane.cwd.is_empty() {
        pane.cwd = requested_cwd.to_string();
    }
    Ok(pane)
}

/// Extracts the text from `herdr pane read` output: the `pane_read` result when the output is
/// a response envelope (a JSON object with `result` or `error`), otherwise the output itself.
pub fn parse_read_output(out: &str) -> anyhow::Result<String> {
    match envelope(out) {
        Some(value) => {
            let read: PaneRead = result_from_value(value).context("herdr pane read")?;
            Ok(read.read.text)
        }
        None => Ok(out.to_string()),
    }
}

/// The CLI commands run before focusing a pane by ID: its workspace, then its tab.
pub fn focus_commands(pane: &LivePane) -> Vec<Vec<String>> {
    vec![
        strings(&["workspace", "focus", &pane.workspace_id]),
        strings(&["tab", "focus", &pane.tab_id]),
    ]
}

/// The CLI command that focuses `pane` when herdr's socket is unavailable. Only an agent pane
/// can be focused by ID through the CLI (`agent focus`; `pane focus` only moves to a
/// neighbour), so a shell pane is an error.
pub fn fallback_focus_args(pane: &LivePane) -> anyhow::Result<Vec<String>> {
    if pane.kind.is_some() {
        Ok(agent_focus_args(&pane.pane_id))
    } else {
        bail!(
            "could not focus pane {} by ID: the herdr socket is unavailable and the CLI can only focus agent panes",
            pane.pane_id
        )
    }
}

/// `agent focus <target>`, where `target` is an agent name or a pane ID.
pub fn agent_focus_args(target: &str) -> Vec<String> {
    strings(&["agent", "focus", target])
}

/// The four list commands whose outputs [`parse_snapshot`] joins, in its argument order.
pub fn snapshot_commands() -> [Vec<String>; 4] {
    [
        strings(&["workspace", "list"]),
        strings(&["tab", "list"]),
        strings(&["pane", "list"]),
        strings(&["agent", "list"]),
    ]
}

/// Whether `label` can be given to `--machine`: not empty, no leading or trailing whitespace,
/// not starting with `-` (it would read as a flag), and no control characters. Inner spaces
/// are allowed. A [`MachineHerdr`] cannot be built with a label that fails this.
pub fn validate_machine_label(label: &str) -> anyhow::Result<()> {
    if label.trim().is_empty() {
        bail!("machine label is empty");
    }
    if label.trim() != label {
        bail!("machine label {label:?} has leading or trailing whitespace");
    }
    if label.starts_with('-') {
        bail!("machine label {label:?} starts with '-'");
    }
    if label.chars().any(char::is_control) {
        bail!("machine label {label:?} contains a control character");
    }
    Ok(())
}

/// Whether `value`, reported by a remote machine, can be passed to herdr as an argument: not
/// empty, not starting with `-`, no control characters. herdr's focus commands take their
/// target as a bare positional with no `--` separator, so a bad value is refused, not escaped.
/// `what` names the value in the error, e.g. `"pane ID"`.
fn check_remote_value(what: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        bail!("remote {what} is empty");
    }
    if value.starts_with('-') {
        bail!("remote {what} {value:?} starts with '-'");
    }
    if value.chars().any(char::is_control) {
        bail!("remote {what} {value:?} contains a control character");
    }
    Ok(())
}

/// `args` forwarded to the attached machine `label`: `--machine <label> <args…>`. The label
/// stays one argument whatever it contains; [`validate_machine_label`] decides which labels
/// are allowed.
pub fn machine_args(label: &str, args: &[String]) -> Vec<String> {
    let mut v = strings(&["--machine", label]);
    v.extend(args.iter().cloned());
    v
}

/// The commands (before the machine prefix) that focus `pane` on a remote machine. An agent
/// pane is focused by `agent focus`, by name when it has one, which also moves the client's
/// view to it; any other pane by its workspace, then its tab. An ID or name that could be
/// read as a flag (or is empty) is an error.
pub fn machine_focus_commands(pane: &LivePane) -> anyhow::Result<Vec<Vec<String>>> {
    if pane.kind.is_some() {
        match pane.agent_name.as_deref() {
            Some(name) => check_remote_value("agent name", name)?,
            None => check_remote_value("pane ID", &pane.pane_id)?,
        }
        let target = pane.agent_name.as_deref().unwrap_or(&pane.pane_id);
        Ok(vec![agent_focus_args(target)])
    } else {
        check_remote_value("workspace ID", &pane.workspace_id)?;
        check_remote_value("tab ID", &pane.tab_id)?;
        Ok(focus_commands(pane))
    }
}

/// `tab create` (before the machine prefix) for a remote machine; a workspace ID or cwd that
/// could be read as a flag (or is empty) is an error.
pub fn machine_create_tab_args(workspace_id: &str, cwd: &str) -> anyhow::Result<Vec<String>> {
    check_remote_value("workspace ID", workspace_id)?;
    check_remote_value("working directory", cwd)?;
    Ok(create_tab_args(workspace_id, cwd))
}

/// The commands (before the machine prefix) that start an agent on a remote machine: `agent
/// start …`, then `agent focus <name>` so the client's view follows it. A name, kind or pane
/// ID that could be read as a flag (or is empty) is an error. `args` follow herdr's own `--`
/// and are passed through as they are.
pub fn machine_start_agent_commands(
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> anyhow::Result<[Vec<String>; 2]> {
    check_remote_value("agent name", name)?;
    check_remote_value("agent kind", kind)?;
    check_remote_value("pane ID", pane_id)?;
    Ok([
        start_agent_args(name, kind, pane_id, args),
        agent_focus_args(name),
    ])
}

/// `text` with every run of whitespace, line breaks included, collapsed to one space.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `pane.focus` socket request for `pane_id`, as one newline-terminated line.
pub fn pane_focus_request(pane_id: &str) -> String {
    socket::request_line(
        "hound:pane.focus",
        "pane.focus",
        &serde_json::json!({ "pane_id": pane_id }),
    )
}

pub fn create_tab_args(workspace_id: &str, cwd: &str) -> Vec<String> {
    strings(&[
        "tab",
        "create",
        "--workspace",
        workspace_id,
        "--cwd",
        cwd,
        "--focus",
    ])
}

pub fn start_agent_args(name: &str, kind: &str, pane_id: &str, args: &[String]) -> Vec<String> {
    let mut v = strings(&["agent", "start", name, "--kind", kind, "--pane", pane_id]);
    if !args.is_empty() {
        v.push("--".to_string());
        v.extend(args.iter().cloned());
    }
    v
}

pub fn read_args(pane_id: &str, lines: u32) -> Vec<String> {
    strings(&[
        "pane",
        "read",
        pane_id,
        "--source",
        "recent-unwrapped",
        "--lines",
        &lines.to_string(),
    ])
}

/// Whether hound runs inside herdr, and which herdr binary to drive: `Some(binary)` when
/// `HERDR_ENV` is `1` or `HERDR_SOCKET_PATH` is non-empty, and a binary is known. The
/// binary is `HERDR_BIN_PATH` when non-empty, else whatever `find_on_path("herdr")` finds
/// ([`CliHerdr::detect`] passes [`resolve_on_this_machine`]).
///
/// herdr documents `HERDR_SOCKET_PATH` and `HERDR_BIN_PATH` for `[[keys.command]]`
/// popups, and those plus `HERDR_ENV=1` for plugin panes and panes generally.
pub fn detect_herdr(
    herdr_env: Option<&OsStr>,
    socket_path: Option<&OsStr>,
    bin_path: Option<&OsStr>,
    find_on_path: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let inside = herdr_env.is_some_and(|v| v == "1") || socket_path.is_some_and(|v| !v.is_empty());
    if !inside {
        return None;
    }
    match bin_path.filter(|v| !v.is_empty()) {
        Some(bin) => Some(PathBuf::from(bin)),
        None => find_on_path("herdr"),
    }
}

/// The herdr session hound is running in, driven through its CLI.
pub struct CliHerdr {
    /// The herdr binary every command runs.
    bin: PathBuf,
}

impl CliHerdr {
    /// `Some` when the environment says hound runs inside herdr; see [`detect_herdr`].
    pub fn detect() -> Option<CliHerdr> {
        detect_herdr(
            env::var_os("HERDR_ENV").as_deref(),
            env::var_os("HERDR_SOCKET_PATH").as_deref(),
            env::var_os("HERDR_BIN_PATH").as_deref(),
            resolve_on_this_machine,
        )
        .map(|bin| CliHerdr { bin })
    }

    /// A host for the attached machine `label`, driven through this host's herdr binary; an
    /// error if [`validate_machine_label`] rejects the label.
    pub fn for_machine(&self, label: impl Into<String>) -> anyhow::Result<MachineHerdr> {
        MachineHerdr::new(self.bin.clone(), label.into())
    }

    fn run(&self, args: &[String]) -> anyhow::Result<String> {
        run_herdr(&self.bin, args)
    }

    fn run_checked(&self, args: &[String]) -> anyhow::Result<()> {
        run_herdr_checked(&self.bin, args)
    }
}

impl Host for CliHerdr {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        let [workspaces, tabs, panes, agents] = snapshot_commands();
        let workspaces = self.run(&workspaces)?;
        let tabs = self.run(&tabs)?;
        let panes = self.run(&panes)?;
        let agents = self.run(&agents)?;
        parse_snapshot(&workspaces, &tabs, &panes, &agents)
    }

    fn read_scrollback(&self, pane_id: &str, lines: u32) -> anyhow::Result<String> {
        parse_read_output(&self.run(&read_args(pane_id, lines))?)
    }

    /// Focuses the workspace and tab through the CLI, then the pane by ID through the
    /// socket's `pane.focus`. Without a reachable socket, falls back to `agent focus`.
    fn focus(&self, pane: &LivePane) -> anyhow::Result<()> {
        for cmd in focus_commands(pane) {
            self.run_checked(&cmd)?;
        }
        if let Some(mut conn) = socket::connect_from_env() {
            conn.call(&pane_focus_request(&pane.pane_id))
                .with_context(|| format!("herdr pane.focus {}", pane.pane_id))?;
            return Ok(());
        }
        self.run_checked(&fallback_focus_args(pane)?)
    }

    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane> {
        parse_tab_created(&self.run(&create_tab_args(workspace_id, cwd))?, cwd)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        self.run_checked(&start_agent_args(name, kind, pane_id, args))
    }
}

/// Another computer the local herdr is attached to, driven with `herdr --machine <label> …`.
///
/// IDs come from that machine's own listings and are passed back unchanged. herdr never falls
/// back to the local session, and a failed connection does not prove a mutation was not
/// applied, so errors report herdr's message as it is, on one line.
pub struct MachineHerdr {
    /// The local herdr binary, which forwards each command.
    bin: PathBuf,
    /// The machine label given to `--machine`.
    label: String,
}

impl MachineHerdr {
    /// An error if [`validate_machine_label`] rejects `label`.
    pub fn new(bin: PathBuf, label: String) -> anyhow::Result<MachineHerdr> {
        validate_machine_label(&label)?;
        Ok(MachineHerdr { bin, label })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The arguments given to the herdr binary to run `args` on this machine.
    pub fn command_args(&self, args: &[String]) -> Vec<String> {
        machine_args(&self.label, args)
    }

    fn run(&self, args: &[String]) -> anyhow::Result<String> {
        run_herdr(&self.bin, &self.command_args(args)).map_err(single_line)
    }

    fn run_checked(&self, args: &[String]) -> anyhow::Result<()> {
        run_herdr_checked(&self.bin, &self.command_args(args)).map_err(single_line)
    }
}

impl Host for MachineHerdr {
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>> {
        let [workspaces, tabs, panes, agents] = snapshot_commands();
        let workspaces = self.run(&workspaces)?;
        let tabs = self.run(&tabs)?;
        let panes = self.run(&panes)?;
        let agents = self.run(&agents)?;
        parse_snapshot(&workspaces, &tabs, &panes, &agents).map_err(single_line)
    }

    fn read_scrollback(&self, _pane_id: &str, _lines: u32) -> anyhow::Result<String> {
        bail!("scrollback is not available on a remote machine")
    }

    fn focus(&self, pane: &LivePane) -> anyhow::Result<()> {
        for cmd in machine_focus_commands(pane).map_err(single_line)? {
            self.run_checked(&cmd)?;
        }
        Ok(())
    }

    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane> {
        let args = machine_create_tab_args(workspace_id, cwd).map_err(single_line)?;
        parse_tab_created(&self.run(&args)?, cwd).map_err(single_line)
    }

    /// Starts the agent, then focuses it by name so the client's view follows. The agent has
    /// started once the first command succeeds, so a failed focus is not an error.
    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        let [start, focus] =
            machine_start_agent_commands(name, kind, pane_id, args).map_err(single_line)?;
        self.run_checked(&start)?;
        let _ = self.run_checked(&focus);
        Ok(())
    }
}

/// Runs `<bin> <args>` and returns stdout; a non-zero exit is an error carrying stderr.
fn run_herdr(bin: &Path, args: &[String]) -> anyhow::Result<String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("running herdr {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "herdr {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout)
        .with_context(|| format!("herdr {}: output is not UTF-8", args.join(" ")))
}

/// Runs a command whose output carries nothing hound needs: exit 0 is success unless stdout
/// is a JSON object carrying an `error`.
fn run_herdr_checked(bin: &Path, args: &[String]) -> anyhow::Result<()> {
    check_output(&run_herdr(bin, args)?).with_context(|| format!("herdr {}", args.join(" ")))
}

/// `err` with its whole context chain flattened onto one line.
fn single_line(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(one_line(&format!("{err:#}")))
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Success unless `out` is a JSON object carrying an `error`; anything else, including empty
/// or non-JSON output, is success.
pub fn check_output(out: &str) -> anyhow::Result<()> {
    match envelope(out) {
        Some(value) if error_of(&value).is_some() => {
            result_from_value::<serde_json::Value>(value).map(drop)
        }
        _ => Ok(()),
    }
}

/// The envelope's `error`, unless it is absent or `null`.
fn error_of(value: &serde_json::Value) -> Option<&serde_json::Value> {
    value.get("error").filter(|e| !e.is_null())
}

/// `out` parsed as a response envelope: a JSON object with a `result` or `error` key.
fn envelope(out: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(out)
        .ok()
        .filter(|v| v.get("result").is_some() || v.get("error").is_some())
}

/// herdr's response envelope: `{"id", "result"}` on success, `{"id", "error"}` on failure.
fn parse_result<T: DeserializeOwned>(json: &str) -> anyhow::Result<T> {
    let value: serde_json::Value = serde_json::from_str(json).context("invalid JSON")?;
    result_from_value(value)
}

fn result_from_value<T: DeserializeOwned>(mut value: serde_json::Value) -> anyhow::Result<T> {
    if let Some(error) = error_of(&value) {
        let code = error
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or("unknown");
        let message = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
        bail!("herdr error {code}: {message}");
    }
    let result = value
        .get_mut("result")
        .map(serde_json::Value::take)
        .context("response has neither result nor error")?;
    Ok(serde_json::from_value(result)?)
}

#[derive(Deserialize)]
struct WorkspaceList {
    workspaces: Vec<LabelledWorkspace>,
}

#[derive(Deserialize)]
struct LabelledWorkspace {
    workspace_id: String,
    label: String,
}

#[derive(Deserialize)]
struct TabList {
    tabs: Vec<LabelledTab>,
}

#[derive(Deserialize)]
struct LabelledTab {
    tab_id: String,
    label: String,
}

#[derive(Deserialize)]
struct PaneList {
    panes: Vec<PaneRow>,
}

#[derive(Deserialize)]
struct AgentList {
    agents: Vec<AgentRow>,
}

#[derive(Deserialize)]
struct AgentRow {
    pane_id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct TabCreated {
    tab: LabelledTab,
    root_pane: PaneRow,
}

#[derive(Deserialize)]
struct PaneRead {
    read: PaneReadText,
}

#[derive(Deserialize)]
struct PaneReadText {
    text: String,
}

#[derive(Deserialize)]
struct PaneRow {
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSession>,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    focused: bool,
}

#[derive(Deserialize)]
struct AgentSession {
    value: String,
}

impl PaneRow {
    fn into_live_pane(
        self,
        workspace_label: String,
        tab_label: String,
        agent_name: Option<String>,
    ) -> LivePane {
        let has_agent = self.agent.is_some();
        LivePane {
            pane_id: self.pane_id,
            tab_id: self.tab_id,
            workspace_id: self.workspace_id,
            workspace_label,
            tab_label,
            agent_name,
            kind: self.agent,
            session_id: self.agent_session.map(|s| s.value),
            title: self.terminal_title_stripped.unwrap_or_default(),
            cwd: self.cwd.or(self.foreground_cwd).unwrap_or_default(),
            status: if has_agent {
                self.agent_status.unwrap_or_default()
            } else {
                String::new()
            },
            focused: self.focused,
            scrollback: None,
        }
    }
}
