//! Development/test stand-in for a guest: serves the real agent session code
//! over a Unix socket that speaks vhost-device-vsock's hybrid handshake
//! (`CONNECT <port>\n` -> `OK <port>\n`), so the host-side clients
//! (guest_cmd.py, the telnet/ssh gateways, guest_fs.py) can be exercised end to
//! end against this machine's filesystem with no VM and no vsock.
//!
//!   cargo run --example uds_agent -- /tmp/x/vsocket
//!
//! Used by tests/test_scp_e2e.py. Never shipped to a guest.

use std::sync::Arc;
use std::time::Duration;

use guesthopper::session::run_session;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: uds_agent <socket-path>")?;
    let listener = UnixListener::bind(&path)?;
    eprintln!("uds_agent: listening on {path}");
    let shell = Arc::new("/bin/sh".to_string());
    loop {
        let (mut stream, _) = listener.accept().await?;
        let shell = Arc::clone(&shell);
        tokio::spawn(async move {
            // Read the handshake line byte-wise so no frame bytes are consumed.
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            while line.len() < 64 {
                match stream.read(&mut b).await {
                    Ok(1) if b[0] != b'\n' => line.push(b[0]),
                    Ok(1) => break,
                    _ => return,
                }
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            let port = match line.strip_prefix("CONNECT ") {
                Some(p) => p.to_string(),
                None => return,
            };
            if stream.write_all(format!("OK {port}\n").as_bytes()).await.is_err() {
                return;
            }
            let (reader, writer) = tokio::io::split(stream);
            let _ = run_session(
                reader,
                writer,
                shell,
                Duration::from_secs(30),
                Duration::from_secs(60),
                None,
            )
            .await;
            eprintln!("uds_agent: session ended");
        });
    }
}
