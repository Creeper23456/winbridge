/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Which Vulkan features does vkCreateDevice accept? Loads the Vulkan loader, takes physical
 * device N (default 0), queries the core, 1.1, 1.2 and 1.3 feature structs and
 *   1. creates a device with every feature the driver reports as supported (what an application
 *      that enables all it can query does), then
 *   2. if that fails, tries each reported feature on its own and lists the ones that fail.
 * Usage: featprobe.exe [physical-device-index]
 * The features are printed as <group>[<index>] in the order of the Vulkan header's structs
 * (core: VkPhysicalDeviceFeatures; v11, v12, v13: VkPhysicalDeviceVulkan1xFeatures).
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#define VK_USE_PLATFORM_WIN32_KHR
#include <vulkan/vulkan.h>

static VkInstance inst;
static PFN_vkGetInstanceProcAddr gipa;
static PFN_vkGetDeviceProcAddr gdpa;
static PFN_vkCreateDevice create_device;
static VkPhysicalDevice phys;

struct group {
   const char *name;
   size_t first, last; /* offsets of the first and last VkBool32 of the struct */
   VkBool32 *values;   /* as queried */
   size_t count;
};

static VkPhysicalDeviceFeatures2 f2;
static VkPhysicalDeviceVulkan11Features f11;
static VkPhysicalDeviceVulkan12Features f12;
static VkPhysicalDeviceVulkan13Features f13;

static struct group groups[4];

static const char *
result_name(VkResult r)
{
   switch (r) {
   case VK_SUCCESS: return "VK_SUCCESS";
   case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
   case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
   case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
   case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
   case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
   default: return "other";
   }
}

static VkBool32 *
bools(void *s, const struct group *g)
{
   return (VkBool32 *)((char *)s + g->first);
}

static void *
queried_struct(int g)
{
   return g == 0 ? (void *)&f2.features : g == 1 ? (void *)&f11 : g == 2 ? (void *)&f12 : (void *)&f13;
}

/* Creates a device with the reported features (only_group < 0) or with just one of them. */
static VkResult
try_create(int only_group, size_t only_index)
{
   VkPhysicalDeviceFeatures2 e2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2 };
   VkPhysicalDeviceVulkan11Features e11 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES };
   VkPhysicalDeviceVulkan12Features e12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES };
   VkPhysicalDeviceVulkan13Features e13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES };
   void *enabled[4] = { &e2.features, &e11, &e12, &e13 };
   for (int g = 0; g < 4; g++) {
      VkBool32 *dst = bools(enabled[g], &groups[g]);
      for (size_t i = 0; i < groups[g].count; i++) {
         const bool keep = only_group < 0 ? groups[g].values[i] != 0
                                          : (g == only_group && i == only_index);
         dst[i] = keep ? VK_TRUE : VK_FALSE;
      }
   }
   e2.pNext = &e11;
   e11.pNext = &e12;
   e12.pNext = &e13;
   const float prio = 1.0f;
   VkDeviceQueueCreateInfo q = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio };
   VkDeviceCreateInfo info = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &e2,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &q };
   VkDevice dev = VK_NULL_HANDLE;
   const VkResult r = create_device(phys, &info, NULL, &dev);
   if (r == VK_SUCCESS) {
      PFN_vkDestroyDevice destroy = (PFN_vkDestroyDevice)gdpa(dev, "vkDestroyDevice");
      destroy(dev, NULL);
   }
   return r;
}

int
main(int argc, char **argv)
{
   const uint32_t index = argc > 1 ? (uint32_t)atoi(argv[1]) : 0;
   HMODULE lib = LoadLibraryA("vulkan-1.dll");
   if (!lib) {
      printf("RESULT: FAIL: no vulkan-1.dll (%lu)\n", GetLastError());
      return 1;
   }
   gipa = (PFN_vkGetInstanceProcAddr)GetProcAddress(lib, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance create_instance = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "featprobe",
                             .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkResult r = create_instance(&ici, NULL, &inst);
   if (r != VK_SUCCESS) {
      printf("RESULT: FAIL: vkCreateInstance %d (%s)\n", r, result_name(r));
      return 1;
   }
   PFN_vkEnumeratePhysicalDevices enumerate =
      (PFN_vkEnumeratePhysicalDevices)gipa(inst, "vkEnumeratePhysicalDevices");
   PFN_vkGetPhysicalDeviceProperties2 props2 =
      (PFN_vkGetPhysicalDeviceProperties2)gipa(inst, "vkGetPhysicalDeviceProperties2");
   PFN_vkGetPhysicalDeviceFeatures2 feats2 =
      (PFN_vkGetPhysicalDeviceFeatures2)gipa(inst, "vkGetPhysicalDeviceFeatures2");
   create_device = (PFN_vkCreateDevice)gipa(inst, "vkCreateDevice");
   gdpa = (PFN_vkGetDeviceProcAddr)gipa(inst, "vkGetDeviceProcAddr");
   VkPhysicalDevice devs[8];
   uint32_t n = 8;
   r = enumerate(inst, &n, devs);
   if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || index >= n) {
      printf("RESULT: FAIL: vkEnumeratePhysicalDevices %d, %u devices, wanted %u\n", r, n, index);
      return 1;
   }
   phys = devs[index];
   VkPhysicalDeviceProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 };
   props2(phys, &p2);
   printf("device %u: %s, API %u.%u.%u\n", index, p2.properties.deviceName,
          VK_API_VERSION_MAJOR(p2.properties.apiVersion), VK_API_VERSION_MINOR(p2.properties.apiVersion),
          VK_API_VERSION_PATCH(p2.properties.apiVersion));

   f2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
   f11.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES;
   f12.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
   f13.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES;
   f2.pNext = &f11;
   f11.pNext = &f12;
   f12.pNext = &f13;
   feats2(phys, &f2);
   printf("v12 samplerMirrorClampToEdge=%u drawIndirectCount=%u descriptorIndexing=%u samplerFilterMinmax=%u "
          "shaderOutputViewportIndex=%u shaderOutputLayer=%u subgroupBroadcastDynamicId=%u\n",
          f12.samplerMirrorClampToEdge, f12.drawIndirectCount, f12.descriptorIndexing,
          f12.samplerFilterMinmax, f12.shaderOutputViewportIndex, f12.shaderOutputLayer,
          f12.subgroupBroadcastDynamicId);

   groups[0] = (struct group){ "core", offsetof(VkPhysicalDeviceFeatures, robustBufferAccess),
                               offsetof(VkPhysicalDeviceFeatures, inheritedQueries) };
   groups[1] = (struct group){ "v11", offsetof(VkPhysicalDeviceVulkan11Features, storageBuffer16BitAccess),
                               offsetof(VkPhysicalDeviceVulkan11Features, shaderDrawParameters) };
   groups[2] = (struct group){ "v12", offsetof(VkPhysicalDeviceVulkan12Features, samplerMirrorClampToEdge),
                               offsetof(VkPhysicalDeviceVulkan12Features, subgroupBroadcastDynamicId) };
   groups[3] = (struct group){ "v13", offsetof(VkPhysicalDeviceVulkan13Features, robustImageAccess),
                               offsetof(VkPhysicalDeviceVulkan13Features, maintenance4) };
   size_t reported = 0;
   for (int g = 0; g < 4; g++) {
      groups[g].count = (groups[g].last - groups[g].first) / sizeof(VkBool32) + 1;
      groups[g].values = bools(queried_struct(g), &groups[g]);
      for (size_t i = 0; i < groups[g].count; i++)
         reported += groups[g].values[i] != 0;
   }
   printf("features reported as supported: %zu\n", reported);

   r = try_create(-1, 0);
   printf("vkCreateDevice with all reported features: %d (%s)\n", r, result_name(r));
   if (r != VK_SUCCESS) {
      int failures = 0;
      for (int g = 0; g < 4; g++) {
         for (size_t i = 0; i < groups[g].count; i++) {
            if (!groups[g].values[i])
               continue;
            const VkResult one = try_create(g, i);
            if (one != VK_SUCCESS) {
               printf("  alone: %s[%zu] -> %d (%s)\n", groups[g].name, i, one, result_name(one));
               failures++;
            }
         }
      }
      printf("single features that fail: %d\n", failures);
   }
   printf("RESULT: %s\n", r == VK_SUCCESS ? "PASS" : "FAIL");
   return r == VK_SUCCESS ? 0 : 1;
}
