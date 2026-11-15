// SPDX-License-Identifier: GPL-3.0-or-later

//! Swapchains of the guest, kept as host images, and presentation by readback.

use crate::session::{Obj, Session};
use ash::vk::{self, Handle as _};
use std::sync::Arc;
use winbridge_gfx_vk::Host;
use winbridge_gfx_proto::{Frame, ScAcquire, ScCreate, ScPresent};

pub struct Swapchain {
    dev: Arc<ash::Device>,
    device_raw: u64,
    ids: Vec<u64>,
    images: Vec<(vk::Image, vk::DeviceMemory)>,
    width: u32,
    height: u32,
    readback: vk::Buffer,
    readback_mem: vk::DeviceMemory,
    mapped: *const u8,
    coherent: bool,
    /// Created lazily for the family of the first presenting queue.
    pool: Option<(u32, vk::CommandPool, vk::CommandBuffer)>,
    fence: vk::Fence,
    /// The previous frame, to find the bands that changed.
    prev: Vec<u8>,
}

impl Swapchain {
    unsafe fn destroy(&mut self) {
        let _ = self.dev.device_wait_idle();
        if let Some((_, pool, _)) = self.pool.take() {
            self.dev.destroy_command_pool(pool, None);
        }
        self.dev.destroy_fence(self.fence, None);
        self.dev.unmap_memory(self.readback_mem);
        self.dev.destroy_buffer(self.readback, None);
        self.dev.free_memory(self.readback_mem, None);
        for (img, mem) in self.images.drain(..) {
            self.dev.destroy_image(img, None);
            self.dev.free_memory(mem, None);
        }
    }
}

fn find_type(mp: &vk::PhysicalDeviceMemoryProperties, bits: u32, want: vk::MemoryPropertyFlags, avoid: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..mp.memory_type_count).find(|i| {
        let f = mp.memory_types[*i as usize].property_flags;
        bits & (1 << i) != 0 && f.contains(want) && !f.intersects(avoid)
    })
}

impl Session {
    /// Creates the host images behind a guest swapchain. Returns the Vulkan result.
    pub fn sc_create(&mut self, req: &ScCreate) -> Result<vk::Result, String> {
        let dev = self.device(req.device).map_err(|e| e.to_string())?;
        let device_raw = self.obj_raw(req.device, "VkDevice").map_err(|e| e.to_string())?;
        let inst_raw = self.instance_raw_of_device(device_raw).ok_or("device has no instance")?;
        let phys = self.phys_of_device(device_raw).ok_or("device has no physical device")?;
        let inst = self.instances.get(&inst_raw).cloned().ok_or("unknown instance")?;
        let mp = unsafe { inst.get_physical_device_memory_properties(phys) };
        let (w, h) = (req.width, req.height);
        if w == 0 || h == 0 || w > 16384 || h > 16384 || req.images.is_empty() || req.images.len() > 8 {
            return Ok(vk::Result::ERROR_INITIALIZATION_FAILED);
        }
        let format = vk::Format::from_raw(req.format as i32);
        if format != vk::Format::B8G8R8A8_UNORM && format != vk::Format::B8G8R8A8_SRGB {
            return Ok(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
        }
        // The host always adds what presentation needs: copy-out and the layouts the guest uses.
        let usage = vk::ImageUsageFlags::from_raw(req.usage) | vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
        let mut sc = Swapchain {
            dev: dev.clone(),
            device_raw,
            ids: Vec::new(),
            images: Vec::new(),
            width: w,
            height: h,
            readback: vk::Buffer::null(),
            readback_mem: vk::DeviceMemory::null(),
            mapped: std::ptr::null(),
            coherent: true,
            pool: None,
            fence: vk::Fence::null(),
            prev: Vec::new(),
        };
        let built = unsafe {
            (|| -> Result<(), vk::Result> {
                for _ in &req.images {
                    let ici = vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED);
                    let img = dev.create_image(&ici, None)?;
                    let r = dev.get_image_memory_requirements(img);
                    let ty = find_type(&mp, r.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty())
                        .or_else(|| find_type(&mp, r.memory_type_bits, vk::MemoryPropertyFlags::empty(), vk::MemoryPropertyFlags::empty()))
                        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
                    let mem = match dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(r.size).memory_type_index(ty), None) {
                        Ok(m) => m,
                        Err(e) => {
                            dev.destroy_image(img, None);
                            return Err(e);
                        }
                    };
                    sc.images.push((img, mem));
                    dev.bind_image_memory(img, mem, 0)?;
                }
                let size = w as u64 * h as u64 * 4;
                sc.readback = dev.create_buffer(
                    &vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST).sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )?;
                let r = dev.get_buffer_memory_requirements(sc.readback);
                let vis = vk::MemoryPropertyFlags::HOST_VISIBLE;
                let ty = find_type(&mp, r.memory_type_bits, vis | vk::MemoryPropertyFlags::HOST_CACHED | vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::empty())
                    .or_else(|| find_type(&mp, r.memory_type_bits, vis | vk::MemoryPropertyFlags::HOST_CACHED, vk::MemoryPropertyFlags::empty()))
                    .or_else(|| find_type(&mp, r.memory_type_bits, vis, vk::MemoryPropertyFlags::empty()))
                    .ok_or(vk::Result::ERROR_OUT_OF_HOST_MEMORY)?;
                sc.coherent = mp.memory_types[ty as usize].property_flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT);
                sc.readback_mem = dev.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(r.size).memory_type_index(ty), None)?;
                dev.bind_buffer_memory(sc.readback, sc.readback_mem, 0)?;
                sc.mapped = dev.map_memory(sc.readback_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?.cast::<u8>();
                sc.fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)?;
                Ok(())
            })()
        };
        if let Err(e) = built {
            // Release what was created; the readback objects may be partially set up.
            unsafe {
                for (img, mem) in sc.images.drain(..) {
                    dev.destroy_image(img, None);
                    dev.free_memory(mem, None);
                }
                if sc.fence != vk::Fence::null() {
                    dev.destroy_fence(sc.fence, None);
                }
                if !sc.mapped.is_null() {
                    dev.unmap_memory(sc.readback_mem);
                }
                if sc.readback != vk::Buffer::null() {
                    dev.destroy_buffer(sc.readback, None);
                }
                if sc.readback_mem != vk::DeviceMemory::null() {
                    dev.free_memory(sc.readback_mem, None);
                }
            }
            return Ok(e);
        }
        for (i, id) in req.images.iter().enumerate() {
            if self.objs.contains_key(id) || *id == 0 {
                unsafe { sc.destroy() };
                return Err("swapchain image id is null or in use".into());
            }
            self.objs.insert(*id, Obj { raw: sc.images[i].0.as_raw(), kind: "VkImage", parent: req.device });
            sc.ids.push(*id);
        }
        self.swapchains.insert(req.swapchain, sc);
        Ok(vk::Result::SUCCESS)
    }

    pub fn sc_destroy(&mut self, id: u64) {
        if let Some(mut sc) = self.swapchains.remove(&id) {
            for i in &sc.ids {
                self.objs.remove(i);
            }
            // SAFETY: the swapchain owns these objects and nothing else references them.
            unsafe { sc.destroy() };
        }
    }

    /// Signals the acquire semaphore and fence, as an empty queue submission does.
    pub fn sc_acquire(&mut self, req: &ScAcquire) -> Result<vk::Result, String> {
        let sc = self.swapchains.get(&req.swapchain).ok_or("unknown swapchain")?;
        if req.index as usize >= sc.images.len() {
            return Ok(vk::Result::ERROR_OUT_OF_DATE_KHR);
        }
        let dev = sc.dev.clone();
        let queue = vk::Queue::from_raw(self.obj_raw(req.queue, "VkQueue").map_err(|e| e.to_string())?);
        let sem = if req.semaphore != 0 { Some(vk::Semaphore::from_raw(self.obj_raw(req.semaphore, "VkSemaphore").map_err(|e| e.to_string())?)) } else { None };
        let fence = if req.fence != 0 { vk::Fence::from_raw(self.obj_raw(req.fence, "VkFence").map_err(|e| e.to_string())?) } else { vk::Fence::null() };
        let sems: Vec<vk::Semaphore> = sem.into_iter().collect();
        // SAFETY: handles were validated against the id tables.
        let r = unsafe { dev.queue_submit(queue, &[vk::SubmitInfo::default().signal_semaphores(&sems)], fence) };
        Ok(r.err().unwrap_or(vk::Result::SUCCESS))
    }

    /// Copies the image to host memory and returns the changed bands.
    pub fn sc_present(&mut self, req: &ScPresent) -> Result<(vk::Result, Frame), String> {
        let queue = vk::Queue::from_raw(self.obj_raw(req.queue, "VkQueue").map_err(|e| e.to_string())?);
        let waits: Vec<vk::Semaphore> = req
            .waits
            .iter()
            .map(|w| self.obj_raw(*w, "VkSemaphore").map(vk::Semaphore::from_raw))
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let sc = self.swapchains.get_mut(&req.swapchain).ok_or("unknown swapchain")?;
        let (w, h) = (sc.width, sc.height);
        let empty = Frame { width: w, height: h, band_rows: Frame::band_rows_for(w), bands: Vec::new() };
        let Some(&(image, _)) = sc.images.get(req.index as usize) else {
            return Ok((vk::Result::ERROR_OUT_OF_DATE_KHR, empty));
        };
        let dev = sc.dev.clone();
        // SAFETY: every handle below belongs to `dev`; the submission is waited for before return.
        unsafe {
            if sc.pool.as_ref().map(|p| p.0) != Some(req.family) {
                if let Some((_, pool, _)) = sc.pool.take() {
                    dev.destroy_command_pool(pool, None);
                }
                let pool = dev
                    .create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(req.family), None)
                    .map_err(|e| e.to_string())?;
                let cb = dev
                    .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
                    .map_err(|e| e.to_string())?[0];
                sc.pool = Some((req.family, pool, cb));
            }
            let cb = sc.pool.as_ref().unwrap().2;
            dev.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty()).map_err(|e| e.to_string())?;
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(|e| e.to_string())?;
            let range = vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
            let to_src = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range);
            dev.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_src]);
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
            dev.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, sc.readback, &[region]);
            let to_host = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(sc.readback)
                .size(vk::WHOLE_SIZE);
            dev.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::HOST, vk::DependencyFlags::empty(), &[], &[to_host], &[]);
            dev.end_command_buffer(cb).map_err(|e| e.to_string())?;
            let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
            let cbs = [cb];
            let submit = vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&cbs);
            dev.queue_submit(queue, &[submit], sc.fence).map_err(|e| e.to_string())?;
            dev.wait_for_fences(&[sc.fence], true, 10_000_000_000).map_err(|e| format!("present wait: {e}"))?;
            dev.reset_fences(&[sc.fence]).map_err(|e| e.to_string())?;
            if !sc.coherent {
                let r = vk::MappedMemoryRange::default().memory(sc.readback_mem).size(vk::WHOLE_SIZE);
                dev.invalidate_mapped_memory_ranges(&[r]).map_err(|e| e.to_string())?;
            }
        }
        // SAFETY: the buffer is `w * h * 4` bytes, mapped for the swapchain's lifetime.
        let pixels = unsafe { std::slice::from_raw_parts(sc.mapped, w as usize * h as usize * 4) };
        let band_rows = Frame::band_rows_for(w);
        let stride = w as usize * 4;
        let mut bands = Vec::new();
        for (i, rows) in (0..h).step_by(band_rows as usize).enumerate() {
            let (a, b) = (rows as usize * stride, ((rows + band_rows).min(h)) as usize * stride);
            if sc.prev.len() != pixels.len() || sc.prev[a..b] != pixels[a..b] {
                bands.push((i as u32, pixels[a..b].to_vec()));
            }
        }
        sc.prev.clear();
        sc.prev.extend_from_slice(pixels);
        Ok((vk::Result::SUCCESS, Frame { width: w, height: h, band_rows, bands }))
    }

    pub(crate) fn drop_swapchains(&mut self) {
        let ids: Vec<u64> = self.swapchains.keys().copied().collect();
        for id in ids {
            self.sc_destroy(id);
        }
    }

    #[allow(dead_code)]
    pub fn swapchain_device(&self, id: u64) -> Option<u64> {
        self.swapchains.get(&id).map(|s| s.device_raw)
    }
}
