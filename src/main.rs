use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};
use tokio_vsock::{VsockListener,VsockAddr, VsockStream};
use tokio::process::Command;
use std::process::Stdio;
use structopt::StructOpt;
use log::{info,warn,error};
use env_logger;
use std::error::Error;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use serde_json;
use shlex;

const BUF_SIZE: usize = 65536;
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Vsock port - best to use greater than 16bit
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

    let shell = Arc::new(args.shell.unwrap_or_else(
        || match std::fs::read_link("/igloo/utils/sh.orig")  {
            Ok(resolved_path) => resolved_path.to_str().unwrap().to_string(),
            Err(_) => "/bin/busybox".to_string()
        }
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

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0;

    if let Some((program, args)) = shlex::split(&shell).unwrap().split_first() {
        info!("Running command in program '{}' with args '{}'", program, args.join(" "));

        //If our program isn't a shell, let's run the shell (this is for busybox)
        let args: Vec<String> = if program.ends_with("sh") {
            args.to_vec()
        } else {
            //Probably a better way to do this
            std::iter::once("sh".to_string()).chain(args.iter().cloned()).collect()
        };

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // Could Stdio::inherit() if we wanted to combine streams
            .spawn()?;

        let _ = timeout(CMD_TIMEOUT, async {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&format!("{}\n", command).into_bytes())
                .await.unwrap();
                stdin.write_all(b"exit $?\n")
                .await.unwrap();
            }

            let status = child.wait().await.unwrap();
            exit_code = status.code().unwrap();
            child.stdout.unwrap().read_to_string(&mut stdout).await.unwrap();
            child.stderr.unwrap().read_to_string(&mut stderr).await.unwrap();
        }).await;
    }

    let result = CmdResult {
        stdout: stdout,
        stderr: stderr,
        exit_code: exit_code
    };

    let serialized = serde_json::to_string(&result)?;

    vsock.write_all(serialized.as_bytes()).await?;
    vsock.shutdown(std::net::Shutdown::Both)?;

    Ok(())
}
