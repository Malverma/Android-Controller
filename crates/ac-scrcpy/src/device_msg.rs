//! Device → client messages on the control socket (`app/src/device_msg.c`).

use crate::ProtocolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceMsg {
    Clipboard(String),
    AckClipboard(u64),
    UhidOutput { id: u16, data: Vec<u8> },
}

/// Parses one message from the front of `buf`. `Ok(None)` means more bytes
/// are needed; `Ok(Some((msg, n)))` consumed `n` bytes.
pub fn parse(buf: &[u8]) -> Result<Option<(DeviceMsg, usize)>, ProtocolError> {
    let Some(&ty) = buf.first() else { return Ok(None) };
    match ty {
        0 => {
            let Some(len) = buf.get(1..5) else { return Ok(None) };
            let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
            if len > crate::control::CLIPBOARD_TEXT_MAX {
                return Err(ProtocolError::Oversize(len));
            }
            let Some(text) = buf.get(5..5 + len) else { return Ok(None) };
            Ok(Some((DeviceMsg::Clipboard(String::from_utf8_lossy(text).into_owned()), 5 + len)))
        }
        1 => {
            let Some(seq) = buf.get(1..9) else { return Ok(None) };
            Ok(Some((DeviceMsg::AckClipboard(u64::from_be_bytes(seq.try_into().unwrap())), 9)))
        }
        2 => {
            let Some(hdr) = buf.get(1..5) else { return Ok(None) };
            let id = u16::from_be_bytes([hdr[0], hdr[1]]);
            let size = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
            let Some(data) = buf.get(5..5 + size) else { return Ok(None) };
            Ok(Some((DeviceMsg::UhidOutput { id, data: data.to_vec() }, 5 + size)))
        }
        other => Err(ProtocolError::UnknownDeviceMsg(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_needs_whole_message() {
        let msg = [0, 0, 0, 0, 3, b'a', b'b', b'c', 0xAA];
        assert_eq!(parse(&msg[..4]).unwrap(), None);
        assert_eq!(parse(&msg[..7]).unwrap(), None);
        assert_eq!(parse(&msg).unwrap(), Some((DeviceMsg::Clipboard("abc".into()), 8)));
    }

    #[test]
    fn ack_and_uhid() {
        let ack = [1, 0, 0, 0, 0, 0, 0, 1, 2];
        assert_eq!(parse(&ack).unwrap(), Some((DeviceMsg::AckClipboard(0x102), 9)));
        let uhid = [2, 0, 7, 0, 2, 9, 8];
        assert_eq!(parse(&uhid).unwrap(), Some((DeviceMsg::UhidOutput { id: 7, data: vec![9, 8] }, 7)));
        assert_eq!(parse(&[]).unwrap(), None);
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(matches!(parse(&[9]), Err(ProtocolError::UnknownDeviceMsg(9))));
    }
}
