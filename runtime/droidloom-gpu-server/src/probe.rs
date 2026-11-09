//! Facts about the host GPUs that need a device to learn, gathered once
//! when the listener starts, in a child process so the listener itself
//! never loads a driver before it forks.

use core::ffi::{c_char, c_void, CStr};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use droidloom_gpu_protocol::generated::vk::*;
use droidloom_gpu_protocol::sys::Shm;

use crate::rt;

const VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT: i32 = 0x80;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: i32 = 0x200;
const DEVICE_LOCAL: u32 = 0x1;
const HOST_VISIBLE: u32 = 0x2;

type GetHostPointerProperties =
    unsafe extern "system" fn(VkDevice, i32, *const c_void, *mut VkMemoryHostPointerPropertiesEXT) -> VkResult;
type MapMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory, u64, u64, u32, *mut *mut c_void) -> VkResult;

/// The memory types whose mappings a client can share with the GPU.
#[derive(Clone, Copy)]
pub struct Shareable {
    /// System memory: types a memfd page can be imported into as a host
    /// allocation.
    pub memfd: u32,
    /// Mappable VRAM: types whose DMA-BUF export maps in another process.
    pub vram: u32,
}

/// The name of the GPU at PCI address `gpu`, and the memory types it can
/// share with a client.
pub fn host_gpu(gpu: &str) -> (String, Shareable) {
    let mut pipe = [0; 2];
    // SAFETY: a fresh pipe; the child only probes, writes and exits.
    unsafe {
        if libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            rt::fail("create the probe pipe");
        }
        match libc::fork() {
            -1 => rt::fail("fork the probe"),
            0 => {
                libc::close(pipe[0]);
                let (name, shareable) = probe(gpu);
                let mut out = std::fs::File::from_raw_fd(pipe[1]);
                let _ = out.write_all(&[&shareable.memfd.to_le_bytes(), &shareable.vram.to_le_bytes(), name.as_bytes()].concat());
                libc::_exit(0);
            }
            child => {
                libc::close(pipe[1]);
                let mut bytes = Vec::new();
                let _ = std::fs::File::from_raw_fd(pipe[0]).read_to_end(&mut bytes);
                let mut status = 0;
                libc::waitpid(child, &raw mut status, 0);
                if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                    rt::fail("the probe failed");
                }
                let (masks, name) = bytes.split_at(8);
                let mask = |at: usize| u32::from_le_bytes([masks[at], masks[at + 1], masks[at + 2], masks[at + 3]]);
                (String::from_utf8_lossy(name).into_owned(), Shareable { memfd: mask(0), vram: mask(4) })
            }
        }
    }
}

unsafe fn probe(gpu: &str) -> (String, Shareable) {
    unsafe {
        let gipa = crate::load_vulkan();
        let lookup = |instance: VkInstance, name: &CStr| {
            gipa(instance, name.as_ptr()).unwrap_or_else(|| rt::fail(&format!("the host driver has no {}", name.to_string_lossy())))
        };
        let application = VkApplicationInfo {
            sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: core::ptr::null(),
            pApplicationName: c"droidloom-gpu-probe".as_ptr(),
            applicationVersion: 0,
            pEngineName: core::ptr::null(),
            engineVersion: 0,
            apiVersion: (1 << 22) | (3 << 12),
        };
        let info = VkInstanceCreateInfo {
            sType: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
            pNext: core::ptr::null(),
            flags: 0,
            pApplicationInfo: &raw const application,
            enabledLayerCount: 0,
            ppEnabledLayerNames: core::ptr::null(),
            enabledExtensionCount: 0,
            ppEnabledExtensionNames: core::ptr::null(),
        };
        let mut instance: VkInstance = 0;
        let create: PFN_vkCreateInstance = core::mem::transmute(lookup(0, c"vkCreateInstance"));
        if create(&raw const info, core::ptr::null(), &raw mut instance) != VK_SUCCESS {
            rt::fail("create the probe instance");
        }
        let enumerate: PFN_vkEnumeratePhysicalDevices = core::mem::transmute(lookup(instance, c"vkEnumeratePhysicalDevices"));
        let properties: PFN_vkGetPhysicalDeviceProperties = core::mem::transmute(lookup(instance, c"vkGetPhysicalDeviceProperties"));
        let properties2: PFN_vkGetPhysicalDeviceProperties2 = core::mem::transmute(lookup(instance, c"vkGetPhysicalDeviceProperties2"));
        let memory_properties: PFN_vkGetPhysicalDeviceMemoryProperties = core::mem::transmute(lookup(instance, c"vkGetPhysicalDeviceMemoryProperties"));
        let create_device: PFN_vkCreateDevice = core::mem::transmute(lookup(instance, c"vkCreateDevice"));
        let gdpa: PFN_vkGetDeviceProcAddr = core::mem::transmute(lookup(instance, c"vkGetDeviceProcAddr"));
        let mut count = 0;
        enumerate(instance, &raw mut count, core::ptr::null_mut());
        let mut all = vec![0usize; count as usize];
        enumerate(instance, &raw mut count, all.as_mut_ptr());
        let page = Shm::create(4096).unwrap_or_else(|error| rt::fail(&format!("create a probe page: {error}"))).0;
        for physical in all {
            if crate::gpu::pci_address(properties2, physical) != gpu {
                continue;
            }
            let mut props = core::mem::zeroed::<VkPhysicalDeviceProperties>();
            properties(physical, &raw mut props);
            let name = CStr::from_ptr(props.deviceName.as_ptr()).to_string_lossy().into_owned();
            let priority = 1.0f32;
            let queue = VkDeviceQueueCreateInfo {
                sType: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                queueFamilyIndex: 0,
                queueCount: 1,
                pQueuePriorities: &raw const priority,
            };
            let extensions: [*const c_char; 3] =
                [c"VK_EXT_external_memory_host".as_ptr(), c"VK_KHR_external_memory_fd".as_ptr(), c"VK_EXT_external_memory_dma_buf".as_ptr()];
            let info = VkDeviceCreateInfo {
                sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                queueCreateInfoCount: 1,
                pQueueCreateInfos: &raw const queue,
                enabledLayerCount: 0,
                ppEnabledLayerNames: core::ptr::null(),
                enabledExtensionCount: extensions.len() as u32,
                ppEnabledExtensionNames: extensions.as_ptr(),
                pEnabledFeatures: core::ptr::null(),
            };
            let mut device: VkDevice = 0;
            let created = create_device(physical, &raw const info, core::ptr::null(), &raw mut device);
            if created != VK_SUCCESS {
                rt::fail(&format!("create the probe device: {created}"));
            }
            let get: GetHostPointerProperties =
                core::mem::transmute(gdpa(device, c"vkGetMemoryHostPointerPropertiesEXT".as_ptr()).unwrap_or_else(|| rt::fail("no vkGetMemoryHostPointerPropertiesEXT")));
            let mut result = VkMemoryHostPointerPropertiesEXT {
                sType: VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT,
                pNext: core::ptr::null_mut(),
                memoryTypeBits: 0,
            };
            let queried = get(device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, page.base.cast(), &raw mut result);
            if queried != VK_SUCCESS || result.memoryTypeBits == 0 {
                rt::fail(&format!("import a memfd page: {queried}, memory types {:x}", result.memoryTypeBits));
            }
            let mut memory = core::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
            memory_properties(physical, &raw mut memory);
            let vram = mappable_vram(device, gdpa, &memory);
            let destroy: PFN_vkDestroyDevice = core::mem::transmute(gdpa(device, c"vkDestroyDevice".as_ptr()));
            destroy(device, core::ptr::null());
            return (name, Shareable { memfd: result.memoryTypeBits, vram });
        }
        rt::fail(&format!("the host's Vulkan has no GPU at {gpu}; is its Vulkan driver installed?"))
    }
}

/// The mappable VRAM types a client can map through a DMA-BUF: a word
/// written through the descriptor's mapping must read back through the
/// driver's own. A driver that refuses a step keeps the type unmappable for
/// Android, which then maps system memory only.
unsafe fn mappable_vram(device: VkDevice, gdpa: PFN_vkGetDeviceProcAddr, memory: &VkPhysicalDeviceMemoryProperties) -> u32 {
    const SIZE: usize = 1 << 16;
    unsafe {
        let lookup = |name: &CStr| gdpa(device, name.as_ptr()).unwrap_or_else(|| rt::fail(&format!("the host driver has no {}", name.to_string_lossy())));
        let allocate: PFN_vkAllocateMemory = core::mem::transmute(lookup(c"vkAllocateMemory"));
        let free: PFN_vkFreeMemory = core::mem::transmute(lookup(c"vkFreeMemory"));
        let map: MapMemory = core::mem::transmute(lookup(c"vkMapMemory"));
        let get_fd: PFN_vkGetMemoryFdKHR = core::mem::transmute(lookup(c"vkGetMemoryFdKHR"));
        let mut mask = 0;
        for index in 0..memory.memoryTypeCount {
            if memory.memoryTypes[index as usize].propertyFlags & (DEVICE_LOCAL | HOST_VISIBLE) != DEVICE_LOCAL | HOST_VISIBLE {
                continue;
            }
            let export = VkExportMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                pNext: core::ptr::null(),
                handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT as u32,
            };
            let info = VkMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                pNext: (&raw const export).cast(),
                allocationSize: SIZE as u64,
                memoryTypeIndex: index,
            };
            let mut handle: VkDeviceMemory = 0;
            if allocate(device, &raw const info, core::ptr::null(), &raw mut handle) != VK_SUCCESS {
                continue;
            }
            let get = VkMemoryGetFdInfoKHR {
                sType: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                pNext: core::ptr::null(),
                memory: handle,
                handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            };
            let mut fd = -1;
            let mut own = core::ptr::null_mut();
            if get_fd(device, &raw const get, &raw mut fd) == VK_SUCCESS && map(device, handle, 0, SIZE as u64, 0, &raw mut own) == VK_SUCCESS {
                if let Ok(shared) = Shm::map(fd, SIZE) {
                    let word = 0x6d61_7076 ^ index;
                    shared.base.cast::<u32>().write_volatile(word);
                    if own.cast::<u32>().read_volatile() == word {
                        mask |= 1 << index;
                    }
                }
            }
            if fd >= 0 {
                libc::close(fd);
            }
            free(device, handle, core::ptr::null());
        }
        mask
    }
}
