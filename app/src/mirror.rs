//! A live mirror: a `GtkPicture` showing the slot's frames, with the pointer,
//! scroll and keyboard mapping of spec §4.7–4.8. The grid card and the focus
//! view each own one per slot; both share the slot's [`SlotInput`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use ac_config::Input;
use ac_core::SlotId;
use ac_device::SlotIo;
use ac_input::keymap::android;
use ac_input::meta::InjectMode;
use ac_input::{Letterbox, keycode_for, metastate, sends_as_text};
use ac_scrcpy::control::{POINTER_GENERIC_FINGER, POINTER_MOUSE, POINTER_VIRTUAL_FINGER};
use ac_scrcpy::{ControlMsg, KeyAction, MotionAction, Position};
use gtk::gdk;
use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;

/// A host-side action bound to a mouse button or toolbar button (the
/// `input.right_click` … `input.button9` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAction {
    Key(u32),
    BackOrScreenOn,
    Notifications,
    QuickSettings,
    CollapsePanels,
    Rotate,
    None,
}

impl DeviceAction {
    pub fn parse(s: &str) -> Self {
        match s {
            "back" => Self::BackOrScreenOn,
            "home" => Self::Key(android::HOME),
            "app_switch" => Self::Key(android::APP_SWITCH),
            "menu" => Self::Key(android::MENU),
            "power" => Self::Key(android::POWER),
            "volume_up" => Self::Key(android::VOLUME_UP),
            "volume_down" => Self::Key(android::VOLUME_DOWN),
            "notifications" => Self::Notifications,
            "quick_settings" => Self::QuickSettings,
            "collapse_panels" => Self::CollapsePanels,
            "rotate" => Self::Rotate,
            _ => Self::None,
        }
    }
}

pub struct InputPrefs {
    pub mode: InjectMode,
    pub right_click: DeviceAction,
    pub middle_click: DeviceAction,
    pub button8: DeviceAction,
    pub button9: DeviceAction,
    pub forward_hover: bool,
    pub pixels_per_notch: f64,
}

impl InputPrefs {
    pub fn new(cfg: &Input) -> Self {
        Self {
            mode: InjectMode::parse(&cfg.key_inject_mode),
            right_click: DeviceAction::parse(&cfg.right_click),
            middle_click: DeviceAction::parse(&cfg.middle_click),
            button8: DeviceAction::parse(&cfg.button8),
            button9: DeviceAction::parse(&cfg.button9),
            forward_hover: cfg.forward_hover,
            pixels_per_notch: if cfg.scroll_pixels_per_notch > 0.0 { cfg.scroll_pixels_per_notch } else { 20.0 },
        }
    }
}

/// Input state for one slot.
pub struct SlotInput {
    pub slot: SlotId,
    pub io: Arc<SlotIo>,
    prefs: Rc<InputPrefs>,
    /// Android keycode → repeat count, for keys sent DOWN and not yet UP.
    pressed: RefCell<HashMap<u32, u32>>,
    /// Called when a mirror of this slot takes keyboard focus.
    on_activate: Rc<dyn Fn(SlotId)>,
}

impl SlotInput {
    pub fn new(slot: SlotId, io: Arc<SlotIo>, prefs: Rc<InputPrefs>, on_activate: Rc<dyn Fn(SlotId)>) -> Rc<Self> {
        Rc::new(Self { slot, io, prefs, pressed: RefCell::default(), on_activate })
    }

    fn send(&self, msg: ControlMsg) {
        tracing::debug!(target: "input", slot = self.slot, ?msg);
        self.io.send(msg);
    }

    pub fn key(&self, action: KeyAction, keycode: u32, metastate: u32) {
        self.send(ControlMsg::InjectKeycode { action, keycode, repeat: 0, metastate });
    }

    /// Press (`down`) or release a mapped action. Panel actions fire on press only.
    pub fn action(&self, a: DeviceAction, down: bool) {
        let ka = if down { KeyAction::Down } else { KeyAction::Up };
        match a {
            DeviceAction::Key(code) => self.key(ka, code, 0),
            DeviceAction::BackOrScreenOn => self.send(ControlMsg::BackOrScreenOn(ka)),
            DeviceAction::Notifications if down => self.send(ControlMsg::ExpandNotificationPanel),
            DeviceAction::QuickSettings if down => self.send(ControlMsg::ExpandSettingsPanel),
            DeviceAction::CollapsePanels if down => self.send(ControlMsg::CollapsePanels),
            DeviceAction::Rotate if down => self.send(ControlMsg::RotateDevice),
            _ => {}
        }
    }

    pub fn tap_action(&self, a: DeviceAction) {
        self.action(a, true);
        self.action(a, false);
    }

    pub fn inject_text(&self, text: &str) {
        for chunk in ac_scrcpy::control::text_chunks(text) {
            self.send(ControlMsg::InjectText(chunk.to_owned()));
        }
    }

    /// Releases every key still held on the device (focus lost mid-press).
    fn release_all(&self) {
        for (code, _) in self.pressed.borrow_mut().drain() {
            self.key(KeyAction::Up, code, 0);
        }
    }

    fn letterbox(&self, w: &gtk::Widget) -> Option<Letterbox> {
        let (fw, fh) = self.io.mailbox.size()?;
        Letterbox::new(w.width() as f64, w.height() as f64, fw, fh)
    }

    fn touch(&self, action: MotionAction, pointer_id: u64, lb: &Letterbox, (x, y): (i32, i32)) {
        let pressure = if action == MotionAction::Up { 0.0 } else { 1.0 };
        let position = Position { x, y, screen_w: lb.frame_w as u16, screen_h: lb.frame_h as u16 };
        self.send(ControlMsg::InjectTouch { action, pointer_id, position, pressure, action_button: 0, buttons: 0 });
    }
}

/// A drag in progress: the letterbox at its start (so a rotation mid-drag
/// keeps mapping consistently) and whether it is a Ctrl pinch.
struct Drag {
    lb: Letterbox,
    start: (f64, f64),
    pinch: bool,
}

fn mirrored(lb: &Letterbox, (x, y): (i32, i32)) -> (i32, i32) {
    (lb.frame_w as i32 - 1 - x, lb.frame_h as i32 - 1 - y)
}

/// Builds the picture (wrapped for graphics offload) and attaches input.
/// Returns `(picture, widget to place)`.
pub fn new_mirror(input: &Rc<SlotInput>) -> (gtk::Picture, gtk::Widget) {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .build();
    picture.add_css_class("mirror");
    let w = picture.upcast_ref::<gtk::Widget>().clone();
    attach_pointer(&w, input);
    attach_scroll(&w, input);
    attach_keys(&w, input);
    let offload = gtk::GraphicsOffload::new(Some(&picture));
    offload.set_black_background(true);
    (picture, offload.upcast())
}

fn attach_pointer(w: &gtk::Widget, input: &Rc<SlotInput>) {
    // Left button: touch.
    let drag = gtk::GestureDrag::builder().button(gdk::BUTTON_PRIMARY).build();
    let state: Rc<RefCell<Option<Drag>>> = Rc::default();
    {
        let (input, state, w) = (input.clone(), state.clone(), w.clone());
        drag.connect_drag_begin(move |g, x, y| {
            w.grab_focus();
            (input.on_activate)(input.slot);
            let Some(lb) = input.letterbox(&w) else { return };
            let Some(p) = lb.map(x, y) else { return };
            g.set_state(gtk::EventSequenceState::Claimed);
            let pinch = g.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            input.touch(MotionAction::Down, POINTER_GENERIC_FINGER, &lb, p);
            if pinch {
                input.touch(MotionAction::Down, POINTER_VIRTUAL_FINGER, &lb, mirrored(&lb, p));
            }
            *state.borrow_mut() = Some(Drag { lb, start: (x, y), pinch });
        });
    }
    {
        let (input, state) = (input.clone(), state.clone());
        drag.connect_drag_update(move |_, dx, dy| {
            let st = state.borrow();
            let Some(d) = st.as_ref() else { return };
            let p = d.lb.map_clamped(d.start.0 + dx, d.start.1 + dy);
            input.touch(MotionAction::Move, POINTER_GENERIC_FINGER, &d.lb, p);
            if d.pinch {
                input.touch(MotionAction::Move, POINTER_VIRTUAL_FINGER, &d.lb, mirrored(&d.lb, p));
            }
        });
    }
    let end = {
        let (input, state) = (input.clone(), state.clone());
        move |dx: f64, dy: f64| {
            let Some(d) = state.borrow_mut().take() else { return };
            let p = d.lb.map_clamped(d.start.0 + dx, d.start.1 + dy);
            if d.pinch {
                input.touch(MotionAction::Up, POINTER_VIRTUAL_FINGER, &d.lb, mirrored(&d.lb, p));
            }
            input.touch(MotionAction::Up, POINTER_GENERIC_FINGER, &d.lb, p);
        }
    };
    {
        let end = end.clone();
        drag.connect_drag_end(move |_, dx, dy| end(dx, dy));
    }
    drag.connect_cancel(move |g, _| {
        let (dx, dy) = g.offset().unwrap_or((0.0, 0.0));
        end(dx, dy);
    });
    w.add_controller(drag);

    // Other buttons: navigation (spec §4.7 table). Press/release map to
    // DOWN/UP so holding works like on the phone.
    let click = gtk::GestureClick::builder().button(0).build();
    let held: Rc<Cell<Option<DeviceAction>>> = Rc::default();
    {
        let (input, held, w) = (input.clone(), held.clone(), w.clone());
        click.connect_pressed(move |g, n, _, _| {
            let p = &input.prefs;
            let a = match g.current_button() {
                gdk::BUTTON_PRIMARY => return,
                gdk::BUTTON_MIDDLE => p.middle_click,
                gdk::BUTTON_SECONDARY => p.right_click,
                8 => p.button8,
                9 if n >= 2 && p.button9 == DeviceAction::Notifications => DeviceAction::QuickSettings,
                9 => p.button9,
                _ => return,
            };
            w.grab_focus();
            (input.on_activate)(input.slot);
            g.set_state(gtk::EventSequenceState::Claimed);
            input.action(a, true);
            held.set(Some(a));
        });
    }
    {
        let (input, held) = (input.clone(), held.clone());
        click.connect_released(move |_, _, _, _| {
            if let Some(a) = held.take() {
                input.action(a, false);
            }
        });
    }
    {
        let input = input.clone();
        click.connect_stopped(move |_| {
            if let Some(a) = held.take() {
                input.action(a, false);
            }
        });
    }
    w.add_controller(click);
}

fn attach_scroll(w: &gtk::Widget, input: &Rc<SlotInput>) {
    let last = Rc::new(Cell::new((0.0f64, 0.0f64)));
    let motion = gtk::EventControllerMotion::new();
    {
        let last = last.clone();
        motion.connect_enter(move |_, x, y| last.set((x, y)));
    }
    {
        let (last, input, w) = (last.clone(), input.clone(), w.clone());
        motion.connect_motion(move |_, x, y| {
            last.set((x, y));
            if input.prefs.forward_hover
                && let Some(lb) = input.letterbox(&w)
                && let Some((px, py)) = lb.map(x, y)
            {
                let position = Position { x: px, y: py, screen_w: lb.frame_w as u16, screen_h: lb.frame_h as u16 };
                input.send(ControlMsg::InjectTouch {
                    action: MotionAction::HoverMove,
                    pointer_id: POINTER_MOUSE,
                    position,
                    pressure: 0.0,
                    action_button: 0,
                    buttons: 0,
                });
            }
        });
    }
    w.add_controller(motion);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let input = input.clone();
    let w2 = w.clone();
    scroll.connect_scroll(move |c, dx, dy| {
        let Some(lb) = input.letterbox(&w2) else { return gtk::glib::Propagation::Proceed };
        let (x, y) = last.get();
        let Some((px, py)) = lb.map(x, y) else { return gtk::glib::Propagation::Proceed };
        let div = if c.unit() == gdk::ScrollUnit::Wheel { 1.0 } else { input.prefs.pixels_per_notch };
        let position = Position { x: px, y: py, screen_w: lb.frame_w as u16, screen_h: lb.frame_h as u16 };
        input.send(ControlMsg::InjectScroll {
            position,
            hscroll: (-dx / div) as f32,
            vscroll: (-dy / div) as f32,
            buttons: 0,
        });
        gtk::glib::Propagation::Stop
    });
    w.add_controller(scroll);
}

fn is_modifier(key: gdk::Key) -> bool {
    use gdk::Key;
    matches!(
        key,
        Key::Shift_L
            | Key::Shift_R
            | Key::Control_L
            | Key::Control_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Super_L
            | Key::Super_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::ISO_Level3_Shift
            | Key::Caps_Lock
            | Key::Num_Lock
    )
}

fn attach_keys(w: &gtk::Widget, input: &Rc<SlotInput>) {
    let keys = gtk::EventControllerKey::new();
    let raw = input.prefs.mode == InjectMode::Raw;
    let im = (!raw).then(gtk::IMMulticontext::new);
    if let Some(im) = &im {
        im.set_client_widget(Some(w));
        keys.set_im_context(Some(im));
        let input = input.clone();
        // Ctrl/Alt combinations never reach commit (the IM passes them on to
        // key-pressed), so the modifier check in `sends_as_text` is moot here.
        im.connect_commit(move |_, text| {
            if sends_as_text(input.prefs.mode, text, 0) {
                input.inject_text(text);
            }
        });
    }
    {
        let input = input.clone();
        keys.connect_key_pressed(move |_, keyval, _, state| {
            if !raw && is_modifier(keyval) {
                // Sent as meta state on the keys they modify.
                return gtk::glib::Propagation::Proceed;
            }
            let Some(code) = keycode_for(keyval.to_lower().into_glib()) else {
                tracing::debug!("no Android keycode for {:?}", keyval.name());
                return gtk::glib::Propagation::Proceed;
            };
            let repeat = {
                let mut p = input.pressed.borrow_mut();
                let r = p.entry(code).and_modify(|r| *r += 1).or_insert(0);
                *r
            };
            input.send(ControlMsg::InjectKeycode {
                action: KeyAction::Down,
                keycode: code,
                repeat,
                metastate: metastate(state.bits()),
            });
            gtk::glib::Propagation::Stop
        });
    }
    {
        let input = input.clone();
        keys.connect_key_released(move |_, keyval, _, state| {
            let Some(code) = keycode_for(keyval.to_lower().into_glib()) else { return };
            if input.pressed.borrow_mut().remove(&code).is_some() {
                input.key(KeyAction::Up, code, metastate(state.bits()));
            }
        });
    }
    w.add_controller(keys);

    let focus = gtk::EventControllerFocus::new();
    {
        let (im, input) = (im.clone(), input.clone());
        focus.connect_enter(move |_| {
            if let Some(im) = &im {
                im.focus_in();
            }
            (input.on_activate)(input.slot);
        });
    }
    let input = input.clone();
    focus.connect_leave(move |_| {
        if let Some(im) = &im {
            im.focus_out();
        }
        input.release_all();
    });
    w.add_controller(focus);
}
