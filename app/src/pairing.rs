//! "Add device" dialog: QR pairing, 6-digit code pairing, and manual /
//! legacy (USB → `adb tcpip`) connection.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use ac_core::SlotId;
use ac_device::{NearbyView, UiCommand, UsbDeviceView};
use ac_discovery::ServiceKind;
use adw::prelude::*;
use gtk::glib;

use crate::window::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Qr,
    Code,
    Manual,
}

impl Page {
    fn name(self) -> &'static str {
        match self {
            Self::Qr => "qr",
            Self::Code => "code",
            Self::Manual => "manual",
        }
    }
}

pub struct PairingDialog {
    pub dialog: adw::Dialog,
    app: Weak<App>,
    stack: adw::ViewStack,
    slot_choice: gtk::DropDown,
    qr_picture: gtk::Picture,
    qr_status: gtk::Label,
    qr_refresh: gtk::Button,
    qr_slot: Cell<Option<SlotId>>,
    qr_deadline: Cell<Option<Instant>>,
    code_list: gtk::ListBox,
    usb_list: gtk::ListBox,
    /// Code entries keyed by service instance, preserved across list rebuilds.
    typed_codes: RefCell<Vec<(String, String)>>,
}

fn instructions(text: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).wrap(true).xalign(0.0).use_markup(true).build();
    l.add_css_class("dim-label");
    l
}

fn page_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build()
}

impl PairingDialog {
    pub fn new(app: &Rc<App>, page: Page, slot: Option<SlotId>) -> Rc<Self> {
        let dialog = adw::Dialog::builder().title("Add Device").content_width(460).content_height(680).build();
        let stack = adw::ViewStack::new();
        let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();

        // Slot selection shared by all pages.
        let slot_choice = gtk::DropDown::from_strings(&["Next free slot", "Slot 1", "Slot 2", "Slot 3", "Slot 4", "Slot 5"]);
        slot_choice.set_selected(slot.map_or(0, u32::from));
        let slot_row = adw::ActionRow::builder().title("Assign to").build();
        slot_row.add_suffix(&slot_choice);
        slot_choice.set_valign(gtk::Align::Center);
        let slot_group = adw::PreferencesGroup::new();
        slot_group.add(&slot_row);

        // --- QR page ---
        let qr_page = page_box();
        qr_page.append(&instructions(
            "On the phone open <b>Settings › Developer options › Wireless debugging</b>, \
tap <b>Pair device with QR code</b> and scan this code.",
        ));
        let qr_picture = gtk::Picture::builder().width_request(240).height_request(240).halign(gtk::Align::Center).build();
        qr_picture.add_css_class("qr");
        let qr_status = gtk::Label::new(Some("Generating…"));
        let qr_refresh = gtk::Button::with_label("New QR code");
        qr_refresh.set_halign(gtk::Align::Center);
        qr_refresh.add_css_class("pill");
        qr_page.append(&qr_picture);
        qr_page.append(&qr_status);
        qr_page.append(&qr_refresh);

        // --- Code page ---
        let code_page = page_box();
        code_page.append(&instructions(
            "On the phone open <b>Wireless debugging</b> and tap <b>Pair device with pairing code</b>. \
It appears below; type the 6-digit code it shows.",
        ));
        let code_list = gtk::ListBox::new();
        code_list.add_css_class("boxed-list");
        code_list.set_selection_mode(gtk::SelectionMode::None);
        code_page.append(&code_list);
        let typed_group = adw::PreferencesGroup::builder()
            .title("Phone not listed?")
            .description(
                "Some routers don't pass discovery between Wi-Fi and wired PCs. Type what the phone shows instead: \
the pairing dialog's IP address &amp; port and code, and the port from the top of the Wireless debugging screen.",
            )
            .build();
        let pair_addr_row = adw::EntryRow::builder().title("Pairing IP:port, e.g. 192.168.0.84:41567").build();
        let pair_code_row = adw::EntryRow::builder().title("Wi-Fi pairing code").input_purpose(gtk::InputPurpose::Digits).build();
        let connect_row = adw::EntryRow::builder().title("Wireless debugging port, e.g. 37115").build();
        let typed_pair = gtk::Button::builder().label("Pair").halign(gtk::Align::End).margin_top(6).margin_bottom(6).build();
        typed_pair.add_css_class("suggested-action");
        typed_pair.add_css_class("pill");
        for r in [&pair_addr_row, &pair_code_row, &connect_row] {
            typed_group.add(r);
        }
        code_page.append(&typed_group);
        code_page.append(&typed_pair);

        // --- Manual page ---
        let manual_page = page_box();
        let manual_group = adw::PreferencesGroup::builder()
            .title("Connect by address")
            .description("The IP address and port shown under Wireless debugging (or IP alone for adb tcpip 5555).")
            .build();
        let addr_row = adw::EntryRow::builder().title("IP address[:port]").show_apply_button(true).build();
        addr_row.set_input_purpose(gtk::InputPurpose::FreeForm);
        manual_group.add(&addr_row);
        let usb_group = adw::PreferencesGroup::builder()
            .title("Legacy phones over USB")
            .description("Android 10 and older: plug the phone in once to switch it to Wi-Fi ADB (adb tcpip 5555).")
            .build();
        let usb_list = gtk::ListBox::new();
        usb_list.add_css_class("boxed-list");
        usb_list.set_selection_mode(gtk::SelectionMode::None);
        usb_group.add(&usb_list);
        manual_page.append(&manual_group);
        manual_page.append(&usb_group);

        let scrolled = |child: &gtk::Box| gtk::ScrolledWindow::builder().child(child).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
        stack.add_titled_with_icon(&scrolled(&qr_page), Some(Page::Qr.name()), "QR Code", "view-grid-symbolic");
        stack.add_titled_with_icon(&scrolled(&code_page), Some(Page::Code.name()), "Code", "dialog-password-symbolic");
        stack.add_titled_with_icon(&scrolled(&manual_page), Some(Page::Manual.name()), "Manual", "network-wired-symbolic");

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&stack);
        let slot_box = gtk::Box::builder().margin_start(18).margin_end(18).margin_bottom(18).build();
        slot_group.set_hexpand(true);
        slot_box.append(&slot_group);
        body.append(&slot_box);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));

        let this = Rc::new(Self {
            dialog,
            app: Rc::downgrade(app),
            stack,
            slot_choice,
            qr_picture,
            qr_status,
            qr_refresh,
            qr_slot: Cell::new(None),
            qr_deadline: Cell::new(None),
            code_list,
            usb_list,
            typed_codes: RefCell::new(Vec::new()),
        });

        {
            let weak = Rc::downgrade(&this);
            addr_row.connect_apply(move |row| {
                if let Some(this) = weak.upgrade() {
                    this.send(UiCommand::Connect { slot: this.chosen_slot(), addr: row.text().to_string() });
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            let submit = move || {
                let Some(this) = weak.upgrade() else { return };
                let connect = connect_row.text().to_string();
                this.send(UiCommand::PairAddr {
                    slot: this.chosen_slot(),
                    pair_addr: pair_addr_row.text().to_string(),
                    code: pair_code_row.text().to_string(),
                    connect: (!connect.trim().is_empty()).then_some(connect),
                });
            };
            typed_pair.connect_clicked(move |_| submit());
        }
        {
            let weak = Rc::downgrade(&this);
            this.qr_refresh.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.start_qr();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.stack.connect_visible_child_name_notify(move |stack| {
                let Some(this) = weak.upgrade() else { return };
                if stack.visible_child_name().as_deref() == Some(Page::Qr.name()) {
                    if this.qr_slot.get().is_none() {
                        this.start_qr();
                    }
                } else {
                    this.cancel_qr();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.dialog.connect_closed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_qr();
                    if let Some(app) = this.app.upgrade() {
                        app.pairing_closed();
                    }
                }
            });
        }
        // Countdown for the QR session.
        {
            let weak = Rc::downgrade(&this);
            glib::timeout_add_seconds_local(1, move || {
                let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
                this.tick();
                glib::ControlFlow::Continue
            });
        }

        this.stack.set_visible_child_name(page.name());
        if page == Page::Qr {
            this.start_qr();
        }
        this.update_nearby(&app.nearby());
        this.update_usb(&app.usb());
        this
    }

    fn send(&self, cmd: UiCommand) {
        if let Some(app) = self.app.upgrade() {
            app.send(cmd);
        }
    }

    fn chosen_slot(&self) -> Option<SlotId> {
        slot_from(&self.slot_choice)
    }

    fn start_qr(&self) {
        self.cancel_qr();
        self.qr_picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.qr_status.set_label("Generating…");
        self.qr_refresh.set_visible(false);
        self.send(UiCommand::PairQr { slot: self.chosen_slot() });
    }

    fn cancel_qr(&self) {
        if let Some(slot) = self.qr_slot.take() {
            self.send(UiCommand::CancelPairing { slot });
        }
        self.qr_deadline.set(None);
    }

    pub fn show_qr(&self, slot: SlotId, payload: &str, expires_in: Duration) {
        self.qr_slot.set(Some(slot));
        self.qr_deadline.set(Some(Instant::now() + expires_in));
        if let Some(tex) = crate::qr::texture(payload) {
            self.qr_picture.set_paintable(Some(&tex));
        }
        self.tick();
    }

    /// The manager ended the QR session (expired, failed, or paired).
    pub fn qr_slot_left_pairing(&self, slot: SlotId, error: Option<&str>) {
        if self.qr_slot.get() != Some(slot) {
            return;
        }
        self.qr_slot.set(None);
        self.qr_deadline.set(None);
        if let Some(e) = error {
            self.qr_status.set_label(e);
        }
        self.qr_picture.set_opacity(0.25);
        self.qr_refresh.set_visible(true);
    }

    fn tick(&self) {
        let Some(deadline) = self.qr_deadline.get() else { return };
        self.qr_picture.set_opacity(1.0);
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        self.qr_status.set_label(&format!("Waiting for the phone…  {}:{:02}", left / 60, left % 60));
    }

    pub fn update_nearby(&self, nearby: &[NearbyView]) {
        // Keep anything already typed into code entries.
        let mut typed = Vec::new();
        let mut child = self.code_list.first_child();
        while let Some(row) = child {
            if let (Some(instance), Some(entry)) = (row.widget_name().strip_prefix("pair:").map(str::to_owned), find_entry(&row)) {
                typed.push((instance, entry.text().to_string()));
            }
            child = row.next_sibling();
        }
        *self.typed_codes.borrow_mut() = typed;
        self.code_list.remove_all();

        let pairing: Vec<&NearbyView> = nearby.iter().filter(|n| n.kind == ServiceKind::Pairing && !n.instance.starts_with("studio-")).collect();
        if pairing.is_empty() {
            let row = adw::ActionRow::builder()
                .title("Waiting for a phone in pairing mode…")
                .subtitle("Phones appear here while their pairing dialog is open")
                .build();
            let spinner = gtk::Spinner::builder().spinning(true).build();
            row.add_prefix(&spinner);
            self.code_list.append(&row);
            return;
        }
        for n in pairing {
            let title = n.known_name.clone().or_else(|| n.serial.clone()).unwrap_or_else(|| n.instance.clone());
            let row = adw::ActionRow::builder().title(&title).subtitle(n.addr.to_string()).build();
            row.set_widget_name(&format!("pair:{}", n.instance));
            let entry = gtk::Entry::builder()
                .placeholder_text("000000")
                .max_length(7)
                .width_chars(7)
                .input_purpose(gtk::InputPurpose::Digits)
                .valign(gtk::Align::Center)
                .build();
            if let Some((_, code)) = self.typed_codes.borrow().iter().find(|(i, _)| *i == n.instance) {
                entry.set_text(code);
            }
            let button = gtk::Button::builder().label("Pair").valign(gtk::Align::Center).build();
            button.add_css_class("suggested-action");
            let submit = {
                let (app, instance, entry, choice) = (self.app.clone(), n.instance.clone(), entry.clone(), self.slot_choice.clone());
                move || {
                    if let Some(app) = app.upgrade() {
                        let slot = slot_from(&choice);
                        app.send(UiCommand::PairCode { slot, instance: instance.clone(), code: entry.text().to_string() });
                    }
                }
            };
            let s = submit.clone();
            entry.connect_activate(move |_| s());
            button.connect_clicked(move |_| submit());
            row.add_suffix(&entry);
            row.add_suffix(&button);
            self.code_list.append(&row);
        }
    }

    pub fn update_usb(&self, usb: &[UsbDeviceView]) {
        self.usb_list.remove_all();
        if usb.is_empty() {
            self.usb_list.append(&adw::ActionRow::builder().title("No USB phones attached").build());
            return;
        }
        for d in usb {
            let row = adw::ActionRow::builder().title(&d.model).subtitle(&d.serial).build();
            if d.authorized {
                let b = gtk::Button::builder().label("Enable Wi-Fi ADB").valign(gtk::Align::Center).build();
                let (app, serial, choice) = (self.app.clone(), d.serial.clone(), self.slot_choice.clone());
                b.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    if let Some(app) = app.upgrade() {
                        app.send(UiCommand::EnableWifiAdb { usb_serial: serial.clone(), slot: slot_from(&choice) });
                    }
                });
                row.add_suffix(&b);
            } else {
                row.set_subtitle("Accept the USB debugging prompt on the phone");
            }
            self.usb_list.append(&row);
        }
    }
}

/// "Next free slot" is index 0; "Slot N" is index N.
fn slot_from(choice: &gtk::DropDown) -> Option<SlotId> {
    match choice.selected() {
        0 => None,
        n => Some(n as SlotId),
    }
}

fn find_entry(row: &gtk::Widget) -> Option<gtk::Entry> {
    let mut stack = vec![row.clone()];
    while let Some(w) = stack.pop() {
        if let Ok(e) = w.clone().downcast::<gtk::Entry>() {
            return Some(e);
        }
        let mut c = w.first_child();
        while let Some(ch) = c {
            c = ch.next_sibling();
            stack.push(ch);
        }
    }
    None
}
