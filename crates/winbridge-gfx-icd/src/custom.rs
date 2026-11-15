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

/// Device extensions the ICD advertises besides `VK_KHR_swapchain`. An extension that adds
/// commands is listed only when the ICD exports every command the applications we target call
/// (the application otherwise calls a NULL pointer): maintenance5 and maintenance6 are listed
/// for `vkCmdBindIndexBuffer2`, `vkCmdBindDescriptorSets2` and `vkCmdPushConstants2`; their
/// `vkGetDeviceImageSubresourceLayout`, `vkGetImageSubresourceLayout2` and
/// `vkGetRenderingAreaGranularity` are not implemented yet.
const IMPLEMENTED_DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_swapchain_mutable_format",
    c"VK_KHR_load_store_op_none",
    c"VK_KHR_maintenance5",
    c"VK_KHR_maintenance6",
    c"VK_EXT_memory_priority",
    c"VK_EXT_non_seamless_cube_map",
    c"VK_EXT_custom_border_color",
    c"VK_EXT_depth_clip_enable",
    c"VK_EXT_robustness2",
];

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
        all.retain(|e| {
            let n = CStr::from_ptr(e.extension_name.as_ptr());
            n != c"VK_KHR_swapchain" && IMPLEMENTED_DEVICE_EXTENSIONS.contains(&n)
        });
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

/// Host-visible memory is offered coherent so applications never need invalidate or flush
/// calls (DXVK, for one, uses only coherent types and never invalidates). Types without
/// `HOST_CACHED` are upload heaps: the guest's writes are write-watched and sent before the
/// host uses them. Types with `HOST_CACHED` are readback heaps: the bit `HOST_COHERENT` is
/// added when the driver lacks it, and every wait that can make GPU writes visible downloads
/// them (`after_wait`).
fn adjust_coherency(st: &mut crate::State, p: &mut vk::PhysicalDeviceMemoryProperties) {
    let n = p.memory_type_count as usize;
    let (vis, coh, cached) = (vk::MemoryPropertyFlags::HOST_VISIBLE, vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::HOST_CACHED);
    for t in p.memory_types[..n].iter_mut().filter(|t| t.property_flags.contains(vis | cached)) {
        t.property_flags |= coh;
    }
    st.mem_types = p.memory_types[..n].iter().map(|t| t.property_flags).collect();
}

/// Brings coherent readback memory up to date after a wait: first what the application wrote,
/// then what submitted copy commands wrote (`State::pending`). An allocation with no buffer
/// bound (a linear image) is downloaded whole. GPU writes to host-visible buffers by shaders or
/// transform feedback are not tracked and are not downloaded.
fn after_wait(st: &mut crate::State) -> Result<(), vk::Result> {
    stats::WAIT.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    flush_dirty(st)?;
    stats::report();
    let pending = std::mem::take(&mut st.pending);
    for (id, s) in st.mems.iter_mut() {
        if !s.readback {
            continue;
        }
        let Some(m) = s.mem.as_mut() else { continue };
        let size = m.size() as u64;
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        if s.bufs.is_empty() {
            ranges.push((0, size));
        } else {
            for (buf, off, len) in &pending {
                if let Some(&(boff, bsize)) = s.bufs.get(buf) {
                    let len = if *len == u64::MAX { bsize.saturating_sub(*off) } else { (*len).min(bsize.saturating_sub(*off)) };
                    ranges.push((boff + off, len));
                }
            }
        }
        let mut any = false;
        for (off, len) in ranges {
            let end = off.saturating_add(len).min(size);
            if off >= end {
                continue;
            }
            // SAFETY: the shadow spans `size` bytes and `off..end` lies inside it.
            let out = unsafe { std::slice::from_raw_parts_mut(m.base().add(off as usize), (end - off) as usize) };
            let t = std::time::Instant::now();
            st.link.mem_download(*id, off, out).map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
            stats::add(&stats::DOWNLOAD, end - off, t.elapsed());
            any = true;
        }
        if any {
            m.take_dirty(); // the downloaded bytes are not application writes
        }
    }
    Ok(())
}

/// Records that the commands of `cb` write `len` bytes of `buffer` from `off`.
fn note_write(cb: vk::CommandBuffer, buffer: vk::Buffer, off: u64, len: u64) {
    let _ = with_state(|st| st.cmd_writes.entry(cb.as_raw()).or_default().push((buffer.as_raw(), off, len)));
}

/// Submitted command buffers: what they write becomes pending for the next wait.
fn note_submit(st: &mut crate::State, cbs: impl Iterator<Item = u64>) {
    for cb in cbs {
        if let Some(w) = st.cmd_writes.remove(&cb) {
            st.pending.extend(w);
        }
    }
}

pub unsafe extern "system" fn vk_cmd_copy_buffer(
    cb: vk::CommandBuffer,
    src: vk::Buffer,
    dst: vk::Buffer,
    count: u32,
    p_regions: *const vk::BufferCopy,
) {
    crate::shims::shim_vk_cmd_copy_buffer(cb, src, dst, count, p_regions);
    for r in std::slice::from_raw_parts(p_regions, count as usize) {
        note_write(cb, dst, r.dst_offset, r.size);
    }
}

pub unsafe extern "system" fn vk_cmd_copy_buffer2(cb: vk::CommandBuffer, p_info: *const vk::CopyBufferInfo2) {
    crate::shims::shim_vk_cmd_copy_buffer2(cb, p_info);
    let i = &*p_info;
    for r in std::slice::from_raw_parts(i.p_regions, i.region_count as usize) {
        note_write(cb, i.dst_buffer, r.dst_offset, r.size);
    }
}

pub unsafe extern "system" fn vk_cmd_copy_image_to_buffer(
    cb: vk::CommandBuffer,
    image: vk::Image,
    layout: vk::ImageLayout,
    dst: vk::Buffer,
    count: u32,
    p_regions: *const vk::BufferImageCopy,
) {
    crate::shims::shim_vk_cmd_copy_image_to_buffer(cb, image, layout, dst, count, p_regions);
    for r in std::slice::from_raw_parts(p_regions, count as usize) {
        note_write(cb, dst, r.buffer_offset, u64::MAX);
    }
}

pub unsafe extern "system" fn vk_cmd_copy_image_to_buffer2(cb: vk::CommandBuffer, p_info: *const vk::CopyImageToBufferInfo2) {
    crate::shims::shim_vk_cmd_copy_image_to_buffer2(cb, p_info);
    let i = &*p_info;
    for r in std::slice::from_raw_parts(i.p_regions, i.region_count as usize) {
        note_write(cb, i.dst_buffer, r.buffer_offset, u64::MAX);
    }
}

pub unsafe extern "system" fn vk_cmd_fill_buffer(cb: vk::CommandBuffer, dst: vk::Buffer, off: vk::DeviceSize, size: vk::DeviceSize, data: u32) {
    crate::shims::shim_vk_cmd_fill_buffer(cb, dst, off, size, data);
    note_write(cb, dst, off, if size == vk::WHOLE_SIZE { u64::MAX } else { size });
}

pub unsafe extern "system" fn vk_cmd_update_buffer(cb: vk::CommandBuffer, dst: vk::Buffer, off: vk::DeviceSize, size: vk::DeviceSize, p_data: *const c_void) {
    crate::shims::shim_vk_cmd_update_buffer(cb, dst, off, size, p_data);
    note_write(cb, dst, off, size);
}

pub unsafe extern "system" fn vk_cmd_copy_query_pool_results(
    cb: vk::CommandBuffer,
    pool: vk::QueryPool,
    first: u32,
    count: u32,
    dst: vk::Buffer,
    off: vk::DeviceSize,
    stride: vk::DeviceSize,
    flags: vk::QueryResultFlags,
) {
    crate::shims::shim_vk_cmd_copy_query_pool_results(cb, pool, first, count, dst, off, stride, flags);
    note_write(cb, dst, off, u64::MAX);
}

pub unsafe extern "system" fn vk_create_buffer(
    device: vk::Device,
    p_create_info: *const vk::BufferCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_buffer: *mut vk::Buffer,
) -> vk::Result {
    let r = crate::shims::shim_vk_create_buffer(device, p_create_info, p_allocator, p_buffer);
    if r == vk::Result::SUCCESS && !p_create_info.is_null() && !p_buffer.is_null() {
        let size = (*p_create_info).size;
        let _ = with_state(|st| st.buffers.insert((*p_buffer).as_raw(), size));
    }
    r
}

pub unsafe extern "system" fn vk_destroy_buffer(device: vk::Device, buffer: vk::Buffer, p_allocator: *const vk::AllocationCallbacks) {
    let _ = with_state(|st| {
        st.buffers.remove(&buffer.as_raw());
        for s in st.mems.values_mut() {
            s.bufs.remove(&buffer.as_raw());
        }
    });
    crate::shims::shim_vk_destroy_buffer(device, buffer, p_allocator);
}

fn track_bind(st: &mut crate::State, buffer: vk::Buffer, memory: vk::DeviceMemory, offset: u64) {
    let Some(&size) = st.buffers.get(&buffer.as_raw()) else { return };
    if let Some(s) = st.mems.get_mut(&memory.as_raw()) {
        s.bufs.insert(buffer.as_raw(), (offset, size));
    }
}

pub unsafe extern "system" fn vk_bind_buffer_memory(
    device: vk::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    memory_offset: vk::DeviceSize,
) -> vk::Result {
    let r = crate::shims::shim_vk_bind_buffer_memory(device, buffer, memory, memory_offset);
    if r == vk::Result::SUCCESS {
        let _ = with_state(|st| track_bind(st, buffer, memory, memory_offset));
    }
    r
}

pub unsafe extern "system" fn vk_bind_buffer_memory2(
    device: vk::Device,
    bind_info_count: u32,
    p_bind_infos: *const vk::BindBufferMemoryInfo,
) -> vk::Result {
    let r = crate::shims::shim_vk_bind_buffer_memory2(device, bind_info_count, p_bind_infos);
    if r == vk::Result::SUCCESS && !p_bind_infos.is_null() {
        let infos = std::slice::from_raw_parts(p_bind_infos, bind_info_count as usize);
        let _ = with_state(|st| {
            for i in infos {
                track_bind(st, i.buffer, i.memory, i.memory_offset);
            }
        });
    }
    r
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
        let t = std::time::Instant::now();
        let r = gv::vk_wait_semaphores(&mut st.link, device, p_wait_info, timeout);
        stats::WAIT.micros.fetch_add(t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
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
                    bufs: Default::default(),
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
            let t = std::time::Instant::now();
            st.link.mem_upload(*id, off as u64, data).map_err(|_| vk::Result::ERROR_DEVICE_LOST)?;
            stats::add(&stats::UPLOAD, len as u64, t.elapsed());
        }
    }
    Ok(())
}

/// Transfer totals, logged every 100 waits so a slowdown can be attributed to a cause.
mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::Duration;

    pub struct Counter {
        pub calls: AtomicU64,
        pub bytes: AtomicU64,
        pub micros: AtomicU64,
    }
    impl Counter {
        const fn new() -> Self {
            Self { calls: AtomicU64::new(0), bytes: AtomicU64::new(0), micros: AtomicU64::new(0) }
        }
    }
    pub static UPLOAD: Counter = Counter::new();
    pub static DOWNLOAD: Counter = Counter::new();
    pub static WAIT: Counter = Counter::new();

    pub fn add(c: &Counter, bytes: u64, d: Duration) {
        c.calls.fetch_add(1, Relaxed);
        c.bytes.fetch_add(bytes, Relaxed);
        c.micros.fetch_add(d.as_micros() as u64, Relaxed);
    }

    /// Logs one line with the totals when the wait counter reaches a multiple of 100.
    pub fn report() {
        let n = WAIT.calls.load(Relaxed);
        if n % 100 != 0 {
            return;
        }
        let f = |c: &Counter| format!("{} calls {} KiB {} ms", c.calls.load(Relaxed), c.bytes.load(Relaxed) / 1024, c.micros.load(Relaxed) / 1000);
        crate::link::log(&format!("stats after {n} waits: upload {}; download {}; wait-in-host {}", f(&UPLOAD), f(&DOWNLOAD), f(&WAIT)));
    }
}

pub unsafe extern "system" fn vk_queue_submit(
    queue: vk::Queue,
    submit_count: u32,
    p_submits: *const vk::SubmitInfo,
    fence: vk::Fence,
) -> vk::Result {
    with_state(|st| match flush_dirty(st) {
        Ok(()) => {
            for s in std::slice::from_raw_parts(p_submits, submit_count as usize) {
                let cbs = std::slice::from_raw_parts(s.p_command_buffers, s.command_buffer_count as usize);
                note_submit(st, cbs.iter().map(|c| c.as_raw()));
            }
            gv::vk_queue_submit(&mut st.link, queue, submit_count, p_submits, fence)
        }
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
        Ok(()) => {
            for s in std::slice::from_raw_parts(p_submits, submit_count as usize) {
                let infos = std::slice::from_raw_parts(s.p_command_buffer_infos, s.command_buffer_info_count as usize);
                note_submit(st, infos.iter().map(|i| i.command_buffer.as_raw()));
            }
            gv::vk_queue_submit2(&mut st.link, queue, submit_count, p_submits, fence)
        }
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

/// The host driver's pipeline cache blob is not forwarded: the cache has no data to save.
/// Reporting an empty cache is valid (the application then starts cold).
pub unsafe extern "system" fn vk_get_pipeline_cache_data(
    _device: vk::Device,
    _pipeline_cache: vk::PipelineCache,
    p_data_size: *mut usize,
    _p_data: *mut c_void,
) -> vk::Result {
    if !p_data_size.is_null() {
        *p_data_size = 0;
    }
    vk::Result::SUCCESS
}

/// The generated forwarder creates the template on the host; the guest also keeps the entries,
/// because `vkUpdateDescriptorSetWithTemplate` reads the application's `pData` in the guest.
pub unsafe extern "system" fn vk_create_descriptor_update_template(
    device: vk::Device,
    p_create_info: *const vk::DescriptorUpdateTemplateCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_template: *mut vk::DescriptorUpdateTemplate,
) -> vk::Result {
    let r = crate::shims::shim_vk_create_descriptor_update_template(device, p_create_info, p_allocator, p_template);
    if r != vk::Result::SUCCESS || p_create_info.is_null() || p_template.is_null() {
        return r;
    }
    let ci = &*p_create_info;
    let entries = if ci.descriptor_update_entry_count == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(ci.p_descriptor_update_entries, ci.descriptor_update_entry_count as usize).to_vec()
    };
    let id = (*p_template).as_raw();
    let _ = with_state(|st| st.templates.insert(id, entries));
    r
}

pub unsafe extern "system" fn vk_destroy_descriptor_update_template(
    device: vk::Device,
    template: vk::DescriptorUpdateTemplate,
    p_allocator: *const vk::AllocationCallbacks,
) {
    let _ = with_state(|st| st.templates.remove(&template.as_raw()));
    crate::shims::shim_vk_destroy_descriptor_update_template(device, template, p_allocator);
}

/// Expands the template into ordinary descriptor writes: the host never sees `pData`.
pub unsafe extern "system" fn vk_update_descriptor_set_with_template(
    device: vk::Device,
    descriptor_set: vk::DescriptorSet,
    template: vk::DescriptorUpdateTemplate,
    p_data: *const c_void,
) {
    if p_data.is_null() {
        return;
    }
    let Some(Some(entries)) = with_state(|st| st.templates.get(&template.as_raw()).cloned()) else {
        crate::link::log("vkUpdateDescriptorSetWithTemplate: unknown template");
        return;
    };
    let base = p_data.cast::<u8>();
    // The info arrays must outlive the writes that point into them.
    let mut images: Vec<Vec<vk::DescriptorImageInfo>> = Vec::with_capacity(entries.len());
    let mut buffers: Vec<Vec<vk::DescriptorBufferInfo>> = Vec::with_capacity(entries.len());
    let mut views: Vec<Vec<vk::BufferView>> = Vec::with_capacity(entries.len());
    let mut writes: Vec<vk::WriteDescriptorSet<'static>> = Vec::with_capacity(entries.len());
    for e in &entries {
        let n = e.descriptor_count as usize;
        let at = |i: usize| base.add(e.offset + i * e.stride);
        let mut w = vk::WriteDescriptorSet {
            dst_set: descriptor_set,
            dst_binding: e.dst_binding,
            dst_array_element: e.dst_array_element,
            descriptor_count: e.descriptor_count,
            descriptor_type: e.descriptor_type,
            ..Default::default()
        };
        match e.descriptor_type {
            vk::DescriptorType::SAMPLER
            | vk::DescriptorType::COMBINED_IMAGE_SAMPLER
            | vk::DescriptorType::SAMPLED_IMAGE
            | vk::DescriptorType::STORAGE_IMAGE
            | vk::DescriptorType::INPUT_ATTACHMENT => {
                images.push((0..n).map(|i| std::ptr::read_unaligned(at(i).cast::<vk::DescriptorImageInfo>())).collect());
                w.p_image_info = images.last().unwrap().as_ptr();
            }
            vk::DescriptorType::UNIFORM_BUFFER
            | vk::DescriptorType::STORAGE_BUFFER
            | vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC
            | vk::DescriptorType::STORAGE_BUFFER_DYNAMIC => {
                buffers.push((0..n).map(|i| std::ptr::read_unaligned(at(i).cast::<vk::DescriptorBufferInfo>())).collect());
                w.p_buffer_info = buffers.last().unwrap().as_ptr();
            }
            vk::DescriptorType::UNIFORM_TEXEL_BUFFER | vk::DescriptorType::STORAGE_TEXEL_BUFFER => {
                views.push((0..n).map(|i| std::ptr::read_unaligned(at(i).cast::<vk::BufferView>())).collect());
                w.p_texel_buffer_view = views.last().unwrap().as_ptr();
            }
            other => {
                crate::link::log(&format!("vkUpdateDescriptorSetWithTemplate: descriptor type {} is not supported", other.as_raw()));
                continue;
            }
        }
        writes.push(w);
    }
    if !writes.is_empty() {
        crate::shims::shim_vk_update_descriptor_sets(device, writes.len() as u32, writes.as_ptr(), 0, std::ptr::null());
    }
}
