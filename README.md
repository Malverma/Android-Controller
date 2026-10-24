# Android Controller

Native Arch Linux (GTK4 + libadwaita) app for viewing and controlling up to
four Android phones over Wi-Fi (five with `max_slots = 5`). The full design is in [`spec.md`](spec.md).

## Status: Milestones 3–4 in progress (needs real-phone testing)

Working (Milestones 1–2):

- **Discovery & pairing**: Avahi browsing, QR / 6-digit code / manual `IP[:port]`
  pairing, and the legacy USB → `adb tcpip 5555` bootstrap.
- **Connection lifecycle**: the per-slot state machine from spec §3.3 with
  reconnects, identity binding and give-up after 300 s.
- **Mirroring & input**: hardware-decoded video (VA-API dma-buf, with copy and
  software fallbacks), touch, pinch, scroll, keyboard, hardware buttons,
  clipboard sync, and a focus view.
- **Persistence**: `devices.toml` and `config.toml` (spec §7.3–7.4).

New in Milestone 3:

- **Grid layout** (spec §6.1): every rows × columns shape is scored by visible
  phone area and the best one wins (relayout animates over 200 ms). Empty slots
  collapse into a narrow column, or are hidden with `ui.hide_empty_slots`.
- **Focus view thumbnails** (§6.2): the other phones stay live in a rail on the
  left; click one or press Alt+1…5 to switch. Escape (pointer off the phone)
  or Alt+G returns to the grid. Only the switched phone's stream restarts.
- **Broadcast input** (§4.10): the toolbar toggle or Alt+Shift+B sends input to
  every ticked phone. Taps are mapped to each phone's own resolution. Power is
  excluded unless `input.broadcast_includes_power = true`.
- **Per-device profile overrides** (`[device.profile_overrides.grid|focus]` in
  `devices.toml`) and grid `max_size = "auto"` restarts with 25 % hysteresis.
- **Status strip**: fps · codec · Mbit/s · size · decoder per tile; total
  bandwidth and decoder in the status bar. `video.graphics_offload = "never"`
  turns off Wayland graphics offload.
- **`state.toml`** (§7.6): window size, focus view and broadcast targets.

Started in Milestone 4:

- **File transfer** (spec §5): drop files or folders on a tile (or use the
  tile's send button → *Send Files…* / *Send Folder…*) to copy them to
  `transfer.default_target_dir` (`/sdcard/Download`). Large files go in
  resumable 32 MiB segments: after a Wi-Fi drop the transfer continues from
  the last finished segment. Files ≥ 64 MiB are checked with SHA-256. A
  name already on the phone gets ` (1)` (`transfer.on_conflict`). Photos,
  videos and music are added to the gallery. A progress strip under the tile
  shows rate, ETA and a cancel button. While a phone is mirroring, its
  transfers are capped at 40 Mbit/s so video keeps up. In broadcast mode a
  drop goes to every ticked phone.
- **APK install**: a dropped `.apk` is installed (Shift+drop copies it
  instead; see `transfer.apk_drop_action`).

- **Android helper** ([`android-helper/`](android-helper/README.md)): an
  optional app for the phones that keeps wireless debugging on (and restarts
  Wi-Fi when a router drop leaves the phone disconnected), so phones
  reconnect without anyone touching them. `android-helper/setup.sh`
  installs it over adb.

Not yet: transfers resuming after an app restart (the journal), adaptive
bitrate (§3.6), audio, preferences UI. The 2-hour, 5-phone soak test from
spec §9 has not been run.

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
cargo test --workspace     # 172 tests, no phone needed (uses an in-process mock ADB server)
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
| `crates/ac-device` | Device manager actor, the pure slot state machine, transfer jobs     |
| `crates/ac-transfer` | Transfer planning: segments, conflict names, APK check, rate limit |
| `crates/adb-mock`  | Fake ADB server for integration tests                                |
| `app`              | GTK4/libadwaita UI                                                   |

## Package

```sh
cd packaging/arch && makepkg -si   # builds the last committed revision
```
