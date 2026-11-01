// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
// Exercise the production probe with a fake loader; no GPU or Android required.
#include "DroidloomVulkan.h"

#include <cassert>
#include <cstring>
#include <iostream>
#include <string>
#include <vector>

namespace {
struct Device {
    VkPhysicalDeviceProperties properties{};
    VkPhysicalDeviceFeatures features{};
    bool graphics = true;
    uint32_t graphicsQueues = 2;
    bool hardwareBuffer = true;
    bool semaphoreFd = true;
    bool fenceFd = true;
    bool ycbcr = true;
    bool semaphoreImport = true;
    bool semaphoreExport = true;
    bool fenceImport = true;
    bool fenceExport = true;
};
std::vector<Device> devices;
uint32_t loaderVersion;
uint32_t maxInstanceVersion;
VkResult createResult;
VkResult enumerateResult;
std::string missingFunction;
int created;
int destroyed;
int createAttempts;

Device& device(VkPhysicalDevice handle) {
    return devices.at(reinterpret_cast<uintptr_t>(handle) - 1);
}
Device gpu(uint32_t version = VK_API_VERSION_1_4, int level = 1) {
    Device result;
    result.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;
    result.properties.apiVersion = version;
    auto& f = result.features;
    f.textureCompressionETC2 = VK_TRUE;
    f.fullDrawIndexUint32 = f.imageCubeArray = f.independentBlend = VK_TRUE;
    f.geometryShader = level == 1;
    f.tessellationShader = f.sampleRateShading = f.textureCompressionASTC_LDR = VK_TRUE;
    f.fragmentStoresAndAtomics = f.shaderImageGatherExtended = VK_TRUE;
    f.shaderUniformBufferArrayDynamicIndexing = f.shaderSampledImageArrayDynamicIndexing = VK_TRUE;
    return result;
}
void reset(std::vector<Device> list = {gpu()}) {
    devices = std::move(list);
    loaderVersion = VK_API_VERSION_1_4;
    maxInstanceVersion = VK_API_VERSION_1_4;
    createResult = enumerateResult = VK_SUCCESS;
    missingFunction.clear();
    created = destroyed = createAttempts = 0;
}
VKAPI_ATTR VkResult VKAPI_CALL version(uint32_t* result) {
    *result = loaderVersion;
    return VK_SUCCESS;
}
VKAPI_ATTR VkResult VKAPI_CALL create(const VkInstanceCreateInfo* info,
        const VkAllocationCallbacks*, VkInstance* result) {
    ++createAttempts;
    assert(info->pApplicationInfo->apiVersion <= loaderVersion);
    if (info->pApplicationInfo->apiVersion > maxInstanceVersion) return VK_ERROR_INCOMPATIBLE_DRIVER;
    if (createResult != VK_SUCCESS) return createResult;
    ++created;
    *result = reinterpret_cast<VkInstance>(1);
    return VK_SUCCESS;
}
VKAPI_ATTR void VKAPI_CALL destroy(VkInstance, const VkAllocationCallbacks*) { ++destroyed; }
VKAPI_ATTR VkResult VKAPI_CALL enumerate(VkInstance, uint32_t* count, VkPhysicalDevice* result) {
    if (!result) {
        *count = devices.size();
        return VK_SUCCESS;
    }
    if (enumerateResult != VK_SUCCESS) return enumerateResult;
    assert(*count >= devices.size());
    *count = devices.size();
    for (uint32_t i = 0; i < *count; ++i) result[i] = reinterpret_cast<VkPhysicalDevice>(i + 1);
    return VK_SUCCESS;
}
VKAPI_ATTR void VKAPI_CALL properties(VkPhysicalDevice handle, VkPhysicalDeviceProperties* result) {
    *result = device(handle).properties;
}
VKAPI_ATTR void VKAPI_CALL features(VkPhysicalDevice handle, VkPhysicalDeviceFeatures* result) {
    *result = device(handle).features;
}
VKAPI_ATTR void VKAPI_CALL features2(VkPhysicalDevice handle, VkPhysicalDeviceFeatures2* result) {
    auto* ycbcr = static_cast<VkPhysicalDeviceSamplerYcbcrConversionFeatures*>(result->pNext);
    assert(ycbcr->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES);
    ycbcr->samplerYcbcrConversion = device(handle).ycbcr;
}
VKAPI_ATTR void VKAPI_CALL queues(VkPhysicalDevice handle, uint32_t* count,
        VkQueueFamilyProperties* result) {
    *count = 1;
    if (result) {
        result[0] = {};
        result[0].queueCount = device(handle).graphicsQueues;
        result[0].queueFlags = device(handle).graphics ? VK_QUEUE_GRAPHICS_BIT : VK_QUEUE_COMPUTE_BIT;
    }
}
VKAPI_ATTR VkResult VKAPI_CALL extensions(VkPhysicalDevice handle, const char*, uint32_t* count,
        VkExtensionProperties* result) {
    const auto& d = device(handle);
    *count = 0;
    for (const auto& extension : std::vector<std::pair<bool, const char*>>{
            {d.hardwareBuffer, "VK_ANDROID_external_memory_android_hardware_buffer"},
            {d.semaphoreFd, "VK_KHR_external_semaphore_fd"},
            {d.fenceFd, "VK_KHR_external_fence_fd"}}) {
        if (!extension.first) continue;
        if (result) std::strcpy(result[*count].extensionName, extension.second);
        ++*count;
    }
    return VK_SUCCESS;
}
VKAPI_ATTR void VKAPI_CALL semaphore(VkPhysicalDevice handle,
        const VkPhysicalDeviceExternalSemaphoreInfo* info, VkExternalSemaphoreProperties* result) {
    assert(info->handleType == VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT);
    const auto& d = device(handle);
    result->externalSemaphoreFeatures =
            (d.semaphoreImport ? VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT : 0) |
            (d.semaphoreExport ? VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT : 0);
    result->compatibleHandleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT;
}
VKAPI_ATTR void VKAPI_CALL fence(VkPhysicalDevice handle,
        const VkPhysicalDeviceExternalFenceInfo* info, VkExternalFenceProperties* result) {
    assert(info->handleType == VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT);
    const auto& d = device(handle);
    result->externalFenceFeatures = (d.fenceImport ? VK_EXTERNAL_FENCE_FEATURE_IMPORTABLE_BIT : 0) |
            (d.fenceExport ? VK_EXTERNAL_FENCE_FEATURE_EXPORTABLE_BIT : 0);
    result->compatibleHandleTypes = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT;
}
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL getProc(VkInstance, const char* name) {
    if (name == missingFunction) return nullptr;
#define FUNCTION(vkName, callback) \
    if (std::strcmp(name, vkName) == 0) return reinterpret_cast<PFN_vkVoidFunction>(callback)
    FUNCTION("vkEnumerateInstanceVersion", version);
    FUNCTION("vkCreateInstance", create);
    FUNCTION("vkDestroyInstance", destroy);
    FUNCTION("vkEnumeratePhysicalDevices", enumerate);
    FUNCTION("vkGetPhysicalDeviceProperties", properties);
    FUNCTION("vkGetPhysicalDeviceFeatures", features);
    FUNCTION("vkGetPhysicalDeviceFeatures2", features2);
    FUNCTION("vkGetPhysicalDeviceQueueFamilyProperties", queues);
    FUNCTION("vkEnumerateDeviceExtensionProperties", extensions);
    FUNCTION("vkGetPhysicalDeviceExternalSemaphoreProperties", semaphore);
    FUNCTION("vkGetPhysicalDeviceExternalFenceProperties", fence);
#undef FUNCTION
    return nullptr;
}
droidloom::VulkanCapabilities probe() {
    const auto result = droidloom::detectVulkan(getProc);
    assert(created == destroyed);
    return result;
}
} // namespace

int main() {
    reset();
    auto result = probe();
    assert(result.version == VK_API_VERSION_1_4 && result.level == 1 && result.hwui);
    reset({gpu(VK_API_VERSION_1_3, 0)});
    result = probe();
    assert(result.version == VK_API_VERSION_1_3 && result.level == 0);

    // A high-version CPU must never influence hardware capabilities.
    auto cpu = gpu();
    cpu.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_CPU;
    reset({cpu, gpu(VK_API_VERSION_1_1)});
    assert(probe().version == VK_API_VERSION_1_1);
    reset({cpu});
    assert(probe().version == 0);
    reset({});
    assert(probe().version == 0);
    reset(); devices[0].graphics = false;
    assert(probe().version == 0);
    // Without ETC2 there is no hardware level, but HWUI still runs.
    reset(); devices[0].features.textureCompressionETC2 = VK_FALSE;
    result = probe();
    assert(result.version == 0 && result.level == -1 && result.hwui);
    reset({gpu(VK_API_VERSION_1_1, 0), gpu()}); devices[0].features.textureCompressionETC2 = VK_FALSE;
    result = probe();
    assert(result.version == VK_API_VERSION_1_4 && result.level == 1 && result.hwui);

    // Choose a coherent level/version pair, independent of enumeration order.
    for (bool reversed : {false, true}) {
        auto highVersion = gpu(VK_API_VERSION_1_4, 0);
        auto highLevel = gpu(VK_API_VERSION_1_1, 1);
        reset(reversed ? std::vector<Device>{highLevel, highVersion}
                       : std::vector<Device>{highVersion, highLevel});
        result = probe();
        assert(result.level == 1 && result.version == VK_API_VERSION_1_1);
    }

    // API 1.1+ does not imply Android buffer/synchronization interop.
    for (auto flag : {&Device::hardwareBuffer, &Device::semaphoreFd, &Device::fenceFd,
            &Device::ycbcr, &Device::semaphoreImport,
            &Device::semaphoreExport, &Device::fenceImport, &Device::fenceExport}) {
        reset(); devices[0].*flag = false;
        result = probe();
        assert(result.version == VK_MAKE_API_VERSION(0, 1, 0, 3) && !result.hwui);
    }

    // HWUI takes the first device and needs two queues in its graphics family.
    reset(); devices[0].graphicsQueues = 1;
    result = probe();
    assert(result.version == VK_API_VERSION_1_4 && !result.hwui);
    reset({gpu(VK_API_VERSION_1_0), gpu()});
    result = probe();
    assert(result.version == VK_API_VERSION_1_4 && !result.hwui);
    reset({cpu, gpu()});
    assert(!probe().hwui);
    reset(); missingFunction = "vkGetPhysicalDeviceFeatures2";
    assert(probe().version == VK_MAKE_API_VERSION(0, 1, 0, 3));
    reset(); loaderVersion = VK_API_VERSION_1_1;
    assert(probe().version == VK_API_VERSION_1_1);
    reset(); loaderVersion = VK_API_VERSION_1_0; missingFunction = "vkEnumerateInstanceVersion";
    assert(probe().version == VK_API_VERSION_1_0);
    reset({gpu(VK_MAKE_API_VERSION(0, 1, 0, 3))}); maxInstanceVersion = VK_API_VERSION_1_0;
    assert(probe().version == VK_MAKE_API_VERSION(0, 1, 0, 3) && createAttempts == 2);

    reset(); createResult = VK_ERROR_INCOMPATIBLE_DRIVER;
    assert(probe().version == 0);
    reset(); enumerateResult = VK_INCOMPLETE;
    assert(probe().version == 0);
    reset(); enumerateResult = VK_ERROR_INITIALIZATION_FAILED;
    assert(probe().version == 0);
    reset(); missingFunction = "vkGetPhysicalDeviceProperties";
    assert(probe().version == 0);
    assert(droidloom::detectVulkan(nullptr).version == 0);
    std::cout << "PASS: hardware-only Vulkan detection, levels, coherent versions, Android interop, HWUI and failures\n";
}
