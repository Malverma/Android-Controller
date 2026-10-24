//! Per-slot connection state machine (spec §3.3), written as a pure
//! function: `Slot::handle(event) -> Vec<Effect>`. The manager performs the
//! effects and feeds results back as events, so every transition is unit
//! testable without sockets.
//!
//! Milestone 1 ends at `Online` (ADB transport up and identified). Milestone 2
//! inserts `PushingServer` and `Streaming` after it.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use ac_adb::DeviceState;
use ac_config::Connection;
use ac_core::{ErrorCode, SlotId, TransportKind};
use ac_discovery::serial_from_instance;

use crate::qr::QrSecret;

/// Attempts for a user/auto-initiated connect before giving up (§3.3.2 #11–12).
pub const MAX_CONNECT_ATTEMPTS: u32 = 6;
pub const PAIR_CODE_TIMEOUT: Duration = Duration::from_secs(30);
pub const PAIR_QR_TIMEOUT: Duration = Duration::from_secs(120);
pub const AWAIT_CONNECT_SERVICE: Duration = Duration::from_secs(30);
pub const AWAIT_ONLINE: Duration = Duration::from_secs(10);
pub const AWAIT_AUTHORIZATION: Duration = Duration::from_secs(30);
/// Legacy devices: start ARP re-resolution from this reconnect attempt on.
pub const LEGACY_LOOKUP_FROM_ATTEMPT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `ro.serialno`, or `aid:<android_id>`.
    pub id: String,
    pub model: String,
    pub manufacturer: String,
    pub sdk: u32,
    pub features: Vec<String>,
    pub wifi_mac: Option<String>,
}

impl DeviceInfo {
    pub fn shell_v2(&self) -> bool {
        self.features.iter().any(|f| f == "shell_v2")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectCause {
    Heartbeat,
    Offline,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Empty,
    Disconnected {
        error: Option<String>,
    },
    Pairing {
        qr: Option<QrSecret>,
        /// Set once `host:pair` is in flight.
        pair_ip: Option<IpAddr>,
        /// Set after a successful pairing while we wait for the connect service.
        paired: bool,
    },
    ConnectingAdb {
        attempt: u32,
        /// `host:connect` succeeded; waiting for the transport to reach `device`.
        connected: bool,
        /// Background auto-connect: failures are not toasted.
        silent: bool,
        unauthorized_warned: bool,
    },
    Online {
        heartbeat_failures: u32,
    },
    Reconnecting {
        attempt: u32,
        since: Instant,
        cause: ReconnectCause,
    },
}

impl Phase {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Disconnected { .. } => "Disconnected",
            Self::Pairing { .. } => "Pairing",
            Self::ConnectingAdb { .. } => "Connecting",
            Self::Online { .. } => "Online",
            Self::Reconnecting { .. } => "Reconnecting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    Backoff,
    PairDeadline,
    AwaitOnline,
    AwaitConnectService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotEvent {
    Connect { addr: String, transport: TransportKind, expect_serial: Option<String>, silent: bool },
    /// A network transport adb already has (adb's own mDNS auto-connect, or a
    /// CLI `adb connect`) is taken over by this slot.
    Adopt { adb_serial: String, transport: TransportKind },
    /// `connect` is the phone's `ip:port` from Wireless debugging, when the
    /// user typed it: connect there after pairing instead of waiting for mDNS.
    PairCode { addr: SocketAddr, code: String, connect: Option<String> },
    PairQr { secret: QrSecret },
    QrServiceSeen { addr: SocketAddr },
    /// `Ok(guid)` where guid is `adb-<serial>-<suffix>` when adb reports it.
    PairDone(Result<Option<String>, String>),
    ConnectDone(Result<(), String>),
    AdbState { serial: String, state: DeviceState },
    AdbGone { serial: String },
    Identified(DeviceInfo),
    IdentifyFailed(String),
    Heartbeat(Result<u32, String>),
    /// `_adb-tls-connect._tcp` announced for this slot's device.
    ConnectSeen { addr: SocketAddr },
    LegacyAddrFound { addr: String },
    Timer { kind: TimerKind, generation: u64 },
    Disconnect,
    Forget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Pair { code: String, addr: SocketAddr },
    Connect { addr: String },
    /// `host:disconnect` then `host:connect`, so adb drops a stale transport.
    Reconnect { addr: String },
    Disconnect { serial: String },
    Identify { serial: String },
    StartMonitor { serial: String, shell_v2: bool },
    StopMonitor,
    Timer { kind: TimerKind, after: Duration, generation: u64 },
    /// Look for an already-discovered connect service for `serial` (or on `ip`).
    FindConnectService { serial: Option<String>, ip: Option<IpAddr> },
    LookupLegacy { mac: String, port: u16 },
    /// Ask the network again for `_adb-tls-connect._tcp` services.
    RescanConnect,
    Persist,
    Forget { id: String },
    Toast { code: ErrorCode, message: String },
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: SlotId,
    pub phase: Phase,
    /// Identity bound to the slot (after `Identified`).
    pub device: Option<DeviceInfo>,
    /// Device id remembered from devices.toml before this session identified it.
    pub known_id: Option<String>,
    pub alias: String,
    pub transport: TransportKind,
    /// Last `ip:port` we connected to.
    pub addr: Option<String>,
    /// Current adb transport serial (`ip:port` or mDNS form).
    pub adb_serial: Option<String>,
    /// Device serial we expect from mDNS (pairing guid, or devices.toml).
    pub expect_serial: Option<String>,
    pub wifi_mac: Option<String>,
    /// Address to connect to once pairing succeeds (manual pairing).
    pair_connect: Option<String>,
    identify_inflight: bool,
    generation: u64,
}

impl Slot {
    pub fn new(id: SlotId) -> Self {
        Self {
            id,
            phase: Phase::Empty,
            device: None,
            known_id: None,
            alias: String::new(),
            transport: TransportKind::Tls,
            addr: None,
            adb_serial: None,
            expect_serial: None,
            wifi_mac: None,
            pair_connect: None,
            identify_inflight: false,
            generation: 0,
        }
    }

    /// Slot restored from devices.toml: shows the device, not yet connected.
    pub fn restored(id: SlotId, device_id: &str, transport: TransportKind, addr: Option<String>, mac: Option<String>) -> Self {
        Self {
            phase: Phase::Disconnected { error: None },
            known_id: Some(device_id.to_owned()),
            expect_serial: Some(device_id.to_owned()),
            transport,
            addr,
            wifi_mac: mac,
            ..Self::new(id)
        }
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device.as_ref().map(|d| d.id.as_str()).or(self.known_id.as_deref())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Free for a new device: empty, or disconnected with no remembered device.
    pub fn is_free(&self) -> bool {
        match self.phase {
            Phase::Empty => true,
            Phase::Disconnected { .. } => self.device_id().is_none(),
            _ => false,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Pairing { .. } | Phase::ConnectingAdb { .. } | Phase::Online { .. } | Phase::Reconnecting { .. })
    }

    /// Does an adb transport with this serial belong to this slot?
    pub fn wants_adb_serial(&self, serial: &str) -> bool {
        if self.adb_serial.as_deref() == Some(serial) || self.addr.as_deref() == Some(serial) {
            return true;
        }
        let Some(mdns_serial) = serial_from_instance(serial) else { return false };
        let expecting = matches!(
            self.phase,
            Phase::Pairing { paired: true, .. } | Phase::ConnectingAdb { .. } | Phase::Reconnecting { .. } | Phase::Disconnected { .. }
        );
        expecting && (self.expect_serial.as_deref() == Some(mdns_serial) || self.device_id() == Some(mdns_serial))
    }

    fn set_phase(&mut self, phase: Phase) {
        self.generation += 1;
        self.identify_inflight = false;
        self.phase = phase;
    }

    /// Arms the slot's single timer; any previously armed timer becomes stale.
    fn timer(&mut self, kind: TimerKind, after: Duration) -> Effect {
        self.generation += 1;
        Effect::Timer { kind, after, generation: self.generation }
    }

    fn backoff(cfg: &Connection, attempt: u32) -> Duration {
        let steps = &cfg.reconnect_backoff_ms;
        let ms = steps.get(attempt as usize).or(steps.last()).copied().unwrap_or(1000);
        Duration::from_millis(ms)
    }

    fn disconnect_effect(&self) -> Option<Effect> {
        let connected = matches!(
            self.phase,
            Phase::ConnectingAdb { connected: true, .. } | Phase::Online { .. } | Phase::Reconnecting { .. }
        );
        let serial = self.adb_serial.clone().or_else(|| self.addr.clone())?;
        connected.then_some(Effect::Disconnect { serial })
    }

    fn fail_to_disconnected(&mut self, code: ErrorCode, message: String, toast: bool) -> Vec<Effect> {
        self.set_phase(Phase::Disconnected { error: Some(message.clone()) });
        if toast { vec![Effect::Toast { code, message }] } else { Vec::new() }
    }

    fn start_connect(&mut self, addr: String, transport: TransportKind, expect_serial: Option<String>, silent: bool) -> Vec<Effect> {
        self.addr = Some(addr.clone());
        self.adb_serial = None;
        self.transport = transport;
        if expect_serial.is_some() {
            self.expect_serial = expect_serial;
        }
        self.set_phase(Phase::ConnectingAdb { attempt: 0, connected: false, silent, unauthorized_warned: false });
        vec![Effect::Connect { addr }]
    }

    fn enter_reconnecting(&mut self, cause: ReconnectCause, now: Instant, cfg: &Connection) -> Vec<Effect> {
        self.set_phase(Phase::Reconnecting { attempt: 0, since: now, cause });
        vec![Effect::StopMonitor, self.timer(TimerKind::Backoff, Self::backoff(cfg, 0))]
    }

    fn identify(&mut self, serial: String) -> Vec<Effect> {
        self.adb_serial = Some(serial.clone());
        if self.identify_inflight {
            return Vec::new();
        }
        self.identify_inflight = true;
        vec![Effect::Identify { serial }]
    }

    pub fn handle(&mut self, ev: SlotEvent, now: Instant, cfg: &Connection) -> Vec<Effect> {
        use Phase as P;
        use SlotEvent as E;

        match (self.phase.clone(), ev) {
            // ---- user / auto connect ------------------------------------------
            (P::Empty | P::Disconnected { .. } | P::Reconnecting { .. }, E::Connect { addr, transport, expect_serial, silent }) => {
                let mut fx = Vec::new();
                if matches!(self.phase, P::Reconnecting { .. }) {
                    fx.extend(self.disconnect_effect());
                }
                fx.extend(self.start_connect(addr, transport, expect_serial, silent));
                fx
            }
            (P::Empty | P::Disconnected { .. }, E::Adopt { adb_serial, transport }) => {
                self.transport = transport;
                if adb_serial.contains(':') && serial_from_instance(&adb_serial).is_none() {
                    self.addr = Some(adb_serial.clone());
                }
                self.set_phase(P::ConnectingAdb { attempt: 0, connected: true, silent: true, unauthorized_warned: false });
                let mut fx = self.identify(adb_serial);
                fx.push(self.timer(TimerKind::AwaitOnline, AWAIT_ONLINE));
                fx
            }

            // ---- pairing --------------------------------------------------------
            (P::Empty | P::Disconnected { .. }, E::PairCode { addr, code, connect }) => {
                self.transport = TransportKind::Tls;
                self.pair_connect = connect;
                self.set_phase(P::Pairing { qr: None, pair_ip: Some(addr.ip()), paired: false });
                vec![Effect::Pair { code, addr }, self.timer(TimerKind::PairDeadline, PAIR_CODE_TIMEOUT)]
            }
            (P::Empty | P::Disconnected { .. }, E::PairQr { secret }) => {
                self.transport = TransportKind::Tls;
                self.pair_connect = None;
                self.set_phase(P::Pairing { qr: Some(secret), pair_ip: None, paired: false });
                vec![self.timer(TimerKind::PairDeadline, PAIR_QR_TIMEOUT)]
            }
            (P::Pairing { qr: Some(secret), pair_ip: None, paired: false }, E::QrServiceSeen { addr }) => {
                self.phase = P::Pairing { qr: Some(secret.clone()), pair_ip: Some(addr.ip()), paired: false };
                vec![Effect::Pair { code: secret.password, addr }]
            }
            (P::Pairing { pair_ip, paired: false, .. }, E::PairDone(Ok(guid))) => {
                let serial = guid.as_deref().and_then(serial_from_instance).map(str::to_owned);
                self.expect_serial = serial.clone();
                if let Some(addr) = self.pair_connect.take() {
                    return self.start_connect(addr, TransportKind::Tls, serial, false);
                }
                self.phase = P::Pairing { qr: None, pair_ip, paired: true };
                vec![
                    Effect::FindConnectService { serial, ip: pair_ip },
                    self.timer(TimerKind::AwaitConnectService, AWAIT_CONNECT_SERVICE),
                ]
            }
            (P::Pairing { paired: false, .. }, E::PairDone(Err(msg))) => {
                self.fail_to_disconnected(ErrorCode::PairFailed, format!("Pairing failed: {msg}"), true)
            }
            (P::Pairing { paired: true, .. }, E::ConnectSeen { addr }) => {
                let expect = self.expect_serial.clone();
                self.start_connect(addr.to_string(), TransportKind::Tls, expect, false)
            }
            (P::Pairing { paired: true, .. }, E::AdbState { serial, state: DeviceState::Device }) => {
                // adb auto-connected the freshly paired device via its own mDNS.
                self.set_phase(P::ConnectingAdb { attempt: 0, connected: true, silent: false, unauthorized_warned: false });
                let mut fx = self.identify(serial);
                fx.push(self.timer(TimerKind::AwaitOnline, AWAIT_ONLINE));
                fx
            }

            // ---- connecting -----------------------------------------------------
            (P::ConnectingAdb { attempt, silent, unauthorized_warned, connected: false }, E::ConnectDone(Ok(()))) => {
                self.adb_serial = self.addr.clone();
                self.phase = P::ConnectingAdb { attempt, connected: true, silent, unauthorized_warned };
                vec![self.timer(TimerKind::AwaitOnline, AWAIT_ONLINE)]
            }
            (P::ConnectingAdb { attempt, silent, .. }, E::ConnectDone(Err(msg)) | E::IdentifyFailed(msg)) => {
                self.connect_failed(attempt, silent, msg, cfg)
            }
            (P::ConnectingAdb { attempt, silent, connected: true, .. }, E::Timer { kind: TimerKind::AwaitOnline, generation })
                if generation == self.generation =>
            {
                self.connect_failed(attempt, silent, "device did not come online".into(), cfg)
            }
            (P::ConnectingAdb { attempt, silent, unauthorized_warned, .. }, E::Timer { kind: TimerKind::Backoff, generation })
                if generation == self.generation =>
            {
                self.phase = P::ConnectingAdb { attempt, connected: false, silent, unauthorized_warned };
                self.addr.clone().map(|addr| vec![Effect::Connect { addr }]).unwrap_or_default()
            }
            (P::ConnectingAdb { connected: false, silent, .. }, E::ConnectSeen { addr })
                if self.addr.as_deref() != Some(addr.to_string().as_str()) =>
            {
                // mDNS reports the current port (wireless debugging picks a new
                // one each time it is toggled); stop retrying the stale one.
                let expect = self.expect_serial.clone();
                self.start_connect(addr.to_string(), TransportKind::Tls, expect, silent)
            }
            (P::ConnectingAdb { .. } | P::Reconnecting { .. }, E::AdbState { serial, state: DeviceState::Device }) => {
                self.identify(serial)
            }
            (P::ConnectingAdb { attempt, silent, unauthorized_warned: false, .. }, E::AdbState { state: DeviceState::Unauthorized, .. }) => {
                self.phase = P::ConnectingAdb { attempt, connected: true, silent, unauthorized_warned: true };
                vec![
                    Effect::Toast {
                        code: ErrorCode::Unauthorized,
                        message: "Accept the USB debugging prompt on the phone".into(),
                    },
                    self.timer(TimerKind::AwaitOnline, AWAIT_AUTHORIZATION),
                ]
            }
            (P::ConnectingAdb { attempt, silent, connected: true, .. }, E::AdbGone { .. }) => {
                self.connect_failed(attempt, silent, "transport closed during connect".into(), cfg)
            }
            (P::ConnectingAdb { .. } | P::Reconnecting { .. } | P::Pairing { .. }, E::Identified(info)) => {
                self.identified(info)
            }

            // ---- online ---------------------------------------------------------
            (P::Online { heartbeat_failures }, E::Heartbeat(result)) => match result {
                Ok(_) => {
                    self.phase = P::Online { heartbeat_failures: 0 };
                    Vec::new()
                }
                Err(_) if heartbeat_failures + 1 >= cfg.heartbeat_failures_to_reconnect => {
                    self.enter_reconnecting(ReconnectCause::Heartbeat, now, cfg)
                }
                Err(_) => {
                    self.phase = P::Online { heartbeat_failures: heartbeat_failures + 1 };
                    Vec::new()
                }
            },
            (P::Online { .. }, E::AdbState { state, .. }) if state != DeviceState::Device => {
                self.enter_reconnecting(ReconnectCause::Offline, now, cfg)
            }
            (P::Online { .. }, E::AdbGone { .. }) => self.enter_reconnecting(ReconnectCause::Gone, now, cfg),

            // ---- reconnecting ---------------------------------------------------
            (P::Reconnecting { attempt, since, cause }, E::Timer { kind: TimerKind::Backoff, generation })
                if generation == self.generation =>
            {
                if now.duration_since(since) >= Duration::from_secs(cfg.reconnect_give_up_after_s) {
                    let name = self.display_name();
                    return self.fail_to_disconnected(ErrorCode::ReconnectGaveUp, format!("{name} is offline"), true);
                }
                let attempt = attempt + 1;
                self.phase = P::Reconnecting { attempt, since, cause };
                let mut fx = Vec::new();
                if let Some(addr) = self.addr.clone() {
                    fx.push(Effect::Reconnect { addr });
                }
                if self.transport == TransportKind::Tls {
                    // The port may have changed; only a fresh mDNS answer tells us.
                    fx.push(Effect::RescanConnect);
                }
                if self.transport == TransportKind::Legacy
                    && attempt >= LEGACY_LOOKUP_FROM_ATTEMPT
                    && let Some(mac) = self.wifi_mac.clone()
                {
                    let port = self.addr.as_deref().and_then(port_of).unwrap_or(5555);
                    fx.push(Effect::LookupLegacy { mac, port });
                }
                fx.push(self.timer(TimerKind::Backoff, Self::backoff(cfg, attempt)));
                fx
            }
            (P::Reconnecting { .. }, E::ConnectSeen { addr }) => {
                let addr = addr.to_string();
                let mut fx = Vec::new();
                if self.addr.as_deref() != Some(addr.as_str())
                    && let Some(serial) = self.adb_serial.clone().or_else(|| self.addr.clone())
                {
                    fx.push(Effect::Disconnect { serial });
                }
                self.addr = Some(addr.clone());
                self.adb_serial = Some(addr.clone());
                fx.push(Effect::Connect { addr });
                fx
            }
            (P::Reconnecting { .. }, E::LegacyAddrFound { addr }) if self.addr.as_deref() != Some(addr.as_str()) => {
                self.addr = Some(addr.clone());
                self.adb_serial = Some(addr.clone());
                vec![Effect::Reconnect { addr }]
            }
            (P::Reconnecting { .. }, E::IdentifyFailed(_)) => {
                self.identify_inflight = false;
                Vec::new()
            }

            // ---- disconnected: mDNS says the remembered device is back ------------
            (P::Disconnected { .. }, E::ConnectSeen { addr }) if self.device_id().is_some() => {
                let expect = self.device_id().map(str::to_owned);
                self.start_connect(addr.to_string(), TransportKind::Tls, expect, true)
            }

            // ---- pairing timeouts ------------------------------------------------
            (P::Pairing { paired: false, qr, .. }, E::Timer { kind: TimerKind::PairDeadline, generation })
                if generation == self.generation =>
            {
                let msg = if qr.is_some() { "QR code expired — refresh and scan again" } else { "Pairing timed out" };
                self.fail_to_disconnected(ErrorCode::PairTimeout, msg.into(), true)
            }
            (P::Pairing { paired: true, .. }, E::Timer { kind: TimerKind::AwaitConnectService, generation })
                if generation == self.generation =>
            {
                self.fail_to_disconnected(
                    ErrorCode::ConnectFailed,
                    "Paired, but the phone is not advertising a connect port. Enter the IP:port shown under Wireless debugging.".into(),
                    true,
                )
            }

            // ---- teardown ----------------------------------------------------------
            (P::Empty, E::Disconnect | E::Forget) => Vec::new(),
            (_, E::Disconnect) => {
                let mut fx = vec![Effect::StopMonitor];
                fx.extend(self.disconnect_effect());
                self.adb_serial = None;
                let phase = if self.device_id().is_some() { P::Disconnected { error: None } } else { P::Empty };
                self.set_phase(phase);
                fx
            }
            (_, E::Forget) => {
                let mut fx = vec![Effect::StopMonitor];
                fx.extend(self.disconnect_effect());
                if let Some(id) = self.device_id() {
                    fx.push(Effect::Forget { id: id.to_owned() });
                }
                let id = self.id;
                *self = Slot { generation: self.generation + 1, ..Slot::new(id) };
                fx
            }

            // Everything else (stale timers, duplicate tracker updates, …) is a no-op.
            _ => Vec::new(),
        }
    }

    fn connect_failed(&mut self, attempt: u32, silent: bool, msg: String, cfg: &Connection) -> Vec<Effect> {
        let attempt = attempt + 1;
        if attempt >= MAX_CONNECT_ATTEMPTS {
            let addr = self.addr.clone().unwrap_or_default();
            let mut fx: Vec<Effect> = self.disconnect_effect().into_iter().collect();
            fx.extend(self.fail_to_disconnected(ErrorCode::ConnectFailed, format!("Could not connect to {addr}: {msg}"), !silent));
            return fx;
        }
        self.identify_inflight = false;
        self.phase = Phase::ConnectingAdb { attempt, connected: false, silent, unauthorized_warned: false };
        vec![self.timer(TimerKind::Backoff, Self::backoff(cfg, attempt - 1))]
    }

    fn identified(&mut self, info: DeviceInfo) -> Vec<Effect> {
        let reconnecting = matches!(self.phase, Phase::Reconnecting { .. });
        if reconnecting && self.device_id().is_some_and(|id| id != info.id) {
            // Another phone now answers at the old address (DHCP reuse). Drop
            // it and keep looking for ours.
            self.identify_inflight = false;
            let serial = self.adb_serial.take().unwrap_or_default();
            return vec![Effect::Disconnect { serial }];
        }
        if self.wifi_mac.is_none() {
            self.wifi_mac = info.wifi_mac.clone();
        }
        let serial = self.adb_serial.clone().or_else(|| self.addr.clone()).unwrap_or_default();
        let shell_v2 = info.shell_v2();
        self.known_id = Some(info.id.clone());
        self.expect_serial = Some(info.id.clone());
        self.device = Some(info);
        self.set_phase(Phase::Online { heartbeat_failures: 0 });
        vec![Effect::StartMonitor { serial, shell_v2 }, Effect::Persist]
    }

    pub fn display_name(&self) -> String {
        if !self.alias.is_empty() {
            return self.alias.clone();
        }
        if let Some(d) = &self.device
            && !d.model.is_empty()
        {
            return d.model.clone();
        }
        self.device_id().or(self.addr.as_deref()).unwrap_or("New device").to_owned()
    }
}

fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':')?.1.parse().ok()
}
