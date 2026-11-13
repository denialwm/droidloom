/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.app.ActivityManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.hardware.biometrics.BiometricPrompt;
import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;
import android.os.ServiceManager;
import android.os.SystemProperties;
import android.os.UserHandle;
import android.util.Slog;
import com.android.internal.widget.LockPatternUtils;
import com.android.internal.widget.LockscreenCredential;
import com.android.internal.widget.VerifyCredentialResponse;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import org.json.JSONException;
import org.json.JSONObject;

/**
 * Android's fingerprint sensor and its auth-bound keys need an Android credential. Droidloom
 * sets a random password nobody types and verifies it at each start, which unlocks user 0 and
 * keeps its secure user ID. Droidloom's supervisor keeps the password where only root reads it
 * and gives it to this process alone, at each start; it never reaches Android's storage. The host's lock screen stays the only lock: SystemUI reports the
 * keyguard gone once {@link #STATE} is "verified", and showing while {@link #HOST_LOCKED} says
 * the host session is locked. Each host unlock verifies the credential again, which unlocks the
 * keys that need an unlocked device. A credential Droidloom did not set is left alone and the
 * cell stays locked, as before.
 *
 * <p>An app that asks to confirm the device credential gets the host's own: SystemUI hands the
 * prompt to {@link #PROMPT_SERVICE}, which announces it to the supervisor on the framework's
 * credential-events socket. The host asks the user for their screen lock and root checks it;
 * the supervisor's answer comes back as a framework operation, and a confirmed prompt is
 * attested by Gatekeeper with this password, as Android's own credential screen would.
 */
final class DroidloomCredential {
    static final String STATE = "sys.droidloom.credential";
    static final String HOST_LOCKED = "sys.droidloom.host_locked";
    private static final String TAG = "DroidloomCredential";
    private static final int LENGTH = 32;
    private static final Object LOCK = new Object();
    private static String sPassword;
    private static boolean sBooted, sApplied;
    static final String PROMPT_SERVICE = "droidloom.credential";
    static final String PROMPT_DESCRIPTOR = "droidloom.credential.prompts";
    static final int PROMPT_ASK = IBinder.FIRST_CALL_TRANSACTION, PROMPT_END = PROMPT_ASK + 1;
    private static final int MAX_TEXT = 512;
    /** Prompts the host shows, by BiometricService's request ID; guarded by {@link #LOCK}. */
    private static final Map<Long, Prompt> PROMPTS = new HashMap<>();

    private static final class Prompt {
        final IBiometricSysuiReceiver receiver;
        final long operation;
        Prompt(IBiometricSysuiReceiver receiver, long operation) {
            this.receiver = receiver; this.operation = operation;
        }
    }
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
        ServiceManager.addService(PROMPT_SERVICE, new PromptService(context));
    }

    /** SystemUI's prompts, from SystemUI alone. */
    private static final class PromptService extends Binder {
        private final Context context;
        private int systemUi = -1;
        PromptService(Context context) { this.context = context; }

        private int systemUi() {
            if (systemUi < 0) {
                long identity = Binder.clearCallingIdentity();
                try {
                    systemUi = UserHandle.getAppId(context.getPackageManager()
                            .getPackageUid("com.android.systemui", 0));
                } catch (PackageManager.NameNotFoundException error) {
                    return -1;
                } finally {
                    Binder.restoreCallingIdentity(identity);
                }
            }
            return systemUi;
        }

        @Override protected boolean onTransact(int code, Parcel data, Parcel reply, int flags)
                throws RemoteException {
            if (code != PROMPT_ASK && code != PROMPT_END) {
                return super.onTransact(code, data, reply, flags);
            }
            if (UserHandle.getAppId(Binder.getCallingUid()) != systemUi()) {
                throw new SecurityException("Only SystemUI shows credential prompts");
            }
            data.enforceInterface(PROMPT_DESCRIPTOR);
            if (code == PROMPT_END) {
                end(data.readLong());
                return true;
            }
            IBiometricSysuiReceiver receiver =
                    IBiometricSysuiReceiver.Stub.asInterface(data.readStrongBinder());
            long id = data.readLong();
            int user = data.readInt();
            long operation = data.readLong();
            String owner = data.readString(), title = data.readString(),
                    description = data.readString();
            boolean fingerprint = data.readBoolean();
            boolean shown = receiver != null && user == 0 && ask(context, id,
                    new Prompt(receiver, operation), owner, title, description, fingerprint);
            if (reply != null) {
                reply.writeNoException();
                reply.writeBoolean(shown);
            }
            return true;
        }
    }

    private static String bounded(String text) {
        if (text == null) return "";
        return text.length() > MAX_TEXT ? text.substring(0, MAX_TEXT) : text;
    }

    /** The app's name as the user knows it; the package name when it has none. */
    private static String label(Context context, String owner) {
        if (owner == null) return "";
        long identity = Binder.clearCallingIdentity();
        try {
            PackageManager packages = context.getPackageManager();
            return packages.getApplicationLabel(packages.getApplicationInfo(owner, 0)).toString();
        } catch (PackageManager.NameNotFoundException error) {
            return owner;
        } finally {
            Binder.restoreCallingIdentity(identity);
        }
    }

    /** Announces a prompt to the supervisor; false when the host cannot show it. */
    private static boolean ask(Context context, long id, Prompt prompt, String owner, String title,
            String description, boolean fingerprint) {
        if (password() == null) return false;
        byte[] record;
        try {
            record = new JSONObject()
                    .put("ask", id)
                    .put("app", bounded(label(context, owner)))
                    .put("title", bounded(title))
                    .put("description", bounded(description))
                    .put("fingerprint", fingerprint)
                    .toString().getBytes(StandardCharsets.UTF_8);
        } catch (JSONException error) {
            return false;
        }
        synchronized (LOCK) {
            if (PROMPTS.containsKey(id)) return false;
            PROMPTS.put(id, prompt);
        }
        if (DroidloomFrameworkNative.credentialAsk(id, record)) return true;
        synchronized (LOCK) { PROMPTS.remove(id); }
        return false;
    }

    /** BiometricService ended the prompt, by a fingerprint or an app that gave up. */
    private static void end(long id) {
        synchronized (LOCK) { PROMPTS.remove(id); }
        DroidloomFrameworkNative.credentialEnd(id);
    }

    /**
     * The supervisor's answer. A confirmed prompt is attested for its operation the way
     * Android's credential screen does it, with the password only this process holds.
     */
    static String answer(Context context, long id, boolean confirmed) {
        Prompt prompt;
        synchronized (LOCK) { prompt = PROMPTS.remove(id); }
        if (prompt == null) throw new IllegalStateException("No such credential prompt");
        DroidloomFrameworkNative.credentialEnd(id);
        WORKER.execute(() -> {
            byte[] attestation = confirmed ? attest(context, prompt.operation) : null;
            try {
                if (attestation == null) {
                    prompt.receiver.onDialogDismissed(
                            BiometricPrompt.DISMISSED_REASON_USER_CANCEL, null);
                    return;
                }
                // As Android's own dialog does when it turns to the credential: the sensors stop.
                prompt.receiver.onDeviceCredentialPressed();
                prompt.receiver.onDialogDismissed(
                        BiometricPrompt.DISMISSED_REASON_CREDENTIAL_CONFIRMED, attestation);
            } catch (RemoteException error) {
                Slog.w(TAG, "Credential prompt result not delivered", error);
            }
        });
        return "answered";
    }

    /**
     * The user turned from the fingerprint to the host's screen lock, as with Android's own
     * button: BiometricService stops the sensors and waits for the credential.
     */
    static String chosen(long id) {
        Prompt prompt;
        synchronized (LOCK) { prompt = PROMPTS.get(id); }
        if (prompt == null) throw new IllegalStateException("No such credential prompt");
        WORKER.execute(() -> {
            try {
                prompt.receiver.onDeviceCredentialPressed();
            } catch (RemoteException error) {
                Slog.w(TAG, "Credential choice not delivered", error);
            }
        });
        return "chosen";
    }

    private static byte[] attest(Context context, long operation) {
        String stored = password();
        if (stored == null) return null;
        LockPatternUtils locks = new LockPatternUtils(context);
        try (LockscreenCredential credential = LockscreenCredential.createPassword(stored)) {
            VerifyCredentialResponse verified = locks.verifyCredential(
                    credential, 0, LockPatternUtils.VERIFY_FLAG_REQUEST_GK_PW_HANDLE);
            if (!verified.isMatched()) {
                Slog.w(TAG, "Android's credential no longer matches Droidloom's");
                return null;
            }
            long handle = verified.getGatekeeperPasswordHandle();
            try {
                VerifyCredentialResponse attested =
                        locks.verifyGatekeeperPasswordHandle(handle, operation, 0);
                return attested.isMatched() ? attested.getGatekeeperHAT() : null;
            } finally {
                locks.removeGatekeeperPasswordHandle(handle);
            }
        } catch (Exception error) {
            Slog.e(TAG, "Credential prompt not attested", error);
            return null;
        }
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
