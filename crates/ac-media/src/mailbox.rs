//! Latest-wins hand-off from a decode thread to the GTK main thread.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::frame::Frame;

#[derive(Default)]
pub struct VideoStats {
    pub decoded: AtomicU64,
    /// Replaced in the mailbox before the UI took them.
    pub dropped: AtomicU64,
    pub presented: AtomicU64,
    pub bytes: AtomicU64,
    pub decode_errors: AtomicU64,
    /// 95th-percentile network lag over the last 10 s (spec §3.6).
    pub lag_ms: AtomicU32,
}

/// One per slot, shared by its decode thread (writer) and the UI (reader).
#[derive(Default)]
pub struct Mailbox {
    frame: Mutex<Option<Frame>>,
    /// Bumped on every `put`, so the UI can skip the lock when idle.
    seq: AtomicU64,
    /// Current capture size `(w << 32) | h`, 0 when not streaming.
    size: AtomicU64,
    pub stats: VideoStats,
}

impl Mailbox {
    pub fn put(&self, frame: Frame) {
        let old = self.frame.lock().unwrap().replace(frame);
        if old.is_some() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.decoded.fetch_add(1, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// Takes the newest frame, if one arrived since the last call.
    pub fn take(&self) -> Option<Frame> {
        self.frame.lock().unwrap().take()
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn set_size(&self, w: u32, h: u32) {
        self.size.store((u64::from(w) << 32) | u64::from(h), Ordering::Release);
    }

    /// The capture size from the latest session packet.
    pub fn size(&self) -> Option<(u32, u32)> {
        let v = self.size.load(Ordering::Acquire);
        (v != 0).then_some(((v >> 32) as u32, v as u32))
    }

    /// Ends a stream: forget the size and any undisplayed frame.
    pub fn clear(&self) {
        self.size.store(0, Ordering::Release);
        self.frame.lock().unwrap().take();
        self.stats.lag_ms.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ColorInfo, FrameData, MemoryFrame, PlaneLayout};

    fn frame(pts: u64) -> Frame {
        Frame {
            width: 2,
            height: 2,
            pts_us: pts,
            color: ColorInfo::resolve(2, 2, 2, false, 2),
            data: FrameData::Memory(MemoryFrame {
                layout: PlaneLayout::I420,
                data: vec![0; 6],
                offsets: [0, 4, 5],
                strides: [2, 1, 1],
            }),
            decoded_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn latest_wins() {
        let m = Mailbox::default();
        assert!(m.take().is_none());
        m.put(frame(1));
        m.put(frame(2));
        assert_eq!(m.seq(), 2);
        assert_eq!(m.take().unwrap().pts_us, 2);
        assert!(m.take().is_none());
        assert_eq!(m.stats.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(m.stats.decoded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn size() {
        let m = Mailbox::default();
        assert_eq!(m.size(), None);
        m.set_size(1080, 2400);
        assert_eq!(m.size(), Some((1080, 2400)));
        m.clear();
        assert_eq!(m.size(), None);
    }
}
