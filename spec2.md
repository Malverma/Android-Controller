# Test Bed Multi — Specification 2

Plan for the next version of Android Controller (v1.1.0, commit `529e285`). It
covers three changes:

1. **Rename** the product to **Test Bed Multi**.
2. **Reconnect button:** find every saved phone again after its address changed.
3. **Monitor mode:** a per-phone toggle that turns the phone into a monitor for
   the Arch Linux PC.

[`spec.md`](spec.md) stays the reference for everything not changed here.
Section numbers such as "§3.5" refer to it.

**Status (2026-11-01):** M8 to M11 are built and pass their tests, except as listed
in §10. M12's phone measurements and the GitHub and AUR rename are not done.

## 1. What was found

### 1.1 This PC is not ready to run the app yet

Checked on this machine on 2026-11-01:

| Item | State | Consequence |
|---|---|---|
| `adb` (`android-tools`) | Not installed | The app cannot start a connection. |
| `scrcpy` | Not installed. The Arch repos now carry **5.0.1** | **Blocker:** `is_compatible()` in `crates/ac-scrcpy/src/server.rs` accepts only 4.x, so mirroring would report as unavailable. See M8. |
| `~/.android/` | Missing, so no `adbkey` and no `debug.keystore` | **Every phone must pair again with this PC**, unless the folder is copied from the old PC. See §4.6. |
| `~/.config/android-controller/` | Missing | No saved phones here, so there is nothing for Reconnect to find until phones are paired or the config is copied. |
| `intel-media-driver` | Not installed. No Intel VA-API driver is present | No hardware encode or decode on the UHD 620 until it is installed. See §5.4. |
| Android SDK | Missing. JDK 27 is present | The helper APK cannot be built yet. `build.sh` needs build-tools 35 and platform 35. |
| `avahi-daemon` | Running | Discovery can work. |
| `ufw` | Active | Assume inbound connections to the PC are blocked. The design has the PC open every connection. I could not read the rules without sudo. |
| Hyprland | 0.56.2. `hyprctl output` exists. The binary contains screencopy, image-copy-capture, virtual-pointer and virtual-keyboard protocols | Monitor mode looks feasible. **Not run yet**, because creating an output would rearrange the desktop. See M0. |
| Network | Wired `192.168.1.216` and Wi-Fi `192.168.1.112`, same `/24`. `docker0` is down | The sweep must scan this subnet once and skip Docker. |
| Phones right now | `avahi-browse` shows no `_adb-tls-connect` or `_acdirect` service | Nothing to test against yet. They may just have wireless debugging off. |

Test data in the repo, such as `192.168.0.253` for a Schok Volt, suggests the
phones used to be on `192.168.0.x`. If so, the subnet changed and not just the
last number. No address is hardcoded in the app's logic, so this is purely a
matter of how the app finds phones again.

### 1.2 Why phones do not come back today

The app identifies phones by serial and already follows address changes while
it is `Reconnecting`. These gaps are what break it:

| # | Gap | Where |
|---|---|---|
| G1 | The tile's Reconnect button only retries the saved `ip:port`. It never asks the network again. | `UiCommand::Reconnect`, `manager.rs:1701` |
| G2 | Rescans run only during `Reconnecting`, which gives up after 300 s. After that, Avahi's long-running browser drops to about one query an hour (comment in `avahi.rs:85`). | `slot.rs:533-562`, `avahi.rs` |
| G3 | `discovery.legacy_scan_subnet` exists in the config and in §3.5, but **nothing implements it**. | Only in `ac-config` |
| G4 | `/proc/net/arp` is only read, and only while `Reconnecting`. A phone the PC has not spoken to yet is not in that table. | `LookupLegacy` |
| G5 | **Bug: a phone can take over the wrong slot.** The identity check runs only in `Reconnecting`. On a restored slot in `ConnectingAdb`, a different phone now holding the old address is accepted, and `known_id` and `devices.toml` are overwritten. | `Slot::identified`, `slot.rs:709` |
| G6 | `Connect` is ignored in `ConnectingAdb`, so a click during a retry cycle does nothing. | `slot.rs:396` |
| G7 | Without working multicast there is no way to learn the wireless debugging port, which changes on every toggle. The user has to type it. | §3.5 |

## 2. Goals and non-goals

**Goals**

- One click brings back every saved phone after an address change, with no
  re-pairing and no typing in the normal case.
- A clear per-phone reason, and the fix to make, when a phone cannot come back.
- Any phone can become a monitor from a button on the PC or on the phone, and go
  back the same way.
- Monitor mode survives short Wi-Fi drops without shuffling the desktop's
  windows.
- Existing settings, saved phones and helper installs carry over.

**Non-goals for this version**

- Compositors other than Hyprland. The button is shown disabled with the reason.
- Phone audio from the PC while in Monitor mode.
- Reaching phones over the internet or a VPN. `lan_only` still applies.
- Reconnecting by itself when the PC's network changes. The button is the
  trigger for now.

## 3. Rename to Test Bed Multi

Rule: **rename what people see or type. Keep identifiers that live on phones or
in other people's game builds**, because changing them would break upgrades
without giving users anything.

| Thing | From | To |
|---|---|---|
| Display name, window title, docs | Android Controller | Test Bed Multi |
| Binary, Cargo package, Arch `pkgname` | `android-controller` | `test-bed-multi` |
| MCP binary and server name | `android-controller-mcp`, `android-controller` | `test-bed-multi-mcp`, `test-bed-multi` |
| App ID, desktop file | `org.androidcontroller.AndroidController` | `org.testbedmulti.TestBedMulti` |
| Config, state and runtime folders | `android-controller` | `test-bed-multi` |
| Default run folder | `~/Documents/Android Controller/Runs/` | `~/Documents/Test Bed Multi/Runs/` |
| Socket override variable | `ANDROID_CONTROLLER_SOCKET` | `TEST_BED_MULTI_SOCKET` (old name still read) |
| Helper's mentions of the PC app, accessibility label | Android Controller | Test Bed Multi |
| GitHub repo | `Malverma/Android-Controller` | `Malverma/Test-Bed-Multi` |

**Unchanged on purpose**

| Thing | Why |
|---|---|
| Android package and Java package `org.androidcontroller.keeper` | Changing the package would make it a new app: lost permission grant, lost paired PCs. |
| Helper app name, Wireless Debugging Keeper | It describes the app, and you already use the name. |
| `_acdirect._tcp`, `ACD1`, ports 27190 and 27191 | Older helpers and already-built game add-ons speak these. |
| `localabstract:android-controller-dev` | Older helpers listen on it. |
| `/data/local/tmp/android-controller-screen-timeout` | It holds a phone's saved screen timeout. A new name would lose the value to restore. |
| Internal crate names `ac-*` | Nobody sees them, and renaming would bury real changes in churn. |

**Migration on first launch**

- If `~/.config/test-bed-multi` is missing and `~/.config/android-controller`
  exists, move it. Do the same for `~/.local/state`.
- Keep a saved custom run folder as it is. Only the default changes.
- The new package declares `replaces` and `conflicts` for `android-controller`.
- Rename the phone's pushed `android-controller-server.jar` too, and remove the
  old file on connect.
- MCP clients must change their config. The README and release notes will say
  how.

The GitHub repo rename is outward-facing, so it is done last and only after you
confirm it at that point. GitHub redirects the old URL, which keeps the helper
download link in old READMEs working. The AUR package gets a new name in the
same step.

## 4. Reconnect button

### 4.1 UI

- **Toolbar:** a **Reconnect** button beside **Nearby** and **Add Device**
  (`view-refresh-symbolic`). Shortcut Ctrl+R. Also offered through MCP as
  `reconnect_all`. Saved phones always have tiles, so the welcome page never needs
  this button.
- **Tile:** the existing per-phone Reconnect button runs the same search for that
  one phone. This fixes G1.
- **Progress:** the toolbar button shows a spinner. Each affected tile shows its
  current step, such as "Searching the network…" or "Found at 192.168.1.84,
  connecting…".
- **Result:** one toast, such as "3 of 4 phones reconnected", with a **Details**
  button listing the rest.
- **Clicking again** while a search runs does nothing. Phones that are already
  online are left alone, so their mirrors and monitors are not interrupted.

### 4.2 Search order

A new `ReconnectJob` in `crates/ac-device/src/manager/reconnect.rs` drives the
steps. For each saved phone that is not online, it goes through these until one
succeeds. Steps 1 and 2 run in parallel for all phones.

| Step | What it does | Finds |
|---|---|---|
| 0. Prepare | Heartbeat the phones that look online (1.5 s). Drop stale `adb` transports. Clear each target's "user disconnected" flag. Reset its give-up timer. Skip saved addresses outside every local subnet. | |
| 1. mDNS | Replay what is already known, and start a fresh Avahi query for all service types. A browser started in the last 5 s is still querying, so the existing merge stays. Wait up to 4 s. | Address and port, when multicast works |
| 2. Sweep | Connect to each host on the local subnets: 27193 (keeper), 27190 (direct), 5555 (legacy). 64 at a time, 300 ms timeout. | Address, and with keeper the port |
| 3. Neighbour table | Read `/proc/net/arp`, which step 2 just filled, and match the saved `wifi_mac`. | Address only |
| 4. Port search | Only for a phone with a known address but no port: scan TCP 30000–49999 on that one IP, then try `adb connect` on what answers. | Port, as a last resort |
| 5. Verify | Identify the device and compare it with the slot's saved ID (§4.4). | |

**Subnets for the sweep:** IPv4 on interfaces that are up. Skip loopback,
`docker*`, `br-*`, `veth*`, `virbr*`. Merge duplicates, so this PC scans
`192.168.1.0/24` once despite having two interfaces. A network larger than `/24`
scans only the `/24` holding the PC's own address. It respects `lan_only`, and it
only runs when the user clicks. The `legacy_scan_subnet` setting does not control
it. That setting still does nothing, and the docs will say so.

**Time limit:** 20 s for the whole job. Slower answers still land through normal
discovery, because rescans stay active.

### 4.3 How the Wireless Debugging Keeper helps

Helper 0.8 adds a small **locator** to `KeeperService`, so phones can be found
with multicast blocked:

- Listens on TCP **27193**. Request: magic `TBL1`. Reply: one JSON line, then
  close.
- `{"keeper_id":"<random uuid>","version":8,"adb_on":true,"adb_port":41927,"wifi_trusted":true}`
- `keeper_id` is generated by the helper on first run. The PC reads it over ADB
  when connected (a `KEEPER_ID` broadcast to `BootReceiver`, answered in its result
  data) and stores it in `devices.toml`, so a later reply can be matched to a slot. It is not used for trust: the phone must still pass the ADB key
  check and the identity check.
- It accepts no commands and gives away only that one random ID and the port. A
  switch in the helper turns it off.
- `adb_on: false` lets the app tell you the actual problem. See below.

Reading `adb_port` from an app may be blocked by SELinux, so M0 tests
`service.adb.tls.port` first. If it is blocked, the reply leaves out the port and
step 4 finds it.

### 4.4 Identity fix (G5)

`Slot::identified` must reject a mismatch whenever the slot already has a device
ID, not just while `Reconnecting`. On mismatch it disconnects that transport and
carries on searching. The state machine tests get cases for `ConnectingAdb` and
for two phones that swapped addresses. A brand-new pairing on a free slot is
unaffected.

### 4.5 State machine changes

- New `SlotEvent::Relocate { addr, transport }`, accepted from `Disconnected`,
  `ConnectingAdb` and `Reconnecting`. It resets attempts and the give-up clock,
  which fixes G6.
- `UiCommand::Reconnect { slot }` starts a one-phone job. `UiCommand::ReconnectAll`
  starts one for all.
- `UiEvent::Reconnecting { active }` and `UiEvent::ReconnectDone { results }`. The
  current step reaches the tile through `SlotView.detail`.
- New pure, tested modules: subnet listing and filtering, and sweep planning in
  `ac-discovery/src/sweep.rs`.

### 4.6 What the button cannot fix

| Situation | What the app says | Why |
|---|---|---|
| Phone on a Wi-Fi network Android has not trusted | "On *phone*: turn on Wireless debugging once and tick **Always allow on this network**" | From my memory of AOSP, trust is per access point (BSSID). So a replaced router or a new mesh node needs one tap per phone. The helper has the same limit today. To confirm in M0. |
| Phone paired with another PC's key | "*Phone* isn't paired with this PC. Pair it" with a button to **Add Device** | No saved key means no access. This applies to this PC today unless `~/.android/adbkey` is copied from the old one. |
| Phone without the helper and multicast blocked | "Could not find *phone*. Type its IP:port" with a field | The sweep finds nothing to ask. |
| Phone off or out of range | "*Phone* did not answer" | |

## 5. Monitor mode

### 5.1 Behaviour

- **Extended display.** The phone becomes a new Hyprland monitor to the right of
  the others. Windows and workspaces can move to it as usual. This fits "a monitor
  for the PC" better than copying an existing screen. Copying can be added later
  with the same parts.
- **Buttons, all driving one state:**
  - PC tile: a **Monitor** toggle (`video-display-symbolic`, tooltip "Use this
    phone as a monitor for this PC"). Also in the focus view's rail.
  - Phone: **Use as PC monitor** in the helper's main screen, plus an action on the
    Keeper notification.
  - Phone, while active: tap the screen with three fingers for an **Exit Monitor**
    bar, or use the notification action. Back does not exit, so a stray swipe does
    not close your display.
- **Touch** controls the PC pointer on that monitor: tap to click, drag to drag,
  long press to right-click, two fingers to scroll. A **view only** switch on the
  tile turns it off.
- **Tile while active:** the mirror stops, because mirroring a phone that is
  showing the PC is pointless and wastes bandwidth. The tile shows "Monitor ·
  1920×864 · 60 fps · 8 Mbit/s" and the toggle.

### 5.2 Choice of approach

| Option | Verdict | Why |
|---|---|---|
| **Own stream to the helper** | **Chosen** | The helper is already on every phone, so one tap works with no extra apps. It shares ADB tunnels and pairing. Reuses `rl-host` capture and input from your RemoteLinux project. |
| wayvnc plus a VNC app | No | A separate app and login per phone, weak touch mapping, no toggle from the tile. |
| Sunshine plus Moonlight | No | Heavy, its own pairing, and meant for one game-streaming client. |
| xdg-desktop-portal capture | No | It prompts on every start, which defeats a one-tap toggle. |

### 5.3 Data flow

```
Hyprland headless output TBM-<slot>
   │ wlr-screencopy (ported from rl-host/capture.rs)
   ▼
ac-monitor: H.264 encode ──► ADB forward (TLS) or direct socket ──► helper
                                                                     │ MediaCodec
zwlr_virtual_pointer_v1 ◄── touch events ◄── control channel ◄── MonitorActivity
```

**Starting** (PC side, new crate `crates/ac-monitor`):

1. Stop the slot's mirror stream.
2. Ask the helper for screen size and density.
3. `hyprctl output create headless TBM-<slot>`, then
   `hyprctl keyword monitor TBM-<slot>,<W>x<H>@60,auto-right,<scale>`. Nothing is
   written to `~/.config/hypr`.
4. Start capture, encoder and the video connection. The helper opens
   `MonitorActivity`.

**Stopping** reverses this, then restarts the mirror. Removing the output returns
its windows to the other monitors, which Hyprland handles. Leftover `TBM-*`
outputs from a crash are removed at startup.

**Video:**

- Landscape by default, with a per-phone portrait option in `devices.toml`.
- Longest side capped at 1920 by default.
- Default scale 1.5, so text is readable on a small screen. M0 confirms Hyprland
  accepts that value at these sizes.
- H.264, no B-frames, short GOP, frames sent as they arrive. Uses the same 12-byte
  frame header as the scrcpy format the code already parses.
- 8 Mbit/s by default. Reuses the logic in `adaptive.rs`, stepping down when the
  socket backs up. When the send queue is full, the PC drops frames and asks for a
  keyframe, so latency stays bounded instead of growing.

### 5.4 Encoder order on the PC

`nvenc`, then `vaapi`, then `libx264` (ultrafast, zerolatency).

- `rl-host` has NVENC and x264 only, so **VA-API must be added**. This PC needs it:
  it is a ThinkPad with UHD 620 and no NVIDIA GPU.
- It needs `intel-media-driver`, added as an `optdepends`. The app warns when it
  falls back to x264.
- `rl-host` uses `ffmpeg-next`, while `ac-media` uses `ffmpeg-sys-next`. M11 ports
  the encoder to `ffmpeg-sys-next`, so only one binding is linked.

Up to two monitor phones at once by default (`monitor.max_concurrent`). M12
measures whether this PC can sustain more.

### 5.5 Connection and security

The PC opens every connection, because `ufw` is active here and this avoids
needing a firewall rule.

| Phone connects by | Tunnel | Encryption | Touch default |
|---|---|---|---|
| Wireless debugging | `adb forward` to `localabstract:test-bed-multi-monitor`, UID-checked as shell, like `Dev.SOCKET` | Yes, ADB TLS | On |
| Legacy `adb tcpip` | Same | **No** | **Off**, with a warning explaining why |
| No ADB (Direct) | New purpose `MONITOR = 5` on port 27190, after the existing HMAC challenge | **Added:** AES-256-GCM with a per-session key derived from the pairing key | On |

The reason for the extra care: this is the first feature where the phone can
drive the PC. An unauthenticated network packet that clicks on your desktop is
much worse than one that taps a test phone. So the Direct path gets real
encryption, and the plaintext path has touch off unless you turn it on.

Other safeguards:

- The status bar shows when a phone has control of the pointer.
- Input goes only through a virtual pointer bound to that one output, so it needs
  no `/dev/uinput` access and no sudo. Keyboard input is out of scope.
- The stream shows your full desktop on that monitor, so anyone holding the phone
  sees it. If the phone locks, the stream stops and the display goes blank until
  it is unlocked, unless "show over lock screen" is enabled in the helper. That
  setting is off by default.

**Phone button to PC.** The PC keeps one long-lived control connection to each
online phone running helper 0.8 or later. It carries JSON in the existing `Proto`
framing, both ways:

`monitor_request`, `monitor_start`, `monitor_stop`, `touch`, `size_changed`,
`ping`.

If the PC is not connected when the button is tapped, the helper makes sure
wireless debugging is on, then shows "Waiting for the PC… open Test Bed Multi and
press Reconnect". The request stays valid for 60 s.

### 5.6 Stability, using the Keeper

New `MonitorService` in the helper (foreground, same `specialUse` pattern) holds
these while active:

- `WIFI_MODE_FULL_LOW_LATENCY` on Android 10+, `FULL_HIGH_PERF` below.
- A partial wake lock, plus `FLAG_KEEP_SCREEN_ON` on the activity.
- `KEY_LOW_LATENCY` on the decoder on Android 11+.

The Keeper changes behaviour while a monitor session is active:

- **Wi-Fi watchdog:** reacts after 20 s instead of 45 s.
- **Wireless debugging:** already restored by `KeeperService`; the new port is
  found via §4.
- **Battery exemption:** the app warns if it is missing, since Android would
  otherwise kill the session.

**Drop handling:**

| Time since the link dropped | PC | Phone |
|---|---|---|
| 0–30 s | Keep the headless output, pause the encoder, and run the normal reconnect. Windows stay put. | Last frame with "Reconnecting to PC…" |
| On reconnect | Resume with a keyframe. | Picture returns. |
| After 30 s | Remove the output so windows are not stranded on an invisible monitor. Keep `monitor_desired`. | Keep waiting. |
| Comes back later | Recreate the output and resume. | Picture returns. |
| User pressed Exit | Clear `monitor_desired`. Do not restart on reconnect. | Exit. |

`monitor_desired` lasts for the app session, and is not restored after the PC
restarts. Waking up with a phone unexpectedly on your desktop would be confusing.

### 5.7 Slot changes and conflicts

The code already uses "monitor" for the ADB device tracker
(`Effect::StartMonitor`, `StopMonitor`) and for the Graphs button. So the new code
is named **display**: `DisplayPhase`, `Effect::StartDisplay`,
`UiCommand::SetDisplay { slot, on }`. The UI still says "Monitor".

`DisplayPhase` is `Off | Starting | Live | Paused | Failed`. It sits next to
`StreamPhase`, and only one of the two runs. In `identified()`, `monitor_desired`
decides which starts.

| Feature, while this phone is a monitor | Behaviour |
|---|---|
| Mirror, focus view, Record | Unavailable on this tile, with an explanation. |
| Broadcast, Mirror clicks | This phone is skipped. |
| Test run, crash watch, logcat | Keep working. |
| Starting Monitor during a test run on that phone | Asks first, because it hides the app under test. |
| File transfer, APK install | Keep working. |
| Automation tap, swipe, screenshot | Return a new `E_MONITOR_ACTIVE`. New MCP tool `set_monitor`. |
| Disconnect or Forget | Ends Monitor first. |

**Failures, each giving a clear reason:** not Hyprland, helper older than 0.8
(offers **Update**), encoder failure, phone has no `video/avc` decoder.

### 5.8 Config additions

```toml
[monitor]
max_concurrent = 2
max_size = 1920
fps = 60
bitrate_mbps = 8
scale = 1.5
encoder = "auto"          # auto | nvenc | vaapi | x264
position = "auto-right"
```

`devices.toml` gains `keeper_id`, `monitor_orientation` and
`monitor_view_only`, all optional, so old files still load.

## 6. Helper signing risk

`build.sh` signs with `~/.android/debug.keystore`, and that file is **not on this
PC**. Android refuses an update signed with a different key. So if helper 0.8 is
signed with a new key, phones with helper 0.7 will reject the update.

- **If the old keystore can be copied from the old PC:** updates install normally.
  This is much the better outcome.
- **If it is lost:**
  - ADB phones: on `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, the app asks, then
    uninstalls and reinstalls. Setup then grants the permission again by itself.
  - No-ADB phones: uninstall by hand on the phone, then pair again.
- **Either way:** from now on, sign with a dedicated release keystore stored
  outside the repo and backed up, so this cannot happen again. `build.sh` already
  accepts `KEYSTORE`.

## 7. Milestones

Each milestone ends with `cargo test --workspace`, `cargo clippy` and a working
app. They continue spec.md's numbering. Commits are made per milestone on a
feature branch.

### M0 — Setup and feasibility checks (do first)

Needs you: `sudo pacman -S android-tools scrcpy intel-media-driver` and the SDK
parts, plus at least one phone.

| Question | How it is tested | If the answer is no |
|---|---|---|
| Can a headless output be created, named and scaled on Hyprland 0.56? | `hyprctl output create`, with your go-ahead since windows may move | Monitor mode needs a redesign, and I come back to you. |
| Does screencopy of a headless output produce frames at 60 fps? | Small test program using `rl-host/capture.rs` | Try `ext-image-copy-capture`. |
| Does VA-API H.264 encode work on the UHD 620? | `ffmpeg` with `h264_vaapi` | x264, and measure the CPU cost. |
| Does a virtual pointer map to the right output? | Small test program | Fall back to `uinput`, which needs a udev rule. |
| Can the helper read `service.adb.tls.port`? | `getprop` from the app on real phones | Step 4's port search. |
| Is wireless debugging trust per BSSID? | Read AOSP, try it on a phone | Adjust the §4.6 wording. |
| Does scrcpy 5.0.1 still match the 4.x wire format? | Run `ac-scrcpy`'s fixtures and a real stream | Port the demuxer. See M8. |

### M8 — Rename and restore the build

- Support scrcpy 5.x, or vendor a 4.x server if the protocol changed badly. New
  fixture from 5.0.1. This is first because nothing can be tested without it.
- Everything in §3 except the GitHub rename.
- Config migration, with tests.
- **Done when:** the app starts as Test Bed Multi, an old config folder migrates,
  and one phone mirrors. `git grep -i "android.controller"` returns only the items
  on the unchanged list, the migration code, and `spec.md`.

### M9 — Reconnect

- G1–G6: `ReconnectJob`, sweep, `Relocate`, identity fix, UI, MCP tool.
- **Tests with no phone:** mock ADB plus fake discovery covering a changed address,
  a swapped pair of phones, an already-online phone left alone, a wrong phone at
  the old address, and the 20 s limit.
- **Done when (real phones):** each case below reconnects with one click and no
  typing:
  - DHCP lease changed.
  - Router subnet changed, for example `192.168.0.x` to `192.168.1.x`, simulated by
    editing `last_address` if you do not want to touch the router.
  - App restarted after the change.
  - Phones that have the old address in `devices.toml` while another device now
    holds it.

### M10 — Helper 0.8

- Locator on 27193 (closes G7), with its off switch.
- Control channel, `MonitorActivity`, `MonitorService`, decoder, touch capture,
  buttons, notification action, watchdog changes.
- Signing handled per §6.
- **Done when:** a test stream from a small PC tool displays on the phone at the
  right size, touch events reach the tool, and existing helper features still work
  on an Android 6–10 phone and an Android 11+ phone.

### M11 — Monitor on the PC

- `ac-monitor` crate: `hypr.rs`, `capture.rs`, `encode.rs`, `input.rs`,
  `session.rs`. Parsing and command building are pure and unit tested.
- Slot and manager changes, tile button, Direct encryption.
- **Done when:** on both ADB and no-ADB phones, a tap on the tile or the phone turns
  it into a monitor, a window can be dragged onto it and clicked by touch, and a tap
  turns it back with the mirror restored.

### M12 — Hardening and release

- Drop tests, using the §5.6 table with Wi-Fi turned off for 10 s and for 60 s.
- Measure latency, CPU and battery on this PC. Targets to confirm: under 80 ms
  glass to glass on good Wi-Fi, and 30 minutes with no stall.
- Find the real limit for `max_concurrent`.
- Update `README.md` and helper docs. Add a pointer to this file in `spec.md`.
- Version 2.0.0, helper APK, PKGBUILDs.
- GitHub repo and AUR rename, after you confirm.

## 8. Needed from you when building starts

1. **Install** the packages listed in M0 (needs sudo).
2. **Old PC, if it still exists:** copy `~/.android/` and
   `~/.config/android-controller/` over. This avoids re-pairing every phone and
   avoids the signing problem in §6. Without it, each phone needs one pairing.
3. **Have at least one phone** with wireless debugging on, ideally one phone on
   Android 11+ and one older.
4. **Allow the Hyprland test** in M0, since it may briefly move windows.

## 9. Decisions made where the request left room

Say so if any of these is wrong. Each is cheaper to change before M9.

| Decision | Choice | Alternative |
|---|---|---|
| Where the Monitor button lives | Both the PC tile and the phone | Only one of them |
| Monitor type | Extended display | Copy of an existing screen |
| Touch on the phone in Monitor mode | Controls the pointer, can be turned off | View only |
| Reconnect button scope | All saved phones, plus the per-tile button | Per-tile only |
| Network search | Active scan of the local subnet, only on click | mDNS only, which fails on routers that filter multicast |
| Android package ID | Stays `org.androidcontroller.keeper` | Change it, and reinstall on every phone |
| Version number | 2.0.0, because of the rename and new config paths | 1.2.0 |

## 10. What changed while building, and what is still open

**Changes from this plan**

| Plan | Built | Why |
|---|---|---|
| Port the encoder to `ffmpeg-sys-next` (§5.4) | Uses `ffmpeg-next`, which sits on the same `ffmpeg-sys-next` | One copy of FFmpeg is linked either way, and the port would add only risk |
| Direct path with AES-GCM (§5.5) | **Not built.** A phone on the no-ADB connection is told monitor mode needs wireless debugging | Touch must never travel unencrypted, and refusing is the safe state until the encrypted path exists |
| `monitor_orientation` in `devices.toml` (§5.8) | `monitor_portrait`, plus a tile toggle | Two states only; hand edits would be overwritten while the app runs |
| Short GOP (§5.3) | A keyframe every 2 s, each carrying SPS and PPS | The phone's decoder joins at any keyframe |
| `monitor_start`, `monitor_stop` (§5.5) | `start`, `stop`, and separate `control` and `video` connections | Video never delays touch or control |
| Reuse `adaptive.rs` (§5.3) | `RateControl` in `ac-monitor`: drops above 10% step the bitrate down, 20 clean seconds step it up | The monitor's signal is its own send queue, not the scrcpy stream |
| `MonitorActivity` private | Exported, limited to the shell by the `DUMP` permission | `am start` over ADB needs it to be exported; other apps still cannot open it |

**Not verified yet, because this PC has no phone, Android SDK or `adb`**

- The helper's Java only compiles against the Android API. It has never run, and
  `android-helper/dist` still holds **0.7**, so monitor mode is blocked with a clear
  reason on every phone until 0.8 is built and installed.
- Decoding, touch, drop handling and latency on a real phone, and all of §7 M12.
- The UI changes have been compiled but not looked at.
- `tests/monitor.rs` runs the real manager and framing against a stand-in for the
  helper. It does not prove the Java agrees, beyond reading both sides.

**Not verified yet on this PC**

- `zwlr_virtual_pointer_v1` output mapping: the one run happened over a fullscreen
  game holding the pointer, so its result means nothing. Run `monitor_probe` with
  nothing fullscreen.
- Capture and encode throughput at 60 fps, and VA-API, which needs
  `intel-media-driver`.

**Test reliability:** all tests pass in normal runs. With 12 copies of the manager
tests at once, one run of 36 copies had failures in three Reconnect tests. It did not
happen again in 60 further copies, and the cause was not found. The tests use short
search deadlines, so timing is the likely reason, but that is unproven.

**Limits to expect**

- While an app elsewhere has locked or confined the pointer, such as a fullscreen
  game, touches on the phone cannot move it to the phone's monitor.
- Removing an output can move the pointer: Hyprland decides where it goes.
