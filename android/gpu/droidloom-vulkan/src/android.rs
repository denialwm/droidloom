//! `VK_ANDROID_native_buffer`: the driver half of Android's WSI, and the
//! `hwvulkan` module Android's loader opens.
//!
//! The loader owns swapchains. It hands the driver gralloc buffers to wrap
//! as images, a sync file to wait on before rendering into one, and asks
//! for a sync file that signals when rendering is done. Gralloc buffers are
//! DMA-BUFs the host GPU imports directly: LINEAR in system memory, or in
//! VRAM in one of the host GPU's own modifiers.

#![allow(non_snake_case)]

use core::ffi::{c_int, c_void};

use droidloom_gpu_protocol::generated::vk::*;

use crate::generated::vkImportSemaphoreFdKHR;
use crate::rt;

const VK_STRUCTURE_TYPE_NATIVE_BUFFER_ANDROID: VkStructureType = 1000010000;
const VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT: VkImageTiling = 1000158000;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: u32 = 0x200;
const VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT: u32 = 0x10;
const VK_SEMAPHORE_IMPORT_TEMPORARY_BIT: u32 = 1;
const VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT: u32 = 0x80;

/// Gralloc 1 usage bits (`hardware/gralloc1.h`).
const GRALLOC1_PRODUCER_USAGE_CPU_READ: u64 = 1 << 1;
const GRALLOC1_CONSUMER_USAGE_GPU_TEXTURE: u64 = 1 << 8;
const GRALLOC1_PRODUCER_USAGE_GPU_RENDER_TARGET: u64 = 1 << 9;

#[repr(C)]
struct VkNativeBufferUsage2ANDROID {
    consumer: u64,
    producer: u64,
}

#[repr(C)]
struct VkNativeBufferANDROID {
    sType: VkStructureType,
    pNext: *const c_void,
    /// `buffer_handle_t`: a `native_handle_t`.
    handle: *const NativeHandle,
    stride: c_int,
    format: c_int,
    usage: c_int,
    usage2: VkNativeBufferUsage2ANDROID,
}

/// `native_handle_t`: descriptors, then integers.
#[repr(C)]
pub struct NativeHandle {
    version: c_int,
    fds: c_int,
    ints: c_int,
    data: [c_int; 0],
}

/// Planes a gralloc buffer has at most: three, for planar YUV.
pub const MAX_PLANES: usize = 3;

/// A gralloc buffer as the host sees it: one DMA-BUF, with a plane for RGB
/// and two or three for YUV. The descriptor stays the handle's.
pub struct Gralloc {
    pub fd: c_int,
    /// The DRM fourcc gralloc laid it out in, which AHardwareBuffer import reads.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fourcc: u32,
    pub modifier: u64,
    /// Where each plane lies in the DMA-BUF. Every plane descriptor is the
    /// same DMA-BUF.
    pub planes: [VkSubresourceLayout; MAX_PLANES],
    pub plane_count: u32,
}

pub unsafe fn gralloc(handle: *const NativeHandle) -> Gralloc {
    unsafe {
        let handle = &*handle;
        let data = handle.data.as_ptr();
        let ints = core::slice::from_raw_parts(data.add(handle.fds as usize), handle.ints as usize);
        let buffer = droidloom_minigbm::parse_minigbm_handle(handle.fds as usize, ints)
            .unwrap_or_else(|error| rt::fail(&format!("gralloc buffer: {error}")));
        let count = buffer.buffer.planes.len();
        if count == 0 || count > MAX_PLANES {
            rt::fail(&format!("gralloc buffer with {count} planes"));
        }
        let mut planes = [VkSubresourceLayout { offset: 0, size: 0, rowPitch: 0, arrayPitch: 0, depthPitch: 0 }; MAX_PLANES];
        for (layout, plane) in planes.iter_mut().zip(&buffer.buffer.planes) {
            layout.offset = u64::from(plane.offset);
            layout.rowPitch = u64::from(plane.stride);
        }
        Gralloc {
            fd: *data,
            fourcc: buffer.buffer.format.fourcc,
            modifier: buffer.buffer.format.modifier,
            planes,
            plane_count: count as u32,
        }
    }
}

/// An image's creation rewritten as one over a gralloc buffer's DMA-BUF,
/// kept in place while the record is written.
pub struct DmaBufImage {
    info: VkImageCreateInfo,
    external: VkExternalMemoryImageCreateInfo,
    modifier: VkImageDrmFormatModifierExplicitCreateInfoEXT,
    planes: [VkSubresourceLayout; MAX_PLANES],
    import: VkImportMemoryFdInfoKHR,
}

impl DmaBufImage {
    /// `rest` follows the rewritten structures. With `bind`, the chain also
    /// carries the buffer, which the host then binds at creation.
    pub unsafe fn build<'a>(
        out: &'a mut core::mem::MaybeUninit<Self>,
        info: &VkImageCreateInfo,
        rest: *const c_void,
        buffer: &Gralloc,
        bind: bool,
    ) -> &'a VkImageCreateInfo {
        let fd = if bind { duplicate(buffer.fd) } else { -1 };
        let image = out.write(DmaBufImage {
            info: *info,
            external: VkExternalMemoryImageCreateInfo {
                sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
                pNext: core::ptr::null(),
                handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
            },
            modifier: VkImageDrmFormatModifierExplicitCreateInfoEXT {
                sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
                pNext: rest,
                drmFormatModifier: buffer.modifier,
                drmFormatModifierPlaneCount: buffer.plane_count,
                pPlaneLayouts: core::ptr::null(),
            },
            planes: buffer.planes,
            // Not valid in an image chain: it tells the host which buffer to bind.
            import: VkImportMemoryFdInfoKHR {
                sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                pNext: rest,
                handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT as i32,
                fd,
            },
        });
        image.modifier.pPlaneLayouts = image.planes.as_ptr();
        if bind {
            image.modifier.pNext = (&raw const image.import).cast();
        }
        image.external.pNext = (&raw const image.modifier).cast();
        image.info.pNext = (&raw const image.external).cast();
        image.info.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
        // Linear images are no input attachments on NVIDIA. ANGLE asks for
        // one on every image it may read through framebuffer fetch.
        if buffer.modifier == 0 {
            image.info.usage &= !VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT;
        }
        &image.info
    }
}

pub fn duplicate(fd: c_int) -> c_int {
    let copy = unsafe { libc::dup(fd) };
    if copy < 0 {
        rt::fail("duplicate a gralloc descriptor");
    }
    copy
}

/// The image to create: the application's, or for a gralloc buffer the
/// same image over its DMA-BUF, which the host binds at creation.
pub unsafe fn native_image(info: *const VkImageCreateInfo, out: &mut core::mem::MaybeUninit<DmaBufImage>) -> *const VkImageCreateInfo {
    unsafe {
        match native_buffer((*info).pNext) {
            Some(handle) => DmaBufImage::build(out, &*info, (*info).pNext, &gralloc(handle), true),
            None => info,
        }
    }
}

/// The gralloc buffer a `VkNativeBufferANDROID` in `chain` names.
pub unsafe fn native_buffer(chain: *const c_void) -> Option<*const NativeHandle> {
    unsafe {
        let mut next = chain.cast::<rt::Base>();
        while !next.is_null() && (*next).sType != VK_STRUCTURE_TYPE_NATIVE_BUFFER_ANDROID {
            next = (*next).pNext.cast();
        }
        (!next.is_null()).then(|| (*next.cast::<VkNativeBufferANDROID>()).handle)
    }
}

/// Render target and texture, as Mesa's drivers ask; CPU read keeps the
/// allocator to linear system memory, which a GPU other than the
/// allocator's can import.
pub unsafe extern "system" fn vkGetSwapchainGrallocUsage2ANDROID(
    _device: VkDevice,
    _format: VkFormat,
    _imageUsage: VkImageUsageFlags,
    _swapchainImageUsage: u32,
    grallocConsumerUsage: *mut u64,
    grallocProducerUsage: *mut u64,
) -> VkResult {
    unsafe {
        *grallocConsumerUsage = GRALLOC1_CONSUMER_USAGE_GPU_TEXTURE;
        *grallocProducerUsage = GRALLOC1_PRODUCER_USAGE_GPU_RENDER_TARGET | GRALLOC1_PRODUCER_USAGE_CPU_READ;
    }
    VK_SUCCESS
}

/// The image may be written once `nativeFenceFd` signals (-1: now). The
/// semaphore takes the sync file on the host, the fence a copy on the
/// client. Takes the descriptor.
pub unsafe extern "system" fn vkAcquireImageANDROID(
    device: VkDevice,
    _image: VkImage,
    nativeFenceFd: c_int,
    semaphore: VkSemaphore,
    fence: VkFence,
) -> VkResult {
    unsafe {
        if fence != 0 {
            crate::manual::signal_fence(fence, if nativeFenceFd >= 0 { libc::dup(nativeFenceFd) } else { -1 });
        }
        if semaphore == 0 {
            if nativeFenceFd >= 0 {
                libc::close(nativeFenceFd);
            }
            return VK_SUCCESS;
        }
        // SYNC_FD -1 imports as already signaled.
        let import = VkImportSemaphoreFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
            pNext: core::ptr::null(),
            semaphore,
            flags: VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
            handleType: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT as i32,
            fd: nativeFenceFd,
        };
        vkImportSemaphoreFdKHR(device, &raw const import)
    }
}

/// Device commands of the Android extensions, which the generated table
/// cannot list: the registry leaves them disabled.
pub fn proc_addr(name: &[u8]) -> Option<*const c_void> {
    Some(match name {
        b"vkGetSwapchainGrallocUsage2ANDROID" => vkGetSwapchainGrallocUsage2ANDROID as *const c_void,
        b"vkAcquireImageANDROID" => vkAcquireImageANDROID as *const c_void,
        // The host signals the release itself, after updating the buffer.
        b"vkQueueSignalReleaseImageANDROID" => crate::generated::vkQueueSignalReleaseImageANDROID as *const c_void,
        #[cfg(target_os = "android")]
        _ => return crate::hardware_buffer::proc_addr(name),
        #[cfg(not(target_os = "android"))]
        _ => return None,
    })
}

// ---------------------------------------------------------------- hwvulkan

#[cfg(target_os = "android")]
mod hal {
    use core::ffi::{c_char, c_int, c_void, CStr};

    const fn tag(name: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*name)
    }

    #[repr(C)]
    pub struct HwModule {
        tag: u32,
        module_api_version: u16,
        hal_api_version: u16,
        id: *const c_char,
        name: *const c_char,
        author: *const c_char,
        methods: *const HwModuleMethods,
        /// Written by the loader.
        dso: *mut c_void,
        reserved: [u64; 32 - 7],
    }

    #[repr(C)]
    struct HwModuleMethods {
        open: unsafe extern "C" fn(*const HwModule, *const c_char, *mut *mut HwDevice) -> c_int,
    }

    #[repr(C)]
    struct HwDevice {
        tag: u32,
        version: u32,
        module: *const HwModule,
        reserved: [u64; 12],
        close: unsafe extern "C" fn(*mut HwDevice) -> c_int,
    }

    /// `hwvulkan_device_t`.
    #[repr(C)]
    struct VulkanDevice {
        common: HwDevice,
        enumerate_instance_extension_properties: *const c_void,
        create_instance: *const c_void,
        get_instance_proc_addr: *const c_void,
    }

    // SAFETY: immutable after initialization except `dso`, which only the
    // loader writes, once, before any other use.
    unsafe impl Sync for HwModule {}
    unsafe impl Sync for HwModuleMethods {}

    static METHODS: HwModuleMethods = HwModuleMethods { open };

    /// `HAL_MODULE_INFO_SYM`, found by the loader with dlsym.
    #[unsafe(no_mangle)]
    pub static mut HMI: HwModule = HwModule {
        tag: tag(b"HWMT"),
        // HWVULKAN_MODULE_API_VERSION_0_1 and HARDWARE_HAL_API_VERSION.
        module_api_version: 0x0001,
        hal_api_version: 0x0100,
        id: c"vulkan".as_ptr(),
        name: c"Droidloom GPU forwarding".as_ptr(),
        author: c"Droidloom".as_ptr(),
        methods: &raw const METHODS,
        dso: core::ptr::null_mut(),
        reserved: [0; 32 - 7],
    };

    unsafe extern "C" fn open(module: *const HwModule, id: *const c_char, device: *mut *mut HwDevice) -> c_int {
        unsafe {
            if CStr::from_ptr(id) != c"vk0" {
                return -libc::ENOENT;
            }
            let vulkan = Box::new(VulkanDevice {
                common: HwDevice {
                    tag: tag(b"HWDT"),
                    // HWVULKAN_DEVICE_API_VERSION_0_1.
                    version: 0x0001_0000,
                    module,
                    reserved: [0; 12],
                    close,
                },
                enumerate_instance_extension_properties: crate::manual::vkEnumerateInstanceExtensionProperties as *const c_void,
                create_instance: crate::manual::vkCreateInstance as *const c_void,
                get_instance_proc_addr: crate::manual::vkGetInstanceProcAddr as *const c_void,
            });
            *device = Box::into_raw(vulkan).cast();
            0
        }
    }

    unsafe extern "C" fn close(device: *mut HwDevice) -> c_int {
        drop(unsafe { Box::from_raw(device.cast::<VulkanDevice>()) });
        0
    }
}
