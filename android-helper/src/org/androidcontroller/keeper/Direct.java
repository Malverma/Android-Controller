package org.androidcontroller.keeper;

import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.provider.Settings;
import android.text.TextUtils;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.Collections;
import java.util.Map;

/**
 * Settings and shared state of the no-ADB PC connection ("direct mode"):
 * the phone runs a small server that Android Controller connects to over
 * Wi-Fi, for phones that cannot use wireless debugging or adb tcpip.
 */
final class Direct {
    static final String TAG = "WDKeeper.direct";
    static final int PORT = 27190;
    static final String SERVICE_TYPE = "_acdirect._tcp";
    /** How long "Pair with PC" accepts a pairing. */
    static final long PAIR_WINDOW_MS = 120_000;

    private static final String PREFS = "direct";
    private static final String KEY_ENABLED = "enabled";
    private static final String KEY_STAY_AWAKE = "stay_awake";
    private static final String PCS = "direct_pcs";

    /** Open pairing window (process-wide). */
    private static String pairCode;
    private static long pairUntil;
    /** Shown on the screen after a pairing: who just paired. */
    static volatile String lastPaired;

    private Direct() {}

    private static SharedPreferences prefs(Context c) {
        return c.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    static boolean enabled(Context c) {
        return prefs(c).getBoolean(KEY_ENABLED, false);
    }

    static void setEnabled(Context c, boolean on) {
        prefs(c).edit().putBoolean(KEY_ENABLED, on).apply();
        if (on) {
            start(c);
        } else {
            c.stopService(new Intent(c, DirectService.class));
        }
    }

    static void start(Context c) {
        Intent i = new Intent(c, DirectService.class);
        try {
            if (Keeper.isAtLeast(26)) {
                c.startForegroundService(i);
            } else {
                c.startService(i);
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "cannot start the PC connection service", e);
        }
    }

    static boolean stayAwake(Context c) {
        return prefs(c).getBoolean(KEY_STAY_AWAKE, false);
    }

    static void setStayAwake(Context c, boolean on) {
        prefs(c).edit().putBoolean(KEY_STAY_AWAKE, on).apply();
    }

    /** Stable id for this phone: "direct:" + the app-scoped ANDROID_ID. */
    static String deviceId(Context c) {
        String aid = Settings.Secure.getString(c.getContentResolver(), Settings.Secure.ANDROID_ID);
        return "direct:" + (TextUtils.isEmpty(aid) ? "unknown" : aid);
    }

    static String model() {
        return Build.MODEL == null ? "Android phone" : Build.MODEL;
    }

    // ---- pairing window ---------------------------------------------------------

    static synchronized String openPairing() {
        pairCode = String.format("%06d", Crypto.randomInt(1_000_000));
        pairUntil = System.currentTimeMillis() + PAIR_WINDOW_MS;
        lastPaired = null;
        return pairCode;
    }

    static synchronized void closePairing() {
        pairCode = null;
        pairUntil = 0;
    }

    /** The code while the window is open, else null. */
    static synchronized String pairCode() {
        if (pairCode != null && System.currentTimeMillis() > pairUntil) pairCode = null;
        return pairCode;
    }

    static synchronized long pairSecondsLeft() {
        return pairCode() == null ? 0 : Math.max(0, (pairUntil - System.currentTimeMillis()) / 1000);
    }

    /** A wrong code: show a new one so it can't be guessed by trying. */
    static synchronized void rotatePairCode() {
        if (pairCode() != null) pairCode = String.format("%06d", Crypto.randomInt(1_000_000));
    }

    // ---- paired PCs ---------------------------------------------------------------

    static void addPc(Context c, String pcId, String name, byte[] key) {
        JSONObject o = new JSONObject();
        try {
            o.put("key", Crypto.hex(key));
            o.put("name", name);
            o.put("since", System.currentTimeMillis());
        } catch (JSONException e) {
            throw new IllegalStateException(e);
        }
        c.getSharedPreferences(PCS, Context.MODE_PRIVATE).edit().putString(pcId, o.toString()).apply();
    }

    /** The key shared with {@code pcId}, or null when it is not paired. */
    static byte[] pcKey(Context c, String pcId) {
        String s = c.getSharedPreferences(PCS, Context.MODE_PRIVATE).getString(pcId, null);
        if (s == null) return null;
        try {
            return Crypto.unhex(new JSONObject(s).getString("key"));
        } catch (JSONException | IllegalArgumentException e) {
            return null;
        }
    }

    static int pcCount(Context c) {
        return c.getSharedPreferences(PCS, Context.MODE_PRIVATE).getAll().size();
    }

    static String pcNames(Context c) {
        StringBuilder s = new StringBuilder();
        Map<String, ?> all = c.getSharedPreferences(PCS, Context.MODE_PRIVATE).getAll();
        for (Object v : all.values()) {
            try {
                if (s.length() > 0) s.append(", ");
                s.append(new JSONObject(String.valueOf(v)).optString("name", "PC"));
            } catch (JSONException ignored) {
                // skip a damaged entry
            }
        }
        return s.toString();
    }

    static void forgetPcs(Context c) {
        c.getSharedPreferences(PCS, Context.MODE_PRIVATE).edit().clear().apply();
    }

    // ---- network ------------------------------------------------------------------

    /** The phone's Wi-Fi IPv4 address, or null. */
    @SuppressWarnings("deprecation")
    static String wifiAddress(Context c) {
        WifiManager wm = (WifiManager) c.getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wm != null) {
            int ip = wm.getConnectionInfo() == null ? 0 : wm.getConnectionInfo().getIpAddress();
            if (ip != 0) {
                return (ip & 0xff) + "." + ((ip >> 8) & 0xff) + "." + ((ip >> 16) & 0xff) + "." + ((ip >> 24) & 0xff);
            }
        }
        try {
            for (NetworkInterface nif : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                if (!nif.getName().startsWith("wlan")) continue;
                for (InetAddress a : Collections.list(nif.getInetAddresses())) {
                    if (a instanceof Inet4Address && !a.isLoopbackAddress()) return a.getHostAddress();
                }
            }
        } catch (Exception ignored) {
            // no address
        }
        return null;
    }
}
