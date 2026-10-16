// vktest <frames | seconds"s">
// image to a cycling color with a moving bar; built 32-bit to test the
// i686 layer. Prints the average frame time.
#define VK_USE_PLATFORM_XCB_KHR
#include <vulkan/vulkan.h>
#include <xcb/xcb.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <string.h>

#define CK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s: %d\n", #x, r_); exit(1); } } while (0)

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    int frames = argc > 1 ? atoi(argv[1]) : 600;
    double dur = argc > 1 && strchr(argv[1], 's') ? atof(argv[1]) : 0;
    uint32_t W = 1280, H = 720;
    xcb_connection_t *c = xcb_connect(NULL, NULL);
    xcb_screen_t *s = xcb_setup_roots_iterator(xcb_get_setup(c)).data;
    xcb_window_t win = xcb_generate_id(c);
    xcb_create_window(c, XCB_COPY_FROM_PARENT, win, s->root, 0, 0, W, H, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT, s->root_visual, 0, NULL);
    xcb_map_window(c, win);
    xcb_flush(c);

    const char *iext[] = {"VK_KHR_surface", "VK_KHR_xcb_surface"};
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = iext};
    VkInstance inst;
    CK(vkCreateInstance(&ici, NULL, &inst));
    VkXcbSurfaceCreateInfoKHR sci = {.sType = VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR, .connection = c, .window = win};
    VkSurfaceKHR surf;
    CK(vkCreateXcbSurfaceKHR(inst, &sci, NULL, &surf));
    uint32_t n = 8;
    VkPhysicalDevice pds[8];
    CK(vkEnumeratePhysicalDevices(inst, &n, pds));
    VkPhysicalDevice pd = pds[0];
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio};
    const char *dext[] = {"VK_KHR_swapchain"};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = dext};
    VkDevice dev;
    CK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q;
    vkGetDeviceQueue(dev, 0, 0, &q);
    VkSwapchainCreateInfoKHR swci = {.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surf, .minImageCount = 3, .imageFormat = VK_FORMAT_B8G8R8A8_UNORM, .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = {W, H}, .imageArrayLayers = 1, .imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR, .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, .presentMode = VK_PRESENT_MODE_IMMEDIATE_KHR, .clipped = 1};
    VkSwapchainKHR sc;
    if (vkCreateSwapchainKHR(dev, &swci, NULL, &sc) != VK_SUCCESS) {
        swci.presentMode = VK_PRESENT_MODE_FIFO_KHR;
        CK(vkCreateSwapchainKHR(dev, &swci, NULL, &sc));
    }
    uint32_t ni = 0;
    vkGetSwapchainImagesKHR(dev, sc, &ni, NULL);
    VkImage imgs[16];
    vkGetSwapchainImagesKHR(dev, sc, &ni, imgs);
    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT};
    VkCommandPool pool;
    CK(vkCreateCommandPool(dev, &pci, NULL, &pool));
    VkCommandBuffer cb[2];
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .commandBufferCount = 2};
    CK(vkAllocateCommandBuffers(dev, &cai, cb));
    VkSemaphore acq[2], done[2];
    VkFence fence[2];
    for (int i = 0; i < 2; i++) {
        VkSemaphoreCreateInfo si = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        vkCreateSemaphore(dev, &si, NULL, &acq[i]);
        vkCreateSemaphore(dev, &si, NULL, &done[i]);
        VkFenceCreateInfo fi = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT};
        vkCreateFence(dev, &fi, NULL, &fence[i]);
    }
    VkImageSubresourceRange rng = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    double t0 = now(), worst = 0, prev = t0;
    int f;
    for (f = 0; dur ? now() - t0 < dur : f < frames; f++) {
        int k = f & 1;
        vkWaitForFences(dev, 1, &fence[k], 1, UINT64_MAX);
        vkResetFences(dev, 1, &fence[k]);
        uint32_t idx;
        CK(vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acq[k], VK_NULL_HANDLE, &idx));
        VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
        vkBeginCommandBuffer(cb[k], &bi);
        VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = imgs[idx], .subresourceRange = rng};
        vkCmdPipelineBarrier(cb[k], VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
        VkClearColorValue col = {.float32 = {(f % 120) / 120.0f, 0.3f, 0.7f, 1}};
        vkCmdClearColorImage(cb[k], imgs[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &col, 1, &rng);
        b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        b.dstAccessMask = 0;
        b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        b.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        vkCmdPipelineBarrier(cb[k], VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &b);
        vkEndCommandBuffer(cb[k]);
        VkPipelineStageFlags st = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1, .pWaitSemaphores = &acq[k], .pWaitDstStageMask = &st, .commandBufferCount = 1, .pCommandBuffers = &cb[k], .signalSemaphoreCount = 1, .pSignalSemaphores = &done[k]};
        CK(vkQueueSubmit(q, 1, &si, fence[k]));
        VkPresentInfoKHR pi = {.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &done[k], .swapchainCount = 1, .pSwapchains = &sc, .pImageIndices = &idx};
        vkQueuePresentKHR(q, &pi);
        double t = now();
        if (t - prev > worst) worst = t - prev;
        prev = t;
    }
    vkDeviceWaitIdle(dev);
    double el = now() - t0;
    printf("frames %d avg %.3f ms worst %.3f ms\n", f, el * 1000 / f, worst * 1000);
    return 0;
}
