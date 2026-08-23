//! Binary-safe, length-prefixed framing for the guest command channel.
//!
//! Wire format of one frame: `[type: u8][len: u32 big-endian][payload: len bytes]`.
//! The payload is opaque bytes -- data frames (STDOUT/STDERR/STDIN) carry raw
//! bytes and are binary-safe; control frames (REQUEST/EXIT/ERROR/...) carry a
//! small JSON object. This replaces the old one-shot 64 KB / `from_utf8_lossy`
//! path, which could neither stream nor carry binary data.

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Host -> guest.
pub const FRAME_REQUEST: u8 = 1;
pub const FRAME_STDIN: u8 = 2;
pub const FRAME_STDIN_EOF: u8 = 3;
/// Client->agent liveness keepalive. Carries no payload and needs no reply: the
/// agent treats *any* received frame as proof the client is alive, so a PING
/// arriving on an otherwise-idle session just resets the read deadline. Lets the
/// agent detect an abrupt client death that the vsock transport does not surface
/// as a read EOF or a write error.
pub const FRAME_PING: u8 = 4;
// SIGNAL=4 reserved for a later slice.
pub const FRAME_RESIZE: u8 = 5; // pty window size {rows, cols}

// Guest -> host.
pub const FRAME_STDOUT: u8 = 16;
pub const FRAME_STDERR: u8 = 17;
pub const FRAME_EXIT: u8 = 18;
pub const FRAME_ERROR: u8 = 19;

/// Absolute hard ceiling on a frame payload. The wire length is a `u32`, so this
/// also guards `write_frame`'s `as u32` cast against silent truncation. The
/// *inbound* accept limit is separate and normally much smaller (see
/// [`max_inbound_frame_len`]); this is only the ceiling that limit may be raised
/// to.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Default cap on a single *inbound* frame the agent will allocate for. A hostile
/// or buggy peer can otherwise make the agent `vec![0u8; len]` up to the header's
/// advertised length for every concurrent session at once; at the old 16 MiB that
/// was ~1 GiB across the default 64 sessions -- and a single 16 MiB frame alone
/// can OOM a 32 MiB firmware guest. Real inbound frames are tiny (keystrokes, a
/// resize JSON, an exec string, or a <=32 KiB stdin chunk), so 1 MiB is ~16x
/// headroom over anything legitimate while shrinking the worst case ~16x.
pub const DEFAULT_MAX_INBOUND_FRAME_LEN: usize = 1024 * 1024;

/// Floor for the configurable inbound cap: still comfortably clears any control
/// JSON and a full stdin chunk, so a mis-set tiny value can't wedge legitimate
/// use.
pub const MIN_INBOUND_FRAME_LEN: usize = 64 * 1024;

/// The live inbound cap, set once at startup from the environment (see
/// `main.rs`). An `AtomicUsize` rather than a threaded parameter because it is a
/// genuinely process-global limit; `read_frame` reads it with `Relaxed` ordering
/// (a stale read is harmless -- it is only ever set before any session accepts).
static MAX_INBOUND_FRAME_LEN: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_INBOUND_FRAME_LEN);

// Compile-time invariants: the default must sit within the clamp bounds.
const _: () = assert!(DEFAULT_MAX_INBOUND_FRAME_LEN >= MIN_INBOUND_FRAME_LEN);
const _: () = assert!(DEFAULT_MAX_INBOUND_FRAME_LEN <= MAX_FRAME_LEN);

/// Clamp a requested cap to `[MIN_INBOUND_FRAME_LEN, MAX_FRAME_LEN]`. Pure so it
/// is testable without touching the shared global.
fn clamp_inbound(v: usize) -> usize {
    v.clamp(MIN_INBOUND_FRAME_LEN, MAX_FRAME_LEN)
}

/// Set the inbound frame cap, clamped so neither a hostile-small nor an
/// overflowing value can be installed. Call once at startup, before accepting
/// connections.
pub fn set_max_inbound_frame_len(v: usize) {
    MAX_INBOUND_FRAME_LEN.store(clamp_inbound(v), Ordering::Relaxed);
}

/// The current inbound frame cap.
pub fn max_inbound_frame_len() -> usize {
    MAX_INBOUND_FRAME_LEN.load(Ordering::Relaxed)
}

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
    let cap = max_inbound_frame_len();
    if len > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload {len} exceeds inbound cap {cap}"),
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
    // Guard the write side the same way `read_frame` guards the read side: a
    // payload over MAX_FRAME_LEN would truncate in the `as u32` length header
    // and desync the stream. Not reachable today (our chunks are READ_CHUNK),
    // but the cast must not silently lie about the length.
    if payload.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload {} exceeds MAX_FRAME_LEN {MAX_FRAME_LEN}", payload.len()),
        ));
    }
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

    #[test]
    fn inbound_cap_is_clamped_to_sane_bounds() {
        // A hostile-small request is floored (can't wedge legitimate frames);
        // an overflowing request is capped at the hard ceiling; an in-range
        // value passes through.
        assert_eq!(clamp_inbound(0), MIN_INBOUND_FRAME_LEN);
        assert_eq!(clamp_inbound(1), MIN_INBOUND_FRAME_LEN);
        assert_eq!(clamp_inbound(usize::MAX), MAX_FRAME_LEN);
        assert_eq!(clamp_inbound(2 * 1024 * 1024), 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn frame_over_default_inbound_cap_is_rejected() {
        // A header advertising 2 MiB -- well under the 16 MiB hard ceiling but
        // over the 1 MiB default inbound cap -- must be refused, not allocated.
        // (Uses the default cap so it never mutates the shared global and stays
        // parallel-safe with other tests.)
        let (a, b) = tokio::io::duplex(64);
        let (_a_rd, mut a_wr) = tokio::io::split(a);
        let (mut b_rd, _b_wr) = tokio::io::split(b);
        let mut hdr = [0u8; 5];
        hdr[0] = FRAME_STDIN;
        hdr[1..5].copy_from_slice(&(2u32 * 1024 * 1024).to_be_bytes());
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = a_wr.write_all(&hdr).await;
        });
        let err = read_frame(&mut b_rd).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
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
