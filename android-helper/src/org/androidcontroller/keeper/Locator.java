package org.androidcontroller.keeper;

import android.content.Context;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.DataInputStream;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Lets the PC find this phone when mDNS cannot, for example on a router that
 * filters multicast: answers on a fixed port with this app's random id and the
 * current wireless debugging port. It takes no commands, and being found gives
 * no access: the PC still needs its ADB key accepted.
 */
final class Locator {
    static final int PORT = 27193;
    private static final byte[] REQUEST = {'T', 'B', 'L', '1', '\n'};
    private static final int READ_TIMEOUT_MS = 1_000;
    private static final long RETRY_MS = 30_000;

    private static Thread thread;
    private static ServerSocket server;
    private static volatile boolean stopping;

    private Locator() {}

    static synchronized void start(Context context) {
        if (thread != null || !Keeper.locatorEnabled(context)) return;
        Context app = context.getApplicationContext();
        stopping = false;
        thread = new Thread(() -> serve(app), "locator");
        thread.setDaemon(true);
        thread.start();
    }

    static synchronized void stop() {
        stopping = true;
        try {
            if (server != null) server.close();
        } catch (IOException ignored) {
            // closing
        }
        server = null;
        thread = null;
    }

    private static void serve(Context c) {
        while (!stopping) {
            try (ServerSocket s = new ServerSocket()) {
                s.setReuseAddress(true);
                s.bind(new InetSocketAddress(PORT));
                synchronized (Locator.class) {
                    server = s;
                }
                while (!stopping) {
                    Socket conn = s.accept();
                    Thread t = new Thread(() -> answer(c, conn), "locator-conn");
                    t.setDaemon(true);
                    t.start();
                }
            } catch (IOException e) {
                if (stopping) return;
                Log.w(Keeper.TAG, "locator: " + e);
                try {
                    Thread.sleep(RETRY_MS);
                } catch (InterruptedException ie) {
                    return;
                }
            }
        }
    }

    private static void answer(Context c, Socket s) {
        try (Socket conn = s) {
            conn.setSoTimeout(READ_TIMEOUT_MS);
            byte[] got = new byte[REQUEST.length];
            new DataInputStream(conn.getInputStream()).readFully(got);
            if (!Arrays.equals(got, REQUEST)) return;
            conn.getOutputStream().write(reply(c).toString().getBytes(StandardCharsets.UTF_8));
            conn.getOutputStream().flush();
        } catch (IOException e) {
            Log.d(Keeper.TAG, "locator connection: " + e);
        }
    }

    private static JSONObject reply(Context c) {
        JSONObject o = Proto.obj("keeper_id", Keeper.keeperId(c), "version", Apps.versionCode(c));
        try {
            // Android 10 and older have no wireless debugging to be off.
            o.put("adb_on", !Keeper.hasWirelessDebugging() || Keeper.wirelessDebuggingOn(c));
            int port = Keeper.adbPort();
            if (port > 0) o.put("adb_port", port);
        } catch (JSONException e) {
            throw new IllegalStateException(e);
        }
        return o;
    }
}
