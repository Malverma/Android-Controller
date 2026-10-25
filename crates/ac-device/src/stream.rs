//! One mirroring session: launch scrcpy-server, run the decode thread and the
//! control channel, and report how it ended (spec §3.3.3, §4).

use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ac_adb::AdbClient;
use ac_config::{Config, DeviceRecord, MaxSize, Profile};
use ac_core::SlotId;
use ac_media::{Backend, Mailbox, MediaContext, StreamEvents};
use ac_scrcpy::{ControlMsg, DeviceMsg, ServerParams, VideoCodec};
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::slot::{SlotEvent, StreamProfile};

/// Per-slot I/O shared with the UI: the frame mailbox and a direct line to
/// the device's control socket. The UI sends input here without going
/// through the manager actor.
#[derive(Default)]
pub struct SlotIo {
    pub mailbox: Mailbox,
    control: Mutex<Option<mpsc::UnboundedSender<ControlMsg>>>,
    backend: Mutex<Option<Backend>>,
    info: Mutex<Option<StreamInfo>>,
    /// The adaptive bitrate controller lowered this stream (spec §3.6).
    reduced: std::sync::atomic::AtomicBool,
}

/// What the running session asked the phone for (shown in the status strip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub codec: VideoCodec,
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
}

impl SlotIo {
    /// Queues a control message; false when no stream is running.
    pub fn send(&self, msg: ControlMsg) -> bool {
        self.control.lock().unwrap().as_ref().is_some_and(|tx| tx.send(msg).is_ok())
    }

    pub fn is_streaming(&self) -> bool {
        self.control.lock().unwrap().is_some()
    }

    /// Attaches a control channel as a running stream would (for tests of
    /// input routing); the receiver gets what is sent.
    #[doc(hidden)]
    pub fn attach_control_for_test(&self) -> mpsc::UnboundedReceiver<ControlMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.control.lock().unwrap() = Some(tx);
        rx
    }

    /// Backend decoding this slot's stream, while it runs.
    pub fn backend(&self) -> Option<Backend> {
        *self.backend.lock().unwrap()
    }

    /// Parameters of the running session.
    pub fn info(&self) -> Option<StreamInfo> {
        *self.info.lock().unwrap()
    }

    /// The stream runs below its profile because the Wi-Fi lagged.
    pub fn reduced(&self) -> bool {
        self.reduced.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_reduced(&self, on: bool) {
        self.reduced.store(on, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a session reports back to the manager.
pub enum SessionMsg {
    Slot(SlotId, SlotEvent),
    DeviceClipboard(SlotId, String),
    /// Something to tell the user that does not end the session.
    Notice(SlotId, String),
}

/// Everything a session needs; cheap to clone per launch.
#[derive(Clone)]
pub struct StreamEnv {
    pub adb: AdbClient,
    pub server_jar: PathBuf,
    pub server_version: String,
    pub media: Arc<MediaContext>,
}

/// The `[profiles.*]` table a slot runs, with the device's
/// `profile_overrides` for it applied (spec §7.4). An override that does not
/// fit the schema is reported and ignored.
pub fn resolve_profile(cfg: &Config, profile: StreamProfile, record: Option<&DeviceRecord>) -> (Profile, Option<String>) {
    let name = match profile {
        StreamProfile::Grid => &cfg.video.grid_profile,
        StreamProfile::Focus => &cfg.video.focus_profile,
    };
    let base = cfg.profiles.get(name).cloned().unwrap_or_default();
    match record.map(|r| r.profile(name, &base)) {
        None => (base, None),
        Some(Ok(p)) => (p, None),
        Some(Err(e)) => {
            let who = record.map(|r| r.display_name().to_owned()).unwrap_or_default();
            (base, Some(format!("Ignoring {who}'s profile_overrides.{name}: {}", e.message())))
        }
    }
}

/// `max_size` for a profile: fixed, or for `"auto"` the grid tile's longest
/// side in device pixels, rounded up to 16 and clamped (spec §4.3).
pub fn max_size_for(p: &Profile, tile_px: Option<u32>) -> u32 {
    match p.max_size {
        MaxSize::Px(px) => px,
        MaxSize::Auto(_) => {
            let px = tile_px.unwrap_or(p.max_size_max);
            px.div_ceil(16).saturating_mul(16).clamp(p.max_size_min, p.max_size_max)
        }
    }
}

/// The codec for a profile's `codec` setting: `*-if-supported` only when the
/// phone has a hardware encoder for it (devices.toml `supported_codecs`) and
/// this PC's VA-API decodes it (spec §4.3); H.264 otherwise.
pub fn pick_codec(want: &str, device: &[String], host: &ac_media::ProbeResult) -> VideoCodec {
    let has = |c: &str| device.iter().any(|d| d == c);
    match want {
        "h265" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        "h265-if-supported" if has("h265") && host.hevc => VideoCodec::H265,
        "av1-if-supported" if has("av1") && host.av1 => VideoCodec::Av1,
        _ => VideoCodec::H264,
    }
}

/// Launch parameters for a resolved profile (spec §4.3).
pub fn server_params(cfg: &Config, p: &Profile, tile_px: Option<u32>, bitrate_kbps: Option<u32>) -> ServerParams {
    let max_size = max_size_for(p, tile_px);
    let codec = match p.codec.as_str() {
        "h265" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        // "*-if-supported" depends on the phone and this PC: see `pick_codec`.
        _ => VideoCodec::H264,
    };
    ServerParams {
        scid: rand::rng().random::<u32>() & 0x7FFF_FFFF,
        codec,
        bit_rate: bitrate_kbps.unwrap_or(p.bitrate_kbps).saturating_mul(1000),
        max_size,
        max_fps: p.max_fps,
        codec_options: p.codec_options.clone(),
        control: true,
        clipboard_autosync: cfg.input.clipboard_autosync,
        stay_awake: cfg.connection.stay_awake,
        show_touches: cfg.connection.show_touches,
        power_on: true,
        audio: p.audio,
    }
}

/// A running session task; dropping `stop` (or sending on it) ends it.
pub struct SessionHandle {
    pub stop: Option<oneshot::Sender<()>>,
}

impl SessionHandle {
    pub fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

struct DecodeEvents {
    slot: SlotId,
    id: u64,
    io: Arc<SlotIo>,
    tx: mpsc::UnboundedSender<SessionMsg>,
}

impl StreamEvents for DecodeEvents {
    fn request_key_frame(&mut self) {
        self.io.send(ControlMsg::ResetVideo);
    }

    fn backend(&mut self, backend: Backend) {
        *self.io.backend.lock().unwrap() = Some(backend);
        tracing::info!(slot = self.slot, "decoding with {backend}");
    }

    fn session(&mut self, width: u32, height: u32) {
        let _ = self.tx.send(SessionMsg::Slot(self.slot, SlotEvent::StreamLive { id: self.id, width, height }));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_session(
    env: StreamEnv,
    serial: String,
    shell_v2: bool,
    params: ServerParams,
    audio: ac_audio::Options,
    slot: SlotId,
    id: u64,
    io: Arc<SlotIo>,
    tx: mpsc::UnboundedSender<SessionMsg>,
) -> SessionHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let error = run_session(&env, &serial, shell_v2, &params, audio, slot, id, &io, &tx, stop_rx).await;
        let _ = tx.send(SessionMsg::Slot(slot, SlotEvent::StreamEnded { id, error }));
    });
    SessionHandle { stop: Some(stop_tx) }
}

/// Returns `None` when stopped on purpose, or the reason it ended.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    env: &StreamEnv,
    serial: &str,
    shell_v2: bool,
    params: &ServerParams,
    audio_opts: ac_audio::Options,
    slot: SlotId,
    id: u64,
    io: &Arc<SlotIo>,
    tx: &mpsc::UnboundedSender<SessionMsg>,
    mut stop: oneshot::Receiver<()>,
) -> Option<String> {
    tracing::info!(slot, max_size = params.max_size, bit_rate = params.bit_rate, max_fps = params.max_fps, "starting mirroring");
    let launch = ac_scrcpy::launch(&env.adb, serial, &env.server_jar, &env.server_version, params, shell_v2);
    let launched = tokio::select! {
        r = launch => r,
        _ = &mut stop => return None,
    };
    let mut launched = match launched {
        Ok(l) => l,
        Err(e) => {
            let detail = if e.server_output.is_empty() { String::new() } else { format!("\n{}", e.server_output) };
            tracing::warn!(slot, "mirroring failed to start: {e}{detail}");
            return Some(e.message);
        }
    };
    tracing::info!(slot, device = %launched.device_name, "mirroring started");

    // Video: a blocking socket read by a dedicated thread. Shutting the
    // socket down from here unblocks it.
    let video = match launched.video.into_std().and_then(|s| s.set_nonblocking(false).map(|()| s)) {
        Ok(s) => s,
        Err(e) => return Some(format!("video socket: {e}")),
    };
    let video_shutdown = video.try_clone().ok();
    let (done_tx, done_rx) = oneshot::channel();
    let decode = {
        let (io, media) = (io.clone(), env.media.clone());
        let mut events = DecodeEvents { slot, id, io: io.clone(), tx: tx.clone() };
        std::thread::Builder::new().name(format!("video-decode-{slot}")).spawn(move || {
            let r = ac_media::stream::run(video, &io.mailbox, &media, &mut events);
            let _ = done_tx.send(r);
        })
    };
    if let Err(e) = decode {
        return Some(format!("decode thread: {e}"));
    }

    // Audio (focus view): played on its own thread; a failure there is
    // reported but leaves the mirror running.
    let audio_shutdown = launched.audio.take().and_then(|a| {
        let a = a.into_std().and_then(|s| s.set_nonblocking(false).map(|()| s)).ok()?;
        let shutdown = a.try_clone().ok()?;
        let tx = tx.clone();
        let spawned = std::thread::Builder::new().name(format!("audio-{slot}")).spawn(move || {
            let player = audio_opts.command();
            match ac_audio::run(a, &audio_opts, player) {
                Ok(ac_audio::Ended::Closed) => {}
                Ok(ac_audio::Ended::Unsupported) => {
                    let _ = tx.send(SessionMsg::Notice(slot, format!("{}: this phone can't share its audio (needs Android 11 or newer)", audio_opts.description)));
                }
                Err(e) => {
                    tracing::warn!(slot, "audio: {e}");
                    let _ = tx.send(SessionMsg::Notice(slot, format!("{}: no audio — {e}", audio_opts.description)));
                }
            }
        });
        spawned.ok().map(|_| shutdown)
    });

    // Control: writer task fed by `SlotIo::send`, reader task for clipboard.
    let (read_half, write_half) = launched.control.into_split();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    *io.control.lock().unwrap() = Some(ctl_tx);
    *io.info.lock().unwrap() = Some(StreamInfo {
        codec: params.codec,
        max_size: params.max_size,
        bit_rate: params.bit_rate,
        max_fps: params.max_fps,
    });
    let mut writer = tokio::spawn(control_writer(write_half, ctl_rx));
    let mut reader = tokio::spawn(control_reader(read_half, slot, tx.clone()));

    let ended = tokio::select! {
        _ = &mut stop => None,
        r = done_rx => Some(match r {
            Ok(Ok(())) => "the video stream ended".to_owned(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "the decode thread stopped".to_owned(),
        }),
        _ = launched.server.wait() => Some(server_exit_reason(&launched.server.output_tail())),
        r = &mut reader => Some(r.ok().flatten().unwrap_or_else(|| "the control channel closed".into())),
        _ = &mut writer => Some("the control channel closed".to_owned()),
    };

    // Teardown (spec §3.5): sockets, server, forward.
    *io.control.lock().unwrap() = None;
    *io.backend.lock().unwrap() = None;
    *io.info.lock().unwrap() = None;
    if let Some(v) = video_shutdown {
        let _ = v.shutdown(Shutdown::Both);
    }
    if let Some(a) = audio_shutdown {
        let _ = a.shutdown(Shutdown::Both);
    }
    writer.abort();
    reader.abort();
    launched.server.kill();
    let _ = env.adb.kill_forward(serial, &launched.forward_local).await;
    io.mailbox.clear();
    if let Some(reason) = &ended {
        tracing::info!(slot, "mirroring ended: {reason}");
    }
    ended
}

/// Where a no-ADB session connects (the Android helper, see `ac_direct`).
#[derive(Clone)]
pub struct DirectEnv {
    pub media: Arc<MediaContext>,
    pub addr: std::net::SocketAddr,
    pub creds: ac_direct::Credentials,
    pub device_id: String,
}

/// Mirroring without ADB: the helper encodes the screen itself and sends
/// the same video format, so decoding is shared with scrcpy sessions.
pub fn spawn_direct_session(
    env: DirectEnv,
    params: ServerParams,
    slot: SlotId,
    id: u64,
    io: Arc<SlotIo>,
    tx: mpsc::UnboundedSender<SessionMsg>,
) -> SessionHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let error = run_direct_session(&env, &params, slot, id, &io, &tx, stop_rx).await;
        let _ = tx.send(SessionMsg::Slot(slot, SlotEvent::StreamEnded { id, error }));
    });
    SessionHandle { stop: Some(stop_tx) }
}

async fn run_direct_session(
    env: &DirectEnv,
    params: &ServerParams,
    slot: SlotId,
    id: u64,
    io: &Arc<SlotIo>,
    tx: &mpsc::UnboundedSender<SessionMsg>,
    mut stop: oneshot::Receiver<()>,
) -> Option<String> {
    tracing::info!(slot, max_size = params.max_size, bit_rate = params.bit_rate, "starting mirroring without ADB");
    let p = ac_direct::StreamParams {
        max_size: params.max_size,
        bit_rate: params.bit_rate,
        max_fps: params.max_fps,
        power_on: params.power_on,
        codec: if params.codec == VideoCodec::H265 { "h265" } else { "h264" },
    };
    let opened = tokio::select! {
        r = ac_direct::open_stream(env.addr, &env.creds, &env.device_id, p) => r,
        _ = &mut stop => return None,
    };
    let socket = match opened {
        Ok((s, _input)) => s,
        Err(e) => {
            tracing::warn!(slot, "mirroring without ADB failed to start: {e}");
            return Some(e.to_string());
        }
    };
    // Both directions run on blocking threads: video in, control out.
    let video = match socket.into_std().and_then(|s| s.set_nonblocking(false).map(|()| s)) {
        Ok(s) => s,
        Err(e) => return Some(format!("video socket: {e}")),
    };
    let (Ok(control), Ok(shutdown)) = (video.try_clone(), video.try_clone()) else {
        return Some("video socket: cannot share it".into());
    };
    let (done_tx, done_rx) = oneshot::channel();
    let decode = {
        let (io, media) = (io.clone(), env.media.clone());
        let mut events = DecodeEvents { slot, id, io: io.clone(), tx: tx.clone() };
        std::thread::Builder::new().name(format!("video-decode-{slot}")).spawn(move || {
            let r = ac_media::stream::run(video, &io.mailbox, &media, &mut events);
            let _ = done_tx.send(r);
        })
    };
    if let Err(e) = decode {
        return Some(format!("decode thread: {e}"));
    }
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let writer = std::thread::Builder::new().name(format!("direct-control-{slot}")).spawn(move || blocking_control_writer(control, ctl_rx));
    if let Err(e) = writer {
        let _ = shutdown.shutdown(Shutdown::Both);
        return Some(format!("control thread: {e}"));
    }
    *io.control.lock().unwrap() = Some(ctl_tx);
    *io.info.lock().unwrap() = Some(StreamInfo {
        codec: params.codec,
        max_size: params.max_size,
        bit_rate: params.bit_rate,
        max_fps: params.max_fps,
    });
    let ended = tokio::select! {
        _ = &mut stop => None,
        r = done_rx => Some(match r {
            Ok(Ok(())) => "the video stream ended".to_owned(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "the decode thread stopped".to_owned(),
        }),
    };
    // Dropping the sender ends the writer thread; the shutdown ends the reader.
    *io.control.lock().unwrap() = None;
    *io.backend.lock().unwrap() = None;
    *io.info.lock().unwrap() = None;
    let _ = shutdown.shutdown(Shutdown::Both);
    io.mailbox.clear();
    if let Some(reason) = &ended {
        tracing::info!(slot, "mirroring ended: {reason}");
    }
    ended
}

/// `control_writer` for a blocking socket.
fn blocking_control_writer(mut w: std::net::TcpStream, mut rx: mpsc::UnboundedReceiver<ControlMsg>) {
    use std::io::Write;
    let mut batch: Vec<ControlMsg> = Vec::new();
    let mut buf = Vec::new();
    while let Some(first) = rx.blocking_recv() {
        batch.push(first);
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        coalesce_moves(&mut batch);
        buf.clear();
        for m in batch.drain(..) {
            m.encode(&mut buf);
        }
        if w.write_all(&buf).is_err() {
            return;
        }
    }
}

fn server_exit_reason(output: &str) -> String {
    match output.lines().rev().find(|l| l.contains("ERROR") || l.contains("Exception")) {
        Some(line) => format!("the mirroring agent stopped: {}", line.trim()),
        None => "the mirroring agent stopped".to_owned(),
    }
}

/// Writes control messages, coalescing queued moves per pointer (spec §4.7).
async fn control_writer(mut w: tokio::net::tcp::OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<ControlMsg>) {
    let mut batch: Vec<ControlMsg> = Vec::new();
    let mut buf = Vec::new();
    while let Some(first) = rx.recv().await {
        batch.push(first);
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        coalesce_moves(&mut batch);
        buf.clear();
        for m in batch.drain(..) {
            m.encode(&mut buf);
        }
        if w.write_all(&buf).await.is_err() {
            return;
        }
    }
}

/// Drops a `MOVE` when a later `MOVE` of the same pointer follows it with
/// nothing else for that pointer in between. DOWN/UP and keys are kept.
pub fn coalesce_moves(batch: &mut Vec<ControlMsg>) {
    let mut keep = vec![true; batch.len()];
    for i in 0..batch.len() {
        let Some(id) = batch[i].coalescible_move() else { continue };
        for later in &batch[i + 1..] {
            match later {
                m if m.coalescible_move() == Some(id) => {
                    keep[i] = false;
                    break;
                }
                ControlMsg::InjectTouch { pointer_id, .. } if *pointer_id == id => break,
                _ => {}
            }
        }
    }
    let mut it = keep.into_iter();
    batch.retain(|_| it.next().unwrap());
}

/// Reads device messages; returns an error description if the stream breaks.
async fn control_reader(
    mut r: tokio::net::tcp::OwnedReadHalf,
    slot: SlotId,
    tx: mpsc::UnboundedSender<SessionMsg>,
) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Some(format!("control channel: {e}")),
        }
        loop {
            match ac_scrcpy::device_msg::parse(&buf) {
                Ok(Some((msg, n))) => {
                    buf.drain(..n);
                    if let DeviceMsg::Clipboard(text) = msg {
                        let _ = tx.send(SessionMsg::DeviceClipboard(slot, text));
                    }
                }
                Ok(None) => break,
                Err(e) => return Some(format!("control channel: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_scrcpy::control::{MotionAction, POINTER_GENERIC_FINGER, POINTER_MOUSE};
    use ac_scrcpy::{KeyAction, Position};

    fn touch(action: MotionAction, id: u64, x: i32) -> ControlMsg {
        ControlMsg::InjectTouch {
            action,
            pointer_id: id,
            position: Position { x, y: 0, screen_w: 100, screen_h: 100 },
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        }
    }

    #[test]
    fn coalescing_keeps_order_and_non_moves() {
        let f = POINTER_GENERIC_FINGER;
        let mut b = vec![
            touch(MotionAction::Down, f, 0),
            touch(MotionAction::Move, f, 1),
            touch(MotionAction::Move, f, 2),
            touch(MotionAction::Move, POINTER_MOUSE, 9),
            touch(MotionAction::Move, f, 3),
            touch(MotionAction::Up, f, 4),
            ControlMsg::InjectKeycode { action: KeyAction::Down, keycode: 4, repeat: 0, metastate: 0 },
        ];
        coalesce_moves(&mut b);
        let xs: Vec<_> = b
            .iter()
            .map(|m| match m {
                ControlMsg::InjectTouch { position, .. } => position.x,
                _ => -1,
            })
            .collect();
        assert_eq!(xs, vec![0, 9, 3, 4, -1]);
    }

    #[test]
    fn move_before_up_is_not_dropped() {
        let f = POINTER_GENERIC_FINGER;
        let mut b = vec![touch(MotionAction::Move, f, 1), touch(MotionAction::Up, f, 2), touch(MotionAction::Move, f, 3)];
        coalesce_moves(&mut b);
        assert_eq!(b.len(), 3, "the UP in between separates the moves");
    }

    #[test]
    fn auto_max_size_follows_the_tile() {
        let cfg = Config::default();
        let grid = resolve_profile(&cfg, StreamProfile::Grid, None).0;
        let p = server_params(&cfg, &grid, Some(700), None);
        assert_eq!(p.max_size, 704, "rounded up to 16");
        assert_eq!(server_params(&cfg, &grid, Some(100), None).max_size, 640, "min");
        assert_eq!(server_params(&cfg, &grid, Some(4000), None).max_size, 1280, "max");
        assert_eq!(server_params(&cfg, &grid, None, None).max_size, 1280);
        let focus = resolve_profile(&cfg, StreamProfile::Focus, None).0;
        let f = server_params(&cfg, &focus, None, None);
        assert_eq!((f.max_size, f.bit_rate, f.max_fps), (1920, 12_000_000, 60));
        assert_eq!(f.codec, VideoCodec::H264);
        assert_eq!(server_params(&cfg, &grid, None, Some(1800)).bit_rate, 1_800_000);
    }

    #[test]
    fn codec_choice_needs_both_ends() {
        let host = |hevc, av1| ac_media::ProbeResult { vaapi: Ok(()), dmabuf_export: true, hevc, av1 };
        let phone: Vec<String> = vec!["h264".into(), "h265".into()];
        assert_eq!(pick_codec("h265-if-supported", &phone, &host(true, false)), VideoCodec::H265);
        assert_eq!(pick_codec("h265-if-supported", &phone, &host(false, false)), VideoCodec::H264, "PC can't decode it");
        assert_eq!(pick_codec("h265-if-supported", &["h264".into()], &host(true, true)), VideoCodec::H264, "phone can't encode it");
        assert_eq!(pick_codec("h265-if-supported", &[], &host(true, true)), VideoCodec::H264, "not probed yet");
        assert_eq!(pick_codec("av1-if-supported", &phone, &host(true, true)), VideoCodec::H264);
        assert_eq!(pick_codec("h265", &[], &host(false, false)), VideoCodec::H265, "explicit");
        assert_eq!(pick_codec("h264", &phone, &host(true, true)), VideoCodec::H264);
    }

    #[test]
    fn per_device_profile_overrides() {
        let cfg = Config::default();
        let mut rec = DeviceRecord::new("SER1", ac_core::TransportKind::Tls, chrono::Utc::now());
        rec.profile_overrides.insert("grid".into(), toml::toml! { bitrate_kbps = 2000 });
        rec.profile_overrides.insert("focus".into(), toml::toml! { codec = "h264"
            max_fps = 45 });
        let (grid, err) = resolve_profile(&cfg, StreamProfile::Grid, Some(&rec));
        assert!(err.is_none());
        assert_eq!(grid.bitrate_kbps, 2000);
        assert_eq!(grid.max_fps, 30, "untouched keys keep the profile's value");
        let (focus, _) = resolve_profile(&cfg, StreamProfile::Focus, Some(&rec));
        assert_eq!((focus.codec.as_str(), focus.max_fps, focus.bitrate_kbps), ("h264", 45, 12000));

        // A device without overrides, and a broken override.
        assert_eq!(resolve_profile(&cfg, StreamProfile::Grid, None).0, cfg.profiles["grid"]);
        rec.profile_overrides.insert("grid".into(), toml::toml! { bitrate_kbps = "fast" });
        let (grid, err) = resolve_profile(&cfg, StreamProfile::Grid, Some(&rec));
        assert_eq!(grid, cfg.profiles["grid"]);
        assert!(err.unwrap().contains("profile_overrides.grid"));
    }

    #[test]
    fn coalescing_never_drops_down_or_up_under_a_flood() {
        // Spec §9 M3: 10 000 events, DOWN/UP never dropped, order kept.
        let f = POINTER_GENERIC_FINGER;
        let mut batch = Vec::new();
        let mut expected_edges = Vec::new();
        for i in 0..10_000 {
            let action = match i % 50 {
                0 => MotionAction::Down,
                49 => MotionAction::Up,
                _ => MotionAction::Move,
            };
            if action != MotionAction::Move {
                expected_edges.push((action, i));
            }
            let id = if i % 7 == 0 { POINTER_MOUSE } else { f };
            let id = if action == MotionAction::Move { id } else { f };
            batch.push(touch(action, id, i));
        }
        coalesce_moves(&mut batch);
        let edges: Vec<_> = batch
            .iter()
            .filter_map(|m| match m {
                ControlMsg::InjectTouch { action, position, .. } if *action != MotionAction::Move => Some((*action, position.x)),
                _ => None,
            })
            .collect();
        assert_eq!(edges, expected_edges);
        // Each stroke keeps at most one finger move (the last before UP).
        let finger_moves = batch.iter().filter(|m| m.coalescible_move() == Some(f)).count();
        assert!(finger_moves <= 200, "{finger_moves}");
        let xs: Vec<i32> = batch
            .iter()
            .map(|m| match m {
                ControlMsg::InjectTouch { position, .. } => position.x,
                _ => unreachable!(),
            })
            .collect();
        assert!(xs.windows(2).all(|w| w[0] < w[1]), "order preserved");
    }
}
