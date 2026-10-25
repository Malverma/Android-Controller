//! End-to-end check of app testing through the device manager, without the
//! UI: `testing_e2e <output-dir>` adopts the phones the ADB server knows,
//! starts a test run, samples performance, takes screenshots, records each
//! phone for a few seconds and stops the run. Nothing is installed or changed.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ac_device::{ManagerOptions, PhaseView, StreamView, UiCommand, UiEvent};

#[tokio::main]
async fn main() {
    let out = std::env::args().nth(1).expect("output dir");
    let mut cfg = ac_config::Config::default();
    cfg.testing.output_dir = out;
    cfg.testing.perf_interval_ms = 1000;
    let mut opts = ManagerOptions::new(cfg, None);
    opts.discovery = false;
    let h = ac_device::spawn(opts);
    let start = Instant::now();
    let mut live: BTreeMap<u8, String> = BTreeMap::new();
    let mut step = 0;
    let mut at = Instant::now();
    let mut perf = 0;
    while start.elapsed() < Duration::from_secs(90) {
        let slots: Vec<u8> = live.keys().copied().collect();
        if step == 0 && live.len() >= 2 && at.elapsed() > Duration::from_secs(3) || step == 0 && !live.is_empty() && start.elapsed() > Duration::from_secs(30) {
            println!("== start run on {slots:?}");
            h.commands.send(UiCommand::StartRun { name: "e2e check".into() }).await.unwrap();
            h.commands.send(UiCommand::Record { slots: slots.clone(), on: true }).await.unwrap();
            (step, at) = (1, Instant::now());
        } else if step == 1 && at.elapsed() > Duration::from_secs(3) {
            h.commands.send(UiCommand::Screenshot { slots: slots.clone() }).await.unwrap();
            h.commands.send(UiCommand::ListPackages { slot: slots[0] }).await.unwrap();
            (step, at) = (2, Instant::now());
        } else if step == 2 && at.elapsed() > Duration::from_secs(4) {
            h.commands.send(UiCommand::Record { slots: slots.clone(), on: false }).await.unwrap();
            (step, at) = (3, Instant::now());
        } else if step == 3 && at.elapsed() > Duration::from_secs(3) {
            h.commands.send(UiCommand::StopRun).await.unwrap();
            (step, at) = (4, Instant::now());
        } else if step == 4 && at.elapsed() > Duration::from_secs(2) {
            break;
        }
        let ev = match tokio::time::timeout(Duration::from_millis(200), h.events.recv()).await {
            Ok(Ok(ev)) => ev,
            _ => continue,
        };
        match ev {
            UiEvent::Slot(v) => {
                if v.phase == PhaseView::Online && matches!(v.stream, StreamView::Live { .. }) {
                    if live.insert(v.slot, v.title.clone()).is_none() {
                        println!("slot {} live: {} {:?}", v.slot, v.title, v.helper_update);
                        at = Instant::now();
                    }
                } else if v.phase == PhaseView::Online {
                    println!("slot {}: {} {:?} {:?}", v.slot, v.title, v.stream, v.helper_update);
                }
            }
            UiEvent::Perf { slot, sample } => {
                perf += 1;
                if perf <= 4 {
                    println!("perf {slot}: {sample:?}");
                }
            }
            UiEvent::Packages { slot, packages } => println!("packages {slot}: {} apps", packages.len()),
            UiEvent::Toast { message, .. } => println!("toast: {message}"),
            UiEvent::Run(r) => println!("run: {r:?}"),
            UiEvent::Recording { slot, file } => println!("recording {slot}: {file:?}"),
            UiEvent::Saved { message, folder } => println!("saved: {message} in {}", folder.display()),
            UiEvent::SaveFrame { slot, path, .. } => println!("save frame {slot}: {}", path.display()),
            UiEvent::Crash { slot, crash } => println!("crash {slot}: {crash:?}"),
            _ => {}
        }
    }
    println!("perf samples: {perf}");
    h.abort();
}
