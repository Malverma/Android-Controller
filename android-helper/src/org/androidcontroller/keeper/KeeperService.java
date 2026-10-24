package org.androidcontroller.keeper;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.database.ContentObserver;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.provider.Settings;
import android.util.Log;

import java.util.ArrayDeque;

/**
 * Keeps wireless debugging on. Android switches it off whenever Wi-Fi drops
 * and after a reboot; this turns it back on as soon as Wi-Fi is connected
 * again, and whenever anything else switches it off.
 *
 * Wi-Fi watchdog: when a router drops the phone (e.g. "AP busy"), Android
 * can block that access point for many minutes and stay disconnected even
 * though Wi-Fi is on. If Wi-Fi stays switched on but disconnected, Wi-Fi is
 * restarted, which clears the block, with growing pauses between tries.
 */
public final class KeeperService extends Service {
    static final String ACTION_PAUSE = "org.androidcontroller.keeper.PAUSE";
    private static final String CHANNEL = "keeper";
    private static final int NOTIFICATION = 1;
    /** At most this many attempts per minute, so we never fight the system in a loop. */
    private static final int MAX_ATTEMPTS_PER_MINUTE = 6;
    /** Android may switch it off again while it settles on a new network. */
    private static final long[] RECHECK_MS = {2_000, 8_000, 30_000};
    /** Disconnected this long (with Wi-Fi switched on) before restarting Wi-Fi. */
    private static final long WATCHDOG_AFTER_MS = 45_000;
    /** Pause after each restart that did not help: 1, 2, 4, 8, then 15 min. */
    private static final long WATCHDOG_MIN_BACKOFF_MS = 60_000;
    private static final long WATCHDOG_MAX_BACKOFF_MS = 15 * 60_000;
    /** Connected this long means the last restart worked: back to quick retries. */
    private static final long WATCHDOG_STABLE_MS = 5 * 60_000;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final ArrayDeque<Long> attempts = new ArrayDeque<>();
    private ConnectivityManager connectivity;
    private ContentObserver observer;
    private ConnectivityManager.NetworkCallback networkCallback;
    private String lastText = "";
    private boolean lastFailed;
    /** Wi-Fi networks currently connected (from the callback). */
    private int wifiNetworks;
    private long disconnectedSince = System.currentTimeMillis();
    private long connectedSince;
    private long nextRestartAt;
    private long backoffMs = WATCHDOG_MIN_BACKOFF_MS;
    private int restarts;

    private final Runnable ensure = this::ensure;
    private final Runnable watchdog = this::watchdog;

    @Override
    public void onCreate() {
        super.onCreate();
        if (Keeper.isAtLeast(26)) {
            NotificationManager nm = getSystemService(NotificationManager.class);
            NotificationChannel ch = new NotificationChannel(CHANNEL, "Wireless debugging", NotificationManager.IMPORTANCE_LOW);
            ch.setDescription("Shows whether wireless debugging is being kept on");
            ch.setShowBadge(false);
            nm.createNotificationChannel(ch);
        }
        Notification n = build("Starting…");
        if (Keeper.isAtLeast(34)) {
            startForeground(NOTIFICATION, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION, n);
        }

        observer = new ContentObserver(handler) {
            @Override
            public void onChange(boolean selfChange) {
                schedule(500);
            }
        };
        getContentResolver().registerContentObserver(Settings.Global.getUriFor(Keeper.ADB_WIFI_ENABLED), false, observer);
        getContentResolver().registerContentObserver(Settings.Global.getUriFor(Settings.Global.ADB_ENABLED), false, observer);

        connectivity = getSystemService(ConnectivityManager.class);
        networkCallback = new ConnectivityManager.NetworkCallback() {
            @Override
            public void onAvailable(Network network) {
                handler.post(() -> {
                    wifiNetworks++;
                    connectedSince = System.currentTimeMillis();
                    attempts.clear(); // a new network deserves fresh attempts
                    schedule(1_000);
                });
            }

            @Override
            public void onLost(Network network) {
                handler.post(() -> {
                    wifiNetworks = Math.max(0, wifiNetworks - 1);
                    if (wifiNetworks == 0) {
                        // A connection that lasted a while means Wi-Fi works again.
                        if (connectedSince > 0 && System.currentTimeMillis() - connectedSince > WATCHDOG_STABLE_MS) {
                            backoffMs = WATCHDOG_MIN_BACKOFF_MS;
                            restarts = 0;
                        }
                        disconnectedSince = System.currentTimeMillis();
                    }
                    schedule(500);
                });
            }
        };
        NetworkRequest wifi = new NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build();
        connectivity.registerNetworkCallback(wifi, networkCallback);
        handler.postDelayed(watchdog, WATCHDOG_AFTER_MS);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_PAUSE.equals(intent.getAction())) {
            Keeper.setActive(this, false);
            stopSelf();
            return START_NOT_STICKY;
        }
        schedule(0);
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        handler.removeCallbacksAndMessages(null);
        if (observer != null) getContentResolver().unregisterContentObserver(observer);
        if (networkCallback != null) connectivity.unregisterNetworkCallback(networkCallback);
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    private void schedule(long delayMs) {
        handler.removeCallbacks(ensure);
        handler.postDelayed(ensure, delayMs);
    }

    private void ensure() {
        if (!Keeper.hasWirelessDebugging()) {
            // Android 10 and older: no wireless debugging; the PC reaches the
            // phone with adb tcpip, so only keeping Wi-Fi connected matters.
            show(Keeper.onWifi(this) ? "Keeping Wi-Fi connected (this Android has no wireless debugging)"
                    : "Waiting for Wi-Fi to connect", false);
            return;
        }
        if (!Keeper.canWriteSettings(this)) {
            show("Needs one-time setup from the PC (tap for details)", true);
            return;
        }
        if (!Keeper.onWifi(this)) {
            show(Keeper.wifiSwitchedOn(this) ? "Waiting for Wi-Fi to connect" : "Wi-Fi is off", false);
            return;
        }
        if (Keeper.wirelessDebuggingOn(this)) {
            show("Wireless debugging is on", false);
            return;
        }
        long now = System.currentTimeMillis();
        while (!attempts.isEmpty() && now - attempts.peekFirst() > 60_000) attempts.removeFirst();
        if (attempts.size() >= MAX_ATTEMPTS_PER_MINUTE) {
            // Android keeps refusing: usually this Wi-Fi network was never
            // allowed ("Always allow on this network") for wireless debugging.
            show("Android keeps turning wireless debugging off. Turn it on once by hand on this Wi-Fi and tick \"Always allow on this network\"", true);
            schedule(60_000);
            return;
        }
        attempts.addLast(now);
        Keeper.turnOn(this);
        show("Turning wireless debugging on…", false);
        for (long ms : RECHECK_MS) handler.postDelayed(ensure, ms);
    }

    /** Restarts Wi-Fi when it is switched on but has not connected for a while. */
    private void watchdog() {
        handler.postDelayed(watchdog, 15_000);
        long now = System.currentTimeMillis();
        if (!Keeper.active(this) || !Keeper.watchdog(this) || Keeper.onWifi(this)) {
            if (Keeper.onWifi(this)) disconnectedSince = now;
            return;
        }
        // Wi-Fi switched off by the user (or airplane mode): leave it alone.
        if (!Keeper.wifiSwitchedOn(this)) {
            disconnectedSince = now;
            return;
        }
        if (now - disconnectedSince < WATCHDOG_AFTER_MS || now < nextRestartAt) return;
        restarts++;
        Log.i(Keeper.TAG, "Wi-Fi on but disconnected for " + (now - disconnectedSince) / 1000 + " s: restarting Wi-Fi (try " + restarts + ")");
        show("Wi-Fi did not reconnect: restarting Wi-Fi (try " + restarts + ")", false);
        nextRestartAt = now + backoffMs;
        backoffMs = Math.min(backoffMs * 2, WATCHDOG_MAX_BACKOFF_MS);
        if (Keeper.setWifi(this, false)) {
            handler.postDelayed(() -> Keeper.setWifi(this, true), 3_000);
        } else {
            show("Wi-Fi did not reconnect, and Android refused to restart it", true);
        }
    }

    private void show(String text, boolean problem) {
        if (text.equals(lastText) && problem == lastFailed) return;
        lastText = text;
        lastFailed = problem;
        Log.i(Keeper.TAG, text);
        getSystemService(NotificationManager.class).notify(NOTIFICATION, build(text));
    }

    private Notification build(String text) {
        PendingIntent open = PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent pause = PendingIntent.getService(this, 1, new Intent(this, KeeperService.class).setAction(ACTION_PAUSE),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        @SuppressWarnings("deprecation")
        Notification.Builder b = Keeper.isAtLeast(26) ? new Notification.Builder(this, CHANNEL) : new Notification.Builder(this);
        return b
                .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
                .setContentTitle("Wireless Debugging Keeper")
                .setContentText(text)
                .setStyle(new Notification.BigTextStyle().bigText(text))
                .setContentIntent(open)
                .addAction(new Notification.Action.Builder(null, "Pause", pause).build())
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setCategory(Notification.CATEGORY_SERVICE)
                .build();
    }
}
