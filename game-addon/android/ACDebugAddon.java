package com.example.acdebug; // move into your app's package

import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageInfo;
import android.os.Build;
import android.os.Looper;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Android Controller debug add-on for native Android apps and games (Java
 * or Kotlin): protocol v1, see ACaddon.md.
 *
 * <pre>
 * // Application.onCreate():
 * ACDebugAddon.start(this);   // does nothing unless the app is debuggable
 * ACDebugAddon.command("give_coins", "Add coins", "amount:int", args -> wallet.add(args.optInt("amount", 100)));
 * ACDebugAddon.state(() -> new JSONObject().put("level", game.level()).put("coins", wallet.coins()));
 * ACDebugAddon.event("level_complete", new JSONObject().put("level", 3));
 * ACDebugAddon.log("warn", "slow frame", null);
 * </pre>
 *
 * Listens on 127.0.0.1:27191 (this phone only). Uncaught exceptions are sent
 * as {@code exception} logs before the app's crash handling runs. Commands and
 * the state run on the main thread; throw to fail a command.
 */
public final class ACDebugAddon {
    public static final int PROTOCOL = 1;
    public static final int PORT = 27191;
    private static final String TAG = "ACDebug";
    private static final int MAX_TEXT = 8000;
    private static final long MAIN_TIMEOUT_MS = 10_000;

    /** A command: gets its args, returns a JSON value (JSONObject, JSONArray, String, number, boolean or null). */
    public interface Command {
        Object run(JSONObject args) throws Exception;
    }

    /** The game's own state for game_state. */
    public interface StateProvider {
        Object get() throws Exception;
    }

    private static final class Cmd {
        final String help;
        final String[] args;
        final Command fn;

        Cmd(String help, String[] args, Command fn) {
            this.help = help;
            this.args = args;
            this.fn = fn;
        }
    }

    private static final Map<String, Cmd> commands = new LinkedHashMap<>();
    private static final List<OutputStream> clients = new CopyOnWriteArrayList<>();
    private static final android.os.Handler ui = new android.os.Handler(Looper.getMainLooper());
    private static volatile StateProvider stateProvider;
    private static volatile boolean started;
    private static String game = "", version = "", pkg = "";

    private ACDebugAddon() {}

    /** Starts the add-on in debuggable builds only. Call once, early. */
    public static synchronized void start(Context context) {
        Context c = context.getApplicationContext();
        if (started || (c.getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) == 0) return;
        started = true;
        pkg = c.getPackageName();
        CharSequence label = c.getApplicationInfo().loadLabel(c.getPackageManager());
        game = label == null ? pkg : label.toString();
        try {
            PackageInfo pi = c.getPackageManager().getPackageInfo(pkg, 0);
            version = pi.versionName == null ? "" : pi.versionName;
        } catch (Exception ignored) {
            // unknown
        }
        Thread.UncaughtExceptionHandler previous = Thread.getDefaultUncaughtExceptionHandler();
        Thread.setDefaultUncaughtExceptionHandler((t, e) -> {
            log("exception", e.toString(), stack(e));
            try {
                Thread.sleep(150); // let it reach the PC
            } catch (InterruptedException ignored) {
                // going down anyway
            }
            if (previous != null) previous.uncaughtException(t, e);
        });
        Thread server = new Thread(ACDebugAddon::serve, "ACDebug server");
        server.setDaemon(true);
        server.start();
    }

    /** Registers a command; each of {@code args} is "name:type" (for the help Claude sees). */
    public static synchronized void command(String name, String help, String args, Command handler) {
        commands.put(name, new Cmd(help, args == null || args.isEmpty() ? new String[0] : args.split(",\\s*"), handler));
    }

    public static void state(StateProvider provider) {
        stateProvider = provider;
    }

    /** Something happened in the game (level started, purchase, boss defeated...). */
    public static void event(String name, Object data) {
        send(obj("type", "event", "name", name, "data", data == null ? JSONObject.NULL : data));
    }

    /** A log line: level is debug, info, warn, error or exception. */
    public static void log(String level, String message, String stack) {
        send(obj("type", "log", "level", level, "message", clip(message), "stack", clip(stack)));
    }

    // ---- the server ---------------------------------------------------------------------

    private static void serve() {
        try (ServerSocket server = new ServerSocket(PORT, 4, InetAddress.getByName("127.0.0.1"))) {
            while (true) {
                Socket s = server.accept();
                Thread t = new Thread(() -> client(s), "ACDebug client");
                t.setDaemon(true);
                t.start();
            }
        } catch (IOException e) {
            Log.w(TAG, "cannot listen on port " + PORT + ": " + e);
        }
    }

    private static void client(Socket s) {
        OutputStream out = null;
        try {
            s.setTcpNoDelay(true);
            out = s.getOutputStream();
            write(out, hello());
            clients.add(out);
            BufferedReader in = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
            String line;
            while ((line = in.readLine()) != null) {
                if (line.trim().isEmpty()) continue;
                JSONObject req;
                try {
                    req = new JSONObject(line);
                } catch (JSONException e) {
                    continue;
                }
                write(out, answer(req));
            }
        } catch (IOException e) {
            Log.d(TAG, "client gone: " + e);
        } finally {
            if (out != null) clients.remove(out);
            try {
                s.close();
            } catch (IOException ignored) {
                // closing
            }
        }
    }

    private static synchronized JSONObject hello() {
        JSONArray list = new JSONArray();
        for (Map.Entry<String, Cmd> e : commands.entrySet()) {
            JSONObject args = new JSONObject();
            for (String a : e.getValue().args) {
                String[] p = a.split(":", 2);
                put(args, p[0].trim(), p.length > 1 ? p[1].trim() : "any");
            }
            list.put(obj("name", e.getKey(), "args", args, "help", e.getValue().help));
        }
        return obj("type", "hello", "protocol", PROTOCOL, "game", game, "version", version,
                "engine", "Android " + Build.VERSION.RELEASE + " (native)", "package", pkg, "commands", list);
    }

    private static JSONObject answer(JSONObject req) {
        Object id = req.opt("id");
        String type = req.optString("type");
        try {
            Object result;
            switch (type) {
                case "state": {
                    StateProvider p = stateProvider;
                    Object game = p == null ? JSONObject.NULL : onMain(p::get);
                    result = obj("game", game == null ? JSONObject.NULL : game);
                    break;
                }
                case "command": {
                    String name = req.optString("name");
                    Cmd c;
                    synchronized (ACDebugAddon.class) {
                        c = commands.get(name);
                    }
                    if (c == null) throw new IllegalArgumentException("unknown command " + name + " (see game_info)");
                    JSONObject args = req.optJSONObject("args");
                    JSONObject a = args == null ? new JSONObject() : args;
                    result = onMain(() -> c.fn.run(a));
                    break;
                }
                case "ping":
                    result = "pong";
                    break;
                default:
                    throw new IllegalArgumentException("unknown request type " + type);
            }
            return obj("id", id, "type", "result", "ok", true, "result", result == null ? JSONObject.NULL : result);
        } catch (Exception e) {
            Throwable cause = e.getCause() != null && e instanceof java.util.concurrent.ExecutionException ? e.getCause() : e;
            return obj("id", id, "type", "result", "ok", false, "error", cause.toString());
        }
    }

    /** Runs on the main thread and waits for it (game objects live there). */
    private static Object onMain(StateProvider work) throws Exception {
        if (Looper.myLooper() == Looper.getMainLooper()) return work.get();
        AtomicReference<Object> result = new AtomicReference<>();
        AtomicReference<Exception> error = new AtomicReference<>();
        CountDownLatch done = new CountDownLatch(1);
        ui.post(() -> {
            try {
                result.set(work.get());
            } catch (Exception e) {
                error.set(e);
            } finally {
                done.countDown();
            }
        });
        if (!done.await(MAIN_TIMEOUT_MS, TimeUnit.MILLISECONDS)) throw new IllegalStateException("the main thread is busy (frozen?)");
        if (error.get() != null) throw error.get();
        return result.get();
    }

    // ---- writing -----------------------------------------------------------------------

    private static void send(JSONObject msg) {
        if (!started) return;
        for (OutputStream out : clients) {
            try {
                write(out, msg);
            } catch (IOException e) {
                clients.remove(out);
            }
        }
    }

    private static void write(OutputStream out, JSONObject msg) throws IOException {
        byte[] line = (msg.toString() + "\n").getBytes(StandardCharsets.UTF_8);
        synchronized (out) {
            out.write(line);
            out.flush();
        }
    }

    private static String stack(Throwable e) {
        StringWriter w = new StringWriter();
        e.printStackTrace(new PrintWriter(w));
        return w.toString();
    }

    private static Object clip(String s) {
        if (s == null) return JSONObject.NULL;
        return s.length() <= MAX_TEXT ? s : s.substring(0, MAX_TEXT) + "…";
    }

    private static JSONObject obj(Object... kv) {
        JSONObject o = new JSONObject();
        for (int i = 0; i + 1 < kv.length; i += 2) put(o, (String) kv[i], kv[i + 1]);
        return o;
    }

    private static void put(JSONObject o, String k, Object v) {
        try {
            o.put(k, v == null ? JSONObject.NULL : v);
        } catch (JSONException ignored) {
            // leave it out
        }
    }
}
