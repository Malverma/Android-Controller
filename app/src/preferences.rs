//! Preferences (Ctrl+,): an `AdwPreferencesDialog` over `config.toml`
//! (spec §7.3). Every change is validated, saved and applied at once; the
//! subtitles say when a setting only takes effect later.

use std::rc::Rc;

use ac_config::{Config, MaxSize};
use adw::prelude::*;

use crate::window::App;

const LATER_STREAM: &str = "Applies the next time a phone starts mirroring";

type Set<T> = fn(&mut Config, T);

pub fn show(app: &Rc<App>) {
    let dialog = adw::PreferencesDialog::builder().title("Preferences").search_enabled(true).build();
    let cfg = app.config();
    let locked = app.config_locked();
    let ctx = Ctx { app: app.clone(), dialog: dialog.clone(), locked: locked.is_some() };

    let pages = [general(&ctx, &cfg), video(&ctx, &cfg), input(&ctx, &cfg), files(&ctx, &cfg), testing(&ctx, &cfg)];
    for page in &pages {
        if let Some(why) = &locked {
            page.set_description(&format!(
                "Read-only: config.toml has an error, so the app is using defaults. Fix or delete the file, then restart.\n{why}"
            ));
        }
        dialog.add(page);
    }
    dialog.present(Some(app.window()));
}

#[derive(Clone)]
struct Ctx {
    app: Rc<App>,
    dialog: adw::PreferencesDialog,
    /// config.toml could not be read: show the settings but disable them.
    locked: bool,
}

impl Ctx {
    fn group(&self, page: &adw::PreferencesPage, title: &str, description: &str) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title(title).sensitive(!self.locked).build();
        if !description.is_empty() {
            g.set_description(Some(description));
        }
        page.add(&g);
        g
    }

    /// Applies one edit to the current settings; shows why it was refused.
    fn change(&self, edit: impl FnOnce(&mut Config)) -> bool {
        let mut cfg = self.app.config();
        edit(&mut cfg);
        match self.app.apply_config(cfg) {
            Ok(()) => true,
            Err(e) => {
                self.dialog.add_toast(adw::Toast::new(&format!("Not saved: {e}")));
                false
            }
        }
    }

    fn switch(&self, group: &adw::PreferencesGroup, title: &str, subtitle: &str, value: bool, set: Set<bool>) {
        let row = adw::SwitchRow::builder().title(title).active(value).build();
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        let ctx = self.clone();
        row.connect_active_notify(move |r| {
            ctx.change(|c| set(c, r.is_active()));
        });
        group.add(&row);
    }

    /// A drop-down of `(value, label)`; an unknown current value shows the first.
    fn combo(&self, group: &adw::PreferencesGroup, title: &str, subtitle: &str, options: &'static [(&'static str, &'static str)], value: &str, set: Set<&'static str>) {
        let labels: Vec<&str> = options.iter().map(|o| o.1).collect();
        let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&labels)).build();
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        row.set_selected(options.iter().position(|o| o.0 == value).unwrap_or(0) as u32);
        let ctx = self.clone();
        row.connect_selected_notify(move |r| {
            if let Some(o) = options.get(r.selected() as usize) {
                ctx.change(|c| set(c, o.0));
            }
        });
        group.add(&row);
    }

    #[allow(clippy::too_many_arguments)]
    fn spin(&self, group: &adw::PreferencesGroup, title: &str, subtitle: &str, (min, max, step): (u32, u32, u32), value: u32, set: Set<u32>) {
        let row = adw::SpinRow::with_range(min as f64, max as f64, step as f64);
        row.set_title(title);
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        row.set_value(value as f64);
        let ctx = self.clone();
        row.connect_value_notify(move |r| {
            ctx.change(|c| set(c, r.value().round() as u32));
        });
        group.add(&row);
    }
}

fn page(title: &str, icon: &str) -> adw::PreferencesPage {
    adw::PreferencesPage::builder().title(title).icon_name(icon).build()
}

fn general(ctx: &Ctx, cfg: &Config) -> adw::PreferencesPage {
    let p = page("General", "preferences-system-symbolic");
    let g = ctx.group(&p, "Phones", "");
    ctx.spin(&g, "Phone slots", "How many phones fit in the window. Takes effect after restarting the app", (1, ac_core::MAX_SLOTS as u32, 1), cfg.general.max_slots as u32, |c, v| c.general.max_slots = v as u8);
    ctx.switch(&g, "Reconnect saved phones automatically", "Connect to known phones as soon as they show up on the network", cfg.discovery.auto_connect_known, |c, v| c.discovery.auto_connect_known = v);

    let g = ctx.group(&p, "Appearance", "");
    const SCHEMES: &[(&str, &str)] = &[("system", "Follow system"), ("light", "Light"), ("dark", "Dark")];
    ctx.combo(&g, "Style", "", SCHEMES, &cfg.ui.color_scheme, |c, v| c.ui.color_scheme = v.into());
    p
}

fn profile_mut(c: &mut Config, name: fn(&Config) -> &String) -> &mut ac_config::Profile {
    let n = name(c).clone();
    c.profiles.entry(n).or_default()
}

fn grid_name(c: &Config) -> &String {
    &c.video.grid_profile
}

fn focus_name(c: &Config) -> &String {
    &c.video.focus_profile
}

fn video(ctx: &Ctx, cfg: &Config) -> adw::PreferencesPage {
    let p = page("Video & Audio", "video-display-symbolic");
    let grid = cfg.profiles.get(&cfg.video.grid_profile).cloned().unwrap_or_default();
    let focus = cfg.profiles.get(&cfg.video.focus_profile).cloned().unwrap_or_default();

    let g = ctx.group(&p, "All phones view", LATER_STREAM);
    ctx.spin(&g, "Bitrate (kbit/s)", "Higher looks sharper but uses more Wi-Fi", (250, 50_000, 250), grid.bitrate_kbps, |c, v| profile_mut(c, grid_name).bitrate_kbps = v);
    ctx.spin(&g, "Frames per second", "", (5, 120, 5), grid.max_fps, |c, v| profile_mut(c, grid_name).max_fps = v);

    ctx.switch(&g, "Adapt to the Wi-Fi", "Lower the quality for a while when the video falls behind, and raise it again once the network keeps up", cfg.video.adaptive_bitrate, |c, v| c.video.adaptive_bitrate = v);

    let g = ctx.group(&p, "Focus view", LATER_STREAM);
    ctx.spin(&g, "Bitrate (kbit/s)", "", (250, 50_000, 250), focus.bitrate_kbps, |c, v| profile_mut(c, focus_name).bitrate_kbps = v);
    ctx.spin(&g, "Frames per second", "", (5, 120, 5), focus.max_fps, |c, v| profile_mut(c, focus_name).max_fps = v);
    const SIZES: &[(&str, &str)] = &[("1280", "1280 px"), ("1600", "1600 px"), ("1920", "1920 px (default)"), ("2560", "2560 px"), ("0", "Phone's full resolution")];
    let size = match focus.max_size {
        MaxSize::Px(px) => px.to_string(),
        MaxSize::Auto(_) => "1920".into(),
    };
    ctx.combo(&g, "Longest side", "", SIZES, &size, |c, v| profile_mut(c, focus_name).max_size = MaxSize::Px(v.parse().unwrap_or(1920)));
    const CODECS: &[(&str, &str)] = &[("h265-if-supported", "H.265 when the phone has it"), ("h264", "H.264 (works everywhere)"), ("av1-if-supported", "AV1 when the phone has it")];
    ctx.combo(&g, "Video format", "", CODECS, &focus.codec, |c, v| profile_mut(c, focus_name).codec = v.into());

    let g = ctx.group(&p, "Audio", "The focused phone's sound plays on this PC. Needs Android 11 or newer and a wireless-debugging (ADB) connection; the phone itself goes quiet meanwhile");
    ctx.switch(&g, "Play phone audio in focus view", LATER_STREAM, focus.audio, |c, v| profile_mut(c, focus_name).audio = v);
    ctx.spin(&g, "Volume (%)", "Applies the next time focus view opens", (0, 100, 5), (cfg.audio.volume * 100.0).round() as u32, |c, v| c.audio.volume = v as f64 / 100.0);

    let g = ctx.group(&p, "On the phone", LATER_STREAM);
    ctx.switch(&g, "Show touches", "Draw a dot where each tap lands", cfg.connection.show_touches, |c, v| c.connection.show_touches = v);
    p
}

const ACTIONS: &[(&str, &str)] = &[
    ("back", "Back"),
    ("home", "Home"),
    ("app_switch", "Recent apps"),
    ("notifications", "Notifications"),
    ("quick_settings", "Quick settings"),
    ("menu", "Menu"),
    ("volume_up", "Volume up"),
    ("volume_down", "Volume down"),
    ("rotate", "Rotate"),
    ("power", "Power"),
    ("none", "Nothing"),
];

fn input(ctx: &Ctx, cfg: &Config) -> adw::PreferencesPage {
    let p = page("Input", "input-keyboard-symbolic");
    let g = ctx.group(&p, "Keyboard", "");
    const MODES: &[(&str, &str)] = &[("mixed", "Mixed (recommended)"), ("text", "Text"), ("raw", "Raw keys (for games)")];
    ctx.combo(&g, "Typing", "Raw sends every letter as a key press, which some games need", MODES, &cfg.input.key_inject_mode, |c, v| c.input.key_inject_mode = v.into());
    ctx.switch(&g, "Share the clipboard", "Copying on the PC or a phone makes it available on the other", cfg.input.clipboard_autosync, |c, v| c.input.clipboard_autosync = v);

    let g = ctx.group(&p, "Mouse", "");
    ctx.combo(&g, "Right click", "", ACTIONS, &cfg.input.right_click, |c, v| c.input.right_click = v.into());
    ctx.combo(&g, "Middle click", "", ACTIONS, &cfg.input.middle_click, |c, v| c.input.middle_click = v.into());

    let g = ctx.group(&p, "Broadcast and Mirror clicks", "Input sent to several phones at once");
    ctx.switch(&g, "Include the Power button", "Off: Power only reaches the phone you press it on", cfg.input.broadcast_includes_power, |c, v| c.input.broadcast_includes_power = v);
    ctx.switch(&g, "Skip phones turned the other way", "Off: taps land in the same relative spot even on a phone in the other orientation", cfg.input.broadcast_skip_mismatched_orientation, |c, v| c.input.broadcast_skip_mismatched_orientation = v);
    p
}

fn files(ctx: &Ctx, cfg: &Config) -> adw::PreferencesPage {
    let p = page("Files", "folder-symbolic");
    let g = ctx.group(&p, "Sending files", "");
    let dir = adw::EntryRow::builder().title("Folder on the phone").text(&cfg.transfer.default_target_dir).show_apply_button(true).build();
    {
        let ctx = ctx.clone();
        dir.connect_apply(move |r| {
            let text = r.text().trim().trim_end_matches('/').to_owned();
            if let Some(why) = ac_config::check_target_dir(&text) {
                ctx.dialog.add_toast(adw::Toast::new(&format!("The folder {why}")));
                return;
            }
            if ctx.change(|c| c.transfer.default_target_dir = text.clone()) {
                ctx.dialog.add_toast(adw::Toast::new(&format!("Files now go to {text}")));
            }
        });
    }
    g.add(&dir);
    const CONFLICT: &[(&str, &str)] = &[("rename", "Keep both (add a number)"), ("overwrite", "Replace it"), ("skip", "Skip the file")];
    ctx.combo(&g, "When the phone already has the file", "", CONFLICT, &cfg.transfer.on_conflict, |c, v| c.transfer.on_conflict = v.into());
    const APK: &[(&str, &str)] = &[("install", "Install it"), ("push", "Copy it as a file"), ("ask", "Ask each time")];
    ctx.combo(&g, "Dropping an APK", "Hold Shift while dropping to do the other one", APK, &cfg.transfer.apk_drop_action, |c, v| c.transfer.apk_drop_action = v.into());
    ctx.switch(&g, "Add photos, videos and music to the gallery", "", cfg.transfer.trigger_media_scan, |c, v| c.transfer.trigger_media_scan = v);
    const VERIFY: &[(&str, &str)] = &[("large", "For files of 64 MB or more"), ("always", "Always"), ("never", "Never")];
    ctx.combo(&g, "Check files arrived intact", "Compares a checksum after sending", VERIFY, &cfg.transfer.verify_checksum, |c, v| c.transfer.verify_checksum = v.into());

    let g = ctx.group(&p, "Speed", "0 means no limit");
    ctx.spin(&g, "Limit while mirroring (Mbit/s)", "Keeps the phone's video smooth during a transfer", (0, 1000, 5), cfg.transfer.rate_limit_while_streaming_mbps, |c, v| c.transfer.rate_limit_while_streaming_mbps = v);
    ctx.spin(&g, "Limit otherwise (Mbit/s)", "", (0, 1000, 5), cfg.transfer.rate_limit_idle_mbps, |c, v| c.transfer.rate_limit_idle_mbps = v);
    p
}

fn testing(ctx: &Ctx, cfg: &Config) -> adw::PreferencesPage {
    let p = page("Testing", "applications-engineering-symbolic");
    let g = ctx.group(&p, "Saving", "Test runs, screenshots and recordings go here, one folder per run");
    let default = ac_device::testing::output_root("");
    let dir = adw::EntryRow::builder()
        .title(format!("Folder (empty: {})", default.display()))
        .text(&cfg.testing.output_dir)
        .show_apply_button(true)
        .build();
    {
        let ctx = ctx.clone();
        dir.connect_apply(move |r| {
            let text = r.text().trim().to_owned();
            if ctx.change(|c| c.testing.output_dir = text.clone()) {
                let shown = ac_device::testing::output_root(&text);
                ctx.dialog.add_toast(adw::Toast::new(&format!("Saving to {}", shown.display())));
            }
        });
    }
    g.add(&dir);

    let g = ctx.group(&p, "Watching", "");
    ctx.switch(&g, "Show crashes and freezes", "A red badge on the tile when an app crashes or stops responding. Needs a wireless-debugging (ADB) connection", cfg.testing.watch_crashes, |c, v| c.testing.watch_crashes = v);
    ctx.spin(&g, "Graph sample interval (ms)", "How often frame rate, CPU, memory and temperature are measured. ", (500, 60_000, 500), cfg.testing.perf_interval_ms as u32, |c, v| c.testing.perf_interval_ms = v as u64);

    let g = ctx.group(&p, "Automation", "Lets tools on this PC drive the phones: Claude through android-controller-mcp, or your own scripts. Only your user account can connect");
    ctx.switch(&g, "Allow automation", "Takes effect after a restart", cfg.automation.enabled, |c, v| c.automation.enabled = v);
    ctx.switch(&g, "Ask before deleting app data", "Clearing an app's data or uninstalling it waits for you to allow it here", cfg.automation.confirm_destructive, |c, v| c.automation.confirm_destructive = v);
    p
}

pub fn apply_color_scheme(scheme: &str) {
    adw::StyleManager::default().set_color_scheme(match scheme {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
}
