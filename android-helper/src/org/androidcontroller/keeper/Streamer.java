package org.androidcontroller.keeper;

import android.accessibilityservice.AccessibilityService;
import android.content.Context;
import android.content.Intent;
import android.graphics.Point;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.media.MediaScannerConnection;
import android.media.projection.MediaProjection;
import android.os.Bundle;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.Display;
import android.view.Surface;

import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/**
 * One mirroring session on a STREAM connection: H.264 from the screen in the
 * scrcpy video format (what the PC's decoder already reads), and scrcpy
 * control messages back, turned into accessibility input.
 */
final class Streamer {
    private static final long FLAG_CONFIG = 1L << 62;
    private static final long FLAG_KEY_FRAME = 1L << 61;
    /** Re-encode the last frame when the screen is still, so a fresh PC view fills in. */
    private static final long REPEAT_AFTER_US = 100_000;

    private final Context ctx;
    private final Socket socket;
    private final DataInputStream in;
    private final int maxSize;
    private final int bitRate;
    private final int maxFps;
    /** {@code video/avc} or, when the PC asked and the phone can, {@code video/hevc}. */
    private String mime;
    private volatile boolean stopped;
    private volatile boolean keyFrameWanted;
    /** Current capture (frame) size and the real screen size it maps to. */
    private volatile int frameW, frameH, realW, realH;

    Streamer(Context ctx, Socket socket, DataInputStream in, int maxSize, int bitRate, int maxFps, String codec) {
        this.mime = codec.equals("h265") && Apps.hardwareEncoder(MediaFormat.MIMETYPE_VIDEO_HEVC) != null
                ? MediaFormat.MIMETYPE_VIDEO_HEVC : MediaFormat.MIMETYPE_VIDEO_AVC;
        this.ctx = ctx;
        this.socket = socket;
        this.in = in;
        this.maxSize = maxSize;
        this.bitRate = bitRate;
        this.maxFps = maxFps;
    }

    void run(MediaProjection projection) throws IOException {
        DataOutputStream out = new DataOutputStream(new BufferedOutputStream(socket.getOutputStream(), 1 << 16));
        Thread control = new Thread(this::readControl, "direct-control");
        control.start();
        DisplayManager dm = ctx.getSystemService(DisplayManager.class);
        Display display = dm.getDisplay(Display.DEFAULT_DISPLAY);
        DisplayMetrics metrics = new DisplayMetrics();
        display.getRealMetrics(metrics);
        VirtualDisplay vd = null;
        try {
            boolean announced = false;
            long ptsOrigin = -1;
            while (!stopped) {
                Point real = new Point();
                display.getRealSize(real);
                MediaCodec codec = null;
                Surface surface = null;
                int[] size = null;
                for (int attempt = 0; attempt < 2 && codec == null; attempt++) {
                    for (int limit : new int[] {maxSize, 1280, 1024, 800}) {
                        if (limit > maxSize && maxSize > 0) continue;
                        size = scaled(real.x, real.y, limit);
                        try {
                            codec = MediaCodec.createEncoderByType(mime);
                            codec.configure(format(size[0], size[1]), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
                            surface = codec.createInputSurface();
                            codec.start();
                            break;
                        } catch (IOException | RuntimeException e) {
                            Log.w(Direct.TAG, mime + " encoder at " + size[0] + "x" + size[1] + " failed", e);
                            if (codec != null) codec.release();
                            codec = null;
                        }
                    }
                    // H.265 refused before the PC heard which format: fall back to H.264.
                    if (codec == null && !announced && !mime.equals(MediaFormat.MIMETYPE_VIDEO_AVC)) {
                        mime = MediaFormat.MIMETYPE_VIDEO_AVC;
                    } else {
                        break;
                    }
                }
                if (codec == null) throw new IOException("the phone's video encoder did not start");
                if (!announced) {
                    // scrcpy's codec id: what the PC's demuxer reads first.
                    out.write((mime.equals(MediaFormat.MIMETYPE_VIDEO_HEVC) ? "h265" : "h264").getBytes(StandardCharsets.US_ASCII));
                    announced = true;
                }
                frameW = size[0];
                frameH = size[1];
                realW = real.x;
                realH = real.y;
                if (vd == null) {
                    vd = projection.createVirtualDisplay("android-controller", size[0], size[1], metrics.densityDpi,
                            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, surface, null, null);
                } else {
                    vd.resize(size[0], size[1], metrics.densityDpi);
                    vd.setSurface(surface);
                }
                Log.i(Direct.TAG, "capturing " + real.x + "x" + real.y + " as " + size[0] + "x" + size[1]);
                writeSession(out, size[0], size[1]);
                ptsOrigin = encode(codec, out, display, real, ptsOrigin);
                vd.setSurface(null);
                try {
                    codec.stop();
                } catch (RuntimeException ignored) {
                    // already failed
                }
                codec.release();
                surface.release();
            }
        } finally {
            stopped = true;
            if (vd != null) vd.release();
            try {
                socket.close();
            } catch (IOException ignored) {
                // closing
            }
            Projection.used();
        }
    }

    /** Pumps encoder output until stopped or the screen size changes (rotation). */
    private long encode(MediaCodec codec, DataOutputStream out, Display display, Point real, long ptsOrigin)
            throws IOException {
        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        Point now = new Point();
        long lastCheck = 0;
        while (!stopped) {
            if (keyFrameWanted) {
                keyFrameWanted = false;
                Bundle b = new Bundle();
                b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0);
                codec.setParameters(b);
            }
            long t = System.currentTimeMillis();
            if (t - lastCheck > 300) {
                lastCheck = t;
                display.getRealSize(now);
                if (now.x != real.x || now.y != real.y) return ptsOrigin;
            }
            int idx;
            try {
                idx = codec.dequeueOutputBuffer(info, 100_000);
            } catch (IllegalStateException e) {
                throw new IOException("video encoder stopped", e);
            }
            if (idx < 0) continue;
            ByteBuffer buf = codec.getOutputBuffer(idx);
            try {
                if (buf == null || info.size == 0) continue;
                boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
                long header;
                if (config) {
                    header = FLAG_CONFIG;
                } else {
                    if (ptsOrigin < 0) ptsOrigin = info.presentationTimeUs;
                    header = Math.max(0, info.presentationTimeUs - ptsOrigin) & (FLAG_KEY_FRAME - 1);
                    if ((info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0) header |= FLAG_KEY_FRAME;
                }
                byte[] data = new byte[info.size];
                buf.position(info.offset);
                buf.get(data, 0, info.size);
                out.writeLong(header);
                out.writeInt(data.length);
                out.write(data);
                out.flush();
            } finally {
                codec.releaseOutputBuffer(idx, false);
            }
        }
        return ptsOrigin;
    }

    private MediaFormat format(int w, int h) {
        MediaFormat f = MediaFormat.createVideoFormat(mime, w, h);
        f.setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
        f.setInteger(MediaFormat.KEY_BIT_RATE, bitRate > 0 ? bitRate : 4_000_000);
        f.setInteger(MediaFormat.KEY_FRAME_RATE, 60);
        f.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 10);
        f.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, REPEAT_AFTER_US);
        if (maxFps > 0) f.setFloat("max-fps-to-encoder", maxFps);
        return f;
    }

    /** The screen scaled so its longest side is at most {@code limit}, multiples of 8. */
    static int[] scaled(int w, int h, int limit) {
        int longest = Math.max(w, h);
        if (limit > 0 && longest > limit) {
            w = (int) ((long) w * limit / longest);
            h = (int) ((long) h * limit / longest);
        }
        return new int[] {Math.max(8, w & ~7), Math.max(8, h & ~7)};
    }

    private static void writeSession(DataOutputStream out, int w, int h) throws IOException {
        out.writeByte(0x80);
        out.writeByte(0);
        out.writeByte(0);
        out.writeByte(0);
        out.writeInt(w);
        out.writeInt(h);
        out.flush();
    }

    // ---- control messages (scrcpy 4.1 layouts, app/src/control_msg.c) -----------------

    private void readControl() {
        try {
            while (!stopped) handle(in);
        } catch (IOException e) {
            // the PC closed the session
        } finally {
            stopped = true;
        }
    }

    private void handle(DataInputStream in) throws IOException {
        int type = in.readUnsignedByte();
        InputService input = InputService.get();
        switch (type) {
            case 0: { // INJECT_KEYCODE
                int action = in.readUnsignedByte();
                int keycode = in.readInt();
                in.readInt(); // repeat
                int meta = in.readInt();
                if (action == 0 && input != null) input.key(keycode, meta);
                break;
            }
            case 1: { // INJECT_TEXT
                String text = readString(in, 4);
                if (input != null) input.text(text);
                break;
            }
            case 2: { // INJECT_TOUCH_EVENT
                int action = in.readUnsignedByte();
                long pointer = in.readLong();
                int x = in.readInt(), y = in.readInt();
                int sw = in.readUnsignedShort(), sh = in.readUnsignedShort();
                in.readUnsignedShort(); // pressure
                in.readInt(); // action button
                in.readInt(); // buttons
                if (input != null && sw == frameW && sh == frameH && action <= 3) {
                    input.touch(action, pointer, (float) x * realW / sw, (float) y * realH / sh);
                }
                break;
            }
            case 3: { // INJECT_SCROLL_EVENT
                int x = in.readInt(), y = in.readInt();
                int sw = in.readUnsignedShort(), sh = in.readUnsignedShort();
                float h = in.readShort() / 32768f * 16f;
                float v = in.readShort() / 32768f * 16f;
                in.readInt(); // buttons
                if (input != null && sw == frameW && sh == frameH) {
                    input.scroll((float) x * realW / sw, (float) y * realH / sh, h, v, realW, realH);
                }
                break;
            }
            case 4: { // BACK_OR_SCREEN_ON
                int action = in.readUnsignedByte();
                if (action != 0) break;
                if (!Screen.isOn(ctx)) {
                    Screen.wake(ctx);
                } else if (input != null) {
                    input.globalAction(AccessibilityService.GLOBAL_ACTION_BACK);
                }
                break;
            }
            case 5: // EXPAND_NOTIFICATION_PANEL
                if (input != null) input.globalAction(AccessibilityService.GLOBAL_ACTION_NOTIFICATIONS);
                break;
            case 6: // EXPAND_SETTINGS_PANEL
                if (input != null) input.globalAction(AccessibilityService.GLOBAL_ACTION_QUICK_SETTINGS);
                break;
            case 7: // COLLAPSE_PANELS
                if (input != null) {
                    // GLOBAL_ACTION_DISMISS_NOTIFICATION_SHADE on Android 12+.
                    input.globalAction(Keeper.isAtLeast(31) ? 15 : AccessibilityService.GLOBAL_ACTION_BACK);
                }
                break;
            case 8: // GET_CLIPBOARD
                in.readUnsignedByte();
                break;
            case 9: { // SET_CLIPBOARD
                in.readLong(); // sequence
                boolean paste = in.readUnsignedByte() != 0;
                String text = readString(in, 4);
                if (input != null) input.setClipboard(text, paste);
                break;
            }
            case 10: // SET_DISPLAY_POWER
                in.readUnsignedByte();
                break;
            case 11: // ROTATE_DEVICE
            case 15: // OPEN_HARD_KEYBOARD_SETTINGS
                break;
            case 16: { // START_APP
                String name = readString(in, 1);
                startApp(name);
                break;
            }
            case 17: // RESET_VIDEO
                keyFrameWanted = true;
                break;
            case 22: { // SCAN_FILE
                String path = readString(in, 4);
                MediaScannerConnection.scanFile(ctx, new String[] {path}, null, null);
                break;
            }
            default:
                throw new IOException("unknown control message " + type);
        }
    }

    private static String readString(DataInputStream in, int lenBytes) throws IOException {
        int n = lenBytes == 1 ? in.readUnsignedByte() : in.readInt();
        if (n < 0 || n > (1 << 18)) throw new IOException("bad string length " + n);
        byte[] b = new byte[n];
        in.readFully(b);
        return new String(b, StandardCharsets.UTF_8);
    }

    private void startApp(String name) {
        String pkg = name.startsWith("+") || name.startsWith("?") ? name.substring(1) : name;
        Intent i = ctx.getPackageManager().getLaunchIntentForPackage(pkg);
        if (i == null) return;
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            ctx.startActivity(i);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "cannot start " + pkg, e);
        }
    }

    void stop() {
        stopped = true;
        try {
            socket.close();
        } catch (IOException ignored) {
            // closing
        }
    }
}
