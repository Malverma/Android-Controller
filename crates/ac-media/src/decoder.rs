//! libavcodec decoder with VA-API acceleration (spec §1.5, §2.4).

use std::ffi::CString;
use std::ptr;
use std::time::Instant;

use ac_scrcpy::VideoCodec;
use ffmpeg_sys_next as ff;

use crate::frame::{
    AvFrame, ColorInfo, DRM_FORMAT_GR88, DRM_FORMAT_NV12, DRM_FORMAT_R8, DRM_FORMAT_RG88, DmabufFrame, DmabufPlane, Frame, FrameData,
    MemoryFrame, PlaneLayout,
};

const EAGAIN: i32 = 11;
const AVERROR_EOF: i32 = -(i32::from_le_bytes(*b"EOF "));
const FF_THREAD_SLICE: i32 = 2;
/// Spare surfaces beyond the decoder's needs: frames held by the mailbox and
/// by GTK while displayed.
const EXTRA_HW_FRAMES: i32 = 6;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0}")]
    Ffmpeg(String),
    #[error("unsupported pixel format {0}")]
    PixelFormat(i32),
}

pub(crate) fn av_error(code: i32) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: av_strerror writes a NUL-terminated string into `buf`.
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(code: i32, what: &str) -> Result<i32, DecodeError> {
    if code < 0 { Err(DecodeError::Ffmpeg(format!("{what}: {}", av_error(code)))) } else { Ok(code) }
}

/// A shared VA-API device (`AVHWDeviceContext`), refcounted by FFmpeg.
pub struct HwDevice(*mut ff::AVBufferRef);

// SAFETY: AVBufferRef refcounting is atomic and the device context is
// documented as usable from multiple threads once created.
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// Opens VA-API on `device` (a render node path) or libva's default.
    pub fn open_vaapi(device: Option<&str>) -> Result<Self, DecodeError> {
        let path = device.map(|d| CString::new(d).map_err(|_| DecodeError::Ffmpeg("bad device path".into())));
        let path = path.transpose()?;
        let mut ctx = ptr::null_mut();
        // SAFETY: out-pointer and optional C string are valid for the call.
        let r = unsafe {
            ff::av_hwdevice_ctx_create(
                &mut ctx,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                path.as_ref().map_or(ptr::null(), |p| p.as_ptr()),
                ptr::null_mut(),
                0,
            )
        };
        check(r, "VA-API device")?;
        Ok(Self(ctx))
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

/// Output wanted from a hardware frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwOutput {
    /// Export the surface as a dma-buf (zero copy).
    Dmabuf,
    /// Download to NV12 in system memory.
    Copy,
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: AvFrame,
    hw: bool,
}

// SAFETY: the decoder is used from one thread at a time (its decode thread).
unsafe impl Send for Decoder {}

unsafe extern "C" fn pick_vaapi(_ctx: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        let mut p = fmts;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            p = p.add(1);
        }
        // No VA-API for this stream (unsupported profile): let FFmpeg fall
        // back to its first (software) format.
        *fmts
    }
}

fn codec_id(codec: VideoCodec) -> ff::AVCodecID {
    match codec {
        VideoCodec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
        VideoCodec::H265 => ff::AVCodecID::AV_CODEC_ID_HEVC,
        VideoCodec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
    }
}

impl Decoder {
    /// A hardware decoder when `hw` is given, software otherwise.
    pub fn new(codec: VideoCodec, hw: Option<&HwDevice>) -> Result<Self, DecodeError> {
        // SAFETY: standard libavcodec setup; every pointer is checked.
        unsafe {
            let dec = ff::avcodec_find_decoder(codec_id(codec));
            if dec.is_null() {
                return Err(DecodeError::Ffmpeg(format!("no {codec:?} decoder in this FFmpeg")));
            }
            let mut ctx = ff::avcodec_alloc_context3(dec);
            if ctx.is_null() {
                return Err(DecodeError::Ffmpeg("out of memory".into()));
            }
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= ff::AV_CODEC_FLAG2_FAST;
            (*ctx).pkt_timebase = ff::AVRational { num: 1, den: 1_000_000 };
            if let Some(hw) = hw {
                (*ctx).hw_device_ctx = ff::av_buffer_ref(hw.0);
                (*ctx).get_format = Some(pick_vaapi);
                (*ctx).extra_hw_frames = EXTRA_HW_FRAMES;
                (*ctx).thread_count = 1;
            } else {
                // Slice threads only: frame threads add a frame of latency each.
                (*ctx).thread_type = FF_THREAD_SLICE;
                (*ctx).thread_count = 2;
            }
            let r = ff::avcodec_open2(ctx, dec, ptr::null_mut());
            if r < 0 {
                ff::avcodec_free_context(&mut ctx);
                return Err(DecodeError::Ffmpeg(format!("open decoder: {}", av_error(r))));
            }
            let pkt = ff::av_packet_alloc();
            let frame = AvFrame::alloc();
            if pkt.is_null() || frame.0.is_null() {
                ff::avcodec_free_context(&mut ctx);
                return Err(DecodeError::Ffmpeg("out of memory".into()));
            }
            Ok(Self { ctx, pkt, frame, hw: hw.is_some() })
        }
    }

    pub fn is_hw(&self) -> bool {
        self.hw
    }

    /// Decodes one access unit and returns the newest frame it produced.
    pub fn decode(&mut self, data: &[u8], pts_us: u64, key_frame: bool, out: HwOutput) -> Result<Option<Frame>, DecodeError> {
        // SAFETY: the packet is (re)allocated with FFmpeg's padding before
        // the copy; frames are owned by `self.frame` between calls.
        unsafe {
            check(ff::av_new_packet(self.pkt, data.len() as i32), "packet")?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            (*self.pkt).pts = pts_us as i64;
            (*self.pkt).dts = pts_us as i64;
            if key_frame {
                (*self.pkt).flags |= ff::AV_PKT_FLAG_KEY;
            }
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            check(r, "decode")?;

            let mut latest = None;
            loop {
                let r = ff::avcodec_receive_frame(self.ctx, self.frame.0);
                if r == -EAGAIN || r == AVERROR_EOF {
                    break;
                }
                check(r, "decode")?;
                latest = Some(self.convert(out)?);
                ff::av_frame_unref(self.frame.0);
            }
            Ok(latest)
        }
    }

    unsafe fn convert(&mut self, out: HwOutput) -> Result<Frame, DecodeError> {
        // SAFETY (whole fn): `self.frame` holds a frame just returned by
        // avcodec_receive_frame.
        unsafe {
            let f = self.frame.0;
            let (w, h) = ((*f).width as u32, (*f).height as u32);
            let full_range = (*f).color_range == ff::AVColorRange::AVCOL_RANGE_JPEG
                || (*f).format == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
            let color = ColorInfo::resolve(
                (*f).color_primaries as u32,
                (*f).color_trc as u32,
                (*f).colorspace as u32,
                full_range,
                h,
            );
            let data = if (*f).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                match out {
                    HwOutput::Dmabuf => FrameData::Dmabuf(export_dmabuf(f)?),
                    HwOutput::Copy => {
                        let sw = AvFrame::alloc();
                        (*sw.0).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                        check(ff::av_hwframe_transfer_data(sw.0, f, 0), "download frame")?;
                        FrameData::Memory(copy_planes(sw.0, PlaneLayout::Nv12, h))
                    }
                }
            } else {
                let layout = match (*f).format {
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32 => PlaneLayout::I420,
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32 => PlaneLayout::I420,
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => PlaneLayout::Nv12,
                    other => return Err(DecodeError::PixelFormat(other)),
                };
                FrameData::Memory(copy_planes(f, layout, h))
            };
            Ok(Frame { width: w, height: h, pts_us: (*f).pts.max(0) as u64, color, data, decoded_at: Instant::now() })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: we own the context and the packet.
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

/// Copies the planes of a software frame into one buffer, keeping strides.
unsafe fn copy_planes(f: *const ff::AVFrame, layout: PlaneLayout, height: u32) -> MemoryFrame {
    let h = height as usize;
    let ch = h.div_ceil(2);
    let n = match layout {
        PlaneLayout::Nv12 => 2,
        PlaneLayout::I420 => 3,
    };
    let rows = [h, ch, ch];
    let mut strides = [0usize; 3];
    let mut offsets = [0usize; 3];
    let mut total = 0;
    for i in 0..n {
        // SAFETY: linesize is valid for the frame's planes.
        strides[i] = unsafe { (*f).linesize[i] } as usize;
        offsets[i] = total;
        total += strides[i] * rows[i];
    }
    let mut data = vec![0u8; total];
    for i in 0..n {
        // SAFETY: each plane holds `linesize * rows` bytes.
        unsafe { ptr::copy_nonoverlapping((*f).data[i], data.as_mut_ptr().add(offsets[i]), strides[i] * rows[i]) };
    }
    MemoryFrame { layout, data, offsets, strides }
}

/// Maps a VA-API frame to DRM PRIME and describes it as one NV12 buffer.
unsafe fn export_dmabuf(f: *const ff::AVFrame) -> Result<DmabufFrame, DecodeError> {
    let drm = AvFrame::alloc();
    // SAFETY: av_hwframe_map fills `drm` with a reference to `f`'s surface;
    // the descriptor lives in drm->data[0] for as long as `drm` does.
    unsafe {
        (*drm.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        check(ff::av_hwframe_map(drm.0, f, ff::AV_HWFRAME_MAP_READ as i32), "export dma-buf")?;
        let desc = &*((*drm.0).data[0] as *const ff::AVDRMFrameDescriptor);
        let mut planes = Vec::new();
        let mut modifier = None;
        for layer in &desc.layers[..desc.nb_layers as usize] {
            for p in &layer.planes[..layer.nb_planes as usize] {
                let obj = &desc.objects[p.object_index as usize];
                modifier.get_or_insert(obj.format_modifier);
                planes.push(DmabufPlane { fd: obj.fd, offset: p.offset as u32, stride: p.pitch as u32 });
            }
        }
        // VA-API exports NV12 either as one 2-plane layer or as separate
        // R8 + GR88 (Mesa) / R8 + RG88 (NVIDIA) layers; all are NV12 to GTK.
        let formats: Vec<u32> = desc.layers[..desc.nb_layers as usize].iter().map(|l| l.format).collect();
        let fourcc = match formats.as_slice() {
            [DRM_FORMAT_NV12] => DRM_FORMAT_NV12,
            [DRM_FORMAT_R8, DRM_FORMAT_GR88 | DRM_FORMAT_RG88] => DRM_FORMAT_NV12,
            [single] => *single,
            _ => return Err(DecodeError::Ffmpeg(format!("unexpected dma-buf layers {formats:x?}"))),
        };
        if planes.len() != 2 {
            return Err(DecodeError::Ffmpeg(format!("expected 2 dma-buf planes, got {}", planes.len())));
        }
        Ok(DmabufFrame { fourcc, modifier: modifier.unwrap_or(0), planes, _keep: drm })
    }
}

/// The embedded 64×64 H.264 IDR frame used by [`probe`].
pub const PROBE_H264: &[u8] = include_bytes!("../assets/probe.h264");

/// Which decode paths work on this host, without involving the display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub vaapi: Result<(), String>,
    /// The VA-API surface could be exported as a dma-buf.
    pub dmabuf_export: bool,
}

/// Decodes the probe frame with VA-API (copy and export) on `device`.
pub fn probe(device: Option<&str>) -> (Option<HwDevice>, ProbeResult) {
    let hw = match HwDevice::open_vaapi(device) {
        Ok(hw) => hw,
        Err(e) => return (None, ProbeResult { vaapi: Err(e.to_string()), dmabuf_export: false }),
    };
    let run = |out| -> Result<Frame, String> {
        let mut d = Decoder::new(VideoCodec::H264, Some(&hw)).map_err(|e| e.to_string())?;
        d.decode(PROBE_H264, 0, true, out).map_err(|e| e.to_string())?.ok_or_else(|| "no frame".to_owned())
    };
    // VA-API downloads come back as NV12; a software fallback (VA-API
    // refused the stream) comes back as I420.
    match run(HwOutput::Copy) {
        Ok(Frame { data: FrameData::Memory(MemoryFrame { layout: PlaneLayout::Nv12, .. }), .. }) => {
            let dmabuf_export = run(HwOutput::Dmabuf).is_ok_and(|f| f.is_dmabuf());
            (Some(hw), ProbeResult { vaapi: Ok(()), dmabuf_export })
        }
        Ok(_) => (None, ProbeResult { vaapi: Err("VA-API did not decode the probe frame".into()), dmabuf_export: false }),
        Err(e) => (None, ProbeResult { vaapi: Err(e), dmabuf_export: false }),
    }
}
