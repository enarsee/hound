//! User configuration and the locations of hound's own files.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::platform::Platform;

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
        let Locations {
            claude_root,
            pi_root,
            ..
        } = current_locations();
        Config {
            index_subagents: true,
            search_subagents: true,
            claude_root,
            pi_root,
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
        let home = home_dir().unwrap_or_default();
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

/// Where hound and the agents keep their files by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locations {
    /// hound's config file.
    pub config: PathBuf,
    /// hound's state directory, holding the index.
    pub state: PathBuf,
    /// Claude Code's session files.
    pub claude_root: PathBuf,
    /// pi's session files.
    pub pi_root: PathBuf,
}

/// The default locations on `platform` for the user whose home directory is
/// `home`. On Windows `appdata` and `local_appdata` are the values of
/// `APPDATA` and `LOCALAPPDATA`, falling back to `<home>\AppData\Roaming` and
/// `<home>\AppData\Local` when unset; on Unix they are ignored.
pub fn default_locations(
    platform: Platform,
    home: &Path,
    appdata: Option<&Path>,
    local_appdata: Option<&Path>,
) -> Locations {
    match platform {
        Platform::Unix => Locations {
            config: home.join(".config/hound/config.toml"),
            state: home.join(".local/state/hound"),
            claude_root: home.join(".claude/projects"),
            pi_root: home.join(".pi/agent/sessions"),
        },
        Platform::Windows => {
            let roaming = appdata.map_or_else(
                || windows_join(home, &["AppData", "Roaming"]),
                Path::to_path_buf,
            );
            let local = local_appdata.map_or_else(
                || windows_join(home, &["AppData", "Local"]),
                Path::to_path_buf,
            );
            Locations {
                config: windows_join(&roaming, &["hound", "config.toml"]),
                state: windows_join(&local, &["hound"]),
                claude_root: windows_join(home, &[".claude", "projects"]),
                pi_root: windows_join(home, &[".pi", "agent", "sessions"]),
            }
        }
    }
}

/// `base` followed by `parts`, each after a backslash. Works on the string so
/// that a Windows path is built the same on any platform.
fn windows_join(base: &Path, parts: &[&str]) -> PathBuf {
    let mut out = base.as_os_str().to_owned();
    for part in parts {
        let bytes = out.as_encoded_bytes();
        if !matches!(bytes.last(), None | Some(b'\\' | b'/')) {
            out.push("\\");
        }
        out.push(part);
    }
    PathBuf::from(out)
}

/// [`default_locations`] for this machine and user.
fn current_locations() -> Locations {
    let appdata = env_path("APPDATA");
    let local_appdata = env_path("LOCALAPPDATA");
    default_locations(
        Platform::current(),
        &home_dir().unwrap_or_default(),
        appdata.as_deref(),
        local_appdata.as_deref(),
    )
}

/// `$HOUND_STATE_DIR`, or the platform's default (`~/.local/state/hound` on
/// Unix, `%LOCALAPPDATA%\hound` on Windows).
pub fn state_dir() -> PathBuf {
    env_path("HOUND_STATE_DIR").unwrap_or_else(|| current_locations().state)
}

/// `$HOUND_CONFIG`, or the platform's default (`~/.config/hound/config.toml`
/// on Unix, `%APPDATA%\hound\config.toml` on Windows).
pub fn config_path() -> PathBuf {
    env_path("HOUND_CONFIG").unwrap_or_else(|| current_locations().config)
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

/// The user's home directory: `$HOME` on Unix (or the password database when
/// unset), the profile folder (`%USERPROFILE%`) on Windows.
pub fn home_dir() -> Option<PathBuf> {
    env::home_dir()
}
