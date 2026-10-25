//! `cargo run -p ac-scrcpy --example encoders -- <serial>`: the phone's video
//! encoders, as the app sees them when it decides on H.265.

#[tokio::main]
async fn main() {
    let serial = std::env::args().nth(1).expect("usage: encoders <serial>");
    let adb = ac_adb::AdbClient::new(([127, 0, 0, 1], 5037).into());
    let version = ac_scrcpy::detect_version("scrcpy".as_ref()).expect("scrcpy --version");
    match ac_scrcpy::list_encoders(&adb, &serial, "/usr/share/scrcpy/scrcpy-server".as_ref(), &version, true).await {
        Ok(list) => {
            for e in list {
                println!("{:?} {} {}", e.codec, e.name, if e.hw { "(hardware)" } else { "(software)" });
            }
        }
        Err(e) => eprintln!("{e}\n{}", e.server_output),
    }
}
