//! The `video-decode-<slot>` loop: demux → decode → mailbox.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ac_scrcpy::{DemuxError, DemuxItem, Demuxer};

use crate::decoder::{Decoder, HwOutput};
use crate::mailbox::Mailbox;
use crate::recorder::RecMsg;
use crate::{Backend, MediaContext};

/// Spec Appendix A `E_DECODER`: 5 errors within 2 s.
const ERROR_BURST: usize = 5;
const ERROR_WINDOW: Duration = Duration::from_secs(2);
const LAG_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum StreamEnd {
    #[error(transparent)]
    Demux(#[from] DemuxError),
    #[error("video decoder: {0}")]
    Decoder(String),
}

/// Callbacks from the decode thread.
pub trait StreamEvents {
    /// The decoder needs a key frame (send `RESET_VIDEO`).
    fn request_key_frame(&mut self);
    /// The backend in use for this stream changed (or was chosen).
    fn backend(&mut self, _backend: Backend) {}
    /// The capture (re)started at a new size: stream start and rotation.
    fn session(&mut self, _width: u32, _height: u32) {}
}

/// Runs until the socket closes or an unrecoverable error. The caller
/// stops it by shutting the socket down.
pub fn run<R: Read>(video: R, mailbox: &Mailbox, media: &MediaContext, events: &mut impl StreamEvents) -> Result<(), StreamEnd> {
    let mut demux = Demuxer::new(video);
    let codec = demux.read_codec()?;

    let mut backend = media.backend();
    let mut decoder = open(codec, media, &mut backend)?;
    events.backend(backend);
    let mut errors: VecDeque<Instant> = VecDeque::new();
    let mut lag = LagEstimator::default();
    let mut last_bytes = 0;
    // For a recording: the capture size, the latest codec config, and the
    // recording generation this thread already introduced itself to.
    let mut size = (0, 0);
    let mut config: Vec<u8> = Vec::new();
    let mut rec_seen = 0;

    loop {
        let item = demux.next_item()?;
        let bytes = demux.bytes_read();
        mailbox.stats.bytes.fetch_add(bytes - last_bytes, Ordering::Relaxed);
        last_bytes = bytes;
        let (data, pts_us, key_frame, config_len) = match item {
            DemuxItem::Session { width, height, .. } => {
                mailbox.set_size(width, height);
                events.session(width, height);
                lag = LagEstimator::default();
                size = (width, height);
                continue;
            }
            DemuxItem::Packet { data, pts_us, key_frame, config_len } => (data, pts_us, key_frame, config_len),
        };
        mailbox.stats.lag_ms.store(lag.push(pts_us, Instant::now()), Ordering::Relaxed);
        if config_len > 0 {
            config = data[..config_len].to_vec();
        }
        if let Some((generation, rec)) = mailbox.recorder() {
            let fresh = generation != rec_seen;
            if (fresh || config_len > 0) && !config.is_empty() {
                let _ = rec.send(RecMsg::Config { codec, width: size.0, height: size.1, config: config.clone() });
            }
            if fresh {
                rec_seen = generation;
                if !key_frame {
                    // A recording starts on a key frame: ask for one now.
                    events.request_key_frame();
                }
            }
            let _ = rec.send(RecMsg::Packet { data: data[config_len..].to_vec(), pts_us, key: key_frame });
        }

        // The UI may have rejected dma-bufs since the last frame.
        if backend == Backend::VaapiDmabuf && media.backend() != Backend::VaapiDmabuf {
            backend = media.backend();
            events.backend(backend);
        }
        let out = if backend == Backend::VaapiDmabuf { HwOutput::Dmabuf } else { HwOutput::Copy };
        match decoder.decode(&data, pts_us, key_frame, out) {
            Ok(Some(frame)) => mailbox.put(frame),
            Ok(None) => {}
            Err(e) => {
                mailbox.stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("decode error: {e}");
                let now = Instant::now();
                errors.push_back(now);
                while errors.front().is_some_and(|t| now.duration_since(*t) > ERROR_WINDOW) {
                    errors.pop_front();
                }
                if errors.len() == 1 {
                    events.request_key_frame();
                }
                if errors.len() >= ERROR_BURST {
                    if !decoder.is_hw() {
                        return Err(StreamEnd::Decoder(e.to_string()));
                    }
                    tracing::warn!("hardware decoding keeps failing ({e}); switching this stream to software");
                    backend = Backend::Software;
                    decoder = Decoder::new(codec, None).map_err(|e| StreamEnd::Decoder(e.to_string()))?;
                    events.backend(backend);
                    events.request_key_frame();
                    errors.clear();
                }
            }
        }
    }
}

fn open(codec: ac_scrcpy::VideoCodec, media: &MediaContext, backend: &mut Backend) -> Result<Decoder, StreamEnd> {
    if *backend != Backend::Software
        && let Some(hw) = media.hw_device()
    {
        match Decoder::new(codec, Some(hw)) {
            Ok(d) => return Ok(d),
            Err(e) => tracing::warn!("VA-API decoder for {codec:?} unavailable ({e}); using software"),
        }
    }
    *backend = Backend::Software;
    Decoder::new(codec, None).map_err(|e| StreamEnd::Decoder(e.to_string()))
}

/// Spec §3.6: `lag = (arrival − first_arrival) − (pts − first_pts)`,
/// re-based to its minimum over the window; reports the 95th percentile.
#[derive(Default)]
pub struct LagEstimator {
    origin: Option<(Instant, u64)>,
    samples: VecDeque<(Instant, i64)>,
}

impl LagEstimator {
    pub fn push(&mut self, pts_us: u64, now: Instant) -> u32 {
        let (t0, p0) = *self.origin.get_or_insert((now, pts_us));
        let arrival_us = now.duration_since(t0).as_micros() as i64;
        let media_us = pts_us as i64 - p0 as i64;
        self.samples.push_back((now, arrival_us - media_us));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > LAG_WINDOW) {
            self.samples.pop_front();
        }
        let mut v: Vec<i64> = self.samples.iter().map(|s| s.1).collect();
        v.sort_unstable();
        let min = v[0];
        let p95 = v[(v.len() - 1) * 95 / 100];
        ((p95 - min) / 1000).clamp(0, u32::MAX as i64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_stream_has_no_lag() {
        let mut l = LagEstimator::default();
        let t = Instant::now();
        for i in 0..100u64 {
            let lag = l.push(i * 16_666, t + Duration::from_micros(i * 16_666));
            assert_eq!(lag, 0);
        }
    }

    #[test]
    fn late_burst_shows_up() {
        let mut l = LagEstimator::default();
        let t = Instant::now();
        let mut last = 0;
        for i in 0..100u64 {
            // From frame 50 on, frames arrive 300 ms late.
            let late = if i >= 50 { 300_000 } else { 0 };
            last = l.push(i * 16_666, t + Duration::from_micros(i * 16_666 + late));
        }
        assert!((290..=310).contains(&last), "{last}");
    }
}
