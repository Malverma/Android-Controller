//! `cargo run -p ac-discovery --example browse` — prints ADB mDNS services for 10 s.
use std::time::Duration;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (rescan, rescan_rx) = tokio::sync::mpsc::unbounded_channel();
    let _task = ac_discovery::spawn_avahi(tx, rescan_rx);
    // Ask once more after a few seconds, like the app does after pairing.
    let mut rescan_at = Box::pin(tokio::time::sleep(Duration::from_secs(3)));
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            Some(ev) = rx.recv() => println!("{ev:?}"),
            _ = &mut rescan_at => {
                let _ = rescan.send(ac_discovery::ServiceKind::Connect);
                rescan_at.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
            }
            _ = &mut deadline => break,
        }
    }
    println!("done");
}
