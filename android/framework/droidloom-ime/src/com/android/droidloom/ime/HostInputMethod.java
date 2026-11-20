package com.android.droidloom.ime;

import android.icu.util.ULocale;
import android.inputmethodservice.InputMethodService;
import com.android.droidloom.runtime.HostChannel;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.view.inputmethod.InputMethodSubtype;
import org.json.JSONObject;
import java.io.InputStream;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/** No Android keyboard window: the host owns the input panel. */
public final class HostInputMethod extends InputMethodService {
    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService writes = Executors.newSingleThreadExecutor();
    private volatile HostChannel socket;
    private volatile boolean stopped;
    private Thread reader;
    private long session;
    private long showRequest;
    private boolean composing;
    private JSONObject state;
    private final EditorVisibility visibility = new EditorVisibility();
    // The host's layout, kept until Android has attached this method: a
    // subtype switch before that is ignored.
    private String layoutLanguage, layoutType;
    // The layout this process last switched Android to, as "language/type".
    private String followedLayout;
    private boolean attached;

    @Override public void onCreate() {
        super.onCreate();
        reader = new Thread(this::connect, "droidloom-ime");
        reader.start();
    }

    @Override public boolean onEvaluateFullscreenMode() { return false; }
    @Override public boolean onEvaluateInputViewShown() { return false; }
    @Override public boolean onShowInputRequested(int flags, boolean configChange) {
        android.util.Log.i("DroidloomIME", "show request configChange=" + configChange);
        showRequest++;
        visibility.show(getCurrentInputEditorInfo() != null
                && getCurrentInputEditorInfo().inputType != InputType.TYPE_NULL);
        publish(visibility.isVisible());
        return false;
    }

    @Override public void hideWindow() {
        // onWindowHidden is conditional on an Android window having been
        // visible. Our headless method has none, but receives hideWindow for
        // the framework's hideSoftInput request just like a graphical IME.
        visibility.hide();
        publish(false);
        android.util.Log.i("DroidloomIME", "hide input panel");
        super.hideWindow();
    }

    @Override public void onStartInput(EditorInfo editor, boolean restarting) {
        android.util.Log.i("DroidloomIME", "start input restarting=" + restarting
                + " editable=" + (editor.inputType != InputType.TYPE_NULL));
        super.onStartInput(editor, restarting);
        attached = true;
        followHostLayout();
        session++;
        composing = false;
        visibility.start(editor.inputType != InputType.TYPE_NULL, restarting);
        publish(visibility.isVisible());
    }

    @Override public void onFinishInput() {
        android.util.Log.i("DroidloomIME", "finish input");
        visibility.finish();
        publish(false);
        composing = false;
        super.onFinishInput();
    }

    private void publish(boolean active) {
        try {
            EditorInfo editor = getCurrentInputEditorInfo();
            int task = editor == null || editor.extras == null ? 0
                    : editor.extras.getInt("com.android.droidloom.ime.task", 0);
            state = new JSONObject().put("session", session).put("active", active && task > 0)
                    .put("show_request", showRequest)
                    .put("task", active ? task : 0)
                    .put("input_type", editor == null ? 0 : editor.inputType);
            sendState();
        } catch (Exception error) {
            android.util.Log.w("DroidloomIME", "Unable to publish editor lifecycle", error);
        }
    }

    private void sendState() {
        if (state == null || stopped) return;
        final byte[] bytes = (state.toString() + "\n").getBytes(StandardCharsets.UTF_8);
        final HostChannel target = socket;
        if (target == null) return;
        writes.execute(() -> {
            try { target.getOutputStream().write(bytes); }
            catch (Exception error) { close(target); }
        });
    }

    private void connect() {
        while (!stopped) {
            try (HostChannel client = HostChannel.open(HostChannel.TEXT_INPUT)) {
                socket = client;
                main.post(this::sendState);
                InputStream input = client.getInputStream();
                ByteArrayOutputStream line = new ByteArrayOutputStream();
                int value;
                while (!stopped && (value = input.read()) >= 0) {
                    if (value == '\n') {
                        JSONObject edit = new JSONObject(line.toString("UTF-8"));
                        line.reset();
                        main.post(() -> apply(edit));
                    } else {
                        if (line.size() >= 16384) throw new java.io.IOException("oversized edit");
                        line.write(value);
                    }
                }
            } catch (Exception error) {
                // Reconnect only after transport loss, never poll editor state.
            } finally { socket = null; }
            if (!stopped) {
                try { Thread.sleep(1000); }
                catch (InterruptedException error) { return; }
            }
        }
    }

    private void apply(JSONObject edit) {
        JSONObject layout = edit.optJSONObject("keyboard_layout");
        if (layout != null) {
            String language = layout.optString("language"), type = layout.optString("type");
            if (language.isEmpty() || language.length() > 35 || type.length() > 16) return;
            layoutLanguage = language;
            layoutType = type;
            followHostLayout();
            return;
        }
        if (state == null || !state.optBoolean("active")
                || edit.optLong("session", -1) != session) return;
        if (edit.optBoolean("dismiss")) {
            // Host dismissal is not Back, Done, or forced focus loss. Tell
            // Android's IMMS so the app's requested IME visibility is reset;
            // tapping the same focused editor can then request it again.
            if (!visibility.dismissFromHost(edit.optLong("show_request", -1), showRequest)) return;
            composing = false;
            InputConnection current = getCurrentInputConnection();
            if (current != null) current.finishComposingText();
            publish(false);
            requestHideSelf(0);
            android.util.Log.i("DroidloomIME", "host dismissed input panel session=" + session);
            return;
        }
        InputConnection connection = getCurrentInputConnection();
        if (connection == null) return;
        connection.beginBatchEdit();
        try {
            int beforeBytes = edit.optInt("before");
            int afterBytes = edit.optInt("after");
            if (beforeBytes != 0 || afterBytes != 0) {
                if (composing) connection.setComposingText("", 1);
                connection.finishComposingText();
                composing = false;
            }
            int before = beforeBytes == 0 ? 0 : ByteOffsets.utf16Length(
                    connection.getTextBeforeCursor(4096, 0), beforeBytes, true);
            int after = afterBytes == 0 ? 0 : ByteOffsets.utf16Length(
                    connection.getTextAfterCursor(4096, 0), afterBytes, false);
            if (before < 0 || after < 0) return;
            if (before != 0 || after != 0) connection.deleteSurroundingText(before, after);
            if (edit.has("commit")) {
                connection.commitText(edit.optString("commit"), 1);
                composing = false;
            }
            if (edit.has("preedit")) {
                String preedit = edit.optString("preedit");
                if (!preedit.isEmpty() || composing) connection.setComposingText(preedit, 1);
                composing = !preedit.isEmpty();
                if (!composing) connection.finishComposingText();
            }
        } finally { connection.endBatchEdit(); }
    }

    /**
     * The host's keyboard layout, as a BCP 47 language tag and Android layout
     * type. Android picks the physical keyboard layout of the current input
     * method subtype's hint, so the host keyboard types as configured on the
     * host and Android never asks. The subtype replaces the last one. A
     * switch before Android has attached this method is ignored, so the
     * first editor start makes it.
     *
     * Android's keyboard layout manager learns a subtype only when the
     * selection changes, while the input method service restores the saved
     * subtype, or picks the only one left, without telling it. Each switch
     * therefore names a subtype other than the current one.
     */
    private void followHostLayout() {
        final String language = layoutLanguage, type = layoutType;
        if (language == null || !attached) return;
        final String layout = language + "/" + type;
        if (layout.equals(followedLayout)) return;
        InputMethodSubtype current = getSystemService(InputMethodManager.class)
                .getCurrentInputMethodSubtype();
        int id = layout.hashCode();
        if (current != null && current.hashCode() == id) id++;
        ULocale locale = ULocale.forLanguageTag(language);
        InputMethodSubtype subtype = new InputMethodSubtype.InputMethodSubtypeBuilder()
                .setSubtypeId(id)
                .setLanguageTag(language)
                .setSubtypeLocale(locale.toString())
                .setSubtypeMode("keyboard")
                .setIsAsciiCapable(true)
                .setPhysicalKeyboardHint(locale, type)
                .build();
        String component = new android.content.ComponentName(this, HostInputMethod.class)
                .flattenToShortString();
        try {
            getSystemService(InputMethodManager.class)
                    .setAdditionalInputMethodSubtypes(component, new InputMethodSubtype[] {subtype});
            switchInputMethod(component, subtype);
            followedLayout = layout;
            android.util.Log.i("DroidloomIME", "host keyboard layout " + language + " " + type);
        } catch (RuntimeException error) {
            android.util.Log.w("DroidloomIME", "Unable to follow the host keyboard layout", error);
        }
    }

    private static void close(HostChannel client) {
        if (client != null) try { client.close(); } catch (Exception ignored) { }
    }

    @Override public void onDestroy() {
        stopped = true;
        close(socket);
        reader.interrupt();
        writes.shutdownNow();
        super.onDestroy();
    }
}
