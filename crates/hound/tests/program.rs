use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use hound::program::resolve_program;
use hound_core::platform::Platform;

const PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// `resolve_program` over a fake filesystem holding exactly `files`.
fn resolve(
    name: &str,
    platform: Platform,
    path_var: Option<&str>,
    pathext: Option<&str>,
    files: &[&str],
) -> Option<PathBuf> {
    let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
    resolve_program(
        name,
        platform,
        path_var.map(OsStr::new),
        pathext.map(OsStr::new),
        &|p: &Path| files.contains(p),
    )
}

#[test]
fn windows_finds_cmd_shim_in_path_order() {
    let found = resolve(
        "claude",
        Platform::Windows,
        Some("C:\\a;C:\\b"),
        Some(PATHEXT),
        &["C:\\b\\claude.cmd", "C:\\b\\claude.exe"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\b\\claude.exe")));
}

#[test]
fn windows_finds_cmd_shim_alone() {
    let found = resolve(
        "claude",
        Platform::Windows,
        Some("C:\\a;C:\\b"),
        Some(PATHEXT),
        &["C:\\b\\claude.cmd"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\b\\claude.cmd")));
}

#[test]
fn windows_earlier_directory_wins() {
    let found = resolve(
        "claude",
        Platform::Windows,
        Some("C:\\a;C:\\b"),
        Some(PATHEXT),
        &["C:\\a\\claude.cmd", "C:\\b\\claude.exe"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\a\\claude.cmd")));
}

#[test]
fn windows_never_returns_bare_name() {
    let found = resolve(
        "claude",
        Platform::Windows,
        Some("C:\\a;C:\\b"),
        Some(PATHEXT),
        &["C:\\a\\claude"],
    );
    assert_eq!(found, None);
}

#[test]
fn windows_name_with_extension_used_as_is() {
    let probed = RefCell::new(Vec::new());
    let found = resolve_program(
        "pi.cmd",
        Platform::Windows,
        Some(OsStr::new("C:\\a")),
        Some(OsStr::new(PATHEXT)),
        &|p: &Path| {
            probed.borrow_mut().push(p.to_path_buf());
            p == Path::new("C:\\a\\pi.cmd")
        },
    );
    assert_eq!(found, Some(PathBuf::from("C:\\a\\pi.cmd")));
    assert!(
        !probed
            .borrow()
            .contains(&PathBuf::from("C:\\a\\pi.cmd.EXE"))
    );

    // Not found as is: no extension is appended either.
    let probed = RefCell::new(Vec::new());
    let found = resolve_program(
        "pi.cmd",
        Platform::Windows,
        Some(OsStr::new("C:\\a")),
        Some(OsStr::new(PATHEXT)),
        &|p: &Path| {
            probed.borrow_mut().push(p.to_path_buf());
            false
        },
    );
    assert_eq!(found, None);
    assert_eq!(*probed.borrow(), [PathBuf::from("C:\\a\\pi.cmd")]);
}

#[test]
fn windows_extension_match_is_case_insensitive() {
    let found = resolve(
        "pi.CMD",
        Platform::Windows,
        Some("C:\\a"),
        Some(".com;.exe;.bat;.cmd"),
        &["C:\\a\\pi.CMD"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\a\\pi.CMD")));
}

#[test]
fn pathext_variants() {
    let files = ["C:\\a\\pi.cmd"];
    let trimmed = resolve(
        "pi",
        Platform::Windows,
        Some("C:\\a"),
        Some(".exe; ;.CMD"),
        &files,
    );
    assert!(trimmed.is_some());
    let default = resolve("pi", Platform::Windows, Some("C:\\a"), None, &files);
    assert!(default.is_some());
}

#[test]
fn windows_path_split_uses_semicolon() {
    let found = resolve(
        "herdr",
        Platform::Windows,
        Some("C:\\x:y;C:\\a"),
        Some(PATHEXT),
        &["C:\\a\\herdr.exe"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\a\\herdr.exe")));
}

#[test]
fn windows_quoted_and_trailing_separator_entries() {
    let found = resolve(
        "herdr",
        Platform::Windows,
        Some("\"C:\\Program Files\\herdr\";C:\\b\\"),
        Some(PATHEXT),
        &["C:\\Program Files\\herdr\\herdr.exe"],
    );
    assert_eq!(
        found,
        Some(PathBuf::from("C:\\Program Files\\herdr\\herdr.exe"))
    );
    let found = resolve(
        "herdr",
        Platform::Windows,
        Some("C:\\b\\"),
        Some(PATHEXT),
        &["C:\\b\\herdr.exe"],
    );
    assert_eq!(found, Some(PathBuf::from("C:\\b\\herdr.exe")));
}

#[test]
fn unix_bare_name_with_colon_separator() {
    let found = resolve(
        "herdr",
        Platform::Unix,
        Some("/usr/bin:/opt/bin"),
        Some(PATHEXT),
        &["/opt/bin/herdr", "/opt/bin/herdr.EXE"],
    );
    assert_eq!(found, Some(PathBuf::from("/opt/bin/herdr")));
}

#[test]
fn name_with_separator_is_used_directly() {
    let tool = "C:\\tools\\herdr.exe";
    let windows = resolve(tool, Platform::Windows, Some("C:\\a"), None, &[tool]);
    assert_eq!(windows, Some(PathBuf::from(tool)));
    let windows = resolve(tool, Platform::Windows, Some("C:\\a"), None, &[]);
    assert_eq!(windows, None);

    let unix = resolve(
        "./herdr",
        Platform::Unix,
        Some("/usr/bin"),
        None,
        &["./herdr"],
    );
    assert_eq!(unix, Some(PathBuf::from("./herdr")));
    let unix = resolve("./herdr", Platform::Unix, Some("/usr/bin"), None, &[]);
    assert_eq!(unix, None);
}

#[test]
fn empty_or_missing_path_finds_nothing() {
    for platform in [Platform::Unix, Platform::Windows] {
        let files = ["herdr", "herdr.exe", "/herdr", "\\herdr.exe"];
        assert_eq!(resolve("herdr", platform, None, None, &files), None);
        assert_eq!(resolve("herdr", platform, Some(""), None, &files), None);
    }
    assert_eq!(
        resolve(
            "herdr",
            Platform::Unix,
            Some("::"),
            None,
            &["/herdr", "herdr"]
        ),
        None
    );
    assert_eq!(
        resolve(
            "herdr",
            Platform::Windows,
            Some(";;"),
            None,
            &["\\herdr.exe", "herdr.exe"]
        ),
        None
    );
}

#[test]
fn platform_current_matches_cfg() {
    assert_eq!(Platform::current() == Platform::Windows, cfg!(windows));
}
