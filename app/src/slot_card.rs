//! One device slot in the dashboard: header, live mirror (or a placeholder
//! while not streaming), telemetry and actions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ac_core::{DeviceStats, SlotId, TransportKind};
use ac_device::transfer::human_bytes;
use ac_device::{PhaseView, SlotView, StreamView, TransferView};
use adw::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    AddDevice,
    Reconnect,
    Cancel,
    Disconnect,
    Forget,
    Focus,
    RetryStream,
    /// The broadcast target checkbox changed.
    BroadcastTarget(bool),
    /// The "keep screen on" toggle changed.
    StayAwake(bool),
    SendFiles,
    SendFolder,
    CancelTransfers,
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
    focus: gtk::Button,
    retry: gtk::Button,
    frame_stack: gtk::Stack,
    pub picture: gtk::Picture,
    video_info: gtk::Label,
    target: gtk::CheckButton,
    target_warning: gtk::Image,
    send: gtk::MenuButton,
    stay_awake: gtk::ToggleButton,
    /// Set while `update` moves the toggle, so it doesn't echo a command.
    syncing: Rc<Cell<bool>>,
    drop_hint: gtk::Label,
    transfer_row: gtk::Box,
    transfer_label: gtk::Label,
    transfer_bar: gtk::ProgressBar,
    phase: Rc<Cell<PhaseView>>,
    /// "Pixel 8, streaming" for the accessible description.
    a11y: RefCell<String>,
}

fn dim_label(text: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    l.add_css_class("dim-label");
    l
}

impl SlotCard {
    pub fn new(slot: SlotId, on_action: Rc<dyn Fn(SlotId, CardAction)>, picture: gtk::Picture, video: gtk::Widget) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Group)
            .build();
        root.add_css_class("card");
        root.add_css_class("slot-card");

        // Header: slot number, name, state badge.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let number = gtk::Label::new(Some(&slot.to_string()));
        number.add_css_class("slot-number");
        // Broadcast target (spec §4.10), shown while broadcast is on.
        let target = gtk::CheckButton::builder()
            .tooltip_text("Send broadcast input to this phone")
            .visible(false)
            .build();
        target.update_property(&[gtk::accessible::Property::Label(&format!("Broadcast to slot {slot}"))]);
        let target_warning = gtk::Image::builder().icon_name("dialog-warning-symbolic").visible(false).build();
        target_warning.add_css_class("warning");
        let title = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        title.add_css_class("heading");
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        header.append(&number);
        header.append(&target);
        header.append(&target_warning);
        header.append(&title);
        header.append(&badge);
        let focus = gtk::Button::from_icon_name("view-fullscreen-symbolic");
        focus.set_tooltip_text(Some("Focus this phone (Alt+F)"));
        focus.add_css_class("flat");
        focus.set_visible(false);
        // Send Files… / Send Folder… (spec §5.1 internal picker).
        let send_popover = gtk::Popover::new();
        let send_menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for (label, action) in [("Send Files…", CardAction::SendFiles), ("Send Folder…", CardAction::SendFolder)] {
            let b = gtk::Button::builder().label(label).build();
            b.add_css_class("flat");
            b.child().and_downcast::<gtk::Label>().inspect(|l| l.set_xalign(0.0));
            let (on_action, popover) = (on_action.clone(), send_popover.clone());
            b.connect_clicked(move |_| {
                popover.popdown();
                on_action(slot, action);
            });
            send_menu.append(&b);
        }
        send_popover.set_child(Some(&send_menu));
        let send = gtk::MenuButton::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Send files to this phone (or drop them on it)")
            .popover(&send_popover)
            .visible(false)
            .build();
        send.add_css_class("flat");
        let stay_awake = gtk::ToggleButton::builder()
            .icon_name("display-brightness-symbolic")
            .tooltip_text("Keep screen on: the phone's screen never turns off by itself")
            .visible(false)
            .build();
        stay_awake.add_css_class("flat");
        stay_awake.add_css_class("stay-awake");
        stay_awake.update_property(&[gtk::accessible::Property::Label("Keep screen on")]);
        let syncing = Rc::new(Cell::new(false));
        {
            let (on_action, syncing) = (on_action.clone(), syncing.clone());
            stay_awake.connect_toggled(move |b| {
                if !syncing.get() {
                    on_action(slot, CardAction::StayAwake(b.is_active()));
                }
            });
        }
        header.append(&stay_awake);
        header.append(&send);
        header.append(&focus);

        // The mirror, or a placeholder while not streaming.
        let frame = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).vexpand(true).valign(gtk::Align::Fill).build();
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).vexpand(true).valign(gtk::Align::Center).build();
        let frame_icon = gtk::Image::from_icon_name("phone-symbolic");
        frame_icon.set_pixel_size(56);
        frame_icon.add_css_class("dim-label");
        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).max_width_chars(22).build();
        inner.append(&frame_icon);
        inner.append(&spinner);
        inner.append(&status);
        let retry = gtk::Button::builder().label("Retry mirroring").halign(gtk::Align::Center).visible(false).build();
        inner.append(&retry);
        frame.append(&inner);
        frame.add_css_class("phone-frame");
        let frame_stack = gtk::Stack::builder().vexpand(true).transition_type(gtk::StackTransitionType::Crossfade).build();
        frame_stack.add_named(&frame, Some("placeholder"));
        video.add_css_class("video-frame");
        frame_stack.add_named(&video, Some("video"));
        // Shown while files are dragged over the tile.
        let drop_hint = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .visible(false)
            .build();
        drop_hint.add_css_class("drop-hint");
        let frame_overlay = gtk::Overlay::builder().child(&frame_stack).vexpand(true).build();
        frame_overlay.add_overlay(&drop_hint);

        // Transfer progress (spec §5.7): never covers the mirror.
        let transfer_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        transfer_row.add_css_class("transfer-row");
        let transfer_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3).hexpand(true).valign(gtk::Align::Center).build();
        let transfer_label = dim_label("");
        transfer_label.add_css_class("caption");
        transfer_label.add_css_class("numeric");
        let transfer_bar = gtk::ProgressBar::new();
        transfer_col.append(&transfer_label);
        transfer_col.append(&transfer_bar);
        let transfer_cancel = gtk::Button::from_icon_name("window-close-symbolic");
        transfer_cancel.set_tooltip_text(Some("Cancel transfers"));
        transfer_cancel.add_css_class("flat");
        transfer_cancel.add_css_class("circular");
        transfer_cancel.set_valign(gtk::Align::Center);
        {
            let on_action = on_action.clone();
            transfer_cancel.connect_clicked(move |_| on_action(slot, CardAction::CancelTransfers));
        }
        transfer_row.append(&transfer_col);
        transfer_row.append(&transfer_cancel);
        transfer_row.set_visible(false);

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
        let video_info = dim_label("");
        video_info.add_css_class("caption");
        video_info.add_css_class("numeric");
        let rtt = gtk::Label::builder().hexpand(true).xalign(1.0).build();
        rtt.add_css_class("numeric");
        for w in [
            battery_icon.upcast_ref::<gtk::Widget>(),
            battery.upcast_ref(),
            wifi_icon.upcast_ref(),
            wifi.upcast_ref(),
            rtt.upcast_ref(),
        ] {
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
        {
            let on_action = on_action.clone();
            focus.connect_clicked(move |_| on_action(slot, CardAction::Focus));
        }
        {
            let on_action = on_action.clone();
            retry.connect_clicked(move |_| on_action(slot, CardAction::RetryStream));
        }
        {
            let on_action = on_action.clone();
            target.connect_toggled(move |b| on_action(slot, CardAction::BroadcastTarget(b.is_active())));
        }
        {
            // Double-clicking the header focuses the phone (spec §6.2).
            let on_action = on_action.clone();
            let dbl = gtk::GestureClick::new();
            dbl.connect_pressed(move |_, n, _, _| {
                if n == 2 {
                    on_action(slot, CardAction::Focus);
                }
            });
            header.add_controller(dbl);
        }
        forget.connect_clicked(move |_| on_action(slot, CardAction::Forget));

        root.append(&header);
        root.append(&frame_overlay);
        root.append(&transfer_row);
        root.append(&detail);
        root.append(&video_info);
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
            focus,
            retry,
            frame_stack,
            picture,
            video_info,
            target,
            target_warning,
            send,
            stay_awake,
            syncing,
            drop_hint,
            transfer_row,
            transfer_label,
            transfer_bar,
            phase,
            a11y: RefCell::default(),
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
            stay_awake: None,
            stream: StreamView::Off,
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
            PhaseView::Online => ("Online", "online", "Disconnect", match &v.stream {
                StreamView::Off | StreamView::Starting => "Starting mirroring…".into(),
                StreamView::Live { .. } => String::new(),
                StreamView::Failed(e) => format!("Mirroring failed: {e}"),
                StreamView::Unavailable(e) if e.starts_with("Mirroring unavailable") => e.clone(),
                StreamView::Unavailable(e) => format!("Mirroring unavailable: {e}"),
            }),
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

        let online = v.phase == PhaseView::Online;
        let starting = online && matches!(v.stream, StreamView::Off | StreamView::Starting);
        // Keep the last frame on screen across a restart (profile switch, rotation retry).
        let show_video = online
            && match v.stream {
                StreamView::Live { .. } => true,
                StreamView::Starting => self.picture.paintable().is_some(),
                _ => false,
            };
        if !online {
            self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        }
        self.frame_stack.set_visible_child_name(if show_video { "video" } else { "placeholder" });
        self.focus.set_visible(online && matches!(v.stream, StreamView::Live { .. } | StreamView::Starting));
        self.retry.set_visible(online && matches!(v.stream, StreamView::Failed(_)));
        self.send.set_visible(online);
        self.stay_awake.set_visible(online);
        // Insensitive until the phone's current state is known.
        self.stay_awake.set_sensitive(v.stay_awake.is_some());
        self.syncing.set(true);
        self.stay_awake.set_active(v.stay_awake == Some(true));
        self.syncing.set(false);
        self.video_info.set_visible(show_video);

        let busy = starting || matches!(v.phase, PhaseView::Pairing | PhaseView::Connecting | PhaseView::Reconnecting);
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
                TransportKind::Direct => "no ADB".into(),
            });
        }
        self.detail.set_label(&detail.join(" · "));
        self.detail.set_tooltip_text(v.device_id.as_deref().map(|id| format!("Serial: {id}")).as_deref());

        if v.phase != PhaseView::Online {
            self.stats_row.set_visible(false);
        }

        let state = match (v.phase, &v.stream) {
            (PhaseView::Online, StreamView::Live { .. }) => "streaming",
            (PhaseView::Online, StreamView::Failed(_) | StreamView::Unavailable(_)) => "connected, not mirroring",
            (PhaseView::Online, _) => "starting",
            _ => badge,
        };
        let who = if v.phase == PhaseView::Empty { format!("Slot {}", v.slot) } else { v.title.clone() };
        *self.a11y.borrow_mut() = format!("{who}, {}", state.to_lowercase());
        self.root.update_property(&[gtk::accessible::Property::Description(&self.a11y.borrow())]);
    }

    /// Text over the tile while files are dragged onto it; `None` hides it.
    pub fn set_drop_hint(&self, hint: Option<&str>) {
        self.drop_hint.set_label(hint.unwrap_or_default());
        self.drop_hint.set_visible(hint.is_some());
        if hint.is_some() {
            self.root.add_css_class("drop-hover");
        } else {
            self.root.remove_css_class("drop-hover");
        }
    }

    pub fn set_transfer(&self, v: &TransferView) {
        self.transfer_row.set_visible(v.active);
        if !v.active {
            return;
        }
        let verb = if v.installing { "Installing" } else { "Sending" };
        let mut parts = vec![if v.file.is_empty() { format!("{verb}…") } else { format!("{verb} {}", v.file) }];
        if v.count > 1 {
            parts.push(format!("{} of {}", v.index, v.count));
        }
        if v.waiting {
            parts.push("Paused — waiting for the phone".into());
        } else {
            parts.push(format!("{} / {}", human_bytes(v.sent), human_bytes(v.total)));
            if v.rate_bps > 0 {
                parts.push(format!("{}/s", human_bytes(v.rate_bps)));
            }
            parts.push(match v.eta_s {
                Some(s) if s >= 3600 => format!("{}h {:02}m left", s / 3600, s / 60 % 60),
                Some(s) => format!("{}:{:02} left", s / 60, s % 60),
                None => "—".into(),
            });
        }
        let text = parts.join(" · ");
        self.transfer_label.set_label(&text);
        self.transfer_label.set_tooltip_text(Some(&text));
        self.transfer_bar.set_fraction(if v.total == 0 { 0.0 } else { v.sent as f64 / v.total as f64 });
    }

    /// An empty slot: shrinks to the narrow "Add" column (spec §6.1).
    pub fn set_compact(&self, compact: bool) {
        if compact {
            self.root.add_css_class("compact");
        } else {
            self.root.remove_css_class("compact");
        }
        self.frame_icon.set_pixel_size(if compact { 32 } else { 56 });
    }

    /// Broadcast checkbox, its ⚠ (with the reason as tooltip), and the red
    /// border on targets (spec §4.10).
    pub fn set_broadcast(&self, enabled: bool, target: bool, warning: Option<&str>) {
        self.target.set_visible(enabled);
        self.target.set_active(target);
        self.target_warning.set_visible(enabled && target && warning.is_some());
        self.target_warning.set_tooltip_text(warning);
        if enabled && target {
            self.root.add_css_class("broadcast-target");
        } else {
            self.root.remove_css_class("broadcast-target");
        }
    }

    /// `fps · backend · lag` under the mirror, refreshed once a second.
    pub fn set_video_info(&self, text: &str) {
        self.video_info.set_label(text);
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
        let mut desc = self.a11y.borrow().clone();
        if let Some(pct) = s.battery_pct {
            desc.push_str(&format!(", battery {pct} percent"));
        }
        self.root.update_property(&[gtk::accessible::Property::Description(&desc)]);
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
