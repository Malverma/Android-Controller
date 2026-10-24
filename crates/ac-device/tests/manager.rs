//! End-to-end manager tests against the in-process mock ADB server.

use std::collections::BTreeMap;
use std::time::Duration;

use ac_config::Config;
use ac_core::TransportKind;
use ac_device::probe::{IDENTIFY_CMD, STATS_CMD};
use ac_device::*;
use ac_discovery::{DiscoveredDevice, DiscoveryEvent, ServiceKind};
use adb_mock::{MockAdb, MockAdbHandle, MockDevice};

fn phone(serial: &str, model: &str) -> MockDevice {
    MockDevice::new("placeholder", model)
        .on_shell(
            IDENTIFY_CMD,
            &format!("@serial\n{serial}\n@model\n{model}\n@manufacturer\nGoogle\n@sdk\n35\n@aid\nffff\n@mac\n3: wlan0: <UP> mtu 1500\\    link/ether a6:3b:91:0c:4e:12 brd ff:ff:ff:ff:ff:ff\n"),
            0,
        )
        .on_shell(STATS_CMD, "@battery\n  level: 64\n  AC powered: true\n@wifi\n  RSSI: -50, Link speed: 866Mbps\n", 0)
}

fn start(mock: &MockAdbHandle) -> ManagerHandle {
    let mut cfg = Config::default();
    cfg.general.adb_server = mock.addr;
    cfg.general.start_adb_server = false;
    cfg.connection.heartbeat_interval_ms = 250;
    cfg.connection.heartbeat_timeout_ms = 500;
    cfg.connection.reconnect_backoff_ms = vec![50, 100, 200];
    cfg.connection.reconnect_jitter_pct = 0;
    let mut opts = ManagerOptions::new(cfg, None);
    opts.discovery = false;
    ac_device::spawn(opts)
}

/// Waits for the first event satisfying `pred`, failing after 5 s.
async fn wait_for<T>(h: &ManagerHandle, what: &str, mut pred: impl FnMut(&UiEvent) -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, h.events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("manager alive");
        if let Some(v) = pred(&ev) {
            return v;
        }
    }
}

async fn wait_phase(h: &ManagerHandle, slot: u8, phase: PhaseView) -> SlotView {
    wait_for(h, &format!("slot {slot} {phase:?}"), |e| match e {
        UiEvent::Slot(v) if v.slot == slot && v.phase == phase => Some(v.clone()),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn connect_identify_and_report_stats() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert("192.168.1.42:37115".into(), phone("SER1", "Pixel 8"));
    });
    let h = start(&mock);

    h.commands.send(UiCommand::Connect { slot: None, addr: "192.168.1.42:37115".into() }).await.unwrap();
    let v = wait_phase(&h, 1, PhaseView::Online).await;
    assert_eq!(v.title, "Pixel 8");
    assert_eq!(v.device_id.as_deref(), Some("SER1"));
    assert_eq!(v.transport, TransportKind::Tls);

    let stats = wait_for(&h, "stats", |e| match e {
        UiEvent::Stats { slot: 1, stats } if stats.battery_pct.is_some() => Some(stats.clone()),
        _ => None,
    })
    .await;
    assert_eq!(stats.battery_pct, Some(64));
    assert!(stats.charging);
    assert_eq!(stats.wifi_rssi_dbm, Some(-50));
    assert!(stats.rtt_ms.is_some());
    // phone_low_latency_wifi is on by default for SDK >= 29.
    assert!(mock.requests().iter().any(|r| r.contains("force-low-latency-mode enabled")));
}

#[tokio::test]
async fn wifi_drop_reconnects_automatically() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert("192.168.1.42:5555".into(), phone("SER1", "Pixel 8"));
    });
    let h = start(&mock);
    h.commands.send(UiCommand::Connect { slot: None, addr: "192.168.1.42".into() }).await.unwrap();
    let v = wait_phase(&h, 1, PhaseView::Online).await;
    assert_eq!(v.transport, TransportKind::Legacy, "bare IP means adb tcpip on 5555");

    mock.set_state("192.168.1.42:5555", "offline");
    wait_phase(&h, 1, PhaseView::Reconnecting).await;
    // The backoff timer disconnects and reconnects; the mock brings it back.
    let v = wait_phase(&h, 1, PhaseView::Online).await;
    assert_eq!(v.device_id.as_deref(), Some("SER1"));
}

#[tokio::test]
async fn rejects_non_lan_addresses() {
    let mock = MockAdb::start().await;
    let h = start(&mock);
    h.commands.send(UiCommand::Connect { slot: None, addr: "8.8.8.8:5555".into() }).await.unwrap();
    let msg = wait_for(&h, "toast", |e| match e {
        UiEvent::Toast { message, .. } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(msg.contains("not a local network address"), "{msg}");
    assert!(!mock.requests().iter().any(|r| r.starts_with("host:connect")));
}

#[tokio::test]
async fn pair_with_code_then_connect_via_mdns() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.pairable.insert("192.168.1.50:41000".into(), ("482913".into(), "adb-SER7-OpVSoG".into()));
        s.connectable.insert("192.168.1.50:37999".into(), phone("SER7", "Galaxy S23"));
    });
    let h = start(&mock);
    let svc = |kind, instance: &str, addr: &str| DiscoveredDevice {
        instance: instance.into(),
        kind,
        addr: addr.parse().unwrap(),
        host: "phone.local".into(),
        txt: BTreeMap::new(),
    };

    h.discovery_tx.send(DiscoveryEvent::Added(svc(ServiceKind::Pairing, "adb-SER7-OpVSoG", "192.168.1.50:41000"))).unwrap();
    let nearby = wait_for(&h, "nearby", |e| match e {
        UiEvent::Nearby(n) if !n.is_empty() => Some(n.clone()),
        _ => None,
    })
    .await;
    assert_eq!(nearby[0].serial.as_deref(), Some("SER7"));

    h.commands
        .send(UiCommand::PairCode { slot: Some(2), instance: "adb-SER7-OpVSoG".into(), code: "482 913".into() })
        .await
        .unwrap();
    wait_for(&h, "paired", |e| match e {
        UiEvent::Slot(v) if v.slot == 2 && v.detail.starts_with("Paired") => Some(()),
        _ => None,
    })
    .await;

    // The phone now advertises its connect port.
    h.discovery_tx.send(DiscoveryEvent::Added(svc(ServiceKind::Connect, "adb-SER7-OpVSoG", "192.168.1.50:37999"))).unwrap();
    let v = wait_phase(&h, 2, PhaseView::Online).await;
    assert_eq!(v.title, "Galaxy S23");
    assert!(mock.requests().contains(&"host:pair:482913:192.168.1.50:41000".to_owned()));
}

#[tokio::test]
async fn pair_by_typed_address_without_mdns() {
    // Wired PC, phone on Wi-Fi, router filters multicast: nothing is ever discovered.
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.pairable.insert("192.168.0.84:41000".into(), ("482913".into(), "adb-SER7-OpVSoG".into()));
        s.connectable.insert("192.168.0.84:37999".into(), phone("SER7", "Galaxy S23"));
    });
    let h = start(&mock);
    h.commands
        .send(UiCommand::PairAddr {
            slot: None,
            pair_addr: "192.168.0.84:41000".into(),
            code: "482913".into(),
            connect: Some("37999".into()),
        })
        .await
        .unwrap();
    let v = wait_phase(&h, 1, PhaseView::Online).await;
    assert_eq!(v.title, "Galaxy S23");
    assert_eq!(v.device_id.as_deref(), Some("SER7"));
    let reqs = mock.requests();
    assert!(reqs.contains(&"host:pair:482913:192.168.0.84:41000".to_owned()));
    assert!(reqs.contains(&"host:connect:192.168.0.84:37999".to_owned()), "{reqs:?}");
}

#[tokio::test]
async fn wrong_pairing_code_reports_failure() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.pairable.insert("192.168.1.50:41000".into(), ("482913".into(), "adb-SER7-OpVSoG".into()));
    });
    let h = start(&mock);
    h.discovery_tx
        .send(DiscoveryEvent::Added(DiscoveredDevice {
            instance: "adb-SER7-OpVSoG".into(),
            kind: ServiceKind::Pairing,
            addr: "192.168.1.50:41000".parse().unwrap(),
            host: String::new(),
            txt: BTreeMap::new(),
        }))
        .unwrap();
    // Let the manager register the service before pairing against it.
    wait_for(&h, "nearby", |e| matches!(e, UiEvent::Nearby(n) if !n.is_empty()).then_some(())).await;
    h.commands.send(UiCommand::PairCode { slot: None, instance: "adb-SER7-OpVSoG".into(), code: "000000".into() }).await.unwrap();
    // Never identified, so the slot shows as empty again — with the error.
    let v = wait_for(&h, "pairing error", |e| match e {
        UiEvent::Slot(v) if v.slot == 1 && v.error.is_some() => Some(v.clone()),
        _ => None,
    })
    .await;
    assert_eq!(v.phase, PhaseView::Empty);
    assert!(v.error.unwrap().contains("Wrong password"));
}

#[tokio::test]
async fn adopts_existing_transports_and_respects_user_disconnect() {
    let mock = MockAdb::start().await;
    let mut dev = phone("SER3", "OnePlus 12");
    dev.serial = "192.168.1.60:40123".into();
    mock.add_device(dev.clone());
    mock.with_state(|s| {
        s.connectable.insert("192.168.1.60:40123".into(), dev);
    });
    let h = start(&mock);

    // Already connected in adb (e.g. adb's own mDNS auto-connect): adopted.
    let v = wait_phase(&h, 1, PhaseView::Online).await;
    assert_eq!(v.title, "OnePlus 12");

    h.commands.send(UiCommand::Disconnect { slot: 1 }).await.unwrap();
    wait_phase(&h, 1, PhaseView::Disconnected).await;

    // adb reconnects it behind our back: the user's disconnect wins.
    mock.with_state(|s| {
        let d = s.connectable["192.168.1.60:40123"].clone();
        s.devices.push(MockDevice { serial: "192.168.1.60:40123".into(), ..d });
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(ev) = h.events.try_recv() {
        if let UiEvent::Slot(v) = ev {
            assert_ne!(v.phase, PhaseView::Online, "suppressed device was re-adopted into slot {}", v.slot);
        }
    }

    // An explicit reconnect clears the suppression.
    h.commands.send(UiCommand::Reconnect { slot: 1 }).await.unwrap();
    wait_phase(&h, 1, PhaseView::Online).await;
}

#[tokio::test]
async fn sixth_device_gets_no_slot() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        for i in 1..=6 {
            s.connectable.insert(format!("192.168.1.{i}:5555"), phone(&format!("SER{i}"), &format!("Phone {i}")));
        }
    });
    let h = start(&mock);
    for i in 1..=5u8 {
        h.commands.send(UiCommand::Connect { slot: None, addr: format!("192.168.1.{i}") }).await.unwrap();
        wait_phase(&h, i, PhaseView::Online).await;
    }
    h.commands.send(UiCommand::Connect { slot: None, addr: "192.168.1.6".into() }).await.unwrap();
    let msg = wait_for(&h, "toast", |e| match e {
        UiEvent::Toast { message, .. } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert_eq!(msg, "All 5 slots are in use");
}
