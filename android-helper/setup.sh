#!/usr/bin/env bash
# Installs Wireless Debugging Keeper on phones and grants what it needs.
#   ./setup.sh                 every phone adb currently sees
#   ./setup.sh SERIAL [...]    just these (USB serial or ip:port)
# The phone must already be reachable by adb (USB, or wireless debugging on).
set -euo pipefail
cd "$(dirname "$0")"
PKG=org.androidcontroller.keeper
APK=build/wireless-debugging-keeper.apk
[[ -f $APK ]] || ./build.sh

serials=("$@")
if [[ ${#serials[@]} -eq 0 ]]; then
    mapfile -t serials < <(adb devices | awk 'NR > 1 && $2 == "device" { print $1 }')
fi
[[ ${#serials[@]} -gt 0 ]] || { echo "no phones found by adb" >&2; exit 1; }

# One phone can be listed twice (USB and Wi-Fi); set each up once.
declare -A done_ids=()
for s in "${serials[@]}"; do
    id=$(adb -s "$s" shell getprop ro.serialno | tr -d '\r')
    [[ -n ${done_ids[$id]:-} ]] && continue
    done_ids[$id]=1
    model=$(adb -s "$s" shell getprop ro.product.model | tr -d '\r')
    sdk=$(adb -s "$s" shell getprop ro.build.version.sdk | tr -d '\r')
    echo "== $model ($s, Android SDK $sdk)"
    if (( sdk < 30 )); then
        echo "   skipped: wireless debugging needs Android 11 or newer"
        continue
    fi
    adb -s "$s" install -r "$APK" >/dev/null
    adb -s "$s" shell pm grant $PKG android.permission.WRITE_SECURE_SETTINGS
    if (( sdk >= 33 )); then
        adb -s "$s" shell pm grant $PKG android.permission.POST_NOTIFICATIONS || true
    fi
    # Keep it running in the background and through Doze.
    adb -s "$s" shell dumpsys deviceidle whitelist +$PKG >/dev/null || true
    adb -s "$s" shell cmd appops set $PKG RUN_ANY_IN_BACKGROUND allow || true
    adb -s "$s" shell am broadcast -a $PKG.START -n $PKG/.BootReceiver >/dev/null
    echo "   installed, permission granted, running"
done
echo
echo "Each phone shows a \"Wireless Debugging Keeper\" notification. Wireless debugging must have been"
echo "turned on by hand once on each Wi-Fi network with \"Always allow on this network\" ticked."
