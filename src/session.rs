//! One session over the command channel. Slice 1: a single session per vsock
//! connection (the transport already gives one independent stream per CONNECT,
//! so we do not multiplex logical channels in-band). The session reads a
//! `REQUEST` frame, runs it, and streams `STDOUT`/`STDERR` as they are produced
//! (not buffered-after-exit), forwarding host `STDIN`, and emits `EXIT` on
//! close. Later slices add `open-pty` / `run-script` verbs on the same frames.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{timeout, Duration};

use crate::frame;

const READ_CHUNK: usize = 32 * 1024;
const LONG_COMMAND_THRESHOLD: usize = 2048;
/// Bound on queued outbound frames per session. A fast command with a slow (or
/// backpressured-but-alive) client must not grow the queue without limit and
/// OOM a scarce-RAM guest, so the channel is bounded: producers block on a full
/// queue (backpressure) instead of allocating forever. ~2 MiB worst case
/// (CHANNEL_BOUND * READ_CHUNK).
const CHANNEL_BOUND: usize = 64;
static LONG_COMMAND_WARNED: AtomicBool = AtomicBool::new(false);

/// A control-plane request. `verb` selects behavior; slice 1 supports `exec`
/// with a shell-string `cmd`. `deadline` is seconds: absent -> the agent's
/// generous default cap; `0` -> uncapped (opt-out, e.g. long-running debug
/// tools); a positive value -> that many seconds. Validated before use so a
/// hostile/garbage value can't panic `Duration::from_secs_f64`.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub verb: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub deadline: Option<f64>,
    // open-pty: initial window size.
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct Resize {
    rows: u16,
    cols: u16,
}

/// One-shot, multi-waiter cancellation. `fire()` wakes every current and future
/// waiter; `wait()` resolves immediately once fired. Unlike a bare `Notify`
/// (whose single stored permit is consumed by one waiter), this lets the writer
/// task *and* the running verb both observe the same teardown signal -- which we
/// need, because tearing down a dead-peer session requires killing the child
/// *and* unblocking the writer.
#[derive(Default)]
pub struct Cancel {
    fired: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Cancel {
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            let n = self.notify.notified();
            tokio::pin!(n);
            // Arm the notification, then re-check the flag: `notify_waiters`
            // only wakes already-armed waiters, so a `fire()` that races our
            // load would otherwise be lost.
            n.as_mut().enable();
            if self.fired.load(Ordering::SeqCst) {
                return;
            }
            n.await;
        }
    }
}

type Tx = Sender<(u8, Vec<u8>)>;

async fn send(tx: &Tx, ftype: u8, payload: Vec<u8>) {
    // A closed receiver just means the peer went away; drop the frame.
    let _ = tx.send((ftype, payload)).await;
}

async fn send_error(tx: &Tx, msg: &str) {
    let payload = serde_json::to_vec(&serde_json::json!({ "message": msg }))
        .unwrap_or_else(|_| Vec::new());
    send(tx, frame::FRAME_ERROR, payload).await;
}

/// Derive the EXIT frame's `(code, reason)`. A teardown `override` ("timeout" /
/// "disconnected") wins over the signal we used to kill the child; otherwise the
/// reason is derived from how the child actually ended:
///   - "exited"   normal exit (code is the process's exit status)
///   - "signaled" killed by a signal we did not send (code = 128 + signal)
///   - "error"    we failed to reap the child
fn exit_fields(
    status: std::io::Result<std::process::ExitStatus>,
    reason_override: &'static str,
) -> (i32, &'static str) {
    match status {
        Ok(s) => {
            if let Some(code) = s.code() {
                (code, if reason_override.is_empty() { "exited" } else { reason_override })
            } else if let Some(sig) = s.signal() {
                // Killed by a signal. Report the conventional 128+signal code so
                // the number is still meaningful, and label *why* if we know.
                (128 + sig, if reason_override.is_empty() { "signaled" } else { reason_override })
            } else {
                (-1, if reason_override.is_empty() { "exited" } else { reason_override })
            }
        }
        Err(_) => (-1, "error"),
    }
}

/// SIGKILL the child's whole process group (negative pid). `exec` spawns the
/// child with `process_group(0)`, so this reaps `cmd &` grandchildren too, not
/// just the shell -- otherwise they are reparented to init and survive. A
/// fully-detached daemon that called `setsid()` itself escapes; the bounded
/// output drain in `exec` covers that residual case.
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
}

/// Run one session to completion over the given reader/writer halves.
pub async fn run_session<R, W>(
    mut reader: R,
    writer: W,
    shell: Arc<String>,
    idle_timeout: Duration,
    default_deadline: Option<Duration>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // A single writer task owns the write half; every producer sends frames
    // through the bounded channel. This serializes writes without a lock and
    // gives per-session backpressure.
    //
    // `cancel` is the peer-gone signal. Two things can make a session need to
    // tear down against a dead client:
    //   * the write to the peer errors (a genuine RST) -> the writer fires it;
    //   * the peer dies abruptly and the vsock surfaces neither a read EOF nor
    //     a write error, just backpressure -> the reader's idle timeout fires it.
    // The writer selects on `cancel` around *both* its recv and its write, so a
    // wedged socket (write blocked on a dead-but-backpressuring peer) can't pin
    // the task forever -- firing `cancel` breaks it out and closes the channel,
    // which in turn unblocks any producer parked on a full queue.
    let (tx, mut rx) = mpsc::channel::<(u8, Vec<u8>)>(CHANNEL_BOUND);
    let cancel = Arc::new(Cancel::default());
    let cancel_w = Arc::clone(&cancel);
    let writer_task = tokio::spawn(async move {
        let mut w = writer;
        loop {
            tokio::select! {
                biased;
                _ = cancel_w.wait() => break,
                maybe = rx.recv() => match maybe {
                    Some((ftype, payload)) => {
                        tokio::select! {
                            biased;
                            _ = cancel_w.wait() => break,
                            r = frame::write_frame(&mut w, ftype, &payload) => {
                                if r.is_err() {
                                    cancel_w.fire();
                                    break;
                                }
                            }
                        }
                    }
                    None => break, // all senders dropped: session done
                }
            }
        }
    });

    let result = drive(&mut reader, &tx, shell, cancel, idle_timeout, default_deadline).await;
    if let Err(e) = &result {
        send_error(&tx, &format!("session error: {e}")).await;
    }
    drop(tx);
    // Bounded by construction: the writer exits when the channel closes (all
    // senders dropped) or when `cancel` fires, so this never parks forever.
    let _ = writer_task.await;
    result
}

async fn drive<R>(
    reader: &mut R,
    tx: &Tx,
    shell: Arc<String>,
    cancel: Arc<Cancel>,
    idle_timeout: Duration,
    default_deadline: Option<Duration>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Bound the initial REQUEST read the same way the verb loops bound theirs.
    // Without this a client that CONNECTs and then sends nothing -- or dribbles
    // a partial header and stalls -- parks the session forever: `cancel` is only
    // armed once a verb handler runs, so nothing tears it down, and the stream +
    // writer task leak. A zero-cost slowloris. Give up if no REQUEST arrives in
    // time.
    let first = match timeout(idle_timeout, frame::read_frame(reader)).await {
        Err(_elapsed) => return Ok(()),
        Ok(r) => r?,
    };
    let req = match first {
        Some(f) if f.ftype == frame::FRAME_REQUEST => {
            match serde_json::from_slice::<Request>(&f.payload) {
                Ok(r) => r,
                Err(e) => {
                    send_error(tx, &format!("invalid REQUEST json: {e}")).await;
                    return Ok(());
                }
            }
        }
        Some(_) => {
            send_error(tx, "expected a REQUEST frame first").await;
            return Ok(());
        }
        None => return Ok(()), // peer closed before sending anything
    };

    match req.verb.as_str() {
        "exec" => exec(reader, tx, &shell, req, &cancel, idle_timeout, default_deadline).await,
        "open-pty" => open_pty(reader, tx, &shell, req, &cancel, idle_timeout).await,
        other => {
            send_error(tx, &format!("unsupported verb: {other:?}")).await;
            Ok(())
        }
    }
}

async fn exec<R>(
    reader: &mut R,
    tx: &Tx,
    shell: &str,
    req: Request,
    cancel: &Cancel,
    idle_timeout: Duration,
    default_deadline: Option<Duration>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let cmd = req.cmd.unwrap_or_default();
    warn_long_command(&cmd);

    // Resolve the effective deadline. Absent -> the agent's generous default (a
    // safety net so a wedged command can't run forever); an explicit `0` -> no
    // cap (opt-out for gdbserver/strace and other session-length commands); a
    // positive value -> that many seconds. Reject non-finite/negative rather
    // than letting `Duration::from_secs_f64` panic on a client-supplied f64.
    let effective_deadline: Option<Duration> = match req.deadline {
        None => default_deadline,
        Some(s) if s.is_finite() && s >= 0.0 => {
            // 0 is the explicit opt-out (no cap); any positive value is the cap.
            if s == 0.0 {
                None
            } else {
                Some(Duration::from_secs_f64(s))
            }
        }
        Some(bad) => {
            send_error(tx, &format!("invalid deadline {bad}: must be finite and >= 0")).await;
            return Ok(());
        }
    };

    // Resolve the shell program (same logic as before): split the configured
    // shell string; if the program isn't a shell, run `sh` with it as argv[0].
    let parts = shlex::split(shell).unwrap_or_default();
    let (program, extra) = match parts.split_first() {
        Some((p, rest)) => (p.clone(), rest.to_vec()),
        None => ("/bin/sh".to_string(), Vec::new()),
    };
    let arg0 = if program.ends_with("sh") {
        program.clone()
    } else {
        "sh".to_string()
    };

    // `-c <cmd>` runs the command via argv, leaving the child's stdin free to
    // carry host STDIN frames (the old path fed the command through stdin, so
    // it could not also stream input). `process_group(0)` puts the child in its
    // own group so a deadline/disconnect kill can signal the whole tree.
    let mut child = Command::new(&program)
        .arg0(&arg0)
        .args(&extra)
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;

    let mut cstdout = child.stdout.take().expect("piped stdout");
    let mut cstderr = child.stderr.take().expect("piped stderr");
    let cstdin = child.stdin.take();

    let tx_out = tx.clone();
    let mut out_task = tokio::spawn(async move {
        pump(&mut cstdout, frame::FRAME_STDOUT, &tx_out).await;
    });
    let tx_err = tx.clone();
    let mut err_task = tokio::spawn(async move {
        pump(&mut cstderr, frame::FRAME_STDERR, &tx_err).await;
    });

    // Forward host STDIN frames to the child until EOF/close.
    let stdin_task = {
        let mut cstdin = cstdin;
        async move {
            loop {
                // Bound each read by idle_timeout. A live client keeps this fresh
                // with FRAME_PING on idle; a lapse means the peer is gone, so fire
                // cancel (the select below is biased to kill the child on it).
                match timeout(idle_timeout, frame::read_frame(reader)).await {
                    Err(_elapsed) => {
                        cancel.fire();
                        break;
                    }
                    Ok(Ok(Some(f))) if f.ftype == frame::FRAME_STDIN => {
                        if let Some(si) = cstdin.as_mut() {
                            if si.write_all(&f.payload).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Ok(Some(f))) if f.ftype == frame::FRAME_STDIN_EOF => {
                        cstdin = None; // drop -> close child's stdin
                    }
                    Ok(Ok(Some(_))) => {} // PING / unknown frames: just liveness
                    Ok(Ok(None)) => break, // host closed its write half
                    Ok(Err(_)) => break,
                }
            }
        }
    };

    // Run stdin forwarding concurrently with waiting for the child, plus the
    // deadline timer and the cancel signal. Capture the pid before `wait` takes
    // its borrow so the kill branches can signal without a second borrow.
    let child_pid = child.id();
    let (raw, reason_override): (std::io::Result<std::process::ExitStatus>, &'static str) = {
        tokio::pin!(stdin_task);
        let wait = child.wait();
        tokio::pin!(wait);
        // A never-ready timer stands in for "no deadline", so the select arm is
        // always present and we don't branch the whole loop on Option.
        let deadline_timer = async {
            match effective_deadline {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(deadline_timer);
        let mut stdin_done = false;
        loop {
            tokio::select! {
                // Biased so a stdin-idle lapse that fires `cancel` and ends
                // stdin_task in the same poll kills the child rather than looping.
                biased;
                _ = cancel.wait() => {
                    // Peer gone (writer failed, or no frame within idle_timeout).
                    kill_group(child_pid);
                    break ((&mut wait).await, "disconnected");
                }
                _ = &mut deadline_timer => {
                    kill_group(child_pid);
                    break ((&mut wait).await, "timeout");
                }
                status = &mut wait => {
                    break (status, "");
                }
                // Guarded so a completed stdin_task is never re-polled; cancel
                // and the deadline stay live for the rest of the child's life,
                // so a client that half-closes stdin and then dies is still
                // caught (previously exec stopped honoring cancel after EOF).
                _ = &mut stdin_task, if !stdin_done => {
                    stdin_done = true;
                }
            }
        }
    };

    // Drain remaining output before signalling exit -- but bound it. If the
    // command backgrounded a process (`cmd &`) or a killed shell left an orphan
    // that escaped the group kill (its own setsid), that grandchild inherits the
    // stdout/stderr pipe write-end, so the pumps never see EOF and would block
    // EXIT indefinitely. Give the pumps a short grace to flush, then abort them
    // so their `tx` clones drop and the writer can finish.
    let grace = Duration::from_millis(250);
    if timeout(grace, &mut out_task).await.is_err() {
        out_task.abort();
    }
    if timeout(grace, &mut err_task).await.is_err() {
        err_task.abort();
    }

    let (code, reason) = exit_fields(raw, reason_override);
    let payload = serde_json::to_vec(&serde_json::json!({ "code": code, "reason": reason }))?;
    send(tx, frame::FRAME_EXIT, payload).await;
    Ok(())
}

/// Allocate a pty, run an interactive shell on the slave, and stream the master
/// both directions over frames. This is what the console becomes (slice 2): a
/// real tty (job control, `isatty()`, TUI apps), unlike `exec`'s pipes. STDIN
/// frames are written to the master, RESIZE frames set the window size, and pty
/// output is streamed as STDOUT. Login-shell/profile specifics are the caller's
/// concern (via the configured shell); this just gives an interactive tty.
async fn open_pty<R>(
    reader: &mut R,
    tx: &Tx,
    shell: &str,
    req: Request,
    cancel: &Cancel,
    idle_timeout: Duration,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: valid out-pointers; null term/winsize means kernel defaults.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc < 0 {
        // e.g. no /dev/ptmx / CONFIG_UNIX98_PTYS in the guest kernel.
        send_error(tx, &format!("openpty failed: {}", std::io::Error::last_os_error())).await;
        return Ok(());
    }
    set_winsize(master, req.rows.unwrap_or(24), req.cols.unwrap_or(80));

    let parts = shlex::split(shell).unwrap_or_default();
    let (program, extra) = match parts.split_first() {
        Some((p, rest)) => (p.clone(), rest.to_vec()),
        None => ("/bin/sh".to_string(), Vec::new()),
    };
    let arg0 = if program.ends_with("sh") { program.clone() } else { "sh".to_string() };

    // Give the child the slave as stdio via three dups (Command owns them).
    // Check each dup: under fd exhaustion `dup` returns -1, and wrapping -1 in
    // an OwnedFd/Stdio is a latent footgun -- fail the request cleanly instead
    // of relying on `spawn()` to reject the bogus fd.
    let (d0, d1, d2) = unsafe { (libc::dup(slave), libc::dup(slave), libc::dup(slave)) };
    if d0 < 0 || d1 < 0 || d2 < 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            for d in [d0, d1, d2] {
                if d >= 0 {
                    libc::close(d);
                }
            }
            libc::close(master);
            libc::close(slave);
        }
        send_error(tx, &format!("failed to dup pty slave: {e}")).await;
        return Ok(());
    }

    // Make the child the controlling terminal's session leader (setsid +
    // TIOCSCTTY) in a pre-exec hook.
    let mut cmd = Command::new(&program);
    cmd.arg0(&arg0).args(&extra);
    // SAFETY: d0/d1/d2 are live dups the OwnedFd takes ownership of; Command
    // closes them. The pre_exec closure runs in the forked child before exec.
    unsafe {
        cmd.stdin(Stdio::from(OwnedFd::from_raw_fd(d0)));
        cmd.stdout(Stdio::from(OwnedFd::from_raw_fd(d1)));
        cmd.stderr(Stdio::from(OwnedFd::from_raw_fd(d2)));
        let ctty = slave;
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(ctty, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            send_error(tx, &format!("failed to spawn pty shell: {e}")).await;
            return Ok(());
        }
    };
    // The parent doesn't use the slave; closing it means reads on the master
    // report EOF/EIO once the child's copies are gone.
    unsafe { libc::close(slave) };

    // These two failures happen *after* the shell is spawned and the slave
    // closed, so a bare `?` would leak the master fd and orphan the running
    // shell. Kill the child's group, reap it, and (for the non-blocking case,
    // where master is still a raw fd) close master before bailing.
    if let Err(e) = set_nonblocking(master) {
        kill_group(child.id());
        let _ = child.wait().await;
        unsafe { libc::close(master) };
        send_error(tx, &format!("failed to set pty master non-blocking: {e}")).await;
        return Ok(());
    }
    // SAFETY: master is a live, now-owned fd. On AsyncFd::new failure the moved
    // OwnedFd is dropped, which closes master -- so we must not close it again.
    let am = match AsyncFd::new(unsafe { OwnedFd::from_raw_fd(master) }) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            kill_group(child.id());
            let _ = child.wait().await;
            send_error(tx, &format!("failed to register pty master with the runtime: {e}")).await;
            return Ok(());
        }
    };

    // Pty master -> STDOUT frames.
    let tx_out = tx.clone();
    let am_read = Arc::clone(&am);
    let mut out_task = tokio::spawn(async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let mut guard = match am_read.readable().await {
                Ok(g) => g,
                Err(_) => break,
            };
            let res = guard.try_io(|inner| {
                let n = unsafe {
                    libc::read(inner.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    if tx_out.send((frame::FRAME_STDOUT, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                // When the child exits and its slave fds close, Linux reports
                // EIO on the master -- treat as EOF.
                Ok(Err(ref e)) if e.raw_os_error() == Some(libc::EIO) => break,
                Ok(Err(_)) => break,
                Err(_would_block) => continue,
            }
        }
    });

    // Host frames -> pty master (STDIN) / window size (RESIZE).
    let am_write = Arc::clone(&am);
    let input_task = async move {
        loop {
            // Bound each read by idle_timeout; a live client keeps it fresh with
            // FRAME_PING on idle. A lapse means the peer is gone (a state the
            // vsock transport may not surface as EOF), so fire cancel and stop --
            // the select below hangs up the shell either way.
            match timeout(idle_timeout, frame::read_frame(reader)).await {
                Err(_elapsed) => {
                    cancel.fire();
                    break;
                }
                Ok(Ok(Some(f))) if f.ftype == frame::FRAME_STDIN => {
                    if write_all_fd(&am_write, &f.payload).await.is_err() {
                        break;
                    }
                }
                Ok(Ok(Some(f))) if f.ftype == frame::FRAME_RESIZE => {
                    if let Ok(r) = serde_json::from_slice::<Resize>(&f.payload) {
                        set_winsize(am_write.get_ref().as_raw_fd(), r.rows, r.cols);
                    }
                }
                Ok(Ok(Some(_))) => {} // PING / unknown frames: just liveness
                Ok(Ok(None)) => break,
                Ok(Err(_)) => break,
            }
        }
    };

    tokio::pin!(input_task);
    // Either the shell exits on its own, or the host disconnects. On disconnect
    // we hang up the shell like a real terminal HUP rather than orphaning it.
    // Disconnect is detected two ways: a clean half-close makes input_task's
    // read return EOF; an abrupt client death (no read EOF delivered) instead
    // trips `cancel` (writer failure or idle timeout).
    let (status, reason_override): (std::io::Result<std::process::ExitStatus>, &'static str) =
        tokio::select! {
            _ = &mut input_task => (hangup(&mut child).await, "disconnected"),
            s = child.wait() => (s, ""),
            _ = cancel.wait() => (hangup(&mut child).await, "disconnected"),
        };

    // The child is gone. Unlike a pipe (clean Ok(0) EOF), a pty master does not
    // reliably deliver a read-readiness edge on HUP, so out_task can park on
    // readable() forever. Bound the final drain: it flushes buffered output via
    // the POLLIN edge, then we stop rather than block.
    if tokio::time::timeout(Duration::from_millis(200), &mut out_task).await.is_err() {
        out_task.abort();
    }
    let (code, reason) = exit_fields(status, reason_override);
    let payload = serde_json::to_vec(&serde_json::json!({ "code": code, "reason": reason }))?;
    send(tx, frame::FRAME_EXIT, payload).await;
    Ok(())
}

/// Terminal hangup: SIGHUP the shell's process group, escalating to SIGKILL if
/// it lingers. The pty child called `setsid()`, so it leads its own group;
/// signalling the group (negative pid) hangs up background jobs too, not just
/// the foreground process.
async fn hangup(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGHUP) };
    }
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(s) => s,
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
            child.wait().await
        }
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Best-effort; a bad size just leaves the prior geometry.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

async fn write_all_fd(am: &AsyncFd<OwnedFd>, data: &[u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < data.len() {
        let mut guard = am.writable().await?;
        let res = guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.as_raw_fd(),
                    data[off..].as_ptr() as *const libc::c_void,
                    data.len() - off,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match res {
            // A zero-byte write on a non-empty request would never advance the
            // offset -> a hot busy-loop that starves the single-threaded runtime
            // (every other task, including the socket writer). Treat it as an
            // error instead of spinning.
            Ok(Ok(0)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "wrote zero bytes to pty master",
                ))
            }
            Ok(Ok(n)) => off += n,
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

async fn pump<S>(src: &mut S, ftype: u8, tx: &Tx)
where
    S: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match src.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => send(tx, ftype, buf[..n].to_vec()).await,
            Err(_) => break,
        }
    }
}

/// Preserve the old guidance: a very large command is better staged via
/// static_files/init.d than shipped inline. Warn once, on the guest console.
fn warn_long_command(command: &str) {
    if command.len() < LONG_COMMAND_THRESHOLD {
        return;
    }
    if LONG_COMMAND_WARNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let warning = concat!(
        "[IGLOO] warning: long guest command detected; consider putting large ",
        "commands in static_files, init.d, or the shared results directory instead.\n"
    );
    // Open/write /dev/ttyS0 on a blocking thread: synchronous file I/O on the
    // single-threaded runtime would stall every other task (including the
    // writer draining the client socket) for its duration.
    tokio::task::spawn_blocking(move || {
        match std::fs::OpenOptions::new().write(true).open("/dev/ttyS0") {
            Ok(mut tty) => {
                let _ = std::io::Write::write_all(&mut tty, warning.as_bytes());
            }
            Err(_) => {
                log::warn!("{}", warning.trim_end());
            }
        }
    });
}
