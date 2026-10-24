package org.androidcontroller.keeper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Starts the keeper (and the no-ADB PC connection) after a reboot or an update, or when setup asks. */
public final class BootReceiver extends BroadcastReceiver {
    static final String ACTION_START = "org.androidcontroller.keeper.START";

    @Override
    public void onReceive(Context context, Intent intent) {
        String a = intent.getAction();
        if (ACTION_START.equals(a)) {
            Keeper.setActive(context, true);
            return;
        }
        if (Intent.ACTION_BOOT_COMPLETED.equals(a) || Intent.ACTION_MY_PACKAGE_REPLACED.equals(a)) {
            if (Keeper.active(context)) Keeper.start(context);
            if (Direct.enabled(context)) Direct.start(context);
        }
    }
}
