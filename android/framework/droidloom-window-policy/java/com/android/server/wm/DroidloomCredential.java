/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.app.ActivityManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.IBinder;
import android.os.Parcel;
import android.os.SystemProperties;
import android.util.Slog;
import com.android.internal.widget.LockPatternUtils;
import com.android.internal.widget.LockscreenCredential;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Android's fingerprint sensor and its auth-bound keys need an Android credential. Droidloom
 * sets a random password nobody types and verifies it at each start, which unlocks user 0 and
 * keeps its secure user ID. Droidloom's supervisor keeps the password where only root reads it
 * and gives it to this process alone, at each start; it never reaches Android's storage. The host's lock screen stays the only lock: SystemUI reports the
 * keyguard gone once {@link #STATE} is "verified", and showing while {@link #HOST_LOCKED} says
 * the host session is locked. Each host unlock verifies the credential again, which unlocks the
 * keys that need an unlocked device. A credential Droidloom did not set is left alone and the
 * cell stays locked, as before.
 */
final class DroidloomCredential {
    static final String STATE = "sys.droidloom.credential";
    static final String HOST_LOCKED = "sys.droidloom.host_locked";
    private static final String TAG = "DroidloomCredential";
    private static final int LENGTH = 32;
    private static final Object LOCK = new Object();
    private static String sPassword;
    private static boolean sBooted, sApplied;
    /** LockPatternUtils refuses to verify on the main thread; one worker keeps the order. */
    private static final ExecutorService WORKER = Executors.newSingleThreadExecutor(
            runnable -> { Thread thread = new Thread(runnable, TAG); thread.setDaemon(true); return thread; });

    /**
     * Applies once user 0 reached its locked boot and the supervisor gave the password; verifying
     * the credential is what unlocks the user.
     */
    static void register(Context context) {
        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override public void onReceive(Context ignored, Intent intent) {
                if (getSendingUserId() != 0) return;
                synchronized (LOCK) { sBooted = true; }
                applyWhenReady(context);
            }
        };
        context.registerReceiver(receiver, new IntentFilter(Intent.ACTION_LOCKED_BOOT_COMPLETED),
                Context.RECEIVER_NOT_EXPORTED);
    }
    /** The supervisor's password; the framework control listener takes requests from root only. */
    static String received(Context context, String password) {
        if (password.length() != LENGTH) throw new IllegalArgumentException("Invalid credential");
        synchronized (LOCK) { sPassword = password; }
        applyWhenReady(context);
        return "credential";
    }
    private static String password() {
        synchronized (LOCK) { return sPassword; }
    }
    private static void applyWhenReady(Context context) {
        synchronized (LOCK) {
            if (!sBooted || sPassword == null || sApplied) return;
            sApplied = true;
        }
        WORKER.execute(() -> apply(context));
    }
    private static void apply(Context context) {
        LockPatternUtils locks = new LockPatternUtils(context);
        String stored = password();
        boolean secure = locks.isSecure(0), verified = false;
        try {
            if (secure) {
                try (LockscreenCredential credential = LockscreenCredential.createPassword(stored)) {
                    verified = locks.verifyCredential(credential, 0, 0).isMatched();
                }
                if (!verified) Slog.w(TAG, "Android's credential no longer matches Droidloom's");
            } else {
                try (LockscreenCredential credential = LockscreenCredential.createPassword(stored);
                        LockscreenCredential none = LockscreenCredential.createNone()) {
                    verified = locks.setLockCredential(credential, none, 0);
                }
                if (verified) Slog.i(TAG, "Android credential set");
            }
        } catch (Exception error) {
            Slog.e(TAG, "Android credential unavailable", error);
        }
        SystemProperties.set(STATE, verified ? "verified" : "locked");
        announce();
    }
    /**
     * Every app process runs its property callbacks, so SystemUI's keyguard reads the state
     * again. SystemProperties.reportSyspropChanged() reaches only this process.
     */
    private static void announce() {
        Parcel data = Parcel.obtain();
        try {
            ActivityManager.getService().asBinder()
                    .transact(IBinder.SYSPROPS_TRANSACTION, data, null, 0);
        } catch (Exception error) {
            Slog.w(TAG, "Credential state not announced", error);
        } finally {
            data.recycle();
        }
    }
    /** The host session locked or unlocked; Droidloom's supervisor follows logind's hint. */
    static String hostLocked(Context context, boolean locked) {
        WORKER.execute(() -> {
            if (!locked) verify(context);
            SystemProperties.set(HOST_LOCKED, locked ? "1" : "0");
            announce();
        });
        return "locked=" + locked;
    }
    private static void verify(Context context) {
        String stored = password();
        LockPatternUtils locks = new LockPatternUtils(context);
        if (stored == null || !locks.isSecure(0)) return;
        try (LockscreenCredential credential = LockscreenCredential.createPassword(stored)) {
            if (!locks.verifyCredential(credential, 0, 0).isMatched()) {
                Slog.w(TAG, "Android's credential no longer matches Droidloom's");
            }
        } catch (Exception error) {
            Slog.e(TAG, "Android credential not verified at the host unlock", error);
        }
    }
    private DroidloomCredential() { }
}
