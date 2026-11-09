//! Records with host-side state: transport records, the instance, physical
//! device filtering, device loading, queue timelines and fenced submits.

#![allow(non_snake_case)]

use core::ffi::{c_int, c_void, CStr};
use std::os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd};

use droidloom_gpu_protocol::generated::frame;
use droidloom_gpu_protocol::generated::vk::*;
use droidloom_gpu_protocol::layout::Record;
use droidloom_gpu_protocol::sys::Shm;

use crate::generated::{
    patch_VkComputePipelineCreateInfo, patch_VkDeviceCreateInfo, patch_VkFenceCreateInfo, patch_VkGraphicsPipelineCreateInfo, patch_VkImageCreateInfo, patch_VkInstanceCreateInfo, patch_VkMemoryAllocateInfo, patch_VkSemaphoreCreateInfo, patch_VkSubmitInfo2,
    TABLE,
};
use crate::probe::Shareable;
use crate::rt::{self, Server, Timeline, SLOTS};

const VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT: u32 = 0x8;
const VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT: u32 = 0x10;
const VK_SEMAPHORE_TYPE_TIMELINE: i32 = 1;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT: i32 = 0x80;
const DEVICE_LOCAL: u32 = 0x1;
const HOST_VISIBLE: u32 = 0x2;
const HOST_COHERENT: u32 = 0x4;
const HOST_CACHED: u32 = 0x8;
/// Row alignment of a linear buffer image. NVIDIA loses the device on a
/// linear image with shorter-aligned rows instead of refusing it.
const LINEAR_PITCH: u64 = 128;

/// Extensions the server itself relies on, enabled on every device: fence
/// export, shared mappable memory, gralloc buffer import, release fences
/// and the texture decoder's descriptors.
const DEVICE_EXTENSIONS: [&CStr; 8] = [
    c"VK_KHR_external_fence_fd",
    c"VK_EXT_external_memory_host",
    c"VK_KHR_external_memory_fd",
    c"VK_EXT_external_memory_dma_buf",
    c"VK_EXT_image_drm_format_modifier",
    c"VK_EXT_queue_family_foreign",
    c"VK_KHR_external_semaphore_fd",
    c"VK_KHR_push_descriptor",
];

/// The least instance version the server works with: it finds the GPU it
/// shows by PCI address, which a Vulkan 1.0 instance does not report.
const INSTANCE_VERSION: u32 = (1 << 22) | (1 << 12);

const NATIVE_BUFFER: &CStr = c"VK_ANDROID_native_buffer";
/// AHardwareBuffers, which the client imports as the DMA-BUFs inside them.
const HARDWARE_BUFFER: &CStr = c"VK_ANDROID_external_memory_android_hardware_buffer";
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: i32 = 0x200;

/// Device extensions Android's loader implements itself on top of
/// `VK_ANDROID_native_buffer`; the host's own would be listed twice.
const LOADER_OWNED: [&CStr; 15] = [
    c"VK_KHR_swapchain",
    c"VK_KHR_display_swapchain",
    c"VK_KHR_incremental_present",
    c"VK_KHR_shared_presentable_image",
    c"VK_KHR_present_id",
    c"VK_KHR_present_id2",
    c"VK_KHR_present_wait",
    c"VK_KHR_present_wait2",
    c"VK_KHR_swapchain_maintenance1",
    c"VK_KHR_swapchain_mutable_format",
    c"VK_EXT_swapchain_maintenance1",
    c"VK_EXT_hdr_metadata",
    c"VK_EXT_present_mode_fifo_latest_ready",
    c"VK_EXT_image_compression_control_swapchain",
    c"VK_GOOGLE_display_timing",
];

pub unsafe fn pad(_: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe { rt::next(record) }
}

pub unsafe fn stop(_: &mut Server, _record: *mut u8) -> *mut u8 {
    core::ptr::null_mut()
}

/// Replay part of a command-buffer stream: one table jump per command.
pub unsafe fn record(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let request = record.cast::<Record>();
        s.cb = s.h((*request).command_buffer) as usize;
        s.stream = match s.streams.get_mut(&s.cb) {
            Some(stream) => &raw mut **stream,
            None => rt::fail("commands for an unknown command buffer"),
        };
        let mut p = s.base.add((*request).stream as usize);
        while !p.is_null() {
            let op = (*p.cast::<droidloom_gpu_protocol::layout::Header>()).op;
            p = TABLE[op as usize & (SLOTS - 1)](s, p);
        }
        rt::next(record)
    }
}

/// The last record of a stream.
pub unsafe fn h_vkEndCommandBuffer(s: &mut Server, _record: *mut u8) -> *mut u8 {
    let result = unsafe { (s.fns.vkEndCommandBuffer)(s.cb) };
    if result != VK_SUCCESS {
        rt::failed("vkEndCommandBuffer", result);
    }
    core::ptr::null_mut()
}

pub unsafe fn h_vkCreateInstance(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateInstance>();
        let info: *mut VkInstanceCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkInstanceCreateInfo(s, record, info);
        // The host instance is the server's too: an application that asks
        // for Vulkan 1.0, or names no version, still gets its GPU.
        let own = VkApplicationInfo {
            sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: core::ptr::null(),
            pApplicationName: core::ptr::null(),
            applicationVersion: 0,
            pEngineName: core::ptr::null(),
            engineVersion: 0,
            apiVersion: INSTANCE_VERSION,
        };
        if (*info).pApplicationInfo.is_null() {
            (*info).pApplicationInfo = &raw const own;
        } else if (*(*info).pApplicationInfo).apiVersion < INSTANCE_VERSION {
            (*(*info).pApplicationInfo.cast_mut()).apiVersion = INSTANCE_VERSION;
        }
        let mut instance: VkInstance = 0;
        let result = (s.fns.vkCreateInstance)(info, core::ptr::null(), &raw mut instance);
        if result == VK_SUCCESS {
            s.instance = instance;
            let gipa = s.gipa;
            s.fns.load_instance(gipa, instance);
            s.set((*f).pInstance, instance as u64);
        }
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

/// Show Android only the GPUs it was given.
pub unsafe fn h_vkEnumeratePhysicalDevices(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkEnumeratePhysicalDevices>();
        let capacity: *mut u32 = rt::at(record, (*f).pPhysicalDeviceCount);
        let ids: *const u64 = rt::at(record, (*f).pPhysicalDevices);
        let mut count = 0u32;
        let mut result = (s.fns.vkEnumeratePhysicalDevices)(s.instance, &raw mut count, core::ptr::null_mut());
        let mut all = vec![0usize; count as usize];
        if result == VK_SUCCESS {
            result = (s.fns.vkEnumeratePhysicalDevices)(s.instance, &raw mut count, all.as_mut_ptr());
        }
        let mut shown = 0u32;
        for &physical in &all[..count as usize] {
            if crate::gpu::pci_address(s.fns.vkGetPhysicalDeviceProperties2, physical) == s.gpu && shown < *capacity {
                s.set(*ids.add(shown as usize), physical as u64);
                // A later instance enumerates new handles for the same GPUs.
                s.shown.insert(physical, shown as usize);
                shown += 1;
            }
        }
        *capacity = shown;
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

/// Only extensions whose commands are forwarded. Android sees the host's
/// swapchain support as `VK_ANDROID_native_buffer`, none of the
/// presentation extensions its loader provides, and AHardwareBuffer import.
pub unsafe fn h_vkEnumerateDeviceExtensionProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkEnumerateDeviceExtensionProperties>();
        let physical = s.h((*f).physicalDevice) as usize;
        let capacity: *mut u32 = rt::at(record, (*f).pPropertyCount);
        let out: *mut VkExtensionProperties = rt::at(record, (*f).pProperties);
        let enumerate = s.fns.vkEnumerateDeviceExtensionProperties;
        let mut count = 0u32;
        enumerate(physical, core::ptr::null(), &raw mut count, core::ptr::null_mut());
        let mut all = vec![core::mem::zeroed::<VkExtensionProperties>(); count as usize];
        enumerate(physical, core::ptr::null(), &raw mut count, all.as_mut_ptr());
        all.truncate(count as usize);
        all.retain(|extension| !crate::generated::UNFORWARDED_EXTENSIONS.contains(&CStr::from_ptr(extension.extensionName.as_ptr())));
        if s.android {
            all.retain(|extension| !LOADER_OWNED.contains(&CStr::from_ptr(extension.extensionName.as_ptr())));
            for (name, specVersion) in [(NATIVE_BUFFER, 8), (HARDWARE_BUFFER, 5)] {
                let mut extension = VkExtensionProperties { extensionName: [0; 256], specVersion };
                for (to, &from) in extension.extensionName.iter_mut().zip(name.to_bytes()) {
                    *to = from as core::ffi::c_char;
                }
                all.push(extension);
            }
        }
        let result = if out.is_null() {
            *capacity = all.len() as u32;
            VK_SUCCESS
        } else {
            let n = (*capacity as usize).min(all.len());
            core::ptr::copy_nonoverlapping(all.as_ptr(), out, n);
            *capacity = n as u32;
            if n < all.len() { VK_INCOMPLETE } else { VK_SUCCESS }
        };
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

/// Commands up to Vulkan 1.3 are forwarded: the device reports no newer
/// version, keeping the host's patch level.
fn forwarded_version(version: u32) -> u32 {
    const VK_API_VERSION_1_3: u32 = (1 << 22) | (3 << 12);
    if version >= VK_API_VERSION_1_3 { VK_API_VERSION_1_3 | (version & 0xfff) } else { version }
}

pub unsafe fn h_vkGetPhysicalDeviceProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceProperties>();
        let properties: *mut VkPhysicalDeviceProperties = rt::at(record, (*f).pProperties);
        (s.fns.vkGetPhysicalDeviceProperties)(s.h((*f).physicalDevice) as usize, properties);
        (*properties).apiVersion = forwarded_version((*properties).apiVersion);
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceProperties2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceProperties2>();
        let properties: *mut VkPhysicalDeviceProperties2 = rt::at(record, (*f).pProperties);
        crate::generated::link_chain(record, (&raw mut (*properties).pNext).cast());
        (s.fns.vkGetPhysicalDeviceProperties2)(s.h((*f).physicalDevice) as usize, properties);
        (*properties).properties.apiVersion = forwarded_version((*properties).properties.apiVersion);
        rt::next(record)
    }
}

/// Queue capabilities whose commands are not forwarded: sparse binding,
/// video coding and optical flow.
const UNFORWARDED_QUEUE_FLAGS: u32 = 0x8 | 0x20 | 0x40 | 0x100;

/// Sparse resources are not forwarded (`vkQueueBindSparse`).
fn hide_unforwarded_features(features: &mut VkPhysicalDeviceFeatures) {
    features.sparseBinding = 0;
    features.sparseResidencyBuffer = 0;
    features.sparseResidencyImage2D = 0;
    features.sparseResidencyImage3D = 0;
    features.sparseResidency2Samples = 0;
    features.sparseResidency4Samples = 0;
    features.sparseResidency8Samples = 0;
    features.sparseResidency16Samples = 0;
    features.sparseResidencyAliased = 0;
}

pub unsafe fn h_vkGetPhysicalDeviceFeatures(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceFeatures>();
        let features: *mut VkPhysicalDeviceFeatures = rt::at(record, (*f).pFeatures);
        (s.fns.vkGetPhysicalDeviceFeatures)(s.h((*f).physicalDevice) as usize, features);
        hide_unforwarded_features(&mut *features);
        crate::compressed::features(&mut *features);
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceFeatures2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceFeatures2>();
        let features: *mut VkPhysicalDeviceFeatures2 = rt::at(record, (*f).pFeatures);
        crate::generated::link_chain(record, (&raw mut (*features).pNext).cast());
        (s.fns.vkGetPhysicalDeviceFeatures2)(s.h((*f).physicalDevice) as usize, features);
        hide_unforwarded_features(&mut (*features).features);
        crate::compressed::features(&mut (*features).features);
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceQueueFamilyProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceQueueFamilyProperties>();
        let count: *mut u32 = rt::at(record, (*f).pQueueFamilyPropertyCount);
        let families: *mut VkQueueFamilyProperties = rt::at(record, (*f).pQueueFamilyProperties);
        (s.fns.vkGetPhysicalDeviceQueueFamilyProperties)(s.h((*f).physicalDevice) as usize, count, families);
        if !families.is_null() {
            for i in 0..*count as usize {
                (*families.add(i)).queueFlags &= !UNFORWARDED_QUEUE_FLAGS;
            }
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceQueueFamilyProperties2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceQueueFamilyProperties2>();
        let count: *mut u32 = rt::at(record, (*f).pQueueFamilyPropertyCount);
        let families: *mut VkQueueFamilyProperties2 = rt::at(record, (*f).pQueueFamilyProperties);
        if !families.is_null() {
            for i in 0..*count as usize {
                crate::generated::link_chain(record, (&raw mut (*families.add(i)).pNext).cast());
            }
        }
        (s.fns.vkGetPhysicalDeviceQueueFamilyProperties2)(s.h((*f).physicalDevice) as usize, count, families);
        if !families.is_null() {
            for i in 0..*count as usize {
                (*families.add(i)).queueFamilyProperties.queueFlags &= !UNFORWARDED_QUEUE_FLAGS;
            }
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCreateDevice(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateDevice>();
        let physical = s.h((*f).physicalDevice) as usize;
        let info: *mut VkDeviceCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkDeviceCreateInfo(s, record, info);
        crate::compressed::strip_features(info);
        let mut extensions = Vec::with_capacity((*info).enabledExtensionCount as usize + DEVICE_EXTENSIONS.len());
        for i in 0..(*info).enabledExtensionCount as usize {
            let name = *(*info).ppEnabledExtensionNames.add(i);
            if CStr::from_ptr(name) == HARDWARE_BUFFER {
                continue;
            }
            // Gralloc images are presented in PRESENT_SRC layout, which the
            // host knows from its swapchain extension.
            extensions.push(if CStr::from_ptr(name) == NATIVE_BUFFER { c"VK_KHR_swapchain".as_ptr() } else { name });
        }
        for wanted in DEVICE_EXTENSIONS {
            if !extensions.iter().any(|&name| CStr::from_ptr(name) == wanted) {
                extensions.push(wanted.as_ptr());
            }
        }
        (*info).enabledExtensionCount = extensions.len() as u32;
        (*info).ppEnabledExtensionNames = extensions.as_ptr();
        let mut device: VkDevice = 0;
        let result = (s.fns.vkCreateDevice)(physical, info, core::ptr::null(), &raw mut device);
        if result == VK_SUCCESS {
            let gdpa = core::mem::transmute::<*const core::ffi::c_void, PFN_vkGetDeviceProcAddr>(rt::instance_proc(s, c"vkGetDeviceProcAddr"));
            if s.devices == 0 {
                let mut memory = core::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
                (s.fns.vkGetPhysicalDeviceMemoryProperties)(physical, &raw mut memory);
                hide_unshareable(&mut memory, host_memory(s, physical));
                for i in 0..memory.memoryTypeCount as usize {
                    s.memory_types[i] = memory.memoryTypes[i].propertyFlags;
                }
            }
            crate::compressed::device_created(s, physical, device, gdpa);
            s.devices += 1;
            s.set((*f).pDevice, device as u64);
        } else {
            let names: Vec<_> = extensions.iter().map(|&name| CStr::from_ptr(name).to_string_lossy()).collect();
            let mut chain = Vec::new();
            let mut next = (*info).pNext.cast::<rt::Base>();
            while !next.is_null() {
                chain.push((*next).sType);
                next = (*next).pNext;
            }
            eprintln!("droidloom-gpu-server: vkCreateDevice failed with {result} for {}, chain {chain:?}", names.join(" "));
        }
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

pub unsafe fn h_vkGetDeviceQueue(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetDeviceQueue>();
        let fd = OwnedFd::from_raw_fd(s.take_fd());
        let handle = s.drm.import(fd.as_fd()).unwrap_or_else(|error| rt::fail(&format!("import a queue timeline: {error}")));
        let scratch = s.drm.create().unwrap_or_else(|error| rt::fail(&format!("create a syncobj: {error}")));
        let device = s.h((*f).device) as usize;
        let mut queue: VkQueue = 0;
        (s.fns.vkGetDeviceQueue)(device, (*f).queueFamilyIndex, (*f).queueIndex, &raw mut queue);
        s.set((*f).pQueue, queue as u64);
        let export = VkExportSemaphoreCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
            pNext: core::ptr::null(),
            handleTypes: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        let info = VkSemaphoreCreateInfo { sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, pNext: (&raw const export).cast(), flags: 0 };
        let mut release: VkSemaphore = 0;
        let result = (s.fns.vkCreateSemaphore)(device, &raw const info, core::ptr::null(), &raw mut release);
        if result != VK_SUCCESS {
            rt::failed("vkCreateSemaphore for release fences", result);
        }
        let export = VkExportFenceCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXPORT_FENCE_CREATE_INFO,
            pNext: core::ptr::null(),
            handleTypes: VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        let info = VkFenceCreateInfo { sType: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, pNext: (&raw const export).cast(), flags: 0 };
        let mut fence: VkFence = 0;
        let result = (s.fns.vkCreateFence)(device, &raw const info, core::ptr::null(), &raw mut fence);
        if result != VK_SUCCESS {
            rt::failed("vkCreateFence for timeline semaphores", result);
        }
        let timeline = Timeline { device, family: (*f).queueFamilyIndex, release, fence, handle, scratch, point: 0 };
        s.timelines.insert((*f).pQueue, timeline);
        rt::next(record)
    }
}

/// Fences exist on the host only to export completion into the queue's
/// timeline, which the client waits on in the kernel.
pub unsafe fn h_vkCreateFence(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateFence>();
        let info: *mut VkFenceCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkFenceCreateInfo(s, record, info);
        // Signaled-at-creation lives in the client's fence state; the host
        // fence only ever reports submissions.
        (*info).flags &= !1;
        let export = VkExportFenceCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXPORT_FENCE_CREATE_INFO,
            pNext: (*info).pNext,
            handleTypes: VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        (*info).pNext = (&raw const export).cast();
        let mut fence: VkFence = 0;
        let result = (s.fns.vkCreateFence)(s.h((*f).device) as usize, info, core::ptr::null(), &raw mut fence);
        if result != VK_SUCCESS {
            rt::failed("vkCreateFence", result);
        }
        s.set((*f).pFence, fence);
        rt::next(record)
    }
}

/// A fenced submission signals the next point of its queue's timeline, and
/// every timeline semaphore it signals gets the value on its own kernel
/// timeline, both through one sync file of the submission's completion.
pub unsafe fn h_vkQueueSubmit2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkQueueSubmit2>();
        let queue = s.h((*f).queue) as usize;
        let submits: *mut VkSubmitInfo2 = rt::at(record, (*f).pSubmits);
        let mut signals = Vec::new();
        for i in 0..(*f).submitCount as usize {
            patch_VkSubmitInfo2(s, record, submits.add(i));
            if !s.semaphore_timelines.is_empty() {
                let submit = &*submits.add(i);
                for j in 0..submit.signalSemaphoreInfoCount as usize {
                    let signal = &*submit.pSignalSemaphoreInfos.add(j);
                    if let Some(&timeline) = s.semaphore_timelines.get(&signal.semaphore) {
                        signals.push((timeline, signal.value));
                    }
                }
            }
        }
        let fenced = s.h((*f).fence);
        let timeline = s.timelines.get_mut(&(*f).queue).unwrap_or_else(|| rt::fail("submit to an unknown queue"));
        let fence = if fenced == 0 && !signals.is_empty() { timeline.fence } else { fenced };
        if fenced != 0 {
            timeline.point += 1;
            signals.push((timeline.handle, timeline.point));
        }
        let (device, scratch) = (timeline.device, timeline.scratch);
        let result = (s.fns.vkQueueSubmit2)(queue, (*f).submitCount, submits, fence);
        if result != VK_SUCCESS {
            rt::failed("vkQueueSubmit2", result);
        }
        if fence == 0 {
            return rt::next(record);
        }
        let info = VkFenceGetFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_FENCE_GET_FD_INFO_KHR,
            pNext: core::ptr::null(),
            fence,
            handleType: VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT as i32,
        };
        let mut fd: c_int = -1;
        let result = (s.fns.vkGetFenceFdKHR)(device, &raw const info, &raw mut fd);
        if result != VK_SUCCESS {
            rt::failed("vkGetFenceFdKHR", result);
        }
        // -1: already complete, nothing to wait for.
        let sync_file = (fd >= 0).then(|| OwnedFd::from_raw_fd(fd));
        for (handle, point) in signals {
            let done = match &sync_file {
                Some(sync_file) => s.drm.attach(handle, point, scratch, sync_file.as_fd()),
                None => s.drm.signal(handle, point),
            };
            done.unwrap_or_else(|error| rt::fail(&format!("signal a timeline: {error}")));
        }
        let _ = (s.fns.vkResetFences)(device, 1, &raw const fence);
        rt::next(record)
    }
}

/// A timeline semaphore arrives with its kernel timeline.
pub unsafe fn h_vkCreateSemaphore(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateSemaphore>();
        let info: *mut VkSemaphoreCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkSemaphoreCreateInfo(s, record, info);
        let kind = find::<VkSemaphoreTypeCreateInfo>((*info).pNext, VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO);
        let timeline = kind.is_some_and(|kind| (*kind).semaphoreType == VK_SEMAPHORE_TYPE_TIMELINE).then(|| {
            let fd = OwnedFd::from_raw_fd(s.take_fd());
            s.drm.import(fd.as_fd()).unwrap_or_else(|error| rt::fail(&format!("import a semaphore timeline: {error}")))
        });
        let mut semaphore: VkSemaphore = 0;
        let result = (s.fns.vkCreateSemaphore)(s.h((*f).device) as usize, info, core::ptr::null(), &raw mut semaphore);
        if result != VK_SUCCESS {
            rt::failed("vkCreateSemaphore", result);
        }
        if let Some(timeline) = timeline {
            s.semaphore_timelines.insert(semaphore, timeline);
        }
        s.set((*f).pSemaphore, semaphore);
        rt::next(record)
    }
}

pub unsafe fn h_vkDestroySemaphore(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkDestroySemaphore>();
        let semaphore = s.h((*f).semaphore);
        (s.fns.vkDestroySemaphore)(s.h((*f).device) as usize, semaphore, core::ptr::null());
        if let Some(timeline) = s.semaphore_timelines.remove(&semaphore) {
            s.drm.destroy(timeline);
        }
        s.set((*f).semaphore, 0);
        rt::next(record)
    }
}

// ---------------------------------------------------------------- memory

/// Mappable memory is shared with the client: system memory through a
/// memfd, mappable VRAM (ReBAR) through a DMA-BUF. Types the probe found
/// no way to share are offered without their host flags.
fn hide_unshareable(memory: &mut VkPhysicalDeviceMemoryProperties, shareable: Shareable) {
    for (index, kind) in memory.memoryTypes[..memory.memoryTypeCount as usize].iter_mut().enumerate() {
        let shared = if kind.propertyFlags & DEVICE_LOCAL != 0 { shareable.vram } else { shareable.memfd };
        if shared & (1 << index) == 0 {
            kind.propertyFlags &= !(HOST_VISIBLE | HOST_COHERENT | HOST_CACHED);
        }
    }
}

/// The probed shareable types of a shown physical device.
fn host_memory(s: &Server, physical: usize) -> Shareable {
    let index = *s.shown.get(&physical).unwrap_or_else(|| rt::fail("unknown physical device"));
    *s.host_memory.get(index).unwrap_or_else(|| rt::fail("no memory probe for this GPU"))
}

pub unsafe fn h_vkGetPhysicalDeviceMemoryProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceMemoryProperties>();
        let properties: *mut VkPhysicalDeviceMemoryProperties = rt::at(record, (*f).pMemoryProperties);
        let physical = s.h((*f).physicalDevice) as usize;
        (s.fns.vkGetPhysicalDeviceMemoryProperties)(physical, properties);
        hide_unshareable(&mut *properties, host_memory(s, physical));
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceMemoryProperties2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceMemoryProperties2>();
        let properties: *mut VkPhysicalDeviceMemoryProperties2 = rt::at(record, (*f).pMemoryProperties);
        crate::generated::link_chain(record, (&raw mut (*properties).pNext).cast());
        let physical = s.h((*f).physicalDevice) as usize;
        (s.fns.vkGetPhysicalDeviceMemoryProperties2)(physical, properties);
        hide_unshareable(&mut (*properties).memoryProperties, host_memory(s, physical));
        rt::next(record)
    }
}

/// True when the chain imports or exports: such memory keeps the driver's
/// own backing and is never mapped through the shared path.
unsafe fn external(info: *const VkMemoryAllocateInfo) -> bool {
    let mut next = unsafe { (*info).pNext.cast::<rt::Base>() };
    while !next.is_null() {
        let kind = unsafe { (*next).sType };
        if matches!(kind, VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO | VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR | VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT) {
            return true;
        }
        next = unsafe { (*next).pNext };
    }
    false
}

/// Host-visible memory is shared, so the client maps the very pages the
/// GPU works on: system memory is a memfd imported as a host allocation,
/// mappable VRAM the driver's own memory exported as a DMA-BUF. Reply:
/// result, then the shared size in 4 KiB pages (0 when not shared).
pub unsafe fn h_vkAllocateMemory(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkAllocateMemory>();
        let info: *mut VkMemoryAllocateInfo = rt::at(record, (*f).pAllocateInfo);
        patch_VkMemoryAllocateInfo(s, record, info);
        let reply: *mut [u32; 2] = rt::at(record, (*f).reply);
        let flags = s.memory_types[(*info).memoryTypeIndex as usize & 31];
        let device = s.h((*f).device) as usize;
        let mut memory: VkDeviceMemory = 0;
        if flags & HOST_VISIBLE == 0 || external(info) {
            crate::shadow::import_planes(s, device, info);
            crate::shadow::dedicate(s, info);
            let result = (s.fns.vkAllocateMemory)(device, info, core::ptr::null(), &raw mut memory);
            s.set((*f).pMemory, memory);
            *reply = [result as u32, 0];
            return rt::next(record);
        }
        let size = ((*info).allocationSize as usize + 4095) & !4095;
        if flags & DEVICE_LOCAL != 0 {
            let export = VkExportMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                pNext: (*info).pNext,
                handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT as u32,
            };
            (*info).pNext = (&raw const export).cast();
            let result = (s.fns.vkAllocateMemory)(device, info, core::ptr::null(), &raw mut memory);
            s.set((*f).pMemory, memory);
            if result != VK_SUCCESS {
                *reply = [result as u32, 0];
                return rt::next(record);
            }
            let get = VkMemoryGetFdInfoKHR {
                sType: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                pNext: core::ptr::null(),
                memory,
                handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            };
            let mut fd = -1;
            let exported = (s.fns.vkGetMemoryFdKHR)(device, &raw const get, &raw mut fd);
            if exported != VK_SUCCESS {
                rt::fail(&format!("export mappable VRAM: {exported}"));
            }
            *reply = [result as u32, (size >> 12) as u32];
            s.send_fd(fd);
            return rt::next(record);
        }
        let (shm, fd) = Shm::create(size).unwrap_or_else(|error| rt::fail(&format!("create shared memory: {error}")));
        // A host allocation has no dedicated owner; dropping the hint is valid.
        unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO);
        let import = VkImportMemoryHostPointerInfoEXT {
            sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
            pNext: (*info).pNext,
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
            pHostPointer: shm.base.cast::<c_void>(),
        };
        (*info).pNext = (&raw const import).cast();
        (*info).allocationSize = size as u64;
        let result = (s.fns.vkAllocateMemory)(device, info, core::ptr::null(), &raw mut memory);
        s.set((*f).pMemory, memory);
        if result != VK_SUCCESS {
            *reply = [result as u32, 0];
            return rt::next(record);
        }
        s.shared.insert((*f).pMemory, shm);
        *reply = [result as u32, (size >> 12) as u32];
        s.send_fd(fd.into_raw_fd());
        rt::next(record)
    }
}

pub unsafe fn h_vkFreeMemory(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkFreeMemory>();
        (s.fns.vkFreeMemory)(s.h((*f).device) as usize, s.h((*f).memory), core::ptr::null());
        s.shared.remove(&(*f).memory);
        s.set((*f).memory, 0);
        rt::next(record)
    }
}

// ---------------------------------------------------------------- images

pub use crate::shadow::{
    h_vkBindImageMemory, h_vkBindImageMemory2, h_vkCmdPipelineBarrier, h_vkCmdPipelineBarrier2, h_vkGetImageMemoryRequirements, h_vkGetImageMemoryRequirements2,
    h_vkQueueSignalReleaseImageANDROID,
};
pub use crate::compressed::{
    h_vkAllocateCommandBuffers, h_vkBeginCommandBuffer, h_vkCmdBindDescriptorSets, h_vkCmdBindPipeline, h_vkCmdCopyBufferToImage, h_vkCmdCopyBufferToImage2,
    h_vkCmdCopyImage, h_vkCmdCopyImage2, h_vkCmdCopyImageToBuffer, h_vkCmdCopyImageToBuffer2, h_vkCreateCommandPool, h_vkCreateImageView,
    h_vkDestroyCommandPool, h_vkDestroyDevice, h_vkFreeCommandBuffers, h_vkGetDeviceImageMemoryRequirements, h_vkGetPhysicalDeviceFormatProperties,
    h_vkGetPhysicalDeviceFormatProperties2, h_vkGetPhysicalDeviceImageFormatProperties, h_vkGetPhysicalDeviceImageFormatProperties2,
};

/// A gralloc buffer arrives as a DMA-BUF import chained to its image, which
/// takes the buffer's memory at creation as `VK_ANDROID_native_buffer`
/// requires. The memory lives as long as the image. Linear window buffers
/// drawn into go through a shadow (shadow.rs); one in VRAM is the image.
pub unsafe fn h_vkCreateImage(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateImage>();
        let device = s.h((*f).device) as usize;
        let info: *mut VkImageCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkImageCreateInfo(s, record, info);
        let buffer = find::<VkImportMemoryFdInfoKHR>((*info).pNext, VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR).map(|import| (*import).fd);
        unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR);
        let image = match buffer {
            Some(fd) if crate::shadow::wanted(&*info) => crate::shadow::create(s, device, info, fd, (*f).pImage),
            Some(fd) => {
                let image = create_image(s, device, info);
                let memory = import_gralloc(s, device, image, fd);
                s.image_memory.insert((*f).pImage, memory);
                image
            }
            None if crate::shadow::wanted_yuv(&*info) => crate::shadow::create_yuv(s, device, info),
            None if crate::shadow::wanted_foreign(&*info) => crate::shadow::create_foreign(s, device, info),
            None => match crate::compressed::format((*info).format) {
                Some(format) => crate::compressed::create(s, device, info, format),
                None => create_image(s, device, info),
            },
        };
        s.set((*f).pImage, image);
        rt::next(record)
    }
}

/// Create an image or end the connection, naming what the host refused.
pub unsafe fn create_image(s: &Server, device: usize, info: *const VkImageCreateInfo) -> VkImage {
    unsafe {
        if let Some(explicit) = find::<VkImageDrmFormatModifierExplicitCreateInfoEXT>((*info).pNext, VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT) {
            let e = &*explicit;
            let planes = if e.drmFormatModifier == 0 { e.drmFormatModifierPlaneCount as usize } else { 0 };
            for plane in 0..planes {
                let pitch = (*e.pPlaneLayouts.add(plane)).rowPitch;
                if pitch % LINEAR_PITCH != 0 {
                    rt::fail(&format!("buffer plane {plane} has a row pitch of {pitch} bytes, not a multiple of {LINEAR_PITCH}"));
                }
            }
        }
        let mut image: VkImage = 0;
        let result = (s.fns.vkCreateImage)(device, info, core::ptr::null(), &raw mut image);
        if result != VK_SUCCESS {
            let mut chain = Vec::new();
            let mut next = (*info).pNext.cast::<rt::Base>();
            while !next.is_null() {
                chain.push((*next).sType);
                next = (*next).pNext;
            }
            let i = &*info;
            rt::fail(&format!(
                "vkCreateImage failed with {result}: format {} type {} tiling {} usage {:x} flags {:x} {}x{}x{} samples {} chain {chain:?}",
                i.format, i.imageType, i.tiling, i.usage, i.flags, i.extent.width, i.extent.height, i.extent.depth, i.samples
            ));
        }
        image
    }
}

/// Bind `image` to the gralloc buffer `fd`, returning the imported memory.
pub unsafe fn import_gralloc(s: &Server, device: usize, image: VkImage, fd: c_int) -> VkDeviceMemory {
    unsafe {
        let mut requirements = core::mem::zeroed::<VkMemoryRequirements>();
        (s.fns.vkGetImageMemoryRequirements)(device, image, &raw mut requirements);
        let mut properties = VkMemoryFdPropertiesKHR { sType: VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR, pNext: core::ptr::null_mut(), memoryTypeBits: 0 };
        let result = (s.fns.vkGetMemoryFdPropertiesKHR)(device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &raw mut properties);
        if result != VK_SUCCESS {
            rt::failed("vkGetMemoryFdPropertiesKHR", result);
        }
        let types = requirements.memoryTypeBits & properties.memoryTypeBits;
        if types == 0 {
            rt::fail("the host cannot bind this gralloc buffer to its image");
        }
        let dedicated = VkMemoryDedicatedAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            pNext: core::ptr::null(),
            image,
            buffer: 0,
        };
        let import = VkImportMemoryFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
            pNext: (&raw const dedicated).cast(),
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            fd,
        };
        // The whole DMA-BUF: a plane's requirement leaves out its offset.
        let size = libc::lseek(fd, 0, libc::SEEK_END);
        if size < 0 {
            rt::fail("size a gralloc buffer's DMA-BUF");
        }
        let allocate = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: (&raw const import).cast(),
            allocationSize: size as u64,
            memoryTypeIndex: types.trailing_zeros(),
        };
        let mut memory: VkDeviceMemory = 0;
        let result = (s.fns.vkAllocateMemory)(device, &raw const allocate, core::ptr::null(), &raw mut memory);
        if result != VK_SUCCESS {
            rt::failed("vkAllocateMemory for a gralloc buffer", result);
        }
        let result = (s.fns.vkBindImageMemory)(device, image, memory, 0);
        if result != VK_SUCCESS {
            rt::failed("vkBindImageMemory for a gralloc buffer", result);
        }
        memory
    }
}

pub unsafe fn h_vkDestroyImage(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkDestroyImage>();
        let device = s.h((*f).device) as usize;
        if let Some(shadow) = s.shadows.remove(&(*f).image) {
            crate::shadow::destroy(s, device, shadow);
        }
        if let Some(foreign) = s.foreign.remove(&s.h((*f).image)) {
            crate::shadow::destroy_foreign(s, device, foreign);
        }
        crate::compressed::destroy(s, s.h((*f).image));
        (s.fns.vkDestroyImage)(device, s.h((*f).image), core::ptr::null());
        if let Some(memory) = s.image_memory.remove(&(*f).image) {
            (s.fns.vkFreeMemory)(device, memory, core::ptr::null());
        }
        s.set((*f).image, 0);
        rt::next(record)
    }
}

/// The first structure of one type in a pNext chain.
pub unsafe fn find<T>(mut next: *const c_void, kind: VkStructureType) -> Option<*const T> {
    unsafe {
        while !next.is_null() {
            if (*next.cast::<rt::Base>()).sType == kind {
                return Some(next.cast());
            }
            next = (*next.cast::<rt::Base>()).pNext.cast();
        }
        None
    }
}

/// Drop one structure type from a pNext chain, given its head link.
pub unsafe fn unlink(head: *mut *mut rt::Base, kind: VkStructureType) {
    unsafe {
        let mut link = head;
        while !(*link).is_null() {
            if (**link).sType == kind {
                *link = (**link).pNext;
            } else {
                link = &raw mut (**link).pNext;
            }
        }
    }
}

// ---------------------------------------------------------------- pipelines

/// `VK_PIPELINE_CREATE_FAIL_ON_PIPELINE_COMPILE_REQUIRED_BIT`: an application
/// asks not to wait for a compile. Creation is asynchronous here, so the
/// application never waits; the pipeline is compiled instead of refused.
const FAIL_ON_COMPILE_REQUIRED: u32 = 0x100;

pub unsafe fn h_vkCreateGraphicsPipelines(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateGraphicsPipelines>();
        let count = (*f).createInfoCount as usize;
        let infos: *mut VkGraphicsPipelineCreateInfo = rt::at(record, (*f).pCreateInfos);
        for i in 0..count {
            patch_VkGraphicsPipelineCreateInfo(s, record, infos.add(i));
            (*infos.add(i)).flags &= !FAIL_ON_COMPILE_REQUIRED;
        }
        let mut pipelines = vec![0 as VkPipeline; count];
        let create = s.fns.vkCreateGraphicsPipelines;
        let result = create(s.h((*f).device) as usize, s.h((*f).pipelineCache), count as u32, infos, core::ptr::null(), pipelines.as_mut_ptr());
        if result != VK_SUCCESS {
            rt::failed("vkCreateGraphicsPipelines", result);
        }
        let ids: *mut u64 = rt::at(record, (*f).pPipelines);
        for (i, &pipeline) in pipelines.iter().enumerate() {
            s.set(*ids.add(i), pipeline);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCreateComputePipelines(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateComputePipelines>();
        let count = (*f).createInfoCount as usize;
        let infos: *mut VkComputePipelineCreateInfo = rt::at(record, (*f).pCreateInfos);
        for i in 0..count {
            patch_VkComputePipelineCreateInfo(s, record, infos.add(i));
            (*infos.add(i)).flags &= !FAIL_ON_COMPILE_REQUIRED;
        }
        let mut pipelines = vec![0 as VkPipeline; count];
        let create = s.fns.vkCreateComputePipelines;
        let result = create(s.h((*f).device) as usize, s.h((*f).pipelineCache), count as u32, infos, core::ptr::null(), pipelines.as_mut_ptr());
        if result != VK_SUCCESS {
            rt::failed("vkCreateComputePipelines", result);
        }
        let ids: *mut u64 = rt::at(record, (*f).pPipelines);
        for (i, &pipeline) in pipelines.iter().enumerate() {
            s.set(*ids.add(i), pipeline);
        }
        rt::next(record)
    }
}
