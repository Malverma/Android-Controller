//! Semantic checks that serde cannot express (spec §7.3 "Validation rules").
//!
//! Accelerator syntax is checked by the UI with `gtk::accelerator_parse`;
//! this crate only checks for duplicates so it stays GTK-free.

use std::collections::HashMap;

use crate::config::{Config, MaxSize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Dotted TOML key path, e.g. `profiles.grid.bitrate_kbps`.
    pub key: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

pub fn validate(cfg: &Config) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let mut err = |key: String, message: String| errs.push(ValidationError { key, message });

    if !(1..=ac_core::MAX_SLOTS as u8).contains(&cfg.general.max_slots) {
        err("general.max_slots".into(), format!("must be 1..={}", ac_core::MAX_SLOTS));
    }

    let c = &cfg.connection;
    if c.reconnect_backoff_ms.is_empty() {
        err("connection.reconnect_backoff_ms".into(), "must not be empty".into());
    }
    if c.reconnect_jitter_pct > 100 {
        err("connection.reconnect_jitter_pct".into(), "must be 0..=100".into());
    }
    if c.heartbeat_interval_ms < 250 {
        err("connection.heartbeat_interval_ms".into(), "must be at least 250".into());
    }
    if c.heartbeat_failures_to_reconnect == 0 {
        err("connection.heartbeat_failures_to_reconnect".into(), "must be at least 1".into());
    }

    if !(1..=128).contains(&cfg.discovery.legacy_scan_concurrency) {
        err("discovery.legacy_scan_concurrency".into(), "must be 1..=128".into());
    }

    for (name, p) in &cfg.profiles {
        let key = |field: &str| format!("profiles.{name}.{field}");
        if !(250..=50_000).contains(&p.bitrate_kbps) {
            err(key("bitrate_kbps"), "must be 250..=50000".into());
        }
        if !(1..=120).contains(&p.max_fps) {
            err(key("max_fps"), "must be 1..=120".into());
        }
        if let MaxSize::Px(px) = p.max_size
            && px != 0
            && (!(320..=4096).contains(&px) || px % 8 != 0)
        {
            err(key("max_size"), "must be 0, \"auto\", or a multiple of 8 in 320..=4096".into());
        }
        if p.max_size_min > p.max_size_max {
            err(key("max_size_min"), "must not exceed max_size_max".into());
        }
        let codecs = ["h264", "h265", "av1", "h265-if-supported", "av1-if-supported"];
        if !codecs.contains(&p.codec.as_str()) {
            err(key("codec"), format!("must be one of {codecs:?}"));
        }
    }
    for (key, name) in [("video.grid_profile", &cfg.video.grid_profile), ("video.focus_profile", &cfg.video.focus_profile)] {
        if !cfg.profiles.contains_key(name) {
            err(key.into(), format!("no [profiles.{name}] table"));
        }
    }

    if let Some(msg) = check_target_dir(&cfg.transfer.default_target_dir) {
        err("transfer.default_target_dir".into(), msg.into());
    }
    if !(4..=256).contains(&cfg.transfer.segment_size_mib) {
        err("transfer.segment_size_mib".into(), "must be 4..=256".into());
    }

    if !(0.0..=1.0).contains(&cfg.audio.volume) {
        err("audio.volume".into(), "must be 0.0..=1.0".into());
    }
    if !(64..=16384).contains(&cfg.audio.pipewire_latency_frames) {
        err("audio.pipewire_latency_frames".into(), "must be 64..=16384".into());
    }

    if !(500..=60_000).contains(&cfg.testing.perf_interval_ms) {
        err("testing.perf_interval_ms".into(), "must be 500..=60000".into());
    }

    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (action, accel) in &cfg.keybindings {
        if accel.is_empty() {
            continue;
        }
        if let Some(other) = seen.insert(accel.as_str(), action.as_str()) {
            err(format!("keybindings.{action}"), format!("{accel} is already bound to {other}"));
        }
    }

    errs
}

/// Returns an error message when `dir` is not an acceptable push target.
pub fn check_target_dir(dir: &str) -> Option<&'static str> {
    if !(dir.starts_with("/sdcard/") || dir == "/sdcard" || dir.starts_with("/storage/")) {
        return Some("must be under /sdcard/ or /storage/");
    }
    if dir.contains('\'') || dir.contains('\0') {
        return Some("must not contain quotes or NUL");
    }
    if dir.split('/').any(|seg| seg == "..") {
        return Some("must not contain '..' segments");
    }
    None
}
