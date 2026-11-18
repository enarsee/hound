//! User configuration and the locations of hound's own files.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub index_subagents: bool,
    pub search_subagents: bool,
    pub claude_root: PathBuf,
    pub pi_root: PathBuf,
    pub enrich: EnrichConfig,
    /// `hound serve`: off unless `enabled` is set.
    pub serve: ServeConfig,
    /// Other machines' hound services; `[[remote]]` blocks.
    #[serde(rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
}

/// Optional LLM enrichment against an OpenAI-compatible endpoint.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EnrichConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable holding the API key, if one is needed.
    pub api_key_env: Option<String>,
    pub categories: Vec<String>,
    /// A session must be quiet this long before it is enriched.
    pub quiet_secs: i64,
}

/// `hound serve`, which offers this machine's index to other machines.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ServeConfig {
    pub enabled: bool,
    /// The address to listen on, like `100.64.0.1:7717`.
    pub bind: Option<String>,
    /// Seconds between the service's index refreshes.
    pub refresh_secs: u64,
    /// Allows binding an unspecified address (`0.0.0.0`, `[::]`), which
    /// listens on every interface.
    pub allow_any_interface: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        ServeConfig {
            enabled: false,
            bind: None,
            refresh_secs: 30,
            allow_any_interface: false,
        }
    }
}

/// Another machine's hound service.
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteConfig {
    /// The machine's name, as the host knows it.
    pub machine: String,
    /// The service's base URL, like `http://homelab:7717`.
    pub url: String,
    /// A file holding the service's token; a leading `~` is the home directory.
    pub token_file: PathBuf,
    /// A disabled remote is ignored entirely.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Bounds connecting to the service and, separately, each request to
    /// it, in seconds; see [`RemoteConfig::timeout`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A remote's timeout when `timeout_secs` is not set, in seconds.
pub const DEFAULT_REMOTE_TIMEOUT_SECS: u64 = 3;
/// The shortest and longest remote timeouts; values outside are clamped.
pub const REMOTE_TIMEOUT_SECS: std::ops::RangeInclusive<u64> = 1..=30;

impl RemoteConfig {
    /// `timeout_secs`, [`DEFAULT_REMOTE_TIMEOUT_SECS`] if unset, clamped to
    /// [`REMOTE_TIMEOUT_SECS`].
    pub fn timeout(&self) -> Duration {
        let secs = self
            .timeout_secs
            .unwrap_or(DEFAULT_REMOTE_TIMEOUT_SECS)
            .clamp(*REMOTE_TIMEOUT_SECS.start(), *REMOTE_TIMEOUT_SECS.end());
        Duration::from_secs(secs)
    }
}

fn enabled_by_default() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir();
        Config {
            index_subagents: true,
            search_subagents: true,
            claude_root: home.join(".claude/projects"),
            pi_root: home.join(".pi/agent/sessions"),
            enrich: EnrichConfig::default(),
            serve: ServeConfig::default(),
            remotes: Vec::new(),
        }
    }
}

impl Default for EnrichConfig {
    fn default() -> Self {
        EnrichConfig {
            enabled: false,
            base_url: "http://localhost:11434/v1".to_string(),
            model: String::new(),
            api_key_env: None,
            categories: Vec::new(),
            quiet_secs: 600,
        }
    }
}

impl Config {
    /// Reads the config at `path`; a missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading config {}", path.display()));
            }
        };
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        let home = home_dir();
        config.claude_root = expand_home(&config.claude_root, &home);
        config.pi_root = expand_home(&config.pi_root, &home);
        for remote in &mut config.remotes {
            remote.token_file = expand_home(&remote.token_file, &home);
        }
        Ok(config)
    }

    /// The `[[remote]]` blocks not marked `enabled = false`.
    pub fn enabled_remotes(&self) -> impl Iterator<Item = &RemoteConfig> {
        self.remotes.iter().filter(|r| r.enabled)
    }
}

/// `path` with a leading `~` component replaced by `home`; other paths,
/// including `~user/...`, unchanged.
pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) if rest.as_os_str().is_empty() => home.to_path_buf(),
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// `$HOUND_STATE_DIR`, or `~/.local/state/hound`.
pub fn state_dir() -> PathBuf {
    env_path("HOUND_STATE_DIR").unwrap_or_else(|| home_dir().join(".local/state/hound"))
}

/// `$HOUND_CONFIG`, or `~/.config/hound/config.toml`.
pub fn config_path() -> PathBuf {
    env_path("HOUND_CONFIG").unwrap_or_else(|| home_dir().join(".config/hound/config.toml"))
}

/// `serve-token` in the directory holding the config file.
pub fn serve_token_path() -> PathBuf {
    config_path()
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
        .join("serve-token")
}

fn env_path(var: &str) -> Option<PathBuf> {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    env::home_dir().unwrap_or_default()
}
