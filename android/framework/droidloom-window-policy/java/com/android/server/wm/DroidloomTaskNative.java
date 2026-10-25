/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

final class DroidloomTaskNative {
    static final String BUILD_ABI = "DROIDLOOM_TASK_BACKEND_ABI=3;";
    static {
        System.loadLibrary("inputflinger");
        if (version() != 3) throw new LinkageError(BUILD_ABI + " native ABI mismatch");
    }
    private static native int version();
    static native long epoch();
    static native long[] registerTask(int id, long incarnation, int user, int display, String owner,
            boolean activate, boolean keepScreenOn);
    static native boolean retireTask(int id, long incarnation);
    // serial, task ID, incarnation, Android display, kind, width, height.
    static native long[] nextCommand();
    static native boolean acknowledgeCommand(long serial);
    private DroidloomTaskNative() {}
}
