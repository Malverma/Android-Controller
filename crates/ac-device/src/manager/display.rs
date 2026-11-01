//! Monitor mode's connections (spec2 §5): one control link to each online
//! phone's helper, and starting, pausing and stopping a phone's display. The
//! decisions are in [`Slot`]; this carries them out.

use std::any::Any;
use std::fmt;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

use ac_monitor::protocol::{self, FromPhone, Hello};
use ac_monitor::session::{self, SessionEvent, SessionParams};
use ac_monitor::touch::TouchEvent;
use ac_monitor::{hypr, output_name, pointer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::*;
use crate::slot::DisplayPhase;

const ACTIVITY: &str = "org.androidcontroller.keeper/.MonitorActivity";
/// After the phone drops, windows stay on its output this long (spec2 §5.6).
pub const OUTPUT_GRACE: Duration = Duration::from_secs(30);
/// How long a start waits for the helper to answer.
const HELPER_WAIT: Duration = Duration::from_secs(10);
const LINK_RETRY: Duration = Duration::from_secs(3);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const PING_EVERY: Duration = Duration::from_secs(15);
/// The lowest bitrate the rate control may step down to.
const MIN_BITRATE_KBPS: u32 = 2000;

/// A running picture: dropping it stops it.
pub type SessionGuard = Box<dyn Any + Send>;
pub type EventSink = Box<dyn Fn(SessionEvent) + Send>;

/// What monitor mode does to the desktop, so tests can leave the real one alone.
pub trait DisplayHost: fmt::Debug + Send + Sync + 'static {
    /// Why monitor mode cannot work on this PC.
    fn unavailable(&self) -> Option<String>;
    fn create(&self, output: &str, rule: &str) -> Result<(), String>;
    fn remove(&self, output: &str) -> Result<(), String>;
    fn remove_leftovers(&self);
    fn pointer(&self, output: &str) -> Result<std_mpsc::Sender<TouchEvent>, String>;
    /// Starts sending `output` over `video`. The flag asks for a keyframe.
    fn session(&self, params: SessionParams, video: std::net::TcpStream, on_event: EventSink) -> (SessionGuard, Arc<AtomicBool>);
}

/// The real thing: Hyprland, screen capture and the virtual pointer.
#[derive(Debug, Default)]
pub struct RealHost;

impl DisplayHost for RealHost {
    fn unavailable(&self) -> Option<String> {
        (!hypr::available()).then(|| "Using a phone as a monitor needs a Hyprland session".to_owned())
    }

    fn create(&self, output: &str, rule: &str) -> Result<(), String> {
        hypr::create(output, rule).map_err(|e| format!("{e:#}"))
    }

    fn remove(&self, output: &str) -> Result<(), String> {
        hypr::remove(output).map_err(|e| format!("{e:#}"))
    }

    fn remove_leftovers(&self) {
        if self.unavailable().is_none() {
            match hypr::remove_leftovers() {
                Ok(0) => {}
                Ok(n) => tracing::info!("removed {n} monitor output(s) left by an earlier run"),
                Err(e) => tracing::warn!("removing leftover monitor outputs: {e:#}"),
            }
        }
    }

    fn pointer(&self, output: &str) -> Result<std_mpsc::Sender<TouchEvent>, String> {
        pointer::spawn(output).map_err(|e| format!("{e:#}"))
    }

    fn session(&self, params: SessionParams, video: std::net::TcpStream, on_event: EventSink) -> (SessionGuard, Arc<AtomicBool>) {
        let handle = session::spawn(params, video, on_event);
        let flag = handle.keyframe_flag();
        (Box::new(handle), flag)
    }
}

pub(super) enum DisplayMsg {
    Hello { slot: SlotId, conn: u64, hello: Hello, port: u16 },
    Request { slot: SlotId, conn: u64, on: bool },
    LinkClosed { slot: SlotId, conn: u64 },
    /// Sent before the picture starts, so no touch after it is lost.
    PointerReady { slot: SlotId, id: u64, pointer: std_mpsc::Sender<TouchEvent> },
    Started { slot: SlotId, id: u64, guard: SessionGuard, encoder_flag: Arc<AtomicBool> },
    StartFailed { slot: SlotId, id: u64, error: String, retry: bool },
    Session { slot: SlotId, id: u64, event: SessionEvent },
    HelperDeadline { slot: SlotId, id: u64 },
    GraceOver { slot: SlotId, seq: u64 },
    Confirmed { slot: SlotId },
}

/// What the link's reader hands straight to the running display, without
/// waiting for the manager's task: touches must not queue behind other work.
#[derive(Default)]
struct Direct {
    touch: StdMutex<Option<std_mpsc::Sender<TouchEvent>>>,
    keyframe: StdMutex<Option<Arc<AtomicBool>>>,
}

impl Direct {
    fn clear(&self) {
        *self.touch.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.keyframe.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

struct Link {
    conn: u64,
    serial: String,
    tx: mpsc::UnboundedSender<Value>,
    task: JoinHandle<()>,
    /// The helper's greeting, and the local port forwarded to it.
    ready: Option<(Hello, u16)>,
}

#[derive(Debug, Clone, Copy)]
struct Live {
    fps: u32,
    kbps: u32,
    encoder: &'static str,
}

pub(super) struct Displays {
    host: Option<Arc<dyn DisplayHost>>,
    grace_for: Duration,
    links: Vec<Option<Link>>,
    direct: Vec<Arc<Direct>>,
    sessions: Vec<Option<SessionGuard>>,
    /// A start waiting for the helper's greeting.
    waiting: Vec<Option<(u64, String, bool)>>,
    helper: Vec<Option<u32>>,
    live: Vec<Option<Live>>,
    grace: Vec<Option<(u64, JoinHandle<()>)>>,
    /// Starting and stopping one slot's display happen in order.
    ops: Vec<Arc<tokio::sync::Mutex<()>>>,
    /// Outputs this run created and has not removed.
    outputs: Vec<bool>,
    seq: u64,
}

impl Displays {
    pub(super) fn new(host: Option<Arc<dyn DisplayHost>>, grace_for: Duration, n: usize) -> Self {
        Self {
            host,
            grace_for,
            links: (0..n).map(|_| None).collect(),
            direct: (0..n).map(|_| Arc::default()).collect(),
            sessions: (0..n).map(|_| None).collect(),
            waiting: vec![None; n],
            helper: vec![None; n],
            live: vec![None; n],
            grace: (0..n).map(|_| None).collect(),
            ops: (0..n).map(|_| Arc::default()).collect(),
            outputs: vec![false; n],
            seq: 0,
        }
    }

    pub(super) fn remove_leftovers(&self) {
        if let Some(host) = self.host.clone() {
            tokio::task::spawn_blocking(move || host.remove_leftovers());
        }
    }

    pub(super) fn set_helper_version(&mut self, i: usize, version: u32) {
        self.helper[i] = Some(version);
    }

    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn abort_grace(&mut self, i: usize) {
        if let Some((_, task)) = self.grace[i].take() {
            task.abort();
        }
    }
}

impl Drop for Displays {
    /// Do not leave invisible monitors behind when the app closes.
    fn drop(&mut self) {
        let Some(host) = self.host.clone() else { return };
        for (i, present) in self.outputs.iter().enumerate() {
            if *present {
                self.sessions[i] = None;
                let _ = host.remove(&output_name(i as SlotId + 1));
            }
        }
    }
}

async fn read_message(r: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let mut body = vec![0u8; protocol::body_len(len)?];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// One control connection, from greeting to its end.
async fn run_link(
    addr: SocketAddr,
    slot: SlotId,
    conn: u64,
    port: u16,
    commands: &mut mpsc::UnboundedReceiver<Value>,
    direct: &Direct,
    tx: &mpsc::UnboundedSender<Msg>,
) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    stream.write_all(&protocol::encode_json(&json!({ "role": "control" }))).await?;
    let first = tokio::time::timeout(HELLO_TIMEOUT, read_message(&mut stream))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no greeting from the helper"))??;
    let FromPhone::Hello(hello) = protocol::parse_message(&first) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected a greeting"));
    };
    let _ = tx.send(Msg::Display(DisplayMsg::Hello { slot, conn, hello, port }));

    let (mut reader, mut writer) = stream.into_split();
    // Reading whole messages is not cancel-safe, so it gets its own task.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    let read_task = tokio::spawn(async move {
        loop {
            let message = read_message(&mut reader).await;
            let end = message.is_err();
            if in_tx.send(message).is_err() || end {
                return;
            }
        }
    });
    let mut ping = tokio::time::interval(PING_EVERY);
    let result = loop {
        tokio::select! {
            message = in_rx.recv() => match message {
                Some(Ok(v)) => match protocol::parse_message(&v) {
                    FromPhone::Touch(t) => {
                        if let Some(s) = direct.touch.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                            let _ = s.send(t);
                        }
                    }
                    FromPhone::Keyframe => {
                        if let Some(k) = direct.keyframe.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                            k.store(true, Ordering::Relaxed);
                        }
                    }
                    FromPhone::Request { on } => {
                        let _ = tx.send(Msg::Display(DisplayMsg::Request { slot, conn, on }));
                    }
                    _ => {}
                },
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
            command = commands.recv() => match command {
                Some(v) => {
                    if let Err(e) = writer.write_all(&protocol::encode_json(&v)).await {
                        break Err(e);
                    }
                }
                None => break Ok(()),
            },
            _ = ping.tick() => {
                if let Err(e) = writer.write_all(&protocol::encode_json(&json!({ "op": "ping" }))).await {
                    break Err(e);
                }
            }
        }
    };
    read_task.abort();
    result
}

impl Manager {
    /// Keeps a control link to each online phone whose helper has monitor mode.
    pub(super) fn reconcile_display_links(&mut self) {
        if self.displays.host.is_none() {
            return;
        }
        for i in 0..self.slots.len() {
            let s = &self.slots[i];
            let online = matches!(s.phase, Phase::Online { .. });
            if !online {
                self.displays.helper[i] = None;
            }
            let wanted = (online && s.transport != TransportKind::Direct)
                .then(|| s.adb_serial.clone())
                .flatten()
                .filter(|_| self.displays.helper[i].is_some_and(|v| v >= protocol::MIN_HELPER_VERSION));
            let current = self.displays.links[i].as_ref().map(|l| l.serial.clone());
            if wanted == current {
                continue;
            }
            if let Some(link) = self.displays.links[i].take() {
                link.task.abort();
                let (adb, serial) = (self.adb.clone(), link.serial);
                if let Some((_, port)) = link.ready {
                    tokio::spawn(async move {
                        let _ = adb.kill_forward(&serial, &format!("tcp:{port}")).await;
                    });
                }
            }
            if let Some(serial) = wanted {
                self.open_display_link(i, serial);
            }
        }
    }

    fn open_display_link(&mut self, i: usize, serial: String) {
        let conn = self.displays.next();
        let slot = self.slots[i].id;
        let (tx, mut commands) = mpsc::unbounded_channel();
        let (adb, to_manager, direct) = (self.adb.clone(), self.tx.clone(), self.displays.direct[i].clone());
        let forward_serial = serial.clone();
        let task = tokio::spawn(async move {
            loop {
                match adb.forward(&forward_serial, "tcp:0", protocol::SOCKET).await {
                    Ok(Some(port)) => {
                        let addr = SocketAddr::new(adb.server().ip(), port);
                        let result = run_link(addr, slot, conn, port, &mut commands, &direct, &to_manager).await;
                        tracing::debug!(slot, "monitor link ended: {result:?}");
                        let _ = to_manager.send(Msg::Display(DisplayMsg::LinkClosed { slot, conn }));
                        let _ = adb.kill_forward(&forward_serial, &format!("tcp:{port}")).await;
                    }
                    Ok(None) => tracing::warn!(slot, "adb gave no port for the monitor link"),
                    Err(e) => tracing::debug!(slot, "monitor link: {e}"),
                }
                tokio::time::sleep(LINK_RETRY).await;
            }
        });
        self.displays.links[i] = Some(Link { conn, serial, tx, task, ready: None });
    }

    pub(super) fn display_view_only(&self, i: usize) -> bool {
        let s = &self.slots[i];
        let saved = s.device_id().and_then(|id| self.devices.get(id)).and_then(|r| r.monitor_view_only);
        // Touches drive the PC, so an unencrypted connection needs a clear yes.
        saved.unwrap_or(s.transport == TransportKind::Legacy)
    }

    pub(super) fn display_view(&self, i: usize) -> DisplayView {
        match &self.slots[i].display {
            DisplayPhase::Off => DisplayView::Off,
            DisplayPhase::Starting { .. } => DisplayView::Starting,
            DisplayPhase::Live { width, height } => {
                let live = self.displays.live[i];
                DisplayView::Live {
                    width: *width,
                    height: *height,
                    fps: live.map_or(0, |l| l.fps),
                    kbps: live.map_or(0, |l| l.kbps),
                    encoder: live.map_or("", |l| l.encoder).to_owned(),
                }
            }
            DisplayPhase::Paused => DisplayView::Waiting,
            DisplayPhase::Failed { error } => DisplayView::Failed(error.clone()),
        }
    }

    /// Why this phone cannot become a monitor now. `None` also while it is one.
    pub(super) fn display_blocked(&self, i: usize) -> Option<String> {
        let s = &self.slots[i];
        if s.display_desired {
            return None;
        }
        let Some(host) = &self.displays.host else { return Some("Monitor mode is not available in this build".into()) };
        if let Some(reason) = host.unavailable() {
            return Some(reason);
        }
        if s.transport == TransportKind::Direct {
            return Some("Monitor mode needs wireless debugging on this phone: its connection without ADB is not encrypted".into());
        }
        if !matches!(s.phase, Phase::Online { .. }) {
            return Some("The phone is not connected".into());
        }
        match self.displays.helper[i] {
            None => return Some("Wireless Debugging Keeper 0.8 or newer is needed on this phone".into()),
            Some(v) if v < protocol::MIN_HELPER_VERSION => {
                return Some(format!("This phone has Wireless Debugging Keeper {}; monitor mode needs 0.8 or newer", helper_version_name(v)));
            }
            Some(_) => {}
        }
        if let Some(Link { ready: Some((hello, _)), .. }) = &self.displays.links[i]
            && !hello.avc
        {
            return Some("This phone has no H.264 decoder".into());
        }
        let max = self.cfg.monitor.max_concurrent as usize;
        if self.slots.iter().filter(|o| o.display_desired).count() >= max {
            return Some(format!("{max} phones are already monitors. The limit is monitor.max_concurrent in the settings file"));
        }
        None
    }

    /// `ask`: a test run on the phone is hidden by this, so check with the user.
    pub(super) fn request_display(&mut self, slot: SlotId, on: bool, ask: bool) {
        let Some(i) = self.idx(slot) else { return };
        if on && !self.slots[i].display_desired {
            if let Some(reason) = self.display_blocked(i) {
                return self.toast(Some(ErrorCode::MonitorFailed), reason);
            }
            if ask && self.run.is_some() {
                let title = self.title(i);
                let answer = self.confirm(
                    format!("Use {title} as a monitor during this test run?"),
                    "The PC's desktop will cover the app being tested. Its log and crash watch keep running.".into(),
                );
                return self.spawn_msg(async move {
                    if answer.await {
                        Msg::Display(DisplayMsg::Confirmed { slot })
                    } else {
                        Msg::Prune
                    }
                });
            }
        }
        self.feed(slot, SlotEvent::SetDisplay { on });
    }

    pub(super) fn set_display_options(&mut self, slot: SlotId, view_only: Option<bool>, portrait: Option<bool>) {
        let Some(i) = self.idx(slot) else { return };
        let Some(id) = self.slots[i].device_id().map(str::to_owned) else { return };
        let Some(rec) = self.devices.get_mut(&id) else { return };
        if let Some(v) = view_only {
            rec.monitor_view_only = Some(v);
        }
        if let Some(p) = portrait {
            rec.monitor_portrait = p;
        }
        self.save_devices();
        // Stop obeying touches at once, before anything else happens.
        if view_only == Some(true) {
            *self.displays.direct[i].touch.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.feed(slot, SlotEvent::RestartDisplay);
    }

    pub(super) fn start_display(&mut self, i: usize, id: u64, serial: String, shell_v2: bool) {
        let slot = self.slots[i].id;
        self.displays.abort_grace(i);
        self.displays.live[i] = None;
        if let Some(old) = self.displays.sessions[i].take() {
            self.displays.direct[i].clear();
            let lock = self.displays.ops[i].clone();
            tokio::spawn(async move {
                let _held = lock.lock_owned().await;
                let _ = tokio::task::spawn_blocking(move || drop(old)).await;
            });
        }
        if self.displays.links[i].as_ref().is_some_and(|l| l.ready.is_some()) {
            return self.launch_display(i, id, serial, shell_v2);
        }
        // The link opens once the phone is online and its helper checked.
        self.displays.waiting[i] = Some((id, serial, shell_v2));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(HELPER_WAIT).await;
            let _ = tx.send(Msg::Display(DisplayMsg::HelperDeadline { slot, id }));
        });
    }

    fn launch_display(&mut self, i: usize, id: u64, serial: String, shell_v2: bool) {
        let slot = self.slots[i].id;
        let Some(host) = self.displays.host.clone() else { return };
        let Some(link) = &self.displays.links[i] else { return };
        let Some((hello, port)) = link.ready else { return };
        let link_tx = link.tx.clone();
        if !hello.avc {
            let _ = self.tx.send(Msg::Slot(slot, SlotEvent::DisplayFailed { id, error: "This phone has no H.264 decoder".into() }));
            return;
        }
        let m = self.cfg.monitor.clone();
        let portrait = self.slots[i].device_id().and_then(|d| self.devices.get(d)).is_some_and(|r| r.monitor_portrait);
        let view_only = self.display_view_only(i);
        let (w, h) = hypr::phone_mode(hello.width, hello.height, portrait, m.max_size);
        let output = output_name(slot);
        let rule = hypr::monitor_rule(&output, w, h, m.fps, &m.position, m.scale);
        let params = SessionParams {
            output: output.clone(),
            fps: m.fps,
            bitrate_kbps: m.bitrate_mbps * 1000,
            min_bitrate_kbps: MIN_BITRATE_KBPS,
            encoder: m.encoder,
            render_node: m.render_node,
        };
        self.displays.outputs[i] = true;
        let (adb, tx, lock) = (self.adb.clone(), self.tx.clone(), self.displays.ops[i].clone());
        let video_addr = SocketAddr::new(adb.server().ip(), port);
        tokio::spawn(async move {
            let _held = lock.lock_owned().await;
            let fail = |error: String, retry: bool| Msg::Display(DisplayMsg::StartFailed { slot, id, error, retry });
            let (h2, name) = (host.clone(), output.clone());
            let created = tokio::task::spawn_blocking(move || h2.create(&name, &rule)).await;
            match created {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return drop(tx.send(fail(format!("Hyprland refused the new display: {e}"), false))),
                Err(e) => return drop(tx.send(fail(e.to_string(), false))),
            }
            let _ = link_tx.send(json!({ "op": "start", "portrait": portrait, "view_only": view_only }));
            // Over ADB because Android lets the shell open a screen at any time,
            // while the helper may not while it is in the background. The manifest
            // limits this screen to the shell through the DUMP permission.
            let shown = adb.shell(&serial, &format!("am start -n {ACTIVITY}"), shell_v2).await;
            match &shown {
                Ok(o) if [&o.stdout, &o.stderr].iter().any(|t| t.contains("Error") || t.contains("xception")) => {
                    return drop(tx.send(fail(format!("The phone would not open its monitor screen: {}{}", o.stdout, o.stderr), false)));
                }
                Err(e) => return drop(tx.send(fail(format!("Could not reach the phone: {e}"), true))),
                Ok(_) => {}
            }
            let video = async {
                let mut s = TcpStream::connect(video_addr).await?;
                s.set_nodelay(true)?;
                s.write_all(&protocol::encode_json(&json!({ "role": "video", "w": w, "h": h }))).await?;
                let s = s.into_std()?;
                s.set_nonblocking(false)?;
                std::io::Result::Ok(s)
            };
            let video = match video.await {
                Ok(v) => v,
                Err(e) => return drop(tx.send(fail(format!("Could not open the video connection: {e}"), true))),
            };
            let (h2, name) = (host.clone(), output.clone());
            if !view_only {
                match tokio::task::spawn_blocking(move || h2.pointer(&name)).await {
                    Ok(Ok(pointer)) => {
                        let _ = tx.send(Msg::Display(DisplayMsg::PointerReady { slot, id, pointer }));
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(slot, "no pointer for the monitor: {e}");
                        let _ = tx.send(Msg::Toast(None, format!("Touch will not control the PC: {e}")));
                    }
                    Err(e) => tracing::warn!(slot, "no pointer for the monitor: {e}"),
                }
            }
            let events = tx.clone();
            let (guard, encoder_flag) = host.session(
                params,
                video,
                Box::new(move |event| {
                    let _ = events.send(Msg::Display(DisplayMsg::Session { slot, id, event }));
                }),
            );
            let _ = tx.send(Msg::Display(DisplayMsg::Started { slot, id, guard, encoder_flag }));
        });
    }

    /// Ends the picture and input; the output is removed unless `keep_output`.
    fn end_display(&mut self, i: usize, keep_output: bool) {
        let d = &mut self.displays;
        d.waiting[i] = None;
        d.live[i] = None;
        d.direct[i].clear();
        let session = d.sessions[i].take();
        let (host, lock) = (d.host.clone(), d.ops[i].clone());
        let output = output_name(i as SlotId + 1);
        if !keep_output {
            d.abort_grace(i);
            d.outputs[i] = false;
        }
        tokio::spawn(async move {
            let _held = lock.lock_owned().await;
            let _ = tokio::task::spawn_blocking(move || {
                drop(session);
                if !keep_output
                    && let Some(host) = host
                    && let Err(e) = host.remove(&output)
                {
                    tracing::warn!("removing {output}: {e}");
                }
            })
            .await;
        });
    }

    pub(super) fn stop_display(&mut self, i: usize) {
        if let Some(link) = &self.displays.links[i] {
            let _ = link.tx.send(json!({ "op": "stop" }));
        }
        self.end_display(i, false);
    }

    pub(super) fn pause_display(&mut self, i: usize) {
        self.end_display(i, true);
        let slot = self.slots[i].id;
        let seq = self.displays.next();
        let (tx, grace_for) = (self.tx.clone(), self.displays.grace_for);
        let task = tokio::spawn(async move {
            tokio::time::sleep(grace_for).await;
            let _ = tx.send(Msg::Display(DisplayMsg::GraceOver { slot, seq }));
        });
        self.displays.abort_grace(i);
        self.displays.grace[i] = Some((seq, task));
    }

    /// Is `id` the display this slot wants running? (The picture may be live
    /// before the task that started it has reported back.)
    fn display_current(&self, i: usize, id: u64) -> bool {
        let s = &self.slots[i];
        s.display_id() == id && matches!(s.display, DisplayPhase::Starting { .. } | DisplayPhase::Live { .. }) && matches!(s.phase, Phase::Online { .. })
    }

    pub(super) fn on_display_msg(&mut self, msg: DisplayMsg) {
        use DisplayMsg as M;
        match msg {
            M::Hello { slot, conn, hello, port } => {
                let Some(i) = self.idx(slot) else { return };
                let Some(link) = self.displays.links[i].as_mut().filter(|l| l.conn == conn) else { return };
                link.ready = Some((hello, port));
                self.displays.helper[i] = Some(hello.version);
                if let Some((id, serial, shell_v2)) = self.displays.waiting[i].take() {
                    self.launch_display(i, id, serial, shell_v2);
                } else if hello.requested {
                    self.request_display(slot, true, true);
                }
            }
            M::Request { slot, conn, on } => {
                let Some(i) = self.idx(slot) else { return };
                if self.displays.links[i].as_ref().is_some_and(|l| l.conn == conn) {
                    self.request_display(slot, on, true);
                }
            }
            M::LinkClosed { slot, conn } => {
                let Some(i) = self.idx(slot) else { return };
                if let Some(link) = self.displays.links[i].as_mut().filter(|l| l.conn == conn) {
                    link.ready = None;
                }
            }
            M::PointerReady { slot, id, pointer } => {
                let Some(i) = self.idx(slot) else { return };
                // Dropping it otherwise ends the pointer's thread.
                if self.display_current(i, id) && !self.display_view_only(i) {
                    *self.displays.direct[i].touch.lock().unwrap_or_else(|e| e.into_inner()) = Some(pointer);
                }
            }
            M::Started { slot, id, guard, encoder_flag } => {
                let Some(i) = self.idx(slot) else { return };
                if !self.display_current(i, id) {
                    // It was stopped or replaced while starting.
                    tokio::task::spawn_blocking(move || drop(guard));
                    return;
                }
                *self.displays.direct[i].keyframe.lock().unwrap_or_else(|e| e.into_inner()) = Some(encoder_flag);
                self.displays.sessions[i] = Some(guard);
            }
            M::StartFailed { slot, id, error, retry } => {
                let event = if retry { SlotEvent::DisplayEnded { id, error } } else { SlotEvent::DisplayFailed { id, error } };
                self.feed(slot, event);
            }
            M::Session { slot, id, event } => {
                let Some(i) = self.idx(slot) else { return };
                if self.slots[i].display_id() != id {
                    return;
                }
                match event {
                    SessionEvent::Live { width, height, encoder } => {
                        self.displays.live[i] = Some(Live { fps: 0, kbps: 0, encoder: encoder.label() });
                        if !encoder.hardware() {
                            self.toast(
                                None,
                                "Encoding on the CPU because no hardware encoder works here. On Intel graphics, install intel-media-driver.",
                            );
                        }
                        self.feed(slot, SlotEvent::DisplayLive { id, width, height });
                    }
                    SessionEvent::Stats { fps, kbps, .. } => {
                        if let Some(l) = &mut self.displays.live[i] {
                            l.fps = fps;
                            l.kbps = kbps;
                        }
                    }
                    SessionEvent::Ended { error } => {
                        let error = if error.is_empty() { "the picture stopped".to_owned() } else { error };
                        self.feed(slot, SlotEvent::DisplayEnded { id, error });
                    }
                }
            }
            M::HelperDeadline { slot, id } => {
                let Some(i) = self.idx(slot) else { return };
                if self.displays.waiting[i].as_ref().is_some_and(|(w, _, _)| *w == id) {
                    self.displays.waiting[i] = None;
                    let error = "Wireless Debugging Keeper did not answer. Open it once on the phone, then try again.".to_owned();
                    self.feed(slot, SlotEvent::DisplayFailed { id, error });
                }
            }
            M::GraceOver { slot, seq } => {
                let Some(i) = self.idx(slot) else { return };
                if self.displays.grace[i].as_ref().is_some_and(|(s, _)| *s == seq) {
                    self.displays.grace[i] = None;
                    // The phone stays wanted as a monitor: returning recreates this.
                    self.end_display(i, false);
                }
            }
            M::Confirmed { slot } => self.feed(slot, SlotEvent::SetDisplay { on: true }),
        }
    }
}
