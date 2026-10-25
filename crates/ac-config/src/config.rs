//! `config.toml` (spec §7.3). Every field has a default, so a missing or
//! partial file is valid.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub general: General,
    pub discovery: Discovery,
    pub connection: Connection,
    pub video: Video,
    #[serde(deserialize_with = "merge_profiles")]
    pub profiles: BTreeMap<String, Profile>,
    pub input: Input,
    pub transfer: Transfer,
    pub audio: Audio,
    pub testing: Testing,
    pub automation: Automation,
    pub ui: Ui,
    #[serde(deserialize_with = "merge_keybindings")]
    pub keybindings: BTreeMap<String, String>,
}

/// User keybindings override individual defaults; unmentioned actions keep
/// their default accelerator (an empty string disables one).
fn merge_keybindings<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    let mut bindings = default_keybindings();
    bindings.extend(BTreeMap::<String, String>::deserialize(d)?);
    Ok(bindings)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            general: General::default(),
            discovery: Discovery::default(),
            connection: Connection::default(),
            video: Video::default(),
            profiles: default_profiles(),
            input: Input::default(),
            transfer: Transfer::default(),
            audio: Audio::default(),
            testing: Testing::default(),
            automation: Automation::default(),
            ui: Ui::default(),
            keybindings: default_keybindings(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub adb_server: SocketAddr,
    pub adb_binary: PathBuf,
    pub scrcpy_binary: PathBuf,
    pub scrcpy_server: PathBuf,
    pub start_adb_server: bool,
    pub lan_only: bool,
    pub max_slots: u8,
}

impl Default for General {
    fn default() -> Self {
        Self {
            adb_server: "127.0.0.1:5037".parse().unwrap(),
            adb_binary: "/usr/bin/adb".into(),
            scrcpy_binary: "/usr/bin/scrcpy".into(),
            scrcpy_server: "/usr/share/scrcpy/scrcpy-server".into(),
            start_adb_server: true,
            lan_only: true,
            max_slots: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Discovery {
    pub avahi: bool,
    pub builtin_mdns_fallback: bool,
    pub auto_connect_known: bool,
    pub legacy_port: u16,
    pub legacy_scan_subnet: bool,
    pub legacy_scan_concurrency: u32,
    pub legacy_scan_timeout_ms: u64,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            avahi: true,
            builtin_mdns_fallback: true,
            auto_connect_known: true,
            legacy_port: 5555,
            legacy_scan_subnet: false,
            legacy_scan_concurrency: 32,
            legacy_scan_timeout_ms: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    pub heartbeat_interval_ms: u64,
    pub heartbeat_timeout_ms: u64,
    pub heartbeat_failures_to_reconnect: u32,
    pub connect_timeout_ms: u64,
    /// The last value repeats.
    pub reconnect_backoff_ms: Vec<u64>,
    pub reconnect_jitter_pct: u8,
    pub reconnect_give_up_after_s: u64,
    pub phone_low_latency_wifi: bool,
    pub stay_awake: bool,
    pub show_touches: bool,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 2000,
            heartbeat_timeout_ms: 3000,
            heartbeat_failures_to_reconnect: 2,
            connect_timeout_ms: 5000,
            reconnect_backoff_ms: vec![500, 1000, 2000, 4000, 8000, 10000],
            reconnect_jitter_pct: 20,
            reconnect_give_up_after_s: 300,
            phone_low_latency_wifi: true,
            stay_awake: false,
            show_touches: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Video {
    pub decoder: String,
    pub vaapi_device: String,
    pub graphics_offload: String,
    pub adaptive_bitrate: bool,
    pub grid_profile: String,
    pub focus_profile: String,
    pub lag_downgrade_ms: u32,
    pub lag_upgrade_ms: u32,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            decoder: "auto".into(),
            vaapi_device: "auto".into(),
            graphics_offload: "auto".into(),
            adaptive_bitrate: true,
            grid_profile: "grid".into(),
            focus_profile: "focus".into(),
            lag_downgrade_ms: 250,
            lag_upgrade_ms: 80,
        }
    }
}

/// `max_size` is either `"auto"` or a pixel count (0 = native).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaxSize {
    Px(u32),
    Auto(AutoTag),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoTag {
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub codec: String,
    pub max_size: MaxSize,
    pub max_size_min: u32,
    pub max_size_max: u32,
    pub bitrate_kbps: u32,
    pub max_fps: u32,
    pub codec_options: String,
    pub audio: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            codec: "h264".into(),
            max_size: MaxSize::Px(1280),
            max_size_min: 640,
            max_size_max: 1280,
            bitrate_kbps: 3000,
            max_fps: 30,
            codec_options: "i-frame-interval:int=10".into(),
            audio: false,
        }
    }
}

fn default_profiles() -> BTreeMap<String, Profile> {
    let grid = Profile { max_size: MaxSize::Auto(AutoTag::Auto), ..Profile::default() };
    let focus = Profile {
        codec: "h265-if-supported".into(),
        max_size: MaxSize::Px(1920),
        bitrate_kbps: 12000,
        max_fps: 60,
        audio: true,
        ..Profile::default()
    };
    let low = Profile { max_size: MaxSize::Px(640), bitrate_kbps: 1000, max_fps: 15, ..Profile::default() };
    BTreeMap::from([("grid".into(), grid), ("focus".into(), focus), ("low".into(), low)])
}

/// User `[profiles.<name>]` tables are layered key-by-key over the built-in
/// profile of the same name, so `[profiles.grid] bitrate_kbps = 2000` keeps
/// every other grid default and the other built-in profiles.
fn merge_profiles<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Profile>, D::Error> {
    use serde::de::Error;
    let user = BTreeMap::<String, toml::Table>::deserialize(d)?;
    let mut profiles = default_profiles();
    for (name, table) in user {
        let base = profiles.get(&name).cloned().unwrap_or_default();
        let mut merged = toml::Table::try_from(&base).map_err(D::Error::custom)?;
        merged.extend(table);
        let profile = merged.try_into().map_err(|e| D::Error::custom(format!("profiles.{name}: {e}")))?;
        profiles.insert(name, profile);
    }
    Ok(profiles)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Input {
    pub shortcut_modifier: String,
    pub key_inject_mode: String,
    pub right_click: String,
    pub middle_click: String,
    pub button8: String,
    pub button9: String,
    pub forward_hover: bool,
    pub scroll_pixels_per_notch: f64,
    pub clipboard_autosync: bool,
    pub broadcast_mapping: String,
    pub broadcast_includes_power: bool,
    pub broadcast_skip_mismatched_orientation: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            shortcut_modifier: "alt".into(),
            key_inject_mode: "mixed".into(),
            right_click: "back".into(),
            middle_click: "home".into(),
            button8: "app_switch".into(),
            button9: "notifications".into(),
            forward_hover: false,
            scroll_pixels_per_notch: 20.0,
            clipboard_autosync: true,
            broadcast_mapping: "normalized".into(),
            broadcast_includes_power: false,
            broadcast_skip_mismatched_orientation: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transfer {
    pub default_target_dir: String,
    pub on_conflict: String,
    pub apk_drop_action: String,
    pub apk_alternate_modifier: String,
    pub segment_size_mib: u32,
    pub max_concurrent_per_device: u32,
    pub max_concurrent_global: u32,
    pub rate_limit_while_streaming_mbps: u32,
    pub rate_limit_idle_mbps: u32,
    pub verify_checksum: String,
    pub verify_checksum_threshold_mib: u32,
    pub trigger_media_scan: bool,
    pub recursive_directories: bool,
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            default_target_dir: "/sdcard/Download".into(),
            on_conflict: "rename".into(),
            apk_drop_action: "install".into(),
            apk_alternate_modifier: "shift".into(),
            segment_size_mib: 32,
            max_concurrent_per_device: 2,
            max_concurrent_global: 4,
            rate_limit_while_streaming_mbps: 40,
            rate_limit_idle_mbps: 0,
            verify_checksum: "large".into(),
            verify_checksum_threshold_mib: 64,
            trigger_media_scan: true,
            recursive_directories: true,
        }
    }
}

/// Focus-view audio (spec §4.4 step 5), played through PipeWire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// PipeWire buffer at 48 kHz: lower is snappier, higher survives Wi-Fi hiccups.
    pub pipewire_latency_frames: u32,
    /// 0.0..=1.0.
    pub volume: f64,
}

impl Default for Audio {
    fn default() -> Self {
        Self { pipewire_latency_frames: 2048, volume: 1.0 }
    }
}

/// App testing on every phone at once (Milestone 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Testing {
    /// Test runs, screenshots and recordings; empty = `~/Documents/Android Controller`.
    pub output_dir: String,
    /// Watch each phone's log for crashes and ANRs (a red badge on the tile).
    pub watch_crashes: bool,
    /// How often frame rate, CPU, memory and temperature are sampled.
    pub perf_interval_ms: u64,
}

impl Default for Testing {
    fn default() -> Self {
        Self { output_dir: String::new(), watch_crashes: true, perf_interval_ms: 2000 }
    }
}

/// The automation socket for Claude (`android-controller-mcp`) and scripts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Automation {
    /// Listen on `$XDG_RUNTIME_DIR/android-controller/control.sock` (this user only).
    pub enabled: bool,
    /// Ask in the window before an automated "clear data" or "uninstall".
    pub confirm_destructive: bool,
    /// TCP port the game add-on listens on, on the phone's localhost.
    pub game_port: u16,
}

impl Default for Automation {
    fn default() -> Self {
        Self { enabled: true, confirm_destructive: true, game_port: 27191 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub hide_empty_slots: bool,
    pub color_scheme: String,
    pub show_fps_overlay: bool,
    pub confirm_forget: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            hide_empty_slots: false,
            color_scheme: "system".into(),
            show_fps_overlay: true,
            confirm_forget: true,
        }
    }
}

pub fn default_keybindings() -> BTreeMap<String, String> {
    [
        ("back", "<Alt>b"),
        ("home", "<Alt>h"),
        ("app_switch", "<Alt>s"),
        ("volume_up", "<Alt>Up"),
        ("volume_down", "<Alt>Down"),
        ("power", "<Alt>p"),
        ("menu", "<Alt>m"),
        ("rotate", "<Alt>r"),
        ("notifications", "<Alt>n"),
        ("quick_settings", "<Alt><Shift>n"),
        ("collapse_panels", "<Alt><Ctrl>n"),
        ("screen_off_toggle", "<Alt>o"),
        ("paste_clipboard", "<Alt>v"),
        ("type_clipboard", "<Alt><Shift>v"),
        ("toggle_focus", "<Alt>f"),
        ("grid_view", "<Alt>g"),
        ("broadcast_toggle", "<Alt><Shift>b"),
        ("mirror_clicks_toggle", "<Alt><Shift>m"),
        ("screenshot_all", "<Alt><Shift>s"),
        ("record_all", "<Alt><Shift>r"),
        ("focus_slot_1", "<Alt>1"),
        ("focus_slot_2", "<Alt>2"),
        ("focus_slot_3", "<Alt>3"),
        ("focus_slot_4", "<Alt>4"),
        ("focus_slot_5", "<Alt>5"),
        ("send_files", "<Alt>u"),
        ("pair_device", "<Ctrl>n"),
        ("preferences", "<Ctrl>comma"),
        ("quit", "<Ctrl>q"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}
