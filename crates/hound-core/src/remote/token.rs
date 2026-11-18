//! The service's access token: 32 random bytes as hex in a file only its
//! owner can read.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use anyhow::Context;

const TOKEN_BYTES: usize = 32;

/// The token in `path`, creating it (and any missing parent directories, mode
/// 0700) if the file does not exist. A new file is created with mode 0600
/// before anything is written to it.
pub fn load_or_create_token(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        return read_token(path);
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let token = random_hex()?;
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        // Created concurrently by another process: use that one.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return read_token(path),
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    };
    if let Err(e) = file
        .write_all(format!("{token}\n").as_bytes())
        .and_then(|()| file.sync_all())
    {
        // Leave no empty or partial token file behind for the next start.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(token)
}

/// The token in `path`, trimmed; an error if the file is missing or blank.
pub fn read_token(path: &Path) -> anyhow::Result<String> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let token = text.trim();
    anyhow::ensure!(!token.is_empty(), "token file {} is empty", path.display());
    Ok(token.to_string())
}

/// Whether the two tokens are equal, taking the same time whatever the
/// position of the first difference: every byte up to the longer length is
/// compared.
pub fn tokens_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// `TOKEN_BYTES` bytes from the operating system's random source, as
/// lowercase hex.
fn random_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
