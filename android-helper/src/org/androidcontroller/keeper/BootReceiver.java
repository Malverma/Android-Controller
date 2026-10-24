package org.androidcontroller.keeper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Starts the keeper after a reboot or an update of this app, or when setup asks. */
public final class BootReceiver extends BroadcastReceiver {
    static final String ACTION_START = "org.androidcontroller.keeper.START";

    @Override
    public void onReceive(Context context, Intent intent) {
        String a = intent.getAction();
        if (ACTION_START.equals(a)) {
            Keeper.setActive(context, true);
            return;
        }
        if ((Intent.ACTION_BOOT_COMPLETED.equals(a) || Intent.ACTION_MY_PACKAGE_REPLACED.equals(a))
                && Keeper.active(context)) {
            Keeper.start(context);
        }
    }
}
