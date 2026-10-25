//! `cargo run -p ac-device --example audio_probe -- <serial> [seconds] [play]`
//!
//! Starts a session with audio on a connected phone (Android 11+) and
//! counts the PCM that arrives. With `play`, it goes to `pw-cat` instead,
//! so the phone's sound comes out of this PC.

use std::net::Shutdown;
use std::process::Command;
use std::time::{Duration, Instant};

use ac_adb::AdbClient;
use ac_scrcpy::ServerParams;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let serial = args.next().expect("usage: audio_probe <serial> [seconds] [play]");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let play = args.next().as_deref() == Some("play");
    let adb = AdbClient::new(([127, 0, 0, 1], 5037).into());
    let version = ac_scrcpy::detect_version("scrcpy".as_ref()).expect("scrcpy --version");
    let params = ServerParams { scid: rand::random::<u32>() & 0x7FFF_FFFF, max_size: 480, audio: true, ..Default::default() };
    let mut launched = ac_scrcpy::launch(&adb, &serial, "/usr/share/scrcpy/scrcpy-server".as_ref(), &version, &params, true)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", e.server_output));
    println!("launched: {}", launched.device_name);

    // Keep the video moving so the server never blocks on it.
    let mut video = launched.video.into_std().unwrap();
    video.set_nonblocking(false).unwrap();
    let video_stop = video.try_clone().unwrap();
    std::thread::spawn(move || std::io::copy(&mut video, &mut std::io::sink()));

    let audio = launched.audio.take().expect("audio socket").into_std().unwrap();
    audio.set_nonblocking(false).unwrap();
    let audio_stop = audio.try_clone().unwrap();
    let out = std::env::temp_dir().join(format!("audio-probe-{}.pcm", std::process::id()));
    let opts = ac_audio::Options { description: launched.device_name.clone(), ..Default::default() };
    let player = if play {
        opts.command()
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("cat > '{}'", out.display()));
        c
    };
    let t = Instant::now();
    let h = std::thread::spawn(move || ac_audio::run(audio, &opts, player));
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let _ = audio_stop.shutdown(Shutdown::Both);
    let r = tokio::task::spawn_blocking(move || h.join().unwrap()).await.unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    println!("audio ended: {r:?}");
    if !play {
        let n = std::fs::metadata(&out).map_or(0, |m| m.len());
        println!("{n} bytes of PCM in {elapsed:.1} s = {:.0} kB/s (48 kHz stereo s16 is 192 kB/s)", n as f64 / elapsed / 1000.0);
        let _ = std::fs::remove_file(&out);
    }
    let _ = video_stop.shutdown(Shutdown::Both);
    launched.server.kill();
    let _ = adb.kill_forward(&serial, &launched.forward_local).await;
}
