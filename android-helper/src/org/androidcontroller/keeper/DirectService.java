package org.androidcontroller.keeper;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.media.projection.MediaProjection;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;

import org.json.JSONObject;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.security.GeneralSecurityException;
import java.security.KeyPair;
import java.security.PublicKey;
import java.util.Collections;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * The no-ADB PC connection: a small TCP server on {@link Direct#PORT},
 * announced over mDNS as {@code _acdirect._tcp}. Only PCs paired through
 * "Pair with PC" (a code shown on this phone) are let in.
 */
public final class DirectService extends Service {
    static final String CHANNEL = "direct";
    private static final int NOTIFICATION = 2;
    private static final int HANDSHAKE_TIMEOUT_MS = 10_000;
    /** A link with no ping for this long is dead. */
    private static final int LINK_TIMEOUT_MS = 30_000;

    private static volatile DirectService running;

    private final Handler main = new Handler(Looper.getMainLooper());
    private ServerSocket server;
    private Thread acceptor;
    private volatile boolean stopping;
    private NsdManager nsd;
    private NsdManager.RegistrationListener nsdListener;
    private boolean nsdPairing;
    private WifiManager.WifiLock wifiLock;
    private PowerManager.WakeLock cpuLock;
    private final Set<Socket> links = Collections.newSetFromMap(new ConcurrentHashMap<>());
    private final Set<Streamer> streams = Collections.newSetFromMap(new ConcurrentHashMap<>());
    private volatile String status = "Starting…";
    private final Runnable tick = new Runnable() {
        @Override
        public void run() {
            boolean pairing = Direct.pairCode() != null;
            if (pairing != nsdPairing) announce();
            refreshNotification();
            main.postDelayed(this, 2_000);
        }
    };

    static DirectService get() {
        return running;
    }

    /** One line for the app screen. */
    static String statusLine() {
        DirectService s = running;
        return s == null ? "off" : s.status;
    }

    static int linkCount() {
        DirectService s = running;
        return s == null ? 0 : s.links.size();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        running = this;
        if (Keeper.isAtLeast(26)) {
            NotificationManager nm = getSystemService(NotificationManager.class);
            NotificationChannel ch = new NotificationChannel(CHANNEL, "PC connection without ADB", NotificationManager.IMPORTANCE_LOW);
            ch.setShowBadge(false);
            nm.createNotificationChannel(ch);
        }
        startForeground(NOTIFICATION, notification());
        WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wm != null) {
            wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "keeper:direct");
            wifiLock.setReferenceCounted(false);
            wifiLock.acquire();
        }
        PowerManager pm = getSystemService(PowerManager.class);
        if (pm != null) {
            cpuLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "keeper:direct-link");
            cpuLock.setReferenceCounted(false);
        }
        nsd = getSystemService(NsdManager.class);
        acceptor = new Thread(this::serve, "direct-accept");
        acceptor.start();
        main.post(tick);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (!Direct.enabled(this)) {
            stopSelf();
            return START_NOT_STICKY;
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopping = true;
        running = null;
        main.removeCallbacks(tick);
        unannounce();
        try {
            if (server != null) server.close();
        } catch (IOException ignored) {
            // closing
        }
        for (Streamer s : streams) s.stop();
        for (Socket s : links) closeQuietly(s);
        if (wifiLock != null) wifiLock.release();
        if (cpuLock != null) cpuLock.release();
        Screen.keepOn(this, false);
        super.onDestroy();
    }

    // ---- accepting -----------------------------------------------------------------

    private void serve() {
        while (!stopping) {
            try {
                server = new ServerSocket();
                server.setReuseAddress(true);
                server.bind(new InetSocketAddress(Direct.PORT));
                setStatus("Waiting for the PC");
                main.post(this::announce);
                while (!stopping) {
                    Socket s = server.accept();
                    Thread t = new Thread(() -> connection(s), "direct-conn");
                    t.start();
                }
            } catch (IOException e) {
                if (stopping) return;
                Log.w(Direct.TAG, "server failed; retrying", e);
                setStatus("Port " + Direct.PORT + " is busy; retrying");
                try {
                    if (server != null) server.close();
                    Thread.sleep(5_000);
                } catch (IOException | InterruptedException ignored) {
                    // retry
                }
            }
        }
    }

    private void connection(Socket s) {
        try {
            s.setTcpNoDelay(true);
            s.setSoTimeout(HANDSHAKE_TIMEOUT_MS);
            DataInputStream in = new DataInputStream(new BufferedInputStream(s.getInputStream()));
            DataOutputStream out = new DataOutputStream(new BufferedOutputStream(s.getOutputStream()));
            byte[] magic = new byte[4];
            in.readFully(magic);
            if (!java.util.Arrays.equals(magic, Proto.MAGIC)) return;
            int purpose = in.readUnsignedByte();
            if (purpose == Proto.PAIR) {
                pair(in, out);
                return;
            }
            JSONObject req = authenticate(in, out, purpose);
            if (req == null) return;
            switch (purpose) {
                case Proto.LINK:
                    link(s, in, out);
                    break;
                case Proto.STREAM:
                    stream(s, req, in, out);
                    break;
                case Proto.PUSH:
                    s.setSoTimeout(60_000);
                    Receiver.handle(this, req, in, out);
                    break;
                default:
                    Proto.error(out, "unknown request");
            }
        } catch (IOException e) {
            Log.d(Direct.TAG, "connection ended: " + e);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "connection failed", e);
        } finally {
            closeQuietly(s);
        }
    }

    // ---- pairing and authentication --------------------------------------------------

    /**
     * P-256 ECDH, confirmed with the code shown on the phone. Both sides then
     * keep key = HMAC(shared, "acd-key" || transcript || SHA-256(code)).
     */
    private void pair(DataInputStream in, DataOutputStream out) throws IOException {
        JSONObject req = Proto.read(in);
        String code = Direct.pairCode();
        if (code == null) {
            Proto.error(out, "Pairing is not open on the phone: tap \"Pair with PC\" in Wireless Debugging Keeper");
            return;
        }
        String pcId = req.optString("pc_id"), pcName = req.optString("pc_name", "PC");
        try {
            byte[] theirPub = Crypto.unhex(req.getString("pub"));
            KeyPair kp = Crypto.ecKeyPair();
            byte[] ourPub = Crypto.encodePoint(kp.getPublic());
            PublicKey theirs = Crypto.decodePoint(theirPub, kp.getPublic());
            byte[] shared = Crypto.ecdh(kp.getPrivate(), theirs);
            Proto.write(out, Proto.obj("id", Direct.deviceId(this), "model", Direct.model(), "pub", Crypto.hex(ourPub)));
            byte[] transcript = Crypto.sha256(theirPub, ourPub);
            byte[] codeKey = Crypto.sha256(Crypto.utf8("acd-code:" + code));
            JSONObject confirm = Proto.read(in);
            byte[] want = Crypto.hmac(codeKey, Crypto.utf8("pc"), transcript, shared);
            if (!Crypto.same(want, Crypto.unhex(confirm.getString("confirm")))) {
                Direct.rotatePairCode();
                Proto.error(out, "Wrong code. The phone now shows a new one; type that.");
                return;
            }
            byte[] key = Crypto.hmac(shared, Crypto.utf8("acd-key"), transcript, codeKey);
            Direct.addPc(this, pcId, pcName, key);
            Direct.closePairing();
            Direct.lastPaired = pcName;
            Proto.write(out, Proto.obj("ok", true, "confirm",
                    Crypto.hex(Crypto.hmac(codeKey, Crypto.utf8("phone"), transcript, shared))));
            Log.i(Direct.TAG, "paired with " + pcName);
            main.post(this::announce);
        } catch (GeneralSecurityException | org.json.JSONException | IllegalArgumentException e) {
            Proto.error(out, "pairing failed: " + e.getMessage());
        }
    }

    /**
     * Mutual HMAC challenge: the phone proves it holds the key first, then the
     * PC. Returns the PC's request (with its parameters), or null.
     */
    private JSONObject authenticate(DataInputStream in, DataOutputStream out, int purpose) throws IOException {
        JSONObject hello = Proto.read(in);
        byte[] key = Direct.pcKey(this, hello.optString("pc_id"));
        if (key == null) {
            Proto.write(out, Proto.obj("error", "This PC is not paired with the phone. Pair it again.", "unpaired", true));
            return null;
        }
        byte[] nc;
        try {
            nc = Crypto.unhex(hello.optString("nonce"));
        } catch (IllegalArgumentException e) {
            return null;
        }
        byte[] ns = Crypto.random(16);
        byte[] p = {(byte) purpose};
        Proto.write(out, Proto.obj("id", Direct.deviceId(this), "nonce", Crypto.hex(ns),
                "proof", Crypto.hex(Crypto.hmac(key, Crypto.utf8("S"), nc, ns, p))));
        JSONObject req = Proto.read(in);
        byte[] proof;
        try {
            proof = Crypto.unhex(req.optString("proof"));
        } catch (IllegalArgumentException e) {
            proof = null;
        }
        if (!Crypto.same(Crypto.hmac(key, Crypto.utf8("C"), nc, ns, p), proof)) {
            Proto.error(out, "authentication failed");
            return null;
        }
        return req;
    }

    // ---- link: heartbeat, telemetry, keep screen on ------------------------------------

    private void link(Socket s, DataInputStream in, DataOutputStream out) throws IOException {
        links.add(s);
        updateLocks();
        try {
            s.setSoTimeout(LINK_TIMEOUT_MS);
            JSONObject hello = Proto.obj("ok", true, "id", Direct.deviceId(this), "model", Direct.model(),
                    "manufacturer", Build.MANUFACTURER, "sdk", Build.VERSION.SDK_INT, "release", Build.VERSION.RELEASE,
                    "input", InputService.get() != null, "capture", Projection.get() != null);
            Proto.write(out, hello);
            setStatus("Connected to the PC");
            if (Direct.stayAwake(this)) Screen.keepOn(this, true);
            while (!stopping) {
                JSONObject m;
                try {
                    m = Proto.read(in);
                } catch (SocketTimeoutException e) {
                    return;
                }
                String op = m.optString("op");
                if (op.equals("ping")) {
                    Proto.write(out, pong());
                } else if (op.equals("stay_awake")) {
                    if (m.has("on") && !m.isNull("on")) {
                        boolean on = m.optBoolean("on");
                        Direct.setStayAwake(this, on);
                        Screen.keepOn(this, on);
                    }
                    Proto.write(out, Proto.obj("op", "stay_awake", "on", Screen.keptOn()));
                }
            }
        } finally {
            links.remove(s);
            updateLocks();
            if (links.isEmpty()) {
                setStatus("Waiting for the PC");
                // The choice is remembered and applied again when the PC reconnects.
                Screen.keepOn(this, false);
            }
        }
    }

    private JSONObject pong() {
        JSONObject o = Proto.obj("op", "pong", "input", InputService.get() != null, "capture", Projection.get() != null);
        Intent bat = registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
        try {
            if (bat != null) {
                int level = bat.getIntExtra(BatteryManager.EXTRA_LEVEL, -1), scale = bat.getIntExtra(BatteryManager.EXTRA_SCALE, 100);
                if (level >= 0 && scale > 0) o.put("battery", level * 100 / scale);
                int st = bat.getIntExtra(BatteryManager.EXTRA_STATUS, -1);
                o.put("charging", st == BatteryManager.BATTERY_STATUS_CHARGING || st == BatteryManager.BATTERY_STATUS_FULL);
            }
            WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            WifiInfo wi = wm == null ? null : wm.getConnectionInfo();
            if (wi != null && wi.getRssi() > -127 && wi.getRssi() < 0) {
                o.put("rssi", wi.getRssi());
                if (wi.getLinkSpeed() > 0) o.put("link_mbps", wi.getLinkSpeed());
            }
        } catch (org.json.JSONException ignored) {
            // leave it out
        }
        return o;
    }

    // ---- mirroring -------------------------------------------------------------------

    private void stream(Socket s, JSONObject req, DataInputStream in, DataOutputStream out) throws IOException {
        if (req.optBoolean("power_on", true)) Screen.wake(this);
        MediaProjection p = Projection.get();
        if (p == null) {
            Projection.ask(this);
            // Up to Android 9, "Don't show again" makes this instant.
            for (int i = 0; i < 40 && p == null && !stopping; i++) {
                try {
                    Thread.sleep(500);
                } catch (InterruptedException e) {
                    return;
                }
                p = Projection.get();
            }
        }
        if (p == null) {
            Proto.write(out, Proto.obj("error",
                    "Allow screen capture on the phone: tap \"Start now\" (tick \"Don't show again\" if offered)",
                    "need_capture", true));
            return;
        }
        Proto.write(out, Proto.obj("ok", true, "input", InputService.get() != null));
        s.setSoTimeout(0);
        Streamer st = new Streamer(this, s, in, req.optInt("max_size", 1280), req.optInt("bit_rate", 4_000_000),
                req.optInt("max_fps", 30));
        streams.add(st);
        setStatus("Mirroring to the PC");
        try {
            st.run(p);
        } finally {
            streams.remove(st);
            setStatus(links.isEmpty() ? "Waiting for the PC" : "Connected to the PC");
        }
    }

    // ---- mDNS ----------------------------------------------------------------------------

    private void announce() {
        if (nsd == null || stopping) return;
        unannounce();
        boolean pairing = Direct.pairCode() != null;
        NsdServiceInfo info = new NsdServiceInfo();
        String id = Direct.deviceId(this);
        info.setServiceName("AC-" + Direct.model().replaceAll("[^A-Za-z0-9 _-]", "") + "-" + id.substring(Math.max(0, id.length() - 4)));
        info.setServiceType(Direct.SERVICE_TYPE);
        info.setPort(Direct.PORT);
        info.setAttribute("id", id);
        info.setAttribute("model", Direct.model());
        info.setAttribute("sdk", String.valueOf(Build.VERSION.SDK_INT));
        info.setAttribute("pair", pairing ? "1" : "0");
        nsdPairing = pairing;
        nsdListener = new NsdManager.RegistrationListener() {
            @Override
            public void onRegistrationFailed(NsdServiceInfo s, int err) {
                Log.w(Direct.TAG, "mDNS registration failed: " + err);
            }

            @Override
            public void onUnregistrationFailed(NsdServiceInfo s, int err) {}

            @Override
            public void onServiceRegistered(NsdServiceInfo s) {
                Log.i(Direct.TAG, "announced as " + s.getServiceName());
            }

            @Override
            public void onServiceUnregistered(NsdServiceInfo s) {}
        };
        try {
            nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, nsdListener);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "mDNS unavailable", e);
            nsdListener = null;
        }
    }

    private void unannounce() {
        if (nsd != null && nsdListener != null) {
            try {
                nsd.unregisterService(nsdListener);
            } catch (RuntimeException ignored) {
                // not registered
            }
        }
        nsdListener = null;
    }

    // ---- bookkeeping -------------------------------------------------------------------

    private void updateLocks() {
        if (cpuLock == null) return;
        if (links.isEmpty()) {
            cpuLock.release();
        } else if (!cpuLock.isHeld()) {
            cpuLock.acquire();
        }
    }

    private void setStatus(String s) {
        status = s;
        main.post(this::refreshNotification);
    }

    private void refreshNotification() {
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (nm != null && !stopping) nm.notify(NOTIFICATION, notification());
    }

    @SuppressWarnings("deprecation")
    private Notification notification() {
        Intent open = new Intent(this, MainActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        PendingIntent pi = PendingIntent.getActivity(this, 1, open, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        String ip = Direct.wifiAddress(this);
        String code = Direct.pairCode();
        String text = code != null ? "Pairing code " + code + (ip == null ? "" : " · " + ip + ":" + Direct.PORT)
                : status + (ip == null ? " · no Wi-Fi" : " · " + ip + ":" + Direct.PORT);
        Notification.Builder b = Keeper.isAtLeast(26) ? new Notification.Builder(this, CHANNEL) : new Notification.Builder(this);
        return b.setSmallIcon(android.R.drawable.presence_video_online)
                .setContentTitle("PC connection without ADB")
                .setContentText(text)
                .setContentIntent(pi)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setShowWhen(false)
                .build();
    }

    private static void closeQuietly(Socket s) {
        try {
            s.close();
        } catch (IOException ignored) {
            // closing
        }
    }
}
