//! High-level host and device services on top of the smart-socket framing.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::devices::{DeviceEntry, parse_device_list};
use crate::protocol::{read_hex_string, read_status, read_to_end_string, send_request};
use crate::shell::{ShellOutput, collect_v2};
use crate::sync::SyncSession;
use crate::AdbError;

pub const DEFAULT_SERVER: &str = "127.0.0.1:5037";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectOutcome {
    Connected,
    AlreadyConnected,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairOutcome {
    /// `guid` is the device's mDNS identity, `adb-<serial>-<suffix>`.
    Paired { guid: Option<String> },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct AdbClient {
    server: SocketAddr,
    timeout: Duration,
}

impl AdbClient {
    pub fn new(server: SocketAddr) -> Self {
        Self { server, timeout: Duration::from_secs(10) }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    async fn dial(&self) -> Result<TcpStream, AdbError> {
        let stream = timeout(self.timeout, TcpStream::connect(self.server))
            .await
            .map_err(|_| AdbError::Timeout)??;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    async fn bounded<T>(&self, fut: impl Future<Output = Result<T, AdbError>>) -> Result<T, AdbError> {
        timeout(self.timeout, fut).await.map_err(|_| AdbError::Timeout)?
    }

    /// Host service answering `OKAY` followed by a hex-length string.
    pub async fn host_query(&self, service: &str) -> Result<String, AdbError> {
        self.bounded(async {
            let mut s = self.dial().await?;
            send_request(&mut s, service).await?;
            read_status(&mut s).await?;
            read_hex_string(&mut s).await
        })
        .await
    }

    /// Host service answering a bare `OKAY`.
    pub async fn host_command(&self, service: &str) -> Result<(), AdbError> {
        self.bounded(async {
            let mut s = self.dial().await?;
            send_request(&mut s, service).await?;
            read_status(&mut s).await
        })
        .await
    }

    /// ADB server protocol version (e.g. 41).
    pub async fn version(&self) -> Result<u32, AdbError> {
        let hex = self.host_query("host:version").await?;
        u32::from_str_radix(hex.trim(), 16)
            .map_err(|_| AdbError::Protocol(format!("bad version {hex:?}")))
    }

    pub async fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
        Ok(parse_device_list(&self.host_query("host:devices-l").await?))
    }

    /// Opens `host:track-devices-l`; each `next()` yields the full current list.
    pub async fn track_devices(&self) -> Result<DeviceTracker, AdbError> {
        let mut s = self.dial().await?;
        send_request(&mut s, "host:track-devices-l").await?;
        self.bounded(read_status(&mut s)).await?;
        Ok(DeviceTracker { stream: s })
    }

    pub async fn connect_device(&self, addr: &str) -> Result<ConnectOutcome, AdbError> {
        let msg = self.host_query(&format!("host:connect:{addr}")).await?;
        Ok(parse_connect_reply(&msg))
    }

    pub async fn disconnect_device(&self, addr: &str) -> Result<(), AdbError> {
        self.host_query(&format!("host:disconnect:{addr}")).await.map(|_| ())
    }

    /// TLS pairing with a 6-digit code or a QR password.
    pub async fn pair(&self, code: &str, addr: &str) -> Result<PairOutcome, AdbError> {
        // Pairing does a SPAKE2 + TLS handshake with the phone; allow longer.
        let client = self.clone().with_timeout(self.timeout.max(Duration::from_secs(30)));
        match client.host_query(&format!("host:pair:{code}:{addr}")).await {
            Ok(msg) => Ok(parse_pair_reply(&msg)),
            // adb reports pairing failures as OKAY + nested FAIL.
            Err(AdbError::Fail(msg)) => Ok(parse_pair_reply(&msg)),
            Err(e) => Err(e),
        }
    }

    pub async fn features(&self, serial: &str) -> Result<Vec<String>, AdbError> {
        let text = self.host_query(&format!("host-serial:{serial}:features")).await?;
        Ok(text.split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned).collect())
    }

    /// Creates a forward tunnel. With `local = "tcp:0"` the server allocates a
    /// port and the returned value is that port.
    pub async fn forward(&self, serial: &str, local: &str, remote: &str) -> Result<Option<u16>, AdbError> {
        self.bounded(async {
            let mut s = self.dial().await?;
            send_request(&mut s, &format!("host-serial:{serial}:forward:{local};{remote}")).await?;
            read_status(&mut s).await?; // host accepted the request
            read_status(&mut s).await?; // forward created
            if local == "tcp:0" {
                let port = read_hex_string(&mut s).await?;
                let port = port
                    .trim()
                    .parse()
                    .map_err(|_| AdbError::Protocol(format!("bad forward port {port:?}")))?;
                Ok(Some(port))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn kill_forward(&self, serial: &str, local: &str) -> Result<(), AdbError> {
        self.bounded(async {
            let mut s = self.dial().await?;
            send_request(&mut s, &format!("host-serial:{serial}:killforward:{local}")).await?;
            read_status(&mut s).await?;
            read_status(&mut s).await
        })
        .await
    }

    /// Opens a device service (`shell:`, `sync:`, `tcpip:5555`, …) and returns
    /// the raw stream positioned after the service's `OKAY`.
    pub async fn open_service(&self, serial: &str, service: &str) -> Result<TcpStream, AdbError> {
        self.bounded(async {
            let mut s = self.dial().await?;
            send_request(&mut s, &format!("host:transport:{serial}")).await?;
            read_status(&mut s).await?;
            send_request(&mut s, service).await?;
            read_status(&mut s).await?;
            Ok(s)
        })
        .await
    }

    /// Runs a command to completion. Uses shell protocol v2 (with exit code)
    /// when `shell_v2` is true, the legacy `shell:` service otherwise.
    pub async fn shell(&self, serial: &str, cmd: &str, shell_v2: bool) -> Result<ShellOutput, AdbError> {
        if shell_v2 {
            let mut s = self.open_service(serial, &format!("shell,v2,raw:{cmd}")).await?;
            self.bounded(collect_v2(&mut s)).await
        } else {
            let mut s = self.open_service(serial, &format!("shell:{cmd}")).await?;
            let stdout = self.bounded(read_to_end_string(&mut s)).await?;
            Ok(ShellOutput { stdout, stderr: String::new(), exit_code: None })
        }
    }

    pub async fn getprop(&self, serial: &str, name: &str, shell_v2: bool) -> Result<String, AdbError> {
        let out = self.shell(serial, &format!("getprop {name}"), shell_v2).await?;
        Ok(out.stdout.trim().to_owned())
    }

    /// Restarts adbd on the device listening on TCP `port` (legacy wireless).
    pub async fn tcpip(&self, serial: &str, port: u16) -> Result<String, AdbError> {
        let mut s = self.open_service(serial, &format!("tcpip:{port}")).await?;
        self.bounded(read_to_end_string(&mut s)).await
    }

    pub async fn sync(&self, serial: &str) -> Result<SyncSession<TcpStream>, AdbError> {
        Ok(SyncSession::new(self.open_service(serial, "sync:").await?))
    }
}

pub struct DeviceTracker {
    stream: TcpStream,
}

impl DeviceTracker {
    /// Waits for the next device-list update.
    pub async fn next(&mut self) -> Result<Vec<DeviceEntry>, AdbError> {
        Ok(parse_device_list(&read_hex_string(&mut self.stream).await?))
    }
}

pub fn parse_connect_reply(msg: &str) -> ConnectOutcome {
    let m = msg.trim();
    if m.starts_with("already connected") {
        ConnectOutcome::AlreadyConnected
    } else if m.starts_with("connected to") {
        ConnectOutcome::Connected
    } else {
        ConnectOutcome::Failed(m.to_owned())
    }
}

pub fn parse_pair_reply(msg: &str) -> PairOutcome {
    let m = msg.trim();
    if m.starts_with("Successfully paired") {
        let guid = m
            .split_once("[guid=")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(g, _)| g.to_owned());
        PairOutcome::Paired { guid }
    } else {
        PairOutcome::Failed(m.strip_prefix("Failed:").unwrap_or(m).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_replies() {
        assert_eq!(parse_connect_reply("connected to 192.168.1.4:5555"), ConnectOutcome::Connected);
        assert_eq!(
            parse_connect_reply("already connected to 192.168.1.4:5555"),
            ConnectOutcome::AlreadyConnected
        );
        assert_eq!(
            parse_connect_reply("failed to connect to '192.168.1.4:5555': Connection refused"),
            ConnectOutcome::Failed("failed to connect to '192.168.1.4:5555': Connection refused".into())
        );
    }

    #[test]
    fn parses_pair_replies() {
        assert_eq!(
            parse_pair_reply("Successfully paired to 192.168.1.4:41231 [guid=adb-28291FDH200A1Z-OpVSoG]"),
            PairOutcome::Paired { guid: Some("adb-28291FDH200A1Z-OpVSoG".into()) }
        );
        assert_eq!(
            parse_pair_reply("Successfully paired to 192.168.1.4:41231"),
            PairOutcome::Paired { guid: None }
        );
        assert_eq!(
            parse_pair_reply("Failed: Wrong password or connection was dropped."),
            PairOutcome::Failed("Wrong password or connection was dropped.".into())
        );
    }
}
