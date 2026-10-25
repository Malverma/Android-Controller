package org.androidcontroller.keeper;

import android.app.ActivityManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.net.Uri;
import android.provider.Settings;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.RandomAccessFile;
import java.util.ArrayList;
import java.util.List;

/**
 * App testing from the PC without ADB (helper 0.6): the user's apps, and
 * launch / stop / clear / uninstall as far as Android lets an ordinary app
 * go. Also the phone's own measurements for the PC's graphs.
 */
final class Apps {
    private Apps() {}

    static int versionCode(Context c) {
        try {
            PackageInfo p = c.getPackageManager().getPackageInfo(c.getPackageName(), 0);
            return p.versionCode;
        } catch (PackageManager.NameNotFoundException e) {
            return 0;
        }
    }

    /** Video formats a hardware encoder handles, as the PC names them. */
    static JSONArray codecs() {
        JSONArray a = new JSONArray();
        a.put("h264");
        if (hardwareEncoder(MediaFormat.MIMETYPE_VIDEO_HEVC) != null) a.put("h265");
        return a;
    }

    /** The name of a hardware encoder for {@code mime}, or null. */
    static String hardwareEncoder(String mime) {
        try {
            for (MediaCodecInfo info : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
                if (!info.isEncoder()) continue;
                boolean hw = Keeper.isAtLeast(29) ? info.isHardwareAccelerated() : !isSoftwareName(info.getName());
                if (!hw) continue;
                for (String t : info.getSupportedTypes()) {
                    if (t.equalsIgnoreCase(mime)) return info.getName();
                }
            }
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "listing encoders", e);
        }
        return null;
    }

    private static boolean isSoftwareName(String n) {
        String l = n.toLowerCase(java.util.Locale.ROOT);
        return l.startsWith("omx.google.") || l.startsWith("c2.android.") || l.contains(".sw.");
    }

    static JSONObject packages(Context c) {
        JSONArray a = new JSONArray();
        for (ApplicationInfo ai : c.getPackageManager().getInstalledApplications(0)) {
            boolean user = (ai.flags & ApplicationInfo.FLAG_SYSTEM) == 0 || (ai.flags & ApplicationInfo.FLAG_UPDATED_SYSTEM_APP) != 0;
            if (user && !ai.packageName.equals(c.getPackageName())) a.put(ai.packageName);
        }
        return Proto.obj("op", "packages", "packages", a);
    }

    static JSONObject act(Context c, String action, String pkg) {
        PackageManager pm = c.getPackageManager();
        try {
            pm.getApplicationInfo(pkg, 0);
        } catch (PackageManager.NameNotFoundException e) {
            return result(false, "The app is not installed");
        }
        switch (action) {
            case "launch": {
                Intent i = pm.getLaunchIntentForPackage(pkg);
                if (i == null) return result(false, "The app has no launcher icon");
                i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
                c.startActivity(i);
                return result(true, "Launched");
            }
            case "stop": {
                // Only ADB can force-stop an app on screen: send it to the
                // background first, then end it.
                InputService in = InputService.get();
                if (in != null) in.globalAction(android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_HOME);
                try {
                    Thread.sleep(600);
                } catch (InterruptedException ignored) {
                    // go on
                }
                ActivityManager am = c.getSystemService(ActivityManager.class);
                if (am != null) am.killBackgroundProcesses(pkg);
                return result(true, in != null ? "Stopped" : "Ended in the background (turn on touch control to also leave it)");
            }
            case "clear": {
                // No app may wipe another's data: open its page for the user.
                Intent i = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:" + pkg));
                i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                c.startActivity(i);
                return result(false, "Without ADB, tap Storage › Clear data on the phone (its page is open)");
            }
            case "uninstall": {
                Intent i = new Intent(Intent.ACTION_DELETE, Uri.parse("package:" + pkg));
                i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                c.startActivity(i);
                return result(true, "Confirm the uninstall on the phone");
            }
            default:
                return result(false, "Unknown action " + action);
        }
    }

    private static JSONObject result(boolean ok, String message) {
        return Proto.obj("op", "app", "ok", ok, "message", message);
    }

    // ---- measurements -----------------------------------------------------------------

    private static long[] lastCpu;

    /** Fills memory, battery temperature and (where readable) CPU use. */
    static void perf(Context c, JSONObject o, android.content.Intent battery) throws org.json.JSONException {
        ActivityManager am = c.getSystemService(ActivityManager.class);
        if (am != null) {
            ActivityManager.MemoryInfo mi = new ActivityManager.MemoryInfo();
            am.getMemoryInfo(mi);
            o.put("mem_total_mb", mi.totalMem >> 20);
            o.put("mem_used_mb", (mi.totalMem - mi.availMem) >> 20);
        }
        if (battery != null) {
            int t = battery.getIntExtra(android.os.BatteryManager.EXTRA_TEMPERATURE, Integer.MIN_VALUE);
            if (t != Integer.MIN_VALUE) o.put("battery_temp", t / 10.0);
        }
        Double cpu = cpu();
        if (cpu != null) o.put("cpu", cpu);
    }

    /** Busy share since the last call; null where /proc/stat is closed (Android 8+). */
    private static synchronized Double cpu() {
        try (RandomAccessFile f = new RandomAccessFile("/proc/stat", "r")) {
            String[] w = f.readLine().trim().split("\\s+");
            List<Long> v = new ArrayList<>();
            for (int i = 1; i < w.length; i++) v.add(Long.parseLong(w[i]));
            long total = 0;
            for (long x : v) total += x;
            long idle = v.get(3) + (v.size() > 4 ? v.get(4) : 0);
            long[] now = {total - idle, total};
            long[] before = lastCpu;
            lastCpu = now;
            if (before == null || now[1] <= before[1]) return null;
            return Math.max(0, Math.min(100, 100.0 * (now[0] - before[0]) / (now[1] - before[1])));
        } catch (Exception e) {
            return null;
        }
    }
}
