//! The device manager actor: owns the slots, performs their effects, and
//! routes ADB tracker and mDNS events to them.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ac_adb::{AdbClient, ConnectOutcome, DeviceEntry, DeviceState, PairOutcome};
use ac_config::{Config, DeviceRecord, Devices, Paths};
use ac_core::{DeviceStats, ErrorCode, MAX_SLOTS, SlotId, TransportKind};
use ac_discovery::{DiscoveredDevice, DiscoveryEvent, ServiceKind, is_lan, serial_from_instance};
use rand::Rng;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::adaptive::{Adaptive, Rate};
use crate::api::*;
use crate::logcat::LogEvent;
use crate::perf::{LAYERS_CMD, PerfState, parse_layers, parse_sample, sample_cmd};
use crate::testing::{Run, app_cmd, app_result, capture_name, crash_entry, output_root, parse_packages};
use crate::probe::{IDENTIFY_CMD, STATS_CMD, parse_identity, parse_stats};
use crate::qr::QrSecret;
use crate::slot::{DeviceInfo, Effect, PAIR_QR_TIMEOUT, Phase, Slot, SlotEvent, StreamPhase, StreamProfile, TimerKind};
use crate::transfer::{DirectTarget, Link, Transfers};
use crate::stream::{
    DirectEnv, SessionHandle, SessionMsg, SlotIo, StreamEnv, max_size_for, pick_codec, resolve_profile, server_params, spawn_direct_session,
    spawn_session,
};
use ac_media::MediaContext;
use serde_json::Value;
use tokio::sync::oneshot;
use ac_scrcpy::VideoCodec;

/// Phone-side copy of the user's screen timeout while "keep screen on" is
/// active. Living on the phone, it survives app crashes and restarts.
const STAY_AWAKE_FILE: &str = "/data/local/tmp/android-controller-screen-timeout";
/// `Integer.MAX_VALUE` ms: the screen never times out.
const STAY_AWAKE_TIMEOUT: u32 = i32::MAX as u32;

/// Finishes setting up the Wireless Debugging Keeper helper (android-helper/)
/// on a phone that has it installed: the one permission it needs can only be
/// granted over adb. Prints `@granted` when it did the setup now.
const HELPER_SETUP_CMD: &str = "p=org.androidcontroller.keeper; \
pm path $p >/dev/null 2>&1 || { echo @none; exit 0; }; \
echo @$(dumpsys package $p | grep -m 1 -o 'versionCode=[0-9]*'); \
dumpsys package $p | grep -q 'WRITE_SECURE_SETTINGS: granted=true' && { echo @ready; exit 0; }; \
pm grant $p android.permission.WRITE_SECURE_SETTINGS && echo @granted; \
pm grant $p android.permission.POST_NOTIFICATIONS >/dev/null 2>&1; \
dumpsys deviceidle whitelist +$p >/dev/null 2>&1; \
cmd appops set $p RUN_ANY_IN_BACKGROUND allow >/dev/null 2>&1; \
am broadcast -a $p.START -n $p/.BootReceiver >/dev/null 2>&1; true";

/// How often a given-up `adb tcpip` phone is checked for being back (it
/// does not announce itself over mDNS, so nothing else would notice).
const LEGACY_PROBE_EVERY: Duration = Duration::from_secs(30);

/// Stale mDNS entries are kept this long so brief flaps don't flicker the UI.
const NEARBY_GRACE: Duration = Duration::from_secs(30);
/// The Wireless Debugging Keeper build this app ships (android-helper/dist):
/// phones with an older one are offered the update.
const HELPER_APK: &[u8] = include_bytes!("../../../android-helper/dist/wireless-debugging-keeper.apk");

/// A slot's logcat stream that ended is restarted no sooner than this.
const LOGCAT_RETRY: Duration = Duration::from_secs(5);
/// The perf sampler lists SurfaceFlinger layers every N samples.
const LAYERS_EVERY: u32 = 5;

/// Run the stats command every N heartbeats.
const STATS_EVERY: u32 = 5;
/// A grid `max_size: auto` change restarts the stream only past this
/// fraction of the running value (spec §4.3 hysteresis).
const TILE_RESTART_RATIO: f64 = 0.25;

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub config: Config,
    /// `None` keeps devices in memory only (tests).
    pub paths: Option<Paths>,
    pub discovery: bool,
    pub arp_table: PathBuf,
    /// Mirror online devices with scrcpy-server (off in tests).
    pub streaming: bool,
    /// Decoding setup; created (with a VA-API probe) when `None`.
    pub media: Option<Arc<MediaContext>>,
    /// Serve the automation socket here (when `config.automation.enabled`);
    /// the app uses `control::socket_path()`, tests none.
    pub control_socket: Option<PathBuf>,
}

impl ManagerOptions {
    pub fn new(config: Config, paths: Option<Paths>) -> Self {
        Self { config, paths, discovery: true, arp_table: "/proc/net/arp".into(), streaming: true, media: None, control_socket: None }
    }
}

pub struct ManagerHandle {
    pub commands: async_channel::Sender<UiCommand>,
    pub events: async_channel::Receiver<UiEvent>,
    /// Lets tests inject discovery events.
    pub discovery_tx: mpsc::UnboundedSender<DiscoveryEvent>,
    /// Per-slot frame mailbox and control channel, indexed by `slot - 1`.
    pub io: Vec<Arc<SlotIo>>,
    pub media: Option<Arc<MediaContext>>,
    task: JoinHandle<()>,
}

impl ManagerHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

mod automation;
mod games;

enum Msg {
    Ui(UiCommand),
    Slot(SlotId, SlotEvent),
    ConnectResult(SlotId, Result<(), String>),
    AdbDevices(Vec<DeviceEntry>),
    AdbStatus(Option<u32>, Option<String>),
    Stats(SlotId, DeviceStats),
    Toast(Option<ErrorCode>, String),
    Prune,
    Session(SessionMsg),
    /// "Keep screen on" state read from or set on the phone `device_id`.
    StayAwake { slot: SlotId, device_id: String, on: Option<bool> },
    /// A disconnected slot's `adb tcpip` (or helper) port accepts connections again.
    LegacyAlive { slot: SlotId, addr: String },
    /// A no-ADB pairing finished.
    DirectPaired { slot: SlotId, addr: SocketAddr, result: Result<ac_direct::Paired, String> },
    /// A no-ADB link attempt `conn` finished.
    DirectLinked { slot: SlotId, conn: u64, result: Result<DirectLink, String> },
    /// The no-ADB link `conn` broke.
    DirectGone { slot: SlotId, conn: u64 },
    /// The helper's touch control (accessibility service) came on or went off.
    DirectInput { slot: SlotId, conn: u64, on: bool },
    /// Once a second: feed the adaptive bitrate controllers.
    AdaptiveTick,
    /// The phone's hardware video encoders (codec names), or why not known.
    Codecs { slot: SlotId, device_id: String, codecs: Result<Vec<String>, String> },
    Log(LogEvent),
    Perf { slot: SlotId, sample: PerfSample },
    /// The phone's clipboard, from the helper (no ADB).
    DirectClip { slot: SlotId, text: String },
    /// Pass this on to the UI.
    Emit(UiEvent),
    /// The helper's version code on the phone `device_id` (over ADB).
    HelperVersion { slot: SlotId, device_id: String, version: u32 },
    /// The helper's screen size (no ADB) changed.
    DirectScreen { slot: SlotId, conn: u64, size: (u32, u32) },
    /// The game add-on on an ADB phone (connection `serial`).
    Game { slot: SlotId, serial: String, ev: crate::game::GameEvent },
    /// The game add-on through the helper's relay (no ADB).
    GameNews { slot: SlotId, conn: u64, news: Value },
}

/// A slot's connection to the Android helper (no ADB): heartbeat, telemetry
/// and "keep screen on" run on it; mirroring and files open their own.
struct DirectLink {
    conn: u64,
    addr: SocketAddr,
    hello: ac_direct::Hello,
    cmds: mpsc::UnboundedSender<LinkCmd>,
    task: JoinHandle<()>,
}

enum LinkCmd {
    StayAwake { set: Option<bool>, device_id: String },
    /// `reply`: an automation request (else the UI hears the result).
    App { action: AppAction, reply: Option<oneshot::Sender<Result<String, String>>> },
    Packages { reply: Option<oneshot::Sender<Result<Vec<String>, String>>> },
    /// Helper 0.7+: an automation op.
    Dev { req: Value, reply: oneshot::Sender<Result<Value, String>> },
}

/// A slot's running logcat stream and what it was started for.
struct LogTask {
    task: JoinHandle<()>,
    serial: String,
    file: Option<PathBuf>,
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
    let n = opts.config.general.max_slots.clamp(1, MAX_SLOTS as u8) as usize;
    let io: Vec<Arc<SlotIo>> = (0..n).map(|_| Arc::new(SlotIo::default())).collect();
    let (stream, media) = if opts.streaming { stream_env(&opts) } else { (Err(String::new()), None) };
    let handle_io = io.clone();
    let task = tokio::spawn(async move {
        let mut m = Manager::new(opts, ev_tx, stream, io);
        m.run(cmd_rx, disc_tx, disc_rx).await;
    });
    ManagerHandle { commands: cmd_tx, events: ev_rx, discovery_tx, io: handle_io, media, task }
}

/// Checks scrcpy (spec §1.4 version coupling) and sets up decoding.
fn stream_env(opts: &ManagerOptions) -> (Result<StreamEnv, String>, Option<Arc<MediaContext>>) {
    let g = &opts.config.general;
    let version = match ac_scrcpy::detect_version(&g.scrcpy_binary) {
        Ok(v) if ac_scrcpy::is_compatible(&v) => v,
        Ok(v) => return (Err(format!("Installed scrcpy {v} is not supported (need 4.x)")), None),
        Err(e) => return (Err(format!("Mirroring unavailable: {e}")), None),
    };
    if !g.scrcpy_server.is_file() {
        return (Err(format!("Mirroring unavailable: {} not found", g.scrcpy_server.display())), None);
    }
    let media = opts
        .media
        .clone()
        .unwrap_or_else(|| Arc::new(MediaContext::new(&opts.config.video.decoder, &opts.config.video.vaapi_device)));
    tracing::info!("scrcpy-server {version}, decoder {}", media.backend());
    let adb = AdbClient::new(g.adb_server).with_timeout(Duration::from_secs(10));
    let env = StreamEnv { adb, server_jar: g.scrcpy_server.clone(), server_version: version, media: media.clone() };
    (Ok(env), Some(media))
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
    /// `Err(reason)` when mirroring is unavailable.
    stream_env: Result<StreamEnv, String>,
    io: Vec<Arc<SlotIo>>,
    sessions: Vec<Option<SessionHandle>>,
    tile_px: Vec<Option<u32>>,
    /// `max_size` of a running grid session whose profile says `"auto"`.
    auto_max_size: Vec<Option<u32>>,
    session_tx: mpsc::UnboundedSender<SessionMsg>,
    session_rx: Option<mpsc::UnboundedReceiver<SessionMsg>>,
    /// Asks the Avahi browser to query the network again.
    rescan: Option<mpsc::UnboundedSender<ServiceKind>>,
    tx: mpsc::UnboundedSender<Msg>,
    rx: Option<mpsc::UnboundedReceiver<Msg>>,
    ui: async_channel::Sender<UiEvent>,
    transfers: Transfers,
    /// Per slot: "keep screen on" (`None` = not known yet) and whether a
    /// query or change is in flight.
    stay_awake: Vec<Option<bool>>,
    stay_awake_busy: Vec<bool>,
    last_legacy_probe: Instant,
    /// Per slot: the helper check ran for the phone currently online.
    helper_checked: Vec<bool>,
    /// Keys of phones paired for the connection without ADB.
    keys: ac_direct::KeyStore,
    direct: Vec<Option<DirectLink>>,
    /// Per slot: the link attempt whose answer is still wanted (0 = none).
    direct_pending: Vec<u64>,
    direct_conn: u64,
    /// Per slot: adaptive bitrate (spec §3.6), the profile rate of the
    /// running stream, and the video byte count at the last tick.
    adaptive: Vec<Adaptive>,
    stream_rate: Vec<Option<(Rate, StreamProfile)>>,
    video_bytes: Vec<u64>,
    /// Per slot: the encoder list was asked for the phone online now, and
    /// the codec the current stream asked for.
    codecs_checked: Vec<bool>,
    stream_codec: Vec<VideoCodec>,
    // ---- app testing (Milestone 5) ----
    logcat: Vec<Option<LogTask>>,
    logcat_started: Vec<Option<Instant>>,
    /// Per slot: the perf sampler and the serial it samples.
    perf: Vec<Option<(JoinHandle<()>, String)>>,
    perf_on: bool,
    test_app: tokio::sync::watch::Sender<Option<String>>,
    run: Option<Run>,
    /// The shipped helper written to disk, and its version code.
    helper_apk: Option<(PathBuf, u32)>,
    /// Per slot: the phone's older helper version, while an update is offered.
    helper_old: Vec<Option<u32>>,
    /// The control socket's state (Claude, scripts).
    auto: automation::Automation,
    control_socket: Option<PathBuf>,
    games: games::Games,
}

impl Manager {
    fn new(
        opts: ManagerOptions,
        ui: async_channel::Sender<UiEvent>,
        stream_env: Result<StreamEnv, String>,
        io: Vec<Arc<SlotIo>>,
    ) -> Self {
        let devices = match &opts.paths {
            Some(p) => ac_config::load_devices(&p.devices).unwrap_or_else(|e| {
                tracing::warn!("{e}; starting with no saved devices");
                Devices::default()
            }),
            None => Devices::default(),
        };
        let n = opts.config.general.max_slots.clamp(1, MAX_SLOTS as u8);
        let mut slots: Vec<Slot> = (1..=n).map(Slot::new).collect();
        // A device saved in a slot beyond `max_slots` waits unassigned until it
        // is seen again, then takes the next free slot.
        for rec in devices.devices.iter().filter(|d| d.slot != 0 && d.slot <= n) {
            let mut s = Slot::restored(rec.slot, &rec.id, rec.transport, rec.last_address.clone(), rec.wifi_mac.clone());
            s.alias = rec.alias.clone();
            slots[rec.slot as usize - 1] = s;
        }
        for s in &mut slots {
            s.stream_enabled = stream_env.is_ok();
        }
        let keys = match &opts.paths {
            Some(p) => ac_direct::KeyStore::load(&p.devices.with_file_name("direct-keys.toml")),
            None => ac_direct::KeyStore::in_memory(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let (session_tx, session_rx) = mpsc::unbounded_channel();
        let adb = AdbClient::new(opts.config.general.adb_server)
            .with_timeout(Duration::from_millis(opts.config.connection.connect_timeout_ms.max(1000) * 2));
        let journal = opts.paths.as_ref().map(|p| p.state_dir.join("transfers.json"));
        Self {
            adb,
            paths: opts.paths,
            arp_table: opts.arp_table,
            discovery: opts.discovery,
            devices,
            slots,
            timers: (0..n).map(|_| None).collect(),
            monitors: (0..n).map(|_| None).collect(),
            views: vec![None; n as usize],
            nearby: BTreeMap::new(),
            last_nearby: Vec::new(),
            adb_snapshot: Vec::new(),
            last_usb: Vec::new(),
            suppressed: HashSet::new(),
            stream_env,
            sessions: (0..n).map(|_| None).collect(),
            tile_px: vec![None; n as usize],
            auto_max_size: vec![None; n as usize],
            session_tx,
            session_rx: Some(session_rx),
            rescan: None,
            tx,
            rx: Some(rx),
            transfers: Transfers::new(
                opts.config.transfer.clone(),
                opts.config.general.adb_server,
                io.clone(),
                ui.clone(),
                journal,
            ),
            io,
            ui,
            cfg: opts.config,
            stay_awake: vec![None; n as usize],
            stay_awake_busy: vec![false; n as usize],
            last_legacy_probe: Instant::now(),
            helper_checked: vec![false; n as usize],
            keys,
            direct: (0..n).map(|_| None).collect(),
            direct_pending: vec![0; n as usize],
            direct_conn: 0,
            adaptive: vec![Adaptive::default(); n as usize],
            stream_rate: vec![None; n as usize],
            video_bytes: vec![0; n as usize],
            codecs_checked: vec![false; n as usize],
            stream_codec: vec![VideoCodec::H264; n as usize],
            logcat: (0..n).map(|_| None).collect(),
            logcat_started: vec![None; n as usize],
            perf: (0..n).map(|_| None).collect(),
            perf_on: false,
            test_app: tokio::sync::watch::channel(None).0,
            run: None,
            helper_apk: None,
            helper_old: vec![None; n as usize],
            auto: automation::Automation::new(n as usize),
            control_socket: opts.control_socket,
            games: games::Games::new(n as usize),
        }
    }

    async fn run(
        &mut self,
        commands: async_channel::Receiver<UiCommand>,
        disc_tx: mpsc::UnboundedSender<DiscoveryEvent>,
        mut disc_rx: mpsc::UnboundedReceiver<DiscoveryEvent>,
    ) {
        let mut rx = self.rx.take().expect("run called once");
        let mut session_rx = self.session_rx.take().expect("run called once");
        if let Err(reason) = &self.stream_env
            && !reason.is_empty()
        {
            tracing::warn!("{reason}");
            self.toast(Some(ErrorCode::ServerIncompatible), reason.clone());
        }
        // Transfers left unfinished when the app last closed (spec §7.5).
        self.transfers.restore(|id| self.slots.iter().find(|s| s.device_id() == Some(id)).map(|s| s.id));
        let _tracker = tokio::spawn(track_devices(self.adb.clone(), self.cfg.clone(), self.tx.clone()));
        let _discovery = (self.discovery && self.cfg.discovery.avahi).then(|| {
            let (rescan, rescan_rx) = mpsc::unbounded_channel();
            self.rescan = Some(rescan);
            ac_discovery::spawn_avahi(disc_tx, rescan_rx)
        });
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
        let _adaptive = {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let mut t = tokio::time::interval(Duration::from_secs(1));
                loop {
                    t.tick().await;
                    if tx.send(Msg::AdaptiveTick).is_err() {
                        return;
                    }
                }
            })
        };

        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
        let _control = self
            .control_socket
            .clone()
            .filter(|_| self.cfg.automation.enabled)
            .map(|path| tokio::spawn(crate::control::serve(path, ctl_tx, self.auto.clients.clone())));

        self.emit_all();
        self.auto_connect_restored();

        loop {
            tokio::select! {
                Some(req) = ctl_rx.recv() => self.on_control(req),
                cmd = commands.recv() => match cmd {
                    Ok(cmd) => self.on_ui(cmd),
                    Err(_) => return, // UI gone
                },
                Some(msg) = rx.recv() => self.on_msg(msg),
                Some(msg) = session_rx.recv() => self.on_msg(Msg::Session(msg)),
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
            stay_awake: self.stay_awake[i],
            helper_update: match (self.helper_old[i], &self.helper_apk, &s.phase) {
                (Some(old), Some((_, new)), Phase::Online { .. }) => Some(format!(
                    "Wireless Debugging Keeper {} is on this phone; {} is available{}",
                    helper_version_name(old),
                    helper_version_name(*new),
                    if s.transport == TransportKind::Direct {
                        " (multi-touch, the phone's clipboard, app actions and graphs). The phone asks to confirm the install"
                    } else {
                        ""
                    }
                )),
                _ => None,
            },
            warning: match (&self.direct[i], &s.phase) {
                (Some(l), Phase::Online { .. }) if !l.hello.input => Some(
                    "Touch control is off on the phone, so it only shows the screen. On the phone open Wireless Debugging Keeper, \
tap “Turn on touch control” and switch on “Android Controller touch control”."
                        .into(),
                ),
                _ => None,
            },
            stream: match (&self.stream_env, &s.phase, &s.stream) {
                (Err(reason), Phase::Online { .. }, _) if !reason.is_empty() => StreamView::Unavailable(reason.clone()),
                (_, Phase::Online { .. }, StreamPhase::Starting { .. }) => StreamView::Starting,
                (_, Phase::Online { .. }, StreamPhase::Live { width, height }) => StreamView::Live { width: *width, height: *height },
                (_, Phase::Online { .. }, StreamPhase::Failed { error }) => StreamView::Failed(error.clone()),
                _ => StreamView::Off,
            },
        }
    }

    fn emit_all(&mut self) {
        self.views = vec![None; self.slots.len()];
        self.emit_changed();
    }

    fn emit_changed(&mut self) {
        for i in 0..self.slots.len() {
            if !matches!(self.slots[i].phase, Phase::Online { .. }) {
                self.stay_awake[i] = None;
                self.helper_checked[i] = false;
                self.adaptive[i].reset();
                self.codecs_checked[i] = false;
                self.helper_old[i] = None;
                continue;
            }
            if !self.codecs_checked[i] {
                self.codecs_checked[i] = true;
                self.probe_codecs(i);
            }
            if self.stay_awake[i].is_none() && !self.stay_awake_busy[i] {
                self.run_stay_awake(i, None);
            }
            if !self.helper_checked[i] {
                self.helper_checked[i] = true;
                self.setup_helper(i);
                // Without ADB the helper said its version when it connected
                // (0.5 and older did not say: 5).
                if let Some(l) = &self.direct[i] {
                    let v = if l.hello.version == 0 { 5 } else { l.hello.version };
                    self.check_helper_version(i, v);
                }
            }
        }
        for (i, s) in self.slots.iter().enumerate() {
            let link = match (&s.phase, &s.adb_serial, &s.device) {
                (Phase::Online { .. }, Some(serial), Some(d)) => Some(Link {
                    serial: serial.clone(),
                    device_id: d.id.clone(),
                    shell_v2: d.shell_v2(),
                    sdk: d.sdk,
                    direct: match (&self.direct[i], s.transport) {
                        (Some(l), TransportKind::Direct) => Some(DirectTarget { addr: l.addr, creds: self.keys.credentials() }),
                        _ => None,
                    },
                }),
                _ => None,
            };
            // A no-ADB slot without its link has nothing to send files over.
            let link = link.filter(|l| s.transport != TransportKind::Direct || l.direct.is_some());
            self.transfers.set_link(i as SlotId + 1, link);
        }
        for i in 0..self.slots.len() {
            let v = self.view(i);
            if self.views[i].as_ref() != Some(&v) {
                self.views[i] = Some(v.clone());
                self.send(UiEvent::Slot(v));
            }
        }
        self.reconcile_testing();
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
                let paired = n.dev.kind == ServiceKind::Direct && serial.as_deref().is_some_and(|s| self.keys.is_paired(s));
                NearbyView {
                    instance: n.dev.instance.clone(),
                    kind: n.dev.kind,
                    addr: n.dev.addr,
                    known_name: serial
                        .as_deref()
                        .and_then(|s| self.devices.get(s))
                        .map(|r| r.display_name().to_owned())
                        .or_else(|| n.dev.txt.get("model").filter(|_| n.dev.kind == ServiceKind::Direct).cloned()),
                    serial,
                    in_slot,
                    pairing_open: n.dev.pairing_open(),
                    paired,
                }
            })
            .collect()
    }

    // ---- slot plumbing -----------------------------------------------------------

    fn idx(&self, slot: SlotId) -> Option<usize> {
        (1..=self.slots.len() as SlotId).contains(&slot).then(|| slot as usize - 1)
    }

    fn feed(&mut self, slot: SlotId, ev: SlotEvent) {
        let Some(i) = self.idx(slot) else { return };
        tracing::debug!(slot, phase = self.slots[i].phase.name(), event = ?ev, "slot event");
        let effects = self.slots[i].handle(ev, Instant::now(), &self.cfg.connection);
        for fx in effects {
            self.apply(i, fx);
        }
    }

    fn slots_full(&self) -> String {
        format!("All {} slots are in use", self.slots.len())
    }

    fn pick_slot(&self, requested: Option<SlotId>) -> Option<SlotId> {
        if let Some(s) = requested {
            return self.idx(s).filter(|&i| !self.slots[i].is_active()).map(|_| s);
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
        // `Forget` resets the slot before its effects run: a helper link may
        // still be open on a slot that no longer says Direct.
        let direct = self.slots[i].transport == TransportKind::Direct
            || matches!(fx, Effect::Disconnect { .. }) && (self.direct[i].is_some() || self.direct_pending[i] != 0);
        if direct && self.apply_direct(i, &fx) {
            return;
        }
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
                let arp = self.arp_table.clone();
                self.spawn_msg(async move { Msg::Slot(slot, identify(&adb, &serial, low_latency, &arp).await) });
            }
            Effect::StartMonitor { serial, shell_v2 } => {
                if let Some(h) = self.monitors[i].take() {
                    h.abort();
                }
                let (tx, c) = (self.tx.clone(), self.cfg.connection.clone());
                self.monitors[i] = Some(tokio::spawn(monitor(adb, serial, shell_v2, slot, c, tx)));
            }
            Effect::StartStream { id, serial, profile, shell_v2 } => {
                if let Some(old) = self.sessions[i].take() {
                    old.stop();
                }
                self.auto_max_size[i] = None;
                match &self.stream_env {
                    Ok(env) => {
                        let record = self.slots[i].device_id().and_then(|id| self.devices.get(id));
                        let (p, err) = resolve_profile(&self.cfg, profile, record);
                        if let Some(err) = err {
                            tracing::warn!(slot, "{err}");
                            self.toast(None, err);
                        }
                        let mut params = server_params(&self.cfg, &p, self.tile_px[i], None);
                        if profile == StreamProfile::Grid && matches!(p.max_size, ac_config::MaxSize::Auto(_)) {
                            self.auto_max_size[i] = Some(params.max_size);
                        }
                        adapt(&self.cfg, &self.adaptive[i], &mut self.stream_rate[i], &self.io[i], profile, &p, &mut params);
                        let codecs = record.map_or(&[][..], |r| r.supported_codecs.as_slice());
                        params.codec = pick_codec(&p.codec, codecs, &env.media.probe);
                        self.stream_codec[i] = params.codec;
                        // Audio capture needs Android 11 (spec §4.1); older phones
                        // are not asked, so they don't report a failure each time.
                        params.audio &= self.slots[i].device.as_ref().is_some_and(|d| d.sdk >= 30);
                        let audio = ac_audio::Options {
                            node_name: format!("android-controller.slot{slot}"),
                            description: self.views[i].as_ref().map_or_else(|| self.slots[i].display_name(), |v| v.title.clone()),
                            volume: self.cfg.audio.volume,
                            latency_frames: self.cfg.audio.pipewire_latency_frames,
                            ..ac_audio::Options::default()
                        };
                        let io = self.io[i].clone();
                        let h = spawn_session(env.clone(), serial, shell_v2, params, audio, slot, id, io, self.session_tx.clone());
                        self.sessions[i] = Some(h);
                    }
                    Err(reason) => {
                        let _ = self.tx.send(Msg::Slot(slot, SlotEvent::StreamEnded { id, error: Some(reason.clone()) }));
                    }
                }
            }
            Effect::StopStream => {
                self.auto_max_size[i] = None;
                if let Some(old) = self.sessions[i].take() {
                    old.stop();
                }
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
                // Prefer the serial; fall back to the phone's IP in case adb's
                // pairing guid and the mDNS instance name disagree.
                let connect = || self.nearby.values().map(|n| &n.dev).filter(|d| d.kind == ServiceKind::Connect);
                let found = connect()
                    .find(|d| serial.is_some() && d.serial() == serial.as_deref())
                    .or_else(|| connect().find(|d| ip.is_some() && Some(d.addr.ip()) == ip));
                match found {
                    Some(d) => {
                        let _ = self.tx.send(Msg::Slot(slot, SlotEvent::ConnectSeen { addr: d.addr }));
                    }
                    None => self.rescan(ServiceKind::Connect),
                }
            }
            Effect::RescanConnect => self.rescan(ServiceKind::Connect),
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
                if self.keys.remove(&id)
                    && let Err(e) = self.keys.save()
                {
                    tracing::error!("saving direct-keys.toml: {e}");
                }
            }
            Effect::Toast { code, message } => self.toast(Some(code), message),
        }
    }

    /// Effects that work differently without ADB. Returns false for the ones
    /// handled the usual way (timers, toasts, persistence, …).
    fn apply_direct(&mut self, i: usize, fx: &Effect) -> bool {
        let slot = self.slots[i].id;
        match fx {
            Effect::Connect { addr } | Effect::Reconnect { addr } => self.connect_direct(i, addr),
            Effect::Disconnect { .. } => self.drop_direct(i),
            Effect::Identify { .. } => {
                let ev = match &self.direct[i] {
                    Some(l) => SlotEvent::Identified(DeviceInfo {
                        id: l.hello.id.clone(),
                        model: l.hello.model.clone(),
                        manufacturer: l.hello.manufacturer.clone(),
                        sdk: l.hello.sdk,
                        features: Vec::new(),
                        wifi_mac: None,
                    }),
                    None => SlotEvent::IdentifyFailed("the helper connection closed".into()),
                };
                let _ = self.tx.send(Msg::Slot(slot, ev));
            }
            // The link itself is the heartbeat.
            Effect::StartMonitor { .. } | Effect::StopMonitor => {}
            Effect::StartStream { id, profile, .. } => {
                if let Some(old) = self.sessions[i].take() {
                    old.stop();
                }
                self.auto_max_size[i] = None;
                let (media, link, device_id) = match (&self.stream_env, &self.direct[i], self.slots[i].device_id()) {
                    (Ok(env), Some(l), Some(d)) => (env.media.clone(), l, d.to_owned()),
                    (Err(reason), ..) => {
                        let _ = self.tx.send(Msg::Slot(slot, SlotEvent::StreamEnded { id: *id, error: Some(reason.clone()) }));
                        return true;
                    }
                    _ => {
                        let error = Some("the helper connection closed".to_owned());
                        let _ = self.tx.send(Msg::Slot(slot, SlotEvent::StreamEnded { id: *id, error }));
                        return true;
                    }
                };
                let record = self.devices.get(&device_id);
                let (p, _) = resolve_profile(&self.cfg, *profile, record);
                let mut params = server_params(&self.cfg, &p, self.tile_px[i], None);
                if *profile == StreamProfile::Grid && matches!(p.max_size, ac_config::MaxSize::Auto(_)) {
                    self.auto_max_size[i] = Some(params.max_size);
                }
                adapt(&self.cfg, &self.adaptive[i], &mut self.stream_rate[i], &self.io[i], *profile, &p, &mut params);
                // Helper 0.6 encodes H.265 where the phone has the hardware;
                // AV1 is never offered by it.
                // devices.toml narrows it once a format failed on this phone.
                let known = record.map_or(&[][..], |r| r.supported_codecs.as_slice());
                let codecs: Vec<String> =
                    link.hello.codecs.iter().filter(|c| *c != "av1" && (known.is_empty() || known.contains(c))).cloned().collect();
                params.codec = pick_codec(&p.codec, &codecs, &media.probe);
                self.stream_codec[i] = params.codec;
                let env = DirectEnv { media, addr: link.addr, creds: self.keys.credentials(), device_id };
                let h = spawn_direct_session(env, params, slot, *id, self.io[i].clone(), self.session_tx.clone());
                self.sessions[i] = Some(h);
            }
            _ => return false,
        }
        true
    }

    /// Opens the helper link (replacing any previous one) and reports back as
    /// `ConnectDone` and an `AdbState`, like an ADB transport would.
    fn connect_direct(&mut self, i: usize, addr: &str) {
        let slot = self.slots[i].id;
        self.drop_direct(i);
        let sa = match addr.parse::<SocketAddr>() {
            Ok(sa) if self.check_lan(addr).is_ok() => sa,
            _ => {
                let _ = self.tx.send(Msg::ConnectResult(slot, Err(format!("{addr} is not a local IP:port"))));
                return;
            }
        };
        self.direct_conn += 1;
        let conn = self.direct_conn;
        self.direct_pending[i] = conn;
        let creds = self.keys.credentials();
        let expect = self.slots[i].device_id().map(str::to_owned).or_else(|| self.slots[i].expect_serial.clone());
        let (tx, cfg, game_port) = (self.tx.clone(), self.cfg.connection.clone(), self.cfg.automation.game_port);
        self.spawn_msg(async move {
            let result = match ac_direct::Link::connect(sa, &creds, expect.as_deref()).await {
                Ok(mut link) => {
                    link.game_port = game_port;
                    let hello = link.hello.clone();
                    let (cmds, rx) = mpsc::unbounded_channel();
                    let task = tokio::spawn(run_direct_link(link, slot, conn, cfg, tx, rx));
                    Ok(DirectLink { conn, addr: sa, hello, cmds, task })
                }
                Err(e) => Err(e.to_string()),
            };
            Msg::DirectLinked { slot, conn, result }
        });
    }

    fn drop_direct(&mut self, i: usize) {
        self.direct_pending[i] = 0;
        if let Some(l) = self.direct[i].take() {
            l.task.abort();
        }
    }

    fn pair_direct(&mut self, slot: Option<SlotId>, addr: &str, code: &str) {
        let Some(code) = parse_pair_code(code) else { return self.toast(None, "The pairing code is 6 digits") };
        let Some(sa) = ac_direct::parse_addr(addr) else {
            return self.toast(None, format!("“{}” is not an IP address", addr.trim()));
        };
        if let Err(m) = self.check_lan(&sa.to_string()) {
            return self.toast(Some(ErrorCode::NotLan), m);
        }
        let Some(slot) = self.pick_slot(slot) else { return self.toast(None, self.slots_full()) };
        let (pc_id, pc_name) = (self.keys.pc_id.clone(), ac_direct::pc_name());
        self.spawn_msg(async move {
            let result = ac_direct::pair(sa, &pc_id, &pc_name, &code).await.map_err(|e| e.to_string());
            Msg::DirectPaired { slot, addr: sa, result }
        });
    }

    fn on_direct_paired(&mut self, slot: SlotId, addr: SocketAddr, result: Result<ac_direct::Paired, String>) {
        let p = match result {
            Ok(p) => p,
            Err(e) => return self.toast(Some(ErrorCode::PairFailed), format!("Pairing failed: {e}")),
        };
        self.keys.insert(&p);
        if let Err(e) = self.keys.save() {
            tracing::error!("saving direct-keys.toml: {e}");
        }
        tracing::info!(slot, "paired with {} ({}) without ADB", p.model, p.id);
        self.toast(None, format!("Paired with {}", p.model));
        self.suppressed.remove(&p.id);
        // Already showing in a slot (paired again)? Reconnect it there.
        let target = self
            .slots
            .iter()
            .find(|s| s.device_id() == Some(p.id.as_str()))
            .map(|s| s.id)
            .or_else(|| self.pick_slot(Some(slot)))
            .or_else(|| self.pick_slot(None));
        let Some(target) = target else { return self.toast(None, self.slots_full()) };
        if self.idx(target).is_some_and(|i| matches!(self.slots[i].phase, Phase::Online { .. })) {
            return;
        }
        let ev = SlotEvent::Connect { addr: addr.to_string(), transport: TransportKind::Direct, expect_serial: Some(p.id), silent: false };
        self.feed(target, ev);
    }

    fn on_direct_linked(&mut self, slot: SlotId, conn: u64, result: Result<DirectLink, String>) {
        let Some(i) = self.idx(slot) else { return };
        if conn != self.direct_pending[i] {
            if let Ok(l) = result {
                l.task.abort();
            }
            return;
        }
        match result {
            Ok(l) => {
                let addr = l.addr.to_string();
                self.direct_pending[i] = 0;
                self.direct[i] = Some(l);
                self.feed(slot, SlotEvent::ConnectDone(Ok(())));
                self.feed(slot, SlotEvent::AdbState { serial: addr, state: DeviceState::Device });
            }
            Err(e) => {
                self.direct_pending[i] = 0;
                self.feed(slot, SlotEvent::ConnectDone(Err(e)));
            }
        }
    }

    /// Checks the `adb tcpip` port of remembered phones the reconnect loop gave
    /// up on, and reconnects the ones that answer (spec §3.5 legacy transports).
    fn probe_legacy(&self) {
        if !self.cfg.discovery.auto_connect_known {
            return;
        }
        for s in &self.slots {
            let Phase::Disconnected { .. } = s.phase else { continue };
            let Some(id) = s.device_id() else { continue };
            if self.suppressed.contains(id) || self.devices.get(id).is_some_and(|r| !r.auto_connect) {
                continue;
            }
            let addr = match (&s.legacy_addr, s.transport, &s.addr) {
                (Some(a), _, _) => a.clone(),
                (None, TransportKind::Legacy | TransportKind::Direct, Some(a)) => a.clone(),
                _ => continue,
            };
            if self.suppressed.contains(&addr) {
                continue;
            }
            let Ok(sock) = addr.parse::<SocketAddr>() else { continue };
            let slot = s.id;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let open = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(sock)).await;
                if matches!(open, Ok(Ok(_))) {
                    let _ = tx.send(Msg::LegacyAlive { slot, addr });
                }
            });
        }
    }

    /// Grants the helper app its permission if the phone has it installed.
    fn setup_helper(&self, i: usize) {
        let s = &self.slots[i];
        if s.transport == TransportKind::Direct {
            return; // it is already running: that is how we got here
        }
        let (Some(serial), Some(d)) = (s.adb_serial.clone(), s.device.as_ref()) else { return };
        if d.sdk != 0 && d.sdk < 23 {
            return; // the helper needs Android 6+
        }
        let shell_v2 = d.shell_v2();
        let title = if s.alias.is_empty() && !d.model.is_empty() { d.model.clone() } else { s.display_name() };
        let (adb, tx, slot, device_id) = (self.adb.clone(), self.tx.clone(), s.id, d.id.clone());
        self.spawn_msg(async move {
            let out = adb.shell(&serial, HELPER_SETUP_CMD, shell_v2).await;
            if let Ok(o) = &out
                && let Some(v) = o.stdout.lines().find_map(|l| l.trim().strip_prefix("@versionCode=")?.parse().ok())
            {
                let _ = tx.send(Msg::HelperVersion { slot, device_id, version: v });
            }
            match out {
                Ok(out) if out.stdout.lines().any(|l| l.trim() == "@granted") => {
                    tracing::info!("set up Wireless Debugging Keeper on {title}");
                    Msg::Toast(None, format!("Wireless Debugging Keeper is set up on {title}: wireless debugging now stays on"))
                }
                Ok(out) if out.stdout.contains("@none") || out.stdout.contains("@ready") => Msg::Prune,
                Ok(out) => {
                    tracing::warn!("helper setup on {title}: {}{}", out.stdout, out.stderr);
                    Msg::Prune
                }
                Err(e) => {
                    tracing::warn!("helper setup on {title}: {e}");
                    Msg::Prune
                }
            }
        });
    }

    /// The shipped helper APK on disk (written once) and its version code.
    fn helper_apk(&mut self) -> Option<(PathBuf, u32)> {
        if self.helper_apk.is_none() {
            let dir = self.paths.as_ref().map_or_else(std::env::temp_dir, |p| p.state_dir.clone());
            let path = dir.join("wireless-debugging-keeper.apk");
            let fresh = std::fs::read(&path).is_ok_and(|b| b == HELPER_APK);
            if !fresh && let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, HELPER_APK)) {
                tracing::warn!("writing {}: {e}", path.display());
                return None;
            }
            match ac_transfer::apk::info(&path) {
                Ok(info) => self.helper_apk = Some((path, info.version_code.unwrap_or(0) as u32)),
                Err(e) => tracing::warn!("the shipped helper APK: {e}"),
            }
        }
        self.helper_apk.clone()
    }

    fn check_helper_version(&mut self, i: usize, version: u32) {
        let Some((_, shipped)) = self.helper_apk() else { return };
        if version < shipped {
            tracing::info!(slot = i + 1, version, shipped, "the phone's helper is older than the shipped one");
            self.helper_old[i] = Some(version);
        }
    }

    /// Installs the shipped helper on the phone (silently over ADB; without
    /// ADB the phone asks its user).
    fn update_helper(&mut self, slot: SlotId) {
        let Some(i) = self.idx(slot) else { return };
        let (Some((path, _)), Some(d)) = (self.helper_apk(), self.slots[i].device.as_ref()) else { return };
        if !matches!(self.slots[i].phase, Phase::Online { .. }) {
            return;
        }
        let title = self.title(i);
        self.transfers.submit(slot, d.id.clone(), title, vec![path], true);
        self.helper_old[i] = None;
        // Check again once it has restarted.
        self.helper_checked[i] = self.slots[i].transport == TransportKind::Direct;
    }

    /// Reads (`set = None`) or changes the phone's "keep screen on" state.
    fn run_stay_awake(&mut self, i: usize, set: Option<bool>) {
        let s = &self.slots[i];
        if s.transport == TransportKind::Direct {
            let (Some(l), Some(d)) = (&self.direct[i], s.device.as_ref()) else { return };
            if l.cmds.send(LinkCmd::StayAwake { set, device_id: d.id.clone() }).is_ok() {
                self.stay_awake_busy[i] = true;
            }
            return;
        }
        let (Some(serial), Some(d)) = (s.adb_serial.clone(), s.device.as_ref()) else { return };
        let (slot, device_id, shell_v2) = (s.id, d.id.clone(), d.shell_v2());
        let cmd = stay_awake_cmd(set);
        self.stay_awake_busy[i] = true;
        let adb = self.adb.clone();
        self.spawn_msg(async move {
            let on = match adb.shell(&serial, &cmd, shell_v2).await {
                Ok(out) if out.stdout.lines().any(|l| l.trim() == "@ok") => Some(out.stdout.lines().any(|l| l.trim() == "@on")),
                Ok(out) => {
                    tracing::warn!(slot, "keep screen on: {}{}", out.stdout, out.stderr);
                    None
                }
                Err(e) => {
                    tracing::warn!(slot, "keep screen on: {e}");
                    None
                }
            };
            Msg::StayAwake { slot, device_id, on }
        });
    }

    /// Asks a newly seen phone which video encoders it has (spec §4.3),
    /// once: the answer is kept in devices.toml.
    fn probe_codecs(&self, i: usize) {
        let s = &self.slots[i];
        let (Ok(env), Some(serial), Some(d)) = (&self.stream_env, s.adb_serial.clone(), s.device.as_ref()) else { return };
        if s.transport == TransportKind::Direct || self.devices.get(&d.id).is_some_and(|r| !r.supported_codecs.is_empty()) {
            return;
        }
        let (env, slot, device_id, shell_v2) = (env.clone(), s.id, d.id.clone(), d.shell_v2());
        self.spawn_msg(async move {
            let codecs = ac_scrcpy::list_encoders(&env.adb, &serial, &env.server_jar, &env.server_version, shell_v2)
                .await
                .map(|list| hardware_codecs(&list))
                .map_err(|e| format!("{e}{}", if e.server_output.is_empty() { String::new() } else { format!("\n{}", e.server_output) }));
            Msg::Codecs { slot, device_id, codecs }
        });
    }

    fn on_codecs(&mut self, slot: SlotId, device_id: &str, codecs: Result<Vec<String>, String>) {
        match codecs {
            Ok(codecs) => {
                tracing::info!(slot, "{device_id} has hardware encoders for {}", codecs.join(", "));
                if let Some(rec) = self.devices.get_mut(device_id) {
                    rec.supported_codecs = codecs;
                    self.save_devices();
                }
            }
            Err(e) => tracing::warn!(slot, "listing {device_id}'s video encoders failed: {e}"),
        }
    }

    /// An H.265/AV1 stream that never got going: forget that codec for this
    /// phone, so the retry (and later sessions) use H.264.
    fn check_codec_failure(&mut self, slot: SlotId, ev: &SlotEvent) {
        let Some(i) = self.idx(slot) else { return };
        let SlotEvent::StreamEnded { id, error: Some(error) } = ev else { return };
        let codec = self.stream_codec[i];
        if *id != self.slots[i].stream_id() || codec == VideoCodec::H264 || !matches!(self.slots[i].stream, StreamPhase::Starting { .. }) {
            return;
        }
        self.stream_codec[i] = VideoCodec::H264;
        let name = codec_name(codec);
        let Some(rec) = self.slots[i].device_id().and_then(|id| self.devices.get_mut(id)) else { return };
        if rec.supported_codecs.is_empty() {
            // A phone without ADB: its helper's list was used.
            rec.supported_codecs = vec!["h264".into(), codec.option().into()];
        }
        let before = rec.supported_codecs.len();
        rec.supported_codecs.retain(|c| c != codec.option());
        if rec.supported_codecs.len() != before {
            let who = rec.display_name().to_owned();
            tracing::warn!(slot, "{name} failed on {who} ({error}); using H.264 from now on");
            self.save_devices();
            self.toast(None, format!("{who}: {name} mirroring did not start, so it uses H.264 from now on"));
        }
    }

    /// One second of lag samples for every live stream.
    fn adaptive_tick(&mut self) {
        let now = Instant::now();
        for i in 0..self.slots.len() {
            let bytes = self.io[i].mailbox.stats.bytes.load(std::sync::atomic::Ordering::Relaxed);
            let moved = bytes != std::mem::replace(&mut self.video_bytes[i], bytes);
            let live = matches!(self.slots[i].phase, Phase::Online { .. }) && matches!(self.slots[i].stream, StreamPhase::Live { .. });
            let (Some((base, profile)), true, true) = (self.stream_rate[i], live, self.cfg.video.adaptive_bitrate) else {
                continue;
            };
            if !moved {
                self.adaptive[i].idle();
                continue;
            }
            let lag = self.io[i].mailbox.stats.lag_ms.load(std::sync::atomic::Ordering::Relaxed);
            let v = &self.cfg.video;
            let grid = profile == StreamProfile::Grid;
            if let Some(r) = self.adaptive[i].sample(lag, now, base, grid, v.lag_downgrade_ms, v.lag_upgrade_ms) {
                let slot = self.slots[i].id;
                tracing::info!(slot, lag, kbps = r.kbps, fps = r.fps, "adaptive bitrate: restarting the stream");
                self.feed(slot, SlotEvent::RestartStream);
            }
        }
    }

    // ---- app testing (Milestone 5) ---------------------------------------------------

    fn title(&self, i: usize) -> String {
        self.views[i].as_ref().map_or_else(|| self.slots[i].display_name(), |v| v.title.clone())
    }

    /// `(serial, shell_v2, sdk)` of an online phone reached over ADB.
    fn adb_target(&self, i: usize) -> Option<(String, bool, u32)> {
        let s = &self.slots[i];
        match (&s.phase, &s.adb_serial, &s.device) {
            (Phase::Online { .. }, Some(serial), Some(d)) if s.transport != TransportKind::Direct => {
                Some((serial.clone(), d.shell_v2(), d.sdk))
            }
            _ => None,
        }
    }

    fn run_view(&self) -> RunView {
        match &self.run {
            Some(r) => RunView { active: true, name: r.name.clone(), dir: r.dir.clone(), crashes: r.crash_count() },
            None => RunView { active: false, name: String::new(), dir: PathBuf::new(), crashes: 0 },
        }
    }

    /// Starts and stops each slot's logcat stream and perf sampler to match
    /// what is wanted: crash watching, a test run, the perf graphs. A phone
    /// joining during a run gets its folder.
    fn reconcile_testing(&mut self) {
        let now = Instant::now();
        for i in 0..self.slots.len() {
            let online = matches!(self.slots[i].phase, Phase::Online { .. });
            if !online {
                if self.io[i].mailbox.stop_recording() {
                    tracing::info!(slot = i + 1, "phone gone: recording stopped");
                }
            } else if self.run.is_some() && self.views[i].is_some() {
                let (title, logs) = (self.title(i), self.slots[i].transport != TransportKind::Direct);
                if let Some(run) = &mut self.run
                    && run.phones[i].is_none()
                {
                    run.phone(i, &title, logs);
                }
            }
            let target = self.adb_target(i);
            // Logcat: the full log into the run's folder, else crash lines only.
            let file = self.run.as_ref().and_then(|r| r.phones[i].as_ref()).map(|p| p.dir.join("logcat.txt"));
            let want_log = target.as_ref().filter(|_| self.cfg.testing.watch_crashes || file.is_some());
            let keep = match (&self.logcat[i], want_log) {
                (Some(t), Some((serial, ..))) => !t.task.is_finished() && t.serial == *serial && t.file == file,
                _ => false,
            };
            if !keep {
                if let Some(t) = self.logcat[i].take() {
                    t.task.abort();
                }
                if let Some((serial, shell_v2, sdk)) = want_log
                    && self.logcat_started[i].is_none_or(|t| now.duration_since(t) >= LOGCAT_RETRY)
                {
                    self.logcat_started[i] = Some(now);
                    let (adb, slot, tx) = (self.adb.clone(), i as SlotId + 1, self.tx.clone());
                    let (serial, shell_v2, sdk, f) = (serial.clone(), *shell_v2, *sdk, file.clone());
                    let watched = serial.clone();
                    let (log_tx, mut log_rx) = mpsc::unbounded_channel();
                    let task = tokio::spawn(async move {
                        let forward = async {
                            while let Some(ev) = log_rx.recv().await {
                                if tx.send(Msg::Log(ev)).is_err() {
                                    return;
                                }
                            }
                        };
                        tokio::join!(crate::logcat::run(adb, serial, shell_v2, sdk, slot, f, log_tx), forward);
                    });
                    self.logcat[i] = Some(LogTask { task, serial: watched, file });
                }
            }
            // Perf sampler (ADB; the helper reports in its heartbeat).
            let want_perf = target.filter(|_| self.perf_wanted());
            let keep = matches!((&self.perf[i], &want_perf), (Some((t, s)), Some((serial, ..))) if !t.is_finished() && s == serial);
            if !keep {
                if let Some((t, _)) = self.perf[i].take() {
                    t.abort();
                }
                if let Some((serial, shell_v2, _)) = want_perf {
                    let every = Duration::from_millis(self.cfg.testing.perf_interval_ms.clamp(500, 60_000));
                    let (adb, slot, tx, app) = (self.adb.clone(), i as SlotId + 1, self.tx.clone(), self.test_app.subscribe());
                    let task = tokio::spawn(sample_perf(adb, serial.clone(), shell_v2, slot, every, app, tx));
                    self.perf[i] = Some((task, serial));
                }
            }
        }
        self.reconcile_games();
    }

    /// The graphs, a test run or automation want performance samples.
    fn perf_wanted(&self) -> bool {
        self.perf_on || self.run.is_some() || self.control_perf()
    }

    fn on_log(&mut self, ev: LogEvent) {
        let LogEvent::Crash(slot, crash) = ev else { return }; // an ended stream is restarted by reconcile
        let Some(i) = self.idx(slot) else { return };
        tracing::info!(slot, "{} in {}: {}", crash.kind.label(), crash.package, crash.summary);
        self.note_crash(slot, &crash);
        if let Some(p) = self.run.as_mut().and_then(|r| r.phones[i].as_mut()) {
            p.crashes.push(crash.clone());
            append(&p.dir.join("crashes.txt"), &crash_entry(&crash));
        }
        if self.run.is_some() {
            self.send(UiEvent::Run(self.run_view()));
        }
        self.send(UiEvent::Crash { slot, crash });
    }

    fn on_perf(&mut self, slot: SlotId, sample: PerfSample) {
        let Some(i) = self.idx(slot) else { return };
        if !matches!(self.slots[i].phase, Phase::Online { .. }) || !self.perf_wanted() {
            return;
        }
        self.note_perf(i, &sample);
        if let Some(p) = self.run.as_ref().and_then(|r| r.phones[i].as_ref()) {
            let path = p.dir.join("stats.csv");
            if !path.exists() {
                append(&path, crate::perf::CSV_HEADER);
            }
            append(&path, &crate::perf::csv_row(&chrono::Local::now().format("%H:%M:%S").to_string(), &sample));
        }
        self.send(UiEvent::Perf { slot, sample });
    }

    fn start_run(&mut self, name: &str) {
        if self.run.is_some() {
            self.stop_run();
        }
        let root = output_root(&self.cfg.testing.output_dir);
        match Run::new(&root, name, self.slots.len(), chrono::Local::now()) {
            Ok(run) => {
                tracing::info!("test run started in {}", run.dir.display());
                self.run = Some(run);
                // Phone folders, logcat files and samplers follow in reconcile.
                self.send(UiEvent::Run(self.run_view()));
            }
            Err(e) => self.toast(None, format!("Could not create the test run folder in {}: {e}", root.display())),
        }
    }

    fn stop_run(&mut self) {
        let Some(run) = self.run.take() else { return };
        let summary = run.summary(chrono::Local::now());
        if let Err(e) = std::fs::write(run.dir.join("summary.txt"), &summary) {
            tracing::warn!("writing summary.txt: {e}");
        }
        for i in 0..self.slots.len() {
            if self.io[i].mailbox.is_recording() && run.phones[i].is_some() {
                self.io[i].mailbox.stop_recording();
            }
        }
        let n = run.crash_count();
        let what = match n {
            0 => "no crashes".to_owned(),
            1 => "1 crash or ANR".to_owned(),
            n => format!("{n} crashes or ANRs"),
        };
        self.send(UiEvent::Run(self.run_view()));
        self.send(UiEvent::Saved { message: format!("Test run saved: {what}"), folder: run.dir });
    }

    /// Where a capture of slot `i` goes: its phone folder in the test run,
    /// else `<root>/<what>`. Returns the folder and the file stem.
    fn capture_target(&mut self, i: usize, what: &str, folder: &str) -> (PathBuf, String) {
        let now = chrono::Local::now();
        let title = self.title(i);
        let logs = self.slots[i].transport != TransportKind::Direct;
        match &mut self.run {
            Some(run) => (run.phone(i, &title, logs).dir.clone(), capture_name(what, now, "x").trim_end_matches(".x").to_owned()),
            None => {
                let dir = output_root(&self.cfg.testing.output_dir).join(folder);
                let _ = std::fs::create_dir_all(&dir);
                (dir, capture_name(&crate::testing::safe_name(&title), now, "x").trim_end_matches(".x").to_owned())
            }
        }
    }

    fn screenshot(&mut self, slots: Vec<SlotId>) {
        let mut jobs = Vec::new();
        let mut folder = None;
        for slot in slots {
            let Some(i) = self.idx(slot) else { continue };
            if !matches!(self.slots[i].phase, Phase::Online { .. }) {
                continue;
            }
            let (dir, stem) = self.capture_target(i, "Screenshot", "Screenshots");
            let path = unique(&dir, &stem, "png");
            folder.get_or_insert(dir);
            match self.adb_target(i) {
                Some((serial, ..)) => jobs.push((self.title(i), serial, path)),
                // No ADB: the window saves the mirror's frame.
                None => self.send(UiEvent::SaveFrame { slot, path, request: None }),
            }
        }
        let Some(mut folder) = folder else { return self.toast(None, "No connected phone to take a screenshot of") };
        if let Some(run) = &self.run
            && jobs.len() > 1
        {
            folder = run.dir.clone(); // one folder per phone in there
        }
        if jobs.is_empty() {
            return;
        }
        let (adb, ui) = (self.adb.clone(), self.ui.clone());
        tokio::spawn(async move {
            let mut set = tokio::task::JoinSet::new();
            for (title, serial, path) in jobs {
                let adb = adb.clone();
                set.spawn(async move { (title, screencap(&adb, &serial, &path).await) });
            }
            let (mut ok, mut failed) = (0, Vec::new());
            while let Some(Ok((title, r))) = set.join_next().await {
                match r {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        tracing::warn!("screenshot of {title}: {e}");
                        failed.push(title);
                    }
                }
            }
            if ok > 0 {
                let message = if ok == 1 { "Screenshot saved".to_owned() } else { format!("{ok} screenshots saved") };
                let _ = ui.try_send(UiEvent::Saved { message, folder });
            }
            if !failed.is_empty() {
                let _ = ui.try_send(UiEvent::Toast { code: None, message: format!("No screenshot from {}", failed.join(", ")) });
            }
        });
    }

    fn record(&mut self, slots: Vec<SlotId>, on: bool) {
        for slot in slots {
            let Some(i) = self.idx(slot) else { continue };
            if !on {
                self.io[i].mailbox.stop_recording();
                continue;
            }
            if self.io[i].mailbox.is_recording() || !matches!(self.slots[i].phase, Phase::Online { .. }) {
                continue;
            }
            if !matches!(self.slots[i].stream, StreamPhase::Live { .. } | StreamPhase::Starting { .. }) {
                self.toast(None, format!("{} is not mirroring, so there is nothing to record", self.title(i)));
                continue;
            }
            let (dir, stem) = self.capture_target(i, "Recording", "Recordings");
            let ui = self.ui.clone();
            let on_done: ac_media::recorder::OnDone = Box::new(move |r| {
                let _ = ui.try_send(UiEvent::Recording { slot, file: None });
                let ev = match r {
                    Ok(files) if files.is_empty() => UiEvent::Toast { code: None, message: "The recording is empty: no video arrived".into() },
                    Ok(files) => UiEvent::Saved {
                        message: if files.len() == 1 { "Recording saved".into() } else { format!("Recording saved in {} parts", files.len()) },
                        folder: files[0].parent().map(PathBuf::from).unwrap_or_default(),
                    },
                    Err(e) => UiEvent::Toast { code: None, message: format!("Recording failed: {e}") },
                };
                let _ = ui.try_send(ev);
            });
            let path = dir.join(&stem);
            match self.io[i].mailbox.start_recording(path.clone(), on_done) {
                Ok(()) => {
                    // A still screen sends nothing: ask for a key frame now so
                    // the file starts at once (the decode loop asks again if
                    // other frames come first).
                    self.io[i].send(ac_scrcpy::ControlMsg::ResetVideo);
                    self.send(UiEvent::Recording { slot, file: Some(path.with_extension("mp4")) });
                }
                Err(e) => self.toast(None, format!("Could not start recording: {e}")),
            }
        }
    }

    fn app_action(&mut self, slot: SlotId, action: AppAction) {
        let Some(i) = self.idx(slot) else { return };
        let kind = action.kind();
        let reply = move |ok: bool, message: String| UiEvent::AppResult { slot, kind, ok, message };
        if !matches!(self.slots[i].phase, Phase::Online { .. }) {
            return self.send(reply(false, "Not connected".into()));
        }
        if let Some((serial, shell_v2, _)) = self.adb_target(i) {
            let (adb, ui) = (self.adb.clone(), self.ui.clone());
            tokio::spawn(async move {
                let ev = match adb.shell(&serial, &app_cmd(&action), shell_v2).await {
                    Ok(out) => match app_result(&action, &format!("{}{}", out.stdout, out.stderr)) {
                        Ok(m) => reply(true, m),
                        Err(m) => reply(false, m),
                    },
                    Err(e) => reply(false, e.to_string()),
                };
                let _ = ui.try_send(ev);
            });
            return;
        }
        match &self.direct[i] {
            Some(l) if l.hello.version < ac_direct::HELPER_V6 => {
                self.send(reply(false, "Update Wireless Debugging Keeper on this phone (0.6 or newer) for app actions".into()))
            }
            Some(l) => {
                let _ = l.cmds.send(LinkCmd::App { action, reply: None });
            }
            None => self.send(reply(false, "Not connected".into())),
        }
    }

    fn list_packages(&mut self, slot: SlotId) {
        let Some(i) = self.idx(slot) else { return };
        if let Some((serial, shell_v2, _)) = self.adb_target(i) {
            let (adb, ui) = (self.adb.clone(), self.ui.clone());
            tokio::spawn(async move {
                if let Ok(out) = adb.shell(&serial, "pm list packages -3", shell_v2).await {
                    let _ = ui.try_send(UiEvent::Packages { slot, packages: parse_packages(&out.stdout) });
                }
            });
        } else if let Some(l) = self.direct[i].as_ref().filter(|l| l.hello.version >= ac_direct::HELPER_V6) {
            let _ = l.cmds.send(LinkCmd::Packages { reply: None });
        }
    }

    fn rescan(&self, kind: ServiceKind) {
        if let Some(r) = &self.rescan {
            let _ = r.send(kind);
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
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, self.slots_full()) };
                self.suppressed.remove(&addr);
                self.feed(slot, SlotEvent::Connect { addr, transport, expect_serial: None, silent: false });
            }
            UiCommand::PairCode { slot, instance, code } => {
                let Some(code) = parse_pair_code(&code) else { return self.toast(None, "The pairing code is 6 digits") };
                let Some(n) = self.nearby.get(&(ServiceKind::Pairing, instance.clone())) else {
                    return self.toast(Some(ErrorCode::PairFailed), "That pairing request is no longer advertised; reopen it on the phone");
                };
                let addr = n.dev.addr;
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, self.slots_full()) };
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
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, self.slots_full()) };
                if let Some(c) = &connect {
                    self.suppressed.remove(c);
                }
                self.feed(slot, SlotEvent::PairCode { addr, code, connect });
            }
            UiCommand::PairDirect { slot, addr, code } => self.pair_direct(slot, &addr, &code),
            UiCommand::PairQr { slot } => {
                let Some(slot) = self.pick_slot(slot) else { return self.toast(None, self.slots_full()) };
                let secret = QrSecret::generate(&mut rand::rng());
                let payload = secret.payload();
                self.feed(slot, SlotEvent::PairQr { secret });
                self.send(UiEvent::QrCode { slot, payload, expires_in: PAIR_QR_TIMEOUT });
            }
            UiCommand::CancelPairing { slot } => {
                if let Some(i) = self.idx(slot)
                    && matches!(self.slots[i].phase, Phase::Pairing { .. })
                {
                    self.feed(slot, SlotEvent::Disconnect);
                }
            }
            UiCommand::EnableWifiAdb { usb_serial, slot } => self.enable_wifi_adb(usb_serial, slot),
            UiCommand::Disconnect { slot } => {
                if let Some(i) = self.idx(slot) {
                    let s = &self.slots[i];
                    self.suppressed.extend(s.device_id().map(str::to_owned));
                    self.suppressed.extend(s.adb_serial.clone());
                    self.feed(slot, SlotEvent::Disconnect);
                }
            }
            UiCommand::Reconnect { slot } => {
                let Some(i) = self.idx(slot) else { return };
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
            UiCommand::Forget { slot } => {
                self.transfers.cancel(slot);
                self.feed(slot, SlotEvent::Forget);
            }
            UiCommand::Rename { slot, alias } => {
                let Some(i) = self.idx(slot) else { return };
                self.slots[i].alias = alias.trim().to_owned();
                if let Some(id) = self.slots[i].device_id().map(str::to_owned)
                    && let Some(rec) = self.devices.get_mut(&id)
                {
                    rec.alias = alias.trim().to_owned();
                    self.save_devices();
                }
            }
            UiCommand::SetProfile { slot, profile } => {
                // Each profile starts from its own bitrate.
                if let Some(i) = self.idx(slot)
                    && self.slots[i].profile != profile
                {
                    self.adaptive[i].reset();
                }
                self.feed(slot, SlotEvent::SetProfile(profile));
            }
            UiCommand::RestartStream { slot } => self.feed(slot, SlotEvent::RestartStream),
            UiCommand::SendFiles { slots, paths, install_apks } => {
                for slot in slots {
                    let Some(i) = self.idx(slot) else { continue };
                    let s = &self.slots[i];
                    match (&s.phase, s.device.as_ref()) {
                        (Phase::Online { .. }, Some(d)) => {
                            let title = self.views[i].as_ref().map_or_else(|| s.display_name(), |v| v.title.clone());
                            self.transfers.submit(slot, d.id.clone(), title, paths.clone(), install_apks);
                        }
                        _ => self.toast(None, format!("Slot {slot} has no connected phone")),
                    }
                }
            }
            UiCommand::CancelTransfers { slot } => self.transfers.cancel(slot),
            UiCommand::UpdateConfig(cfg) => {
                self.transfers.configure(cfg.transfer.clone());
                // What the running tasks copied at start (ADB server, slot
                // count, the device tracker) stays until the next launch.
                let general = self.cfg.general.clone();
                if cfg.testing.perf_interval_ms != self.cfg.testing.perf_interval_ms {
                    // The samplers restart with the new interval.
                    for p in &mut self.perf {
                        if let Some((t, _)) = p.take() {
                            t.abort();
                        }
                    }
                }
                self.cfg = *cfg;
                self.cfg.general = general;
            }
            UiCommand::SetStayAwake { slot, on } => {
                if let Some(i) = self.idx(slot)
                    && matches!(self.slots[i].phase, Phase::Online { .. })
                {
                    self.run_stay_awake(i, Some(on));
                }
            }
            UiCommand::AppAction { slots, action } => {
                for slot in slots {
                    self.app_action(slot, action.clone());
                }
            }
            UiCommand::ListPackages { slot } => self.list_packages(slot),
            UiCommand::SetTestApp { package } => {
                let package = package.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
                self.test_app.send_replace(package);
            }
            UiCommand::StartRun { name } => self.start_run(&name),
            UiCommand::StopRun => self.stop_run(),
            UiCommand::Screenshot { slots } => self.screenshot(slots),
            UiCommand::Record { slots, on } => self.record(slots, on),
            UiCommand::SetPerf { on } => self.perf_on = on,
            UiCommand::UpdateHelper { slot } => self.update_helper(slot),
            UiCommand::Confirm { id, ok } => self.on_confirm(id, ok),
            UiCommand::FrameSaved { request, error } => self.on_frame_saved(request, error),
            UiCommand::SetTileSize { slot, px } => {
                let Some(i) = self.idx(slot) else { return };
                self.tile_px[i] = Some(px);
                // Restart a live grid stream only for a big change (spec §4.3);
                // the UI sends this once the window has been still for 1 s.
                let Some(running) = self.auto_max_size[i] else { return };
                if !matches!(self.slots[i].stream, StreamPhase::Live { .. }) {
                    return;
                }
                let record = self.slots[i].device_id().and_then(|id| self.devices.get(id));
                let want = max_size_for(&resolve_profile(&self.cfg, StreamProfile::Grid, record).0, Some(px));
                if tile_change_needs_restart(running, want) {
                    tracing::info!(slot, running, want, "grid tile resized; restarting the stream");
                    self.feed(slot, SlotEvent::RestartStream);
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
            Msg::Session(SessionMsg::Slot(slot, ev)) => {
                self.check_codec_failure(slot, &ev);
                self.feed(slot, ev);
            }
            Msg::Session(SessionMsg::DeviceClipboard(slot, text)) => self.send(UiEvent::DeviceClipboard { slot, text }),
            Msg::Session(SessionMsg::Notice(_, message)) => self.toast(None, message),
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
                for i in 0..self.slots.len() {
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
            Msg::StayAwake { slot, device_id, on } => {
                let Some(i) = self.idx(slot) else { return };
                self.stay_awake_busy[i] = false;
                // Ignore answers about a phone that has since left the slot.
                if self.slots[i].device_id() == Some(device_id.as_str()) && matches!(self.slots[i].phase, Phase::Online { .. }) {
                    if on.is_none() {
                        self.toast(None, "Could not change the phone's screen timeout");
                    }
                    // Unknown after a failure: show "off" rather than re-querying in a loop.
                    self.stay_awake[i] = Some(on.unwrap_or(false));
                }
            }
            Msg::Prune => {
                self.check_clients();
                let now = Instant::now();
                self.nearby.retain(|_, n| n.stale_since.is_none_or(|t| now.duration_since(t) < NEARBY_GRACE));
                if now.duration_since(self.last_legacy_probe) >= LEGACY_PROBE_EVERY {
                    self.last_legacy_probe = now;
                    self.probe_legacy();
                }
            }
            Msg::LegacyAlive { slot, addr } => {
                let Some(i) = self.idx(slot) else { return };
                let s = &self.slots[i];
                if matches!(s.phase, Phase::Disconnected { .. }) && s.device_id().is_some() {
                    tracing::info!(slot, "{addr} answers again; reconnecting");
                    let expect = s.device_id().map(str::to_owned);
                    let transport = if s.transport == TransportKind::Direct { TransportKind::Direct } else { TransportKind::Legacy };
                    self.feed(slot, SlotEvent::Connect { addr, transport, expect_serial: expect, silent: true });
                }
            }
            Msg::AdaptiveTick => self.adaptive_tick(),
            Msg::Log(ev) => self.on_log(ev),
            Msg::Perf { slot, sample } => self.on_perf(slot, sample),
            Msg::DirectClip { slot, text } => self.send(UiEvent::DeviceClipboard { slot, text }),
            Msg::Emit(ev) => self.send(ev),
            Msg::HelperVersion { slot, device_id, version } => {
                if let Some(i) = self.idx(slot)
                    && self.slots[i].device_id() == Some(device_id.as_str())
                {
                    self.check_helper_version(i, version);
                }
            }
            Msg::Codecs { slot, device_id, codecs } => self.on_codecs(slot, &device_id, codecs),
            Msg::Game { slot, serial, ev } => self.on_game(slot, &serial, ev),
            Msg::GameNews { slot, conn, news } => self.on_game_news(slot, conn, news),
            Msg::DirectScreen { slot, conn, size } => {
                if let Some(i) = self.idx(slot)
                    && self.direct[i].as_ref().is_some_and(|l| l.conn == conn)
                {
                    self.auto.screen[i] = Some(size);
                }
            }
            Msg::DirectPaired { slot, addr, result } => self.on_direct_paired(slot, addr, result),
            Msg::DirectLinked { slot, conn, result } => self.on_direct_linked(slot, conn, result),
            Msg::DirectInput { slot, conn, on } => {
                let Some(i) = self.idx(slot) else { return };
                let Some(l) = self.direct[i].as_mut().filter(|l| l.conn == conn) else { return };
                l.hello.input = on;
                if on {
                    let msg = format!("Touch control is on for {}", l.hello.model);
                    self.toast(None, msg);
                }
            }
            Msg::DirectGone { slot, conn } => {
                let Some(i) = self.idx(slot) else { return };
                if let Some(l) = self.direct[i].take_if(|l| l.conn == conn) {
                    tracing::info!(slot, "helper connection to {} lost", l.addr);
                    self.feed(slot, SlotEvent::AdbGone { serial: l.addr.to_string() });
                }
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
        let Some(i) = self.idx(slot) else { return };
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
        // The same phone is being reconnected in another slot (e.g. its
        // wireless-debugging port died and it came back on `adb tcpip`): hand
        // this transport to that slot instead of showing the phone twice.
        if let Some(j) = self.slots.iter().position(|s| {
            s.id != slot
                && matches!(s.phase, Phase::Reconnecting { .. } | Phase::ConnectingAdb { .. })
                && s.device_id() == Some(info.id.as_str())
        }) && let Some(serial) = self.slots[i].adb_serial.clone()
        {
            if let Some(h) = self.timers[i].take() {
                h.abort();
            }
            self.slots[i] = Slot::new(slot);
            let target = &mut self.slots[j];
            if serial.ends_with(":5555") {
                target.legacy_addr = Some(serial.clone());
            } else {
                target.addr = Some(serial.clone());
            }
            let owner = target.id;
            tracing::info!(slot = owner, "{} came back as {serial}", info.model);
            self.feed(owner, SlotEvent::AdbState { serial, state: DeviceState::Device });
            return;
        }
        // The phone moves here from a remembered (inactive) slot.
        for j in 0..self.slots.len() {
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
                    let just_paired_here = matches!(s.phase, Phase::Pairing { paired: true, pair_ip: Some(ip), .. } if ip == dev.addr.ip());
                    let mine = s.device_id() == Some(serial) || s.expect_serial.as_deref() == Some(serial) || just_paired_here;
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
            ServiceKind::Direct => {
                let Some(id) = dev.serial() else { return };
                if !self.keys.is_paired(id) {
                    return;
                }
                let auto = self.cfg.discovery.auto_connect_known && !self.suppressed.contains(id);
                let addr = dev.addr.to_string();
                let slot = self.slots.iter().find(|s| {
                    s.device_id() == Some(id)
                        && match s.phase {
                            // A new address (DHCP): go there instead.
                            Phase::Reconnecting { .. } | Phase::ConnectingAdb { connected: false, .. } => s.addr.as_deref() != Some(addr.as_str()),
                            Phase::Disconnected { .. } => auto && self.devices.get(id).is_none_or(|r| r.auto_connect),
                            _ => false,
                        }
                });
                if let Some(s) = slot {
                    let (slot, expect) = (s.id, Some(id.to_owned()));
                    self.feed(slot, SlotEvent::Connect { addr, transport: TransportKind::Direct, expect_serial: expect, silent: true });
                }
            }
        }
    }
}

// ---- free functions running on worker tasks -------------------------------------------

/// Version code 6 → `0.6`.
fn helper_version_name(code: u32) -> String {
    format!("0.{code}")
}

/// devices.toml `supported_codecs`: H.264 (always: it marks the list as
/// probed) plus the codecs a hardware encoder handles.
fn hardware_codecs(list: &[ac_scrcpy::Encoder]) -> Vec<String> {
    let mut out = vec!["h264".to_owned()];
    for c in [VideoCodec::H265, VideoCodec::Av1] {
        if list.iter().any(|e| e.codec == c && e.hw) {
            out.push(c.option().to_owned());
        }
    }
    out
}

fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "H.265",
        VideoCodec::Av1 => "AV1",
    }
}

/// Launches with the adapted bitrate and frame rate, if the controller
/// lowered them (spec §3.6), and remembers the profile's own values.
fn adapt(
    cfg: &Config,
    a: &Adaptive,
    rate: &mut Option<(Rate, StreamProfile)>,
    io: &SlotIo,
    profile: StreamProfile,
    p: &ac_config::Profile,
    params: &mut ac_scrcpy::ServerParams,
) {
    let base = Rate { kbps: p.bitrate_kbps, fps: p.max_fps };
    *rate = Some((base, profile));
    let r = if cfg.video.adaptive_bitrate { a.rate(base) } else { base };
    params.bit_rate = r.kbps.saturating_mul(1000);
    params.max_fps = r.fps;
    io.set_reduced(r != base);
}

/// Shell command that reads (`None`) or sets "keep screen on". Prints `@on`
/// when it is on afterwards, and always ends with `@ok`.
fn stay_awake_cmd(set: Option<bool>) -> String {
    let f = STAY_AWAKE_FILE;
    match set {
        None => format!("[ -e {f} ] && echo @on; echo @ok"),
        // Save the user's timeout once, then never time out; wake the screen.
        Some(true) => format!(
            "[ -e {f} ] || settings get system screen_off_timeout > {f}; \
             settings put system screen_off_timeout {STAY_AWAKE_TIMEOUT} && input keyevent KEYCODE_WAKEUP && echo @on; echo @ok"
        ),
        // Restore it (Android's 1-minute default if the copy is missing).
        Some(false) => format!(
            "v=$(cat {f} 2>/dev/null); case \"$v\" in ''|*[!0-9]*) v=60000;; esac; \
             settings put system screen_off_timeout $v && rm -f {f}; [ -e {f} ] && echo @on; echo @ok"
        ),
    }
}

async fn connect(adb: &AdbClient, addr: &str) -> Result<(), String> {
    match adb.connect_device(addr).await {
        Ok(ConnectOutcome::Connected | ConnectOutcome::AlreadyConnected) => Ok(()),
        Ok(ConnectOutcome::Failed(m)) => Err(m),
        Err(e) => Err(e.to_string()),
    }
}

fn tile_change_needs_restart(running: u32, want: u32) -> bool {
    running > 0 && (want as f64 - running as f64).abs() / running as f64 > TILE_RESTART_RATIO
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

async fn identify(adb: &AdbClient, serial: &str, low_latency_wifi: bool, arp_table: &std::path::Path) -> SlotEvent {
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
        wifi_mac: match id.wifi_mac {
            Some(mac) => Some(mac),
            // Android 11+ hides the MAC from the shell; the host's neighbour
            // table knows it for a phone reached over Wi-Fi.
            None => match serial.parse::<SocketAddr>() {
                Ok(SocketAddr::V4(a)) => tokio::fs::read_to_string(arp_table)
                    .await
                    .ok()
                    .and_then(|t| ac_discovery::find_mac_by_ip(&t, *a.ip())),
                _ => None,
            },
        },
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

/// Heartbeat and telemetry over the helper link (no ADB), plus the slot's
/// requests. Ends (reporting `DirectGone`) when the link breaks.
async fn run_direct_link(
    mut link: ac_direct::Link,
    slot: SlotId,
    conn: u64,
    cfg: ac_config::Connection,
    tx: mpsc::UnboundedSender<Msg>,
    mut cmds: mpsc::UnboundedReceiver<LinkCmd>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(cfg.heartbeat_interval_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = Duration::from_millis(cfg.heartbeat_timeout_ms);
    let mut input = link.hello.input;
    let mut clip_seq = u64::MAX;
    let mut screen = None;
    loop {
        tokio::select! {
            _ = tick.tick() => match link.ping(deadline).await {
                Ok((pong, rtt)) => {
                    if pong.input != input {
                        input = pong.input;
                        let _ = tx.send(Msg::DirectInput { slot, conn, on: input });
                    }
                    if let Some(news) = pong.game.clone() {
                        let _ = tx.send(Msg::GameNews { slot, conn, news });
                    }
                    if pong.screen.is_some() && pong.screen != screen {
                        screen = pong.screen;
                        let _ = tx.send(Msg::DirectScreen { slot, conn, size: pong.screen.unwrap_or_default() });
                    }
                    let rtt_ms = rtt.as_millis() as u32;
                    let _ = tx.send(Msg::Slot(slot, SlotEvent::Heartbeat(Ok(rtt_ms))));
                    let stats = DeviceStats {
                        battery_pct: pong.battery,
                        charging: pong.charging,
                        wifi_rssi_dbm: pong.rssi,
                        wifi_link_mbps: pong.link_mbps,
                        rtt_ms: Some(rtt_ms),
                    };
                    if tx.send(Msg::Stats(slot, stats)).is_err() {
                        return;
                    }
                    if pong.clip_seq != clip_seq {
                        // The first answer only tells where the count is.
                        if clip_seq != u64::MAX
                            && let Some(text) = pong.clip.clone()
                        {
                            let _ = tx.send(Msg::DirectClip { slot, text });
                        }
                        clip_seq = pong.clip_seq;
                    }
                    // Helper 0.6+ measures itself; the manager keeps it only
                    // while the graphs or a test run want it.
                    if pong.mem_total_mb.is_some() {
                        let sample = PerfSample {
                            cpu_pct: pong.cpu_pct,
                            mem_used_mb: pong.mem_used_mb,
                            mem_total_mb: pong.mem_total_mb,
                            battery_temp_c: pong.battery_temp_c,
                            ..PerfSample::default()
                        };
                        let _ = tx.send(Msg::Perf { slot, sample });
                    }
                }
                Err(e) => {
                    // A late answer would desynchronize the link: start over.
                    tracing::info!(slot, "helper heartbeat failed: {e}");
                    let _ = tx.send(Msg::Slot(slot, SlotEvent::Heartbeat(Err(e.to_string()))));
                    let _ = tx.send(Msg::DirectGone { slot, conn });
                    return;
                }
            },
            cmd = cmds.recv() => match cmd {
                Some(LinkCmd::StayAwake { set, device_id }) => {
                    let on = match link.stay_awake(set, deadline * 2).await {
                        Ok(on) => Some(on),
                        Err(e) => {
                            tracing::warn!(slot, "keep screen on: {e}");
                            None
                        }
                    };
                    let _ = tx.send(Msg::StayAwake { slot, device_id, on });
                }
                Some(LinkCmd::App { action, reply }) => {
                    let name = match &action {
                        AppAction::Launch(_) => "launch",
                        AppAction::Stop(_) => "stop",
                        AppAction::ClearData(_) => "clear",
                        AppAction::Uninstall(_) => "uninstall",
                    };
                    // Uninstalling waits for the user to confirm on the phone.
                    let limit = if name == "uninstall" { Duration::from_secs(60) } else { deadline * 4 };
                    let (ok, message) = match link.app(name, action.package(), limit).await {
                        Ok(Ok(m)) => (true, m),
                        Ok(Err(m)) => (false, m),
                        Err(e) => {
                            // A timed-out request leaves an answer in flight.
                            match reply {
                                Some(r) => drop(r.send(Err(e.to_string()))),
                                None => drop(tx.send(Msg::Emit(UiEvent::AppResult { slot, kind: action.kind(), ok: false, message: e.to_string() }))),
                            }
                            let _ = tx.send(Msg::DirectGone { slot, conn });
                            return;
                        }
                    };
                    match reply {
                        Some(r) => drop(r.send(if ok { Ok(message) } else { Err(message) })),
                        None => drop(tx.send(Msg::Emit(UiEvent::AppResult { slot, kind: action.kind(), ok, message }))),
                    }
                }
                Some(LinkCmd::Packages { reply }) => match link.packages(deadline * 4).await {
                    Ok(packages) => match reply {
                        Some(r) => drop(r.send(Ok(packages))),
                        None => drop(tx.send(Msg::Emit(UiEvent::Packages { slot, packages }))),
                    },
                    Err(e) => {
                        tracing::warn!(slot, "listing apps: {e}");
                        if let Some(r) = reply {
                            let _ = r.send(Err(e.to_string()));
                        }
                        let _ = tx.send(Msg::DirectGone { slot, conn });
                        return;
                    }
                },
                Some(LinkCmd::Dev { req, reply }) => {
                    // Waiting on screen changes happens on the PC; each op is quick.
                    match link.dev(req, Duration::from_secs(15)).await {
                        Ok(v) => drop(reply.send(Ok(v))),
                        Err(e) => {
                            let _ = reply.send(Err(e.to_string()));
                            let _ = tx.send(Msg::DirectGone { slot, conn });
                            return;
                        }
                    }
                }
                None => return,
            },
        }
    }
}

/// Samples a phone's performance every `every` until aborted.
async fn sample_perf(
    adb: AdbClient,
    serial: String,
    shell_v2: bool,
    slot: SlotId,
    every: Duration,
    app: tokio::sync::watch::Receiver<Option<String>>,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut state = PerfState::default();
    let (mut top, mut layers) = (None, Vec::new());
    for n in 0u32.. {
        tick.tick().await;
        if n % LAYERS_EVERY == 0
            && let Ok(Ok(out)) = tokio::time::timeout(every * 2, adb.shell(&serial, LAYERS_CMD, shell_v2)).await
        {
            (top, layers) = parse_layers(&out.stdout);
        }
        // The test app's memory; without one, the app on screen's.
        let want = app.borrow().clone().or_else(|| top.clone());
        let cmd = sample_cmd(&layers, want.as_deref());
        let Ok(Ok(out)) = tokio::time::timeout(every * 2, adb.shell(&serial, &cmd, shell_v2)).await else { continue };
        let sample = parse_sample(&out.stdout, &mut state, want.as_deref());
        if tx.send(Msg::Perf { slot, sample }).is_err() {
            return;
        }
    }
}

/// Saves a full-resolution screenshot (PNG) from the phone.
async fn screencap(adb: &AdbClient, serial: &str, path: &std::path::Path) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let mut s = adb.open_service(serial, "exec:screencap -p").await.map_err(|e| e.to_string())?;
    let mut png = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut png))
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if !png.starts_with(b"\x89PNG") {
        let text = String::from_utf8_lossy(&png[..png.len().min(200)]).trim().to_owned();
        return Err(if text.is_empty() { "the phone sent no image".into() } else { text });
    }
    tokio::fs::write(path, &png).await.map_err(|e| e.to_string())
}

/// Appends to a test run file (best effort: a full disk must not stop the run).
fn append(path: &std::path::Path, text: &str) {
    use std::io::Write;
    let r = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(e) = r {
        tracing::warn!("writing {}: {e}", path.display());
    }
}

/// `dir/stem.ext`, or `dir/stem (2).ext` when that exists.
fn unique(dir: &std::path::Path, stem: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
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
    let transport = match sa.port() {
        p if p == legacy_port => TransportKind::Legacy,
        ac_direct::PORT => TransportKind::Direct,
        _ => TransportKind::Tls,
    };
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
        assert_eq!(normalize_addr("192.168.1.5:27190", 5555), Some(("192.168.1.5:27190".into(), TransportKind::Direct)));
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
    fn stay_awake_commands() {
        let f = "/data/local/tmp/android-controller-screen-timeout";
        assert_eq!(stay_awake_cmd(None), format!("[ -e {f} ] && echo @on; echo @ok"));
        assert_eq!(
            stay_awake_cmd(Some(true)),
            format!("[ -e {f} ] || settings get system screen_off_timeout > {f}; settings put system screen_off_timeout 2147483647 && input keyevent KEYCODE_WAKEUP && echo @on; echo @ok")
        );
        assert_eq!(
            stay_awake_cmd(Some(false)),
            format!("v=$(cat {f} 2>/dev/null); case \"$v\" in ''|*[!0-9]*) v=60000;; esac; settings put system screen_off_timeout $v && rm -f {f}; [ -e {f} ] && echo @on; echo @ok")
        );
    }

    #[test]
    fn tile_hysteresis() {
        assert!(!tile_change_needs_restart(1280, 1280));
        assert!(!tile_change_needs_restart(1280, 1000), "22 % smaller");
        assert!(tile_change_needs_restart(1280, 944), "26 % smaller");
        assert!(!tile_change_needs_restart(640, 800), "exactly 25 % larger");
        assert!(tile_change_needs_restart(640, 816));
        assert!(!tile_change_needs_restart(0, 640));
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
