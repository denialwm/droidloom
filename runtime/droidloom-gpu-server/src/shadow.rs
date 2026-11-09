//! Gralloc buffers the application draws into: window buffers and
//! AHardwareBuffers.
//!
//! A gralloc buffer in VRAM, laid out in one of the GPU's own DRM modifiers,
//! is the application's image itself and never comes here. The others are
//! linear, and NVIDIA loses the device when a render
//! pass draws into a linear colour image with a depth or stencil attachment.
//! The application's image is therefore a device-local optimal one, and the
//! gralloc buffer a second image beside it. A window buffer's frame is copied
//! into the buffer when it is handed to the compositor; an AHardwareBuffer's
//! content crosses when the application hands it to or takes it from the
//! foreign queue. Which images get a shadow (`wanted`, `wanted_foreign`) and
//! what a hand-over does is decided here only.
//!
//! YUV AHardwareBuffers, which video decoders fill, take the same path the
//! other way round: NVIDIA imports no linear DMA-BUF into a multi-planar
//! image, so each plane is a linear image of its own over the buffer, copied
//! into its plane of the optimal image when the application takes the
//! buffer (`wanted_yuv`).

use std::os::fd::RawFd;

use droidloom_gpu_protocol::generated::frame;
use droidloom_gpu_protocol::generated::vk::*;

use crate::generated::{patch_VkBufferMemoryBarrier, patch_VkDependencyInfo, patch_VkImageMemoryBarrier, patch_VkMemoryBarrier, Fns};
use crate::rt::{self, Server};

const COLOR_ATTACHMENT: u32 = 0x10;
const TRANSFER_SRC: u32 = 0x1;
const TRANSFER_DST: u32 = 0x2;
const DEVICE_LOCAL: u32 = 0x1;
const OPTIMAL: i32 = 0;
/// `DRM_FORMAT_MOD_LINEAR`.
const LINEAR: u64 = 0;
const SIMULTANEOUS_USE: u32 = 0x4;
const PRIMARY: i32 = 0;
const SYNC_FD: u32 = 0x10;
const COLOR: u32 = 0x1;
const PLANE_0: u32 = 0x10;

const UNDEFINED: i32 = 0;
const GENERAL: i32 = 1;
const TRANSFER_SRC_OPTIMAL: i32 = 6;
const TRANSFER_DST_OPTIMAL: i32 = 7;
const PRESENT_SRC: i32 = 1000001002;

// vkCmdPipelineBarrier stages and accesses: synchronization2 is the
// application's to enable.
const ALL_COMMANDS: u32 = 0x10000;
const TRANSFER: u32 = 0x1000;
const TRANSFER_READ: u32 = 0x800;
const TRANSFER_WRITE: u32 = 0x1000;
const MEMORY_READ: u32 = 0x8000;
const MEMORY_WRITE: u32 = 0x10000;

const QUEUE_FAMILY_EXTERNAL: u32 = !1;
const QUEUE_FAMILY_FOREIGN: u32 = !2;
const IGNORED: u32 = !0;
const REMAINING: u32 = !0;

pub struct Shadow {
    /// The gralloc buffer as a linear image, and the memory it imported.
    buffer: VkImage,
    buffer_memory: VkDeviceMemory,
    /// Memory of the optimal image the application sees.
    memory: VkDeviceMemory,
    extent: VkExtent3D,
    /// The hand-off copy, recorded once per queue family it runs on.
    copies: Vec<Copy>,
}

struct Copy {
    family: u32,
    pool: VkCommandPool,
    commands: VkCommandBuffer,
}

/// An AHardwareBuffer image, under the host handle of the optimal image the
/// application sees.
pub struct Foreign {
    /// The buffer as a linear image; the application allocates and binds
    /// its memory.
    buffer: VkImage,
    /// Memory of the optimal image.
    memory: VkDeviceMemory,
    extent: VkExtent3D,
    levels: u32,
    layers: u32,
    /// A YUV image's planes after the first, which is `buffer`.
    planes: Vec<Plane>,
}

/// A plane of a YUV buffer as a linear image, with its own import of the
/// buffer: NVIDIA dedicates each DMA-BUF import to one image.
struct Plane {
    image: VkImage,
    memory: VkDeviceMemory,
    extent: VkExtent3D,
}

/// The single-plane format of each plane of a YUV format, and how much
/// smaller than the image the plane is.
fn planes_of(format: VkFormat) -> Option<&'static [(VkFormat, u32)]> {
    match format {
        // NV12, G8_B8R8_2PLANE_420_UNORM: R8, R8G8.
        1000156003 => Some(&[(9, 1), (16, 2)]),
        // YV12, G8_B8_R8_3PLANE_420_UNORM: R8 each.
        1000156002 => Some(&[(9, 1), (9, 2), (9, 2)]),
        // P010, G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16: R10X6, R10X6G10X6.
        1000156013 => Some(&[(1000156007, 1), (1000156008, 2)]),
        _ => None,
    }
}

/// Whether `info` lays its buffer out LINEAR.
unsafe fn linear(info: &VkImageCreateInfo) -> bool {
    unsafe {
        crate::manual::find::<VkImageDrmFormatModifierExplicitCreateInfoEXT>(info.pNext, VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT)
            .is_some_and(|explicit| (*explicit).drmFormatModifier == LINEAR)
    }
}

/// A linear window buffer the application renders into.
pub unsafe fn wanted(info: &VkImageCreateInfo) -> bool {
    unsafe { info.usage & COLOR_ATTACHMENT != 0 && linear(info) }
}

/// Create the application's image `id` over the gralloc buffer `fd`: the
/// buffer as a linear copy target, the image itself optimal in VRAM.
/// `info` still carries the buffer's modifier chain.
pub unsafe fn create(s: &mut Server, device: usize, info: *mut VkImageCreateInfo, fd: RawFd, id: u64) -> VkImage {
    unsafe {
        let usage = (*info).usage;
        (*info).usage = TRANSFER_DST;
        let buffer = crate::manual::create_image(s, device, info);
        let buffer_memory = crate::manual::import_gralloc(s, device, buffer, fd);

        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT);
        (*info).usage = usage | TRANSFER_SRC;
        (*info).tiling = OPTIMAL;
        let image = crate::manual::create_image(s, device, info);
        let memory = local_memory(s, device, image);
        s.shadows.insert(id, Shadow { buffer, buffer_memory, memory, extent: (*info).extent, copies: Vec::new() });
        image
    }
}

/// A linear AHardwareBuffer the application renders into: it comes with
/// its modifier and without memory, which the application imports later.
pub unsafe fn wanted_foreign(info: &VkImageCreateInfo) -> bool {
    unsafe { info.usage & COLOR_ATTACHMENT != 0 && linear(info) }
}

/// Create an AHardwareBuffer image: the buffer as a linear image the
/// application's memory goes to, the image it sees optimal in VRAM.
pub unsafe fn create_foreign(s: &mut Server, device: usize, info: *mut VkImageCreateInfo) -> VkImage {
    unsafe {
        let usage = (*info).usage;
        (*info).usage = TRANSFER_SRC | TRANSFER_DST;
        let buffer = crate::manual::create_image(s, device, info);

        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT);
        (*info).usage = usage | TRANSFER_SRC | TRANSFER_DST;
        (*info).tiling = OPTIMAL;
        let image = crate::manual::create_image(s, device, info);
        let memory = local_memory(s, device, image);
        let foreign = Foreign { buffer, memory, extent: (*info).extent, levels: (*info).mipLevels, layers: (*info).arrayLayers, planes: Vec::new() };
        s.foreign.insert(image, foreign);
        image
    }
}

/// A YUV AHardwareBuffer: it comes with its modifier and one layout per
/// plane, without memory.
pub unsafe fn wanted_yuv(info: &VkImageCreateInfo) -> bool {
    unsafe {
        planes_of(info.format).is_some()
            && crate::manual::find::<rt::Base>(info.pNext, VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT).is_some()
    }
}

/// Create a YUV AHardwareBuffer image: each plane a linear image at its
/// layout, the first taking the application's memory, and the image it
/// samples optimal in VRAM.
pub unsafe fn create_yuv(s: &mut Server, device: usize, info: *mut VkImageCreateInfo) -> VkImage {
    unsafe {
        let formats = planes_of((*info).format).unwrap_or_else(|| rt::fail("not a YUV format"));
        let explicit = crate::manual::find::<VkImageDrmFormatModifierExplicitCreateInfoEXT>((*info).pNext, VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT)
            .unwrap_or_else(|| rt::fail("a YUV buffer without its layout"))
            .cast_mut();
        if (*explicit).drmFormatModifierPlaneCount as usize != formats.len() {
            rt::fail(&format!("a YUV buffer of format {} with {} planes", (*info).format, (*explicit).drmFormatModifierPlaneCount));
        }
        let layouts = core::slice::from_raw_parts((*explicit).pPlaneLayouts, formats.len()).to_vec();
        let (format, extent, usage, flags) = ((*info).format, (*info).extent, (*info).usage, (*info).flags);
        let mut planes: Vec<Plane> = Vec::with_capacity(formats.len());
        for (layout, &(plane_format, divisor)) in layouts.iter().zip(formats) {
            let plane_extent = VkExtent3D { width: extent.width.div_ceil(divisor), height: extent.height.div_ceil(divisor), depth: 1 };
            (*info).format = plane_format;
            (*info).extent = plane_extent;
            (*info).usage = TRANSFER_SRC;
            (*info).flags = 0;
            (*explicit).drmFormatModifierPlaneCount = 1;
            (*explicit).pPlaneLayouts = layout;
            planes.push(Plane { image: crate::manual::create_image(s, device, info), memory: 0, extent: plane_extent });
        }

        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT);
        (*info).format = format;
        (*info).extent = extent;
        (*info).usage = usage | TRANSFER_DST;
        (*info).flags = flags;
        (*info).tiling = OPTIMAL;
        let image = crate::manual::create_image(s, device, info);
        let memory = local_memory(s, device, image);
        let buffer = planes.remove(0).image;
        let foreign = Foreign { buffer, memory, extent, levels: 1, layers: 1, planes };
        s.foreign.insert(image, foreign);
        image
    }
}

/// Memory dedicated to a YUV image goes to its first plane; each other
/// plane imports the same DMA-BUF again.
pub unsafe fn import_planes(s: &mut Server, device: usize, info: *const VkMemoryAllocateInfo) {
    unsafe {
        let Some(dedicated) = crate::manual::find::<VkMemoryDedicatedAllocateInfo>((*info).pNext, VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO) else { return };
        let Some(foreign) = s.foreign.get(&(*dedicated).image) else { return };
        if foreign.planes.is_empty() {
            return;
        }
        let Some(import) = crate::manual::find::<VkImportMemoryFdInfoKHR>((*info).pNext, VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR) else {
            rt::fail("YUV AHardwareBuffer memory that imports no DMA-BUF");
        };
        let images: Vec<VkImage> = foreign.planes.iter().map(|plane| plane.image).collect();
        let memories: Vec<VkDeviceMemory> = images
            .iter()
            .map(|&image| {
                let fd = libc::dup((*import).fd);
                if fd < 0 {
                    rt::fail("duplicate a YUV buffer's descriptor");
                }
                crate::manual::import_gralloc(s, device, image, fd)
            })
            .collect();
        let foreign = s.foreign.get_mut(&(*dedicated).image).unwrap_or_else(|| rt::fail("YUV image vanished"));
        for (plane, memory) in foreign.planes.iter_mut().zip(memories) {
            plane.memory = memory;
        }
    }
}

/// The answer for a linear AHardwareBuffer the application renders into,
/// as `create_foreign` makes it: the buffer imports as a linear copy target,
/// and the optimal image does what the application asked for.
pub unsafe fn format_properties(
    s: &Server,
    physical: usize,
    info: *mut VkPhysicalDeviceImageFormatInfo2,
    properties: *mut VkImageFormatProperties2,
) -> VkResult {
    unsafe {
        let query = s.fns.vkGetPhysicalDeviceImageFormatProperties2;
        let modifier = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT;
        let layout = crate::manual::find::<VkPhysicalDeviceImageDrmFormatModifierInfoEXT>((*info).pNext, modifier);
        if (*info).usage & COLOR_ATTACHMENT == 0 || layout.is_none_or(|layout| (*layout).drmFormatModifier != LINEAR) {
            return query(physical, info, properties);
        }
        let usage = (*info).usage;
        (*info).usage = TRANSFER_SRC | TRANSFER_DST;
        let result = query(physical, info, properties);
        if result != VK_SUCCESS {
            return result;
        }
        let linear = (*properties).imageFormatProperties;
        let external = crate::manual::find::<VkExternalImageFormatProperties>((*properties).pNext, VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES);
        let memory = external.map(|external| (*external).externalMemoryProperties);

        crate::manual::unlink((&raw mut (*info).pNext).cast(), VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO);
        crate::manual::unlink((&raw mut (*info).pNext).cast(), modifier);
        (*info).usage = usage | TRANSFER_SRC | TRANSFER_DST;
        (*info).tiling = OPTIMAL;
        let result = query(physical, info, properties);
        if result != VK_SUCCESS {
            return result;
        }
        if let (Some(external), Some(memory)) = (external, memory) {
            (*external.cast_mut()).externalMemoryProperties = memory;
        }
        let optimal = &mut (*properties).imageFormatProperties;
        optimal.maxExtent.width = optimal.maxExtent.width.min(linear.maxExtent.width);
        optimal.maxExtent.height = optimal.maxExtent.height.min(linear.maxExtent.height);
        optimal.maxExtent.depth = optimal.maxExtent.depth.min(linear.maxExtent.depth);
        optimal.maxMipLevels = optimal.maxMipLevels.min(linear.maxMipLevels);
        optimal.maxArrayLayers = optimal.maxArrayLayers.min(linear.maxArrayLayers);
        optimal.sampleCounts &= linear.sampleCounts;
        VK_SUCCESS
    }
}

pub unsafe fn destroy_foreign(s: &Server, device: usize, foreign: Foreign) {
    unsafe {
        (s.fns.vkDestroyImage)(device, foreign.buffer, core::ptr::null());
        (s.fns.vkFreeMemory)(device, foreign.memory, core::ptr::null());
        for plane in foreign.planes {
            (s.fns.vkDestroyImage)(device, plane.image, core::ptr::null());
            (s.fns.vkFreeMemory)(device, plane.memory, core::ptr::null());
        }
    }
}

/// The image the application's memory belongs to.
fn bound(s: &Server, image: VkImage) -> VkImage {
    s.foreign.get(&image).map_or(image, |foreign| foreign.buffer)
}

/// An allocation dedicated to an AHardwareBuffer image goes to its buffer.
pub unsafe fn dedicate(s: &Server, info: *const VkMemoryAllocateInfo) {
    unsafe {
        if let Some(dedicated) = crate::manual::find::<VkMemoryDedicatedAllocateInfo>((*info).pNext, VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO) {
            let dedicated = dedicated.cast_mut();
            (*dedicated).image = bound(s, (*dedicated).image);
        }
    }
}

pub unsafe fn h_vkGetImageMemoryRequirements(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetImageMemoryRequirements>();
        let requirements: *mut VkMemoryRequirements = rt::at(record, (*f).pMemoryRequirements);
        (s.fns.vkGetImageMemoryRequirements)(s.h((*f).device) as usize, bound(s, s.h((*f).image)), requirements);
        rt::next(record)
    }
}

pub unsafe fn h_vkGetImageMemoryRequirements2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkGetImageMemoryRequirements2>();
        let info: *mut VkImageMemoryRequirementsInfo2 = rt::at(record, (*f).pInfo);
        crate::generated::patch_VkImageMemoryRequirementsInfo2(s, record, info);
        (*info).image = bound(s, (*info).image);
        let requirements: *mut VkMemoryRequirements2 = rt::at(record, (*f).pMemoryRequirements);
        crate::generated::link_chain(record, (&raw mut (*requirements).pNext).cast());
        (s.fns.vkGetImageMemoryRequirements2)(s.h((*f).device) as usize, info, requirements);
        rt::next(record)
    }
}

pub unsafe fn h_vkBindImageMemory(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkBindImageMemory>();
        let image = bound(s, s.h((*f).image));
        let result = (s.fns.vkBindImageMemory)(s.h((*f).device) as usize, image, s.h((*f).memory), (*f).memoryOffset);
        if result != VK_SUCCESS {
            rt::failed("vkBindImageMemory", result);
        }
        rt::next(record)
    }
}

pub unsafe fn h_vkBindImageMemory2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkBindImageMemory2>();
        let binds: *mut VkBindImageMemoryInfo = rt::at(record, (*f).pBindInfos);
        for i in 0..(*f).bindInfoCount as usize {
            crate::generated::patch_VkBindImageMemoryInfo(s, record, binds.add(i));
            (*binds.add(i)).image = bound(s, (*binds.add(i)).image);
        }
        let result = (s.fns.vkBindImageMemory2)(s.h((*f).device) as usize, (*f).bindInfoCount, binds);
        if result != VK_SUCCESS {
            rt::failed("vkBindImageMemory2", result);
        }
        rt::next(record)
    }
}

/// A queue family outside this device: the AHardwareBuffer's other users.
fn outside(family: u32) -> bool {
    family == QUEUE_FAMILY_FOREIGN || family == QUEUE_FAMILY_EXTERNAL
}

/// Whether barrier `image`, `src`, `dst` hands an AHardwareBuffer image over.
fn hands_over(s: &Server, image: VkImage, src: u32, dst: u32) -> bool {
    (outside(src) || outside(dst)) && s.foreign.contains_key(&image)
}

/// `vkCmdPipelineBarrier`: hand-overs of AHardwareBuffer images become
/// copies between the image and its buffer (`hand_over`); the rest passes.
pub unsafe fn h_vkCmdPipelineBarrier(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdPipelineBarrier>();
        let memory: *mut VkMemoryBarrier = rt::at(record, (*f).pMemoryBarriers);
        for i in 0..(*f).memoryBarrierCount as usize {
            patch_VkMemoryBarrier(s, record, memory.add(i));
        }
        let buffers: *mut VkBufferMemoryBarrier = rt::at(record, (*f).pBufferMemoryBarriers);
        for i in 0..(*f).bufferMemoryBarrierCount as usize {
            patch_VkBufferMemoryBarrier(s, record, buffers.add(i));
        }
        let images: *mut VkImageMemoryBarrier = rt::at(record, (*f).pImageMemoryBarriers);
        for i in 0..(*f).imageMemoryBarrierCount as usize {
            patch_VkImageMemoryBarrier(s, record, images.add(i));
        }
        let images = core::slice::from_raw_parts(images, (*f).imageMemoryBarrierCount as usize);
        let (src, dst) = ((*f).srcStageMask, (*f).dstStageMask);
        if !images.iter().any(|b| hands_over(s, b.image, b.srcQueueFamilyIndex, b.dstQueueFamilyIndex)) {
            (s.fns.vkCmdPipelineBarrier)(s.cb, src, dst, (*f).dependencyFlags, (*f).memoryBarrierCount, memory, (*f).bufferMemoryBarrierCount, buffers, images.len() as u32, images.as_ptr());
            return rt::next(record);
        }
        let (over, rest): (Vec<VkImageMemoryBarrier>, Vec<VkImageMemoryBarrier>) =
            images.iter().partition(|b| hands_over(s, b.image, b.srcQueueFamilyIndex, b.dstQueueFamilyIndex));
        (s.fns.vkCmdPipelineBarrier)(s.cb, src, dst, (*f).dependencyFlags, (*f).memoryBarrierCount, memory, (*f).bufferMemoryBarrierCount, buffers, rest.len() as u32, rest.as_ptr());
        for b in &over {
            let app = VkImageMemoryBarrier2 {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2,
                pNext: core::ptr::null(),
                srcStageMask: src.into(),
                srcAccessMask: b.srcAccessMask.into(),
                dstStageMask: dst.into(),
                dstAccessMask: b.dstAccessMask.into(),
                oldLayout: b.oldLayout,
                newLayout: b.newLayout,
                srcQueueFamilyIndex: b.srcQueueFamilyIndex,
                dstQueueFamilyIndex: b.dstQueueFamilyIndex,
                image: b.image,
                subresourceRange: b.subresourceRange,
            };
            hand_over(s, false, &app);
        }
        rt::next(record)
    }
}

/// `vkCmdPipelineBarrier2`, as `h_vkCmdPipelineBarrier`.
pub unsafe fn h_vkCmdPipelineBarrier2(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkCmdPipelineBarrier2>();
        let info: *mut VkDependencyInfo = rt::at(record, (*f).pDependencyInfo);
        patch_VkDependencyInfo(s, record, info);
        let images = core::slice::from_raw_parts((*info).pImageMemoryBarriers, (*info).imageMemoryBarrierCount as usize);
        if !images.iter().any(|b| hands_over(s, b.image, b.srcQueueFamilyIndex, b.dstQueueFamilyIndex)) {
            (s.fns.vkCmdPipelineBarrier2)(s.cb, info);
            return rt::next(record);
        }
        let (over, rest): (Vec<VkImageMemoryBarrier2>, Vec<VkImageMemoryBarrier2>) =
            images.iter().partition(|b| hands_over(s, b.image, b.srcQueueFamilyIndex, b.dstQueueFamilyIndex));
        (*info).imageMemoryBarrierCount = rest.len() as u32;
        (*info).pImageMemoryBarriers = rest.as_ptr();
        (s.fns.vkCmdPipelineBarrier2)(s.cb, info);
        for b in &over {
            hand_over(s, true, b);
        }
        rt::next(record)
    }
}

/// The application's barrier `app` hands an AHardwareBuffer image to or
/// takes it from outside the device. Taking it copies the buffer into the
/// image, even from UNDEFINED: Chromium takes buffers GL drew that way,
/// and drivers whose image is the buffer keep its content. Handing it
/// copies the image into the buffer, unless it comes from UNDEFINED. The
/// buffer carries the queue family transfer; the image stays with the
/// application's families.
unsafe fn hand_over(s: &Server, v2: bool, app: &VkImageMemoryBarrier2) {
    unsafe {
        let foreign = &s.foreign[&app.image];
        if !foreign.planes.is_empty() {
            return hand_over_yuv(s, v2, app, foreign);
        }
        let mut range = app.subresourceRange;
        if range.levelCount == REMAINING {
            range.levelCount = foreign.levels - range.baseMipLevel;
        }
        if range.layerCount == REMAINING {
            range.layerCount = foreign.layers - range.baseArrayLayer;
        }
        let image = |from, to, src: (u64, u64), dst: (u64, u64)| VkImageMemoryBarrier2 {
            srcStageMask: src.0,
            srcAccessMask: src.1,
            dstStageMask: dst.0,
            dstAccessMask: dst.1,
            oldLayout: from,
            newLayout: to,
            srcQueueFamilyIndex: IGNORED,
            dstQueueFamilyIndex: IGNORED,
            subresourceRange: range,
            ..*app
        };
        let buffer = |from, to, src: (u64, u64), dst: (u64, u64)| VkImageMemoryBarrier2 {
            image: foreign.buffer,
            srcQueueFamilyIndex: app.srcQueueFamilyIndex,
            dstQueueFamilyIndex: app.dstQueueFamilyIndex,
            ..image(from, to, src, dst)
        };
        let (from, to) = ((app.srcStageMask, app.srcAccessMask), (app.dstStageMask, app.dstAccessMask));
        // The last copy's transfer is part of what each first step waits for.
        let after = (app.srcStageMask | u64::from(TRANSFER), app.srcAccessMask);
        let read = (u64::from(TRANSFER), u64::from(TRANSFER_READ));
        let write = (u64::from(TRANSFER), u64::from(TRANSFER_WRITE));
        let (src, dst) = (TRANSFER_SRC_OPTIMAL, TRANSFER_DST_OPTIMAL);
        if outside(app.srcQueueFamilyIndex) {
            emit(&s.fns, s.cb, v2, &[buffer(app.oldLayout, src, after, read), image(UNDEFINED, dst, after, write)]);
            copy(&s.fns, s.cb, foreign, range, foreign.buffer, app.image);
            emit(&s.fns, s.cb, v2, &[image(dst, app.newLayout, write, to)]);
        } else if app.oldLayout == UNDEFINED {
            emit(&s.fns, s.cb, v2, &[buffer(UNDEFINED, app.newLayout, from, to)]);
        } else {
            emit(&s.fns, s.cb, v2, &[image(app.oldLayout, src, after, read), buffer(UNDEFINED, dst, after, write)]);
            copy(&s.fns, s.cb, foreign, range, app.image, foreign.buffer);
            emit(&s.fns, s.cb, v2, &[buffer(dst, app.newLayout, write, to)]);
        }
    }
}

/// A YUV image's hand-over. Taking it copies each plane into its plane of
/// the image. Handing it back copies nothing: the application only
/// samples it, and the buffer carries the queue family transfer alone.
unsafe fn hand_over_yuv(s: &Server, v2: bool, app: &VkImageMemoryBarrier2, foreign: &Foreign) {
    unsafe {
        let range = VkImageSubresourceRange { aspectMask: COLOR, baseMipLevel: 0, levelCount: 1, baseArrayLayer: 0, layerCount: 1 };
        let planes: Vec<(VkImage, VkExtent3D)> =
            core::iter::once((foreign.buffer, foreign.extent)).chain(foreign.planes.iter().map(|plane| (plane.image, plane.extent))).collect();
        let plane = |image, from, to, src: (u64, u64), dst: (u64, u64)| VkImageMemoryBarrier2 {
            srcStageMask: src.0,
            srcAccessMask: src.1,
            dstStageMask: dst.0,
            dstAccessMask: dst.1,
            oldLayout: from,
            newLayout: to,
            image,
            subresourceRange: range,
            ..*app
        };
        let (from, to) = ((app.srcStageMask, app.srcAccessMask), (app.dstStageMask, app.dstAccessMask));
        if !outside(app.srcQueueFamilyIndex) {
            let release: Vec<_> = planes.iter().map(|&(image, _)| plane(image, UNDEFINED, app.newLayout, from, to)).collect();
            emit(&s.fns, s.cb, v2, &release);
            return;
        }
        let after = (app.srcStageMask | u64::from(TRANSFER), app.srcAccessMask);
        let read = (u64::from(TRANSFER), u64::from(TRANSFER_READ));
        let write = (u64::from(TRANSFER), u64::from(TRANSFER_WRITE));
        let image = |from, to, src: (u64, u64), dst: (u64, u64)| VkImageMemoryBarrier2 {
            srcQueueFamilyIndex: IGNORED,
            dstQueueFamilyIndex: IGNORED,
            ..plane(app.image, from, to, src, dst)
        };
        let mut take: Vec<_> = planes.iter().map(|&(image, _)| plane(image, app.oldLayout, TRANSFER_SRC_OPTIMAL, after, read)).collect();
        take.push(image(UNDEFINED, TRANSFER_DST_OPTIMAL, after, write));
        emit(&s.fns, s.cb, v2, &take);
        for (index, &(source, extent)) in planes.iter().enumerate() {
            let layers = |aspectMask| VkImageSubresourceLayers { aspectMask, mipLevel: 0, baseArrayLayer: 0, layerCount: 1 };
            let region = VkImageCopy {
                srcSubresource: layers(COLOR),
                srcOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                dstSubresource: layers(PLANE_0 << index),
                dstOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                extent,
            };
            (s.fns.vkCmdCopyImage)(s.cb, source, TRANSFER_SRC_OPTIMAL, app.image, TRANSFER_DST_OPTIMAL, 1, &raw const region);
        }
        emit(&s.fns, s.cb, v2, &[image(TRANSFER_DST_OPTIMAL, app.newLayout, write, to)]);
    }
}

/// Record `barriers` the way the application records its own: legacy
/// barriers carry stage and access values both forms share.
unsafe fn emit(fns: &Fns, commands: VkCommandBuffer, v2: bool, barriers: &[VkImageMemoryBarrier2]) {
    unsafe {
        if v2 {
            let info = VkDependencyInfo {
                sType: VK_STRUCTURE_TYPE_DEPENDENCY_INFO,
                pNext: core::ptr::null(),
                dependencyFlags: 0,
                memoryBarrierCount: 0,
                pMemoryBarriers: core::ptr::null(),
                bufferMemoryBarrierCount: 0,
                pBufferMemoryBarriers: core::ptr::null(),
                imageMemoryBarrierCount: barriers.len() as u32,
                pImageMemoryBarriers: barriers.as_ptr(),
            };
            (fns.vkCmdPipelineBarrier2)(commands, &raw const info);
            return;
        }
        // At most a YUV image's three planes and the image.
        let mut legacy = [core::mem::zeroed::<VkImageMemoryBarrier>(); 4];
        let (mut src, mut dst) = (0, 0);
        for (out, b) in legacy.iter_mut().zip(barriers) {
            *out = VkImageMemoryBarrier {
                srcQueueFamilyIndex: b.srcQueueFamilyIndex,
                dstQueueFamilyIndex: b.dstQueueFamilyIndex,
                ..barrier(b.image, b.subresourceRange, b.oldLayout, b.newLayout, b.srcAccessMask as u32, b.dstAccessMask as u32)
            };
            src |= b.srcStageMask as u32;
            dst |= b.dstStageMask as u32;
        }
        dependency(fns, commands, &legacy[..barriers.len()], src, dst);
    }
}

/// Copy `range` of an AHardwareBuffer image between it and its buffer.
unsafe fn copy(fns: &Fns, commands: VkCommandBuffer, foreign: &Foreign, range: VkImageSubresourceRange, from: VkImage, to: VkImage) {
    unsafe {
        let regions: Vec<VkImageCopy> = (range.baseMipLevel..range.baseMipLevel + range.levelCount)
            .map(|level| {
                let layers = VkImageSubresourceLayers { aspectMask: COLOR, mipLevel: level, baseArrayLayer: range.baseArrayLayer, layerCount: range.layerCount };
                let e = foreign.extent;
                VkImageCopy {
                    srcSubresource: layers,
                    srcOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                    dstSubresource: layers,
                    dstOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                    extent: VkExtent3D { width: (e.width >> level).max(1), height: (e.height >> level).max(1), depth: 1 },
                }
            })
            .collect();
        (fns.vkCmdCopyImage)(commands, from, TRANSFER_SRC_OPTIMAL, to, TRANSFER_DST_OPTIMAL, regions.len() as u32, regions.as_ptr());
    }
}

/// Dedicated memory of the lowest device-local type, bound to `image`.
unsafe fn local_memory(s: &Server, device: usize, image: VkImage) -> VkDeviceMemory {
    unsafe {
        let mut requirements = core::mem::zeroed::<VkMemoryRequirements>();
        (s.fns.vkGetImageMemoryRequirements)(device, image, &raw mut requirements);
        // The lowest device-local type: NVIDIA's type 0 is system memory.
        let local = (0..32).find(|&i| requirements.memoryTypeBits & (1 << i) != 0 && s.memory_types[i] & DEVICE_LOCAL != 0);
        let Some(index) = local else { rt::fail("no device-local memory for a window buffer") };
        let dedicated = VkMemoryDedicatedAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            pNext: core::ptr::null(),
            image,
            buffer: 0,
        };
        let allocate = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: (&raw const dedicated).cast(),
            allocationSize: requirements.size,
            memoryTypeIndex: index as u32,
        };
        let mut memory: VkDeviceMemory = 0;
        let result = (s.fns.vkAllocateMemory)(device, &raw const allocate, core::ptr::null(), &raw mut memory);
        if result != VK_SUCCESS {
            rt::failed("vkAllocateMemory for a window buffer", result);
        }
        let result = (s.fns.vkBindImageMemory)(device, image, memory, 0);
        if result != VK_SUCCESS {
            rt::failed("vkBindImageMemory for a window buffer", result);
        }
        memory
    }
}

/// Free the buffer side of image `image`, which the caller destroys.
pub unsafe fn destroy(s: &mut Server, device: usize, shadow: Shadow) {
    unsafe {
        if !shadow.copies.is_empty() {
            // The last hand-off may still run; the compositor does not wait
            // for it before the application destroys its swapchain.
            let _ = (s.fns.vkDeviceWaitIdle)(device);
        }
        for copy in &shadow.copies {
            (s.fns.vkDestroyCommandPool)(device, copy.pool, core::ptr::null());
        }
        (s.fns.vkDestroyImage)(device, shadow.buffer, core::ptr::null());
        (s.fns.vkFreeMemory)(device, shadow.buffer_memory, core::ptr::null());
        (s.fns.vkFreeMemory)(device, shadow.memory, core::ptr::null());
    }
}

/// `vkQueueSignalReleaseImageANDROID`: once the waits are done, bring the
/// buffer up to date and signal the queue's release semaphore, exported as
/// the compositor's sync file.
pub unsafe fn h_vkQueueSignalReleaseImageANDROID(s: &mut Server, record: *mut u8) -> *mut u8 {
    unsafe {
        let f = record.add(8).cast::<frame::vkQueueSignalReleaseImageANDROID>();
        let queue = s.h((*f).queue) as usize;
        let timeline = s.timelines.get(&(*f).queue).unwrap_or_else(|| rt::fail("release on an unknown queue"));
        let (device, family, release) = (timeline.device, timeline.family, timeline.release);
        let ids: *mut u64 = rt::at(record, (*f).pWaitSemaphores);
        let waits: Vec<VkSemaphoreSubmitInfo> = (0..(*f).waitSemaphoreCount as usize).map(|i| semaphore_info(s.h(*ids.add(i)))).collect();
        let image = s.h((*f).image);
        let commands = match s.shadows.get_mut(&(*f).image) {
            Some(shadow) => copy_commands(&s.fns, device, family, image, shadow),
            None => 0,
        };
        let command = VkCommandBufferSubmitInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
            pNext: core::ptr::null(),
            commandBuffer: commands,
            deviceMask: 0,
        };
        let signal = semaphore_info(release);
        let submit = VkSubmitInfo2 {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
            pNext: core::ptr::null(),
            flags: 0,
            waitSemaphoreInfoCount: waits.len() as u32,
            pWaitSemaphoreInfos: waits.as_ptr(),
            commandBufferInfoCount: u32::from(commands != 0),
            pCommandBufferInfos: &raw const command,
            signalSemaphoreInfoCount: 1,
            pSignalSemaphoreInfos: &raw const signal,
        };
        let result = (s.fns.vkQueueSubmit2)(queue, 1, &raw const submit, 0);
        if result != VK_SUCCESS {
            rt::failed("vkQueueSubmit2 for a release", result);
        }
        let info = VkSemaphoreGetFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
            pNext: core::ptr::null(),
            semaphore: release,
            handleType: SYNC_FD as i32,
        };
        let fd: *mut core::ffi::c_int = rt::at(record, (*f).pNativeFenceFd);
        let result = (s.fns.vkGetSemaphoreFdKHR)(device, &raw const info, fd);
        *rt::at::<VkResult>(record, (*f).reply) = result;
        s.send_fd(*fd);
        rt::next(record)
    }
}

fn semaphore_info(semaphore: VkSemaphore) -> VkSemaphoreSubmitInfo {
    VkSemaphoreSubmitInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
        pNext: core::ptr::null(),
        semaphore,
        value: 0,
        stageMask: ALL_COMMANDS.into(),
        deviceIndex: 0,
    }
}

/// The copy from the application's image into the buffer, recorded on first
/// use. The image arrives in PRESENT_SRC and leaves in it; the buffer ends
/// in GENERAL for the compositor.
unsafe fn copy_commands(fns: &Fns, device: usize, family: u32, image: VkImage, shadow: &mut Shadow) -> VkCommandBuffer {
    unsafe {
        if let Some(copy) = shadow.copies.iter().find(|copy| copy.family == family) {
            return copy.commands;
        }
        let info = VkCommandPoolCreateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            pNext: core::ptr::null(),
            flags: 0,
            queueFamilyIndex: family,
        };
        let mut pool: VkCommandPool = 0;
        let result = (fns.vkCreateCommandPool)(device, &raw const info, core::ptr::null(), &raw mut pool);
        if result != VK_SUCCESS {
            rt::failed("vkCreateCommandPool for a release", result);
        }
        let allocate = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: core::ptr::null(),
            commandPool: pool,
            level: PRIMARY,
            commandBufferCount: 1,
        };
        let mut commands: VkCommandBuffer = 0;
        let result = (fns.vkAllocateCommandBuffers)(device, &raw const allocate, &raw mut commands);
        if result != VK_SUCCESS {
            rt::failed("vkAllocateCommandBuffers for a release", result);
        }
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: core::ptr::null(),
            flags: SIMULTANEOUS_USE,
            pInheritanceInfo: core::ptr::null(),
        };
        (fns.vkBeginCommandBuffer)(commands, &raw const begin);
        let whole = range(0, 1, 0, 1);
        let before = [
            barrier(image, whole, PRESENT_SRC, TRANSFER_SRC_OPTIMAL, MEMORY_WRITE, TRANSFER_READ),
            barrier(shadow.buffer, whole, UNDEFINED, TRANSFER_DST_OPTIMAL, 0, TRANSFER_WRITE),
        ];
        dependency(fns, commands, &before, ALL_COMMANDS, TRANSFER);
        let layers = VkImageSubresourceLayers { aspectMask: COLOR, mipLevel: 0, baseArrayLayer: 0, layerCount: 1 };
        let region = VkImageCopy {
            srcSubresource: layers,
            srcOffset: VkOffset3D { x: 0, y: 0, z: 0 },
            dstSubresource: layers,
            dstOffset: VkOffset3D { x: 0, y: 0, z: 0 },
            extent: shadow.extent,
        };
        (fns.vkCmdCopyImage)(
            commands,
            image,
            TRANSFER_SRC_OPTIMAL,
            shadow.buffer,
            TRANSFER_DST_OPTIMAL,
            1,
            &raw const region,
        );
        let after = [
            barrier(image, whole, TRANSFER_SRC_OPTIMAL, PRESENT_SRC, 0, 0),
            barrier(shadow.buffer, whole, TRANSFER_DST_OPTIMAL, GENERAL, TRANSFER_WRITE, MEMORY_READ),
        ];
        dependency(fns, commands, &after, TRANSFER, ALL_COMMANDS);
        let result = (fns.vkEndCommandBuffer)(commands);
        if result != VK_SUCCESS {
            rt::failed("vkEndCommandBuffer for a release", result);
        }
        shadow.copies.push(Copy { family, pool, commands });
        commands
    }
}

fn range(level: u32, levels: u32, layer: u32, layers: u32) -> VkImageSubresourceRange {
    VkImageSubresourceRange { aspectMask: COLOR, baseMipLevel: level, levelCount: levels, baseArrayLayer: layer, layerCount: layers }
}

fn barrier(image: VkImage, range: VkImageSubresourceRange, from: i32, to: i32, src: u32, dst: u32) -> VkImageMemoryBarrier {
    VkImageMemoryBarrier {
        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        pNext: core::ptr::null(),
        srcAccessMask: src,
        dstAccessMask: dst,
        oldLayout: from,
        newLayout: to,
        srcQueueFamilyIndex: IGNORED,
        dstQueueFamilyIndex: IGNORED,
        image,
        subresourceRange: range,
    }
}

unsafe fn dependency(fns: &Fns, commands: VkCommandBuffer, barriers: &[VkImageMemoryBarrier], src: u32, dst: u32) {
    unsafe { (fns.vkCmdPipelineBarrier)(commands, src, dst, 0, 0, core::ptr::null(), 0, core::ptr::null(), barriers.len() as u32, barriers.as_ptr()) };
}
