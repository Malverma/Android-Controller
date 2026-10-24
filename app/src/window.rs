//! Main window: one card per slot (`general.max_slots`) in the grid layout
//! of spec §6.1, the "Nearby" list, a status bar, broadcast input (§4.10),
//! and the focus view (§6.2: one phone large, live thumbnails of the others,
//! and a hardware-button rail).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ac_config::{AppState, BroadcastState, WindowState};
use ac_core::SlotId;
use ac_device::{
    ManagerHandle, NearbyView, PhaseView, SlotIo, SlotView, StreamProfile, StreamView, TransferView, UiCommand, UiEvent,
    UsbDeviceView,
};
use ac_discovery::ServiceKind;
use ac_input::keymap::android;
use ac_media::MediaContext;
use ac_input::broadcast::orientation_differs;
use ac_scrcpy::{ControlMsg, VideoCodec};
use ac_scrcpy::control::{CLIPBOARD_TEXT_MAX, truncate_utf8};
use adw::prelude::*;
use gtk::{gdk, glib};

use crate::broadcast::Broadcast;
use crate::dnd::{self, ApkChoice};
use crate::grid_layout::{Grid, TileInfo};
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
.slot-card.compact .phone-frame { min-height: 0; padding: 8px; border-radius: 12px; }
.slot-card.broadcast-target, .video-frame.broadcast-target { outline: 3px solid @error_color; outline-offset: -3px; }
.broadcast-badge { color: @error_color; font-weight: 800; margin-right: 6px; }
.thumb-rail { padding: 12px 0 12px 12px; }
.thumb { padding: 6px; }
.thumb picture { border-radius: 10px; background: black; }
.slot-card.drop-hover, .video-frame.drop-hover { outline: 3px dashed @accent_color; outline-offset: -3px; }
.drop-hint { background: alpha(@window_bg_color, 0.92); color: @window_fg_color; border-radius: 12px; padding: 10px 14px; margin: 12px; font-weight: bold; }
togglebutton.stay-awake:checked { color: @accent_color; background: alpha(@accent_color, 0.15); }
.transfer-row progressbar trough, .transfer-row progressbar progress { min-height: 4px; }
"#;

/// A restored focus view waits this long for its phone to start streaming.
const RESTORE_FOCUS_WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
struct State {
    slots: Vec<Option<SlotView>>,
    nearby: Vec<NearbyView>,
    usb: Vec<UsbDeviceView>,
    adb: Option<Result<u32, String>>,
    discovery_error: Option<String>,
}

/// A live thumbnail of a non-focused phone in the focus view.
struct Thumb {
    button: gtk::Button,
    picture: gtk::Picture,
    label: gtk::Label,
}

pub struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    grid: Grid,
    cards: Vec<SlotCard>,
    thumbs: Vec<Thumb>,
    io: Vec<Arc<SlotIo>>,
    media: Option<Arc<MediaContext>>,
    inputs: Vec<Rc<SlotInput>>,
    /// The focus view's mirror per slot (the grid's live in the cards).
    focus_pictures: Vec<gtk::Picture>,
    focus_widgets: Vec<gtk::Widget>,
    /// The pointer is over the focused mirror (Escape then goes to the phone).
    pointer_in_mirror: Rc<Cell<bool>>,
    broadcast: Rc<Broadcast>,
    broadcast_toggle: gtk::ToggleButton,
    broadcast_badge: gtk::Label,
    hide_empty: bool,
    /// Bytes per second over all streams, for the status bar.
    total_kbps: Cell<u32>,
    app_state: RefCell<AppState>,
    state_path: Option<PathBuf>,
    /// Focus view to restore once its phone streams (from `state.toml`).
    pending_focus: Cell<Option<(SlotId, Instant)>>,
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
    /// The focus view's "keep screen on" toggle, and its echo guard.
    rail_stay_awake: gtk::ToggleButton,
    rail_syncing: Cell<bool>,
    transfer_cfg: ac_config::Transfer,
    transfers: RefCell<Vec<Option<TransferView>>>,
    /// "Remember for this session" answer of the APK question: install?
    apk_answer: Cell<Option<bool>>,
}

/// Per-slot presentation bookkeeping.
#[derive(Default, Clone)]
struct VideoBook {
    seq: u64,
    presented_at_last_tick: u64,
    bytes_at_last_tick: u64,
    /// Tile size last sent to the manager, and a candidate waiting to be
    /// seen unchanged for a second (spec §4.3 hysteresis).
    tile_px: u32,
    pending_px: u32,
    screen_off: bool,
    /// Last known capture size; kept across stream restarts so the grid
    /// does not jump while a phone switches profile.
    frame: Option<(u32, u32)>,
}


pub struct Restore {
    pub state: AppState,
    pub path: Option<PathBuf>,
}

pub fn build(
    app: &adw::Application,
    manager: ManagerHandle,
    cfg: &ac_config::Config,
    slots: u8,
    config_error: Option<String>,
    restore: Restore,
) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let saved = restore.state.clone();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Android Controller")
        .default_width(saved.window.width)
        .default_height(saved.window.height)
        .build();
    if saved.window.maximized {
        window.maximize();
    }
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
    let broadcast_toggle = gtk::ToggleButton::builder()
        .icon_name("network-transmit-symbolic")
        .tooltip_text("Broadcast input to the selected phones (Alt+Shift+B)")
        .build();
    broadcast_toggle.update_property(&[gtk::accessible::Property::Label("Broadcast input")]);
    let broadcast_badge = gtk::Label::builder().label("BROADCAST ●").visible(false).build();
    broadcast_badge.add_css_class("broadcast-badge");
    header.pack_end(&broadcast_toggle);
    header.pack_end(&broadcast_badge);

    // Slots, laid out by the grid scorer (spec §6.1).
    let grid = Grid::default();
    grid.set_margin_top(12);
    grid.set_margin_start(12);
    grid.set_margin_end(12);
    grid.set_vexpand(true);

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

    // The layout wraps to more rows when the window is narrow; scroll
    // vertically rather than clip when a tiling compositor gives it less.
    let grid_scroll = gtk::ScrolledWindow::builder()
        .child(&grid)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&grid_scroll);
    content.append(&nearby_scroll);

    // Focus view: live thumbnails of the other phones, one phone large, and
    // the hardware buttons (spec §6.2).
    let thumbs_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    let thumbs_scroll = gtk::ScrolledWindow::builder()
        .child(&thumbs_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .width_request(150)
        .build();
    let thumbs_grid_button = gtk::Button::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("All phones (Alt+G)")
        .halign(gtk::Align::Center)
        .build();
    let thumb_rail = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    thumb_rail.add_css_class("thumb-rail");
    thumb_rail.append(&thumbs_scroll);
    thumb_rail.append(&thumbs_grid_button);
    let focus_stack = gtk::Stack::builder().hexpand(true).vexpand(true).margin_top(12).margin_bottom(12).margin_start(12).build();
    let rail = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).build();
    rail.add_css_class("rail");
    let focus_page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    focus_page.append(&thumb_rail);
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
        let broadcast = Broadcast::new(manager.io.clone(), &cfg.input);
        let inputs: Vec<Rc<SlotInput>> = (1..=slots)
            .map(|s| SlotInput::new(s, manager.io[s as usize - 1].clone(), prefs.clone(), broadcast.clone(), on_activate.clone()))
            .collect();
        let offload = cfg.video.graphics_offload != "never";
        let pointer_in_mirror = Rc::new(Cell::new(false));
        let mut cards = Vec::new();
        let mut focus_pictures = Vec::new();
        let mut focus_widgets = Vec::new();
        let mut thumbs = Vec::new();
        for input in &inputs {
            let (picture, widget) = new_mirror(input, offload);
            let card = SlotCard::new(input.slot, on_action.clone(), picture, widget);
            grid.append(&card.root);
            cards.push(card);
            let (picture, widget) = new_mirror(input, offload);
            widget.add_css_class("video-frame");
            let hover = gtk::EventControllerMotion::new();
            {
                let inside = pointer_in_mirror.clone();
                hover.connect_enter(move |_, _, _| inside.set(true));
            }
            {
                let inside = pointer_in_mirror.clone();
                hover.connect_leave(move |_| inside.set(false));
            }
            widget.add_controller(hover);
            focus_stack.add_named(&widget, Some(&input.slot.to_string()));
            focus_pictures.push(picture);
            focus_widgets.push(widget);
            thumbs.push(new_thumb(input.slot, weak.clone()));
            thumbs_box.append(&thumbs.last().unwrap().button);
        }
        App {
            window: window.clone(),
            toasts,
            grid: grid.clone(),
            cards,
            thumbs,
            io: manager.io.clone(),
            media: manager.media.clone(),
            inputs,
            focus_pictures,
            focus_widgets,
            pointer_in_mirror,
            broadcast,
            broadcast_toggle: broadcast_toggle.clone(),
            broadcast_badge,
            hide_empty: cfg.ui.hide_empty_slots,
            total_kbps: Cell::new(0),
            app_state: RefCell::new(restore.state),
            state_path: restore.path,
            pending_focus: Cell::new(
                (saved.view == "focus" && saved.focused_slot > 0).then(|| (saved.focused_slot, Instant::now())),
            ),
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
            rail_stay_awake: gtk::ToggleButton::builder()
                .icon_name("display-brightness-symbolic")
                .tooltip_text("Keep screen on: the phone's screen never turns off by itself")
                .focusable(false)
                .build(),
            rail_syncing: Cell::new(false),
            transfer_cfg: cfg.transfer.clone(),
            transfers: RefCell::new(vec![None; slots as usize]),
            apk_answer: Cell::new(None),
        }
    });
    for i in 0..app_rc.cards.len() {
        let slot = i as SlotId + 1;
        app_rc.install_drop(slot, app_rc.cards[i].root.upcast_ref(), true);
        app_rc.install_drop(slot, &app_rc.focus_widgets[i], false);
    }

    install_actions(&app_rc, app);
    install_shortcuts(&app_rc, cfg);
    build_rail(&app_rc, &rail);
    for b in [&grid_button, &thumbs_grid_button] {
        let weak = Rc::downgrade(&app_rc);
        b.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.leave_focus();
            }
        });
    }
    {
        let weak = Rc::downgrade(&app_rc);
        broadcast_toggle.connect_toggled(move |b| {
            if let Some(app) = weak.upgrade() {
                app.set_broadcast(b.is_active());
            }
        });
    }
    {
        let weak = Rc::downgrade(&app_rc);
        *app_rc.broadcast.on_power_blocked.borrow_mut() = Some(Box::new(move || {
            if let Some(app) = weak.upgrade() {
                app.toast("Power is not broadcast (input.broadcast_includes_power = false)");
            }
        }));
    }
    // Escape leaves the focus view unless the pointer is on the phone (spec §6.2).
    {
        let weak = Rc::downgrade(&app_rc);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| match weak.upgrade() {
            Some(app) if key == gdk::Key::Escape && app.focused.get().is_some() && !app.pointer_in_mirror.get() => {
                app.leave_focus();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        window.add_controller(keys);
    }
    app_rc.broadcast.set_targets(&saved.broadcast.targets);
    app_rc.broadcast.set_enabled(saved.broadcast.enabled);
    broadcast_toggle.set_active(saved.broadcast.enabled);
    app_rc.refresh_layout();
    app_rc.refresh_broadcast();

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
        if let Some((app, manager)) = owned.borrow_mut().take() {
            app.save_state();
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
    let stay = &app.rail_stay_awake;
    stay.add_css_class("circular");
    stay.add_css_class("stay-awake");
    stay.update_property(&[gtk::accessible::Property::Label("Keep screen on")]);
    let weak = Rc::downgrade(app);
    stay.connect_toggled(move |b| {
        if let Some(app) = weak.upgrade()
            && !app.rail_syncing.get()
            && let Some(slot) = app.focused.get()
        {
            app.set_stay_awake(slot, b.is_active());
        }
    });
    rail.append(stay);
}

/// A thumbnail for the focus view's rail; clicking it focuses that phone.
fn new_thumb(slot: SlotId, app: std::rc::Weak<App>) -> Thumb {
    let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).can_shrink(true).height_request(170).build();
    let label = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(16).build();
    label.add_css_class("caption");
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    content.append(&picture);
    content.append(&label);
    let button = gtk::Button::builder().child(&content).visible(false).tooltip_text(format!("Focus slot {slot} (Alt+{slot})")).build();
    button.add_css_class("flat");
    button.add_css_class("thumb");
    button.connect_clicked(move |_| {
        if let Some(app) = app.upgrade() {
            app.enter_focus(slot);
        }
    });
    Thumb { button, picture, label }
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
            "broadcast_toggle" => {
                self.broadcast_toggle.set_active(!self.broadcast_toggle.is_active());
                return true;
            }
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

    let pair_direct = gtk::gio::SimpleAction::new("pair-direct", None);
    let weak = Rc::downgrade(app);
    pair_direct.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            app.open_pairing(Page::Direct, None);
        }
    });
    app.window.add_action(&pair_direct);

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
            CardAction::BroadcastTarget(on) => {
                if self.broadcast.is_target(slot) != on {
                    self.broadcast.set_target(slot, on);
                    self.refresh_broadcast();
                    self.save_state();
                }
            }
            CardAction::StayAwake(on) => self.set_stay_awake(slot, on),
            CardAction::SendFiles => self.pick_files(slot, false),
            CardAction::SendFolder => self.pick_files(slot, true),
            CardAction::CancelTransfers => self.send(UiCommand::CancelTransfers { slot }),
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
                let idx = v.slot as usize - 1;
                if v.phase != PhaseView::Online {
                    self.focus_pictures[idx].set_paintable(None::<&gdk::Paintable>);
                    self.thumbs[idx].picture.set_paintable(None::<&gdk::Paintable>);
                    self.video.borrow_mut()[idx].frame = None;
                }
                let (slot, live) = (v.slot, v.phase == PhaseView::Online && matches!(v.stream, StreamView::Live { .. }));
                self.state.borrow_mut().slots[idx] = Some(v);
                self.refresh_rail_stay_awake();
                self.render_status();
                self.render_nearby();
                self.refresh_layout();
                self.refresh_thumbs();
                self.refresh_broadcast();
                if live
                    && let Some((s, since)) = self.pending_focus.get()
                    && s == slot
                {
                    self.pending_focus.set(None);
                    if since.elapsed() < RESTORE_FOCUS_WINDOW && self.focused.get().is_none() {
                        self.enter_focus(slot);
                    }
                }
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
            UiEvent::Transfer(v) => {
                let i = v.slot as usize - 1;
                let Some(card) = self.cards.get(i) else { return };
                card.set_transfer(&v);
                let was_active = self.transfers.borrow()[i].as_ref().is_some_and(|t| t.active);
                self.transfers.borrow_mut()[i] = v.active.then_some(v.clone());
                if was_active != v.active {
                    self.render_status();
                }
            }
        }
    }

    fn set_stay_awake(&self, slot: SlotId, on: bool) {
        self.send(UiCommand::SetStayAwake { slot, on });
        let name = self.slot_view(slot).map(|v| v.title).unwrap_or_default();
        self.toast(&if on {
            format!("{name}: screen stays on")
        } else {
            format!("{name}: screen turns off normally again")
        });
    }

    /// Mirrors the focused phone's "keep screen on" state on the rail toggle.
    fn refresh_rail_stay_awake(&self) {
        let state = self.focused.get().and_then(|s| self.slot_view(s)).and_then(|v| v.stay_awake);
        self.rail_stay_awake.set_sensitive(state.is_some());
        self.rail_syncing.set(true);
        self.rail_stay_awake.set_active(state == Some(true));
        self.rail_syncing.set(false);
    }

    // ---- file transfer (spec §5) -------------------------------------------

    /// Phones a drop on `slot` goes to: every broadcast target while
    /// broadcasting from it, otherwise just that one.
    fn transfer_targets(&self, slot: SlotId) -> Vec<SlotId> {
        let online = |s: SlotId| self.slot_view(s).is_some_and(|v| v.phase == PhaseView::Online);
        if self.broadcast.active_for(slot) {
            self.broadcast.targets().into_iter().filter(|&s| online(s)).collect()
        } else if online(slot) {
            vec![slot]
        } else {
            Vec::new()
        }
    }

    fn install_drop(self: &Rc<Self>, slot: SlotId, widget: &gtk::Widget, show_hint: bool) {
        let target = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY | gdk::DragAction::MOVE);
        target.set_types(&dnd::drop_types());
        target.set_preload(true);
        let hint = {
            let (weak, widget) = (Rc::downgrade(self), widget.clone());
            move |text: Option<String>| {
                let Some(app) = weak.upgrade() else { return };
                if show_hint {
                    app.cards[slot as usize - 1].set_drop_hint(text.as_deref());
                } else if text.is_some() {
                    widget.add_css_class("drop-hover");
                } else {
                    widget.remove_css_class("drop-hover");
                }
            }
        };
        let hint = Rc::new(hint);
        // The preloaded value also changes when a drag ends; only redraw the
        // hint while something is actually over the tile.
        let hovering = Rc::new(Cell::new(false));
        let describe = {
            let weak = Rc::downgrade(self);
            move |t: &gtk::DropTarget| -> Option<String> {
                let app = weak.upgrade()?;
                let phones = app.transfer_targets(slot).len();
                if phones == 0 {
                    return Some("Connect this phone to send files to it".into());
                }
                let cfg = &app.transfer_cfg;
                let choice = dnd::apk_choice(&cfg.apk_drop_action, false);
                Some(match t.value().map(|v| dnd::paths_from_value(&v)) {
                    Some(Ok(paths)) => dnd::hint(&paths, &cfg.default_target_dir, choice, &cfg.apk_alternate_modifier, phones),
                    Some(Err(e)) => e,
                    None => "Drop to send".into(),
                })
            }
        };
        let describe = Rc::new(describe);
        {
            let (hint, describe, hovering) = (hint.clone(), describe.clone(), hovering.clone());
            let weak = Rc::downgrade(self);
            target.connect_enter(move |t, _, _| {
                hovering.set(true);
                hint(describe(t));
                match weak.upgrade() {
                    Some(app) if !app.transfer_targets(slot).is_empty() => gdk::DragAction::COPY,
                    _ => gdk::DragAction::empty(),
                }
            });
        }
        {
            let (hint, describe, hovering) = (hint.clone(), describe.clone(), hovering.clone());
            target.connect_value_notify(move |t| {
                if hovering.get() {
                    hint(describe(t));
                }
            });
        }
        {
            let (hint, hovering) = (hint.clone(), hovering.clone());
            target.connect_leave(move |_| {
                hovering.set(false);
                hint(None);
            });
        }
        {
            let weak = Rc::downgrade(self);
            target.connect_drop(move |t, value, _, _| {
                hovering.set(false);
                hint(None);
                let Some(app) = weak.upgrade() else { return false };
                let paths = match dnd::paths_from_value(value) {
                    Ok(p) => p,
                    Err(e) => {
                        app.toast(&e);
                        return false;
                    }
                };
                // Alternate modifier (spec §5.2): keyboard state at drop
                // time, or a Shift-drag negotiated as MOVE.
                let m = dnd::modifier(&app.transfer_cfg.apk_alternate_modifier);
                let alternate = t.current_drop().is_some_and(|d| {
                    let dev = d.device();
                    let held = dev.modifier_state().contains(m)
                        || dev.seat().keyboard().is_some_and(|k| k.modifier_state().contains(m));
                    held || d.actions() == gdk::DragAction::MOVE
                });
                app.send_paths(slot, paths, alternate);
                true
            });
        }
        widget.add_controller(target);
    }

    fn send_paths(self: &Rc<Self>, slot: SlotId, paths: Vec<PathBuf>, alternate: bool) {
        let slots = self.transfer_targets(slot);
        if slots.is_empty() {
            self.toast("Connect this phone to send files to it");
            return;
        }
        let has_apk = paths.iter().any(|p| ac_transfer::apk::has_apk_ext(p));
        let choice = dnd::apk_choice(&self.transfer_cfg.apk_drop_action, alternate);
        match (has_apk, choice, self.apk_answer.get()) {
            (false, ..) => self.send(UiCommand::SendFiles { slots, paths, install_apks: false }),
            (true, ApkChoice::Ask, Some(install)) => self.send(UiCommand::SendFiles { slots, paths, install_apks: install }),
            (true, ApkChoice::Ask, None) => self.ask_apk(slots, paths),
            (true, c, _) => self.send(UiCommand::SendFiles { slots, paths, install_apks: c == ApkChoice::Install }),
        }
    }

    /// `apk_drop_action = "ask"`: Install / Copy as file / Cancel.
    fn ask_apk(self: &Rc<Self>, slots: Vec<SlotId>, paths: Vec<PathBuf>) {
        let n = paths.iter().filter(|p| ac_transfer::apk::has_apk_ext(p)).count();
        let alert = adw::AlertDialog::new(
            Some(if n == 1 { "Install this app?" } else { "Install these apps?" }),
            Some("APK files can be installed on the phone or copied to it like any other file."),
        );
        alert.add_response("cancel", "Cancel");
        alert.add_response("push", "Copy as File");
        alert.add_response("install", "Install");
        alert.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        alert.set_default_response(Some("install"));
        let remember = gtk::CheckButton::with_label("Remember for this session");
        alert.set_extra_child(Some(&remember));
        let weak = Rc::downgrade(self);
        alert.connect_response(None, move |_, resp| {
            let Some(app) = weak.upgrade() else { return };
            let install = match resp {
                "install" => true,
                "push" => false,
                _ => return,
            };
            if remember.is_active() {
                app.apk_answer.set(Some(install));
            }
            app.send(UiCommand::SendFiles { slots: slots.clone(), paths: paths.clone(), install_apks: install });
        });
        alert.present(Some(&self.window));
    }

    /// Tile menu → Send Files… / Send Folder… (portal-backed on Wayland).
    fn pick_files(self: &Rc<Self>, slot: SlotId, folder: bool) {
        let title = self.slot_view(slot).map(|v| v.title).unwrap_or_default();
        let dialog = gtk::FileDialog::builder()
            .title(if folder { format!("Send a folder to {title}") } else { format!("Send files to {title}") })
            .accept_label("Send")
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        if folder {
            dialog.select_folder(Some(&self.window), gtk::gio::Cancellable::NONE, move |r| {
                if let (Some(app), Ok(f)) = (weak.upgrade(), r)
                    && let Some(p) = f.path()
                {
                    app.send_paths(slot, vec![p], false);
                }
            });
        } else {
            dialog.open_multiple(Some(&self.window), gtk::gio::Cancellable::NONE, move |r| {
                let (Some(app), Ok(list)) = (weak.upgrade(), r) else { return };
                let paths: Vec<PathBuf> = (0..list.n_items())
                    .filter_map(|i| list.item(i).and_downcast::<gtk::gio::File>())
                    .filter_map(|f| f.path())
                    .collect();
                if !paths.is_empty() {
                    app.send_paths(slot, paths, false);
                }
            });
        }
    }

    // ---- video -------------------------------------------------------------

    fn present_frames(&self) {
        let display = WidgetExt::display(&self.window);
        let mut books = self.video.borrow_mut();
        let mut resized = false;
        for (i, io) in self.io.iter().enumerate().take(self.cards.len()) {
            if let Some(size) = io.mailbox.size()
                && books[i].frame != Some(size)
            {
                books[i].frame = Some(size);
                resized = true;
            }
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
                    self.thumbs[i].picture.set_paintable(Some(&t));
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
        drop(books);
        if resized {
            // A rotation: re-score the grid (spec §6.1) and re-check broadcast orientations.
            self.refresh_layout();
            self.refresh_broadcast();
        }
    }

    /// Once a second: the status strip (fps, codec, bitrate, backend), the
    /// status bar total, and the tile size for `max_size = "auto"`.
    fn video_tick(&self) {
        let mut books = self.video.borrow_mut();
        let mut total_kbps = 0;
        for (i, card) in self.cards.iter().enumerate() {
            let io = &self.io[i];
            let presented = io.mailbox.stats.presented.load(Ordering::Relaxed);
            let fps = presented.saturating_sub(books[i].presented_at_last_tick);
            books[i].presented_at_last_tick = presented;
            let bytes = io.mailbox.stats.bytes.load(Ordering::Relaxed);
            let kbps = (bytes.saturating_sub(books[i].bytes_at_last_tick) * 8 / 1000) as u32;
            books[i].bytes_at_last_tick = bytes;
            if let (Some((w, h)), Some(backend)) = (io.mailbox.size(), io.backend()) {
                total_kbps += kbps;
                card.set_video_info(&status_strip(fps, kbps, (w, h), io.info().map(|i| i.codec), &backend.to_string(), io));
            }
            let pic = &card.picture;
            if pic.is_mapped() && pic.width() > 0 {
                let px = (pic.width().max(pic.height()) * pic.scale_factor()) as u32;
                let b = &mut books[i];
                if px.abs_diff(b.tile_px) >= 32 {
                    // The first size goes out at once; later ones once the
                    // window has been still for a second.
                    if b.tile_px == 0 || px == b.pending_px {
                        b.tile_px = px;
                        self.send(UiCommand::SetTileSize { slot: i as SlotId + 1, px });
                    }
                    b.pending_px = px;
                }
            }
        }
        drop(books);
        if self.total_kbps.replace(total_kbps) != total_kbps {
            self.render_status();
        }
        self.refresh_broadcast();
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
        self.refresh_rail_stay_awake();
        self.refresh_thumbs();
        self.refresh_broadcast();
    }

    fn leave_focus(&self) {
        let Some(slot) = self.focused.take() else { return };
        self.send(UiCommand::SetProfile { slot, profile: StreamProfile::Grid });
        self.views.set_visible_child_name("grid");
        self.grid_button.set_visible(false);
        self.window_title.set_title("Android Controller");
        self.window_title.set_subtitle("");
        self.cards[slot as usize - 1].picture.grab_focus();
        self.pointer_in_mirror.set(false);
        self.refresh_thumbs();
        self.refresh_broadcast();
    }

    // ---- grid, thumbnails, broadcast --------------------------------------

    /// Tells the grid layout about each card: empty slots go to the narrow
    /// column (or are hidden with `ui.hide_empty_slots`), phones are scored
    /// by their frame shape.
    fn refresh_layout(&self) {
        let st = self.state.borrow();
        let books = self.video.borrow();
        let empty: Vec<bool> = (0..self.cards.len())
            .map(|i| st.slots[i].as_ref().is_none_or(|v| v.phase == PhaseView::Empty))
            .collect();
        let any_phone = empty.iter().any(|e| !e);
        let tiles = empty
            .iter()
            .enumerate()
            .map(|(i, &compact)| {
                self.cards[i].set_compact(compact && any_phone);
                self.cards[i].root.set_visible(!(compact && any_phone && self.hide_empty));
                TileInfo { frame: books[i].frame, compact }
            })
            .collect();
        self.grid.grid_layout().set_tiles(tiles);
    }

    fn refresh_thumbs(&self) {
        let focused = self.focused.get();
        for (i, th) in self.thumbs.iter().enumerate() {
            let slot = i as SlotId + 1;
            let v = self.slot_view(slot);
            let show = focused.is_some_and(|f| f != slot)
                && v.as_ref().is_some_and(|v| {
                    v.phase == PhaseView::Online && matches!(v.stream, StreamView::Live { .. } | StreamView::Starting)
                });
            th.button.set_visible(show);
            if let Some(v) = v {
                th.label.set_label(&format!("{slot} · {}", v.title));
            }
        }
    }

    fn set_broadcast(&self, on: bool) {
        if self.broadcast.enabled() == on {
            return;
        }
        if on && self.broadcast.targets().is_empty() {
            // Nothing chosen yet: start with every phone that is mirroring.
            let all: Vec<SlotId> = (1..=self.cards.len() as SlotId).filter(|&s| self.streaming(s)).collect();
            self.broadcast.set_targets(&all);
        }
        self.broadcast.set_enabled(on);
        self.refresh_broadcast();
        self.save_state();
        if on {
            let n = self.broadcast.targets().len();
            self.toast(&format!("Broadcast on: input goes to {n} selected phone{}", if n == 1 { "" } else { "s" }));
        }
    }

    /// Checkboxes, ⚠ badges, red borders and the header badge (spec §4.10).
    fn refresh_broadcast(&self) {
        let on = self.broadcast.enabled();
        self.broadcast_badge.set_visible(on);
        if self.broadcast_toggle.is_active() != on {
            self.broadcast_toggle.set_active(on);
        }
        // Orientation warnings are relative to the phone being driven.
        let reference = self
            .focused
            .get()
            .or(self.active.get())
            .filter(|&s| self.broadcast.is_target(s) && self.streaming(s))
            .or_else(|| self.broadcast.targets().into_iter().find(|&s| self.streaming(s)));
        let ref_frame = reference.and_then(|s| self.io[s as usize - 1].mailbox.size());
        for (i, card) in self.cards.iter().enumerate() {
            let slot = i as SlotId + 1;
            let target = self.broadcast.is_target(slot);
            let frame = self.io[i].mailbox.size();
            let warning = if !self.streaming(slot) {
                Some("Not mirroring: broadcast skips this phone".to_owned())
            } else if let (Some(r), Some(f), Some(rs)) = (ref_frame, frame, reference)
                && rs != slot
                && orientation_differs(r, f)
            {
                Some(if self.broadcast.skips_mismatched() {
                    format!("Orientation differs from slot {rs}: skipped")
                } else {
                    format!("Orientation differs from slot {rs}: taps land in the same relative spot")
                })
            } else {
                None
            };
            card.set_broadcast(on, target, warning.as_deref());
            let w = &self.focus_widgets[i];
            if on && target {
                w.add_css_class("broadcast-target");
            } else {
                w.remove_css_class("broadcast-target");
            }
        }
    }

    /// Writes `state.toml` (spec §7.6).
    fn save_state(&self) {
        let Some(path) = &self.state_path else { return };
        let mut st = self.app_state.borrow_mut();
        let (width, height) = self.window.default_size();
        st.window = WindowState { width, height, maximized: self.window.is_maximized() };
        st.view = if self.focused.get().is_some() { "focus" } else { "grid" }.into();
        if let Some(f) = self.focused.get() {
            st.focused_slot = f;
        }
        st.broadcast = BroadcastState { enabled: self.broadcast.enabled(), targets: self.broadcast.targets() };
        if let Err(e) = ac_config::save_state(path, &st) {
            tracing::warn!("saving window state: {e}");
        }
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
        let mut line = format!("{adb}  ·  {disc}  ·  {online}/{} connected", self.cards.len());
        if let Some(media) = &self.media {
            line.push_str(&format!("  ·  decoder {}", media.backend()));
        }
        let sending = self.transfers.borrow().iter().flatten().count();
        if sending > 0 {
            line.push_str(&format!("  ·  ⇅ sending to {sending} phone{}", if sending == 1 { "" } else { "s" }));
        }
        let kbps = self.total_kbps.get();
        if kbps > 0 {
            line.push_str(&format!("  ·  ↓ {:.1} Mbit/s", kbps as f64 / 1000.0));
        }
        self.status.set_label(&line);
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
                ServiceKind::Direct if n.paired => (format!("No ADB · {}", n.addr), "Connect"),
                ServiceKind::Direct if n.pairing_open => (format!("Ready to pair without ADB · {}", n.addr), "Pair…"),
                ServiceKind::Direct => (format!("Helper without ADB · {} · tap Pair with PC on the phone", n.addr), "Pair…"),
            };
            row.set_subtitle(&subtitle);
            let button = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
            let (cmds, kind, addr, paired) = (self.commands.clone(), n.kind, n.addr.to_string(), n.paired);
            let window = self.window.clone();
            button.connect_clicked(move |_| match kind {
                ServiceKind::Pairing => {
                    let _ = WidgetExt::activate_action(&window, "win.pair-code", None);
                }
                ServiceKind::Direct if !paired => {
                    let _ = WidgetExt::activate_action(&window, "win.pair-direct", None);
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

fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "H.265",
        VideoCodec::Av1 => "AV1",
    }
}

/// `30 fps · H.264 · 2.9 Mbit/s · 1080×2400 · vaapi-dmabuf` (spec §6.1 status strip).
fn status_strip(fps: u64, kbps: u32, (w, h): (u32, u32), codec: Option<VideoCodec>, backend: &str, io: &SlotIo) -> String {
    // scrcpy sends frames only when the screen changes.
    let mut parts = vec![if fps == 0 { "idle".to_owned() } else { format!("{fps} fps") }];
    if let Some(c) = codec {
        parts.push(codec_name(c).to_owned());
    }
    parts.push(format!("{:.1} Mbit/s", kbps as f64 / 1000.0));
    parts.push(format!("{w}×{h}"));
    parts.push(backend.to_owned());
    let lag = io.mailbox.stats.lag_ms.load(Ordering::Relaxed);
    if lag >= 20 {
        parts.push(format!("+{lag} ms"));
    }
    parts.join(" · ")
}
