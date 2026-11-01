package org.androidcontroller.keeper;

import android.app.Activity;
import android.content.pm.ActivityInfo;
import android.graphics.Color;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

/** The full screen picture of the PC. Touches go to the PC's pointer. */
public final class MonitorActivity extends Activity {
    private static final long EXIT_BAR_MS = 5_000;
    private static final int EXIT_FINGERS = 3;
    private static final long MIN_MOVE_GAP_MS = 8;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private TextView statusView;
    private Button exitBar;
    private final Runnable hideExitBar = () -> exitBar.setVisibility(View.GONE);
    /** Three fingers are down or were: this gesture is not for the PC. */
    private boolean gestureIsOurs;
    private long lastMoveSent;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        requestWindowFeature(Window.FEATURE_NO_TITLE);
        Window w = getWindow();
        w.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON | WindowManager.LayoutParams.FLAG_FULLSCREEN);
        if (Keeper.isAtLeast(28)) {
            WindowManager.LayoutParams lp = w.getAttributes();
            lp.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            w.setAttributes(lp);
        }
        setRequestedOrientation(Monitor.portrait() ? ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
                : ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE);

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);

        SurfaceView video = new SurfaceView(this);
        video.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder h) {
                Monitor.setSurface(h.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder h, int format, int width, int height) {
                Monitor.setSurface(h.getSurface());
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder h) {
                Monitor.setSurface(null);
            }
        });
        root.addView(video, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));

        statusView = new TextView(this);
        statusView.setTextColor(Color.WHITE);
        statusView.setTextSize(20);
        statusView.setGravity(Gravity.CENTER);
        root.addView(statusView, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.CENTER));

        exitBar = new Button(this);
        exitBar.setText("Exit Monitor");
        exitBar.setVisibility(View.GONE);
        exitBar.setOnClickListener(v -> Monitor.userToggle(this, false));
        root.addView(exitBar, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL));

        setContentView(root);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (!Monitor.active() && !Monitor.pcConnected()) {
            finish(); // reopened from recents after the session ended
            return;
        }
        hideSystemBars();
        Monitor.attach(this);
    }

    @Override
    protected void onPause() {
        Monitor.detach(this);
        super.onPause();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) hideSystemBars();
    }

    @SuppressWarnings("deprecation")
    private void hideSystemBars() {
        getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_STABLE | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN);
    }

    void onStatus(String text) {
        statusView.setText(text);
        statusView.setVisibility(text.isEmpty() ? View.GONE : View.VISIBLE);
    }

    @Override
    public void onBackPressed() {
        // A stray swipe must not close the display.
        Toast.makeText(this, "Tap with three fingers to exit", Toast.LENGTH_SHORT).show();
    }

    @Override
    public boolean onTouchEvent(MotionEvent e) {
        int action = e.getActionMasked();
        if (action == MotionEvent.ACTION_DOWN) gestureIsOurs = false;
        if (!gestureIsOurs && e.getPointerCount() >= EXIT_FINGERS) {
            gestureIsOurs = true;
            sendTouch("cancel", e);
            exitBar.setVisibility(View.VISIBLE);
            handler.removeCallbacks(hideExitBar);
            handler.postDelayed(hideExitBar, EXIT_BAR_MS);
            return true;
        }
        if (gestureIsOurs || Monitor.viewOnly()) return true;
        switch (action) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                sendTouch("down", e);
                break;
            case MotionEvent.ACTION_MOVE:
                long now = System.currentTimeMillis();
                if (now - lastMoveSent >= MIN_MOVE_GAP_MS) {
                    lastMoveSent = now;
                    sendTouch("move", e);
                }
                break;
            case MotionEvent.ACTION_UP:
                sendTouch("up", e);
                break;
            case MotionEvent.ACTION_POINTER_UP:
                sendTouch("pointer_up", e);
                break;
            case MotionEvent.ACTION_CANCEL:
                sendTouch("cancel", e);
                break;
            default:
                break;
        }
        return true;
    }

    /** Positions are 0..1 of this screen, so the PC needs no phone sizes. */
    private void sendTouch(String action, MotionEvent e) {
        View v = getWindow().getDecorView();
        float w = Math.max(1, v.getWidth()), h = Math.max(1, v.getHeight());
        try {
            JSONArray points = new JSONArray();
            for (int i = 0; i < e.getPointerCount(); i++) {
                JSONArray p = new JSONArray();
                p.put(e.getPointerId(i));
                p.put((double) (e.getX(i) / w));
                p.put((double) (e.getY(i) / h));
                points.put(p);
            }
            JSONObject o = Proto.obj("op", "touch", "a", action, "t", e.getEventTime(), "p", points);
            Monitor.sendTouch(o);
        } catch (JSONException ex) {
            throw new IllegalStateException(ex);
        }
    }
}
