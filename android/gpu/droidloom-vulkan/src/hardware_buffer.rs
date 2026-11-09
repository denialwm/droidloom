//! `VK_ANDROID_external_memory_android_hardware_buffer`. An AHardwareBuffer
//! is a gralloc buffer, so the host only ever sees the DMA-BUF inside. Its
//! layout is known once the buffer is imported, so an image made for one
//! reaches the host then, just before its memory. Export allocates the
//! buffer from gralloc and imports it the same way. The layout is LINEAR in
//! system memory, or for an RGB buffer nobody maps one of the host GPU's own
//! modifiers in VRAM.
//!
//! YUV buffers, which video decoders fill, are images of Vulkan's
//! multi-planar formats, sampled through a YCbCr conversion. Their external
//! format is that Vulkan format, so an image, view or conversion that names
//! it reaches the host with the format behind it.

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::mem::MaybeUninit;

use droidloom_gpu_protocol::generated::vk::*;

use crate::android::{self, DmaBufImage, Gralloc, NativeHandle};
use crate::rt;

const HANDLE_TYPE: u32 = 0x400;
const DMA_BUF: u32 = 0x200;
const VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_USAGE_ANDROID: VkStructureType = 1000129000;
const VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_ANDROID: VkStructureType = 1000129002;
const VK_STRUCTURE_TYPE_IMPORT_ANDROID_HARDWARE_BUFFER_INFO_ANDROID: VkStructureType = 1000129003;
const VK_STRUCTURE_TYPE_EXTERNAL_FORMAT_ANDROID: VkStructureType = 1000129005;
const VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_2_ANDROID: VkStructureType = 1000129006;
const VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT: VkImageTiling = 1000158000;
const VK_ERROR_INVALID_EXTERNAL_HANDLE: VkResult = -1000072003;
const VK_ERROR_OUT_OF_DEVICE_MEMORY: VkResult = -2;
const VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT: u32 = 0x1;
const VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT: u32 = 0x2;
const VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT: u32 = 0x4;
const VK_SHARING_MODE_CONCURRENT: VkSharingMode = 1;
const VK_FORMAT_UNDEFINED: VkFormat = 0;
const VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_601: i32 = 3;
const VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_2020: i32 = 4;
const VK_SAMPLER_YCBCR_RANGE_ITU_NARROW: i32 = 1;
const VK_CHROMA_LOCATION_MIDPOINT: i32 = 1;
const VK_IMAGE_USAGE_SAMPLED_BIT: u32 = 0x4;
const VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT: u32 = 0x10;
const VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT: u32 = 0x20;
const VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT: u32 = 0x80;
const VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT: u32 = 0x10;
const VK_IMAGE_CREATE_PROTECTED_BIT: u32 = 0x800;

/// `AHARDWAREBUFFER_USAGE_GPU_*`.
const GPU_SAMPLED_IMAGE: u64 = 1 << 8;
const GPU_FRAMEBUFFER: u64 = 1 << 9;
const PROTECTED_CONTENT: u64 = 1 << 14;
const GPU_CUBE_MAP: u64 = 1 << 25;
const GPU_MIPMAP_COMPLETE: u64 = 1 << 26;

/// `AHardwareBuffer_Desc`.
#[repr(C)]
struct Desc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}

#[link(name = "nativewindow")]
unsafe extern "C" {
    fn AHardwareBuffer_allocate(desc: *const Desc, buffer: *mut *mut c_void) -> i32;
    fn AHardwareBuffer_acquire(buffer: *mut c_void);
    fn AHardwareBuffer_release(buffer: *mut c_void);
    fn AHardwareBuffer_getNativeHandle(buffer: *const c_void) -> *const NativeHandle;
}

#[repr(C)]
struct VkAndroidHardwareBufferPropertiesANDROID {
    sType: VkStructureType,
    pNext: *mut c_void,
    allocationSize: VkDeviceSize,
    memoryTypeBits: u32,
}

/// Both versions: `formatFeatures` is 32 bits in the first and 64 in the
/// second, and the fields after it move.
#[repr(C)]
struct FormatProperties<F> {
    sType: VkStructureType,
    pNext: *mut c_void,
    format: VkFormat,
    externalFormat: u64,
    formatFeatures: F,
    samplerYcbcrConversionComponents: [i32; 4],
    suggestedYcbcrModel: i32,
    suggestedYcbcrRange: i32,
    suggestedXChromaOffset: i32,
    suggestedYChromaOffset: i32,
}

#[repr(C)]
struct VkImportAndroidHardwareBufferInfoANDROID {
    sType: VkStructureType,
    pNext: *const c_void,
    buffer: *const c_void,
}

#[repr(C)]
struct VkExternalFormatANDROID {
    sType: VkStructureType,
    pNext: *mut c_void,
    externalFormat: u64,
}

#[repr(C)]
struct VkAndroidHardwareBufferUsageANDROID {
    sType: VkStructureType,
    pNext: *mut c_void,
    androidHardwareBufferUsage: u64,
}

#[repr(C)]
struct VkMemoryGetAndroidHardwareBufferInfoANDROID {
    sType: VkStructureType,
    pNext: *const c_void,
    memory: VkDeviceMemory,
}

/// The Vulkan format of each layout the DMA-heap allocator lays RGB
/// buffers out in, by DRM fourcc. The fourcc, not the HAL format asked for,
/// is how the bytes lie, and what the compositor reads them as.
const LAYOUTS: [(u32, VkFormat); 8] = [
    (0x3432_4241, 37), // ABGR8888: R8G8B8A8_UNORM
    (0x3432_4258, 37), // XBGR8888
    (0x3432_5241, 44), // ARGB8888: B8G8R8A8_UNORM
    (0x3432_5258, 44), // XRGB8888
    (0x3631_4752, 4),  // RGB565: R5G6B5_UNORM_PACK16
    (0x3033_4241, 64), // ABGR2101010: A2B10G10R10_UNORM_PACK32
    (0x4834_4241, 97), // ABGR16161616F: R16G16B16A16_SFLOAT
    (0x2020_3852, 9),  // R8: R8_UNORM
];

/// The `HAL_PIXEL_FORMAT_*` an exported image of a Vulkan format is
/// allocated as.
const FORMATS: [(i32, VkFormat); 8] = [
    (1, 37),    // RGBA_8888: R8G8B8A8_UNORM
    (1, 43),    // R8G8B8A8_SRGB
    (4, 4),     // RGB_565: R5G6B5_UNORM_PACK16
    (5, 44),    // BGRA_8888: B8G8R8A8_UNORM
    (0x16, 97), // RGBA_FP16: R16G16B16A16_SFLOAT
    (0x2b, 64), // RGBA_1010102: A2B10G10R10_UNORM_PACK32
    (0x38, 9),  // R_8: R8_UNORM
    (5, 50),    // B8G8R8A8_SRGB
];

/// A YUV layout the allocator makes: its Vulkan format, how many planes
/// that has, and whether the layout stores Cr before Cb.
struct Yuv {
    fourcc: u32,
    format: VkFormat,
    planes: u32,
    cr_first: bool,
}

const YUV: [Yuv; 4] = [
    // NV12: G8_B8R8_2PLANE_420_UNORM
    Yuv { fourcc: 0x3231_564e, format: 1000156003, planes: 2, cr_first: false },
    // YVU420 (YV12): G8_B8_R8_3PLANE_420_UNORM
    Yuv { fourcc: 0x3231_5659, format: 1000156002, planes: 3, cr_first: true },
    // YVU420_ANDROID ("9997"), YV12 with Android's chroma stride
    Yuv { fourcc: 0x3739_3939, format: 1000156002, planes: 3, cr_first: true },
    // P010: G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
    Yuv { fourcc: 0x3031_3050, format: 1000156013, planes: 2, cr_first: false },
];

fn vk_format(fourcc: u32) -> Option<VkFormat> {
    LAYOUTS
        .iter()
        .find(|&&(layout, _)| layout == fourcc)
        .map(|&(_, vk)| vk)
        .or_else(|| YUV.iter().find(|yuv| yuv.fourcc == fourcc).map(|yuv| yuv.format))
}

fn is_yuv(format: VkFormat) -> bool {
    YUV.iter().any(|yuv| yuv.format == format)
}

/// The Vulkan format behind an external format, which is the format itself.
fn external_format(external: u64) -> VkFormat {
    match VkFormat::try_from(external) {
        Ok(format) if is_yuv(format) => format,
        _ => rt::fail(&format!("external format {external:#x} is not one this driver gave")),
    }
}

fn hal_format(format: VkFormat) -> Option<i32> {
    FORMATS.iter().find(|&&(_, vk)| vk == format).map(|&(hal, _)| hal)
}

unsafe fn hardware_buffer(buffer: *const c_void) -> Gralloc {
    unsafe {
        let mut buffer = android::gralloc(AHardwareBuffer_getNativeHandle(buffer));
        let planes = match YUV.iter().find(|yuv| yuv.fourcc == buffer.fourcc) {
            Some(yuv) => {
                // The image's planes come in Vulkan's order: Y, Cb, Cr.
                if yuv.cr_first {
                    buffer.planes.swap(1, 2);
                }
                yuv.planes
            }
            None => 1,
        };
        if buffer.plane_count != planes {
            rt::fail(&format!("AHardwareBuffer of fourcc {:#x} with {} planes", buffer.fourcc, buffer.plane_count));
        }
        buffer
    }
}

// ---------------------------------------------------------------- properties

pub unsafe extern "system" fn vkGetAndroidHardwareBufferPropertiesANDROID(
    device: VkDevice,
    buffer: *const c_void,
    pProperties: *mut c_void,
) -> VkResult {
    unsafe {
        let buffer = hardware_buffer(buffer);
        let Some(format) = vk_format(buffer.fourcc) else {
            return VK_ERROR_INVALID_EXTERNAL_HANDLE;
        };
        let mut memory = VkMemoryFdPropertiesKHR {
            sType: VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
            pNext: core::ptr::null_mut(),
            memoryTypeBits: 0,
        };
        let result = crate::generated::vkGetMemoryFdPropertiesKHR(device, DMA_BUF as i32, buffer.fd, &raw mut memory);
        if result != VK_SUCCESS {
            return result;
        }
        let properties = &mut *pProperties.cast::<VkAndroidHardwareBufferPropertiesANDROID>();
        // The DMA-BUF's own size, which an import must name; gralloc's
        // metadata size is not page-rounded.
        let size = libc::lseek(buffer.fd, 0, libc::SEEK_END);
        if size < 0 {
            rt::fail("size an AHardwareBuffer's DMA-BUF");
        }
        properties.allocationSize = size as u64;
        properties.memoryTypeBits = memory.memoryTypeBits;
        let mut next = properties.pNext.cast::<rt::Base>().cast_const();
        while !next.is_null() {
            match (*next).sType {
                VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_ANDROID => {
                    let features = layout_features(device, format, buffer.modifier);
                    describe::<u32>(next.cast_mut().cast(), format, features);
                }
                VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_2_ANDROID => {
                    let features = layout_features(device, format, buffer.modifier);
                    describe::<u64>(next.cast_mut().cast(), format, u64::from(features));
                }
                _ => {}
            }
            next = (*next).pNext.cast();
        }
        VK_SUCCESS
    }
}

/// RGB formats sample directly. YUV ones are external formats: ANGLE
/// refuses a named format that cannot also be rendered to and filtered,
/// which linear YUV cannot. They suggest video's usual conversion: BT.601,
/// or BT.2020 for 10 bits, in narrow range with chroma between the luma
/// samples.
unsafe fn describe<F>(out: *mut FormatProperties<F>, format: VkFormat, features: F) {
    unsafe {
        let yuv = is_yuv(format);
        (*out).format = if yuv { VK_FORMAT_UNDEFINED } else { format };
        (*out).externalFormat = if yuv { format as u64 } else { 0 };
        (*out).formatFeatures = features;
        (*out).samplerYcbcrConversionComponents = [0; 4];
        (*out).suggestedYcbcrModel = match format {
            _ if !yuv => 0,
            1000156013 => VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_2020,
            _ => VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_601,
        };
        let (range, chroma) = if yuv { (VK_SAMPLER_YCBCR_RANGE_ITU_NARROW, VK_CHROMA_LOCATION_MIDPOINT) } else { (0, 0) };
        (*out).suggestedYcbcrRange = range;
        (*out).suggestedXChromaOffset = chroma;
        (*out).suggestedYChromaOffset = chroma;
    }
}

/// What the host does with an image of `format` laid out in `modifier`.
unsafe fn layout_features(device: VkDevice, format: VkFormat, modifier: u64) -> u32 {
    unsafe {
        let physical = *rt::PHYSICAL.at(rt::id(device as usize));
        if modifier == 0 {
            let mut properties = core::mem::zeroed::<VkFormatProperties>();
            crate::generated::vkGetPhysicalDeviceFormatProperties(physical as _, format, &raw mut properties);
            return properties.linearTilingFeatures;
        }
        let mut entries = [core::mem::zeroed::<VkDrmFormatModifierPropertiesEXT>(); 64];
        let mut list = VkDrmFormatModifierPropertiesListEXT {
            sType: VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
            pNext: core::ptr::null_mut(),
            drmFormatModifierCount: entries.len() as u32,
            pDrmFormatModifierProperties: entries.as_mut_ptr(),
        };
        let mut properties = core::mem::zeroed::<VkFormatProperties2>();
        properties.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2;
        properties.pNext = (&raw mut list).cast();
        crate::generated::vkGetPhysicalDeviceFormatProperties2(physical as _, format, &raw mut properties);
        entries[..list.drmFormatModifierCount as usize]
            .iter()
            .find(|entry| entry.drmFormatModifier == modifier)
            .map(|entry| entry.drmFormatModifierTilingFeatures)
            .unwrap_or_else(|| rt::fail(&format!("AHardwareBuffer with DRM modifier {modifier:#x}, which the host does not list")))
    }
}

/// The AHardwareBuffer each exported allocation was made from.
static EXPORTED: rt::IdTable<*mut c_void> = rt::IdTable::new("exported memory");

pub unsafe extern "system" fn vkGetMemoryAndroidHardwareBufferANDROID(_device: VkDevice, pInfo: *const c_void, pBuffer: *mut *mut c_void) -> VkResult {
    unsafe {
        let buffer = *EXPORTED.at((*pInfo.cast::<VkMemoryGetAndroidHardwareBufferInfoANDROID>()).memory);
        if buffer.is_null() {
            rt::fail("AHardwareBuffer export of memory not allocated for it");
        }
        AHardwareBuffer_acquire(buffer);
        *pBuffer = buffer;
        VK_SUCCESS
    }
}

/// Keeps the buffer an exported allocation `memory` was made from; null
/// for any other allocation.
pub unsafe fn allocated(memory: VkDeviceMemory, buffer: *mut c_void) {
    unsafe { *EXPORTED.at(memory) = buffer }
}

/// Releases the buffer of an exported allocation, or of one the host refused.
pub unsafe fn release(buffer: *mut c_void) {
    unsafe {
        if !buffer.is_null() {
            AHardwareBuffer_release(buffer);
        }
    }
}

/// Forgets `memory`'s buffer when it is freed.
pub unsafe fn free(memory: VkDeviceMemory) {
    unsafe { release(core::mem::replace(&mut *EXPORTED.at(memory), core::ptr::null_mut())) }
}

/// The host's answer for the same image over a LINEAR DMA-BUF, which is
/// what an AHardwareBuffer image becomes. Other queries pass unchanged.
pub struct FormatQuery {
    info: VkPhysicalDeviceImageFormatInfo2,
    external: VkPhysicalDeviceExternalImageFormatInfo,
    modifier: VkPhysicalDeviceImageDrmFormatModifierInfoEXT,
}

pub unsafe fn format_query(
    info: *const VkPhysicalDeviceImageFormatInfo2,
    out: &mut MaybeUninit<FormatQuery>,
) -> Option<*const VkPhysicalDeviceImageFormatInfo2> {
    unsafe {
        let first = (*info).pNext.cast::<VkPhysicalDeviceExternalImageFormatInfo>();
        let mut next = first.cast::<rt::Base>();
        while !next.is_null() {
            if (*next).sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO
                && (*next.cast::<VkPhysicalDeviceExternalImageFormatInfo>()).handleType as u32 == HANDLE_TYPE
            {
                break;
            }
            next = (*next).pNext.cast();
        }
        if next.is_null() {
            return None;
        }
        if next != first.cast() {
            rt::fail("the AHardwareBuffer handle type comes first in an image format query");
        }
        let query = out.write(FormatQuery {
            info: *info,
            external: VkPhysicalDeviceExternalImageFormatInfo {
                sType: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
                pNext: core::ptr::null(),
                handleType: DMA_BUF as i32,
            },
            modifier: VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
                sType: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
                pNext: (*first).pNext,
                drmFormatModifier: 0,
                sharingMode: 0,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: core::ptr::null(),
            },
        });
        query.external.pNext = (&raw const query.modifier).cast();
        query.info.pNext = (&raw const query.external).cast();
        query.info.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
        Some(&raw const query.info)
    }
}

/// The host answered for a DMA-BUF; the application asked about an
/// AHardwareBuffer.
pub unsafe fn format_answer(info: *const VkPhysicalDeviceImageFormatInfo2, properties: *mut VkImageFormatProperties2) {
    unsafe {
        let mut next = (*properties).pNext.cast::<rt::Base>().cast_const();
        while !next.is_null() {
            match (*next).sType {
                VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES => {
                    let memory = &mut (*next.cast_mut().cast::<VkExternalImageFormatProperties>()).externalMemoryProperties;
                    let importable = memory.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT != 0;
                    let exportable = importable && hal_format((*info).format).is_some();
                    *memory = VkExternalMemoryProperties {
                        externalMemoryFeatures: if importable {
                            VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT
                                | VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT
                                | if exportable { VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT } else { 0 }
                        } else {
                            0
                        },
                        exportFromImportedHandleTypes: if importable { HANDLE_TYPE } else { 0 },
                        compatibleHandleTypes: if importable { HANDLE_TYPE } else { 0 },
                    };
                }
                VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_USAGE_ANDROID => {
                    (*next.cast_mut().cast::<VkAndroidHardwareBufferUsageANDROID>()).androidHardwareBufferUsage = usage((*info).usage, (*info).flags);
                }
                _ => {}
            }
            next = (*next).pNext.cast();
        }
    }
}

/// The gralloc usage an image with this usage and these flags needs.
fn usage(image_usage: VkImageUsageFlags, flags: VkImageCreateFlags) -> u64 {
    let mut usage = 0;
    if image_usage & (VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT) != 0 {
        usage |= GPU_SAMPLED_IMAGE;
    }
    if image_usage & (VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT) != 0 {
        usage |= GPU_FRAMEBUFFER;
    }
    if flags & VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT != 0 {
        usage |= GPU_CUBE_MAP;
    }
    if flags & VK_IMAGE_CREATE_PROTECTED_BIT != 0 {
        usage |= PROTECTED_CONTENT;
    }
    usage
}

// ---------------------------------------------------------------- images

/// An image waiting for its buffer: an AHardwareBuffer import, or the
/// gralloc buffer a swapchain with deferred allocation binds.
struct Deferred {
    info: VkImageCreateInfo,
    families: Vec<u32>,
    formats: Vec<VkFormat>,
    list: VkImageFormatListCreateInfo,
}

static DEFERRED: rt::IdTable<*mut Deferred> = rt::IdTable::new("deferred image");

/// The format behind each image created with an external format. Only
/// those images take views without a format, so a stale entry of a reused
/// id is never read.
static EXTERNAL: rt::IdTable<VkFormat> = rt::IdTable::new("external format");

/// A view of an image with an external format names none: the host gets the
/// image's format.
pub unsafe fn view_format(info: &VkImageViewCreateInfo) -> Option<VkImageViewCreateInfo> {
    unsafe {
        if info.format != VK_FORMAT_UNDEFINED {
            return None;
        }
        let format = *EXTERNAL.at(info.image);
        if format == VK_FORMAT_UNDEFINED {
            rt::fail("an image view without a format, of an image without an external format");
        }
        Some(VkImageViewCreateInfo { format, ..*info })
    }
}

/// A conversion for an external format names it in its chain instead of
/// its format.
pub unsafe fn conversion_format(info: &VkSamplerYcbcrConversionCreateInfo) -> Option<VkSamplerYcbcrConversionCreateInfo> {
    unsafe {
        if info.format != VK_FORMAT_UNDEFINED {
            return None;
        }
        let mut next = info.pNext.cast::<rt::Base>();
        while !next.is_null() && (*next).sType != VK_STRUCTURE_TYPE_EXTERNAL_FORMAT_ANDROID {
            next = (*next).pNext.cast();
        }
        if next.is_null() {
            rt::fail("a YCbCr conversion without a format or an external format");
        }
        let format = external_format((*next.cast::<VkExternalFormatANDROID>()).externalFormat);
        Some(VkSamplerYcbcrConversionCreateInfo { format, ..*info })
    }
}

/// Keeps an image that waits for its buffer under `id` instead of creating it.
pub unsafe fn defer(info: *const VkImageCreateInfo, id: u64) -> bool {
    unsafe {
        let mut hardware_buffer = false;
        let mut swapchain = false;
        let mut formats = Vec::new();
        let mut external = None;
        let mut unknown = None;
        let mut next = (*info).pNext.cast::<rt::Base>();
        while !next.is_null() {
            match (*next).sType {
                VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO => {
                    let types = (*next.cast::<VkExternalMemoryImageCreateInfo>()).handleTypes;
                    if types & HANDLE_TYPE != 0 {
                        if types != HANDLE_TYPE {
                            rt::fail("an AHardwareBuffer image takes no other handle types");
                        }
                        hardware_buffer = true;
                    }
                }
                VK_STRUCTURE_TYPE_EXTERNAL_FORMAT_ANDROID => {
                    let format = (*next.cast::<VkExternalFormatANDROID>()).externalFormat;
                    if format != 0 {
                        external = Some(external_format(format));
                    }
                }
                VK_STRUCTURE_TYPE_IMAGE_SWAPCHAIN_CREATE_INFO_KHR => {
                    swapchain = (*next.cast::<VkImageSwapchainCreateInfoKHR>()).swapchain != 0;
                }
                VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO => {
                    let list = &*next.cast::<VkImageFormatListCreateInfo>();
                    formats.extend_from_slice(core::slice::from_raw_parts(list.pViewFormats, list.viewFormatCount as usize));
                }
                other => unknown = Some(other),
            }
            next = (*next).pNext.cast();
        }
        if !hardware_buffer && !swapchain {
            return false;
        }
        if let Some(other) = unknown {
            rt::fail(&format!("deferred image with structure {other} in its chain"));
        }
        let families = if (*info).sharingMode == VK_SHARING_MODE_CONCURRENT {
            core::slice::from_raw_parts((*info).pQueueFamilyIndices, (*info).queueFamilyIndexCount as usize).to_vec()
        } else {
            Vec::new()
        };
        let mut deferred = Box::new(Deferred {
            info: *info,
            families,
            formats,
            list: VkImageFormatListCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
                pNext: core::ptr::null(),
                viewFormatCount: 0,
                pViewFormats: core::ptr::null(),
            },
        });
        deferred.info.pNext = core::ptr::null();
        deferred.info.pQueueFamilyIndices = deferred.families.as_ptr();
        if let Some(format) = external {
            deferred.info.format = format;
            *EXTERNAL.at(id) = format;
        }
        deferred.list.viewFormatCount = deferred.formats.len() as u32;
        deferred.list.pViewFormats = deferred.formats.as_ptr();
        *DEFERRED.at(id) = Box::into_raw(deferred);
        true
    }
}

/// Drops an image that never got its buffer; true if `image` was one.
pub unsafe fn forget(image: VkImage) -> bool {
    unsafe {
        let deferred = core::mem::replace(&mut *DEFERRED.at(image), core::ptr::null_mut());
        if deferred.is_null() {
            return false;
        }
        drop(Box::from_raw(deferred));
        true
    }
}

/// Creates a deferred swapchain image on the host over the gralloc buffer
/// its bind brings. False for a bind without one.
pub unsafe fn bind_native(device: VkDevice, bind: &VkBindImageMemoryInfo) -> bool {
    unsafe {
        let Some(handle) = android::native_buffer(bind.pNext) else { return false };
        let deferred = core::mem::replace(&mut *DEFERRED.at(bind.image), core::ptr::null_mut());
        if deferred.is_null() {
            rt::fail("a gralloc bind for an image created without a swapchain");
        }
        let deferred = Box::from_raw(deferred);
        let rest = if deferred.formats.is_empty() { core::ptr::null() } else { (&raw const deferred.list).cast() };
        let mut created = MaybeUninit::uninit();
        let create = DmaBufImage::build(&mut created, &deferred.info, rest, &android::gralloc(handle), true);
        crate::manual::send_image(device, create, bind.image);
        true
    }
}

// ---------------------------------------------------------------- memory

/// An AHardwareBuffer import or export rewritten as a DMA-BUF import.
pub struct Import {
    info: VkMemoryAllocateInfo,
    fd: VkImportMemoryFdInfoKHR,
    dedicated: VkMemoryDedicatedAllocateInfo,
}

/// The allocation to forward. Importing an AHardwareBuffer first creates
/// its image on the host over the buffer's DMA-BUF; exporting one first
/// allocates the buffer, into `exported`, then imports it.
pub unsafe fn import(
    device: VkDevice,
    info: *const VkMemoryAllocateInfo,
    out: &mut MaybeUninit<Import>,
    exported: &mut *mut c_void,
) -> Result<*const VkMemoryAllocateInfo, VkResult> {
    unsafe {
        let mut buffer: *const c_void = core::ptr::null();
        let mut image: VkImage = 0;
        let mut export = false;
        let mut next = (*info).pNext.cast::<rt::Base>();
        while !next.is_null() {
            match (*next).sType {
                VK_STRUCTURE_TYPE_IMPORT_ANDROID_HARDWARE_BUFFER_INFO_ANDROID => {
                    buffer = (*next.cast::<VkImportAndroidHardwareBufferInfoANDROID>()).buffer;
                }
                VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO => image = (*next.cast::<VkMemoryDedicatedAllocateInfo>()).image,
                VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO => {
                    let types = (*next.cast::<VkExportMemoryAllocateInfo>()).handleTypes;
                    if types & HANDLE_TYPE != 0 {
                        if types != HANDLE_TYPE {
                            rt::fail("an AHardwareBuffer export takes no other handle types");
                        }
                        export = true;
                    }
                }
                _ => {}
            }
            next = (*next).pNext.cast();
        }
        if buffer.is_null() && !export {
            return Ok(info);
        }
        if image == 0 {
            rt::fail("AHardwareBuffer memory for buffers (BLOB) is not forwarded");
        }
        let deferred = core::mem::replace(&mut *DEFERRED.at(image), core::ptr::null_mut());
        if deferred.is_null() {
            rt::fail("AHardwareBuffer memory for an image not created for one");
        }
        let deferred = Box::from_raw(deferred);
        if export {
            let created = &deferred.info;
            let format = hal_format(created.format)
                .unwrap_or_else(|| rt::fail(&format!("AHardwareBuffer export of format {}", created.format)));
            let desc = Desc {
                width: created.extent.width,
                height: created.extent.height,
                layers: created.arrayLayers,
                format: format as u32,
                usage: usage(created.usage, created.flags) | if created.mipLevels > 1 { GPU_MIPMAP_COMPLETE } else { 0 },
                stride: 0,
                rfu0: 0,
                rfu1: 0,
            };
            if AHardwareBuffer_allocate(&raw const desc, exported) != 0 {
                *DEFERRED.at(image) = Box::into_raw(deferred);
                return Err(VK_ERROR_OUT_OF_DEVICE_MEMORY);
            }
            buffer = *exported;
        }
        let buffer = hardware_buffer(buffer);
        let rest = if deferred.formats.is_empty() { core::ptr::null() } else { (&raw const deferred.list).cast() };
        let mut created = MaybeUninit::uninit();
        let create = DmaBufImage::build(&mut created, &deferred.info, rest, &buffer, false);
        crate::manual::send_image(device, create, image);
        let import = out.write(Import {
            info: *info,
            // An import keeps the application's chain, whose AHardwareBuffer
            // import is not encoded; an export names only the image, as its
            // AHardwareBuffer export would be.
            fd: VkImportMemoryFdInfoKHR {
                sType: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                pNext: if export { core::ptr::null() } else { (*info).pNext },
                handleType: DMA_BUF as i32,
                fd: android::duplicate(buffer.fd),
            },
            dedicated: VkMemoryDedicatedAllocateInfo {
                sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                pNext: core::ptr::null(),
                image,
                buffer: 0,
            },
        });
        if export {
            import.fd.pNext = (&raw const import.dedicated).cast();
            // An export names no size, and a memory type the application
            // chose before the buffer existed: the DMA-BUF's own size, and a
            // type it imports into.
            import.info.allocationSize = libc::lseek(buffer.fd, 0, libc::SEEK_END) as u64;
            let mut memory = VkMemoryFdPropertiesKHR {
                sType: VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
                pNext: core::ptr::null_mut(),
                memoryTypeBits: 0,
            };
            let result = crate::generated::vkGetMemoryFdPropertiesKHR(device, DMA_BUF as i32, buffer.fd, &raw mut memory);
            if result != VK_SUCCESS || memory.memoryTypeBits == 0 {
                rt::fail("no memory type imports an exported AHardwareBuffer");
            }
            if memory.memoryTypeBits & (1 << import.info.memoryTypeIndex) == 0 {
                import.info.memoryTypeIndex = memory.memoryTypeBits.trailing_zeros();
            }
        }
        import.info.pNext = (&raw const import.fd).cast();
        Ok(&raw const import.info)
    }
}

pub fn proc_addr(name: &[u8]) -> Option<*const c_void> {
    Some(match name {
        b"vkGetAndroidHardwareBufferPropertiesANDROID" => vkGetAndroidHardwareBufferPropertiesANDROID as *const c_void,
        b"vkGetMemoryAndroidHardwareBufferANDROID" => vkGetMemoryAndroidHardwareBufferANDROID as *const c_void,
        _ => return None,
    })
}
