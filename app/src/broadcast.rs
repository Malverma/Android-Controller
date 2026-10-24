//! Broadcast input (spec §4.10): input on a selected tile goes to every
//! selected phone that is mirroring, with pointer positions mapped to each
//! phone's own frame (spec §4.5 normalization).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use ac_config::Input;
use ac_core::SlotId;
use ac_device::SlotIo;
use ac_input::broadcast::{orientation_differs, remap};
use ac_input::keymap::android;
use ac_scrcpy::{ControlMsg, KeyAction, Position};

pub struct Broadcast {
    enabled: Cell<bool>,
    /// Bit `slot - 1` set for each target.
    targets: Cell<u8>,
    ios: Vec<Arc<SlotIo>>,
    include_power: bool,
    skip_mismatched: bool,
    /// Called when a broadcast Power press is withheld.
    pub on_power_blocked: RefCell<Option<Box<dyn Fn()>>>,
}

impl Broadcast {
    pub fn new(ios: Vec<Arc<SlotIo>>, cfg: &Input) -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            targets: Cell::new(0),
            ios,
            include_power: cfg.broadcast_includes_power,
            skip_mismatched: cfg.broadcast_skip_mismatched_orientation,
            on_power_blocked: RefCell::default(),
        })
    }

    pub fn skips_mismatched(&self) -> bool {
        self.skip_mismatched
    }

    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
    }

    pub fn is_target(&self, slot: SlotId) -> bool {
        (1..=8).contains(&slot) && self.targets.get() & (1 << (slot - 1)) != 0
    }

    pub fn set_target(&self, slot: SlotId, on: bool) {
        if !(1..=self.ios.len() as SlotId).contains(&slot) {
            return;
        }
        let bit = 1u8 << (slot - 1);
        let t = self.targets.get();
        self.targets.set(if on { t | bit } else { t & !bit });
    }

    pub fn targets(&self) -> Vec<SlotId> {
        (1..=self.ios.len() as SlotId).filter(|&s| self.is_target(s)).collect()
    }

    pub fn set_targets(&self, slots: &[SlotId]) {
        self.targets.set(0);
        for &s in slots {
            self.set_target(s, true);
        }
    }

    /// Whether input on `slot` is fanned out.
    pub fn active_for(&self, slot: SlotId) -> bool {
        self.enabled.get() && self.is_target(slot)
    }

    fn io(&self, slot: SlotId) -> Option<&Arc<SlotIo>> {
        self.ios.get((slot as usize).wrapping_sub(1))
    }

    /// Sends input that arrived on `source`'s tile.
    pub fn send(&self, source: SlotId, msg: ControlMsg) {
        if !self.active_for(source) {
            if let Some(io) = self.io(source) {
                io.send(msg);
            }
            return;
        }
        if let ControlMsg::InjectKeycode { action, keycode, .. } = &msg
            && *keycode == android::POWER
            && !self.include_power
        {
            if *action == KeyAction::Down
                && let Some(f) = self.on_power_blocked.borrow().as_ref()
            {
                f();
            }
            return;
        }
        let src = frame_of(&msg).or_else(|| self.io(source).and_then(|io| io.mailbox.size()));
        // Every target in the same loop iteration: host-side skew stays well
        // under a millisecond.
        for t in self.targets() {
            let Some(io) = self.io(t) else { continue };
            if !io.is_streaming() {
                continue;
            }
            let dst = io.mailbox.size();
            if t != source
                && self.skip_mismatched
                && let (Some(s), Some(d)) = (src, dst)
                && orientation_differs(s, d)
            {
                continue;
            }
            let m = if t == source { Some(msg.clone()) } else { retarget(&msg, dst) };
            if let Some(m) = m {
                io.send(m);
            }
        }
    }
}

/// The frame a pointer message was stamped with.
fn frame_of(msg: &ControlMsg) -> Option<(u32, u32)> {
    match msg {
        ControlMsg::InjectTouch { position, .. } | ControlMsg::InjectScroll { position, .. } => {
            Some((position.screen_w as u32, position.screen_h as u32))
        }
        _ => None,
    }
}

/// `msg` for a phone showing a `dst` frame: pointer positions are remapped
/// (and dropped while that phone has no frame); everything else is unchanged.
pub fn retarget(msg: &ControlMsg, dst: Option<(u32, u32)>) -> Option<ControlMsg> {
    let mut m = msg.clone();
    if let ControlMsg::InjectTouch { position, .. } | ControlMsg::InjectScroll { position, .. } = &mut m {
        let (w, h) = dst?;
        let (x, y) = remap(position.x, position.y, (position.screen_w as u32, position.screen_h as u32), (w, h));
        *position = Position { x, y, screen_w: w as u16, screen_h: h as u16 };
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_scrcpy::MotionAction;
    use ac_scrcpy::control::POINTER_GENERIC_FINGER;

    fn touch(x: i32, y: i32, w: u16, h: u16) -> ControlMsg {
        ControlMsg::InjectTouch {
            action: MotionAction::Down,
            pointer_id: POINTER_GENERIC_FINGER,
            position: Position { x, y, screen_w: w, screen_h: h },
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        }
    }

    #[test]
    fn pointer_events_are_remapped_per_target() {
        let m = retarget(&touch(540, 1200, 1080, 2400), Some((1440, 3200))).unwrap();
        assert_eq!(m, touch(720, 1600, 1440, 3200));
        let scroll = ControlMsg::InjectScroll {
            position: Position { x: 0, y: 2399, screen_w: 1080, screen_h: 2400 },
            hscroll: 0.0,
            vscroll: -1.0,
            buttons: 0,
        };
        let ControlMsg::InjectScroll { position, .. } = retarget(&scroll, Some((720, 1600))).unwrap() else { panic!() };
        assert_eq!((position.x, position.y, position.screen_w, position.screen_h), (0, 1599, 720, 1600));
    }

    #[test]
    fn pointer_needs_a_target_frame_but_keys_do_not() {
        assert!(retarget(&touch(1, 1, 10, 10), None).is_none());
        let key = ControlMsg::InjectKeycode { action: KeyAction::Down, keycode: 4, repeat: 0, metastate: 0 };
        assert_eq!(retarget(&key, None), Some(key.clone()));
        let text = ControlMsg::InjectText("hi".into());
        assert_eq!(retarget(&text, Some((1, 1))), Some(text.clone()));
    }

    #[test]
    fn target_bookkeeping() {
        let ios: Vec<_> = (0..4).map(|_| Arc::new(SlotIo::default())).collect();
        let b = Broadcast::new(ios, &Input::default());
        b.set_targets(&[1, 3, 9]);
        assert_eq!(b.targets(), vec![1, 3], "slot 9 does not exist");
        assert!(!b.active_for(1), "off until enabled");
        b.set_enabled(true);
        assert!(b.active_for(1) && !b.active_for(2));
        b.set_target(3, false);
        b.set_target(2, true);
        assert_eq!(b.targets(), vec![1, 2]);
    }

    #[test]
    fn broadcast_power_is_withheld() {
        let ios: Vec<_> = (0..2).map(|_| Arc::new(SlotIo::default())).collect();
        let b = Broadcast::new(ios, &Input::default());
        b.set_targets(&[1, 2]);
        b.set_enabled(true);
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        *b.on_power_blocked.borrow_mut() = Some(Box::new(move || h.set(h.get() + 1)));
        let power = |action| ControlMsg::InjectKeycode { action, keycode: android::POWER, repeat: 0, metastate: 0 };
        b.send(1, power(KeyAction::Down));
        b.send(1, power(KeyAction::Up));
        assert_eq!(hits.get(), 1, "one notice per press");
    }
}
