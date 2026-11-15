// SPDX-License-Identifier: GPL-3.0-or-later

//! The command subset the generator covers, and the policy lists around it.
//!
//! The forwarded command list is derived from the registry (every core 1.0-1.3 command the
//! generator can handle, sorted by name); the wire command id is the index in that list.

/// Commands whose host side is a trait method (`Hooks`) instead of a straight driver call:
/// the session filters devices and keeps memory bookkeeping.
pub const HOST_HOOKS: &[&str] = &["vkCreateInstance", "vkEnumeratePhysicalDevices", "vkAllocateMemory", "vkFreeMemory", "vkCreateDevice"];

/// Commands whose guest entry point is written by hand in `winbridge-gfx-icd::custom`; the
/// generator still emits their guest call function and host executor, but not the shim.
pub const ICD_CUSTOM: &[&str] = &[
    "vkEnumeratePhysicalDevices",
    "vkGetPhysicalDeviceProperties",
    "vkGetPhysicalDeviceProperties2",
    "vkGetPhysicalDeviceMemoryProperties",
    "vkGetPhysicalDeviceMemoryProperties2",
    "vkGetDeviceQueue",
    "vkAllocateMemory",
    "vkFreeMemory",
    "vkCreateInstance",
    "vkCreateDevice",
    "vkEnumerateDeviceExtensionProperties",
    "vkQueueSubmit",
    "vkQueueSubmit2",
    "vkWaitForFences",
    "vkGetFenceStatus",
    "vkQueueWaitIdle",
    "vkDeviceWaitIdle",
    "vkWaitSemaphores",
    "vkDestroyInstance",
    "vkDestroyDevice",
];

/// `pNext` structures that may travel in chains. A node whose `sType` is not here (or is not
/// listed as extending the chain's root) is dropped by the guest serializer, as the Vulkan
/// specification allows for structures an implementation does not recognise.
pub const CHAIN_ALLOW: &[&str] = &[
    "VkPhysicalDeviceFeatures2",
    "VkPhysicalDeviceVulkan11Features",
    "VkPhysicalDeviceVulkan12Features",
    "VkPhysicalDeviceVulkan13Features",
    "VkPhysicalDeviceRobustness2FeaturesEXT",
    "VkPhysicalDeviceTransformFeedbackFeaturesEXT",
    "VkPhysicalDeviceMaintenance5FeaturesKHR",
    "VkPhysicalDeviceMaintenance6FeaturesKHR",
    "VkPhysicalDeviceDepthClipEnableFeaturesEXT",
    "VkPhysicalDeviceVulkan11Properties",
    "VkPhysicalDeviceVulkan12Properties",
    "VkPhysicalDeviceVulkan13Properties",
    "VkPhysicalDeviceRobustness2PropertiesEXT",
    "VkPhysicalDeviceTransformFeedbackPropertiesEXT",
    "VkPhysicalDeviceMaintenance5PropertiesKHR",
    "VkPhysicalDeviceMaintenance6PropertiesKHR",
    "VkFormatProperties3",
    "VkMemoryAllocateFlagsInfo",
    "VkMemoryDedicatedAllocateInfo",
    "VkMemoryPriorityAllocateInfoEXT",
];

/// Commands that always wait for the host: their result or side effect is what the caller
/// needs (waits, status queries, allocations whose failure the application handles).
/// Every other command without output data is sent asynchronously and reports success; a
/// host-side failure is logged by the host and surfaces as a closed channel only if fatal.
pub const SYNC: &[&str] = &[
    "vkCreateInstance",
    "vkCreateDevice",
    "vkDeviceWaitIdle",
    "vkQueueWaitIdle",
    "vkWaitForFences",
    "vkWaitSemaphores",
    "vkGetFenceStatus",
    "vkGetEventStatus",
    "vkAllocateMemory",
    "vkAllocateDescriptorSets",
    "vkResetDescriptorPool",
];

/// Asynchronous commands after which the pending batch is sent at once, because the host
/// should start work on them now.
pub const FLUSH_AFTER: &[&str] = &["vkQueueSubmit", "vkQueueSubmit2", "vkQueueBindSparse"];
