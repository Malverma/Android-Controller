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
}
