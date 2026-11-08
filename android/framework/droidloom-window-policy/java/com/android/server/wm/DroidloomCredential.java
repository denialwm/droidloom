/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.FileUtils;
import android.os.SystemProperties;
import android.util.AtomicFile;
import android.util.Slog;
import com.android.internal.widget.LockPatternUtils;
import com.android.internal.widget.LockscreenCredential;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.security.SecureRandom;

/**
 * Android's fingerprint sensor and its auth-bound keys need an Android credential. Droidloom
 * sets a random password nobody types and verifies it at each start, which unlocks user 0 and
 * keeps its secure user ID. The host's lock screen stays the only lock: SystemUI reports the
 * keyguard gone once {@link #STATE} is "verified". A credential Droidloom did not set is left
 * alone and the cell stays locked, as before.
 */
final class DroidloomCredential {
    static final String STATE = "sys.droidloom.credential";
    private static final String TAG = "DroidloomCredential";
    private static final File DIRECTORY = new File("/data/system/droidloom");
    private static final AtomicFile FILE = new AtomicFile(new File(DIRECTORY, "credential"));
    private static final String ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    private static final int LENGTH = 32;

    /** Applies at user 0's locked boot; verifying the credential is what unlocks the user. */
    static void register(Context context) {
        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override public void onReceive(Context ignored, Intent intent) {
                if (getSendingUserId() != 0) return;
                // LockPatternUtils refuses to verify on the main thread.
                Thread worker = new Thread(() -> apply(context), TAG);
                worker.setDaemon(true); worker.start();
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
        // SystemUI's keyguard reads the state again.
        SystemProperties.reportSyspropChanged();
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
