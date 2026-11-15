// SPDX-License-Identifier: GPL-3.0-or-later

//! `winbridge-gfx-probe [--require-winbridge] [--mib N]`
//!
//! Talks to whatever Vulkan loader the process finds (`vulkan-1.dll` / `libvulkan.so.1`) and
//! reports, line by line:
//! 1. the physical devices the loader lists;
//! 2. the DXVK requirement checks (same table as `experiments/vkcaps`) on the first device whose
//!    name starts with `WinBridge` (or the first device without `--require-winbridge`);
//! 3. a device created with the supported DXVK extensions and the whole feature chain;
//! 4. a mapped-buffer round trip: pattern written through the mapping, flushed, the shadow
//!    wiped, invalidated, checksummed; then a partial flush checked against the full buffer.
//!
//! Exit status 0 only if everything passed. Output lines are `key: value` for scripted checks.

mod render;

use ash::vk;
use std::ffi::{c_char, CStr};
use std::process::ExitCode;
use std::time::Instant;

const CALL_SAMPLES: usize = 2000;

const DXVK_EXTENSIONS: [&CStr; 6] = [
    c"VK_KHR_load_store_op_none",
    c"VK_KHR_maintenance5",
    c"VK_KHR_maintenance6",
    c"VK_EXT_depth_clip_enable",
    c"VK_EXT_robustness2",
    c"VK_EXT_transform_feedback",
];

fn cstr(bytes: &[c_char]) -> String {
    // SAFETY: Vulkan NUL-terminates names in fixed-size arrays.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_string_lossy().into_owned()
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

fn pattern(buf: &mut [u8], seed: u64) {
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
}

fn run(require_winbridge: bool, mib: usize) -> Result<(), String> {
    let e = |what: &'static str| move |err: vk::Result| format!("{what}: {err}");
    // SAFETY: loads the system Vulkan loader.
    let entry = unsafe { ash::Entry::load() }.map_err(|x| format!("load the Vulkan loader: {x}"))?;
    // SAFETY: loader entry points.
    let ver = unsafe { entry.try_enumerate_instance_version() }.map_err(e("vkEnumerateInstanceVersion"))?.unwrap_or(vk::API_VERSION_1_0);
    println!("loader_instance_version: {}.{}.{}", vk::api_version_major(ver), vk::api_version_minor(ver), vk::api_version_patch(ver));
    let app = vk::ApplicationInfo::default().application_name(c"winbridge-gfx-probe").api_version(if std::env::var("WINBRIDGE_PROBE_API").as_deref() == Ok("1.0") { vk::API_VERSION_1_0 } else { vk::API_VERSION_1_3 });
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: plain instance creation without layers or extensions.
    let instance = unsafe { entry.create_instance(&ci, None) }.map_err(e("vkCreateInstance"))?;
    let result = run_instance(&instance, require_winbridge, mib);
    // SAFETY: no object of the instance is alive any more.
    unsafe { instance.destroy_instance(None) };
    result
}

fn run_instance(instance: &ash::Instance, require_winbridge: bool, mib: usize) -> Result<(), String> {
    let e = |what: &'static str| move |err: vk::Result| format!("{what}: {err}");
    // SAFETY: valid instance.
    let pds = unsafe { instance.enumerate_physical_devices() }.map_err(e("vkEnumeratePhysicalDevices"))?;
    println!("physical_device_count: {}", pds.len());
    let mut chosen = None;
    for (i, pd) in pds.iter().enumerate() {
        // SAFETY: handles come from this instance.
        let p = unsafe { instance.get_physical_device_properties(*pd) };
        let name = cstr(&p.device_name);
        println!(
            "device[{i}]: {name} (type {:?}, vendor {:#06x}, api {}.{}.{})",
            p.device_type,
            p.vendor_id,
            vk::api_version_major(p.api_version),
            vk::api_version_minor(p.api_version),
            vk::api_version_patch(p.api_version)
        );
        if chosen.is_none() && (!require_winbridge || name.starts_with("WinBridge")) {
            chosen = Some((*pd, p, name));
        }
    }
    let (pd, _, name) = chosen.ok_or(if require_winbridge { "no device named WinBridge is listed" } else { "no physical device" })?;
    println!("selected_device: {name}");

    // SAFETY: valid instance and physical device.
    let rows = unsafe { winbridge_gfx_vk::dxvk::evaluate(instance, pd) };
    // SAFETY: valid physical device.
    let exts = unsafe { instance.enumerate_device_extension_properties(pd) }.map_err(e("vkEnumerateDeviceExtensionProperties"))?;
    let ext_names: Vec<String> = exts.iter().map(|x| cstr(&x.extension_name)).collect();
    println!("device_extension_count: {}", ext_names.len());
    let passed = rows.iter().filter(|r| r.1).count();
    for (n, _) in rows.iter().filter(|r| !r.1) {
        println!("dxvk_missing: {n}");
    }
    println!("dxvk_checks: {passed}/{}", rows.len());

    // Device with the supported DXVK extensions and the whole feature chain enabled.
    let mut g11 = vk::PhysicalDeviceVulkan11Features::default();
    let mut g12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut g13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut grob = vk::PhysicalDeviceRobustness2FeaturesEXT::default();
    let mut gxfb = vk::PhysicalDeviceTransformFeedbackFeaturesEXT::default();
    let mut gm5 = vk::PhysicalDeviceMaintenance5FeaturesKHR::default();
    let mut gm6 = vk::PhysicalDeviceMaintenance6FeaturesKHR::default();
    let mut gdce = vk::PhysicalDeviceDepthClipEnableFeaturesEXT::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut g11)
        .push_next(&mut g12)
        .push_next(&mut g13)
        .push_next(&mut grob)
        .push_next(&mut gxfb)
        .push_next(&mut gm5)
        .push_next(&mut gm6)
        .push_next(&mut gdce);
    // SAFETY: valid physical device and chain.
    unsafe { instance.get_physical_device_features2(pd, &mut f2) };
    let enabled_exts: Vec<*const c_char> =
        DXVK_EXTENSIONS.iter().filter(|x| ext_names.iter().any(|n| n.as_bytes() == x.to_bytes())).map(|x| x.as_ptr()).collect();
    println!("enabled_extension_count: {}", enabled_exts.len());
    // SAFETY: valid physical device.
    let qfs = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    println!("queue_family_count: {}", qfs.len());
    let qf = qfs.iter().position(|q| q.queue_count > 0).ok_or("no queue family")? as u32;
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(qf).queue_priorities(&prio)];
    let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci).enabled_extension_names(&enabled_exts).push_next(&mut f2);
    // SAFETY: chain and arrays outlive the call.
    let device = unsafe { instance.create_device(pd, &dci, None) }.map_err(e("vkCreateDevice"))?;
    println!("device_created: yes");
    let mut tested = 0;
    let mut result = Ok(());
    for coherent in [true, false] {
        match buffer_round_trip(instance, &device, pd, mib, qf, coherent) {
            Ok(true) => tested += 1,
            Ok(false) => println!("memory_type_{}: none", if coherent { "coherent" } else { "noncoherent" }),
            Err(e) => {
                result = Err(format!("{} memory: {e}", if coherent { "coherent" } else { "non-coherent" }));
                break;
            }
        }
    }
    if result.is_ok() {
        result = async_batching(&device, qf);
    }
    if result.is_ok() && tested == 0 {
        result = Err("no host-visible memory type".into());
    }
    // SAFETY: the round trip released its objects.
    unsafe { device.destroy_device(None) };
    result?;
    // A 1.0 instance cannot query the 1.1+ feature structs (the loader answers zeros), so that
    // mode only checks that the device gets created.
    let api10 = std::env::var("WINBRIDGE_PROBE_API").as_deref() == Ok("1.0");
    if passed != rows.len() && !api10 {
        return Err(format!("only {passed}/{} DXVK required checks pass", rows.len()));
    }
    Ok(())
}

/// Cost of asynchronous commands (command recording) and of the synchronous submit + wait.
fn async_batching(device: &ash::Device, qf: u32) -> Result<(), String> {
    const N: usize = 20_000;
    let e = |what: &'static str| move |err: vk::Result| format!("{what}: {err}");
    // SAFETY: valid device; every object is destroyed below.
    unsafe {
        let pool = device
            .create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(qf), None)
            .map_err(e("vkCreateCommandPool"))?;
        let r = (|| {
            let cb = device
                .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
                .map_err(e("vkAllocateCommandBuffers"))?[0];
            device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).map_err(e("vkBeginCommandBuffer"))?;
            let t = Instant::now();
            for i in 0..N {
                device.cmd_set_line_width(cb, 1.0 + (i % 4) as f32);
            }
            let record = t.elapsed();
            device.end_command_buffer(cb).map_err(e("vkEndCommandBuffer"))?;
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(e("vkCreateFence"))?;
            let queue = device.get_device_queue(qf, 0);
            let t = Instant::now();
            device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], fence).map_err(e("vkQueueSubmit"))?;
            device.wait_for_fences(&[fence], true, 10_000_000_000).map_err(e("vkWaitForFences"))?;
            let submit = t.elapsed();
            device.destroy_fence(fence, None);
            println!("async_record: {N} vkCmdSetLineWidth in {:.1} ms = {:.2} us per command", record.as_secs_f64() * 1e3, record.as_secs_f64() * 1e6 / N as f64);
            println!("submit_and_wait_ms: {:.2}", submit.as_secs_f64() * 1e3);
            Ok(())
        })();
        device.destroy_command_pool(pool, None);
        r
    }
}

fn buffer_round_trip(instance: &ash::Instance, device: &ash::Device, pd: vk::PhysicalDevice, mib: usize, qf: u32, want_coherent: bool) -> Result<bool, String> {
    let e = |what: &'static str| move |err: vk::Result| format!("{what}: {err}");
    let size = (mib << 20) as u64;
    let bci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: valid device and create info.
    let buffer = unsafe { device.create_buffer(&bci, None) }.map_err(e("vkCreateBuffer"))?;
    let out = (|| {
        // SAFETY: valid device and buffer.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        // SAFETY: valid physical device.
        let mp = unsafe { instance.get_physical_device_memory_properties(pd) };
        let Some(ty) = (0..mp.memory_type_count).find(|i| {
            let f = mp.memory_types[*i as usize].property_flags;
            req.memory_type_bits & (1 << i) != 0
                && f.contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
                && f.contains(vk::MemoryPropertyFlags::HOST_COHERENT) == want_coherent
        }) else {
            return Ok(false);
        };
        let flags = mp.memory_types[ty as usize].property_flags;
        println!("memory_type{}: {ty} flags {flags:?}", if want_coherent { "_coherent" } else { "_noncoherent" });
        println!("memory_type_detail: {ty} flags {flags:?}, requirement size {} align {}", req.size, req.alignment);
        let ai = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
        // SAFETY: valid device and allocation info.
        let mem = unsafe { device.allocate_memory(&ai, None) }.map_err(e("vkAllocateMemory"))?;
        let r = (|| {
            // SAFETY: memory and buffer belong to the device; offset 0 is aligned.
            unsafe { device.bind_buffer_memory(buffer, mem, 0) }.map_err(e("vkBindBufferMemory"))?;
            // Latency of one small synchronous call through the whole path.
            let mut us: Vec<f64> = (0..CALL_SAMPLES)
                .map(|_| {
                    let t = Instant::now();
                    // SAFETY: valid device and buffer.
                    let _ = unsafe { device.get_buffer_memory_requirements(buffer) };
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            us.sort_by(f64::total_cmp);
            let pct = |q: f64| us[((us.len() - 1) as f64 * q).round() as usize];
            println!("call_latency_us: p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} ({CALL_SAMPLES} vkGetBufferMemoryRequirements calls)", pct(0.5), pct(0.9), pct(0.99), us[us.len() - 1]);
            // SAFETY: host-visible memory, mapped once.
            let p = unsafe { device.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }.map_err(e("vkMapMemory"))? as *mut u8;
            // SAFETY: the mapping covers the whole allocation, which is at least `size` bytes.
            let map = unsafe { std::slice::from_raw_parts_mut(p, size as usize) };
            let range = |off: u64, len: u64| vk::MappedMemoryRange::default().memory(mem).offset(off).size(len);
            // Coherent memory is never flushed by the application: a queue submission makes the
            // host see the writes. Non-coherent memory is flushed explicitly.
            // SAFETY: queue 0 of family `qf` was requested at device creation.
            let queue = unsafe { device.get_device_queue(qf, 0) };
            let make_visible = |off: u64, len: u64| -> Result<(), String> {
                if want_coherent {
                    // SAFETY: valid queue; an empty submission.
                    unsafe { device.queue_submit(queue, &[], vk::Fence::null()) }.map_err(e("vkQueueSubmit"))
                } else {
                    // SAFETY: range inside the mapped allocation.
                    unsafe { device.flush_mapped_memory_ranges(&[range(off, len)]) }.map_err(e("vkFlushMappedMemoryRanges"))
                }
            };

            // 1. Full write, flush, wipe the shadow, invalidate.
            let mut a = vec![0u8; size as usize];
            pattern(&mut a, 0x9E37_79B9_7F4A_7C15);
            map.copy_from_slice(&a);
            let sum_a = fnv1a(&a);
            // SAFETY: range covers the mapped allocation.
            let t = Instant::now();
            make_visible(0, vk::WHOLE_SIZE)?;
            println!("upload_MiB_per_s: {:.1}", mib as f64 / t.elapsed().as_secs_f64());
            map.fill(0);
            println!("buffer_checksum_written: {sum_a:#018x}");
            // SAFETY: as above.
            let t = Instant::now();
            unsafe { device.invalidate_mapped_memory_ranges(&[range(0, vk::WHOLE_SIZE)]) }.map_err(e("vkInvalidateMappedMemoryRanges"))?;
            println!("download_MiB_per_s: {:.1}", mib as f64 / t.elapsed().as_secs_f64());
            let sum_back = fnv1a(map);
            println!("buffer_checksum_read_back: {sum_back:#018x}");
            if sum_back != sum_a {
                return Err("full-buffer checksum differs after the host round trip".to_string());
            }

            // 2. Partial flush: only [off, off+len) changes on the host.
            let (off, len) = (4096u64, 8192u64.min(size - 4096));
            let mut b = vec![0u8; len as usize];
            pattern(&mut b, 0xD1B5_4A32_D192_ED03);
            map[off as usize..(off + len) as usize].copy_from_slice(&b);
            // SAFETY: sub-range inside the allocation, atom-aligned (4096/8192).
            make_visible(off, len)?;
            map.fill(0xEE);
            // SAFETY: as above.
            unsafe { device.invalidate_mapped_memory_ranges(&[range(0, vk::WHOLE_SIZE)]) }.map_err(e("vkInvalidateMappedMemoryRanges"))?;
            let mut expect = a.clone();
            expect[off as usize..(off + len) as usize].copy_from_slice(&b);
            let (sum_expect, sum_got) = (fnv1a(&expect), fnv1a(map));
            println!("partial_flush_checksum_expected: {sum_expect:#018x}");
            println!("partial_flush_checksum_read_back: {sum_got:#018x}");
            if sum_expect != sum_got {
                return Err("partial flush did not leave exactly the flushed range changed on the host".to_string());
            }
            // SAFETY: mapped above.
            unsafe { device.unmap_memory(mem) };
            Ok(true)
        })();
        // SAFETY: nothing uses the buffer or memory any more.
        unsafe {
            device.destroy_buffer(buffer, None);
            device.free_memory(mem, None);
        }
        r
    })();
    out
}

fn flag_value(args: &[String], name: &str) -> Option<u64> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--reference") {
        let (w, h) = (args.get(i + 1).and_then(|v| v.parse().ok()), args.get(i + 2).and_then(|v| v.parse().ok()));
        let name = args.iter().position(|a| a == "--device-name").and_then(|j| args.get(j + 1)).cloned().unwrap_or_default();
        let (Some(w), Some(h)) = (w, h) else {
            println!("RESULT: FAIL: --reference needs WIDTH HEIGHT");
            return ExitCode::FAILURE;
        };
        return match render::reference(w, h, &name, args.iter().position(|a| a == "--reference-out").and_then(|j| args.get(j + 1)).map(String::as_str)) {
            Ok(sum) => {
                println!("reference_fnv1a: {sum:#018x}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("RESULT: FAIL: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.iter().any(|a| a == "--render") {
        return match render::present(flag_value(&args, "--frames").unwrap_or(3) as u32, flag_value(&args, "--hold-ms").unwrap_or(0)) {
            Ok(()) => {
                println!("RESULT: PASS");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("RESULT: FAIL: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let require = args.iter().any(|a| a == "--require-winbridge");
    let mib = args.iter().position(|a| a == "--mib").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(8);
    match run(require, mib) {
        Ok(()) => {
            println!("RESULT: PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("RESULT: FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}
