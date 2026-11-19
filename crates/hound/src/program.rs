//! Finding a program the way the platform's shell would: `PATH`, and on Windows `PATHEXT`.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use hound_core::platform::Platform;

/// `PATHEXT` when the environment does not set it.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Where `name` runs from, or `None`.
///
/// A name containing `/` or `\` is used as is. Otherwise each `PATH` directory is tried in
/// order: split on `;` on Windows, empty entries skipped; split on `:` on Unix, an empty
/// entry meaning the current directory. On Unix the bare name is tried in each directory. On Windows a name already ending in a `PATHEXT` extension is
/// tried as is; any other name is tried with each `PATHEXT` extension in order, never bare,
/// the extension in lower case (Windows file names are case-insensitive).
/// `is_runnable` decides whether a candidate exists and can be run; the returned path is
/// the candidate it accepted.
pub fn resolve_program(
    name: &str,
    platform: Platform,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
    is_runnable: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains(['/', '\\']) {
        let path = PathBuf::from(name);
        return is_runnable(&path).then_some(path);
    }
    let file_names = file_names(name, platform, pathext);
    for dir in split_path(path_var?, platform) {
        for file_name in &file_names {
            let candidate = join(&dir, file_name, platform);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// [`resolve_program`] for this machine: its platform, `PATH` and `PATHEXT`, and its
/// filesystem (on Unix a file with an execute bit, on Windows any file).
pub fn resolve_on_this_machine(name: &str) -> Option<PathBuf> {
    let platform = Platform::current();
    let path_var = env::var_os("PATH");
    let pathext = match platform {
        Platform::Windows => env::var_os("PATHEXT"),
        Platform::Unix => None,
    };
    resolve_program(
        name,
        platform,
        path_var.as_deref(),
        pathext.as_deref(),
        &is_runnable,
    )
}

/// The file names to look for in each directory.
fn file_names(name: &str, platform: Platform, pathext: Option<&OsStr>) -> Vec<String> {
    match platform {
        Platform::Unix => vec![name.to_string()],
        Platform::Windows => {
            let pathext = pathext.map_or_else(
                || DEFAULT_PATHEXT.to_string(),
                |v| v.to_string_lossy().into_owned(),
            );
            let extensions: Vec<&str> = pathext
                .split(';')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .collect();
            let lower = name.to_ascii_lowercase();
            if extensions
                .iter()
                .any(|e| lower.ends_with(&e.to_ascii_lowercase()))
            {
                vec![name.to_string()]
            } else {
                extensions
                    .iter()
                    .map(|e| format!("{name}{}", e.to_ascii_lowercase()))
                    .collect()
            }
        }
    }
}

/// The directories in `path_var`, split with the platform's separator. On Unix an empty
/// entry means the current directory (`.`), as for `execvp`; on Windows empty entries are
/// skipped and surrounding double quotes are removed from an entry.
fn split_path(path_var: &OsStr, platform: Platform) -> Vec<OsString> {
    let separator = match platform {
        Platform::Unix => b':',
        Platform::Windows => b';',
    };
    path_var
        .as_encoded_bytes()
        .split(|&b| b == separator)
        .filter_map(|entry| match platform {
            Platform::Windows => Some(trim_quotes(entry)).filter(|e| !e.is_empty()),
            Platform::Unix if entry.is_empty() => Some(b".".as_slice()),
            Platform::Unix => Some(entry),
        })
        // SAFETY: each entry is the ASCII `.` or a slice of `path_var`'s encoded bytes split
        // on an ASCII byte and trimmed of ASCII quotes, so it starts and ends on a valid
        // boundary.
        .map(|entry| unsafe { OsStr::from_encoded_bytes_unchecked(entry) }.to_os_string())
        .collect()
}

/// `entry` without one pair of surrounding double quotes.
fn trim_quotes(entry: &[u8]) -> &[u8] {
    match entry {
        [b'"', inner @ .., b'"'] => inner,
        _ => entry,
    }
}

/// `dir` and `file_name` joined with the platform's separator, so a Windows path is built
/// the same way whichever system the code runs on.
fn join(dir: &OsStr, file_name: &str, platform: Platform) -> PathBuf {
    let separator = match platform {
        Platform::Unix => "/",
        Platform::Windows => "\\",
    };
    let bytes = dir.as_encoded_bytes();
    let ends_with_separator = match platform {
        Platform::Unix => bytes.ends_with(b"/"),
        Platform::Windows => bytes.ends_with(b"\\") || bytes.ends_with(b"/"),
    };
    let mut path = dir.to_os_string();
    if !ends_with_separator {
        path.push(separator);
    }
    path.push(file_name);
    PathBuf::from(path)
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(path: &Path) -> bool {
    path.is_file()
}
