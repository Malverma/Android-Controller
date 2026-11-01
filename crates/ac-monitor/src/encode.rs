//! H.264 encoding with FFmpeg: NVENC, VA-API or x264, whichever works first.
//! Every setting favours latency. Keyframes carry SPS and PPS inline, which is
//! what lets the phone's decoder join at any keyframe. The NVENC and x264 setup
//! is ported from RemoteLinux (`rl-host`).

use anyhow::{Context, Result, anyhow, bail};
use ff::format::Pixel;
use ffmpeg_next as ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Nvenc,
    Vaapi,
    X264,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "NVENC",
            Self::Vaapi => "VA-API",
            Self::X264 => "x264 (CPU)",
        }
    }

    /// True when encoding runs on the graphics card.
    pub fn hardware(self) -> bool {
        self != Self::X264
    }
}

/// The encoders to try for a `monitor.encoder` setting.
pub fn order(preference: &str) -> &'static [Kind] {
    match preference {
        "nvenc" => &[Kind::Nvenc],
        "vaapi" => &[Kind::Vaapi],
        "x264" => &[Kind::X264],
        _ => &[Kind::Nvenc, Kind::Vaapi, Kind::X264],
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub src_fmt: Pixel,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// VA-API render node.
    pub render_node: String,
}

pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// A VA-API device and frame pool, owned for the encoder's lifetime.
struct Vaapi {
    device: *mut ff::ffi::AVBufferRef,
    frames: *mut ff::ffi::AVBufferRef,
}

// The buffers are only used by the thread owning the encoder.
unsafe impl Send for Vaapi {}

impl Drop for Vaapi {
    fn drop(&mut self) {
        unsafe {
            ff::ffi::av_buffer_unref(&mut self.frames);
            ff::ffi::av_buffer_unref(&mut self.device);
        }
    }
}

pub struct Encoder {
    pub params: Params,
    pub kind: Kind,
    enc: ff::encoder::Video,
    frame: ff::frame::Video,
    scaler: Option<ff::software::scaling::Context>,
    packet: ff::Packet,
    next_pts: i64,
    vaapi: Option<Vaapi>,
}

pub fn init() {
    let _ = ff::init();
    ff::util::log::set_level(ff::util::log::Level::Error);
}

fn check(what: &str, code: i32) -> Result<()> {
    if code < 0 { Err(anyhow!("{what}: {}", ff::Error::from(code))) } else { Ok(()) }
}

fn open_vaapi(p: &Params) -> Result<Vaapi> {
    let node = std::ffi::CString::new(p.render_node.as_str())?;
    unsafe {
        let mut device = std::ptr::null_mut();
        check(
            "opening the VA-API device",
            ff::ffi::av_hwdevice_ctx_create(&raw mut device, ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, node.as_ptr(), std::ptr::null_mut(), 0),
        )?;
        let mut guard = Vaapi { device, frames: std::ptr::null_mut() };
        guard.frames = ff::ffi::av_hwframe_ctx_alloc(device);
        if guard.frames.is_null() {
            bail!("allocating VA-API frames");
        }
        let ctx = (*guard.frames).data.cast::<ff::ffi::AVHWFramesContext>();
        (*ctx).format = ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*ctx).sw_format = ff::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).width = p.width as i32;
        (*ctx).height = p.height as i32;
        (*ctx).initial_pool_size = 8;
        check("setting up VA-API frames", ff::ffi::av_hwframe_ctx_init(guard.frames))?;
        Ok(guard)
    }
}

impl Encoder {
    /// `preference` is `"auto"`, `"nvenc"`, `"vaapi"` or `"x264"`.
    pub fn new(params: Params, preference: &str) -> Result<Self> {
        let mut last = None;
        for &kind in order(preference) {
            match Self::open(params.clone(), kind) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::info!("{} encoder unavailable: {e:#}", kind.label());
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no encoder")))
    }

    fn open(p: Params, kind: Kind) -> Result<Self> {
        let name = match kind {
            Kind::Nvenc => "h264_nvenc",
            Kind::Vaapi => "h264_vaapi",
            Kind::X264 => "libx264",
        };
        let codec = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("FFmpeg has no {name}"))?;
        let sw_fmt = match kind {
            Kind::Nvenc => p.src_fmt,
            Kind::Vaapi => Pixel::NV12,
            Kind::X264 => Pixel::YUV420P,
        };
        let vaapi = if kind == Kind::Vaapi { Some(open_vaapi(&p)?) } else { None };
        let mut enc = ff::codec::context::Context::new_with_codec(codec).encoder().video()?;
        enc.set_width(p.width);
        enc.set_height(p.height);
        enc.set_format(if kind == Kind::Vaapi { Pixel::VAAPI } else { sw_fmt });
        enc.set_time_base((1, 1_000_000));
        enc.set_frame_rate(Some((p.fps as i32, 1)));
        enc.set_bit_rate(p.bitrate_kbps as usize * 1000);
        enc.set_max_bit_rate(p.bitrate_kbps as usize * 1000);
        enc.set_max_b_frames(0);
        // A keyframe every 2 s: a phone that lost its picture recovers quickly.
        enc.set_gop(p.fps.max(1) * 2);
        unsafe {
            let ctx = enc.as_mut_ptr();
            (*ctx).rc_buffer_size = ((p.bitrate_kbps as i64 * 1000 * 2) / p.fps.max(1) as i64) as i32;
            (*ctx).flags |= ff::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).color_range = ff::ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*ctx).colorspace = ff::ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*ctx).color_primaries = ff::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*ctx).color_trc = ff::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            if let Some(v) = &vaapi {
                (*ctx).hw_frames_ctx = ff::ffi::av_buffer_ref(v.frames);
            }
        }
        let mut opts = ff::Dictionary::new();
        match kind {
            Kind::Nvenc => {
                opts.set("preset", "p1");
                opts.set("tune", "ull");
                opts.set("zerolatency", "1");
                opts.set("rc", "cbr");
                opts.set("delay", "0");
                opts.set("rc-lookahead", "0");
                opts.set("no-scenecut", "1");
                opts.set("forced-idr", "1");
                opts.set("profile", "high");
            }
            Kind::Vaapi => {
                opts.set("rc_mode", "CBR");
                opts.set("async_depth", "1");
                opts.set("profile", "high");
            }
            Kind::X264 => {
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("x264-params", "repeat-headers=1:sync-lookahead=0:rc-lookahead=0");
            }
        }
        let enc = enc.open_with(opts).with_context(|| format!("opening {name}"))?;
        let frame = ff::frame::Video::new(sw_fmt, p.width, p.height);
        let scaler = if sw_fmt == p.src_fmt {
            None
        } else {
            Some(ff::software::scaling::Context::get(
                p.src_fmt,
                p.width,
                p.height,
                sw_fmt,
                p.width,
                p.height,
                ff::software::scaling::Flags::FAST_BILINEAR,
            )?)
        };
        tracing::info!("encoder {name}: {}x{} @{} fps, {} kbit/s", p.width, p.height, p.fps, p.bitrate_kbps);
        Ok(Self { params: p, kind, enc, frame, scaler, packet: ff::Packet::empty(), next_pts: 0, vaapi })
    }

    /// Encodes one captured frame, `stride` bytes per row.
    pub fn encode(&mut self, data: &[u8], stride: usize, y_invert: bool, pts_us: i64, force_idr: bool) -> Result<Vec<Encoded>> {
        let (w, h) = (self.params.width as usize, self.params.height as usize);
        let row = w * 4;
        if stride < row || data.len() < stride * h {
            bail!("capture buffer smaller than expected");
        }
        match &mut self.scaler {
            None => {
                let dst_stride = self.frame.stride(0);
                let dst = self.frame.data_mut(0);
                for y in 0..h {
                    let sy = if y_invert { h - 1 - y } else { y };
                    dst[y * dst_stride..y * dst_stride + row].copy_from_slice(&data[sy * stride..sy * stride + row]);
                }
            }
            Some(sws) => unsafe {
                let (src_ptr, src_stride) = if y_invert {
                    (data.as_ptr().add(stride * (h - 1)), -(stride as i32))
                } else {
                    (data.as_ptr(), stride as i32)
                };
                let srcs = [src_ptr, std::ptr::null(), std::ptr::null(), std::ptr::null()];
                let src_strides = [src_stride, 0, 0, 0];
                let f = self.frame.as_mut_ptr();
                let r = ff::ffi::sws_scale(
                    sws.as_mut_ptr(),
                    srcs.as_ptr(),
                    src_strides.as_ptr(),
                    0,
                    h as i32,
                    (*f).data.as_ptr(),
                    (*f).linesize.as_ptr(),
                );
                if r <= 0 {
                    bail!("sws_scale failed ({r})");
                }
            },
        }
        // Timestamps must strictly increase even if the clock is coarse.
        let pts = pts_us.max(self.next_pts);
        self.next_pts = pts + 1;
        self.frame.set_pts(Some(pts));
        self.frame.set_kind(if force_idr { ff::picture::Type::I } else { ff::picture::Type::None });
        unsafe {
            let f = self.frame.as_mut_ptr();
            if force_idr {
                (*f).flags |= ff::ffi::AV_FRAME_FLAG_KEY;
            } else {
                (*f).flags &= !(ff::ffi::AV_FRAME_FLAG_KEY);
            }
        }
        match self.vaapi.as_ref().map(|v| v.frames) {
            None => self.enc.send_frame(&self.frame).context("sending frame to encoder")?,
            Some(frames) => self.send_vaapi(frames, pts, force_idr)?,
        }
        let mut out = Vec::new();
        loop {
            match self.enc.receive_packet(&mut self.packet) {
                Ok(()) => {
                    if let Some(d) = self.packet.data() {
                        out.push(Encoded { data: d.to_vec(), keyframe: self.packet.is_key() });
                    }
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => break,
                Err(ff::Error::Eof) => break,
                Err(e) => return Err(e).context("receiving encoded packet"),
            }
        }
        Ok(out)
    }

    /// Uploads the converted frame to the graphics card and encodes it there.
    fn send_vaapi(&mut self, frames: *mut ff::ffi::AVBufferRef, pts: i64, force_idr: bool) -> Result<()> {
        let mut hw = ff::frame::Video::empty();
        unsafe {
            check("getting a VA-API frame", ff::ffi::av_hwframe_get_buffer(frames, hw.as_mut_ptr(), 0))?;
            check("uploading a frame", ff::ffi::av_hwframe_transfer_data(hw.as_mut_ptr(), self.frame.as_ptr(), 0))?;
        }
        hw.set_pts(Some(pts));
        hw.set_kind(if force_idr { ff::picture::Type::I } else { ff::picture::Type::None });
        self.enc.send_frame(&hw).context("sending frame to encoder")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params { width: 640, height: 360, src_fmt: Pixel::BGRZ, fps: 60, bitrate_kbps: 4000, render_node: "/dev/dri/renderD128".into() }
    }

    fn annex_b_has(data: &[u8], nal_type: u8) -> bool {
        data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == nal_type)
    }

    #[test]
    fn encoder_order() {
        assert_eq!(order("auto"), &[Kind::Nvenc, Kind::Vaapi, Kind::X264]);
        assert_eq!(order("x264"), &[Kind::X264]);
        assert!(!Kind::X264.hardware());
    }

    /// The phone joins at keyframes, so each must carry SPS and PPS inline.
    fn check_encoder(enc: &mut Encoder) {
        let (w, h) = (enc.params.width as usize, enc.params.height as usize);
        let stride = w * 4;
        let mut img = vec![0u8; stride * h];
        let mut packets = 0;
        for i in 0..12i64 {
            for (j, px) in img.chunks_mut(4).enumerate() {
                px[0] = (j % w) as u8;
                px[1] = (i * 20) as u8;
                px[2] = (j / w) as u8;
            }
            let force = i == 0 || i == 6;
            for e in enc.encode(&img, stride, false, i * 16_666, force).unwrap() {
                packets += 1;
                if e.keyframe {
                    assert!(annex_b_has(&e.data, 7), "keyframe without SPS");
                    assert!(annex_b_has(&e.data, 8), "keyframe without PPS");
                }
                if force && packets == i + 1 {
                    assert!(e.keyframe, "frame {i} was asked to be a keyframe");
                }
            }
        }
        assert!(packets >= 10, "should emit about one packet per frame, got {packets}");
    }

    #[test]
    fn x264_makes_joinable_keyframes() {
        init();
        check_encoder(&mut Encoder::new(params(), "x264").expect("x264"));
    }

    /// Runs where the driver exists; `monitor_probe` reports which encoder a PC gets.
    #[test]
    fn vaapi_makes_joinable_keyframes_when_available() {
        init();
        if let Ok(mut enc) = Encoder::new(params(), "vaapi") {
            check_encoder(&mut enc);
        }
    }
}
