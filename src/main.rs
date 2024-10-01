use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::{VsockListener,VsockAddr, VsockStream};
use tokio::process::Command;
use structopt::StructOpt;
use log::{info,warn,error};
use env_logger;
use std::error::Error;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use serde_json;

const BUF_SIZE: usize = 4096;

#[derive(Serialize, Deserialize, Debug)]
struct CmdResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Clone, StructOpt)]
pub struct ListenAddress {
    /// Context ID.
    #[structopt(short, long)]
    cid: Option<u32>,
    /// Vsock port - default to greater than 16bit
    #[structopt(short = "p", long, default_value = "12341234")]
    port: u32,
    /// Shell to run command under - defaults to original guest shell
    #[structopt(short = "s", long)]
    shell: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = ListenAddress::from_args();
    let cid = args.cid.unwrap_or(libc::VMADDR_CID_ANY);
    let addr = VsockAddr::new(cid, args.port);
    let mut listener = VsockListener::bind(addr)?;

    warn!("Listening on VSOCK cid: {}, port: {}", cid, args.port);

    let shell: Arc<_> = Arc::new(args.shell.unwrap_or_else(
        || format!("{} sh",
            { match std::fs::read_link("/igloo/utils/sh.orig")  {
                Ok(resolved_path) => resolved_path.to_str().unwrap().to_string(),
                Err(_) => "/bin/busybox sh".to_string()
            }})
    ));

    info!("Running commands with {}", shell);

    loop {
        // Accept an incoming connection
        let (vsock, addr) = listener.accept().await?;
        let shell_clone = Arc::clone(&shell);
        tokio::spawn(async move { 
            if let Err(e) = process_request(vsock, addr, shell_clone).await {
                error!("Error: {}", e);
            }
        });
    }
}

async fn process_request(mut vsock: VsockStream, addr: VsockAddr, shell: Arc<String>) -> Result<(), Box<dyn Error>> {
    info!("Received connection from {}",addr);

    let mut buffer = [0; BUF_SIZE];
    let n = vsock.read(&mut buffer).await?;
    let command = String::from_utf8_lossy(&buffer[..n]);

    let command = command.trim();
    info!("Received command: {}", command);

    // This gets a wee bit messy since the shell might be "busybox sh", "bash", etc... 
    // For busybox, we need to pass the 'sh' argument, hence the gymnastics below
    let mut shell_parts = shell.split_whitespace();
    let shell_exec = shell_parts.next().expect("No shell provided");
    let shell_args: Vec<&str> = shell_parts.collect();

    let mut runner = Command::new(shell_exec);

    // Add the remaining arguments (e.g., if the shell is "busybox sh")
    runner.args(&shell_args);
    let output = runner.arg("-c")
        .arg(command)
        .output()
        .await?;

    let result = CmdResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap(),
    };

    let serialized = serde_json::to_string(&result)?;

    assert!(serialized.len() <= BUF_SIZE);

    vsock.write_all(serialized.as_bytes()).await?;

    vsock.shutdown(std::net::Shutdown::Both)?;

    Ok(())
}
