// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
// JNI only converts Java strings/results. Task state and transport live in Rust.
#include <jni.h>
#include <cstdint>
#include <cstddef>

struct TaskBinding { uint64_t object, display, android_display, task; };
struct TaskCommand { uint64_t serial, task, incarnation, android_display, kind, width, height; };
extern "C" bool droidloom_task_register(int32_t, int64_t, int32_t, int32_t,
        const uint8_t*, size_t, TaskBinding*, bool, bool);
extern "C" bool droidloom_task_retire(int32_t, int64_t);
extern "C" int64_t droidloom_task_epoch();
extern "C" int32_t droidloom_task_version();
extern "C" bool droidloom_task_next_command(TaskCommand*);
extern "C" bool droidloom_task_acknowledge(int64_t);

extern "C" JNIEXPORT jint JNICALL
Java_com_android_server_wm_DroidloomTaskNative_version(JNIEnv*, jclass) {
    return droidloom_task_version();
}

extern "C" JNIEXPORT jlong JNICALL
Java_com_android_server_wm_DroidloomTaskNative_epoch(JNIEnv*, jclass) {
    return droidloom_task_epoch();
}

extern "C" JNIEXPORT jlongArray JNICALL
Java_com_android_server_wm_DroidloomTaskNative_registerTask(JNIEnv* env, jclass,
        jint id, jlong incarnation, jint user, jint display, jstring package, jboolean activate,
        jboolean keepScreenOn) {
    if (package == nullptr) return nullptr;
    const jsize length = env->GetStringUTFLength(package);
    if (length < 1 || length > 255) return nullptr;
    const char* chars = env->GetStringUTFChars(package, nullptr);
    if (chars == nullptr) return nullptr;
    TaskBinding binding{};
    const bool okay = droidloom_task_register(id, incarnation, user, display,
            reinterpret_cast<const uint8_t*>(chars), static_cast<size_t>(length), &binding, activate != JNI_FALSE,
            keepScreenOn != JNI_FALSE);
    env->ReleaseStringUTFChars(package, chars);
    if (!okay) return nullptr;
    const jlong values[] = {static_cast<jlong>(binding.object), static_cast<jlong>(binding.display),
            static_cast<jlong>(binding.android_display), static_cast<jlong>(binding.task)};
    jlongArray result = env->NewLongArray(4);
    if (result != nullptr) env->SetLongArrayRegion(result, 0, 4, values);
    return result;
}

extern "C" JNIEXPORT jboolean JNICALL
Java_com_android_server_wm_DroidloomTaskNative_retireTask(JNIEnv*, jclass,
        jint id, jlong incarnation) {
    return droidloom_task_retire(id, incarnation) ? JNI_TRUE : JNI_FALSE;
}

extern "C" JNIEXPORT jlongArray JNICALL
Java_com_android_server_wm_DroidloomTaskNative_nextCommand(JNIEnv* env, jclass) {
    TaskCommand command{};
    if (!droidloom_task_next_command(&command)) return nullptr;
    const jlong values[] = {static_cast<jlong>(command.serial), static_cast<jlong>(command.task),
            static_cast<jlong>(command.incarnation), static_cast<jlong>(command.android_display),
            static_cast<jlong>(command.kind), static_cast<jlong>(command.width),
            static_cast<jlong>(command.height)};
    jlongArray result = env->NewLongArray(7);
    if (result != nullptr) env->SetLongArrayRegion(result, 0, 7, values);
    return result;
}

extern "C" JNIEXPORT jboolean JNICALL
Java_com_android_server_wm_DroidloomTaskNative_acknowledgeCommand(JNIEnv*, jclass, jlong serial) {
    return droidloom_task_acknowledge(serial) ? JNI_TRUE : JNI_FALSE;
}
