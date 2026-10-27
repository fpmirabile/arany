use crate::tools::ToolError;
use crate::tools::network::{BRIDGE_OVERFLOW, OPEN_CONNECTIONS, PROXY_PORT, PROXY_SOCKET_TARGET};
use std::io::{Read, Write};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Mutex;
use tokio::net::{TcpListener, TcpStream, UnixStream};

const READY: &[u8; 2] = b"R\n";

// The helper reaps its own child so the descendant check never sees a zombie.
static BRIDGE: Mutex<Option<Child>> = Mutex::new(None);

// Started before the seccomp filter lands: the filtered payload and helper can no longer create the sockets it needs.
pub(in crate::tools::guard) fn spawn() -> Result<(), ToolError> {
    let mut child = Command::new("/proc/self/exe")
        .args(["--internal-tool-guard", "--network-bridge"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut ready = [0; READY.len()];
    let started = child
        .stdout
        .take()
        .ok_or(ToolError::ProtectionUnavailable)?
        .read_exact(&mut ready)
        .is_ok()
        && ready == *READY;
    if !started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ToolError::ProtectionUnavailable);
    }
    *BRIDGE.lock().map_err(|_| ToolError::Operation)? = Some(child);
    Ok(())
}

pub(in crate::tools::guard) fn reap() {
    if let Ok(mut bridge) = BRIDGE.lock()
        && let Some(child) = bridge.as_mut()
        && child.try_wait().is_ok_and(|status| status.is_some())
    {
        *bridge = None;
    }
}

pub(in crate::tools::guard) fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), ToolError> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .block_on(serve())
}

async fn serve() -> Result<(), ToolError> {
    let listener = TcpListener::bind(("127.0.0.1", PROXY_PORT))
        .await
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(READY)
        .and_then(|()| stdout.flush())
        .map_err(|_| ToolError::Operation)?;
    drop(stdout);
    // The proxy refuses beyond its own open bound and closes at once, so live relays stay under this one; past it the proxy is still told.
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(2 * OPEN_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        };
        match std::sync::Arc::clone(&slots).try_acquire_owned() {
            Ok(slot) => {
                tokio::spawn(async move {
                    relay(stream).await;
                    drop(slot);
                });
            }
            Err(_) => {
                drop(stream);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(1), report_overflow())
                    .await;
            }
        }
    }
}

async fn report_overflow() {
    use tokio::io::AsyncWriteExt;
    if let Ok(mut proxy) = UnixStream::connect(PROXY_SOCKET_TARGET).await {
        let _ = proxy
            .write_all(format!("{BRIDGE_OVERFLOW}\r\n\r\n").as_bytes())
            .await;
    }
}

async fn relay(mut inbound: TcpStream) {
    if let Ok(mut proxy) = UnixStream::connect(PROXY_SOCKET_TARGET).await {
        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut proxy).await;
    }
}
