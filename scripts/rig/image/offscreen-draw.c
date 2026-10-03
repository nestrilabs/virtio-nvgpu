// offscreen-draw.c -- the smallest thing that actually draws.
//
// vkcube cannot run on either GPU box: the NVIDIA card's display outputs are
// empty on both, so VK_KHR_display enumerates zero displays and there is no WSI
// surface to present to. That is a property of the machines, not of forwarding.
//
// This is the replacement, and it is a better test anyway. It renders a
// triangle into a VkImage with no swapchain, copies it back to host-visible
// memory, and checks the pixels. It needs no display, and its shape is the one
// nescapture actually uses: render to an image, then get the image out.
//
// What it proves that vulkaninfo does not:
//   - a command buffer reaches the GPU and a shader rasterises
//   - the completion path works end to end -- vkQueueSubmit then a fence wait.
//     Handoff open item 2 (the guest maps everything write-combine, ignoring the
//     caching type the backend chose) bites exactly here if it bites at all.
//   - the result travels back across the boundary with the right bytes in it
//
// It deliberately does NOT touch dma-buf. The guest does not advertise
// VK_EXT_external_memory_dma_buf; see exporting-a-frame.md. Keep that a
// separate measurement so a failure here is unambiguous.
//
// Build with build-offscreen.sh (it compiles the two shaders in first).
// Exit status: 0 only if every check passed.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <vulkan/vulkan.h>

#include "offscreen_vert.h"   // unsigned char offscreen_vert_spv[]
#include "offscreen_frag.h"   // unsigned char offscreen_frag_spv[]

#define W 256
#define H 256

// Clear to blue, draw in red. Two distinct channels so a half-working copy
// cannot be mistaken for a pass.
static const float CLEAR_RGBA[4] = { 0.0f, 0.0f, 1.0f, 1.0f };

#define VK(expr) do {                                                        \
    VkResult _r = (expr);                                                    \
    if (_r != VK_SUCCESS) {                                                  \
        fprintf(stderr, "GUEST: FAIL %s -> VkResult %d (%s:%d)\n",           \
                #expr, (int)_r, __FILE__, __LINE__);                         \
        return 1;                                                            \
    }                                                                        \
} while (0)

static int fails = 0;

static void check(const char *what, int ok, const char *detail)
{
    printf("GUEST: %-28s %s%s%s\n", what, ok ? "PASS" : "FAIL",
           detail && *detail ? "  " : "", detail ? detail : "");
    if (!ok) fails++;
}

// Pick a memory type satisfying both the resource's bits and the properties we
// need. Getting this wrong is the classic way to get a silent wrong answer
// rather than an error, so it is a named function and not an inline loop.
static int memory_type(const VkPhysicalDeviceMemoryProperties *mp,
                       uint32_t type_bits, VkMemoryPropertyFlags want)
{
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
        if ((type_bits & (1u << i)) &&
            (mp->memoryTypes[i].propertyFlags & want) == want)
            return (int)i;
    return -1;
}

int main(int argc, char **argv)
{
    const char *ppm_path = (argc > 1) ? argv[1] : NULL;

    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "nvgpu-offscreen-draw",
                              .apiVersion = VK_API_VERSION_1_0 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &app };
    VkInstance inst;
    VK(vkCreateInstance(&ici, NULL, &inst));

    uint32_t n = 0;
    VK(vkEnumeratePhysicalDevices(inst, &n, NULL));
    if (n == 0) { fprintf(stderr, "GUEST: FAIL no physical devices\n"); return 1; }
    VkPhysicalDevice *devs = calloc(n, sizeof *devs);
    VK(vkEnumeratePhysicalDevices(inst, &n, devs));

    // Prefer a discrete GPU; report whatever we land on so a software device
    // cannot pass unnoticed.
    VkPhysicalDevice pd = devs[0];
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(devs[i], &p);
        if (p.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) { pd = devs[i]; break; }
    }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    printf("GUEST: device = %s (type %d, driver %u.%u.%u)\n", props.deviceName,
           (int)props.deviceType, VK_VERSION_MAJOR(props.driverVersion),
           VK_VERSION_MINOR(props.driverVersion), VK_VERSION_PATCH(props.driverVersion));
    check("discrete gpu",
          props.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU, props.deviceName);

    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
    VkQueueFamilyProperties *qf = calloc(qn, sizeof *qf);
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qf);
    uint32_t gq = UINT32_MAX;
    for (uint32_t i = 0; i < qn; i++)
        if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { gq = i; break; }
    if (gq == UINT32_MAX) { fprintf(stderr, "GUEST: FAIL no graphics queue\n"); return 1; }

    float prio = 1.0f;
    VkDeviceQueueCreateInfo dqi = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = gq, .queueCount = 1,
                                    .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &dqi };
    VkDevice dev;
    VK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue queue;
    vkGetDeviceQueue(dev, gq, 0, &queue);

    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);

    // ---- the render target, in device-local memory ----
    VkImageCreateInfo imgci = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .extent = { W, H, 1 },
        .mipLevels = 1, .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage img;
    VK(vkCreateImage(dev, &imgci, NULL, &img));
    VkMemoryRequirements imreq;
    vkGetImageMemoryRequirements(dev, img, &imreq);
    int it = memory_type(&mp, imreq.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    if (it < 0) { fprintf(stderr, "GUEST: FAIL no device-local memory type\n"); return 1; }
    VkMemoryAllocateInfo imai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                  .allocationSize = imreq.size,
                                  .memoryTypeIndex = (uint32_t)it };
    VkDeviceMemory immem;
    VK(vkAllocateMemory(dev, &imai, NULL, &immem));
    VK(vkBindImageMemory(dev, img, immem, 0));

    VkImageViewCreateInfo ivci = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = img, .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
    };
    VkImageView view;
    VK(vkCreateImageView(dev, &ivci, NULL, &view));

    // ---- readback buffer, host-visible ----
    VkDeviceSize bytes = (VkDeviceSize)W * H * 4;
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                               .size = bytes,
                               .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                               .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
    VkBuffer buf;
    VK(vkCreateBuffer(dev, &bci, NULL, &buf));
    VkMemoryRequirements breq;
    vkGetBufferMemoryRequirements(dev, buf, &breq);
    int bt = memory_type(&mp, breq.memoryTypeBits,
                         VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if (bt < 0) { fprintf(stderr, "GUEST: FAIL no host-visible coherent memory type\n"); return 1; }
    VkMemoryAllocateInfo bai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = breq.size,
                                 .memoryTypeIndex = (uint32_t)bt };
    VkDeviceMemory bmem;
    VK(vkAllocateMemory(dev, &bai, NULL, &bmem));
    VK(vkBindBufferMemory(dev, buf, bmem, 0));

    // ---- render pass ----
    VkAttachmentDescription att = {
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
        .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
        .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
    };
    VkAttachmentReference ref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                                 .colorAttachmentCount = 1, .pColorAttachments = &ref };
    VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                                    .attachmentCount = 1, .pAttachments = &att,
                                    .subpassCount = 1, .pSubpasses = &sub };
    VkRenderPass rp;
    VK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

    VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                     .renderPass = rp, .attachmentCount = 1,
                                     .pAttachments = &view,
                                     .width = W, .height = H, .layers = 1 };
    VkFramebuffer fb;
    VK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

    // ---- pipeline. No vertex buffers: the vertex shader builds the triangle
    // from gl_VertexIndex, so there is one less thing to get wrong. ----
    VkShaderModuleCreateInfo vsci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                      .codeSize = sizeof offscreen_vert_spv,
                                      .pCode = (const uint32_t *)offscreen_vert_spv };
    VkShaderModuleCreateInfo fsci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                      .codeSize = sizeof offscreen_frag_spv,
                                      .pCode = (const uint32_t *)offscreen_frag_spv };
    VkShaderModule vs, fs;
    VK(vkCreateShaderModule(dev, &vsci, NULL, &vs));
    VK(vkCreateShaderModule(dev, &fsci, NULL, &fs));

    VkPipelineShaderStageCreateInfo stages[2] = {
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
          .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" },
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
          .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main" },
    };
    VkPipelineVertexInputStateCreateInfo vi = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
    VkPipelineInputAssemblyStateCreateInfo ia = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
        .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
    VkViewport vp = { 0, 0, (float)W, (float)H, 0.0f, 1.0f };
    VkRect2D sc = { { 0, 0 }, { W, H } };
    VkPipelineViewportStateCreateInfo vps = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
        .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
    VkPipelineRasterizationStateCreateInfo rs = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
        .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f };
    VkPipelineMultisampleStateCreateInfo ms = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
        .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
    VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
    VkPipelineColorBlendStateCreateInfo cb = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
        .attachmentCount = 1, .pAttachments = &cba };
    VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
    VkPipelineLayout pl;
    VK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
    VkGraphicsPipelineCreateInfo gpci = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
        .stageCount = 2, .pStages = stages,
        .pVertexInputState = &vi, .pInputAssemblyState = &ia,
        .pViewportState = &vps, .pRasterizationState = &rs,
        .pMultisampleState = &ms, .pColorBlendState = &cb,
        .layout = pl, .renderPass = rp, .subpass = 0 };
    VkPipeline pipe;
    VK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

    // ---- record ----
    VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                     .queueFamilyIndex = gq };
    VkCommandPool pool;
    VK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                         .commandPool = pool,
                                         .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                         .commandBufferCount = 1 };
    VkCommandBuffer cmd;
    VK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

    VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    VK(vkBeginCommandBuffer(cmd, &cbbi));

    VkClearValue clear;
    memcpy(clear.color.float32, CLEAR_RGBA, sizeof CLEAR_RGBA);
    VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                   .renderPass = rp, .framebuffer = fb,
                                   .renderArea = { { 0, 0 }, { W, H } },
                                   .clearValueCount = 1, .pClearValues = &clear };
    vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    vkCmdDraw(cmd, 3, 1, 0, 0);
    vkCmdEndRenderPass(cmd);

    VkBufferImageCopy region = {
        .bufferOffset = 0, .bufferRowLength = 0, .bufferImageHeight = 0,
        .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
        .imageOffset = { 0, 0, 0 }, .imageExtent = { W, H, 1 },
    };
    vkCmdCopyImageToBuffer(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buf, 1, &region);

    // Make the copy visible to a host read. Without this the mapped bytes are
    // undefined, which is exactly the kind of "works by luck" this probe exists
    // to rule out.
    VkMemoryBarrier hostBarrier = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                                    .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                                    .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                         0, 1, &hostBarrier, 0, NULL, 0, NULL);
    VK(vkEndCommandBuffer(cmd));

    // ---- submit and wait. This is the completion path. ----
    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    VK(vkCreateFence(dev, &fci, NULL, &fence));
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                        .commandBufferCount = 1, .pCommandBuffers = &cmd };
    VK(vkQueueSubmit(queue, 1, &si, fence));

    // A bounded wait, not UINT64_MAX. If the completion path is broken -- open
    // item 2, a semaphore mapped write-combine and polled as an uncached read --
    // the difference between a hang and a reported timeout is the whole value of
    // this run.
    VkResult wr = vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ULL * 1000 * 1000 * 1000);
    check("fence signalled", wr == VK_SUCCESS,
          wr == VK_TIMEOUT ? "TIMEOUT after 10s -- the GPU never reported completion" : "");
    if (wr != VK_SUCCESS) return 1;

    // ---- verify the pixels ----
    uint8_t *px = NULL;
    VK(vkMapMemory(dev, bmem, 0, bytes, 0, (void **)&px));

#define AT(x, y) (px + ((size_t)(y) * W + (x)) * 4)
    const uint8_t *centre = AT(W / 2, H / 2);
    const uint8_t *corner = AT(2, 2);
    char d[128];

    snprintf(d, sizeof d, "rgba=%u,%u,%u,%u", centre[0], centre[1], centre[2], centre[3]);
    check("centre is triangle (red)",
          centre[0] > 200 && centre[1] < 60 && centre[2] < 60, d);

    snprintf(d, sizeof d, "rgba=%u,%u,%u,%u", corner[0], corner[1], corner[2], corner[3]);
    check("corner is clear (blue)",
          corner[0] < 60 && corner[1] < 60 && corner[2] > 200, d);

    // A clear alone would pass neither of the above together, but count the
    // covered pixels too: it distinguishes "something rasterised" from "the
    // whole target got painted red".
    size_t red = 0, blue = 0, other = 0;
    for (size_t i = 0; i < (size_t)W * H; i++) {
        const uint8_t *p = px + i * 4;
        if (p[0] > 200 && p[1] < 60 && p[2] < 60) red++;
        else if (p[0] < 60 && p[1] < 60 && p[2] > 200) blue++;
        else other++;
    }
    double frac = (double)red / ((double)W * H);
    snprintf(d, sizeof d, "red=%zu blue=%zu other=%zu (%.1f%% covered)", red, blue, other, frac * 100.0);
    // The triangle spans 1.2 x 1.2 in NDC of a 2 x 2 clip area, so it covers
    // 0.5*1.2*1.2/4 = 18% of the target. Bracket it generously; the point is to
    // catch 0% and 100%, not to grade the rasteriser.
    check("triangle covers ~18%", frac > 0.10 && frac < 0.30, d);

    if (ppm_path) {
        FILE *f = fopen(ppm_path, "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", W, H);
            for (size_t i = 0; i < (size_t)W * H; i++) fwrite(px + i * 4, 1, 3, f);
            fclose(f);
            printf("GUEST: wrote %s\n", ppm_path);
        } else {
            printf("GUEST: could not write %s\n", ppm_path);
        }
    }
    vkUnmapMemory(dev, bmem);

    printf("GUEST: offscreen-draw %s (%d failed)\n", fails ? "FAILED" : "OK", fails);
    return fails ? 1 : 0;
}
