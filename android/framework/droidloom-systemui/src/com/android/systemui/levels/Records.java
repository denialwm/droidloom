/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.levels;

import com.android.droidloom.runtime.HostChannel;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/** The host brokers' DLDEV001 records for levels: one per packet, little-endian. */
final class Records {
    static final int VOLUME_SET = 80, VOLUME_STATE = 81, VOLUME_UNAVAILABLE = 82;
    static final int BRIGHTNESS_SET = 96, BRIGHTNESS_RELEASE = 97, BRIGHTNESS_STATE = 98,
            BRIGHTNESS_UNAVAILABLE = 99;
    private static final byte[] MAGIC = "DLDEV001".getBytes(StandardCharsets.US_ASCII);

    /** One received record: its kind, a level from 0 to 1 and a flag, where it has them. */
    static final class Record {
        final int kind;
        final float level;
        final boolean flag;
        Record(int kind, float level, boolean flag) {
            this.kind = kind; this.level = level; this.flag = flag;
        }
    }

    static byte[] plain(int kind) {
        return ByteBuffer.allocate(12).order(ByteOrder.LITTLE_ENDIAN).put(MAGIC).putInt(kind).array();
    }

    static byte[] level(int kind, float level, boolean flag) {
        return ByteBuffer.allocate(17).order(ByteOrder.LITTLE_ENDIAN).put(MAGIC).putInt(kind)
                .putFloat(Math.max(0f, Math.min(1f, level))).put((byte) (flag ? 1 : 0)).array();
    }

    static byte[] level(int kind, float level) {
        return ByteBuffer.allocate(16).order(ByteOrder.LITTLE_ENDIAN).put(MAGIC).putInt(kind)
                .putFloat(Math.max(0f, Math.min(1f, level))).array();
    }

    /** The next record, or null when the host closed the channel. */
    static Record read(InputStream input) throws IOException {
        byte[] bytes = new byte[64];
        int length = input.read(bytes);
        if (length <= 0) return null;
        if (length < 12 || !Arrays.equals(Arrays.copyOf(bytes, 8), MAGIC))
            throw new IOException("Not a host level record");
        ByteBuffer buffer = ByteBuffer.wrap(bytes, 0, length).order(ByteOrder.LITTLE_ENDIAN);
        buffer.position(8);
        int kind = buffer.getInt();
        float level = buffer.remaining() >= 4 ? buffer.getFloat() : 0f;
        boolean flag = buffer.remaining() >= 1 && buffer.get() != 0;
        if (!(level >= 0f && level <= 1f)) throw new IOException("Host level outside 0 to 1");
        return new Record(kind, level, flag);
    }

    static void send(HostChannel channel, byte[] record) {
        if (channel == null) return;
        try {
            // One write is one packet on the broker's sequenced-packet socket.
            channel.getOutputStream().write(record);
        } catch (IOException error) {
            channel.shutdownInput();
        }
    }

    private Records() {}
}
