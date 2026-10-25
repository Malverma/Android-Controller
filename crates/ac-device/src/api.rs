//! Messages between the UI and the device manager.

use std::net::SocketAddr;
use std::time::Duration;

use ac_core::{DeviceStats, ErrorCode, SlotId, TransportKind};
use ac_discovery::ServiceKind;

#[derive(Debug, Clone, PartialEq)]
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
    /// Settings changed in Preferences. Stream settings apply the next time
    /// a phone starts mirroring; ADB paths and slot count after a restart.
    UpdateConfig(Box<ac_config::Config>),
    /// App testing (Milestone 5): run an action on each phone; each answers
    /// with `UiEvent::AppResult`. Installing goes through `SendFiles`.
    AppAction { slots: Vec<SlotId>, action: AppAction },
    /// The phone's own (not system) apps; answered with `UiEvent::Packages`.
    ListPackages { slot: SlotId },
    /// The app being tested: its memory is sampled with the other stats.
    SetTestApp { package: Option<String> },
    /// Start a test run: each phone's log, crashes and stats are saved in
    /// one folder until `StopRun`.
    StartRun { name: String },
    StopRun,
    /// Save a screenshot of each phone (full resolution over ADB).
    Screenshot { slots: Vec<SlotId> },
    /// Start or stop recording each phone's mirror to an MP4 file.
    Record { slots: Vec<SlotId>, on: bool },
    /// Sample frame rate, CPU, memory and temperature (always on during a run).
    SetPerf { on: bool },
    /// Install the Wireless Debugging Keeper this app ships over an older one.
    UpdateHelper { slot: SlotId },
    /// The user's answer to `UiEvent::Confirm`.
    Confirm { id: u64, ok: bool },
    /// The window saved the frame asked for by `UiEvent::SaveFrame { request: Some(_) }`.
    FrameSaved { request: u64, error: Option<String> },
}

/// Something to do with an installed app, by package name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    Launch(String),
    Stop(String),
    ClearData(String),
    Uninstall(String),
}

impl AppAction {
    pub fn package(&self) -> &str {
        match self {
            Self::Launch(p) | Self::Stop(p) | Self::ClearData(p) | Self::Uninstall(p) => p,
        }
    }

    pub fn kind(&self) -> AppKind {
        match self {
            Self::Launch(_) => AppKind::Launch,
            Self::Stop(_) => AppKind::Stop,
            Self::ClearData(_) => AppKind::ClearData,
            Self::Uninstall(_) => AppKind::Uninstall,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Install,
    Launch,
    Stop,
    ClearData,
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    /// An uncaught Java/Kotlin exception.
    Crash,
    /// Application Not Responding.
    Anr,
    /// A native (C/C++) crash, e.g. in a game engine.
    Native,
    /// An exception the game's debug add-on reported (the game kept running).
    Exception,
}

impl CrashKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Crash => "Crash",
            Self::Anr => "ANR",
            Self::Native => "Native crash",
            Self::Exception => "Exception",
        }
    }
}

/// A crash or ANR seen in a phone's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashView {
    pub kind: CrashKind,
    pub package: String,
    /// One line: the exception, the ANR reason or the signal.
    pub summary: String,
    /// The phone's log time, `MM-DD hh:mm:ss.mmm`.
    pub time: String,
    /// The stack trace (up to 40 lines).
    pub detail: String,
}

/// One performance sample of a phone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PerfSample {
    /// Frames the app on screen drew in the last second (`None`: unknown).
    pub fps: Option<f32>,
    pub cpu_pct: Option<f32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    /// The test app's memory (PSS), when one is set.
    pub app_mem_mb: Option<u32>,
    pub app: Option<String>,
    pub cpu_temp_c: Option<f32>,
    pub battery_temp_c: Option<f32>,
}

/// The test run, if one is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    pub active: bool,
    pub name: String,
    pub dir: std::path::PathBuf,
    pub crashes: usize,
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
    /// The phone's helper app is older than the one this app ships.
    pub helper_update: Option<String>,
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
    /// How an app action (or an APK install) went on one phone.
    AppResult { slot: SlotId, kind: AppKind, ok: bool, message: String },
    Packages { slot: SlotId, packages: Vec<String> },
    /// An app crashed or stopped responding on the phone.
    Crash { slot: SlotId, crash: CrashView },
    Run(RunView),
    Perf { slot: SlotId, sample: PerfSample },
    /// A phone without ADB has no screenshot command: save its mirror's
    /// current frame to `path` instead.
    /// `request`: an automation request waiting for `UiCommand::FrameSaved`
    /// (no toast then).
    SaveFrame { slot: SlotId, path: std::path::PathBuf, request: Option<u64> },
    /// Recording started (`Some(file)`) or stopped on a phone.
    Recording { slot: SlotId, file: Option<std::path::PathBuf> },
    /// Files were saved: a toast with an "Open folder" button.
    Saved { message: String, folder: std::path::PathBuf },
    /// An automation client (e.g. Claude through the MCP server) wants to do
    /// something that loses data: answer with `UiCommand::Confirm`.
    Confirm { id: u64, title: String, body: String },
    /// How many automation clients are connected.
    Automation { clients: usize },
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
