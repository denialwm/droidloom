/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.biometrics;

import android.app.Activity;
import android.app.ActivityManager;
import android.app.ActivityOptions;
import android.content.Context;
import android.content.Intent;
import android.hardware.biometrics.BiometricConstants;
import android.hardware.biometrics.BiometricPrompt;
import android.hardware.biometrics.IBiometricSysuiReceiver;
import android.hardware.biometrics.PromptInfo;
import android.hardware.fingerprint.FingerprintManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.RemoteException;
import android.text.TextUtils;
import android.util.Log;
import android.view.Gravity;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;
import java.util.List;

/**
 * BiometricPrompt as a small dialog over the asking app, in its task, so it shares that app's
 * window. It offers the fingerprint and Cancel: the Android credential is Droidloom's own and
 * never typed. All state lives on the main thread.
 */
public final class FingerprintPrompt {
    private static final String TAG = "DroidloomSystemUI";
    private static final Handler MAIN = new Handler(Looper.getMainLooper());
    /** Long enough to read an error before the prompt closes, as stock SystemUI does. */
    private static final long ERROR_MS = 2000;
    private static Request current;

    private static final class Request {
        final long id;
        final IBiometricSysuiReceiver receiver;
        final CharSequence title, subtitle, description, negative;
        final boolean confirm;
        CharSequence status;
        Activity activity;
        boolean done;
        Request(long id, IBiometricSysuiReceiver receiver, PromptInfo info, boolean confirm) {
            this.id = id; this.receiver = receiver; this.confirm = confirm;
            title = info.getTitle(); subtitle = info.getSubtitle();
            description = info.getDescription(); negative = info.getNegativeButtonText();
        }
    }

    public static void show(Context context, PromptInfo info, IBiometricSysuiReceiver receiver,
            boolean requireConfirmation, String packageName, long requestId) {
        MAIN.post(() -> {
            close(current);
            Request request = new Request(requestId, receiver, info, requireConfirmation);
            request.status = context.getString(com.android.internal.R.string.fingerprint_dialog_default_subtitle);
            current = request;
            Intent intent = new Intent(context, PromptActivity.class)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_NO_ANIMATION);
            ActivityOptions options = ActivityOptions.makeBasic();
            int task = taskOf(context, packageName);
            if (task != -1) {
                options.setLaunchTaskId(task);
                options.setTaskOverlay(true, true);
            }
            try {
                context.startActivity(intent, options.toBundle());
                receiver.onDialogAnimatedIn(true);
            } catch (Exception error) {
                Log.w(TAG, "Fingerprint prompt not shown", error);
                dismiss(request, BiometricPrompt.DISMISSED_REASON_ERROR_NO_WM);
            }
        });
    }

    /** The asking app's most recent task. */
    private static int taskOf(Context context, String packageName) {
        List<ActivityManager.RunningTaskInfo> tasks =
                context.getSystemService(ActivityManager.class).getRunningTasks(16);
        for (ActivityManager.RunningTaskInfo task : tasks) {
            if ((task.topActivity != null && packageName.equals(task.topActivity.getPackageName()))
                    || (task.baseActivity != null
                            && packageName.equals(task.baseActivity.getPackageName()))) {
                return task.taskId;
            }
        }
        return -1;
    }

    public static void authenticated() {
        MAIN.post(() -> {
            if (current == null) return;
            dismiss(current, current.confirm ? BiometricPrompt.DISMISSED_REASON_BIOMETRIC_CONFIRMED
                    : BiometricPrompt.DISMISSED_REASON_BIOMETRIC_CONFIRM_NOT_REQUIRED);
        });
    }

    public static void help(String message) {
        MAIN.post(() -> status(message));
    }

    /** A rejected finger or a timeout keeps listening; any other error ends the prompt. */
    public static void error(Context context, int error, int vendorCode) {
        MAIN.post(() -> {
            Request request = current;
            if (request == null) return;
            if (error == BiometricConstants.BIOMETRIC_PAUSED_REJECTED) {
                status(context.getString(com.android.internal.R.string.fingerprint_error_not_match));
                return;
            }
            status(FingerprintManager.getErrorString(context, error, vendorCode));
            if (error == BiometricConstants.BIOMETRIC_ERROR_TIMEOUT) return;
            MAIN.postDelayed(() -> {
                if (current == request) dismiss(request, BiometricPrompt.DISMISSED_REASON_ERROR);
            }, ERROR_MS);
        });
    }

    /** BiometricService ended the request itself: close without answering. */
    public static void hide(long requestId) {
        MAIN.post(() -> {
            if (current != null && current.id == requestId) close(current);
        });
    }

    private static void status(CharSequence message) {
        if (current == null || TextUtils.isEmpty(message)) return;
        current.status = message;
        if (current.activity instanceof PromptActivity prompt) prompt.render(current);
    }

    private static void dismiss(Request request, int reason) {
        if (request.done) return;
        try {
            request.receiver.onDialogDismissed(reason, null);
        } catch (RemoteException error) {
            Log.w(TAG, "Biometric prompt result not delivered", error);
        }
        close(request);
    }

    private static void close(Request request) {
        if (request == null) return;
        request.done = true;
        if (request.activity != null) request.activity.finish();
        if (current == request) current = null;
    }

    public static final class PromptActivity extends Activity {
        private Request request;
        private TextView status;

        @Override protected void onCreate(Bundle saved) {
            super.onCreate(saved);
            request = current;
            if (request == null || request.done || request.activity != null) {
                finish();
                return;
            }
            request.activity = this;
            setFinishOnTouchOutside(false);
            int padding = Math.round(24 * getResources().getDisplayMetrics().density);
            LinearLayout content = new LinearLayout(this);
            content.setOrientation(LinearLayout.VERTICAL);
            content.setPadding(padding, padding, padding, padding / 2);
            CharSequence title = TextUtils.isEmpty(request.title)
                    ? getString(com.android.internal.R.string.biometric_dialog_default_title)
                    : request.title;
            content.addView(text(title, android.R.style.TextAppearance_DeviceDefault_DialogWindowTitle));
            if (!TextUtils.isEmpty(request.subtitle)) {
                content.addView(text(request.subtitle, android.R.style.TextAppearance_DeviceDefault_Medium));
            }
            if (!TextUtils.isEmpty(request.description)) {
                content.addView(text(request.description, android.R.style.TextAppearance_DeviceDefault_Small));
            }
            status = text("", android.R.style.TextAppearance_DeviceDefault_Small);
            status.setGravity(Gravity.CENTER_HORIZONTAL);
            status.setPadding(0, padding, 0, padding);
            content.addView(status);
            Button cancel = new Button(this, null, android.R.attr.borderlessButtonStyle);
            boolean negative = !TextUtils.isEmpty(request.negative);
            cancel.setText(negative ? request.negative : getString(android.R.string.cancel));
            cancel.setOnClickListener(view -> dismiss(request, negative
                    ? BiometricPrompt.DISMISSED_REASON_NEGATIVE
                    : BiometricPrompt.DISMISSED_REASON_USER_CANCEL));
            LinearLayout.LayoutParams end = new LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            end.gravity = Gravity.END;
            content.addView(cancel, end);
            setContentView(content);
            render(request);
        }

        private TextView text(CharSequence value, int appearance) {
            TextView view = new TextView(this);
            view.setTextAppearance(appearance);
            view.setText(value);
            return view;
        }

        void render(Request shown) {
            if (shown == request && status != null) status.setText(shown.status);
        }

        @Override public void onBackPressed() {
            dismiss(request, BiometricPrompt.DISMISSED_REASON_USER_CANCEL);
        }

        @Override protected void onDestroy() {
            super.onDestroy();
            if (request == null || request.activity != this) return;
            // A configuration change hands the request to the next instance.
            request.activity = null;
            // Closed some other way, such as the task going away: the app hears a cancel.
            if (!isChangingConfigurations()) dismiss(request, BiometricPrompt.DISMISSED_REASON_USER_CANCEL);
        }
    }

    private FingerprintPrompt() { }
}
