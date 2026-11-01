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

type GetHostPointerProperties =
    unsafe extern "system" fn(VkDevice, i32, *const c_void, *mut VkMemoryHostPointerPropertiesEXT) -> VkResult;

/// The name of the GPU at PCI address `gpu`, and the memory types a memfd
/// page can be imported into on it as a host allocation.
pub fn host_gpu(gpu: &str) -> (String, u32) {
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
                let (name, mask) = probe(gpu);
                let mut out = std::fs::File::from_raw_fd(pipe[1]);
                let _ = out.write_all(&[&mask.to_le_bytes(), name.as_bytes()].concat());
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
                let (mask, name) = bytes.split_at(4);
                (String::from_utf8_lossy(name).into_owned(), u32::from_le_bytes([mask[0], mask[1], mask[2], mask[3]]))
            }
        }
    }
}

unsafe fn probe(gpu: &str) -> (String, u32) {
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
            let extensions: [*const c_char; 1] = [c"VK_EXT_external_memory_host".as_ptr()];
            let info = VkDeviceCreateInfo {
                sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                queueCreateInfoCount: 1,
                pQueueCreateInfos: &raw const queue,
                enabledLayerCount: 0,
                ppEnabledLayerNames: core::ptr::null(),
                enabledExtensionCount: 1,
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
            let destroy: PFN_vkDestroyDevice = core::mem::transmute(gdpa(device, c"vkDestroyDevice".as_ptr()));
            destroy(device, core::ptr::null());
            return (name, result.memoryTypeBits);
        }
        rt::fail(&format!("the host's Vulkan has no GPU at {gpu}; is its Vulkan driver installed?"))
    }
}
