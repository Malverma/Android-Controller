//! `state.toml` (spec §7.6): window and view state the app restores on launch.

use ac_core::SlotId;
use serde::{Deserialize, Serialize};

use crate::config::SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    pub schema_version: u32,
    pub window: WindowState,
    /// `"grid"` or `"focus"`.
    pub view: String,
    /// Slot shown in the focus view; 0 = none.
    pub focused_slot: SlotId,
    pub broadcast: BroadcastState,
    /// `"qr"`, `"code"` or `"manual"`.
    pub last_pair_tab: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            window: WindowState::default(),
            view: "grid".into(),
            focused_slot: 0,
            broadcast: BroadcastState::default(),
            last_pair_tab: "qr".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self { width: 1280, height: 820, maximized: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BroadcastState {
    pub enabled: bool,
    pub targets: Vec<SlotId>,
}

impl AppState {
    /// Drops values that no longer make sense (e.g. after lowering `max_slots`).
    pub fn normalize(&mut self, slots: u8) {
        let valid = |s: SlotId| (1..=slots).contains(&s);
        if !valid(self.focused_slot) {
            self.focused_slot = 0;
        }
        if self.view != "focus" || self.focused_slot == 0 {
            self.view = "grid".into();
        }
        self.broadcast.targets.retain(|&s| valid(s));
        self.broadcast.targets.sort_unstable();
        self.broadcast.targets.dedup();
        self.window.width = self.window.width.clamp(360, 16384);
        self.window.height = self.window.height.clamp(360, 16384);
    }
}
