#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use super::*;
use std::os::unix::net::UnixListener;
use std::thread::JoinHandle;

/// Builds a canned response with a correct Content-Length.
fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Reads one full request (head plus Content-Length body) as text.
fn read_request(stream: &mut UnixStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "client closed early");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
            let want = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
            if buf.len() >= pos + 4 + want {
                break;
            }
        }
    }
    String::from_utf8(buf).unwrap()
}

/// Serves one canned reply per connection; the handle yields the requests.
fn serve(replies: Vec<String>) -> (tempfile::TempDir, ChClient, JoinHandle<Vec<String>>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ch.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let handle = std::thread::spawn(move || {
        replies
            .into_iter()
            .map(|reply| {
                let (mut stream, _) = listener.accept().unwrap();
                let req = read_request(&mut stream);
                stream.write_all(reply.as_bytes()).unwrap();
                req
            })
            .collect()
    });
    (dir, ChClient::new(&path), handle)
}

#[test]
fn ping_parses_json() {
    let (_d, c, h) = serve(vec![http("200 OK", r#"{"version":"v1"}"#)]);
    assert_eq!(c.ping().unwrap()["version"], "v1");
    let req = &h.join().unwrap()[0];
    assert!(req.starts_with("GET /api/v1/vmm.ping HTTP/1.1\r\n"));
    assert!(req.contains("Host: localhost\r\n"));
    assert!(req.contains("Accept: application/json\r\n"));
    assert!(req.contains("Connection: close\r\n"));
}

#[test]
fn create_sends_body_and_accepts_204() {
    let (_d, c, h) = serve(vec!["HTTP/1.1 204 No Content\r\n\r\n".to_owned()]);
    let cfg = serde_json::json!({"cpus": {"boot_vcpus": 2}});
    c.create(&cfg).unwrap();
    let req = &h.join().unwrap()[0];
    let body = cfg.to_string();
    assert!(req.starts_with("PUT /api/v1/vm.create HTTP/1.1\r\n"));
    assert!(req.contains("Content-Type: application/json\r\n"));
    assert!(req.contains(&format!("Content-Length: {}\r\n", body.len())));
    assert!(req.ends_with(&body));
}

#[test]
fn put_without_body_sends_zero_length() {
    let (_d, c, h) = serve(vec!["HTTP/1.1 204 No Content\r\n\r\n".to_owned()]);
    c.boot().unwrap();
    let req = &h.join().unwrap()[0];
    assert!(req.starts_with("PUT /api/v1/vm.boot HTTP/1.1\r\n"));
    assert!(req.contains("Content-Length: 0\r\n"));
}

#[test]
fn chunked_body_is_decoded() {
    let reply = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"state\r\n8\r\n\":\"Run\"}\r\n0\r\n\r\n";
    let (_d, c, _h) = serve(vec![reply.to_owned()]);
    assert_eq!(c.info().unwrap()["state"], "Run");
}

#[test]
fn error_joins_json_messages() {
    let reply = http("500 Internal Server Error", r#"["outer","inner"]"#);
    let (_d, c, _h) = serve(vec![reply]);
    assert_eq!(
        c.boot().unwrap_err(),
        "cloud-hypervisor vm.boot: HTTP 500: outer: inner"
    );
}

#[test]
fn error_falls_back_to_truncated_raw_body() {
    let (_d, c, _h) = serve(vec![http("400 Bad Request", "  nope  ")]);
    assert_eq!(
        c.boot().unwrap_err(),
        "cloud-hypervisor vm.boot: HTTP 400: nope"
    );
    let long = "x".repeat(500);
    assert_eq!(
        error_message("e", 400, &long),
        format!("cloud-hypervisor e: HTTP 400: {}", "x".repeat(300))
    );
}

#[test]
fn shutdown_tolerates_not_running() {
    let reply = http(
        "500 Internal Server Error",
        r#"["Error from API","VM is not running"]"#,
    );
    let (_d, c, _h) = serve(vec![reply]);
    c.shutdown().unwrap();
}

#[test]
fn shutdown_other_500_is_error() {
    let (_d, c, _h) = serve(vec![http("500 Internal Server Error", r#"["boom"]"#)]);
    assert!(c.shutdown().unwrap_err().contains("HTTP 500: boom"));
}

#[test]
fn delete_tolerates_not_created() {
    let reply = http("500 Internal Server Error", r#"["VM is not created"]"#);
    let (_d, c, _h) = serve(vec![reply]);
    c.delete().unwrap();
}

#[test]
fn unreachable_socket_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let c = ChClient::new(&dir.path().join("missing.sock")).with_timeout(Duration::from_secs(1));
    let err = c.vmm_shutdown().unwrap_err();
    assert!(
        err.starts_with("cloud-hypervisor api unreachable: "),
        "{err}"
    );
}
