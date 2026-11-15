// SPDX-License-Identifier: GPL-3.0-or-later

//! WinBridge Vulkan ICD: a user-mode driver for the Khronos Vulkan loader that forwards calls
//! over `org.winbridge.gfx.0` to the host renderer. See docs/GFX-DESIGN.md.
//!
//! State of the implementation: every command is a synchronous round trip; host-visible memory
//! is shadowed in guest RAM (write-watched where coherent, see `shadow.rs` and `custom.rs`).

mod custom;
mod link;
mod shadow;
mod shims;
mod transport;
mod wsi;

use ash::vk;
use link::HostLink;
use shadow::ShadowMem;
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::Mutex;

/// Where the loader may ask for a command.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Global,
    Instance,
    Device,
}

/// Guest side bookkeeping for one `VkDeviceMemory`.
pub struct Shadow {
    pub size: u64,
    /// Guest RAM backing `vkMapMemory`; created on first map.
    pub mem: Option<ShadowMem>,
    pub mapped: bool,
    pub host_visible: bool,
    pub coherent: bool,
    /// Coherent and cached: GPU writes must be downloaded after waits.
    pub readback: bool,
}

pub struct State {
    pub link: HostLink,
    /// Physical devices per instance id, so repeated enumeration returns the same handles.
    pub phys: HashMap<u64, Vec<u64>>,
    /// Queue handles per (device id, family, index).
    pub queues: HashMap<(u64, u32, u32), u64>,
    pub mems: HashMap<u64, Shadow>,
    /// Property flags of the memory types as the application sees them.
    pub mem_types: Vec<ash::vk::MemoryPropertyFlags>,
    pub swapchains: HashMap<u64, wsi::Swapchain>,
}


static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Runs `f` with the connection state, connecting on first use. `None`: the host is unreachable.
pub fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|s| s.link.is_dead()) {
        return None;
    }
    if g.is_none() {
        match HostLink::connect() {
            Ok(link) => *g = Some(State { link, phys: HashMap::new(), queues: HashMap::new(), mems: HashMap::new(), mem_types: Vec::new(), swapchains: HashMap::new() }),
            Err(e) => {
                link::log(&format!("cannot reach the host: {e}"));
                return None;
            }
        }
    }
    g.as_mut().map(f)
}

/// Runs `f` with just the link (what the generated shims need).
pub fn with_link<R>(f: impl FnOnce(&mut HostLink) -> R) -> Option<R> {
    with_state(|s| f(&mut s.link))
}

/// Highest loader/ICD interface version implemented.
const ICD_INTERFACE_VERSION: u32 = 5;

#[no_mangle]
pub unsafe extern "system" fn vk_icdNegotiateLoaderICDInterfaceVersion(p_supported_version: *mut u32) -> vk::Result {
    if p_supported_version.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    if *p_supported_version > ICD_INTERFACE_VERSION {
        *p_supported_version = ICD_INTERFACE_VERSION;
    }
    vk::Result::SUCCESS
}

#[no_mangle]
pub unsafe extern "system" fn vk_icdGetInstanceProcAddr(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    proc_addr(p_name, instance, false)
}

/// Physical-device-level extension commands the loader does not know. None are implemented.
#[no_mangle]
pub unsafe extern "system" fn vk_icdGetPhysicalDeviceProcAddr(
    _instance: vk::Instance,
    _p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    None
}

unsafe extern "system" fn get_device_proc_addr(_device: vk::Device, p_name: *const c_char) -> vk::PFN_vkVoidFunction {
    proc_addr(p_name, vk::Instance::null(), true)
}

unsafe fn proc_addr(p_name: *const c_char, _instance: vk::Instance, device_only: bool) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    let name = CStr::from_ptr(p_name).to_bytes();
    let hit: Option<(*const (), Level)> = match name {
        b"vkGetInstanceProcAddr" => Some((vk_icdGetInstanceProcAddr as *const (), Level::Instance)),
        b"vkGetDeviceProcAddr" => Some((get_device_proc_addr as *const (), Level::Device)),
        b"vkEnumerateInstanceExtensionProperties" => Some((custom::vk_enumerate_instance_extension_properties as *const (), Level::Global)),
        b"vkEnumerateInstanceLayerProperties" => Some((custom::vk_enumerate_instance_layer_properties as *const (), Level::Global)),
        b"vkEnumerateInstanceVersion" => Some((custom::vk_enumerate_instance_version as *const (), Level::Global)),
        b"vkGetPhysicalDeviceSurfaceSupportKHR" => Some((wsi::vk_get_physical_device_surface_support_khr as *const (), Level::Instance)),
        b"vkGetPhysicalDeviceSurfaceCapabilitiesKHR" => Some((wsi::vk_get_physical_device_surface_capabilities_khr as *const (), Level::Instance)),
        b"vkGetPhysicalDeviceSurfaceFormatsKHR" => Some((wsi::vk_get_physical_device_surface_formats_khr as *const (), Level::Instance)),
        b"vkGetPhysicalDeviceSurfacePresentModesKHR" => Some((wsi::vk_get_physical_device_surface_present_modes_khr as *const (), Level::Instance)),
        #[cfg(windows)]
        b"vkCreateWin32SurfaceKHR" => Some((wsi::vk_create_win32_surface_khr as *const (), Level::Instance)),
        #[cfg(windows)]
        b"vkDestroySurfaceKHR" => Some((wsi::vk_destroy_surface_khr as *const (), Level::Instance)),
        #[cfg(windows)]
        b"vkGetPhysicalDeviceWin32PresentationSupportKHR" => Some((wsi::vk_get_physical_device_win32_presentation_support_khr as *const (), Level::Instance)),
        b"vkCreateSwapchainKHR" => Some((wsi::vk_create_swapchain_khr as *const (), Level::Device)),
        b"vkDestroySwapchainKHR" => Some((wsi::vk_destroy_swapchain_khr as *const (), Level::Device)),
        b"vkGetSwapchainImagesKHR" => Some((wsi::vk_get_swapchain_images_khr as *const (), Level::Device)),
        b"vkAcquireNextImageKHR" => Some((wsi::vk_acquire_next_image_khr as *const (), Level::Device)),
        b"vkQueuePresentKHR" => Some((wsi::vk_queue_present_khr as *const (), Level::Device)),
        b"vkMapMemory" => Some((custom::vk_map_memory as *const (), Level::Device)),
        b"vkUnmapMemory" => Some((custom::vk_unmap_memory as *const (), Level::Device)),
        b"vkFlushMappedMemoryRanges" => Some((custom::vk_flush_mapped_memory_ranges as *const (), Level::Device)),
        b"vkInvalidateMappedMemoryRanges" => Some((custom::vk_invalidate_mapped_memory_ranges as *const (), Level::Device)),
        _ => shims::lookup(name),
    };
    let Some((p, level)) = hit else {
        link::log(&format!("proc address not implemented: {}", String::from_utf8_lossy(name)));
        return None;
    };
    if device_only && level != Level::Device {
        link::log(&format!("proc address is not device-level: {}", String::from_utf8_lossy(name)));
        return None;
    }
    // SAFETY: every table entry is an `extern "system"` function; the loader casts it back to
    // the command's real type before calling it.
    Some(std::mem::transmute::<*const (), unsafe extern "system" fn()>(p))
}
