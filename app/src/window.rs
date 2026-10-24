//! Main window: five slot cards, the "Nearby" list, and a status bar.

use std::cell::RefCell;
use std::rc::Rc;

use ac_core::{MAX_SLOTS, SlotId};
use ac_device::{ManagerHandle, NearbyView, PhaseView, SlotView, UiCommand, UiEvent, UsbDeviceView};
use ac_discovery::ServiceKind;
use adw::prelude::*;
use gtk::{gdk, glib};

use crate::pairing::{Page, PairingDialog};
use crate::slot_card::{CardAction, SlotCard};

const CSS: &str = r#"
.slot-card { padding: 12px; min-width: 150px; }
.slot-number { font-weight: 800; min-width: 22px; border-radius: 6px; background: alpha(currentColor, 0.1); }
.badge { border-radius: 99px; padding: 1px 8px; font-size: smaller; font-weight: bold; }
.badge.online { background: alpha(@success_color, 0.18); color: @success_color; }
.badge.pairing, .badge.connecting { background: alpha(@accent_color, 0.18); color: @accent_color; }
.badge.reconnecting { background: alpha(@warning_color, 0.2); color: @warning_color; }
.badge.disconnected, .badge.empty { background: alpha(currentColor, 0.1); }
.phone-frame { border-radius: 20px; border: 2px solid alpha(currentColor, 0.12); background: alpha(currentColor, 0.03); min-height: 240px; padding: 12px; }
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
    nearby_list: gtk::ListBox,
    status: gtk::Label,
    commands: async_channel::Sender<UiCommand>,
    state: RefCell<State>,
    pairing: RefCell<Option<Rc<PairingDialog>>>,
}

pub fn build(app: &adw::Application, manager: ManagerHandle, config_error: Option<String>) {
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
    let add = gtk::Button::builder().label("Add Device").tooltip_text("Pair or connect a phone (Ctrl+N)").build();
    add.add_css_class("suggested-action");
    add.set_action_name(Some("win.pair"));
    header.pack_start(&add);

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

    // Five cards need ~900 px; scroll sideways rather than clip when a tiling
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
    toasts.set_child(Some(&content));

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
        let cards: Vec<SlotCard> = (1..=MAX_SLOTS as SlotId).map(|s| SlotCard::new(s, on_action.clone())).collect();
        for c in &cards {
            grid.append(&c.root);
        }
        App {
            window: window.clone(),
            toasts,
            cards,
            nearby_list,
            status,
            commands: manager.commands.clone(),
            state: RefCell::new(State { slots: vec![None; MAX_SLOTS], ..Default::default() }),
            pairing: RefCell::new(None),
        }
    });

    install_actions(&app_rc, app);
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
                self.cards[v.slot as usize - 1].update(&v);
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
                let idx = v.slot as usize - 1;
                self.state.borrow_mut().slots[idx] = Some(v);
                self.render_status();
                self.render_nearby();
            }
            UiEvent::Stats { slot, stats } => self.cards[slot as usize - 1].update_stats(&stats),
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
        }
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
        self.status.set_label(&format!("{adb}  ·  {disc}  ·  {online}/{MAX_SLOTS} connected"));
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
