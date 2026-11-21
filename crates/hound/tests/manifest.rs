//! The herdr plugin manifest carries its own copy of hound's version; a
//! release (cargo-release) rewrites both. This keeps them from drifting.

const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../herdr-plugin.toml"
));

/// The quoted value of the first top-level `version = "…"` line.
fn top_level_version(toml: &str) -> Option<&str> {
    toml.lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.split('"').next())
}

#[test]
fn plugin_manifest_version_matches_cargo() {
    assert_eq!(
        top_level_version(MANIFEST),
        Some(env!("CARGO_PKG_VERSION")),
        "herdr-plugin.toml version must match Cargo.toml"
    );
}

#[test]
fn guard_reads_the_top_level_version_line() {
    let toml = "id = \"example\"\nmin_herdr_version = \"9.9.9\"\nversion = \"1.2.3\"\n";
    assert_eq!(top_level_version(toml), Some("1.2.3"));
}
