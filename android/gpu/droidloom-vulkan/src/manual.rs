//! Entry points with client-side state: the instance and its physical
//! devices, queues and their timelines, fences, and command-buffer streams.

#![allow(non_snake_case)]

use core::ffi::{c_char, c_int, CStr};
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Mutex;

use droidloom_gpu_protocol::generated::vk::*;
use droidloom_gpu_protocol::generated::{frame, op};
use droidloom_gpu_protocol::sys;

use crate::generated::*;
use crate::rt::{self, CommandBuffer, Sink};
pub use crate::wsi::*;

const VK_API_VERSION_1_3: u32 = (1 << 22) | (3 << 12);
const VK_FENCE_CREATE_SIGNALED_BIT: u32 = 1;

/// Only the WSI the client answers alone; the host's instance extensions
/// stay the host's.
pub unsafe extern "system" fn vkEnumerateInstanceExtensionProperties(
    _layer: *const c_char,
    count: *mut u32,
    properties: *mut VkExtensionProperties,
) -> VkResult {
    let all = INSTANCE_EXTENSIONS.map(|(name, specVersion)| {
        let mut extension = VkExtensionProperties { extensionName: [0; 256], specVersion };
        for (to, &from) in extension.extensionName.iter_mut().zip(name.to_bytes()) {
            *to = from as c_char;
        }
        extension
    });
    if properties.is_null() {
        *count = all.len() as u32;
        return VK_SUCCESS;
    }
    let n = (*count as usize).min(all.len());
    core::ptr::copy_nonoverlapping(all.as_ptr(), properties, n);
    *count = n as u32;
    if n < all.len() { VK_INCOMPLETE } else { VK_SUCCESS }
}

pub unsafe extern "system" fn vkEnumerateInstanceVersion(version: *mut u32) -> VkResult {
    *version = VK_API_VERSION_1_3;
    VK_SUCCESS
}

pub unsafe extern "system" fn vkGetInstanceProcAddr(_instance: VkInstance, name: *const c_char) -> PFN_vkVoidFunction {
    let name = CStr::from_ptr(name).to_bytes();
    proc_addr(name)
        .map(|(entry, _)| entry)
        .or_else(|| crate::android::proc_addr(name))
        .map(|entry| core::mem::transmute::<*const core::ffi::c_void, unsafe extern "system" fn()>(entry))
}

pub unsafe extern "system" fn vkGetDeviceProcAddr(_device: VkDevice, name: *const c_char) -> PFN_vkVoidFunction {
    let name = CStr::from_ptr(name).to_bytes();
    match proc_addr(name) {
        Some((entry, rt::Level::Device)) => Some(entry),
        Some(_) => None,
        None => crate::android::proc_addr(name),
    }
    .map(|entry| core::mem::transmute::<*const core::ffi::c_void, unsafe extern "system" fn()>(entry))
}

/// Commands the loader may look up per physical device.
pub unsafe fn physical_proc_addr(name: *const c_char) -> PFN_vkVoidFunction {
    match proc_addr(CStr::from_ptr(name).to_bytes()) {
        Some((entry, rt::Level::Physical)) => Some(core::mem::transmute::<*const core::ffi::c_void, unsafe extern "system" fn()>(entry)),
        _ => None,
    }
}

const MAX_PHYSICAL: usize = 8;

pub unsafe extern "system" fn vkCreateInstance(
    pCreateInfo: *const VkInstanceCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pInstance: *mut VkInstance,
) -> VkResult {
    if let Err(error) = rt::ensure_connected() {
        eprintln!("droidloom-vulkan: {error}");
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // The host never sees the client's own WSI extensions.
    let mut info = *pCreateInfo;
    let names: Vec<*const c_char> = (0..info.enabledExtensionCount as usize)
        .map(|i| *info.ppEnabledExtensionNames.add(i))
        .filter(|&name| !INSTANCE_EXTENSIONS.iter().any(|(own, _)| CStr::from_ptr(name) == *own))
        .collect();
    info.enabledExtensionCount = names.len() as u32;
    info.ppEnabledExtensionNames = names.as_ptr();
    let pCreateInfo = &raw const info;
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let mut m = rt::Measure::new(8 + size_of::<frame::vkCreateInstance>());
    body_vkCreateInstance(&mut m, &ids, &mut outs, pCreateInfo, pAllocator, pInstance);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateInstance, m.size());
    let mut w = rt::Write::new(rec, 8 + size_of::<frame::vkCreateInstance>());
    body_vkCreateInstance(&mut w, &ids, &mut outs, pCreateInfo, pAllocator, pInstance);
    let end = ring.publish(true);
    drop(ring);
    rt::wait(end);
    let result = *w.at::<VkResult>(outs[0]);
    if result != VK_SUCCESS {
        rt::free_id(ids[0]);
        return result;
    }

    // Physical devices are fixed for the instance's life: fetch them now.
    let physical_ids: Vec<u64> = (0..MAX_PHYSICAL).map(|_| rt::new_id()).collect();
    let frame_size = 8 + size_of::<frame::vkEnumeratePhysicalDevices>();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkEnumeratePhysicalDevices, frame_size + 8 * MAX_PHYSICAL);
    let mut w = rt::Write::new(rec, frame_size);
    let field = |name: usize| 8 + name as i64;
    let reply = w.reply_alloc(8);
    let count = w.reply_alloc(8);
    w.put(count, MAX_PHYSICAL as u32);
    let list = rt::enc_pod::<u64, _>(&mut w, physical_ids.as_ptr(), MAX_PHYSICAL);
    w.put(field(offset_of!(frame::vkEnumeratePhysicalDevices, instance)), ids[0]);
    w.put(field(offset_of!(frame::vkEnumeratePhysicalDevices, pPhysicalDeviceCount)), count);
    w.put(field(offset_of!(frame::vkEnumeratePhysicalDevices, pPhysicalDevices)), list);
    w.put(field(offset_of!(frame::vkEnumeratePhysicalDevices, reply)), reply);
    let end = ring.publish(true);
    drop(ring);
    rt::wait(end);
    let found = (*w.at::<u32>(count) as usize).min(MAX_PHYSICAL);
    for &unused in &physical_ids[found..] {
        rt::free_id(unused);
    }
    let instance = Box::new(rt::Instance {
        object: rt::object(ids[0]),
        physical: physical_ids[..found].iter().map(|&id| rt::new_dispatchable(id)).collect(),
    });
    *pInstance = Box::into_raw(instance) as VkInstance;
    VK_SUCCESS
}

pub unsafe extern "system" fn vkDestroyInstance(instance: VkInstance, pAllocator: *const VkAllocationCallbacks) {
    if instance == 0 {
        return;
    }
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let size = 8 + size_of::<frame::vkDestroyInstance>();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkDestroyInstance, size);
    let mut w = rt::Write::new(rec, size);
    body_vkDestroyInstance(&mut w, &ids, &mut outs, instance, pAllocator);
    ring.publish(true);
    drop(ring);
    let instance = Box::from_raw(instance as *mut rt::Instance);
    for &physical in &instance.physical {
        rt::free_dispatchable(physical);
    }
    rt::free_id(instance.object.id);
}

pub unsafe extern "system" fn vkEnumeratePhysicalDevices(
    instance: VkInstance,
    pPhysicalDeviceCount: *mut u32,
    pPhysicalDevices: *mut VkPhysicalDevice,
) -> VkResult {
    let physical = &(*(instance as *const rt::Instance)).physical;
    if pPhysicalDevices.is_null() {
        *pPhysicalDeviceCount = physical.len() as u32;
        return VK_SUCCESS;
    }
    let n = (*pPhysicalDeviceCount as usize).min(physical.len());
    core::ptr::copy_nonoverlapping(physical.as_ptr(), pPhysicalDevices, n);
    *pPhysicalDeviceCount = n as u32;
    if n < physical.len() { VK_INCOMPLETE } else { VK_SUCCESS }
}

/// Layers belong to the application's loader; the driver has none.
pub unsafe extern "system" fn vkEnumerateInstanceLayerProperties(count: *mut u32, _properties: *mut VkLayerProperties) -> VkResult {
    *count = 0;
    VK_SUCCESS
}

pub unsafe extern "system" fn vkEnumerateDeviceLayerProperties(
    _physicalDevice: VkPhysicalDevice,
    count: *mut u32,
    _properties: *mut VkLayerProperties,
) -> VkResult {
    *count = 0;
    VK_SUCCESS
}

/// Each physical device is a group of one.
pub unsafe extern "system" fn vkEnumeratePhysicalDeviceGroups(
    instance: VkInstance,
    count: *mut u32,
    groups: *mut VkPhysicalDeviceGroupProperties,
) -> VkResult {
    let physical = &(*(instance as *const rt::Instance)).physical;
    if groups.is_null() {
        *count = physical.len() as u32;
        return VK_SUCCESS;
    }
    let n = (*count as usize).min(physical.len());
    for (i, &device) in physical[..n].iter().enumerate() {
        let group = &mut *groups.add(i);
        group.physicalDeviceCount = 1;
        group.physicalDevices = [0; 32];
        group.physicalDevices[0] = device as VkPhysicalDevice;
        group.subsetAllocation = 0;
    }
    *count = n as u32;
    if n < physical.len() { VK_INCOMPLETE } else { VK_SUCCESS }
}

static QUEUES: Mutex<Option<HashMap<(u64, u32, u32), usize>>> = Mutex::new(None);

pub unsafe extern "system" fn vkGetDeviceQueue(device: VkDevice, queueFamilyIndex: u32, queueIndex: u32, pQueue: *mut VkQueue) {
    let mut queues = QUEUES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (rt::id(device), queueFamilyIndex, queueIndex);
    if let Some(&queue) = queues.get_or_insert_with(HashMap::new).get(&key) {
        *pQueue = queue;
        return;
    }
    let connection = rt::conn();
    let timeline = connection.drm.create().unwrap_or_else(|error| rt::fail(&format!("create a queue timeline: {error}")));
    let exported = connection.drm.export(timeline).unwrap_or_else(|error| rt::fail(&format!("export a queue timeline: {error}")));
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let size = 8 + size_of::<frame::vkGetDeviceQueue>();
    let mut ring = rt::ring();
    // The host takes the timeline when it reaches this record.
    connection.send_fd(exported.as_raw_fd());
    let rec = ring.reserve(op::vkGetDeviceQueue, size);
    let mut w = rt::Write::new(rec, size);
    body_vkGetDeviceQueue(&mut w, &ids, &mut outs, device, queueFamilyIndex, queueIndex, pQueue);
    ring.publish(false);
    drop(ring);
    let queue = Box::into_raw(Box::new(rt::Queue { object: rt::object(ids[0]), device, timeline, point: 0.into() })) as usize;
    queues.get_or_insert_with(HashMap::new).insert(key, queue);
    *pQueue = queue;
}

/// The same queues; protected ones are not forwarded.
pub unsafe extern "system" fn vkGetDeviceQueue2(device: VkDevice, pQueueInfo: *const VkDeviceQueueInfo2, pQueue: *mut VkQueue) {
    if (*pQueueInfo).flags != 0 {
        rt::fail("protected queues are not forwarded");
    }
    vkGetDeviceQueue(device, (*pQueueInfo).queueFamilyIndex, (*pQueueInfo).queueIndex, pQueue);
}

/// The 1.0 submit, rewritten as `vkQueueSubmit2` so fences keep one path.
/// Timeline values come from `VkTimelineSemaphoreSubmitInfo`.
pub unsafe extern "system" fn vkQueueSubmit(queue: VkQueue, submitCount: u32, pSubmits: *const VkSubmitInfo, fence: VkFence) -> VkResult {
    const ALL_COMMANDS: u64 = 0x10000;
    let semaphore = |semaphore: VkSemaphore, value: u64, stageMask: u64| VkSemaphoreSubmitInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
        pNext: core::ptr::null(),
        semaphore,
        value,
        stageMask,
        deviceIndex: 0,
    };
    let submits = core::slice::from_raw_parts(pSubmits, submitCount as usize);
    let mut waits = Vec::with_capacity(submits.len());
    let mut commands = Vec::with_capacity(submits.len());
    let mut signals = Vec::with_capacity(submits.len());
    for submit in submits {
        let mut values: Option<&VkTimelineSemaphoreSubmitInfo> = None;
        let mut next = submit.pNext.cast::<rt::Base>();
        while !next.is_null() {
            if (*next).sType == VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO {
                values = Some(&*next.cast());
            }
            next = (*next).pNext.cast();
        }
        let value = |list: Option<*const u64>, count: u32, i: usize| match list {
            Some(list) if !list.is_null() && (i as u32) < count => *list.add(i),
            _ => 0,
        };
        waits.push(
            (0..submit.waitSemaphoreCount as usize)
                .map(|i| {
                    let stage = u64::from(*submit.pWaitDstStageMask.add(i));
                    let v = value(values.map(|t| t.pWaitSemaphoreValues), values.map_or(0, |t| t.waitSemaphoreValueCount), i);
                    semaphore(*submit.pWaitSemaphores.add(i), v, stage)
                })
                .collect::<Vec<_>>(),
        );
        commands.push(
            (0..submit.commandBufferCount as usize)
                .map(|i| VkCommandBufferSubmitInfo {
                    sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                    pNext: core::ptr::null(),
                    commandBuffer: *submit.pCommandBuffers.add(i),
                    deviceMask: 0,
                })
                .collect::<Vec<_>>(),
        );
        signals.push(
            (0..submit.signalSemaphoreCount as usize)
                .map(|i| {
                    let v = value(values.map(|t| t.pSignalSemaphoreValues), values.map_or(0, |t| t.signalSemaphoreValueCount), i);
                    semaphore(*submit.pSignalSemaphores.add(i), v, ALL_COMMANDS)
                })
                .collect::<Vec<_>>(),
        );
    }
    let converted: Vec<VkSubmitInfo2> = (0..submits.len())
        .map(|i| VkSubmitInfo2 {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
            pNext: core::ptr::null(),
            flags: 0,
            waitSemaphoreInfoCount: waits[i].len() as u32,
            pWaitSemaphoreInfos: waits[i].as_ptr(),
            commandBufferInfoCount: commands[i].len() as u32,
            pCommandBufferInfos: commands[i].as_ptr(),
            signalSemaphoreInfoCount: signals[i].len() as u32,
            pSignalSemaphoreInfos: signals[i].as_ptr(),
        })
        .collect();
    vkQueueSubmit2(queue, converted.len() as u32, converted.as_ptr(), fence)
}

pub unsafe extern "system" fn vkQueueSubmit2(queue: VkQueue, submitCount: u32, pSubmits: *const VkSubmitInfo2, fence: VkFence) -> VkResult {
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkQueueSubmit2>();
    let mut m = rt::Measure::new(frame_size);
    body_vkQueueSubmit2(&mut m, &ids, &mut outs, queue, submitCount, pSubmits, fence);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkQueueSubmit2, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkQueueSubmit2(&mut w, &ids, &mut outs, queue, submitCount, pSubmits, fence);
    if fence != 0 {
        // The host numbers fenced submissions the same way, in ring order.
        let queue = rt::queue(queue);
        let point = queue.point.fetch_add(1, Ordering::Relaxed) + 1;
        let state = rt::fence(fence);
        state.timeline.store(u64::from(queue.timeline), Ordering::Relaxed);
        state.point.store(point, Ordering::Release);
    }
    ring.publish(true);
    VK_SUCCESS
}

pub unsafe extern "system" fn vkCreateFence(
    device: VkDevice,
    pCreateInfo: *const VkFenceCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pFence: *mut VkFence,
) -> VkResult {
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkCreateFence>();
    let mut m = rt::Measure::new(frame_size);
    body_vkCreateFence(&mut m, &ids, &mut outs, device, pCreateInfo, pAllocator, pFence);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateFence, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkCreateFence(&mut w, &ids, &mut outs, device, pCreateInfo, pAllocator, pFence);
    ring.publish(false);
    drop(ring);
    let signaled = (*pCreateInfo).flags & VK_FENCE_CREATE_SIGNALED_BIT != 0;
    rt::fence(ids[0]).point.store(if signaled { 0 } else { rt::UNSUBMITTED }, Ordering::Release);
    *pFence = ids[0];
    VK_SUCCESS
}

pub unsafe extern "system" fn vkWaitForFences(_device: VkDevice, fenceCount: u32, pFences: *const VkFence, waitAll: VkBool32, timeout: u64) -> VkResult {
    let mut handles = Vec::with_capacity(fenceCount as usize);
    let mut points = Vec::with_capacity(fenceCount as usize);
    for i in 0..fenceCount as usize {
        let state = rt::fence(*pFences.add(i));
        match state.point.load(Ordering::Acquire) {
            0 if waitAll == 0 => return VK_SUCCESS,
            0 => {}
            rt::UNSUBMITTED => {
                if timeout == 0 {
                    return VK_TIMEOUT;
                }
                rt::fail("waiting for a fence that was never submitted");
            }
            point => {
                handles.push(state.timeline.load(Ordering::Relaxed) as u32);
                points.push(point);
            }
        }
    }
    if handles.is_empty() {
        return VK_SUCCESS;
    }
    let deadline = if timeout >= i64::MAX as u64 { i64::MAX } else { sys::monotonic_ns().saturating_add(timeout as i64) };
    let flags = sys::WAIT_FOR_SUBMIT | if waitAll != 0 { sys::WAIT_ALL } else { 0 };
    match rt::conn().drm.wait(&handles, &points, flags, deadline) {
        Ok(true) => VK_SUCCESS,
        Ok(false) => VK_TIMEOUT,
        Err(_) => VK_ERROR_DEVICE_LOST,
    }
}

pub unsafe extern "system" fn vkResetFences(_device: VkDevice, fenceCount: u32, pFences: *const VkFence) -> VkResult {
    for i in 0..fenceCount as usize {
        rt::fence(*pFences.add(i)).point.store(rt::UNSUBMITTED, Ordering::Release);
    }
    VK_SUCCESS
}

pub unsafe extern "system" fn vkGetFenceStatus(_device: VkDevice, fence: VkFence) -> VkResult {
    let state = rt::fence(fence);
    match state.point.load(Ordering::Acquire) {
        0 => VK_SUCCESS,
        rt::UNSUBMITTED => VK_NOT_READY,
        point => match rt::conn().drm.query(state.timeline.load(Ordering::Relaxed) as u32) {
            Ok(reached) if reached >= point => VK_SUCCESS,
            Ok(_) => VK_NOT_READY,
            Err(_) => VK_ERROR_DEVICE_LOST,
        },
    }
}

// ---------------------------------------------------------------- timeline semaphores

/// Each timeline semaphore's kernel timeline (0 for a binary semaphore).
/// The host adds every submitted signal to it at the signalled value, the
/// client adds its own host-side signals, and waits and counter reads go
/// straight to the kernel, like fences.
static TIMELINES: rt::IdTable<AtomicU32> = rt::IdTable::new("semaphore");

const VK_SEMAPHORE_TYPE_TIMELINE: i32 = 1;
const VK_SEMAPHORE_WAIT_ANY_BIT: u32 = 1;

/// The initial value of a timeline semaphore; None for a binary one.
unsafe fn timeline_initial(info: *const VkSemaphoreCreateInfo) -> Option<u64> {
    unsafe {
        let mut next = (*info).pNext.cast::<rt::Base>();
        while !next.is_null() {
            if (*next).sType == VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO {
                let kind = &*next.cast::<VkSemaphoreTypeCreateInfo>();
                return (kind.semaphoreType == VK_SEMAPHORE_TYPE_TIMELINE).then_some(kind.initialValue);
            }
            next = (*next).pNext.cast();
        }
        None
    }
}

fn timeline(semaphore: VkSemaphore) -> u32 {
    // SAFETY: entries are atomics.
    let handle = unsafe { (*TIMELINES.at(semaphore)).load(Ordering::Acquire) };
    if handle == 0 {
        rt::fail("a timeline operation on a binary semaphore");
    }
    handle
}

/// A timeline semaphore sends its kernel timeline along, ahead of the record.
pub unsafe extern "system" fn vkCreateSemaphore(
    device: VkDevice,
    pCreateInfo: *const VkSemaphoreCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pSemaphore: *mut VkSemaphore,
) -> VkResult {
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkCreateSemaphore>();
    let mut m = rt::Measure::new(frame_size);
    body_vkCreateSemaphore(&mut m, &ids, &mut outs, device, pCreateInfo, pAllocator, pSemaphore);
    let drm = &rt::conn().drm;
    let mirror = timeline_initial(pCreateInfo).map(|initial| {
        let handle = drm.create().unwrap_or_else(|error| rt::fail(&format!("create a semaphore timeline: {error}")));
        if initial > 0 {
            drm.signal(handle, initial).unwrap_or_else(|error| rt::fail(&format!("set a semaphore's initial value: {error}")));
        }
        let fd = drm.export(handle).unwrap_or_else(|error| rt::fail(&format!("export a semaphore timeline: {error}")));
        (handle, fd)
    });
    let _fds = rt::fd_lock();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateSemaphore, m.size());
    if let Some((_, fd)) = &mirror {
        rt::send_fd(fd.as_raw_fd());
    }
    let mut w = rt::Write::new(rec, frame_size);
    body_vkCreateSemaphore(&mut w, &ids, &mut outs, device, pCreateInfo, pAllocator, pSemaphore);
    ring.publish(false);
    drop(ring);
    (*TIMELINES.at(ids[0])).store(mirror.map_or(0, |(handle, _)| handle), Ordering::Release);
    *pSemaphore = ids[0];
    VK_SUCCESS
}

pub unsafe extern "system" fn vkDestroySemaphore(device: VkDevice, semaphore: VkSemaphore, pAllocator: *const VkAllocationCallbacks) {
    if semaphore == 0 {
        return;
    }
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkDestroySemaphore>();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkDestroySemaphore, frame_size);
    let mut w = rt::Write::new(rec, frame_size);
    body_vkDestroySemaphore(&mut w, &ids, &mut outs, device, semaphore, pAllocator);
    ring.publish(false);
    drop(ring);
    let mirror = (*TIMELINES.at(semaphore)).swap(0, Ordering::AcqRel);
    if mirror != 0 {
        rt::conn().drm.destroy(mirror);
    }
    rt::free_id(semaphore);
}

/// The host semaphore gets the value for the GPU, the kernel timeline for
/// the client's waiters.
pub unsafe extern "system" fn vkSignalSemaphore(device: VkDevice, pSignalInfo: *const VkSemaphoreSignalInfo) -> VkResult {
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkSignalSemaphore>();
    let mut m = rt::Measure::new(frame_size);
    body_vkSignalSemaphore(&mut m, &ids, &mut outs, device, pSignalInfo);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkSignalSemaphore, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkSignalSemaphore(&mut w, &ids, &mut outs, device, pSignalInfo);
    ring.publish(true);
    drop(ring);
    let info = &*pSignalInfo;
    match rt::conn().drm.signal(timeline(info.semaphore), info.value) {
        Ok(()) => VK_SUCCESS,
        Err(_) => VK_ERROR_DEVICE_LOST,
    }
}

pub unsafe extern "system" fn vkWaitSemaphores(_device: VkDevice, pWaitInfo: *const VkSemaphoreWaitInfo, timeout: u64) -> VkResult {
    let info = &*pWaitInfo;
    let count = info.semaphoreCount as usize;
    if count == 0 {
        return VK_SUCCESS;
    }
    let handles: Vec<u32> = (0..count).map(|i| timeline(*info.pSemaphores.add(i))).collect();
    let points = core::slice::from_raw_parts(info.pValues, count);
    let deadline = if timeout >= i64::MAX as u64 { i64::MAX } else { sys::monotonic_ns().saturating_add(timeout as i64) };
    let flags = sys::WAIT_FOR_SUBMIT | if info.flags & VK_SEMAPHORE_WAIT_ANY_BIT != 0 { 0 } else { sys::WAIT_ALL };
    match rt::conn().drm.wait(&handles, points, flags, deadline) {
        Ok(true) => VK_SUCCESS,
        Ok(false) => VK_TIMEOUT,
        Err(_) => VK_ERROR_DEVICE_LOST,
    }
}

pub unsafe extern "system" fn vkGetSemaphoreCounterValue(_device: VkDevice, semaphore: VkSemaphore, pValue: *mut u64) -> VkResult {
    match rt::conn().drm.query(timeline(semaphore)) {
        Ok(value) => {
            *pValue = value;
            VK_SUCCESS
        }
        Err(_) => VK_ERROR_DEVICE_LOST,
    }
}

const VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT: u32 = 0x8;
const VK_EXTERNAL_FENCE_FEATURE_EXPORTABLE_BIT: u32 = 1;
const VK_EXTERNAL_FENCE_FEATURE_IMPORTABLE_BIT: u32 = 2;
const VK_FENCE_IMPORT_TEMPORARY_BIT: u32 = 1;

/// Client points for fences that sync files signal. The lock keeps the
/// points in order on the timeline and the scratch syncobj to one user.
struct SyncFileTimeline {
    handle: u32,
    scratch: u32,
    point: u64,
}

static SYNC_FILES: std::sync::OnceLock<Mutex<SyncFileTimeline>> = std::sync::OnceLock::new();

/// Signal `fence` when `sync_file` signals (-1: now). Takes the descriptor.
pub unsafe fn signal_fence(fence: VkFence, sync_file: c_int) {
    let state = rt::fence(fence);
    if sync_file < 0 {
        state.point.store(0, Ordering::Release);
        return;
    }
    // SAFETY: the caller hands the descriptor over.
    let file = unsafe { OwnedFd::from_raw_fd(sync_file) };
    let drm = &rt::conn().drm;
    let timelines = SYNC_FILES.get_or_init(|| {
        let create = || drm.create().unwrap_or_else(|error| rt::fail(&format!("create a syncobj: {error}")));
        Mutex::new(SyncFileTimeline { handle: create(), scratch: create(), point: 0 })
    });
    let mut timeline = timelines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    timeline.point += 1;
    drm.attach(timeline.handle, timeline.point, timeline.scratch, file.as_fd())
        .unwrap_or_else(|error| rt::fail(&format!("attach a sync file to a fence: {error}")));
    state.timeline.store(u64::from(timeline.handle), Ordering::Relaxed);
    state.point.store(timeline.point, Ordering::Release);
}

pub unsafe extern "system" fn vkGetPhysicalDeviceExternalFenceProperties(
    _physicalDevice: VkPhysicalDevice,
    pExternalFenceInfo: *const VkPhysicalDeviceExternalFenceInfo,
    pExternalFenceProperties: *mut VkExternalFenceProperties,
) {
    unsafe {
        let sync_file = (*pExternalFenceInfo).handleType as u32 == VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT;
        let types = if sync_file { VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT } else { 0 };
        (*pExternalFenceProperties).exportFromImportedHandleTypes = types;
        (*pExternalFenceProperties).compatibleHandleTypes = types;
        (*pExternalFenceProperties).externalFenceFeatures =
            if sync_file { VK_EXTERNAL_FENCE_FEATURE_EXPORTABLE_BIT | VK_EXTERNAL_FENCE_FEATURE_IMPORTABLE_BIT } else { 0 };
    }
}

/// Like any sync file export, this resets the fence.
pub unsafe extern "system" fn vkGetFenceFdKHR(_device: VkDevice, pGetFdInfo: *const VkFenceGetFdInfoKHR, pFd: *mut c_int) -> VkResult {
    unsafe {
        if (*pGetFdInfo).handleType as u32 != VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT {
            rt::fail("fences export sync files only");
        }
        let state = rt::fence((*pGetFdInfo).fence);
        *pFd = match state.point.load(Ordering::Acquire) {
            0 => -1,
            rt::UNSUBMITTED => rt::fail("exporting a fence that was never submitted"),
            point => rt::conn()
                .drm
                .sync_file(state.timeline.load(Ordering::Relaxed) as u32, point)
                .unwrap_or_else(|error| rt::fail(&format!("export a fence: {error}")))
                .into_raw_fd(),
        };
        state.point.store(rt::UNSUBMITTED, Ordering::Release);
        VK_SUCCESS
    }
}

pub unsafe extern "system" fn vkImportFenceFdKHR(_device: VkDevice, pImportFenceFdInfo: *const VkImportFenceFdInfoKHR) -> VkResult {
    unsafe {
        let info = &*pImportFenceFdInfo;
        if info.handleType as u32 != VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT || info.flags & VK_FENCE_IMPORT_TEMPORARY_BIT == 0 {
            rt::fail("fences import sync files only, temporarily");
        }
        signal_fence(info.fence, info.fd);
        VK_SUCCESS
    }
}

pub unsafe extern "system" fn vkAllocateCommandBuffers(
    device: VkDevice,
    pAllocateInfo: *const VkCommandBufferAllocateInfo,
    pCommandBuffers: *mut VkCommandBuffer,
) -> VkResult {
    let count = (*pAllocateInfo).commandBufferCount as usize;
    let ids: Vec<u64> = (0..count).map(|_| rt::new_id()).collect();
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkAllocateCommandBuffers>();
    let mut m = rt::Measure::new(frame_size);
    body_vkAllocateCommandBuffers(&mut m, &ids, &mut outs, device, pAllocateInfo, pCommandBuffers);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkAllocateCommandBuffers, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkAllocateCommandBuffers(&mut w, &ids, &mut outs, device, pAllocateInfo, pCommandBuffers);
    ring.publish(false);
    drop(ring);
    for (i, &id) in ids.iter().enumerate() {
        *pCommandBuffers.add(i) = CommandBuffer::new(id);
    }
    VK_SUCCESS
}

pub unsafe extern "system" fn vkFreeCommandBuffers(
    device: VkDevice,
    commandPool: VkCommandPool,
    commandBufferCount: u32,
    pCommandBuffers: *const VkCommandBuffer,
) {
    let live: Vec<VkCommandBuffer> = (0..commandBufferCount as usize).map(|i| *pCommandBuffers.add(i)).filter(|&cb| cb != 0).collect();
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkFreeCommandBuffers>();
    let mut m = rt::Measure::new(frame_size);
    body_vkFreeCommandBuffers(&mut m, &ids, &mut outs, device, commandPool, live.len() as u32, live.as_ptr());
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkFreeCommandBuffers, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkFreeCommandBuffers(&mut w, &ids, &mut outs, device, commandPool, live.len() as u32, live.as_ptr());
    ring.publish(false);
    drop(ring);
    for cb in live {
        CommandBuffer::free(cb);
    }
}

pub unsafe extern "system" fn vkBeginCommandBuffer(commandBuffer: VkCommandBuffer, pBeginInfo: *const VkCommandBufferBeginInfo) -> VkResult {
    let stream = rt::stream(commandBuffer);
    stream.begin();
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkBeginCommandBuffer>();
    let mut m = rt::Measure::new(frame_size);
    body_vkBeginCommandBuffer(&mut m, &ids, &mut outs, commandBuffer, pBeginInfo);
    let rec = stream.reserve(op::vkBeginCommandBuffer, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkBeginCommandBuffer(&mut w, &ids, &mut outs, commandBuffer, pBeginInfo);
    VK_SUCCESS
}

pub unsafe extern "system" fn vkEndCommandBuffer(commandBuffer: VkCommandBuffer) -> VkResult {
    let stream = rt::stream(commandBuffer);
    stream.reserve(op::vkEndCommandBuffer, 8 + size_of::<frame::vkEndCommandBuffer>());
    stream.finish();
    VK_SUCCESS
}

// ---------------------------------------------------------------- memory

/// Remembers the physical device, which the swapchain reads memory types
/// from.
pub unsafe extern "system" fn vkCreateDevice(
    physicalDevice: VkPhysicalDevice,
    pCreateInfo: *const VkDeviceCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pDevice: *mut VkDevice,
) -> VkResult {
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let mut m = rt::Measure::new(8 + size_of::<frame::vkCreateDevice>());
    body_vkCreateDevice(&mut m, &ids, &mut outs, physicalDevice, pCreateInfo, pAllocator, pDevice);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateDevice, m.size());
    let mut w = rt::Write::new(rec, 8 + size_of::<frame::vkCreateDevice>());
    body_vkCreateDevice(&mut w, &ids, &mut outs, physicalDevice, pCreateInfo, pAllocator, pDevice);
    let end = ring.publish(true);
    drop(ring);
    rt::wait(end);
    *rt::PHYSICAL.at(ids[0]) = physicalDevice as usize;
    *pDevice = rt::new_dispatchable(ids[0]) as _;
    *w.at::<VkResult>(outs[0])
}

/// The host answers with the result and, in the next word, how many 4 KiB
/// pages of shared memory follow as a descriptor (0: not host-visible
/// here): a memfd for system memory, a DMA-BUF for mappable VRAM.
pub unsafe extern "system" fn vkAllocateMemory(
    device: VkDevice,
    pAllocateInfo: *const VkMemoryAllocateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pMemory: *mut VkDeviceMemory,
) -> VkResult {
    #[cfg(target_os = "android")]
    let mut import = core::mem::MaybeUninit::uninit();
    #[cfg(target_os = "android")]
    let mut exported = core::ptr::null_mut();
    #[cfg(target_os = "android")]
    let pAllocateInfo = match crate::hardware_buffer::import(device, pAllocateInfo, &mut import, &mut exported) {
        Ok(info) => info,
        Err(result) => return result,
    };
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let mut m = rt::Measure::new(8 + size_of::<frame::vkAllocateMemory>());
    body_vkAllocateMemory(&mut m, &ids, &mut outs, device, pAllocateInfo, pAllocator, pMemory);
    let _fds = rt::fd_lock();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkAllocateMemory, m.size());
    let mut w = rt::Write::new(rec, 8 + size_of::<frame::vkAllocateMemory>());
    body_vkAllocateMemory(&mut w, &ids, &mut outs, device, pAllocateInfo, pAllocator, pMemory);
    let end = ring.publish(true);
    drop(ring);
    rt::wait(end);
    let result = *w.at::<VkResult>(outs[0]);
    let pages = *w.at::<u32>(outs[0] + 4) as usize;
    if pages != 0 {
        let fd = rt::recv_fd();
        let size = pages << 12;
        let shared = sys::Shm::map(fd, size).unwrap_or_else(|error| rt::fail(&format!("map shared memory: {error}")));
        libc::close(fd);
        *rt::MAPPINGS.at(ids[0]) = rt::Mapping { base: shared.base, size };
        core::mem::forget(shared);
    }
    if result != VK_SUCCESS {
        #[cfg(target_os = "android")]
        crate::hardware_buffer::release(exported);
        rt::free_id(ids[0]);
        return result;
    }
    #[cfg(target_os = "android")]
    crate::hardware_buffer::allocated(ids[0], exported);
    *pMemory = ids[0];
    result
}

pub unsafe extern "system" fn vkFreeMemory(device: VkDevice, memory: VkDeviceMemory, pAllocator: *const VkAllocationCallbacks) {
    if memory == 0 {
        return;
    }
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let size = 8 + size_of::<frame::vkFreeMemory>();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkFreeMemory, size);
    let mut w = rt::Write::new(rec, size);
    body_vkFreeMemory(&mut w, &ids, &mut outs, device, memory, pAllocator);
    ring.publish(false);
    drop(ring);
    let mapping = &mut *rt::MAPPINGS.at(memory);
    if !mapping.base.is_null() {
        libc::munmap(mapping.base.cast(), mapping.size);
        *mapping = rt::Mapping { base: core::ptr::null_mut(), size: 0 };
    }
    #[cfg(target_os = "android")]
    crate::hardware_buffer::free(memory);
    rt::free_id(memory);
}

pub unsafe extern "system" fn vkMapMemory(
    _device: VkDevice,
    memory: VkDeviceMemory,
    offset: VkDeviceSize,
    _size: VkDeviceSize,
    _flags: VkMemoryMapFlags,
    ppData: *mut *mut core::ffi::c_void,
) -> VkResult {
    let base = (*rt::MAPPINGS.at(memory)).base;
    if base.is_null() {
        rt::fail("vkMapMemory on memory the host did not share");
    }
    *ppData = base.add(offset as usize).cast();
    VK_SUCCESS
}

pub unsafe extern "system" fn vkUnmapMemory(_device: VkDevice, _memory: VkDeviceMemory) {}

// ---------------------------------------------------------------- images

/// Gralloc images become a DMA-BUF import the host binds at creation; see
/// `android::native_image`. AHardwareBuffer images and images of swapchains
/// with deferred allocation wait for their buffer; see `hardware_buffer`.
pub unsafe extern "system" fn vkCreateImage(
    device: VkDevice,
    pCreateInfo: *const VkImageCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pImage: *mut VkImage,
) -> VkResult {
    let id = rt::new_id();
    *pImage = id;
    #[cfg(target_os = "android")]
    if crate::hardware_buffer::defer(pCreateInfo, id) {
        return VK_SUCCESS;
    }
    let mut native = core::mem::MaybeUninit::uninit();
    send_image(device, crate::android::native_image(pCreateInfo, &mut native), id);
    let _ = pAllocator;
    VK_SUCCESS
}

/// Swapchain images that waited for their gralloc buffer are created on the
/// host here; the other binds are forwarded.
pub unsafe extern "system" fn vkBindImageMemory2(device: VkDevice, bindInfoCount: u32, pBindInfos: *const VkBindImageMemoryInfo) -> VkResult {
    #[cfg(target_os = "android")]
    let kept: Vec<VkBindImageMemoryInfo> = core::slice::from_raw_parts(pBindInfos, bindInfoCount as usize)
        .iter()
        .filter(|bind| !crate::hardware_buffer::bind_native(device, bind))
        .copied()
        .collect();
    #[cfg(target_os = "android")]
    let (bindInfoCount, pBindInfos) = (kept.len() as u32, kept.as_ptr());
    if bindInfoCount == 0 {
        return VK_SUCCESS;
    }
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkBindImageMemory2>();
    let mut m = rt::Measure::new(frame_size);
    body_vkBindImageMemory2(&mut m, &ids, &mut outs, device, bindInfoCount, pBindInfos);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkBindImageMemory2, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkBindImageMemory2(&mut w, &ids, &mut outs, device, bindInfoCount, pBindInfos);
    ring.publish(false);
    VK_SUCCESS
}

/// A view of an AHardwareBuffer image with an external format names none;
/// the host gets the format behind it.
pub unsafe extern "system" fn vkCreateImageView(
    device: VkDevice,
    pCreateInfo: *const VkImageViewCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pView: *mut VkImageView,
) -> VkResult {
    #[cfg(target_os = "android")]
    let named = crate::hardware_buffer::view_format(&*pCreateInfo);
    #[cfg(target_os = "android")]
    let pCreateInfo = named.as_ref().map_or(pCreateInfo, core::ptr::from_ref);
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkCreateImageView>();
    let mut m = rt::Measure::new(frame_size);
    body_vkCreateImageView(&mut m, &ids, &mut outs, device, pCreateInfo, pAllocator, pView);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateImageView, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkCreateImageView(&mut w, &ids, &mut outs, device, pCreateInfo, pAllocator, pView);
    ring.publish(false);
    *pView = ids[0];
    VK_SUCCESS
}

/// A conversion for an external format names it in its chain; the host
/// gets the format behind it.
pub unsafe extern "system" fn vkCreateSamplerYcbcrConversion(
    device: VkDevice,
    pCreateInfo: *const VkSamplerYcbcrConversionCreateInfo,
    pAllocator: *const VkAllocationCallbacks,
    pYcbcrConversion: *mut VkSamplerYcbcrConversion,
) -> VkResult {
    #[cfg(target_os = "android")]
    let named = crate::hardware_buffer::conversion_format(&*pCreateInfo);
    #[cfg(target_os = "android")]
    let pCreateInfo = named.as_ref().map_or(pCreateInfo, core::ptr::from_ref);
    let ids = [rt::new_id()];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkCreateSamplerYcbcrConversion>();
    let mut m = rt::Measure::new(frame_size);
    body_vkCreateSamplerYcbcrConversion(&mut m, &ids, &mut outs, device, pCreateInfo, pAllocator, pYcbcrConversion);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateSamplerYcbcrConversion, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkCreateSamplerYcbcrConversion(&mut w, &ids, &mut outs, device, pCreateInfo, pAllocator, pYcbcrConversion);
    ring.publish(false);
    *pYcbcrConversion = ids[0];
    VK_SUCCESS
}

pub unsafe fn send_image(device: VkDevice, info: *const VkImageCreateInfo, id: u64) {
    let ids = [id];
    let mut outs = [0i64; 1];
    let frame_size = 8 + size_of::<frame::vkCreateImage>();
    let mut m = rt::Measure::new(frame_size);
    body_vkCreateImage(&mut m, &ids, &mut outs, device, info, core::ptr::null(), core::ptr::null_mut());
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkCreateImage, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkCreateImage(&mut w, &ids, &mut outs, device, info, core::ptr::null(), core::ptr::null_mut());
    ring.publish(false);
}

pub unsafe extern "system" fn vkDestroyImage(device: VkDevice, image: VkImage, pAllocator: *const VkAllocationCallbacks) {
    if image == 0 {
        return;
    }
    #[cfg(target_os = "android")]
    if crate::hardware_buffer::forget(image) {
        rt::free_id(image);
        return;
    }
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 1];
    let record_size = 8 + size_of::<frame::vkDestroyImage>();
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkDestroyImage, record_size);
    let mut w = rt::Write::new(rec, record_size);
    body_vkDestroyImage(&mut w, &ids, &mut outs, device, image, pAllocator);
    ring.publish(false);
    drop(ring);
    rt::free_id(image);
}

/// AHardwareBuffer queries become the host's DMA-BUF queries.
pub unsafe extern "system" fn vkGetPhysicalDeviceImageFormatProperties2(
    physicalDevice: VkPhysicalDevice,
    pImageFormatInfo: *const VkPhysicalDeviceImageFormatInfo2,
    pImageFormatProperties: *mut VkImageFormatProperties2,
) -> VkResult {
    #[cfg(target_os = "android")]
    let mut query = core::mem::MaybeUninit::uninit();
    #[cfg(target_os = "android")]
    let asked = pImageFormatInfo;
    #[cfg(target_os = "android")]
    let hardware_buffer = crate::hardware_buffer::format_query(pImageFormatInfo, &mut query);
    #[cfg(target_os = "android")]
    let pImageFormatInfo = hardware_buffer.unwrap_or(pImageFormatInfo);
    let ids: [u64; 0] = [];
    let mut outs = [0i64; 2];
    let frame_size = 8 + size_of::<frame::vkGetPhysicalDeviceImageFormatProperties2>();
    let mut m = rt::Measure::new(frame_size);
    body_vkGetPhysicalDeviceImageFormatProperties2(&mut m, &ids, &mut outs, physicalDevice, pImageFormatInfo, pImageFormatProperties);
    let mut ring = rt::ring();
    let rec = ring.reserve(op::vkGetPhysicalDeviceImageFormatProperties2, m.size());
    let mut w = rt::Write::new(rec, frame_size);
    body_vkGetPhysicalDeviceImageFormatProperties2(&mut w, &ids, &mut outs, physicalDevice, pImageFormatInfo, pImageFormatProperties);
    let end = ring.publish(true);
    drop(ring);
    rt::wait(end);
    let reply = w.at::<u8>(outs[1]);
    core::ptr::copy_nonoverlapping(reply.add(16), pImageFormatProperties.cast::<u8>().add(16), size_of::<VkImageFormatProperties2>() - 16);
    copy_back_chain((*pImageFormatProperties).pNext, reply.add(rt::align(size_of::<VkImageFormatProperties2>())));
    let result = *w.at::<VkResult>(outs[0]);
    #[cfg(target_os = "android")]
    if hardware_buffer.is_some() && result == VK_SUCCESS {
        crate::hardware_buffer::format_answer(asked, pImageFormatProperties);
    }
    result
}

// ---------------------------------------------------------------- templates

const VK_DESCRIPTOR_TYPE_INLINE_UNIFORM_BLOCK: VkDescriptorType = 1000138000;

/// A template is its entries; each update becomes one `vkUpdateDescriptorSets`.
pub unsafe extern "system" fn vkCreateDescriptorUpdateTemplate(
    _device: VkDevice,
    pCreateInfo: *const VkDescriptorUpdateTemplateCreateInfo,
    _pAllocator: *const VkAllocationCallbacks,
    pDescriptorUpdateTemplate: *mut VkDescriptorUpdateTemplate,
) -> VkResult {
    let info = &*pCreateInfo;
    if info.templateType != 0 {
        rt::fail("push descriptor templates are not forwarded");
    }
    let entries = core::slice::from_raw_parts(info.pDescriptorUpdateEntries, info.descriptorUpdateEntryCount as usize).to_vec();
    *pDescriptorUpdateTemplate = Box::into_raw(Box::new(entries)) as VkDescriptorUpdateTemplate;
    VK_SUCCESS
}

pub unsafe extern "system" fn vkDestroyDescriptorUpdateTemplate(
    _device: VkDevice,
    descriptorUpdateTemplate: VkDescriptorUpdateTemplate,
    _pAllocator: *const VkAllocationCallbacks,
) {
    if descriptorUpdateTemplate != 0 {
        drop(Box::from_raw(descriptorUpdateTemplate as *mut Vec<VkDescriptorUpdateTemplateEntry>));
    }
}

pub unsafe extern "system" fn vkUpdateDescriptorSetWithTemplate(
    device: VkDevice,
    descriptorSet: VkDescriptorSet,
    descriptorUpdateTemplate: VkDescriptorUpdateTemplate,
    pData: *const core::ffi::c_void,
) {
    let entries = &*(descriptorUpdateTemplate as *const Vec<VkDescriptorUpdateTemplateEntry>);
    let data = pData.cast::<u8>();
    // Strided arrays are packed here; pointers into these stay put.
    let mut packed: Vec<Vec<u8>> = Vec::new();
    let mut blocks: Vec<VkWriteDescriptorSetInlineUniformBlock> = Vec::with_capacity(entries.len());
    let mut writes = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut write = VkWriteDescriptorSet {
            sType: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            pNext: core::ptr::null(),
            dstSet: descriptorSet,
            dstBinding: entry.dstBinding,
            dstArrayElement: entry.dstArrayElement,
            descriptorCount: entry.descriptorCount,
            descriptorType: entry.descriptorType,
            pImageInfo: core::ptr::null(),
            pBufferInfo: core::ptr::null(),
            pTexelBufferView: core::ptr::null(),
        };
        let first = data.add(entry.offset);
        let mut array = |size: usize| -> *const u8 {
            if entry.stride == size || entry.descriptorCount <= 1 {
                return first;
            }
            let mut copy = Vec::with_capacity(size * entry.descriptorCount as usize);
            for i in 0..entry.descriptorCount as usize {
                copy.extend_from_slice(core::slice::from_raw_parts(first.add(i * entry.stride), size));
            }
            packed.push(copy);
            packed.last().map_or(core::ptr::null(), |copy| copy.as_ptr())
        };
        match entry.descriptorType {
            0..=3 | 10 => write.pImageInfo = array(size_of::<VkDescriptorImageInfo>()).cast(),
            4 | 5 => write.pTexelBufferView = array(size_of::<VkBufferView>()).cast(),
            6..=9 => write.pBufferInfo = array(size_of::<VkDescriptorBufferInfo>()).cast(),
            VK_DESCRIPTOR_TYPE_INLINE_UNIFORM_BLOCK => {
                blocks.push(VkWriteDescriptorSetInlineUniformBlock {
                    sType: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_INLINE_UNIFORM_BLOCK,
                    pNext: core::ptr::null(),
                    dataSize: entry.descriptorCount,
                    pData: first.cast(),
                });
                write.pNext = (&raw const blocks[blocks.len() - 1]).cast();
            }
            other => rt::fail(&format!("descriptor template entry of type {other}")),
        }
        writes.push(write);
    }
    vkUpdateDescriptorSets(device, writes.len() as u32, writes.as_ptr(), 0, core::ptr::null());
}

// ---------------------------------------------------------------- private data

static PRIVATE_DATA: Mutex<Option<HashMap<(VkPrivateDataSlot, u64), u64>>> = Mutex::new(None);

pub unsafe extern "system" fn vkCreatePrivateDataSlot(
    _device: VkDevice,
    _pCreateInfo: *const VkPrivateDataSlotCreateInfo,
    _pAllocator: *const VkAllocationCallbacks,
    pPrivateDataSlot: *mut VkPrivateDataSlot,
) -> VkResult {
    *pPrivateDataSlot = rt::new_id();
    VK_SUCCESS
}

pub unsafe extern "system" fn vkDestroyPrivateDataSlot(_device: VkDevice, privateDataSlot: VkPrivateDataSlot, _pAllocator: *const VkAllocationCallbacks) {
    if privateDataSlot == 0 {
        return;
    }
    let mut data = PRIVATE_DATA.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(data) = data.as_mut() {
        data.retain(|&(slot, _), _| slot != privateDataSlot);
    }
    rt::free_id(privateDataSlot);
}

pub unsafe extern "system" fn vkSetPrivateData(
    _device: VkDevice,
    _objectType: VkObjectType,
    objectHandle: u64,
    privateDataSlot: VkPrivateDataSlot,
    data: u64,
) -> VkResult {
    let mut all = PRIVATE_DATA.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    all.get_or_insert_with(HashMap::new).insert((privateDataSlot, objectHandle), data);
    VK_SUCCESS
}

pub unsafe extern "system" fn vkGetPrivateData(
    _device: VkDevice,
    _objectType: VkObjectType,
    objectHandle: u64,
    privateDataSlot: VkPrivateDataSlot,
    pData: *mut u64,
) {
    let all = PRIVATE_DATA.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *pData = all.as_ref().and_then(|all| all.get(&(privateDataSlot, objectHandle)).copied()).unwrap_or(0);
}
