/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.server.wm;

import static android.view.WindowManager.TRANSIT_TO_BACK;
import static android.view.WindowManager.TRANSIT_TO_FRONT;
import static android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON;

import android.app.ActivityManager;
import android.app.ActivityOptions;
import android.app.WaitResult;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.content.res.Configuration;
import android.graphics.Rect;
import android.os.Binder;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.Process;
import android.os.SystemProperties;
import android.os.SystemClock;
import android.util.Slog;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.Map;
import java.util.concurrent.FutureTask;
import java.util.concurrent.TimeUnit;

/** Framework-owned task publication. No task-list Binder calls or child processes. */
final class DroidloomTaskBackend {
    static final boolean ENABLED = SystemProperties.getBoolean(
            "ro.vendor.droidloom.surfaceflinger_tasks", false);
    private static final String TAG = "DroidloomTasks";
    private final ActivityTaskManagerService mService;
    private final Handler mWorker;
    // Accessed only under mService.mGlobalLock.
    private final IdentityHashMap<Task, Long> mIncarnations = new IdentityHashMap<>();
    // The task in front when each task was created, under mService.mGlobalLock.
    private final IdentityHashMap<Task, Task> mCreatedOver = new IdentityHashMap<>();
    private final IdentityHashMap<WaitResult, ActivityRecord> mLaunches = new IdentityHashMap<>();
    // A new system_server in the same boot must not reuse earlier incarnations.
    private long mNextIncarnation = SystemClock.elapsedRealtimeNanos();
    private Task mHostFocused;
    // The density the host expects to give the next window; 0 is the display's own.
    private int mNewTaskDensity;
    // Tasks with a shown FLAG_KEEP_SCREEN_ON window, as of the last surface placement.
    private final IdentityHashMap<Task, Boolean> mKeepScreenOn = new IdentityHashMap<>();
    private final IdentityHashMap<Task, Boolean> mPlacedKeepScreenOn = new IdentityHashMap<>();
    // Accessed only on mWorker. Background tasks retain their existing host window.
    private final IdentityHashMap<Task, Snapshot> mPublished = new IdentityHashMap<>();
    private final IdentityHashMap<Task, Snapshot> mInputPublished = new IdentityHashMap<>();
    private long mEpoch;
    private Task mFocused;
    private final Runnable mReconcile = this::reconcile;
    private final Runnable mHealth = this::checkConnection;

    private static final class Snapshot {
        final Task token;
        final int id, user, display, parent;
        final long incarnation;
        final String owner;
        final boolean ready, focused, hostFocused, keepScreenOn;
        final Rect bounds;
        final IBinder inputToken;
        Snapshot(Task task, long incarnation, boolean hostFocused, boolean keepScreenOn,
                Task createdOver, int lastParent) {
            token = task; id = task.mTaskId; user = task.mUserId;
            display = task.getDisplayId(); this.incarnation = incarnation;
            owner = task.realActivity == null ? null : task.realActivity.getPackageName();
            ActivityRecord top = task.getTopNonFinishingActivity();
            bounds = new Rect(task.getBounds());
            inputToken = top == null || top.isNoDisplay() || bounds.isEmpty() ? null
                    : top.getInputApplicationHandle(false).token;
            ready = task.isVisibleRequested() && top != null && !top.isNoDisplay();
            focused = task.isFocused();
            this.hostFocused = hostFocused;
            this.keepScreenOn = keepScreenOn;
            // As on Android, a task that does not cover the one it opened over,
            // such as a dialog, leaves that task running: the host gets it as
            // the window's parent. Only when that task's app started it, so a
            // launch from the host is never a dialog of the app it replaces.
            // A finished dialog keeps its parent until its window goes, so
            // the host hands the front back to that task.
            ActivityRecord root = task.getRootActivity();
            parent = top == null ? lastParent
                    : top.occludesParent() || root == null || createdOver == null
                    || !createdOver.isAttached() || root.launchedFromUid != createdOver.effectiveUid
                    ? 0 : createdOver.mTaskId;
        }
        long[] publish(boolean activate) {
            return DroidloomTaskNative.registerTask(id, incarnation, user, display, owner, activate,
                    keepScreenOn, parent);
        }
        boolean sameInput(Snapshot other) {
            return other != null && incarnation == other.incarnation && display == other.display
                    && inputToken == other.inputToken && bounds.equals(other.bounds);
        }
        boolean publishInput() {
            return inputToken != null && DroidloomInputNative.publish(
                    id, incarnation, display, bounds, inputToken);
        }
    }

    DroidloomTaskBackend(ActivityTaskManagerService service) {
        mService = service;
        HandlerThread thread = new HandlerThread(TAG);
        thread.start();
        mWorker = new Handler(thread.getLooper());
        new DroidloomFrameworkControl(service, this);
        schedule();
        mWorker.postDelayed(mHealth, 5000);
        // This receiver sleeps in native socket I/O at idle. It never performs
        // publication or holds WMS's lock while waiting/acknowledging records.
        new Thread(this::receiveCommands, TAG + "Control").start();
    }

    // Notification hooks may run under WMS locks: only enqueue/coalesce here.
    void schedule() {
        mWorker.removeCallbacks(mReconcile);
        mWorker.post(mReconcile);
    }

    /**
     * Surface placement of the default display visits a window. Android keeps
     * the screen on while a shown window has FLAG_KEEP_SCREEN_ON; here that is
     * per task, and the host keeps its displays on while the task's window is
     * visible there. Android's obscuring order is ignored: the host knows
     * which windows are seen.
     */
    void placeWindow(WindowState window) {
        if ((window.mAttrs.flags & FLAG_KEEP_SCREEN_ON) == 0 || !window.mHasSurface
                || !window.isDisplayed()) return;
        final Task task = window.getTask();
        if (task != null) mPlacedKeepScreenOn.put(task, Boolean.TRUE);
    }

    /** Surface placement ended: republish when a task's keep-screen-on changed. */
    void windowsPlaced() {
        if (!mPlacedKeepScreenOn.keySet().equals(mKeepScreenOn.keySet())) {
            mKeepScreenOn.clear();
            mKeepScreenOn.putAll(mPlacedKeepScreenOn);
            schedule();
        }
        mPlacedKeepScreenOn.clear();
    }

    void launchCompleted(WaitResult result, ActivityRecord record) {
        if (mLaunches.containsKey(result)) mLaunches.put(result, record);
    }

    private Snapshot snapshot(Task task) {
        Long incarnation = mIncarnations.get(task);
        if (incarnation == null) {
            if (mNextIncarnation == Long.MAX_VALUE) throw new IllegalStateException("Task epoch exhausted");
            incarnation = ++mNextIncarnation;
            mIncarnations.put(task, incarnation);
        }
        Snapshot published = mPublished.get(task);
        return new Snapshot(task, incarnation, task == mHostFocused, mKeepScreenOn.containsKey(task),
                mCreatedOver.get(task), published == null ? 0 : published.parent);
    }

    private ArrayList<Snapshot> snapshot() {
        ArrayList<Snapshot> tasks = new ArrayList<>();
        IdentityHashMap<Task, Boolean> live = new IdentityHashMap<>();
        synchronized (mService.mGlobalLock) {
            if (mHostFocused != null && !mHostFocused.isFocused()) mHostFocused = null;
            mService.mRootWindowContainer.forAllLeafTasks(task -> {
                if (task.mTaskId > 0 && task.mUserId == 0 && task.getDisplayId() == 0
                        && task.isActivityTypeStandard() && task.realActivity != null) {
                    live.put(task, true);
                    tasks.add(snapshot(task));
                }
            }, true);
            mIncarnations.keySet().retainAll(live.keySet());
            mCreatedOver.keySet().retainAll(live.keySet());
        }
        return tasks;
    }

    private void reconcile() {
        // Copy Android facts first. Rust socket waits never hold the global lock.
        ArrayList<Snapshot> tasks = snapshot();
        IdentityHashMap<Task, Snapshot> live = new IdentityHashMap<>();
        for (Snapshot task : tasks) live.put(task.token, task);
        boolean retry = false;
        // Invalidate input before attempting any potentially deferred graphics
        // retirement. Native dispatch/cancellation runs outside WMS's lock.
        for (Map.Entry<Task, Snapshot> entry : new ArrayList<>(mInputPublished.entrySet())) {
            Snapshot current = live.get(entry.getKey());
            if (current == null || current.inputToken == null) {
                Snapshot old = entry.getValue();
                if (DroidloomInputNative.retire(old.id, old.incarnation)) mInputPublished.remove(entry.getKey());
                else retry = true;
            }
        }
        for (Snapshot task : tasks) {
            if (task.inputToken == null || task.sameInput(mInputPublished.get(task.token))) continue;
            if (task.publishInput()) mInputPublished.put(task.token, task);
            else retry = true;
        }
        Task focused = null;
        for (Snapshot task : tasks) if (task.focused && task.ready) focused = task.token;
        for (Map.Entry<Task, Snapshot> entry : new ArrayList<>(mPublished.entrySet())) {
            if (!live.containsKey(entry.getKey())) {
                Snapshot task = entry.getValue();
                if (DroidloomTaskNative.retireTask(task.id, task.incarnation)) {
                    mPublished.remove(entry.getKey());
                } else {
                    // A retiring task must not starve publication of live apps.
                    retry = true;
                }
            }
        }
        for (Snapshot task : tasks) {
            if (!task.ready && !mPublished.containsKey(task.token)) continue;
            if (task.publish(task.focused && task.token != mFocused && !task.hostFocused) != null) {
                mPublished.put(task.token, task);
                if (task.focused) mFocused = task.token;
            }
            else { retry = true; break; }
        }
        if (retry) mWorker.postDelayed(mReconcile, 1000);
        else mFocused = focused;
    }

    private void checkConnection() {
        // Health only: enumerate tasks on events or connection changes, never at idle.
        long epoch = DroidloomTaskNative.epoch();
        if (epoch != mEpoch || epoch == 0) {
            mEpoch = epoch;
            schedule();
        }
        mWorker.postDelayed(mHealth, 5000);
    }

    private void receiveCommands() {
        while (true) {
            long[] command = DroidloomTaskNative.nextCommand();
            if (command == null) {
                SystemClock.sleep(1000); // reconnect backoff, never an idle task poll
                continue;
            }
            try {
                applyCommand(command);
            } catch (RuntimeException error) {
                Slog.w(TAG, "Rejected host window command for task " + command[1], error);
            } finally {
                // Rejected/stale commands are consumed too. Unacknowledged
                // commands remain retryable if this receiver disconnects.
                DroidloomTaskNative.acknowledgeCommand(command[0]);
            }
        }
    }

    private void applyCommand(long[] command) {
        if (command.length != 7) return;
        if (command[4] == 7) {
            // For the tasks to come: it names none.
            if (command[1] != 0 || command[2] != 0 || command[3] != 0
                    || (command[5] != 0 && (command[5] < 72 || command[5] > 1280))) return;
            synchronized (mService.mGlobalLock) { mNewTaskDensity = (int) command[5]; }
            return;
        }
        if (command[1] <= 0 || command[1] > Integer.MAX_VALUE
                || command[2] <= 0 || command[3] != 0) return;
        synchronized (mService.mGlobalLock) {
            final Task task = mService.mRootWindowContainer.anyTaskForId((int) command[1],
                    RootWindowContainer.MATCH_ATTACHED_TASK_ONLY);
            if (task == null || !task.isAttached() || task.mUserId != 0
                    || task.getDisplayId() != 0 || !task.isLeafTask()
                    || !task.isActivityTypeStandard() || task.realActivity == null
                    || !Long.valueOf(command[2]).equals(mIncarnations.get(task))) return;
            switch ((int) command[4]) {
                case 1:
                    // Host focus implies the window is shown.
                    setHostHidden(task, false);
                    mHostFocused = task;
                    mService.setFocusedTask(task.mTaskId, null /* touchedActivity */);
                    if (!task.isFocused()) mHostFocused = null;
                    break;
                case 2:
                    if (command[5] <= 0 || command[5] > 16384
                            || command[6] <= 0 || command[6] > 16384) return;
                    final Rect bounds = new Rect(0, 0, (int) command[5], (int) command[6]);
                    if (task.getBounds().equals(bounds)) return;
                    task.setResizeMode(ActivityInfo.RESIZE_MODE_RESIZEABLE);
                    // Direct call in system_server: preserve ATMS's existing
                    // Shell transition collection, configuration and surface crop.
                    mService.resizeTask(task.mTaskId, bounds, 0 /* RESIZE_MODE_SYSTEM */);
                    break;
                case 3:
                    mService.removeTask(task, "droidloom-host-close");
                    break;
                case 4:
                case 5:
                    setHostHidden(task, command[4] == 4);
                    if (command[4] == 4) {
                        // Hiding a task moves Android's focus to the one below
                        // it, such as the app behind a dialog. That is the
                        // host's doing, not that app asking for the front.
                        final Task below =
                                mService.mRootWindowContainer.getTopDisplayFocusedRootTask();
                        final Task leaf = below == null ? null : below.getTopLeafTask();
                        if (leaf != null && leaf != task) mHostFocused = leaf;
                    }
                    break;
                case 6:
                    if (command[5] != 0 && (command[5] < 72 || command[5] > 1280)) return;
                    setHostDensity(task, (int) command[5]);
                    break;
                default:
                    throw new IllegalArgumentException("Unknown host window command");
            }
        }
        schedule();
    }

    /**
     * Task.Builder made an application task, not yet attached. It starts at the
     * density the host expects to give its window, so its first activity is
     * created laid out for it: a density that arrives after the activity started
     * is a configuration change, and relaunches every activity that does not
     * handle one. Set directly, as the builder sets the activity type:
     * onParentChanged resolves it.
     */
    void taskCreated(Task task) {
        if (mNewTaskDensity != 0) {
            task.getRequestedOverrideConfiguration().densityDpi = mNewTaskDensity;
        }
        // Under the global lock, before the new task moves to the front.
        final Task focused = mService.mRootWindowContainer.getTopDisplayFocusedRootTask();
        final Task front = focused == null ? null : focused.getTopLeafTask();
        if (front != null && front != task) mCreatedOver.put(task, front);
    }

    /**
     * Lays the task out at the density of the host output its window is on,
     * so it keeps its size in that output's units on every screen. 0, or the
     * display's own density, drops the override. Activities that do not
     * handle density changes relaunch, as for any configuration change.
     */
    private void setHostDensity(Task task, int dpi) {
        final int inherited = task.getParent().getConfiguration().densityDpi;
        final int wanted = dpi == 0 || dpi == inherited
                ? Configuration.DENSITY_DPI_UNDEFINED : dpi;
        final Configuration requested = task.getRequestedOverrideConfiguration();
        if (requested.densityDpi == wanted) return;
        final Configuration changed = new Configuration(requested);
        changed.densityDpi = wanted;
        task.onRequestedOverrideConfigurationChanged(changed);
        // As WindowOrganizerController does for a configuration-only change.
        task.forAllActivities(r -> {
            if (r.isVisibleRequested()) r.ensureActivityConfiguration(true /* ignoreVisibility */);
        });
        mService.mWindowManager.mWindowPlacerLocked.requestTraversal();
    }

    /**
     * A minimized host window stops its task, like leaving an app on a phone:
     * activities pause and stop, and stay stopped even if Android reorders tasks.
     * Restoring makes the task visible again; host focus brings it to the front.
     */
    private void setHostHidden(Task task, boolean hidden) {
        final int flag = TaskFragment.FLAG_FORCE_HIDDEN_FOR_TASK_ORG;
        if (((task.mForceHiddenFlags & flag) != 0) == hidden) return;
        // Same transition handling as ATMS.setFocusedTask.
        final ActionChain chain = mService.mChainTracker.startTransit("droidloom-host-visibility");
        final TransitionController transitions = mService.getTransitionController();
        final Transition transition = !chain.isCollecting()
                && transitions.isShellTransitionsEnabled()
                ? transitions.createTransition(hidden ? TRANSIT_TO_BACK : TRANSIT_TO_FRONT) : null;
        if (transition != null) {
            chain.attachTransition(transition);
            transition.setReady(task, true /* ready */);
        }
        chain.collect(task);
        task.setForceHidden(flag, hidden);
        if (transition != null) {
            transitions.requestStartTransition(transition, task, null /* remote */, null /* display */);
        }
        mService.mRootWindowContainer.ensureActivitiesVisible();
        mService.mRootWindowContainer.resumeFocusedTasksTopActivities();
        mService.mChainTracker.end();
    }

    /** Launch through the real ActivityStarter; retain the exact completed record. */
    String launch(String owner, ComponentName component, int user, Rect bounds) throws Exception {
        if (user != 0 || owner == null || !owner.matches("[A-Za-z0-9_]+(\\.[A-Za-z0-9_]+)+")
                || (component != null && !owner.equals(component.getPackageName()))) {
            throw new IllegalArgumentException("Invalid Droidloom launcher identity");
        }
        Intent intent = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        if (component != null) intent.setComponent(component); else intent.setPackage(owner);
        return launch(intent, owner, user, bounds);
    }

    /** Starts `intent` as a new freeform task of `owner` and publishes its window. */
    String launch(Intent intent, String owner, int user, Rect bounds) throws Exception {
        int uid = Binder.getCallingUid();
        int pid = Binder.getCallingPid();
        if (uid != Process.ROOT_UID && uid != Process.SHELL_UID && uid != Process.SYSTEM_UID) {
            throw new SecurityException("Droidloom launch requires root, system or shell");
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        ActivityOptions options = ActivityOptions.makeBasic();
        options.setLaunchDisplayId(0);
        options.setLaunchWindowingMode(android.app.WindowConfiguration.WINDOWING_MODE_FREEFORM);
        if (bounds != null) options.setLaunchBounds(bounds);
        WaitResult result = new WaitResult();
        ActivityRecord[] initial = new ActivityRecord[1];
        synchronized (mService.mGlobalLock) { mLaunches.put(result, null); }
        final Snapshot task;
        try {
            int status = mService.getActivityStartController().obtainStarter(intent, "Droidloom launch")
                    .setCallingPid(pid).setCallingUid(uid)
                    .setCallingPackage(uid == Process.SHELL_UID ? "com.android.shell" : null)
                    .setActivityOptions(options.toBundle(), pid, uid).setUserId(user)
                    .setWaitResult(result).setOutActivity(initial).execute();
            if (result.timeout || (status != ActivityManager.START_SUCCESS
                    && status != ActivityManager.START_TASK_TO_FRONT
                    && status != ActivityManager.START_DELIVERED_TO_TOP)) {
                throw new IllegalStateException("Activity start failed: " + status);
            }
            synchronized (mService.mGlobalLock) {
                ActivityRecord completed = mLaunches.get(result);
                if (completed == null) completed = initial[0];
                Task selected = completed == null ? null : completed.getTask();
                if (selected == null || selected.mUserId != user || selected.getDisplayId() != 0
                        || !selected.isActivityTypeStandard() || completed.isNoDisplay()
                        || selected.realActivity == null) {
                    throw new IllegalStateException("Launch has no attached application task");
                }
                task = snapshot(selected);
            }
        } finally {
            synchronized (mService.mGlobalLock) { mLaunches.remove(result); }
        }
        FutureTask<long[]> publication = new FutureTask<>(() -> {
            synchronized (mService.mGlobalLock) {
                if (!task.token.isAttached() || task.token.getDisplayId() != task.display
                        || !Long.valueOf(task.incarnation).equals(mIncarnations.get(task.token))) {
                    return null;
                }
            }
            if (!task.publishInput()) return null;
            mInputPublished.put(task.token, task);
            long[] binding = task.publish(true);
            if (binding != null) mPublished.put(task.token, task);
            schedule();
            return binding;
        });
        mWorker.post(publication);
        long[] binding = publication.get(30, TimeUnit.SECONDS);
        if (binding == null) throw new IllegalStateException("Task publication failed");
        return "object=" + binding[0] + " composer_display=" + binding[1]
                + " android_display=" + binding[2] + " task=" + binding[3];
    }
}
