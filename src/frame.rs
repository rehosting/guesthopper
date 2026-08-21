//! Binary-safe, length-prefixed framing for the guest command channel.
//!
//! Wire format of one frame: `[type: u8][len: u32 big-endian][payload: len bytes]`.
//! The payload is opaque bytes -- data frames (STDOUT/STDERR/STDIN) carry raw
//! bytes and are binary-safe; control frames (REQUEST/EXIT/ERROR/...) carry a
//! small JSON object. This replaces the old one-shot 64 KB / `from_utf8_lossy`
//! path, which could neither stream nor carry binary data.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Host -> guest.
pub const FRAME_REQUEST: u8 = 1;
pub const FRAME_STDIN: u8 = 2;
pub const FRAME_STDIN_EOF: u8 = 3;
// SIGNAL=4 reserved for a later slice.
pub const FRAME_RESIZE: u8 = 5; // pty window size {rows, cols}

// Guest -> host.
pub const FRAME_STDOUT: u8 = 16;
pub const FRAME_STDERR: u8 = 17;
pub const FRAME_EXIT: u8 = 18;
pub const FRAME_ERROR: u8 = 19;

/// Upper bound on a single frame's payload, to cap the allocation a peer can
/// force. 16 MiB comfortably clears any control JSON or reasonable I/O chunk.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ftype: u8,
    pub payload: Vec<u8>,
}

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary
/// (peer closed), which callers treat as "no more input".
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
    let mut hdr = [0u8; 5];
    match r.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let ftype = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload {len} exceeds MAX_FRAME_LEN {MAX_FRAME_LEN}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Some(Frame { ftype, payload }))
}

/// Write one frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    ftype: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut hdr = [0u8; 5];
    hdr[0] = ftype;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_binary_safe_and_large() {
        // NUL and high bytes must survive, and a payload larger than the old
        // 64 KB one-shot cap must round-trip intact.
        let mut payload = vec![0u8, 255u8, 1u8, 0u8, 254u8];
        payload.extend((0..100_000u32).map(|i| (i % 256) as u8));

        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_rd, mut a_wr) = tokio::io::split(a);
        let (mut b_rd, _b_wr) = tokio::io::split(b);

        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a_wr, FRAME_STDOUT, &p2).await.unwrap();
        });

        let f = read_frame(&mut b_rd).await.unwrap().expect("frame");
        writer.await.unwrap();
        assert_eq!(f.ftype, FRAME_STDOUT);
        assert_eq!(f.payload, payload);

        // No further frame written -> clean EOF is None, not an error.
        drop(a_rd);
        assert!(read_frame(&mut b_rd).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversize_len_is_rejected() {
        // A header claiming a huge payload must error instead of allocating.
        let (a, b) = tokio::io::duplex(64);
        let (_a_rd, mut a_wr) = tokio::io::split(a);
        let (mut b_rd, _b_wr) = tokio::io::split(b);
        let mut hdr = [0u8; 5];
        hdr[0] = FRAME_STDOUT;
        hdr[1..5].copy_from_slice(&(u32::MAX).to_be_bytes());
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = a_wr.write_all(&hdr).await;
        });
        let err = read_frame(&mut b_rd).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
