use tokio_vsock::{VsockListener, VsockAddr};
use structopt::StructOpt;
use log::{info, warn, error};
use env_logger;
use std::sync::Arc;

use guesthopper::session::run_session;

mod portalcall;
use portalcall::{URegSize, RegSize};

const INDIV_DEBUG_PORTALCALL_MAGIC: URegSize = 0xfeedbeef;

#[derive(Clone, StructOpt)]
pub struct ListenAddress {
    /// Context ID.
    #[structopt(short, long)]
    cid: Option<u32>,
    /// Vsock port - best to use greater than 16bit
    #[structopt(short = "p", long, default_value = "12341234")]
    port: u32,
    /// Shell to run command under - defaults to original guest shell
    #[structopt(short = "s", long)]
    shell: Option<String>,
}

// A current-thread runtime: the guest is typically one emulated vCPU, so a
// multi-threaded work-stealing scheduler is pure emulated overhead (worker
// threads, cross-thread wakeups). tokio::spawn still works -- tasks are
// cooperatively scheduled on the single thread. Keeps guest CPU near zero when
// idle (blocked on accept/epoll) and lean under load.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let _: RegSize = portalcall::portal_call0(INDIV_DEBUG_PORTALCALL_MAGIC);
    let args = ListenAddress::from_args();
    let cid = args.cid.unwrap_or(libc::VMADDR_CID_ANY);
    let addr = VsockAddr::new(cid, args.port);
    let listener = VsockListener::bind(addr)?;

    warn!("Listening on VSOCK cid: {}, port: {}", cid, args.port);

    let shell = Arc::new(args.shell.unwrap_or_else(
        || match std::fs::read_link("/igloo/utils/sh.orig") {
            // Lossy rather than `.unwrap()`: a non-UTF-8 symlink target must not
            // panic the agent before it ever accepts a connection.
            Ok(resolved_path) => resolved_path.to_string_lossy().into_owned(),
            Err(_) => "/bin/sh".to_string(),
        },
    ));

    info!("Running commands with {}", shell);

    // A session with no frame (not even a keepalive PING) for this long is
    // treated as a dead client and torn down -- the backstop for an abrupt
    // disconnect the vsock transport doesn't surface as EOF/error. Generous by
    // default (guest time runs slow under emulation and the client PINGs every
    // few seconds); override with GUESTHOPPER_IDLE_TIMEOUT_SECS.
    let idle_timeout = std::time::Duration::from_secs(
        std::env::var("GUESTHOPPER_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            // Floor at 1s: a 0 would make every read time out instantly and
            // kill every command the moment it goes idle.
            .unwrap_or(30)
            .max(1),
    );
    info!("Session idle timeout: {}s", idle_timeout.as_secs());

    // Bound on how long a single frame write to the client may stall before the
    // session is torn down. This is the backstop for a peer that stays *alive
    // but stops reading* -- it keeps its write half active (PINGs), so the idle
    // timeout never trips, while our writes block on a full send buffer. Without
    // it that one client parks the writer forever and its session never releases
    // its slot, so max_sessions of them wedge the agent. Generous by default
    // (guest time runs slow under emulation); override with
    // GUESTHOPPER_WRITE_TIMEOUT_SECS.
    let write_timeout = std::time::Duration::from_secs(
        std::env::var("GUESTHOPPER_WRITE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            // Floor at 1s: a 0 would make every write time out instantly.
            .unwrap_or(60)
            .max(1),
    );
    info!("Session write timeout: {}s", write_timeout.as_secs());

    // Cap the largest inbound frame the agent will allocate for. The default is
    // modest (see frame::DEFAULT_MAX_INBOUND_FRAME_LEN) so a hostile peer cannot
    // drive a big allocation per session and OOM a scarce-RAM guest; operators on
    // especially tiny guests can lower it, and anyone streaming large stdin
    // chunks can raise it (clamped to the u32 hard ceiling). Set once here,
    // before the accept loop, so every session sees the same limit.
    if let Some(v) = std::env::var("GUESTHOPPER_MAX_FRAME_LEN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        guesthopper::frame::set_max_inbound_frame_len(v);
    }
    info!(
        "Max inbound frame: {} bytes",
        guesthopper::frame::max_inbound_frame_len()
    );

    // Cap concurrent sessions. Each session forks a real shell and holds a vsock
    // fd + two tasks, so an unbounded accept loop is a fork/fd-exhaustion vector
    // for a confused or malicious client. Acquire a permit *before* accepting so
    // excess connections wait in the kernel backlog rather than being served.
    let max_sessions: usize = std::env::var("GUESTHOPPER_MAX_SESSIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(64)
        .max(1);
    info!("Max concurrent sessions: {}", max_sessions);
    let sessions = Arc::new(tokio::sync::Semaphore::new(max_sessions));

    // Generous default cap on a single `exec` command: absent a per-request
    // deadline, a wedged command is killed after this long so it can't run
    // forever. A client opts a specific command out with deadline 0 (see
    // guest_cmd.py --timeout 0). Set GUESTHOPPER_COMMAND_TIMEOUT_SECS=0 to
    // disable the default entirely. Guest time runs slow under emulation, so an
    // hour of guest wall-clock is very generous.
    let default_deadline = {
        let secs: f64 = std::env::var("GUESTHOPPER_COMMAND_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600.0);
        if secs <= 0.0 {
            None
        } else {
            // Fallible conversion: an absurd operator-set value (e.g. 1e300)
            // must not panic the agent at startup. Fall back to 1h if it won't
            // fit a Duration.
            Some(
                std::time::Duration::try_from_secs_f64(secs)
                    .unwrap_or_else(|_| std::time::Duration::from_secs(3600)),
            )
        }
    };
    match default_deadline {
        Some(d) => info!("Default command timeout: {}s (per-command deadline 0 opts out)", d.as_secs()),
        None => info!("Default command timeout: disabled"),
    }

    loop {
        // Block until a session slot is free, so we never accept beyond the cap.
        // The semaphore is never closed, so this only errors if the runtime is
        // shutting down -- treat that as "stop accepting".
        let permit = match Arc::clone(&sessions).acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        // Accept an incoming connection. The vsock transport already gives one
        // independent stream per CONNECT, so each accepted stream is one
        // session -- we split it into read/write halves and hand it off.
        let (vsock, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // A transient accept error (fd pressure, an aborted connection)
                // must not tear down the whole agent for the rest of the run;
                // log it, drop the permit, and keep serving.
                error!("accept failed: {}", e);
                drop(permit);
                continue;
            }
        };
        info!("Received connection from {}", addr);
        let shell_clone = Arc::clone(&shell);
        tokio::spawn(async move {
            // Hold the permit for the whole session; dropped on task exit,
            // releasing the slot.
            let _permit = permit;
            let (reader, writer) = tokio::io::split(vsock);
            if let Err(e) = run_session(
                reader,
                writer,
                shell_clone,
                idle_timeout,
                write_timeout,
                default_deadline,
            )
            .await
            {
                error!("Session error from {}: {}", addr, e);
            }
        });
    }
    Ok(())
}
