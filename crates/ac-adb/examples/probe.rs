//! `cargo run -p ac-adb --example probe` — prints server version and devices.
use ac_adb::{AdbClient, DEFAULT_SERVER};

#[tokio::main]
async fn main() -> Result<(), ac_adb::AdbError> {
    let adb = AdbClient::new(DEFAULT_SERVER.parse().unwrap());
    println!("adb server protocol version: {}", adb.version().await?);
    for d in adb.devices().await? {
        println!("{:<40} {:<12} {}", d.serial, d.state.as_str(), d.model.unwrap_or_default());
    }
    Ok(())
}
