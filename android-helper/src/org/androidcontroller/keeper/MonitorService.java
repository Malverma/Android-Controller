package org.androidcontroller.keeper;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.net.wifi.WifiManager;
import android.os.IBinder;
import android.os.PowerManager;
import android.util.Log;

/** Keeps the phone and its Wi-Fi at full speed while it is a monitor. */
public final class MonitorService extends Service {
    static final String ACTION_EXIT = "org.androidcontroller.keeper.MONITOR_EXIT";
    private static final String CHANNEL = "monitor";
    private static final int NOTIFICATION = 3;
    /** WifiManager.WIFI_MODE_FULL_LOW_LATENCY, from Android 10. */
    private static final int WIFI_LOW_LATENCY = 4;

    private WifiManager.WifiLock wifiLock;
    private PowerManager.WakeLock cpuLock;

    static void start(Context c) {
        Intent i = new Intent(c, MonitorService.class);
        try {
            if (Keeper.isAtLeast(26)) {
                c.startForegroundService(i);
            } else {
                c.startService(i);
            }
        } catch (RuntimeException e) {
            Log.w(Keeper.TAG, "cannot start the monitor service", e);
        }
    }

    static void stop(Context c) {
        c.stopService(new Intent(c, MonitorService.class));
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        if (Keeper.isAtLeast(26)) {
            NotificationChannel ch = new NotificationChannel(CHANNEL, "PC monitor", NotificationManager.IMPORTANCE_LOW);
            ch.setShowBadge(false);
            getSystemService(NotificationManager.class).createNotificationChannel(ch);
        }
        Notification n = notification();
        if (Keeper.isAtLeast(34)) {
            startForeground(NOTIFICATION, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION, n);
        }
        WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wm != null) {
            int mode = Keeper.isAtLeast(29) ? WIFI_LOW_LATENCY : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
            wifiLock = wm.createWifiLock(mode, "keeper:monitor");
            wifiLock.setReferenceCounted(false);
            wifiLock.acquire();
        }
        PowerManager pm = getSystemService(PowerManager.class);
        if (pm != null) {
            cpuLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "keeper:monitor");
            cpuLock.setReferenceCounted(false);
            cpuLock.acquire();
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_EXIT.equals(intent.getAction())) {
            Monitor.userToggle(this, false);
            return START_NOT_STICKY;
        }
        // Not sticky: without a PC there is nothing to show after a restart.
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        if (wifiLock != null) wifiLock.release();
        if (cpuLock != null) cpuLock.release();
        super.onDestroy();
    }

    private Notification notification() {
        PendingIntent open = PendingIntent.getActivity(this, 0, new Intent(this, MonitorActivity.class),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent exit = PendingIntent.getService(this, 1, new Intent(this, MonitorService.class).setAction(ACTION_EXIT),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        @SuppressWarnings("deprecation")
        Notification.Builder b = Keeper.isAtLeast(26) ? new Notification.Builder(this, CHANNEL) : new Notification.Builder(this);
        return b
                .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
                .setContentTitle("Showing your PC")
                .setContentText("This phone is a monitor for Test Bed Multi")
                .setContentIntent(open)
                .addAction(new Notification.Action.Builder(null, "Exit Monitor", exit).build())
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setCategory(Notification.CATEGORY_SERVICE)
                .build();
    }
}
