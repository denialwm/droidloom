/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server;

import android.os.SystemProperties;
import android.util.Slog;

import java.util.function.Supplier;

/** One boot-time query of the Android Vulkan loader, before PackageManager publishes features. */
public final class DroidloomGraphicsCapabilities {
    private static final String TAG = "DroidloomGraphicsCapabilities";
    private static final String ABI = "DROIDLOOM_VULKAN_CAPABILITIES_ABI=2;";
    private static final String HWUI_VULKAN = "ro.hwui.use_vulkan";

    /**
     * Returns {API version, hardware level, HWUI Vulkan 0 or 1}, or null when support cannot be
     * established. Version and level are 0 and -1 when no device has a hardware level, as on
     * GPUs without ETC2; HWUI may still run there.
     */
    public static int[] detectVulkan() {
        return detectVulkan(() -> {
            System.loadLibrary("inputflinger");
            return probeVulkan();
        });
    }

    static int[] detectVulkan(Supplier<int[]> probe) {
        try {
            int[] capabilities = probe.get();
            if (capabilities == null || capabilities.length != 3
                    || capabilities[2] < 0 || capabilities[2] > 1
                    || !(capabilities[0] == 0 && capabilities[1] == -1 && capabilities[2] == 1
                            || capabilities[0] >= 0x00400000 && (capabilities[0] >>> 29) == 0
                                    && capabilities[1] >= 0 && capabilities[1] <= 1)) {
                Slog.i(TAG, "No hardware Vulkan capabilities detected");
                return null;
            }
            Slog.i(TAG, ABI + " version=0x" + Integer.toHexString(capabilities[0])
                    + " level=" + capabilities[1] + " hwui=" + capabilities[2]);
            return capabilities;
        } catch (LinkageError | RuntimeException failure) {
            Slog.w(TAG, "Vulkan capability probe failed; leaving features absent", failure);
            return null;
        }
    }

    /** PackageManager's Vulkan features need a hardware level. */
    public static boolean hasHardwareLevel(int[] capabilities) {
        return capabilities != null && capabilities[1] >= 0;
    }

    /**
     * Make Vulkan HWUI's default where the probe found its requirements met. Each app reads
     * this when it starts; debug.hwui.renderer still overrides it, and a product value stays.
     */
    public static void selectHwuiRenderer(int[] capabilities) {
        if (capabilities == null || capabilities[2] != 1
                || !SystemProperties.get(HWUI_VULKAN).isEmpty()) {
            return;
        }
        try {
            SystemProperties.set(HWUI_VULKAN, "true");
        } catch (RuntimeException failure) {
            Slog.w(TAG, "Could not make Vulkan the HWUI default", failure);
        }
    }

    private static native int[] probeVulkan();

    private DroidloomGraphicsCapabilities() {}
}
