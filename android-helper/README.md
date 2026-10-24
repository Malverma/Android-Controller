# Wireless Debugging Keeper (Android helper)

A small app for the phones (Android 11+) that keeps **Wireless debugging**
on, so Android Controller can reconnect by itself:

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

## Install

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
or lower may switch Wi-Fi off and on, which the watchdog needs.

## Remove

```sh
adb shell pm uninstall org.androidcontroller.keeper
```
