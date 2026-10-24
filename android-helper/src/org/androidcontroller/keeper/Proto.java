package org.androidcontroller.keeper;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;

/**
 * Wire format of the no-ADB PC connection (crates/ac-direct on the PC).
 *
 * <p>Every connection starts with "ACD1" and a purpose byte from the PC.
 * Control messages are JSON objects, each sent as a big-endian u32 length
 * and UTF-8 bytes. After authentication a STREAM connection carries the
 * scrcpy video format to the PC and scrcpy control messages from it; a PUSH
 * connection carries the file's bytes.
 */
final class Proto {
    static final byte[] MAGIC = {'A', 'C', 'D', '1'};
    static final int PAIR = 1;
    static final int LINK = 2;
    static final int STREAM = 3;
    static final int PUSH = 4;
    private static final int MAX_JSON = 1 << 20;

    private Proto() {}

    static JSONObject read(DataInputStream in) throws IOException {
        int n = in.readInt();
        if (n < 2 || n > MAX_JSON) throw new IOException("bad message length " + n);
        byte[] b = new byte[n];
        in.readFully(b);
        try {
            return new JSONObject(new String(b, StandardCharsets.UTF_8));
        } catch (JSONException e) {
            throw new IOException("bad message", e);
        }
    }

    static void write(DataOutputStream out, JSONObject o) throws IOException {
        byte[] b = o.toString().getBytes(StandardCharsets.UTF_8);
        synchronized (out) {
            out.writeInt(b.length);
            out.write(b);
            out.flush();
        }
    }

    static void error(DataOutputStream out, String message) throws IOException {
        write(out, obj("error", message));
    }

    static JSONObject obj(Object... kv) {
        JSONObject o = new JSONObject();
        try {
            for (int i = 0; i + 1 < kv.length; i += 2) o.put((String) kv[i], kv[i + 1]);
        } catch (JSONException e) {
            throw new IllegalArgumentException(e);
        }
        return o;
    }
}
