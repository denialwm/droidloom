/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.wifi;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.SharedPreferences;
import android.net.wifi.WifiConfiguration;
import android.net.wifi.WifiManager;
import android.os.Handler;
import android.os.Looper;
import android.os.UserManager;
import android.util.Log;

/**
 * Joins the cell to Droidloom's access point once: saves the open network
 * "Droidloom" and turns Wi-Fi on. Cells from before Wi-Fi had it off and no
 * network saved. After that the user's choices stand, as on any phone.
 */
public final class WifiProvisioning {
    private static final String TAG = "DroidloomWifi";
    private static final String SSID = "\"Droidloom\"";
    private static final String DONE = "wifi_provisioned";
    // Wi-Fi reads its saved networks after the user unlocks; until then
    // addNetwork fails, so a few attempts follow the unlock.
    private static final long RETRY_MS = 2000;
    private static final int ATTEMPTS = 30;

    private final Context context;
    private final Handler main = new Handler(Looper.getMainLooper());
    private final SharedPreferences preferences;
    private BroadcastReceiver unlocked;
    private int attempts;

    public WifiProvisioning(Context context) {
        this.context = context;
        preferences = context.getSharedPreferences("droidloom-wifi", Context.MODE_PRIVATE);
    }

    public void start() {
        if (preferences.getBoolean(DONE, false)) return;
        if (context.getSystemService(UserManager.class).isUserUnlocked()) {
            main.post(this::attempt);
            return;
        }
        unlocked = new BroadcastReceiver() {
            @Override public void onReceive(Context context, Intent intent) {
                close();
                attempt();
            }
        };
        context.registerReceiver(unlocked, new IntentFilter(Intent.ACTION_USER_UNLOCKED),
                Context.RECEIVER_NOT_EXPORTED);
    }

    public void close() {
        if (unlocked != null) context.unregisterReceiver(unlocked);
        unlocked = null;
        main.removeCallbacksAndMessages(null);
    }

    private void attempt() {
        WifiManager wifi = context.getSystemService(WifiManager.class);
        WifiConfiguration config = new WifiConfiguration();
        config.SSID = SSID;
        config.setSecurityParams(WifiConfiguration.SECURITY_TYPE_OPEN);
        // The link's address is the cell's own; a random one would only be
        // refused by the missing vendor HAL.
        config.macRandomizationSetting = WifiConfiguration.RANDOMIZATION_NONE;
        if (wifi.addNetwork(config) < 0) {
            if (++attempts < ATTEMPTS) {
                main.postDelayed(this::attempt, RETRY_MS);
            } else {
                Log.e(TAG, "Wi-Fi did not accept the Droidloom network");
            }
            return;
        }
        wifi.setWifiEnabled(true);
        preferences.edit().putBoolean(DONE, true).apply();
        Log.i(TAG, "Wi-Fi on, network Droidloom saved");
    }
}
