package org.androidcontroller.keeper;

import android.content.Context;
import android.os.PowerManager;

/** Waking the screen and keeping it on, without ADB. */
final class Screen {
    private static PowerManager.WakeLock keepOn;
    private static boolean wanted;

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

    /**
     * "Keep screen on" from the PC, while it is connected. Two holds, since
     * phone makers treat them differently: a 1-pixel window flagged
     * FLAG_KEEP_SCREEN_ON from the touch-control service (honored everywhere,
     * the way video apps keep the screen on), and a screen wake lock, which
     * some phones (LG on Android 9) let time out from a background app.
     */
    @SuppressWarnings("deprecation")
    static synchronized void keepOn(Context c, boolean on) {
        wanted = on;
        if (on && keepOn == null) {
            PowerManager pm = c.getSystemService(PowerManager.class);
            if (pm != null) {
                keepOn = pm.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK | PowerManager.ACQUIRE_CAUSES_WAKEUP,
                        "keeper:stay-awake");
                keepOn.acquire();
            }
        } else if (!on && keepOn != null) {
            keepOn.release();
            keepOn = null;
        }
        InputService input = InputService.get();
        if (input != null) input.keepScreenOn(on);
    }

    /** Whether the PC asked for the screen to stay on. */
    static synchronized boolean keptOn() {
        return wanted;
    }
}
