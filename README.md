# Android Controller

Native Arch Linux (GTK4 + libadwaita) app for viewing and controlling up to
five Android phones over Wi-Fi. The full design is in [`spec.md`](spec.md).

## Status: Milestone 1 — ADB LAN manager & discovery

Working now:

- **Discovery**: Avahi (D-Bus) browsing of `_adb-tls-pairing._tcp`,
  `_adb-tls-connect._tcp` and `_adb._tcp`, with the LAN-only address filter.
- **Pairing**: QR code (Android 11+ "Pair device with QR code"), 6-digit
  pairing code, manual `IP[:port]`, and the legacy USB → `adb tcpip 5555`
  bootstrap.
- **Connection lifecycle**: the per-slot state machine from spec §3.3:
  connect with backoff and retry, identity binding by `ro.serialno`,
  heartbeat, automatic reconnect after Wi-Fi drops, mDNS port/IP changes for
  TLS devices, ARP lookup by MAC for legacy devices, and give-up after 300 s.
- **Dashboard**: 5 slot cards with battery, Wi-Fi RSSI and ADB round-trip time,
  plus a Nearby list and toasts.
- **Persistence**: `~/.config/android-controller/devices.toml` (saved devices
  and slots, auto-connect on launch) and `config.toml` (every setting from
  spec §7.3, all optional).

Not yet: screen mirroring and input (Milestone 2), the 5-device video grid and
broadcast input (Milestone 3), and file drag-and-drop (Milestone 4). An online
slot currently shows a placeholder where the mirror will go.

## Run

```sh
sudo pacman -S --needed rust gtk4 libadwaita android-tools scrcpy avahi
sudo systemctl enable --now avahi-daemon.service
cargo run --release -p android-controller
```

On the phone: **Settings › Developer options › Wireless debugging**, then
**Add Device** (Ctrl+N) in the app.

## Test

```sh
cargo test --workspace     # 81 tests, no phone needed (uses an in-process mock ADB server)
cargo clippy --workspace --all-targets
cargo run -p ac-adb --example probe            # talk to your real adb server
cargo run -p ac-discovery --example browse     # list ADB mDNS services for 10 s
```

## Layout

| Crate              | Purpose                                                              |
|--------------------|----------------------------------------------------------------------|
| `crates/ac-core`   | Shared types: slot ids, error codes, device stats                    |
| `crates/ac-config` | `config.toml` / `devices.toml` schemas, validation, atomic saves     |
| `crates/ac-adb`    | Native ADB server client: smart sockets, shell v2, SYNC, forward, pair |
| `crates/ac-discovery` | Avahi browser, LAN filter, mDNS serial parsing, ARP lookup        |
| `crates/ac-device` | Device manager actor and the pure slot state machine                 |
| `crates/adb-mock`  | Fake ADB server for integration tests                                |
| `app`              | GTK4/libadwaita UI                                                   |

## Package

```sh
cd packaging/arch && makepkg -si   # builds the last committed revision
```
