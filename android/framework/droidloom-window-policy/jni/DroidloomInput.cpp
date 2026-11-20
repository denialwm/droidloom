// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
// Android event construction only; decoding, routing and input state live in Rust.
#include "DroidloomInput.h"
#include <dispatcher/InputDispatcher.h>
#include <InputReaderBase.h>
#include <binder/IPCThreadState.h>
#include <input/Input.h>
#include <private/android_filesystem_config.h>
#include <processgroup/processgroup.h>
#include <utils/Log.h>
#include <array>
#include <map>

namespace android {
namespace {
// Registration and task-publication Binder calls borrow this ownership. Input
// events use the Rust worker's existing callback and never touch the registry.
std::mutex gManagersLock;
std::map<InputManager*, std::shared_ptr<DroidloomInput>> gManagers;
}

void DroidloomInput::registerManager(InputManager* manager,
        inputdispatcher::InputDispatcher& dispatcher, InputReaderInterface& reader, JavaVM* vm) {
    auto input = std::make_shared<DroidloomInput>(dispatcher, reader, vm);
    std::lock_guard lock(gManagersLock);
    gManagers.emplace(manager, std::move(input));
}
std::shared_ptr<DroidloomInput> DroidloomInput::forManager(InputManager* manager) {
    std::lock_guard lock(gManagersLock);
    auto found = gManagers.find(manager);
    return found == gManagers.end() ? nullptr : found->second;
}
void DroidloomInput::unregisterManager(InputManager* manager) {
    std::shared_ptr<DroidloomInput> input;
    {
        std::lock_guard lock(gManagersLock);
        auto found = gManagers.find(manager);
        if (found == gManagers.end()) return;
        input = std::move(found->second);
        gManagers.erase(found);
    }
    // Destruction joins the native worker outside the registry lock.
}

struct DroidloomInputPointer { int32_t id; float x, y, pressure; };
struct DroidloomInputEvent {
    int32_t kind, display, action, keyCode, scanCode, meta, repeat, flags, count;
    uint64_t downTime, eventTime, serial;
    DroidloomInputPointer pointers[32];
    float verticalScroll, horizontalScroll;
};
static_assert(sizeof(DroidloomInputEvent) == 584);
static_assert(offsetof(DroidloomInputEvent, downTime) == 40);
extern "C" void* droidloom_input_create(uintptr_t,
        bool (*)(uintptr_t, const DroidloomInputEvent*, uintptr_t), bool (*)(uintptr_t, bool));
extern "C" bool droidloom_input_start(void*);
extern "C" void droidloom_input_destroy(void*);
extern "C" bool droidloom_input_publish(int32_t, int32_t, int64_t, int32_t, int32_t,
        uint64_t, uintptr_t, void (*)(uintptr_t));
extern "C" void droidloom_input_retire(int32_t, int64_t);

static void releaseToken(uintptr_t token) { delete reinterpret_cast<sp<IBinder>*>(token); }

DroidloomInput::DroidloomInput(inputdispatcher::InputDispatcher& dispatcher,
        InputReaderInterface& reader, JavaVM* vm)
      : mDispatcher(dispatcher), mReader(reader), mVm(vm),
        mNative(droidloom_input_create(reinterpret_cast<uintptr_t>(this), inject, workerLifecycle)) {}
DroidloomInput::~DroidloomInput() { stop(); }
void DroidloomInput::stop() {
    std::lock_guard lock(mRegistrationLock);
    droidloom_input_destroy(mNative);
    mNative = nullptr;
}

status_t DroidloomInput::transact(uint32_t code, const Parcel& data, Parcel* reply) {
    if (reply == nullptr || !data.enforceInterface(String16("android.os.IInputFlinger"))) return BAD_VALUE;
    const uid_t uid = IPCThreadState::self()->getCallingUid();
    if (uid != AID_SYSTEM) return PERMISSION_DENIED;
    int32_t abi;
    if (data.readInt32(&abi) != OK || abi != (code == 0x00444c03 ? 2 : 1)) return BAD_VALUE;
    bool okay = false;
    if (code == 0x00444c03) {
        sp<IBinder> power;
        if (data.readStrongBinder(&power) != OK || power == nullptr || data.dataAvail() != 0) return BAD_VALUE;
        std::lock_guard registration(mRegistrationLock);
        if (mNative == nullptr) mNative = droidloom_input_create(reinterpret_cast<uintptr_t>(this), inject, workerLifecycle);
        {
            std::lock_guard lock(mPowerLock);
            mPower = power;
        }
        okay = droidloom_input_start(mNative);
    } else {
        int32_t task, display = 0, left = 0, top = 0;
        int64_t incarnation;
        if (data.readInt32(&task) != OK || data.readInt64(&incarnation) != OK || task <= 0 || incarnation <= 0) return BAD_VALUE;
        if (code == 0x00444c04) {
            sp<IBinder> token;
            if (data.readInt32(&display) != OK || data.readInt32(&left) != OK || data.readInt32(&top) != OK
                    || data.readStrongBinder(&token) != OK || token == nullptr || display < 0 || data.dataAvail() != 0) return BAD_VALUE;
            okay = droidloom_input_publish(task, display, incarnation, left, top,
                    reinterpret_cast<uint64_t>(token.get()), reinterpret_cast<uintptr_t>(new sp<IBinder>(token)), releaseToken);
        } else if (code == 0x00444c05 && data.dataAvail() == 0) {
            droidloom_input_retire(task, incarnation);
            okay = true;
        } else return BAD_VALUE;
    }
    const status_t status = reply->writeNoException();
    return status == OK ? reply->writeBool(okay) : status;
}

bool DroidloomInput::workerLifecycle(uintptr_t context, bool starting) {
    auto& input = *reinterpret_cast<DroidloomInput*>(context);
    if (input.mVm == nullptr) return true;
    // InputDispatcher invokes existing Java policy hooks synchronously. Attach
    // once per Rust worker, including cancellation dispatch before shutdown.
    if (!starting) return input.mVm->DetachCurrentThread() == JNI_OK;
    JNIEnv* env = nullptr;
    JavaVMAttachArgs args{JNI_VERSION_1_6, const_cast<char*>("DroidloomInput"), nullptr};
    const bool attached = input.mVm->AttachCurrentThreadAsDaemon(&env, &args) == JNI_OK;
    if (!attached) ALOGE("Droidloom input worker could not attach to the VM");
    // Every event passes through this thread on its way to InputDispatcher, so
    // it gets the dispatcher's own scheduling (InputThread.cpp).
    if (!SetTaskProfiles(0, {"InputPolicy"})) ALOGW("Droidloom input worker kept normal scheduling");
    return attached;
}

bool DroidloomInput::inject(uintptr_t context, const DroidloomInputEvent* event, uintptr_t token) {
    auto& input = *reinterpret_cast<DroidloomInput*>(context);
    if (event->kind == 6) {
        sp<IBinder> power;
        { std::lock_guard lock(input.mPowerLock); power = input.mPower; }
        if (power == nullptr) return false;
        Parcel data;
        data.writeInterfaceToken(String16("com.android.droidloom.input.PowerCallback"));
        data.writeBool(event->action != 0);
        return power->transact(IBinder::FIRST_CALL_TRANSACTION, data, nullptr, IBinder::FLAG_ONEWAY) == OK;
    }
    if (token == 0) return false;
    const sp<IBinder> target = *reinterpret_cast<const sp<IBinder>*>(token);
    const IdGenerator ids(IdGenerator::Source::OTHER);
    KeyEvent key;
    MotionEvent motion;
    InputEvent* androidEvent;
    if (event->kind == 2) {
        // The host keyboard's device, so apps read characters from the layout
        // Android chose for it (KeyboardLayoutManager, from the input method's
        // physical keyboard hint). That layout also moves keys, as AZERTY does:
        // the key at this scan code is the one its character map names.
        int32_t deviceId = -1;
        int32_t keyCode = event->keyCode;
        for (const InputDeviceInfo& device : input.mReader.getInputDevices()) {
            if (device.getIdentifier().uniqueId != "<droidloom-host-keyboard>") continue;
            deviceId = device.getId();
            int32_t mapped;
            const KeyCharacterMap* map = device.getKeyCharacterMap();
            if (map != nullptr && map->mapKey(event->scanCode, 0, &mapped) == OK) keyCode = mapped;
            break;
        }
        key.initialize(ids.nextId(), deviceId, AINPUT_SOURCE_KEYBOARD,
                ui::LogicalDisplayId{event->display}, {}, event->action, event->flags, keyCode,
                event->scanCode, event->meta, event->repeat, event->downTime, event->eventTime);
        androidEvent = &key;
    } else if (event->kind == 1 && event->count > 0 && event->count <= 32) {
        std::array<PointerProperties, 32> properties{};
        std::array<PointerCoords, 32> coords{};
        for (int32_t i = 0; i < event->count; ++i) {
            properties[i].id = event->pointers[i].id;
            properties[i].toolType = ToolType::FINGER;
            coords[i].setAxisValue(AMOTION_EVENT_AXIS_X, event->pointers[i].x);
            coords[i].setAxisValue(AMOTION_EVENT_AXIS_Y, event->pointers[i].y);
            coords[i].setAxisValue(AMOTION_EVENT_AXIS_PRESSURE, event->pointers[i].pressure);
            coords[i].setAxisValue(AMOTION_EVENT_AXIS_SIZE, 1.0f);
        }
        motion.initialize(ids.nextId(), 0, AINPUT_SOURCE_TOUCHSCREEN, ui::LogicalDisplayId{event->display},
                {}, event->action, 0, ftl::Flags<MotionFlag>(event->flags), 0, 0, 0, MotionClassification::NONE,
                ui::Transform{}, 1.0f, 1.0f, AMOTION_EVENT_INVALID_CURSOR_POSITION,
                AMOTION_EVENT_INVALID_CURSOR_POSITION, ui::Transform{}, event->downTime,
                event->eventTime, event->count, properties.data(), coords.data());
        androidEvent = &motion;
    } else if (event->kind == 7) {
        // A wheel or touchpad: Android's own mouse scroll, which views read
        // through onGenericMotionEvent. It is its own device, so it never
        // joins or interrupts a touch gesture.
        PointerProperties properties{};
        properties.id = 0;
        properties.toolType = ToolType::MOUSE;
        PointerCoords coords{};
        coords.setAxisValue(AMOTION_EVENT_AXIS_X, event->pointers[0].x);
        coords.setAxisValue(AMOTION_EVENT_AXIS_Y, event->pointers[0].y);
        coords.setAxisValue(AMOTION_EVENT_AXIS_VSCROLL, event->verticalScroll);
        coords.setAxisValue(AMOTION_EVENT_AXIS_HSCROLL, event->horizontalScroll);
        motion.initialize(ids.nextId(), 1, AINPUT_SOURCE_MOUSE, ui::LogicalDisplayId{event->display},
                {}, AMOTION_EVENT_ACTION_SCROLL, 0, ftl::Flags<MotionFlag>(0), 0, 0, 0,
                MotionClassification::NONE, ui::Transform{}, 1.0f, 1.0f, event->pointers[0].x,
                event->pointers[0].y, ui::Transform{}, event->downTime, event->eventTime, 1,
                &properties, &coords);
        androidEvent = &motion;
    } else return false;
    // The pinned dispatcher validates and updates its injection verifier before
    // enqueueing even in NONE mode. FAILED therefore must reach Rust; a socket
    // reconnect alone cannot retire Android's accepted pointer identities.
    const auto result = input.mDispatcher.injectInputEventToApplication(androidEvent, std::nullopt,
            android::os::InputEventInjectionSync::NONE, std::chrono::milliseconds(30'000),
            POLICY_FLAG_DISABLE_KEY_REPEAT, target);
    return result == android::os::InputEventInjectionResult::SUCCEEDED;
}
} // namespace android
