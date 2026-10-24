# Wireless Debugging Keeper (Android helper)

A small app for the phones (Android 6+) that keeps **Wireless debugging**
on (Android 11+), so Android Controller can reconnect by itself, and that
lets phones **without** wireless debugging connect without ADB at all:

- Android turns wireless debugging off whenever Wi-Fi drops and after a
  reboot. The keeper turns it back on as soon as Wi-Fi is connected, and
  whenever anything else switches it off. The PC app finds the new port
  over mDNS.
- **Wi-Fi watchdog:** when a router drops a phone (for example "AP busy"),
  Android can avoid that network for many minutes even though Wi-Fi is on.
  If Wi-Fi stays on but disconnected for 45 s, the keeper switches it off
  and on (then waits 1, 2, 4, 8, up to 15 minutes between further tries).
  It never touches Wi-Fi you switched off yourself.
- A silent notification shows what it is doing, with a **Pause** button.
- **PC connection without ADB** (off by default): for Android 10 and older
  when no USB data cable is at hand. See below.

## Install on a new phone (download link)

1. On the phone, open this link and install the app (allow your browser to
   "install unknown apps" when Android asks):
   **https://github.com/Malverma/Android-Controller/raw/main/android-helper/dist/wireless-debugging-keeper.apk**
   One APK works on 32-bit and 64-bit phones (it has no native code).
   If Android says "There was a problem parsing the package", the phone is
   older than Android 6, or the download is incomplete: delete it from
   Downloads and download it again.
2. Turn on **Developer options › Wireless debugging** and pair the phone in
   Android Controller (**Add Device**). Tick **Always allow on this network**.
3. That's it: when the phone connects, Android Controller grants the
   helper its permission and starts it, and shows a toast. Until then the
   helper's notification says it needs setup from the PC.

## Phones without wireless debugging: PC connection without ADB

Android 10 and older have no wireless debugging, and without a USB cable
`adb tcpip` can't be switched on either. The helper can then be the
connection itself:

1. Install the app from the link above and open it.
2. Switch on **Allow PC connection without ADB** and allow storing files.
3. Tap **Turn on touch control** and enable *Android Controller touch
   control* under Accessibility (without it the PC can only watch).
4. Tap **Pair with PC**. In Android Controller open **Add Device › No ADB**,
   pick the phone (or type the IP shown on the phone) and enter the 6-digit
   code.
5. The first time the PC mirrors, Android asks to start capturing: tap
   **Start now** (on Android 9 and older tick **Don't show again**, and it
   won't ask after a reboot either).

What works compared with ADB phones:

| | ADB (wireless debugging / tcpip) | Without ADB |
|---|---|---|
| Mirroring | scrcpy | Android screen capture, H.264, slightly more delay |
| Taps, swipes, long press, scrolling | yes | yes (accessibility gestures, Android 7+) |
| Multi-touch / pinch | yes | no |
| Typing | key events | text put into the focused field; Enter, Backspace, arrows |
| Back / Home / Recents / volume / power | yes | yes |
| Files to Download | yes | yes (resumable, checksummed) |
| APK install | silent | Android's installer asks on the phone |
| Clipboard from the phone | yes | no (PC → phone works) |

Security: only PCs paired with the on-screen code are accepted (P-256 key
exchange confirmed by the code, then an HMAC challenge on every
connection). The video and input are not encrypted on the network, like
`adb tcpip`. Pairing is open only while the pairing screen shows, for two
minutes. **Forget paired PCs** in the app revokes them.

The helper listens on TCP port 27190 and announces `_acdirect._tcp` over
mDNS. Keys on the PC are in `~/.config/android-controller/direct-keys.toml`.

## Install from the PC

With each phone reachable by adb (USB, or wireless debugging already on):

```sh
android-helper/setup.sh                # every phone adb sees
android-helper/setup.sh SERIAL ...     # or just these
```

This builds the APK if needed, installs it, grants
`WRITE_SECURE_SETTINGS` (the one permission that lets an app change the
wireless debugging switch; it can only be granted over adb and survives
reboots), lifts battery restrictions, and starts it.

Wireless debugging must have been turned on by hand once on each Wi-Fi
network with **Always allow on this network** ticked; Android asks again
for networks it has not seen.

## Build

`android-helper/build.sh` uses the SDK tools directly (no Gradle): a JDK
17+ and `sdkmanager "build-tools;35.0.0" "platforms;android-35"`. It signs
with `~/.android/debug.keystore` (created if missing).

The app targets SDK 28 on purpose: since Android 10 only apps targeting 28
or lower may switch Wi-Fi off and on, which the watchdog needs. (It also
keeps screen capture and file access simple on the older phones that use
the connection without ADB.)

## Remove

```sh
adb shell pm uninstall org.androidcontroller.keeper
```
