//! Executables for tests, written so that exec never fails with ETXTBSY.
//!
//! The test harness forks from many threads. A forked child holds every fd
//! open at fork time until it execs, so a file this process just wrote can
//! still have a writer when a test execs it. Closing or renaming the file does
//! not help: the leaked fd is on the inode. So the bytes are written by a
//! short-lived child process, whose fds no test thread can inherit.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `#!/bin/sh` and `body` to `path` as a 0755 executable.
pub(crate) fn script(path: &Path, body: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat >"$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success(), "write {}", path.display());
}

/// A process that blocks until its stdin closes, with comm set to `path`'s
/// file name.
pub(crate) fn idle_as(path: &Path) -> Command {
    // A link to /bin/sleep does not work here: a multicall coreutils such as
    // uutils exits at once when run under a name it does not know.
    script(path, "read _");
    let mut idle = Command::new(path);
    idle.stdin(Stdio::piped());
    idle
}
