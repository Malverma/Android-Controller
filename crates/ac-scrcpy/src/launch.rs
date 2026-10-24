//! `PushingServer` sub-phases (spec §3.3.3): push → forward + launch →
//! socket handshake.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ac_adb::AdbClient;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::server::{DEVICE_JAR, ServerParams};

/// Mode for the pushed jar: regular file, 0644.
const JAR_MODE: u32 = 0o100644;
const HANDSHAKE_ATTEMPTS: u32 = 100;
const HANDSHAKE_RETRY: Duration = Duration::from_millis(100);
const DUMMY_BYTE_TIMEOUT: Duration = Duration::from_secs(1);
const OUTPUT_TAIL_LINES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Pushing,
    Launching,
    Handshaking,
}

#[derive(Debug, thiserror::Error)]
#[error("{stage:?}: {message}")]
pub struct LaunchError {
    pub stage: Stage,
    pub message: String,
    /// Last lines the server printed.
    pub server_output: String,
}

impl LaunchError {
    fn new(stage: Stage, message: impl std::fmt::Display) -> Self {
        Self { stage, message: message.to_string(), server_output: String::new() }
    }
}

/// The server process, seen through its shell stream. The stream is read by
/// a task that logs each line; aborting that task closes the stream, which
/// terminates the server (with `cleanup=true` restoring device settings).
pub struct ServerProcess {
    task: JoinHandle<()>,
    exited: watch::Receiver<bool>,
    output: Arc<Mutex<Vec<String>>>,
}

impl ServerProcess {
    fn spawn(mut shell: TcpStream, shell_v2: bool, log_target: String) -> Self {
        let (tx, exited) = watch::channel(false);
        let output = Arc::new(Mutex::new(Vec::new()));
        let out = output.clone();
        let task = tokio::spawn(async move {
            let mut on_text = |text: &str| {
                let mut lines = out.lock().unwrap();
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    tracing::info!(target: "scrcpy", device = %log_target, "{line}");
                    lines.push(line.to_owned());
                    let n = lines.len();
                    if n > OUTPUT_TAIL_LINES {
                        lines.drain(..n - OUTPUT_TAIL_LINES);
                    }
                }
            };
            read_output(&mut shell, shell_v2, &mut on_text).await;
            let _ = tx.send(true);
        });
        Self { task, exited, output }
    }

    pub fn has_exited(&self) -> bool {
        *self.exited.borrow()
    }

    /// Resolves when the server process ends.
    pub async fn wait(&mut self) {
        let _ = self.exited.wait_for(|e| *e).await;
    }

    /// The last lines the server printed.
    pub fn output_tail(&self) -> String {
        self.output.lock().unwrap().join("\n")
    }

    /// Terminates the server by closing its shell stream.
    pub fn kill(&self) {
        self.task.abort();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A running server session.
pub struct Launched {
    /// Positioned after the dummy byte and the 64-byte device name.
    pub video: TcpStream,
    pub control: TcpStream,
    pub server: ServerProcess,
    pub device_name: String,
    /// The `tcp:<port>` forward to remove at teardown.
    pub forward_local: String,
}

pub async fn launch(
    adb: &AdbClient,
    serial: &str,
    server_jar: &Path,
    version: &str,
    params: &ServerParams,
    shell_v2: bool,
) -> Result<Launched, LaunchError> {
    push(adb, serial, server_jar).await?;

    let port = adb
        .forward(serial, "tcp:0", &format!("localabstract:{}", params.socket_name()))
        .await
        .map_err(|e| LaunchError::new(Stage::Launching, format!("forward: {e}")))?
        .ok_or_else(|| LaunchError::new(Stage::Launching, "forward returned no port"))?;
    let forward_local = format!("tcp:{port}");

    match start(adb, serial, version, params, shell_v2, port).await {
        Ok((video, control, server, device_name)) => Ok(Launched { video, control, server, device_name, forward_local }),
        Err(e) => {
            let _ = adb.kill_forward(serial, &forward_local).await;
            Err(e)
        }
    }
}

async fn push(adb: &AdbClient, serial: &str, server_jar: &Path) -> Result<(), LaunchError> {
    let err = |e: &dyn std::fmt::Display| LaunchError::new(Stage::Pushing, e);
    let file = tokio::fs::File::open(server_jar).await.map_err(|e| err(&format!("{}: {e}", server_jar.display())))?;
    let mtime = file
        .metadata()
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as u32);
    let mut sync = adb.sync(serial).await.map_err(|e| err(&e))?;
    tokio::time::timeout(Duration::from_secs(15), sync.send(DEVICE_JAR, JAR_MODE, mtime, file, |_| {}))
        .await
        .map_err(|_| err(&"timed out"))?
        .map_err(|e| err(&e))?;
    let _ = sync.quit().await;
    Ok(())
}

async fn start(
    adb: &AdbClient,
    serial: &str,
    version: &str,
    params: &ServerParams,
    shell_v2: bool,
    port: u16,
) -> Result<(TcpStream, TcpStream, ServerProcess, String), LaunchError> {
    let cmd = params.command(version);
    let service = if shell_v2 { format!("shell,v2,raw:{cmd}") } else { format!("shell:{cmd}") };
    let shell = adb
        .open_service(serial, &service)
        .await
        .map_err(|e| LaunchError::new(Stage::Launching, format!("shell: {e}")))?;
    let server = ServerProcess::spawn(shell, shell_v2, serial.to_owned());
    let failed = |stage, message: &str, server: &ServerProcess| LaunchError {
        stage,
        message: message.to_owned(),
        server_output: server.output_tail(),
    };

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut video = None;
    for _ in 0..HANDSHAKE_ATTEMPTS {
        if server.has_exited() {
            // Give the reader a moment to collect the last output.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(failed(Stage::Launching, "the server exited", &server));
        }
        if let Ok(mut s) = TcpStream::connect(addr).await {
            // adb accepts the tunnel connection even before the server
            // listens, then closes it: wait for the dummy byte.
            let mut b = [0u8; 1];
            if let Ok(Ok(1)) = tokio::time::timeout(DUMMY_BYTE_TIMEOUT, s.read(&mut b)).await {
                if b[0] != 0 {
                    return Err(failed(Stage::Handshaking, &format!("bad dummy byte {:#04x}", b[0]), &server));
                }
                video = Some(s);
                break;
            }
        }
        tokio::time::sleep(HANDSHAKE_RETRY).await;
    }
    let Some(mut video) = video else { return Err(failed(Stage::Handshaking, "the server did not answer", &server)) };

    // The server sends the device name only once every socket is connected
    // (video, then audio if enabled, then control).
    let control = TcpStream::connect(addr)
        .await
        .map_err(|e| failed(Stage::Handshaking, &format!("control socket: {e}"), &server))?;

    let mut name = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(5), video.read_exact(&mut name)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(failed(Stage::Handshaking, &format!("device name: {e}"), &server)),
        Err(_) => return Err(failed(Stage::Handshaking, "timed out reading the device name", &server)),
    }
    let end = name.iter().position(|&b| b == 0).unwrap_or(64);
    let device_name = String::from_utf8_lossy(&name[..end]).into_owned();
    let _ = video.set_nodelay(true);
    let _ = control.set_nodelay(true);
    Ok((video, control, server, device_name))
}

/// Feeds server output to `on_text` until the stream ends.
async fn read_output<S: AsyncRead + Unpin>(shell: &mut S, shell_v2: bool, on_text: &mut impl FnMut(&str)) {
    if shell_v2 {
        // Shell v2 packets: 1 = stdout, 2 = stderr, 3 = exit code.
        while let Ok(Some((id, data))) = ac_adb::shell::read_packet(shell).await {
            match id {
                1 | 2 => on_text(&String::from_utf8_lossy(&data)),
                3 => return,
                _ => {}
            }
        }
    } else {
        let mut buf = [0u8; 4096];
        loop {
            match shell.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => on_text(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    }
}
