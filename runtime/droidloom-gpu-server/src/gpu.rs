//! The GPU the server renders on, named by PCI address as Droidloom's setup
//! chooses it (`DROIDLOOM_GPU` in `/etc/droidloom/runtime.env`).

use droidloom_gpu_protocol::generated::vk::*;

/// `physical`'s PCI address as sysfs writes it, e.g. `0000:04:00.0`.
///
/// # Safety
///
/// `properties` is the instance's `vkGetPhysicalDeviceProperties2` and
/// `physical` one of its physical devices.
pub unsafe fn pci_address(properties: PFN_vkGetPhysicalDeviceProperties2, physical: VkPhysicalDevice) -> String {
    unsafe {
        let mut bus = core::mem::zeroed::<VkPhysicalDevicePCIBusInfoPropertiesEXT>();
        bus.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PCI_BUS_INFO_PROPERTIES_EXT;
        let mut all = core::mem::zeroed::<VkPhysicalDeviceProperties2>();
        all.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
        all.pNext = (&raw mut bus).cast();
        properties(physical, &raw mut all);
        format!("{:04x}:{:02x}:{:02x}.{:x}", bus.pciDomain, bus.pciBus, bus.pciDevice, bus.pciFunction)
    }
}

/// The first GPU the `nvidia` kernel driver runs, for a server started
/// without a choice.
pub fn first_nvidia() -> Option<String> {
    let mut nodes: Vec<_> = std::fs::read_dir("/sys/class/drm").ok()?.flatten().map(|entry| entry.path()).collect();
    nodes.sort();
    nodes.into_iter().find_map(|node| {
        let name = node.file_name()?.to_str()?;
        let driver = std::fs::canonicalize(node.join("device/driver")).ok()?;
        if !name.starts_with("renderD") || driver.file_name()? != "nvidia" {
            return None;
        }
        Some(std::fs::canonicalize(node.join("device")).ok()?.file_name()?.to_str()?.to_owned())
    })
}

/// The render node of the GPU at `pci`.
pub fn render_node(pci: &str) -> Option<String> {
    let mut nodes: Vec<_> = std::fs::read_dir(format!("/sys/bus/pci/devices/{pci}/drm"))
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes.first().map(|node| format!("/dev/dri/{node}"))
}

/// The kernel driver of the GPU at `pci`, e.g. `nvidia`.
pub fn driver(pci: &str) -> Option<String> {
    let driver = std::fs::read_link(format!("/sys/bus/pci/devices/{pci}/driver")).ok()?;
    Some(driver.file_name()?.to_str()?.to_owned())
}
