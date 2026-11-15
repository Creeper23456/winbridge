// SPDX-License-Identifier: GPL-3.0-or-later

//! DXVK 3.x requirement checks: `VP_DXVK_requirements.json` minus what DXVK 3.1.1 treats as
//! optional (`VK_EXT_transform_feedback` is `ENABLE_EXT_FEATURE(..., false)` in
//! `dxvk_device_info.cpp`; without it only D3D11 stream output is unavailable).

use ash::vk;
use std::ffi::{c_char, CStr};

/// DXVK 2.x required features, as in `experiments/vkcaps` (`VP_DXVK_requirements.json`).
pub fn dxvk_checks(
    f10: &vk::PhysicalDeviceFeatures,
    f11: &vk::PhysicalDeviceVulkan11Features,
    f12: &vk::PhysicalDeviceVulkan12Features,
    f13: &vk::PhysicalDeviceVulkan13Features,
    rob2: &vk::PhysicalDeviceRobustness2FeaturesEXT,
    m5: &vk::PhysicalDeviceMaintenance5FeaturesKHR,
    m6: &vk::PhysicalDeviceMaintenance6FeaturesKHR,
    dce: &vk::PhysicalDeviceDepthClipEnableFeaturesEXT,
    api: u32,
    has_ext: &dyn Fn(&str) -> bool,
) -> Vec<(&'static str, bool)> {
    let t = |b: vk::Bool32| b == vk::TRUE;
    let mut r: Vec<(&'static str, bool)> = Vec::new();
    r.push(("apiVersion >= 1.3", api >= vk::API_VERSION_1_3));
    for e in [
        "VK_KHR_load_store_op_none",
        "VK_KHR_maintenance5",
        "VK_KHR_maintenance6",
        "VK_EXT_depth_clip_enable",
        "VK_EXT_robustness2",
    ] {
        r.push((e, has_ext(e)));
    }
    r.extend([
        ("robustBufferAccess", t(f10.robust_buffer_access)),
        ("fragmentStoresAndAtomics", t(f10.fragment_stores_and_atomics)),
        ("samplerAnisotropy", t(f10.sampler_anisotropy)),
        ("shaderInt16", t(f10.shader_int16)),
        ("shaderSampledImageArrayDynamicIndexing", t(f10.shader_sampled_image_array_dynamic_indexing)),
        ("geometryShader", t(f10.geometry_shader)),
        ("tessellationShader", t(f10.tessellation_shader)),
        ("multiDrawIndirect", t(f10.multi_draw_indirect)),
        ("drawIndirectFirstInstance", t(f10.draw_indirect_first_instance)),
        ("logicOp", t(f10.logic_op)),
        ("vertexPipelineStoresAndAtomics", t(f10.vertex_pipeline_stores_and_atomics)),
        ("dualSrcBlend", t(f10.dual_src_blend)),
        ("multiViewport", t(f10.multi_viewport)),
        ("depthClamp", t(f10.depth_clamp)),
        ("depthBiasClamp", t(f10.depth_bias_clamp)),
        ("imageCubeArray", t(f10.image_cube_array)),
        ("fillModeNonSolid", t(f10.fill_mode_non_solid)),
        ("textureCompressionBC", t(f10.texture_compression_bc)),
        ("occlusionQueryPrecise", t(f10.occlusion_query_precise)),
        ("shaderClipDistance", t(f10.shader_clip_distance)),
        ("shaderCullDistance", t(f10.shader_cull_distance)),
        ("shaderImageGatherExtended", t(f10.shader_image_gather_extended)),
        ("independentBlend", t(f10.independent_blend)),
        ("fullDrawIndexUint32", t(f10.full_draw_index_uint32)),
        ("sampleRateShading", t(f10.sample_rate_shading)),
        ("1.1 multiview", t(f11.multiview)),
        ("1.1 storageBuffer16BitAccess", t(f11.storage_buffer16_bit_access)),
        ("1.1 shaderDrawParameters", t(f11.shader_draw_parameters)),
        ("1.2 timelineSemaphore", t(f12.timeline_semaphore)),
        ("1.2 bufferDeviceAddress", t(f12.buffer_device_address)),
        ("1.2 vulkanMemoryModel", t(f12.vulkan_memory_model)),
        ("1.2 vulkanMemoryModelDeviceScope", t(f12.vulkan_memory_model_device_scope)),
        ("1.2 descriptorIndexing", t(f12.descriptor_indexing)),
        ("1.2 runtimeDescriptorArray", t(f12.runtime_descriptor_array)),
        ("1.2 descriptorBindingPartiallyBound", t(f12.descriptor_binding_partially_bound)),
        ("1.2 descriptorBindingUpdateUnusedWhilePending", t(f12.descriptor_binding_update_unused_while_pending)),
        ("1.2 descriptorBindingSampledImageUpdateAfterBind", t(f12.descriptor_binding_sampled_image_update_after_bind)),
        ("1.2 imagelessFramebuffer", t(f12.imageless_framebuffer)),
        ("1.2 separateDepthStencilLayouts", t(f12.separate_depth_stencil_layouts)),
        ("1.2 hostQueryReset", t(f12.host_query_reset)),
        ("1.2 uniformBufferStandardLayout", t(f12.uniform_buffer_standard_layout)),
        ("1.2 storageBuffer8BitAccess", t(f12.storage_buffer8_bit_access)),
        ("1.2 shaderInt8", t(f12.shader_int8)),
        ("1.2 samplerMirrorClampToEdge", t(f12.sampler_mirror_clamp_to_edge)),
        ("1.2 subgroupBroadcastDynamicId", t(f12.subgroup_broadcast_dynamic_id)),
        ("1.2 shaderSubgroupExtendedTypes", t(f12.shader_subgroup_extended_types)),
        ("1.3 synchronization2", t(f13.synchronization2)),
        ("1.3 dynamicRendering", t(f13.dynamic_rendering)),
        ("1.3 maintenance4", t(f13.maintenance4)),
        ("1.3 robustImageAccess", t(f13.robust_image_access)),
        ("1.3 inlineUniformBlock", t(f13.inline_uniform_block)),
        ("1.3 shaderDemoteToHelperInvocation", t(f13.shader_demote_to_helper_invocation)),
        ("1.3 pipelineCreationCacheControl", t(f13.pipeline_creation_cache_control)),
        ("1.3 subgroupSizeControl", t(f13.subgroup_size_control)),
        ("1.3 computeFullSubgroups", t(f13.compute_full_subgroups)),
        ("1.3 shaderIntegerDotProduct", t(f13.shader_integer_dot_product)),
        ("1.3 shaderZeroInitializeWorkgroupMemory", t(f13.shader_zero_initialize_workgroup_memory)),
        ("1.3 shaderTerminateInvocation", t(f13.shader_terminate_invocation)),
        ("robustness2.nullDescriptor", t(rob2.null_descriptor)),
        ("robustness2.robustBufferAccess2", t(rob2.robust_buffer_access2)),
        ("maintenance5", t(m5.maintenance5)),
        ("maintenance6", t(m6.maintenance6)),
        ("depthClipEnable", t(dce.depth_clip_enable)),
    ]);
    r
}


/// Queries `pd` and returns every requirement with its pass/fail state.
///
/// # Safety
/// `instance` and `pd` must be valid.
pub unsafe fn evaluate(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Vec<(&'static str, bool)> {
    macro_rules! query {
        ($t:ty) => {{
            let mut s = <$t>::default();
            let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut s);
            instance.get_physical_device_features2(pd, &mut f2);
            s
        }};
    }
    let props = instance.get_physical_device_properties(pd);
    let f10 = instance.get_physical_device_features(pd);
    let f11 = query!(vk::PhysicalDeviceVulkan11Features);
    let f12 = query!(vk::PhysicalDeviceVulkan12Features);
    let f13 = query!(vk::PhysicalDeviceVulkan13Features);
    let rob2 = query!(vk::PhysicalDeviceRobustness2FeaturesEXT);
    let m5 = query!(vk::PhysicalDeviceMaintenance5FeaturesKHR);
    let m6 = query!(vk::PhysicalDeviceMaintenance6FeaturesKHR);
    let dce = query!(vk::PhysicalDeviceDepthClipEnableFeaturesEXT);
    let exts = instance.enumerate_device_extension_properties(pd).unwrap_or_default();
    let names: Vec<String> = exts.iter().map(|x| CStr::from_ptr(x.extension_name.as_ptr() as *const c_char).to_string_lossy().into_owned()).collect();
    dxvk_checks(&f10, &f11, &f12, &f13, &rob2, &m5, &m6, &dce, props.api_version, &|n| names.iter().any(|x| x == n))
}
