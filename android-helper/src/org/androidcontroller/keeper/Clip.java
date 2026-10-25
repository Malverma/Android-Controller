package org.androidcontroller.keeper;

import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;

/**
 * The phone's clipboard for the PC (helper 0.6), sent in the heartbeat.
 * Up to Android 9 a listener sees every copy. From Android 10 only the app
 * in front may read the clipboard, so the notification's "Send clipboard to
 * PC" opens {@link ClipActivity} for a moment to read it.
 */
final class Clip {
    private static volatile String text;
    private static volatile long seq;
    private static ClipboardManager.OnPrimaryClipChangedListener listener;

    private Clip() {}

    static String text() {
        return text;
    }

    static long seq() {
        return seq;
    }

    /** What the PC put there itself is not sent back. */
    private static volatile String fromPc;

    static void fromPc(String t) {
        fromPc = t;
    }

    static void listen(Context c) {
        ClipboardManager cm = c.getSystemService(ClipboardManager.class);
        if (cm == null || listener != null) return;
        listener = () -> read(c);
        cm.addPrimaryClipChangedListener(listener);
    }

    static void stop(Context c) {
        ClipboardManager cm = c.getSystemService(ClipboardManager.class);
        if (cm != null && listener != null) cm.removePrimaryClipChangedListener(listener);
        listener = null;
    }

    /** Reads the clipboard now; true when there was text. */
    static boolean read(Context c) {
        ClipboardManager cm = c.getSystemService(ClipboardManager.class);
        if (cm == null) return false;
        ClipData d;
        try {
            d = cm.getPrimaryClip();
        } catch (RuntimeException e) {
            return false;
        }
        if (d == null || d.getItemCount() == 0) return false;
        CharSequence t = d.getItemAt(0).coerceToText(c);
        if (t == null || t.length() == 0) return false;
        String s = t.toString();
        if (s.length() > 200_000) s = s.substring(0, 200_000);
        if (s.equals(fromPc)) return true;
        text = s;
        seq++;
        return true;
    }
}
