//! One device slot in the dashboard. Milestone 2 replaces the placeholder
//! phone frame with the live mirror.

use std::cell::Cell;
use std::rc::Rc;

use ac_core::{DeviceStats, SlotId, TransportKind};
use ac_device::{PhaseView, SlotView};
use adw::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    AddDevice,
    Reconnect,
    Cancel,
    Disconnect,
    Forget,
}

pub struct SlotCard {
    pub root: gtk::Box,
    title: gtk::Label,
    badge: gtk::Label,
    frame_icon: gtk::Image,
    spinner: gtk::Spinner,
    status: gtk::Label,
    detail: gtk::Label,
    stats_row: gtk::Box,
    battery_icon: gtk::Image,
    battery: gtk::Label,
    wifi_icon: gtk::Image,
    wifi: gtk::Label,
    rtt: gtk::Label,
    primary: gtk::Button,
    forget: gtk::Button,
    phase: Rc<Cell<PhaseView>>,
}

fn dim_label(text: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    l.add_css_class("dim-label");
    l
}

impl SlotCard {
    pub fn new(slot: SlotId, on_action: Rc<dyn Fn(SlotId, CardAction)>) -> Self {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).hexpand(true).build();
        root.add_css_class("card");
        root.add_css_class("slot-card");

        // Header: slot number, name, state badge.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let number = gtk::Label::new(Some(&slot.to_string()));
        number.add_css_class("slot-number");
        let title = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        title.add_css_class("heading");
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        header.append(&number);
        header.append(&title);
        header.append(&badge);

        // Placeholder for the mirror.
        let frame = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).vexpand(true).valign(gtk::Align::Fill).build();
        frame.add_css_class("phone-frame");
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).vexpand(true).valign(gtk::Align::Center).build();
        let frame_icon = gtk::Image::from_icon_name("phone-symbolic");
        frame_icon.set_pixel_size(56);
        frame_icon.add_css_class("dim-label");
        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).max_width_chars(22).build();
        inner.append(&frame_icon);
        inner.append(&spinner);
        inner.append(&status);
        frame.append(&inner);

        let detail = dim_label("");
        detail.add_css_class("caption");

        // Telemetry.
        let stats_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        stats_row.add_css_class("caption");
        let battery_icon = gtk::Image::from_icon_name("battery-level-50-symbolic");
        let battery = gtk::Label::new(None);
        let wifi_icon = gtk::Image::from_icon_name("network-wireless-signal-none-symbolic");
        wifi_icon.set_margin_start(8);
        let wifi = gtk::Label::new(None);
        let rtt = gtk::Label::builder().hexpand(true).xalign(1.0).build();
        rtt.add_css_class("numeric");
        for w in [battery_icon.upcast_ref::<gtk::Widget>(), battery.upcast_ref(), wifi_icon.upcast_ref(), wifi.upcast_ref(), rtt.upcast_ref()] {
            stats_row.append(w);
        }
        stats_row.set_visible(false);

        // Actions.
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let primary = gtk::Button::builder().hexpand(true).build();
        primary.add_css_class("pill");
        let forget = gtk::Button::from_icon_name("user-trash-symbolic");
        forget.set_tooltip_text(Some("Forget this device"));
        forget.add_css_class("flat");
        buttons.append(&primary);
        buttons.append(&forget);

        let phase = Rc::new(Cell::new(PhaseView::Empty));
        {
            let (phase, on_action) = (phase.clone(), on_action.clone());
            primary.connect_clicked(move |_| {
                let action = match phase.get() {
                    PhaseView::Empty => CardAction::AddDevice,
                    PhaseView::Disconnected => CardAction::Reconnect,
                    PhaseView::Pairing | PhaseView::Connecting => CardAction::Cancel,
                    PhaseView::Online | PhaseView::Reconnecting => CardAction::Disconnect,
                };
                on_action(slot, action);
            });
        }
        forget.connect_clicked(move |_| on_action(slot, CardAction::Forget));

        root.append(&header);
        root.append(&frame);
        root.append(&detail);
        root.append(&stats_row);
        root.append(&buttons);

        let card = Self {
            root,
            title,
            badge,
            frame_icon,
            spinner,
            status,
            detail,
            stats_row,
            battery_icon,
            battery,
            wifi_icon,
            wifi,
            rtt,
            primary,
            forget,
            phase,
        };
        card.update(&SlotView {
            slot,
            phase: PhaseView::Empty,
            title: String::new(),
            device_id: None,
            model: None,
            manufacturer: None,
            sdk: None,
            addr: None,
            transport: TransportKind::Tls,
            detail: String::new(),
            error: None,
        });
        card
    }

    pub fn update(&self, v: &SlotView) {
        self.phase.set(v.phase);
        let (badge, class, primary, status) = match v.phase {
            PhaseView::Empty => ("Empty", "empty", "Add device", v.error.clone().unwrap_or_else(|| "No device".into())),
            PhaseView::Disconnected => (
                "Offline",
                "disconnected",
                "Connect",
                v.error.clone().unwrap_or_else(|| "Disconnected".into()),
            ),
            PhaseView::Pairing => ("Pairing", "pairing", "Cancel", v.detail.clone()),
            PhaseView::Connecting => ("Connecting", "connecting", "Cancel", v.detail.clone()),
            PhaseView::Online => ("Online", "online", "Disconnect", "Connected — mirroring arrives in Milestone 2".into()),
            PhaseView::Reconnecting => ("Reconnecting", "reconnecting", "Stop", v.detail.clone()),
        };
        for c in ["empty", "disconnected", "pairing", "connecting", "online", "reconnecting"] {
            self.badge.remove_css_class(c);
        }
        self.badge.add_css_class(class);
        self.badge.set_label(badge);

        self.title.set_label(if v.phase == PhaseView::Empty { "" } else { &v.title });
        self.status.set_label(&status);
        self.primary.set_label(primary);
        if v.phase == PhaseView::Online || v.phase == PhaseView::Empty {
            self.primary.remove_css_class("suggested-action");
        } else if v.phase == PhaseView::Disconnected {
            self.primary.add_css_class("suggested-action");
        }
        self.forget.set_visible(v.device_id.is_some());

        let busy = matches!(v.phase, PhaseView::Pairing | PhaseView::Connecting | PhaseView::Reconnecting);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        self.frame_icon.set_opacity(if v.phase == PhaseView::Online { 1.0 } else { 0.5 });

        let mut detail = Vec::new();
        if let Some(sdk) = v.sdk {
            detail.push(format!("Android {}", android_version(sdk)));
        }
        if let Some(addr) = &v.addr {
            detail.push(addr.clone());
        }
        if v.phase != PhaseView::Empty {
            detail.push(match v.transport {
                TransportKind::Tls => "TLS".into(),
                TransportKind::Legacy => "tcpip".into(),
            });
        }
        self.detail.set_label(&detail.join(" · "));
        self.detail.set_tooltip_text(v.device_id.as_deref().map(|id| format!("Serial: {id}")).as_deref());

        if v.phase != PhaseView::Online {
            self.stats_row.set_visible(false);
        }
    }

    pub fn update_stats(&self, s: &DeviceStats) {
        if self.phase.get() != PhaseView::Online {
            return;
        }
        self.stats_row.set_visible(true);
        match s.battery_pct {
            Some(pct) => {
                let level = (pct as u32 + 5) / 10 * 10;
                let suffix = if s.charging { "-charging" } else { "" };
                self.battery_icon.set_icon_name(Some(&format!("battery-level-{level}{suffix}-symbolic")));
                self.battery.set_label(&format!("{pct}%"));
            }
            None => self.battery.set_label("—"),
        }
        let icon = match s.wifi_bars() {
            Some(4) => "network-wireless-signal-excellent-symbolic",
            Some(3) => "network-wireless-signal-good-symbolic",
            Some(2) => "network-wireless-signal-ok-symbolic",
            Some(_) => "network-wireless-signal-weak-symbolic",
            None => "network-wireless-signal-none-symbolic",
        };
        self.wifi_icon.set_icon_name(Some(icon));
        self.wifi.set_label(&s.wifi_rssi_dbm.map(|r| format!("{r} dBm")).unwrap_or_default());
        self.rtt.set_label(&s.rtt_ms.map(|ms| format!("{ms} ms")).unwrap_or_else(|| "—".into()));
    }
}

pub fn android_version(sdk: u32) -> String {
    match sdk {
        36 => "16".into(),
        35 => "15".into(),
        34 => "14".into(),
        33 => "13".into(),
        32 => "12L".into(),
        31 => "12".into(),
        30 => "11".into(),
        29 => "10".into(),
        28 => "9".into(),
        27 => "8.1".into(),
        26 => "8".into(),
        25 => "7.1".into(),
        24 => "7".into(),
        23 => "6".into(),
        22 => "5.1".into(),
        21 => "5".into(),
        s => format!("(API {s})"),
    }
}
