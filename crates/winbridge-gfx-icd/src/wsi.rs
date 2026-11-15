// SPDX-License-Identifier: GPL-3.0-or-later

//! `VK_KHR_surface` (as seen by an ICD), `VK_KHR_swapchain` and presentation into the window.
//!
//! The loader creates the surface objects itself and hands the ICD a pointer to a
//! `VkIcdSurfaceBase`; the ICD only reads the platform and the window handle from it. The
//! swapchain images live on the host; a present is one synchronous request whose reply carries
//! the changed bands of the frame, which are then drawn into the window.

use crate::custom::flush_dirty;
use crate::{with_state, State};
use ash::vk::{self, Handle as _};
#[cfg(windows)]
use std::ffi::c_void;
use winbridge_gfx_proto::codec::Dec;
use winbridge_gfx_proto::{Frame, ScAcquire, ScCreate, ScPresent, OP_SC_ACQUIRE, OP_SC_CREATE, OP_SC_DESTROY, OP_SC_PRESENT};
use winbridge_gfx_vk as gv;

const PLATFORM_WIN32: i32 = 2;
const PLATFORM_HEADLESS: i32 = 9;
/// Swapchain sizes the ICD accepts.
const MIN_IMAGES: u32 = 2;
const MAX_IMAGES: u32 = 8;
const FORMATS: [vk::Format; 2] = [vk::Format::B8G8R8A8_UNORM, vk::Format::B8G8R8A8_SRGB];

#[repr(C)]
struct IcdSurfaceBase {
    platform: i32,
}

#[repr(C)]
struct IcdSurfaceWin32 {
    base: IcdSurfaceBase,
    hinstance: usize,
    hwnd: usize,
}

pub struct Swapchain {
    device_id: u64,
    width: u32,
    height: u32,
    images: Vec<u64>,
    next: u32,
    /// BGRA8 frame as last presented.
    frame: Vec<u8>,
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: usize,
    presents: u64,
}

/// Window handle and client size of a loader-created surface.
unsafe fn surface_window(surface: vk::SurfaceKHR) -> Option<(i32, usize, u32, u32)> {
    let base = surface.as_raw() as *const IcdSurfaceBase;
    if base.is_null() {
        return None;
    }
    let platform = (*base).platform;
    match platform {
        PLATFORM_WIN32 => {
            let hwnd = (*(base as *const IcdSurfaceWin32)).hwnd;
            let (w, h) = client_size(hwnd);
            Some((platform, hwnd, w, h))
        }
        PLATFORM_HEADLESS => {
            // The host loopback tests have no window: their size comes from the environment.
            let (w, h) = std::env::var("WINBRIDGE_GFX_WINDOW")
                .ok()
                .and_then(|v| v.split_once('x').map(|(a, b)| (a.parse().ok(), b.parse().ok())))
                .and_then(|(a, b)| Some((a?, b?)))
                .unwrap_or((640, 480));
            Some((platform, 0, w, h))
        }
        _ => None,
    }
}

#[cfg(windows)]
unsafe fn client_size(hwnd: usize) -> (u32, u32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let mut r: RECT = std::mem::zeroed();
    if GetClientRect(hwnd as *mut c_void, &mut r) == 0 {
        return (0, 0);
    }
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

#[cfg(not(windows))]
unsafe fn client_size(_hwnd: usize) -> (u32, u32) {
    (0, 0)
}

pub unsafe extern "system" fn vk_get_physical_device_surface_support_khr(
    physical_device: vk::PhysicalDevice,
    queue_family_index: u32,
    surface: vk::SurfaceKHR,
    p_supported: *mut vk::Bool32,
) -> vk::Result {
    if surface_window(surface).is_none() {
        return vk::Result::ERROR_SURFACE_LOST_KHR;
    }
    with_state(|st| {
        let mut n = 0u32;
        gv::vk_get_physical_device_queue_family_properties(&mut st.link, physical_device, &mut n, std::ptr::null_mut());
        let mut props = vec![vk::QueueFamilyProperties::default(); n as usize];
        gv::vk_get_physical_device_queue_family_properties(&mut st.link, physical_device, &mut n, props.as_mut_ptr());
        let ok = props.get(queue_family_index as usize).is_some_and(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS));
        *p_supported = ok as vk::Bool32;
        vk::Result::SUCCESS
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_get_physical_device_surface_capabilities_khr(
    _physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_caps: *mut vk::SurfaceCapabilitiesKHR,
) -> vk::Result {
    let Some((_, _, w, h)) = surface_window(surface) else { return vk::Result::ERROR_SURFACE_LOST_KHR };
    let extent = vk::Extent2D { width: w, height: h };
    *p_caps = vk::SurfaceCapabilitiesKHR {
        min_image_count: MIN_IMAGES,
        max_image_count: MAX_IMAGES,
        current_extent: extent,
        min_image_extent: extent,
        max_image_extent: extent,
        max_image_array_layers: 1,
        supported_transforms: vk::SurfaceTransformFlagsKHR::IDENTITY,
        current_transform: vk::SurfaceTransformFlagsKHR::IDENTITY,
        supported_composite_alpha: vk::CompositeAlphaFlagsKHR::OPAQUE,
        supported_usage_flags: vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED,
    };
    vk::Result::SUCCESS
}

pub unsafe extern "system" fn vk_get_physical_device_surface_formats_khr(
    _physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_count: *mut u32,
    p_formats: *mut vk::SurfaceFormatKHR,
) -> vk::Result {
    if surface_window(surface).is_none() {
        return vk::Result::ERROR_SURFACE_LOST_KHR;
    }
    let all: Vec<vk::SurfaceFormatKHR> =
        FORMATS.iter().map(|f| vk::SurfaceFormatKHR { format: *f, color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR }).collect();
    enumerate(&all, p_count, p_formats)
}

pub unsafe extern "system" fn vk_get_physical_device_surface_present_modes_khr(
    _physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_count: *mut u32,
    p_modes: *mut vk::PresentModeKHR,
) -> vk::Result {
    if surface_window(surface).is_none() {
        return vk::Result::ERROR_SURFACE_LOST_KHR;
    }
    enumerate(&[vk::PresentModeKHR::FIFO], p_count, p_modes)
}

/// The ICD owns its Win32 surfaces: the loader then hands these back instead of its own objects,
/// whose layout the ICD cannot rely on.
#[cfg(windows)]
pub unsafe extern "system" fn vk_create_win32_surface_khr(
    _instance: vk::Instance,
    p_create_info: *const vk::Win32SurfaceCreateInfoKHR<'_>,
    _p_allocator: *const vk::AllocationCallbacks<'_>,
    p_surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let ci = &*p_create_info;
    let s = Box::new(IcdSurfaceWin32 { base: IcdSurfaceBase { platform: PLATFORM_WIN32 }, hinstance: ci.hinstance as usize, hwnd: ci.hwnd as usize });
    *p_surface = vk::SurfaceKHR::from_raw(Box::into_raw(s) as u64);
    crate::link::log(&format!("vkCreateWin32SurfaceKHR -> {:#x} hwnd {:#x}", (*p_surface).as_raw(), ci.hwnd as usize));
    vk::Result::SUCCESS
}

#[cfg(windows)]
pub unsafe extern "system" fn vk_destroy_surface_khr(_instance: vk::Instance, surface: vk::SurfaceKHR, _p_allocator: *const vk::AllocationCallbacks<'_>) {
    if surface.as_raw() != 0 {
        drop(Box::from_raw(surface.as_raw() as *mut IcdSurfaceWin32));
    }
}

#[cfg(windows)]
pub unsafe extern "system" fn vk_get_physical_device_win32_presentation_support_khr(
    _physical_device: vk::PhysicalDevice,
    _queue_family_index: u32,
) -> vk::Bool32 {
    vk::TRUE
}

/// The standard two-call enumeration over a fixed list.
pub unsafe fn enumerate<T: Copy>(all: &[T], p_count: *mut u32, p_out: *mut T) -> vk::Result {
    if p_out.is_null() {
        *p_count = all.len() as u32;
        return vk::Result::SUCCESS;
    }
    let n = all.len().min(*p_count as usize);
    for (i, v) in all.iter().take(n).enumerate() {
        *p_out.add(i) = *v;
    }
    *p_count = n as u32;
    if n < all.len() { vk::Result::INCOMPLETE } else { vk::Result::SUCCESS }
}

fn request(st: &mut State, op: u16, payload: &[u8]) -> Result<Vec<u8>, vk::Result> {
    st.link.request(op, payload).map_err(|_| vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_create_swapchain_khr(
    device: vk::Device,
    p_create_info: *const vk::SwapchainCreateInfoKHR,
    _p_allocator: *const vk::AllocationCallbacks,
    p_swapchain: *mut vk::SwapchainKHR,
) -> vk::Result {
    let info = &*p_create_info;
    let Some((_, hwnd, _, _)) = surface_window(info.surface) else { return vk::Result::ERROR_SURFACE_LOST_KHR };
    if !FORMATS.contains(&info.image_format) || info.image_array_layers != 1 || info.image_extent.width == 0 || info.image_extent.height == 0 {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    with_state(|st| {
        let count = info.min_image_count.clamp(MIN_IMAGES, MAX_IMAGES);
        let sc_id = st.link.new_handle_id();
        let images: Vec<u64> = (0..count).map(|_| st.link.new_handle_id()).collect();
        let req = ScCreate {
            device: gv::hid_disp(device.as_raw()),
            swapchain: sc_id,
            width: info.image_extent.width,
            height: info.image_extent.height,
            format: info.image_format.as_raw() as u32,
            usage: info.image_usage.as_raw(),
            images: images.clone(),
        };
        let rep = match request(st, OP_SC_CREATE, &req.encode()) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let Ok(res) = Dec::new(&rep).i32() else { return vk::Result::ERROR_DEVICE_LOST };
        let res = vk::Result::from_raw(res);
        if res != vk::Result::SUCCESS {
            return res;
        }
        st.swapchains.insert(
            sc_id,
            Swapchain {
                device_id: req.device,
                width: req.width,
                height: req.height,
                images,
                next: 0,
                frame: vec![0; req.width as usize * req.height as usize * 4],
                hwnd,
                presents: 0,
            },
        );
        *p_swapchain = vk::SwapchainKHR::from_raw(sc_id);
        vk::Result::SUCCESS
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_destroy_swapchain_khr(
    _device: vk::Device,
    swapchain: vk::SwapchainKHR,
    _p_allocator: *const vk::AllocationCallbacks,
) {
    let _ = with_state(|st| {
        let id = swapchain.as_raw();
        if st.swapchains.remove(&id).is_some() {
            let _ = request(st, OP_SC_DESTROY, &id.to_le_bytes());
        }
    });
}

pub unsafe extern "system" fn vk_get_swapchain_images_khr(
    _device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_count: *mut u32,
    p_images: *mut vk::Image,
) -> vk::Result {
    with_state(|st| {
        let Some(sc) = st.swapchains.get(&swapchain.as_raw()) else { return vk::Result::ERROR_DEVICE_LOST };
        let all: Vec<vk::Image> = sc.images.iter().map(|i| vk::Image::from_raw(*i)).collect();
        enumerate(&all, p_count, p_images)
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_acquire_next_image_khr(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    _timeout: u64,
    semaphore: vk::Semaphore,
    fence: vk::Fence,
    p_image_index: *mut u32,
) -> vk::Result {
    with_state(|st| {
        let dev_id = gv::hid_disp(device.as_raw());
        let Some(queue) = st.queues.iter().find(|(k, _)| k.0 == dev_id).map(|(_, q)| *q) else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        let Some(sc) = st.swapchains.get_mut(&swapchain.as_raw()) else { return vk::Result::ERROR_DEVICE_LOST };
        let index = sc.next % sc.images.len() as u32;
        sc.next = sc.next.wrapping_add(1);
        let req = ScAcquire {
            swapchain: swapchain.as_raw(),
            index,
            semaphore: semaphore.as_raw(),
            fence: fence.as_raw(),
            queue: gv::hid_disp(queue),
        };
        match request(st, OP_SC_ACQUIRE, &req.encode()) {
            Ok(rep) => match Dec::new(&rep).i32() {
                Ok(r) if r >= 0 => {
                    *p_image_index = index;
                    vk::Result::from_raw(r)
                }
                Ok(r) => vk::Result::from_raw(r),
                Err(_) => vk::Result::ERROR_DEVICE_LOST,
            },
            Err(e) => e,
        }
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

pub unsafe extern "system" fn vk_queue_present_khr(queue: vk::Queue, p_present_info: *const vk::PresentInfoKHR) -> vk::Result {
    let info = &*p_present_info;
    with_state(|st| {
        if let Err(r) = flush_dirty(st) {
            return r;
        }
        let queue_id = gv::hid_disp(queue.as_raw());
        let Some(family) = st.queues.iter().find(|(_, q)| **q == queue.as_raw()).map(|(k, _)| k.1) else {
            return vk::Result::ERROR_DEVICE_LOST;
        };
        let waits: Vec<u64> = (0..info.wait_semaphore_count as usize).map(|i| (*info.p_wait_semaphores.add(i)).as_raw()).collect();
        let mut overall = vk::Result::SUCCESS;
        for i in 0..info.swapchain_count as usize {
            let sc_id = (*info.p_swapchains.add(i)).as_raw();
            let index = *info.p_image_indices.add(i);
            let req = ScPresent { queue: queue_id, family, swapchain: sc_id, index, waits: if i == 0 { waits.clone() } else { Vec::new() } };
            let r = present_one(st, &req);
            if !info.p_results.is_null() {
                *info.p_results.add(i) = r;
            }
            if r.as_raw() < 0 && overall.as_raw() >= 0 {
                overall = r;
            }
        }
        overall
    })
    .unwrap_or(vk::Result::ERROR_DEVICE_LOST)
}

fn present_one(st: &mut State, req: &ScPresent) -> vk::Result {
    let rep = match request(st, OP_SC_PRESENT, &req.encode()) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let mut d = Dec::new(&rep);
    let Ok(res) = d.i32() else { return vk::Result::ERROR_DEVICE_LOST };
    let Ok(frame) = Frame::decode_from(&mut d) else { return vk::Result::ERROR_DEVICE_LOST };
    let Some(sc) = st.swapchains.get_mut(&req.swapchain) else { return vk::Result::ERROR_DEVICE_LOST };
    if (frame.width, frame.height) != (sc.width, sc.height) {
        return vk::Result::ERROR_DEVICE_LOST;
    }
    let stride = frame.width as usize * 4;
    for (band, bytes) in &frame.bands {
        let off = *band as usize * frame.band_rows as usize * stride;
        sc.frame[off..off + bytes.len()].copy_from_slice(bytes);
    }
    sc.presents += 1;
    show(sc);
    let _ = sc.device_id;
    vk::Result::from_raw(res)
}

#[cfg(windows)]
fn show(sc: &Swapchain) {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    // SAFETY: plain GDI calls on the application's own window; the pixel buffer is w*h*4 bytes.
    unsafe {
        let hwnd = sc.hwnd as *mut c_void;
        let dc = GetDC(hwnd);
        if dc.is_null() {
            return;
        }
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: sc.width as i32,
            biHeight: -(sc.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        SetDIBitsToDevice(dc, 0, 0, sc.width, sc.height, 0, 0, 0, sc.height, sc.frame.as_ptr().cast(), &bi, DIB_RGB_COLORS);
        dump(sc);
        ReleaseDC(hwnd, dc);
    }
}

/// With `WINBRIDGE_GFX_LOG` or `WINBRIDGE_GFX_FRAME_OUT` set, every presented frame is hashed and
/// logged, and written as a PPM when `WINBRIDGE_GFX_FRAME_OUT` names a file. Used by the tests.
fn dump(sc: &Swapchain) {
    if std::env::var_os("WINBRIDGE_GFX_LOG").is_none() && std::env::var_os("WINBRIDGE_GFX_FRAME_OUT").is_none() {
        return;
    }
    if sc.presents > 5 {
        return;
    }
    let sum = sc.frame.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    crate::link::log(&format!("frame {} {}x{} fnv1a {sum:#018x}", sc.presents, sc.width, sc.height));
    if let Ok(path) = std::env::var("WINBRIDGE_GFX_FRAME_OUT") {
        let mut out = format!("P6\n{} {}\n255\n", sc.width, sc.height).into_bytes();
        for px in sc.frame.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        let _ = std::fs::write(path, out);
    }
}

#[cfg(not(windows))]
fn show(sc: &Swapchain) {
    dump(sc);
}
