package org.androidcontroller.keeper;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Build;
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
    private Switch direct;
    private TextView directStatus;
    private TextView pairView;
    private Button pair;
    private Button touch;
    private Button capture;
    private Button storage;
    private Button forget;
    private final Runnable refresh = new Runnable() {
        @Override
        public void run() {
            render();
            handler.postDelayed(this, 1_000);
        }
    };

    @Override
    protected void onCreate(Bundle saved) {
        if (Keeper.isAtLeast(29)) setTheme(android.R.style.Theme_DeviceDefault_DayNight);
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

        addDirectSection(col);

        ScrollView scroll = new ScrollView(this);
        scroll.addView(col);
        setContentView(scroll);

        if (Keeper.isAtLeast(33) && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
        if (Keeper.active(this)) Keeper.start(this);
        if (Direct.enabled(this)) Direct.start(this);
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
        if (!Keeper.hasWirelessDebugging()) {
            s.append("This phone runs Android ").append(Build.VERSION.RELEASE)
                    .append(", which has no wireless debugging (it needs Android 11). Either connect it once by USB and use ")
                    .append("\"Enable Wi-Fi ADB\" in Android Controller (this app then keeps Wi-Fi connected), or turn on ")
                    .append("\"PC connection without ADB\" below.\n\n");
        }
        s.append("Wireless debugging: ").append(Keeper.wirelessDebuggingOn(this) ? "on" : "off").append('\n');
        s.append("USB debugging: ").append(Keeper.usbDebuggingOn(this) ? "on" : "off").append('\n');
        s.append("Wi-Fi: ").append(Keeper.onWifi(this) ? "connected" : "not connected").append('\n');
        s.append("Permission to change it: ").append(granted ? "granted" : "missing");
        PowerManager pm = getSystemService(PowerManager.class);
        boolean unrestricted = pm != null && pm.isIgnoringBatteryOptimizations(getPackageName());
        s.append('\n').append("Battery restrictions: ").append(unrestricted ? "off (good)" : "on — may stop the app");
        status.setText(s);
        battery.setVisibility(unrestricted ? View.GONE : View.VISIBLE);
        setup.setText(granted || !Keeper.hasWirelessDebugging() ? ""
                : "One-time setup: connect the phone to the PC with USB debugging on, then run\n\n"
                        + Keeper.GRANT_COMMAND + "\n\n(or android-helper/setup.sh from the Android Controller repository).");
        renderDirect();
    }

    /** "PC connection without ADB": for phones with no wireless debugging and no USB cable. */
    private void addDirectSection(LinearLayout col) {
        TextView head = new TextView(this);
        head.setText("PC connection without ADB");
        head.setTextSize(19);
        head.setTypeface(Typeface.DEFAULT_BOLD);
        head.setPadding(0, dp(28), 0, 0);
        col.addView(head);

        TextView about = new TextView(this);
        about.setText("For phones without wireless debugging (Android 10 and older) and no USB cable: Android Controller "
                + "mirrors and controls this phone over Wi-Fi through this app. Taps, swipes and typing go through "
                + "Android's accessibility service. Phones with wireless debugging don't need this.");
        about.setTextSize(13);
        about.setPadding(0, dp(6), 0, dp(10));
        col.addView(about);

        direct = new Switch(this);
        direct.setText("Allow PC connection without ADB");
        direct.setTextSize(17);
        direct.setOnCheckedChangeListener((b, on) -> {
            if (on == Direct.enabled(this)) return;
            Direct.setEnabled(this, on);
            if (on && checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
                requestPermissions(new String[] {Manifest.permission.WRITE_EXTERNAL_STORAGE}, 2);
            }
            render();
        });
        col.addView(direct);

        directStatus = new TextView(this);
        directStatus.setPadding(0, dp(10), 0, dp(6));
        directStatus.setTextSize(15);
        directStatus.setTextIsSelectable(true);
        col.addView(directStatus);

        pairView = new TextView(this);
        pairView.setTextSize(15);
        pairView.setTextIsSelectable(true);
        pairView.setPadding(0, 0, 0, dp(6));
        col.addView(pairView);

        pair = new Button(this);
        pair.setOnClickListener(v -> {
            if (Direct.pairCode() != null) {
                Direct.closePairing();
            } else {
                Direct.openPairing();
            }
            render();
        });
        col.addView(pair);

        touch = new Button(this);
        touch.setText("Turn on touch control (Accessibility)");
        touch.setOnClickListener(v -> startActivity(new Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)));
        col.addView(touch);

        capture = new Button(this);
        capture.setText("Allow screen capture");
        capture.setOnClickListener(v -> Projection.ask(this));
        col.addView(capture);

        storage = new Button(this);
        storage.setText("Allow receiving files");
        storage.setOnClickListener(v -> requestPermissions(new String[] {Manifest.permission.WRITE_EXTERNAL_STORAGE}, 2));
        col.addView(storage);

        forget = new Button(this);
        forget.setText("Forget paired PCs");
        forget.setOnClickListener(v -> {
            Direct.forgetPcs(this);
            render();
        });
        col.addView(forget);
    }

    private void renderDirect() {
        boolean on = Direct.enabled(this);
        direct.setChecked(on);
        String ip = Direct.wifiAddress(this);
        boolean input = InputService.enabled(this);
        boolean files = checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED;
        StringBuilder s = new StringBuilder();
        if (on) {
            s.append("Status: ").append(DirectService.statusLine()).append('\n');
            s.append("Address: ").append(ip == null ? "no Wi-Fi" : ip + ":" + Direct.PORT).append('\n');
            s.append("Touch control: ").append(input ? "on" : "off — the PC can only watch").append('\n');
            s.append("Screen capture: ").append(Projection.get() != null ? "allowed" : "asked when the PC connects").append('\n');
            s.append("Receiving files: ").append(files ? "allowed" : "not allowed").append('\n');
            int n = Direct.pcCount(this);
            s.append("Paired PCs: ").append(n == 0 ? "none yet — tap Pair with PC" : Direct.pcNames(this));
        }
        directStatus.setText(s);
        directStatus.setVisibility(on ? View.VISIBLE : View.GONE);
        String code = Direct.pairCode();
        if (on && code != null) {
            pairView.setText("Pairing code:  " + code.substring(0, 3) + " " + code.substring(3) + "\n"
                    + "In Android Controller on the PC: Add Device › No ADB. Pick this phone (or type "
                    + (ip == null ? "its IP" : ip + ":" + Direct.PORT) + ") and enter the code. "
                    + Direct.pairSecondsLeft() + " s left.");
            pairView.setTextSize(17);
        } else if (on && Direct.lastPaired != null) {
            pairView.setText("Paired with " + Direct.lastPaired + ".");
            pairView.setTextSize(15);
        } else {
            pairView.setText("");
        }
        pairView.setVisibility(pairView.getText().length() > 0 ? View.VISIBLE : View.GONE);
        pair.setText(code != null ? "Stop pairing" : "Pair with PC");
        for (View v : new View[] {pair, forget}) v.setVisibility(on ? View.VISIBLE : View.GONE);
        touch.setVisibility(on && !input ? View.VISIBLE : View.GONE);
        capture.setVisibility(on && Projection.get() == null ? View.VISIBLE : View.GONE);
        storage.setVisibility(on && !files ? View.VISIBLE : View.GONE);
        forget.setEnabled(Direct.pcCount(this) > 0);
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }
}
