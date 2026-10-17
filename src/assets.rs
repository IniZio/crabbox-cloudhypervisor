//! Cached, checksum-verified host assets (base image, firmware) and raw conversion.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// How much of a cached asset can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// The file does not exist.
    Missing,
    /// The file exists but has no matching checksum marker.
    Present,
    /// The file exists and its marker holds the expected checksum.
    Verified,
}

/// Longest stderr tail included in an error message.
const STDERR_TAIL: usize = 300;

/// Returns `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// File name of `path` for error messages.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Streams `path` through SHA-256 and returns lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", file_label(path)))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", file_label(path)))?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        for nibble in [byte >> 4, byte & 0x0f] {
            hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    Ok(hex)
}

/// Reports whether `path` is missing, present, or verified against `sha256`.
#[must_use]
pub fn cache_state(path: &Path, sha256: &str) -> CacheState {
    if !path.exists() {
        return CacheState::Missing;
    }
    match fs::read_to_string(with_suffix(path, ".sha256")) {
        Ok(marker) if marker.trim() == sha256 => CacheState::Verified,
        _ => CacheState::Present,
    }
}

/// Takes an exclusive lock on `<dest>.lock`, creating the parent directory.
fn lock_for(dest: &Path) -> Result<File, String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create cache dir for {}: {e}", file_label(dest)))?;
    }
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(with_suffix(dest, ".lock"))
        .map_err(|e| format!("open lock for {}: {e}", file_label(dest)))?;
    lock.lock()
        .map_err(|e| format!("lock {}: {e}", file_label(dest)))?;
    Ok(lock)
}

/// Writes the checksum marker next to `dest`.
fn write_marker(dest: &Path, sha256: &str) -> Result<(), String> {
    fs::write(with_suffix(dest, ".sha256"), format!("{sha256}\n"))
        .map_err(|e| format!("write marker for {}: {e}", file_label(dest)))
}

/// Last `STDERR_TAIL` characters of `bytes` decoded lossily and trimmed.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let skip = text.chars().count().saturating_sub(STDERR_TAIL);
    text.chars().skip(skip).collect()
}

/// Downloads `url` to `dest` unless a verified copy is cached.
pub fn ensure_download(
    curl: &Path,
    url: &str,
    sha256: &str,
    dest: &Path,
    diag: &mut dyn Write,
) -> Result<(), String> {
    let _lock = lock_for(dest)?;
    let name = file_label(dest);
    match cache_state(dest, sha256) {
        CacheState::Verified => return Ok(()),
        CacheState::Present => {
            if sha256_file(dest)? == sha256 {
                return write_marker(dest, sha256);
            }
            fs::remove_file(dest).map_err(|e| format!("remove stale {name}: {e}"))?;
        }
        CacheState::Missing => {}
    }
    let part = with_suffix(dest, ".part");
    let _ = writeln!(diag, "downloading {name}");
    let output = Command::new(curl)
        .args(["-fsSL", "--retry", "3", "--retry-delay", "2", "-o"])
        .arg(&part)
        .arg(url)
        .output()
        .map_err(|e| format!("run curl for {name}: {e}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&part);
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        return Err(format!("download of {name} failed: curl exit {code}"));
    }
    let actual = sha256_file(&part)?;
    if actual != sha256 {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "checksum mismatch for {name}: expected {sha256}, got {actual}"
        ));
    }
    fs::rename(&part, dest).map_err(|e| format!("install {name}: {e}"))?;
    write_marker(dest, sha256)
}

/// Converts `source` to a raw image at `dest` unless `dest` already exists.
pub fn ensure_raw(
    qemu_img: &Path,
    source: &Path,
    dest: &Path,
    diag: &mut dyn Write,
) -> Result<(), String> {
    let _lock = lock_for(dest)?;
    if dest.exists() {
        return Ok(());
    }
    let name = file_label(dest);
    let part = with_suffix(dest, ".part");
    let _ = writeln!(diag, "converting image to raw: {name}");
    let output = Command::new(qemu_img)
        .args(["convert", "-O", "raw"])
        .arg(source)
        .arg(&part)
        .output()
        .map_err(|e| format!("run qemu-img for {name}: {e}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "qemu-img convert for {name} failed: {}",
            tail(&output.stderr)
        ));
    }
    fs::rename(&part, dest).map_err(|e| format!("install {name}: {e}"))
}

#[cfg(test)]
mod tests;
