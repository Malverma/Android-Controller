//! Native client for the ADB server (`adb start-server`, default
//! `127.0.0.1:5037`). Talks the smart-socket protocol directly instead of
//! spawning the `adb` CLI.

mod client;
pub mod devices;
pub mod protocol;
pub mod shell;
pub mod sync;

pub use client::{
    AdbClient, ConnectOutcome, DEFAULT_SERVER, DeviceTracker, PairOutcome, parse_connect_reply,
    parse_pair_reply,
};
pub use devices::{DeviceEntry, DeviceState};
pub use shell::{ShellOutput, quote};

#[derive(Debug, thiserror::Error)]
pub enum AdbError {
    #[error("I/O error talking to adb: {0}")]
    Io(#[from] std::io::Error),
    /// The server or device answered `FAIL`.
    #[error("adb: {0}")]
    Fail(String),
    #[error("adb protocol error: {0}")]
    Protocol(String),
    #[error("adb request timed out")]
    Timeout,
}

impl AdbError {
    /// True when the ADB server itself could not be reached.
    pub fn is_server_unreachable(&self) -> bool {
        matches!(self, Self::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused)
    }
}
