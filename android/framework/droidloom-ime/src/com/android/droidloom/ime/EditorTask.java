package com.android.droidloom.ime;

/**
 * The task of the window whose editor is started. Android names it in the
 * editor it hands over at a start. An app that restarts its own editor, as a
 * text field does when its text is replaced, reaches the method directly with
 * the app's copy of the editor, which names no task: the restarted editor is
 * in the same window, so its task stays.
 */
final class EditorTask {
    private int task;

    void start(int named, boolean restarting) {
        if (named > 0 || !restarting) task = named;
    }

    int get() { return task; }
}
