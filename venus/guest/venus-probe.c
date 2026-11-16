/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Loads the Venus ICD directly (no Vulkan loader) and walks the first calls a driver must get
 * right: instance, physical device, properties, memory types, queues, device, a buffer.
 * Usage: venus-probe.exe [path\to\vulkan_virtio.dll]
 */

#include <windows.h>
#include <stdio.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#define VK_USE_PLATFORM_WIN32_KHR
#include <vulkan/vulkan.h>

typedef PFN_vkVoidFunction(VKAPI_PTR *PFN_icdGetInstanceProcAddr)(VkInstance, const char *);
typedef VkResult(VKAPI_PTR *PFN_icdNegotiate)(uint32_t *);

#define CHECK(call)                                                                      \
   do {                                                                                  \
      VkResult r_ = (call);                                                              \
      if (r_ != VK_SUCCESS) {                                                            \
         printf("FAIL %s:%d %s -> %d\n", __FILE__, __LINE__, #call, (int)r_);            \
         return 1;                                                                       \
      }                                                                                  \
      printf("ok   %s\n", #call);                                                        \
   } while (0)

int
main(int argc, char **argv)
{
   setvbuf(stdout, NULL, _IONBF, 0);
   const char *path = argc > 1 ? argv[1] : "vulkan_virtio.dll";
   HMODULE dll = LoadLibraryA(path);
   if (!dll) {
      printf("FAIL LoadLibrary(%s): %lu\n", path, GetLastError());
      return 1;
   }
   PFN_icdNegotiate negotiate =
      (PFN_icdNegotiate)GetProcAddress(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
   PFN_icdGetInstanceProcAddr gipa =
      (PFN_icdGetInstanceProcAddr)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
   if (!negotiate || !gipa) {
      printf("FAIL the ICD does not export its entry points\n");
      return 1;
   }
   uint32_t iface = 5;
   CHECK(negotiate(&iface));
   printf("icd interface version %u\n", iface);

   PFN_vkCreateInstance create_instance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
   if (!create_instance) {
      printf("FAIL no vkCreateInstance\n");
      return 1;
   }

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "venus-probe",
      .apiVersion = VK_API_VERSION_1_1,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst;
   CHECK(create_instance(&ici, NULL, &inst));

#define GET(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
   GET(vkEnumeratePhysicalDevices);
   GET(vkGetPhysicalDeviceProperties);
   GET(vkGetPhysicalDeviceMemoryProperties);
   GET(vkGetPhysicalDeviceQueueFamilyProperties);
   GET(vkCreateDevice);
   GET(vkGetDeviceProcAddr);
   GET(vkDestroyInstance);

   uint32_t count = 0;
   CHECK(vkEnumeratePhysicalDevices(inst, &count, NULL));
   printf("physical devices: %u\n", count);
   if (!count)
      return 1;
   VkPhysicalDevice pd;
   count = 1;
   VkResult r = vkEnumeratePhysicalDevices(inst, &count, &pd);
   if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
      printf("FAIL vkEnumeratePhysicalDevices -> %d\n", (int)r);
      return 1;
   }

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   printf("device: %s vendor=0x%x device=0x%x type=%d api=%u.%u.%u driver=0x%x\n",
          props.deviceName, props.vendorID, props.deviceID, (int)props.deviceType,
          VK_API_VERSION_MAJOR(props.apiVersion), VK_API_VERSION_MINOR(props.apiVersion),
          VK_API_VERSION_PATCH(props.apiVersion), props.driverVersion);

   VkPhysicalDeviceMemoryProperties mem;
   vkGetPhysicalDeviceMemoryProperties(pd, &mem);
   for (uint32_t i = 0; i < mem.memoryTypeCount; i++)
      printf("memory type %u: flags=0x%x heap=%u\n", i, mem.memoryTypes[i].propertyFlags,
             mem.memoryTypes[i].heapIndex);
   for (uint32_t i = 0; i < mem.memoryHeapCount; i++)
      printf("memory heap %u: size=%llu flags=0x%x\n", i,
             (unsigned long long)mem.memoryHeaps[i].size, mem.memoryHeaps[i].flags);

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, NULL);
   VkQueueFamilyProperties qf[16];
   if (nq > 16)
      nq = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
   uint32_t family = UINT32_MAX;
   for (uint32_t i = 0; i < nq; i++) {
      printf("queue family %u: flags=0x%x count=%u\n", i, qf[i].queueFlags, qf[i].queueCount);
      if (family == UINT32_MAX && (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT))
         family = i;
   }
   if (family == UINT32_MAX) {
      printf("FAIL no graphics queue family\n");
      return 1;
   }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = family,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

   PFN_vkGetDeviceQueue get_queue = (PFN_vkGetDeviceQueue)vkGetDeviceProcAddr(dev, "vkGetDeviceQueue");
   PFN_vkQueueWaitIdle wait_idle = (PFN_vkQueueWaitIdle)vkGetDeviceProcAddr(dev, "vkQueueWaitIdle");
   PFN_vkCreateFence create_fence = (PFN_vkCreateFence)vkGetDeviceProcAddr(dev, "vkCreateFence");
   PFN_vkWaitForFences wait_fences = (PFN_vkWaitForFences)vkGetDeviceProcAddr(dev, "vkWaitForFences");
   PFN_vkQueueSubmit submit = (PFN_vkQueueSubmit)vkGetDeviceProcAddr(dev, "vkQueueSubmit");
   PFN_vkDestroyFence destroy_fence = (PFN_vkDestroyFence)vkGetDeviceProcAddr(dev, "vkDestroyFence");
   PFN_vkDestroyDevice destroy_device = (PFN_vkDestroyDevice)vkGetDeviceProcAddr(dev, "vkDestroyDevice");

   VkQueue queue;
   get_queue(dev, family, 0, &queue);

   /* An empty submit with a fence: exercises the timeline sync and the fence retire path. */
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(create_fence(dev, &fci, NULL, &fence));
   CHECK(submit(queue, 0, NULL, fence));
   CHECK(wait_fences(dev, 1, &fence, VK_TRUE, 5000000000ull));
   CHECK(wait_idle(queue));
   destroy_fence(dev, fence, NULL);

   destroy_device(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("RESULT ok\n");
   return 0;
}
