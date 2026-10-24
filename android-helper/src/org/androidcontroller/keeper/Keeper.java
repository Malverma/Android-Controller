package org.androidcontroller.keeper;

import android.Manifest;
import android.content.ContentResolver;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.provider.Settings;
import android.util.Log;

/** Settings access shared by the service, the boot receiver and the screen. */
final class Keeper {
    static final String TAG = "WDKeeper";
    /** Settings.Global.ADB_WIFI_ENABLED (hidden constant): the Wireless debugging switch. */
    static final String ADB_WIFI_ENABLED = "adb_wifi_enabled";
    static final String GRANT_COMMAND =
            "adb shell pm grant org.androidcontroller.keeper android.permission.WRITE_SECURE_SETTINGS";

    private static final String PREFS = "keeper";
    private static final String KEY_ACTIVE = "active";
    private static final String KEY_WATCHDOG = "wifi_watchdog";

    private Keeper() {}

    /** Wireless debugging exists from Android 11 (API 30). */
    static boolean hasWirelessDebugging() {
        return Build.VERSION.SDK_INT >= 30;
    }

    static boolean canWriteSettings(Context c) {
        return c.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) == PackageManager.PERMISSION_GRANTED;
    }

    static boolean wirelessDebuggingOn(Context c) {
        return Settings.Global.getInt(c.getContentResolver(), ADB_WIFI_ENABLED, 0) == 1;
    }

    static boolean usbDebuggingOn(Context c) {
        return Settings.Global.getInt(c.getContentResolver(), Settings.Global.ADB_ENABLED, 0) == 1;
    }

    /** Wireless debugging only runs on Wi-Fi, so there is nothing to do without it. */
    static boolean onWifi(Context c) {
        ConnectivityManager cm = c.getSystemService(ConnectivityManager.class);
        if (cm == null) return false;
        for (Network n : cm.getAllNetworks()) {
            NetworkCapabilities caps = cm.getNetworkCapabilities(n);
            if (caps != null && caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) return true;
        }
        return false;
    }

    /**
     * Turns wireless debugging on (and the developer "USB debugging" switch it
     * depends on). Returns false when the permission has not been granted.
     */
    static boolean turnOn(Context c) {
        if (!hasWirelessDebugging() || !canWriteSettings(c)) return false;
        ContentResolver r = c.getContentResolver();
        try {
            if (!usbDebuggingOn(c)) {
                Settings.Global.putInt(r, Settings.Global.ADB_ENABLED, 1);
            }
            if (!wirelessDebuggingOn(c)) {
                Log.i(TAG, "turning wireless debugging on");
                Settings.Global.putInt(r, ADB_WIFI_ENABLED, 1);
            }
            return true;
        } catch (SecurityException e) {
            Log.w(TAG, "cannot write settings", e);
            return false;
        }
    }

    static boolean active(Context c) {
        return prefs(c).getBoolean(KEY_ACTIVE, true);
    }

    static void setActive(Context c, boolean on) {
        prefs(c).edit().putBoolean(KEY_ACTIVE, on).apply();
        if (on) {
            start(c);
        } else {
            c.stopService(new Intent(c, KeeperService.class));
        }
    }

    static void start(Context c) {
        Intent i = new Intent(c, KeeperService.class);
        try {
            if (isAtLeast(26)) {
                c.startForegroundService(i);
            } else {
                c.startService(i);
            }
        } catch (IllegalStateException | SecurityException e) {
            Log.w(TAG, "cannot start the service", e);
        }
    }

    /** Whether to restart Wi-Fi when it stays disconnected (see KeeperService). */
    static boolean watchdog(Context c) {
        return prefs(c).getBoolean(KEY_WATCHDOG, true);
    }

    static void setWatchdog(Context c, boolean on) {
        prefs(c).edit().putBoolean(KEY_WATCHDOG, on).apply();
    }

    static boolean wifiSwitchedOn(Context c) {
        WifiManager wm = c.getSystemService(WifiManager.class);
        return wm != null && wm.isWifiEnabled();
    }

    /** Turns Wi-Fi off or on; only works because the app targets SDK 28. */
    @SuppressWarnings("deprecation")
    static boolean setWifi(Context c, boolean on) {
        WifiManager wm = c.getSystemService(WifiManager.class);
        try {
            return wm != null && wm.setWifiEnabled(on);
        } catch (SecurityException e) {
            Log.w(TAG, "cannot switch Wi-Fi", e);
            return false;
        }
    }

    private static SharedPreferences prefs(Context c) {
        return c.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    static boolean isAtLeast(int sdk) {
        return Build.VERSION.SDK_INT >= sdk;
    }
}
