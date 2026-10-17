//! ssh `ProxyCommand` over cloud-hypervisor's hybrid vsock (unix socket + `CONNECT`).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Longest accepted handshake reply line, in bytes.
const MAX_REPLY: usize = 128;
/// Read timeout while waiting for the handshake reply.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Splice buffer size.
const BUF_SIZE: usize = 32 * 1024;

/// Connects to the guest vsock `port` through `socket`, then splices
/// `input` -> guest and guest -> `output` until the guest closes.
pub fn proxy(
    socket: &Path,
    port: u32,
    input: impl Read + Send + 'static,
    output: impl Write,
) -> Result<(), String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("vsock connect failed: {}", e.kind()))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("vsock handshake failed: {}", e.kind()))?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("vsock handshake failed: {}", e.kind()))?;
    handshake(&mut stream)?;
    stream
        .set_read_timeout(None)
        .map_err(|e| format!("vsock setup failed: {}", e.kind()))?;
    let writer = stream
        .try_clone()
        .map_err(|e| format!("vsock setup failed: {}", e.kind()))?;
    // Not joined: it may stay blocked on input; process exit ends it.
    thread::spawn(move || pump_input(input, writer));
    pump_output(&mut stream, output)
}

/// Reads the reply line one byte at a time so no ssh data is consumed.
fn handshake(stream: &mut UnixStream) -> Result<(), String> {
    let mut line = Vec::with_capacity(MAX_REPLY);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("vsock closed before handshake".to_string()),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("vsock handshake failed: {}", e.kind())),
        }
        let Some(&b) = byte.first() else { continue };
        if b == b'\n' {
            break;
        }
        line.push(b);
        if line.len() >= MAX_REPLY {
            return Err("vsock handshake rejected".to_string());
        }
    }
    if line.starts_with(b"OK") {
        Ok(())
    } else {
        Err("vsock handshake rejected".to_string())
    }
}

/// Copies `input` into the stream, then half-closes the write side.
fn pump_input(mut input: impl Read, mut writer: UnixStream) {
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let Some(chunk) = buf.get(..n) else { break };
                if writer.write_all(chunk).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // Peer may already be gone; nothing useful to do on failure.
    let _ = writer.shutdown(std::net::Shutdown::Write);
}

/// Copies the stream to `output`, flushing after every chunk.
fn pump_output(stream: &mut UnixStream, mut output: impl Write) -> Result<(), String> {
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("vsock read failed: {}", e.kind())),
        };
        let Some(chunk) = buf.get(..n) else {
            return Ok(());
        };
        match output.write_all(chunk).and_then(|()| output.flush()) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(format!("output write failed: {}", e.kind())),
        }
    }
}

#[cfg(test)]
mod tests;
