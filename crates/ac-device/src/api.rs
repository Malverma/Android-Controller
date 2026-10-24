//! Messages between the UI and the device manager.

use std::net::SocketAddr;
use std::time::Duration;

use ac_core::{DeviceStats, ErrorCode, SlotId, TransportKind};
use ac_discovery::ServiceKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// `addr` is `ip[:port]`; port 5555 (or none) means a legacy transport.
    Connect { slot: Option<SlotId>, addr: String },
    /// Pair with the 6-digit code shown for a discovered pairing service.
    PairCode { slot: Option<SlotId>, instance: String, code: String },
    /// Pair by typed address, for networks where mDNS does not reach this PC
    /// (e.g. a wired PC and a router that filters multicast to Wi-Fi).
    /// `pair_addr` is the `ip:port` from the phone's pairing-code screen;
    /// `connect` is the port (or `ip:port`) shown under Wireless debugging.
    PairAddr { slot: Option<SlotId>, pair_addr: String, code: String, connect: Option<String> },
    /// Pair with the Android helper's "PC connection without ADB" (phones
    /// without wireless debugging): `addr` is `ip[:port]` as the helper shows
    /// it, `code` the 6 digits on its screen. Connects once paired.
    PairDirect { slot: Option<SlotId>, addr: String, code: String },
    /// Start a QR pairing session; answered with `UiEvent::QrCode`.
    PairQr { slot: Option<SlotId> },
    CancelPairing { slot: SlotId },
    /// Legacy bootstrap: switch a USB-attached phone to `adb tcpip 5555` and
    /// connect to it over Wi-Fi.
    EnableWifiAdb { usb_serial: String, slot: Option<SlotId> },
    Disconnect { slot: SlotId },
    /// Connect a remembered, disconnected slot to its last known address.
    Reconnect { slot: SlotId },
    Forget { slot: SlotId },
    Rename { slot: SlotId, alias: String },
    /// Grid ⇄ focus (spec §4.4): restarts only this slot's stream.
    SetProfile { slot: SlotId, profile: crate::StreamProfile },
    /// Retry mirroring after it failed.
    RestartStream { slot: SlotId },
    /// Grid tile's longest side in device pixels, for `max_size = "auto"`.
    SetTileSize { slot: SlotId, px: u32 },
    /// Send files and folders to each slot's phone (spec §5). APKs at the top
    /// level are installed when `install_apks`, copied otherwise.
    SendFiles { slots: Vec<SlotId>, paths: Vec<std::path::PathBuf>, install_apks: bool },
    CancelTransfers { slot: SlotId },
    /// Keep the phone's screen on (never time out) or restore its timeout.
    SetStayAwake { slot: SlotId, on: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseView {
    Empty,
    Disconnected,
    Pairing,
    Connecting,
    Online,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    pub slot: SlotId,
    pub phase: PhaseView,
    /// Alias, model, or id.
    pub title: String,
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub sdk: Option<u32>,
    pub addr: Option<String>,
    pub transport: TransportKind,
    /// Human-readable sub-status ("attempt 3", "waiting for phone…").
    pub detail: String,
    pub error: Option<String>,
    /// "Keep screen on" while Online; `None` until read from the phone.
    pub stay_awake: Option<bool>,
    /// Something the user should fix on the phone (e.g. touch control off).
    pub warning: Option<String>,
    pub stream: StreamView,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StreamView {
    /// Not online, or mirroring not started.
    #[default]
    Off,
    Starting,
    Live { width: u32, height: u32 },
    Failed(String),
    /// scrcpy-server missing or incompatible.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyView {
    pub instance: String,
    pub kind: ServiceKind,
    pub addr: SocketAddr,
    pub serial: Option<String>,
    /// Name from devices.toml when this is a remembered device.
    pub known_name: Option<String>,
    /// Slot currently holding this device.
    pub in_slot: Option<SlotId>,
    /// `Direct` only: the helper's "Pair with PC" is open.
    pub pairing_open: bool,
    /// `Direct` only: this PC holds a key for the phone.
    pub paired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceView {
    pub serial: String,
    pub model: String,
    pub authorized: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Slot(SlotView),
    Stats { slot: SlotId, stats: DeviceStats },
    Nearby(Vec<NearbyView>),
    UsbDevices(Vec<UsbDeviceView>),
    QrCode { slot: SlotId, payload: String, expires_in: Duration },
    Toast { code: Option<ErrorCode>, message: String },
    AdbStatus { version: Option<u32>, error: Option<String> },
    DiscoveryStatus { available: bool, message: String },
    /// The phone's clipboard changed (scrcpy clipboard autosync).
    DeviceClipboard { slot: SlotId, text: String },
    /// A slot's transfer progress, ~4 Hz while active and once when it ends.
    Transfer(TransferView),
}

/// Aggregate progress of one slot's queued transfers (spec §5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferView {
    pub slot: SlotId,
    /// False once everything finished or was cancelled.
    pub active: bool,
    /// File being sent (the most recently started one).
    pub file: String,
    pub installing: bool,
    /// 1-based position among `count` files.
    pub index: u32,
    pub count: u32,
    pub sent: u64,
    pub total: u64,
    pub rate_bps: u64,
    /// `None` for the first 2 s and while stalled.
    pub eta_s: Option<u64>,
    /// Paused until the phone reconnects.
    pub waiting: bool,
}
