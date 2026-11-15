// SPDX-License-Identifier: GPL-3.0-or-later

//! Commands the ICD implements itself, on top of the generated forwarding functions.

use crate::shadow::ShadowMem;
use crate::{with_state, Shadow};
use ash::vk::{self, Handle as _};
use std::ffi::{c_char, c_void, CStr};
use winbridge_gfx_vk as gv;

/// Instance extensions the ICD implements itself (the loader supplies the surface objects).
fn own_instance_extensions() -> Vec<vk::ExtensionProperties> {
    let mut v = vec![ext("VK_KHR_surface", 25), ext("VK_EXT_headless_surface", 1)];
    if cfg!(windows) {
        v.push(ext("VK_KHR_win32_surface", 6));
    }
    v
}

fn ext(name: &str, version: u32) -> vk::ExtensionProperties {
    let mut e = vk::ExtensionProperties::default();
    for (d, s) in e.extension_name.iter_mut().zip(name.bytes()) {
        *d = s as c_char;
    }
    e.spec_version = version;
    e
}

fn is_own_instance_extension(name: &CStr) -> bool {
    let n = name.to_bytes();
    own_instance_extensions().iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }.to_bytes() == n)
}

pub unsafe extern "system" fn vk_enumerate_instance_extension_properties(
    p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties: *mut vk::ExtensionProperties,
) -> vk::Result {
    if !p_layer_name.is_null() {
        return vk::Result::ERROR_LAYER_NOT_PRESENT;
    }
    crate::wsi::enumerate(&own_instance_extensions(), p_property_count, p_properties)
}

/// Copies `names` without the entries `drop` selects.
unsafe fn filtered_names(count: u32, names: *const *const c_char, drop: impl Fn(&CStr) -> bool) -> Vec<*const c_char> {
    (0..count as usize).map(|i| *names.add(i)).filter(|n| !drop(CStr::from_ptr(*n))).collect()
}

/// The instance extensions above exist only in the ICD: the host never sees them.
pub unsafe extern "system" fn vk_create_instance(
    p_create_info: *const vk::InstanceCreateInfo,
    _p_allocator: *const vk::AllocationCallbacks,
    p_instance: *mut vk::Instance,
) -> vk::Result {
    let info = *p_create_info;
    let names = filtered_names(info.enabled_extension_count, info.pp_enabled_extension_names, is_own_instance_extension);
    let mut patched = info;
    patched.enabled_extension_count = names.len() as u32;
    patched.pp_enabled_extension_names = names.as_ptr();
    with_state(|st| gv::vk_create_instance(&mut st.link, &patched, p_instance)).unwrap_or(vk::Result::ERROR_INCOMPATIBLE_DRIVER)
}

pub unsafe extern "system" fn vk_create_device(
    physical_device: vk::PhysicalDevice,
    p_create_info: *const vk::DeviceCreateInfo,
    _p_allocator: *const vk::AllocationCallbacks,
    p_device: *mut vk::Device,
) -> vk::Result {
    let info = *p_create_info;
    let names = filtered_names(info.enabled_extension_count, info.pp_enabled_extension_names, |n| n == c"VK_KHR_swapchain");
    let mut patched = info;
    patched.enabled_extension_count = names.len() as u32;
    patched.pp_enabled_extension_names = names.as_ptr();
    with_state(|st| gv::vk_create_device(&mut st.link, physical_device, &patched, p_device)).unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

/// The host's device extensions that the ICD can honour, plus `VK_KHR_swapchain`.
pub unsafe extern "system" fn vk_enumerate_device_extension_properties(
    physical_device: vk::PhysicalDevice,
    p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties: *mut vk::ExtensionProperties,
) -> vk::Result {
    if !p_layer_name.is_null() {
        return vk::Result::ERROR_LAYER_NOT_PRESENT;
    }
    with_state(|st| {
        let mut n = 0u32;
        let r = gv::vk_enumerate_device_extension_properties(&mut st.link, physical_device, std::ptr::null(), &mut n, std::ptr::null_mut());
        if r.as_raw() < 0 {
            return r;
        }
        let mut all = vec![vk::ExtensionProperties::default(); n as usize];
        let r = gv::vk_enumerate_device_extension_properties(&mut st.link, physical_device, std::ptr::null(), &mut n, all.as_mut_ptr());
        if r.as_raw() < 0 {
            return r;
        }
        all.truncate(n as usize);
        all.retain(|e| CStr::from_ptr(e.extension_name.as_ptr()) != c"VK_KHR_swapchain");
        all.push(ext("VK_KHR_swapchain", 70));
        crate::wsi::enumerate(&all, p_property_count, p_properties)
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_enumerate_instance_layer_properties(
    p_property_count: *mut u32,
    _p_properties: *mut vk::LayerProperties,
) -> vk::Result {
    *p_property_count = 0;
    vk::Result::SUCCESS
}

pub unsafe extern "system" fn vk_enumerate_instance_version(p_api_version: *mut u32) -> vk::Result {
    *p_api_version = vk::API_VERSION_1_3;
    vk::Result::SUCCESS
}

pub unsafe extern "system" fn vk_enumerate_physical_devices(
    instance: vk::Instance,
    p_physical_device_count: *mut u32,
    p_physical_devices: *mut vk::PhysicalDevice,
) -> vk::Result {
    with_state(|st| {
        let inst_id = gv::hid_disp(instance.as_raw());
        if !st.phys.contains_key(&inst_id) {
            let mut n = 0u32;
            let r = gv::vk_enumerate_physical_devices(&mut st.link, instance, &mut n, std::ptr::null_mut());
            if r.as_raw() < 0 {
                return r;
            }
            let mut v = vec![vk::PhysicalDevice::from_raw(0); n as usize];
            let r = gv::vk_enumerate_physical_devices(&mut st.link, instance, &mut n, v.as_mut_ptr());
            if r.as_raw() < 0 {
                return r;
            }
            st.phys.insert(inst_id, v.iter().take(n as usize).map(|p| p.as_raw()).collect());
        }
        let list = &st.phys[&inst_id];
        if p_physical_devices.is_null() {
            *p_physical_device_count = list.len() as u32;
            return vk::Result::SUCCESS;
        }
        let n = list.len().min(*p_physical_device_count as usize);
        for (i, raw) in list.iter().take(n).enumerate() {
            *p_physical_devices.add(i) = vk::PhysicalDevice::from_raw(*raw);
        }
        *p_physical_device_count = n as u32;
        if n < list.len() {
            vk::Result::INCOMPLETE
        } else {
            vk::Result::SUCCESS
        }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

/// Marks the device as the WinBridge one: `deviceName` becomes `WinBridge: <host GPU>`.
fn brand(name: &mut [c_char; vk::MAX_PHYSICAL_DEVICE_NAME_SIZE]) {
    // SAFETY: the host filled the array with a NUL-terminated string.
    let host = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
    let branded = format!("WinBridge: {host}");
    let bytes = branded.as_bytes();
    let n = bytes.len().min(name.len() - 1);
    for (d, s) in name.iter_mut().zip(&bytes[..n]) {
        *d = *s as c_char;
    }
    name[n] = 0;
}

pub unsafe extern "system" fn vk_get_physical_device_properties(
    physical_device: vk::PhysicalDevice,
    p_properties: *mut vk::PhysicalDeviceProperties,
) {
    let _ = with_state(|st| {
        gv::vk_get_physical_device_properties(&mut st.link, physical_device, p_properties);
        brand(&mut (*p_properties).device_name);
    });
}

pub unsafe extern "system" fn vk_get_physical_device_properties2(
    physical_device: vk::PhysicalDevice,
    p_properties: *mut vk::PhysicalDeviceProperties2,
) {
    let _ = with_state(|st| {
        gv::vk_get_physical_device_properties2(&mut st.link, physical_device, p_properties);
        brand(&mut (*p_properties).properties.device_name);
    });
}

/// Coherent memory is only offered where the guest never needs to read back GPU writes
/// (types without `HOST_CACHED`, which is how drivers expose upload heaps): those are
/// write-watched and flushed before submission. Cached (readback) types lose the coherent bit,
/// so applications invalidate them explicitly, which the Vulkan specification allows.
/// A host that has no uncached coherent host-visible type (for example llvmpipe) keeps the bit
/// on its cached types; those are downloaded after every wait that can make GPU writes
/// visible (`after_wait`).
fn adjust_coherency(st: &mut crate::State, p: &mut vk::PhysicalDeviceMemoryProperties) {
    let n = p.memory_type_count as usize;
    let vis = vk::MemoryPropertyFlags::HOST_VISIBLE;
    let (coh, cached) = (vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::HOST_CACHED);
    let has_plain = p.memory_types[..n].iter().any(|t| t.property_flags.contains(vis | coh) && !t.property_flags.contains(cached));
    if has_plain {
        for t in p.memory_types[..n].iter_mut().filter(|t| t.property_flags.contains(cached)) {
            t.property_flags &= !coh;
        }
    }
    st.mem_types = p.memory_types[..n].iter().map(|t| t.property_flags).collect();
}

/// Brings coherent readback memory up to date after a wait: first what the application wrote,
/// then what the GPU wrote.
fn after_wait(st: &mut crate::State) -> Result<(), vk::Result> {
    flush_dirty(st)?;
    for (id, s) in st.mems.iter_mut() {
        if !s.readback {
            continue;
        }
        let Some(m) = s.mem.as_mut() else { continue };
        // SAFETY: the shadow spans `size` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(m.base(), m.size()) };
        st.link.mem_download(*id, 0, out).map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
        m.take_dirty(); // the downloaded bytes are not application writes
    }
    Ok(())
}

pub unsafe extern "system" fn vk_wait_for_fences(
    device: vk::Device,
    fence_count: u32,
    p_fences: *const vk::Fence,
    wait_all: vk::Bool32,
    timeout: u64,
) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_wait_for_fences(&mut st.link, device, fence_count, p_fences, wait_all, timeout);
        if r == vk::Result::SUCCESS { after_wait(st).err().unwrap_or(r) } else { r }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_get_fence_status(device: vk::Device, fence: vk::Fence) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_get_fence_status(&mut st.link, device, fence);
        if r == vk::Result::SUCCESS { after_wait(st).err().unwrap_or(r) } else { r }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_queue_wait_idle(queue: vk::Queue) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_queue_wait_idle(&mut st.link, queue);
        if r == vk::Result::SUCCESS { after_wait(st).err().unwrap_or(r) } else { r }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_device_wait_idle(device: vk::Device) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_device_wait_idle(&mut st.link, device);
        if r == vk::Result::SUCCESS { after_wait(st).err().unwrap_or(r) } else { r }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_wait_semaphores(
    device: vk::Device,
    p_wait_info: *const vk::SemaphoreWaitInfo,
    timeout: u64,
) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_wait_semaphores(&mut st.link, device, p_wait_info, timeout);
        if r == vk::Result::SUCCESS { after_wait(st).err().unwrap_or(r) } else { r }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_get_physical_device_memory_properties(
    physical_device: vk::PhysicalDevice,
    p_memory_properties: *mut vk::PhysicalDeviceMemoryProperties,
) {
    let _ = with_state(|st| {
        gv::vk_get_physical_device_memory_properties(&mut st.link, physical_device, p_memory_properties);
        adjust_coherency(st, &mut *p_memory_properties);
    });
}

pub unsafe extern "system" fn vk_get_physical_device_memory_properties2(
    physical_device: vk::PhysicalDevice,
    p_memory_properties: *mut vk::PhysicalDeviceMemoryProperties2,
) {
    let _ = with_state(|st| {
        gv::vk_get_physical_device_memory_properties2(&mut st.link, physical_device, p_memory_properties);
        adjust_coherency(st, &mut (*p_memory_properties).memory_properties);
    });
}

pub unsafe extern "system" fn vk_get_device_queue(
    device: vk::Device,
    queue_family_index: u32,
    queue_index: u32,
    p_queue: *mut vk::Queue,
) {
    let _ = with_state(|st| {
        let key = (gv::hid_disp(device.as_raw()), queue_family_index, queue_index);
        if let Some(raw) = st.queues.get(&key) {
            *p_queue = vk::Queue::from_raw(*raw);
            return;
        }
        gv::vk_get_device_queue(&mut st.link, device, queue_family_index, queue_index, p_queue);
        if !st.link.is_dead() {
            st.queues.insert(key, (*p_queue).as_raw());
        }
    });
}

pub unsafe extern "system" fn vk_allocate_memory(
    device: vk::Device,
    p_allocate_info: *const vk::MemoryAllocateInfo,
    _p_allocator: *const vk::AllocationCallbacks,
    p_memory: *mut vk::DeviceMemory,
) -> vk::Result {
    with_state(|st| {
        let r = gv::vk_allocate_memory(&mut st.link, device, p_allocate_info, p_memory);
        if r == vk::Result::SUCCESS {
            let info = &*p_allocate_info;
            let flags = st.mem_types.get(info.memory_type_index as usize).copied().unwrap_or_default();
            st.mems.insert(
                (*p_memory).as_raw(),
                Shadow {
                    size: info.allocation_size,
                    mem: None,
                    mapped: false,
                    host_visible: flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE),
                    coherent: flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT),
                    readback: flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED),
                },
            );
        }
        r
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_free_memory(
    device: vk::Device,
    memory: vk::DeviceMemory,
    _p_allocator: *const vk::AllocationCallbacks,
) {
    let _ = with_state(|st| {
        st.mems.remove(&memory.as_raw());
        gv::vk_free_memory(&mut st.link, device, memory);
    });
}

pub unsafe extern "system" fn vk_map_memory(
    _device: vk::Device,
    memory: vk::DeviceMemory,
    offset: vk::DeviceSize,
    size: vk::DeviceSize,
    _flags: vk::MemoryMapFlags,
    pp_data: *mut *mut c_void,
) -> vk::Result {
    with_state(|st| {
        let Some(s) = st.mems.get_mut(&memory.as_raw()) else {
            return vk::Result::ERROR_MEMORY_MAP_FAILED;
        };
        if !s.host_visible || s.mapped || offset >= s.size || (size != vk::WHOLE_SIZE && size > s.size - offset) {
            return vk::Result::ERROR_MEMORY_MAP_FAILED;
        }
        if s.mem.is_none() {
            // Coherent memory is write-watched so that what the application wrote is sent
            // before the host uses it; non-coherent memory is sent only by an explicit flush.
            match ShadowMem::new(s.size as usize, s.coherent) {
                Some(m) => s.mem = Some(m),
                None => return vk::Result::ERROR_OUT_OF_HOST_MEMORY,
            }
        }
        s.mapped = true;
        *pp_data = s.mem.as_ref().unwrap().base().add(offset as usize).cast();
        vk::Result::SUCCESS
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_unmap_memory(_device: vk::Device, memory: vk::DeviceMemory) {
    let _ = with_state(|st| {
        if let Some(s) = st.mems.get_mut(&memory.as_raw()) {
            s.mapped = false;
        }
    });
}

/// `(offset, length)` of a mapped range inside its shadow, or `None` if it does not fit.
fn range_of(s: &Shadow, r: &vk::MappedMemoryRange) -> Option<(u64, u64)> {
    if r.offset > s.size {
        return None;
    }
    let len = if r.size == vk::WHOLE_SIZE { s.size - r.offset } else { r.size };
    (len <= s.size - r.offset).then_some((r.offset, len))
}

/// Sends every write the application made into coherent mapped memory since the last call.
/// Called before anything that lets the host use that memory.
pub fn flush_dirty(st: &mut crate::State) -> Result<(), vk::Result> {
    for (id, s) in st.mems.iter_mut() {
        let Some(m) = s.mem.as_mut() else { continue };
        for (off, len) in m.take_dirty() {
            // SAFETY: `take_dirty` returns ranges inside the allocation.
            let data = unsafe { std::slice::from_raw_parts(m.base().add(off), len) };
            st.link.mem_upload(*id, off as u64, data).map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
        }
    }
    Ok(())
}

pub unsafe extern "system" fn vk_queue_submit(
    queue: vk::Queue,
    submit_count: u32,
    p_submits: *const vk::SubmitInfo,
    fence: vk::Fence,
) -> vk::Result {
    with_state(|st| match flush_dirty(st) {
        Ok(()) => gv::vk_queue_submit(&mut st.link, queue, submit_count, p_submits, fence),
        Err(r) => r,
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_queue_submit2(
    queue: vk::Queue,
    submit_count: u32,
    p_submits: *const vk::SubmitInfo2,
    fence: vk::Fence,
) -> vk::Result {
    with_state(|st| match flush_dirty(st) {
        Ok(()) => gv::vk_queue_submit2(&mut st.link, queue, submit_count, p_submits, fence),
        Err(r) => r,
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_flush_mapped_memory_ranges(
    _device: vk::Device,
    memory_range_count: u32,
    p_memory_ranges: *const vk::MappedMemoryRange,
) -> vk::Result {
    with_state(|st| {
        for r in std::slice::from_raw_parts(p_memory_ranges, memory_range_count as usize) {
            let id = r.memory.as_raw();
            let Some(s) = st.mems.get(&id) else { return vk::Result::ERROR_MEMORY_MAP_FAILED };
            let (Some((off, len)), Some(m)) = (range_of(s, r), s.mem.as_ref()) else {
                return vk::Result::ERROR_MEMORY_MAP_FAILED;
            };
            let data = std::slice::from_raw_parts(m.base().add(off as usize), len as usize);
            if st.link.mem_upload(id, off, data).is_err() {
                return vk::Result::ERROR_DEVICE_LOST;
            }
        }
        vk::Result::SUCCESS
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_invalidate_mapped_memory_ranges(
    _device: vk::Device,
    memory_range_count: u32,
    p_memory_ranges: *const vk::MappedMemoryRange,
) -> vk::Result {
    with_state(|st| {
        for r in std::slice::from_raw_parts(p_memory_ranges, memory_range_count as usize) {
            let id = r.memory.as_raw();
            let Some(s) = st.mems.get(&id) else { return vk::Result::ERROR_MEMORY_MAP_FAILED };
            let (Some((off, len)), Some(m)) = (range_of(s, r), s.mem.as_ref()) else {
                return vk::Result::ERROR_MEMORY_MAP_FAILED;
            };
            let out = std::slice::from_raw_parts_mut(m.base().add(off as usize), len as usize);
            if st.link.mem_download(id, off, out).is_err() {
                return vk::Result::ERROR_DEVICE_LOST;
            }
        }
        vk::Result::SUCCESS
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_destroy_instance(instance: vk::Instance, _p_allocator: *const vk::AllocationCallbacks) {
    let _ = with_state(|st| {
        let id = gv::hid_disp(instance.as_raw());
        gv::vk_destroy_instance(&mut st.link, instance);
        if id != 0 {
            for raw in st.phys.remove(&id).unwrap_or_default() {
                gv::Link::discard_handle(&mut st.link, raw, true);
            }
            gv::Link::discard_handle(&mut st.link, instance.as_raw(), true);
        }
    });
}

pub unsafe extern "system" fn vk_destroy_device(device: vk::Device, _p_allocator: *const vk::AllocationCallbacks) {
    let _ = with_state(|st| {
        let id = gv::hid_disp(device.as_raw());
        gv::vk_destroy_device(&mut st.link, device);
        if id != 0 {
            let keys: Vec<_> = st.queues.keys().filter(|k| k.0 == id).copied().collect();
            for k in keys {
                if let Some(raw) = st.queues.remove(&k) {
                    gv::Link::discard_handle(&mut st.link, raw, true);
                }
            }
            gv::Link::discard_handle(&mut st.link, device.as_raw(), true);
        }
    });
}
