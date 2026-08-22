//! Integration tests for the streaming exec session, driven over an in-memory
//! duplex so no vsock/guest is required. These pin the behaviors that the old
//! one-shot `process_request` could not provide: streaming (not
//! buffered-after-exit), binary safety, no 64 KB truncation, and correct rc.

use std::sync::Arc;

use guesthopper::frame::{self, Frame};
use guesthopper::session::run_session;
use tokio::io::{split, duplex, AsyncRead, AsyncWriteExt};

/// Drive one session: send a REQUEST, then collect frames until EXIT/ERROR.
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    reason: Option<String>,
    error: Option<String>,
}

async fn run_exec(cmd: &str, deadline: Option<f64>) -> Collected {
    run_exec_dd(cmd, deadline, None).await
}

/// Like `run_exec`, but lets a test set the agent's default command deadline
/// (the generous cap applied when the request carries no explicit deadline).
async fn run_exec_dd(
    cmd: &str,
    deadline: Option<f64>,
    default_deadline: Option<std::time::Duration>,
) -> Collected {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let shell = Arc::new("/bin/sh".to_string());
    let session = tokio::spawn(run_session(
        g_rd,
        g_wr,
        shell,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
        default_deadline,
    ));

    let (mut h_rd, mut h_wr) = split(host);
    let mut req = serde_json::json!({ "verb": "exec", "cmd": cmd });
    if let Some(d) = deadline {
        req["deadline"] = serde_json::json!(d);
    }
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();

    let collected = collect(&mut h_rd).await;
    // Session should finish cleanly once EXIT is emitted / peer closes.
    let _ = session.await.unwrap();
    collected
}

async fn collect<R: AsyncRead + Unpin>(r: &mut R) -> Collected {
    let mut out = Collected {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        reason: None,
        error: None,
    };
    while let Some(Frame { ftype, payload }) = frame::read_frame(r).await.unwrap() {
        match ftype {
            frame::FRAME_STDOUT => out.stdout.extend_from_slice(&payload),
            frame::FRAME_STDERR => out.stderr.extend_from_slice(&payload),
            frame::FRAME_EXIT => {
                let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                out.exit_code = Some(v["code"].as_i64().unwrap() as i32);
                out.reason = v["reason"].as_str().map(|s| s.to_string());
                break;
            }
            frame::FRAME_ERROR => {
                let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                out.error = Some(v["message"].as_str().unwrap_or("").to_string());
                break;
            }
            _ => {}
        }
    }
    out
}

#[tokio::test]
async fn interleaves_stdout_stderr_and_returns_rc() {
    let r = run_exec("echo a; echo b >&2; exit 3", None).await;
    assert_eq!(r.exit_code, Some(3));
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "a");
    assert_eq!(String::from_utf8_lossy(&r.stderr).trim(), "b");
    assert!(r.error.is_none());
}

#[tokio::test]
async fn large_output_is_not_truncated() {
    // Far more than the old 64 KB single-read cap; proves streaming.
    let cmd = "i=0; while [ $i -lt 5000 ]; do echo line$i; i=$((i+1)); done";
    let r = run_exec(cmd, None).await;
    assert_eq!(r.exit_code, Some(0));
    let text = String::from_utf8_lossy(&r.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5000);
    assert_eq!(lines[0], "line0");
    assert_eq!(lines[4999], "line4999");
}

#[tokio::test]
async fn binary_output_survives() {
    // printf octal escapes emit raw bytes including NUL; the old
    // from_utf8_lossy path would have mangled these.
    let r = run_exec(r"printf 'A\000B\377C'", None).await;
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, vec![b'A', 0x00, b'B', 0xFF, b'C']);
}

#[tokio::test]
async fn stdin_is_forwarded_to_child() {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let shell = Arc::new("/bin/sh".to_string());
    let session = tokio::spawn(run_session(g_rd, g_wr, shell, std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let (mut h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "exec", "cmd": "cat" });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();
    frame::write_frame(&mut h_wr, frame::FRAME_STDIN, b"hello stdin\n")
        .await
        .unwrap();
    frame::write_frame(&mut h_wr, frame::FRAME_STDIN_EOF, b"")
        .await
        .unwrap();

    let r = collect(&mut h_rd).await;
    let _ = session.await.unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(String::from_utf8_lossy(&r.stdout), "hello stdin\n");
}

#[tokio::test]
async fn deadline_kills_a_hung_command() {
    let r = run_exec("sleep 30", Some(0.3)).await;
    // Killed via SIGKILL on the deadline -> conventional 128+9 code and a
    // distinct "timeout" reason (not just a bare -1, so the client can tell a
    // timeout apart from a normal exit).
    assert_eq!(r.reason.as_deref(), Some("timeout"));
    assert_eq!(r.exit_code, Some(137));
}

#[tokio::test]
async fn exit_reason_is_exited_on_normal_exit() {
    let r = run_exec("exit 3", None).await;
    assert_eq!(r.exit_code, Some(3));
    assert_eq!(r.reason.as_deref(), Some("exited"));
}

#[tokio::test]
async fn default_deadline_kills_a_runaway_command() {
    // No per-request deadline, but the agent's generous default still bounds a
    // wedged command. Reason must say "timeout", distinct from a clean exit.
    let r = run_exec_dd("sleep 30", None, Some(std::time::Duration::from_millis(300))).await;
    assert_eq!(r.reason.as_deref(), Some("timeout"));
    assert_eq!(r.exit_code, Some(137));
}

#[tokio::test]
async fn explicit_zero_deadline_opts_out_of_the_default() {
    // deadline 0 means "no cap" even when a default is set: a fast command runs
    // to a normal exit rather than being rejected or killed.
    let r = run_exec_dd("echo ok", Some(0.0), Some(std::time::Duration::from_secs(60))).await;
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.reason.as_deref(), Some("exited"));
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "ok");
}

#[tokio::test]
async fn invalid_deadline_is_rejected() {
    // A non-finite/negative client-supplied deadline must not panic
    // Duration::try_from_secs_f64 -- it is rejected with an ERROR before the
    // command runs.
    let r = run_exec("echo nope", Some(-1.0)).await;
    assert!(
        r.error.unwrap_or_default().contains("invalid deadline"),
        "expected an invalid-deadline error"
    );
    assert!(r.exit_code.is_none());
}

#[tokio::test]
async fn overflowing_deadline_is_rejected_not_panicked() {
    // A finite, positive, but absurdly large deadline overflows Duration. The
    // old `from_secs_f64` would *panic* the session task on this; the fallible
    // `try_from_secs_f64` rejects it cleanly with an ERROR instead.
    let r = run_exec("echo nope", Some(1e300)).await;
    assert!(
        r.error.unwrap_or_default().contains("invalid deadline"),
        "expected an invalid-deadline error for an overflowing value"
    );
    assert!(r.exit_code.is_none());
}

#[tokio::test]
async fn backgrounded_child_does_not_stall_exit() {
    // A backgrounded process (or an orphan left by a deadline-killed shell)
    // inherits the stdout/stderr pipe, so the pumps never see EOF. EXIT must
    // still arrive promptly via the bounded drain rather than blocking until
    // that grandchild dies. Without the bound this would take ~30s; the 5s cap
    // fails loudly if the drain regresses to unbounded.
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_exec("sleep 30 & echo done", None),
    )
    .await
    .expect("exec stalled: EXIT blocked on a backgrounded child's pipe");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "done");
}

#[tokio::test]
async fn writer_stall_tears_down_a_nonreading_client() {
    // A client that stays connected but never *reads* (here: we simply never
    // read the host side) while the command floods stdout must not pin the
    // session forever. The writer's own write_timeout has to fire, tear the
    // session down, and let run_session return -- otherwise the session task
    // never completes and its concurrency slot leaks (the DoS this guards).
    let (host, guest) = duplex(1 << 16);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(
        g_rd,
        g_wr,
        Arc::new("/bin/sh".to_string()),
        std::time::Duration::from_secs(60), // idle: long, so it is NOT what fires
        std::time::Duration::from_millis(300), // write: short, the backstop under test
        None,
    ));

    // Keep the read half alive but never read it, so the guest->host buffer
    // fills and the guest's writes stall (rather than erroring on a closed peer).
    let (_h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "exec", "cmd": "yes" });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();

    // With the write-timeout backstop the session tears itself down well within
    // a few seconds; without it this hangs until the outer timeout trips.
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), session).await;
    assert!(done.is_ok(), "session did not tear down a non-reading client");
}

// Read STDOUT/STDERR frames until `needle` appears (or EXIT/ERROR/EOF/timeout).
// Interactive shells auto-exit unreliably when driven programmatically, so tests
// assert on emitted output and then disconnect to terminate the session.
async fn read_until<R: AsyncRead + Unpin>(r: &mut R, needle: &str) -> String {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), frame::read_frame(r)).await {
            Ok(Ok(Some(f))) => {
                if f.ftype == frame::FRAME_STDOUT || f.ftype == frame::FRAME_STDERR {
                    acc.extend_from_slice(&f.payload);
                    if String::from_utf8_lossy(&acc).contains(needle) {
                        return String::from_utf8_lossy(&acc).into_owned();
                    }
                }
                if f.ftype == frame::FRAME_EXIT || f.ftype == frame::FRAME_ERROR {
                    return String::from_utf8_lossy(&acc).into_owned();
                }
            }
            _ => return String::from_utf8_lossy(&acc).into_owned(),
        }
    }
}

#[tokio::test]
async fn open_pty_runs_interactive_shell_with_a_tty() {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(g_rd, g_wr, Arc::new("/bin/sh".to_string()), std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let (mut h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "open-pty", "rows": 30, "cols": 100 });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();
    // `stty size` reads the controlling tty -> proves a real pty at the
    // requested geometry; `echo` proves the shell is interactive.
    frame::write_frame(&mut h_wr, frame::FRAME_STDIN, b"stty size\n").await.unwrap();
    frame::write_frame(&mut h_wr, frame::FRAME_STDIN, b"echo marker-$((6*7))\n").await.unwrap();

    let text = read_until(&mut h_rd, "marker-42").await;
    assert!(text.contains("30 100"), "expected tty size 30x100 in: {text:?}");
    assert!(text.contains("marker-42"), "expected interactive shell output in: {text:?}");

    // Disconnect (shutdown the write half so the guest read half sees EOF) ->
    // the session must hang up the shell and terminate.
    h_wr.shutdown().await.unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(8), session).await;
    assert!(done.is_ok(), "session did not terminate after disconnect");
}

#[tokio::test]
async fn open_pty_honors_resize_frame() {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(g_rd, g_wr, Arc::new("/bin/sh".to_string()), std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let (mut h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "open-pty", "rows": 24, "cols": 80 });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();
    let resize = serde_json::json!({ "rows": 40, "cols": 120 });
    frame::write_frame(&mut h_wr, frame::FRAME_RESIZE, &serde_json::to_vec(&resize).unwrap())
        .await
        .unwrap();
    frame::write_frame(&mut h_wr, frame::FRAME_STDIN, b"stty size\n").await.unwrap();

    let text = read_until(&mut h_rd, "40 120").await;
    assert!(text.contains("40 120"), "expected resized tty 40x120 in: {text:?}");
    h_wr.shutdown().await.unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(8), session).await;
    assert!(done.is_ok(), "session did not terminate after disconnect");
}

#[tokio::test]
async fn open_pty_hangs_up_shell_on_disconnect() {
    // A shell with no input should be torn down when the host disconnects, and
    // the session must terminate (not orphan the shell / hang on wait).
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let session = tokio::spawn(run_session(g_rd, g_wr, Arc::new("/bin/sh".to_string()), std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let (mut h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "open-pty" });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();
    // Let the shell come up, then hang up (shutdown -> guest sees EOF).
    let _ = read_until(&mut h_rd, "$ ").await;
    h_wr.shutdown().await.unwrap();

    // The session future must complete (EXIT emitted) within the timeout.
    let done = tokio::time::timeout(std::time::Duration::from_secs(8), session).await;
    assert!(done.is_ok(), "session did not terminate after host disconnect");
}

#[tokio::test]
async fn open_pty_tears_down_on_abrupt_disconnect_via_writer_failure() {
    // An abrupt client death (crash/SIGKILL) may not deliver a read EOF to the
    // guest, so input_task can stay blocked forever. The session must still tear
    // down: the writer's failed push to the dead peer trips `cancel`, which
    // hangs up the shell. Model exactly that with a reader that yields the
    // request+stdin once and then parks (never EOF) paired with a writer that
    // fails -- the only thing that can end the session here is the cancel path.
    let mut data = Vec::new();
    let req = serde_json::to_vec(&serde_json::json!({ "verb": "open-pty" })).unwrap();
    data.push(frame::FRAME_REQUEST);
    data.extend_from_slice(&(req.len() as u32).to_be_bytes());
    data.extend_from_slice(&req);
    let stdin = b"echo hi\n";
    data.push(frame::FRAME_STDIN);
    data.extend_from_slice(&(stdin.len() as u32).to_be_bytes());
    data.extend_from_slice(stdin);

    let reader = OnceThenPending { data, pos: 0 };
    let writer = FailingWriter;
    let session = tokio::spawn(run_session(reader, writer, Arc::new("/bin/sh".to_string()), std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let done = tokio::time::timeout(std::time::Duration::from_secs(8), session).await;
    assert!(
        done.is_ok(),
        "session did not tear down via cancel when the peer's write side died"
    );
}

#[tokio::test]
async fn session_times_out_when_client_goes_silent() {
    // The keepalive backstop: no frame (not even a PING) within idle_timeout
    // means the client is gone, even when the transport surfaces neither a read
    // EOF nor a write error. Reader delivers the open-pty request then parks
    // forever; the writer is a sink that always succeeds (so the write-failure
    // cancel path can't fire) -- only the idle timeout can end this session.
    let mut data = Vec::new();
    let req = serde_json::to_vec(&serde_json::json!({ "verb": "open-pty" })).unwrap();
    data.push(frame::FRAME_REQUEST);
    data.extend_from_slice(&(req.len() as u32).to_be_bytes());
    data.extend_from_slice(&req);

    let reader = OnceThenPending { data, pos: 0 };
    let session = tokio::spawn(run_session(
        reader,
        tokio::io::sink(),
        Arc::new("/bin/sh".to_string()),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_secs(60),
        None,
    ));

    let done = tokio::time::timeout(std::time::Duration::from_secs(8), session).await;
    assert!(done.is_ok(), "session did not time out a silent (no-ping) client");
}

#[tokio::test]
async fn silent_client_that_sends_no_request_is_dropped() {
    // A client that connects and then never sends a REQUEST (or dribbles a
    // partial header and stalls) must not park the session forever -- the
    // initial read is bounded by idle_timeout, so the session gives up.
    let reader = OnceThenPending { data: Vec::new(), pos: 0 };
    let session = tokio::spawn(run_session(
        reader,
        tokio::io::sink(),
        Arc::new("/bin/sh".to_string()),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_secs(60),
        None,
    ));
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), session).await;
    assert!(done.is_ok(), "session did not give up on a silent (no-request) client");
}

/// AsyncRead that returns its buffer once, then parks forever (never EOF) --
/// models a vsock whose read side never signals an abrupt peer death.
struct OnceThenPending {
    data: Vec<u8>,
    pos: usize,
}

impl AsyncRead for OnceThenPending {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.data.len() {
            let n = (self.data.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.data[start..start + n]);
            self.pos += n;
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending // never EOF: the read side gives no disconnect signal
        }
    }
}

/// AsyncWrite whose writes always fail -- models the dead peer's write side.
struct FailingWriter;

impl tokio::io::AsyncWrite for FailingWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "peer gone",
        )))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn unsupported_verb_reports_error() {
    let (host, guest) = duplex(1 << 20);
    let (g_rd, g_wr) = split(guest);
    let shell = Arc::new("/bin/sh".to_string());
    let session = tokio::spawn(run_session(g_rd, g_wr, shell, std::time::Duration::from_secs(60), std::time::Duration::from_secs(60), None));

    let (mut h_rd, mut h_wr) = split(host);
    let req = serde_json::json!({ "verb": "teleport" });
    frame::write_frame(&mut h_wr, frame::FRAME_REQUEST, &serde_json::to_vec(&req).unwrap())
        .await
        .unwrap();

    let r = collect(&mut h_rd).await;
    let _ = session.await.unwrap();
    assert!(r.error.unwrap().contains("unsupported verb"));
}
