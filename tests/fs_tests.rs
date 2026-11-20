//! Integration tests for the `fs` verb (src/files.rs), driven over an in-memory
//! duplex like session_tests.rs -- no vsock or guest required. These pin the
//! wire contract the host-side client (guest_fs.py) relies on: one in-order
//! response per request, binary-safe data and paths, portable error kinds.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use guesthopper::frame::{self, Frame};
use guesthopper::session::run_session;
use serde_json::{json, Value};
use tokio::io::{duplex, split, DuplexStream, ReadHalf, WriteHalf};

/// A scratch directory unique to one test, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "guesthopper-fs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hp(p: &Path) -> String {
    hex(p.as_os_str().as_bytes())
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Host end of one `fs` session.
struct Client {
    rd: ReadHalf<DuplexStream>,
    wr: WriteHalf<DuplexStream>,
}

impl Client {
    async fn start() -> Client {
        let (host, guest) = duplex(1 << 20);
        let (g_rd, g_wr) = split(guest);
        tokio::spawn(run_session(
            g_rd,
            g_wr,
            Arc::new("/bin/sh".to_string()),
            Duration::from_secs(60),
            Duration::from_secs(60),
            None,
        ));
        let (rd, mut wr) = split(host);
        frame::write_frame(&mut wr, frame::FRAME_REQUEST, br#"{"verb":"fs"}"#)
            .await
            .unwrap();
        Client { rd, wr }
    }

    async fn send(&mut self, hdr: Value, data: &[u8]) {
        let hdr = serde_json::to_vec(&hdr).unwrap();
        let mut payload = (hdr.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(&hdr);
        payload.extend_from_slice(data);
        frame::write_frame(&mut self.wr, frame::FRAME_FS_REQ, &payload)
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> (Value, Vec<u8>) {
        let Frame { ftype, payload } = frame::read_frame(&mut self.rd).await.unwrap().expect("frame");
        assert_eq!(ftype, frame::FRAME_FS_RESP, "payload: {:?}", String::from_utf8_lossy(&payload));
        let n = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let hdr = serde_json::from_slice(&payload[4..4 + n]).unwrap();
        (hdr, payload[4 + n..].to_vec())
    }

    async fn call(&mut self, hdr: Value, data: &[u8]) -> (Value, Vec<u8>) {
        self.send(hdr, data).await;
        self.recv().await
    }

    /// A call that must succeed.
    async fn ok(&mut self, hdr: Value, data: &[u8]) -> (Value, Vec<u8>) {
        let (h, d) = self.call(hdr.clone(), data).await;
        assert_eq!(h["ok"], true, "request {hdr} failed: {h}");
        (h, d)
    }

    /// A call that must fail; returns the error kind.
    async fn err(&mut self, hdr: Value) -> String {
        let (h, _) = self.call(hdr.clone(), b"").await;
        assert_eq!(h["ok"], false, "request {hdr} unexpectedly succeeded: {h}");
        h["kind"].as_str().unwrap().to_string()
    }

    async fn open_write(&mut self, p: &Path) -> u64 {
        let (h, _) = self
            .ok(json!({"op":"open","path":hp(p),"write":true,"create":true,"trunc":true}), b"")
            .await;
        h["h"].as_u64().unwrap()
    }
}

#[tokio::test]
async fn write_then_read_roundtrips_binary_at_offsets() {
    let dir = Scratch::new();
    let p = dir.path("data.bin");
    let mut c = Client::start().await;

    // 100 KB of every byte value, written as chunks at explicit offsets -- out
    // of order, as a pipelining SFTP client may deliver them.
    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let h = c.open_write(&p).await;
    for (i, chunk) in content.chunks(30_000).enumerate().collect::<Vec<_>>().into_iter().rev() {
        c.ok(json!({"op":"write","h":h,"off":i * 30_000}), chunk).await;
    }
    let (st, _) = c.ok(json!({"op":"fstat","h":h}), b"").await;
    assert_eq!(st["attrs"]["size"], 100_000);
    c.ok(json!({"op":"close","h":h}), b"").await;
    assert_eq!(std::fs::read(&p).unwrap(), content);

    // Read it back: a full chunk, then a short read at the tail flagged eof.
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"read":true}), b"").await;
    let h = o["h"].as_u64().unwrap();
    let (r, data) = c.ok(json!({"op":"read","h":h,"off":0,"len":32768}), b"").await;
    assert_eq!(data, &content[..32768]);
    assert_eq!(r["eof"], false);
    let (r, data) = c.ok(json!({"op":"read","h":h,"off":99_000,"len":32768}), b"").await;
    assert_eq!(data, &content[99_000..]);
    assert_eq!(r["eof"], true);
    // Past the end: empty + eof, not an error.
    let (r, data) = c.ok(json!({"op":"read","h":h,"off":500_000,"len":10}), b"").await;
    assert!(data.is_empty());
    assert_eq!(r["eof"], true);
    // A read larger than the per-response cap is clamped, not refused.
    let (_, data) = c.ok(json!({"op":"read","h":h,"off":0,"len":10_000_000}), b"").await;
    assert_eq!(data.len(), guesthopper::files::FS_READ_MAX);
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let dir = Scratch::new();
    let p = dir.path("f");
    std::fs::write(&p, b"0123456789").unwrap();
    let mut c = Client::start().await;
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p)}), b"").await;
    let h = o["h"].as_u64().unwrap();

    // Send every request before reading any response.
    for off in 0..10u64 {
        c.send(json!({"op":"read","h":h,"off":off,"len":1}), b"").await;
    }
    for off in 0..10u8 {
        let (hdr, data) = c.recv().await;
        assert_eq!(hdr["ok"], true);
        assert_eq!(data, vec![b'0' + off]);
    }
}

#[tokio::test]
async fn errors_carry_a_portable_kind_and_do_not_end_the_session() {
    let dir = Scratch::new();
    let mut c = Client::start().await;

    assert_eq!(c.err(json!({"op":"stat","path":hp(&dir.path("missing"))})).await, "noent");
    assert_eq!(c.err(json!({"op":"open","path":hp(&dir.path("missing"))})).await, "noent");

    let p = dir.path("exists");
    std::fs::write(&p, b"x").unwrap();
    assert_eq!(
        c.err(json!({"op":"open","path":hp(&p),"write":true,"create":true,"excl":true})).await,
        "exist"
    );
    assert_eq!(c.err(json!({"op":"read","h":9999,"len":1})).await, "invalid");
    assert_eq!(c.err(json!({"op":"stat","path":"zz"})).await, "invalid");
    assert_eq!(c.err(json!({"op":"stat"})).await, "invalid");
    assert_eq!(c.err(json!({"op":"frobnicate"})).await, "unsupported");
    // rmdir of a non-empty directory: a plain failure with the OS message.
    let (h, _) = c.call(json!({"op":"rmdir","path":hp(&dir.0)}), b"").await;
    assert_eq!(h["kind"], "fail");
    assert!(h["message"].as_str().unwrap().len() > 3);

    // A malformed payload (too short to hold the header length) is an error
    // response too, keeping the response stream one-to-one.
    frame::write_frame(&mut c.wr, frame::FRAME_FS_REQ, b"\x00").await.unwrap();
    assert_eq!(c.recv().await.0["kind"], "invalid");

    // PINGs are just liveness; the session is still usable afterwards.
    frame::write_frame(&mut c.wr, frame::FRAME_PING, b"").await.unwrap();
    c.ok(json!({"op":"stat","path":hp(&p)}), b"").await;
}

#[tokio::test]
async fn readdir_pages_through_entries_with_binary_names() {
    let dir = Scratch::new();
    let mut expected: Vec<Vec<u8>> = (0..300).map(|i| format!("f{i:03}").into_bytes()).collect();
    // A filename that is not valid UTF-8 must survive (hex on the wire).
    expected.push(vec![b'b', 0xff, 0xfe, b'x']);
    for name in &expected {
        std::fs::write(dir.0.join(std::ffi::OsStr::from_bytes(name)), b"").unwrap();
    }
    expected.sort();

    let mut c = Client::start().await;
    let (o, _) = c.ok(json!({"op":"opendir","path":hp(&dir.0)}), b"").await;
    let h = o["h"].as_u64().unwrap();
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let (r, _) = c.ok(json!({"op":"readdir","h":h}), b"").await;
        pages += 1;
        for ent in r["entries"].as_array().unwrap() {
            assert_eq!(ent["attrs"]["size"], 0);
            seen.push(unhex(ent["name"].as_str().unwrap()));
        }
        if r["eof"] == true {
            break;
        }
    }
    c.ok(json!({"op":"close","h":h}), b"").await;
    seen.sort();
    assert_eq!(seen, expected);
    assert!(pages > 1, "301 entries should not fit one bounded page");
    // A directory handle is not a file handle.
    assert_eq!(c.err(json!({"op":"readdir","h":h})).await, "invalid");
}

#[tokio::test]
async fn setstat_applies_mode_times_and_size() {
    let dir = Scratch::new();
    let p = dir.path("f");
    std::fs::write(&p, b"0123456789").unwrap();
    let mut c = Client::start().await;

    c.ok(json!({"op":"setstat","path":hp(&p),"mode":0o640,"mtime":1_000_000_000,"size":4}), b"")
        .await;
    let m = std::fs::metadata(&p).unwrap();
    assert_eq!(m.permissions().mode() & 0o7777, 0o640);
    assert_eq!(m.mtime(), 1_000_000_000);
    assert_eq!(m.len(), 4);

    // Only atime given: mtime must be left alone (UTIME_OMIT), and vice versa.
    c.ok(json!({"op":"setstat","path":hp(&p),"atime":5}), b"").await;
    let m = std::fs::metadata(&p).unwrap();
    assert_eq!((m.atime(), m.mtime()), (5, 1_000_000_000));

    // Via an open handle (what `scp -p` does after an upload).
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"write":true}), b"").await;
    let h = o["h"].as_u64().unwrap();
    c.ok(json!({"op":"setstat","h":h,"mode":0o600,"atime":7,"mtime":8}), b"").await;
    let m = std::fs::metadata(&p).unwrap();
    assert_eq!(m.permissions().mode() & 0o7777, 0o600);
    assert_eq!((m.atime(), m.mtime()), (7, 8));
}

#[tokio::test]
async fn open_honours_create_mode_and_append() {
    let dir = Scratch::new();
    let p = dir.path("new");
    let mut c = Client::start().await;
    let (o, _) = c
        .ok(json!({"op":"open","path":hp(&p),"write":true,"create":true,"mode":0o600}), b"")
        .await;
    c.ok(json!({"op":"write","h":o["h"],"off":0}), b"abc").await;
    c.ok(json!({"op":"close","h":o["h"]}), b"").await;
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);

    // Append ignores the offset and lands at the end.
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"append":true}), b"").await;
    c.ok(json!({"op":"write","h":o["h"],"off":0}), b"def").await;
    c.ok(json!({"op":"close","h":o["h"]}), b"").await;
    assert_eq!(std::fs::read(&p).unwrap(), b"abcdef");

    // A closed handle is gone.
    assert_eq!(c.err(json!({"op":"close","h":o["h"]})).await, "invalid");
}

#[tokio::test]
async fn namespace_ops_mkdir_rename_links_remove() {
    let dir = Scratch::new();
    let mut c = Client::start().await;
    let (d, a, b) = (dir.path("d"), dir.path("d/a"), dir.path("d/b"));

    c.ok(json!({"op":"mkdir","path":hp(&d),"mode":0o750}), b"").await;
    assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o750);
    std::fs::write(&a, b"A").unwrap();
    std::fs::write(&b, b"B").unwrap();

    // Plain rename refuses to clobber; replace (posix-rename) overwrites.
    assert_eq!(c.err(json!({"op":"rename","path":hp(&a),"to":hp(&b)})).await, "exist");
    assert_eq!(std::fs::read(&b).unwrap(), b"B");
    c.ok(json!({"op":"rename","path":hp(&a),"to":hp(&b),"replace":true}), b"").await;
    assert_eq!(std::fs::read(&b).unwrap(), b"A");
    assert!(!a.exists());

    // Symlink: target stored verbatim; stat follows, lstat (follow=false) doesn't.
    let link = dir.path("d/link");
    c.ok(json!({"op":"symlink","path":hex(b"b"),"to":hp(&link)}), b"").await;
    let (r, _) = c.ok(json!({"op":"readlink","path":hp(&link)}), b"").await;
    assert_eq!(unhex(r["target"].as_str().unwrap()), b"b");
    let (s, _) = c.ok(json!({"op":"stat","path":hp(&link)}), b"").await;
    let (l, _) = c.ok(json!({"op":"stat","path":hp(&link),"follow":false}), b"").await;
    assert_eq!(s["attrs"]["mode"].as_u64().unwrap() & 0o170000, 0o100000);
    assert_eq!(l["attrs"]["mode"].as_u64().unwrap() & 0o170000, 0o120000);

    let hard = dir.path("d/hard");
    c.ok(json!({"op":"link","path":hp(&b),"to":hp(&hard)}), b"").await;
    let (s, _) = c.ok(json!({"op":"stat","path":hp(&b)}), b"").await;
    assert_eq!(s["attrs"]["nlink"], 2);

    for p in [&link, &hard, &b] {
        c.ok(json!({"op":"remove","path":hp(p)}), b"").await;
    }
    c.ok(json!({"op":"rmdir","path":hp(&d)}), b"").await;
    assert!(!d.exists());

    let (v, _) = c.ok(json!({"op":"statvfs","path":hp(&dir.0)}), b"").await;
    assert!(v["bsize"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn realpath_resolves_existing_and_not_yet_existing_paths() {
    let dir = Scratch::new();
    let real = std::fs::canonicalize(&dir.0).unwrap();
    let mut c = Client::start().await;

    let (r, _) = c.ok(json!({"op":"realpath","path":hp(&dir.0.join("."))}), b"").await;
    assert_eq!(unhex(r["path"].as_str().unwrap()), real.as_os_str().as_bytes());

    // An upload destination that doesn't exist yet still resolves (lexically).
    let (r, _) = c.ok(json!({"op":"realpath","path":hex(b"/no-such-dir/x/../y/./z")}), b"").await;
    assert_eq!(unhex(r["path"].as_str().unwrap()), b"/no-such-dir/y/z");

    // Relative paths resolve against the agent's working directory.
    let (r, _) = c.ok(json!({"op":"realpath","path":hex(b".")}), b"").await;
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(unhex(r["path"].as_str().unwrap()), cwd.as_os_str().as_bytes());
}

#[tokio::test]
async fn handle_count_is_capped_per_session() {
    let dir = Scratch::new();
    let p = dir.path("f");
    std::fs::write(&p, b"x").unwrap();
    let mut c = Client::start().await;
    let mut handles = Vec::new();
    loop {
        let (h, _) = c.call(json!({"op":"open","path":hp(&p)}), b"").await;
        if h["ok"] != true {
            assert_eq!(h["kind"], "fail");
            break;
        }
        handles.push(h["h"].as_u64().unwrap());
        assert!(handles.len() <= 1024, "no cap on open handles");
    }
    // Closing one frees a slot.
    c.ok(json!({"op":"close","h":handles[0]}), b"").await;
    c.ok(json!({"op":"open","path":hp(&p)}), b"").await;
}

#[tokio::test]
async fn session_ends_cleanly_when_the_client_disconnects() {
    let (host, guest) = duplex(1 << 16);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        Duration::from_secs(60),
        Duration::from_secs(60),
        None,
    ));
    let (rd, mut wr) = split(host);
    frame::write_frame(&mut wr, frame::FRAME_REQUEST, br#"{"verb":"fs"}"#).await.unwrap();
    drop(wr);
    drop(rd);
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("fs session did not end on disconnect")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn silent_client_is_timed_out() {
    let (host, guest) = duplex(1 << 16);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        Duration::from_millis(200), // idle timeout
        Duration::from_secs(60),
        None,
    ));
    let (_rd, mut wr) = split(host);
    frame::write_frame(&mut wr, frame::FRAME_REQUEST, br#"{"verb":"fs"}"#).await.unwrap();
    // Hold the connection open but send nothing more.
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("idle fs session was never torn down")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn unsupported_op_error_does_not_echo_a_huge_op_string() {
    // A client-controlled op name up to a whole inbound frame long must not be
    // reflected back (escaped, it would be several times larger still): a
    // client that never reads could otherwise pin that much per queued reply.
    let mut c = Client::start().await;
    let op = "\u{7f}".repeat(500_000);
    c.send(json!({ "op": op }), b"").await;
    let Frame { ftype, payload } = frame::read_frame(&mut c.rd).await.unwrap().expect("frame");
    assert_eq!(ftype, frame::FRAME_FS_RESP);
    assert!(payload.len() < 1024, "error response is {} bytes", payload.len());
    // Session still usable, and a normal unknown op is still named.
    let (h, _) = c.call(json!({"op":"frobnicate"}), b"").await;
    assert!(h["message"].as_str().unwrap().contains("frobnicate"));
}

#[tokio::test]
async fn unsupported_verb_error_does_not_echo_a_huge_verb() {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        Duration::from_secs(60),
        Duration::from_secs(60),
        None,
    ));
    let (mut rd, mut wr) = split(host);
    let req = serde_json::to_vec(&json!({ "verb": "\u{7f}".repeat(500_000) })).unwrap();
    frame::write_frame(&mut wr, frame::FRAME_REQUEST, &req).await.unwrap();
    let Frame { ftype, payload } = frame::read_frame(&mut rd).await.unwrap().expect("frame");
    assert_eq!(ftype, frame::FRAME_ERROR);
    assert!(payload.len() < 1024, "error frame is {} bytes", payload.len());
}

#[tokio::test]
async fn malformed_request_diagnostics_are_bounded_end_to_end() {
    // Both parsers quote the offending value in their error; neither may send
    // a frame-sized value back.
    let mut c = Client::start().await;
    c.send(json!({ "op": "read", "h": "B".repeat(500_000) }), b"").await;
    let Frame { payload, .. } = frame::read_frame(&mut c.rd).await.unwrap().expect("frame");
    assert!(payload.len() < 1024, "fs error response is {} bytes", payload.len());

    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        Duration::from_secs(60),
        Duration::from_secs(60),
        None,
    ));
    let (mut rd, mut wr) = split(host);
    let req = serde_json::to_vec(&json!({ "verb": "exec", "deadline": "C".repeat(500_000) })).unwrap();
    frame::write_frame(&mut wr, frame::FRAME_REQUEST, &req).await.unwrap();
    let Frame { ftype, payload } = frame::read_frame(&mut rd).await.unwrap().expect("frame");
    assert_eq!(ftype, frame::FRAME_ERROR);
    assert!(payload.len() < 1024, "ERROR frame is {} bytes", payload.len());
}

#[tokio::test]
async fn truncate_by_path_never_blocks_on_a_fifo() {
    // A blocking open of a reader-less FIFO would park the blocking thread for
    // good (nothing can interrupt it) and hold the session until the op
    // timeout. It must fail fast instead.
    let dir = Scratch::new();
    let fifo = dir.path("fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let mut c = Client::start().await;
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        c.call(json!({"op":"setstat","path":hp(&fifo),"size":0}), b""),
    )
    .await
    .expect("setstat size on a FIFO blocked");
    assert_eq!(reply.0["ok"], false);
    // Opening a FIFO for reading doesn't block either.
    tokio::time::timeout(Duration::from_secs(5), c.ok(json!({"op":"open","path":hp(&fifo)}), b""))
        .await
        .expect("open of a FIFO blocked");
}

#[tokio::test]
async fn nofollow_truncate_refuses_a_symlink_instead_of_truncating_its_target() {
    let dir = Scratch::new();
    let (target, link) = (dir.path("target"), dir.path("link"));
    std::fs::write(&target, b"0123456789").unwrap();
    std::os::unix::fs::symlink("target", &link).unwrap();
    let mut c = Client::start().await;

    let (h, _) = c.call(json!({"op":"setstat","path":hp(&link),"follow":false,"size":3}), b"").await;
    assert_eq!(h["ok"], false);
    assert_eq!(std::fs::read(&target).unwrap(), b"0123456789");

    // Following (the default) truncates the target, as setstat should.
    c.ok(json!({"op":"setstat","path":hp(&link),"size":3}), b"").await;
    assert_eq!(std::fs::read(&target).unwrap(), b"012");
}

async fn resolve(c: &mut Client, p: PathBuf) -> PathBuf {
    let (r, _) = c.ok(json!({"op":"realpath","path":hp(&p)}), b"").await;
    PathBuf::from(std::ffi::OsStr::from_bytes(&unhex(r["path"].as_str().unwrap())))
}

#[tokio::test]
async fn realpath_follows_symlinks_in_the_existing_part_of_a_new_path() {
    // dir/link -> real/sub. For a destination that doesn't exist yet, the
    // existing prefix must resolve the way the kernel would: `link/..` is
    // real/, not dir/.
    let dir = Scratch::new();
    let root = std::fs::canonicalize(&dir.0).unwrap();
    std::fs::create_dir_all(root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink("real/sub", root.join("link")).unwrap();
    let mut c = Client::start().await;

    assert_eq!(resolve(&mut c, root.join("link/../new")).await, root.join("real/new"));
    assert_eq!(resolve(&mut c, root.join("link/new")).await, root.join("real/sub/new"));
    assert_eq!(resolve(&mut c, root.join("link/a/../b/./c")).await, root.join("real/sub/b/c"));

    // A non-directory in the middle is an error, as a real lookup would be.
    std::fs::write(root.join("file"), b"").unwrap();
    let (h, _) = c.call(json!({"op":"realpath","path":hp(&root.join("file/x"))}), b"").await;
    assert_eq!(h["ok"], false);
}

#[tokio::test]
async fn realpath_resumes_real_resolution_after_dotdot_cancels_a_missing_component() {
    // `missing/..` is back at an existing directory, so what follows must be
    // resolved for real again: `link/..` is real/, not the directory holding
    // the symlink.
    let dir = Scratch::new();
    let root = std::fs::canonicalize(&dir.0).unwrap();
    std::fs::create_dir_all(root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink("real/sub", root.join("link")).unwrap();
    let mut c = Client::start().await;

    assert_eq!(resolve(&mut c, root.join("missing/../link/../new")).await, root.join("real/new"));
    assert_eq!(resolve(&mut c, root.join("a/b/../../link/new")).await, root.join("real/sub/new"));
    // Still lexical while inside the missing part.
    assert_eq!(resolve(&mut c, root.join("a/b/../c")).await, root.join("a/c"));
    assert_eq!(resolve(&mut c, root.join("missing/..")).await, root);
}

#[tokio::test]
async fn open_refused_for_a_full_handle_table_has_no_side_effects() {
    // The capacity check must come before the open: a rejected create/truncate
    // that had already run would empty (or create) its target.
    let dir = Scratch::new();
    let (keep, victim, fresh) = (dir.path("keep"), dir.path("victim"), dir.path("fresh"));
    std::fs::write(&keep, b"x").unwrap();
    std::fs::write(&victim, b"precious").unwrap();
    let mut c = Client::start().await;
    loop {
        let (h, _) = c.call(json!({"op":"open","path":hp(&keep)}), b"").await;
        if h["ok"] != true {
            break;
        }
    }
    let trunc = json!({"op":"open","path":hp(&victim),"write":true,"create":true,"trunc":true});
    assert_eq!(c.call(trunc, b"").await.0["ok"], false);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    let create = json!({"op":"open","path":hp(&fresh),"write":true,"create":true});
    assert_eq!(c.call(create, b"").await.0["ok"], false);
    assert!(!fresh.exists());
    assert_eq!(c.call(json!({"op":"opendir","path":hp(&dir.0)}), b"").await.0["ok"], false);
}

#[tokio::test]
async fn fstatvfs_follows_the_open_handle_not_its_original_path() {
    let dir = Scratch::new();
    let p = dir.path("f");
    std::fs::write(&p, b"x").unwrap();
    let mut c = Client::start().await;
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p)}), b"").await;
    std::fs::remove_file(&p).unwrap();

    // The path is gone, the handle is not.
    assert_eq!(c.err(json!({"op":"statvfs","path":hp(&p)})).await, "noent");
    let (v, _) = c.ok(json!({"op":"fstatvfs","h":o["h"]}), b"").await;
    assert!(v["bsize"].as_u64().unwrap() > 0);
    assert!(v["namemax"].as_u64().unwrap() > 0);
    assert_eq!(c.err(json!({"op":"fstatvfs","h":9999})).await, "invalid");
}

#[tokio::test]
async fn open_accepts_the_flag_combinations_sftp_clients_send() {
    let dir = Scratch::new();
    let mut c = Client::start().await;

    // create alone (read-only create), and read+create.
    let p = dir.path("created");
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"create":true}), b"").await;
    c.ok(json!({"op":"close","h":o["h"]}), b"").await;
    assert!(p.exists());
    c.ok(json!({"op":"open","path":hp(&dir.path("rc")),"read":true,"create":true}), b"").await;

    // append + truncate: empties the file, then writes land at the end.
    std::fs::write(&p, b"old contents").unwrap();
    let (o, _) = c
        .ok(json!({"op":"open","path":hp(&p),"write":true,"append":true,"trunc":true}), b"")
        .await;
    c.ok(json!({"op":"write","h":o["h"],"off":999}), b"new").await;
    c.ok(json!({"op":"close","h":o["h"]}), b"").await;
    assert_eq!(std::fs::read(&p).unwrap(), b"new");

    // read + write: both directions work on one handle.
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"read":true,"write":true}), b"").await;
    c.ok(json!({"op":"write","h":o["h"],"off":3}), b"!").await;
    let (_, data) = c.ok(json!({"op":"read","h":o["h"],"off":0,"len":10}), b"").await;
    assert_eq!(data, b"new!");

    // A write-only handle can't be read; truncate without write is refused
    // (Linux would otherwise truncate on a read-only open).
    let (o, _) = c.ok(json!({"op":"open","path":hp(&p),"write":true}), b"").await;
    assert_eq!(c.call(json!({"op":"read","h":o["h"],"off":0,"len":1}), b"").await.0["ok"], false);
    assert_eq!(c.err(json!({"op":"open","path":hp(&p),"trunc":true})).await, "invalid");
    assert_eq!(std::fs::read(&p).unwrap(), b"new!");
}

#[tokio::test]
async fn plain_rename_refuses_any_existing_destination() {
    let dir = Scratch::new();
    let (a, dangling, d) = (dir.path("a"), dir.path("dangling"), dir.path("d"));
    std::fs::write(&a, b"A").unwrap();
    // A dangling symlink "exists" as a directory entry even though stat fails.
    std::os::unix::fs::symlink("nowhere", &dangling).unwrap();
    std::fs::create_dir(&d).unwrap();
    let mut c = Client::start().await;

    for to in [&dangling, &d] {
        assert_eq!(c.err(json!({"op":"rename","path":hp(&a),"to":hp(to)})).await, "exist");
    }
    assert_eq!(std::fs::read(&a).unwrap(), b"A");
    c.ok(json!({"op":"rename","path":hp(&a),"to":hp(&dir.path("b"))}), b"").await;
    assert_eq!(std::fs::read(dir.path("b")).unwrap(), b"A");
}
