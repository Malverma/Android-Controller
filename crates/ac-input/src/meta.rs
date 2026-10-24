//! Modifier state (spec §4.8 "Meta state") and the mixed text/keycode rule.

/// `GdkModifierType` bits (GTK 4).
pub mod gdk {
    pub const SHIFT: u32 = 1 << 0;
    pub const LOCK: u32 = 1 << 1;
    pub const CONTROL: u32 = 1 << 2;
    pub const ALT: u32 = 1 << 3;
    pub const SUPER: u32 = 1 << 26;
}

/// Android `KeyEvent.META_*`.
pub mod android {
    pub const SHIFT_ON: u32 = 0x1;
    pub const ALT_ON: u32 = 0x2;
    pub const ALT_LEFT_ON: u32 = 0x10;
    pub const SHIFT_LEFT_ON: u32 = 0x40;
    pub const CTRL_ON: u32 = 0x1000;
    pub const CTRL_LEFT_ON: u32 = 0x2000;
    pub const META_ON: u32 = 0x10000;
    pub const META_LEFT_ON: u32 = 0x20000;
    pub const CAPS_LOCK_ON: u32 = 0x100000;
}

/// Android meta state for a GDK modifier mask.
pub fn metastate(mods: u32) -> u32 {
    use android::*;
    let mut m = 0;
    if mods & gdk::SHIFT != 0 {
        m |= SHIFT_ON | SHIFT_LEFT_ON;
    }
    if mods & gdk::CONTROL != 0 {
        m |= CTRL_ON | CTRL_LEFT_ON;
    }
    if mods & gdk::ALT != 0 {
        m |= ALT_ON | ALT_LEFT_ON;
    }
    if mods & gdk::SUPER != 0 {
        m |= META_ON | META_LEFT_ON;
    }
    if mods & gdk::LOCK != 0 {
        m |= CAPS_LOCK_ON;
    }
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// Printable IME commits go as text; everything else as keycodes.
    Mixed,
    /// Like mixed (shifted letters are already text in mixed mode).
    Text,
    /// Never text: letters go as keycodes (games).
    Raw,
}

impl InjectMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "text" => Self::Text,
            "raw" => Self::Raw,
            _ => Self::Mixed,
        }
    }
}

/// Whether an IME commit should be injected as `INJECT_TEXT` rather than key
/// events: only printable text, typed without Ctrl/Alt/Super.
pub fn sends_as_text(mode: InjectMode, commit: &str, mods: u32) -> bool {
    if mode == InjectMode::Raw || commit.is_empty() {
        return false;
    }
    let shortcut = mods & (gdk::CONTROL | gdk::ALT | gdk::SUPER) != 0;
    !shortcut && commit.chars().all(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition() {
        assert_eq!(metastate(0), 0);
        assert_eq!(metastate(gdk::SHIFT), 0x41);
        assert_eq!(metastate(gdk::CONTROL), 0x3000);
        assert_eq!(metastate(gdk::ALT), 0x12);
        assert_eq!(metastate(gdk::SUPER), 0x30000);
        assert_eq!(metastate(gdk::LOCK), 0x100000);
        assert_eq!(metastate(gdk::SHIFT | gdk::CONTROL), 0x3041);
    }

    #[test]
    fn text_rule() {
        assert!(sends_as_text(InjectMode::Mixed, "a", 0));
        assert!(sends_as_text(InjectMode::Mixed, "A", gdk::SHIFT));
        assert!(sends_as_text(InjectMode::Mixed, "é", 0));
        assert!(!sends_as_text(InjectMode::Mixed, "a", gdk::CONTROL), "Ctrl+A is a shortcut");
        assert!(!sends_as_text(InjectMode::Mixed, "\t", 0));
        assert!(!sends_as_text(InjectMode::Raw, "a", 0));
        assert!(!sends_as_text(InjectMode::Mixed, "", 0));
        assert_eq!(InjectMode::parse("raw"), InjectMode::Raw);
        assert_eq!(InjectMode::parse("bogus"), InjectMode::Mixed);
    }
}
