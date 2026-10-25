//! `cargo run -p ac-transfer --example apkinfo -- <file.apk>…`: package and version.

fn main() {
    for p in std::env::args().skip(1) {
        match ac_transfer::apk::info(p.as_ref()) {
            Ok(i) => println!("{p}: {} {} ({})", i.package, i.version_name.unwrap_or_default(), i.version_code.unwrap_or(0)),
            Err(e) => println!("{p}: {e}"),
        }
    }
}
