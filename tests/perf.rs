//! Native micro-benchmarks isolating the command channel's *inherent* CPU cost
//! (framing + streaming), separate from guest emulation. Run explicitly:
//!   cargo test --release -- --ignored --nocapture perf_
//! These are not correctness gates; they bound the per-byte / per-frame work the
//! agent does so we can reason about guest overhead (executed-instruction count
//! scales with this work, minus emulation constant factors).

use std::time::Instant;
use std::sync::Arc;

use guesthopper::frame;
use guesthopper::session::run_session;
use tokio::io::{split, duplex};

#[tokio::test]
#[ignore]
async fn perf_codec_throughput() {
    // Pure serialize+parse cost of the frame codec, no IO/emulation.
    let frame_payload = 32 * 1024usize; // matches the agent's READ_CHUNK
    let n = 20_000usize; // 20k * 32KiB ~= 640 MiB
    let payload = vec![0xABu8; frame_payload];

    let mut buf: Vec<u8> = Vec::with_capacity(n * (frame_payload + 5));
    let t = Instant::now();
    for _ in 0..n {
        frame::write_frame(&mut buf, frame::FRAME_STDOUT, &payload).await.unwrap();
    }
    let wdur = t.elapsed();

    let total = buf.len() as f64;
    let mut r = &buf[..];
    let mut got = 0usize;
    let t = Instant::now();
    while let Some(f) = frame::read_frame(&mut r).await.unwrap() {
        got += f.payload.len();
    }
    let rdur = t.elapsed();
    assert_eq!(got, n * frame_payload);

    let wgbs = total / wdur.as_secs_f64() / 1e9;
    let rgbs = total / rdur.as_secs_f64() / 1e9;
    println!(
        "[codec] {n} frames x {frame_payload}B = {:.0} MiB | write {:.2} GB/s ({:.0} ns/frame) | read {:.2} GB/s ({:.0} ns/frame) | header overhead 5B/frame = {:.4}%",
        total / 1048576.0,
        wgbs,
        wdur.as_nanos() as f64 / n as f64,
        rgbs,
        rdur.as_nanos() as f64 / n as f64,
        5.0 / frame_payload as f64 * 100.0,
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn perf_exec_stream_throughput() {
    // End-to-end streaming through the agent (framing + mpsc single-writer +
    // pump), on the SAME single-thread runtime the guest uses. The shell emits
    // ~256 MiB via dd; we time from request to EXIT.
    let mib = 256usize;
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        std::time::Duration::from_secs(120),
        std::time::Duration::from_secs(120),
        None,
    ));

    let (mut h_rd, mut h_wr) = split(host);
    let cmd = format!("dd if=/dev/zero bs=1M count={mib} 2>/dev/null");
    let req = serde_json::json!({ "verb": "exec", "cmd": cmd });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();

    let mut out = 0usize;
    let t = Instant::now();
    while let Some(f) = frame::read_frame(&mut h_rd).await.unwrap() {
        match f.ftype {
            frame::FRAME_STDOUT => out += f.payload.len(),
            frame::FRAME_EXIT => break,
            _ => {}
        }
    }
    let dur = t.elapsed();
    let _ = session.await;
    let mbps = out as f64 / 1048576.0 / dur.as_secs_f64();
    println!(
        "[exec-stream] streamed {:.0} MiB through the channel in {:.2}s = {:.0} MiB/s (single-thread runtime)",
        out as f64 / 1048576.0,
        dur.as_secs_f64(),
        mbps,
    );
    assert!(out >= mib * 1024 * 1024);
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn perf_exec_roundtrip_latency() {
    // Per-command overhead. Dominated by fork+exec of the shell (inherent to
    // "run a command", old channel or new); the channel's share is the framing.
    let iters = 300usize;
    let t = Instant::now();
    for _ in 0..iters {
        let (host, guest) = duplex(1 << 16);
        let (g_rd, g_wr) = split(guest);
        let session = tokio::spawn(run_session(
            g_rd,
            g_wr,
            Arc::new("/bin/sh".to_string()),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            None,
        ));
        let (mut h_rd, mut h_wr) = split(host);
        let req = serde_json::json!({ "verb": "exec", "cmd": ":" });
        frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
            .await
            .unwrap();
        while let Some(f) = frame::read_frame(&mut h_rd).await.unwrap() {
            if f.ftype == frame::FRAME_EXIT {
                break;
            }
        }
        let _ = session.await;
    }
    let dur = t.elapsed();
    println!(
        "[exec-roundtrip] {iters} exec ':' round-trips in {:.2}s = {:.0} us/command (incl. shell fork+exec)",
        dur.as_secs_f64(),
        dur.as_micros() as f64 / iters as f64,
    );
}
