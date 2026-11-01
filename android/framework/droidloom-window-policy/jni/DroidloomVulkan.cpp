// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
#include "DroidloomVulkan.h"

#include <algorithm>
#include <cstring>
#include <vector>

namespace droidloom {
namespace {

template <typename T>
T function(PFN_vkGetInstanceProcAddr getProc, VkInstance instance, const char* name) {
    return reinterpret_cast<T>(getProc(instance, name));
}

struct Instance {
    VkInstance handle;
    PFN_vkDestroyInstance destroy;
    ~Instance() { destroy(handle, nullptr); }
};

int32_t hardwareLevel(const VkPhysicalDeviceFeatures& f) {
    // PackageManager.FEATURE_VULKAN_HARDWARE_LEVEL: level 0 requires ETC2;
    // level 1 additionally requires this complete set of optional features.
    if (!f.textureCompressionETC2) return -1;
    return f.fullDrawIndexUint32 && f.imageCubeArray && f.independentBlend &&
            f.geometryShader && f.tessellationShader && f.sampleRateShading &&
            f.textureCompressionASTC_LDR && f.fragmentStoresAndAtomics &&
            f.shaderImageGatherExtended && f.shaderUniformBufferArrayDynamicIndexing &&
            f.shaderSampledImageArrayDynamicIndexing ? 1 : 0;
}

bool androidInterop(PFN_vkGetInstanceProcAddr getProc, VkInstance instance,
        VkPhysicalDevice device, PFN_vkEnumerateDeviceExtensionProperties enumerateExtensions) {
    auto getFeatures = function<PFN_vkGetPhysicalDeviceFeatures2>(
            getProc, instance, "vkGetPhysicalDeviceFeatures2");
    auto getSemaphore = function<PFN_vkGetPhysicalDeviceExternalSemaphoreProperties>(
            getProc, instance, "vkGetPhysicalDeviceExternalSemaphoreProperties");
    auto getFence = function<PFN_vkGetPhysicalDeviceExternalFenceProperties>(
            getProc, instance, "vkGetPhysicalDeviceExternalFenceProperties");
    if (!getFeatures || !getSemaphore || !getFence || !enumerateExtensions) return false;

    uint32_t count = 0;
    if (enumerateExtensions(device, nullptr, &count, nullptr) != VK_SUCCESS || count > 1024) {
        return false;
    }
    std::vector<VkExtensionProperties> extensions(count);
    if (count == 0 || enumerateExtensions(device, nullptr, &count, extensions.data()) != VK_SUCCESS) {
        return false;
    }
    bool hardwareBuffer = false;
    bool semaphoreFd = false;
    bool fenceFd = false;
    for (uint32_t i = 0; i < count; ++i) {
        hardwareBuffer |= std::strcmp(extensions[i].extensionName,
                "VK_ANDROID_external_memory_android_hardware_buffer") == 0;
        semaphoreFd |= std::strcmp(extensions[i].extensionName,
                "VK_KHR_external_semaphore_fd") == 0;
        fenceFd |= std::strcmp(extensions[i].extensionName,
                "VK_KHR_external_fence_fd") == 0;
    }
    if (!hardwareBuffer || !semaphoreFd || !fenceFd) return false;

    VkPhysicalDeviceSamplerYcbcrConversionFeatures ycbcr{};
    ycbcr.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES;
    VkPhysicalDeviceFeatures2 features{};
    features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
    features.pNext = &ycbcr;
    getFeatures(device, &features);

    VkPhysicalDeviceExternalSemaphoreInfo semaphoreInfo{};
    semaphoreInfo.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO;
    semaphoreInfo.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT;
    VkExternalSemaphoreProperties semaphore{};
    semaphore.sType = VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES;
    getSemaphore(device, &semaphoreInfo, &semaphore);

    VkPhysicalDeviceExternalFenceInfo fenceInfo{};
    fenceInfo.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_FENCE_INFO;
    fenceInfo.handleType = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT;
    VkExternalFenceProperties fence{};
    fence.sType = VK_STRUCTURE_TYPE_EXTERNAL_FENCE_PROPERTIES;
    getFence(device, &fenceInfo, &fence);

    constexpr auto semaphoreFlags = VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT |
            VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT;
    constexpr auto fenceFlags = VK_EXTERNAL_FENCE_FEATURE_IMPORTABLE_BIT |
            VK_EXTERNAL_FENCE_FEATURE_EXPORTABLE_BIT;
    return ycbcr.samplerYcbcrConversion &&
            (semaphore.externalSemaphoreFeatures & semaphoreFlags) == semaphoreFlags &&
            (semaphore.compatibleHandleTypes & VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT) &&
            (fence.externalFenceFeatures & fenceFlags) == fenceFlags &&
            (fence.compatibleHandleTypes & VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT);
}

} // namespace

VulkanCapabilities detectVulkan(PFN_vkGetInstanceProcAddr getProc) {
    if (!getProc) return {};
    auto create = function<PFN_vkCreateInstance>(getProc, VK_NULL_HANDLE, "vkCreateInstance");
    auto enumerateVersion = function<PFN_vkEnumerateInstanceVersion>(
            getProc, VK_NULL_HANDLE, "vkEnumerateInstanceVersion");
    uint32_t loaderVersion = VK_API_VERSION_1_0;
    if (!create || (enumerateVersion && enumerateVersion(&loaderVersion) != VK_SUCCESS) ||
            VK_API_VERSION_VARIANT(loaderVersion) != 0 || loaderVersion < VK_API_VERSION_1_0) {
        return {};
    }
    VkApplicationInfo application{};
    application.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    application.pApplicationName = "Droidloom Vulkan capabilities";
    // Only 1.1 instance queries are needed. A newer loader can still be paired
    // with a 1.0-only driver, so retry that case at 1.0 before giving up.
    application.apiVersion = std::min(loaderVersion, VK_API_VERSION_1_1);
    VkInstanceCreateInfo info{};
    info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    info.pApplicationInfo = &application;
    VkInstance handle = VK_NULL_HANDLE;
    VkResult created = create(&info, nullptr, &handle);
    if (created == VK_ERROR_INCOMPATIBLE_DRIVER && application.apiVersion > VK_API_VERSION_1_0) {
        application.apiVersion = VK_API_VERSION_1_0;
        created = create(&info, nullptr, &handle);
    }
    if (created != VK_SUCCESS || handle == VK_NULL_HANDLE) return {};
    auto destroy = function<PFN_vkDestroyInstance>(getProc, handle, "vkDestroyInstance");
    if (!destroy) return {};
    const Instance instance{handle, destroy};
    auto enumerate = function<PFN_vkEnumeratePhysicalDevices>(
            getProc, handle, "vkEnumeratePhysicalDevices");
    auto properties = function<PFN_vkGetPhysicalDeviceProperties>(
            getProc, handle, "vkGetPhysicalDeviceProperties");
    auto features = function<PFN_vkGetPhysicalDeviceFeatures>(
            getProc, handle, "vkGetPhysicalDeviceFeatures");
    auto queues = function<PFN_vkGetPhysicalDeviceQueueFamilyProperties>(
            getProc, handle, "vkGetPhysicalDeviceQueueFamilyProperties");
    auto extensions = function<PFN_vkEnumerateDeviceExtensionProperties>(
            getProc, handle, "vkEnumerateDeviceExtensionProperties");
    if (!enumerate || !properties || !features || !queues) return {};
    uint32_t count = 0;
    if (enumerate(handle, &count, nullptr) != VK_SUCCESS || count == 0 || count > 256) return {};
    std::vector<VkPhysicalDevice> devices(count);
    if (enumerate(handle, &count, devices.data()) != VK_SUCCESS) return {};

    VulkanCapabilities result;
    for (uint32_t i = 0; i < count; ++i) {
        VkPhysicalDeviceProperties p{};
        properties(devices[i], &p);
        if ((p.deviceType != VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU &&
                p.deviceType != VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU &&
                p.deviceType != VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU) ||
                VK_API_VERSION_VARIANT(p.apiVersion) != 0 || p.apiVersion < VK_API_VERSION_1_0) {
            continue;
        }
        uint32_t queueCount = 0;
        queues(devices[i], &queueCount, nullptr);
        if (queueCount == 0 || queueCount > 256) continue;
        std::vector<VkQueueFamilyProperties> families(queueCount);
        queues(devices[i], &queueCount, families.data());
        // HWUI renders and uploads on two queues of the first graphics family.
        const auto graphics = std::find_if(families.begin(), families.end(), [](const auto& f) {
            return f.queueCount > 0 && (f.queueFlags & VK_QUEUE_GRAPHICS_BIT);
        });
        if (graphics == families.end()) continue;
        VkPhysicalDeviceFeatures f{};
        features(devices[i], &f);
        const int32_t level = hardwareLevel(f);
        uint32_t version = std::min(loaderVersion, p.apiVersion);
        // Android's version feature also promises AHB, YCbCr and SYNC_FD
        // interop at 1.1+. An API version alone does not establish that.
        if (version >= VK_API_VERSION_1_1 && (application.apiVersion < VK_API_VERSION_1_1 ||
                !androidInterop(getProc, handle, devices[i], extensions))) {
            version = VK_MAKE_API_VERSION(0, 1, 0, 3);
        }
        // HWUI always takes the first physical device. It needs no texture
        // compression, so a device without a hardware level still runs it.
        if (i == 0) result.hwui = version >= VK_API_VERSION_1_1 && graphics->queueCount >= 2;
        if (level < 0) continue;
        // Version must come from a device that supports the advertised level.
        if (level > result.level || (level == result.level && version > result.version)) {
            result.version = version;
            result.level = level;
        }
    }
    return result;
}

} // namespace droidloom
