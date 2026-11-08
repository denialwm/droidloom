/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui;

import android.app.ITransientNotificationCallback;
import android.app.StatusBarManager;
import android.hardware.biometrics.BiometricPrompt;
import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.os.*;
import com.android.internal.statusbar.IAddTileResultCallback;
import com.android.systemui.compat.EmptyKeyguard;

/** Semantic callback tests; no windows, notifications, prompts, or input injection. */
public final class CallbacksProbe {
    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
    public static void main(String[] args) throws Exception {
        check(android.os.Process.myUid() == 2000, "Run as Android shell UID 2000");
        StatusBarCallbacks callbacks = new StatusBarCallbacks();
        int[] result = {-1};
        callbacks.showAuthenticationDialog(null, new IBiometricSysuiReceiver.Default() {
            @Override public void onDialogDismissed(int reason, byte[] credential) {
                check(credential == null, "Never fabricate a credential attestation");
                result[0] = reason;
            }
        }, new int[0], true, false, 0, 0, "probe", 1);
        check(result[0] == BiometricPrompt.DISMISSED_REASON_USER_CANCEL,
                "A prompt with no sensor must complete as cancelled");
        result[0] = -1;
        callbacks.requestAddTile(2000, null, "probe", "probe", null,
                new IAddTileResultCallback.Default() {
                    @Override public void onTileRequest(int response) { result[0] = response; }
                });
        check(result[0] == StatusBarManager.TILE_ADD_REQUEST_RESULT_TILE_NOT_ADDED,
                "Tile caller must receive a negative result");
        result[0] = 0;
        callbacks.showToast(2000, "com.android.droidloom.callbackprobe", new Binder(), "",
                new Binder(), 0, new ITransientNotificationCallback.Default() {
                    @Override public void onToastHidden() { result[0]++; }
                    @Override public void onToastShown() { throw new AssertionError("No toast shown"); }
                }, 0);
        check(result[0] == 1, "Suppressed toast must complete its hidden callback exactly once");
        for (boolean proto : new boolean[] {false, true}) {
            ParcelFileDescriptor[] pipe = ParcelFileDescriptor.createPipe();
            if (proto) callbacks.dumpProto(new String[0], pipe[1]);
            else callbacks.passThroughShellCommand(new String[0], pipe[1]);
            check(!pipe[1].getFileDescriptor().valid(), "Output FD must be closed");
            pipe[0].close();
        }
        Parcel data = Parcel.obtain();
        try {
            try {
                callbacks.onTransact(IBinder.FIRST_CALL_TRANSACTION, data, null, IBinder.FLAG_ONEWAY);
                throw new AssertionError("Unprivileged status-bar callback accepted");
            } catch (SecurityException expected) {}
            try {
                new EmptyKeyguard() {}.onTransact(IBinder.FIRST_CALL_TRANSACTION, data, null,
                        IBinder.FLAG_ONEWAY);
                throw new AssertionError("Unprivileged keyguard callback accepted");
            } catch (SecurityException expected) {}
        } finally { data.recycle(); }
        System.out.println("PASS: authentication cancellation, tile rejection, toast completion, "
                + "shell/proto FD closure, status-bar/keyguard caller guards");
    }
}
