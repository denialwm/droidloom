/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.biometrics;

import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.hardware.biometrics.PromptInfo;
import android.os.IBinder;
import android.os.Parcel;
import android.os.ServiceManager;
import android.text.TextUtils;
import android.util.Log;

/**
 * The device credential, asked for by the host. system_server announces the prompt to Droidloom's
 * supervisor; the host shows its own screen lock, root checks what the user types, and
 * system_server answers BiometricService with the attestation. SystemUI opens no window and never
 * sees the credential.
 */
public final class HostCredential {
    private static final String TAG = "DroidloomSystemUI";
    // DroidloomCredential's prompt service in system_server.
    private static final String SERVICE = "droidloom.credential";
    private static final String DESCRIPTOR = "droidloom.credential.prompts";
    private static final int ASK = IBinder.FIRST_CALL_TRANSACTION, END = ASK + 1;

    /**
     * False when the host cannot ask: the prompt then has no way through the credential. With
     * fingerprint, the host's sensor is asked for the same prompt.
     */
    public static boolean show(PromptInfo info, IBiometricSysuiReceiver receiver, int userId,
            long operationId, String packageName, long requestId, boolean fingerprint) {
        IBinder service = ServiceManager.getService(SERVICE);
        if (service == null) return false;
        Parcel data = Parcel.obtain(), reply = Parcel.obtain();
        try {
            data.writeInterfaceToken(DESCRIPTOR);
            data.writeStrongBinder(receiver.asBinder());
            data.writeLong(requestId);
            data.writeInt(userId);
            data.writeLong(operationId);
            data.writeString(packageName);
            data.writeString(text(info == null ? null : info.getDeviceCredentialTitle(),
                    info == null ? null : info.getTitle()));
            data.writeString(text(info == null ? null : info.getDeviceCredentialDescription(),
                    info == null ? null : info.getDescription(),
                    info == null ? null : info.getSubtitle()));
            data.writeBoolean(fingerprint);
            service.transact(ASK, data, reply, 0);
            reply.readException();
            return reply.readBoolean();
        } catch (Exception error) {
            Log.w(TAG, "Credential prompt not handed to the host", error);
            return false;
        } finally {
            data.recycle();
            reply.recycle();
        }
    }

    /** BiometricService ended the request; the host closes its screen if it still shows. */
    public static void hide(long requestId) {
        IBinder service = ServiceManager.getService(SERVICE);
        if (service == null) return;
        Parcel data = Parcel.obtain();
        try {
            data.writeInterfaceToken(DESCRIPTOR);
            data.writeLong(requestId);
            service.transact(END, data, null, IBinder.FLAG_ONEWAY);
        } catch (Exception error) {
            Log.w(TAG, "Credential prompt end not handed to the host", error);
        } finally {
            data.recycle();
        }
    }

    private static String text(CharSequence... candidates) {
        for (CharSequence candidate : candidates) {
            if (!TextUtils.isEmpty(candidate)) return candidate.toString();
        }
        return null;
    }

    private HostCredential() { }
}
