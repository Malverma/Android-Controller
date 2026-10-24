//! Main window: one card per slot (`general.max_slots`), the "Nearby" list,
//! a status bar, and the focus view (one phone large, with a hardware-button
//! rail).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ac_core::SlotId;
use ac_device::{ManagerHandle, NearbyView, PhaseView, SlotIo, SlotView, StreamProfile, UiCommand, UiEvent, UsbDeviceView};
use ac_discovery::ServiceKind;
use ac_input::keymap::android;
use ac_media::MediaContext;
use ac_scrcpy::ControlMsg;
use ac_scrcpy::control::{CLIPBOARD_TEXT_MAX, truncate_utf8};
use adw::prelude::*;
use gtk::{gdk, glib};

use crate::mirror::{DeviceAction, InputPrefs, SlotInput, new_mirror};
use crate::pairing::{Page, PairingDialog};
use crate::slot_card::{CardAction, SlotCard};
use crate::texture;

const CSS: &str = r#"
.slot-card { padding: 12px; min-width: 150px; }
.slot-number { font-weight: 800; min-width: 22px; border-radius: 6px; background: alpha(currentColor, 0.1); }
.badge { border-radius: 99px; padding: 1px 8px; font-size: smaller; font-weight: bold; }
.badge.online { background: alpha(@success_color, 0.18); color: @success_color; }
.badge.pairing, .badge.connecting { background: alpha(@accent_color, 0.18); color: @accent_color; }
.badge.reconnecting { background: alpha(@warning_color, 0.2); color: @warning_color; }
.badge.disconnected, .badge.empty { background: alpha(currentColor, 0.1); }
.phone-frame { border-radius: 20px; border: 2px solid alpha(currentColor, 0.12); background: alpha(currentColor, 0.03); min-height: 240px; padding: 12px; }
.video-frame { border-radius: 16px; background: black; }
.mirror:focus-visible { outline: 2px solid @accent_color; outline-offset: -2px; }
.rail { padding: 12px 8px; }
.rail button { min-width: 44px; min-height: 44px; }
.qr { border-radius: 12px; }
.statusbar { padding: 4px 12px; }
"#;

#[derive(Default)]
struct State {
    slots: Vec<Option<SlotView>>,
    nearby: Vec<NearbyView>,
    usb: Vec<UsbDeviceView>,
    adb: Option<Result<u32, String>>,
    discovery_error: Option<String>,
}

pub struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    cards: Vec<SlotCard>,
    io: Vec<Arc<SlotIo>>,
    media: Option<Arc<MediaContext>>,
    inputs: Vec<Rc<SlotInput>>,
    /// The focus view's mirror per slot (the grid's live in the cards).
    focus_pictures: Vec<gtk::Picture>,
    views: gtk::Stack,
    focus_stack: gtk::Stack,
    window_title: adw::WindowTitle,
    grid_button: gtk::Button,
    focused: Cell<Option<SlotId>>,
    /// The slot keyboard shortcuts act on: the last one clicked or focused.
    active: Cell<Option<SlotId>>,
    video: RefCell<Vec<VideoBook>>,
    clipboard_autosync: bool,
    /// Last text the phone put on the clipboard, to avoid echoing it back.
    last_from_device: RefCell<Option<String>>,
    nearby_list: gtk::ListBox,
    status: gtk::Label,
    commands: async_channel::Sender<UiCommand>,
    state: RefCell<State>,
    pairing: RefCell<Option<Rc<PairingDialog>>>,
}

/// Per-slot presentation bookkeeping.
#[derive(Default, Clone)]
struct VideoBook {
    seq: u64,
    presented_at_last_tick: u64,
    tile_px: u32,
    screen_off: bool,
}


pub fn build(app: &adw::Application, manager: ManagerHandle, cfg: &ac_config::Config, slots: u8, config_error: Option<String>) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = adw::ApplicationWindow::builder().application(app).title("Android Controller").default_width(1280).default_height(820).build();
    let toasts = adw::ToastOverlay::new();

    let header = adw::HeaderBar::new();
    let window_title = adw::WindowTitle::new("Android Controller", "");
    header.set_title_widget(Some(&window_title));
    let add = gtk::Button::builder().label("Add Device").tooltip_text("Pair or connect a phone (Ctrl+N)").build();
    add.add_css_class("suggested-action");
    add.set_action_name(Some("win.pair"));
    header.pack_start(&add);
    let grid_button = gtk::Button::builder()
        .icon_name("view-grid-symbolic")
        .label("All phones")
        .tooltip_text("Back to all phones (Alt+G)")
        .visible(false)
        .build();
    header.pack_start(&grid_button);

    // Slots.
    let grid = gtk::Box::builder().spacing(12).homogeneous(true).margin_top(12).margin_start(12).margin_end(12).vexpand(true).build();

    // Nearby.
    let nearby_group = adw::PreferencesGroup::builder()
        .title("Nearby")
        .description("Phones advertising wireless debugging on this network")
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    let nearby_list = gtk::ListBox::new();
    nearby_list.add_css_class("boxed-list");
    nearby_list.set_selection_mode(gtk::SelectionMode::None);
    nearby_group.add(&nearby_list);
    let nearby_scroll = gtk::ScrolledWindow::builder()
        .child(&nearby_group)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(260)
        .build();

    let status = gtk::Label::builder().xalign(0.0).build();
    status.add_css_class("caption");
    status.add_css_class("dim-label");
    status.add_css_class("statusbar");

    // Five cards need ~900 px (four ~720 px); scroll sideways rather than clip when a tiling
    // compositor gives the window less.
    let grid_scroll = gtk::ScrolledWindow::builder()
        .child(&grid)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&grid_scroll);
    content.append(&nearby_scroll);

    // Focus view: one phone, large, with the hardware buttons beside it.
    let focus_stack = gtk::Stack::builder().hexpand(true).vexpand(true).margin_top(12).margin_bottom(12).margin_start(12).build();
    let rail = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).build();
    rail.add_css_class("rail");
    let focus_page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    focus_page.append(&focus_stack);
    focus_page.append(&rail);

    let views = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).build();
    views.add_named(&content, Some("grid"));
    views.add_named(&focus_page, Some("focus"));
    toasts.set_child(Some(&views));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    toolbar.add_bottom_bar(&status);
    window.set_content(Some(&toolbar));

    let app_rc = Rc::new_cyclic(|weak: &std::rc::Weak<App>| {
        let on_action: Rc<dyn Fn(SlotId, CardAction)> = {
            let weak = weak.clone();
            Rc::new(move |slot, action| {
                if let Some(app) = weak.upgrade() {
                    app.card_action(slot, action);
                }
            })
        };
        let on_activate: Rc<dyn Fn(SlotId)> = {
            let weak = weak.clone();
            Rc::new(move |slot| {
                if let Some(app) = weak.upgrade() {
                    app.active.set(Some(slot));
                }
            })
        };
        let prefs = Rc::new(InputPrefs::new(&cfg.input));
        let inputs: Vec<Rc<SlotInput>> =
            (1..=slots).map(|s| SlotInput::new(s, manager.io[s as usize - 1].clone(), prefs.clone(), on_activate.clone())).collect();
        let mut cards = Vec::new();
        let mut focus_pictures = Vec::new();
        for input in &inputs {
            let (picture, widget) = new_mirror(input);
            let card = SlotCard::new(input.slot, on_action.clone(), picture, widget);
            grid.append(&card.root);
            cards.push(card);
            let (picture, widget) = new_mirror(input);
            widget.add_css_class("video-frame");
            focus_stack.add_named(&widget, Some(&input.slot.to_string()));
            focus_pictures.push(picture);
        }
        App {
            window: window.clone(),
            toasts,
            cards,
            io: manager.io.clone(),
            media: manager.media.clone(),
            inputs,
            focus_pictures,
            views: views.clone(),
            focus_stack: focus_stack.clone(),
            window_title: window_title.clone(),
            grid_button: grid_button.clone(),
            focused: Cell::new(None),
            active: Cell::new(None),
            video: RefCell::new(vec![VideoBook::default(); slots as usize]),
            clipboard_autosync: cfg.input.clipboard_autosync,
            last_from_device: RefCell::new(None),
            nearby_list,
            status,
            commands: manager.commands.clone(),
            state: RefCell::new(State { slots: vec![None; slots as usize], ..Default::default() }),
            pairing: RefCell::new(None),
        }
    });

    install_actions(&app_rc, app);
    install_shortcuts(&app_rc, cfg);
    build_rail(&app_rc, &rail);
    {
        let weak = Rc::downgrade(&app_rc);
        grid_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.leave_focus();
            }
        });
    }

    // Frames: at most one texture per slot per monitor refresh.
    {
        let weak = Rc::downgrade(&app_rc);
        window.add_tick_callback(move |_, _| match weak.upgrade() {
            Some(app) => {
                app.present_frames();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }
    {
        let weak = Rc::downgrade(&app_rc);
        glib::timeout_add_local(Duration::from_secs(1), move || match weak.upgrade() {
            Some(app) => {
                app.video_tick();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    // Host clipboard → phone (spec §4.9).
    {
        let weak = Rc::downgrade(&app_rc);
        window.clipboard().connect_changed(move |clip| {
            if let Some(app) = weak.upgrade()
                && app.clipboard_autosync
                && !clip.is_local()
            {
                app.with_clipboard_text(|app, slot, text| {
                    if app.last_from_device.borrow().as_deref() != Some(text) {
                        app.set_device_clipboard(slot, text, false);
                    }
                });
            }
        });
    }

    app_rc.render_nearby();
    app_rc.render_status();
    if let Some(msg) = config_error {
        app_rc.toast(&msg);
    }

    // Manager → UI.
    let weak = Rc::downgrade(&app_rc);
    let events = manager.events.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = events.recv().await {
            let Some(app) = weak.upgrade() else { break };
            app.on_event(ev);
        }
    });

    // The window's close handler owns the App and the manager: everything
    // else holds weak references, so this keeps both alive exactly as long
    // as the window.
    let owned = RefCell::new(Some((app_rc.clone(), manager)));
    window.connect_close_request(move |_| {
        if let Some((_app, manager)) = owned.borrow_mut().take() {
            manager.abort();
        }
        glib::Propagation::Proceed
    });
    window.present();
}

/// Hardware buttons beside the focused phone. Press and release map to
/// DOWN/UP, so holding Power opens the power menu (spec §4.8).
fn build_rail(app: &Rc<App>, rail: &gtk::Box) {
    let buttons: [(&str, &str, DeviceAction); 10] = [
        ("go-previous-symbolic", "Back (Alt+B)", DeviceAction::BackOrScreenOn),
        ("go-home-symbolic", "Home (Alt+H)", DeviceAction::Key(android::HOME)),
        ("view-paged-symbolic", "Recent apps (Alt+S)", DeviceAction::Key(android::APP_SWITCH)),
        ("audio-volume-high-symbolic", "Volume up (Alt+Up)", DeviceAction::Key(android::VOLUME_UP)),
        ("audio-volume-low-symbolic", "Volume down (Alt+Down)", DeviceAction::Key(android::VOLUME_DOWN)),
        ("system-shutdown-symbolic", "Power — hold for the power menu (Alt+P)", DeviceAction::Key(android::POWER)),
        ("open-menu-symbolic", "Menu (Alt+M)", DeviceAction::Key(android::MENU)),
        ("object-rotate-right-symbolic", "Rotate (Alt+R)", DeviceAction::Rotate),
        ("preferences-system-notifications-symbolic", "Notifications (Alt+N)", DeviceAction::Notifications),
        ("emblem-system-symbolic", "Quick settings (Alt+Shift+N)", DeviceAction::QuickSettings),
    ];
    for (icon, tip, action) in buttons {
        let b = gtk::Button::builder().icon_name(icon).tooltip_text(tip).focusable(false).build();
        b.add_css_class("circular");
        let press = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).propagation_phase(gtk::PropagationPhase::Capture).build();
        let held: Rc<RefCell<Option<Rc<SlotInput>>>> = Rc::default();
        {
            let (weak, held) = (Rc::downgrade(app), held.clone());
            press.connect_pressed(move |_, _, _, _| {
                if let Some(t) = weak.upgrade().and_then(|a| a.target()) {
                    t.action(action, true);
                    *held.borrow_mut() = Some(t);
                }
            });
        }
        let release = {
            let held = held.clone();
            move || {
                if let Some(t) = held.borrow_mut().take() {
                    t.action(action, false);
                }
            }
        };
        {
            let release = release.clone();
            press.connect_released(move |_, _, _, _| release());
        }
        press.connect_stopped(move |_| release());
        b.add_controller(press);
        rail.append(&b);
    }
}

/// Window-level shortcuts, matched before the phone's key controller so
/// they are never forwarded (spec §4.8).
fn install_shortcuts(app: &Rc<App>, cfg: &ac_config::Config) {
    let modifier = match cfg.input.shortcut_modifier.as_str() {
        "super" => "<Super>",
        "ctrl" | "control" => "<Ctrl>",
        _ => "<Alt>",
    };
    let ctl = gtk::ShortcutController::new();
    ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
    for (name, accel) in &cfg.keybindings {
        let accel = accel.replace("<Alt>", modifier);
        let Some(trigger) = gtk::ShortcutTrigger::parse_string(&accel) else {
            tracing::warn!("keybindings.{name}: cannot parse {accel:?}");
            continue;
        };
        let weak = Rc::downgrade(app);
        let name = name.clone();
        let action = gtk::CallbackAction::new(move |_, _| weak.upgrade().is_some_and(|app| app.shortcut(&name)).into());
        ctl.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    }
    app.window.add_controller(ctl);
}

impl App {
    /// Runs a shortcut; false lets the key through (e.g. no phone streaming).
    fn shortcut(self: &Rc<Self>, name: &str) -> bool {
        if let Some(n) = name.strip_prefix("focus_slot_").and_then(|n| n.parse::<SlotId>().ok()) {
            if (n as usize) > self.cards.len() || !self.streaming(n) {
                return false;
            }
            self.enter_focus(n);
            return true;
        }
        match name {
            "grid_view" if self.focused.get().is_some() => {
                self.leave_focus();
                return true;
            }
            "toggle_focus" => {
                match self.focused.get() {
                    Some(_) => self.leave_focus(),
                    None => match self.target() {
                        Some(t) => self.enter_focus(t.slot),
                        None => return false,
                    },
                }
                return true;
            }
            _ => {}
        }
        let Some(t) = self.target() else { return false };
        let tap = match name {
            "back" => DeviceAction::BackOrScreenOn,
            "home" => DeviceAction::Key(android::HOME),
            "app_switch" => DeviceAction::Key(android::APP_SWITCH),
            "volume_up" => DeviceAction::Key(android::VOLUME_UP),
            "volume_down" => DeviceAction::Key(android::VOLUME_DOWN),
            "power" => DeviceAction::Key(android::POWER),
            "menu" => DeviceAction::Key(android::MENU),
            "rotate" => DeviceAction::Rotate,
            "notifications" => DeviceAction::Notifications,
            "quick_settings" => DeviceAction::QuickSettings,
            "collapse_panels" => DeviceAction::CollapsePanels,
            "screen_off_toggle" => {
                let off = {
                    let mut books = self.video.borrow_mut();
                    let b = &mut books[t.slot as usize - 1];
                    b.screen_off = !b.screen_off;
                    b.screen_off
                };
                t.io.send(ControlMsg::SetDisplayPower(!off));
                self.toast(if off { "Phone screen off (mirroring continues)" } else { "Phone screen on" });
                return true;
            }
            "paste_clipboard" => {
                self.with_clipboard_text(|app, slot, text| app.set_device_clipboard(slot, text, true));
                return true;
            }
            "type_clipboard" => {
                let input = t.clone();
                self.with_clipboard_text(move |_, _, text| input.inject_text(text));
                return true;
            }
            _ => return false,
        };
        t.tap_action(tap);
        true
    }
}

fn install_actions(app: &Rc<App>, gapp: &adw::Application) {
    let pair = gtk::gio::SimpleAction::new("pair", None);
    let weak = Rc::downgrade(app);
    pair.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            app.open_pairing(Page::Qr, None);
        }
    });
    app.window.add_action(&pair);
    gapp.set_accels_for_action("win.pair", &["<Ctrl>n"]);

    let pair_code = gtk::gio::SimpleAction::new("pair-code", None);
    let weak = Rc::downgrade(app);
    pair_code.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            app.open_pairing(Page::Code, None);
        }
    });
    app.window.add_action(&pair_code);

    let quit = gtk::gio::SimpleAction::new("quit", None);
    let window = app.window.clone();
    quit.connect_activate(move |_, _| window.close());
    app.window.add_action(&quit);
    gapp.set_accels_for_action("win.quit", &["<Ctrl>q"]);
}

impl App {
    pub fn slot_count(&self) -> usize {
        self.cards.len()
    }

    pub fn send(&self, cmd: UiCommand) {
        // Unbounded channel: try_send only fails if the manager is gone.
        if self.commands.try_send(cmd).is_err() {
            self.toast("The device manager stopped; restart the app");
        }
    }

    pub fn nearby(&self) -> Vec<NearbyView> {
        self.state.borrow().nearby.clone()
    }

    pub fn usb(&self) -> Vec<UsbDeviceView> {
        self.state.borrow().usb.clone()
    }

    fn toast(&self, msg: &str) {
        let t = adw::Toast::new(msg);
        t.set_timeout(5);
        self.toasts.add_toast(t);
    }

    fn open_pairing(self: &Rc<Self>, page: Page, slot: Option<SlotId>) {
        if let Some(existing) = self.pairing_dialog() {
            existing.dialog.present(Some(&self.window));
            return;
        }
        let dlg = PairingDialog::new(self, page, slot);
        dlg.dialog.present(Some(&self.window));
        *self.pairing.borrow_mut() = Some(dlg);
    }

    /// Clones the dialog out so no `RefCell` borrow is held while calling into
    /// it: closing it re-enters `pairing_closed`, which needs `borrow_mut`.
    fn pairing_dialog(&self) -> Option<Rc<PairingDialog>> {
        self.pairing.borrow().clone()
    }

    pub fn pairing_closed(&self) {
        self.pairing.borrow_mut().take();
    }

    fn card_action(self: &Rc<Self>, slot: SlotId, action: CardAction) {
        match action {
            CardAction::AddDevice => self.open_pairing(Page::Qr, Some(slot)),
            CardAction::Reconnect => self.send(UiCommand::Reconnect { slot }),
            CardAction::Cancel => {
                let pairing = self.slot_view(slot).is_some_and(|v| v.phase == PhaseView::Pairing);
                self.send(if pairing { UiCommand::CancelPairing { slot } } else { UiCommand::Disconnect { slot } });
            }
            CardAction::Disconnect => self.send(UiCommand::Disconnect { slot }),
            CardAction::Focus => self.enter_focus(slot),
            CardAction::RetryStream => self.send(UiCommand::RestartStream { slot }),
            CardAction::Forget => {
                let name = self.slot_view(slot).map(|v| v.title).unwrap_or_default();
                let alert = adw::AlertDialog::new(
                    Some(&format!("Forget {name}?")),
                    Some("It will be disconnected and removed from saved devices. You can pair it again later."),
                );
                alert.add_response("cancel", "Cancel");
                alert.add_response("forget", "Forget");
                alert.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
                alert.set_default_response(Some("cancel"));
                let weak = Rc::downgrade(self);
                alert.connect_response(None, move |_, resp| {
                    if resp == "forget"
                        && let Some(app) = weak.upgrade()
                    {
                        app.send(UiCommand::Forget { slot });
                    }
                });
                alert.present(Some(&self.window));
            }
        }
    }

    fn slot_view(&self, slot: SlotId) -> Option<SlotView> {
        self.state.borrow().slots.get(slot as usize - 1).cloned().flatten()
    }

    fn on_event(self: &Rc<Self>, ev: UiEvent) {
        match ev {
            UiEvent::Slot(v) => {
                let prev = self.slot_view(v.slot);
                let was = prev.as_ref().map(|p| p.phase);
                let Some(card) = self.cards.get(v.slot as usize - 1) else { return };
                card.update(&v);
                if let Some(dlg) = self.pairing_dialog() {
                    if was == Some(PhaseView::Pairing) && v.phase != PhaseView::Pairing {
                        dlg.qr_slot_left_pairing(v.slot, v.error.as_deref());
                    }
                    if v.phase == PhaseView::Online && was.is_some_and(|p| p != PhaseView::Online) {
                        dlg.dialog.close();
                    }
                }
                if v.phase == PhaseView::Online && was.is_some_and(|p| matches!(p, PhaseView::Pairing | PhaseView::Connecting)) {
                    self.toast(&format!("{} connected in slot {}", v.title, v.slot));
                }
                if self.focused.get() == Some(v.slot) {
                    if v.phase != PhaseView::Online {
                        self.leave_focus();
                    } else {
                        self.window_title.set_title(&v.title);
                    }
                }
                if v.phase != PhaseView::Online {
                    self.focus_pictures[v.slot as usize - 1].set_paintable(None::<&gdk::Paintable>);
                }
                let idx = v.slot as usize - 1;
                self.state.borrow_mut().slots[idx] = Some(v);
                self.render_status();
                self.render_nearby();
            }
            UiEvent::Stats { slot, stats } => {
                if let Some(card) = self.cards.get(slot as usize - 1) {
                    card.update_stats(&stats);
                }
            }
            UiEvent::Nearby(list) => {
                self.state.borrow_mut().nearby = list.clone();
                self.render_nearby();
                if let Some(dlg) = self.pairing_dialog() {
                    dlg.update_nearby(&list);
                }
            }
            UiEvent::UsbDevices(list) => {
                self.state.borrow_mut().usb = list.clone();
                self.render_nearby();
                if let Some(dlg) = self.pairing_dialog() {
                    dlg.update_usb(&list);
                }
            }
            UiEvent::QrCode { slot, payload, expires_in } => {
                if let Some(dlg) = self.pairing_dialog() {
                    dlg.show_qr(slot, &payload, expires_in);
                } else {
                    // Dialog closed before the session started.
                    self.send(UiCommand::CancelPairing { slot });
                }
            }
            UiEvent::Toast { message, .. } => self.toast(&message),
            UiEvent::AdbStatus { version, error } => {
                self.state.borrow_mut().adb = Some(match (version, error) {
                    (_, Some(e)) => Err(e),
                    (Some(v), None) => Ok(v),
                    (None, None) => Err("unknown".into()),
                });
                self.render_status();
            }
            UiEvent::DiscoveryStatus { available, message } => {
                self.state.borrow_mut().discovery_error = (!available).then_some(message);
                self.render_status();
            }
            UiEvent::DeviceClipboard { text, .. } => {
                *self.last_from_device.borrow_mut() = Some(text.clone());
                self.window.clipboard().set_text(&text);
            }
        }
    }

    // ---- video -------------------------------------------------------------

    fn present_frames(&self) {
        let display = WidgetExt::display(&self.window);
        let mut books = self.video.borrow_mut();
        for (i, io) in self.io.iter().enumerate().take(self.cards.len()) {
            let seq = io.mailbox.seq();
            if seq == books[i].seq {
                continue;
            }
            books[i].seq = seq;
            let Some(frame) = io.mailbox.take() else { continue };
            let dmabuf = frame.is_dmabuf();
            match texture::build(&display, frame) {
                Ok(t) => {
                    self.cards[i].picture.set_paintable(Some(&t));
                    self.focus_pictures[i].set_paintable(Some(&t));
                    io.mailbox.stats.presented.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) if dmabuf => {
                    if let Some(media) = &self.media {
                        media.disable_dmabuf(&e.to_string());
                    }
                }
                Err(e) => tracing::warn!("slot {}: could not build a texture: {e}", i + 1),
            }
        }
    }

    /// Once a second: fps/backend labels and the tile size for `max_size = "auto"`.
    fn video_tick(&self) {
        let mut books = self.video.borrow_mut();
        for (i, card) in self.cards.iter().enumerate() {
            let io = &self.io[i];
            let presented = io.mailbox.stats.presented.load(Ordering::Relaxed);
            let fps = presented.saturating_sub(books[i].presented_at_last_tick);
            books[i].presented_at_last_tick = presented;
            if let (Some((w, h)), Some(backend)) = (io.mailbox.size(), io.backend()) {
                let lag = io.mailbox.stats.lag_ms.load(Ordering::Relaxed);
                // scrcpy sends frames only when the screen changes.
                let rate = if fps == 0 { "idle".to_owned() } else { format!("{fps} fps") };
                let mut info = format!("{rate} · {w}×{h} · {backend}");
                if lag >= 20 {
                    info.push_str(&format!(" · +{lag} ms"));
                }
                card.set_video_info(&info);
            }
            let pic = &card.picture;
            if pic.is_mapped() && pic.width() > 0 {
                let px = (pic.width().max(pic.height()) * pic.scale_factor()) as u32;
                if px.abs_diff(books[i].tile_px) >= 32 {
                    books[i].tile_px = px;
                    self.send(UiCommand::SetTileSize { slot: i as SlotId + 1, px });
                }
            }
        }
    }

    // ---- focus view --------------------------------------------------------

    fn streaming(&self, slot: SlotId) -> bool {
        self.io.get(slot as usize - 1).is_some_and(|io| io.is_streaming())
    }

    fn enter_focus(&self, slot: SlotId) {
        if !self.slot_view(slot).is_some_and(|v| v.phase == PhaseView::Online) {
            return;
        }
        if let Some(prev) = self.focused.replace(Some(slot)) {
            if prev == slot {
                return;
            }
            self.send(UiCommand::SetProfile { slot: prev, profile: StreamProfile::Grid });
        }
        self.send(UiCommand::SetProfile { slot, profile: StreamProfile::Focus });
        self.focus_stack.set_visible_child_name(&slot.to_string());
        self.views.set_visible_child_name("focus");
        self.grid_button.set_visible(true);
        self.window_title.set_title(&self.slot_view(slot).map(|v| v.title).unwrap_or_default());
        self.window_title.set_subtitle(&format!("Slot {slot}"));
        self.active.set(Some(slot));
        self.focus_pictures[slot as usize - 1].grab_focus();
    }

    fn leave_focus(&self) {
        let Some(slot) = self.focused.take() else { return };
        self.send(UiCommand::SetProfile { slot, profile: StreamProfile::Grid });
        self.views.set_visible_child_name("grid");
        self.grid_button.set_visible(false);
        self.window_title.set_title("Android Controller");
        self.window_title.set_subtitle("");
        self.cards[slot as usize - 1].picture.grab_focus();
    }

    /// Where shortcuts and the rail send input: the focused phone, else the
    /// last one used, else the only one streaming.
    fn target(&self) -> Option<Rc<SlotInput>> {
        let slot = self
            .focused
            .get()
            .or(self.active.get().filter(|s| self.streaming(*s)))
            .or_else(|| (1..=self.cards.len() as SlotId).find(|s| self.streaming(*s)))?;
        self.streaming(slot).then(|| self.inputs[slot as usize - 1].clone())
    }

    // ---- clipboard ---------------------------------------------------------

    /// Reads the host clipboard as text and calls `f` with the target slot.
    fn with_clipboard_text(self: &Rc<Self>, f: impl FnOnce(&App, SlotId, &str) + 'static) {
        let Some(slot) = self.target().map(|t| t.slot) else { return };
        let weak = Rc::downgrade(self);
        self.window.clipboard().read_text_async(None::<&gtk::gio::Cancellable>, move |res| {
            if let (Some(app), Ok(Some(text))) = (weak.upgrade(), res) {
                f(&app, slot, text.as_str());
            }
        });
    }

    fn set_device_clipboard(&self, slot: SlotId, text: &str, paste: bool) {
        let t = truncate_utf8(text, CLIPBOARD_TEXT_MAX);
        if t.len() < text.len() {
            self.toast(&format!("W_CLIPBOARD_TRUNCATED: only the first {} KiB of the clipboard was sent", t.len() / 1024));
        }
        self.io[slot as usize - 1].send(ControlMsg::SetClipboard { sequence: 0, paste, text: t.to_owned() });
    }

    fn render_status(&self) {
        let st = self.state.borrow();
        let adb = match &st.adb {
            None => "adb: starting…".to_owned(),
            Some(Ok(v)) => format!("adb server v{v}"),
            Some(Err(e)) => format!("adb: {e}"),
        };
        let disc = match &st.discovery_error {
            None => "mDNS discovery on".to_owned(),
            Some(e) => format!("mDNS discovery off ({e})"),
        };
        let online = st.slots.iter().flatten().filter(|v| v.phase == PhaseView::Online).count();
        self.status.set_label(&format!("{adb}  ·  {disc}  ·  {online}/{} connected", self.cards.len()));
    }

    fn render_nearby(&self) {
        self.nearby_list.remove_all();
        let st = self.state.borrow();
        let mut rows = 0;
        for n in st.nearby.iter().filter(|n| n.in_slot.is_none() && !n.instance.starts_with("studio-")) {
            let title = n.known_name.clone().or_else(|| n.serial.clone()).unwrap_or_else(|| n.instance.clone());
            let row = adw::ActionRow::builder().title(&title).build();
            row.add_prefix(&gtk::Image::from_icon_name("phone-symbolic"));
            let (subtitle, label) = match n.kind {
                ServiceKind::Pairing => (format!("Ready to pair · {}", n.addr), "Pair…"),
                ServiceKind::Connect => (format!("Wireless debugging · {}", n.addr), "Connect"),
                ServiceKind::Legacy => (format!("adb tcpip · {}", n.addr), "Connect"),
            };
            row.set_subtitle(&subtitle);
            let button = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
            let (cmds, kind, addr) = (self.commands.clone(), n.kind, n.addr.to_string());
            let window = self.window.clone();
            button.connect_clicked(move |_| match kind {
                ServiceKind::Pairing => {
                    let _ = WidgetExt::activate_action(&window, "win.pair-code", None);
                }
                _ => {
                    let _ = cmds.try_send(UiCommand::Connect { slot: None, addr: addr.clone() });
                }
            });
            row.add_suffix(&button);
            self.nearby_list.append(&row);
            rows += 1;
        }
        for d in &st.usb {
            let row = adw::ActionRow::builder().title(&d.model).subtitle(format!("USB · {}", d.serial)).build();
            row.add_prefix(&gtk::Image::from_icon_name("media-removable-symbolic"));
            if d.authorized {
                let b = gtk::Button::builder().label("Enable Wi-Fi ADB").valign(gtk::Align::Center).build();
                let (cmds, serial) = (self.commands.clone(), d.serial.clone());
                b.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    let _ = cmds.try_send(UiCommand::EnableWifiAdb { usb_serial: serial.clone(), slot: None });
                });
                row.add_suffix(&b);
            } else {
                row.set_subtitle("USB · accept the debugging prompt on the phone");
            }
            self.nearby_list.append(&row);
            rows += 1;
        }
        if rows == 0 {
            let row = adw::ActionRow::builder()
                .title("No phones found yet")
                .subtitle("On the phone: Settings › Developer options › Wireless debugging (phone on the same network as this PC)")
                .build();
            let spinner = gtk::Spinner::builder().spinning(true).build();
            row.add_prefix(&spinner);
            self.nearby_list.append(&row);
        }
    }
}
