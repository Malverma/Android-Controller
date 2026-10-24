//! `cargo run -p ac-scrcpy --example stream -- <serial> [seconds]`
//!
//! Launches scrcpy-server on a connected device through the local adb
//! server, demuxes the video for a few seconds and prints what arrived.

use std::time::{Duration, Instant};

use ac_adb::AdbClient;
use ac_scrcpy::{DemuxItem, Demuxer, ServerParams};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let mut args = std::env::args().skip(1);
    let serial = args.next().expect("usage: stream <serial> [seconds]");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let adb = AdbClient::new(([127, 0, 0, 1], 5037).into());
    let version = ac_scrcpy::detect_version("scrcpy".as_ref()).expect("scrcpy --version");
    let params = ServerParams { scid: rand_scid(), max_size: 720, ..Default::default() };

    let t = Instant::now();
    let launched = ac_scrcpy::launch(&adb, &serial, "/usr/share/scrcpy/scrcpy-server".as_ref(), &version, &params, true)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", e.server_output));
    println!("launched in {:?}: device {:?}", t.elapsed(), launched.device_name);

    let video = launched.video.into_std().unwrap();
    video.set_nonblocking(false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let handle = std::thread::spawn(move || {
        let mut d = Demuxer::new(video);
        println!("codec {:?}", d.read_codec().unwrap());
        let (mut frames, mut keys) = (0, 0);
        while Instant::now() < deadline {
            match d.next_item() {
                Ok(DemuxItem::Session { width, height, .. }) => println!("session {width}x{height}"),
                Ok(DemuxItem::Packet { key_frame, .. }) => {
                    frames += 1;
                    keys += u32::from(key_frame);
                }
                Err(e) => {
                    println!("ended: {e}");
                    break;
                }
            }
        }
        println!("{frames} packets ({keys} key frames), {} bytes", d.bytes_read());
    });
    tokio::task::spawn_blocking(move || handle.join().unwrap()).await.unwrap();
    launched.server.kill();
    let _ = adb.kill_forward(&serial, &launched.forward_local).await;
    drop(launched.control);
}

fn rand_scid() -> u32 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()) & 0x7FFF_FFFF
}
