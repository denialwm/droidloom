/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.levels;

import android.content.Context;
import android.media.AudioDeviceAttributes;
import android.media.AudioDeviceInfo;
import android.media.AudioDeviceVolumeManager;
import android.media.AudioManager;
import android.media.VolumeInfo;
import android.util.Log;
import com.android.droidloom.runtime.HostChannel;
import java.io.PrintWriter;
import java.util.concurrent.Executor;
import java.util.concurrent.Executors;

/**
 * Android's media volume is the host's default output volume. While the host
 * has one, Android's output is an absolute-volume device, as Bluetooth
 * headphones are: Android plays music at full scale and hands each change of
 * its music volume here, and the host's own changes come back as the device's
 * volume, so apps see the level the host shows. Without a host output Android
 * scales its music itself again.
 */
public final class HostVolume implements AutoCloseable {
    private static final String TAG = "DroidloomVolume";
    private final AudioManager audio;
    private final AudioDeviceVolumeManager devices;
    private final AudioDeviceAttributes output = new AudioDeviceAttributes(
            AudioDeviceAttributes.ROLE_OUTPUT, AudioDeviceInfo.TYPE_BUS, "droidloom-host-output");
    /** Every Android-side step runs here, in order. */
    private final Executor serial = Executors.newSingleThreadExecutor();
    private final int min, max;
    private volatile boolean stopped;
    private volatile HostChannel channel;
    // Owned by `serial`: whether Android's output follows the host, and the
    // index and mute both sides last agreed on.
    private boolean absolute;
    private int agreedIndex = -1;
    private boolean agreedMuted;

    private final AudioDeviceVolumeManager.OnAudioDeviceVolumeChangedListener changes =
            new AudioDeviceVolumeManager.OnAudioDeviceVolumeChangedListener() {
                @Override public void onAudioDeviceVolumeChanged(
                        AudioDeviceAttributes device, VolumeInfo volume) {
                    fromAndroid(volume);
                }
                // Adjustments arrive as changes: the behavior is registered without
                // handling them separately.
                @Override public void onAudioDeviceVolumeAdjusted(AudioDeviceAttributes device,
                        VolumeInfo volume, int direction, int mode) { }
            };

    public HostVolume(Context context) {
        audio = context.getSystemService(AudioManager.class);
        devices = new AudioDeviceVolumeManager(context);
        min = audio.getStreamMinVolume(AudioManager.STREAM_MUSIC);
        max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC);
    }

    public void start() {
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
                        serial.execute(() -> fromHost(level, muted));
                    } else if (record.kind == Records.VOLUME_UNAVAILABLE) {
                        serial.execute(this::release);
                    }
                }
            } catch (Exception error) {
                if (!stopped) Log.w(TAG, "Host volume channel: " + error.getClass().getSimpleName());
            }
            channel = null;
            serial.execute(this::release);
            if (!stopped) try { Thread.sleep(2000); } catch (InterruptedException ignored) { }
        }
    }

    private VolumeInfo volume(int index, boolean muted) {
        return new VolumeInfo.Builder(AudioManager.STREAM_MUSIC).setMinVolumeIndex(min)
                .setMaxVolumeIndex(max).setVolumeIndex(index).setMuted(muted).build();
    }

    private void fromHost(float level, boolean muted) {
        int index = min + Math.round(level * (max - min));
        agreedIndex = index;
        agreedMuted = muted;
        try {
            if (!absolute) {
                devices.setDeviceAbsoluteVolumeBehavior(output, volume(index, muted), serial, changes);
                absolute = true;
            }
            // Applied by the host already: Android takes it without sending it back.
            devices.notifyAbsoluteVolumeChanged(volume(index, muted), output);
        } catch (RuntimeException error) {
            Log.w(TAG, "Android volume not updated: " + error.getClass().getSimpleName());
        }
    }

    private void fromAndroid(VolumeInfo volume) {
        int index = volume.getVolumeIndex();
        if (!absolute || index == VolumeInfo.INDEX_NOT_SET) return;
        boolean muted = volume.hasMuteState() ? volume.isMuted() : agreedMuted;
        if (index == agreedIndex && muted == agreedMuted) return;
        agreedIndex = index;
        agreedMuted = muted;
        float level = max > min ? (float) (index - min) / (max - min) : 0f;
        Records.send(channel, Records.level(Records.VOLUME_SET, level, muted));
    }

    /** No host output: Android scales its music itself again. */
    private void release() {
        if (!absolute) return;
        absolute = false;
        agreedIndex = -1;
        try {
            devices.setDeviceVolumeBehavior(output, AudioDeviceVolumeManager.DEVICE_VOLUME_BEHAVIOR_VARIABLE);
        } catch (RuntimeException error) {
            Log.w(TAG, "Android volume behavior not restored: " + error.getClass().getSimpleName());
        }
    }

    public void dump(PrintWriter out) {
        out.println("hostVolume connected=" + (channel != null) + " absolute=" + absolute
                + " index=" + agreedIndex + "/" + max + " muted=" + agreedMuted);
    }

    @Override public void close() {
        stopped = true;
        HostChannel current = channel;
        if (current != null) current.shutdownInput();
    }
}
