package org.androidcontroller.keeper;

import android.content.Context;
import android.os.PowerManager;

/** Waking the screen and keeping it on, without ADB. */
final class Screen {
    private static PowerManager.WakeLock keepOn;

    private Screen() {}

    static boolean isOn(Context c) {
        PowerManager pm = c.getSystemService(PowerManager.class);
        return pm == null || pm.isInteractive();
    }

    /** Turns the screen on (it stays on for the user's normal timeout). */
    @SuppressWarnings("deprecation")
    static void wake(Context c) {
        PowerManager pm = c.getSystemService(PowerManager.class);
        if (pm == null || pm.isInteractive()) return;
        PowerManager.WakeLock wl = pm.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK
                | PowerManager.ACQUIRE_CAUSES_WAKEUP | PowerManager.ON_AFTER_RELEASE, "keeper:wake");
        wl.acquire(3_000);
    }

    /** "Keep screen on" from the PC; held until switched off or the app stops. */
    @SuppressWarnings("deprecation")
    static synchronized void keepOn(Context c, boolean on) {
        if (on && keepOn == null) {
            PowerManager pm = c.getSystemService(PowerManager.class);
            if (pm == null) return;
            keepOn = pm.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK | PowerManager.ACQUIRE_CAUSES_WAKEUP,
                    "keeper:stay-awake");
            keepOn.acquire();
        } else if (!on && keepOn != null) {
            keepOn.release();
            keepOn = null;
        }
    }

    static synchronized boolean keptOn() {
        return keepOn != null;
    }
}
