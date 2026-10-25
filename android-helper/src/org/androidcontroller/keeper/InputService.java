package org.androidcontroller.keeper;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.graphics.PixelFormat;
import android.graphics.Path;
import android.media.AudioManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
import android.util.Log;
import android.view.View;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Touch, keys and text from the PC, through Android's accessibility API (the
 * only input path an app has without ADB or root). The user turns it on once
 * under Settings › Accessibility.
 *
 * <p>A finger drag is replayed live as a chain of continued strokes
 * (Android 8+); if Android cancels one, the rest of the drag is replayed in
 * one piece when the finger lifts. With a second finger (pinch, two-finger
 * swipe) every finger is recorded and the whole gesture is replayed, all
 * fingers together, when the last one lifts.
 */
public final class InputService extends AccessibilityService {
    private static volatile InputService instance;
    private final Handler main = new Handler(Looper.getMainLooper());

    // Stroke state, touched on the main thread only.
    private boolean down;
    private float lastX, lastY;
    private long segmentStart;
    private GestureDescription.StrokeDescription stroke;
    private boolean inFlight;
    private boolean upPending;
    private boolean live;
    private final List<float[]> pending = new ArrayList<>();
    /** Whole-stroke fallback: every point since the finger went down (ms after {@link #downAt}). */
    private final List<float[]> all = new ArrayList<>();
    private long downAt;
    // Multi-touch: the first finger's pointer id, and every finger's points.
    private long primary = -1;
    private boolean multi;
    private final Map<Long, List<float[]>> fingers = new LinkedHashMap<>();
    private final Set<Long> lifted = new HashSet<>();
    // Wheel scrolling, accumulated briefly so one flick is one swipe.
    private float scrollV, scrollH, scrollX, scrollY;
    private int scrollW, scrollH0;
    private boolean scrollQueued;
    /** The invisible window that keeps the screen on; main thread only. */
    private View keepOnView;

    static InputService get() {
        return instance;
    }

    static boolean enabled(Context c) {
        String list = Settings.Secure.getString(c.getContentResolver(), Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES);
        return list != null && list.contains(c.getPackageName() + "/");
    }

    @Override
    protected void onServiceConnected() {
        instance = this;
        Log.i(Direct.TAG, "touch control on");
        Dev.serve(this);
        // Turned on while the PC already wants the screen kept on.
        if (Screen.keptOn()) keepScreenOn(true);
    }

    @Override
    public void onDestroy() {
        if (instance == this) instance = null;
        Dev.stop();
        removeKeepOn();
        super.onDestroy();
    }

    @Override
    public boolean onUnbind(android.content.Intent intent) {
        if (instance == this) instance = null;
        Dev.stop();
        main.post(this::removeKeepOn);
        return super.onUnbind(intent);
    }

    /** Shows or removes a 1-pixel, untouchable window with FLAG_KEEP_SCREEN_ON. */
    void keepScreenOn(boolean on) {
        main.post(() -> {
            if (!on) {
                removeKeepOn();
                return;
            }
            if (keepOnView != null) return;
            WindowManager wm = getSystemService(WindowManager.class);
            if (wm == null) return;
            WindowManager.LayoutParams lp = new WindowManager.LayoutParams(1, 1,
                    WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
                    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE | WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE
                            | WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON | WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED,
                    PixelFormat.TRANSLUCENT);
            lp.alpha = 0f;
            View v = new View(this);
            try {
                wm.addView(v, lp);
                keepOnView = v;
            } catch (RuntimeException e) {
                Log.w(Direct.TAG, "keep screen on window: " + e);
            }
        });
    }

    private void removeKeepOn() {
        if (keepOnView == null) return;
        WindowManager wm = getSystemService(WindowManager.class);
        try {
            if (wm != null) wm.removeView(keepOnView);
        } catch (RuntimeException ignored) {
            // already gone with the service
        }
        keepOnView = null;
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent event) {
        Dev.onEvent(this, event);
    }

    @Override
    public void onInterrupt() {}

    // ---- touch ----------------------------------------------------------------------

    /** action: 0 down, 1 up, 2 move, 3 cancel (MotionEvent), per pointer. Real screen pixels. */
    void touch(int action, long pointer, float x, float y) {
        if (!Keeper.isAtLeast(24)) return; // gestures need Android 7
        main.post(() -> onPointer(action, pointer, Math.max(0, x), Math.max(0, y)));
    }

    private void onPointer(int action, long id, float x, float y) {
        if (multi) {
            float t = SystemClock.uptimeMillis() - downAt;
            List<float[]> f = fingers.get(id);
            if (action == 0) {
                if (f != null || fingers.size() >= GestureDescription.getMaxStrokeCount()) return;
                f = new ArrayList<>();
                fingers.put(id, f);
            }
            if (f == null || lifted.contains(id)) return;
            f.add(new float[] {x, y, t});
            if (action == 1 || action == 3) {
                lifted.add(id);
                if (lifted.size() == fingers.size()) replayFingers();
            }
            return;
        }
        if (action == 0 && down && id != primary) {
            // A second finger: stop the live replay and record them all.
            multi = true;
            live = false;
            stroke = null;
            pending.clear();
            upPending = false;
            fingers.clear();
            lifted.clear();
            fingers.put(primary, new ArrayList<>(all));
            List<float[]> f = new ArrayList<>();
            f.add(new float[] {x, y, SystemClock.uptimeMillis() - downAt});
            fingers.put(id, f);
            all.clear();
            return;
        }
        if (down && id != primary) return;
        if (action == 0) primary = id;
        onTouch(action, x, y);
    }

    /** Every finger at once, each starting and lasting as it was drawn. */
    private void replayFingers() {
        long max = GestureDescription.getMaxGestureDuration();
        GestureDescription.Builder b = new GestureDescription.Builder();
        for (List<float[]> f : fingers.values()) {
            float[] first = f.get(0), last = f.get(f.size() - 1);
            Path p = new Path();
            p.moveTo(first[0], first[1]);
            for (int i = 1; i < f.size(); i++) p.lineTo(f.get(i)[0], f.get(i)[1]);
            long start = Math.max(0, Math.min((long) first[2], max - 1));
            long duration = Math.max(1, Math.min((long) (last[2] - first[2]), max - start));
            b.addStroke(new GestureDescription.StrokeDescription(p, start, duration));
        }
        multi = false;
        fingers.clear();
        lifted.clear();
        down = false;
        primary = -1;
        try {
            dispatchGesture(b.build(), null, main);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "multi-touch gesture failed", e);
        }
    }

    private void onTouch(int action, float x, float y) {
        long now = SystemClock.uptimeMillis();
        switch (action) {
            case 0:
                if (down) return;
                down = true;
                upPending = false;
                pending.clear();
                all.clear();
                downAt = now;
                all.add(new float[] {x, y, 0});
                lastX = x;
                lastY = y;
                live = Keeper.isAtLeast(26);
                if (live) {
                    Path p = new Path();
                    p.moveTo(x, y);
                    segmentStart = now;
                    stroke = new GestureDescription.StrokeDescription(p, 0, 1, true);
                    dispatch(stroke);
                }
                break;
            case 2:
                if (!down) return;
                all.add(new float[] {x, y, now - downAt});
                if (live) {
                    pending.add(new float[] {x, y});
                    if (!inFlight) flush();
                }
                break;
            case 1:
            case 3:
                if (!down) return;
                all.add(new float[] {x, y, now - downAt});
                if (live) {
                    pending.add(new float[] {x, y});
                    upPending = true;
                    if (!inFlight) flush();
                } else {
                    down = false;
                    replay();
                }
                break;
            default:
                break;
        }
    }

    /** Sends the points gathered since the last segment as its continuation. */
    private void flush() {
        if (stroke == null || (pending.isEmpty() && !upPending)) return;
        Path p = new Path();
        p.moveTo(lastX, lastY);
        for (float[] pt : pending) p.lineTo(pt[0], pt[1]);
        if (!pending.isEmpty()) {
            float[] end = pending.get(pending.size() - 1);
            lastX = end[0];
            lastY = end[1];
        }
        pending.clear();
        long now = SystemClock.uptimeMillis();
        long duration = Math.max(8, Math.min(100, now - segmentStart));
        segmentStart = now;
        boolean more = !upPending;
        stroke = continueStroke(stroke, p, duration, more);
        if (stroke == null) {
            live = false;
            if (!more) {
                finishLive();
                replay();
            }
            return;
        }
        dispatch(stroke);
        if (!more) finishLive();
    }

    private void finishLive() {
        down = false;
        upPending = false;
        stroke = null;
    }

    private static GestureDescription.StrokeDescription continueStroke(
            GestureDescription.StrokeDescription s, Path p, long duration, boolean more) {
        try {
            return s.continueStroke(p, 0, duration, more);
        } catch (RuntimeException e) {
            return null;
        }
    }

    private void dispatch(GestureDescription.StrokeDescription s) {
        GestureDescription g;
        try {
            g = new GestureDescription.Builder().addStroke(s).build();
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "bad gesture", e);
            return;
        }
        inFlight = true;
        boolean ok = dispatchGesture(g, new GestureResultCallback() {
            @Override
            public void onCompleted(GestureDescription gd) {
                inFlight = false;
                if (live && (down || upPending)) flush();
            }

            @Override
            public void onCancelled(GestureDescription gd) {
                inFlight = false;
                if (!live) return;
                // Android dropped the live chain: replay what is left in one go.
                live = false;
                stroke = null;
                if (upPending) {
                    down = false;
                    upPending = false;
                    replay();
                }
            }
        }, main);
        if (!ok) {
            inFlight = false;
            live = false;
        }
    }

    /** The whole stroke at once, timed as it was drawn. */
    private void replay() {
        if (all.isEmpty()) return;
        float[] first = all.get(0);
        float[] last = all.get(all.size() - 1);
        Path p = new Path();
        p.moveTo(first[0], first[1]);
        for (int i = 1; i < all.size(); i++) p.lineTo(all.get(i)[0], all.get(i)[1]);
        long duration = Math.max(1, Math.min((long) last[2], GestureDescription.getMaxGestureDuration()));
        all.clear();
        try {
            dispatchGesture(new GestureDescription.Builder()
                    .addStroke(new GestureDescription.StrokeDescription(p, 0, duration)).build(), null, main);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "gesture failed", e);
        }
    }

    /** Mouse wheel: a short swipe per flick. vscroll > 0 scrolls up. */
    void scroll(float x, float y, float h, float v, int screenW, int screenH) {
        if (!Keeper.isAtLeast(24)) return;
        main.post(() -> {
            scrollV += v;
            scrollH += h;
            scrollX = x;
            scrollY = y;
            scrollW = screenW;
            scrollH0 = screenH;
            if (!scrollQueued) {
                scrollQueued = true;
                main.postDelayed(this::doScroll, 60);
            }
        });
    }

    private void doScroll() {
        scrollQueued = false;
        float v = scrollV, h = scrollH;
        scrollV = 0;
        scrollH = 0;
        if (down || (v == 0 && h == 0)) return;
        float stepY = scrollH0 * 0.12f, stepX = scrollW * 0.12f;
        float dy = Math.max(-scrollH0 * 0.45f, Math.min(scrollH0 * 0.45f, v * stepY));
        float dx = Math.max(-scrollW * 0.45f, Math.min(scrollW * 0.45f, -h * stepX));
        float sx = clamp(scrollX, 1, scrollW - 2), sy = clamp(scrollY, 1, scrollH0 - 2);
        float ex = clamp(sx + dx, 1, scrollW - 2), ey = clamp(sy + dy, 1, scrollH0 - 2);
        Path p = new Path();
        p.moveTo(sx, sy);
        p.lineTo(ex, ey);
        try {
            dispatchGesture(new GestureDescription.Builder()
                    .addStroke(new GestureDescription.StrokeDescription(p, 0, 150)).build(), null, main);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "scroll failed", e);
        }
    }

    private static float clamp(float v, float lo, float hi) {
        return Math.max(lo, Math.min(hi, v));
    }

    // ---- keys and text ------------------------------------------------------------

    /** A key press (sent on key down). */
    void key(int keycode, int metastate) {
        main.post(() -> onKey(keycode, metastate));
    }

    private void onKey(int code, int meta) {
        switch (code) {
            case 3: // HOME
                performGlobalAction(GLOBAL_ACTION_HOME);
                break;
            case 4: // BACK
            case 111: // ESCAPE
                performGlobalAction(GLOBAL_ACTION_BACK);
                break;
            case 187: // APP_SWITCH
                performGlobalAction(GLOBAL_ACTION_RECENTS);
                break;
            case 26: // POWER
            case 223: // SLEEP
                if (code == 26 && !Screen.isOn(this)) {
                    Screen.wake(this);
                } else if (Keeper.isAtLeast(28)) {
                    performGlobalAction(GLOBAL_ACTION_LOCK_SCREEN);
                }
                break;
            case 224: // WAKEUP
                Screen.wake(this);
                break;
            case 24: // VOLUME_UP
            case 25: // VOLUME_DOWN
            case 164: // VOLUME_MUTE
                volume(code);
                break;
            case 67: // DEL
                edit(-1, null);
                break;
            case 112: // FORWARD_DEL
                edit(1, null);
                break;
            case 66: // ENTER
            case 160: // NUMPAD_ENTER
                enter();
                break;
            case 61: // TAB
                edit(0, "\t");
                break;
            case 21: // DPAD_LEFT
            case 22: // DPAD_RIGHT
                moveCursor(code == 21 ? -1 : 1);
                break;
            case 122: // MOVE_HOME
            case 123: // MOVE_END
                moveCursor(code == 122 ? Integer.MIN_VALUE : Integer.MAX_VALUE);
                break;
            case 82: // MENU
                break;
            default:
                String ch = printable(code, meta);
                if (ch != null) edit(0, ch);
                break;
        }
    }

    /** Letters, digits and space for keys that arrive as key codes. */
    private static String printable(int code, int meta) {
        boolean shift = (meta & 1) != 0;
        if (code >= 29 && code <= 54) {
            char c = (char) ('a' + code - 29);
            return String.valueOf(shift ? Character.toUpperCase(c) : c);
        }
        if (code >= 7 && code <= 16) return String.valueOf((char) ('0' + code - 7));
        if (code == 62) return " ";
        return null;
    }

    private void volume(int code) {
        AudioManager am = getSystemService(AudioManager.class);
        if (am == null) return;
        int dir = code == 24 ? AudioManager.ADJUST_RAISE : code == 25 ? AudioManager.ADJUST_LOWER : AudioManager.ADJUST_TOGGLE_MUTE;
        am.adjustSuggestedStreamVolume(dir, AudioManager.USE_DEFAULT_STREAM_TYPE, AudioManager.FLAG_SHOW_UI);
    }

    void text(String s) {
        main.post(() -> edit(0, s));
    }

    void globalAction(int action) {
        main.post(() -> performGlobalAction(action));
    }

    void setClipboard(String text, boolean paste) {
        main.post(() -> {
            ClipboardManager cm = getSystemService(ClipboardManager.class);
            Clip.fromPc(text);
            if (cm != null) cm.setPrimaryClip(ClipData.newPlainText("Android Controller", text));
            if (paste) {
                AccessibilityNodeInfo n = focused();
                if (n != null) n.performAction(AccessibilityNodeInfo.ACTION_PASTE);
            }
        });
    }

    /** A text field has input focus (typing would land somewhere). */
    boolean hasFocusedField() {
        return focused() != null;
    }

    private AccessibilityNodeInfo focused() {
        AccessibilityNodeInfo root = getRootInActiveWindow();
        if (root == null) return null;
        AccessibilityNodeInfo n = root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        return n != null && n.isEditable() ? n : null;
    }

    private static CharSequence currentText(AccessibilityNodeInfo n) {
        if (Keeper.isAtLeast(26) && n.isShowingHintText()) return "";
        CharSequence t = n.getText();
        return t == null ? "" : t;
    }

    /**
     * Inserts {@code insert} at the cursor (replacing a selection), or deletes
     * one character before ({@code dir < 0}) or after ({@code dir > 0}) it.
     */
    private void edit(int dir, String insert) {
        AccessibilityNodeInfo n = focused();
        if (n == null) return;
        String cur = currentText(n).toString();
        int s = n.getTextSelectionStart(), e = n.getTextSelectionEnd();
        if (s < 0 || e < 0 || s > cur.length() || e > cur.length()) {
            s = cur.length();
            e = s;
        }
        if (s > e) {
            int t = s;
            s = e;
            e = t;
        }
        String next;
        int cursor;
        if (insert != null) {
            next = cur.substring(0, s) + insert + cur.substring(e);
            cursor = s + insert.length();
        } else if (s != e) {
            next = cur.substring(0, s) + cur.substring(e);
            cursor = s;
        } else if (dir < 0) {
            if (s == 0) return;
            int from = cur.offsetByCodePoints(s, -1);
            next = cur.substring(0, from) + cur.substring(s);
            cursor = from;
        } else {
            if (s >= cur.length()) return;
            int to = cur.offsetByCodePoints(s, 1);
            next = cur.substring(0, s) + cur.substring(to);
            cursor = s;
        }
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next);
        if (n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) select(n, cursor);
    }

    private static void select(AccessibilityNodeInfo n, int at) {
        Bundle sel = new Bundle();
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, at);
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, at);
        n.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel);
    }

    private void moveCursor(int by) {
        AccessibilityNodeInfo n = focused();
        if (n == null) return;
        int len = currentText(n).length();
        int s = n.getTextSelectionEnd();
        if (s < 0) s = len;
        long to = by == Integer.MIN_VALUE ? 0 : by == Integer.MAX_VALUE ? len : (long) s + by;
        select(n, (int) Math.max(0, Math.min(len, to)));
    }

    private void enter() {
        AccessibilityNodeInfo n = focused();
        if (n == null) return;
        if (Keeper.isAtLeast(30)) {
            // AccessibilityAction.ACTION_IME_ENTER: "send" / "search" / next line.
            if (n.performAction(0x01020036)) return;
        }
        if (n.isMultiLine()) edit(0, "\n");
    }
}
