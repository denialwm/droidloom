//! Headless WSI (`VK_EXT_headless_surface`), answered in the client.
//!
//! Host drivers need not offer headless surfaces (NVIDIA does not), and
//! nothing is shown, so the host never sees a surface: swapchain images are
//! plain forwarded images, acquire is an empty submit that signals the
//! application's semaphore and fence, present an empty submit that waits.
//! Android presents through its own platform WSI, not through this.

#![allow(non_snake_case)]

use droidloom_gpu_protocol::generated::vk::*;

use crate::generated::{vkBindImageMemory, vkGetImageMemoryRequirements, vkGetPhysicalDeviceMemoryProperties};
use crate::manual::{vkAllocateMemory, vkCreateImage, vkDestroyImage, vkFreeMemory, vkGetDeviceQueue, vkQueueSubmit2};

/// Instance extensions the client answers alone; never sent to the host.
/// Android's loader provides its own surfaces.
#[cfg(not(target_os = "android"))]
pub const INSTANCE_EXTENSIONS: [(&core::ffi::CStr, u32); 2] = [(c"VK_KHR_surface", 25), (c"VK_EXT_headless_surface", 1)];
#[cfg(target_os = "android")]
pub const INSTANCE_EXTENSIONS: [(&core::ffi::CStr, u32); 0] = [];

const VK_FORMAT_R8G8B8A8_UNORM: VkFormat = 37;
const VK_FORMAT_R8G8B8A8_SRGB: VkFormat = 43;
const VK_FORMAT_B8G8R8A8_UNORM: VkFormat = 44;
const VK_FORMAT_B8G8R8A8_SRGB: VkFormat = 50;
const VK_PRESENT_MODE_IMMEDIATE_KHR: VkPresentModeKHR = 0;
const VK_PRESENT_MODE_MAILBOX_KHR: VkPresentModeKHR = 1;
const VK_PRESENT_MODE_FIFO_KHR: VkPresentModeKHR = 2;
const VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT: u64 = 0x10000;
const VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT: u32 = 1;

struct Swapchain {
    device: VkDevice,
    /// Acquire signals through this queue: family 0, index 0, which every
    /// headless benchmark creates.
    queue: VkQueue,
    images: Vec<VkImage>,
    memory: Vec<VkDeviceMemory>,
    next: u32,
}

/// Count-and-array answer, as every enumeration in Vulkan.
unsafe fn answer<T: Copy>(values: &[T], count: *mut u32, out: *mut T) -> VkResult {
    unsafe {
        if out.is_null() {
            *count = values.len() as u32;
            return VK_SUCCESS;
        }
        let n = (*count as usize).min(values.len());
        core::ptr::copy_nonoverlapping(values.as_ptr(), out, n);
        *count = n as u32;
        if n < values.len() { VK_INCOMPLETE } else { VK_SUCCESS }
    }
}

/// The surface carries nothing; any non-null handle will do.
pub unsafe extern "system" fn vkCreateHeadlessSurfaceEXT(
    _instance: VkInstance,
    _info: *const VkHeadlessSurfaceCreateInfoEXT,
    _allocator: *const VkAllocationCallbacks,
    pSurface: *mut VkSurfaceKHR,
) -> VkResult {
    unsafe { *pSurface = 1 };
    VK_SUCCESS
}

pub unsafe extern "system" fn vkDestroySurfaceKHR(_instance: VkInstance, _surface: VkSurfaceKHR, _allocator: *const VkAllocationCallbacks) {}

pub unsafe extern "system" fn vkGetPhysicalDeviceSurfaceSupportKHR(
    _physical: VkPhysicalDevice,
    _family: u32,
    _surface: VkSurfaceKHR,
    pSupported: *mut VkBool32,
) -> VkResult {
    unsafe { *pSupported = 1 };
    VK_SUCCESS
}

pub unsafe extern "system" fn vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
    _physical: VkPhysicalDevice,
    _surface: VkSurfaceKHR,
    pCapabilities: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
    unsafe {
        *pCapabilities = VkSurfaceCapabilitiesKHR {
            minImageCount: 2,
            maxImageCount: 8,
            // A headless surface has no size: the swapchain decides.
            currentExtent: VkExtent2D { width: u32::MAX, height: u32::MAX },
            minImageExtent: VkExtent2D { width: 1, height: 1 },
            maxImageExtent: VkExtent2D { width: 16384, height: 16384 },
            maxImageArrayLayers: 1,
            supportedTransforms: 1,
            currentTransform: 1,
            supportedCompositeAlpha: 1,
            // Color attachment, sampled, storage, transfer source and destination.
            supportedUsageFlags: 0x1f,
        };
    }
    VK_SUCCESS
}

pub unsafe extern "system" fn vkGetPhysicalDeviceSurfaceFormatsKHR(
    _physical: VkPhysicalDevice,
    _surface: VkSurfaceKHR,
    pCount: *mut u32,
    pFormats: *mut VkSurfaceFormatKHR,
) -> VkResult {
    let formats = [VK_FORMAT_B8G8R8A8_SRGB, VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_R8G8B8A8_SRGB, VK_FORMAT_R8G8B8A8_UNORM]
        .map(|format| VkSurfaceFormatKHR { format, colorSpace: 0 });
    unsafe { answer(&formats, pCount, pFormats) }
}

pub unsafe extern "system" fn vkGetPhysicalDeviceSurfacePresentModesKHR(
    _physical: VkPhysicalDevice,
    _surface: VkSurfaceKHR,
    pCount: *mut u32,
    pModes: *mut VkPresentModeKHR,
) -> VkResult {
    unsafe { answer(&[VK_PRESENT_MODE_FIFO_KHR, VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_IMMEDIATE_KHR], pCount, pModes) }
}

pub unsafe extern "system" fn vkCreateSwapchainKHR(
    device: VkDevice,
    pCreateInfo: *const VkSwapchainCreateInfoKHR,
    _allocator: *const VkAllocationCallbacks,
    pSwapchain: *mut VkSwapchainKHR,
) -> VkResult {
    unsafe {
        let info = &*pCreateInfo;
        let mut queue: VkQueue = 0;
        vkGetDeviceQueue(device, 0, 0, &raw mut queue);
        let mut swapchain = Box::new(Swapchain { device, queue, images: Vec::new(), memory: Vec::new(), next: 0 });
        // Nothing leaves the GPU, so images live in device memory; the lowest
        // allowed type on NVIDIA is system memory.
        let mut properties = core::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
        vkGetPhysicalDeviceMemoryProperties(*crate::rt::PHYSICAL.at(crate::rt::id(device as usize)) as _, &raw mut properties);
        let local = (0..properties.memoryTypeCount)
            .filter(|&i| properties.memoryTypes[i as usize].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0)
            .fold(0u32, |bits, i| bits | 1 << i);
        let image_info = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: core::ptr::null(),
            flags: 0,
            imageType: 1,
            format: info.imageFormat,
            extent: VkExtent3D { width: info.imageExtent.width, height: info.imageExtent.height, depth: 1 },
            mipLevels: 1,
            arrayLayers: info.imageArrayLayers,
            samples: 1,
            tiling: 0,
            usage: info.imageUsage,
            sharingMode: 0,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: core::ptr::null(),
            initialLayout: 0,
        };
        for _ in 0..info.minImageCount.max(2) {
            let mut image: VkImage = 0;
            let result = vkCreateImage(device, &raw const image_info, core::ptr::null(), &raw mut image);
            if result != VK_SUCCESS {
                destroy(&swapchain);
                return result;
            }
            swapchain.images.push(image);
            let mut requirements = core::mem::zeroed::<VkMemoryRequirements>();
            vkGetImageMemoryRequirements(device, image, &raw mut requirements);
            let allocate = VkMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                pNext: core::ptr::null(),
                allocationSize: requirements.size,
                memoryTypeIndex: (requirements.memoryTypeBits & local).trailing_zeros(),
            };
            let mut memory: VkDeviceMemory = 0;
            let result = vkAllocateMemory(device, &raw const allocate, core::ptr::null(), &raw mut memory);
            if result != VK_SUCCESS {
                destroy(&swapchain);
                return result;
            }
            swapchain.memory.push(memory);
            let result = vkBindImageMemory(device, image, memory, 0);
            if result != VK_SUCCESS {
                destroy(&swapchain);
                return result;
            }
        }
        *pSwapchain = Box::into_raw(swapchain) as VkSwapchainKHR;
    }
    VK_SUCCESS
}

unsafe fn destroy(swapchain: &Swapchain) {
    for &image in &swapchain.images {
        unsafe { vkDestroyImage(swapchain.device, image, core::ptr::null()) };
    }
    for &memory in &swapchain.memory {
        unsafe { vkFreeMemory(swapchain.device, memory, core::ptr::null()) };
    }
}

pub unsafe extern "system" fn vkDestroySwapchainKHR(_device: VkDevice, swapchain: VkSwapchainKHR, _allocator: *const VkAllocationCallbacks) {
    if swapchain == 0 {
        return;
    }
    let swapchain = unsafe { Box::from_raw(swapchain as *mut Swapchain) };
    unsafe { destroy(&swapchain) };
}

pub unsafe extern "system" fn vkGetSwapchainImagesKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    pCount: *mut u32,
    pImages: *mut VkImage,
) -> VkResult {
    unsafe { answer(&(*(swapchain as *const Swapchain)).images, pCount, pImages) }
}

/// Images rotate in order and nothing else holds them, so the fence is
/// signaled at once; only the semaphore goes through the GPU, where the
/// application's submit waits for it without the CPU.
pub unsafe extern "system" fn vkAcquireNextImageKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    _timeout: u64,
    semaphore: VkSemaphore,
    fence: VkFence,
    pIndex: *mut u32,
) -> VkResult {
    unsafe {
        let swapchain = &mut *(swapchain as *mut Swapchain);
        *pIndex = swapchain.next;
        swapchain.next = (swapchain.next + 1) % swapchain.images.len() as u32;
        if fence != 0 {
            crate::rt::fence(fence).point.store(0, core::sync::atomic::Ordering::Release);
        }
        if semaphore == 0 {
            return VK_SUCCESS;
        }
        let signal = semaphore_info(semaphore);
        let submit = VkSubmitInfo2 {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
            pNext: core::ptr::null(),
            flags: 0,
            waitSemaphoreInfoCount: 0,
            pWaitSemaphoreInfos: core::ptr::null(),
            commandBufferInfoCount: 0,
            pCommandBufferInfos: core::ptr::null(),
            signalSemaphoreInfoCount: 1,
            pSignalSemaphoreInfos: &raw const signal,
        };
        vkQueueSubmit2(swapchain.queue, 1, &raw const submit, 0)
    }
}

/// Nothing is shown; the wait semaphores are consumed so they can be reused.
pub unsafe extern "system" fn vkQueuePresentKHR(queue: VkQueue, pPresentInfo: *const VkPresentInfoKHR) -> VkResult {
    unsafe {
        let info = &*pPresentInfo;
        let waits: Vec<VkSemaphoreSubmitInfo> =
            (0..info.waitSemaphoreCount as usize).map(|i| semaphore_info(*info.pWaitSemaphores.add(i))).collect();
        if !waits.is_empty() {
            let submit = VkSubmitInfo2 {
                sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
                pNext: core::ptr::null(),
                flags: 0,
                waitSemaphoreInfoCount: waits.len() as u32,
                pWaitSemaphoreInfos: waits.as_ptr(),
                commandBufferInfoCount: 0,
                pCommandBufferInfos: core::ptr::null(),
                signalSemaphoreInfoCount: 0,
                pSignalSemaphoreInfos: core::ptr::null(),
            };
            let result = vkQueueSubmit2(queue, 1, &raw const submit, 0);
            if result != VK_SUCCESS {
                return result;
            }
        }
        if !info.pResults.is_null() {
            for i in 0..info.swapchainCount as usize {
                *info.pResults.add(i) = VK_SUCCESS;
            }
        }
    }
    VK_SUCCESS
}

fn semaphore_info(semaphore: VkSemaphore) -> VkSemaphoreSubmitInfo {
    VkSemaphoreSubmitInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
        pNext: core::ptr::null(),
        semaphore,
        value: 0,
        stageMask: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
        deviceIndex: 0,
    }
}
