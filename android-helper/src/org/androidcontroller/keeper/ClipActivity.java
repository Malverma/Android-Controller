package org.androidcontroller.keeper;

import android.app.Activity;
import android.widget.Toast;

/** Comes to the front invisibly, reads the clipboard for the PC, and goes (Android 10+). */
public final class ClipActivity extends Activity {
    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (!hasFocus) return;
        boolean ok = Clip.read(this);
        Toast.makeText(this, ok ? "Clipboard sent to the PC" : "The clipboard has no text", Toast.LENGTH_SHORT).show();
        finish();
        overridePendingTransition(0, 0);
    }
}
