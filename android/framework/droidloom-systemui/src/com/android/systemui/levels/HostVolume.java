/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.levels;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.media.AudioManager;
import android.os.Handler;
import android.util.Log;
import com.android.droidloom.runtime.HostChannel;
import java.io.PrintWriter;

/**
 * Android's media volume is the host's default output volume. Android's
 * volume curve plays music on the host output at full scale at every step
 * (`audio_policy_volumes.xml`), so the music volume only moves the host's:
 * each change goes to the host, and the host's changes become Android's music
 * volume, so apps see the level the host shows. Index 0 is silence in Android
 * whatever the curve says, so a quiet host level stays on step 1 at least.
 */
public final class HostVolume implements AutoCloseable {
    private static final String TAG = "DroidloomVolume";
    private final Context context;
    private final AudioManager audio;
    private final Handler main;
    private final int min, max;
    private volatile boolean stopped;
    private volatile HostChannel channel;
    // Owned by the main thread: whether the host has an output, and the index
    // and mute both sides last agreed on.
    private boolean known;
    private int agreedIndex = -1;
    private boolean agreedMuted;
    private final BroadcastReceiver changes = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) {
            if (intent.getIntExtra(AudioManager.EXTRA_VOLUME_STREAM_TYPE, -1)
                    == AudioManager.STREAM_MUSIC) fromAndroid();
        }
    };

    public HostVolume(Context context) {
        this.context = context;
        audio = context.getSystemService(AudioManager.class);
        main = new Handler(context.getMainLooper());
        min = audio.getStreamMinVolume(AudioManager.STREAM_MUSIC);
        max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC);
    }

    public void start() {
        IntentFilter filter = new IntentFilter(AudioManager.VOLUME_CHANGED_ACTION);
        filter.addAction(AudioManager.STREAM_MUTE_CHANGED_ACTION);
        context.registerReceiver(changes, filter, null, main, Context.RECEIVER_NOT_EXPORTED);
        Thread worker = new Thread(this::connect, "droidloom-volume");
        worker.setDaemon(true);
        worker.start();
    }

    private void connect() {
        com.android.droidloom.runtime.CpuPlacement.background();
        while (!stopped) {
            try (HostChannel current = HostChannel.open(HostChannel.VOLUME)) {
                channel = current;
                Records.Record record;
                while ((record = Records.read(current.getInputStream())) != null) {
                    if (record.kind == Records.VOLUME_STATE) {
                        float level = record.level;
                        boolean muted = record.flag;
                        main.post(() -> fromHost(level, muted));
                    } else if (record.kind == Records.VOLUME_UNAVAILABLE) {
                        main.post(this::forget);
                    }
                }
            } catch (Exception error) {
                if (!stopped) Log.w(TAG, "Host volume channel: " + error.getClass().getSimpleName());
            }
            channel = null;
            main.post(this::forget);
            if (!stopped) try { Thread.sleep(2000); } catch (InterruptedException ignored) { }
        }
    }

    private void fromHost(float level, boolean muted) {
        int index = level <= 0f ? min
                : Math.max(min + 1, min + Math.round(level * (max - min)));
        known = true;
        agreedIndex = index;
        agreedMuted = muted;
        if (audio.getStreamVolume(AudioManager.STREAM_MUSIC) != index)
            audio.setStreamVolume(AudioManager.STREAM_MUSIC, index, 0);
        if (index > min && audio.isStreamMute(AudioManager.STREAM_MUSIC) != muted)
            audio.adjustStreamVolume(AudioManager.STREAM_MUSIC,
                    muted ? AudioManager.ADJUST_MUTE : AudioManager.ADJUST_UNMUTE, 0);
    }

    private void fromAndroid() {
        HostChannel current = channel;
        if (current == null || !known) return;
        int index = audio.getStreamVolume(AudioManager.STREAM_MUSIC);
        // Android mutes the stream by itself at index 0; that is the level, not
        // the host's mute.
        boolean muted = index > min ? audio.isStreamMute(AudioManager.STREAM_MUSIC) : agreedMuted;
        if (index == agreedIndex && muted == agreedMuted) return;
        agreedIndex = index;
        agreedMuted = muted;
        float level = max > min ? (float) (index - min) / (max - min) : 0f;
        Records.send(current, Records.level(Records.VOLUME_SET, level, muted));
    }

    /** Disconnected, or the host has no output: nothing to follow. */
    private void forget() {
        known = false;
        agreedIndex = -1;
    }

    public void dump(PrintWriter out) {
        out.println("hostVolume connected=" + (channel != null) + " known=" + known
                + " index=" + agreedIndex + "/" + max + " muted=" + agreedMuted);
    }

    @Override public void close() {
        stopped = true;
        try { context.unregisterReceiver(changes); } catch (IllegalArgumentException ignored) { }
        HostChannel current = channel;
        if (current != null) current.shutdownInput();
    }
}
