/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.keyguard;

import android.app.Service;
import android.content.Intent;
import android.os.Bundle;
import android.os.IBinder;
import android.os.RemoteException;
import android.os.SystemProperties;
import com.android.internal.policy.*;
import com.android.internal.widget.LockPatternUtils;
import com.android.systemui.Unsupported;
import com.android.systemui.compat.EmptyKeyguard;
import java.io.FileDescriptor;
import java.io.PrintWriter;
import java.util.ArrayList;

/**
 * Native desktop owns locking. The Android credential Droidloom manages is verified by the
 * framework at each start (DroidloomCredential); until then, or with a credential the user set
 * in Android, the cell is reported locked.
 */
public final class KeyguardService extends Service {
    private volatile int userId;
    private LockPatternUtils locks;
    private final ArrayList<IKeyguardStateCallback> callbacks = new ArrayList<>();
    private boolean locked() {
        return locks.isSecure(userId)
                && !(userId == 0 && "verified".equals(SystemProperties.get("sys.droidloom.credential")));
    }
    private void state(IKeyguardStateCallback callback) throws RemoteException {
        if (callback == null) return;
        boolean locked = locked();
        if (locked) Unsupported.report("Android credential lock");
        callback.onShowingStateChanged(locked, userId);
        callback.onInputRestrictedStateChanged(locked);
        callback.onTrustedChanged(false);
        callback.onSimSecureStateChanged(false);
    }
    private void monitor(IKeyguardStateCallback callback) throws RemoteException {
        if (callback == null) return;
        synchronized (callbacks) { if (!callbacks.contains(callback)) callbacks.add(callback); }
        state(callback);
    }
    private final EmptyKeyguard binder = new EmptyKeyguard() {
        @Override public void addStateMonitorCallback(IKeyguardStateCallback callback)
                throws RemoteException { monitor(callback); }
        @Override public void verifyUnlock(IKeyguardExitCallback callback) throws RemoteException {
            if (callback != null) callback.onKeyguardExitResult(!locked());
        }
        @Override public void dismiss(IKeyguardDismissCallback callback, CharSequence message)
                throws RemoteException {
            if (callback != null) {
                if (locked()) callback.onDismissError(); else callback.onDismissSucceeded();
            }
        }
        @Override public void setCurrentUser(int currentUser) { userId = currentUser; }
        @Override public void onScreenTurningOn(int reason, IKeyguardDrawnCallback callback)
                throws RemoteException { if (callback != null) callback.onDrawn(); }
        @Override public void restoreKeyguardState(KeyguardState restored,
                IKeyguardStateCallback callback, IKeyguardDrawnCallback drawn,
                boolean timeoutRequested, Bundle options) throws RemoteException {
            userId = restored.userId;
            monitor(callback);
            if (drawn != null) drawn.onDrawn();
        }
    };
    @Override public void onCreate() {
        super.onCreate();
        locks = new LockPatternUtils(this);
        SystemProperties.addChangeCallback(this::announce);
    }
    /** Resends the state when the credential is verified, or the framework restarts. */
    private void announce() {
        synchronized (callbacks) {
            callbacks.removeIf(callback -> {
                try { state(callback); return false; } catch (RemoteException gone) { return true; }
            });
        }
    }
    @Override public IBinder onBind(Intent intent) { return binder; }
    @Override protected void dump(FileDescriptor fd, PrintWriter out, String[] args) {
        out.println("Droidloom keyguard: user=" + userId + " androidCredentialConfigured=" + locks.isSecure(userId)
                + " locked=" + locked());
    }
}
