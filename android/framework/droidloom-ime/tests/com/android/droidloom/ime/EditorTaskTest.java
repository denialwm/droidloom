package com.android.droidloom.ime;

/** Host-JVM regression tests; no Android process or UI required. */
public final class EditorTaskTest {
    private static void check(boolean value) {
        if (!value) throw new AssertionError();
    }

    public static void main(String[] args) {
        EditorTask task = new EditorTask();
        check(task.get() == 0);
        task.start(7, false);
        check(task.get() == 7);
        // The app replaces the field's text and restarts its editor itself.
        task.start(0, true);
        check(task.get() == 7);
        // Android restarts the editor and names the task again.
        task.start(7, true);
        check(task.get() == 7);
        // A new editor in a window without a task has none.
        task.start(0, false);
        check(task.get() == 0);
        task.start(0, true);
        check(task.get() == 0);
        task.start(9, true);
        check(task.get() == 9);
        System.out.println("EditorTask: 7 assertions passed");
    }
}
