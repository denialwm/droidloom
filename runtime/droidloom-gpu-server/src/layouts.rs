//! The GPU's own DRM modifiers a gralloc buffer may be laid out in.
//!
//! A buffer in VRAM in one of them is the application's image itself, and
//! reaches the compositor without a copy. Any gralloc buffer may end up in
//! front of the compositor, so Android sees only the modifiers the
//! compositor imports: with the compositor on another GPU that is none, and
//! buffers stay LINEAR in system memory.

use droidloom_gpu_protocol::generated::vk::*;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};

use crate::rt::{self, Server};

/// `DRM_FORMAT_MOD_LINEAR`: always offered, any GPU reads it.
const LINEAR: u64 = 0;

const fn fourcc(name: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*name)
}

/// The DRM formats gralloc buffers of a Vulkan format reach the compositor
/// as, with and without alpha.
fn fourccs(format: VkFormat) -> &'static [u32] {
    const RGBA8: [u32; 2] = [fourcc(b"AB24"), fourcc(b"XB24")];
    const BGRA8: [u32; 2] = [fourcc(b"AR24"), fourcc(b"XR24")];
    const RGB565: [u32; 1] = [fourcc(b"RG16")];
    const RGBA10: [u32; 2] = [fourcc(b"AB30"), fourcc(b"XB30")];
    const RGBA16F: [u32; 2] = [fourcc(b"AB4H"), fourcc(b"XB4H")];
    match format {
        // R8G8B8A8_UNORM and _SRGB.
        37 | 43 => &RGBA8,
        // B8G8R8A8_UNORM and _SRGB.
        44 | 50 => &BGRA8,
        // R5G6B5_UNORM_PACK16.
        4 => &RGB565,
        // A2B10G10R10_UNORM_PACK32.
        64 => &RGBA10,
        // R16G16B16A16_SFLOAT.
        97 => &RGBA16F,
        _ => &[],
    }
}

/// Every (DRM format, modifier) the compositor imports, as its
/// `zwp_linux_dmabuf_v1` announces them. Empty without a compositor.
pub fn imported() -> Vec<(u32, u64)> {
    #[derive(Default)]
    struct State {
        dmabuf: Option<ZwpLinuxDmabufV1>,
        imported: Vec<(u32, u64)>,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, (): &(), _: &Connection, handle: &QueueHandle<Self>) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                // Version 3 announces every format and modifier at bind.
                if interface == "zwp_linux_dmabuf_v1" && version >= 3 {
                    state.dmabuf = Some(registry.bind(name, 3, handle, ()));
                }
            }
        }
    }
    impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
        fn event(state: &mut Self, _: &ZwpLinuxDmabufV1, event: zwp_linux_dmabuf_v1::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {
            if let zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } = event {
                state.imported.push((format, (u64::from(modifier_hi) << 32) | u64::from(modifier_lo)));
            }
        }
    }
    let Ok(connection) = Connection::connect_to_env() else { return Vec::new() };
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    // The globals, then what the DMA-BUF global announces.
    for _ in 0..2 {
        if queue.roundtrip(&mut state).is_err() {
            return Vec::new();
        }
    }
    state.imported
}

/// The driver's whole modifier list of one form for `format`, in 8-byte
/// words, and the size of an entry. Each entry starts with its modifier.
unsafe fn listed(query: PFN_vkGetPhysicalDeviceFormatProperties2, physical: usize, format: VkFormat, kind: VkStructureType) -> (Vec<u64>, usize) {
    unsafe {
        let size = if kind == VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT {
            size_of::<VkDrmFormatModifierPropertiesEXT>()
        } else {
            size_of::<VkDrmFormatModifierProperties2EXT>()
        };
        let mut list = VkDrmFormatModifierPropertiesListEXT {
            sType: kind,
            pNext: core::ptr::null_mut(),
            drmFormatModifierCount: 0,
            pDrmFormatModifierProperties: core::ptr::null_mut(),
        };
        let mut properties = core::mem::zeroed::<VkFormatProperties2>();
        properties.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2;
        properties.pNext = (&raw mut list).cast();
        query(physical, format, &raw mut properties);
        let mut all = vec![0u64; list.drmFormatModifierCount as usize * size / 8];
        list.pDrmFormatModifierProperties = all.as_mut_ptr().cast();
        query(physical, format, &raw mut properties);
        all.truncate(list.drmFormatModifierCount as usize * size / 8);
        (all, size)
    }
}

/// Whether Android sees `modifier` for `format`: LINEAR, or one the
/// compositor imports every gralloc buffer of the format with.
fn offered(imported: Option<&[(u32, u64)]>, format: VkFormat, modifier: u64) -> bool {
    let Some(imported) = imported else { return true };
    let formats = fourccs(format);
    modifier == LINEAR || (!formats.is_empty() && formats.iter().all(|&format| imported.contains(&(format, modifier))))
}

/// How many of the GPU's own modifiers Android sees for RGBA buffers, for
/// the start-up line.
pub unsafe fn count(query: PFN_vkGetPhysicalDeviceFormatProperties2, physical: usize, imported: Option<&[(u32, u64)]>) -> u32 {
    const R8G8B8A8_UNORM: VkFormat = 37;
    unsafe {
        let (all, size) = listed(query, physical, R8G8B8A8_UNORM, VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT);
        all.chunks_exact(size / 8).filter(|entry| entry[0] != LINEAR && offered(imported, R8G8B8A8_UNORM, entry[0])).count() as u32
    }
}

/// A modifier list in an output chain, taken out of the driver's hands.
pub struct Asked {
    list: *mut VkDrmFormatModifierPropertiesListEXT,
    entries: *mut u8,
    capacity: u32,
}

/// Before the driver answers a format query: take the modifier lists of
/// `chain`, whose two forms differ in their entries only, and leave the
/// driver empty ones.
pub unsafe fn take(s: &Server, chain: *mut core::ffi::c_void) -> Vec<Asked> {
    unsafe {
        let mut asked = Vec::new();
        if s.imported.is_none() {
            return asked;
        }
        let mut next = chain.cast::<rt::Base>();
        while !next.is_null() {
            if matches!((*next).sType, VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT | VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_2_EXT) {
                let list = next.cast::<VkDrmFormatModifierPropertiesListEXT>();
                asked.push(Asked { list, entries: (*list).pDrmFormatModifierProperties.cast(), capacity: (*list).drmFormatModifierCount });
                (*list).drmFormatModifierCount = 0;
                (*list).pDrmFormatModifierProperties = core::ptr::null_mut();
            }
            next = (*next).pNext;
        }
        asked
    }
}

/// Answer the lists `take` took with the modifiers Android sees.
pub unsafe fn give(s: &Server, physical: usize, format: VkFormat, asked: Vec<Asked>) {
    unsafe {
        for Asked { list, entries, capacity } in asked {
            let (all, size) = listed(s.fns.vkGetPhysicalDeviceFormatProperties2, physical, format, (*list).sType);
            let mut kept = 0u32;
            for entry in all.chunks_exact(size / 8).filter(|entry| offered(s.imported.as_deref(), format, entry[0])) {
                if !entries.is_null() {
                    if kept == capacity {
                        break;
                    }
                    core::ptr::copy_nonoverlapping(entry.as_ptr().cast::<u8>(), entries.add(kept as usize * size), size);
                }
                kept += 1;
            }
            (*list).drmFormatModifierCount = kept;
            (*list).pDrmFormatModifierProperties = entries.cast();
        }
    }
}
