//! Live check of the app-testing pieces on a phone over ADB:
//! `cargo run -p ac-device --example testing_probe -- <serial> [crash-package]`
//! Samples performance, takes a screenshot, launches and stops Settings,
//! and (with a package) crashes it with `am crash` to see the crash parser.

use std::time::Duration;

use ac_adb::AdbClient;
use ac_device::perf::{LAYERS_CMD, PerfState, parse_layers, parse_sample, sample_cmd};
use ac_device::testing::{app_cmd, app_result};
use ac_device::{AppAction, logcat};
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let serial = args.next().expect("serial");
    let crash = args.next();
    let adb = AdbClient::new("127.0.0.1:5037".parse().unwrap()).with_timeout(Duration::from_secs(10));
    let features = adb.features(&serial).await.unwrap();
    let v2 = features.iter().any(|f| f == "shell_v2");
    let sdk: u32 = adb.getprop(&serial, "ro.build.version.sdk", v2).await.unwrap().trim().parse().unwrap();
    println!("sdk {sdk}, shell_v2 {v2}");

    let launch = AppAction::Launch("com.android.settings".into());
    let out = adb.shell(&serial, &app_cmd(&launch), v2).await.unwrap();
    println!("launch: {:?}", app_result(&launch, &out.stdout));
    tokio::time::sleep(Duration::from_secs(2)).await;

    let out = adb.shell(&serial, LAYERS_CMD, v2).await.unwrap();
    let (top, layers) = parse_layers(&out.stdout);
    println!("top {top:?} layers {layers:?}");
    let mut st = PerfState::default();
    for _ in 0..3 {
        let out = adb.shell(&serial, &sample_cmd(&layers, top.as_deref()), v2).await.unwrap();
        println!("{:?}", parse_sample(&out.stdout, &mut st, top.as_deref()));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let mut s = adb.open_service(&serial, "exec:screencap -p").await.unwrap();
    let mut png = Vec::new();
    s.read_to_end(&mut png).await.unwrap();
    println!("screenshot {} bytes, png={}", png.len(), png.starts_with(b"\x89PNG"));

    if let Some(pkg) = crash {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(logcat::run(adb.clone(), serial.clone(), v2, sdk, 1, None, tx));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let out = adb.shell(&serial, &format!("am crash {pkg} 2>&1"), v2).await.unwrap();
        println!("am crash: {}", out.stdout.trim());
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(logcat::LogEvent::Crash(_, c))) => println!("CRASH {:?} {} {}\n{}", c.kind, c.package, c.summary, c.detail.lines().take(4).collect::<Vec<_>>().join("\n")),
            other => println!("no crash seen: {}", other.is_ok()),
        }
        task.abort();
    }
    let stop = AppAction::Stop("com.android.settings".into());
    let out = adb.shell(&serial, &app_cmd(&stop), v2).await.unwrap();
    println!("stop: {:?}", app_result(&stop, &out.stdout));
}
