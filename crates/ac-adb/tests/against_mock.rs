use std::time::Duration;

use ac_adb::{AdbClient, AdbError, ConnectOutcome, DeviceState, PairOutcome};
use adb_mock::{MockAdb, MockDevice};

fn client(mock: &MockAdb) -> AdbClient {
    AdbClient::new(mock.addr).with_timeout(Duration::from_secs(2))
}

#[tokio::test]
async fn version_and_device_listing() {
    let mock = MockAdb::start().await;
    mock.add_device(MockDevice::new("192.168.1.42:5555", "Pixel 8"));
    let adb = client(&mock);

    assert_eq!(adb.version().await.unwrap(), 41);
    let devices = adb.devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].model.as_deref(), Some("Pixel 8"));
    assert!(devices[0].is_network());
}

#[tokio::test]
async fn connect_and_already_connected() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert("10.0.0.7:5555".into(), MockDevice::new("x", "Galaxy S23"));
    });
    let adb = client(&mock);

    assert_eq!(adb.connect_device("10.0.0.7:5555").await.unwrap(), ConnectOutcome::Connected);
    assert_eq!(adb.connect_device("10.0.0.7:5555").await.unwrap(), ConnectOutcome::AlreadyConnected);
    assert!(matches!(adb.connect_device("10.0.0.8:5555").await.unwrap(), ConnectOutcome::Failed(_)));
}

#[tokio::test]
async fn pairing_reports_guid_or_failure() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.pairable.insert("10.0.0.7:41000".into(), ("123456".into(), "adb-SER1-abcdef".into()));
    });
    let adb = client(&mock);

    assert_eq!(
        adb.pair("123456", "10.0.0.7:41000").await.unwrap(),
        PairOutcome::Paired { guid: Some("adb-SER1-abcdef".into()) }
    );
    assert!(matches!(adb.pair("000000", "10.0.0.7:41000").await.unwrap(), PairOutcome::Failed(_)));
}

#[tokio::test]
async fn shell_v2_getprop_features_and_errors() {
    let mock = MockAdb::start().await;
    mock.add_device(
        MockDevice::new("10.0.0.7:5555", "Pixel").prop("ro.serialno", "SER1").on_shell("false", "", 1),
    );
    let adb = client(&mock);

    assert_eq!(adb.getprop("10.0.0.7:5555", "ro.serialno", true).await.unwrap(), "SER1");
    let out = adb.shell("10.0.0.7:5555", "false", true).await.unwrap();
    assert_eq!(out.exit_code, Some(1));
    let legacy = adb.shell("10.0.0.7:5555", "echo hi", false).await.unwrap();
    assert_eq!(legacy.stdout, "hi\n");
    assert!(adb.features("10.0.0.7:5555").await.unwrap().contains(&"shell_v2".to_owned()));

    match adb.shell("nope:1", "echo", true).await {
        Err(AdbError::Fail(msg)) => assert!(msg.contains("not found")),
        other => panic!("expected FAIL, got {other:?}"),
    }
}

#[tokio::test]
async fn forward_allocates_port() {
    let mock = MockAdb::start().await;
    mock.add_device(MockDevice::new("10.0.0.7:5555", "Pixel"));
    let adb = client(&mock);
    let port = adb.forward("10.0.0.7:5555", "tcp:0", "localabstract:scrcpy_0000abcd").await.unwrap();
    assert!(port.is_some_and(|p| p > 40000));
    adb.kill_forward("10.0.0.7:5555", "tcp:40001").await.unwrap();
}

#[tokio::test]
async fn track_devices_streams_changes() {
    let mock = MockAdb::start().await;
    mock.add_device(MockDevice::new("10.0.0.7:5555", "Pixel"));
    let adb = client(&mock);
    let mut tracker = adb.track_devices().await.unwrap();

    let first = tracker.next().await.unwrap();
    assert_eq!(first[0].state, DeviceState::Device);

    mock.set_state("10.0.0.7:5555", "offline");
    let second = tokio::time::timeout(Duration::from_secs(2), tracker.next()).await.unwrap().unwrap();
    assert_eq!(second[0].state, DeviceState::Offline);
}

#[tokio::test]
async fn sync_send_and_stat_round_trip() {
    let mock = MockAdb::start().await;
    mock.add_device(MockDevice::new("10.0.0.7:5555", "Pixel"));
    let adb = client(&mock);

    // 200 KiB: forces several full 64 KiB DATA packets plus a short tail.
    let payload: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
    let mut progress = Vec::new();
    let mut sync = adb.sync("10.0.0.7:5555").await.unwrap();
    let sent = sync
        .send("/sdcard/Download/a,b.bin", 0o100644, 1_700_000_000, payload.as_slice(), |n| progress.push(n))
        .await
        .unwrap();
    assert_eq!(sent, payload.len() as u64);
    assert_eq!(progress, vec![65536, 131072, 196608, 204800]);

    let stat = sync.stat("/sdcard/Download/a,b.bin").await.unwrap();
    assert!(stat.exists());
    assert_eq!(stat.size as usize, payload.len());
    assert!(!sync.stat("/sdcard/missing").await.unwrap().exists());
    sync.quit().await.unwrap();

    assert_eq!(mock.file("10.0.0.7:5555", "/sdcard/Download/a,b.bin").unwrap(), payload);
}

#[tokio::test]
async fn unreachable_server_is_detected() {
    // Bind then drop to get a port that is almost certainly closed.
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let err = AdbClient::new(addr).version().await.unwrap_err();
    assert!(err.is_server_unreachable(), "{err:?}");
}
