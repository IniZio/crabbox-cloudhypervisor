use super::*;
use std::io::{BufRead, BufReader, Cursor};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

/// Starts a fake server; `reply` is written right after reading the request line.
fn serve(dir: &Path, reply: &'static [u8], echo: bool) -> (PathBuf, thread::JoinHandle<String>) {
    let path = dir.join("v.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let handle = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if !reply.is_empty() {
            conn.write_all(reply).unwrap();
        }
        if echo {
            let mut data = Vec::new();
            reader.read_to_end(&mut data).unwrap();
            conn.write_all(&data).unwrap();
        }
        line
    });
    (path, handle)
}

#[test]
fn splices_without_overread() {
    let dir = tempfile::tempdir().unwrap();
    let (path, handle) = serve(dir.path(), b"OK 1073741824\nSSH-2.0-fake\r\n", true);
    let mut out = Vec::new();
    proxy(&path, 22, Cursor::new(b"hello".to_vec()), &mut out).unwrap();
    assert_eq!(handle.join().unwrap(), "CONNECT 22\n");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("SSH-2.0-fake\r\n"));
    assert!(text.ends_with("hello"));
    assert!(!text.contains("OK"));
}

#[test]
fn rejected_handshake_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (path, handle) = serve(dir.path(), b"ERR\n", false);
    let err = proxy(&path, 22, Cursor::new(Vec::new()), Vec::new()).unwrap_err();
    assert_eq!(err, "vsock handshake rejected");
    handle.join().unwrap();
}

#[test]
fn closed_before_handshake_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (path, handle) = serve(dir.path(), b"", false);
    let err = proxy(&path, 22, Cursor::new(Vec::new()), Vec::new()).unwrap_err();
    assert_eq!(err, "vsock closed before handshake");
    handle.join().unwrap();
}

#[test]
fn missing_socket_errors() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let err = proxy(&missing, 22, Cursor::new(Vec::new()), Vec::new()).unwrap_err();
    assert!(err.starts_with("vsock connect failed"));
}
