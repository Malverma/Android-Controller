//! Tests monitor mode on this PC without a phone: creates a temporary output,
//! captures and encodes it to a file, checks where the virtual pointer lands,
//! measures how fast frames can be captured and encoded, then removes the output.
//!
//! This takes over the real pointer for a moment and adds a monitor, so run it
//! when nothing else is in use. It refuses to run over a fullscreen app, which
//! would be disturbed and whose pointer lock would also make the result wrong.
//!
//!     cargo run --release -p ac-monitor --example monitor_probe -- [out.h264] [seconds] [encoder]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ac_monitor::capture::{Capturer, SlotState};
use ac_monitor::encode::{self, Encoder, Params};
use ac_monitor::hypr;
use ac_monitor::pointer::VirtualPointer;
use ac_monitor::session::{self, SessionEvent, SessionParams};
use ac_monitor::touch::PointerAction;

const NAME: &str = "TBM-probe";

fn cursor() -> (i64, i64) {
    let out = std::process::Command::new("hyprctl").args(["-j", "cursorpos"]).output().expect("hyprctl");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("cursor json");
    (v["x"].as_i64().unwrap_or(0), v["y"].as_i64().unwrap_or(0))
}

fn fullscreen_app() -> Option<String> {
    let out = std::process::Command::new("hyprctl").args(["-j", "activewindow"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    (v["fullscreen"].as_i64().unwrap_or(0) != 0).then(|| v["class"].as_str().unwrap_or("?").to_owned())
}

/// Copies every frame, changed or not: the most capture and encoding can do.
fn throughput(encoder: &str) -> anyhow::Result<()> {
    encode::init();
    let mut cap = Capturer::connect()?;
    cap.continuous = true;
    let mut enc: Option<Encoder> = None;
    let (mut copy, mut code) = (Duration::ZERO, Duration::ZERO);
    let started = Instant::now();
    let mut frames = 0u32;
    while frames < 180 && started.elapsed() < Duration::from_secs(10) {
        let t = Instant::now();
        cap.request(0, NAME, true)?;
        while cap.slot_state(0) != SlotState::Ready {
            anyhow::ensure!(cap.slot_state(0) != SlotState::Failed, "capture failed");
            anyhow::ensure!(t.elapsed() < Duration::from_secs(3), "capture timed out");
            cap.poll(Duration::from_millis(5))?;
        }
        copy += t.elapsed();
        let frame = cap.ready(0).expect("ready");
        let info = frame.info;
        let e = match &mut enc {
            Some(e) => e,
            None => enc.insert(Encoder::new(
                Params { width: info.width, height: info.height, src_fmt: info.pixel(), fps: 60, bitrate_kbps: 8000, render_node: "/dev/dri/renderD128".into() },
                encoder,
            )?),
        };
        let t = Instant::now();
        e.encode(frame.data, info.stride as usize, frame.y_invert, i64::from(frames) * 16_666, false)?;
        code += t.elapsed();
        cap.mark_idle(0);
        frames += 1;
    }
    let n = frames.max(1);
    println!(
        "throughput with {}: {:.0} frames/s (copy {:.1} ms, encode {:.1} ms per frame). The picture here is still, so encoding a busy screen costs more.",
        enc.map_or("?", |e| e.kind.label()),
        f64::from(frames) / started.elapsed().as_secs_f64(),
        copy.as_secs_f64() * 1000.0 / f64::from(n),
        code.as_secs_f64() * 1000.0 / f64::from(n),
    );
    Ok(())
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/dev/null".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let encoder = args.next().unwrap_or_else(|| "auto".into());
    let throughput_encoder = encoder.clone();

    anyhow::ensure!(hypr::available(), "not a Hyprland session");
    if let Some(app) = fullscreen_app() {
        anyhow::bail!("{app} is fullscreen: leave it first, this probe takes over the pointer and adds a monitor");
    }
    hypr::create(NAME, &hypr::monitor_rule(NAME, 1920, 864, 60, "auto-right", 1.5))?;
    let info = hypr::monitors()?.into_iter().find(|m| m.name == NAME).expect("just created");
    println!("output: {}x{} at {},{} scale {}", info.width, info.height, info.x, info.y, info.scale);

    // Pointer: the four quarters of the monitor, in layout coordinates.
    let before = cursor();
    let mut p = VirtualPointer::connect(NAME)?;
    let (lw, lh) = (f64::from(info.width) / info.scale, f64::from(info.height) / info.scale);
    for (x, y) in [(0.25, 0.25), (0.75, 0.75)] {
        p.apply(PointerAction::Move { x, y })?;
        p.sync()?;
        std::thread::sleep(Duration::from_millis(150));
        let got = cursor();
        let want = (f64::from(info.x) + x * lw, f64::from(info.y) + y * lh);
        let close = (got.0 as f64 - want.0).abs() < 3.0 && (got.1 as f64 - want.1).abs() < 3.0;
        println!("pointer {x},{y}: at {got:?}, want ({:.0}, {:.0}) {}", want.0, want.1, if close { "OK" } else { "WRONG" });
    }
    drop(p);

    // Video, through a real socket, into a file.
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let reader = std::thread::spawn(move || -> std::io::Result<(u64, u64, u64)> {
        let (mut s, _) = listener.accept()?;
        let mut file = std::fs::File::create(path)?;
        let (mut frames, mut keys, mut bytes) = (0, 0, 0);
        let mut header = [0u8; 12];
        while s.read_exact(&mut header).is_ok() {
            let size = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
            let mut data = vec![0u8; size];
            s.read_exact(&mut data)?;
            std::io::Write::write_all(&mut file, &data)?;
            frames += 1;
            bytes += size as u64;
            if header[0] & 0x40 != 0 {
                keys += 1;
            }
        }
        Ok((frames, keys, bytes))
    });
    let video = TcpStream::connect(("127.0.0.1", port))?;
    let (tx, rx) = mpsc::channel();
    let params = SessionParams {
        output: NAME.into(),
        fps: 60,
        bitrate_kbps: 8000,
        min_bitrate_kbps: 2000,
        encoder,
        render_node: "/dev/dri/renderD128".into(),
    };
    let started = Instant::now();
    let h = session::spawn(params, video, move |e| {
        let _ = tx.send(e);
    });
    while started.elapsed() < Duration::from_secs(seconds) {
        // The picture only changes when something does: keyframes force copies.
        h.request_keyframe();
        while let Ok(e) = rx.recv_timeout(Duration::from_millis(100)) {
            match e {
                SessionEvent::Ended { error } => println!("ended: {error}"),
                e => println!("{e:?}"),
            }
        }
    }
    drop(h);
    let (frames, keys, bytes) = reader.join().expect("reader")?;
    println!("received {frames} frames, {keys} keyframes, {bytes} bytes");
    throughput(&throughput_encoder)?;
    hypr::remove(NAME)?;
    let _ = std::process::Command::new("hyprctl").args(["dispatch", "movecursor", &before.0.to_string(), &before.1.to_string()]).output();
    Ok(())
}

fn main() {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter("info").init();
    let result = run();
    if let Err(e) = hypr::remove(NAME) {
        eprintln!("removing the output: {e:#}");
    }
    if let Err(e) = result {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
