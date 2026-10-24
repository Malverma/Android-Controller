//! `cargo run -p ac-discovery --example browse` — prints ADB mDNS services for 10 s.
use std::time::Duration;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _task = ac_discovery::spawn_avahi(tx);
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            Some(ev) = rx.recv() => println!("{ev:?}"),
            _ = &mut deadline => break,
        }
    }
    println!("done");
}
