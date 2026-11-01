//! ETC2, EAC and ASTC textures on a GPU that samples neither.
//!
//! Desktop NVIDIA GPUs have no ETC2 or ASTC, yet Android announces Vulkan
//! only with ETC2, and GLES 3.2 needs ASTC. The application's compressed
//! image is a decoded host image (RGBA8, or R16 and RG16 for EAC) plus a
//! buffer holding the blocks themselves, laid out per mip level as (layer,
//! row, block). The blocks are the truth: a copy into the image writes
//! them, then a compute shader (shaders/) decodes the touched blocks into
//! the image; a copy out of the image reads them. Queries report these
//! formats as sampled only, in optimal tiling.
//!
//! The decode runs inside the application's command buffer, so the compute
//! pipeline and descriptor sets it had bound are put back afterwards; each
//! command buffer's are kept in its `Stream`.

#![allow(non_snake_case)]

use core::ffi::c_void;

use droidloom_gpu_protocol::generated::frame;
use droidloom_gpu_protocol::generated::vk::*;

use crate::generated::{
    link_chain, patch_VkCommandBufferAllocateInfo, patch_VkCommandBufferBeginInfo, patch_VkCommandPoolCreateInfo, patch_VkCopyBufferToImageInfo2,
    patch_VkCopyImageInfo2, patch_VkCopyImageToBufferInfo2, patch_VkDeviceImageMemoryRequirements, patch_VkImageViewCreateInfo,
    patch_VkPhysicalDeviceImageFormatInfo2,
};
use crate::manual::find;
use crate::rt::{self, Server};

const OPTIMAL: i32 = 0;
const TYPE_2D: i32 = 1;
const VIEW_2D_ARRAY: i32 = 5;
const COLOR: u32 = 0x1;
const REMAINING_LAYERS: u32 = !0;
const ERROR_FORMAT_NOT_SUPPORTED: VkResult = -11;

const USAGE_TRANSFER_SRC: u32 = 0x1;
const USAGE_TRANSFER_DST: u32 = 0x2;
const USAGE_SAMPLED: u32 = 0x4;
const USAGE_STORAGE: u32 = 0x8;
const MUTABLE: u32 = 0x8;
const CUBE_COMPATIBLE: u32 = 0x10;
const EXTENDED_USAGE: u32 = 0x100;

/// Sampling with linear and min/max filters, blit source, transfers.
const FEATURES: u32 = 0x1 | 0x400 | 0x1000 | 0x4000 | 0x8000 | 0x10000;

const BUFFER_TRANSFER_SRC: u32 = 0x1;
const BUFFER_TRANSFER_DST: u32 = 0x2;
const BUFFER_UNIFORM_TEXEL: u32 = 0x4;
const DEVICE_LOCAL: u32 = 0x1;
const HOST_VISIBLE: u32 = 0x2;
const HOST_COHERENT: u32 = 0x4;
const QUEUE_COMPUTE: u32 = 0x2;

const GENERAL: i32 = 1;
const BIND_COMPUTE: i32 = 1;
const STAGE_COMPUTE: u32 = 0x20;
const STORAGE_IMAGE: i32 = 3;
const UNIFORM_TEXEL_BUFFER: i32 = 4;
const PUSH_DESCRIPTOR: u32 = 0x1;
const DISPATCH_BASE: u32 = 0x10;

// vkCmdPipelineBarrier stages and accesses.
const TRANSFER: u32 = 0x1000;
const COMPUTE_SHADER: u32 = 0x800;
const ALL_COMMANDS: u32 = 0x10000;
const SHADER_READ: u32 = 0x20;
const SHADER_WRITE: u32 = 0x40;
const TRANSFER_READ: u32 = 0x800;
const TRANSFER_WRITE: u32 = 0x1000;
const MEMORY_READ: u32 = 0x8000;
const MEMORY_WRITE: u32 = 0x10000;

const RGBA8: VkFormat = 37;
const RGBA8_UINT: VkFormat = 41;
const SRGB8: VkFormat = 43;
const R16: VkFormat = 70;
const R16_SNORM: VkFormat = 71;
const R16_UINT: VkFormat = 74;
const RG16: VkFormat = 77;
const RG16_SNORM: VkFormat = 78;
const RG16_UINT: VkFormat = 81;
const R8_UINT: VkFormat = 13;
const RGBA16_UINT: VkFormat = 95;
const RG32_UINT: VkFormat = 101;
const RGBA32_UINT: VkFormat = 107;

/// Block-buffer level offsets; above any texel buffer offset alignment.
const LEVEL_ALIGNMENT: u64 = 256;

const ASTC_BLOCKS: [(u32, u32); 14] = [(4, 4), (5, 4), (5, 5), (6, 5), (6, 6), (8, 5), (8, 6), (8, 8), (10, 5), (10, 6), (10, 8), (10, 10), (12, 10), (12, 12)];

const ETC2_SPIRV: [&[u8]; 7] = [
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-0.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-1.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-2.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-3.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-4.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-5.spv")),
    include_bytes!(concat!(env!("OUT_DIR"), "/etc2-6.spv")),
];
const ASTC_SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/astc.spv"));

type PushDescriptorSet = unsafe extern "system" fn(VkCommandBuffer, VkPipelineBindPoint, VkPipelineLayout, u32, u32, *const VkWriteDescriptorSet);
type MapMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory, VkDeviceSize, VkDeviceSize, u32, *mut *mut c_void) -> VkResult;

// ---------------------------------------------------------------- formats

#[derive(Clone, Copy)]
pub struct Format {
    /// Block width and height in texels.
    block: (u32, u32),
    /// Bytes per block: 8 or 16.
    bytes: u32,
    /// The host image's format.
    decoded: VkFormat,
    /// The view the decoder writes through.
    storage: VkFormat,
    kernel: Kernel,
}

#[derive(Clone, Copy)]
enum Kernel {
    /// shaders/etc2.comp's VARIANT.
    Etc2(usize),
    /// Index into `ASTC_BLOCKS`.
    Astc(usize),
}

/// The emulation of a compressed format.
pub fn format(format: VkFormat) -> Option<Format> {
    let etc2 = |variant, bytes, decoded, storage| Some(Format { block: (4, 4), bytes, decoded, storage, kernel: Kernel::Etc2(variant) });
    match format {
        147 => etc2(0, 8, RGBA8, RGBA8_UINT),
        148 => etc2(0, 8, SRGB8, RGBA8_UINT),
        149 => etc2(1, 8, RGBA8, RGBA8_UINT),
        150 => etc2(1, 8, SRGB8, RGBA8_UINT),
        151 => etc2(2, 16, RGBA8, RGBA8_UINT),
        152 => etc2(2, 16, SRGB8, RGBA8_UINT),
        153 => etc2(3, 8, R16, R16_UINT),
        154 => etc2(4, 8, R16_SNORM, R16_UINT),
        155 => etc2(5, 16, RG16, RG16_UINT),
        156 => etc2(6, 16, RG16_SNORM, RG16_UINT),
        157..=184 => {
            let i = (format - 157) as usize;
            let decoded = if i % 2 == 0 { RGBA8 } else { SRGB8 };
            Some(Format { block: ASTC_BLOCKS[i / 2], bytes: 16, decoded, storage: RGBA8_UINT, kernel: Kernel::Astc(i / 2) })
        }
        _ => None,
    }
}

fn view_format(view: VkFormat) -> VkFormat {
    format(view).map_or(view, |format| format.decoded)
}

pub fn features(features: &mut VkPhysicalDeviceFeatures) {
    features.textureCompressionETC2 = 1;
    features.textureCompressionASTC_LDR = 1;
}

/// The host has neither feature: a device asking for them gets them here.
pub unsafe fn strip_features(info: *mut VkDeviceCreateInfo) {
    unsafe {
        let mut features = (*info).pEnabledFeatures.cast_mut();
        if features.is_null() {
            if let Some(two) = find::<VkPhysicalDeviceFeatures2>((*info).pNext, VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2) {
                features = &raw mut (*two.cast_mut()).features;
            }
        }
        if !features.is_null() {
            (*features).textureCompressionETC2 = 0;
            (*features).textureCompressionASTC_LDR = 0;
        }
    }
}

/// Usage and flags a compressed image may have here.
fn supported(kind: i32, tiling: i32, usage: u32, flags: u32) -> bool {
    kind == TYPE_2D
        && tiling == OPTIMAL
        && usage & !(USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST | USAGE_SAMPLED) == 0
        && flags & !(MUTABLE | CUBE_COMPATIBLE | EXTENDED_USAGE) == 0
}

/// Point a view format list, if one restricts the views, at the decoded
/// formats and the decoder's; `formats` must outlive the call using it.
unsafe fn rewrite_list(next: *const c_void, storage: VkFormat, formats: &mut Vec<VkFormat>) {
    unsafe {
        let Some(list) = find::<VkImageFormatListCreateInfo>(next, VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO) else { return };
        let list = list.cast_mut();
        if (*list).viewFormatCount == 0 {
            return;
        }
        formats.extend((0..(*list).viewFormatCount as usize).map(|i| view_format(*(*list).pViewFormats.add(i))));
        formats.push(storage);
        (*list).viewFormatCount = formats.len() as u32;
        (*list).pViewFormats = formats.as_ptr();
    }
}

/// The host image behind a compressed one: decoded, and writable by the
/// decoder through its storage view.
unsafe fn rewrite_image(info: *mut VkImageCreateInfo, format: &Format, formats: &mut Vec<VkFormat>) {
    unsafe {
        (*info).format = format.decoded;
        (*info).flags |= MUTABLE | EXTENDED_USAGE;
        (*info).usage |= USAGE_STORAGE;
        rewrite_list((*info).pNext, format.storage, formats);
    }
}

pub unsafe fn h_vkGetPhysicalDeviceFormatProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceFormatProperties>();
        let properties: *mut VkFormatProperties = rt::at(record, (*f).pFormatProperties);
        let emulated = format((*f).format);
        (s.fns.vkGetPhysicalDeviceFormatProperties)(s.h((*f).physicalDevice) as usize, emulated.map_or((*f).format, |c| c.decoded), properties);
        if emulated.is_some() {
            (*properties).linearTilingFeatures = 0;
            (*properties).optimalTilingFeatures &= FEATURES;
            (*properties).bufferFeatures = 0;
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceFormatProperties2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceFormatProperties2>();
        let properties: *mut VkFormatProperties2 = rt::at(record, (*f).pFormatProperties);
        link_chain(record, (&raw mut (*properties).pNext).cast());
        let emulated = format((*f).format);
        (s.fns.vkGetPhysicalDeviceFormatProperties2)(s.h((*f).physicalDevice) as usize, emulated.map_or((*f).format, |c| c.decoded), properties);
        if emulated.is_some() {
            let base = &mut (*properties).formatProperties;
            base.linearTilingFeatures = 0;
            base.optimalTilingFeatures &= FEATURES;
            base.bufferFeatures = 0;
            let mut next = (*properties).pNext.cast::<rt::Base>();
            while !next.is_null() {
                match (*next).sType {
                    VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_3 => {
                        let three = next.cast::<VkFormatProperties3>();
                        (*three).linearTilingFeatures = 0;
                        (*three).optimalTilingFeatures &= u64::from(FEATURES);
                        (*three).bufferFeatures = 0;
                    }
                    // Both lists start with the count.
                    VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT | VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_2_EXT => {
                        (*next.cast::<VkDrmFormatModifierPropertiesListEXT>()).drmFormatModifierCount = 0;
                    }
                    _ => {}
                }
                next = (*next).pNext;
            }
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceImageFormatProperties(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceImageFormatProperties>();
        let physical = s.h((*f).physicalDevice) as usize;
        let properties: *mut VkImageFormatProperties = rt::at(record, (*f).pImageFormatProperties);
        let (kind, tiling, usage, flags) = ((*f).r#type, (*f).tiling, (*f).usage, (*f).flags);
        let result = match format((*f).format) {
            None => (s.fns.vkGetPhysicalDeviceImageFormatProperties)(physical, (*f).format, kind, tiling, usage, flags, properties),
            Some(emulated) if supported(kind, tiling, usage, flags) => {
                let result = (s.fns.vkGetPhysicalDeviceImageFormatProperties)(
                    physical,
                    emulated.decoded,
                    kind,
                    tiling,
                    usage | USAGE_STORAGE,
                    flags | MUTABLE | EXTENDED_USAGE,
                    properties,
                );
                (*properties).sampleCounts = 1;
                result
            }
            Some(_) => {
                *properties = core::mem::zeroed();
                ERROR_FORMAT_NOT_SUPPORTED
            }
        };
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

pub unsafe fn h_vkGetPhysicalDeviceImageFormatProperties2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetPhysicalDeviceImageFormatProperties2>();
        let physical = s.h((*f).physicalDevice) as usize;
        let info: *mut VkPhysicalDeviceImageFormatInfo2 = rt::at(record, (*f).pImageFormatInfo);
        patch_VkPhysicalDeviceImageFormatInfo2(s, record, info);
        let properties: *mut VkImageFormatProperties2 = rt::at(record, (*f).pImageFormatProperties);
        link_chain(record, (&raw mut (*properties).pNext).cast());
        let mut formats = Vec::new();
        let result = match format((*info).format) {
            None => crate::shadow::format_properties(s, physical, info, properties),
            Some(emulated) if supported((*info).r#type, (*info).tiling, (*info).usage, (*info).flags) => {
                (*info).format = emulated.decoded;
                (*info).flags |= MUTABLE | EXTENDED_USAGE;
                (*info).usage |= USAGE_STORAGE;
                rewrite_list((*info).pNext, emulated.storage, &mut formats);
                let result = (s.fns.vkGetPhysicalDeviceImageFormatProperties2)(physical, info, properties);
                (*properties).imageFormatProperties.sampleCounts = 1;
                result
            }
            Some(_) => {
                (*properties).imageFormatProperties = core::mem::zeroed();
                ERROR_FORMAT_NOT_SUPPORTED
            }
        };
        *rt::at::<VkResult>(record, (*f).reply) = result;
        rt::next(record)
    }
}

pub unsafe fn h_vkGetDeviceImageMemoryRequirements(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetDeviceImageMemoryRequirements>();
        let info: *mut VkDeviceImageMemoryRequirements = rt::at(record, (*f).pInfo);
        patch_VkDeviceImageMemoryRequirements(s, record, info);
        let requirements: *mut VkMemoryRequirements2 = rt::at(record, (*f).pMemoryRequirements);
        link_chain(record, (&raw mut (*requirements).pNext).cast());
        let image = (*info).pCreateInfo.cast_mut();
        let mut formats = Vec::new();
        if let Some(emulated) = format((*image).format) {
            rewrite_image(image, &emulated, &mut formats);
        }
        (s.fns.vkGetDeviceImageMemoryRequirements)(s.h((*f).device) as usize, info, requirements);
        rt::next(record)
    }
}

// ---------------------------------------------------------------- images

/// A compressed image's blocks.
pub struct Compressed {
    device: VkDevice,
    format: Format,
    /// The application's usage, which its views keep.
    usage: u32,
    layers: u32,
    buffer: VkBuffer,
    memory: VkDeviceMemory,
    levels: Vec<Level>,
}

struct Level {
    /// Where the level's blocks start in the buffer.
    offset: u64,
    /// Size in blocks.
    width: u32,
    height: u32,
    /// The blocks, for the decoder.
    texels: VkBufferView,
    /// The decoder's view of the level, made at its first decode: the image
    /// has no memory before.
    storage: VkImageView,
}

impl Compressed {
    /// Offset of row `row` of layer `layer` of a block rectangle.
    fn at(&self, b: &Blocks, layer: u32, row: u32) -> u64 {
        let level = &self.levels[b.level as usize];
        let block = (u64::from(b.layer + layer) * u64::from(level.height) + u64::from(b.y + row)) * u64::from(level.width) + u64::from(b.x);
        level.offset + block * u64::from(self.format.bytes)
    }
}

/// Create a compressed image as its decoded host image plus its blocks.
pub unsafe fn create(s: &mut Server, device: VkDevice, info: *mut VkImageCreateInfo, emulated: Format) -> VkImage {
    unsafe {
        let i = &*info;
        if !supported(i.imageType, i.tiling, i.usage, i.flags) || i.samples != 1 {
            rt::fail(&format!(
                "a compressed image of format {} type {} tiling {} usage {:x} flags {:x} samples {} is not emulated",
                i.format, i.imageType, i.tiling, i.usage, i.flags, i.samples
            ));
        }
        let (usage, width, height, layers) = (i.usage, i.extent.width, i.extent.height, i.arrayLayers);
        let mut levels = Vec::with_capacity(i.mipLevels as usize);
        let mut size = 0;
        for level in 0..i.mipLevels {
            let w = (width >> level).max(1).div_ceil(emulated.block.0);
            let h = (height >> level).max(1).div_ceil(emulated.block.1);
            levels.push(Level { offset: size, width: w, height: h, texels: 0, storage: 0 });
            size = (size + u64::from(w * h * emulated.bytes) * u64::from(layers)).next_multiple_of(LEVEL_ALIGNMENT);
        }
        let mut formats = Vec::new();
        rewrite_image(info, &emulated, &mut formats);
        let image = crate::manual::create_image(s, device, info);

        let decoder = s.decoders.get_mut(&device).unwrap_or_else(|| rt::fail("a compressed image on an unknown device"));
        decoder.ready(&s.fns, device);
        let (buffer, memory) = decoder.buffer(&s.fns, device, size, BUFFER_TRANSFER_SRC | BUFFER_TRANSFER_DST | BUFFER_UNIFORM_TEXEL, DEVICE_LOCAL);
        let element = if emulated.bytes == 8 { RG32_UINT } else { RGBA32_UINT };
        for level in &mut levels {
            let blocks = u64::from(level.width * level.height) * u64::from(layers);
            if blocks > u64::from(decoder.max_texels) {
                rt::fail("a compressed mip level larger than a texel buffer");
            }
            level.texels = buffer_view(&s.fns, device, buffer, element, level.offset, blocks * u64::from(emulated.bytes));
        }
        s.compressed.insert(image, Compressed { device, format: emulated, usage, layers, buffer, memory, levels });
        image
    }
}

/// Free a compressed image's blocks, ahead of the image itself.
pub unsafe fn destroy(s: &mut Server, image: VkImage) {
    unsafe {
        let Some(c) = s.compressed.remove(&image) else { return };
        for level in &c.levels {
            (s.fns.vkDestroyBufferView)(c.device, level.texels, core::ptr::null());
            if level.storage != 0 {
                (s.fns.vkDestroyImageView)(c.device, level.storage, core::ptr::null());
            }
        }
        (s.fns.vkDestroyBuffer)(c.device, c.buffer, core::ptr::null());
        (s.fns.vkFreeMemory)(c.device, c.memory, core::ptr::null());
    }
}

/// Views of a compressed image see the decoded format and the
/// application's usage, without the decoder's storage.
pub unsafe fn h_vkCreateImageView(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateImageView>();
        let device = s.h((*f).device) as usize;
        let info: *mut VkImageViewCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkImageViewCreateInfo(s, record, info);
        let mut usage = VkImageViewUsageCreateInfo { sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO, pNext: core::ptr::null(), usage: 0 };
        if let Some(c) = s.compressed.get(&(*info).image) {
            let Some(view) = format((*info).format) else { rt::fail("an uncompressed view of a compressed image is not emulated") };
            (*info).format = view.decoded;
            crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_IMAGE_VIEW_ASTC_DECODE_MODE_EXT);
            if find::<VkImageViewUsageCreateInfo>((*info).pNext, VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO).is_none() {
                usage.pNext = (*info).pNext;
                usage.usage = c.usage;
                (*info).pNext = (&raw const usage).cast();
            }
        }
        let mut view: VkImageView = 0;
        let result = (s.fns.vkCreateImageView)(device, info, core::ptr::null(), &raw mut view);
        if result != VK_SUCCESS {
            rt::failed("vkCreateImageView", result);
        }
        s.set((*f).pView, view);
        rt::next(record)
    }
}

// ---------------------------------------------------------------- decoder

/// A device's decode pipelines and ASTC tables, made at its first
/// compressed image.
pub struct Decoder {
    push: PushDescriptorSet,
    map: MapMemory,
    memory: VkPhysicalDeviceMemoryProperties,
    /// Queue families with compute, by bit.
    compute_families: u32,
    max_texels: u32,
    set_layout: VkDescriptorSetLayout,
    layout: VkPipelineLayout,
    etc2: [VkPipeline; 7],
    astc: [VkPipeline; 14],
    luts: VkBuffer,
    luts_memory: VkDeviceMemory,
    /// Bindings 2 to 6, then binding 7 per ASTC block size.
    lut_views: [VkBufferView; 5],
    partitions: [VkBufferView; 14],
}

/// Called for every device: what the decoder needs from it.
pub unsafe fn device_created(s: &mut Server, physical: usize, device: VkDevice, gdpa: PFN_vkGetDeviceProcAddr) {
    unsafe {
        let load = |name: &core::ffi::CStr| gdpa(device, name.as_ptr()).unwrap_or_else(|| rt::fail(&format!("the host has no {name:?}")));
        let push = core::mem::transmute::<unsafe extern "system" fn(), PushDescriptorSet>(load(c"vkCmdPushDescriptorSetKHR"));
        let map = core::mem::transmute::<unsafe extern "system" fn(), MapMemory>(load(c"vkMapMemory"));
        let mut memory = core::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
        (s.fns.vkGetPhysicalDeviceMemoryProperties)(physical, &raw mut memory);
        let mut count = 0;
        (s.fns.vkGetPhysicalDeviceQueueFamilyProperties)(physical, &raw mut count, core::ptr::null_mut());
        let mut families = vec![core::mem::zeroed::<VkQueueFamilyProperties>(); count as usize];
        (s.fns.vkGetPhysicalDeviceQueueFamilyProperties)(physical, &raw mut count, families.as_mut_ptr());
        let compute_families = families.iter().enumerate().filter(|(_, family)| family.queueFlags & QUEUE_COMPUTE != 0).fold(0, |bits, (i, _)| bits | 1 << i);
        let mut properties = core::mem::zeroed::<VkPhysicalDeviceProperties>();
        (s.fns.vkGetPhysicalDeviceProperties)(physical, &raw mut properties);
        if properties.limits.minTexelBufferOffsetAlignment > LEVEL_ALIGNMENT {
            rt::fail("texel buffer offsets need more alignment than the decoder gives");
        }
        s.decoders.insert(
            device,
            Decoder {
                push,
                map,
                memory,
                compute_families,
                max_texels: properties.limits.maxTexelBufferElements,
                set_layout: 0,
                layout: 0,
                etc2: [0; 7],
                astc: [0; 14],
                luts: 0,
                luts_memory: 0,
                lut_views: [0; 5],
                partitions: [0; 14],
            },
        );
    }
}

pub unsafe fn h_vkDestroyDevice(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkDestroyDevice>();
        let device = s.h((*f).device) as usize;
        if let Some(d) = s.decoders.remove(&device) {
            let fns = &s.fns;
            for &pipeline in d.etc2.iter().chain(&d.astc).filter(|&&p| p != 0) {
                (fns.vkDestroyPipeline)(device, pipeline, core::ptr::null());
            }
            for &view in d.lut_views.iter().chain(&d.partitions).filter(|&&v| v != 0) {
                (fns.vkDestroyBufferView)(device, view, core::ptr::null());
            }
            if d.layout != 0 {
                (fns.vkDestroyPipelineLayout)(device, d.layout, core::ptr::null());
                (fns.vkDestroyDescriptorSetLayout)(device, d.set_layout, core::ptr::null());
                (fns.vkDestroyBuffer)(device, d.luts, core::ptr::null());
                (fns.vkFreeMemory)(device, d.luts_memory, core::ptr::null());
            }
        }
        (s.fns.vkDestroyDevice)(device, core::ptr::null());
        rt::next(record)
    }
}

unsafe fn buffer_view(fns: &crate::generated::Fns, device: VkDevice, buffer: VkBuffer, format: VkFormat, offset: u64, range: u64) -> VkBufferView {
    unsafe {
        let info = VkBufferViewCreateInfo { sType: VK_STRUCTURE_TYPE_BUFFER_VIEW_CREATE_INFO, pNext: core::ptr::null(), flags: 0, buffer, format, offset, range };
        let mut view: VkBufferView = 0;
        let result = (fns.vkCreateBufferView)(device, &raw const info, core::ptr::null(), &raw mut view);
        if result != VK_SUCCESS {
            rt::failed("vkCreateBufferView for the texture decoder", result);
        }
        view
    }
}

impl Decoder {
    /// A buffer in its own memory of the lowest type with `flags`.
    unsafe fn buffer(&self, fns: &crate::generated::Fns, device: VkDevice, size: u64, usage: u32, flags: u32) -> (VkBuffer, VkDeviceMemory) {
        unsafe {
            let info = VkBufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                size: size.max(LEVEL_ALIGNMENT),
                usage,
                sharingMode: 0,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: core::ptr::null(),
            };
            let mut buffer: VkBuffer = 0;
            let result = (fns.vkCreateBuffer)(device, &raw const info, core::ptr::null(), &raw mut buffer);
            if result != VK_SUCCESS {
                rt::failed("vkCreateBuffer for the texture decoder", result);
            }
            let mut requirements = core::mem::zeroed::<VkMemoryRequirements>();
            (fns.vkGetBufferMemoryRequirements)(device, buffer, &raw mut requirements);
            let index = (0..self.memory.memoryTypeCount)
                .find(|&i| requirements.memoryTypeBits & (1 << i) != 0 && self.memory.memoryTypes[i as usize].propertyFlags & flags == flags)
                .unwrap_or_else(|| rt::fail("no memory type for the texture decoder"));
            let allocate = VkMemoryAllocateInfo { sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, pNext: core::ptr::null(), allocationSize: requirements.size, memoryTypeIndex: index };
            let mut memory: VkDeviceMemory = 0;
            let result = (fns.vkAllocateMemory)(device, &raw const allocate, core::ptr::null(), &raw mut memory);
            if result != VK_SUCCESS {
                rt::failed("vkAllocateMemory for the texture decoder", result);
            }
            let result = (fns.vkBindBufferMemory)(device, buffer, memory, 0);
            if result != VK_SUCCESS {
                rt::failed("vkBindBufferMemory for the texture decoder", result);
            }
            (buffer, memory)
        }
    }

    /// Layouts and ASTC tables, once per device.
    unsafe fn ready(&mut self, fns: &crate::generated::Fns, device: VkDevice) {
        unsafe {
            if self.layout != 0 {
                return;
            }
            let bindings: [VkDescriptorSetLayoutBinding; 8] = core::array::from_fn(|i| VkDescriptorSetLayoutBinding {
                binding: i as u32,
                descriptorType: if i == 0 { STORAGE_IMAGE } else { UNIFORM_TEXEL_BUFFER },
                descriptorCount: 1,
                stageFlags: STAGE_COMPUTE,
                pImmutableSamplers: core::ptr::null(),
            });
            let info = VkDescriptorSetLayoutCreateInfo {
                sType: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: PUSH_DESCRIPTOR,
                bindingCount: 8,
                pBindings: bindings.as_ptr(),
            };
            let result = (fns.vkCreateDescriptorSetLayout)(device, &raw const info, core::ptr::null(), &raw mut self.set_layout);
            if result != VK_SUCCESS {
                rt::failed("vkCreateDescriptorSetLayout for the texture decoder", result);
            }
            let info = VkPipelineLayoutCreateInfo {
                sType: VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                setLayoutCount: 1,
                pSetLayouts: &raw const self.set_layout,
                pushConstantRangeCount: 0,
                pPushConstantRanges: core::ptr::null(),
            };
            let result = (fns.vkCreatePipelineLayout)(device, &raw const info, core::ptr::null(), &raw mut self.layout);
            if result != VK_SUCCESS {
                rt::failed("vkCreatePipelineLayout for the texture decoder", result);
            }

            // The ASTC tables in one host-visible buffer.
            let luts = crate::astc_luts::luts();
            let bytes = |words: &[u16]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
            let mut tables: Vec<(Vec<u8>, VkFormat)> = vec![
                (bytes(&luts.endpoint_modes), RGBA16_UINT),
                (luts.endpoint_unquantize, R8_UINT),
                (luts.weight_modes, RGBA8_UINT),
                (luts.weight_unquantize, R8_UINT),
                (bytes(&luts.trits_quints), R16_UINT),
            ];
            tables.extend(ASTC_BLOCKS.iter().map(|&(w, h)| (crate::astc_luts::partition_table(w, h), R8_UINT)));
            let mut offsets = Vec::with_capacity(tables.len());
            let mut size = 0;
            for (table, _) in &tables {
                offsets.push(size);
                size = (size + table.len() as u64).next_multiple_of(LEVEL_ALIGNMENT);
            }
            (self.luts, self.luts_memory) = self.buffer(fns, device, size, BUFFER_UNIFORM_TEXEL, HOST_VISIBLE | HOST_COHERENT);
            let mut mapped: *mut c_void = core::ptr::null_mut();
            let result = (self.map)(device, self.luts_memory, 0, !0, 0, &raw mut mapped);
            if result != VK_SUCCESS {
                rt::failed("vkMapMemory for the ASTC tables", result);
            }
            for (i, (table, format)) in tables.iter().enumerate() {
                core::ptr::copy_nonoverlapping(table.as_ptr(), mapped.cast::<u8>().add(offsets[i] as usize), table.len());
                let view = buffer_view(fns, device, self.luts, *format, offsets[i], table.len() as u64);
                if i < 5 {
                    self.lut_views[i] = view;
                } else {
                    self.partitions[i - 5] = view;
                }
            }
        }
    }

    /// The decode pipeline for a format, compiled at its first use.
    unsafe fn pipeline(&mut self, fns: &crate::generated::Fns, device: VkDevice, kernel: Kernel) -> VkPipeline {
        unsafe {
            let (slot, spirv) = match kernel {
                Kernel::Etc2(variant) => (&raw mut self.etc2[variant], ETC2_SPIRV[variant]),
                Kernel::Astc(size) => (&raw mut self.astc[size], ASTC_SPIRV),
            };
            if *slot != 0 {
                return *slot;
            }
            let code: Vec<u32> = spirv.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
            let info = VkShaderModuleCreateInfo {
                sType: VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: 0,
                codeSize: spirv.len(),
                pCode: code.as_ptr(),
            };
            let mut module: VkShaderModule = 0;
            let result = (fns.vkCreateShaderModule)(device, &raw const info, core::ptr::null(), &raw mut module);
            if result != VK_SUCCESS {
                rt::failed("vkCreateShaderModule for the texture decoder", result);
            }
            // ASTC's workgroup is one block per thread layer: the block size.
            let block = match kernel {
                Kernel::Astc(size) => [ASTC_BLOCKS[size].0, ASTC_BLOCKS[size].1],
                Kernel::Etc2(_) => [0, 0],
            };
            let entries = [
                VkSpecializationMapEntry { constantID: 0, offset: 0, size: 4 },
                VkSpecializationMapEntry { constantID: 1, offset: 4, size: 4 },
            ];
            let specialization = VkSpecializationInfo { mapEntryCount: 2, pMapEntries: entries.as_ptr(), dataSize: 8, pData: block.as_ptr().cast() };
            let info = VkComputePipelineCreateInfo {
                sType: VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
                pNext: core::ptr::null(),
                flags: DISPATCH_BASE,
                stage: VkPipelineShaderStageCreateInfo {
                    sType: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                    pNext: core::ptr::null(),
                    flags: 0,
                    stage: STAGE_COMPUTE as _,
                    module,
                    pName: c"main".as_ptr(),
                    pSpecializationInfo: if matches!(kernel, Kernel::Astc(_)) { &raw const specialization } else { core::ptr::null() },
                },
                layout: self.layout,
                basePipelineHandle: 0,
                basePipelineIndex: -1,
            };
            let result = (fns.vkCreateComputePipelines)(device, 0, 1, &raw const info, core::ptr::null(), slot);
            (fns.vkDestroyShaderModule)(device, module, core::ptr::null());
            if result != VK_SUCCESS {
                rt::failed("vkCreateComputePipelines for the texture decoder", result);
            }
            *slot
        }
    }
}

// ---------------------------------------------------------------- command buffers

/// What one command buffer has bound for compute, which a decode in it
/// puts back.
pub struct Stream {
    pool: VkCommandPool,
    /// The pool's queue family runs compute.
    compute: bool,
    pipeline: VkPipeline,
    /// vkCmdBindDescriptorSets calls in order, without those a later call
    /// replaced entirely.
    sets: Vec<Sets>,
}

struct Sets {
    layout: VkPipelineLayout,
    first: u32,
    sets: Vec<VkDescriptorSet>,
    offsets: Vec<u32>,
}

pub unsafe fn h_vkCreateCommandPool(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCreateCommandPool>();
        let device = s.h((*f).device) as usize;
        let info: *mut VkCommandPoolCreateInfo = rt::at(record, (*f).pCreateInfo);
        patch_VkCommandPoolCreateInfo(s, record, info);
        let mut pool: VkCommandPool = 0;
        let result = (s.fns.vkCreateCommandPool)(device, info, core::ptr::null(), &raw mut pool);
        if result != VK_SUCCESS {
            rt::failed("vkCreateCommandPool", result);
        }
        let decoder = s.decoders.get(&device).unwrap_or_else(|| rt::fail("a command pool on an unknown device"));
        s.pools.insert(pool, decoder.compute_families >> (*info).queueFamilyIndex & 1 != 0);
        s.set((*f).pCommandPool, pool);
        rt::next(record)
    }
}

pub unsafe fn h_vkDestroyCommandPool(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkDestroyCommandPool>();
        let pool = s.h((*f).commandPool);
        (s.fns.vkDestroyCommandPool)(s.h((*f).device) as usize, pool, core::ptr::null());
        s.pools.remove(&pool);
        s.streams.retain(|_, stream| stream.pool != pool);
        rt::next(record)
    }
}

pub unsafe fn h_vkAllocateCommandBuffers(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkAllocateCommandBuffers>();
        let device = s.h((*f).device) as usize;
        let info: *mut VkCommandBufferAllocateInfo = rt::at(record, (*f).pAllocateInfo);
        patch_VkCommandBufferAllocateInfo(s, record, info);
        let ids: *mut u64 = rt::at(record, (*f).pCommandBuffers);
        let mut buffers: Vec<VkCommandBuffer> = vec![0; (*info).commandBufferCount as usize];
        let result = (s.fns.vkAllocateCommandBuffers)(device, info, buffers.as_mut_ptr());
        if result != VK_SUCCESS {
            rt::failed("vkAllocateCommandBuffers", result);
        }
        let pool = (*info).commandPool;
        let compute = *s.pools.get(&pool).unwrap_or_else(|| rt::fail("command buffers from an unknown pool"));
        for (i, &buffer) in buffers.iter().enumerate() {
            s.set(*ids.add(i), buffer as u64);
            s.streams.insert(buffer, Box::new(Stream { pool, compute, pipeline: 0, sets: Vec::new() }));
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkFreeCommandBuffers(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkFreeCommandBuffers>();
        let buffers: *mut u64 = rt::at(record, (*f).pCommandBuffers);
        for i in 0..(*f).commandBufferCount as usize {
            *buffers.add(i) = s.h(*buffers.add(i));
            s.streams.remove(&(*buffers.add(i) as usize));
        }
        (s.fns.vkFreeCommandBuffers)(s.h((*f).device) as usize, s.h((*f).commandPool), (*f).commandBufferCount, buffers.cast());
        rt::next(record)
    }
}

pub unsafe fn h_vkBeginCommandBuffer(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkBeginCommandBuffer>();
        let info: *mut VkCommandBufferBeginInfo = rt::at(record, (*f).pBeginInfo);
        patch_VkCommandBufferBeginInfo(s, record, info);
        let result = (s.fns.vkBeginCommandBuffer)(s.cb, info);
        if result != VK_SUCCESS {
            rt::failed("vkBeginCommandBuffer", result);
        }
        (*s.stream).pipeline = 0;
        (*s.stream).sets.clear();
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdBindPipeline(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdBindPipeline>();
        let pipeline = s.h((*f).pipeline);
        (s.fns.vkCmdBindPipeline)(s.cb, (*f).pipelineBindPoint, pipeline);
        if (*f).pipelineBindPoint == BIND_COMPUTE {
            (*s.stream).pipeline = pipeline;
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdBindDescriptorSets(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdBindDescriptorSets>();
        let layout = s.h((*f).layout);
        let (first, count) = ((*f).firstSet, (*f).descriptorSetCount);
        let sets: *mut u64 = rt::at(record, (*f).pDescriptorSets);
        for i in 0..count as usize {
            *sets.add(i) = s.h(*sets.add(i));
        }
        let offsets: *const u32 = rt::at(record, (*f).pDynamicOffsets);
        (s.fns.vkCmdBindDescriptorSets)(s.cb, (*f).pipelineBindPoint, layout, first, count, sets.cast(), (*f).dynamicOffsetCount, offsets);
        if (*f).pipelineBindPoint == BIND_COMPUTE {
            let bound = &mut (*s.stream).sets;
            bound.retain(|b| b.first < first || b.first + b.sets.len() as u32 > first + count);
            bound.push(Sets {
                layout,
                first,
                sets: core::slice::from_raw_parts(sets.cast_const(), count as usize).to_vec(),
                offsets: if offsets.is_null() { Vec::new() } else { core::slice::from_raw_parts(offsets, (*f).dynamicOffsetCount as usize).to_vec() },
            });
        }
        rt::next(record)
    }
}

// ---------------------------------------------------------------- copies

/// A rectangle of blocks in one mip level, over a run of layers. For an
/// uncompressed image a block is a texel.
#[derive(Clone, Copy)]
struct Blocks {
    level: u32,
    layer: u32,
    layers: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Blocks {
    fn at(sub: &VkImageSubresourceLayers, offset: VkOffset3D, block: (u32, u32), width: u32, height: u32, layers: u32) -> Self {
        Self { level: sub.mipLevel, layer: sub.baseArrayLayer, layers, x: offset.x as u32 / block.0, y: offset.y as u32 / block.1, width, height }
    }
}

fn layer_count(sub: &VkImageSubresourceLayers, layers: u32) -> u32 {
    if sub.layerCount == REMAINING_LAYERS { layers - sub.baseArrayLayer } else { sub.layerCount }
}

/// Row-by-row buffer copies of a block rectangle, merged where both sides
/// run on.
fn rows(b: &Blocks, bytes: u32, src: impl Fn(u32, u32) -> u64, dst: impl Fn(u32, u32) -> u64, out: &mut Vec<VkBufferCopy>) {
    let size = u64::from(b.width * bytes);
    for layer in 0..b.layers {
        for row in 0..b.height {
            let (from, to) = (src(layer, row), dst(layer, row));
            match out.last_mut() {
                Some(last) if last.srcOffset + last.size == from && last.dstOffset + last.size == to => last.size += size,
                _ => out.push(VkBufferCopy { srcOffset: from, dstOffset: to, size }),
            }
        }
    }
}

/// The blocks of a buffer region as an application lays them out.
fn linear(region: &VkBufferImageCopy, format: &Format) -> (u64, u64) {
    let rows = if region.bufferRowLength == 0 { region.imageExtent.width } else { region.bufferRowLength };
    let height = if region.bufferImageHeight == 0 { region.imageExtent.height } else { region.bufferImageHeight };
    let row = u64::from(rows.div_ceil(format.block.0) * format.bytes);
    (row, row * u64::from(height.div_ceil(format.block.1)))
}

unsafe fn memory_barrier(s: &Server, src_stage: u32, src: u32, dst_stage: u32, dst: u32) {
    unsafe {
        let barrier = VkMemoryBarrier { sType: VK_STRUCTURE_TYPE_MEMORY_BARRIER, pNext: core::ptr::null(), srcAccessMask: src, dstAccessMask: dst };
        (s.fns.vkCmdPipelineBarrier)(s.cb, src_stage, dst_stage, 0, 1, &raw const barrier, 0, core::ptr::null(), 0, core::ptr::null());
    }
}

/// Before the blocks are written: earlier decodes and copies are done.
unsafe fn before_write(s: &Server) {
    unsafe { memory_barrier(s, TRANSFER | COMPUTE_SHADER, TRANSFER_WRITE, TRANSFER, TRANSFER_WRITE) };
}

/// Before the blocks are read by a copy: earlier writes are visible.
unsafe fn before_read(s: &Server) {
    unsafe { memory_barrier(s, TRANSFER, TRANSFER_WRITE, TRANSFER, TRANSFER_READ) };
}

/// Decode `regions` of compressed `image`, whose blocks were just written,
/// leaving the image in `layout` as the copy would have, and the
/// application's compute state as it was.
unsafe fn decode(s: &mut Server, image: VkImage, layout: VkImageLayout, regions: &[Blocks]) {
    unsafe {
        if !(*s.stream).compute {
            rt::fail("an ETC2 or ASTC upload on a queue without compute is not emulated");
        }
        let cb = s.cb;
        let c = s.compressed.get_mut(&image).unwrap();
        let decoder = s.decoders.get_mut(&c.device).unwrap();
        let fns = &s.fns;
        let pipeline = decoder.pipeline(fns, c.device, c.format.kernel);
        for b in regions {
            let level = &mut c.levels[b.level as usize];
            if level.storage == 0 {
                let usage = VkImageViewUsageCreateInfo { sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO, pNext: core::ptr::null(), usage: USAGE_STORAGE };
                let info = VkImageViewCreateInfo {
                    sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                    pNext: (&raw const usage).cast(),
                    flags: 0,
                    image,
                    viewType: VIEW_2D_ARRAY,
                    format: c.format.storage,
                    components: core::mem::zeroed(),
                    subresourceRange: VkImageSubresourceRange { aspectMask: COLOR, baseMipLevel: b.level, levelCount: 1, baseArrayLayer: 0, layerCount: c.layers },
                };
                let result = (fns.vkCreateImageView)(c.device, &raw const info, core::ptr::null(), &raw mut level.storage);
                if result != VK_SUCCESS {
                    rt::failed("vkCreateImageView for the texture decoder", result);
                }
            }
        }

        let barriers: Vec<VkImageMemoryBarrier> = subresources(regions)
            .into_iter()
            .map(|range| VkImageMemoryBarrier {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                pNext: core::ptr::null(),
                srcAccessMask: 0,
                dstAccessMask: SHADER_WRITE,
                oldLayout: layout,
                newLayout: GENERAL,
                srcQueueFamilyIndex: !0,
                dstQueueFamilyIndex: !0,
                image,
                subresourceRange: range,
            })
            .collect();
        let blocks = VkMemoryBarrier { sType: VK_STRUCTURE_TYPE_MEMORY_BARRIER, pNext: core::ptr::null(), srcAccessMask: TRANSFER_WRITE, dstAccessMask: SHADER_READ };
        (fns.vkCmdPipelineBarrier)(cb, TRANSFER, COMPUTE_SHADER, 0, 1, &raw const blocks, 0, core::ptr::null(), barriers.len() as u32, barriers.as_ptr());

        (fns.vkCmdBindPipeline)(cb, BIND_COMPUTE, pipeline);
        for b in regions {
            let level = &c.levels[b.level as usize];
            let output = VkDescriptorImageInfo { sampler: 0, imageView: level.storage, imageLayout: GENERAL };
            let mut texels = [level.texels; 7];
            texels[1..6].copy_from_slice(&decoder.lut_views);
            if let Kernel::Astc(size) = c.format.kernel {
                texels[6] = decoder.partitions[size];
            }
            let bindings = if matches!(c.format.kernel, Kernel::Astc(_)) { 8 } else { 2 };
            let writes: Vec<VkWriteDescriptorSet> = (0..bindings)
                .map(|i| VkWriteDescriptorSet {
                    sType: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                    pNext: core::ptr::null(),
                    dstSet: 0,
                    dstBinding: i,
                    dstArrayElement: 0,
                    descriptorCount: 1,
                    descriptorType: if i == 0 { STORAGE_IMAGE } else { UNIFORM_TEXEL_BUFFER },
                    pImageInfo: &raw const output,
                    pBufferInfo: core::ptr::null(),
                    pTexelBufferView: if i == 0 { core::ptr::null() } else { &raw const texels[i as usize - 1] },
                })
                .collect();
            (decoder.push)(cb, BIND_COMPUTE, decoder.layout, 0, writes.len() as u32, writes.as_ptr());
            // A workgroup decodes 2x2 blocks; its neighbours in the region's
            // edge workgroups decode to what they already hold.
            let (x, y) = (b.x / 2, b.y / 2);
            let (end_x, end_y) = ((b.x + b.width).div_ceil(2), (b.y + b.height).div_ceil(2));
            (fns.vkCmdDispatchBase)(cb, x, y, b.layer, end_x - x, end_y - y, b.layers);
        }

        let barriers: Vec<VkImageMemoryBarrier> = barriers
            .into_iter()
            .map(|barrier| VkImageMemoryBarrier {
                srcAccessMask: SHADER_WRITE,
                dstAccessMask: MEMORY_READ | MEMORY_WRITE,
                oldLayout: GENERAL,
                newLayout: layout,
                ..barrier
            })
            .collect();
        (fns.vkCmdPipelineBarrier)(cb, COMPUTE_SHADER, ALL_COMMANDS, 0, 0, core::ptr::null(), 0, core::ptr::null(), barriers.len() as u32, barriers.as_ptr());

        let stream = &*s.stream;
        if stream.pipeline != 0 {
            (fns.vkCmdBindPipeline)(cb, BIND_COMPUTE, stream.pipeline);
        }
        for bound in &stream.sets {
            (fns.vkCmdBindDescriptorSets)(
                cb,
                BIND_COMPUTE,
                bound.layout,
                bound.first,
                bound.sets.len() as u32,
                bound.sets.as_ptr(),
                bound.offsets.len() as u32,
                bound.offsets.as_ptr(),
            );
        }
    }
}

/// The subresources regions touch, each once: one layout transition each.
fn subresources(regions: &[Blocks]) -> Vec<VkImageSubresourceRange> {
    let mut spans: Vec<(u32, u32, u32)> = regions.iter().map(|b| (b.level, b.layer, b.layer + b.layers)).collect();
    spans.sort_unstable();
    let mut merged: Vec<(u32, u32, u32)> = Vec::with_capacity(spans.len());
    for (level, start, end) in spans {
        match merged.last_mut() {
            Some(last) if last.0 == level && start <= last.2 => last.2 = last.2.max(end),
            _ => merged.push((level, start, end)),
        }
    }
    merged
        .into_iter()
        .map(|(level, start, end)| VkImageSubresourceRange { aspectMask: COLOR, baseMipLevel: level, levelCount: 1, baseArrayLayer: start, layerCount: end - start })
        .collect()
}

/// A buffer into a compressed image: the blocks, then the decode.
unsafe fn buffer_to_image(s: &mut Server, buffer: VkBuffer, image: VkImage, layout: VkImageLayout, regions: &[VkBufferImageCopy]) {
    unsafe {
        let c = &s.compressed[&image];
        let mut copies = Vec::new();
        let mut decoded = Vec::with_capacity(regions.len());
        for region in regions {
            let (bw, bh) = c.format.block;
            let b = Blocks::at(
                &region.imageSubresource,
                region.imageOffset,
                c.format.block,
                region.imageExtent.width.div_ceil(bw),
                region.imageExtent.height.div_ceil(bh),
                layer_count(&region.imageSubresource, c.layers),
            );
            let (row, layer) = linear(region, &c.format);
            rows(&b, c.format.bytes, |l, r| region.bufferOffset + u64::from(l) * layer + u64::from(r) * row, |l, r| c.at(&b, l, r), &mut copies);
            decoded.push(b);
        }
        let blocks = c.buffer;
        before_write(s);
        (s.fns.vkCmdCopyBuffer)(s.cb, buffer, blocks, copies.len() as u32, copies.as_ptr());
        decode(s, image, layout, &decoded);
    }
}

/// A compressed image into a buffer: straight from the blocks.
unsafe fn image_to_buffer(s: &mut Server, image: VkImage, buffer: VkBuffer, regions: &[VkBufferImageCopy]) {
    unsafe {
        let c = &s.compressed[&image];
        let mut copies = Vec::new();
        for region in regions {
            let (bw, bh) = c.format.block;
            let b = Blocks::at(
                &region.imageSubresource,
                region.imageOffset,
                c.format.block,
                region.imageExtent.width.div_ceil(bw),
                region.imageExtent.height.div_ceil(bh),
                layer_count(&region.imageSubresource, c.layers),
            );
            let (row, layer) = linear(region, &c.format);
            rows(&b, c.format.bytes, |l, r| c.at(&b, l, r), |l, r| region.bufferOffset + u64::from(l) * layer + u64::from(r) * row, &mut copies);
        }
        let blocks = c.buffer;
        before_read(s);
        (s.fns.vkCmdCopyBuffer)(s.cb, blocks, buffer, copies.len() as u32, copies.as_ptr());
    }
}

/// Image copies with a compressed side go through its blocks: block for
/// block, an uncompressed texel being one block.
unsafe fn image_to_image(s: &mut Server, src: VkImage, src_layout: VkImageLayout, dst: VkImage, dst_layout: VkImageLayout, regions: &[VkImageCopy]) {
    unsafe {
        let (from, to) = (s.compressed.get(&src), s.compressed.get(&dst));
        let block = |c: Option<&Compressed>| c.map_or((1, 1), |c| c.format.block);
        let layers = from.or(to).unwrap().layers;
        let mut pairs = Vec::with_capacity(regions.len());
        for region in regions {
            let (bw, bh) = block(from);
            let (width, height) = (region.extent.width.div_ceil(bw), region.extent.height.div_ceil(bh));
            let count = layer_count(&region.srcSubresource, from.map_or(layers, |c| c.layers));
            let a = Blocks::at(&region.srcSubresource, region.srcOffset, block(from), width, height, count);
            let b = Blocks::at(&region.dstSubresource, region.dstOffset, block(to), width, height, count);
            pairs.push((a, b));
        }
        match (from, to) {
            (Some(a), Some(b)) => {
                let mut copies = Vec::new();
                for (x, y) in &pairs {
                    rows(x, a.format.bytes, |l, r| a.at(x, l, r), |l, r| b.at(y, l, r), &mut copies);
                }
                let (from_blocks, to_blocks) = (a.buffer, b.buffer);
                memory_barrier(s, TRANSFER | COMPUTE_SHADER, TRANSFER_WRITE, TRANSFER, TRANSFER_READ | TRANSFER_WRITE);
                (s.fns.vkCmdCopyBuffer)(s.cb, from_blocks, to_blocks, copies.len() as u32, copies.as_ptr());
                let decoded: Vec<Blocks> = pairs.iter().map(|p| p.1).collect();
                decode(s, dst, dst_layout, &decoded);
            }
            (Some(a), None) => {
                let copies: Vec<VkBufferImageCopy> = pairs
                    .iter()
                    .zip(regions)
                    .map(|((x, y), region)| VkBufferImageCopy {
                        bufferOffset: a.at(x, 0, 0),
                        bufferRowLength: a.levels[x.level as usize].width,
                        bufferImageHeight: a.levels[x.level as usize].height,
                        imageSubresource: VkImageSubresourceLayers { layerCount: y.layers, ..region.dstSubresource },
                        imageOffset: region.dstOffset,
                        imageExtent: VkExtent3D { width: y.width, height: y.height, depth: 1 },
                    })
                    .collect();
                let blocks = a.buffer;
                before_read(s);
                (s.fns.vkCmdCopyBufferToImage)(s.cb, blocks, dst, dst_layout, copies.len() as u32, copies.as_ptr());
            }
            (None, Some(b)) => {
                let copies: Vec<VkBufferImageCopy> = pairs
                    .iter()
                    .zip(regions)
                    .map(|((x, y), region)| VkBufferImageCopy {
                        bufferOffset: b.at(y, 0, 0),
                        bufferRowLength: b.levels[y.level as usize].width,
                        bufferImageHeight: b.levels[y.level as usize].height,
                        imageSubresource: VkImageSubresourceLayers { layerCount: x.layers, ..region.srcSubresource },
                        imageOffset: region.srcOffset,
                        imageExtent: VkExtent3D { width: x.width, height: x.height, depth: 1 },
                    })
                    .collect();
                let blocks = b.buffer;
                before_write(s);
                (s.fns.vkCmdCopyImageToBuffer)(s.cb, src, src_layout, blocks, copies.len() as u32, copies.as_ptr());
                let decoded: Vec<Blocks> = pairs.iter().map(|p| p.1).collect();
                decode(s, dst, dst_layout, &decoded);
            }
            (None, None) => unreachable!(),
        }
    }
}

fn region(r: &VkBufferImageCopy2) -> VkBufferImageCopy {
    VkBufferImageCopy {
        bufferOffset: r.bufferOffset,
        bufferRowLength: r.bufferRowLength,
        bufferImageHeight: r.bufferImageHeight,
        imageSubresource: r.imageSubresource,
        imageOffset: r.imageOffset,
        imageExtent: r.imageExtent,
    }
}

pub unsafe fn h_vkCmdCopyBufferToImage(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyBufferToImage>();
        let (buffer, image) = (s.h((*f).srcBuffer), s.h((*f).dstImage));
        let regions: *const VkBufferImageCopy = rt::at(record, (*f).pRegions);
        if s.compressed.contains_key(&image) {
            buffer_to_image(s, buffer, image, (*f).dstImageLayout, core::slice::from_raw_parts(regions, (*f).regionCount as usize));
        } else {
            (s.fns.vkCmdCopyBufferToImage)(s.cb, buffer, image, (*f).dstImageLayout, (*f).regionCount, regions);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdCopyBufferToImage2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyBufferToImage2>();
        let info: *mut VkCopyBufferToImageInfo2 = rt::at(record, (*f).pCopyBufferToImageInfo);
        patch_VkCopyBufferToImageInfo2(s, record, info);
        let i = &*info;
        if s.compressed.contains_key(&i.dstImage) {
            let regions: Vec<VkBufferImageCopy> = (0..i.regionCount as usize).map(|n| region(&*i.pRegions.add(n))).collect();
            buffer_to_image(s, i.srcBuffer, i.dstImage, i.dstImageLayout, &regions);
        } else {
            (s.fns.vkCmdCopyBufferToImage2)(s.cb, info);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdCopyImageToBuffer(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyImageToBuffer>();
        let (image, buffer) = (s.h((*f).srcImage), s.h((*f).dstBuffer));
        let regions: *const VkBufferImageCopy = rt::at(record, (*f).pRegions);
        if s.compressed.contains_key(&image) {
            image_to_buffer(s, image, buffer, core::slice::from_raw_parts(regions, (*f).regionCount as usize));
        } else {
            (s.fns.vkCmdCopyImageToBuffer)(s.cb, image, (*f).srcImageLayout, buffer, (*f).regionCount, regions);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdCopyImageToBuffer2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyImageToBuffer2>();
        let info: *mut VkCopyImageToBufferInfo2 = rt::at(record, (*f).pCopyImageToBufferInfo);
        patch_VkCopyImageToBufferInfo2(s, record, info);
        let i = &*info;
        if s.compressed.contains_key(&i.srcImage) {
            let regions: Vec<VkBufferImageCopy> = (0..i.regionCount as usize).map(|n| region(&*i.pRegions.add(n))).collect();
            image_to_buffer(s, i.srcImage, i.dstBuffer, &regions);
        } else {
            (s.fns.vkCmdCopyImageToBuffer2)(s.cb, info);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdCopyImage(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyImage>();
        let (src, dst) = (s.h((*f).srcImage), s.h((*f).dstImage));
        let regions: *const VkImageCopy = rt::at(record, (*f).pRegions);
        if s.compressed.contains_key(&src) || s.compressed.contains_key(&dst) {
            image_to_image(s, src, (*f).srcImageLayout, dst, (*f).dstImageLayout, core::slice::from_raw_parts(regions, (*f).regionCount as usize));
        } else {
            (s.fns.vkCmdCopyImage)(s.cb, src, (*f).srcImageLayout, dst, (*f).dstImageLayout, (*f).regionCount, regions);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkCmdCopyImage2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdCopyImage2>();
        let info: *mut VkCopyImageInfo2 = rt::at(record, (*f).pCopyImageInfo);
        patch_VkCopyImageInfo2(s, record, info);
        let i = &*info;
        if s.compressed.contains_key(&i.srcImage) || s.compressed.contains_key(&i.dstImage) {
            let regions: Vec<VkImageCopy> = (0..i.regionCount as usize)
                .map(|n| {
                    let r = &*i.pRegions.add(n);
                    VkImageCopy { srcSubresource: r.srcSubresource, srcOffset: r.srcOffset, dstSubresource: r.dstSubresource, dstOffset: r.dstOffset, extent: r.extent }
                })
                .collect();
            image_to_image(s, i.srcImage, i.srcImageLayout, i.dstImage, i.dstImageLayout, &regions);
        } else {
            (s.fns.vkCmdCopyImage2)(s.cb, info);
        }
        rt::next(record)
    }
}

