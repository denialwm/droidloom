/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui;

import android.app.INotificationManager;
import android.app.ITransientNotificationCallback;
import android.app.StatusBarManager;
import android.content.ComponentName;
import android.content.Context;
import android.graphics.drawable.Icon;
import android.hardware.biometrics.BiometricPrompt;
import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.hardware.biometrics.PromptInfo;
import android.os.IBinder;
import android.os.ParcelFileDescriptor;
import android.os.RemoteException;
import android.os.ServiceManager;
import com.android.internal.statusbar.IAddTileResultCallback;
import com.android.systemui.biometrics.FingerprintPrompt;
import com.android.systemui.compat.EmptyStatusBar;
import java.io.IOException;

final class StatusBarCallbacks extends EmptyStatusBar {
    /** Shows fingerprint prompts; without it, as in the probe, every prompt cancels. */
    private final Context context;
    StatusBarCallbacks() { this(null); }
    StatusBarCallbacks(Context context) { this.context = context; }
    /**
     * The fingerprint is the only way through: the Android credential is Droidloom's own, so a
     * prompt with no sensor to use cancels without credential attestation.
     */
    @Override public void showAuthenticationDialog(PromptInfo info,
            IBiometricSysuiReceiver receiver, int[] sensors, boolean credentialAllowed,
            boolean requireConfirmation, int userId, long operationId, String packageName,
            long requestId) throws RemoteException {
        if (receiver == null) return;
        if (context == null || info == null || sensors == null || sensors.length == 0) {
            Unsupported.report("credential prompt");
            receiver.onDialogDismissed(BiometricPrompt.DISMISSED_REASON_USER_CANCEL, null);
            return;
        }
        FingerprintPrompt.show(context, info, receiver, requireConfirmation, packageName, requestId);
    }
    @Override public void onBiometricAuthenticated(int modality) { FingerprintPrompt.authenticated(); }
    @Override public void onBiometricHelp(int modality, String message) { FingerprintPrompt.help(message); }
    @Override public void onBiometricError(int modality, int error, int vendorCode) {
        if (context != null) FingerprintPrompt.error(context, error, vendorCode);
    }
    @Override public void hideAuthenticationDialog(long requestId) { FingerprintPrompt.hide(requestId); }
    @Override public void requestAddTile(int uid, ComponentName component,
            CharSequence appName, CharSequence label, Icon icon,
            IAddTileResultCallback callback) throws RemoteException {
        Unsupported.report("quick settings tile");
        if (callback != null) callback.onTileRequest(StatusBarManager.TILE_ADD_REQUEST_RESULT_TILE_NOT_ADDED);
    }
    @Override public void showToast(int uid, String packageName, IBinder token, CharSequence text,
            IBinder windowToken, int duration, ITransientNotificationCallback callback,
            int displayId) throws RemoteException {
        Unsupported.report("text toast forwarding");
        // Nothing was shown. Complete the callback and release the framework window token.
        try {
            if (callback != null) callback.onToastHidden();
        } finally {
            INotificationManager.Stub.asInterface(ServiceManager.getService("notification"))
                    .finishToken(packageName, token);
        }
    }
    @Override public void passThroughShellCommand(String[] args, ParcelFileDescriptor fd) {
        close(fd);
    }
    @Override public void dumpProto(String[] args, ParcelFileDescriptor fd) { close(fd); }
    private static void close(ParcelFileDescriptor fd) {
        if (fd != null) try { fd.close(); } catch (IOException ignored) {}
    }
}
