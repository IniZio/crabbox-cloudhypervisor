//! Launch tests: which processes may outlive the adapter.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::vm::tests::config;
use crate::vm::RealBackend;

#[test]
fn a_lease_process_is_launched_to_outlive_the_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/cb"));
    let args = [OsString::from("--api-socket"), OsString::from("/run/a")];
    let (program, launched) = backend.launch(Path::new("/bin/ch"), &args);
    assert_eq!(program, Path::new("/bin/ch"));
    assert_eq!(launched, args);
}

#[test]
fn a_template_process_is_launched_tied_to_the_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let backend =
        RealBackend::new(config(dir.path()), PathBuf::from("/bin/cb")).dying_with_adapter();
    let args = [OsString::from("--api-socket"), OsString::from("/run/a")];
    let (program, launched) = backend.launch(Path::new("/bin/ch"), &args);
    assert_eq!(program, Path::new("/bin/cb"));
    let me = std::process::id().to_string();
    let want: Vec<OsString> = ["tied", &me, "--", "/bin/ch", "--api-socket", "/run/a"]
        .map(OsString::from)
        .to_vec();
    assert_eq!(launched, want);
}
