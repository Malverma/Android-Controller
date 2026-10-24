//! Control messages, client → device (spec §4.6). Layouts follow
//! `app/src/control_msg.c` of scrcpy 4.1; all integers are big-endian.

/// `SC_CONTROL_MSG_INJECT_TEXT_MAX_LENGTH`.
pub const INJECT_TEXT_MAX: usize = 300;
/// `SC_CONTROL_MSG_CLIPBOARD_TEXT_MAX_LENGTH` (256 KiB message − 14 header bytes).
pub const CLIPBOARD_TEXT_MAX: usize = (1 << 18) - 14;
/// `SC_CONTROL_MSG_SCAN_FILE_PATH_MAX_LENGTH`.
pub const SCAN_FILE_PATH_MAX: usize = 256;

pub const POINTER_MOUSE: u64 = u64::MAX; // -1
pub const POINTER_GENERIC_FINGER: u64 = u64::MAX - 1; // -2
pub const POINTER_VIRTUAL_FINGER: u64 = u64::MAX - 2; // -3

/// `android.view.KeyEvent` / `MotionEvent` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyAction {
    Down = 0,
    Up = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MotionAction {
    Down = 0,
    Up = 1,
    Move = 2,
    Cancel = 3,
    HoverMove = 7,
}

/// `MotionEvent.BUTTON_*`.
pub mod buttons {
    pub const PRIMARY: u32 = 1;
    pub const SECONDARY: u32 = 2;
    pub const TERTIARY: u32 = 4;
    pub const BACK: u32 = 8;
    pub const FORWARD: u32 = 16;
}

/// A point in the device's current capture coordinate space. The server
/// drops events whose `screen_w/h` don't match its capture size, which makes
/// events sent during a rotation harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub screen_w: u16,
    pub screen_h: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CopyKey {
    None = 0,
    Copy = 1,
    Cut = 2,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlMsg {
    InjectKeycode { action: KeyAction, keycode: u32, repeat: u32, metastate: u32 },
    InjectText(String),
    InjectTouch { action: MotionAction, pointer_id: u64, position: Position, pressure: f32, action_button: u32, buttons: u32 },
    /// `hscroll`/`vscroll` in notches, clamped to `[-16, 16]`.
    InjectScroll { position: Position, hscroll: f32, vscroll: f32, buttons: u32 },
    BackOrScreenOn(KeyAction),
    ExpandNotificationPanel,
    ExpandSettingsPanel,
    CollapsePanels,
    GetClipboard(CopyKey),
    SetClipboard { sequence: u64, paste: bool, text: String },
    SetDisplayPower(bool),
    RotateDevice,
    OpenHardKeyboardSettings,
    StartApp(String),
    ResetVideo,
    ScanFile(String),
}

mod ty {
    pub const INJECT_KEYCODE: u8 = 0;
    pub const INJECT_TEXT: u8 = 1;
    pub const INJECT_TOUCH_EVENT: u8 = 2;
    pub const INJECT_SCROLL_EVENT: u8 = 3;
    pub const BACK_OR_SCREEN_ON: u8 = 4;
    pub const EXPAND_NOTIFICATION_PANEL: u8 = 5;
    pub const EXPAND_SETTINGS_PANEL: u8 = 6;
    pub const COLLAPSE_PANELS: u8 = 7;
    pub const GET_CLIPBOARD: u8 = 8;
    pub const SET_CLIPBOARD: u8 = 9;
    pub const SET_DISPLAY_POWER: u8 = 10;
    pub const ROTATE_DEVICE: u8 = 11;
    // 12..=14: UHID create/input/destroy (not used).
    pub const OPEN_HARD_KEYBOARD_SETTINGS: u8 = 15;
    pub const START_APP: u8 = 16;
    pub const RESET_VIDEO: u8 = 17;
    // 18..=21: camera torch/zoom, resize display (not used).
    pub const SCAN_FILE: u8 = 22;
}

/// `sc_float_to_u16fp`: `[0, 1]` → `0..=0xFFFF` (1.0 saturates).
pub fn u16fp(f: f32) -> u16 {
    let u = (f.clamp(0.0, 1.0) * 65536.0) as u32;
    u.min(0xFFFF) as u16
}

/// `sc_float_to_i16fp`: `[-1, 1]` → `-0x8000..=0x7FFF` (1.0 saturates).
pub fn i16fp(f: f32) -> i16 {
    let i = (f.clamp(-1.0, 1.0) * 32768.0) as i32;
    i.min(0x7FFF) as i16
}

/// Longest prefix of `s` that fits in `max` bytes without splitting a
/// UTF-8 sequence.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn put_string(buf: &mut Vec<u8>, s: &str, max: usize) {
    let s = truncate_utf8(s, max);
    buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// `write_string_tiny`: one length byte.
fn put_string_tiny(buf: &mut Vec<u8>, s: &str, max: usize) {
    let s = truncate_utf8(s, max);
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
}

fn put_position(buf: &mut Vec<u8>, p: &Position) {
    buf.extend_from_slice(&p.x.to_be_bytes());
    buf.extend_from_slice(&p.y.to_be_bytes());
    buf.extend_from_slice(&p.screen_w.to_be_bytes());
    buf.extend_from_slice(&p.screen_h.to_be_bytes());
}

impl ControlMsg {
    /// Appends the wire form of the message to `buf`.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Self::InjectKeycode { action, keycode, repeat, metastate } => {
                buf.push(ty::INJECT_KEYCODE);
                buf.push(*action as u8);
                buf.extend_from_slice(&keycode.to_be_bytes());
                buf.extend_from_slice(&repeat.to_be_bytes());
                buf.extend_from_slice(&metastate.to_be_bytes());
            }
            Self::InjectText(text) => {
                buf.push(ty::INJECT_TEXT);
                put_string(buf, text, INJECT_TEXT_MAX);
            }
            Self::InjectTouch { action, pointer_id, position, pressure, action_button, buttons } => {
                buf.push(ty::INJECT_TOUCH_EVENT);
                buf.push(*action as u8);
                buf.extend_from_slice(&pointer_id.to_be_bytes());
                put_position(buf, position);
                buf.extend_from_slice(&u16fp(*pressure).to_be_bytes());
                buf.extend_from_slice(&action_button.to_be_bytes());
                buf.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::InjectScroll { position, hscroll, vscroll, buttons } => {
                buf.push(ty::INJECT_SCROLL_EVENT);
                put_position(buf, position);
                buf.extend_from_slice(&i16fp(hscroll / 16.0).to_be_bytes());
                buf.extend_from_slice(&i16fp(vscroll / 16.0).to_be_bytes());
                buf.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::BackOrScreenOn(action) => {
                buf.push(ty::BACK_OR_SCREEN_ON);
                buf.push(*action as u8);
            }
            Self::ExpandNotificationPanel => buf.push(ty::EXPAND_NOTIFICATION_PANEL),
            Self::ExpandSettingsPanel => buf.push(ty::EXPAND_SETTINGS_PANEL),
            Self::CollapsePanels => buf.push(ty::COLLAPSE_PANELS),
            Self::GetClipboard(key) => {
                buf.push(ty::GET_CLIPBOARD);
                buf.push(*key as u8);
            }
            Self::SetClipboard { sequence, paste, text } => {
                buf.push(ty::SET_CLIPBOARD);
                buf.extend_from_slice(&sequence.to_be_bytes());
                buf.push(u8::from(*paste));
                put_string(buf, text, CLIPBOARD_TEXT_MAX);
            }
            Self::SetDisplayPower(on) => {
                buf.push(ty::SET_DISPLAY_POWER);
                buf.push(u8::from(*on));
            }
            Self::RotateDevice => buf.push(ty::ROTATE_DEVICE),
            Self::OpenHardKeyboardSettings => buf.push(ty::OPEN_HARD_KEYBOARD_SETTINGS),
            Self::StartApp(name) => {
                buf.push(ty::START_APP);
                put_string_tiny(buf, name, 255);
            }
            Self::ResetVideo => buf.push(ty::RESET_VIDEO),
            Self::ScanFile(path) => {
                buf.push(ty::SCAN_FILE);
                put_string(buf, path, SCAN_FILE_PATH_MAX);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }

    /// The pointer a `MOVE` belongs to, for coalescing (spec §4.7): a queued
    /// move may be replaced by a newer move of the same pointer.
    pub fn coalescible_move(&self) -> Option<u64> {
        match self {
            Self::InjectTouch { action: MotionAction::Move | MotionAction::HoverMove, pointer_id, .. } => Some(*pointer_id),
            _ => None,
        }
    }

    /// Convenience: a full key press (DOWN then UP).
    pub fn key_press(keycode: u32, metastate: u32) -> [Self; 2] {
        [
            Self::InjectKeycode { action: KeyAction::Down, keycode, repeat: 0, metastate },
            Self::InjectKeycode { action: KeyAction::Up, keycode, repeat: 0, metastate },
        ]
    }
}

/// Splits text for `INJECT_TEXT` into chunks of at most 300 bytes on UTF-8
/// boundaries.
pub fn text_chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let chunk = truncate_utf8(rest, INJECT_TEXT_MAX);
        // A single char is at most 4 bytes, so a chunk is never empty.
        out.push(chunk);
        rest = &rest[chunk.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(msg: &ControlMsg) -> String {
        msg.to_bytes().iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
    }

    const POS: Position = Position { x: 540, y: 1200, screen_w: 1080, screen_h: 2400 };

    #[test]
    fn touch_down_center() {
        // Spec §4.6 golden example.
        let m = ControlMsg::InjectTouch {
            action: MotionAction::Down,
            pointer_id: POINTER_GENERIC_FINGER,
            position: POS,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        };
        assert_eq!(
            hex(&m),
            "02 00 ff ff ff ff ff ff ff fe 00 00 02 1c 00 00 04 b0 04 38 09 60 ff ff 00 00 00 00 00 00 00 00"
        );
        assert_eq!(m.to_bytes().len(), 32);
    }

    #[test]
    fn keycode() {
        let m = ControlMsg::InjectKeycode { action: KeyAction::Up, keycode: 66, repeat: 2, metastate: 0x1001 };
        assert_eq!(hex(&m), "00 01 00 00 00 42 00 00 00 02 00 00 10 01");
    }

    #[test]
    fn text_and_truncation() {
        assert_eq!(hex(&ControlMsg::InjectText("hé".into())), "01 00 00 00 03 68 c3 a9");
        // 299 ASCII + a 2-byte char: the char does not fit in 300 bytes.
        let s = format!("{}é", "a".repeat(299));
        let b = ControlMsg::InjectText(s).to_bytes();
        assert_eq!(&b[1..5], &299u32.to_be_bytes());
        assert_eq!(b.len(), 5 + 299);
    }

    #[test]
    fn text_chunking_respects_utf8() {
        let s = "é".repeat(400); // 800 bytes
        let chunks = text_chunks(&s);
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![300, 300, 200]);
        assert_eq!(chunks.concat(), s);
        assert!(text_chunks("").is_empty());
    }

    #[test]
    fn scroll() {
        let m = ControlMsg::InjectScroll { position: POS, hscroll: 0.0, vscroll: -1.0, buttons: 0 };
        // vscroll -1 notch → -1/16 → i16fp = -2048 = 0xF800.
        assert_eq!(hex(&m), "03 00 00 02 1c 00 00 04 b0 04 38 09 60 00 00 f8 00 00 00 00 00");
        let big = ControlMsg::InjectScroll { position: POS, hscroll: 40.0, vscroll: -40.0, buttons: 0 }.to_bytes();
        assert_eq!(&big[13..17], &[0x7f, 0xff, 0x80, 0x00], "clamped to ±16 notches");
    }

    #[test]
    fn simple_messages() {
        assert_eq!(hex(&ControlMsg::BackOrScreenOn(KeyAction::Down)), "04 00");
        assert_eq!(hex(&ControlMsg::ExpandNotificationPanel), "05");
        assert_eq!(hex(&ControlMsg::ExpandSettingsPanel), "06");
        assert_eq!(hex(&ControlMsg::CollapsePanels), "07");
        assert_eq!(hex(&ControlMsg::GetClipboard(CopyKey::Cut)), "08 02");
        assert_eq!(hex(&ControlMsg::SetDisplayPower(true)), "0a 01");
        assert_eq!(hex(&ControlMsg::RotateDevice), "0b");
        assert_eq!(hex(&ControlMsg::OpenHardKeyboardSettings), "0f");
        assert_eq!(hex(&ControlMsg::StartApp("org.app".into())), "10 07 6f 72 67 2e 61 70 70");
        assert_eq!(hex(&ControlMsg::ResetVideo), "11");
    }

    #[test]
    fn set_clipboard() {
        let m = ControlMsg::SetClipboard { sequence: 0x0102030405060708, paste: true, text: "hi".into() };
        assert_eq!(hex(&m), "09 01 02 03 04 05 06 07 08 01 00 00 00 02 68 69");
    }

    #[test]
    fn scan_file() {
        let m = ControlMsg::ScanFile("/sdcard/a.jpg".into());
        assert_eq!(m.to_bytes()[0], 22);
        assert_eq!(&m.to_bytes()[1..5], &13u32.to_be_bytes());
        let long = ControlMsg::ScanFile("x".repeat(300)).to_bytes();
        assert_eq!(long.len(), 1 + 4 + SCAN_FILE_PATH_MAX);
    }

    #[test]
    fn fixed_point_edges() {
        assert_eq!(u16fp(1.0), 0xFFFF);
        assert_eq!(u16fp(0.0), 0);
        assert_eq!(u16fp(0.5), 0x8000);
        assert_eq!(u16fp(2.0), 0xFFFF, "out of range clamps");
        assert_eq!(u16fp(-1.0), 0);
        assert_eq!(i16fp(1.0), 0x7FFF);
        assert_eq!(i16fp(-1.0), -0x8000);
        assert_eq!(i16fp(0.0), 0);
        assert_eq!(i16fp(3.0), 0x7FFF);
        assert_eq!(i16fp(-3.0), -0x8000);
    }

    #[test]
    fn moves_coalesce_per_pointer() {
        let mv = |id| ControlMsg::InjectTouch {
            action: MotionAction::Move,
            pointer_id: id,
            position: POS,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        };
        assert_eq!(mv(POINTER_GENERIC_FINGER).coalescible_move(), Some(POINTER_GENERIC_FINGER));
        assert_eq!(ControlMsg::ResetVideo.coalescible_move(), None);
    }
}
