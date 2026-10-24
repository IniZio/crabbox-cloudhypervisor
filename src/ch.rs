//! Minimal HTTP/1.1 client for the cloud-hypervisor API over a unix socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Default read/write timeout for API calls.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest raw error body echoed into an error message, in chars.
const MAX_ERROR_CHARS: usize = 300;
/// Separator between a response head and its body.
const HEAD_END: &[u8] = b"\r\n\r\n";

/// Client for one cloud-hypervisor API socket.
#[derive(Debug, Clone)]
pub struct ChClient {
    /// Path of the API unix socket.
    socket: PathBuf,
    /// Read/write timeout applied to each connection.
    timeout: Duration,
}

impl ChClient {
    /// Creates a client for the API socket at `socket`.
    #[must_use]
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Overrides the read/write timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sends one request and returns the HTTP status and body.
    pub fn request(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("cloud-hypervisor api unreachable: {}", e.kind()))?;
        let io_err = |e: std::io::Error| format!("cloud-hypervisor api io error: {}", e.kind());
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(io_err)?;
        stream
            .write_all(build_request(method, endpoint, body).as_bytes())
            .map_err(io_err)?;
        let raw = read_response(&mut stream).map_err(io_err)?;
        parse_response(&raw)
    }

    /// Sends a request and returns the body of a 2xx response.
    fn call(&self, method: &str, endpoint: &str, body: Option<&str>) -> Result<String, String> {
        let (status, text) = self.request(method, endpoint, body)?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(error_message(endpoint, status, &text))
        }
    }

    /// Like `call`, but HTTP 500 whose messages contain a needle counts as success.
    fn call_tolerant(&self, method: &str, endpoint: &str, needles: &[&str]) -> Result<(), String> {
        let (status, text) = self.request(method, endpoint, None)?;
        let message = error_message(endpoint, status, &text);
        let lower = message.to_lowercase();
        if (200..300).contains(&status)
            || (status == 500 && needles.iter().any(|n| lower.contains(n)))
        {
            Ok(())
        } else {
            Err(message)
        }
    }

    /// Calls an endpoint and parses its JSON response.
    fn call_json(&self, method: &str, endpoint: &str) -> Result<Value, String> {
        let text = self.call(method, endpoint, None)?;
        serde_json::from_str(&text)
            .map_err(|e| format!("cloud-hypervisor {endpoint}: invalid JSON response: {e}"))
    }

    /// `GET vmm.ping`.
    pub fn ping(&self) -> Result<Value, String> {
        self.call_json("GET", "vmm.ping")
    }

    /// `PUT vm.create` with the VM config.
    pub fn create(&self, config: &Value) -> Result<(), String> {
        self.call("PUT", "vm.create", Some(&config.to_string()))
            .map(drop)
    }

    /// `PUT vm.boot`.
    pub fn boot(&self) -> Result<(), String> {
        self.call("PUT", "vm.boot", None).map(drop)
    }

    /// `GET vm.info`.
    pub fn info(&self) -> Result<Value, String> {
        self.call_json("GET", "vm.info")
    }

    /// `PUT vm.pause`.
    pub fn pause(&self) -> Result<(), String> {
        self.call("PUT", "vm.pause", None).map(drop)
    }

    /// `PUT vm.resume`.
    pub fn resume(&self) -> Result<(), String> {
        self.call("PUT", "vm.resume", None).map(drop)
    }

    /// `PUT vm.resize`; `None` leaves that resource unchanged.
    pub fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String> {
        let mut body = serde_json::Map::new();
        if let Some(n) = vcpus {
            body.insert("desired_vcpus".to_owned(), n.into());
        }
        if let Some(n) = ram_bytes {
            body.insert("desired_ram".to_owned(), n.into());
        }
        let body = Value::Object(body).to_string();
        self.call("PUT", "vm.resize", Some(&body)).map(drop)
    }

    /// `PUT vm.resize-disk`: grows disk `id` to `bytes` while the VM runs.
    pub fn resize_disk(&self, id: &str, bytes: u64) -> Result<(), String> {
        let body = serde_json::json!({"id": id, "desired_size": bytes}).to_string();
        self.call("PUT", "vm.resize-disk", Some(&body)).map(drop)
    }

    /// `GET vm.info` reduced to its `state` string (`Running`, `Paused`, ...).
    pub fn state(&self) -> Result<String, String> {
        self.info()?
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "cloud-hypervisor vm.info: missing state".to_owned())
    }

    /// `PUT vm.power-button`: ACPI power button, so the guest shuts down cleanly.
    pub fn power_button(&self) -> Result<(), String> {
        self.call("PUT", "vm.power-button", None).map(drop)
    }

    /// `PUT vm.shutdown`; an already stopped VM is not an error.
    pub fn shutdown(&self) -> Result<(), String> {
        self.call_tolerant("PUT", "vm.shutdown", &["not running", "not booted"])
    }

    /// `PUT vm.delete`; an already deleted VM is not an error.
    pub fn delete(&self) -> Result<(), String> {
        self.call_tolerant("PUT", "vm.delete", &["not created", "not running"])
    }

    /// `PUT vmm.shutdown`.
    pub fn vmm_shutdown(&self) -> Result<(), String> {
        self.call("PUT", "vmm.shutdown", None).map(drop)
    }
}

/// Renders the request head and body.
fn build_request(method: &str, endpoint: &str, body: Option<&str>) -> String {
    let tail = match body {
        Some(b) => format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        ),
        None if method == "PUT" => "Content-Length: 0\r\n\r\n".to_owned(),
        None => "\r\n".to_owned(),
    };
    format!(
        "{method} /api/v1/{endpoint} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n{tail}"
    )
}

/// Builds the error text for a non-2xx response.
fn error_message(endpoint: &str, status: u16, body: &str) -> String {
    let detail = match serde_json::from_str::<Vec<String>>(body) {
        Ok(msgs) if !msgs.is_empty() => msgs.join(": "),
        _ => body.trim().chars().take(MAX_ERROR_CHARS).collect(),
    };
    format!("cloud-hypervisor {endpoint}: HTTP {status}: {detail}")
}

/// Parsed response head.
struct Head {
    /// HTTP status code.
    status: u16,
    /// Whether the body uses chunked transfer encoding.
    chunked: bool,
    /// Declared `Content-Length`, if any.
    length: Option<usize>,
}

/// Parses the head; `None` while the head terminator has not arrived.
fn parse_head(raw: &[u8]) -> Result<Option<(Head, &[u8])>, String> {
    let bad = || "cloud-hypervisor api: malformed response".to_owned();
    let Some(split) = raw.windows(HEAD_END.len()).position(|w| w == HEAD_END) else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(raw.get(..split).ok_or_else(bad)?).into_owned();
    let rest = raw
        .get(split.saturating_add(HEAD_END.len())..)
        .ok_or_else(bad)?;
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(bad)?;
    let (mut chunked, mut length) = (false, None);
    for (name, value) in lines.filter_map(|l| l.split_once(':')) {
        let value = value.trim();
        match name.trim().to_lowercase().as_str() {
            "transfer-encoding" => chunked = value.to_lowercase().contains("chunked"),
            "content-length" => length = value.parse::<usize>().ok(),
            _ => {}
        }
    }
    Ok(Some((
        Head {
            status,
            chunked,
            length,
        },
        rest,
    )))
}

/// Whether `raw` holds a whole response, without waiting for EOF.
fn is_complete(raw: &[u8]) -> bool {
    match parse_head(raw) {
        Ok(Some((head, rest))) => {
            if head.chunked {
                decode_chunked(rest).is_ok()
            } else if let Some(n) = head.length {
                rest.len() >= n
            } else {
                matches!(head.status, 100..=199 | 204 | 304)
            }
        }
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Reads until the response is complete or the peer closes.
fn read_response(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(buf.get(..n).unwrap_or_default());
        if is_complete(&raw) {
            return Ok(raw);
        }
    }
}

/// Splits a raw response into status and decoded body.
fn parse_response(raw: &[u8]) -> Result<(u16, String), String> {
    let bad = || "cloud-hypervisor api: malformed response".to_owned();
    let (head, rest) = parse_head(raw)?.ok_or_else(bad)?;
    let body = if head.chunked {
        decode_chunked(rest)?
    } else {
        let end = head.length.map_or(rest.len(), |n| n.min(rest.len()));
        rest.get(..end).ok_or_else(bad)?.to_vec()
    };
    Ok((head.status, String::from_utf8_lossy(&body).into_owned()))
}

/// Decodes a `Transfer-Encoding: chunked` body.
fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "cloud-hypervisor api: malformed chunked body".to_owned();
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let line = std::str::from_utf8(data.get(..eol).ok_or_else(bad)?).map_err(|_| bad())?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| bad())?;
        data = data.get(eol.saturating_add(2)..).ok_or_else(bad)?;
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(data.get(..size).ok_or_else(bad)?);
        data = data.get(size.saturating_add(2)..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests;
