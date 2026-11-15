// SPDX-License-Identifier: GPL-3.0-or-later

//! Round trips of the generated serializers (guest encoder -> host decoder), no GPU needed.

use ash::vk;
use std::ffi::{c_void, CStr};
use winbridge_gfx_proto::codec::{Arena, CodecError, Dec, Enc};
use winbridge_gfx_vk::{dec_DeviceCreateInfo, enc_DeviceCreateInfo, Resolver};

struct Identity;

impl Resolver for Identity {
    fn resolve(&self, id: u64, _kind: &'static str) -> Result<u64, CodecError> {
        Ok(id)
    }
}

fn device_info<'a>(
    queues: &'a [vk::DeviceQueueCreateInfo<'a>],
    exts: &'a [*const std::ffi::c_char],
    f2: &'a mut vk::PhysicalDeviceFeatures2<'a>,
) -> vk::DeviceCreateInfo<'a> {
    vk::DeviceCreateInfo::default().queue_create_infos(queues).enabled_extension_names(exts).push_next(f2)
}

#[test]
fn device_create_info_with_feature_chain_round_trips() {
    let prio = [0.25f32, 1.0];
    let queues = [
        vk::DeviceQueueCreateInfo::default().queue_family_index(2).queue_priorities(&prio),
        vk::DeviceQueueCreateInfo::default().queue_family_index(5).queue_priorities(&prio[..1]),
    ];
    let exts = [c"VK_KHR_maintenance5".as_ptr(), c"VK_EXT_robustness2".as_ptr()];
    let mut v12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true).buffer_device_address(true);
    let mut rob = vk::PhysicalDeviceRobustness2FeaturesEXT::default().null_descriptor(true);
    let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut v12).push_next(&mut rob);
    f2.features.geometry_shader = vk::TRUE;
    f2.features.sampler_anisotropy = vk::TRUE;
    let info = device_info(&queues, &exts, &mut f2);

    let mut e = Enc::new();
    // SAFETY: `info` and everything it points to are alive.
    unsafe { enc_DeviceCreateInfo(&mut e, &info) }.unwrap();
    let wire = e.into_vec();

    let mut arena = Arena::new();
    let out = arena.alloc::<vk::DeviceCreateInfo>(1);
    let mut d = Dec::new(&wire);
    // SAFETY: `out` is zeroed arena storage.
    unsafe { winbridge_gfx_vk::dec_DeviceCreateInfo(&mut d, &mut arena, &Identity, out) }.unwrap();
    d.finish().unwrap();
    let out = unsafe { &*out };

    assert!(out.s_type == vk::StructureType::DEVICE_CREATE_INFO);
    assert_eq!(out.queue_create_info_count, 2);
    let q = unsafe { std::slice::from_raw_parts(out.p_queue_create_infos, 2) };
    assert_eq!((q[0].queue_family_index, q[0].queue_count), (2, 2));
    assert_eq!((q[1].queue_family_index, q[1].queue_count), (5, 1));
    let p0 = unsafe { std::slice::from_raw_parts(q[0].p_queue_priorities, 2) };
    assert_eq!(p0, &prio);
    assert_eq!(out.enabled_extension_count, 2);
    let names: Vec<&CStr> =
        (0..2).map(|i| unsafe { CStr::from_ptr(*out.pp_enabled_extension_names.add(i)) }).collect();
    assert_eq!(names, [c"VK_KHR_maintenance5", c"VK_EXT_robustness2"]);

    // ash's `push_next` inserts right behind the head, so the chain is
    // Features2 -> Robustness2 -> Vulkan12Features. Order and values must survive.
    let n0 = out.p_next as *const vk::PhysicalDeviceFeatures2;
    assert!(unsafe { (*n0).s_type } == vk::StructureType::PHYSICAL_DEVICE_FEATURES_2);
    assert_eq!(unsafe { (*n0).features.geometry_shader }, vk::TRUE);
    assert_eq!(unsafe { (*n0).features.tessellation_shader }, vk::FALSE);
    let n1 = unsafe { (*n0).p_next } as *const vk::PhysicalDeviceRobustness2FeaturesEXT;
    assert!(unsafe { (*n1).s_type } == vk::StructureType::PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT);
    assert_eq!(unsafe { (*n1).null_descriptor }, vk::TRUE);
    let n2 = unsafe { (*n1).p_next } as *const vk::PhysicalDeviceVulkan12Features;
    assert!(unsafe { (*n2).s_type } == vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES);
    assert_eq!(unsafe { ((*n2).timeline_semaphore, (*n2).buffer_device_address) }, (vk::TRUE, vk::TRUE));
    assert!(unsafe { (*n2).p_next }.is_null());
}

#[test]
fn unknown_chain_nodes_are_dropped_not_misparsed() {
    let mut stray = vk::BaseOutStructure::default();
    stray.s_type = vk::StructureType::from_raw(1_000_999_999);
    let mut v13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
    v13.p_next = &mut stray as *mut _ as *mut c_void;
    let mut f2 = vk::PhysicalDeviceFeatures2::default();
    f2.p_next = &mut v13 as *mut _ as *mut c_void;
    let info = vk::DeviceCreateInfo::default().push_next(&mut f2);
    let mut e = Enc::new();
    unsafe { enc_DeviceCreateInfo(&mut e, &info) }.unwrap();
    let mut arena = Arena::new();
    let out = arena.alloc::<vk::DeviceCreateInfo>(1);
    unsafe { dec_DeviceCreateInfo(&mut Dec::new(e.as_slice()), &mut arena, &Identity, out) }.unwrap();
    let n0 = unsafe { (*out).p_next } as *const vk::PhysicalDeviceFeatures2;
    let n1 = unsafe { (*n0).p_next } as *const vk::PhysicalDeviceVulkan13Features;
    assert_eq!(unsafe { (*n1).dynamic_rendering }, vk::TRUE);
    assert!(unsafe { (*n1).p_next }.is_null(), "the unrecognised structure must not reach the host");
}

#[test]
fn truncated_and_inconsistent_input_is_rejected() {
    let prio = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(1).queue_priorities(&prio)];
    let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues);
    let mut e = Enc::new();
    unsafe { enc_DeviceCreateInfo(&mut e, &info) }.unwrap();
    let wire = e.into_vec();
    let decode = |bytes: &[u8]| {
        let mut arena = Arena::new();
        let out = arena.alloc::<vk::DeviceCreateInfo>(1);
        unsafe { dec_DeviceCreateInfo(&mut Dec::new(bytes), &mut arena, &Identity, out) }
    };
    decode(&wire).unwrap();
    for cut in 0..wire.len() {
        assert!(decode(&wire[..cut]).is_err(), "prefix of {cut} bytes must not decode");
    }
    // Flip the serialized queue-array length so it disagrees with queueCreateInfoCount.
    let mut bad = wire.clone();
    // Layout: chain end (4) | flags (4) | queueCreateInfoCount (4) | present (1) | n (4) ...
    assert_eq!(&bad[8..12], &1u32.to_le_bytes());
    bad[13..17].copy_from_slice(&0u32.to_le_bytes());
    assert!(decode(&bad).is_err());
}
