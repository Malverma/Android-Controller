//! One mirroring session: launch scrcpy-server, run the decode thread and the
//! control channel, and report how it ended (spec §3.3.3, §4).

use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ac_adb::AdbClient;
use ac_config::{Config, MaxSize, Profile};
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
}

impl SlotIo {
    /// Queues a control message; false when no stream is running.
    pub fn send(&self, msg: ControlMsg) -> bool {
        self.control.lock().unwrap().as_ref().is_some_and(|tx| tx.send(msg).is_ok())
    }

    pub fn is_streaming(&self) -> bool {
        self.control.lock().unwrap().is_some()
    }

    /// Backend decoding this slot's stream, while it runs.
    pub fn backend(&self) -> Option<Backend> {
        *self.backend.lock().unwrap()
    }
}

/// What a session reports back to the manager.
pub enum SessionMsg {
    Slot(SlotId, SlotEvent),
    DeviceClipboard(SlotId, String),
}

/// Everything a session needs; cheap to clone per launch.
#[derive(Clone)]
pub struct StreamEnv {
    pub adb: AdbClient,
    pub server_jar: PathBuf,
    pub server_version: String,
    pub media: Arc<MediaContext>,
}

/// Launch parameters for a profile (spec §4.3). `tile_px` is the grid
/// tile's longest side in device pixels, for `max_size = "auto"`.
pub fn server_params(cfg: &Config, profile: StreamProfile, tile_px: Option<u32>, bitrate_kbps: Option<u32>) -> ServerParams {
    let name = match profile {
        StreamProfile::Grid => &cfg.video.grid_profile,
        StreamProfile::Focus => &cfg.video.focus_profile,
    };
    let p: Profile = cfg.profiles.get(name).cloned().unwrap_or_default();
    let max_size = match p.max_size {
        MaxSize::Px(px) => px,
        MaxSize::Auto(_) => {
            let px = tile_px.unwrap_or(p.max_size_max);
            px.div_ceil(16).saturating_mul(16).clamp(p.max_size_min, p.max_size_max)
        }
    };
    let codec = match p.codec.as_str() {
        "h265" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        // "h265-if-supported" needs the device's encoder list, which is
        // not probed yet: H.264 works everywhere.
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
    slot: SlotId,
    id: u64,
    io: Arc<SlotIo>,
    tx: mpsc::UnboundedSender<SessionMsg>,
) -> SessionHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let error = run_session(&env, &serial, shell_v2, &params, slot, id, &io, &tx, stop_rx).await;
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

    // Control: writer task fed by `SlotIo::send`, reader task for clipboard.
    let (read_half, write_half) = launched.control.into_split();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    *io.control.lock().unwrap() = Some(ctl_tx);
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
    if let Some(v) = video_shutdown {
        let _ = v.shutdown(Shutdown::Both);
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
        let p = server_params(&cfg, StreamProfile::Grid, Some(700), None);
        assert_eq!(p.max_size, 704, "rounded up to 16");
        assert_eq!(server_params(&cfg, StreamProfile::Grid, Some(100), None).max_size, 640, "min");
        assert_eq!(server_params(&cfg, StreamProfile::Grid, Some(4000), None).max_size, 1280, "max");
        assert_eq!(server_params(&cfg, StreamProfile::Grid, None, None).max_size, 1280);
        let f = server_params(&cfg, StreamProfile::Focus, None, None);
        assert_eq!((f.max_size, f.bit_rate, f.max_fps), (1920, 12_000_000, 60));
        assert_eq!(f.codec, VideoCodec::H264);
        assert_eq!(server_params(&cfg, StreamProfile::Grid, None, Some(1800)).bit_rate, 1_800_000);
    }
}
