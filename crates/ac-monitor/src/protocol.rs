//! The helper's monitor connections (`Monitor.java`): JSON in the helper's
//! length-prefixed framing, and the video frame header.

use std::io::{self, Read, Write};

use serde_json::Value;

pub const SOCKET: &str = "localabstract:test-bed-multi-monitor";
/// The first helper with monitor mode and the locator.
pub const MIN_HELPER_VERSION: u32 = 8;
const MAX_JSON: u32 = 1 << 20;
const KEY_FLAG: u64 = 1 << 62;
const PTS_MASK: u64 = KEY_FLAG - 1;

pub fn write_json(w: &mut impl Write, v: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(v)?;
    let mut msg = Vec::with_capacity(body.len() + 4);
    msg.extend_from_slice(&(body.len() as u32).to_be_bytes());
    msg.extend_from_slice(&body);
    w.write_all(&msg)?;
    w.flush()
}

pub fn read_json(r: &mut impl Read) -> io::Result<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len);
    if !(2..=MAX_JSON).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad message length {n}")));
    }
    let mut body = vec![0u8; n as usize];
    r.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

/// The header before each video frame.
pub fn frame_header(pts_us: u64, keyframe: bool, size: u32) -> [u8; 12] {
    let word = (pts_us & PTS_MASK) | if keyframe { KEY_FLAG } else { 0 };
    let mut h = [0u8; 12];
    h[..8].copy_from_slice(&word.to_be_bytes());
    h[8..].copy_from_slice(&size.to_be_bytes());
    h
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub avc: bool,
    /// The phone's button was pressed while the PC was away.
    pub requested: bool,
}

/// What the helper sends on the control connection.
#[derive(Debug, Clone, PartialEq)]
pub enum FromPhone {
    Hello(Hello),
    /// Its button: on or off.
    Request { on: bool },
    Touch(crate::touch::TouchEvent),
    Keyframe,
    Pong,
    Other,
}

pub fn parse_message(v: &Value) -> FromPhone {
    let int = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    match v.get("op").and_then(Value::as_str) {
        Some("hello") => FromPhone::Hello(Hello {
            version: int("version"),
            width: int("width"),
            height: int("height"),
            avc: flag("avc"),
            requested: flag("requested"),
        }),
        Some("monitor_request") => FromPhone::Request { on: flag("on") },
        Some("touch") => crate::touch::TouchEvent::from_json(v).map_or(FromPhone::Other, FromPhone::Touch),
        Some("keyframe") => FromPhone::Keyframe,
        Some("pong") => FromPhone::Pong,
        _ => FromPhone::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trip_and_limits() {
        let mut buf = Vec::new();
        write_json(&mut buf, &json!({"op":"ping"})).unwrap();
        assert_eq!(&buf[..4], &(buf.len() as u32 - 4).to_be_bytes());
        assert_eq!(read_json(&mut buf.as_slice()).unwrap(), json!({"op":"ping"}));

        let bad = (MAX_JSON + 1).to_be_bytes();
        assert!(read_json(&mut bad.as_slice()).is_err());
    }

    #[test]
    fn frame_header_matches_the_helper() {
        let h = frame_header(5, true, 7);
        assert_eq!(&h[..8], &[0x40, 0, 0, 0, 0, 0, 0, 5]);
        assert_eq!(&h[8..], &[0, 0, 0, 7]);
        assert_eq!(frame_header(5, false, 7)[0], 0);
        // A huge timestamp cannot reach the flag bits.
        assert_eq!(frame_header(u64::MAX, false, 0)[0], 0x3f);
    }

    #[test]
    fn parses_messages() {
        let hello = json!({"op":"hello","version":8,"width":1080,"height":2400,"density":420,"avc":true,"requested":true});
        assert_eq!(
            parse_message(&hello),
            FromPhone::Hello(Hello { version: 8, width: 1080, height: 2400, avc: true, requested: true })
        );
        assert_eq!(parse_message(&json!({"op":"monitor_request","on":false})), FromPhone::Request { on: false });
        assert_eq!(parse_message(&json!({"op":"keyframe"})), FromPhone::Keyframe);
        assert_eq!(parse_message(&json!({"op":"future"})), FromPhone::Other);
        assert_eq!(parse_message(&json!({"op":"touch","a":"bad"})), FromPhone::Other);
    }
}
