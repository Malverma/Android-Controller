//! End-to-end check of the no-ADB path against a real phone, without the UI:
//! `direct_e2e <ip> <code>` pairs (keys kept in memory), mirrors for a few
//! seconds, taps the middle of the screen and reports what happened.
use std::time::{Duration, Instant};

use ac_device::{ManagerOptions, PhaseView, StreamView, UiCommand, UiEvent};
use ac_scrcpy::{ControlMsg, MotionAction, Position};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = ac_config::Config::default();
    // Keep this run away from the real ADB server and its phones.
    cfg.general.adb_server = "127.0.0.1:1".parse().unwrap();
    cfg.general.start_adb_server = false;
    let mut opts = ManagerOptions::new(cfg, None);
    opts.discovery = false;
    let h = ac_device::spawn(opts);
    h.commands.send(UiCommand::PairDirect { slot: Some(1), addr: args[1].clone(), code: args[2].clone() }).await.unwrap();
    let start = Instant::now();
    let mut live_at = None;
    let mut tapped = false;
    while start.elapsed() < Duration::from_secs(60) {
        let ev = match tokio::time::timeout(Duration::from_millis(500), h.events.recv()).await {
            Ok(Ok(ev)) => ev,
            _ => {
                if let (Some(t), false) = (live_at, tapped)
                    && Instant::now().duration_since(t) > Duration::from_secs(4)
                {
                    let (w, hh) = h.io[0].mailbox.size().unwrap();
                    let p = Position { x: w as i32 / 2, y: hh as i32 / 2, screen_w: w as u16, screen_h: hh as u16 };
                    for action in [MotionAction::Down, MotionAction::Up] {
                        h.io[0].send(ControlMsg::InjectTouch { action, pointer_id: u64::MAX - 1, position: p, pressure: 1.0, action_button: 0, buttons: 0 });
                    }
                    println!("tapped {}x{} at centre; frames so far {:?}", w, hh, h.io[0].mailbox.stats.decoded.load(std::sync::atomic::Ordering::Relaxed));
                    tapped = true;
                    if let Some(f) = args.get(3) {
                        h.commands.send(UiCommand::SendFiles { slots: vec![1], paths: vec![f.into()], install_apks: true }).await.unwrap();
                    }
                    h.commands.send(UiCommand::SetStayAwake { slot: 1, on: true }).await.unwrap();
                }
                if tapped && live_at.is_some_and(|t| t.elapsed() > Duration::from_secs(args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8))) {
                    break;
                }
                continue;
            }
        };
        match ev {
            UiEvent::Slot(v) if v.slot == 1 => {
                println!("slot 1: {:?} {:?} {} {:?} stay_awake={:?}", v.phase, v.stream, v.detail, v.error, v.stay_awake);
                if v.phase == PhaseView::Online && matches!(v.stream, StreamView::Live { .. }) && live_at.is_none() {
                    live_at = Some(Instant::now());
                }
            }
            UiEvent::Toast { message, .. } => println!("toast: {message}"),
            UiEvent::Transfer(t) if !t.active || t.sent == t.total => println!("transfer: {t:?}"),
            UiEvent::Stats { slot: 1, stats } if stats.battery_pct.is_some() && start.elapsed().as_secs() % 5 == 0 => {
                println!("stats: {stats:?}");
            }
            _ => {}
        }
    }
    println!("decoder {:?}", h.io[0].backend());
    h.abort();
}
