/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;
final class DroidloomFrameworkNative {
    static { System.loadLibrary("inputflinger"); }
    static native boolean start();
    static native byte[] next();
    static native boolean reply(byte[] bytes, boolean success);
    static native void catalogChanged();
    static native boolean credentialAsk(long id, byte[] json);
    static native void credentialEnd(long id);
    private DroidloomFrameworkNative() {}
}
