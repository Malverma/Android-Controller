package org.androidcontroller.keeper;

import android.Manifest;
import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageInstaller;
import android.content.pm.PackageManager;
import android.media.MediaScannerConnection;
import android.net.Uri;
import android.os.Environment;
import android.provider.Settings;
import android.util.Log;

import org.json.JSONObject;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * A PUSH connection: one file into Download (resumable), or one APK
 * installed through Android's installer (the user confirms on the phone).
 */
final class Receiver {
    private static final String ACTION_INSTALLED = "org.androidcontroller.keeper.INSTALLED";
    private static final long INSTALL_WAIT_MIN = 5;

    private Receiver() {}

    static void handle(Context ctx, JSONObject req, DataInputStream in, DataOutputStream out) throws IOException {
        try {
            handleFile(ctx, req, in, out);
        } catch (java.io.EOFException e) {
            throw e; // the PC went away
        } catch (IOException e) {
            Log.w(Direct.TAG, "receiving a file failed", e);
            Proto.error(out, "The phone could not save the file: " + e.getMessage());
        }
    }

    private static void handleFile(Context ctx, JSONObject req, DataInputStream in, DataOutputStream out) throws IOException {
        String name = cleanName(req.optString("name"));
        long size = req.optLong("size", -1);
        String job = req.optString("job").replaceAll("[^0-9a-fA-F]", "");
        boolean install = req.optBoolean("install");
        if (name == null || size < 0 || job.isEmpty()) {
            Proto.error(out, "bad file request");
            return;
        }
        if (install) {
            installApk(ctx, name, size, job, in, out);
            return;
        }
        if (ctx.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED) {
            Proto.error(out, "Allow Wireless Debugging Keeper to store files: open it on the phone and tap Allow");
            return;
        }
        File dir = new File(Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS),
                cleanDir(req.optString("dir")));
        if (!dir.isDirectory() && !dir.mkdirs()) {
            Proto.error(out, "The phone refused to create " + dir);
            return;
        }
        String conflict = req.optString("conflict", "rename");
        File target = new File(dir, name);
        if (target.exists() && conflict.equals("skip")) {
            Proto.write(out, Proto.obj("ok", true, "skip", true));
            return;
        }
        File part = new File(dir, ".ac-" + job + ".part");
        long offset = part.isFile() ? Math.min(part.length(), size) : 0;
        if (dir.getUsableSpace() < size - offset + (64L << 20)) {
            Proto.error(out, "Not enough free space on the phone");
            return;
        }
        Proto.write(out, Proto.obj("ok", true, "offset", offset));
        String sha256 = receive(in, part, offset, size);
        if (target.exists()) {
            if (conflict.equals("overwrite")) {
                if (!target.delete()) throw new IOException("cannot replace " + target);
            } else {
                target = freeName(dir, name);
            }
        }
        if (!part.renameTo(target)) throw new IOException("cannot rename to " + target);
        MediaScannerConnection.scanFile(ctx, new String[] {target.getAbsolutePath()}, null, null);
        // Hashed while it arrived: Android 11+ may not let us read it back
        // once it is a public download.
        Proto.write(out, Proto.obj("ok", true, "path", target.getAbsolutePath(), "sha256", sha256));
    }

    /**
     * Writes bytes [offset, size) from the PC into {@code part}; returns the
     * SHA-256 of the whole file.
     */
    private static String receive(DataInputStream in, File part, long offset, long size) throws IOException {
        byte[] buf = new byte[1 << 16];
        MessageDigest md;
        try {
            md = MessageDigest.getInstance("SHA-256");
        } catch (NoSuchAlgorithmException e) {
            throw new IOException(e);
        }
        try (RandomAccessFile f = new RandomAccessFile(part, "rw")) {
            f.setLength(offset);
            long have = 0;
            while (have < offset) {
                int n = f.read(buf, 0, (int) Math.min(buf.length, offset - have));
                if (n < 0) throw new IOException("part file changed");
                md.update(buf, 0, n);
                have += n;
            }
            f.seek(offset);
            long left = size - offset;
            while (left > 0) {
                int n = in.read(buf, 0, (int) Math.min(buf.length, left));
                if (n < 0) throw new java.io.EOFException("the PC stopped sending");
                f.write(buf, 0, n);
                md.update(buf, 0, n);
                left -= n;
            }
            f.getFD().sync();
        }
        return Crypto.hex(md.digest());
    }

    private static void installApk(Context ctx, String name, long size, String job, DataInputStream in,
            DataOutputStream out) throws IOException {
        if (Keeper.isAtLeast(26) && !ctx.getPackageManager().canRequestPackageInstalls()) {
            Intent i = new Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:" + ctx.getPackageName()))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            try {
                ctx.startActivity(i);
            } catch (RuntimeException ignored) {
                // the user can find it in Settings
            }
            Proto.error(out, "Allow Wireless Debugging Keeper to install apps on the phone (Install unknown apps), then send again");
            return;
        }
        File dir = new File(ctx.getCacheDir(), "install");
        if (!dir.isDirectory() && !dir.mkdirs()) {
            Proto.error(out, "The phone refused to store the app");
            return;
        }
        File part = new File(dir, job + ".apk");
        long offset = part.isFile() ? Math.min(part.length(), size) : 0;
        Proto.write(out, Proto.obj("ok", true, "offset", offset));
        receive(in, part, offset, size);
        try {
            String result = install(ctx, part, name);
            if (result == null) {
                Proto.write(out, Proto.obj("ok", true, "installed", true));
            } else {
                Proto.error(out, result);
            }
        } finally {
            part.delete();
        }
    }

    /** Runs Android's installer on {@code apk}; null on success, else why not. */
    private static String install(Context ctx, File apk, String name) throws IOException {
        PackageInstaller pi = ctx.getPackageManager().getPackageInstaller();
        PackageInstaller.SessionParams params = new PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL);
        int id = pi.createSession(params);
        final String[] result = {"the phone did not answer the install"};
        CountDownLatch done = new CountDownLatch(1);
        BroadcastReceiver r = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent intent) {
                if (intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1) != id) return;
                int status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
                if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
                    Intent confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT);
                    if (confirm != null) confirm(c, confirm, name);
                    return;
                }
                if (status == PackageInstaller.STATUS_SUCCESS) {
                    result[0] = null;
                } else if (status == PackageInstaller.STATUS_FAILURE_ABORTED) {
                    result[0] = "Install of " + name + " was cancelled on the phone";
                } else {
                    String msg = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE);
                    result[0] = "Install failed" + (msg == null ? "" : ": " + msg);
                }
                done.countDown();
            }
        };
        ctx.registerReceiver(r, new IntentFilter(ACTION_INSTALLED));
        try (PackageInstaller.Session s = pi.openSession(id)) {
            try (OutputStream o = s.openWrite("base.apk", 0, apk.length()); InputStream i = new FileInputStream(apk)) {
                byte[] buf = new byte[1 << 16];
                int n;
                while ((n = i.read(buf)) > 0) o.write(buf, 0, n);
                s.fsync(o);
            }
            int flags = PendingIntent.FLAG_UPDATE_CURRENT | (Keeper.isAtLeast(31) ? 0x02000000 /* FLAG_MUTABLE */ : 0);
            Intent cb = new Intent(ACTION_INSTALLED).setPackage(ctx.getPackageName());
            s.commit(PendingIntent.getBroadcast(ctx, id, cb, flags).getIntentSender());
            if (!done.await(INSTALL_WAIT_MIN, TimeUnit.MINUTES)) return "Nobody confirmed the install on the phone";
            return result[0];
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return "interrupted";
        } finally {
            ctx.unregisterReceiver(r);
            NotificationManager nm = ctx.getSystemService(NotificationManager.class);
            if (nm != null) nm.cancel(id);
        }
    }

    /** Shows Android's "Install this app?" screen (a notification if it can't open). */
    @SuppressWarnings("deprecation")
    private static void confirm(Context c, Intent confirm, String name) {
        confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            c.startActivity(confirm);
        } catch (RuntimeException e) {
            Log.w(Direct.TAG, "cannot open the installer", e);
        }
        if (!Keeper.isAtLeast(29)) return;
        // Android 10+ may block screens opened from the background.
        NotificationManager nm = c.getSystemService(NotificationManager.class);
        if (nm == null) return;
        PendingIntent open = PendingIntent.getActivity(c, 7, confirm,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification n = new Notification.Builder(c, DirectService.CHANNEL)
                .setSmallIcon(android.R.drawable.stat_sys_download_done)
                .setContentTitle("Install " + name + "?")
                .setContentText("Sent from Android Controller. Tap to install.")
                .setContentIntent(open)
                .setAutoCancel(true)
                .build();
        nm.notify(name.hashCode(), n);
    }

    /** A plain file name, or null for anything that could escape the folder. */
    static String cleanName(String s) {
        if (s == null) return null;
        s = s.trim();
        if (s.isEmpty() || s.equals(".") || s.equals("..") || s.contains("/") || s.contains("\0")) return null;
        return s;
    }

    /** A relative folder below Download, without "." or ".." parts. */
    static String cleanDir(String s) {
        StringBuilder out = new StringBuilder();
        if (s == null) return "";
        for (String part : s.split("/")) {
            if (cleanName(part) == null) continue;
            if (out.length() > 0) out.append('/');
            out.append(part.trim());
        }
        return out.toString();
    }

    /** "name (1).ext", "name (2).ext", … */
    static File freeName(File dir, String name) {
        int dot = name.lastIndexOf('.');
        String stem = dot > 0 ? name.substring(0, dot) : name;
        String ext = dot > 0 ? name.substring(dot) : "";
        for (int i = 1; ; i++) {
            File f = new File(dir, stem + " (" + i + ")" + ext);
            if (!f.exists()) return f;
        }
    }

}
