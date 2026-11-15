// SPDX-License-Identifier: GPL-3.0-or-later

//! Render test of the probe: a triangle drawn with a render pass and a graphics pipeline.
//!
//! `--render [--frames N]` presents N frames to a window (Windows) or a headless surface
//! (elsewhere) through whatever Vulkan implementation the loader finds. `--reference W H`
//! renders the same frame offscreen without a surface and prints its checksum; the host test
//! compares it with the checksum the WinBridge ICD logs for the frame it received.

use ash::vk;
use std::ffi::c_char;
use std::time::Instant;

const SPV: &[u8] = include_bytes!("../shaders/tri.spv");
pub const CLEAR: [f32; 4] = [0.05, 0.10, 0.20, 1.0];
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

fn spv_words() -> Vec<u32> {
    SPV.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

struct Scene {
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    module: vk::ShaderModule,
    extent: vk::Extent2D,
}

impl Scene {
    unsafe fn new(device: &ash::Device, extent: vk::Extent2D, final_layout: vk::ImageLayout) -> Result<Scene, String> {
        let attachment = vk::AttachmentDescription::default()
            .format(FORMAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(final_layout);
        let color_ref = [vk::AttachmentReference::default().attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color_ref)];
        let dep = [vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
        let attachments = [attachment];
        let render_pass = device
            .create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpass).dependencies(&dep), None)
            .map_err(vkerr("vkCreateRenderPass"))?;
        let words = spv_words();
        let module = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None).map_err(vkerr("vkCreateShaderModule"))?;
        let pipeline_layout = device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None).map_err(vkerr("vkCreatePipelineLayout"))?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(module).name(c"vs"),
            vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(module).name(c"fs"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport { x: 0.0, y: 0.0, width: extent.width as f32, height: extent.height as f32, min_depth: 0.0, max_depth: 1.0 }];
        let scissors = [vk::Rect2D { offset: vk::Offset2D::default(), extent }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default().viewports(&viewports).scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL).cull_mode(vk::CullModeFlags::NONE).front_face(vk::FrontFace::COUNTER_CLOCKWISE).line_width(1.0);
        let samples = [u32::MAX];
        let multisample = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1).sample_mask(&samples);
        let blend_att = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_att);
        let ci = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0)];
        let pipeline = device.create_graphics_pipelines(vk::PipelineCache::null(), &ci, None).map_err(|(_, e)| format!("vkCreateGraphicsPipelines: {e}"))?[0];
        Ok(Scene { render_pass, pipeline_layout, pipeline, module, extent })
    }

    unsafe fn record(&self, device: &ash::Device, cb: vk::CommandBuffer, framebuffer: vk::Framebuffer) -> Result<(), String> {
        device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).map_err(vkerr("vkBeginCommandBuffer"))?;
        let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: CLEAR } }];
        let rp = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent })
            .clear_values(&clear);
        device.cmd_begin_render_pass(cb, &rp, vk::SubpassContents::INLINE);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
        device.cmd_draw(cb, 3, 1, 0, 0);
        device.cmd_end_render_pass(cb);
        device.end_command_buffer(cb).map_err(vkerr("vkEndCommandBuffer"))
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        device.destroy_pipeline(self.pipeline, None);
        device.destroy_pipeline_layout(self.pipeline_layout, None);
        device.destroy_shader_module(self.module, None);
        device.destroy_render_pass(self.render_pass, None);
    }
}

unsafe fn make_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    qf: u32,
    swapchain: bool,
) -> Result<ash::Device, String> {
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(qf).queue_priorities(&prio)];
    let exts: Vec<*const c_char> = if swapchain { vec![c"VK_KHR_swapchain".as_ptr()] } else { Vec::new() };
    instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qci).enabled_extension_names(&exts), None).map_err(vkerr("vkCreateDevice"))
}

/// Offscreen render of the scene; returns the FNV-1a of the BGRA8 pixels.
pub fn reference(width: u32, height: u32, device_name: &str, ppm_out: Option<&str>) -> Result<u64, String> {
    // SAFETY: standard Vulkan use; every object is destroyed before returning.
    unsafe {
        let entry = ash::Entry::load().map_err(|e| e.to_string())?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).map_err(vkerr("vkCreateInstance"))?;
        let r = (|| {
            let pds = instance.enumerate_physical_devices().map_err(vkerr("vkEnumeratePhysicalDevices"))?;
            let pd = pds
                .into_iter()
                .find(|p| {
                    let props = instance.get_physical_device_properties(*p);
                    std::ffi::CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy().contains(device_name)
                })
                .ok_or_else(|| format!("no device named *{device_name}*"))?;
            let qf = instance.get_physical_device_queue_family_properties(pd).iter().position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS)).ok_or("no graphics queue")? as u32;
            let device = make_device(&instance, pd, qf, false)?;
            let r = render_offscreen(&instance, &device, pd, qf, width, height, ppm_out);
            device.destroy_device(None);
            r
        })();
        instance.destroy_instance(None);
        r
    }
}

unsafe fn render_offscreen(instance: &ash::Instance, device: &ash::Device, pd: vk::PhysicalDevice, qf: u32, w: u32, h: u32, ppm_out: Option<&str>) -> Result<u64, String> {
    let extent = vk::Extent2D { width: w, height: h };
    let mp = instance.get_physical_device_memory_properties(pd);
    let pick = |bits: u32, want: vk::MemoryPropertyFlags| (0..mp.memory_type_count).find(|i| bits & (1 << i) != 0 && mp.memory_types[*i as usize].property_flags.contains(want));
    let image = device
        .create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(FORMAT)
                .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
        .map_err(vkerr("vkCreateImage"))?;
    let req = device.get_image_memory_requirements(image);
    let imem = device
        .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(pick(req.memory_type_bits, vk::MemoryPropertyFlags::empty()).ok_or("no image memory")?), None)
        .map_err(vkerr("vkAllocateMemory"))?;
    device.bind_image_memory(image, imem, 0).map_err(vkerr("vkBindImageMemory"))?;
    let view = device
        .create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(FORMAT)
                .subresource_range(vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),
            None,
        )
        .map_err(vkerr("vkCreateImageView"))?;
    let scene = Scene::new(device, extent, vk::ImageLayout::TRANSFER_SRC_OPTIMAL)?;
    let views = [view];
    let fb = device
        .create_framebuffer(&vk::FramebufferCreateInfo::default().render_pass(scene.render_pass).attachments(&views).width(w).height(h).layers(1), None)
        .map_err(vkerr("vkCreateFramebuffer"))?;
    let size = w as u64 * h as u64 * 4;
    let buf = device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST), None).map_err(vkerr("vkCreateBuffer"))?;
    let breq = device.get_buffer_memory_requirements(buf);
    let bty = pick(breq.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT).ok_or("no host memory")?;
    let bmem = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(breq.size).memory_type_index(bty), None).map_err(vkerr("vkAllocateMemory"))?;
    device.bind_buffer_memory(buf, bmem, 0).map_err(vkerr("vkBindBufferMemory"))?;
    let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(qf), None).map_err(vkerr("vkCreateCommandPool"))?;
    let cb = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)).map_err(vkerr("vkAllocateCommandBuffers"))?[0];
    device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).map_err(vkerr("vkBeginCommandBuffer"))?;
    let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: CLEAR } }];
    device.cmd_begin_render_pass(
        cb,
        &vk::RenderPassBeginInfo::default().render_pass(scene.render_pass).framebuffer(fb).render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent }).clear_values(&clear),
        vk::SubpassContents::INLINE,
    );
    device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, scene.pipeline);
    device.cmd_draw(cb, 3, 1, 0, 0);
    device.cmd_end_render_pass(cb);
    let region = vk::BufferImageCopy::default().image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1)).image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
    device.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf, &[region]);
    device.end_command_buffer(cb).map_err(vkerr("vkEndCommandBuffer"))?;
    let queue = device.get_device_queue(qf, 0);
    let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(vkerr("vkCreateFence"))?;
    let cbs = [cb];
    device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).map_err(vkerr("vkQueueSubmit"))?;
    device.wait_for_fences(&[fence], true, 10_000_000_000).map_err(vkerr("vkWaitForFences"))?;
    let p = device.map_memory(bmem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).map_err(vkerr("vkMapMemory"))? as *const u8;
    let pixels = std::slice::from_raw_parts(p, size as usize);
    let sum = fnv1a(pixels);
    if let Some(path) = ppm_out {
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in pixels.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        std::fs::write(path, out).map_err(|e| format!("write {path}: {e}"))?;
    }
    device.unmap_memory(bmem);
    device.destroy_fence(fence, None);
    device.destroy_command_pool(pool, None);
    device.destroy_buffer(buf, None);
    device.free_memory(bmem, None);
    device.destroy_framebuffer(fb, None);
    scene.destroy(device);
    device.destroy_image_view(view, None);
    device.destroy_image(image, None);
    device.free_memory(imem, None);
    Ok(sum)
}

#[cfg(windows)]
mod window {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW, ShowWindow, TranslateMessage, AdjustWindowRect, MSG, PM_REMOVE, SW_SHOW, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE, CW_USEDEFAULT};
    use windows_sys::Win32::Foundation::RECT;

    unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// A visible window with a client area of `w` x `h`.
    pub unsafe fn create(w: u32, h: u32) -> Result<(isize, isize), String> {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("WinBridgeGfxProbe");
        let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hinst, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, 0);
        let title = wide("WinBridge GPU probe");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | WS_VISIBLE, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null::<c_void>());
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW: {}", std::io::Error::last_os_error()));
        }
        ShowWindow(hwnd, SW_SHOW);
        Ok((hinst as isize, hwnd as isize))
    }

    pub unsafe fn pump(hwnd: isize) {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, hwnd as HWND, 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Presents `frames` frames of the triangle. `size` is the client size of the window (Windows)
/// or the `WINBRIDGE_GFX_WINDOW` size the ICD reports for the headless surface.
pub fn present(frames: u32, hold_ms: u64) -> Result<(), String> {
    // SAFETY: standard Vulkan use; objects are destroyed in reverse order before returning.
    unsafe {
        let entry = ash::Entry::load().map_err(|e| e.to_string())?;
        let app = vk::ApplicationInfo::default().application_name(c"winbridge-gfx-probe").api_version(vk::API_VERSION_1_3);
        #[cfg(windows)]
        let exts = [c"VK_KHR_surface".as_ptr(), c"VK_KHR_win32_surface".as_ptr()];
        #[cfg(not(windows))]
        let exts = [c"VK_KHR_surface".as_ptr(), c"VK_EXT_headless_surface".as_ptr()];
        let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts), None).map_err(vkerr("vkCreateInstance"))?;
        let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);
        #[cfg(windows)]
        let (hwnd, surface) = {
            let size = std::env::var("WINBRIDGE_GFX_WINDOW").ok().and_then(|v| v.split_once('x').map(|(a, b)| (a.parse().ok(), b.parse().ok()))).and_then(|(a, b)| Some((a?, b?))).unwrap_or((1280u32, 720u32));
            let (hinst, hwnd) = window::create(size.0, size.1)?;
            let win32 = ash::khr::win32_surface::Instance::new(&entry, &instance);
            let ci = vk::Win32SurfaceCreateInfoKHR::default().hinstance(hinst).hwnd(hwnd);
            (hwnd, win32.create_win32_surface(&ci, None).map_err(vkerr("vkCreateWin32SurfaceKHR"))?)
        };
        #[cfg(not(windows))]
        let surface = ash::ext::headless_surface::Instance::new(&entry, &instance)
            .create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
            .map_err(vkerr("vkCreateHeadlessSurfaceEXT"))?;
        let pds = instance.enumerate_physical_devices().map_err(vkerr("vkEnumeratePhysicalDevices"))?;
        let pd = *pds.first().ok_or("no physical device")?;
        let qfs = instance.get_physical_device_queue_family_properties(pd);
        let qf = (0..qfs.len() as u32)
            .find(|i| qfs[*i as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS) && surface_fn.get_physical_device_surface_support(pd, *i, surface).unwrap_or(false))
            .ok_or("no graphics queue family that can present")?;
        let caps = surface_fn.get_physical_device_surface_capabilities(pd, surface).map_err(vkerr("vkGetPhysicalDeviceSurfaceCapabilitiesKHR"))?;
        let formats = surface_fn.get_physical_device_surface_formats(pd, surface).map_err(vkerr("vkGetPhysicalDeviceSurfaceFormatsKHR"))?;
        let modes = surface_fn.get_physical_device_surface_present_modes(pd, surface).map_err(vkerr("vkGetPhysicalDeviceSurfacePresentModesKHR"))?;
        println!("surface: {}x{} images {}..{} formats {} modes {:?}", caps.current_extent.width, caps.current_extent.height, caps.min_image_count, caps.max_image_count, formats.len(), modes);
        let format = formats.iter().find(|f| f.format == FORMAT).ok_or("surface has no B8G8R8A8_UNORM")?;
        let device = make_device(&instance, pd, qf, true)?;
        let sc_fn = ash::khr::swapchain::Device::new(&instance, &device);
        let extent = caps.current_extent;
        let sc = sc_fn
            .create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(surface)
                    .min_image_count(caps.min_image_count.max(2))
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                    .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                    .present_mode(vk::PresentModeKHR::FIFO)
                    .clipped(true),
                None,
            )
            .map_err(vkerr("vkCreateSwapchainKHR"))?;
        let images = sc_fn.get_swapchain_images(sc).map_err(vkerr("vkGetSwapchainImagesKHR"))?;
        println!("swapchain_images: {}", images.len());
        let scene = Scene::new(&device, extent, vk::ImageLayout::PRESENT_SRC_KHR)?;
        let mut views = Vec::new();
        let mut fbs = Vec::new();
        for img in &images {
            let v = device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default().image(*img).view_type(vk::ImageViewType::TYPE_2D).format(FORMAT).subresource_range(vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),
                    None,
                )
                .map_err(vkerr("vkCreateImageView"))?;
            let att = [v];
            fbs.push(device.create_framebuffer(&vk::FramebufferCreateInfo::default().render_pass(scene.render_pass).attachments(&att).width(extent.width).height(extent.height).layers(1), None).map_err(vkerr("vkCreateFramebuffer"))?);
            views.push(v);
        }
        let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(qf), None).map_err(vkerr("vkCreateCommandPool"))?;
        let cbs = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(images.len() as u32)).map_err(vkerr("vkAllocateCommandBuffers"))?;
        for (cb, fb) in cbs.iter().zip(&fbs) {
            scene.record(&device, *cb, *fb)?;
        }
        let acquire_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(vkerr("vkCreateSemaphore"))?;
        let render_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(vkerr("vkCreateSemaphore"))?;
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(vkerr("vkCreateFence"))?;
        let queue = device.get_device_queue(qf, 0);
        let mut times = Vec::new();
        for n in 0..frames {
            let t = Instant::now();
            let (index, _) = sc_fn.acquire_next_image(sc, u64::MAX, acquire_sem, vk::Fence::null()).map_err(vkerr("vkAcquireNextImageKHR"))?;
            let waits = [acquire_sem];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let signals = [render_sem];
            let cb = [cbs[index as usize]];
            device.queue_submit(queue, &[vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&cb).signal_semaphores(&signals)], fence).map_err(vkerr("vkQueueSubmit"))?;
            let swapchains = [sc];
            let indices = [index];
            sc_fn.queue_present(queue, &vk::PresentInfoKHR::default().wait_semaphores(&signals).swapchains(&swapchains).image_indices(&indices)).map_err(vkerr("vkQueuePresentKHR"))?;
            device.wait_for_fences(&[fence], true, 10_000_000_000).map_err(vkerr("vkWaitForFences"))?;
            device.reset_fences(&[fence]).map_err(vkerr("vkResetFences"))?;
            times.push(t.elapsed().as_secs_f64() * 1e3);
            #[cfg(windows)]
            window::pump(hwnd);
            println!("frame {n}: {:.2} ms (image {index})", times[n as usize]);
        }
        times.sort_by(f64::total_cmp);
        if !times.is_empty() {
            println!("frame_ms: p50 {:.2} min {:.2} max {:.2} over {} frames", times[times.len() / 2], times[0], times[times.len() - 1], times.len());
        }
        #[cfg(windows)]
        {
            // Keep the window on screen (and answering messages) so that it can be captured.
            let end = Instant::now() + std::time::Duration::from_millis(hold_ms);
            while Instant::now() < end {
                window::pump(hwnd);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        #[cfg(not(windows))]
        let _ = hold_ms;
        device.device_wait_idle().map_err(vkerr("vkDeviceWaitIdle"))?;
        device.destroy_fence(fence, None);
        device.destroy_semaphore(render_sem, None);
        device.destroy_semaphore(acquire_sem, None);
        device.destroy_command_pool(pool, None);
        for fb in fbs {
            device.destroy_framebuffer(fb, None);
        }
        for v in views {
            device.destroy_image_view(v, None);
        }
        scene.destroy(&device);
        sc_fn.destroy_swapchain(sc, None);
        device.destroy_device(None);
        surface_fn.destroy_surface(surface, None);
        instance.destroy_instance(None);
        println!("present_ok: {frames}");
        Ok(())
    }
}
