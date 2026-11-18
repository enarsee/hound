use std::fs;
use std::path::Path;

use hound_core::config::{Config, expand_home};
use hound_core::model::{Harness, SessionKey};

#[test]
fn missing_file_gives_defaults() {
    let c = Config::load(Path::new("/nonexistent/hound.toml")).unwrap();
    assert!(c.index_subagents && c.search_subagents);
    assert!(!c.enrich.enabled);
    assert_eq!(c.enrich.base_url, "http://localhost:11434/v1");
    assert_eq!(c.enrich.quiet_secs, 600);
    assert!(c.enrich.categories.is_empty());
    assert_eq!(c.enrich.model, "");
    assert!(c.claude_root.ends_with(".claude/projects"));
    assert!(c.pi_root.ends_with(".pi/agent/sessions"));
}

#[test]
fn partial_file_overrides_only_what_it_sets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "search_subagents = false\n[enrich]\nenabled = true\nmodel = \"m\"\ncategories = [\"A\",\"B\"]\n",
    )
    .unwrap();
    let c = Config::load(&path).unwrap();
    assert!(!c.search_subagents);
    assert!(c.index_subagents);
    assert!(c.enrich.enabled);
    assert_eq!(c.enrich.model, "m");
    assert_eq!(c.enrich.base_url, "http://localhost:11434/v1");
    assert_eq!(c.enrich.categories, vec!["A".to_string(), "B".to_string()]);
}

#[test]
fn malformed_toml_is_an_error_naming_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.toml");
    fs::write(&path, "search_subagents = = nope\n[enrich\n").unwrap();
    let err = Config::load(&path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&path.display().to_string()),
        "error should name {}: {msg}",
        path.display()
    );
}

#[test]
fn session_key_string() {
    assert_eq!(
        SessionKey {
            harness: Harness::Pi,
            id: "ab".into()
        }
        .as_string(),
        "pi:ab"
    );
    assert_eq!(Harness::parse("claude"), Some(Harness::Claude));
    assert_eq!(Harness::parse("codex"), None);
}

#[test]
fn leading_tilde_is_the_home_directory() {
    let home = Path::new("/home/u");
    assert_eq!(
        expand_home(Path::new("~/.claude/projects"), home),
        Path::new("/home/u/.claude/projects")
    );
    assert_eq!(expand_home(Path::new("~"), home), Path::new("/home/u"));
    assert_eq!(
        expand_home(Path::new("/srv/~/x"), home),
        Path::new("/srv/~/x")
    );
    assert_eq!(expand_home(Path::new("~bob/x"), home), Path::new("~bob/x"));
    assert_eq!(expand_home(Path::new("rel/x"), home), Path::new("rel/x"));
}

#[test]
fn roots_in_the_config_file_expand_tilde() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "claude_root = \"~/.claude/projects\"\npi_root = \"~\"\n",
    )
    .unwrap();
    let c = Config::load(&path).unwrap();
    let home = std::env::home_dir().unwrap();
    assert_eq!(c.claude_root, home.join(".claude/projects"));
    assert_eq!(c.pi_root, home);
}

#[test]
fn serve_and_remote_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = "[serve]\nbind = \"127.0.0.1:7717\"\n[[remote]]\nmachine = \"homelab\"\nurl = \"http://homelab.example-tailnet.ts.net:7717\"\ntoken_file = \"~/t\"\n";
    fs::write(&path, text).unwrap();
    let c = Config::load(&path).unwrap();
    assert!(!c.serve.enabled);
    assert_eq!(c.serve.bind.as_deref(), Some("127.0.0.1:7717"));
    assert_eq!(c.serve.refresh_secs, 30);
    assert!(!c.serve.allow_any_interface);
    assert_eq!(c.remotes.len(), 1);
    assert_eq!(c.remotes[0].machine, "homelab");
    assert_eq!(
        c.remotes[0].url,
        "http://homelab.example-tailnet.ts.net:7717"
    );
    let home = std::env::home_dir().unwrap();
    assert_eq!(c.remotes[0].token_file, home.join("t"));
    assert!(c.remotes[0].enabled);
    assert_eq!(c.enabled_remotes().count(), 1);

    fs::write(&path, format!("{text}enabled = false\n")).unwrap();
    let c = Config::load(&path).unwrap();
    assert_eq!(c.remotes.len(), 1);
    assert_eq!(c.enabled_remotes().count(), 0);

    let c = Config::load(Path::new("/nonexistent/hound.toml")).unwrap();
    assert!(!c.serve.enabled);
    assert_eq!(c.serve.bind, None);
    assert!(c.remotes.is_empty());
}

#[test]
fn serve_token_lives_next_to_the_config() {
    let path = hound_core::config::config_path();
    assert_eq!(
        hound_core::config::serve_token_path(),
        path.parent().unwrap().join("serve-token")
    );
}

#[test]
fn remote_timeout_defaults_to_3_and_is_clamped() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let base = "[[remote]]\nmachine = \"homelab\"\nurl = \"http://100.64.0.2:7717\"\ntoken_file = \"/t\"\n";
    fs::write(&path, base).unwrap();
    let c = Config::load(&path).unwrap();
    assert_eq!(c.remotes[0].timeout_secs, None);
    assert_eq!(c.remotes[0].timeout(), Duration::from_secs(3));
    for (given, used) in [(0, 1), (1, 1), (7, 7), (30, 30), (31, 30), (100_000, 30)] {
        fs::write(&path, format!("{base}timeout_secs = {given}\n")).unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.remotes[0].timeout_secs, Some(given));
        assert_eq!(c.remotes[0].timeout(), Duration::from_secs(used), "{given}");
    }
}
