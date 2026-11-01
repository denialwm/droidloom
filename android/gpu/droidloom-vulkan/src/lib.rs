//! Droidloom's forwarding Vulkan driver.
//!
//! Applications load it like any Vulkan driver. Every call becomes a record
//! in memory shared with `droidloom-gpu-server`, which replays it on the host
//! GPU's own driver. See `docs/gpu-forwarding.md`.

// Lints live here, not in Cargo.toml, so the image's Soong build applies
// them too. Vulkan's names are camel case; the encoders are unsafe code
// throughout.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

mod android;
mod generated;
#[cfg(target_os = "android")]
mod hardware_buffer;
mod manual;
mod rt;
mod wsi;

use core::ffi::c_char;

use droidloom_gpu_protocol::generated::vk::{PFN_vkVoidFunction, VkInstance, VkResult, VK_SUCCESS};

/// Loader–driver interface version this driver speaks.
const LOADER_INTERFACE: u32 = 5;

/// The loader asks which interface version both sides speak.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdNegotiateLoaderICDInterfaceVersion(version: *mut u32) -> VkResult {
    unsafe { *version = (*version).min(LOADER_INTERFACE) };
    VK_SUCCESS
}

/// The loader's entry to every instance-level command.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdGetInstanceProcAddr(instance: VkInstance, name: *const c_char) -> PFN_vkVoidFunction {
    unsafe { manual::vkGetInstanceProcAddr(instance, name) }
}

/// Physical-device commands the loader does not know itself.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdGetPhysicalDeviceProcAddr(_instance: VkInstance, name: *const c_char) -> PFN_vkVoidFunction {
    unsafe { manual::physical_proc_addr(name) }
}
