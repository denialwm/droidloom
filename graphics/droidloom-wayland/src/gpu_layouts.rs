//! Window targets in the compositor GPU's own DMA-BUF layouts.
//!
//! A compositor that offers no linear buffers, as `KWin` on NVIDIA, still
//! imports some of its GPU's own layouts. Android renders a window target
//! through GPU forwarding on that GPU's Vulkan driver, so a target lies in
//! one the driver also draws into, the only ones the forwarding accepts, and
//! is allocated on that GPU: with several GPUs the presenter's render node
//! may be another one.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use ash::vk;
use gbm::Device as GbmDevice;

/// `DRM_FORMAT_MOD_LINEAR`, which this list leaves out.
const LINEAR: u64 = 0;

/// The compositor's GPU, ready to allocate targets in its own layouts.
pub struct Targets {
    pub node: PathBuf,
    pub gbm: GbmDevice<File>,
    /// The non-linear modifiers of `B8G8R8A8_UNORM`, the Vulkan format of
    /// ARGB8888 and XRGB8888, that the GPU's Vulkan driver renders into.
    pub layouts: Vec<u64>,
}

/// Open the render node of the compositor's main device, `dev_t` `device`
/// from its DMA-BUF feedback, and ask its Vulkan driver for its layouts.
///
/// # Errors
///
/// No render node for the device, or no Vulkan GPU that answers for it.
pub fn open(device: u64) -> Result<Targets, String> {
    let (major, minor) = (libc::major(device), libc::minor(device));
    let drm = PathBuf::from(format!("/sys/dev/char/{major}:{minor}/device/drm"));
    let node = std::fs::read_dir(&drm)
        .map_err(|error| format!("{}: {error}", drm.display()))?
        .flatten()
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with("renderD"))
        .map(|name| PathBuf::from("/dev/dri").join(name))
        .ok_or_else(|| format!("device {major}:{minor} has no render node"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .map_err(|error| format!("{}: {error}", node.display()))?;
    let rdev = file.metadata().map_err(|error| error.to_string())?.rdev();
    // SAFETY: the instance is created and destroyed in `query`; the queries
    // follow the valid-usage rules and nothing escapes it.
    let layouts = unsafe { query(rdev) }?;
    let gbm =
        GbmDevice::new(file).map_err(|error| format!("GBM on {}: {error}", node.display()))?;
    Ok(Targets { node, gbm, layouts })
}

unsafe fn query(device: u64) -> Result<Vec<u64>, String> {
    let entry = unsafe { ash::Entry::load() }.map_err(|error| error.to_string())?;
    let app = vk::ApplicationInfo::default()
        .application_name(c"droidloom-wayland")
        .api_version(vk::API_VERSION_1_1);
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        )
    }
    .map_err(|error| format!("create instance: {error}"))?;
    let layouts = unsafe { layouts(&instance, device) };
    unsafe { instance.destroy_instance(None) };
    layouts
}

unsafe fn layouts(instance: &ash::Instance, device: u64) -> Result<Vec<u64>, String> {
    let (major, minor) = (libc::major(device), libc::minor(device));
    for physical in unsafe { instance.enumerate_physical_devices() }
        .map_err(|error| format!("enumerate GPUs: {error}"))?
    {
        let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
            .map_err(|error| format!("list device extensions: {error}"))?;
        let has = |name: &std::ffi::CStr| {
            extensions
                .iter()
                .any(|extension| extension.extension_name_as_c_str() == Ok(name))
        };
        if !has(ash::ext::physical_device_drm::NAME)
            || !has(ash::ext::image_drm_format_modifier::NAME)
        {
            continue;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        if drm.has_render != vk::TRUE
            || drm.render_major != i64::from(major)
            || drm.render_minor != i64::from(minor)
        {
            continue;
        }
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut format,
            );
        }
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        let mut format = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut format,
            );
        }
        let count = list.drm_format_modifier_count as usize;
        return Ok(entries[..count]
            .iter()
            .filter(|entry| {
                entry.drm_format_modifier != LINEAR
                    && entry
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
            })
            .map(|entry| entry.drm_format_modifier)
            .collect());
    }
    Err(format!("no Vulkan GPU drives render node {major}:{minor}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    /// Lists every render node's layouts; needs GPUs, so run by hand.
    #[test]
    #[ignore = "needs the host's GPUs"]
    fn every_render_node_answers() {
        for node in std::fs::read_dir("/dev/dri").unwrap().flatten() {
            if node.file_name().to_string_lossy().starts_with("renderD") {
                let targets = super::open(node.metadata().unwrap().rdev()).unwrap();
                println!("{}: {:#x?}", targets.node.display(), targets.layouts);
            }
        }
    }
}
