//! Slot transitions, named after the rows of spec §3.3.2.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ac_adb::DeviceState;
use ac_config::Connection;
use ac_core::{ErrorCode, TransportKind};
use ac_device::qr::QrSecret;
use ac_device::slot::*;

fn cfg() -> Connection {
    Connection::default()
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn info(id: &str) -> DeviceInfo {
    DeviceInfo {
        id: id.into(),
        model: "Pixel 8".into(),
        manufacturer: "Google".into(),
        sdk: 35,
        features: vec!["shell_v2".into()],
        wifi_mac: Some("a6:3b:91:0c:4e:12".into()),
    }
}

/// Feeds the event and returns the effects.
fn ev(slot: &mut Slot, e: SlotEvent) -> Vec<Effect> {
    slot.handle(e, Instant::now(), &cfg())
}

fn current_timer(fx: &[Effect]) -> (TimerKind, Duration, u64) {
    fx.iter()
        .find_map(|f| match f {
            Effect::Timer { kind, after, generation } => Some((*kind, *after, *generation)),
            _ => None,
        })
        .expect("a timer effect")
}

fn fire(slot: &mut Slot, fx: &[Effect]) -> Vec<Effect> {
    let (kind, _, generation) = current_timer(fx);
    ev(slot, SlotEvent::Timer { kind, generation })
}

fn connect(slot: &mut Slot, addr: &str) -> Vec<Effect> {
    ev(slot, SlotEvent::Connect { addr: addr.into(), transport: TransportKind::Tls, expect_serial: None, silent: false })
}

/// Drives a fresh slot all the way to Online at `addr`.
fn online(addr: &str, id: &str) -> Slot {
    let mut s = Slot::new(1);
    connect(&mut s, addr);
    ev(&mut s, SlotEvent::ConnectDone(Ok(())));
    ev(&mut s, SlotEvent::AdbState { serial: addr.into(), state: DeviceState::Device });
    ev(&mut s, SlotEvent::Identified(info(id)));
    assert!(matches!(s.phase, Phase::Online { .. }));
    s
}

fn has_toast(fx: &[Effect], code: ErrorCode) -> bool {
    fx.iter().any(|f| matches!(f, Effect::Toast { code: c, .. } if *c == code))
}

#[test]
fn t03_disconnected_user_connect() {
    let mut s = Slot::new(1);
    let fx = connect(&mut s, "192.168.1.42:37115");
    assert_eq!(fx, vec![Effect::Connect { addr: "192.168.1.42:37115".into() }]);
    assert!(matches!(s.phase, Phase::ConnectingAdb { attempt: 0, connected: false, .. }));
}

#[test]
fn t04_t06_pair_code_then_connect_service_seen() {
    let mut s = Slot::new(2);
    let fx = ev(&mut s, SlotEvent::PairCode { addr: sa("192.168.1.42:41000"), code: "123456".into(), connect: None });
    assert_eq!(fx[0], Effect::Pair { code: "123456".into(), addr: sa("192.168.1.42:41000") });
    assert_eq!(current_timer(&fx).1, PAIR_CODE_TIMEOUT);

    let fx = ev(&mut s, SlotEvent::PairDone(Ok(Some("adb-SER1-OpVSoG".into()))));
    assert_eq!(fx[0], Effect::FindConnectService { serial: Some("SER1".into()), ip: Some("192.168.1.42".parse().unwrap()) });
    assert_eq!(s.expect_serial.as_deref(), Some("SER1"));

    let fx = ev(&mut s, SlotEvent::ConnectSeen { addr: sa("192.168.1.42:37115") });
    assert_eq!(fx, vec![Effect::Connect { addr: "192.168.1.42:37115".into() }]);
    assert!(matches!(s.phase, Phase::ConnectingAdb { .. }));
}

#[test]
fn t04b_manual_pair_connects_to_typed_address() {
    // Wired PC whose router drops mDNS between Wi-Fi and Ethernet.
    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::PairCode { addr: sa("192.168.0.84:41000"), code: "123456".into(), connect: Some("192.168.0.84:37115".into()) });
    let fx = ev(&mut s, SlotEvent::PairDone(Ok(Some("adb-SER1-OpVSoG".into()))));
    assert_eq!(fx, vec![Effect::Connect { addr: "192.168.0.84:37115".into() }]);
    assert!(matches!(s.phase, Phase::ConnectingAdb { attempt: 0, connected: false, silent: false, .. }));
    assert_eq!(s.expect_serial.as_deref(), Some("SER1"));
}

#[test]
fn t05_qr_session_pairs_when_phone_advertises() {
    let mut s = Slot::new(1);
    let secret = QrSecret { name: "studio-abc".into(), password: "pw123".into() };
    let fx = ev(&mut s, SlotEvent::PairQr { secret });
    assert_eq!(current_timer(&fx).1, PAIR_QR_TIMEOUT);
    let fx = ev(&mut s, SlotEvent::QrServiceSeen { addr: sa("10.0.0.9:40001") });
    assert_eq!(fx, vec![Effect::Pair { code: "pw123".into(), addr: sa("10.0.0.9:40001") }]);
    // A second announcement while pairing must not start a second pair call.
    assert!(ev(&mut s, SlotEvent::QrServiceSeen { addr: sa("10.0.0.9:40001") }).is_empty());
}

#[test]
fn t06b_adb_auto_connects_after_pairing() {
    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::PairCode { addr: sa("10.0.0.9:41000"), code: "111111".into(), connect: None });
    ev(&mut s, SlotEvent::PairDone(Ok(Some("adb-SER9-xYz123".into()))));
    let serial = "adb-SER9-xYz123._adb-tls-connect._tcp";
    assert!(s.wants_adb_serial(serial));
    let fx = ev(&mut s, SlotEvent::AdbState { serial: serial.into(), state: DeviceState::Device });
    assert_eq!(fx[0], Effect::Identify { serial: serial.into() });
    let fx = ev(&mut s, SlotEvent::Identified(info("SER9")));
    assert_eq!(fx, vec![Effect::StartMonitor { serial: serial.into(), shell_v2: true }, Effect::Persist]);
}

#[test]
fn t07_pair_failure_and_timeouts() {
    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::PairCode { addr: sa("10.0.0.9:41000"), code: "000000".into(), connect: None });
    let fx = ev(&mut s, SlotEvent::PairDone(Err("Wrong password".into())));
    assert!(has_toast(&fx, ErrorCode::PairFailed));
    assert!(matches!(&s.phase, Phase::Disconnected { error: Some(e) } if e.contains("Wrong password")));

    let mut s = Slot::new(1);
    let fx = ev(&mut s, SlotEvent::PairQr { secret: QrSecret { name: "studio-x".into(), password: "p".into() } });
    let fx = fire(&mut s, &fx);
    assert!(has_toast(&fx, ErrorCode::PairTimeout));

    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::PairCode { addr: sa("10.0.0.9:41000"), code: "111111".into(), connect: None });
    let fx = ev(&mut s, SlotEvent::PairDone(Ok(None)));
    assert_eq!(current_timer(&fx).0, TimerKind::AwaitConnectService);
    let fx = fire(&mut s, &fx);
    assert!(has_toast(&fx, ErrorCode::ConnectFailed));
}

#[test]
fn t08_pair_deadline_is_stale_after_pairing_succeeds() {
    let mut s = Slot::new(1);
    let first = ev(&mut s, SlotEvent::PairCode { addr: sa("10.0.0.9:41000"), code: "111111".into(), connect: None });
    ev(&mut s, SlotEvent::PairDone(Ok(None)));
    // The original 30 s pairing deadline fires late: it must be ignored.
    assert!(fire(&mut s, &first).is_empty());
    assert!(matches!(s.phase, Phase::Pairing { paired: true, .. }));
}

#[test]
fn t09_connect_ok_then_device_online_then_identified() {
    let mut s = Slot::new(1);
    connect(&mut s, "10.0.0.7:5555");
    let fx = ev(&mut s, SlotEvent::ConnectDone(Ok(())));
    assert_eq!(current_timer(&fx).0, TimerKind::AwaitOnline);
    // Tracker repeats the same state; only one Identify goes out.
    let a = ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    let b = ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    assert_eq!(a, vec![Effect::Identify { serial: "10.0.0.7:5555".into() }]);
    assert!(b.is_empty());
    ev(&mut s, SlotEvent::Identified(info("SER1")));
    assert!(matches!(s.phase, Phase::Online { heartbeat_failures: 0 }));
    assert_eq!(s.device_id(), Some("SER1"));
    assert_eq!(s.wifi_mac.as_deref(), Some("a6:3b:91:0c:4e:12"));
}

#[test]
fn t10_unauthorized_warns_once_and_extends_wait() {
    let mut s = Slot::new(1);
    connect(&mut s, "10.0.0.7:5555");
    ev(&mut s, SlotEvent::ConnectDone(Ok(())));
    let fx = ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Unauthorized });
    assert!(has_toast(&fx, ErrorCode::Unauthorized));
    assert_eq!(current_timer(&fx).1, AWAIT_AUTHORIZATION);
    assert!(ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Unauthorized }).is_empty());
}

#[test]
fn t11_t12_connect_retries_with_backoff_then_gives_up() {
    let mut s = Slot::new(1);
    connect(&mut s, "10.0.0.7:5555");
    let backoff = &cfg().reconnect_backoff_ms;
    for attempt in 1..MAX_CONNECT_ATTEMPTS {
        let fx = ev(&mut s, SlotEvent::ConnectDone(Err("Connection refused".into())));
        let (kind, after, _) = current_timer(&fx);
        assert_eq!(kind, TimerKind::Backoff);
        assert_eq!(after, Duration::from_millis(backoff[(attempt - 1) as usize]));
        let retry = fire(&mut s, &fx);
        assert_eq!(retry, vec![Effect::Connect { addr: "10.0.0.7:5555".into() }]);
    }
    let fx = ev(&mut s, SlotEvent::ConnectDone(Err("Connection refused".into())));
    assert!(has_toast(&fx, ErrorCode::ConnectFailed));
    assert!(matches!(s.phase, Phase::Disconnected { .. }));
}

#[test]
fn t12b_silent_connect_failure_is_not_toasted() {
    let mut s = Slot::restored(1, "SER1", TransportKind::Tls, Some("10.0.0.7:37000".into()), None);
    ev(&mut s, SlotEvent::Connect { addr: "10.0.0.7:37000".into(), transport: TransportKind::Tls, expect_serial: Some("SER1".into()), silent: true });
    let mut fx = Vec::new();
    for _ in 0..MAX_CONNECT_ATTEMPTS {
        fx = ev(&mut s, SlotEvent::ConnectDone(Err("refused".into())));
        if matches!(s.phase, Phase::ConnectingAdb { .. }) {
            fire(&mut s, &fx);
        }
    }
    assert!(!has_toast(&fx, ErrorCode::ConnectFailed));
    assert!(matches!(s.phase, Phase::Disconnected { .. }));
}

#[test]
fn t12c_connect_seen_with_new_port_replaces_stale_address() {
    let mut s = Slot::restored(1, "SER1", TransportKind::Tls, Some("10.0.0.7:37000".into()), None);
    ev(&mut s, SlotEvent::Connect { addr: "10.0.0.7:37000".into(), transport: TransportKind::Tls, expect_serial: Some("SER1".into()), silent: true });
    let fx = ev(&mut s, SlotEvent::ConnectSeen { addr: sa("10.0.0.7:41999") });
    assert_eq!(fx, vec![Effect::Connect { addr: "10.0.0.7:41999".into() }]);
    assert_eq!(s.addr.as_deref(), Some("10.0.0.7:41999"));
}

#[test]
fn t17_heartbeat_failures_trigger_reconnecting() {
    let mut s = online("10.0.0.7:5555", "SER1");
    assert!(ev(&mut s, SlotEvent::Heartbeat(Err("timeout".into()))).is_empty());
    assert!(ev(&mut s, SlotEvent::Heartbeat(Ok(12))).is_empty()); // resets the count
    ev(&mut s, SlotEvent::Heartbeat(Err("timeout".into())));
    let fx = ev(&mut s, SlotEvent::Heartbeat(Err("timeout".into())));
    assert_eq!(fx[0], Effect::StopMonitor);
    assert!(matches!(s.phase, Phase::Reconnecting { cause: ReconnectCause::Heartbeat, .. }));
}

#[test]
fn t17b_offline_or_gone_trigger_reconnecting() {
    let mut s = online("10.0.0.7:5555", "SER1");
    ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Offline });
    assert!(matches!(s.phase, Phase::Reconnecting { cause: ReconnectCause::Offline, .. }));

    let mut s = online("10.0.0.7:5555", "SER1");
    ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:5555".into() });
    assert!(matches!(s.phase, Phase::Reconnecting { cause: ReconnectCause::Gone, .. }));
}

#[test]
fn t18_transport_returns_while_reconnecting() {
    let mut s = online("10.0.0.7:5555", "SER1");
    ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:5555".into() });
    let fx = ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    assert_eq!(fx, vec![Effect::Identify { serial: "10.0.0.7:5555".into() }]);
    ev(&mut s, SlotEvent::Identified(info("SER1")));
    assert!(matches!(s.phase, Phase::Online { .. }));
}

#[test]
fn t18b_different_phone_at_old_address_is_rejected() {
    let mut s = online("10.0.0.7:5555", "SER1");
    ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:5555".into() });
    ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    let fx = ev(&mut s, SlotEvent::Identified(info("OTHER")));
    assert_eq!(fx, vec![Effect::Disconnect { serial: "10.0.0.7:5555".into() }]);
    assert!(matches!(s.phase, Phase::Reconnecting { .. }));
    assert_eq!(s.device_id(), Some("SER1"));
}

#[test]
fn t19_tls_address_change_via_mdns() {
    let mut s = online("10.0.0.7:37115", "SER1");
    ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:37115".into() });
    let fx = ev(&mut s, SlotEvent::ConnectSeen { addr: sa("10.0.0.23:40411") });
    assert_eq!(
        fx,
        vec![Effect::Disconnect { serial: "10.0.0.7:37115".into() }, Effect::Connect { addr: "10.0.0.23:40411".into() }]
    );
    assert_eq!(s.addr.as_deref(), Some("10.0.0.23:40411"));
    assert!(s.wants_adb_serial("10.0.0.23:40411"));
}

#[test]
fn t19b_tls_backoff_asks_mdns_again() {
    // A phone whose Wi-Fi slept may not re-announce its new port by itself.
    let mut s = online("10.0.0.7:37115", "SER1");
    let fx = ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:37115".into() });
    let fx = fire(&mut s, &fx);
    assert_eq!(fx[0], Effect::Reconnect { addr: "10.0.0.7:37115".into() });
    assert!(fx.contains(&Effect::RescanConnect));
}

#[test]
fn t20_backoff_reconnects_and_legacy_looks_up_mac() {
    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::Connect { addr: "10.0.0.7:5555".into(), transport: TransportKind::Legacy, expect_serial: None, silent: false });
    ev(&mut s, SlotEvent::ConnectDone(Ok(())));
    ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    ev(&mut s, SlotEvent::Identified(info("SER1")));
    let fx = ev(&mut s, SlotEvent::AdbGone { serial: "10.0.0.7:5555".into() });

    let fx = fire(&mut s, &fx); // attempt 1
    assert_eq!(fx[0], Effect::Reconnect { addr: "10.0.0.7:5555".into() });
    assert!(!fx.iter().any(|f| matches!(f, Effect::LookupLegacy { .. })));
    assert!(!fx.contains(&Effect::RescanConnect), "legacy devices are not on mDNS");
    let fx = fire(&mut s, &fx); // attempt 2: ARP lookup starts
    assert!(fx.contains(&Effect::LookupLegacy { mac: "a6:3b:91:0c:4e:12".into(), port: 5555 }));

    let fx = ev(&mut s, SlotEvent::LegacyAddrFound { addr: "10.0.0.31:5555".into() });
    assert_eq!(fx, vec![Effect::Reconnect { addr: "10.0.0.31:5555".into() }]);
    // Same address again: nothing to do.
    assert!(ev(&mut s, SlotEvent::LegacyAddrFound { addr: "10.0.0.31:5555".into() }).is_empty());
}

#[test]
fn t21_reconnect_gives_up_after_window() {
    let mut s = online("10.0.0.7:5555", "SER1");
    let start = Instant::now();
    let fx = s.handle(SlotEvent::AdbGone { serial: "10.0.0.7:5555".into() }, start, &cfg());
    let (kind, _, generation) = current_timer(&fx);
    let later = start + Duration::from_secs(cfg().reconnect_give_up_after_s);
    let fx = s.handle(SlotEvent::Timer { kind, generation }, later, &cfg());
    assert!(has_toast(&fx, ErrorCode::ReconnectGaveUp));
    assert!(matches!(s.phase, Phase::Disconnected { .. }));
    assert_eq!(s.device_id(), Some("SER1"), "identity survives giving up");
}

#[test]
fn t02_remembered_device_reappears_on_mdns() {
    let mut s = Slot::restored(3, "SER1", TransportKind::Tls, Some("10.0.0.7:37000".into()), None);
    assert!(!s.is_free());
    let fx = ev(&mut s, SlotEvent::ConnectSeen { addr: sa("10.0.0.7:39999") });
    assert_eq!(fx, vec![Effect::Connect { addr: "10.0.0.7:39999".into() }]);
}

#[test]
fn adopt_existing_transport() {
    let mut s = Slot::new(4);
    let serial = "adb-SER5-aaaaaa._adb-tls-connect._tcp";
    let fx = ev(&mut s, SlotEvent::Adopt { adb_serial: serial.into(), transport: TransportKind::Tls });
    assert_eq!(fx[0], Effect::Identify { serial: serial.into() });
    assert_eq!(s.addr, None, "mDNS-named serials are not IP addresses");
}

#[test]
fn user_disconnect_and_forget() {
    let mut s = online("10.0.0.7:5555", "SER1");
    let fx = ev(&mut s, SlotEvent::Disconnect);
    assert_eq!(fx, vec![Effect::StopMonitor, Effect::Disconnect { serial: "10.0.0.7:5555".into() }]);
    assert!(matches!(s.phase, Phase::Disconnected { error: None }));
    assert_eq!(s.device_id(), Some("SER1"));

    let fx = ev(&mut s, SlotEvent::Forget);
    assert!(fx.contains(&Effect::Forget { id: "SER1".into() }));
    assert!(matches!(s.phase, Phase::Empty));
    assert!(s.is_free());
}

#[test]
fn cancelling_a_pairing_frees_the_slot() {
    let mut s = Slot::new(1);
    ev(&mut s, SlotEvent::PairQr { secret: QrSecret { name: "studio-x".into(), password: "p".into() } });
    assert!(!s.is_free());
    assert_eq!(ev(&mut s, SlotEvent::Disconnect), vec![Effect::StopMonitor]);
    assert!(matches!(s.phase, Phase::Empty));
}

#[test]
fn stale_timer_generations_are_ignored() {
    let mut s = Slot::new(1);
    connect(&mut s, "10.0.0.7:5555");
    let fx = ev(&mut s, SlotEvent::ConnectDone(Ok(())));
    let (kind, _, generation) = current_timer(&fx);
    ev(&mut s, SlotEvent::AdbState { serial: "10.0.0.7:5555".into(), state: DeviceState::Device });
    ev(&mut s, SlotEvent::Identified(info("SER1")));
    assert!(ev(&mut s, SlotEvent::Timer { kind, generation }).is_empty());
    assert!(matches!(s.phase, Phase::Online { .. }));
}
