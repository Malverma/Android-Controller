//! Monitor mode end to end, against the mock ADB server, a stand-in for the
//! phone's helper, and a host that records what it would do to the desktop.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use ac_config::Config;
use ac_device::probe::IDENTIFY_CMD;
use ac_device::*;
use ac_monitor::protocol;
use ac_monitor::session::{SessionEvent, SessionParams};
use ac_monitor::touch::{TouchAction, TouchEvent};
use adb_mock::{MockAdb, MockAdbHandle, MockDevice};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const ADDR: &str = "192.168.1.42:37115";
const LEGACY_ADDR: &str = "192.168.1.42:5555";

#[derive(Debug, Default)]
struct FakeHost {
    log: Mutex<Vec<String>>,
    touches: Mutex<Option<std_mpsc::Receiver<TouchEvent>>>,
    unavailable: Option<String>,
}

impl FakeHost {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
}

struct Stopped(Arc<FakeHost>);

impl Drop for Stopped {
    fn drop(&mut self) {
        self.0.note("session stopped".into());
    }
}

/// `DisplayHost` needs `'static`, and sessions report back to the host.
#[derive(Debug)]
struct Host(Arc<FakeHost>);

impl DisplayHost for Host {
    fn unavailable(&self) -> Option<String> {
        self.0.unavailable.clone()
    }

    fn create(&self, _: &str, rule: &str) -> Result<(), String> {
        self.0.note(format!("create {rule}"));
        Ok(())
    }

    fn remove(&self, output: &str) -> Result<(), String> {
        self.0.note(format!("remove {output}"));
        Ok(())
    }

    fn remove_leftovers(&self) {
        self.0.note("remove leftovers".into());
    }

    fn pointer(&self, output: &str) -> Result<std_mpsc::Sender<TouchEvent>, String> {
        self.0.note(format!("pointer {output}"));
        let (tx, rx) = std_mpsc::channel();
        *self.0.touches.lock().unwrap() = Some(rx);
        Ok(tx)
    }

    fn session(&self, params: SessionParams, _video: std::net::TcpStream, on_event: EventSink) -> (SessionGuard, Arc<AtomicBool>) {
        self.0.note(format!("session {} {}kbit", params.output, params.bitrate_kbps));
        on_event(SessionEvent::Live { width: 1920, height: 864, encoder: ac_monitor::encode::Kind::Vaapi });
        on_event(SessionEvent::Stats { fps: 60, kbps: 7900, target_kbps: 8000 });
        (Box::new((Stopped(self.0.clone()), on_event)), Arc::new(AtomicBool::new(false)))
    }
}

/// Stands in for `Monitor.java` on the far side of the forward.
struct Helper {
    /// What the PC sent on any connection, including each role.
    from_pc: mpsc::UnboundedReceiver<Value>,
    to_pc: mpsc::UnboundedSender<Value>,
}

async fn read_json(s: &mut TcpStream) -> std::io::Result<Value> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let mut body = vec![0u8; protocol::body_len(len)?];
    s.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Listens where the mock's `adb forward` says it forwards to.
async fn fake_helper(mock: &MockAdbHandle, avc: bool) -> Helper {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    mock.with_state(|s| s.forward_port = Some(port));
    let (seen, from_pc) = mpsc::unbounded_channel();
    let (to_pc, mut outgoing) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut control: Option<TcpStream> = None;
        loop {
            tokio::select! {
                conn = listener.accept() => {
                    let (mut s, _) = conn.unwrap();
                    let first = read_json(&mut s).await.unwrap();
                    let control_role = first["role"] == "control";
                    let _ = seen.send(first);
                    if control_role {
                        let hello = json!({"op":"hello","version":8,"width":1080,"height":2400,"density":420,"avc":avc,"requested":false});
                        s.write_all(&protocol::encode_json(&hello)).await.unwrap();
                        control = Some(s);
                    }
                }
                msg = outgoing.recv() => {
                    let msg = msg.unwrap();
                    if let Some(c) = control.as_mut() {
                        c.write_all(&protocol::encode_json(&msg)).await.unwrap();
                    }
                }
                read = async {
                    match control.as_mut() {
                        Some(c) => read_json(c).await,
                        None => std::future::pending().await,
                    }
                } => match read {
                    Ok(v) => {
                        if v["op"] != "ping" {
                            let _ = seen.send(v);
                        }
                    }
                    Err(_) => control = None,
                },
            }
        }
    });
    Helper { from_pc, to_pc }
}

impl Helper {
    async fn expect(&mut self, want: impl Fn(&Value) -> bool, what: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let v = tokio::time::timeout_at(deadline, self.from_pc.recv())
                .await
                .unwrap_or_else(|_| panic!("the phone was not sent {what}"))
                .unwrap();
            if want(&v) {
                return v;
            }
        }
    }
}

fn phone(serial: &str, helper_version: u32) -> MockDevice {
    MockDevice::new("placeholder", "Pixel 8")
        .on_shell(IDENTIFY_CMD, &format!("@serial\n{serial}\n@model\nPixel 8\n@manufacturer\nGoogle\n@sdk\n35\n@aid\nffff\n"), 0)
        .on_shell(HELPER_SETUP_CMD, &format!("@versionCode={helper_version}\n@ready\n"), 0)
        .on_shell("am start -n org.androidcontroller.keeper/.MonitorActivity", "Starting: Intent\n", 0)
}

fn start(mock: &MockAdbHandle, host: Option<Arc<FakeHost>>, grace: Duration) -> ManagerHandle {
    let mut cfg = Config::default();
    cfg.general.adb_server = mock.addr;
    cfg.general.start_adb_server = false;
    cfg.connection.heartbeat_interval_ms = 250;
    cfg.connection.heartbeat_timeout_ms = 500;
    cfg.connection.reconnect_backoff_ms = vec![50, 100, 200];
    cfg.connection.reconnect_jitter_pct = 0;
    let mut opts = ManagerOptions::new(cfg, None);
    opts.discovery = false;
    opts.streaming = false;
    opts.display_host = host.map(|h| Arc::new(Host(h)) as Arc<dyn DisplayHost>);
    opts.display_grace = grace;
    ac_device::spawn(opts)
}

async fn wait_view(h: &ManagerHandle, what: &str, mut pred: impl FnMut(&SlotView) -> bool) -> SlotView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, h.events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap();
        if let UiEvent::Slot(v) = ev
            && v.slot == 1
            && pred(&v)
        {
            return v;
        }
    }
}

async fn wait_log(host: &FakeHost, want: &str) {
    for _ in 0..250 {
        if host.log().iter().any(|l| l == want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never logged {want:?}: {:?}", host.log());
}

async fn wait_gone(host: &FakeHost) {
    wait_log(host, "session stopped").await;
}

/// A phone online and ready to become a monitor.
async fn ready(addr: &str, helper_version: u32, host: &Arc<FakeHost>) -> (MockAdbHandle, ManagerHandle, Helper) {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert(addr.into(), phone("SER1", helper_version));
    });
    let helper = fake_helper(&mock, true).await;
    let h = start(&mock, Some(host.clone()), Duration::from_secs(60));
    h.commands.send(UiCommand::Connect { slot: None, addr: addr.into() }).await.unwrap();
    wait_view(&h, "online", |v| v.phase == PhaseView::Online).await;
    (mock, h, helper)
}

fn is_live(v: &SlotView) -> bool {
    matches!(v.display, DisplayView::Live { .. })
}

#[tokio::test]
async fn toggle_starts_a_monitor_and_touch_reaches_the_pointer_then_stops_cleanly() {
    let host = Arc::new(FakeHost::default());
    let (mock, h, mut phone) = ready(ADDR, 8, &host).await;
    wait_view(&h, "ready", |v| v.display_blocked.is_none()).await;

    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    phone.expect(|v| v["role"] == "control", "the control role").await;
    let start_msg = phone.expect(|v| v["op"] == "start", "start").await;
    assert_eq!(start_msg["view_only"], false, "wireless debugging is encrypted");
    assert_eq!(start_msg["portrait"], false);
    let video = phone.expect(|v| v["role"] == "video", "the video role").await;
    assert_eq!((video["w"].as_u64(), video["h"].as_u64()), (Some(1920), Some(864)), "landscape, capped at 1920");

    let v = wait_view(&h, "live", |v| is_live(v) && matches!(v.display, DisplayView::Live { fps: 60, .. })).await;
    assert_eq!(
        v.display,
        DisplayView::Live { width: 1920, height: 864, fps: 60, kbps: 7900, encoder: "VA-API".into() }
    );
    assert_eq!(v.stream, StreamView::Off);
    assert!(mock.requests().iter().any(|r| r.contains("am start -n org.androidcontroller.keeper/.MonitorActivity")));

    let log = host.log();
    assert!(log.contains(&"remove leftovers".to_owned()));
    assert!(log.contains(&"create TBM-1,1920x864@60,auto-right,1.5".to_owned()), "{log:?}");
    assert!(log.contains(&"pointer TBM-1".to_owned()));
    assert!(log.contains(&"session TBM-1 8000kbit".to_owned()));

    // A touch goes from the phone to the pointer.
    phone.to_pc.send(json!({"op":"touch","a":"down","t":5,"p":[[0,0.5,0.5]]})).unwrap();
    let got = tokio::task::spawn_blocking({
        let host = host.clone();
        move || {
            for _ in 0..250 {
                if let Some(rx) = host.touches.lock().unwrap().as_ref()
                    && let Ok(t) = rx.recv_timeout(Duration::from_millis(20))
                {
                    return Some(t);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            None
        }
    })
    .await
    .unwrap()
    .expect("the touch arrives");
    assert_eq!(got.action, TouchAction::Down);

    h.commands.send(UiCommand::SetDisplay { slot: 1, on: false }).await.unwrap();
    phone.expect(|v| v["op"] == "stop", "stop").await;
    wait_view(&h, "off", |v| v.display == DisplayView::Off).await;
    wait_gone(&host).await;
    wait_log(&host, "remove TBM-1").await;
    // The pointer is gone: the phone's touches no longer go anywhere.
    assert!(host.touches.lock().unwrap().as_ref().unwrap().recv_timeout(Duration::from_millis(300)).is_err());
}

#[tokio::test]
async fn the_phones_own_button_starts_and_stops_it() {
    let host = Arc::new(FakeHost::default());
    let (_mock, h, mut phone) = ready(ADDR, 8, &host).await;
    wait_view(&h, "ready", |v| v.display_blocked.is_none()).await;
    phone.expect(|v| v["role"] == "control", "the control role").await;

    phone.to_pc.send(json!({"op":"monitor_request","on":true})).unwrap();
    wait_view(&h, "live", is_live).await;

    phone.to_pc.send(json!({"op":"monitor_request","on":false})).unwrap();
    wait_view(&h, "off", |v| v.display == DisplayView::Off).await;
    wait_log(&host, "remove TBM-1").await;
}

#[tokio::test]
async fn a_dropped_phone_keeps_the_output_and_resumes() {
    let host = Arc::new(FakeHost::default());
    let (mock, h, _phone) = ready(ADDR, 8, &host).await;
    wait_view(&h, "ready", |v| v.display_blocked.is_none()).await;
    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    wait_view(&h, "live", is_live).await;

    mock.set_state(ADDR, "offline");
    wait_view(&h, "waiting", |v| v.display == DisplayView::Waiting).await;
    wait_gone(&host).await;
    assert!(!host.log().iter().any(|l| l.starts_with("remove TBM")), "windows stay put: {:?}", host.log());

    // The mock brings it back.
    wait_view(&h, "live again", is_live).await;
    let creates = host.log().iter().filter(|l| l.starts_with("create")).count();
    assert_eq!(creates, 2, "the same rule again, which keeps the same output");
    assert!(!host.log().iter().any(|l| l.starts_with("remove TBM")));
}

#[tokio::test]
async fn the_output_goes_after_the_grace_time_but_the_wish_stays() {
    let host = Arc::new(FakeHost::default());
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert(ADDR.into(), phone("SER1", 8));
    });
    let _phone = fake_helper(&mock, true).await;
    let h = start(&mock, Some(host.clone()), Duration::from_millis(150));
    h.commands.send(UiCommand::Connect { slot: None, addr: ADDR.into() }).await.unwrap();
    wait_view(&h, "ready", |v| v.phase == PhaseView::Online && v.display_blocked.is_none()).await;
    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    wait_view(&h, "live", is_live).await;

    mock.with_state(|s| s.connectable.clear());
    mock.remove_device(ADDR);
    wait_view(&h, "waiting", |v| v.display == DisplayView::Waiting).await;
    wait_log(&host, "remove TBM-1").await;
    let v = wait_view(&h, "still reconnecting", |v| v.phase == PhaseView::Reconnecting).await;
    assert_eq!(v.display, DisplayView::Waiting, "it resumes if the phone returns");
}

#[tokio::test]
async fn an_unencrypted_connection_starts_view_only_until_touch_is_allowed() {
    let host = Arc::new(FakeHost::default());
    let (_mock, h, mut phone) = ready(LEGACY_ADDR, 8, &host).await;
    let v = wait_view(&h, "ready", |v| v.display_blocked.is_none()).await;
    assert!(v.display_view_only);

    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    let start_msg = phone.expect(|v| v["op"] == "start", "start").await;
    assert_eq!(start_msg["view_only"], true);
    let v = wait_view(&h, "live", is_live).await;
    assert!(v.warning.as_deref().is_some_and(|w| w.contains("not encrypted")), "{:?}", v.warning);
    assert!(!host.log().iter().any(|l| l.starts_with("pointer")), "no way to touch the PC");

    // Saying yes applies at once, and the output is not touched.
    h.commands.send(UiCommand::SetDisplayOptions { slot: 1, view_only: Some(false), portrait: None }).await.unwrap();
    let start_msg = phone.expect(|v| v["op"] == "start", "a new start").await;
    assert_eq!(start_msg["view_only"], false);
    wait_log(&host, "pointer TBM-1").await;
    assert!(!host.log().iter().any(|l| l.starts_with("remove TBM")));
}

#[tokio::test]
async fn portrait_comes_from_the_phones_options() {
    let host = Arc::new(FakeHost::default());
    let (_mock, h, mut phone) = ready(ADDR, 8, &host).await;
    wait_view(&h, "ready", |v| v.display_blocked.is_none()).await;
    h.commands.send(UiCommand::SetDisplayOptions { slot: 1, view_only: None, portrait: Some(true) }).await.unwrap();
    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    let video = phone.expect(|v| v["role"] == "video", "the video role").await;
    assert_eq!((video["w"].as_u64(), video["h"].as_u64()), (Some(864), Some(1920)));
}

#[tokio::test]
async fn an_old_helper_blocks_it_with_a_reason() {
    let host = Arc::new(FakeHost::default());
    let (_mock, h, _phone) = ready(ADDR, 7, &host).await;
    let v = wait_view(&h, "the reason", |v| v.display_blocked.as_deref().is_some_and(|r| r.contains("0.7"))).await;
    assert!(v.display_blocked.unwrap().contains("0.8"));

    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    let toast = async {
        loop {
            if let Ok(UiEvent::Toast { message, .. }) = h.events.recv().await {
                return message;
            }
        }
    };
    let message = tokio::time::timeout(Duration::from_secs(5), toast).await.expect("a reason is shown");
    assert!(message.contains("0.8"), "{message}");
    assert!(!host.log().iter().any(|l| l.starts_with("create")));
}

#[tokio::test]
async fn off_hyprland_says_so() {
    let host = Arc::new(FakeHost { unavailable: Some("needs a Hyprland session".into()), ..Default::default() });
    let (_mock, h, _phone) = ready(ADDR, 8, &host).await;
    let v = wait_view(&h, "online view", |v| v.phase == PhaseView::Online).await;
    assert_eq!(v.display_blocked.as_deref(), Some("needs a Hyprland session"));
}

#[tokio::test]
async fn without_a_host_nothing_can_touch_the_desktop() {
    let mock = MockAdb::start().await;
    mock.with_state(|s| {
        s.connectable.insert(ADDR.into(), phone("SER1", 8));
    });
    let h = start(&mock, None, Duration::from_secs(1));
    h.commands.send(UiCommand::Connect { slot: None, addr: ADDR.into() }).await.unwrap();
    let v = wait_view(&h, "online", |v| v.phase == PhaseView::Online).await;
    assert!(v.display_blocked.is_some());
    h.commands.send(UiCommand::SetDisplay { slot: 1, on: true }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!mock.requests().iter().any(|r| r.contains("MonitorActivity") || r.contains("forward")));
}
