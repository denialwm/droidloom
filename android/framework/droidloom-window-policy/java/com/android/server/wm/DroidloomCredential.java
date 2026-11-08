/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.app.ActivityManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.FileUtils;
import android.os.IBinder;
import android.os.Parcel;
import android.os.SystemProperties;
import android.util.AtomicFile;
import android.util.Slog;
import com.android.internal.widget.LockPatternUtils;
import com.android.internal.widget.LockscreenCredential;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.security.SecureRandom;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Android's fingerprint sensor and its auth-bound keys need an Android credential. Droidloom
 * sets a random password nobody types and verifies it at each start, which unlocks user 0 and
 * keeps its secure user ID. The host's lock screen stays the only lock: SystemUI reports the
 * keyguard gone once {@link #STATE} is "verified", and showing while {@link #HOST_LOCKED} says
 * the host session is locked. Each host unlock verifies the credential again, which unlocks the
 * keys that need an unlocked device. A credential Droidloom did not set is left alone and the
 * cell stays locked, as before.
 */
final class DroidloomCredential {
    static final String STATE = "sys.droidloom.credential";
    static final String HOST_LOCKED = "sys.droidloom.host_locked";
    private static final String TAG = "DroidloomCredential";
    private static final File DIRECTORY = new File("/data/system/droidloom");
    private static final AtomicFile FILE = new AtomicFile(new File(DIRECTORY, "credential"));
    private static final String ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    private static final int LENGTH = 32;
    /** LockPatternUtils refuses to verify on the main thread; one worker keeps the order. */
    private static final ExecutorService WORKER = Executors.newSingleThreadExecutor(
            runnable -> { Thread thread = new Thread(runnable, TAG); thread.setDaemon(true); return thread; });

    /** Applies at user 0's locked boot; verifying the credential is what unlocks the user. */
    static void register(Context context) {
        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override public void onReceive(Context ignored, Intent intent) {
                if (getSendingUserId() != 0) return;
                WORKER.execute(() -> apply(context));
            }
        };
        context.registerReceiver(receiver, new IntentFilter(Intent.ACTION_LOCKED_BOOT_COMPLETED),
                Context.RECEIVER_NOT_EXPORTED);
    }
    private static void apply(Context context) {
        LockPatternUtils locks = new LockPatternUtils(context);
        String stored = read();
        boolean secure = locks.isSecure(0), verified = false;
        try {
            if (stored == null && secure) {
                Slog.w(TAG, "Android has a credential Droidloom did not set; the cell stays locked");
            } else if (secure) {
                try (LockscreenCredential credential = LockscreenCredential.createPassword(stored)) {
                    verified = locks.verifyCredential(credential, 0, 0).isMatched();
                }
                if (!verified) Slog.w(TAG, "Android's credential no longer matches Droidloom's");
            } else {
                if (stored == null) stored = write(generate());
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
        String stored = read();
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
    private static String read() {
        if (!FILE.exists()) return null;
        try {
            String text = new String(FILE.readFully(), StandardCharsets.US_ASCII);
            return text.length() == LENGTH ? text : null;
        } catch (Exception error) {
            Slog.e(TAG, "Unreadable credential file", error);
            return null;
        }
    }
    private static String generate() {
        SecureRandom random = new SecureRandom();
        StringBuilder text = new StringBuilder(LENGTH);
        for (int i = 0; i < LENGTH; i++) text.append(ALPHABET.charAt(random.nextInt(ALPHABET.length())));
        return text.toString();
    }
    /** Stored before Android sees it, so a crash between the two never loses the credential. */
    private static String write(String text) throws Exception {
        if (!DIRECTORY.isDirectory() && !DIRECTORY.mkdirs()) throw new IllegalStateException("No credential directory");
        FileUtils.setPermissions(DIRECTORY, 0700, -1, -1);
        FileOutputStream out = FILE.startWrite();
        try {
            out.write(text.getBytes(StandardCharsets.US_ASCII));
            FILE.finishWrite(out);
        } catch (Exception error) {
            FILE.failWrite(out);
            throw error;
        }
        FileUtils.setPermissions(FILE.getBaseFile(), 0600, -1, -1);
        return text;
    }
    private DroidloomCredential() { }
}
