//! Video socket demuxer (spec §4.1, upstream `app/src/demuxer.c`).
//!
//! Blocking: it runs on the slot's `video-decode-<slot>` thread.

use std::io::{self, Read};

use crate::ProtocolError;

const FLAG_SESSION: u8 = 0x80;
const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = FLAG_KEY_FRAME - 1;
/// Spec §4.1: anything larger is a protocol error.
pub const MAX_PACKET: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    pub fn from_id(id: u32) -> Option<Self> {
        match &id.to_be_bytes() {
            b"h264" => Some(Self::H264),
            b"h265" => Some(Self::H265),
            b"\0av1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// The `video_codec=` server option.
    pub fn option(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxItem {
    /// Capture (re)started: new frame size, the input coordinate space.
    Session { width: u32, height: u32, client_resized: bool },
    /// One access unit, with any preceding config (SPS/PPS) prepended:
    /// the first `config_len` bytes of `data` (a recorder's extradata).
    Packet { data: Vec<u8>, pts_us: u64, key_frame: bool, config_len: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum DemuxError {
    #[error("video socket: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// Codec id 0: the device disabled the stream.
    #[error("the device disabled the video stream")]
    Disabled,
    /// Codec id 1: the encoder failed to start on the device.
    #[error("the device could not start its video encoder")]
    EncoderFailed,
}

pub struct Demuxer<R> {
    r: R,
    pending_config: Option<Vec<u8>>,
    bytes_read: u64,
}

impl<R: Read> Demuxer<R> {
    pub fn new(r: R) -> Self {
        Self { r, pending_config: None, bytes_read: 0 }
    }

    /// Total bytes read from the socket, for bitrate stats.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.r.read_exact(buf)?;
        self.bytes_read += buf.len() as u64;
        Ok(())
    }

    /// Reads the 64-byte device name (forward tunnel: after the dummy byte,
    /// which the launcher already consumed).
    pub fn read_device_name(&mut self) -> Result<String, DemuxError> {
        let mut name = [0u8; 64];
        self.read_exact(&mut name)?;
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Ok(String::from_utf8_lossy(&name[..end]).into_owned())
    }

    /// Reads the `u32` codec id stream header.
    pub fn read_codec(&mut self) -> Result<VideoCodec, DemuxError> {
        let mut id = [0u8; 4];
        self.read_exact(&mut id)?;
        match u32::from_be_bytes(id) {
            0 => Err(DemuxError::Disabled),
            1 => Err(DemuxError::EncoderFailed),
            id => VideoCodec::from_id(id).ok_or(DemuxError::Protocol(ProtocolError::UnknownCodec(id))),
        }
    }

    /// Next session change or media packet. Config packets are buffered and
    /// prepended to the following packet, like upstream.
    pub fn next_item(&mut self) -> Result<DemuxItem, DemuxError> {
        loop {
            let mut h = [0u8; 12];
            self.read_exact(&mut h)?;
            if h[0] & FLAG_SESSION != 0 {
                let width = u32::from_be_bytes(h[4..8].try_into().unwrap());
                let height = u32::from_be_bytes(h[8..12].try_into().unwrap());
                if width == 0 || height == 0 {
                    return Err(ProtocolError::BadSession(width, height).into());
                }
                return Ok(DemuxItem::Session { width, height, client_resized: h[3] & 1 != 0 });
            }
            let flags = u64::from_be_bytes(h[0..8].try_into().unwrap());
            let len = u32::from_be_bytes(h[8..12].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_PACKET {
                return Err(ProtocolError::Oversize(len).into());
            }
            let mut data = vec![0u8; len];
            self.read_exact(&mut data)?;
            if flags & FLAG_CONFIG != 0 {
                match &mut self.pending_config {
                    Some(c) => c.extend_from_slice(&data),
                    None => self.pending_config = Some(data),
                }
                continue;
            }
            let mut config_len = 0;
            if let Some(mut config) = self.pending_config.take() {
                config_len = config.len();
                config.extend_from_slice(&data);
                data = config;
            }
            return Ok(DemuxItem::Packet { data, pts_us: flags & PTS_MASK, key_frame: flags & FLAG_KEY_FRAME != 0, config_len });
        }
    }

    pub fn into_inner(self) -> R {
        self.r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(w: u32, h: u32, resized: bool) -> Vec<u8> {
        let mut v = vec![0x80, 0, 0, u8::from(resized)];
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    fn media(flags: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = flags.to_be_bytes().to_vec();
        v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn config_is_prepended_and_flags_parsed() {
        let mut s = b"h264".to_vec();
        s.extend(session(1080, 2400, false));
        s.extend(media(FLAG_CONFIG, b"SPS"));
        s.extend(media(FLAG_KEY_FRAME | 1000, b"IDR"));
        s.extend(media(2000, b"P"));
        s.extend(session(2400, 1080, true));
        let mut d = Demuxer::new(&s[..]);
        assert_eq!(d.read_codec().unwrap(), VideoCodec::H264);
        assert_eq!(d.next_item().unwrap(), DemuxItem::Session { width: 1080, height: 2400, client_resized: false });
        assert_eq!(d.next_item().unwrap(), DemuxItem::Packet { data: b"SPSIDR".to_vec(), pts_us: 1000, key_frame: true, config_len: 3 });
        assert_eq!(d.next_item().unwrap(), DemuxItem::Packet { data: b"P".to_vec(), pts_us: 2000, key_frame: false, config_len: 0 });
        assert_eq!(d.next_item().unwrap(), DemuxItem::Session { width: 2400, height: 1080, client_resized: true });
        assert!(matches!(d.next_item(), Err(DemuxError::Io(_))), "EOF");
    }

    #[test]
    fn special_codec_ids() {
        assert!(matches!(Demuxer::new(&[0u8, 0, 0, 0][..]).read_codec(), Err(DemuxError::Disabled)));
        assert!(matches!(Demuxer::new(&[0u8, 0, 0, 1][..]).read_codec(), Err(DemuxError::EncoderFailed)));
        assert_eq!(Demuxer::new(&b"h265"[..]).read_codec().unwrap(), VideoCodec::H265);
        assert_eq!(Demuxer::new(&b"\0av1"[..]).read_codec().unwrap(), VideoCodec::Av1);
    }

    #[test]
    fn oversize_and_zero_size_rejected() {
        let mut s = 0u64.to_be_bytes().to_vec();
        s.extend_from_slice(&((MAX_PACKET + 1) as u32).to_be_bytes());
        assert!(matches!(Demuxer::new(&s[..]).next_item(), Err(DemuxError::Protocol(ProtocolError::Oversize(_)))));
        assert!(matches!(
            Demuxer::new(&session(0, 10, false)[..]).next_item(),
            Err(DemuxError::Protocol(ProtocolError::BadSession(0, 10)))
        ));
    }
}
