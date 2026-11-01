//! One phone's picture: capture, encode and send, on its own threads. A slow
//! Wi-Fi drops frames instead of building up delay.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::capture::{Capturer, SlotState};
use crate::encode::{self, Encoder, Kind, Params};
use crate::protocol::frame_header;

const POLL: Duration = Duration::from_millis(5);
/// Frames waiting to be sent before new ones are dropped.
const SEND_QUEUE: usize = 2;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A static screen sends nothing; this long with a keyframe due re-copies it.
const FORCE_COPY_AFTER: Duration = Duration::from_millis(150);
const OUTPUT_WAIT: Duration = Duration::from_secs(3);
/// Show the cursor in the picture: the phone has no other way to draw it.
const OVERLAY_CURSOR: bool = true;

#[derive(Debug, Clone)]
pub struct SessionParams {
    pub output: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub encoder: String,
    pub render_node: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The first frame was sent.
    Live { width: u32, height: u32, encoder: Kind },
    Stats { fps: u32, kbps: u32, target_kbps: u32 },
    /// The session ended by itself. A closed connection is a normal way to end.
    Ended { error: String },
}

pub struct SessionHandle {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SessionHandle {
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts a session sending to `video`, an open connection to the phone.
pub fn spawn(params: SessionParams, video: TcpStream, on_event: impl Fn(SessionEvent) + Send + 'static) -> SessionHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(true));
    let (s, k) = (stop.clone(), keyframe.clone());
    let thread = std::thread::Builder::new()
        .name(format!("monitor-{}", params.output))
        .spawn(move || {
            let error = match run(&params, video, &s, &k, &on_event) {
                Ok(()) => String::new(),
                Err(e) => format!("{e:#}"),
            };
            if !s.load(Ordering::Relaxed) {
                on_event(SessionEvent::Ended { error });
            }
        })
        .expect("spawning a thread");
    SessionHandle { stop, keyframe, thread: Some(thread) }
}

struct Sender {
    tx: SyncSender<Vec<u8>>,
    thread: JoinHandle<()>,
    broken: Arc<AtomicBool>,
}

fn sender(mut video: TcpStream) -> Result<Sender> {
    video.set_nodelay(true).context("TCP_NODELAY")?;
    video.set_write_timeout(Some(WRITE_TIMEOUT)).context("write timeout")?;
    let (tx, rx) = sync_channel::<Vec<u8>>(SEND_QUEUE);
    let broken = Arc::new(AtomicBool::new(false));
    let flag = broken.clone();
    let thread = std::thread::Builder::new().name("monitor-send".into()).spawn(move || {
        while let Ok(frame) = rx.recv() {
            if video.write_all(&frame).is_err() {
                flag.store(true, Ordering::Relaxed);
                return;
            }
        }
    })?;
    Ok(Sender { tx, thread, broken })
}

fn connect_capture(output: &str) -> Result<Capturer> {
    let start = Instant::now();
    loop {
        // A new connection sees outputs created after the last one.
        let c = Capturer::connect()?;
        if c.output_names().iter().any(|n| n == output) {
            return Ok(c);
        }
        if start.elapsed() >= OUTPUT_WAIT {
            return Err(anyhow!("no Wayland output named {output}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run(p: &SessionParams, video: TcpStream, stop: &AtomicBool, keyframe: &AtomicBool, on_event: &dyn Fn(SessionEvent)) -> Result<()> {
    encode::init();
    let mut cap = connect_capture(&p.output)?;
    let Sender { tx, thread: send_thread, broken } = sender(video)?;
    let mut encoder: Option<Encoder> = None;
    let mut rate = RateControl::new(p.bitrate_kbps, p.min_bitrate_kbps);
    let started = Instant::now();
    let frame_gap = Duration::from_micros(1_000_000 / u64::from(p.fps.max(1)));
    let mut last_frame: Option<Instant> = None;
    let mut copying_since: Option<Instant> = None;
    let mut force_idr = true;
    let mut live = false;
    let mut window = Stats::default();
    let mut window_start = Instant::now();

    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if broken.load(Ordering::Relaxed) {
            break Err(anyhow!("the phone's connection closed"));
        }
        force_idr |= keyframe.swap(false, Ordering::Relaxed);

        match cap.slot_state(0) {
            SlotState::Idle | SlotState::Failed => {
                if last_frame.is_some_and(|t| t.elapsed() < frame_gap) {
                    std::thread::sleep(POLL.min(frame_gap));
                    continue;
                }
                cap.continuous = force_idr;
                cap.request(0, &p.output, OVERLAY_CURSOR)?;
                copying_since = Some(Instant::now());
            }
            SlotState::Ready => {}
            _ => {
                // Waiting for damage on a still screen, with a keyframe owed.
                if force_idr && !cap.continuous && copying_since.is_some_and(|t| t.elapsed() >= FORCE_COPY_AFTER) {
                    cap.continuous = true;
                    cap.request(0, &p.output, OVERLAY_CURSOR)?;
                    copying_since = Some(Instant::now());
                }
            }
        }
        cap.poll(POLL)?;

        let Some(frame) = cap.ready(0) else { continue };
        let info = frame.info;
        let want_kbps = rate.kbps();
        if encoder.as_ref().is_none_or(|e| {
            e.params.width != info.width || e.params.height != info.height || e.params.src_fmt != info.pixel() || e.params.bitrate_kbps != want_kbps
        }) {
            drop(encoder.take()); // only one encoder at a time on small GPUs
            let params = Params {
                width: info.width,
                height: info.height,
                src_fmt: info.pixel(),
                fps: p.fps,
                bitrate_kbps: want_kbps,
                render_node: p.render_node.clone(),
            };
            encoder = Some(Encoder::new(params, &p.encoder).context("no H.264 encoder works on this PC")?);
            force_idr = true;
        }
        let enc = encoder.as_mut().expect("just created");
        let pts = started.elapsed().as_micros() as u64;
        let packets = enc.encode(frame.data, info.stride as usize, frame.y_invert, pts as i64, force_idr)?;
        let kind = enc.kind;
        force_idr = false;
        last_frame = Some(Instant::now());
        // Let the next request start from a clean slot.
        cap.mark_idle(0);

        for packet in packets {
            let mut msg = Vec::with_capacity(packet.data.len() + 12);
            msg.extend_from_slice(&frame_header(pts, packet.keyframe, packet.data.len() as u32));
            msg.extend_from_slice(&packet.data);
            let size = msg.len();
            match tx.try_send(msg) {
                Ok(()) => {
                    window.frames += 1;
                    window.bytes += size as u64;
                    if !live {
                        live = true;
                        on_event(SessionEvent::Live { width: info.width, height: info.height, encoder: kind });
                    }
                }
                Err(TrySendError::Full(_)) => {
                    // What follows would not decode without what was dropped.
                    window.dropped += 1;
                    force_idr = true;
                }
                Err(TrySendError::Disconnected(_)) => {
                    broken.store(true, Ordering::Relaxed);
                }
            }
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            let elapsed = window_start.elapsed().as_secs_f64();
            rate.second(window.frames, window.dropped);
            on_event(SessionEvent::Stats {
                fps: (f64::from(window.frames) / elapsed).round() as u32,
                kbps: (window.bytes as f64 * 8.0 / 1000.0 / elapsed).round() as u32,
                target_kbps: rate.kbps(),
            });
            window = Stats::default();
            window_start = Instant::now();
        }
    };
    drop(tx);
    let _ = send_thread.join();
    result
}

#[derive(Default)]
struct Stats {
    frames: u32,
    dropped: u32,
    bytes: u64,
}

/// Steps the bitrate down while frames are being dropped, and back up slowly
/// once the network has kept up for a while.
#[derive(Debug, Clone)]
pub struct RateControl {
    target: u32,
    min: u32,
    current: u32,
    clean_seconds: u32,
}

/// Dropping more than 1 frame in this many means the Wi-Fi cannot keep up.
const DROP_RATIO: u32 = 10;
const CLEAN_SECONDS_TO_RAISE: u32 = 20;

impl RateControl {
    pub fn new(target: u32, min: u32) -> Self {
        Self { target, min: min.min(target), current: target, clean_seconds: 0 }
    }

    pub fn kbps(&self) -> u32 {
        self.current
    }

    /// Feeds one second's counts.
    pub fn second(&mut self, sent: u32, dropped: u32) {
        if dropped > 0 && dropped * DROP_RATIO >= sent + dropped {
            self.current = (self.current * 7 / 10).max(self.min);
            self.clean_seconds = 0;
        } else if dropped == 0 {
            self.clean_seconds += 1;
            if self.clean_seconds >= CLEAN_SECONDS_TO_RAISE {
                self.current = (self.current * 6 / 5).min(self.target);
                self.clean_seconds = 0;
            }
        } else {
            self.clean_seconds = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_down_when_dropping_and_stops_at_the_floor() {
        let mut r = RateControl::new(8000, 2000);
        r.second(50, 10);
        assert_eq!(r.kbps(), 5600);
        for _ in 0..20 {
            r.second(10, 50);
        }
        assert_eq!(r.kbps(), 2000);
    }

    #[test]
    fn a_rare_drop_is_tolerated() {
        let mut r = RateControl::new(8000, 2000);
        r.second(59, 1);
        assert_eq!(r.kbps(), 8000);
    }

    #[test]
    fn climbs_back_only_after_a_clean_stretch() {
        let mut r = RateControl::new(8000, 2000);
        r.second(50, 10);
        for _ in 0..19 {
            r.second(60, 0);
        }
        assert_eq!(r.kbps(), 5600, "not yet");
        r.second(60, 0);
        assert_eq!(r.kbps(), 6720);
        // A drop restarts the clean count.
        for _ in 0..19 {
            r.second(60, 0);
        }
        r.second(59, 1);
        r.second(60, 0);
        assert_eq!(r.kbps(), 6720);
        for _ in 0..200 {
            r.second(60, 0);
        }
        assert_eq!(r.kbps(), 8000, "never above the target");
    }

    #[test]
    fn floor_never_exceeds_the_target() {
        assert_eq!(RateControl::new(1000, 2000).kbps(), 1000);
    }
}
