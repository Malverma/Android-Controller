package org.androidcontroller.keeper;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.provider.Settings;
import android.view.View;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.Switch;
import android.widget.TextView;

/** Status and the few switches; everything is built in code (no resources). */
public final class MainActivity extends Activity {
    private final Handler handler = new Handler(Looper.getMainLooper());
    private TextView status;
    private TextView setup;
    private Switch keep;
    private Switch watchdog;
    private Button battery;
    private final Runnable refresh = new Runnable() {
        @Override
        public void run() {
            render();
            handler.postDelayed(this, 1_000);
        }
    };

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        int pad = dp(20);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(pad, pad, pad, pad);

        TextView title = new TextView(this);
        title.setText("Wireless Debugging Keeper");
        title.setTextSize(22);
        title.setTypeface(Typeface.DEFAULT_BOLD);
        col.addView(title);

        TextView about = new TextView(this);
        about.setText("Turns Android's Wireless debugging back on whenever Wi-Fi reconnects or the phone restarts, "
                + "so Android Controller on your PC can reconnect by itself.");
        about.setPadding(0, dp(8), 0, dp(16));
        col.addView(about);

        keep = new Switch(this);
        keep.setText("Keep wireless debugging on");
        keep.setTextSize(17);
        keep.setOnCheckedChangeListener((b, on) -> {
            if (on != Keeper.active(this)) Keeper.setActive(this, on);
        });
        col.addView(keep);

        watchdog = new Switch(this);
        watchdog.setText("Restart Wi-Fi if it stays disconnected");
        watchdog.setTextSize(15);
        watchdog.setPadding(0, dp(12), 0, 0);
        watchdog.setOnCheckedChangeListener((b, on) -> Keeper.setWatchdog(this, on));
        col.addView(watchdog);

        TextView watchdogAbout = new TextView(this);
        watchdogAbout.setText("Some routers drop phones and Android then avoids that network for minutes. "
                + "With this on, Wi-Fi is switched off and on after 45 s without a connection (waiting longer after each try).");
        watchdogAbout.setTextSize(13);
        col.addView(watchdogAbout);

        status = new TextView(this);
        status.setPadding(0, dp(16), 0, dp(16));
        status.setTextSize(15);
        col.addView(status);

        Button now = new Button(this);
        now.setText("Turn wireless debugging on now");
        now.setOnClickListener(v -> {
            Keeper.turnOn(this);
            render();
        });
        col.addView(now);

        battery = new Button(this);
        battery.setText("Don't let Android stop this app");
        battery.setOnClickListener(v -> {
            Intent i = new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:" + getPackageName()));
            try {
                startActivity(i);
            } catch (RuntimeException e) {
                startActivity(new Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS));
            }
        });
        col.addView(battery);

        Button dev = new Button(this);
        dev.setText("Open Developer options");
        dev.setOnClickListener(v -> startActivity(new Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS)));
        col.addView(dev);

        setup = new TextView(this);
        setup.setPadding(0, dp(16), 0, 0);
        setup.setTextIsSelectable(true);
        col.addView(setup);

        ScrollView scroll = new ScrollView(this);
        scroll.addView(col);
        setContentView(scroll);

        if (Keeper.isAtLeast(33) && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
        if (Keeper.active(this)) Keeper.start(this);
    }

    @Override
    protected void onResume() {
        super.onResume();
        handler.post(refresh);
    }

    @Override
    protected void onPause() {
        handler.removeCallbacks(refresh);
        super.onPause();
    }

    private void render() {
        boolean granted = Keeper.canWriteSettings(this);
        keep.setChecked(Keeper.active(this));
        watchdog.setChecked(Keeper.watchdog(this));
        watchdog.setEnabled(Keeper.active(this));
        StringBuilder s = new StringBuilder();
        s.append("Wireless debugging: ").append(Keeper.wirelessDebuggingOn(this) ? "on" : "off").append('\n');
        s.append("USB debugging: ").append(Keeper.usbDebuggingOn(this) ? "on" : "off").append('\n');
        s.append("Wi-Fi: ").append(Keeper.onWifi(this) ? "connected" : "not connected").append('\n');
        s.append("Permission to change it: ").append(granted ? "granted" : "missing");
        PowerManager pm = getSystemService(PowerManager.class);
        boolean unrestricted = pm != null && pm.isIgnoringBatteryOptimizations(getPackageName());
        s.append('\n').append("Battery restrictions: ").append(unrestricted ? "off (good)" : "on — may stop the app");
        status.setText(s);
        battery.setVisibility(unrestricted ? View.GONE : View.VISIBLE);
        setup.setText(granted ? ""
                : "One-time setup: connect the phone to the PC with USB debugging on, then run\n\n"
                        + Keeper.GRANT_COMMAND + "\n\n(or android-helper/setup.sh from the Android Controller repository).");
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }
}
