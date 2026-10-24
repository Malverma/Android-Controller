//! GDK keyval → Android `KeyEvent.KEYCODE_*` (spec §4.8 table).

/// Android keycodes used outside the table (hardware buttons).
pub mod android {
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const VOLUME_UP: u32 = 24;
    pub const VOLUME_DOWN: u32 = 25;
    pub const POWER: u32 = 26;
    pub const MENU: u32 = 82;
    pub const APP_SWITCH: u32 = 187;
}

/// Maps a GDK keyval (from `gdk/gdkkeysyms.h`) to an Android keycode.
pub fn keycode_for(keyval: u32) -> Option<u32> {
    Some(match keyval {
        0x61..=0x7a => 29 + (keyval - 0x61), // a..z
        0x41..=0x5a => 29 + (keyval - 0x41), // A..Z
        0x30..=0x39 => 7 + (keyval - 0x30),  // 0..9
        0xffb0..=0xffb9 => 144 + (keyval - 0xffb0), // KP_0..KP_9
        0xffbe..=0xffc9 => 131 + (keyval - 0xffbe), // F1..F12
        0xff0d => 66,          // Return
        0xff8d => 160,         // KP_Enter
        0xff08 => 67,          // BackSpace → DEL
        0xffff => 112,         // Delete → FORWARD_DEL
        0xff09 | 0xfe20 => 61, // Tab, ISO_Left_Tab
        0x20 => 62,            // space
        0xff1b => 111,         // Escape
        0xff52 => 19,          // Up
        0xff54 => 20,          // Down
        0xff51 => 21,          // Left
        0xff53 => 22,          // Right
        0xff50 => 122,         // Home → MOVE_HOME
        0xff57 => 123,         // End → MOVE_END
        0xff55 => 92,          // Page_Up
        0xff56 => 93,          // Page_Down
        0xff63 => 124,         // Insert
        0xffe5 => 115,         // Caps_Lock
        0xffe1 => 59,          // Shift_L
        0xffe2 => 60,          // Shift_R
        0xffe3 => 113,         // Control_L
        0xffe4 => 114,         // Control_R
        0xffe9 => 57,          // Alt_L
        0xffea => 58,          // Alt_R
        0xffeb => 117,         // Super_L → META_LEFT
        0xffec => 118,         // Super_R → META_RIGHT
        0x2c => 55,            // comma
        0x2e => 56,            // period
        0x2d => 69,            // minus
        0x3d => 70,            // equal
        0x5b => 71,            // bracketleft
        0x5d => 72,            // bracketright
        0x5c => 73,            // backslash
        0x3b => 74,            // semicolon
        0x27 => 75,            // apostrophe
        0x2f => 76,            // slash
        0x60 => 68,            // grave
        0x1008ff13 => 24,      // XF86AudioRaiseVolume
        0x1008ff11 => 25,      // XF86AudioLowerVolume
        0x1008ff12 => 164,     // XF86AudioMute → VOLUME_MUTE
        0x1008ff14 => 85,      // XF86AudioPlay → MEDIA_PLAY_PAUSE
        0x1008ff17 => 87,      // XF86AudioNext
        0x1008ff16 => 88,      // XF86AudioPrev
        0xff67 => 82,          // Menu
        0xff61 => 120,         // Print → SYSRQ
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_row() {
        // (keyval, keycode) for each row of spec §4.8.
        let rows: &[(u32, u32)] = &[
            (0x61, 29), (0x7a, 54), (0x41, 29), (0x5a, 54), (0x30, 7), (0x39, 16),
            (0xff0d, 66), (0xff8d, 160), (0xff08, 67), (0xffff, 112), (0xff09, 61), (0xfe20, 61),
            (0x20, 62), (0xff1b, 111), (0xff52, 19), (0xff54, 20), (0xff51, 21), (0xff53, 22),
            (0xff50, 122), (0xff57, 123), (0xff55, 92), (0xff56, 93), (0xff63, 124),
            (0xffbe, 131), (0xffc9, 142), (0xffe5, 115), (0xffe1, 59), (0xffe2, 60),
            (0xffe3, 113), (0xffe4, 114), (0xffe9, 57), (0xffea, 58), (0xffeb, 117), (0xffec, 118),
            (0x2c, 55), (0x2e, 56), (0x2d, 69), (0x3d, 70), (0x5b, 71), (0x5d, 72), (0x5c, 73),
            (0x3b, 74), (0x27, 75), (0x2f, 76), (0x60, 68), (0xffb0, 144), (0xffb9, 153),
            (0x1008ff13, 24), (0x1008ff11, 25), (0x1008ff12, 164), (0x1008ff14, 85),
            (0x1008ff17, 87), (0x1008ff16, 88), (0xff67, 82), (0xff61, 120),
        ];
        for &(keyval, code) in rows {
            assert_eq!(keycode_for(keyval), Some(code), "keyval {keyval:#x}");
        }
    }

    #[test]
    fn unmapped_keys() {
        assert_eq!(keycode_for(0xe9), None, "é has no keycode; it goes as text");
        assert_eq!(keycode_for(0xffca), None, "F13");
    }
}
