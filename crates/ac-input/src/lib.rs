//! Host → Android input mapping (spec §4.5, §4.7, §4.8). Pure functions over
//! GDK keyvals and modifier bits, so this crate needs no GTK.

pub mod keymap;
pub mod letterbox;
pub mod meta;

pub use keymap::keycode_for;
pub use letterbox::Letterbox;
pub use meta::{metastate, sends_as_text};
