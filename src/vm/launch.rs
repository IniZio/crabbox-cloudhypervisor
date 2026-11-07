//! How the real backend launches cloud-hypervisor and passt.

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::RealBackend;

impl RealBackend {
    /// A backend whose processes the kernel kills when this adapter exits.
    pub(super) const fn dying_with_adapter(mut self) -> Self {
        self.dies_with_adapter = true;
        self
    }

    /// Program and arguments that start `program args` for this backend.
    /// Through `<this exe> tied` when they must die with this adapter: the
    /// spawning thread runs the whole build, so it outlives every process.
    pub(super) fn launch(&self, program: &Path, args: &[OsString]) -> (PathBuf, Vec<OsString>) {
        if !self.dies_with_adapter {
            return (program.to_path_buf(), args.to_vec());
        }
        let parent = std::process::id().to_string();
        let mut tied: Vec<OsString> = ["tied", &parent, "--"].map(OsString::from).to_vec();
        tied.push(program.as_os_str().to_owned());
        tied.extend_from_slice(args);
        (self.exe.clone(), tied)
    }
}
