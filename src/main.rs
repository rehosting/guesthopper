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
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );
    info!("Session idle timeout: {}s", idle_timeout.as_secs());

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
            Some(std::time::Duration::from_secs_f64(secs))
        }
    };
    match default_deadline {
        Some(d) => info!("Default command timeout: {}s (per-command deadline 0 opts out)", d.as_secs()),
        None => info!("Default command timeout: disabled"),
    }

    loop {
        // Accept an incoming connection. The vsock transport already gives one
        // independent stream per CONNECT, so each accepted stream is one
        // session -- we split it into read/write halves and hand it off.
        let (vsock, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // A transient accept error (fd pressure, an aborted connection)
                // must not tear down the whole agent for the rest of the run;
                // log it and keep serving.
                error!("accept failed: {}", e);
                continue;
            }
        };
        info!("Received connection from {}", addr);
        let shell_clone = Arc::clone(&shell);
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(vsock);
            if let Err(e) =
                run_session(reader, writer, shell_clone, idle_timeout, default_deadline).await
            {
                error!("Session error from {}: {}", addr, e);
            }
        });
    }
}
