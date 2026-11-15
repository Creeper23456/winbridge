// SPDX-License-Identifier: GPL-3.0-or-later
//! Host-side Vulkan capability probe for the WinBridge DXVK forwarding spike.
//!
//! Creates a Vulkan 1.3 instance (no surface, no swapchain: the planned host
//! renderer is offscreen) and, for every physical device the loader exposes,
//! prints the API version, driver version and whether each feature/extension
//! that DXVK 2.x (master 71bf4bad) marks as required is supported.
//! The required set is taken from DXVK's `VP_DXVK_requirements.json`
//! (`dxvk_common_required`, `vulkan1{0,1,2,3}requirements`, `d3d{9,11}_baseline`)
//! and from `src/dxvk/dxvk_device_info.cpp`.

use ash::{vk, Entry, Instance};
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{c_char, CStr};

const NVIDIA_VENDOR_ID: u32 = 0x10de;

fn cstr(bytes: &[c_char]) -> String {
    // SAFETY: Vulkan guarantees NUL-terminated names in fixed-size arrays.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Query one extension feature struct via `vkGetPhysicalDeviceFeatures2`.
fn query<T: vk::ExtendsPhysicalDeviceFeatures2 + Default>(
    instance: &Instance,
    pd: vk::PhysicalDevice,
) -> T {
    let mut feat = T::default();
    {
        let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut feat);
        // SAFETY: pd was enumerated from this instance; structs are correctly typed.
        unsafe { instance.get_physical_device_features2(pd, &mut f2) };
    }
    feat
}

struct Row {
    group: &'static str,
    name: &'static str,
    ok: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    // SAFETY: loads libvulkan.so.1 dynamically from the system loader.
    let entry = unsafe { Entry::load()? };
    let app = vk::ApplicationInfo::default()
        .application_name(c"winbridge-vkcaps")
        .api_version(vk::API_VERSION_1_3);
    let create = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: standard instance creation with no layers or surface extensions.
    let instance = unsafe { entry.create_instance(&create, None)? };

    // SAFETY: instance is valid.
    let pds = unsafe { instance.enumerate_physical_devices()? };
    println!("loader: {} physical device(s) exposed\n", pds.len());

    for (idx, pd) in pds.into_iter().enumerate() {
        // SAFETY: pd is a valid handle from this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = cstr(&props.device_name);
        let api = props.api_version;
        let (api_maj, api_min, api_pat) = (api >> 22, (api >> 12) & 0x3ff, api & 0xfff);
        let drv = props.driver_version;
        let drv_str = if props.vendor_id == NVIDIA_VENDOR_ID {
            format!(
                "{}.{}.{:02} (build {})",
                drv >> 22,
                (drv >> 14) & 0xff,
                (drv >> 6) & 0xff,
                drv & 0x3f
            )
        } else {
            format!("raw 0x{drv:08x}")
        };
        println!("== device {idx}: {name}");
        println!(
            "   type={:?} vendor=0x{:04x} device=0x{:04x}",
            props.device_type, props.vendor_id, props.device_id
        );
        println!("   apiVersion={api_maj}.{api_min}.{api_pat} driverVersion={drv_str}");

        // SAFETY: pd is valid.
        let exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
        let ext: HashSet<String> = exts.iter().map(|e| cstr(&e.extension_name)).collect();

        // Feature structs. Each query is an independent PhysicalDeviceFeatures2 chain.
        // SAFETY: pd is valid.
        let f10 = unsafe { instance.get_physical_device_features(pd) };
        let f11: vk::PhysicalDeviceVulkan11Features = query(&instance, pd);
        let f12: vk::PhysicalDeviceVulkan12Features = query(&instance, pd);
        let f13: vk::PhysicalDeviceVulkan13Features = query(&instance, pd);
        let rob2: vk::PhysicalDeviceRobustness2FeaturesEXT = query(&instance, pd);
        let xfb: vk::PhysicalDeviceTransformFeedbackFeaturesEXT = query(&instance, pd);
        let m5: vk::PhysicalDeviceMaintenance5FeaturesKHR = query(&instance, pd);
        let m6: vk::PhysicalDeviceMaintenance6FeaturesKHR = query(&instance, pd);
        let dce: vk::PhysicalDeviceDepthClipEnableFeaturesEXT = query(&instance, pd);

        let has = |n: &str| ext.contains(n);
        let mut rows: Vec<Row> = Vec::new();
        let mut r = |group, name, ok| rows.push(Row { group, name, ok });

        // Core version
        r("core", "apiVersion >= 1.3", api >= vk::API_VERSION_1_3);

        // DXVK dxvk_common_required: extensions
        r("ext", "VK_KHR_load_store_op_none", has("VK_KHR_load_store_op_none"));
        r("ext", "VK_KHR_maintenance5", has("VK_KHR_maintenance5"));
        r("ext", "VK_KHR_maintenance6", has("VK_KHR_maintenance6"));
        r("ext", "VK_EXT_depth_clip_enable", has("VK_EXT_depth_clip_enable"));
        r("ext", "VK_EXT_robustness2", has("VK_EXT_robustness2"));
        // d3d11_baseline / d3d9_baseline
        r("ext", "VK_EXT_transform_feedback", has("VK_EXT_transform_feedback"));

        // vulkan10 + dxvk_common_required features
        r("f1.0", "robustBufferAccess", f10.robust_buffer_access == vk::TRUE);
        r("f1.0", "fragmentStoresAndAtomics", f10.fragment_stores_and_atomics == vk::TRUE);
        r("f1.0", "samplerAnisotropy", f10.sampler_anisotropy == vk::TRUE);
        r("f1.0", "shaderInt16", f10.shader_int16 == vk::TRUE);
        r("f1.0", "shaderSampledImageArrayDynamicIndexing",
            f10.shader_sampled_image_array_dynamic_indexing == vk::TRUE);
        r("f1.0", "geometryShader (d3d9/11)", f10.geometry_shader == vk::TRUE);
        r("f1.0", "tessellationShader (d3d11 lvl 11_0)", f10.tessellation_shader == vk::TRUE);
        r("f1.0", "multiDrawIndirect (d3d11 lvl 11_0)", f10.multi_draw_indirect == vk::TRUE);
        r("f1.0", "drawIndirectFirstInstance (d3d11 lvl 11_0)",
            f10.draw_indirect_first_instance == vk::TRUE);
        r("f1.0", "logicOp (d3d11 lvl 11_1)", f10.logic_op == vk::TRUE);
        r("f1.0", "vertexPipelineStoresAndAtomics (d3d11 lvl 11_1)",
            f10.vertex_pipeline_stores_and_atomics == vk::TRUE);
        r("f1.0", "dualSrcBlend (d3d11 baseline)", f10.dual_src_blend == vk::TRUE);
        r("f1.0", "multiViewport (d3d11 baseline)", f10.multi_viewport == vk::TRUE);
        r("f1.0", "depthClamp (d3d9/11)", f10.depth_clamp == vk::TRUE);
        r("f1.0", "depthBiasClamp (d3d9/11)", f10.depth_bias_clamp == vk::TRUE);
        r("f1.0", "imageCubeArray (d3d9/11)", f10.image_cube_array == vk::TRUE);
        r("f1.0", "fillModeNonSolid (d3d9/11)", f10.fill_mode_non_solid == vk::TRUE);
        r("f1.0", "textureCompressionBC (d3d9/11)", f10.texture_compression_bc == vk::TRUE);
        r("f1.0", "occlusionQueryPrecise (d3d9/11)", f10.occlusion_query_precise == vk::TRUE);
        r("f1.0", "shaderClipDistance (d3d9/11)", f10.shader_clip_distance == vk::TRUE);
        r("f1.0", "shaderCullDistance (d3d9/11)", f10.shader_cull_distance == vk::TRUE);
        r("f1.0", "shaderImageGatherExtended (d3d9/11)",
            f10.shader_image_gather_extended == vk::TRUE);
        r("f1.0", "independentBlend (d3d9/11)", f10.independent_blend == vk::TRUE);
        r("f1.0", "fullDrawIndexUint32 (d3d9/11)", f10.full_draw_index_uint32 == vk::TRUE);
        r("f1.0", "sampleRateShading (d3d9/11)", f10.sample_rate_shading == vk::TRUE);

        // vulkan11
        r("f1.1", "multiview", f11.multiview == vk::TRUE);
        r("f1.1", "storageBuffer16BitAccess", f11.storage_buffer16_bit_access == vk::TRUE);
        r("f1.1", "shaderDrawParameters", f11.shader_draw_parameters == vk::TRUE);

        // vulkan12
        r("f1.2", "timelineSemaphore", f12.timeline_semaphore == vk::TRUE);
        r("f1.2", "bufferDeviceAddress", f12.buffer_device_address == vk::TRUE);
        r("f1.2", "vulkanMemoryModel", f12.vulkan_memory_model == vk::TRUE);
        r("f1.2", "vulkanMemoryModelDeviceScope", f12.vulkan_memory_model_device_scope == vk::TRUE);
        r("f1.2", "descriptorIndexing", f12.descriptor_indexing == vk::TRUE);
        r("f1.2", "runtimeDescriptorArray", f12.runtime_descriptor_array == vk::TRUE);
        r("f1.2", "descriptorBindingPartiallyBound", f12.descriptor_binding_partially_bound == vk::TRUE);
        r("f1.2", "descriptorBindingUpdateUnusedWhilePending",
            f12.descriptor_binding_update_unused_while_pending == vk::TRUE);
        r("f1.2", "descriptorBindingSampledImageUpdateAfterBind",
            f12.descriptor_binding_sampled_image_update_after_bind == vk::TRUE);
        r("f1.2", "imagelessFramebuffer", f12.imageless_framebuffer == vk::TRUE);
        r("f1.2", "separateDepthStencilLayouts", f12.separate_depth_stencil_layouts == vk::TRUE);
        r("f1.2", "hostQueryReset", f12.host_query_reset == vk::TRUE);
        r("f1.2", "uniformBufferStandardLayout", f12.uniform_buffer_standard_layout == vk::TRUE);
        r("f1.2", "storageBuffer8BitAccess", f12.storage_buffer8_bit_access == vk::TRUE);
        r("f1.2", "shaderInt8", f12.shader_int8 == vk::TRUE);
        r("f1.2", "samplerMirrorClampToEdge", f12.sampler_mirror_clamp_to_edge == vk::TRUE);
        r("f1.2", "subgroupBroadcastDynamicId", f12.subgroup_broadcast_dynamic_id == vk::TRUE);
        r("f1.2", "shaderSubgroupExtendedTypes", f12.shader_subgroup_extended_types == vk::TRUE);

        // vulkan13 (core: dynamic rendering and sync2 are NOT extensions in 1.3)
        r("f1.3", "synchronization2", f13.synchronization2 == vk::TRUE);
        r("f1.3", "dynamicRendering", f13.dynamic_rendering == vk::TRUE);
        r("f1.3", "maintenance4", f13.maintenance4 == vk::TRUE);
        r("f1.3", "robustImageAccess", f13.robust_image_access == vk::TRUE);
        r("f1.3", "inlineUniformBlock", f13.inline_uniform_block == vk::TRUE);
        r("f1.3", "shaderDemoteToHelperInvocation", f13.shader_demote_to_helper_invocation == vk::TRUE);
        r("f1.3", "pipelineCreationCacheControl", f13.pipeline_creation_cache_control == vk::TRUE);
        r("f1.3", "subgroupSizeControl", f13.subgroup_size_control == vk::TRUE);
        r("f1.3", "computeFullSubgroups", f13.compute_full_subgroups == vk::TRUE);
        r("f1.3", "shaderIntegerDotProduct", f13.shader_integer_dot_product == vk::TRUE);
        r("f1.3", "shaderZeroInitializeWorkgroupMemory",
            f13.shader_zero_initialize_workgroup_memory == vk::TRUE);
        r("f1.3", "shaderTerminateInvocation", f13.shader_terminate_invocation == vk::TRUE);

        // extension feature structs
        r("ext-f", "robustness2.nullDescriptor", rob2.null_descriptor == vk::TRUE);
        r("ext-f", "robustness2.robustBufferAccess2", rob2.robust_buffer_access2 == vk::TRUE);
        r("ext-f", "transformFeedback", xfb.transform_feedback == vk::TRUE);
        r("ext-f", "maintenance5", m5.maintenance5 == vk::TRUE);
        r("ext-f", "maintenance6", m6.maintenance6 == vk::TRUE);
        r("ext-f", "depthClipEnable", dce.depth_clip_enable == vk::TRUE);

        let mut missing = 0usize;
        let mut last_group = "";
        for row in &rows {
            if row.group != last_group {
                println!("   -- {}", row.group);
                last_group = row.group;
            }
            if !row.ok {
                missing += 1;
            }
            println!("      [{}] {}", if row.ok { "ok " } else { "MISS" }, row.name);
        }
        println!(
            "   => DXVK required checks: {}/{} supported",
            rows.len() - missing,
            rows.len()
        );

        // Optional DXVK extensions (presence only), for planning.
        const OPTIONAL: &[&str] = &[
            "VK_KHR_maintenance7",
            "VK_KHR_maintenance8",
            "VK_KHR_maintenance9",
            "VK_KHR_maintenance10",
            "VK_KHR_maintenance11",
            "VK_KHR_present_wait",
            "VK_KHR_present_id2",
            "VK_KHR_present_wait2",
            "VK_KHR_unified_image_layouts",
            "VK_KHR_calibrated_timestamps",
            "VK_KHR_incremental_present",
            "VK_KHR_swapchain_mutable_format",
            "VK_EXT_memory_priority",
            "VK_EXT_vertex_attribute_divisor",
            "VK_EXT_depth_bias_control",
            "VK_EXT_custom_border_color",
            "VK_EXT_extended_dynamic_state3",
            "VK_EXT_line_rasterization",
            "VK_EXT_multi_draw",
            "VK_EXT_pageable_device_local_memory",
            "VK_EXT_descriptor_buffer",
            "VK_EXT_conservative_rasterization",
            "VK_EXT_fragment_shader_interlock",
            "VK_EXT_sample_locations",
        ];
        let opt_present: Vec<&str> = OPTIONAL.iter().copied().filter(|e| has(e)).collect();
        println!(
            "   optional DXVK exts present: {}/{}",
            opt_present.len(),
            OPTIONAL.len()
        );
        let opt_missing: Vec<&str> = OPTIONAL.iter().copied().filter(|e| !has(e)).collect();
        println!("   optional DXVK exts missing: {}", opt_missing.join(", "));
        println!("   total device extensions: {}\n", ext.len());
    }

    // SAFETY: instance is not used afterwards.
    unsafe { instance.destroy_instance(None) };
    Ok(())
}
