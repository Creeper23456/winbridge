// SPDX-License-Identifier: GPL-3.0-or-later

//! One guest connection's Vulkan state on the host: the handle tables, the driver objects the
//! guest created, and the host-visible memory it shadows.

use ash::vk::{self, Handle as _};
use std::collections::HashMap;
use std::sync::Arc;
use winbridge_gfx_proto::codec::CodecError;
use winbridge_gfx_vk::{Hooks, Host, Resolver};

/// Which host physical device a guest instance sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelect {
    /// Discrete GPU, else integrated, else virtual, else CPU; ties by enumeration order.
    Auto,
    /// Position in the driver's enumeration order.
    Index(usize),
    /// First device whose name contains this text (case-insensitive).
    NameContains(String),
}

impl DeviceSelect {
    pub fn parse(s: &str) -> DeviceSelect {
        if s == "auto" {
            DeviceSelect::Auto
        } else if let Ok(i) = s.parse() {
            DeviceSelect::Index(i)
        } else {
            DeviceSelect::NameContains(s.to_string())
        }
    }
}

pub(crate) struct Obj {
    pub(crate) raw: u64,
    pub(crate) kind: &'static str,
    pub(crate) parent: u64,
}

struct DeviceRec {
    dev: Arc<ash::Device>,
    phys: vk::PhysicalDevice,
    inst_raw: u64,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    atom: u64,
}

struct Alloc {
    dev: Arc<ash::Device>,
    memory: vk::DeviceMemory,
    size: u64,
    /// Null unless the memory type is host-visible; mapped for the allocation's lifetime.
    mapped: *mut u8,
    coherent: bool,
    atom: u64,
}

pub struct Session {
    entry: ash::Entry,
    select: DeviceSelect,
    pub(crate) objs: HashMap<u64, Obj>,
    /// Ids in creation order, for teardown.
    order: Vec<u64>,
    pub(crate) instances: HashMap<u64, Arc<ash::Instance>>,
    devices: HashMap<u64, DeviceRec>,
    pub(crate) swapchains: HashMap<u64, crate::present::Swapchain>,
    mems: HashMap<u64, Alloc>,
    /// Raw physical device to raw instance.
    phys_inst: HashMap<u64, u64>,
}

impl Session {
    pub fn new(entry: ash::Entry, select: DeviceSelect) -> Session {
        Session {
            entry,
            select,
            objs: HashMap::new(),
            order: Vec::new(),
            instances: HashMap::new(),
            devices: HashMap::new(),
            swapchains: HashMap::new(),
            mems: HashMap::new(),
            phys_inst: HashMap::new(),
        }
    }

    pub(crate) fn obj_raw(&self, id: u64, kind: &'static str) -> Result<u64, CodecError> {
        Ok(self.obj(id, kind)?.raw)
    }

    pub(crate) fn instance_raw_of_device(&self, device_raw: u64) -> Option<u64> {
        self.devices.get(&device_raw).map(|d| d.inst_raw)
    }

    pub(crate) fn phys_of_device(&self, device_raw: u64) -> Option<vk::PhysicalDevice> {
        self.devices.get(&device_raw).map(|d| d.phys)
    }

    fn obj(&self, id: u64, kind: &'static str) -> Result<&Obj, CodecError> {
        match self.objs.get(&id) {
            Some(o) if o.kind == kind => Ok(o),
            _ => Err(CodecError::BadHandle { id, kind }),
        }
    }

    fn pick(&self, inst: &ash::Instance, all: &[vk::PhysicalDevice]) -> Option<vk::PhysicalDevice> {
        let props: Vec<vk::PhysicalDeviceProperties> =
            all.iter().map(|p| unsafe { inst.get_physical_device_properties(*p) }).collect();
        match &self.select {
            DeviceSelect::Index(i) => all.get(*i).copied(),
            DeviceSelect::NameContains(n) => {
                let n = n.to_lowercase();
                props
                    .iter()
                    .position(|p| device_name(p).to_lowercase().contains(&n))
                    .map(|i| all[i])
            }
            DeviceSelect::Auto => {
                let rank = |t: vk::PhysicalDeviceType| match t {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                    vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
                    vk::PhysicalDeviceType::CPU => 3,
                    _ => 4,
                };
                (0..all.len()).min_by_key(|i| (rank(props[*i].device_type), *i)).map(|i| all[i])
            }
        }
    }

    /// Writes guest bytes into a host-visible allocation.
    pub fn mem_upload(&mut self, id: u64, offset: u64, data: &[u8]) -> Result<(), String> {
        let a = self.alloc_of(id)?;
        let end = offset.checked_add(data.len() as u64).filter(|e| *e <= a.size).ok_or("upload outside the allocation")?;
        // SAFETY: `mapped` covers `size` bytes and the range was bounds-checked above.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), a.mapped.add(offset as usize), data.len()) };
        if !a.coherent && !data.is_empty() {
            let r = a.range(offset, end);
            // SAFETY: the memory is mapped and the range is atom-aligned inside the allocation.
            unsafe { a.dev.flush_mapped_memory_ranges(&[r]) }.map_err(|e| format!("flush: {e}"))?;
        }
        Ok(())
    }

    /// Reads bytes back from a host-visible allocation.
    pub fn mem_download(&mut self, id: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        let a = self.alloc_of(id)?;
        let end = offset.checked_add(len).filter(|e| *e <= a.size).ok_or("download outside the allocation")?;
        if !a.coherent && len > 0 {
            let r = a.range(offset, end);
            // SAFETY: as in `mem_upload`.
            unsafe { a.dev.invalidate_mapped_memory_ranges(&[r]) }.map_err(|e| format!("invalidate: {e}"))?;
        }
        // SAFETY: bounds-checked against the mapped size.
        Ok(unsafe { std::slice::from_raw_parts(a.mapped.add(offset as usize), len as usize) }.to_vec())
    }

    fn alloc_of(&self, id: u64) -> Result<&Alloc, String> {
        let raw = self.obj(id, "VkDeviceMemory").map_err(|e| e.to_string())?.raw;
        let a = self.mems.get(&raw).ok_or("memory has no host bookkeeping")?;
        if a.mapped.is_null() {
            return Err("memory type is not host-visible".into());
        }
        Ok(a)
    }
}

impl Alloc {
    /// Atom-aligned range covering `[offset, end)`, clamped to the allocation.
    fn range(&self, offset: u64, end: u64) -> vk::MappedMemoryRange<'static> {
        let atom = self.atom.max(1);
        let start = offset / atom * atom;
        let end = end.div_ceil(atom).saturating_mul(atom).min(self.size);
        vk::MappedMemoryRange::default().memory(self.memory).offset(start).size(end - start)
    }
}

fn device_name(p: &vk::PhysicalDeviceProperties) -> String {
    // SAFETY: the driver NUL-terminates `device_name`.
    unsafe { std::ffi::CStr::from_ptr(p.device_name.as_ptr()) }.to_string_lossy().into_owned()
}

impl Resolver for Session {
    fn resolve(&self, id: u64, kind: &'static str) -> Result<u64, CodecError> {
        if id == 0 {
            return Ok(0);
        }
        Ok(self.obj(id, kind)?.raw)
    }
}

impl Host for Session {
    fn entry(&self) -> &ash::Entry {
        &self.entry
    }

    fn instance(&self, id: u64) -> Result<Arc<ash::Instance>, CodecError> {
        let raw = self.obj(id, "VkInstance")?.raw;
        self.instances.get(&raw).cloned().ok_or(CodecError::BadHandle { id, kind: "VkInstance" })
    }

    fn instance_of_physical(&self, id: u64) -> Result<Arc<ash::Instance>, CodecError> {
        let parent = self.obj(id, "VkPhysicalDevice")?.parent;
        self.instance(parent)
    }

    fn device(&self, id: u64) -> Result<Arc<ash::Device>, CodecError> {
        let o = self.objs.get(&id).ok_or(CodecError::BadHandle { id, kind: "VkDevice" })?;
        let raw = match o.kind {
            "VkDevice" => o.raw,
            "VkQueue" | "VkCommandBuffer" => self.obj(o.parent, "VkDevice")?.raw,
            _ => return Err(CodecError::BadHandle { id, kind: "VkDevice" }),
        };
        self.devices.get(&raw).map(|d| d.dev.clone()).ok_or(CodecError::BadHandle { id, kind: "VkDevice" })
    }

    fn device_proc(&self, id: u64, name: &std::ffi::CStr) -> Result<vk::PFN_vkVoidFunction, CodecError> {
        let o = self.objs.get(&id).ok_or(CodecError::BadHandle { id, kind: "VkDevice" })?;
        let dev = match o.kind {
            "VkDevice" => o,
            "VkQueue" | "VkCommandBuffer" => self.objs.get(&o.parent).ok_or(CodecError::BadHandle { id, kind: "VkDevice" })?,
            _ => return Err(CodecError::BadHandle { id, kind: "VkDevice" }),
        };
        let inst = self.instance_of_physical(dev.parent)?;
        // SAFETY: `dev.raw` is a live device of `inst`; `name` is NUL-terminated.
        Ok(unsafe { (inst.fp_v1_0().get_device_proc_addr)(vk::Device::from_raw(dev.raw), name.as_ptr()) })
    }

    fn register(&mut self, id: u64, raw: u64, kind: &'static str, parent: u64) -> Result<(), CodecError> {
        if id == 0 || self.objs.contains_key(&id) {
            return Err(CodecError::Invalid("handle id is null or already in use"));
        }
        match kind {
            "VkInstance" => {
                // SAFETY: `raw` is the instance the driver just created through this entry.
                let inst = unsafe { ash::Instance::load(self.entry.static_fn(), vk::Instance::from_raw(raw)) };
                self.instances.insert(raw, Arc::new(inst));
            }
            "VkDevice" => {
                let phys = self.obj(parent, "VkPhysicalDevice")?.raw;
                let inst = self.instance_of_physical(parent)?;
                let phys = vk::PhysicalDevice::from_raw(phys);
                // SAFETY: `raw` is the device the driver just created from `phys`.
                let dev = unsafe { ash::Device::load(inst.fp_v1_0(), vk::Device::from_raw(raw)) };
                // SAFETY: `phys` belongs to `inst`.
                let (mem_props, props) =
                    unsafe { (inst.get_physical_device_memory_properties(phys), inst.get_physical_device_properties(phys)) };
                self.devices.insert(
                    raw,
                    DeviceRec { dev: Arc::new(dev), phys, inst_raw: inst.handle().as_raw(), mem_props, atom: props.limits.non_coherent_atom_size },
                );
            }
            _ => {}
        }
        self.objs.insert(id, Obj { raw, kind, parent });
        self.order.push(id);
        Ok(())
    }

    fn unregister(&mut self, id: u64) {
        if let Some(o) = self.objs.remove(&id) {
            match o.kind {
                "VkInstance" => {
                    self.instances.remove(&o.raw);
                }
                "VkDevice" => {
                    self.devices.remove(&o.raw);
                }
                _ => {}
            }
        }
    }
}

impl Hooks for Session {
    /// The host instance asks for the newest Vulkan the loader offers (up to 1.3), whatever the
    /// application asked for: drivers hide the 1.1+ feature structs from a 1.0 instance, and the
    /// DXVK check of `vkCreateDevice` needs them. The guest still sees the device's own version.
    unsafe fn vk_create_instance(&mut self, p_create_info: *const vk::InstanceCreateInfo, p_instance: *mut vk::Instance) -> vk::Result {
        let loader = self.entry().try_enumerate_instance_version().ok().flatten().unwrap_or(vk::API_VERSION_1_0);
        let mut ci = *p_create_info;
        let mut app = if ci.p_application_info.is_null() { vk::ApplicationInfo::default() } else { *ci.p_application_info };
        app.api_version = app.api_version.max(loader.min(vk::API_VERSION_1_3));
        ci.p_application_info = &app;
        (self.entry().fp_v1_0().create_instance)(&ci, std::ptr::null(), p_instance)
    }

    unsafe fn vk_enumerate_physical_devices(
        &mut self,
        instance: vk::Instance,
        p_physical_device_count: *mut u32,
        p_physical_devices: *mut vk::PhysicalDevice,
    ) -> vk::Result {
        let Some(inst) = self.instances.get(&instance.as_raw()).cloned() else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        let all = match inst.enumerate_physical_devices() {
            Ok(v) => v,
            Err(e) => return e,
        };
        let chosen: Vec<vk::PhysicalDevice> = self.pick(&inst, &all).into_iter().collect();
        for p in &chosen {
            self.phys_inst.insert(p.as_raw(), instance.as_raw());
        }
        if p_physical_devices.is_null() {
            *p_physical_device_count = chosen.len() as u32;
            return vk::Result::SUCCESS;
        }
        let cap = *p_physical_device_count as usize;
        let n = chosen.len().min(cap);
        for (i, p) in chosen.iter().take(n).enumerate() {
            *p_physical_devices.add(i) = *p;
        }
        *p_physical_device_count = n as u32;
        if n < chosen.len() {
            vk::Result::INCOMPLETE
        } else {
            vk::Result::SUCCESS
        }
    }

    unsafe fn vk_create_device(
        &mut self,
        physical_device: vk::PhysicalDevice,
        p_create_info: *const vk::DeviceCreateInfo,
        p_device: *mut vk::Device,
    ) -> vk::Result {
        let Some(inst) = self.phys_inst.get(&physical_device.as_raw()).and_then(|i| self.instances.get(i)).cloned() else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        let missing: Vec<&str> =
            winbridge_gfx_vk::dxvk::evaluate(&inst, physical_device).into_iter().filter(|r| !r.1).map(|r| r.0).collect();
        if !missing.is_empty() {
            tracing::warn!("refusing vkCreateDevice: the host GPU lacks DXVK requirements: {}", missing.join(", "));
            return vk::Result::ERROR_FEATURE_NOT_PRESENT;
        }
        // Presentation copies swapchain images out on the host and uses the present layout:
        // enable VK_KHR_swapchain there when the device has it (the guest never sees the name).
        let have = inst.enumerate_device_extension_properties(physical_device).unwrap_or_default();
        let swapchain = c"VK_KHR_swapchain";
        let supported = have.iter().any(|e| std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) == swapchain);
        let info = &*p_create_info;
        let mut names: Vec<*const std::ffi::c_char> = (0..info.enabled_extension_count as usize).map(|i| *info.pp_enabled_extension_names.add(i)).collect();
        let present = names.iter().any(|n| std::ffi::CStr::from_ptr(*n) == swapchain);
        if supported && !present {
            names.push(swapchain.as_ptr());
        }
        let mut patched = *info;
        patched.enabled_extension_count = names.len() as u32;
        patched.pp_enabled_extension_names = names.as_ptr();
        (inst.fp_v1_0().create_device)(physical_device, &patched, std::ptr::null(), p_device)
    }

    unsafe fn vk_allocate_memory(
        &mut self,
        device: vk::Device,
        p_allocate_info: *const vk::MemoryAllocateInfo,
        p_memory: *mut vk::DeviceMemory,
    ) -> vk::Result {
        let Some(rec) = self.devices.get(&device.as_raw()) else {
            return vk::Result::ERROR_DEVICE_LOST;
        };
        let dev = rec.dev.clone();
        let res = (dev.fp_v1_0().allocate_memory)(device, p_allocate_info, std::ptr::null(), p_memory);
        if res != vk::Result::SUCCESS {
            return res;
        }
        let memory = *p_memory;
        let info = &*p_allocate_info;
        let flags = rec.mem_props.memory_types[info.memory_type_index as usize].property_flags;
        let mut mapped = std::ptr::null_mut();
        if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            match dev.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) {
                Ok(p) => mapped = p.cast::<u8>(),
                Err(e) => {
                    (dev.fp_v1_0().free_memory)(device, memory, std::ptr::null());
                    return e;
                }
            }
        }
        self.mems.insert(
            memory.as_raw(),
            Alloc {
                dev,
                memory,
                size: info.allocation_size,
                mapped,
                coherent: flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT),
                atom: rec.atom,
            },
        );
        vk::Result::SUCCESS
    }

    unsafe fn vk_free_memory(&mut self, device: vk::Device, memory: vk::DeviceMemory) {
        if let Some(a) = self.mems.remove(&memory.as_raw()) {
            if !a.mapped.is_null() {
                a.dev.unmap_memory(a.memory);
            }
        }
        if let Some(rec) = self.devices.get(&device.as_raw()) {
            (rec.dev.fp_v1_0().free_memory)(device, memory, std::ptr::null());
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl Session {
    /// Releases everything the guest left behind, newest first.
    pub fn release_all(&mut self) {
        self.drop_swapchains();
        let order = std::mem::take(&mut self.order);
        for id in order.into_iter().rev() {
            let Some(o) = self.objs.get(&id) else { continue };
            let (raw, kind, parent) = (o.raw, o.kind, o.parent);
            // SAFETY: each object is destroyed once, children before parents, and nothing
            // else uses the driver handles any more.
            unsafe {
                match kind {
                    "VkBuffer" => {
                        if let Ok(dev) = self.device(parent) {
                            dev.destroy_buffer(vk::Buffer::from_raw(raw), None);
                        }
                    }
                    "VkDeviceMemory" => {
                        if let Some(a) = self.mems.remove(&raw) {
                            if !a.mapped.is_null() {
                                a.dev.unmap_memory(a.memory);
                            }
                            a.dev.free_memory(a.memory, None);
                        }
                    }
                    "VkDevice" => {
                        if let Some(rec) = self.devices.remove(&raw) {
                            let _ = rec.dev.device_wait_idle();
                            rec.dev.destroy_device(None);
                        }
                    }
                    "VkInstance" => {
                        if let Some(inst) = self.instances.remove(&raw) {
                            inst.destroy_instance(None);
                        }
                    }
                    _ => {}
                }
            }
            self.objs.remove(&id);
        }
    }
}
