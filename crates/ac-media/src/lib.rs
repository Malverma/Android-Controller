//! Video decoding (spec §1.5, §2.4). FFmpeg decodes each stream on its own
//! thread; frames reach the UI through a latest-wins [`Mailbox`].
//!
//! Backends, chosen once at startup by [`MediaContext::new`]:
//! - `vaapi-dmabuf`: VA-API surfaces exported as dma-bufs (zero copy);
//! - `vaapi-copy`: VA-API, downloaded to NV12 in memory;
//! - `software`: libavcodec on the CPU (I420).
//!
//! The UI downgrades `vaapi-dmabuf` to `vaapi-copy` if GTK cannot import
//! the exported buffers ([`MediaContext::disable_dmabuf`]).

pub mod decoder;
pub mod frame;
pub mod mailbox;
pub mod stream;

use std::sync::atomic::{AtomicU8, Ordering};

pub use decoder::{DecodeError, Decoder, HwDevice, HwOutput, ProbeResult};
pub use frame::{ColorInfo, DmabufFrame, DmabufPlane, Frame, FrameData, MemoryFrame, PlaneLayout};
pub use mailbox::{Mailbox, VideoStats};
pub use stream::{LagEstimator, StreamEnd, StreamEvents};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Backend {
    VaapiDmabuf = 0,
    VaapiCopy = 1,
    Software = 2,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VaapiDmabuf => "vaapi-dmabuf",
            Self::VaapiCopy => "vaapi-copy",
            Self::Software => "software",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::VaapiDmabuf,
            1 => Self::VaapiCopy,
            _ => Self::Software,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Process-wide decoding setup: the shared VA-API device and the backend.
pub struct MediaContext {
    hw: Option<HwDevice>,
    backend: AtomicU8,
    pub probe: ProbeResult,
}

impl std::fmt::Debug for MediaContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaContext").field("backend", &self.backend()).field("probe", &self.probe).finish()
    }
}

impl MediaContext {
    /// `decoder` is `video.decoder` ("auto" | "vaapi-dmabuf" | "vaapi-copy" |
    /// "software"); `device` is `video.vaapi_device` ("auto" or a path).
    pub fn new(decoder: &str, device: &str) -> Self {
        if decoder == "software" {
            let probe = ProbeResult { vaapi: Err("disabled by video.decoder".into()), dmabuf_export: false };
            return Self { hw: None, backend: AtomicU8::new(Backend::Software as u8), probe };
        }
        let device = (device != "auto").then_some(device);
        let (hw, probe) = decoder::probe(device);
        let backend = match (&hw, decoder) {
            (None, _) => Backend::Software,
            (Some(_), "vaapi-copy") => Backend::VaapiCopy,
            (Some(_), _) if probe.dmabuf_export => Backend::VaapiDmabuf,
            (Some(_), _) => Backend::VaapiCopy,
        };
        match &probe.vaapi {
            Ok(()) => tracing::info!("video decoding: {backend} (dma-buf export: {})", probe.dmabuf_export),
            Err(e) => tracing::warn!("VA-API unavailable ({e}); video decoding: software"),
        }
        Self { hw, backend: AtomicU8::new(backend as u8), probe }
    }

    pub fn backend(&self) -> Backend {
        Backend::from_u8(self.backend.load(Ordering::Acquire))
    }

    pub fn hw_device(&self) -> Option<&HwDevice> {
        self.hw.as_ref()
    }

    /// GTK rejected a dma-buf: decode to memory from now on.
    pub fn disable_dmabuf(&self, reason: &str) {
        if self
            .backend
            .compare_exchange(Backend::VaapiDmabuf as u8, Backend::VaapiCopy as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            tracing::warn!("dma-buf import failed ({reason}); video decoding: vaapi-copy");
        }
    }
}
