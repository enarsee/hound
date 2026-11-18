//! The terminal environment hound runs in: its live panes, and how to focus or open one.
//!
//! A host maps its own layout onto workspace / tab / pane. A host with fewer levels
//! repeats IDs or leaves labels empty.

/// One live terminal pane, whether or not a coding agent runs in it.
#[derive(Clone, Debug, PartialEq)]
pub struct LivePane {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub workspace_label: String,
    pub tab_label: String,
    /// The name the host gave the agent in this pane, if any.
    pub agent_name: Option<String>,
    /// The harness running in the pane, as `Harness::as_str()`; `None` for a plain shell.
    pub kind: Option<String>,
    /// The harness session ID, if the host knows it.
    pub session_id: Option<String>,
    pub title: String,
    pub cwd: String,
    /// The agent's lifecycle state; empty for non-agent panes.
    pub status: String,
    pub focused: bool,
    pub scrollback: Option<String>,
}

pub trait Host {
    /// Every live pane, agent or not.
    fn snapshot(&self) -> anyhow::Result<Vec<LivePane>>;
    /// The last `lines` lines of the pane's output.
    fn read_scrollback(&self, pane_id: &str, lines: u32) -> anyhow::Result<String>;
    /// Make the pane focused, switching workspace and tab as needed.
    fn focus(&self, pane: &LivePane) -> anyhow::Result<()>;
    /// Open a new tab in `workspace_id` at `cwd`, focused; returns its root pane.
    fn create_tab(&self, workspace_id: &str, cwd: &str) -> anyhow::Result<LivePane>;
    /// Start the harness `kind` under `name` in the shell pane `pane_id`, passing `args` to it.
    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> anyhow::Result<()>;
}
