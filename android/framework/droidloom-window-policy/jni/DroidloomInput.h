// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
#pragma once
#include <binder/IBinder.h>
#include <binder/Parcel.h>
#include <jni.h>
#include <memory>
#include <mutex>
namespace android {
class InputManager;
class InputReaderInterface;
namespace inputdispatcher { class InputDispatcher; }
class DroidloomInput {
public:
    // Keep InputManager's allocation/layout compatible with prebuilt JNI users.
    static void registerManager(InputManager*, inputdispatcher::InputDispatcher&,
            InputReaderInterface&, JavaVM*);
    static std::shared_ptr<DroidloomInput> forManager(InputManager*);
    static void unregisterManager(InputManager*);
    DroidloomInput(inputdispatcher::InputDispatcher& dispatcher, InputReaderInterface& reader,
            JavaVM* vm);
    ~DroidloomInput();
    status_t transact(uint32_t code, const Parcel& data, Parcel* reply);
    void stop();
private:
    static bool inject(uintptr_t context, const struct DroidloomInputEvent* event, uintptr_t token);
    static bool workerLifecycle(uintptr_t context, bool starting);
    inputdispatcher::InputDispatcher& mDispatcher;
    InputReaderInterface& mReader;
    JavaVM* const mVm;
    std::mutex mRegistrationLock;
    std::mutex mPowerLock;
    sp<IBinder> mPower;
    void* mNative;
};
} // namespace android
