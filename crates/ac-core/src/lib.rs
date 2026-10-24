//! Types shared between the tokio device runtime and the GTK UI.

use serde::{Deserialize, Serialize};

/// Slot number, `1..=MAX_SLOTS`.
pub type SlotId = u8;

pub const MAX_SLOTS: usize = 5;

pub fn slot_ids() -> impl Iterator<Item = SlotId> {
    1..=MAX_SLOTS as SlotId
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Android 11+ wireless debugging (TLS, paired).
    Tls,
    /// `adb tcpip 5555`.
    Legacy,
}

/// Error and warning codes from spec Appendix A.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AdbUnavailable,
    NotLan,
    PairFailed,
    PairTimeout,
    ConnectFailed,
    Unauthorized,
    ReconnectGaveUp,
    ServerIncompatible,
    ServerStart,
    StreamInit,
    Protocol,
    Decoder,
    ClipboardTruncated,
    AvahiMissing,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdbUnavailable => "E_ADB_UNAVAILABLE",
            Self::NotLan => "E_NOT_LAN",
            Self::PairFailed => "E_PAIR_FAILED",
            Self::PairTimeout => "E_PAIR_TIMEOUT",
            Self::ConnectFailed => "E_CONNECT_FAILED",
            Self::Unauthorized => "E_UNAUTHORIZED",
            Self::ReconnectGaveUp => "E_RECONNECT_GAVE_UP",
            Self::ServerIncompatible => "E_SERVER_INCOMPATIBLE",
            Self::ServerStart => "E_SERVER_START",
            Self::StreamInit => "E_STREAM_INIT",
            Self::Protocol => "E_PROTOCOL",
            Self::Decoder => "E_DECODER",
            Self::ClipboardTruncated => "W_CLIPBOARD_TRUNCATED",
            Self::AvahiMissing => "W_AVAHI_MISSING",
        }
    }

    pub fn is_warning(self) -> bool {
        self.as_str().starts_with("W_")
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Periodic device telemetry shown in each tile's status strip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceStats {
    pub battery_pct: Option<u8>,
    pub charging: bool,
    pub wifi_rssi_dbm: Option<i16>,
    pub wifi_link_mbps: Option<u32>,
    /// ADB `shell echo` round trip.
    pub rtt_ms: Option<u32>,
}

impl DeviceStats {
    /// RSSI → 1..=4 bars (spec §6.1 legend).
    pub fn wifi_bars(&self) -> Option<u8> {
        self.wifi_rssi_dbm.map(|r| match r {
            r if r >= -55 => 4,
            r if r >= -67 => 3,
            r if r >= -75 => 2,
            _ => 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_bar_thresholds() {
        let bars = |r| DeviceStats { wifi_rssi_dbm: Some(r), ..Default::default() }.wifi_bars();
        assert_eq!(bars(-40), Some(4));
        assert_eq!(bars(-55), Some(4));
        assert_eq!(bars(-56), Some(3));
        assert_eq!(bars(-75), Some(2));
        assert_eq!(bars(-90), Some(1));
        assert_eq!(DeviceStats::default().wifi_bars(), None);
    }

    #[test]
    fn warning_codes() {
        assert!(ErrorCode::AvahiMissing.is_warning());
        assert!(!ErrorCode::ConnectFailed.is_warning());
    }
}
