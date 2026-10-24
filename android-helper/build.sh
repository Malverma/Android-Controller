#!/usr/bin/env bash
# Builds build/wireless-debugging-keeper.apk with the plain SDK tools (no Gradle).
# Needs: a JDK (17+), and in the Android SDK: build-tools 35 and platforms;android-35.
set -euo pipefail
cd "$(dirname "$0")"

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
BT="$SDK/build-tools/${BUILD_TOOLS:-35.0.0}"
JAR="$SDK/platforms/android-${PLATFORM:-35}/android.jar"
for f in "$BT/aapt2" "$BT/d8" "$BT/zipalign" "$BT/apksigner" "$JAR"; do
    [[ -e $f ]] || { echo "missing $f (sdkmanager \"build-tools;35.0.0\" \"platforms;android-35\")" >&2; exit 1; }
done
command -v javac >/dev/null || { echo "javac not found: install a JDK or set PATH" >&2; exit 1; }

# The standard debug key, so updates install over older builds.
KEYSTORE="${KEYSTORE:-$HOME/.android/debug.keystore}"
if [[ ! -f $KEYSTORE ]]; then
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android -alias androiddebugkey \
        -dname "CN=Android Debug,O=Android,C=US" -keyalg RSA -keysize 2048 -validity 10000 >/dev/null
fi

rm -rf build && mkdir -p build/classes build/dex
"$BT/aapt2" link -o build/unsigned.apk -I "$JAR" --manifest AndroidManifest.xml
javac --release 11 -classpath "$JAR" -d build/classes $(find src -name '*.java')
"$BT/d8" --min-api 30 --lib "$JAR" --output build/dex $(find build/classes -name '*.class')
(cd build/dex && zip -q -j ../unsigned.apk classes.dex)
"$BT/zipalign" -f -p 4 build/unsigned.apk build/aligned.apk
"$BT/apksigner" sign --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
    --out build/wireless-debugging-keeper.apk build/aligned.apk
rm -f build/unsigned.apk build/aligned.apk build/*.idsig
echo "built $(pwd)/build/wireless-debugging-keeper.apk"
