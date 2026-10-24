use std::path::Path;

use ac_config::*;
use ac_core::TransportKind;
use chrono::{TimeZone, Utc};

fn p() -> &'static Path {
    Path::new("config.toml")
}

#[test]
fn empty_file_gives_defaults_and_defaults_validate() {
    let cfg = parse_config("", p()).unwrap();
    assert_eq!(cfg, Config::default());
    assert!(validate(&cfg).is_empty());
    assert_eq!(cfg.profiles["grid"].max_size, MaxSize::Auto(AutoTag::Auto));
    assert_eq!(cfg.profiles["focus"].bitrate_kbps, 12000);
}

#[test]
fn defaults_round_trip_through_toml() {
    let text = toml::to_string_pretty(&Config::default()).unwrap();
    assert_eq!(parse_config(&text, p()).unwrap(), Config::default());
}

#[test]
fn partial_file_keeps_other_defaults() {
    let cfg = parse_config(
        r#"
[connection]
heartbeat_interval_ms = 1000
[profiles.grid]
bitrate_kbps = 2000
max_size = 960
"#,
        p(),
    )
    .unwrap();
    assert_eq!(cfg.connection.heartbeat_interval_ms, 1000);
    assert_eq!(cfg.connection.heartbeat_timeout_ms, 3000);
    assert_eq!(cfg.profiles["grid"].max_size, MaxSize::Px(960));
    assert_eq!(cfg.profiles["grid"].bitrate_kbps, 2000);
    assert_eq!(cfg.profiles["grid"].max_fps, 30);
    // Other built-in profiles survive a partial [profiles.*] table.
    assert_eq!(cfg.profiles["focus"], Config::default().profiles["focus"]);
}

#[test]
fn partial_profile_merges_over_its_own_builtin() {
    let cfg = parse_config("[profiles.focus]\nbitrate_kbps = 8000", p()).unwrap();
    let focus = &cfg.profiles["focus"];
    assert_eq!(focus.bitrate_kbps, 8000);
    assert_eq!(focus.max_fps, 60); // focus default, not Profile::default()'s 30
    assert!(focus.audio);
}

#[test]
fn user_defined_profile_starts_from_profile_defaults() {
    let cfg = parse_config("[profiles.tv]\nmax_size = 1080\nmax_fps = 24", p()).unwrap();
    assert_eq!(cfg.profiles["tv"].max_fps, 24);
    assert_eq!(cfg.profiles["tv"].bitrate_kbps, 3000);
    assert_eq!(cfg.profiles.len(), 4);
}

fn invalid_keys(text: &str) -> Vec<String> {
    match parse_config(text, p()) {
        Err(ConfigError::Invalid { errors, .. }) => errors.into_iter().map(|e| e.key).collect(),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn each_validation_rule_rejects() {
    assert_eq!(invalid_keys("[general]\nmax_slots = 6"), ["general.max_slots"]);
    assert_eq!(invalid_keys("[connection]\nreconnect_backoff_ms = []"), ["connection.reconnect_backoff_ms"]);
    assert_eq!(invalid_keys("[connection]\nreconnect_jitter_pct = 101"), ["connection.reconnect_jitter_pct"]);
    assert_eq!(invalid_keys("[connection]\nheartbeat_interval_ms = 10"), ["connection.heartbeat_interval_ms"]);
    assert_eq!(
        invalid_keys("[connection]\nheartbeat_failures_to_reconnect = 0"),
        ["connection.heartbeat_failures_to_reconnect"]
    );
    assert_eq!(invalid_keys("[discovery]\nlegacy_scan_concurrency = 0"), ["discovery.legacy_scan_concurrency"]);
    assert_eq!(invalid_keys("[profiles.low]\nbitrate_kbps = 100"), ["profiles.low.bitrate_kbps"]);
    assert_eq!(invalid_keys("[profiles.low]\nmax_fps = 0"), ["profiles.low.max_fps"]);
    assert_eq!(invalid_keys("[profiles.low]\nmax_size = 1001"), ["profiles.low.max_size"]);
    assert_eq!(invalid_keys("[profiles.low]\nmax_size = 200"), ["profiles.low.max_size"]);
    assert_eq!(invalid_keys("[profiles.low]\nmax_size_min = 2000"), ["profiles.low.max_size_min"]);
    assert_eq!(invalid_keys("[profiles.low]\ncodec = \"mpeg2\""), ["profiles.low.codec"]);
    assert_eq!(invalid_keys("[video]\nfocus_profile = \"nope\""), ["video.focus_profile"]);
    assert_eq!(invalid_keys("[transfer]\ndefault_target_dir = \"/data/local\""), ["transfer.default_target_dir"]);
    assert_eq!(
        invalid_keys("[transfer]\ndefault_target_dir = \"/sdcard/../data\""),
        ["transfer.default_target_dir"]
    );
    assert_eq!(invalid_keys("[transfer]\ndefault_target_dir = \"/sdcard/it's\""), ["transfer.default_target_dir"]);
    assert_eq!(invalid_keys("[transfer]\nsegment_size_mib = 1"), ["transfer.segment_size_mib"]);
    let dup = invalid_keys("[keybindings]\nback = \"<Alt>h\"");
    assert_eq!(dup.len(), 1);
    assert!(dup[0] == "keybindings.back" || dup[0] == "keybindings.home");
}

#[test]
fn max_size_zero_means_native_and_is_valid() {
    let cfg = parse_config("[profiles.focus]\nmax_size = 0", p()).unwrap();
    assert_eq!(cfg.profiles["focus"].max_size, MaxSize::Px(0));
}

#[test]
fn rejects_newer_schema() {
    assert!(matches!(parse_config("schema_version = 2", p()), Err(ConfigError::TooNew { found: 2, .. })));
}

#[test]
fn devices_round_trip_and_normalize() {
    let now = Utc.with_ymd_and_hms(2026, 10, 23, 9, 0, 0).unwrap();
    let mut a = DeviceRecord::new("SER1", TransportKind::Tls, now);
    a.slot = 1;
    a.mdns_instance = Some("adb-SER1-OpVSoG".into());
    a.profile_overrides.insert("grid".into(), toml::toml! { bitrate_kbps = 2000 });
    let mut b = DeviceRecord::new("SER2", TransportKind::Legacy, now);
    b.slot = 1; // duplicate slot → reset to 0 on load
    b.wifi_mac = Some("a6:3b:91:0c:4e:12".into());
    let devices = Devices { schema_version: 1, devices: vec![a, b] };

    let dir = std::env::temp_dir().join(format!("ac-config-test-{}", std::process::id()));
    let path = dir.join("devices.toml");
    save_devices(&path, &devices).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[[device]]"), "{text}");

    let loaded = load_devices(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(loaded.devices.len(), 2);
    assert_eq!(loaded.get("SER1").unwrap().slot, 1);
    assert_eq!(loaded.get("SER2").unwrap().slot, 0);
    assert_eq!(loaded.by_slot(1).unwrap().id, "SER1");
    assert!(loaded.by_slot(0).is_none());

    let base = &Config::default().profiles["grid"];
    let merged = loaded.get("SER1").unwrap().profile("grid", base).unwrap();
    assert_eq!(merged.bitrate_kbps, 2000);
    assert_eq!(merged.max_fps, base.max_fps);
}

#[test]
fn missing_files_give_defaults() {
    let dir = Path::new("/nonexistent/android-controller");
    assert_eq!(load_config(&dir.join("config.toml")).unwrap(), Config::default());
    assert!(load_devices(&dir.join("devices.toml")).unwrap().devices.is_empty());
}
