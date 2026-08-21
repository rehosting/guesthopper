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
            Ok(resolved_path) => resolved_path.to_str().unwrap().to_string(),
            Err(_) => "/bin/sh".to_string(),
        },
    ));

    info!("Running commands with {}", shell);

    loop {
        // Accept an incoming connection. The vsock transport already gives one
        // independent stream per CONNECT, so each accepted stream is one
        // session -- we split it into read/write halves and hand it off.
        let (vsock, addr) = listener.accept().await?;
        info!("Received connection from {}", addr);
        let shell_clone = Arc::clone(&shell);
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(vsock);
            if let Err(e) = run_session(reader, writer, shell_clone).await {
                error!("Session error from {}: {}", addr, e);
            }
        });
    }
}
