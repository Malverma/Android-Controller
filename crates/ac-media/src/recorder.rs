//! Recording a phone's mirror to MP4, on the PC: the stream's packets are
//! muxed as they arrive (no re-encoding), so there is no time limit and
//! phones without ADB record too. Like upstream scrcpy's recorder, the
//! codec config (SPS/PPS…) becomes the file's extradata.
//!
//! Fragmented MP4: a file cut short by a crash or power loss still plays.
//! A new capture size or codec (rotation, profile switch) starts a new part.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc;

use ac_scrcpy::VideoCodec;
use ffmpeg_sys_next as ff;

use crate::decoder::av_error;

const AVIO_FLAG_WRITE: i32 = 2;
const PADDING: usize = 64;
const MICROS: ff::AVRational = ff::AVRational { num: 1, den: 1_000_000 };

/// From the decode thread to the recorder thread.
pub enum RecMsg {
    /// What the following packets are (sent when recording starts and
    /// whenever the stream sends new config).
    Config { codec: VideoCodec, width: u32, height: u32, config: Vec<u8> },
    /// One access unit, without its config bytes.
    Packet { data: Vec<u8>, pts_us: u64, key: bool },
    Stop,
}

/// Called once with the files written (empty when nothing was recorded).
pub type OnDone = Box<dyn FnOnce(Result<Vec<PathBuf>, String>) + Send>;

/// Starts the recorder thread; `stem` is the path without `.mp4`.
pub fn spawn(stem: PathBuf, on_done: OnDone) -> std::io::Result<mpsc::Sender<RecMsg>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("recorder".into()).spawn(move || on_done(record(&stem, &rx)))?;
    Ok(tx)
}

fn record(stem: &Path, rx: &mpsc::Receiver<RecMsg>) -> Result<Vec<PathBuf>, String> {
    let mut parts = Vec::new();
    let mut mux: Option<Muxer> = None;
    let mut want: Option<(VideoCodec, u32, u32, Vec<u8>)> = None;
    let mut error = None;
    // A closed channel (stream gone for good) also ends the recording.
    while let Ok(msg) = rx.recv() {
        match msg {
            RecMsg::Config { codec, width, height, config } => {
                let new = (codec, width, height, config);
                if mux.as_ref().is_some_and(|m| m.format != new) {
                    if let Some(m) = mux.take() {
                        parts.push(m.finish()?);
                    }
                }
                want = Some(new);
            }
            RecMsg::Packet { data, pts_us, key } => {
                if mux.is_none() {
                    // Start on a key frame, once the config is known.
                    let Some(f) = want.as_ref().filter(|_| key) else { continue };
                    match Muxer::open(&part_path(stem, parts.len()), f.clone()) {
                        Ok(m) => mux = Some(m),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
                if let Some(m) = &mut mux
                    && let Err(e) = m.write(&data, pts_us, key)
                {
                    error = Some(e);
                    break;
                }
            }
            RecMsg::Stop => break,
        }
    }
    if let Some(m) = mux.take() {
        parts.push(m.finish()?);
    }
    match error {
        Some(e) if parts.is_empty() => Err(e),
        Some(e) => {
            tracing::warn!("recording stopped early: {e}");
            Ok(parts)
        }
        None => Ok(parts),
    }
}

/// `stem.mp4`, then `stem (2).mp4`, … (never overwriting a file).
fn part_path(stem: &Path, index: usize) -> PathBuf {
    let base = stem.as_os_str().to_string_lossy().into_owned();
    let mut n = index + 1;
    loop {
        let p = if n == 1 { PathBuf::from(format!("{base}.mp4")) } else { PathBuf::from(format!("{base} ({n}).mp4")) };
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

struct Muxer {
    ctx: *mut ff::AVFormatContext,
    st: *mut ff::AVStream,
    path: PathBuf,
    format: (VideoCodec, u32, u32, Vec<u8>),
    first_pts: Option<u64>,
    last_pts: i64,
    /// Held back until the next one tells its duration (fragmented MP4
    /// wants it on every packet).
    held: Option<(Vec<u8>, i64, bool)>,
    last_duration: i64,
}

fn check(r: i32, what: &str) -> Result<i32, String> {
    if r < 0 { Err(format!("{what}: {}", av_error(r))) } else { Ok(r) }
}

impl Muxer {
    fn open(path: &Path, format: (VideoCodec, u32, u32, Vec<u8>)) -> Result<Self, String> {
        let (codec, width, height, config) = &format;
        let cpath = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| "bad file name".to_owned())?;
        // SAFETY: standard libavformat output setup; every pointer is checked
        // and freed on the error paths.
        unsafe {
            let mut ctx = ptr::null_mut();
            check(ff::avformat_alloc_output_context2(&mut ctx, ptr::null(), c"mp4".as_ptr(), cpath.as_ptr()), "MP4 muxer")?;
            let fail = |ctx: *mut ff::AVFormatContext, e: String| {
                if !(*ctx).pb.is_null() {
                    ff::avio_closep(&mut (*ctx).pb);
                }
                ff::avformat_free_context(ctx);
                Err(e)
            };
            let st = ff::avformat_new_stream(ctx, ptr::null());
            if st.is_null() {
                return fail(ctx, "out of memory".into());
            }
            let par = (*st).codecpar;
            (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = match codec {
                VideoCodec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
                VideoCodec::H265 => ff::AVCodecID::AV_CODEC_ID_HEVC,
                VideoCodec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
            };
            (*par).width = *width as i32;
            (*par).height = *height as i32;
            if !config.is_empty() {
                let extra = ff::av_mallocz(config.len() + PADDING) as *mut u8;
                if extra.is_null() {
                    return fail(ctx, "out of memory".into());
                }
                ptr::copy_nonoverlapping(config.as_ptr(), extra, config.len());
                (*par).extradata = extra;
                (*par).extradata_size = config.len() as i32;
            }
            (*st).time_base = MICROS;
            if let Err(e) = check(ff::avio_open(&mut (*ctx).pb, cpath.as_ptr(), AVIO_FLAG_WRITE), "create file") {
                return fail(ctx, e);
            }
            let mut opts = ptr::null_mut();
            ff::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+frag_keyframe+empty_moov+default_base_moof".as_ptr(), 0);
            let r = ff::avformat_write_header(ctx, &mut opts);
            ff::av_dict_free(&mut opts);
            if let Err(e) = check(r, "MP4 header") {
                return fail(ctx, e);
            }
            tracing::info!("recording to {}", path.display());
            Ok(Self { ctx, st, path: path.to_owned(), format, first_pts: None, last_pts: -1, held: None, last_duration: 33_333 })
        }
    }

    fn write(&mut self, data: &[u8], pts_us: u64, key: bool) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let first = *self.first_pts.get_or_insert(pts_us);
        // Timestamps must increase; a repeated or older one is nudged.
        let pts = (pts_us.saturating_sub(first) as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        match self.held.replace((data.to_vec(), pts, key)) {
            Some((prev, prev_pts, prev_key)) => {
                self.last_duration = pts - prev_pts;
                self.write_now(&prev, prev_pts, self.last_duration, prev_key)
            }
            None => Ok(()),
        }
    }

    fn write_now(&mut self, data: &[u8], pts: i64, duration: i64, key: bool) -> Result<(), String> {
        // SAFETY: the packet is allocated with padding, filled, handed to the
        // muxer (which takes its reference) and freed.
        unsafe {
            let mut pkt = ff::av_packet_alloc();
            if pkt.is_null() {
                return Err("out of memory".into());
            }
            let r = check(ff::av_new_packet(pkt, data.len() as i32), "packet");
            if let Err(e) = r {
                ff::av_packet_free(&mut pkt);
                return Err(e);
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*pkt).data, data.len());
            (*pkt).pts = pts;
            (*pkt).dts = pts;
            (*pkt).duration = duration;
            (*pkt).stream_index = 0;
            if key {
                (*pkt).flags |= ff::AV_PKT_FLAG_KEY;
            }
            ff::av_packet_rescale_ts(pkt, MICROS, (*self.st).time_base);
            let r = ff::av_interleaved_write_frame(self.ctx, pkt);
            ff::av_packet_free(&mut pkt);
            check(r, "write").map(|_| ())
        }
    }

    fn finish(mut self) -> Result<PathBuf, String> {
        if let Some((data, pts, key)) = self.held.take() {
            self.write_now(&data, pts, self.last_duration, key)?;
        }
        // SAFETY: header was written in `open`; closing frees everything once.
        let r = unsafe { ff::av_write_trailer(self.ctx) };
        self.close();
        check(r, "MP4 trailer")?;
        Ok(std::mem::take(&mut self.path))
    }

    fn close(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        // SAFETY: we own the context and its I/O.
        unsafe {
            ff::avio_closep(&mut (*self.ctx).pb);
            ff::avformat_free_context(self.ctx);
        }
        self.ctx = ptr::null_mut();
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_names_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("ac-rec-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stem = dir.join("Moto 21-50-03");
        assert_eq!(part_path(&stem, 0), dir.join("Moto 21-50-03.mp4"));
        std::fs::write(dir.join("Moto 21-50-03.mp4"), b"").unwrap();
        assert_eq!(part_path(&stem, 0), dir.join("Moto 21-50-03 (2).mp4"));
        assert_eq!(part_path(&stem, 2), dir.join("Moto 21-50-03 (3).mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
