/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Loads the Venus ICD directly (no Vulkan loader) and walks the first calls a driver must get
 * right: instance, physical device, properties, memory types, queues, device, a buffer.
 * Usage: venus-probe.exe [path\to\vulkan_virtio.dll]. It also builds on Linux against Mesa's
 * libvulkan_virtio.so, which is how the command streams of both builds are compared.
 */

#ifdef _WIN32
#include <windows.h>
#define LOAD_DRIVER(path) LoadLibraryA(path)
#define DRIVER_SYMBOL(lib, name) GetProcAddress(lib, name)
#define DRIVER_DEFAULT "vulkan_virtio.dll"
typedef HMODULE driver_t;
#else
#include <dlfcn.h>
#define LOAD_DRIVER(path) dlopen(path, RTLD_NOW)
#define DRIVER_SYMBOL(lib, name) dlsym(lib, name)
#define DRIVER_DEFAULT "libvulkan_virtio.so"
typedef void *driver_t;
#endif
#include <stdio.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#ifdef _WIN32
#define VK_USE_PLATFORM_WIN32_KHR
#endif
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
   const char *path = argc > 1 ? argv[1] : DRIVER_DEFAULT;
   driver_t dll = LOAD_DRIVER(path);
   if (!dll) {
      printf("FAIL cannot load %s\n", path);
      return 1;
   }
   PFN_icdNegotiate negotiate =
      (PFN_icdNegotiate)DRIVER_SYMBOL(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
   PFN_icdGetInstanceProcAddr gipa =
      (PFN_icdGetInstanceProcAddr)DRIVER_SYMBOL(dll, "vk_icdGetInstanceProcAddr");
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

   /* Memory (skipped when the second argument is `basic`, which is what a Linux run on a driver
    * that cannot map host-visible memory through vtest uses): fill a host visible buffer, copy it
    * on the GPU into a second one, read it back. Exercises device memory creation, the mapping,
    * the guest-to-host push before the submit and the host-to-guest push when the fence retires. */
   if (argc < 3 || strcmp(argv[2], "basic") != 0) {
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
      DEV(vkCreateBuffer);
      DEV(vkGetBufferMemoryRequirements);
      DEV(vkAllocateMemory);
      DEV(vkBindBufferMemory);
      DEV(vkMapMemory);
      DEV(vkCreateCommandPool);
      DEV(vkAllocateCommandBuffers);
      DEV(vkBeginCommandBuffer);
      DEV(vkCmdCopyBuffer);
      DEV(vkEndCommandBuffer);
      const VkDeviceSize bytes = 1u << 20;
      VkBuffer buf[2];
      VkDeviceMemory memory[2];
      uint32_t *ptr[2];
      for (int i = 0; i < 2; i++) {
         VkBufferCreateInfo bci = {
            .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            .size = bytes,
            .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
         };
         CHECK(vkCreateBuffer(dev, &bci, NULL, &buf[i]));
         VkMemoryRequirements req;
         vkGetBufferMemoryRequirements(dev, buf[i], &req);
         uint32_t type = UINT32_MAX;
         const VkMemoryPropertyFlags want =
            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
         for (uint32_t t = 0; t < mem.memoryTypeCount; t++) {
            if ((req.memoryTypeBits & (1u << t)) &&
                (mem.memoryTypes[t].propertyFlags & want) == want) {
               type = t;
               break;
            }
         }
         if (type == UINT32_MAX) {
            printf("FAIL no host visible coherent type for the buffer\n");
            return 1;
         }
         VkMemoryAllocateInfo mai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .allocationSize = req.size,
            .memoryTypeIndex = type,
         };
         CHECK(vkAllocateMemory(dev, &mai, NULL, &memory[i]));
         CHECK(vkBindBufferMemory(dev, buf[i], memory[i], 0));
         void *p;
         CHECK(vkMapMemory(dev, memory[i], 0, bytes, 0, &p));
         ptr[i] = p;
      }
      for (VkDeviceSize i = 0; i < bytes / 4; i++) {
         ptr[0][i] = (uint32_t)i * 2654435761u;
         ptr[1][i] = 0xdeadbeefu;
      }

      VkCommandPoolCreateInfo pci = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
         .queueFamilyIndex = family,
      };
      VkCommandPool pool;
      CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
      VkCommandBufferAllocateInfo cai = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
         .commandPool = pool,
         .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
         .commandBufferCount = 1,
      };
      VkCommandBuffer cmd;
      CHECK(vkAllocateCommandBuffers(dev, &cai, &cmd));
      VkCommandBufferBeginInfo cbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
      CHECK(vkBeginCommandBuffer(cmd, &cbi));
      VkBufferCopy region = { .size = bytes };
      vkCmdCopyBuffer(cmd, buf[0], buf[1], 1, &region);
      CHECK(vkEndCommandBuffer(cmd));

      VkFence copy_fence;
      CHECK(create_fence(dev, &fci, NULL, &copy_fence));
      VkSubmitInfo si = {
         .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
         .commandBufferCount = 1,
         .pCommandBuffers = &cmd,
      };
      CHECK(submit(queue, 1, &si, copy_fence));
      CHECK(wait_fences(dev, 1, &copy_fence, VK_TRUE, 10000000000ull));
      uint64_t bad = 0;
      VkDeviceSize first_bad = 0;
      for (VkDeviceSize i = 0; i < bytes / 4; i++) {
         if (ptr[1][i] != (uint32_t)i * 2654435761u) {
            if (!bad)
               first_bad = i;
            bad++;
         }
      }
      if (bad) {
         printf("FAIL copy readback: %llu of %llu words differ, first at %llu (got 0x%x)\n",
                (unsigned long long)bad, (unsigned long long)(bytes / 4),
                (unsigned long long)first_bad, ptr[1][first_bad]);
         return 1;
      }
      printf("ok   buffer copy through the GPU, %llu bytes read back\n",
             (unsigned long long)bytes);
      PFN_vkDestroyBuffer destroy_buffer = (PFN_vkDestroyBuffer)vkGetDeviceProcAddr(dev, "vkDestroyBuffer");
      PFN_vkFreeMemory free_memory = (PFN_vkFreeMemory)vkGetDeviceProcAddr(dev, "vkFreeMemory");
      PFN_vkDestroyCommandPool destroy_pool = (PFN_vkDestroyCommandPool)vkGetDeviceProcAddr(dev, "vkDestroyCommandPool");
      printf("destroying the copy resources\n");
      destroy_fence(dev, copy_fence, NULL);
      destroy_pool(dev, pool, NULL);
      for (int i = 0; i < 2; i++) {
         destroy_buffer(dev, buf[i], NULL);
         free_memory(dev, memory[i], NULL);
      }
   }
   printf("destroying the device\n");
   destroy_device(dev, NULL);
   printf("destroying the instance\n");
   vkDestroyInstance(inst, NULL);
   printf("RESULT ok\n");
   return 0;
}
