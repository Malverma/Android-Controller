//! Decoded frames handed from a decode thread to the UI.

use std::os::fd::RawFd;
use std::time::Instant;

use ffmpeg_sys_next as ff;

/// Colour description in ITU-T H.273 (CICP) code points, which is what
/// both FFmpeg and `GdkCicpParams` use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    pub primaries: u32,
    pub transfer: u32,
    pub matrix: u32,
    pub full_range: bool,
}

impl ColorInfo {
    /// Fills in "unspecified" values the way players usually do: BT.709 for
    /// HD, BT.601 (SMPTE 170M) below 720 lines. Android encoders rarely
    /// signal colour, and limited range is their default.
    pub fn resolve(primaries: u32, transfer: u32, matrix: u32, full_range: bool, height: u32) -> Self {
        const UNSPECIFIED: u32 = 2;
        let hd = height >= 720;
        let default_matrix = if hd { 1 } else { 6 };
        Self {
            primaries: if primaries == UNSPECIFIED || primaries == 0 { if hd { 1 } else { 6 } } else { primaries },
            transfer: if transfer == UNSPECIFIED || transfer == 0 { 1 } else { transfer },
            matrix: if matrix == UNSPECIFIED || matrix == 0 { default_matrix } else { matrix },
            full_range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneLayout {
    /// Y plane + interleaved UV (VA-API download).
    Nv12,
    /// Y, U, V planes (software decode).
    I420,
}

/// Planes copied into one buffer.
pub struct MemoryFrame {
    pub layout: PlaneLayout,
    pub data: Vec<u8>,
    pub offsets: [usize; 3],
    pub strides: [usize; 3],
}

pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// A VA-API surface exported as DRM PRIME. The fds stay valid as long as
/// this value lives; the UI keeps it until GTK releases the texture.
pub struct DmabufFrame {
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    pub(crate) _keep: AvFrame,
}

pub enum FrameData {
    Memory(MemoryFrame),
    Dmabuf(DmabufFrame),
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pts_us: u64,
    pub color: ColorInfo,
    pub data: FrameData,
    pub decoded_at: Instant,
}

impl Frame {
    pub fn is_dmabuf(&self) -> bool {
        matches!(self.data, FrameData::Dmabuf(_))
    }
}

/// Owned `AVFrame`.
pub(crate) struct AvFrame(pub *mut ff::AVFrame);

// SAFETY: an AVFrame holding refcounted buffers may be released from any
// thread; nothing else aliases the pointer.
unsafe impl Send for AvFrame {}

impl AvFrame {
    pub fn alloc() -> Self {
        // SAFETY: plain allocation; null is checked by callers via `is_null`.
        Self(unsafe { ff::av_frame_alloc() })
    }
}

impl Drop for AvFrame {
    fn drop(&mut self) {
        // SAFETY: we own the frame; av_frame_free accepts null.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

pub const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
pub const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");
/// nvidia-vaapi-driver labels NV12's chroma layer `RG88`.
pub const DRM_FORMAT_RG88: u32 = u32::from_le_bytes(*b"RG88");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_colour_defaults() {
        let hd = ColorInfo::resolve(2, 2, 2, false, 1080);
        assert_eq!((hd.primaries, hd.transfer, hd.matrix), (1, 1, 1));
        let sd = ColorInfo::resolve(2, 2, 2, false, 480);
        assert_eq!((sd.primaries, sd.matrix), (6, 6));
        let given = ColorInfo::resolve(9, 16, 9, true, 480);
        assert_eq!((given.primaries, given.transfer, given.matrix, given.full_range), (9, 16, 9, true));
    }

    #[test]
    fn fourccs() {
        assert_eq!(DRM_FORMAT_NV12, 0x3231564e);
    }
}
