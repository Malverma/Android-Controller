package org.androidcontroller.keeper;

import android.content.Context;
import android.content.Intent;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionManager;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;

/**
 * The screen-capture permission. Android asks the user once ("Start now";
 * up to Android 9 with a "Don't show again" box) and the grant lasts until
 * the app stops or the user ends the capture.
 */
final class Projection {
    private static MediaProjection projection;

    private Projection() {}

    static synchronized MediaProjection get() {
        return projection;
    }

    static synchronized void granted(Context c, int resultCode, Intent data) {
        MediaProjectionManager m = c.getSystemService(MediaProjectionManager.class);
        if (m == null) return;
        MediaProjection p = m.getMediaProjection(resultCode, data);
        if (p == null) return;
        if (projection != null) projection.stop();
        projection = p;
        p.registerCallback(new MediaProjection.Callback() {
            @Override
            public void onStop() {
                Log.i(Direct.TAG, "screen capture stopped");
                synchronized (Projection.class) {
                    if (projection == p) projection = null;
                }
            }
        }, new Handler(Looper.getMainLooper()));
        Log.i(Direct.TAG, "screen capture allowed");
    }

    /** Android 14+ gives one capture session per grant. */
    static synchronized void used() {
        if (Keeper.isAtLeast(34) && projection != null) {
            projection.stop();
            projection = null;
        }
    }

    /** Opens the consent screen. */
    static void ask(Context c) {
        Intent i = new Intent(c, ProjectionActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            c.startActivity(i);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "cannot ask for screen capture", e);
        }
    }
}
