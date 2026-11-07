//! Makes a fork's cache overlay by copying only what a checkpoint's chain
//! allocated above its base. A safe rebase gets the same result by reading the
//! whole virtual size of both sides, which costs seconds per generation for a
//! delta of a few MiB.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::host;

/// One image of a backing chain, as `qemu-img info` reports it.
#[derive(Debug, Deserialize)]
pub(super) struct ChainImage {
    /// Where it is.
    filename: PathBuf,
}

/// The chain `top` reads through, top first.
pub(super) fn read_chain(qemu_img: &Path, top: &Path) -> Result<Vec<ChainImage>, String> {
    let what = "read the chain";
    let out = Command::new(qemu_img)
        .args(["info", "--output=json", "--backing-chain"])
        .arg(top)
        .output()
        .map_err(|e| format!("{what}: cannot run {}: {e}", qemu_img.display()))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what}: {}", said.trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("{what}: {e}"))
}

/// Whether `chain` ends in the very file `base`, so what it left unallocated
/// reads the same through a link of `base`.
pub(super) fn ends_in(chain: &[ChainImage], base: &Path) -> Result<bool, String> {
    let inode = |path: &Path| {
        fs::metadata(path)
            .map(|m| (m.dev(), m.ino()))
            .map_err(|e| format!("stat {}: {e}", path.display()))
    };
    let Some(bottom) = chain.last() else {
        return Ok(false);
    };
    Ok(inode(&bottom.filename)? == inode(base)?)
}

/// Writes `image`, one overlay over `base` that reads as the top of `chain`.
/// On failure it leaves no file behind.
pub(super) fn commit_chain(
    qemu_img: &Path,
    chain: &[ChainImage],
    base: &Path,
    image: &Path,
) -> Result<(), String> {
    let scratch = image.with_extension("top.qcow2");
    let made = commit_through(qemu_img, chain, base, image, &scratch);
    let _ = fs::remove_file(&scratch);
    if made.is_err() {
        let _ = fs::remove_file(image);
    }
    made
}

/// [`commit_chain`], with `scratch` the throwaway top commit opens writable, so
/// it never asks to write a layer.
fn commit_through(
    qemu_img: &Path,
    chain: &[ChainImage],
    base: &Path,
    image: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let what = "commit the chain";
    let layers = chain.split_last().map_or(&[][..], |(_, layers)| layers);
    let top = layers
        .first()
        .ok_or_else(|| format!("{what}: the chain has no layer"))?;
    // Version 2 has no zero clusters. cloud-hypervisor reads a zero cluster as
    // the backing file's data, and in this format commit writes zeros as data.
    host::qemu_img(
        qemu_img,
        what,
        Command::new(qemu_img)
            .args([
                "create",
                "-q",
                "-f",
                "qcow2",
                "-o",
                "compat=0.10",
                "-F",
                "raw",
                "-b",
            ])
            .arg(base)
            .arg(image),
    )?;
    host::make_overlay(qemu_img, what, &top.filename, "qcow2", scratch)?;
    let stack = stacked(layers, image, scratch);
    host::qemu_img(
        qemu_img,
        what,
        Command::new(qemu_img)
            .args(["commit", "-q", "-d", "-b"])
            .arg(image)
            .arg(format!("json:{stack}")),
    )
}

/// The block graph of `scratch` over `layers` over `image`. Layers never change
/// and forks read them at once, so they take no lock, which commit would hold
/// for writing.
fn stacked(layers: &[ChainImage], image: &Path, scratch: &Path) -> Value {
    let plain = |path: &Path| json!({"driver": "file", "filename": path});
    let over =
        |file: Value, backing: Value| json!({"driver": "qcow2", "file": file, "backing": backing});
    let bottom = json!({"driver": "qcow2", "file": plain(image)});
    let layered = layers.iter().rev().fold(bottom, |below, layer| {
        let file = json!({"driver": "file", "filename": layer.filename, "locking": "off"});
        over(file, below)
    });
    over(plain(scratch), layered)
}

#[cfg(test)]
mod tests;
