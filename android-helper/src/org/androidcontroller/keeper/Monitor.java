package org.androidcontroller.keeper;

import android.content.Context;
import android.content.Intent;
import android.media.MediaCodec;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Handler;
import android.os.Looper;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.Surface;
import android.view.WindowManager;

import org.json.JSONObject;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Collections;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Monitor mode: this phone shows a display of the PC (crates/ac-monitor).
 *
 * <p>The PC connects through {@code adb forward} to a local socket that only
 * adb's shell user may use, and opens two connections. The first message on
 * each is {@code {"role": ...}}. A "control" connection stays open while the
 * phone is online and carries JSON both ways, in {@link Proto} framing. A
 * "video" connection carries H.264 from the PC, each frame as a big-endian u64
 * (bit 62 = keyframe, low bits = pts) and u32 size, then the Annex-B bytes.
 */
final class Monitor {
    static final String SOCKET = "test-bed-multi-monitor";
    /** A press of the phone's button waits this long for the PC to answer. */
    private static final long REQUEST_VALID_MS = 60_000;
    private static final int SHELL_UID = 2000;
    private static final int MAX_FRAME = 16 << 20;
    private static final long KEY_FLAG = 1L << 62;
    private static final long PTS_MASK = (1L << 62) - 1;
    private static final long SURFACE_WAIT_MS = 10_000;
    private static final int INPUT_TIMEOUT_US = 100_000;
    private static final int OUTPUT_TIMEOUT_US = 50_000;

    private static final Handler main = new Handler(Looper.getMainLooper());
    /** One thread, so a touch's down, moves and up reach the PC in order. */
    private static final ExecutorService sender = Executors.newSingleThreadExecutor();
    private static final Set<DataOutputStream> controls = Collections.newSetFromMap(new ConcurrentHashMap<>());
    private static LocalServerSocket server;
    private static Context app;
    private static volatile boolean active;
    private static volatile boolean portrait;
    private static volatile boolean viewOnly;
    private static volatile String status = "";
    private static volatile long requestedAt;
    private static volatile MonitorActivity activity;
    private static final Object surfaceLock = new Object();
    private static Surface surface;

    private Monitor() {}

    // ---- what the rest of the app asks ----------------------------------------------

    static boolean active() {
        return active;
    }

    static boolean portrait() {
        return portrait;
    }

    static boolean viewOnly() {
        return viewOnly;
    }

    static String status() {
        return status;
    }

    static boolean pcConnected() {
        return !controls.isEmpty();
    }

    /** One line for the app's screen. */
    static String statusLine() {
        if (active) return "Showing the PC";
        if (requestPending()) return "Waiting for the PC… open Test Bed Multi and press Reconnect";
        return pcConnected() ? "Ready" : "Test Bed Multi is not connected";
    }

    private static boolean requestPending() {
        return requestedAt != 0 && System.currentTimeMillis() - requestedAt < REQUEST_VALID_MS;
    }

    /** The phone's button: asks the PC to start, or tells it to stop. */
    static void userToggle(Context c, boolean on) {
        Keeper.turnOn(c); // the PC cannot answer without it
        if (on) {
            requestedAt = System.currentTimeMillis();
            setStatus("Waiting for the PC…");
            sendAll(Proto.obj("op", "monitor_request", "on", true));
        } else {
            requestedAt = 0;
            sendAll(Proto.obj("op", "monitor_request", "on", false));
            stopLocally();
        }
    }

    static void sendTouch(JSONObject touch) {
        sendAll(touch);
    }

    static void askKeyframe() {
        sendAll(Proto.obj("op", "keyframe"));
    }

    private static void sendAll(JSONObject o) {
        sender.execute(() -> {
            for (DataOutputStream out : controls) {
                try {
                    Proto.write(out, o);
                } catch (IOException e) {
                    Log.d(Keeper.TAG, "monitor control: " + e);
                }
            }
        });
    }

    // ---- the screen ---------------------------------------------------------------------

    static void attach(MonitorActivity a) {
        activity = a;
        a.onStatus(status);
    }

    static void detach(MonitorActivity a) {
        if (activity == a) activity = null;
    }

    static void setSurface(Surface s) {
        synchronized (surfaceLock) {
            surface = s;
            surfaceLock.notifyAll();
        }
    }

    private static Surface waitSurface() throws InterruptedException {
        long until = System.currentTimeMillis() + SURFACE_WAIT_MS;
        synchronized (surfaceLock) {
            while (surface == null) {
                long left = until - System.currentTimeMillis();
                if (left <= 0) return null;
                surfaceLock.wait(left);
            }
            return surface;
        }
    }

    private static void setStatus(String s) {
        status = s;
        main.post(() -> {
            MonitorActivity a = activity;
            if (a != null) a.onStatus(s);
        });
    }

    private static void startLocally(JSONObject m) {
        requestedAt = 0;
        active = true;
        portrait = m.optBoolean("portrait", false);
        viewOnly = m.optBoolean("view_only", false);
        setStatus("Connecting…");
        MonitorService.start(app);
    }

    private static void stopLocally() {
        active = false;
        setStatus("");
        MonitorService.stop(app);
        main.post(() -> {
            MonitorActivity a = activity;
            if (a != null) a.finish();
        });
    }

    // ---- connections ----------------------------------------------------------------------

    static synchronized void startServer(Context context) {
        if (server != null) return;
        app = context.getApplicationContext();
        try {
            server = new LocalServerSocket(SOCKET);
        } catch (IOException e) {
            Log.w(Keeper.TAG, "monitor socket: " + e);
            return;
        }
        LocalServerSocket s = server;
        Thread t = new Thread(() -> {
            while (true) {
                LocalSocket conn;
                try {
                    conn = s.accept();
                } catch (IOException e) {
                    return; // closed
                }
                new Thread(() -> connection(conn), "monitor-conn").start();
            }
        }, "monitor-accept");
        t.setDaemon(true);
        t.start();
    }

    private static void connection(LocalSocket s) {
        try {
            int uid = s.getPeerCredentials().getUid();
            if (uid != SHELL_UID && uid != 0) {
                Log.w(Keeper.TAG, "monitor socket: refused uid " + uid);
                return;
            }
            DataInputStream in = new DataInputStream(new BufferedInputStream(s.getInputStream(), 1 << 16));
            DataOutputStream out = new DataOutputStream(new BufferedOutputStream(s.getOutputStream()));
            JSONObject first = Proto.read(in);
            if ("video".equals(first.optString("role"))) {
                video(in, first.optInt("w"), first.optInt("h"));
            } else if ("control".equals(first.optString("role"))) {
                control(in, out);
            }
        } catch (EOFException e) {
            Log.d(Keeper.TAG, "monitor connection closed");
        } catch (IOException e) {
            Log.d(Keeper.TAG, "monitor connection: " + e);
        } catch (RuntimeException e) {
            Log.w(Keeper.TAG, "monitor connection failed", e);
        } finally {
            try {
                s.close();
            } catch (IOException ignored) {
                // closing
            }
        }
    }

    private static void control(DataInputStream in, DataOutputStream out) throws IOException {
        controls.add(out);
        try {
            Proto.write(out, hello());
            while (true) {
                JSONObject m = Proto.read(in);
                switch (m.optString("op")) {
                    case "start":
                        startLocally(m);
                        break;
                    case "stop":
                        stopLocally();
                        break;
                    case "ping":
                        Proto.write(out, Proto.obj("op", "pong"));
                        break;
                    default:
                        break;
                }
            }
        } finally {
            controls.remove(out);
            if (active) setStatus("Reconnecting to the PC…");
        }
    }

    private static JSONObject hello() {
        DisplayMetrics dm = new DisplayMetrics();
        WindowManager wm = app.getSystemService(WindowManager.class);
        wm.getDefaultDisplay().getRealMetrics(dm);
        return Proto.obj("op", "hello", "version", Apps.versionCode(app), "width", dm.widthPixels, "height", dm.heightPixels,
                "density", dm.densityDpi, "avc", hasAvcDecoder(), "requested", requestPending());
    }

    private static boolean hasAvcDecoder() {
        MediaFormat f = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, 1280, 720);
        return new MediaCodecList(MediaCodecList.REGULAR_CODECS).findDecoderForFormat(f) != null;
    }

    // ---- video -------------------------------------------------------------------------------

    /** Decodes until the PC closes the connection. The screen may come and go meanwhile. */
    private static void video(DataInputStream in, int w, int h) throws IOException {
        if (w <= 0 || h <= 0) throw new IOException("bad video size");
        setStatus("Connecting…");
        MediaCodec codec = null;
        Thread drain = null;
        Surface current = null;
        boolean needKey = true;
        byte[] data = new byte[1 << 20];
        try {
            while (true) {
                long header = in.readLong();
                int size = in.readInt();
                if (size <= 0 || size > MAX_FRAME) throw new IOException("bad frame size " + size);
                if (size > data.length) data = new byte[size * 2];
                in.readFully(data, 0, size);
                boolean key = (header & KEY_FLAG) != 0;

                Surface now;
                synchronized (surfaceLock) {
                    now = surface;
                }
                if (now != current || codec == null) {
                    // A new screen needs a new decoder, starting at a keyframe.
                    closeCodec(codec, drain);
                    codec = null;
                    drain = null;
                    current = null;
                    needKey = true;
                    if (now == null) {
                        now = waitSurface();
                        if (now == null) continue;
                    }
                    MediaFormat f = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h);
                    if (Keeper.isAtLeast(30)) f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
                    codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
                    codec.configure(f, now, null, 0);
                    codec.start();
                    current = now;
                    drain = startDrain(codec);
                    askKeyframe();
                }
                if (needKey && !key) continue;
                needKey = false;
                try {
                    if (!queue(codec, data, size, header & PTS_MASK)) {
                        // A dropped frame breaks the ones after it until a keyframe.
                        needKey = true;
                        askKeyframe();
                    }
                } catch (IllegalStateException e) {
                    // The surface went away under the decoder.
                    Log.d(Keeper.TAG, "decoder: " + e);
                    closeCodec(codec, drain);
                    codec = null;
                    drain = null;
                    current = null;
                }
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } finally {
            closeCodec(codec, drain);
            if (active) setStatus("Reconnecting to the PC…");
        }
    }

    private static boolean queue(MediaCodec codec, byte[] data, int size, long pts) {
        int i = codec.dequeueInputBuffer(INPUT_TIMEOUT_US);
        if (i < 0) return false;
        ByteBuffer b = codec.getInputBuffer(i);
        if (b == null) return false;
        b.clear();
        b.put(data, 0, size);
        codec.queueInputBuffer(i, 0, size, pts, 0);
        return true;
    }

    private static Thread startDrain(MediaCodec codec) {
        Thread t = new Thread(() -> {
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            boolean first = true;
            try {
                while (!Thread.currentThread().isInterrupted()) {
                    int i = codec.dequeueOutputBuffer(info, OUTPUT_TIMEOUT_US);
                    if (i >= 0) {
                        codec.releaseOutputBuffer(i, true);
                        if (first) {
                            first = false;
                            setStatus("");
                        }
                    }
                }
            } catch (IllegalStateException e) {
                Log.d(Keeper.TAG, "decoder output ended: " + e);
            }
        }, "monitor-drain");
        t.start();
        return t;
    }

    private static void closeCodec(MediaCodec codec, Thread drain) {
        if (drain != null) drain.interrupt();
        if (codec == null) return;
        try {
            if (drain != null) drain.join(500);
            codec.stop();
        } catch (IllegalStateException e) {
            Log.d(Keeper.TAG, "stopping decoder: " + e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        codec.release();
    }
}
