//! One session over the command channel. Slice 1: a single session per vsock
//! connection (the transport already gives one independent stream per CONNECT,
//! so we do not multiplex logical channels in-band). The session reads a
//! `REQUEST` frame, runs it, and streams `STDOUT`/`STDERR` as they are produced
//! (not buffered-after-exit), forwarding host `STDIN`, and emits `EXIT` on
//! close. Later slices add `open-pty` / `run-script` verbs on the same frames.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::{timeout, Duration};

use crate::frame;

const READ_CHUNK: usize = 32 * 1024;
const LONG_COMMAND_THRESHOLD: usize = 2048;
static LONG_COMMAND_WARNED: AtomicBool = AtomicBool::new(false);

/// A control-plane request. `verb` selects behavior; slice 1 supports `exec`
/// with a shell-string `cmd`. `deadline` is opt-in seconds (default: none, so
/// interactive/long commands are not capped -- unlike the old hardcoded 10 s).
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

type Tx = UnboundedSender<(u8, Vec<u8>)>;

fn send(tx: &Tx, ftype: u8, payload: Vec<u8>) {
    // A closed receiver just means the peer went away; drop the frame.
    let _ = tx.send((ftype, payload));
}

fn send_error(tx: &Tx, msg: &str) {
    let payload = serde_json::to_vec(&serde_json::json!({ "message": msg }))
        .unwrap_or_else(|_| Vec::new());
    send(tx, frame::FRAME_ERROR, payload);
}

/// Run one session to completion over the given reader/writer halves.
pub async fn run_session<R, W>(
    mut reader: R,
    writer: W,
    shell: Arc<String>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // A single writer task owns the write half; every producer sends frames
    // through the channel. This serializes writes without a lock and gives
    // natural per-session backpressure via the socket buffer.
    //
    // `cancel` is the peer-gone signal. An abrupt client disconnect (crash,
    // SIGKILL) does not reliably deliver a read EOF to the guest vsock, so the
    // reader side (input_task) can stay blocked forever -- but the first write
    // to the dead peer fails. The writer fires `cancel` on that failure so the
    // running verb can tear down (hang up the pty shell / kill the exec child)
    // instead of orphaning it and letting out_task fill the channel unbounded.
    // notify_one() stores a permit, so a cancel that races ahead of the
    // consumer's notified() is still observed.
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let cancel_w = Arc::clone(&cancel);
    let writer_task = tokio::spawn(async move {
        let mut w = writer;
        while let Some((ftype, payload)) = rx.recv().await {
            if frame::write_frame(&mut w, ftype, &payload).await.is_err() {
                cancel_w.notify_one();
                break;
            }
        }
    });

    let result = drive(&mut reader, &tx, shell, cancel).await;
    if let Err(e) = &result {
        send_error(&tx, &format!("session error: {e}"));
    }
    drop(tx);
    let _ = writer_task.await;
    result
}

async fn drive<R>(
    reader: &mut R,
    tx: &Tx,
    shell: Arc<String>,
    cancel: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let req = match frame::read_frame(reader).await? {
        Some(f) if f.ftype == frame::FRAME_REQUEST => {
            match serde_json::from_slice::<Request>(&f.payload) {
                Ok(r) => r,
                Err(e) => {
                    send_error(tx, &format!("invalid REQUEST json: {e}"));
                    return Ok(());
                }
            }
        }
        Some(_) => {
            send_error(tx, "expected a REQUEST frame first");
            return Ok(());
        }
        None => return Ok(()), // peer closed before sending anything
    };

    match req.verb.as_str() {
        "exec" => exec(reader, tx, &shell, req, &cancel).await,
        "open-pty" => open_pty(reader, tx, &shell, req, &cancel).await,
        other => {
            send_error(tx, &format!("unsupported verb: {other:?}"));
            Ok(())
        }
    }
}

async fn exec<R>(
    reader: &mut R,
    tx: &Tx,
    shell: &str,
    req: Request,
    cancel: &tokio::sync::Notify,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let cmd = req.cmd.unwrap_or_default();
    warn_long_command(&cmd);

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
    // it could not also stream input).
    let mut child = Command::new(&program)
        .arg0(&arg0)
        .args(&extra)
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    // Forward host STDIN frames to the child until EOF/close. This task holds
    // no `tx` clone, so it never blocks the writer from finishing; it is
    // aborted once the child exits.
    let stdin_task = {
        // Move the reader into the task by re-borrowing through an owned value.
        // `reader` is `&mut R`; we cannot move it, so read inline in a loop and
        // hand ownership of stdin to the task via a channel-free closure is not
        // possible -- instead we drive stdin here concurrently with wait below.
        let mut cstdin = cstdin;
        async move {
            loop {
                match frame::read_frame(reader).await {
                    Ok(Some(f)) if f.ftype == frame::FRAME_STDIN => {
                        if let Some(si) = cstdin.as_mut() {
                            if si.write_all(&f.payload).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Some(f)) if f.ftype == frame::FRAME_STDIN_EOF => {
                        cstdin = None; // drop -> close child's stdin
                    }
                    Ok(Some(_)) => {} // ignore unknown frames in slice 1
                    Ok(None) => break, // host closed its write half
                    Err(_) => break,
                }
            }
        }
    };

    // Run stdin forwarding concurrently with waiting for the child. `select`
    // lets whichever finishes first proceed; stdin forwarding ending does not
    // kill the child, and the child exiting stops us waiting on stdin.
    // Capture the pid before `wait` takes its mutable borrow of `child`, so the
    // cancel branch can signal the child without a second borrow (it reaps via
    // the existing `wait` future).
    let child_pid = child.id();
    let code = {
        tokio::pin!(stdin_task);
        let wait = async {
            if let Some(secs) = req.deadline {
                match timeout(Duration::from_secs_f64(secs), child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                }
            } else {
                child.wait().await
            }
        };
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut stdin_task => {
                    // stdin drained/closed; keep waiting for the child.
                    let status = (&mut wait).await;
                    break status.ok().and_then(|s| s.code()).unwrap_or(-1);
                }
                status = &mut wait => {
                    break status.ok().and_then(|s| s.code()).unwrap_or(-1);
                }
                _ = cancel.notified() => {
                    // The peer is gone (writer failed). Don't keep the command
                    // running against a dead client -- kill it, then reap via the
                    // existing `wait` future (which owns the &mut child borrow).
                    if let Some(pid) = child_pid {
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    }
                    let status = (&mut wait).await;
                    break status.ok().and_then(|s| s.code()).unwrap_or(-1);
                }
            }
        }
    };

    // Drain remaining output before signalling exit -- but bound it. If the
    // command backgrounded a process (`cmd &`) or a killed shell left an
    // orphan, that grandchild inherits the stdout/stderr pipe write-end, so the
    // pump tasks never see EOF and would block EXIT indefinitely. After the
    // shell itself has exited, give the pumps a short grace to flush buffered
    // output, then abort them so their `tx` clones drop and the writer can
    // finish (mirrors the pty path's bounded drain).
    let grace = Duration::from_millis(250);
    if timeout(grace, &mut out_task).await.is_err() {
        out_task.abort();
    }
    if timeout(grace, &mut err_task).await.is_err() {
        err_task.abort();
    }

    let payload = serde_json::to_vec(&serde_json::json!({ "code": code }))?;
    send(tx, frame::FRAME_EXIT, payload);
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
    cancel: &tokio::sync::Notify,
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
        send_error(tx, &format!("openpty failed: {}", std::io::Error::last_os_error()));
        return Ok(());
    }
    set_winsize(master, req.rows.unwrap_or(24), req.cols.unwrap_or(80));

    let parts = shlex::split(shell).unwrap_or_default();
    let (program, extra) = match parts.split_first() {
        Some((p, rest)) => (p.clone(), rest.to_vec()),
        None => ("/bin/sh".to_string(), Vec::new()),
    };
    let arg0 = if program.ends_with("sh") { program.clone() } else { "sh".to_string() };

    // Give the child the slave as stdio (three dups Command owns), and make it
    // the controlling terminal in a pre-exec hook (setsid + TIOCSCTTY).
    let mut cmd = Command::new(&program);
    cmd.arg0(&arg0).args(&extra);
    // SAFETY: dup of a live fd; the OwnedFd takes ownership and Command closes it.
    unsafe {
        cmd.stdin(Stdio::from(OwnedFd::from_raw_fd(libc::dup(slave))));
        cmd.stdout(Stdio::from(OwnedFd::from_raw_fd(libc::dup(slave))));
        cmd.stderr(Stdio::from(OwnedFd::from_raw_fd(libc::dup(slave))));
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
            send_error(tx, &format!("failed to spawn pty shell: {e}"));
            return Ok(());
        }
    };
    // The parent doesn't use the slave; closing it means reads on the master
    // report EOF/EIO once the child's copies are gone.
    unsafe { libc::close(slave) };

    set_nonblocking(master)?;
    // SAFETY: master is a live, now-owned fd.
    let am = Arc::new(AsyncFd::new(unsafe { OwnedFd::from_raw_fd(master) })?);

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
                    if tx_out.send((frame::FRAME_STDOUT, buf[..n].to_vec())).is_err() {
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
            match frame::read_frame(reader).await {
                Ok(Some(f)) if f.ftype == frame::FRAME_STDIN => {
                    if write_all_fd(&am_write, &f.payload).await.is_err() {
                        break;
                    }
                }
                Ok(Some(f)) if f.ftype == frame::FRAME_RESIZE => {
                    if let Ok(r) = serde_json::from_slice::<Resize>(&f.payload) {
                        set_winsize(am_write.get_ref().as_raw_fd(), r.rows, r.cols);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    };

    tokio::pin!(input_task);
    // Either the shell exits on its own, or the host disconnects. On disconnect
    // we hang up the shell like a real terminal HUP rather than orphaning it.
    // Disconnect is detected two ways: a clean half-close makes input_task's
    // read return EOF; an abrupt client death (no read EOF delivered) instead
    // trips `cancel` when the writer fails to push pty output to the dead peer.
    let early = tokio::select! {
        _ = &mut input_task => None,
        s = child.wait() => Some(s),
        _ = cancel.notified() => None,
    };
    let status = match early {
        Some(s) => s,
        None => hangup(&mut child).await,
    };
    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);

    // The child is gone. Unlike a pipe (clean Ok(0) EOF), a pty master does not
    // reliably deliver a read-readiness edge on HUP, so out_task can park on
    // readable() forever. Bound the final drain: it flushes buffered output via
    // the POLLIN edge, then we stop rather than block.
    if tokio::time::timeout(Duration::from_millis(200), &mut out_task).await.is_err() {
        out_task.abort();
    }
    let payload = serde_json::to_vec(&serde_json::json!({ "code": code }))?;
    send(tx, frame::FRAME_EXIT, payload);
    Ok(())
}

/// Terminal hangup: SIGHUP the shell, and escalate to SIGKILL if it lingers.
async fn hangup(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
    }
    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(s) => s,
        Err(_) => {
            let _ = child.start_kill();
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
            Ok(n) => send(tx, ftype, buf[..n].to_vec()),
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
    match std::fs::OpenOptions::new().write(true).open("/dev/ttyS0") {
        Ok(mut tty) => {
            let _ = std::io::Write::write_all(&mut tty, warning.as_bytes());
        }
        Err(_) => {
            log::warn!("{}", warning.trim_end());
        }
    }
}
