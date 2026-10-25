/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.content.Context;
import android.os.Process;
import android.provider.MediaStore;
import android.util.Slog;
import java.util.LinkedHashSet;
import java.util.Iterator;
import org.json.JSONArray;

/**
 * Host writes to shared folders bypass MediaProvider's FUSE layer, so the supervisor names the
 * changed paths and MediaProvider scans each one: a new file is indexed, a gone one removed.
 * Scans run on their own thread so a large folder never holds up launches.
 */
final class DroidloomMediaScan {
    private static final String ROOT = "/storage/emulated/0/";
    private static final int MAX_QUEUED = 4096;
    private static final LinkedHashSet<String> QUEUE = new LinkedHashSet<>();
    private static Thread worker;

    static String queue(Context context, JSONArray paths) throws Exception {
        if (paths.length() == 0 || paths.length() > 256) throw new IllegalArgumentException("Invalid scan batch size");
        String[] valid = new String[paths.length()];
        for (int i = 0; i < valid.length; i++) {
            String path = paths.getString(i);
            if (!path.startsWith(ROOT) || path.length() > 4096 || path.indexOf('\0') >= 0
                    || path.contains("/../") || path.endsWith("/..") || path.contains("/./"))
                throw new IllegalArgumentException("Invalid shared-storage path");
            valid[i] = path;
        }
        synchronized (QUEUE) {
            for (String path : valid) if (QUEUE.size() < MAX_QUEUED) QUEUE.add(path);
            if (worker == null) {
                worker = new Thread(() -> drain(context), "DroidloomMedia");
                worker.setDaemon(true); worker.start();
            }
            QUEUE.notify();
            return "queued=" + QUEUE.size();
        }
    }
    private static void drain(Context context) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_BACKGROUND);
        while (true) {
            String path;
            synchronized (QUEUE) {
                while (QUEUE.isEmpty()) {
                    try { QUEUE.wait(); } catch (InterruptedException ignored) { }
                }
                Iterator<String> next = QUEUE.iterator();
                path = next.next(); next.remove();
            }
            try {
                context.getContentResolver().call(MediaStore.AUTHORITY, "scan_file", path, null);
            } catch (Exception error) {
                Slog.w("DroidloomMedia", "Scan failed for a shared-storage path: " + error);
            }
        }
    }
    private DroidloomMediaScan() { }
}
