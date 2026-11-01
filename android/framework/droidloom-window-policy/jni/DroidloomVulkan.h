// Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#ifndef VK_NO_PROTOTYPES
#define VK_NO_PROTOTYPES
#endif
#include <vulkan/vulkan.h>

namespace droidloom {

struct VulkanCapabilities {
    // The best device with a hardware level; 0 and -1 when none has one.
    uint32_t version = 0;
    int32_t level = -1;
    // HWUI's Vulkan pipeline can run: the first physical device has Vulkan
    // 1.1 with Android interop and two queues in its first graphics family.
    bool hwui = false;
};

// Query the Android loader, not a host ICD or a product-name allowlist.
VulkanCapabilities detectVulkan(PFN_vkGetInstanceProcAddr getProc);

} // namespace droidloom
