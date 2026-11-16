// SPDX-License-Identifier: GPL-3.0-or-later
//
// Which step of a Vulkan program throttles the whole Windows display to 31 updates a second?
// Builds a Vulkan window program stage by stage and holds at the stage given, so it can be run
// next to a GDI program (gdiblit) while the display update rate is read from the viewer:
//   stage 1: load the Vulkan loader (or the driver itself with --icd), create an instance, list the devices
//   stage 2: + create a window and a Win32 surface
//   stage 3: + create a device and a queue
//   stage 4: + create a swapchain (nothing is presented)
//   stage 5: + present: a clear colour that changes every frame, paced to --fps (0 = as fast as it goes)
// Usage: vkpresent --stage N [--fps N] [--size WxH] [--icd PATH] [--frames N]
// (--frames 0 runs until the window is closed; the window title shows the frame number and rate)
#define WIN32_LEAN_AND_MEAN
#define VK_NO_PROTOTYPES
#define VK_USE_PLATFORM_WIN32_KHR
#include <windows.h>
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "hud.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { printf("RESULT: FAIL: %s = %d\n", #x, (int)r_); return 1; } } while (0)

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
    return DefWindowProcW(h, m, w, l);
}

static PFN_vkGetInstanceProcAddr gipa;
static PFN_vkGetDeviceProcAddr gdpa;

#define INST(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define DEV(name) PFN_##name name = (PFN_##name)gdpa(dev, #name)

int main(int argc, char **argv) {
    int stage = 5, W = 640, H = 360, fps = 0, frames = 0;
    const char *icd = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--stage") && i + 1 < argc) stage = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--fps") && i + 1 < argc) fps = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--icd") && i + 1 < argc) icd = argv[++i];
        else if (!strcmp(argv[i], "--size") && i + 1 < argc && sscanf(argv[++i], "%dx%d", &W, &H) != 2) { fprintf(stderr, "bad --size\n"); return 2; }
    }

    HMODULE lib = LoadLibraryA(icd ? icd : "vulkan-1.dll");
    if (!lib) { printf("RESULT: FAIL: cannot load %s (%lu)\n", icd ? icd : "vulkan-1.dll", GetLastError()); return 1; }
    gipa = (PFN_vkGetInstanceProcAddr)GetProcAddress(lib, icd ? "vk_icdGetInstanceProcAddr" : "vkGetInstanceProcAddr");
    if (!gipa) { printf("RESULT: FAIL: no GetInstanceProcAddr\n"); return 1; }

    VkInstance inst = 0;
    {
        PFN_vkCreateInstance create = (PFN_vkCreateInstance)gipa(0, "vkCreateInstance");
        const char *exts[] = {"VK_KHR_surface", "VK_KHR_win32_surface"};
        VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO}; app.apiVersion = VK_API_VERSION_1_1; app.pApplicationName = "vkpresent";
        VkInstanceCreateInfo ci = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO}; ci.pApplicationInfo = &app;
        ci.enabledExtensionCount = stage >= 2 ? 2 : 0; ci.ppEnabledExtensionNames = exts;
        CHECK(create(&ci, 0, &inst));
    }
    INST(vkEnumeratePhysicalDevices);
    uint32_t pc = 1; VkPhysicalDevice pd = 0;
    CHECK(vkEnumeratePhysicalDevices(inst, &pc, &pd));
    INST(vkGetPhysicalDeviceProperties);
    VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
    printf("device: %s\n", props.deviceName);
    wchar_t adapter[128] = L""; MultiByteToWideChar(CP_ACP, 0, props.deviceName, -1, adapter, 128);

    WNDCLASSW wc = {}; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"vkpresent"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"vkpresent", L"WinBridge Vulkan present", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                700, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);
    Hud hud; hud.init(hwnd, L"vkpresent", adapter, W, H, frames);
    char title[64]; (void)title;

    VkSurfaceKHR surface = 0; VkDevice dev = 0; VkSwapchainKHR sc = 0; VkQueue queue = 0;
    uint32_t nimg = 0; VkImage images[8];
    VkCommandPool pool = 0; VkCommandBuffer cmd = 0; VkSemaphore acq = 0, rel = 0; VkFence fence = 0;
    if (stage >= 2) {
        INST(vkCreateWin32SurfaceKHR);
        VkWin32SurfaceCreateInfoKHR si = {VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR}; si.hinstance = wc.hInstance; si.hwnd = hwnd;
        CHECK(vkCreateWin32SurfaceKHR(inst, &si, 0, &surface));
    }
    if (stage >= 3) {
        INST(vkCreateDevice); INST(vkGetDeviceProcAddr); INST(vkGetPhysicalDeviceSurfaceSupportKHR);
        INST(vkGetPhysicalDeviceQueueFamilyProperties);
        uint32_t qn = 8; VkQueueFamilyProperties qf[8]; vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qf);
        uint32_t family = qn;
        for (uint32_t i = 0; i < qn; i++) {
            VkBool32 ok = 0; vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, surface, &ok);
            if (ok && (qf[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_TRANSFER_BIT))) { family = i; break; }
        }
        if (family == qn) { printf("RESULT: FAIL: no queue family can present\n"); return 1; }
        float prio = 1.0f; VkDeviceQueueCreateInfo qi = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO}; qi.queueFamilyIndex = family; qi.queueCount = 1; qi.pQueuePriorities = &prio;
        const char *dexts[] = {"VK_KHR_swapchain"};
        VkDeviceCreateInfo di = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO}; di.queueCreateInfoCount = 1; di.pQueueCreateInfos = &qi; di.enabledExtensionCount = 1; di.ppEnabledExtensionNames = dexts;
        CHECK(vkCreateDevice(pd, &di, 0, &dev));
        gdpa = (PFN_vkGetDeviceProcAddr)vkGetDeviceProcAddr;
        DEV(vkGetDeviceQueue); vkGetDeviceQueue(dev, family, 0, &queue);
        if (stage >= 4) {
            INST(vkGetPhysicalDeviceSurfaceCapabilitiesKHR); INST(vkGetPhysicalDeviceSurfaceFormatsKHR);
            DEV(vkCreateSwapchainKHR); DEV(vkGetSwapchainImagesKHR);
            VkSurfaceCapabilitiesKHR caps; CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &caps));
            uint32_t fn = 8; VkSurfaceFormatKHR fm[8]; CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &fn, fm));
            VkSwapchainCreateInfoKHR sci = {VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR}; sci.surface = surface;
            sci.minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount; if (caps.maxImageCount && sci.minImageCount > caps.maxImageCount) sci.minImageCount = caps.maxImageCount;
            sci.imageFormat = fm[0].format; sci.imageColorSpace = fm[0].colorSpace;
            sci.imageExtent = caps.currentExtent.width == 0xffffffffu ? VkExtent2D{(uint32_t)W, (uint32_t)H} : caps.currentExtent;
            sci.imageArrayLayers = 1; sci.imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT; sci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
            sci.preTransform = caps.currentTransform; sci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
            sci.presentMode = VK_PRESENT_MODE_FIFO_KHR; sci.clipped = VK_TRUE;
            CHECK(vkCreateSwapchainKHR(dev, &sci, 0, &sc));
            nimg = 8; CHECK(vkGetSwapchainImagesKHR(dev, sc, &nimg, images));
            printf("swapchain: %u images\n", nimg);
            if (stage >= 5) {
                DEV(vkCreateCommandPool); DEV(vkAllocateCommandBuffers); DEV(vkCreateSemaphore); DEV(vkCreateFence);
                VkCommandPoolCreateInfo pi = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO}; pi.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT; pi.queueFamilyIndex = family;
                CHECK(vkCreateCommandPool(dev, &pi, 0, &pool));
                VkCommandBufferAllocateInfo ai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO}; ai.commandPool = pool; ai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY; ai.commandBufferCount = 1;
                CHECK(vkAllocateCommandBuffers(dev, &ai, &cmd));
                VkSemaphoreCreateInfo semi = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
                CHECK(vkCreateSemaphore(dev, &semi, 0, &acq)); CHECK(vkCreateSemaphore(dev, &semi, 0, &rel));
                VkFenceCreateInfo fi = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}; fi.flags = VK_FENCE_CREATE_SIGNALED_BIT;
                CHECK(vkCreateFence(dev, &fi, 0, &fence));
            }
        }
    }
    printf("stage %d reached\n", stage);

    if (stage < 5) { while (hud.pump()) Sleep(10); printf("RESULT: PASS\n"); return 0; }

    DEV(vkAcquireNextImageKHR); DEV(vkQueueSubmit); DEV(vkQueuePresentKHR); DEV(vkWaitForFences); DEV(vkResetFences);
    DEV(vkResetCommandBuffer); DEV(vkBeginCommandBuffer); DEV(vkEndCommandBuffer); DEV(vkCmdPipelineBarrier); DEV(vkCmdClearColorImage);
    LARGE_INTEGER f, now, next, t0; QueryPerformanceFrequency(&f); QueryPerformanceCounter(&next);
    for (int n = 0; frames <= 0 || n < frames; n++) {
        if (!hud.pump()) break;
        if (fps > 0) {
            next.QuadPart += f.QuadPart / fps;
            for (;;) {
                QueryPerformanceCounter(&now);
                LONGLONG left = next.QuadPart - now.QuadPart;
                if (left <= 0) break;
                if (left > f.QuadPart / 500) Sleep(1);
            }
        }
        QueryPerformanceCounter(&t0);
        CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX)); CHECK(vkResetFences(dev, 1, &fence));
        uint32_t idx = 0; CHECK(vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acq, VK_NULL_HANDLE, &idx));
        CHECK(vkResetCommandBuffer(cmd, 0));
        VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}; bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
        CHECK(vkBeginCommandBuffer(cmd, &bi));
        VkImageSubresourceRange rng = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageMemoryBarrier b = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER}; b.image = images[idx]; b.subresourceRange = rng;
        b.srcQueueFamilyIndex = b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        b.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED; b.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; b.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, 0, 0, 0, 1, &b);
        VkClearColorValue cc = {{(n % 120) / 120.0f, 0.3f, 1.0f - (n % 120) / 120.0f, 1.0f}};
        vkCmdClearColorImage(cmd, images[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &cc, 1, &rng);
        b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; b.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR; b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT; b.dstAccessMask = 0;
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, 0, 0, 0, 1, &b);
        CHECK(vkEndCommandBuffer(cmd));
        VkPipelineStageFlags wait = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO}; si.waitSemaphoreCount = 1; si.pWaitSemaphores = &acq; si.pWaitDstStageMask = &wait;
        si.commandBufferCount = 1; si.pCommandBuffers = &cmd; si.signalSemaphoreCount = 1; si.pSignalSemaphores = &rel;
        CHECK(vkQueueSubmit(queue, 1, &si, fence));
        VkPresentInfoKHR pi = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR}; pi.waitSemaphoreCount = 1; pi.pWaitSemaphores = &rel; pi.swapchainCount = 1; pi.pSwapchains = &sc; pi.pImageIndices = &idx;
        CHECK(vkQueuePresentKHR(queue, &pi));
        QueryPerformanceCounter(&now);
        hud.frame((double)(now.QuadPart - t0.QuadPart) * 1000.0 / (double)f.QuadPart);
    }
    printf("frames: %lld\nRESULT: PASS\n", hud.frames);
    return 0;
}
