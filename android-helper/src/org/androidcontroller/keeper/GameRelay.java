package org.androidcontroller.keeper;

import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.HashMap;
import java.util.Map;

/**
 * Relays the in-game debug add-on (ACaddon.md in the Android Controller
 * repository) to a PC connected without ADB. A game built with the add-on
 * listens on 127.0.0.1:{@code port} (27191 by default) and speaks newline
 * JSON; this connects to it while a PC is linked, keeps what the game sends
 * ({@code hello}, {@code log}, {@code event}) for the next heartbeat, and
 * passes the PC's {@code state} and {@code command} requests through.
 * Over ADB the PC connects to the game itself.
 */
final class GameRelay {
    private static final int RETRY_MS = 3_000;
    private static final int MAX_NEWS = 500;
    private static final int MAX_PER_POLL = 200;
    private static final int MAX_TEXT = 8_000;

    private static Socket socket;
    private static OutputStream out;
    private static long lastTry;
    private static boolean connecting;
    private static int port = 27191;
    private static JSONObject hello;
    private static boolean helloNew;
    private static boolean goneNew;
    private static final ArrayDeque<JSONObject> news = new ArrayDeque<>();
    private static final Map<Long, JSONObject> results = new HashMap<>();
    private static long nextId = 1;

    private GameRelay() {}

    /**
     * Called on each heartbeat: (re)connects when due and returns what the
     * game sent since the last call, or null when nothing changed.
     */
    static synchronized JSONObject poll(int wantPort) {
        if (wantPort > 0 && wantPort != port) {
            port = wantPort;
            close();
        }
        ensure();
        if (!helloNew && !goneNew && news.isEmpty()) return null;
        JSONObject o = new JSONObject();
        try {
            if (helloNew && hello != null) o.put("hello", hello);
            if (goneNew) o.put("gone", true);
            JSONArray items = new JSONArray();
            while (!news.isEmpty() && items.length() < MAX_PER_POLL) items.put(news.removeFirst());
            if (items.length() > 0) o.put("news", items);
            if (!news.isEmpty()) o.put("more", true);
        } catch (JSONException ignored) {
            // leave it out
        }
        helloNew = false;
        goneNew = false;
        return o;
    }

    /** The PC's request (`{"type": "state"}` or a command); waits for the answer. */
    static JSONObject request(JSONObject msg, int timeoutMs) {
        long id;
        OutputStream o;
        synchronized (GameRelay.class) {
            ensure();
            if (out == null) return Proto.obj("op", "game", "error", "No game with the debug add-on is running on this phone (port " + port + ")");
            id = nextId++;
            o = out;
        }
        try {
            msg.put("id", id);
            byte[] line = (msg.toString() + "\n").getBytes(StandardCharsets.UTF_8);
            synchronized (o) {
                o.write(line);
                o.flush();
            }
        } catch (IOException | JSONException e) {
            synchronized (GameRelay.class) {
                close();
            }
            return Proto.obj("op", "game", "error", "The game closed the connection");
        }
        long until = System.currentTimeMillis() + Math.max(100, Math.min(timeoutMs, 60_000));
        synchronized (GameRelay.class) {
            while (!results.containsKey(id)) {
                long left = until - System.currentTimeMillis();
                if (left <= 0 || socket == null) break;
                try {
                    GameRelay.class.wait(left);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    break;
                }
            }
            JSONObject r = results.remove(id);
            if (r == null) return Proto.obj("op", "game", "error", socket == null ? "The game closed the connection" : "The game did not answer in time");
            return Proto.obj("op", "game", "reply", r);
        }
    }

    private static void ensure() {
        if (socket != null || connecting || System.currentTimeMillis() - lastTry < RETRY_MS) return;
        lastTry = System.currentTimeMillis();
        connecting = true;
        int p = port;
        Thread t = new Thread(() -> connect(p), "game-connect");
        t.setDaemon(true);
        t.start();
    }

    private static void connect(int p) {
        Socket s = new Socket();
        try {
            s.connect(new InetSocketAddress("127.0.0.1", p), 500);
            s.setTcpNoDelay(true);
        } catch (IOException e) {
            try {
                s.close();
            } catch (IOException ignored) {
                // nothing there
            }
            synchronized (GameRelay.class) {
                connecting = false;
            }
            return;
        }
        OutputStream o;
        try {
            o = s.getOutputStream();
        } catch (IOException e) {
            synchronized (GameRelay.class) {
                connecting = false;
            }
            return;
        }
        synchronized (GameRelay.class) {
            connecting = false;
            socket = s;
            out = o;
        }
        Log.i(Keeper.TAG, "game add-on connected on port " + p);
        try {
            BufferedReader r = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
            String line;
            while ((line = r.readLine()) != null) {
                if (line.trim().isEmpty()) continue;
                JSONObject m;
                try {
                    m = new JSONObject(line);
                } catch (JSONException e) {
                    continue;
                }
                received(m);
            }
        } catch (IOException e) {
            Log.d(Keeper.TAG, "game add-on: " + e);
        } finally {
            synchronized (GameRelay.class) {
                if (socket == s) {
                    close();
                    goneNew = true;
                }
            }
        }
    }

    private static synchronized void received(JSONObject m) {
        String type = m.optString("type");
        switch (type) {
            case "hello":
                hello = m;
                helloNew = true;
                break;
            case "result":
                results.put(m.optLong("id"), m);
                GameRelay.class.notifyAll();
                break;
            case "log":
            case "event":
                clip(m, "message");
                clip(m, "stack");
                news.addLast(m);
                while (news.size() > MAX_NEWS) news.removeFirst();
                break;
            default:
                break;
        }
    }

    private static void clip(JSONObject m, String k) {
        String v = m.optString(k, null);
        if (v != null && v.length() > MAX_TEXT) {
            try {
                m.put(k, v.substring(0, MAX_TEXT) + "…");
            } catch (JSONException ignored) {
                // keep it
            }
        }
    }

    private static void close() {
        if (socket != null) {
            try {
                socket.close();
            } catch (IOException ignored) {
                // closing
            }
        }
        socket = null;
        out = null;
        hello = null;
        GameRelay.class.notifyAll();
    }
}
