# Android Controller

Native Arch Linux (GTK4 + libadwaita) app for viewing and controlling up to
four Android phones over Wi-Fi (five with `max_slots = 5`). The full design is in [`spec.md`](spec.md).

## Status: v1.1 — Milestones 1–7 built

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
  phone area and the best one wins (relayout animates over 200 ms).
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
- **Phones without ADB** (Android 10 and older, no USB cable): the helper's
  *PC connection without ADB* mirrors the screen (Android screen capture,
  H.264 in scrcpy's stream format), takes taps, swipes and typing through an
  accessibility service, and receives files and APKs. Pair it under
  **Add Device › No ADB** with the code the phone shows (`crates/ac-direct`).
- **Transfers survive an app restart**: unfinished jobs are kept in
  `~/.local/state/android-controller/transfers.json` (spec §7.5). On the next
  launch they wait for their phone and continue from the part already on
  it. A file that changed while the app was closed is skipped with a notice.
- **Audio in focus view**: the focused phone's sound plays on the PC through
  PipeWire (`pw-cat`, from `pipewire-audio`). Needs Android 11+ and an ADB
  connection; while it plays, the phone's own speaker is quiet. Turn it off
  or set the volume in Preferences.
- **Preferences** (menu › Preferences, or Ctrl+,): video quality, audio,
  keyboard and mouse, file transfer folder/conflicts/speed limits, look.
  Changes are saved to `config.toml` at once.
- **Mirror clicks** (header button, or Alt+Shift+M): taps, swipes, keys and
  typing on any phone repeat on every phone that is mirroring, at the same
  relative spot on each screen. Useful for testing an app or game on several
  phones at once; an APK dropped on one phone while it is on installs on all
  of them. Unlike Broadcast, there are no phones to tick: it follows
  whichever phones are connected.

New in Milestone 5 — app testing on every phone (spec §9, M5), from the
toolbar under the header. The toolbar also holds **Add Device** and
**Nearby** (phones on the network or USB that can be added); empty slots get
no tile, and a welcome page shows while no phone is added.

- **Test runs**: *Start run* saves, per phone, the full system log
  (`logcat.txt`), every crash and ANR with its stack trace (`crashes.txt`) and
  the graphs (`stats.csv`) into one folder under `~/Documents/Android
  Controller/Runs/`. *Stop run* adds a `summary.txt`. Change the folder in
  Preferences › Testing.
- **Crash watch**: when an app crashes, freezes (ANR) or dies natively on a
  phone, its tile gets a red ⚠ count. Click it for the stack traces. On by
  default, also outside runs. Needs ADB.
- **Install, launch, stop, clear data, uninstall** on all connected phones
  at once. Choose an APK (its package fills in) or pick an installed app.
  Each tile shows ✓ or ✗ with the phone's reason.
- **Screenshot** at full resolution, and **Record**
  every phone into MP4 files. Recordings are made on the
  PC from the mirror stream, so there is no time limit and phones without
  ADB record too.
- **Graphs** (monitor button): frame rate of the app on screen, CPU, memory
  (and the test app's), and temperature, on every tile.

New in Milestone 6:

- **Adaptive bitrate** (§3.6): when a phone's video falls behind on a busy
  Wi-Fi, its quality steps down, and back up once the network keeps up. The
  tile says “reduced for Wi-Fi” meanwhile. Preferences › Video › *Adapt to
  the Wi-Fi*.
- **H.265**: each phone's hardware encoders and the PC's VA-API decoder are
  checked once. Focus view uses H.265 when both ends have it, and falls back
  to H.264 by itself if it fails.
- **Helper 0.6** for phones without ADB:
  - multi-touch (pinch, two-finger swipe);
  - the phone's clipboard reaches the PC (Android 10+: tap *Send clipboard to
    PC* in its notification);
  - memory, temperature and CPU graphs;
  - app launch, stop and uninstall, and the app list;
  - H.265 encoding.
- **Helper updates**: the app carries the latest helper. A phone with an older
  one gets an *Update* button on its tile.
- **AUR package**: [`packaging/aur/android-controller-git`](packaging/aur/android-controller-git/PKGBUILD).

New in 1.1 — **automation for Claude** (spec §9, Milestone 7):

- **Control socket**: the app listens on
  `$XDG_RUNTIME_DIR/android-controller/control.sock` (your user only) for
  JSON-line requests: list phones, screenshot, tap/swipe/type/keys, read the
  screen's elements, install/launch/stop apps, crashes, logs, performance,
  test runs, recording and test settings. Preferences › Testing ›
  Automation turns it off. Clearing app data or uninstalling waits for you
  to allow it in the window, and the status bar shows when a client is
  connected.
- **MCP server** `android-controller-mcp` (`crates/ac-mcp`): gives Claude in
  Cursor (or Claude Code) these as tools. Add it to `~/.cursor/mcp.json`:

  ```json
  {"mcpServers": {"android-controller": {"command": "android-controller-mcp"}}}
  ```

  Then ask Claude to test your app on the phones. The app must be running.
- **Helper 0.7** (Wireless Debugging Keeper):
  - the screen's elements in ~0.2 s, instead of 1–3 s with `uiautomator`;
  - tap by text or id, and wait for an element;
  - popup, dialog and toast events;
  - auto-allow for permission prompts;
  - animations on or off.

  Over ADB the app switches its touch control on by itself.
- **Game debug add-on** ([`ACaddon.md`](ACaddon.md), [`game-addon/`](game-addon)):
  a debug-only server inside a game (Unity, Godot and native Android
  versions) that lets Claude read the game's state, run its debug commands
  (load a level, give coins), and read its logs. Its exceptions show as
  crash badges.

Not run yet: the 2-hour, 5-phone soak test from spec §9.

## Run

```sh
sudo pacman -S --needed rust gtk4 libadwaita android-tools scrcpy avahi pipewire-audio
sudo systemctl enable --now avahi-daemon.service
cargo run --release -p android-controller
```

On the phone: **Settings › Developer options › Wireless debugging**, then
**Add Device** (toolbar, or Ctrl+N) in the app.

## Test

```sh
cargo test --workspace     # no phone needed (uses an in-process mock ADB server)
cargo clippy --workspace --all-targets
cargo run -p ac-adb --example probe            # talk to your real adb server
cargo run -p ac-discovery --example browse     # list ADB mDNS services for 10 s
cargo run -p ac-device --example audio_probe -- <serial>   # check a phone's audio capture
cargo run -p ac-scrcpy --example encoders -- <serial>      # a phone's video encoders
cargo run -p ac-device --example testing_probe -- <serial> [pkg]  # perf, screenshot, crash parser
cargo run -p ac-device --example testing_e2e -- <out-dir>  # a test run on every phone, headless
cargo run -p ac-device --example automation_host -- $XDG_RUNTIME_DIR/ac-test/control.sock  # headless control socket
ANDROID_CONTROLLER_SOCKET=$XDG_RUNTIME_DIR/ac-test/control.sock android-controller-mcp  # MCP against it
python3 game-addon/tools/fake_game.py   # a fake game add-on (with: adb reverse tcp:27191 tcp:27191)
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
| `crates/ac-direct` | PC side of the helper's no-ADB connection (pairing, stream, files)  |
| `crates/ac-audio`  | Focus-view audio: raw PCM from scrcpy-server played via PipeWire    |
| `crates/ac-mcp`    | `android-controller-mcp`: MCP server for Claude over the control socket |
| `crates/adb-mock`  | Fake ADB server for integration tests                                |
| `app`              | GTK4/libadwaita UI                                                   |

## Package

```sh
cd packaging/arch && makepkg -si   # builds the last committed revision
```

From the AUR (builds the latest commit on GitHub):

```sh
cd packaging/aur/android-controller-git && makepkg -si   # or: yay -S android-controller-git
```

Both install the phone helper to
`/usr/share/android-controller/wireless-debugging-keeper.apk`, and
`android-controller-mcp`, as well.
