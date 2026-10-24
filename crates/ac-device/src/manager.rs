//! The device manager actor: owns the slots, performs their effects, and
//! routes ADB tracker and mDNS events to them.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ac_adb::{AdbClient, ConnectOutcome, DeviceEntry, DeviceState, PairOutcome};
use ac_config::{Config, DeviceRecord, Devices, Paths};
use ac_core::{DeviceStats, ErrorCode, MAX_SLOTS, SlotId, TransportKind};
use ac_discovery::{DiscoveredDevice, DiscoveryEvent, ServiceKind, is_lan, serial_from_instance};
use rand::Rng;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::api::*;
use crate::probe::{IDENTIFY_CMD, STATS_CMD, parse_identity, parse_stats};
use crate::qr::QrSecret;
use crate::slot::{DeviceInfo, Effect, PAIR_QR_TIMEOUT, Phase, Slot, SlotEvent, TimerKind};

/// Stale mDNS entries are kept this long so brief flaps don't flicker the UI.
const NEARBY_GRACE: Duration = Duration::from_secs(30);
/// Run the stats command every N heartbeats.
const STATS_EVERY: u32 = 5;

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub config: Config,
    /// `None` keeps devices in memory only (tests).
    pub paths: Option<Paths>,
    pub discovery: bool,
    pub arp_table: PathBuf,
}

impl ManagerOptions {
    pub fn new(config: Config, paths: Option<Paths>) -> Self {
        Self { config, paths, discovery: true, arp_table: "/proc/net/arp".into() }
    }
}

pub struct ManagerHandle {
    pub commands: async_channel::Sender<UiCommand>,
    pub events: async_channel::Receiver<UiEvent>,
    /// Lets tests inject discovery events.
    pub discovery_tx: mpsc::UnboundedSender<DiscoveryEvent>,
    task: JoinHandle<()>,
}

impl ManagerHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

enum Msg {
    Ui(UiCommand),
    Slot(SlotId, SlotEvent),
    ConnectResult(SlotId, Result<(), String>),
    AdbDevices(Vec<DeviceEntry>),
    AdbStatus(Option<u32>, Option<String>),
    Stats(SlotId, DeviceStats),
    Toast(Option<ErrorCode>, String),
    Prune,
}

struct Nearby {
    dev: DiscoveredDevice,
    stale_since: Option<Instant>,
}

pub fn spawn(opts: ManagerOptions) -> ManagerHandle {
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (disc_tx, disc_rx) = mpsc::unbounded_channel();
    let discovery_tx = disc_tx.clone();
    let task = tokio::spawn(async move {
        let mut m = Manager::new(opts, ev_tx);
        m.run(cmd_rx, disc_tx, disc_rx).await;
    });
    ManagerHandle { commands: cmd_tx, events: ev_rx, discovery_tx, task }
}

struct Manager {
    cfg: Config,
    paths: Option<Paths>,
    arp_table: PathBuf,
    discovery: bool,
    adb: AdbClient,
    devices: Devices,
    slots: Vec<Slot>,
    timers: Vec<Option<JoinHandle<()>>>,
    monitors: Vec<Option<JoinHandle<()>>>,
    views: Vec<Option<SlotView>>,
    nearby: BTreeMap<(ServiceKind, String), Nearby>,
    last_nearby: Vec<NearbyView>,
    adb_snapshot: Vec<DeviceEntry>,
    last_usb: Vec<UsbDeviceView>,
    /// Device ids / adb serials the user disconnected: never auto-adopt or
    /// auto-connect them until the user connects again.
    suppressed: HashSet<String>,
    tx: mpsc::UnboundedSender<Msg>,
    rx: Option<mpsc::UnboundedReceiver<Msg>>,
    ui: async_channel::Sender<UiEvent>,
}

impl Manager {
    fn new(opts: ManagerOptions, ui: async_channel::Sender<UiEvent>) -> Self {
        let devices = match &opts.paths {
            Some(p) => ac_config::load_devices(&p.devices).unwrap_or_else(|e| {
                tracing::warn!("{e}; starting with no saved devices");
                Devices::default()
            }),
            None => Devices::default(),
        };
        let mut slots: Vec<Slot> = (1..=MAX_SLOTS as SlotId).map(Slot::new).collect();
        for rec in devices.devices.iter().filter(|d| d.slot != 0) {
            let mut s = Slot::restored(rec.slot, &rec.id, rec.transport, rec.last_address.clone(), rec.wifi_mac.clone());
            s.alias = rec.alias.clone();
            slots[rec.slot as usize - 1] = s;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let adb = AdbClient::new(opts.config.general.adb_server)
            .with_timeout(Duration::from_millis(opts.config.connection.connect_timeout_ms.max(1000) * 2));
        Self {
            adb,
            paths: opts.paths,
            arp_table: opts.arp_table,
            discovery: opts.discovery,
            devices,
            slots,
            timers: (0..MAX_SLOTS).map(|_| None).collect(),
            monitors: (0..MAX_SLOTS).map(|_| None).collect(),
            views: vec![None; MAX_SLOTS],
            nearby: BTreeMap::new(),
            last_nearby: Vec::new(),
            adb_snapshot: Vec::new(),
            last_usb: Vec::new(),
            suppressed: HashSet::new(),
            tx,
            rx: Some(rx),
            ui,
            cfg: opts.config,
        }
    }

    async fn run(
        &mut self,
        commands: async_channel::Receiver<UiCommand>,
        disc_tx: mpsc::UnboundedSender<DiscoveryEvent>,
        mut disc_rx: mpsc::UnboundedReceiver<DiscoveryEvent>,
    ) {
        let mut rx = self.rx.take().expect("run called once");
        let _tracker = tokio::spawn(track_devices(self.adb.clone(), self.cfg.clone(), self.tx.clone()));
        let _discovery = (self.discovery && self.cfg.discovery.avahi).then(|| ac_discovery::spawn_avahi(disc_tx));
        let _pruner = {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let mut t = tokio::time::interval(Duration::from_secs(5));
                loop {
                    t.tick().await;
                    if tx.send(Msg::Prune).is_err() {
                        return;
                    }
                }
            })
        };

        self.emit_all();
        self.auto_connect_restored();

        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Ok(cmd) => self.on_ui(cmd),
                    Err(_) => return, // UI gone
                },
                Some(msg) = rx.recv() => self.on_msg(msg),
                Some(ev) = disc_rx.recv() => self.on_discovery(ev),
            }
            self.emit_changed();
        }
    }

    // ---- emission -------------------------------------------------------------

    fn send(&self, ev: UiEvent) {
        let _ = self.ui.try_send(ev);
    }

    fn toast(&self, code: Option<ErrorCode>, message: impl Into<String>) {
        self.send(UiEvent::Toast { code, message: message.into() });
    }

    fn view(&self, i: usize) -> SlotView {
        let s = &self.slots[i];
        let record = s.device_id().and_then(|id| self.devices.get(id));
        let (phase, detail) = match &s.phase {
            Phase::Empty => (PhaseView::Empty, String::new()),
            // Never identified (e.g. pairing failed): show as empty, with the error.
            Phase::Disconnected { .. } if s.device_id().is_none() => (PhaseView::Empty, String::new()),
            Phase::Disconnected { .. } => (PhaseView::Disconnected, String::new()),
            Phase::Pairing { qr: Some(_), pair_ip: None, .. } => (PhaseView::Pairing, "Waiting for the phone to scan the QR code…".into()),
            Phase::Pairing { paired: true, .. } => (PhaseView::Pairing, "Paired — looking for the phone…".into()),
            Phase::Pairing { .. } => (PhaseView::Pairing, "Pairing…".into()),
            Phase::ConnectingAdb { attempt, connected, .. } => {
                let what = if *connected { "Waiting for device" } else { "Connecting" };
                let d = if *attempt > 0 { format!("{what} (attempt {})", attempt + 1) } else { format!("{what}…") };
                (PhaseView::Connecting, d)
            }
            Phase::Online { .. } => (PhaseView::Online, String::new()),
            Phase::Reconnecting { attempt, .. } => (PhaseView::Reconnecting, format!("Reconnecting… (attempt {})", attempt + 1)),
        };
        let mut title = s.display_name();
        if title == "New device" && let Some(r) = record {
            title = r.display_name().to_owned();
        }
        SlotView {
            slot: s.id,
            phase,
            title,
            device_id: s.device_id().map(str::to_owned),
            model: s.device.as_ref().map(|d| d.model.clone()).or_else(|| record.map(|r| r.model.clone())).filter(|m| !m.is_empty()),
            manufacturer: s.device.as_ref().map(|d| d.manufacturer.clone()).filter(|m| !m.is_empty()),
            sdk: s.device.as_ref().map(|d| d.sdk).or_else(|| record.map(|r| r.sdk)).filter(|v| *v > 0),
            addr: s.addr.clone().or_else(|| s.adb_serial.clone()),
            transport: s.transport,
            detail,
            error: match &s.phase {
                Phase::Disconnected { error } => error.clone(),
                _ => None,
            },
        }
    }

    fn emit_all(&mut self) {
        self.views = vec![None; MAX_SLOTS];
        self.emit_changed();
    }

    fn emit_changed(&mut self) {
        for i in 0..MAX_SLOTS {
            let v = self.view(i);
            if self.views[i].as_ref() != Some(&v) {
                self.views[i] = Some(v.clone());
                self.send(UiEvent::Slot(v));
            }
        }
        let nearby = self.nearby_views();
        if nearby != self.last_nearby {
            self.last_nearby = nearby.clone();
            self.send(UiEvent::Nearby(nearby));
        }
    }

    fn nearby_views(&self) -> Vec<NearbyView> {
        self.nearby
            .values()
            .map(|n| {
                let serial = n.dev.serial().map(str::to_owned);
                let in_slot = serial.as_deref().and_then(|s| {
                    self.slots.iter().find(|sl| sl.is_active() && sl.device_id() == Some(s)).map(|sl| sl.id)
                });
                NearbyView {
                    instance: n.dev.instance.clone(),
                    kind: n.dev.kind,
                    addr: n.dev.addr,
                    known_name: serial.as_deref().and_then(|s| self.devices.get(s)).map(|r| r.display_name().to_owned()),
                    serial,
                    in_slot,
                }
            })
            .collect()
    }

    // ---- slot plumbing -----------------------------------------------------------

    fn idx(slot: SlotId) -> Option<usize> {
        (1..=MAX_SLOTS as SlotId).contains(&slot).then(|| slot as usize - 1)
    }

    fn feed(&mut self, slot: SlotId, ev: SlotEvent) {
        let Some(i) = Self::idx(slot) else { return };
        tracing::debug!(slot, phase = self.slots[i].phase.name(), event = ?ev, "slot event");
        let effects = self.slots[i].handle(ev, Instant::now(), &self.cfg.connection);
        for fx in effects {
            self.apply(i, fx);
        }
    }

    fn pick_slot(&self, requested: Option<SlotId>) -> Option<SlotId> {
        if let Some(s) = requested {
            return Self::idx(s).filter(|&i| !self.slots[i].is_active()).map(|_| s);
        }
        self.slots
            .iter()
            .find(|s| matches!(s.phase, Phase::Empty))
            .or_else(|| self.slots.iter().find(|s| s.is_free()))
            .map(|s| s.id)
    }

    fn spawn_msg<F>(&self, fut: F)
    where
        F: Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(fut.await);
        });
    }

    fn apply(&mut self, i: usize, fx: Effect) {
        let slot = self.slots[i].id;
        let adb = self.adb.clone();
        match fx {
            Effect::Pair { code, addr } => self.spawn_msg(async move {
                let r = match adb.pair(&code, &addr.to_string()).await {
                    Ok(PairOutcome::Paired { guid }) => Ok(guid),
                    Ok(PairOutcome::Failed(m)) => Err(m),
                    Err(e) => Err(e.to_string()),
                };
                Msg::Slot(slot, SlotEvent::PairDone(r))
            }),
            Effect::Connect { addr } => {
                if let Err(m) = self.check_lan(&addr) {
                    self.toast(Some(ErrorCode::NotLan), m.clone());
                    let _ = self.tx.send(Msg::ConnectResult(slot, Err(m)));
                    return;
                }
                self.spawn_msg(async move { Msg::ConnectResult(slot, connect(&adb, &addr).await) });
            }
            Effect::Reconnect { addr } => self.spawn_msg(async move {
                let _ = adb.disconnect_device(&addr).await;
                Msg::ConnectResult(slot, connect(&adb, &addr).await)
            }),
            Effect::Disconnect { serial } => {
                tokio::spawn(async move {
                    let _ = adb.disconnect_device(&serial).await;
                });
            }
            Effect::Identify { serial } => {
                let low_latency = self.cfg.connection.phone_low_latency_wifi;
                self.spawn_msg(async move { Msg::Slot(slot, identify(&adb, &serial, low_latency).await) });
            }
            Effect::StartMonitor { serial, shell_v2 } => {
                if let Some(h) = self.monitors[i].take() {
                    h.abort();
                }
                let (tx, c) = (self.tx.clone(), self.cfg.connection.clone());
                self.monitors[i] = Some(tokio::spawn(monitor(adb, serial, shell_v2, slot, c, tx)));
            }
            Effect::StopMonitor => {
                if let Some(h) = self.monitors[i].take() {
                    h.abort();
                }
            }
            Effect::Timer { kind, after, generation } => {
                let after = match kind {
                    TimerKind::Backoff => jitter(after, self.cfg.connection.reconnect_jitter_pct),
                    _ => after,
                };
                if let Some(h) = self.timers[i].take() {
                    h.abort();
                }
                let tx = self.tx.clone();
                self.timers[i] = Some(tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = tx.send(Msg::Slot(slot, SlotEvent::Timer { kind, generation }));
                }));
            }
            Effect::FindConnectService { serial, ip } => {
                let found = self.nearby.values().map(|n| &n.dev).find(|d| {
                    d.kind == ServiceKind::Connect
                        && (serial.is_some() && d.serial() == serial.as_deref() || serial.is_none() && Some(d.addr.ip()) == ip)
                });
                if let Some(d) = found {
                    let _ = self.tx.send(Msg::Slot(slot, SlotEvent::ConnectSeen { addr: d.addr }));
                }
            }
            Effect::LookupLegacy { mac, port } => {
                let path = self.arp_table.clone();
                self.spawn_msg(async move {
                    let table = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                    match ac_discovery::find_ip_by_mac(&table, &mac) {
                        Some(ip) => Msg::Slot(slot, SlotEvent::LegacyAddrFound { addr: format!("{ip}:{port}") }),
                        None => Msg::Prune, // nothing found; harmless no-op message
                    }
                });
            }
            Effect::Persist => self.persist(i),
            Effect::Forget { id } => {
                self.devices.remove(&id);
                self.save_devices();
            }
            Effect::Toast { code, message } => self.toast(Some(code), message),
        }
    }

    fn check_lan(&self, addr: &str) -> Result<(), String> {
        if !self.cfg.general.lan_only {
            return Ok(());
        }
        match addr.parse::<SocketAddr>() {
            Ok(sa) if is_lan(sa.ip()) => Ok(()),
            Ok(sa) => Err(format!("{} is not a local network address", sa.ip())),
            Err(_) => Err(format!("{addr} is not an IP:port address")),
        }
    }

    // ---- persistence ----------------------------------------------------------------

    fn persist(&mut self, i: usize) {
        let s = &self.slots[i];
        let Some(info) = &s.device else { return };
        let now = chrono::Utc::now();
        let mut rec = self.devices.get(&info.id).cloned().unwrap_or_else(|| DeviceRecord::new(&info.id, s.transport, now));
        rec.model = info.model.clone();
        rec.sdk = info.sdk;
        rec.slot = s.id;
        rec.transport = s.transport;
        rec.last_seen = now;
        if !s.alias.is_empty() {
            rec.alias = s.alias.clone();
        }
        if let Some(addr) = s.addr.as_ref().filter(|a| a.parse::<SocketAddr>().is_ok()) {
            rec.last_address = Some(addr.clone());
        }
        if let Some(serial) = &s.adb_serial
            && serial_from_instance(serial).is_some()
        {
            rec.mdns_instance = serial.split("._adb").next().map(str::to_owned);
        }
        if s.wifi_mac.is_some() {
            rec.wifi_mac = s.wifi_mac.clone();
        }
        // A slot number belongs to one device.
        for other in &mut self.devices.devices {
            if other.id != rec.id && other.slot == rec.slot {
                other.slot = 0;
            }
        }
        self.devices.upsert(rec);
        self.save_devices();
    }

    fn save_devices(&self) {
        if let Some(p) = &self.paths
            && let Err(e) = ac_config::save_devices(&p.devices, &self.devices)
        {
            tracing::error!("saving devices: {e}");
        }
    }

    // ---- inputs -----------------------------------------------------------------------

    fn auto_connect_restored(&mut self) {
        if !self.cfg.discovery.auto_connect_known {
            return;
        }
        let todo: Vec<(SlotId, String, TransportKind, String)> = self
            .slots
            .iter()
            .filter_map(|s| {
                let rec = self.devices.get(s.device_id()?)?;
                (rec.auto_connect && matches!(s.phase, Phase::Disconnected { .. }))
                    .then(|| Some((s.id, rec.last_address.clone()?, rec.transport, rec.id.clone())))?
            })
            .collect();
        for (slot, addr, transport, id) in todo {
            self.feed(slot, SlotEvent::Connect { addr, transport, expect_serial: Some(id), silent: true });
        }
    }

    fn on_ui(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::Connect { slot, addr } => {
                let Some((addr, transport)) = normalize_addr(&addr, self.cfg.discovery.legacy_port) else {
                    return self.toast(None, format!("“{addr}” is not a valid IP[:port]"));
                };
                if let Err(m) = self.check_lan(&addr) {
                    return self.toast(Some(ErrorCode::NotLan), m);
                }
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, "All 5 slots are in use") };
                self.suppressed.remove(&addr);
                self.feed(slot, SlotEvent::Connect { addr, transport, expect_serial: None, silent: false });
            }
            UiCommand::PairCode { slot, instance, code } => {
                let Some(code) = parse_pair_code(&code) else { return self.toast(None, "The pairing code is 6 digits") };
                let Some(n) = self.nearby.get(&(ServiceKind::Pairing, instance.clone())) else {
                    return self.toast(Some(ErrorCode::PairFailed), "That pairing request is no longer advertised; reopen it on the phone");
                };
                let addr = n.dev.addr;
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, "All 5 slots are in use") };
                self.feed(slot, SlotEvent::PairCode { addr, code, connect: None });
            }
            UiCommand::PairAddr { slot, pair_addr, code, connect } => {
                let Some(code) = parse_pair_code(&code) else { return self.toast(None, "The pairing code is 6 digits") };
                let Ok(addr) = pair_addr.trim().parse::<SocketAddr>() else {
                    return self.toast(None, format!("“{}” is not an IP:port address", pair_addr.trim()));
                };
                if let Err(m) = self.check_lan(&addr.to_string()) {
                    return self.toast(Some(ErrorCode::NotLan), m);
                }
                let connect = match connect.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                    None => None,
                    Some(c) => match connect_addr_for(addr, c) {
                        Some(c) if self.check_lan(&c).is_ok() => Some(c),
                        _ => return self.toast(None, format!("“{c}” is not a port or IP:port")),
                    },
                };
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, "All 5 slots are in use") };
                if let Some(c) = &connect {
                    self.suppressed.remove(c);
                }
                self.feed(slot, SlotEvent::PairCode { addr, code, connect });
            }
            UiCommand::PairQr { slot } => {
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, "All 5 slots are in use") };
                let secret = QrSecret::generate(&mut rand::rng());
                let payload = secret.payload();
                self.feed(slot, SlotEvent::PairQr { secret });
                self.send(UiEvent::QrCode { slot, payload, expires_in: PAIR_QR_TIMEOUT });
            }
            UiCommand::CancelPairing { slot } => {
                if let Some(i) = Self::idx(slot)
                    && matches!(self.slots[i].phase, Phase::Pairing { .. })
                {
                    self.feed(slot, SlotEvent::Disconnect);
                }
            }
            UiCommand::EnableWifiAdb { usb_serial, slot } => self.enable_wifi_adb(usb_serial, slot),
            UiCommand::Disconnect { slot } => {
                if let Some(i) = Self::idx(slot) {
                    let s = &self.slots[i];
                    self.suppressed.extend(s.device_id().map(str::to_owned));
                    self.suppressed.extend(s.adb_serial.clone());
                    self.feed(slot, SlotEvent::Disconnect);
                }
            }
            UiCommand::Reconnect { slot } => {
                let Some(i) = Self::idx(slot) else { return };
                let s = &self.slots[i];
                let Some(addr) = s.addr.clone() else {
                    return self.toast(None, "No known address — pair again or enter IP:port");
                };
                let (transport, expect) = (s.transport, s.device_id().map(str::to_owned));
                if let Some(id) = &expect {
                    self.suppressed.remove(id);
                }
                self.suppressed.remove(&addr);
                self.feed(slot, SlotEvent::Connect { addr, transport, expect_serial: expect, silent: false });
            }
            UiCommand::Forget { slot } => self.feed(slot, SlotEvent::Forget),
            UiCommand::Rename { slot, alias } => {
                let Some(i) = Self::idx(slot) else { return };
                self.slots[i].alias = alias.trim().to_owned();
                if let Some(id) = self.slots[i].device_id().map(str::to_owned)
                    && let Some(rec) = self.devices.get_mut(&id)
                {
                    rec.alias = alias.trim().to_owned();
                    self.save_devices();
                }
            }
        }
    }

    fn enable_wifi_adb(&mut self, usb_serial: String, slot: Option<SlotId>) {
        let (adb, port) = (self.adb.clone(), self.cfg.discovery.legacy_port);
        self.spawn_msg(async move {
            let ip = match adb.shell(&usb_serial, "ip -4 -o addr show wlan0", true).await {
                Ok(out) => parse_inet(&out.stdout),
                Err(e) => return Msg::Toast(None, format!("Could not read the phone's Wi-Fi address: {e}")),
            };
            let Some(ip) = ip else {
                return Msg::Toast(None, "The phone is not connected to Wi-Fi".into());
            };
            if let Err(e) = adb.tcpip(&usb_serial, port).await {
                return Msg::Toast(None, format!("Could not switch the phone to Wi-Fi ADB: {e}"));
            }
            // adbd restarts in TCP mode; give it a moment before connecting.
            tokio::time::sleep(Duration::from_secs(3)).await;
            Msg::Ui(UiCommand::Connect { slot, addr: format!("{ip}:{port}") })
        });
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Ui(cmd) => self.on_ui(cmd),
            Msg::Slot(slot, SlotEvent::Identified(info)) => self.on_identified(slot, info),
            Msg::Slot(slot, ev) => self.feed(slot, ev),
            Msg::ConnectResult(slot, r) => {
                let ok = r.is_ok();
                self.feed(slot, SlotEvent::ConnectDone(r));
                if ok {
                    // The tracker may already have reported the transport.
                    self.route_adb_states(false);
                }
            }
            Msg::AdbDevices(list) => {
                let old = std::mem::replace(&mut self.adb_snapshot, list);
                for i in 0..MAX_SLOTS {
                    let Some(serial) = self.slots[i].adb_serial.clone() else { continue };
                    if old.iter().any(|d| d.serial == serial) && !self.adb_snapshot.iter().any(|d| d.serial == serial) {
                        self.feed(self.slots[i].id, SlotEvent::AdbGone { serial });
                    }
                }
                self.route_adb_states(true);
                let usb: Vec<UsbDeviceView> = self
                    .adb_snapshot
                    .iter()
                    .filter(|d| d.is_usb())
                    .map(|d| UsbDeviceView {
                        serial: d.serial.clone(),
                        model: d.model.clone().unwrap_or_else(|| d.serial.clone()),
                        authorized: d.state == DeviceState::Device,
                    })
                    .collect();
                if usb != self.last_usb {
                    self.last_usb = usb.clone();
                    self.send(UiEvent::UsbDevices(usb));
                }
            }
            Msg::AdbStatus(version, error) => self.send(UiEvent::AdbStatus { version, error }),
            Msg::Stats(slot, stats) => self.send(UiEvent::Stats { slot, stats }),
            Msg::Toast(code, message) => self.toast(code, message),
            Msg::Prune => {
                let now = Instant::now();
                self.nearby.retain(|_, n| n.stale_since.is_none_or(|t| now.duration_since(t) < NEARBY_GRACE));
            }
        }
    }

    /// Delivers tracker state to owning slots and adopts unowned network
    /// transports into free slots.
    fn route_adb_states(&mut self, adopt: bool) {
        let entries = self.adb_snapshot.clone();
        for entry in entries.iter().filter(|e| e.is_network()) {
            let owner = self.slots.iter().position(|s| s.is_active() && s.wants_adb_serial(&entry.serial));
            if let Some(i) = owner {
                self.feed(self.slots[i].id, SlotEvent::AdbState { serial: entry.serial.clone(), state: entry.state.clone() });
                continue;
            }
            if !adopt || entry.state != DeviceState::Device || self.suppressed.contains(&entry.serial) {
                continue;
            }
            let mdns_serial = serial_from_instance(&entry.serial);
            if mdns_serial.is_some_and(|s| self.suppressed.contains(s)) {
                continue;
            }
            // Prefer the slot remembered for this device, else any free slot.
            let remembered = self.slots.iter().find(|s| {
                !s.is_active() && (mdns_serial.is_some() && s.device_id() == mdns_serial || s.addr.as_deref() == Some(&entry.serial))
            });
            let target = remembered.map(|s| s.id).or_else(|| self.pick_slot(None));
            if let Some(slot) = target {
                let transport = if entry.serial.ends_with(&format!(":{}", self.cfg.discovery.legacy_port)) {
                    TransportKind::Legacy
                } else {
                    TransportKind::Tls
                };
                self.feed(slot, SlotEvent::Adopt { adb_serial: entry.serial.clone(), transport });
            }
        }
    }

    fn on_identified(&mut self, slot: SlotId, info: DeviceInfo) {
        let Some(i) = Self::idx(slot) else { return };
        // Same phone already live in another slot (e.g. two transports).
        if let Some(other) = self.slots.iter().find(|s| {
            s.id != slot && matches!(s.phase, Phase::Online { .. }) && s.device_id() == Some(info.id.as_str())
        }) {
            let other_id = other.id;
            self.toast(None, format!("{} is already connected in slot {other_id}", info.model));
            self.feed(slot, SlotEvent::Disconnect);
            if self.slots[i].device_id().is_none() {
                self.slots[i] = Slot::new(slot);
            }
            return;
        }
        // The phone moves here from a remembered (inactive) slot.
        for j in 0..MAX_SLOTS {
            if j != i && !self.slots[j].is_active() && self.slots[j].device_id() == Some(info.id.as_str()) {
                if self.slots[i].alias.is_empty() {
                    self.slots[i].alias = std::mem::take(&mut self.slots[j].alias);
                }
                self.slots[j] = Slot::new(self.slots[j].id);
            }
        }
        if self.slots[i].alias.is_empty()
            && let Some(rec) = self.devices.get(&info.id)
        {
            self.slots[i].alias = rec.alias.clone();
        }
        self.feed(slot, SlotEvent::Identified(info));
    }

    fn on_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Added(dev) => {
                if self.cfg.general.lan_only && !is_lan(dev.addr.ip()) {
                    return;
                }
                let key = (dev.kind, dev.instance.clone());
                self.nearby.insert(key, Nearby { dev: dev.clone(), stale_since: None });
                self.route_discovered(&dev);
            }
            DiscoveryEvent::Removed { instance, kind } => {
                if let Some(n) = self.nearby.get_mut(&(kind, instance)) {
                    n.stale_since.get_or_insert_with(Instant::now);
                }
            }
            DiscoveryEvent::Unavailable(reason) => {
                self.send(UiEvent::DiscoveryStatus { available: false, message: reason });
                self.toast(Some(ErrorCode::AvahiMissing), "mDNS discovery unavailable — enable avahi-daemon.service");
            }
        }
    }

    fn route_discovered(&mut self, dev: &DiscoveredDevice) {
        match dev.kind {
            ServiceKind::Pairing => {
                let slot = self.slots.iter().find(|s| {
                    matches!(&s.phase, Phase::Pairing { qr: Some(q), pair_ip: None, .. } if q.name == dev.instance)
                });
                if let Some(s) = slot {
                    let id = s.id;
                    self.feed(id, SlotEvent::QrServiceSeen { addr: dev.addr });
                }
            }
            ServiceKind::Connect => {
                let Some(serial) = dev.serial() else { return };
                let auto = self.cfg.discovery.auto_connect_known && !self.suppressed.contains(serial);
                let slot = self.slots.iter().find(|s| {
                    let mine = s.device_id() == Some(serial) || s.expect_serial.as_deref() == Some(serial);
                    mine && match s.phase {
                        Phase::Reconnecting { .. } | Phase::Pairing { paired: true, .. } | Phase::ConnectingAdb { connected: false, .. } => true,
                        Phase::Disconnected { .. } => {
                            auto && self.devices.get(serial).is_none_or(|r| r.auto_connect)
                        }
                        _ => false,
                    }
                });
                if let Some(s) = slot {
                    let id = s.id;
                    self.feed(id, SlotEvent::ConnectSeen { addr: dev.addr });
                }
            }
            ServiceKind::Legacy => {}
        }
    }
}

// ---- free functions running on worker tasks -------------------------------------------

async fn connect(adb: &AdbClient, addr: &str) -> Result<(), String> {
    match adb.connect_device(addr).await {
        Ok(ConnectOutcome::Connected | ConnectOutcome::AlreadyConnected) => Ok(()),
        Ok(ConnectOutcome::Failed(m)) => Err(m),
        Err(e) => Err(e.to_string()),
    }
}

/// Digits only, spaces allowed (`482 913`).
fn parse_pair_code(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    (code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())).then_some(code)
}

/// The connect address typed next to a manual pairing: a bare port on the
/// pairing IP, or a full `ip:port`.
fn connect_addr_for(pair: SocketAddr, typed: &str) -> Option<String> {
    if let Ok(port) = typed.parse::<u16>() {
        return (port != 0).then(|| SocketAddr::new(pair.ip(), port).to_string());
    }
    typed.parse::<SocketAddr>().ok().map(|a| a.to_string())
}

async fn identify(adb: &AdbClient, serial: &str, low_latency_wifi: bool) -> SlotEvent {
    let features = match adb.features(serial).await {
        Ok(f) => f,
        Err(e) => return SlotEvent::IdentifyFailed(e.to_string()),
    };
    let shell_v2 = features.iter().any(|f| f == "shell_v2");
    let out = match adb.shell(serial, IDENTIFY_CMD, shell_v2).await {
        Ok(o) => o,
        Err(e) => return SlotEvent::IdentifyFailed(e.to_string()),
    };
    let Some(id) = parse_identity(&out.stdout) else {
        return SlotEvent::IdentifyFailed("device reported no serial number or android_id".into());
    };
    if low_latency_wifi && id.sdk >= 29 {
        // Best effort: some OEMs restrict this command.
        let _ = adb.shell(serial, "cmd wifi force-low-latency-mode enabled", shell_v2).await;
    }
    SlotEvent::Identified(DeviceInfo {
        id: id.id,
        model: id.model,
        manufacturer: id.manufacturer,
        sdk: id.sdk,
        features,
        wifi_mac: id.wifi_mac,
    })
}

async fn monitor(
    adb: AdbClient,
    serial: String,
    shell_v2: bool,
    slot: SlotId,
    cfg: ac_config::Connection,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(cfg.heartbeat_interval_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = Duration::from_millis(cfg.heartbeat_timeout_ms);
    let mut stats = DeviceStats::default();
    for n in 0u32.. {
        tick.tick().await;
        let start = Instant::now();
        let result = match tokio::time::timeout(deadline, adb.shell(&serial, "echo ac", shell_v2)).await {
            Ok(Ok(out)) if out.stdout.trim() == "ac" => Ok(start.elapsed().as_millis() as u32),
            Ok(Ok(out)) => Err(format!("unexpected heartbeat reply {:?}", out.stdout)),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("heartbeat timed out".into()),
        };
        let ok = result.is_ok();
        stats.rtt_ms = result.as_ref().ok().copied();
        if tx.send(Msg::Slot(slot, SlotEvent::Heartbeat(result))).is_err() {
            return;
        }
        if ok && n % STATS_EVERY == 0
            && let Ok(Ok(out)) = tokio::time::timeout(deadline * 2, adb.shell(&serial, STATS_CMD, shell_v2)).await
        {
            parse_stats(&out.stdout, &mut stats);
        }
        let _ = tx.send(Msg::Stats(slot, stats.clone()));
    }
}

/// Keeps a `host:track-devices-l` stream open, restarting the ADB server once
/// if it is not running.
async fn track_devices(adb: AdbClient, cfg: Config, tx: mpsc::UnboundedSender<Msg>) {
    let mut tried_start = false;
    loop {
        match adb.track_devices().await {
            Ok(mut tracker) => {
                let version = adb.version().await.ok();
                let _ = tx.send(Msg::AdbStatus(version, None));
                while let Ok(list) = tracker.next().await {
                    if tx.send(Msg::AdbDevices(list)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Msg::AdbStatus(None, Some("ADB server connection lost".into())));
            }
            Err(e) => {
                if e.is_server_unreachable() && cfg.general.start_adb_server && !tried_start {
                    tried_start = true;
                    tracing::info!("starting adb server via {}", cfg.general.adb_binary.display());
                    let status = tokio::process::Command::new(&cfg.general.adb_binary).arg("start-server").status().await;
                    if status.is_ok_and(|s| s.success()) {
                        continue;
                    }
                }
                let _ = tx.send(Msg::AdbStatus(None, Some(format!("{}: {e}", ErrorCode::AdbUnavailable))));
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn jitter(d: Duration, pct: u8) -> Duration {
    if pct == 0 {
        return d;
    }
    let f = rand::rng().random_range(-(pct as f64)..=(pct as f64)) / 100.0;
    d.mul_f64((1.0 + f).max(0.0))
}

/// `"192.168.1.5"` → (`"192.168.1.5:5555"`, Legacy); `"192.168.1.5:37115"` →
/// (same, Tls). IPv6 needs brackets: `[fe80::1]:5555`.
pub fn normalize_addr(input: &str, legacy_port: u16) -> Option<(String, TransportKind)> {
    let s = input.trim();
    let sa = match s.parse::<SocketAddr>() {
        Ok(sa) => sa,
        Err(_) => SocketAddr::new(s.trim_matches(['[', ']']).parse::<IpAddr>().ok()?, legacy_port),
    };
    let transport = if sa.port() == legacy_port { TransportKind::Legacy } else { TransportKind::Tls };
    Some((sa.to_string(), transport))
}

/// First IPv4 address in `ip -4 -o addr show` output.
pub fn parse_inet(out: &str) -> Option<IpAddr> {
    let mut words = out.split_whitespace();
    words.find(|w| *w == "inet")?;
    words.next()?.split('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_manual_addresses() {
        assert_eq!(normalize_addr("192.168.1.5", 5555), Some(("192.168.1.5:5555".into(), TransportKind::Legacy)));
        assert_eq!(normalize_addr(" 192.168.1.5:37115 ", 5555), Some(("192.168.1.5:37115".into(), TransportKind::Tls)));
        assert_eq!(normalize_addr("[fe80::1]:5555", 5555), Some(("[fe80::1]:5555".into(), TransportKind::Legacy)));
        assert_eq!(normalize_addr("phone.local", 5555), None);
    }

    #[test]
    fn manual_pairing_inputs() {
        assert_eq!(parse_pair_code(" 482 913 ").as_deref(), Some("482913"));
        assert_eq!(parse_pair_code("48291"), None);
        assert_eq!(parse_pair_code("48291a"), None);
        let pair: SocketAddr = "192.168.0.84:41000".parse().unwrap();
        assert_eq!(connect_addr_for(pair, "37115").as_deref(), Some("192.168.0.84:37115"));
        assert_eq!(connect_addr_for(pair, "192.168.0.85:37115").as_deref(), Some("192.168.0.85:37115"));
        assert_eq!(connect_addr_for(pair, "0"), None);
        assert_eq!(connect_addr_for(pair, "phone"), None);
    }

    #[test]
    fn parses_ip_addr_output() {
        let out = "30: wlan0    inet 192.168.1.57/24 brd 192.168.1.255 scope global wlan0\\       valid_lft forever";
        assert_eq!(parse_inet(out), Some("192.168.1.57".parse().unwrap()));
        assert_eq!(parse_inet(""), None);
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let d = jitter(Duration::from_millis(1000), 20);
            assert!((800..=1200).contains(&d.as_millis()), "{d:?}");
        }
        assert_eq!(jitter(Duration::from_millis(1000), 0), Duration::from_millis(1000));
    }
}
