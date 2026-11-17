/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Several Vulkan instances in one process. Every instance is a venus context on the host, and all
 * of them must share the one slot of the process: the VM has only a few slots, so more instances
 * than slots can only work if the slot is shared (FurMark's Vulkan mode creates two).
 *
 * Each instance gets a device, a host-visible buffer and a queue. A round fills the buffer on the
 * GPU with a pattern of its own (vkCmdFillBuffer, a fence wait) and checks what the CPU reads
 * back, so the mirrored memory of every instance is exercised. The test then
 *   1. opens N instances at once and runs a round on each,
 *   2. destroys every other one while the rest keep working,
 *   3. opens one more next to the survivors and runs a round on all,
 *   4. destroys everything, which gives the slot back, and opens one instance again.
 * Usage: multiinst.exe [instances]   (default 4, at most 16)
 * Prints RESULT: PASS or RESULT: FAIL and exits with 0 or 1.
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define MAX_INSTANCES 16
#define BUFFER_BYTES (64 * 1024)

#define FAIL(...)                                                                                  \
   do {                                                                                            \
      printf("FAIL: ");                                                                            \
      printf(__VA_ARGS__);                                                                         \
      printf("\n");                                                                                \
      return false;                                                                                \
   } while (0)

#define VK_FUNCTIONS(X)                                                                            \
   X(vkDestroyInstance) X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties)             \
   X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties)              \
   X(vkCreateDevice) X(vkDestroyDevice) X(vkDeviceWaitIdle) X(vkGetDeviceQueue)                    \
   X(vkCreateBuffer) X(vkDestroyBuffer) X(vkGetBufferMemoryRequirements) X(vkAllocateMemory)       \
   X(vkFreeMemory) X(vkBindBufferMemory) X(vkMapMemory) X(vkUnmapMemory)                           \
   X(vkCreateCommandPool) X(vkDestroyCommandPool) X(vkAllocateCommandBuffers)                      \
   X(vkResetCommandBuffer) X(vkBeginCommandBuffer) X(vkEndCommandBuffer) X(vkCmdFillBuffer)        \
   X(vkCreateFence) X(vkDestroyFence) X(vkResetFences) X(vkQueueSubmit) X(vkWaitForFences)

static PFN_vkGetInstanceProcAddr gipa;
static PFN_vkCreateInstance create_instance;
#define X(name) static PFN_##name name;
VK_FUNCTIONS(X)
#undef X

struct inst {
   bool live;
   VkInstance instance;
   VkDevice device;
   VkQueue queue;
   VkBuffer buffer;
   VkDeviceMemory memory;
   VkCommandPool pool;
   VkCommandBuffer cb;
   VkFence fence;
   uint32_t *ptr;
   char gpu[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE];
};

static struct inst insts[MAX_INSTANCES];

static bool
load_functions(VkInstance instance)
{
#define X(name)                                                                                    \
   name = (PFN_##name)gipa(instance, #name);                                                       \
   if (!name)                                                                                      \
      FAIL("the loader has no " #name);
   VK_FUNCTIONS(X)
#undef X
   return true;
}

static bool
open_instance(struct inst *c, bool first)
{
   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "multiinst",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkResult r = create_instance(&ici, NULL, &c->instance);
   if (r != VK_SUCCESS)
      FAIL("vkCreateInstance: %d", r);
   if (first && !load_functions(c->instance))
      return false;

   uint32_t n = 1;
   VkPhysicalDevice phys;
   r = vkEnumeratePhysicalDevices(c->instance, &n, &phys);
   if (r != VK_SUCCESS && r != VK_INCOMPLETE)
      FAIL("vkEnumeratePhysicalDevices: %d", r);
   if (!n)
      FAIL("no physical device");
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   snprintf(c->gpu, sizeof(c->gpu), "%s", props.deviceName);

   uint32_t qn = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, NULL);
   VkQueueFamilyProperties qf[16];
   if (qn > 16)
      qn = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, qf);
   uint32_t family = qn;
   for (uint32_t i = 0; i < qn; i++) {
      if (qf[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT)) {
         family = i;
         break;
      }
   }
   if (family == qn)
      FAIL("no queue family can fill a buffer");

   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = family,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   r = vkCreateDevice(phys, &dci, NULL, &c->device);
   if (r != VK_SUCCESS)
      FAIL("vkCreateDevice: %d", r);
   vkGetDeviceQueue(c->device, family, 0, &c->queue);

   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = BUFFER_BYTES,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   r = vkCreateBuffer(c->device, &bci, NULL, &c->buffer);
   if (r != VK_SUCCESS)
      FAIL("vkCreateBuffer: %d", r);
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(c->device, c->buffer, &req);
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   const VkMemoryPropertyFlags want =
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
   uint32_t type = mp.memoryTypeCount;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
      if ((req.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) {
         type = i;
         break;
      }
   }
   if (type == mp.memoryTypeCount)
      FAIL("no host-visible coherent memory type");
   const VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = type,
   };
   r = vkAllocateMemory(c->device, &mai, NULL, &c->memory);
   if (r != VK_SUCCESS)
      FAIL("vkAllocateMemory: %d", r);
   r = vkBindBufferMemory(c->device, c->buffer, c->memory, 0);
   if (r != VK_SUCCESS)
      FAIL("vkBindBufferMemory: %d", r);
   void *ptr = NULL;
   r = vkMapMemory(c->device, c->memory, 0, BUFFER_BYTES, 0, &ptr);
   if (r != VK_SUCCESS)
      FAIL("vkMapMemory: %d", r);
   c->ptr = ptr;

   const VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = family,
   };
   r = vkCreateCommandPool(c->device, &pci, NULL, &c->pool);
   if (r != VK_SUCCESS)
      FAIL("vkCreateCommandPool: %d", r);
   const VkCommandBufferAllocateInfo cbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = c->pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   r = vkAllocateCommandBuffers(c->device, &cbi, &c->cb);
   if (r != VK_SUCCESS)
      FAIL("vkAllocateCommandBuffers: %d", r);
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   r = vkCreateFence(c->device, &fci, NULL, &c->fence);
   if (r != VK_SUCCESS)
      FAIL("vkCreateFence: %d", r);
   c->live = true;
   return true;
}

static void
close_instance(struct inst *c)
{
   if (c->device) {
      vkDeviceWaitIdle(c->device);
      if (c->fence)
         vkDestroyFence(c->device, c->fence, NULL);
      if (c->pool)
         vkDestroyCommandPool(c->device, c->pool, NULL);
      if (c->ptr)
         vkUnmapMemory(c->device, c->memory);
      if (c->memory)
         vkFreeMemory(c->device, c->memory, NULL);
      if (c->buffer)
         vkDestroyBuffer(c->device, c->buffer, NULL);
      vkDestroyDevice(c->device, NULL);
   }
   if (c->instance)
      vkDestroyInstance(c->instance, NULL);
   memset(c, 0, sizeof(*c));
}

/* The GPU fills the buffer with `value` after the CPU wrote something else into it. */
static bool
round_trip(struct inst *c, uint32_t value)
{
   memset(c->ptr, 0x5a, BUFFER_BYTES);
   const VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   if (vkResetCommandBuffer(c->cb, 0) != VK_SUCCESS || vkBeginCommandBuffer(c->cb, &bi) != VK_SUCCESS)
      FAIL("cannot record");
   vkCmdFillBuffer(c->cb, c->buffer, 0, BUFFER_BYTES, value);
   if (vkEndCommandBuffer(c->cb) != VK_SUCCESS)
      FAIL("vkEndCommandBuffer");
   const VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &c->cb,
   };
   if (vkResetFences(c->device, 1, &c->fence) != VK_SUCCESS)
      FAIL("vkResetFences");
   VkResult r = vkQueueSubmit(c->queue, 1, &si, c->fence);
   if (r != VK_SUCCESS)
      FAIL("vkQueueSubmit: %d", r);
   r = vkWaitForFences(c->device, 1, &c->fence, VK_TRUE, 5000000000ull);
   if (r != VK_SUCCESS)
      FAIL("vkWaitForFences: %d", r);
   for (size_t i = 0; i < BUFFER_BYTES / 4; i++) {
      if (c->ptr[i] != value)
         FAIL("word %zu is %08x, expected %08x", i, c->ptr[i], value);
   }
   return true;
}

static bool
round_all(const char *what, uint32_t salt)
{
   for (int i = 0; i < MAX_INSTANCES; i++) {
      if (!insts[i].live)
         continue;
      if (!round_trip(&insts[i], 0x10000000u * (uint32_t)(i + 1) + salt)) {
         printf("  (instance %d, %s)\n", i, what);
         return false;
      }
   }
   printf("%s: ok\n", what);
   return true;
}

static int
live_count(void)
{
   int n = 0;
   for (int i = 0; i < MAX_INSTANCES; i++)
      n += insts[i].live;
   return n;
}

int
main(int argc, char **argv)
{
   int count = argc > 1 ? atoi(argv[1]) : 4;
   if (count < 1 || count > MAX_INSTANCES) {
      printf("usage: multiinst.exe [instances 1..%d]\n", MAX_INSTANCES);
      return 2;
   }
   const HMODULE loader = LoadLibraryA("vulkan-1.dll");
   if (!loader) {
      printf("RESULT: FAIL (no vulkan-1.dll)\n");
      return 1;
   }
   gipa = (PFN_vkGetInstanceProcAddr)(void (*)(void))GetProcAddress(loader, "vkGetInstanceProcAddr");
   create_instance = gipa ? (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance") : NULL;
   if (!create_instance) {
      printf("RESULT: FAIL (no vkCreateInstance)\n");
      return 1;
   }

   bool ok = true;
   for (int i = 0; ok && i < count; i++) {
      ok = open_instance(&insts[i], i == 0);
      if (ok)
         printf("instance %d: %s\n", i, insts[i].gpu);
   }
   ok = ok && round_all("all instances open at once", 1);

   for (int i = 0; ok && i < count; i += 2)
      close_instance(&insts[i]);
   ok = ok && round_all("the survivors after every other instance was destroyed", 2);

   if (ok && count < MAX_INSTANCES) {
      ok = open_instance(&insts[count], false);
      ok = ok && round_all("one more instance next to the survivors", 3);
   }

   for (int i = 0; i < MAX_INSTANCES; i++) {
      if (insts[i].instance)
         close_instance(&insts[i]);
   }
   /* The slot went back to the host with the last instance; a new one claims it again. */
   if (ok && live_count() == 0) {
      ok = open_instance(&insts[0], false);
      ok = ok && round_all("a new instance after all were destroyed", 4);
      close_instance(&insts[0]);
   }

   printf("RESULT: %s\n", ok ? "PASS" : "FAIL");
   return ok ? 0 : 1;
}
