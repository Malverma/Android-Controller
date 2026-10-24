package org.androidcontroller.keeper;

import android.app.Activity;
import android.content.Intent;
import android.media.projection.MediaProjectionManager;
import android.os.Bundle;

/** Invisible screen that only shows Android's "Start capturing?" dialog. */
public final class ProjectionActivity extends Activity {
    private static final int REQUEST = 1;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        MediaProjectionManager m = getSystemService(MediaProjectionManager.class);
        if (m == null) {
            finish();
            return;
        }
        try {
            startActivityForResult(m.createScreenCaptureIntent(), REQUEST);
        } catch (RuntimeException e) {
            finish();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        if (requestCode == REQUEST && resultCode == RESULT_OK && data != null) {
            Projection.granted(this, resultCode, data);
        }
        finish();
    }
}
