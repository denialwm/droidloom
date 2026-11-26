/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.levels;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.hardware.display.BrightnessInfo;
import android.hardware.display.DisplayManager;
import android.os.Handler;
import android.os.PowerManager;
import android.util.Log;
import android.view.Display;
import com.android.droidloom.runtime.HostChannel;
import java.io.PrintWriter;

/**
 * Android's screen brightness is the host's backlight. An app that changes
 * Android's brightness setting changes the host's; an app whose window asks
 * for a brightness while it shows, as a video player does, gets it on the host
 * until no window asks, when the host goes back to its own level. The host's
 * changes become Android's setting, so apps read the level the host shows.
 * Nothing goes to the host while Android's display is off: it is off because
 * the host's is.
 */
public final class HostBrightness implements DisplayManager.DisplayListener, AutoCloseable {
    private static final String TAG = "DroidloomBrightness";
    /** Levels closer than this are the same level. */
    private static final float SAME = 0.002f;
    private final Context context;
    private final DisplayManager displays;
    private final PowerManager power;
    private final Handler main;
    private volatile boolean stopped;
    private volatile HostChannel channel;
    // Owned by the main thread: the host's level as Android's setting has it,
    // NaN until the host told it, and the level a window holds on the host.
    private float agreed = Float.NaN;
    private boolean window;
    private float windowLevel = Float.NaN;
    private final BroadcastReceiver screen = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) { evaluate(); }
    };

    public HostBrightness(Context context) {
        this.context = context;
        displays = context.getSystemService(DisplayManager.class);
        power = context.getSystemService(PowerManager.class);
        main = new Handler(context.getMainLooper());
    }

    public void start() {
        displays.registerDisplayListener(this, main, DisplayManager.EVENT_TYPE_DISPLAY_BRIGHTNESS);
        IntentFilter filter = new IntentFilter(Intent.ACTION_SCREEN_ON);
        filter.addAction(Intent.ACTION_SCREEN_OFF);
        context.registerReceiver(screen, filter, null, main, Context.RECEIVER_NOT_EXPORTED);
        Thread worker = new Thread(this::connect, "droidloom-brightness");
        worker.setDaemon(true);
        worker.start();
    }

    private void connect() {
        com.android.droidloom.runtime.CpuPlacement.background();
        while (!stopped) {
            try (HostChannel current = HostChannel.open(HostChannel.BRIGHTNESS)) {
                channel = current;
                Records.Record record;
                while ((record = Records.read(current.getInputStream())) != null) {
                    if (record.kind == Records.BRIGHTNESS_STATE) {
                        float level = record.level;
                        main.post(() -> fromHost(level));
                    } else if (record.kind == Records.BRIGHTNESS_UNAVAILABLE) {
                        main.post(this::forget);
                    }
                }
            } catch (Exception error) {
                if (!stopped) Log.w(TAG, "Host brightness channel: " + error.getClass().getSimpleName());
            }
            channel = null;
            main.post(this::forget);
            if (!stopped) try { Thread.sleep(2000); } catch (InterruptedException ignored) { }
        }
    }

    private void fromHost(float level) {
        if (Math.abs(displays.getBrightness(Display.DEFAULT_DISPLAY) - level) > SAME)
            displays.setBrightness(Display.DEFAULT_DISPLAY, level);
        // Android keeps its setting within its own range: what it holds now is
        // the host's level, so a dim host is not raised to Android's minimum.
        agreed = displays.getBrightness(Display.DEFAULT_DISPLAY);
        evaluateWindow();
    }

    /** Disconnected, or the host has no backlight: nothing to follow. */
    private void forget() {
        agreed = Float.NaN;
        window = false;
        windowLevel = Float.NaN;
    }

    @Override public void onDisplayChanged(int displayId) {
        if (displayId == Display.DEFAULT_DISPLAY) evaluate();
    }
    @Override public void onDisplayAdded(int displayId) { }
    @Override public void onDisplayRemoved(int displayId) { }

    private void evaluate() {
        HostChannel current = channel;
        if (!evaluateWindow() || current == null) return;
        float setting = displays.getBrightness(Display.DEFAULT_DISPLAY);
        if (Math.abs(setting - agreed) > SAME) {
            agreed = setting;
            Records.send(current, Records.level(Records.BRIGHTNESS_SET, setting, false));
        }
    }

    /**
     * Sends a window's level, or its end. True when no window asks and
     * Android's setting may go to the host.
     */
    private boolean evaluateWindow() {
        HostChannel current = channel;
        if (current == null || Float.isNaN(agreed) || !power.isInteractive()) return false;
        Display display = displays.getDisplay(Display.DEFAULT_DISPLAY);
        BrightnessInfo info = display == null ? null : display.getBrightnessInfo();
        if (info == null) return false;
        if (info.isBrightnessOverrideByWindow) {
            // `brightness` stays the user's setting; the window's level is what
            // the display controller applies.
            float level = info.adjustedBrightness;
            if (!window || Math.abs(level - windowLevel) > SAME) {
                window = true;
                windowLevel = level;
                Records.send(current, Records.level(Records.BRIGHTNESS_SET, level, true));
            }
            return false;
        }
        if (window) {
            window = false;
            windowLevel = Float.NaN;
            Records.send(current, Records.plain(Records.BRIGHTNESS_RELEASE));
        }
        return true;
    }

    public void dump(PrintWriter out) {
        out.println("hostBrightness connected=" + (channel != null) + " level=" + agreed
                + " window=" + (window ? windowLevel : "none"));
    }

    @Override public void close() {
        stopped = true;
        displays.unregisterDisplayListener(this);
        try { context.unregisterReceiver(screen); } catch (IllegalArgumentException ignored) { }
        HostChannel current = channel;
        if (current != null) current.shutdownInput();
    }
}
