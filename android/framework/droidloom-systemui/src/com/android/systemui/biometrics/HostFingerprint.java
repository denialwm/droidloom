/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.biometrics;

import android.hardware.biometrics.BiometricConstants;
import android.hardware.biometrics.BiometricPrompt;
import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.os.RemoteException;
import android.util.Log;

/**
 * BiometricPrompt without a window. The host marks the sensor and runs the reader for Droidloom's
 * fingerprint HAL, as a phone's own SystemUI would, so the asking app stays in front and keeps
 * its own UI. SystemUI only answers BiometricService for the dialog it never shows.
 */
public final class HostFingerprint {
    private static final String TAG = "DroidloomSystemUI";
    private static final Object LOCK = new Object();
    private static Request current;

    private static final class Request {
        final long id;
        final IBiometricSysuiReceiver receiver;
        final boolean confirm;
        /** The host also asks for the screen lock, which stays a way through. */
        final boolean credential;
        Request(long id, IBiometricSysuiReceiver receiver, boolean confirm, boolean credential) {
            this.id = id; this.receiver = receiver; this.confirm = confirm;
            this.credential = credential;
        }
    }

    /** The dialog is "in" at once: the sensor starts listening. */
    public static void show(IBiometricSysuiReceiver receiver, boolean requireConfirmation,
            long requestId, boolean credential) {
        Request request = new Request(requestId, receiver, requireConfirmation, credential);
        synchronized (LOCK) { current = request; }
        try {
            receiver.onDialogAnimatedIn(true);
        } catch (RemoteException error) {
            Log.w(TAG, "Fingerprint request not started", error);
            synchronized (LOCK) { if (current == request) current = null; }
        }
    }

    /** Nobody can press a confirm button, so a required confirmation comes with the match. */
    public static void authenticated() {
        Request request = take();
        if (request != null) {
            dismiss(request, request.confirm ? BiometricPrompt.DISMISSED_REASON_BIOMETRIC_CONFIRMED
                    : BiometricPrompt.DISMISSED_REASON_BIOMETRIC_CONFIRM_NOT_REQUIRED);
        }
    }

    /**
     * A rejected finger or a timeout keeps listening. Any other error ends the fingerprint; the
     * request ends with it, and BiometricService hands the error to the app once the dialog is
     * dismissed, unless the host's screen lock is still asked for.
     */
    public static void error(int error) {
        if (error == BiometricConstants.BIOMETRIC_PAUSED_REJECTED
                || error == BiometricConstants.BIOMETRIC_ERROR_TIMEOUT) {
            return;
        }
        Request request = take();
        if (request != null && !request.credential) {
            dismiss(request, BiometricPrompt.DISMISSED_REASON_ERROR);
        }
    }

    /** BiometricService ended the request itself. */
    public static void hide(long requestId) {
        synchronized (LOCK) {
            if (current != null && current.id == requestId) current = null;
        }
    }

    private static Request take() {
        synchronized (LOCK) {
            Request request = current;
            current = null;
            return request;
        }
    }

    private static void dismiss(Request request, int reason) {
        try {
            request.receiver.onDialogDismissed(reason, null);
        } catch (RemoteException error) {
            Log.w(TAG, "Biometric result not delivered", error);
        }
    }

    private HostFingerprint() { }
}
