/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.os.RemoteException;
import android.os.ServiceManager;
import android.os.UserHandle;
import android.util.Log;
import com.android.internal.statusbar.IStatusBarService;
import java.io.FileDescriptor;
import java.io.PrintWriter;

/** The entire resident UI service: a Binder endpoint with no windows or scheduled work. */
public class SystemUIService extends Service {
    private StatusBarCallbacks callbacks;
    private com.android.systemui.notifications.NotificationBridge notifications;
    private com.android.systemui.clipboard.ClipboardBridge clipboard;
    @Override public void onCreate() {
        super.onCreate();
        if (UserHandle.myUserId() != 0) return;
        callbacks = new StatusBarCallbacks();
        try {
            IStatusBarService.Stub.asInterface(ServiceManager.getService("statusbar"))
                    .registerStatusBar(callbacks);
        } catch (RemoteException e) {
            throw e.rethrowFromSystemServer();
        }
        clipboard = new com.android.systemui.clipboard.ClipboardBridge(this);
        clipboard.start();
        notifications = new com.android.systemui.notifications.NotificationBridge(this);
        notifications.start();
        Log.i("DroidloomSystemUI", "DROIDLOOM_SYSTEMUI_ABI=1; status bar registered; no windows");
    }
    @Override public int onStartCommand(Intent intent, int flags, int startId) {
        return START_STICKY;
    }
    @Override public void onDestroy() {
        if (clipboard != null) clipboard.close();
        if (notifications != null) notifications.close();
        super.onDestroy();
    }
    @Override public IBinder onBind(Intent intent) { return null; }
    @Override protected void dump(FileDescriptor fd, PrintWriter out, String[] args) {
        out.println("DROIDLOOM_SYSTEMUI_ABI=1;");
        out.println("statusBarRegistered=" + (callbacks != null));
        out.println("windows=0 timers=0 notificationRenderer=desktop-dbus");
        if (clipboard != null) clipboard.dump(out);
        if (notifications != null) notifications.dump(out);
    }
}
