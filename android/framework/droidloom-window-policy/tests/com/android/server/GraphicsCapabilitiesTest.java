/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server;

public final class GraphicsCapabilitiesTest {
    public static void main(String[] args) {
        for (int level : new int[] {0, 1}) {
            for (int hwui : new int[] {0, 1}) {
                int[] detected = DroidloomGraphicsCapabilities.detectVulkan(
                        () -> new int[] {0x404000, level, hwui});
                if (detected == null || detected[0] != 0x404000 || detected[1] != level
                        || detected[2] != hwui) {
                    throw new AssertionError("Valid hardware capabilities lost");
                }
            }
        }
        int[] noLevel = DroidloomGraphicsCapabilities.detectVulkan(() -> new int[] {0, -1, 1});
        if (noLevel == null || DroidloomGraphicsCapabilities.hasHardwareLevel(noLevel)) {
            throw new AssertionError("HWUI without a hardware level lost or given features");
        }
        for (int[] invalid : new int[][] {null, {}, {0x404000}, {0x404000, 1}, {0, 0, 0}, {0, -1, 0},
                {0x404000, -1, 0}, {0x404000, 2, 0}, {0x20404000, 1, 0}, {0x404000, 1, -1},
                {0x404000, 1, 2}}) {
            if (DroidloomGraphicsCapabilities.detectVulkan(() -> invalid) != null) {
                throw new AssertionError("Invalid capabilities published");
            }
        }
        if (DroidloomGraphicsCapabilities.detectVulkan(() -> { throw new UnsatisfiedLinkError(); }) != null
                || DroidloomGraphicsCapabilities.detectVulkan(() -> { throw new IllegalStateException(); }) != null) {
            throw new AssertionError("Probe failure published features");
        }
        DroidloomGraphicsCapabilities.selectHwuiRenderer(null);
        DroidloomGraphicsCapabilities.selectHwuiRenderer(new int[] {0x404000, 1, 0});
        if (!android.os.SystemProperties.get("ro.hwui.use_vulkan").isEmpty()) {
            throw new AssertionError("HWUI Vulkan selected without its requirements");
        }
        android.os.SystemProperties.set("ro.hwui.use_vulkan", "false");
        DroidloomGraphicsCapabilities.selectHwuiRenderer(new int[] {0x404000, 1, 1});
        if (!android.os.SystemProperties.get("ro.hwui.use_vulkan").equals("false")) {
            throw new AssertionError("Product HWUI choice overridden");
        }
        for (int[] selected : new int[][] {{0x404000, 1, 1}, {0, -1, 1}}) {
            android.os.SystemProperties.set("ro.hwui.use_vulkan", "");
            DroidloomGraphicsCapabilities.selectHwuiRenderer(selected);
            if (!android.os.SystemProperties.get("ro.hwui.use_vulkan").equals("true")) {
                throw new AssertionError("HWUI Vulkan not selected");
            }
        }
        System.out.println("PASS: Java Vulkan feature boundary, HWUI default and failed probe handling");
    }
}
