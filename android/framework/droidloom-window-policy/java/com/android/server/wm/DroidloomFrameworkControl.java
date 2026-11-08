/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import android.content.BroadcastReceiver;
import android.app.ActivityManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ApplicationInfo;
import android.graphics.Rect;
import android.os.PowerManager;
import android.os.SystemClock;
import android.os.SystemProperties;
import android.provider.Settings;
import android.util.Slog;
import com.android.internal.widget.LockPatternUtils;
import java.nio.charset.StandardCharsets;
import org.json.JSONArray;
import org.json.JSONObject;

/** Android API glue. Rust owns authenticated native transport, framing and event coalescing. */
public final class DroidloomFrameworkControl {
    public static final String BUILD_ABI = "DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;";
    private static final DroidloomFrameworkStartup STARTUP = new DroidloomFrameworkStartup();
    private final ActivityTaskManagerService service;
    private final DroidloomTaskBackend backend;
    private boolean started;
    DroidloomFrameworkControl(ActivityTaskManagerService service, DroidloomTaskBackend backend) {
        this.service = service; this.backend = backend;
        STARTUP.attach(this::startInstance);
    }
    public static void start() {
        STARTUP.policyReady();
    }
    private synchronized void startInstance() {
        if (started) return;
        if (!DroidloomFrameworkNative.start()) throw new IllegalStateException("Framework native listener unavailable");
        configure();
        DroidloomCredential.register(service.mContext);
        BroadcastReceiver changes = new BroadcastReceiver() {
            @Override public void onReceive(Context context, Intent intent) { DroidloomFrameworkNative.catalogChanged(); }
        };
        IntentFilter packages = new IntentFilter();
        for (String action : new String[] {Intent.ACTION_PACKAGE_ADDED, Intent.ACTION_PACKAGE_REMOVED,
                Intent.ACTION_PACKAGE_CHANGED, Intent.ACTION_PACKAGE_REPLACED}) packages.addAction(action);
        packages.addDataScheme("package");
        service.mContext.registerReceiver(changes, packages, Context.RECEIVER_NOT_EXPORTED);
        IntentFilter configuration = new IntentFilter();
        for (String action : new String[] {Intent.ACTION_LOCALE_CHANGED, Intent.ACTION_USER_UNLOCKED,
                Intent.ACTION_EXTERNAL_APPLICATIONS_AVAILABLE, Intent.ACTION_EXTERNAL_APPLICATIONS_UNAVAILABLE,
                Intent.ACTION_PACKAGES_SUSPENDED, Intent.ACTION_PACKAGES_UNSUSPENDED}) configuration.addAction(action);
        service.mContext.registerReceiver(changes, configuration, Context.RECEIVER_NOT_EXPORTED);
        Thread worker = new Thread(this::serve, "DroidloomRPC");
        worker.setDaemon(true); worker.start(); started = true;
        SystemProperties.set("sys.droidloom.framework_ready", "1");
        Slog.i("DroidloomRPC", BUILD_ABI + " launch/catalog ready");
    }
    /**
     * Before any task exists: a task created on a fullscreen display stays fullscreen until the
     * display turns freeform, which then gives it Android's default freeform bounds.
     */
    private void configure() {
        Context context = service.mContext;
        Settings.Global.putInt(context.getContentResolver(), "force_resizable_activities", 1);
        new LockPatternUtils(context).setLockScreenDisabled(true, 0);
        Settings.System.putIntForUser(context.getContentResolver(), "screen_off_timeout", Integer.MAX_VALUE, 0);
        for (String setting : new String[] {"peak_refresh_rate", "min_refresh_rate"})
            Settings.System.putFloatForUser(context.getContentResolver(), setting, Float.POSITIVE_INFINITY, 0);
        // The host's name, given by the supervisor at each start (identity.rs).
        String name = SystemProperties.get("ro.droidloom.device_name");
        if (!name.isEmpty()) Settings.Global.putString(context.getContentResolver(), Settings.Global.DEVICE_NAME, name);
        service.mWindowManager.setWindowingMode(0, android.app.WindowConfiguration.WINDOWING_MODE_FREEFORM);
    }
    /**
     * Some apps read the density and font scale once per process and ignore later changes.
     * Visible app activities restart their process with saved state, as Android does for size
     * compat, and cached processes die so the next open starts fresh. Foreground services stay.
     */
    private void restartForSize() {
        synchronized (service.mGlobalLock) {
            service.mRootWindowContainer.forAllActivities(activity -> {
                if (activity.app == null || activity.app.isPersistent() || !activity.isVisibleRequested()
                        || activity.isActivityTypeHome()
                        || (activity.info.applicationInfo.flags & ApplicationInfo.FLAG_SYSTEM) != 0) return;
                activity.restartProcessIfVisible();
            });
        }
        service.mAmInternal.killAllBackgroundProcessesExcept(-1, ActivityManager.PROCESS_STATE_HOME);
    }
    private void serve() {
        while (true) {
            byte[] bytes = DroidloomFrameworkNative.next();
            if (bytes == null) {
                Slog.w("DroidloomRPC", "Framework receiver unavailable; retrying");
                SystemClock.sleep(1000); continue;
            }
            boolean success = false;
            String result;
            try {
                int op = ((bytes[0] & 255) << 8) | (bytes[1] & 255);
                JSONObject request = new JSONObject(new String(bytes, 2, bytes.length - 2, StandardCharsets.UTF_8));
                if (request.optInt("user", -1) != 0) throw new IllegalArgumentException("Only Android user 0 is supported");
                result = dispatch(op, request); success = true;
            } catch (Exception error) {
                result = error.getClass().getSimpleName() + ": " + error.getMessage();
            }
            DroidloomFrameworkNative.reply(result.getBytes(StandardCharsets.UTF_8), success);
        }
    }
    private static String packageOf(JSONObject request) throws Exception {
        String name = request.getString("package");
        if (!name.matches("[A-Za-z0-9_]+(\\.[A-Za-z0-9_]+)+")) throw new IllegalArgumentException("Invalid package");
        return name;
    }
    private String dispatch(int op, JSONObject request) throws Exception {
        switch (op) {
            case 1: {
                String owner = request.getString("package");
                ComponentName component = null;
                if (request.has("component")) {
                    component = ComponentName.unflattenFromString(request.getString("component"));
                    if (component == null) throw new IllegalArgumentException("Invalid component");
                }
                if (!owner.matches("[A-Za-z0-9_]+(\\.[A-Za-z0-9_]+)+")
                        || (component != null && !owner.equals(component.getPackageName())))
                    throw new IllegalArgumentException("Invalid launcher identity");
                Rect bounds = null;
                if (request.has("width") || request.has("height")) {
                    int width = request.getInt("width"), height = request.getInt("height");
                    if (width <= 0 || height <= 0 || width > 16384 || height > 16384)
                        throw new IllegalArgumentException("Invalid launch bounds");
                    bounds = new Rect(0, 0, width, height);
                }
                service.mContext.getSystemService(PowerManager.class).wakeUp(
                        SystemClock.uptimeMillis(), 1, "droidloom:launch");
                service.mWindowManager.dismissKeyguard(null, null);
                // Explicit resolution launches retain the old launcher's -S behavior.
                // Ordinary reopens keep Android's task and activity reuse semantics.
                if (bounds != null) ActivityManager.getService().forceStopPackage(owner, 0);
                return backend.launch(owner, component, 0, bounds);
            }
            case 2: {
                int dpi = request.getInt("dpi");
                if (dpi < 72) throw new IllegalArgumentException("Invalid display density");
                // The size is the density alone: a font scale left from Android's settings goes.
                boolean changed = Settings.System.getFloatForUser(
                        service.mContext.getContentResolver(), Settings.System.FONT_SCALE, 1f, 0) != 1f;
                if (changed) {
                    Settings.System.putFloatForUser(
                            service.mContext.getContentResolver(), Settings.System.FONT_SCALE, 1f, 0);
                    service.updateFontScaleIfNeeded(0);
                }
                if (service.mWindowManager.getBaseDisplayDensity(0) != dpi) {
                    service.mWindowManager.setForcedDisplayDensityForUser(0, dpi, 0);
                    changed = true;
                }
                if (changed) restartForSize();
                return "dpi=" + dpi;
            }
            case 3: return DroidloomApplicationCatalog.listApplications(service.mContext, 0).toString();
            case 4: {
                JSONArray components = request.getJSONArray("components");
                if (components.length() == 0 || components.length() > 32)
                    throw new IllegalArgumentException("Invalid icon batch size");
                int size = request.getInt("size");
                if (size < 16 || size > 512) throw new IllegalArgumentException("Invalid icon size");
                if ((long) size * size * components.length() > 1048576)
                    throw new IllegalArgumentException("Icon batch exceeds pixel budget");
                JSONObject icons = new JSONObject();
                for (int i = 0; i < components.length(); i++) {
                    String name = components.getString(i);
                    try { icons.put(name, DroidloomApplicationCatalog.renderIcon(
                            service.mContext, 0, ComponentName.unflattenFromString(name), size)); }
                    catch (Exception error) { /* Package removal can race a batch; retry after its event. */ }
                }
                return icons.toString();
            }
            case 5: return DroidloomPackageControl.details(service.mContext, packageOf(request)).toString();
            case 6: return DroidloomPackageControl.act(
                    service.mContext, backend, packageOf(request), request.getString("action"));
            case 7: {
                JSONObject display = new JSONObject();
                display.put("dpi", service.mWindowManager.getBaseDisplayDensity(0));
                display.put("initial_dpi", service.mWindowManager.getInitialDisplayDensity(0));
                return display.toString();
            }
            case 8: return DroidloomMediaScan.queue(service.mContext, request.getJSONArray("paths"));
            case 9: return DroidloomCredential.hostLocked(service.mContext, request.getBoolean("locked"));
            default: throw new IllegalArgumentException("Unknown framework operation");
        }
    }
}
