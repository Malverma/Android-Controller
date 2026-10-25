package org.androidcontroller.keeper;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.app.Notification;
import android.content.Context;
import android.graphics.Path;
import android.graphics.Rect;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.util.ArrayDeque;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Automation for Android Controller's test tools (Claude through the MCP
 * server): the screen's elements in ~50 ms, taps and swipes, text, popups
 * and toasts as they appear, "Allow" on permission prompts, test settings.
 *
 * <p>Reached two ways with the same ops ({@code {"op": "tree"}} ...): the
 * no-ADB link (DirectService), and over ADB a local socket that only adb's
 * shell user may use (the PC runs {@code adb ... localabstract:android-controller-dev}).
 * Every answer is a JSON object; {@code error} says what went wrong, with
 * {@code need_input} when touch control (the accessibility service) is off.
 */
final class Dev {
    static final String SOCKET = "android-controller-dev";
    private static final int SHELL_UID = 2000;
    private static final int MAX_NODES = 3000;
    private static final int MAX_EVENTS = 300;
    /** Packages that show Android's runtime permission prompt. */
    private static final String[] PERMISSION_UI = {
        "com.google.android.permissioncontroller", "com.android.permissioncontroller", "com.android.packageinstaller",
    };
    /** Best first: "While using the app", "Allow", "Only this time". */
    private static final String[] ALLOW_IDS = {
        "permission_allow_foreground_only_button", "permission_allow_button", "permission_allow_one_time_button",
        "permission_allow_all_button", "permission_allow_selected_button",
    };

    private static final ArrayDeque<JSONObject> events = new ArrayDeque<>();
    private static long eventSeq;
    private static volatile boolean autoAllow;
    private static final Handler main = new Handler(Looper.getMainLooper());
    private static LocalServerSocket server;

    private Dev() {}

    /** True for the ops this class answers. */
    static boolean handles(String op) {
        switch (op) {
            case "tree":
            case "tap":
            case "swipe":
            case "text":
            case "key":
            case "events":
            case "auto_allow":
            case "settings":
            case "screen":
                return true;
            default:
                return false;
        }
    }

    static JSONObject handle(Context c, JSONObject req) {
        String op = req.optString("op");
        try {
            switch (op) {
                case "screen":
                    return screen(c);
                case "events":
                    return events(req.optLong("since", 0));
                case "auto_allow":
                    autoAllow = req.optBoolean("on", true);
                    return Proto.obj("op", op, "on", autoAllow);
                case "settings":
                    return settings(c, req);
                default:
                    break;
            }
            InputService in = InputService.get();
            if (in == null) {
                return Proto.obj("op", op, "error", "Touch control (the helper's accessibility service) is off", "need_input", true);
            }
            switch (op) {
                case "tree":
                    return tree(in);
                case "tap": {
                    int ms = Math.max(1, req.optInt("ms", 0));
                    float x = (float) req.optDouble("x"), y = (float) req.optDouble("y");
                    return gesture(in, op, line(x, y, x, y), ms < 2 ? 50 : ms);
                }
                case "swipe":
                    return gesture(in, op, line((float) req.optDouble("x1"), (float) req.optDouble("y1"),
                            (float) req.optDouble("x2"), (float) req.optDouble("y2")), Math.max(10, req.optInt("ms", 300)));
                case "text":
                    if (!in.hasFocusedField()) return Proto.obj("op", op, "error", "No text field has focus: tap one first");
                    in.text(req.optString("text"));
                    return Proto.obj("op", op, "ok", true);
                case "key":
                    in.key(req.optInt("code"), 0);
                    return Proto.obj("op", op, "ok", true);
                default:
                    return Proto.obj("op", op, "error", "unknown request");
            }
        } catch (RuntimeException e) {
            Log.w(Keeper.TAG, "dev op " + op, e);
            return Proto.obj("op", op, "error", String.valueOf(e));
        }
    }

    // ---- the screen ----------------------------------------------------------------

    /** The screen's size in pixels as it is turned now. */
    @SuppressWarnings("deprecation")
    static int[] screenSize(Context c) {
        WindowManager wm = c.getSystemService(WindowManager.class);
        if (wm == null) return null;
        DisplayMetrics m = new DisplayMetrics();
        wm.getDefaultDisplay().getRealMetrics(m);
        return new int[] {m.widthPixels, m.heightPixels};
    }

    private static JSONObject screen(Context c) {
        int[] s = screenSize(c);
        JSONObject o = Proto.obj("op", "screen");
        if (s != null) put(o, "screen", new JSONArray().put(s[0]).put(s[1]));
        return o;
    }

    /** Every visible element of every window, topmost window first. */
    private static JSONObject tree(InputService in) {
        JSONArray nodes = new JSONArray();
        String pkg = null;
        List<AccessibilityWindowInfo> windows = in.getWindows();
        if (windows != null && !windows.isEmpty()) {
            java.util.ArrayList<AccessibilityWindowInfo> sorted = new java.util.ArrayList<>(windows);
            sorted.sort((a, b) -> Integer.compare(b.getLayer(), a.getLayer()));
            for (AccessibilityWindowInfo w : sorted) {
                if (w.getType() == AccessibilityWindowInfo.TYPE_ACCESSIBILITY_OVERLAY) continue;
                AccessibilityNodeInfo root = w.getRoot();
                if (root == null) continue;
                if (pkg == null && w.getType() == AccessibilityWindowInfo.TYPE_APPLICATION && root.getPackageName() != null) {
                    pkg = root.getPackageName().toString();
                }
                walk(root, nodes, 0);
            }
        } else {
            AccessibilityNodeInfo root = in.getRootInActiveWindow();
            if (root != null) {
                if (root.getPackageName() != null) pkg = root.getPackageName().toString();
                walk(root, nodes, 0);
            }
        }
        JSONObject o = Proto.obj("op", "tree", "nodes", nodes);
        if (pkg != null) put(o, "pkg", pkg);
        int[] s = screenSize(in);
        if (s != null) put(o, "screen", new JSONArray().put(s[0]).put(s[1]));
        return o;
    }

    private static void walk(AccessibilityNodeInfo n, JSONArray out, int depth) {
        if (n == null || out.length() >= MAX_NODES || depth > 80) return;
        if (n.isVisibleToUser()) out.put(node(n));
        for (int i = 0; i < n.getChildCount(); i++) walk(n.getChild(i), out, depth + 1);
    }

    private static JSONObject node(AccessibilityNodeInfo n) {
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        JSONObject o = new JSONObject();
        try {
            CharSequence t = n.getText(), d = n.getContentDescription();
            if (Keeper.isAtLeast(26) && n.isShowingHintText()) t = null;
            o.put("text", t == null ? "" : t.toString());
            o.put("desc", d == null ? "" : d.toString());
            o.put("id", n.getViewIdResourceName() == null ? "" : n.getViewIdResourceName());
            o.put("cls", n.getClassName() == null ? "" : n.getClassName().toString());
            o.put("pkg", n.getPackageName() == null ? "" : n.getPackageName().toString());
            o.put("b", new JSONArray().put(r.left).put(r.top).put(r.right).put(r.bottom));
            if (n.isClickable()) o.put("click", true);
            if (n.isLongClickable()) o.put("long", true);
            if (n.isScrollable()) o.put("scroll", true);
            if (n.isEditable()) o.put("edit", true);
            if (n.isCheckable()) o.put("check", n.isChecked());
            if (n.isSelected()) o.put("sel", true);
            if (!n.isEnabled()) o.put("on", false);
        } catch (JSONException ignored) {
            // leave it out
        }
        return o;
    }

    // ---- gestures -------------------------------------------------------------------

    private static Path line(float x1, float y1, float x2, float y2) {
        Path p = new Path();
        p.moveTo(Math.max(0, x1), Math.max(0, y1));
        p.lineTo(Math.max(0, x2), Math.max(0, y2));
        return p;
    }

    /** Dispatches one stroke and waits until Android ran (or dropped) it. */
    private static JSONObject gesture(InputService in, String op, Path p, int ms) {
        if (!Keeper.isAtLeast(24)) return Proto.obj("op", op, "error", "Gestures need Android 7 or newer");
        long duration = Math.min(ms, GestureDescription.getMaxGestureDuration());
        GestureDescription g = new GestureDescription.Builder().addStroke(new GestureDescription.StrokeDescription(p, 0, duration)).build();
        CountDownLatch done = new CountDownLatch(1);
        boolean[] ok = {false};
        main.post(() -> {
            boolean sent = in.dispatchGesture(g, new AccessibilityService.GestureResultCallback() {
                @Override
                public void onCompleted(GestureDescription d) {
                    ok[0] = true;
                    done.countDown();
                }

                @Override
                public void onCancelled(GestureDescription d) {
                    done.countDown();
                }
            }, main);
            if (!sent) done.countDown();
        });
        try {
            done.await(duration + 3_000, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        return ok[0] ? Proto.obj("op", op, "ok", true) : Proto.obj("op", op, "error", "Android cancelled the gesture (another touch, or the screen is off)");
    }

    // ---- events: windows, dialogs, toasts, notifications -------------------------------

    /** From InputService: something appeared on screen. */
    static void onEvent(InputService in, AccessibilityEvent e) {
        String pkg = e.getPackageName() == null ? "" : e.getPackageName().toString();
        String cls = e.getClassName() == null ? "" : e.getClassName().toString();
        StringBuilder text = new StringBuilder();
        for (CharSequence t : e.getText()) {
            if (t == null) continue;
            if (text.length() > 0) text.append(" | ");
            text.append(t);
        }
        String type;
        if (e.getEventType() == AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED) {
            if (e.getParcelableData() instanceof Notification) {
                type = "notification";
                Notification n = (Notification) e.getParcelableData();
                Bundle x = n.extras;
                if (x != null && text.length() == 0) {
                    CharSequence title = x.getCharSequence(Notification.EXTRA_TITLE), body = x.getCharSequence(Notification.EXTRA_TEXT);
                    text.append(title == null ? "" : title).append(body == null ? "" : " — " + body);
                }
            } else {
                type = "toast";
            }
        } else if (e.getEventType() == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
            boolean permission = isPermissionUi(pkg);
            type = permission ? "permission" : cls.contains("Dialog") || cls.contains("Popup") ? "dialog" : "window";
            if (permission && autoAllow) main.postDelayed(() -> allow(in), 400);
        } else {
            return;
        }
        if (pkg.equals(in.getPackageName()) && !type.equals("toast")) return;
        record(Proto.obj("type", type, "pkg", pkg, "cls", cls, "text", text.toString()));
    }

    private static synchronized void record(JSONObject o) {
        eventSeq++;
        put(o, "seq", eventSeq);
        put(o, "time", System.currentTimeMillis());
        events.addLast(o);
        while (events.size() > MAX_EVENTS) events.removeFirst();
    }

    private static synchronized JSONObject events(long since) {
        JSONArray a = new JSONArray();
        for (JSONObject o : events) if (o.optLong("seq") > since) a.put(o);
        return Proto.obj("op", "events", "events", a, "next", eventSeq, "auto_allow", autoAllow);
    }

    private static boolean isPermissionUi(String pkg) {
        for (String p : PERMISSION_UI) if (p.equals(pkg)) return true;
        return false;
    }

    /** Taps the best "Allow" button on the permission prompt on screen. */
    private static void allow(InputService in) {
        AccessibilityNodeInfo root = in.getRootInActiveWindow();
        if (root == null || root.getPackageName() == null || !isPermissionUi(root.getPackageName().toString())) return;
        for (String id : ALLOW_IDS) {
            for (String p : PERMISSION_UI) {
                List<AccessibilityNodeInfo> found = root.findAccessibilityNodeInfosByViewId(p + ":id/" + id);
                if (found == null) continue;
                for (AccessibilityNodeInfo b : found) {
                    if (b.isVisibleToUser() && b.isEnabled() && b.performAction(AccessibilityNodeInfo.ACTION_CLICK)) {
                        CharSequence label = b.getText();
                        record(Proto.obj("type", "allowed", "pkg", p, "cls", "", "text", label == null ? id : label.toString()));
                        // Several permissions come one prompt after another.
                        main.postDelayed(() -> allow(in), 700);
                        return;
                    }
                }
            }
        }
    }

    // ---- test settings ------------------------------------------------------------------

    private static JSONObject settings(Context c, JSONObject req) {
        if (!req.has("animations") || req.isNull("animations")) return Proto.obj("op", "settings", "error", "Without ADB only animations can be changed");
        float scale = req.optBoolean("animations", true) ? 1f : 0f;
        try {
            for (String k : new String[] {Settings.Global.WINDOW_ANIMATION_SCALE, Settings.Global.TRANSITION_ANIMATION_SCALE,
                    Settings.Global.ANIMATOR_DURATION_SCALE}) {
                Settings.Global.putFloat(c.getContentResolver(), k, scale);
            }
        } catch (SecurityException e) {
            return Proto.obj("op", "settings", "error", "The helper may not change settings on this phone (it was never set up over ADB)");
        }
        return Proto.obj("op", "settings", "animations", scale > 0);
    }

    // ---- the ADB socket -------------------------------------------------------------------

    /** Opens the local socket (InputService calls this when it starts). */
    static synchronized void serve(Context c) {
        if (server != null) return;
        try {
            server = new LocalServerSocket(SOCKET);
        } catch (IOException e) {
            Log.w(Keeper.TAG, "automation socket: " + e);
            return;
        }
        LocalServerSocket s = server;
        Context app = c.getApplicationContext();
        Thread t = new Thread(() -> {
            while (true) {
                LocalSocket conn;
                try {
                    conn = s.accept();
                } catch (IOException e) {
                    return; // closed
                }
                new Thread(() -> connection(app, conn), "dev-conn").start();
            }
        }, "dev-accept");
        t.setDaemon(true);
        t.start();
    }

    static synchronized void stop() {
        if (server == null) return;
        try {
            server.close();
        } catch (IOException ignored) {
            // closing
        }
        server = null;
    }

    private static void connection(Context c, LocalSocket s) {
        try {
            int uid = s.getPeerCredentials().getUid();
            if (uid != SHELL_UID && uid != 0) {
                Log.w(Keeper.TAG, "automation socket: refused uid " + uid);
                return;
            }
            DataInputStream in = new DataInputStream(new BufferedInputStream(s.getInputStream()));
            DataOutputStream out = new DataOutputStream(new BufferedOutputStream(s.getOutputStream()));
            while (true) {
                JSONObject req;
                try {
                    req = Proto.read(in);
                } catch (EOFException e) {
                    return;
                }
                String op = req.optString("op");
                Proto.write(out, handles(op) ? handle(c, req) : Proto.obj("op", op, "error", "unknown request"));
            }
        } catch (IOException e) {
            Log.d(Keeper.TAG, "automation connection: " + e);
        } finally {
            try {
                s.close();
            } catch (IOException ignored) {
                // closing
            }
        }
    }

    private static void put(JSONObject o, String k, Object v) {
        try {
            o.put(k, v);
        } catch (JSONException ignored) {
            // leave it out
        }
    }
}
