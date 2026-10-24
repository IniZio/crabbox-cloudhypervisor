//! Host-side e2fsprogs calls on cache images. The host never mounts an image.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{LOGIN_GID, LOGIN_UID};

/// Programs this module runs, for `doctor`.
pub const TOOLS: [&str; 3] = ["mke2fs", "e2fsck", "resize2fs"];
/// Journal size in MiB: below the publish threshold, so journal churn alone never publishes.
const JOURNAL_MIB: u64 = 16;
/// Superblock offset in the image.
const SUPERBLOCK: u64 = 1024;
/// Offset of `s_feature_incompat` inside the superblock.
const INCOMPAT_OFFSET: u64 = 0x60;
/// `EXT4_FEATURE_INCOMPAT_RECOVER`: the journal still holds unreplayed writes.
const INCOMPAT_RECOVER: u32 = 0x4;

/// Block usage reported by `e2fsck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Blocks in use.
    pub used: u64,
    /// Blocks in the filesystem.
    pub total: u64,
}

impl Usage {
    /// True when at least `percent` of the blocks are in use.
    #[must_use]
    pub const fn at_least(self, percent: u64) -> bool {
        self.used.saturating_mul(100) >= self.total.saturating_mul(percent)
    }
}

/// Runs `program args...`; Ok(stdout) when the exit code is in `ok_codes`.
fn run(program: &str, args: &[&str], path: &Path, ok_codes: &[i32]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {}", e.kind()))?;
    let code = output.status.code().unwrap_or(-1);
    if ok_codes.contains(&code) {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
    Err(format!(
        "{program} exited {code}: {}",
        last.unwrap_or_default().trim()
    ))
}

/// Creates an empty sparse ext4 image of `bytes` at `path`, its root owned by the login user.
pub fn make(path: &Path, bytes: u64) -> Result<(), String> {
    File::create(path)
        .and_then(|f| f.set_len(bytes))
        .map_err(|e| format!("create image: {}", e.kind()))?;
    let owner = format!("root_owner={LOGIN_UID}:{LOGIN_GID}");
    let journal = format!("size={JOURNAL_MIB}");
    let args = [
        "-q", "-F", "-t", "ext4", "-b", "4096", "-m", "0", "-J", &journal, "-E", &owner,
    ];
    run("mke2fs", &args, path, &[0]).map(drop)
}

/// Read-only full check; the usage on success, `Err` on any problem found.
pub fn check(path: &Path) -> Result<Usage, String> {
    let out = run("e2fsck", &["-fn"], path, &[0])?;
    out.lines()
        .rev()
        .find_map(parse_usage)
        .ok_or_else(|| "e2fsck printed no block summary".to_owned())
}

/// Parses `<name>: 11/65536 files (0.0% non-contiguous), 9001/262144 blocks`.
#[must_use]
pub fn parse_usage(line: &str) -> Option<Usage> {
    let blocks = line.trim().strip_suffix(" blocks")?;
    let (_, pair) = blocks.rsplit_once(' ')?;
    let (used, total) = pair.split_once('/')?;
    Some(Usage {
        used: used.parse().ok()?,
        total: total.parse().ok()?,
    })
}

/// Grows the private copy at `path` to `bytes` (never shrinks): repair, extend, resize.
pub fn grow(path: &Path, bytes: u64) -> Result<(), String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("stat image: {}", e.kind()))?
        .len();
    if len >= bytes {
        return Ok(());
    }
    run("e2fsck", &["-fy"], path, &[0, 1])?;
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_len(bytes))
        .map_err(|e| format!("extend image: {}", e.kind()))?;
    run("resize2fs", &[], path, &[0]).map(drop)
}

/// Bytes the image occupies on the host (allocated blocks, not its length).
pub fn allocated(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|m| m.blocks().saturating_mul(512))
        .map_err(|e| format!("stat image: {}", e.kind()))
}

/// True when the journal still needs replay: the guest did not unmount cleanly.
pub fn needs_recovery(path: &Path) -> Result<bool, String> {
    let mut word = [0u8; 4];
    File::open(path)
        .and_then(|mut f| {
            f.seek(SeekFrom::Start(SUPERBLOCK + INCOMPAT_OFFSET))?;
            f.read_exact(&mut word)
        })
        .map_err(|e| format!("read superblock: {}", e.kind()))?;
    Ok(u32::from_le_bytes(word) & INCOMPAT_RECOVER != 0)
}

#[cfg(test)]
mod tests;
