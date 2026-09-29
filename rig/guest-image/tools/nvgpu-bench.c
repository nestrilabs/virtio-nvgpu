// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-bench -- microbenchmarks for the costs a VM adds to a GPU program:
 * submission and completion, allocation and mapping, the CPU's reach into
 * GPU memory, copies both ways, and the kernel-ABI control path. The same
 * binary runs natively (rig/rig-native-run.sh) and in a guest, so the two
 * numbers differ only by what is between the program and the host driver.
 *
 *   nvgpu-bench <test>... | all | vk | gl | rm | wl
 *
 *   vk-init    vkCreateInstance + vkCreateDevice, cold and warm (ms)
 *   vk-submit  vkQueueSubmit + vkWaitForFences on an empty command buffer:
 *              latency (us, mean/p50/p99); submits per second, fenced once
 *   vk-draws   10,000 draws with a push constant each, recorded and
 *              submitted per frame, waited for (ms/frame): the driver's CPU
 *              cost per draw, which no VM should change
 *   vk-cost    frame time (ms) of one 1920x1080 triangle of dialled fragment
 *              cost, submitted and waited for per frame: GPU-bound at the
 *              top, the round trip alone at cost 0
 *   vk-alloc   vkAllocateMemory + vkFreeMemory (1 MiB, device-local), and
 *              allocate + vkMapMemory + touch + unmap + free for each
 *              host-visible type (us per op)
 *   vk-copy    vkCmdCopyBuffer between a staging buffer and device-local
 *              memory, 256 MiB, both ways (GB/s)
 *   vk-cpu     the CPU's bandwidth into mapped memory of each host-visible
 *              type: write (memset), write (memcpy), read (memcpy) (GB/s)
 *   gl-init    EGL (device platform) display, context, make current (ms)
 *   gl-xfer    glTexSubImage2D and glReadPixels of 4096x4096 RGBA8 (GB/s)
 *   gl-draws   10,000 glDrawArrays with a uniform each per frame (ms/frame);
 *              glClear + glFinish latency (us)
 *   rm-ctl     RM_CONTROL (GPU_GET_ATTACHED_IDS) on a client (us per call);
 *              open + close of /dev/nvidiactl (us)
 *   rm-map     RM_ALLOC video memory + RM_FREE (us); open + RM_MAP_MEMORY +
 *              mmap + touch + UPDATE_DEVICE_MAPPING_INFO (map_touch), then
 *              munmap + RM_UNMAP_MEMORY + close as well (map_unmap) (us)
 *   wl-shm     a wl_shm client committing full 1920x1080 frames as fast as
 *              the compositor releases them, for 5 s (frames/s, CPU)
 *   vk-stream  texture streaming: a frame of fresh staging allocations
 *              filled by the CPU, a copy submit and a render submit joined
 *              by a semaphore, two frames in flight (frame times, and what
 *              the CPU spent where); vk-stream-pool, the same from staging
 *              memory kept mapped. Only by name, for rig/rig-heavy.sh
 *
 * Output: one line per figure, "BENCH <test>.<figure> <value> <unit>", so a
 * harness can grep them; anything else is commentary. Exit status is the
 * number of tests that could not run.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>
#include <vulkan/vulkan.h>
#include <wayland-client.h>

#include "nvgpu-bench-spv.h"
#include "xdg-shell-client-protocol.h"

/* ───────── common ───────── */

static double now_s(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static double cpu_s(void) {
  struct rusage ru;
  getrusage(RUSAGE_SELF, &ru);
  return ru.ru_utime.tv_sec + ru.ru_stime.tv_sec +
         (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) * 1e-6;
}

static void bench(const char *test, const char *fig, double v, const char *unit) {
  printf("BENCH %s.%s %.4g %s\n", test, fig, v, unit);
  fflush(stdout);
}

static int cmp_d(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return x < y ? -1 : x > y;
}

/* mean, p50 and p99 of n samples (sorted in place), in `scale` units */
static void stats(const char *test, const char *fig, double *v, int n, double scale,
                  const char *unit) {
  double sum = 0;
  char name[96];

  qsort(v, n, sizeof(*v), cmp_d);
  for (int i = 0; i < n; i++)
    sum += v[i];
  snprintf(name, sizeof(name), "%s_mean", fig);
  bench(test, name, sum / n * scale, unit);
  snprintf(name, sizeof(name), "%s_p50", fig);
  bench(test, name, v[n / 2] * scale, unit);
  snprintf(name, sizeof(name), "%s_p99", fig);
  bench(test, name, v[(int)(n * 0.99)] * scale, unit);
}

#define TRY(what, x)                                                             \
  do {                                                                           \
    if (!(x)) {                                                                  \
      fprintf(stderr, "nvgpu-bench: %s failed (%s:%d)\n", what, __FILE__, __LINE__); \
      return -1;                                                                 \
    }                                                                            \
  } while (0)
#define VK(x) TRY(#x, (x) == VK_SUCCESS)

/* ───────── Vulkan ───────── */

static struct {
  VkInstance inst;
  VkPhysicalDevice pd;
  VkDevice dev;
  VkQueue q;
  uint32_t qfam;
  VkPhysicalDeviceMemoryProperties mp;
  VkCommandPool pool;
} vk;

static int vk_open(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .pApplicationName = "nvgpu-bench",
                           .apiVersion = VK_API_VERSION_1_3};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                              .pApplicationInfo = &app};
  VkPhysicalDevice pds[8];
  uint32_t n = 8, nq = 16;
  VkQueueFamilyProperties qf[16];

  VK(vkCreateInstance(&ici, NULL, &vk.inst));
  VK(vkEnumeratePhysicalDevices(vk.inst, &n, pds));
  vk.pd = VK_NULL_HANDLE;
  for (uint32_t i = 0; i < n; i++) {
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties(pds[i], &p);
    if (p.vendorID == 0x10de) {
      vk.pd = pds[i];
      break;
    }
  }
  TRY("an NVIDIA physical device", vk.pd != VK_NULL_HANDLE);
  vkGetPhysicalDeviceQueueFamilyProperties(vk.pd, &nq, qf);
  vk.qfam = UINT32_MAX;
  for (uint32_t i = 0; i < nq; i++)
    if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
      vk.qfam = i;
      break;
    }
  TRY("a graphics queue", vk.qfam != UINT32_MAX);
  float prio = 1.0f;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = vk.qfam,
                                 .queueCount = 1,
                                 .pQueuePriorities = &prio};
  VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .pNext = &f13,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qci};
  VK(vkCreateDevice(vk.pd, &dci, NULL, &vk.dev));
  vkGetDeviceQueue(vk.dev, vk.qfam, 0, &vk.q);
  vkGetPhysicalDeviceMemoryProperties(vk.pd, &vk.mp);
  VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                 .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                 .queueFamilyIndex = vk.qfam};
  VK(vkCreateCommandPool(vk.dev, &pci, NULL, &vk.pool));
  return 0;
}

static void vk_close(void) {
  if (vk.dev) {
    vkDeviceWaitIdle(vk.dev);
    vkDestroyCommandPool(vk.dev, vk.pool, NULL);
    vkDestroyDevice(vk.dev, NULL);
  }
  if (vk.inst)
    vkDestroyInstance(vk.inst, NULL);
  memset(&vk, 0, sizeof(vk));
}

static int vk_ready(void) { return vk.dev ? 0 : vk_open(); }

/* The first memory type with all of `want` and none of `avoid`, allowed by `bits`. */
static int vk_type(uint32_t bits, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
  for (uint32_t i = 0; i < vk.mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (vk.mp.memoryTypes[i].propertyFlags & want) == want &&
        !(vk.mp.memoryTypes[i].propertyFlags & avoid))
      return (int)i;
  return -1;
}

struct vkbuf {
  VkBuffer b;
  VkDeviceMemory m;
  void *p;
  VkDeviceSize size;
};

static int vk_buf(struct vkbuf *vb, VkDeviceSize size, VkMemoryPropertyFlags want,
                  VkMemoryPropertyFlags avoid, int map) {
  VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                            .size = size,
                            .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                                     VK_BUFFER_USAGE_TRANSFER_DST_BIT};
  VkMemoryRequirements mr;

  memset(vb, 0, sizeof(*vb));
  vb->size = size;
  VK(vkCreateBuffer(vk.dev, &bci, NULL, &vb->b));
  vkGetBufferMemoryRequirements(vk.dev, vb->b, &mr);
  int t = vk_type(mr.memoryTypeBits, want, avoid);
  if (t < 0) {
    vkDestroyBuffer(vk.dev, vb->b, NULL);
    vb->b = VK_NULL_HANDLE;
    return 1; /* no such type: not an error */
  }
  VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                              .allocationSize = mr.size,
                              .memoryTypeIndex = (uint32_t)t};
  VK(vkAllocateMemory(vk.dev, &mai, NULL, &vb->m));
  VK(vkBindBufferMemory(vk.dev, vb->b, vb->m, 0));
  if (map)
    VK(vkMapMemory(vk.dev, vb->m, 0, VK_WHOLE_SIZE, 0, &vb->p));
  return 0;
}

static void vk_buf_free(struct vkbuf *vb) {
  if (vb->p)
    vkUnmapMemory(vk.dev, vb->m);
  if (vb->b)
    vkDestroyBuffer(vk.dev, vb->b, NULL);
  if (vb->m)
    vkFreeMemory(vk.dev, vb->m, NULL);
  memset(vb, 0, sizeof(*vb));
}

static int vk_cb(VkCommandBuffer *cb) {
  VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                    .commandPool = vk.pool,
                                    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                    .commandBufferCount = 1};
  VK(vkAllocateCommandBuffers(vk.dev, &ai, cb));
  return 0;
}

static int vk_fence(VkFence *f) {
  VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
  VK(vkCreateFence(vk.dev, &fci, NULL, f));
  return 0;
}

static int vk_run(VkCommandBuffer cb, VkFence f) {
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                     .commandBufferCount = 1,
                     .pCommandBuffers = &cb};
  VK(vkQueueSubmit(vk.q, 1, &si, f));
  VK(vkWaitForFences(vk.dev, 1, &f, VK_TRUE, UINT64_MAX));
  VK(vkResetFences(vk.dev, 1, &f));
  return 0;
}

static int t_vk_init(void) {
  double warm = 0;
  const int reps = 5;

  vk_close();
  for (int i = 0; i <= reps; i++) {
    double t0 = now_s();
    if (vk_open())
      return -1;
    double t1 = now_s();
    vk_close();
    if (i == 0)
      bench("vk-init", "cold", (t1 - t0) * 1e3, "ms");
    else
      warm += t1 - t0;
  }
  bench("vk-init", "warm", warm / reps * 1e3, "ms");
  return 0;
}

static int t_vk_submit(void) {
  enum { WARM = 200, N = 3000, RATE = 20000 };
  static double lat[N];
  VkCommandBuffer cb;
  VkFence f;
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                 .flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT};

  if (vk_ready() || vk_cb(&cb) || vk_fence(&f))
    return -1;
  VK(vkBeginCommandBuffer(cb, &bi));
  VK(vkEndCommandBuffer(cb));
  for (int i = 0; i < WARM + N; i++) {
    double t0 = now_s();
    if (vk_run(cb, f))
      return -1;
    if (i >= WARM)
      lat[i - WARM] = now_s() - t0;
  }
  stats("vk-submit", "fenced_rt", lat, N, 1e6, "us");

  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                     .commandBufferCount = 1,
                     .pCommandBuffers = &cb};
  double t0 = now_s(), c0 = cpu_s();
  for (int i = 0; i < RATE; i++)
    VK(vkQueueSubmit(vk.q, 1, &si, VK_NULL_HANDLE));
  VK(vkQueueWaitIdle(vk.q));
  double dt = now_s() - t0;
  bench("vk-submit", "rate", RATE / dt, "submits/s");
  bench("vk-submit", "cpu_per_submit", (cpu_s() - c0) / RATE * 1e6, "us");
  vkDestroyFence(vk.dev, f, NULL);
  vkFreeCommandBuffers(vk.dev, vk.pool, 1, &cb);
  return 0;
}

/* A pipeline of two SPIR-V stages drawing into one RGBA8 attachment of w x h,
 * with a push-constant range of `pcsize` bytes for `pcstage`. */
static int vk_pipeline(const uint32_t *vcode, size_t vsize, const uint32_t *fcode, size_t fsize,
                       VkShaderStageFlags pcstage, uint32_t pcsize, uint32_t w, uint32_t h,
                       VkPipelineLayout *pl, VkPipeline *pipe) {
  const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
  VkShaderModule vs, fs;
  VkShaderModuleCreateInfo smi = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                  .codeSize = vsize,
                                  .pCode = vcode};
  VK(vkCreateShaderModule(vk.dev, &smi, NULL, &vs));
  smi.codeSize = fsize;
  smi.pCode = fcode;
  VK(vkCreateShaderModule(vk.dev, &smi, NULL, &fs));
  VkPushConstantRange pcr = {pcstage, 0, pcsize};
  VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                    .pushConstantRangeCount = 1,
                                    .pPushConstantRanges = &pcr};
  VK(vkCreatePipelineLayout(vk.dev, &pli, NULL, pl));
  VkPipelineShaderStageCreateInfo st[2] = {
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_VERTEX_BIT,
       .module = vs,
       .pName = "main"},
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
       .module = fs,
       .pName = "main"}};
  VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
  VkViewport vp = {0, 0, (float)w, (float)h, 0, 1};
  VkRect2D sc = {{0, 0}, {w, h}};
  VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc};
  VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1};
  VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
  VkPipelineColorBlendStateCreateInfo cbs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &cba};
  VkPipelineRenderingCreateInfo pri = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
                                       .colorAttachmentCount = 1,
                                       .pColorAttachmentFormats = &fmt};
  VkGraphicsPipelineCreateInfo gpi = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
                                      .pNext = &pri,
                                      .stageCount = 2,
                                      .pStages = st,
                                      .pVertexInputState = &vin,
                                      .pInputAssemblyState = &ia,
                                      .pViewportState = &vps,
                                      .pRasterizationState = &rs,
                                      .pMultisampleState = &ms,
                                      .pColorBlendState = &cbs,
                                      .layout = *pl};
  VK(vkCreateGraphicsPipelines(vk.dev, VK_NULL_HANDLE, 1, &gpi, NULL, pipe));
  vkDestroyShaderModule(vk.dev, vs, NULL);
  vkDestroyShaderModule(vk.dev, fs, NULL);
  return 0;
}

/* An RGBA8 colour attachment of w x h in device-local memory. */
static int vk_target(uint32_t w, uint32_t h, VkImage *img, VkDeviceMemory *mem,
                     VkImageView *view) {
  const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
  VkMemoryRequirements mr;
  VkImageCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                          .imageType = VK_IMAGE_TYPE_2D,
                          .format = fmt,
                          .extent = {w, h, 1},
                          .mipLevels = 1,
                          .arrayLayers = 1,
                          .samples = VK_SAMPLE_COUNT_1_BIT,
                          .tiling = VK_IMAGE_TILING_OPTIMAL,
                          .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT};
  VK(vkCreateImage(vk.dev, &ii, NULL, img));
  vkGetImageMemoryRequirements(vk.dev, *img, &mr);
  VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = (uint32_t)vk_type(mr.memoryTypeBits,
                                           VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0)};
  VK(vkAllocateMemory(vk.dev, &mai, NULL, mem));
  VK(vkBindImageMemory(vk.dev, *img, *mem, 0));
  VkImageViewCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                              .image = *img,
                              .viewType = VK_IMAGE_VIEW_TYPE_2D,
                              .format = fmt,
                              .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  VK(vkCreateImageView(vk.dev, &vi, NULL, view));
  return 0;
}

/* Begin rendering into `img`, its layout made an attachment's from undefined. */
static void vk_begin_render(VkCommandBuffer cb, VkImage img, VkImageView view, uint32_t w,
                            uint32_t h) {
  VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                            .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                            .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                            .image = img,
                            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                       VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &b);
  VkRenderingAttachmentInfo ca = {.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
                                  .imageView = view,
                                  .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                  .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                  .storeOp = VK_ATTACHMENT_STORE_OP_STORE};
  VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                        .renderArea = {{0, 0}, {w, h}},
                        .layerCount = 1,
                        .colorAttachmentCount = 1,
                        .pColorAttachments = &ca};
  vkCmdBeginRendering(cb, &ri);
}

static int t_vk_draws(void) {
  enum { W = 64, DRAWS = 10000, FRAMES = 60, WARM = 5 };
  VkImage img;
  VkDeviceMemory mem;
  VkImageView view;
  VkPipelineLayout pl;
  VkPipeline pipe;
  VkCommandBuffer cb;
  VkFence f;

  if (vk_ready() || vk_cb(&cb) || vk_fence(&f) || vk_target(W, W, &img, &mem, &view) ||
      vk_pipeline(bench_vs, sizeof(bench_vs), bench_fs, sizeof(bench_fs),
                  VK_SHADER_STAGE_VERTEX_BIT, 16, W, W, &pl, &pipe))
    return -1;
  double rec = 0, total = 0;
  for (int fr = 0; fr < WARM + FRAMES; fr++) {
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                   .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    double t0 = now_s();
    VK(vkResetCommandBuffer(cb, 0));
    VK(vkBeginCommandBuffer(cb, &bi));
    vk_begin_render(cb, img, view, W, W);
    vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    for (int d = 0; d < DRAWS; d++) {
      float pc[4] = {(d % 100) * 0.02f - 1.0f, (d / 100 % 100) * 0.02f - 1.0f, 0.01f, 0};
      vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(pc), pc);
      vkCmdDraw(cb, 3, 1, 0, 0);
    }
    vkCmdEndRendering(cb);
    VK(vkEndCommandBuffer(cb));
    double t1 = now_s();
    if (vk_run(cb, f))
      return -1;
    double t2 = now_s();
    if (fr >= WARM) {
      rec += t1 - t0;
      total += t2 - t0;
    }
  }
  bench("vk-draws", "frame", total / FRAMES * 1e3, "ms");
  bench("vk-draws", "record", rec / FRAMES * 1e3, "ms");
  bench("vk-draws", "draws_per_s", DRAWS * FRAMES / total, "draws/s");
  vkDestroyPipeline(vk.dev, pipe, NULL);
  vkDestroyPipelineLayout(vk.dev, pl, NULL);
  vkDestroyImageView(vk.dev, view, NULL);
  vkDestroyImage(vk.dev, img, NULL);
  vkFreeMemory(vk.dev, mem, NULL);
  vkDestroyFence(vk.dev, f, NULL);
  vkFreeCommandBuffers(vk.dev, vk.pool, 1, &cb);
  return 0;
}

/*
 * The frame time against the GPU work in it, nesprobe's way: one 1920x1080
 * triangle whose fragments do `cost` iterations of dependent math, recorded
 * once and submitted and waited for per frame, 3 s per cost after a 1.5 s
 * warm-up that the GPU's clock ramp needs. A cost the host renders in 2 ms
 * should cost a guest the same; a cost of 0 is nothing but the round trip.
 */
static int t_vk_cost(void) {
  enum { W = 1920, H = 1080 };
  static const uint32_t costs[] = {0, 64, 512, 2048, 8192, 32768};
  VkImage img;
  VkDeviceMemory mem;
  VkImageView view;
  VkPipelineLayout pl;
  VkPipeline pipe;
  VkCommandBuffer cb;
  VkFence f;
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};

  if (vk_ready() || vk_cb(&cb) || vk_fence(&f) || vk_target(W, H, &img, &mem, &view) ||
      vk_pipeline(bench_full_vs, sizeof(bench_full_vs), bench_cost_fs, sizeof(bench_cost_fs),
                  VK_SHADER_STAGE_FRAGMENT_BIT, 4, W, H, &pl, &pipe))
    return -1;
  for (unsigned c = 0; c < sizeof(costs) / sizeof(costs[0]); c++) {
    static double ft[100000];
    char fig[32];
    int n = 0;

    VK(vkResetCommandBuffer(cb, 0));
    VK(vkBeginCommandBuffer(cb, &bi));
    vk_begin_render(cb, img, view, W, H);
    vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    vkCmdPushConstants(cb, pl, VK_SHADER_STAGE_FRAGMENT_BIT, 0, 4, &costs[c]);
    vkCmdDraw(cb, 3, 1, 0, 0);
    vkCmdEndRendering(cb);
    VK(vkEndCommandBuffer(cb));
    double start = now_s(), warm = start + 1.5, end = warm + 3.0, t = start;
    while (t < end && n < 100000) {
      double t0 = t;
      if (vk_run(cb, f))
        return -1;
      t = now_s();
      if (t0 >= warm)
        ft[n++] = t - t0;
    }
    snprintf(fig, sizeof(fig), "cost%u", costs[c]);
    stats("vk-cost", fig, ft, n, 1e3, "ms");
  }
  vkDestroyPipeline(vk.dev, pipe, NULL);
  vkDestroyPipelineLayout(vk.dev, pl, NULL);
  vkDestroyImageView(vk.dev, view, NULL);
  vkDestroyImage(vk.dev, img, NULL);
  vkFreeMemory(vk.dev, mem, NULL);
  vkDestroyFence(vk.dev, f, NULL);
  vkFreeCommandBuffers(vk.dev, vk.pool, 1, &cb);
  return 0;
}

/*
 * Texture streaming, a frame at a time, as an engine does it without a
 * sub-allocator: each frame allocates NVGPU_BENCH_STREAM_ALLOCS (default 4)
 * fresh host-visible staging buffers of 256 KiB to 2 MiB, maps and fills
 * them from the CPU, copies them into device-local memory on one submit,
 * and renders on a second that waits for the first through a binary
 * semaphore, with a fence per frame and two frames in flight: the oldest
 * frame's fence is waited for (a wait that may sleep) and its staging
 * buffers freed before the next frame is built. The GPU work is one
 * 1920x1080 triangle of dialled cost (vk-cost's 1024, about 0.6 ms).
 * vk-stream-pool is the same frame with each slot's staging memory
 * allocated once and kept mapped: the difference is what allocating,
 * mapping and first-touching memory costs a frame.
 * 2 s of warm-up, then 10 s measured; every frame's time (ms) goes to
 * $NVGPU_BENCH_FRAMES (default /tmp/nvgpu-bench-stream.txt).
 */
static int vk_stream(int pool) {
  enum { W = 1920, H = 1080, SLOTS = 2, MAXA = 32 };
  static const VkDeviceSize sizes[] = {256u << 10, 512u << 10, 1u << 20, 2u << 20};
  static double ft[200000];
  const char *test = pool ? "vk-stream-pool" : "vk-stream";
  const char *e = getenv("NVGPU_BENCH_STREAM_ALLOCS");
  int na = e ? atoi(e) : 4;
  if (na < 1 || na > MAXA)
    na = 4;
  const uint32_t cost = 1024;
  struct vkbuf stage[SLOTS][MAXA], dst;
  VkCommandBuffer xcb[SLOTS], rcb[SLOTS];
  VkFence fence[SLOTS];
  VkSemaphore sem[SLOTS];
  int busy[SLOTS] = {0};
  VkImage img;
  VkDeviceMemory mem;
  VkImageView view;
  VkPipelineLayout pl;
  VkPipeline pipe;
  VkDeviceSize total = 0;

  if (vk_ready() || vk_target(W, H, &img, &mem, &view) ||
      vk_pipeline(bench_full_vs, sizeof(bench_full_vs), bench_cost_fs, sizeof(bench_cost_fs),
                  VK_SHADER_STAGE_FRAGMENT_BIT, 4, W, H, &pl, &pipe))
    return -1;
  for (int a = 0; a < na; a++)
    total += sizes[a % 4];
  if (vk_buf(&dst, total, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0, 0))
    return -1;
  memset(stage, 0, sizeof(stage));
  for (int s = 0; s < SLOTS; s++) {
    VkSemaphoreCreateInfo sci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    if (vk_cb(&xcb[s]) || vk_cb(&rcb[s]) || vk_fence(&fence[s]))
      return -1;
    VK(vkCreateSemaphore(vk.dev, &sci, NULL, &sem[s]));
    if (pool)
      for (int a = 0; a < na; a++)
        if (vk_buf(&stage[s][a], sizes[a % 4], VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                  VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                   VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 1))
          return -1;
  }
  /* The render pass is the same every frame. */
  for (int s = 0; s < SLOTS; s++) {
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    VK(vkBeginCommandBuffer(rcb[s], &bi));
    vk_begin_render(rcb[s], img, view, W, H);
    vkCmdBindPipeline(rcb[s], VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    vkCmdPushConstants(rcb[s], pl, VK_SHADER_STAGE_FRAGMENT_BIT, 0, 4, &cost);
    vkCmdDraw(rcb[s], 3, 1, 0, 0);
    vkCmdEndRendering(rcb[s]);
    VK(vkEndCommandBuffer(rcb[s]));
  }
  double start = now_s(), warm = start + 2.0, end = warm + 10.0, t = start, prev = start;
  double t_alloc = 0, t_wait = 0, t_fill = 0;
  int n = 0, fr = 0;
  while (t < end && n < 200000) {
    int s = fr % SLOTS;
    double a0 = now_s();
    if (busy[s]) {
      VK(vkWaitForFences(vk.dev, 1, &fence[s], VK_TRUE, UINT64_MAX));
      VK(vkResetFences(vk.dev, 1, &fence[s]));
      busy[s] = 0;
    }
    double a1 = now_s();
    if (!pool)
      for (int a = 0; a < na; a++) {
        vk_buf_free(&stage[s][a]);
        if (vk_buf(&stage[s][a], sizes[a % 4],
                   VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                   VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 1))
          return -1;
      }
    double a2 = now_s();
    for (int a = 0; a < na; a++)
      memset(stage[s][a].p, (fr + a) & 0xff, sizes[a % 4]);
    double a3 = now_s();
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                   .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    VK(vkResetCommandBuffer(xcb[s], 0));
    VK(vkBeginCommandBuffer(xcb[s], &bi));
    VkDeviceSize off = 0;
    for (int a = 0; a < na; a++) {
      VkBufferCopy c = {0, off, sizes[a % 4]};
      vkCmdCopyBuffer(xcb[s], stage[s][a].b, dst.b, 1, &c);
      off += sizes[a % 4];
    }
    VK(vkEndCommandBuffer(xcb[s]));
    VkPipelineStageFlags ws = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    VkSubmitInfo si[2] = {{.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                           .commandBufferCount = 1,
                           .pCommandBuffers = &xcb[s],
                           .signalSemaphoreCount = 1,
                           .pSignalSemaphores = &sem[s]},
                          {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                           .waitSemaphoreCount = 1,
                           .pWaitSemaphores = &sem[s],
                           .pWaitDstStageMask = &ws,
                           .commandBufferCount = 1,
                           .pCommandBuffers = &rcb[s]}};
    VK(vkQueueSubmit(vk.q, 1, &si[0], VK_NULL_HANDLE));
    VK(vkQueueSubmit(vk.q, 1, &si[1], fence[s]));
    busy[s] = 1;
    fr++;
    t = now_s();
    if (prev >= warm) {
      ft[n++] = t - prev;
      t_wait += a1 - a0;
      t_alloc += a2 - a1;
      t_fill += a3 - a2;
    }
    prev = t;
  }
  VK(vkDeviceWaitIdle(vk.dev));
  const char *path = getenv("NVGPU_BENCH_FRAMES");
  FILE *fo = fopen(path ? path : "/tmp/nvgpu-bench-stream.txt", "w");
  if (fo) {
    for (int i = 0; i < n; i++)
      fprintf(fo, "%.4f\n", ft[i] * 1e3);
    fclose(fo);
  }
  if (n) {
    bench(test, "fps", n / (t - warm), "frames/s");
    bench(test, "wait_per_frame", t_wait / n * 1e3, "ms");
    bench(test, "alloc_map_per_frame", t_alloc / n * 1e3, "ms");
    bench(test, "fill_per_frame", t_fill / n * 1e3, "ms");
    bench(test, "fill_rate", (double)total * n / t_fill / 1e9, "GB/s");
    stats(test, "frame", ft, n, 1e3, "ms");
  }
  for (int s = 0; s < SLOTS; s++) {
    for (int a = 0; a < na; a++)
      vk_buf_free(&stage[s][a]);
    vkDestroySemaphore(vk.dev, sem[s], NULL);
    vkDestroyFence(vk.dev, fence[s], NULL);
    vkFreeCommandBuffers(vk.dev, vk.pool, 1, &xcb[s]);
    vkFreeCommandBuffers(vk.dev, vk.pool, 1, &rcb[s]);
  }
  vk_buf_free(&dst);
  vkDestroyPipeline(vk.dev, pipe, NULL);
  vkDestroyPipelineLayout(vk.dev, pl, NULL);
  vkDestroyImageView(vk.dev, view, NULL);
  vkDestroyImage(vk.dev, img, NULL);
  vkFreeMemory(vk.dev, mem, NULL);
  return 0;
}

static int t_vk_stream(void) { return vk_stream(0); }
static int t_vk_stream_pool(void) { return vk_stream(1); }

static const struct {
  const char *name;
  VkMemoryPropertyFlags want, avoid;
} vk_kinds[] = {
    {"devlocal_hostvis", VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT,
     0},
    {"sys_coherent",
     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | VK_MEMORY_PROPERTY_HOST_CACHED_BIT},
    {"sys_cached", VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_CACHED_BIT,
     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT},
};

static int t_vk_alloc(void) {
  enum { N = 500, SZ = 1 << 20 };
  static double lat[N];
  struct vkbuf b;

  if (vk_ready())
    return -1;
  for (int i = 0; i < N; i++) {
    double t0 = now_s();
    if (vk_buf(&b, SZ, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0, 0) != 0)
      return -1;
    vk_buf_free(&b);
    lat[i] = now_s() - t0;
  }
  stats("vk-alloc", "devlocal_1m", lat, N, 1e6, "us");
  for (unsigned k = 0; k < sizeof(vk_kinds) / sizeof(vk_kinds[0]); k++) {
    char fig[64];
    int r = 0, i;
    for (i = 0; i < N; i++) {
      double t0 = now_s();
      r = vk_buf(&b, SZ, vk_kinds[k].want, vk_kinds[k].avoid, 1);
      if (r)
        break;
      ((volatile char *)b.p)[0] = 1;
      ((volatile char *)b.p)[SZ - 1] = 1;
      vk_buf_free(&b);
      lat[i] = now_s() - t0;
    }
    if (r > 0)
      continue;
    if (r < 0)
      return -1;
    snprintf(fig, sizeof(fig), "map_%s_1m", vk_kinds[k].name);
    stats("vk-alloc", fig, lat, N, 1e6, "us");
  }
  return 0;
}

static int t_vk_copy(void) {
  const VkDeviceSize SZ = 256u << 20;
  enum { REPS = 8 };
  struct vkbuf st, dl;
  VkCommandBuffer cb;
  VkFence f;
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};

  if (vk_ready() || vk_cb(&cb) || vk_fence(&f))
    return -1;
  if (vk_buf(&st, SZ, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_CACHED_BIT,
             VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 1) ||
      vk_buf(&dl, SZ, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0, 0))
    return -1;
  memset(st.p, 0x5a, SZ);
  for (int dir = 0; dir < 2; dir++) {
    VkBufferCopy c = {0, 0, SZ};
    VK(vkBeginCommandBuffer(cb, &bi));
    if (dir == 0)
      vkCmdCopyBuffer(cb, st.b, dl.b, 1, &c);
    else
      vkCmdCopyBuffer(cb, dl.b, st.b, 1, &c);
    VK(vkEndCommandBuffer(cb));
    if (vk_run(cb, f))
      return -1;
    double t0 = now_s();
    for (int r = 0; r < REPS; r++)
      if (vk_run(cb, f))
        return -1;
    double dt = now_s() - t0;
    bench("vk-copy", dir ? "readback" : "upload", (double)SZ * REPS / dt / 1e9, "GB/s");
    VK(vkResetCommandBuffer(cb, 0));
  }
  vk_buf_free(&st);
  vk_buf_free(&dl);
  vkDestroyFence(vk.dev, f, NULL);
  vkFreeCommandBuffers(vk.dev, vk.pool, 1, &cb);
  return 0;
}

/* Bandwidth of the CPU into `p`: memset, memcpy in, memcpy out. Reads of
 * uncached memory are slow enough natively that `rsz` bounds them. */
static void cpu_bw(const char *test, const char *kind, void *p, size_t sz, size_t rsz) {
  char fig[80];
  char *src = aligned_alloc(4096, sz);
  const int reps = 4;

  memset(src, 0x33, sz);
  /* The first touch of fresh mapping: in a guest, every page a fault of
   * the second-level page tables as well. */
  double t0 = now_s();
  memset(p, 0, sz);
  snprintf(fig, sizeof(fig), "%s_first_touch", kind);
  bench(test, fig, (double)sz / (now_s() - t0) / 1e9, "GB/s");
  t0 = now_s();
  for (int r = 0; r < reps; r++)
    memset(p, r, sz);
  double t1 = now_s();
  snprintf(fig, sizeof(fig), "%s_memset", kind);
  bench(test, fig, (double)sz * reps / (t1 - t0) / 1e9, "GB/s");
  t0 = now_s();
  for (int r = 0; r < reps; r++)
    memcpy(p, src, sz);
  t1 = now_s();
  snprintf(fig, sizeof(fig), "%s_write", kind);
  bench(test, fig, (double)sz * reps / (t1 - t0) / 1e9, "GB/s");
  t0 = now_s();
  memcpy(src, p, rsz);
  t1 = now_s();
  snprintf(fig, sizeof(fig), "%s_read", kind);
  bench(test, fig, (double)rsz / (t1 - t0) / 1e9, "GB/s");
  free(src);
}

static int t_vk_cpu(void) {
  const VkDeviceSize SZ = 64u << 20;
  struct vkbuf b;

  if (vk_ready())
    return -1;
  for (unsigned k = 0; k < sizeof(vk_kinds) / sizeof(vk_kinds[0]); k++) {
    int r = vk_buf(&b, SZ, vk_kinds[k].want, vk_kinds[k].avoid, 1);
    if (r > 0)
      continue;
    if (r < 0)
      return -1;
    /* Uncached reads are slow natively too: 4 MiB of them is plenty. */
    cpu_bw("vk-cpu", vk_kinds[k].name, b.p, SZ, k == 2 ? SZ : (4u << 20));
    vk_buf_free(&b);
  }
  /* The baseline: ordinary memory of the process. */
  void *m = aligned_alloc(4096, SZ);
  cpu_bw("vk-cpu", "malloc", m, SZ, SZ);
  free(m);
  return 0;
}

/* ───────── GL (EGL device platform, no window) ───────── */

static struct {
  EGLDisplay dpy;
  EGLContext ctx;
} gl;

static int gl_open(void) {
  PFNEGLQUERYDEVICESEXTPROC qd = (void *)eglGetProcAddress("eglQueryDevicesEXT");
  PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
  EGLDeviceEXT devs[8];
  EGLint n = 0, maj, min, nc;
  EGLConfig cfg;
  const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                       EGL_OPENGL_ES3_BIT, EGL_NONE};
  const EGLint cx[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE};

  TRY("eglQueryDevicesEXT", qd && gpd && qd(8, devs, &n) && n > 0);
  gl.dpy = gpd(EGL_PLATFORM_DEVICE_EXT, devs[0], NULL);
  TRY("eglInitialize", gl.dpy != EGL_NO_DISPLAY && eglInitialize(gl.dpy, &maj, &min));
  TRY("eglBindAPI", eglBindAPI(EGL_OPENGL_ES_API));
  TRY("eglChooseConfig", eglChooseConfig(gl.dpy, ca, &cfg, 1, &nc) && nc == 1);
  gl.ctx = eglCreateContext(gl.dpy, cfg, EGL_NO_CONTEXT, cx);
  TRY("eglCreateContext", gl.ctx != EGL_NO_CONTEXT);
  TRY("eglMakeCurrent", eglMakeCurrent(gl.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, gl.ctx));
  return 0;
}

static void gl_close(void) {
  if (gl.dpy != EGL_NO_DISPLAY && gl.dpy) {
    eglMakeCurrent(gl.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    if (gl.ctx)
      eglDestroyContext(gl.dpy, gl.ctx);
    eglTerminate(gl.dpy);
  }
  memset(&gl, 0, sizeof(gl));
}

static int gl_ready(void) { return gl.ctx ? 0 : gl_open(); }

static int t_gl_init(void) {
  double warm = 0;
  const int reps = 5;

  gl_close();
  for (int i = 0; i <= reps; i++) {
    double t0 = now_s();
    if (gl_open())
      return -1;
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    double t1 = now_s();
    gl_close();
    if (i == 0)
      bench("gl-init", "cold", (t1 - t0) * 1e3, "ms");
    else
      warm += t1 - t0;
  }
  bench("gl-init", "warm", warm / reps * 1e3, "ms");
  return 0;
}

static GLuint gl_fbo(int w, GLuint *tex) {
  GLuint fbo;
  glGenTextures(1, tex);
  glBindTexture(GL_TEXTURE_2D, *tex);
  glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, w, w);
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, *tex, 0);
  return fbo;
}

static int t_gl_xfer(void) {
  const int W = 4096, REPS = 8;
  const size_t sz = (size_t)W * W * 4;
  GLuint tex, fbo;
  char *buf = aligned_alloc(4096, sz);

  if (gl_ready())
    return -1;
  memset(buf, 0x44, sz);
  fbo = gl_fbo(W, &tex);
  TRY("framebuffer", glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
  glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, W, GL_RGBA, GL_UNSIGNED_BYTE, buf);
  glFinish();
  double t0 = now_s();
  for (int r = 0; r < REPS; r++)
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, W, GL_RGBA, GL_UNSIGNED_BYTE, buf);
  glFinish();
  bench("gl-xfer", "teximage", (double)sz * REPS / (now_s() - t0) / 1e9, "GB/s");
  glReadPixels(0, 0, W, W, GL_RGBA, GL_UNSIGNED_BYTE, buf);
  t0 = now_s();
  for (int r = 0; r < REPS; r++)
    glReadPixels(0, 0, W, W, GL_RGBA, GL_UNSIGNED_BYTE, buf);
  bench("gl-xfer", "readpixels", (double)sz * REPS / (now_s() - t0) / 1e9, "GB/s");
  TRY("no GL error", glGetError() == GL_NO_ERROR);
  glDeleteFramebuffers(1, &fbo);
  glDeleteTextures(1, &tex);
  free(buf);
  return 0;
}

static const char *gl_vs = "#version 300 es\n"
                           "uniform vec4 off;\n"
                           "void main() {\n"
                           "  vec2 p = vec2(gl_VertexID & 1, gl_VertexID >> 1);\n"
                           "  gl_Position = vec4(off.xy + p * off.z, 0.0, 1.0);\n"
                           "}\n";
static const char *gl_fs = "#version 300 es\n"
                           "precision mediump float;\n"
                           "out vec4 c;\n"
                           "void main() { c = vec4(1.0, 0.5, 0.25, 1.0); }\n";

static GLuint gl_prog(void) {
  GLuint p = glCreateProgram();
  GLuint s[2] = {glCreateShader(GL_VERTEX_SHADER), glCreateShader(GL_FRAGMENT_SHADER)};
  glShaderSource(s[0], 1, &gl_vs, NULL);
  glShaderSource(s[1], 1, &gl_fs, NULL);
  for (int i = 0; i < 2; i++) {
    glCompileShader(s[i]);
    glAttachShader(p, s[i]);
  }
  glLinkProgram(p);
  GLint ok = 0;
  glGetProgramiv(p, GL_LINK_STATUS, &ok);
  return ok ? p : 0;
}

static int t_gl_draws(void) {
  enum { W = 64, DRAWS = 10000, FRAMES = 60, WARM = 5, N = 3000 };
  static double lat[N];
  GLuint tex, fbo, vao, prog;

  if (gl_ready())
    return -1;
  fbo = gl_fbo(W, &tex);
  prog = gl_prog();
  TRY("GL program", prog);
  glUseProgram(prog);
  GLint off = glGetUniformLocation(prog, "off");
  glGenVertexArrays(1, &vao);
  glBindVertexArray(vao);
  glViewport(0, 0, W, W);
  double total = 0;
  for (int fr = 0; fr < WARM + FRAMES; fr++) {
    double t0 = now_s();
    glClear(GL_COLOR_BUFFER_BIT);
    for (int d = 0; d < DRAWS; d++) {
      glUniform4f(off, (d % 100) * 0.02f - 1.0f, (d / 100 % 100) * 0.02f - 1.0f, 0.01f, 0);
      glDrawArrays(GL_TRIANGLES, 0, 3);
    }
    glFinish();
    if (fr >= WARM)
      total += now_s() - t0;
  }
  bench("gl-draws", "frame", total / FRAMES * 1e3, "ms");
  bench("gl-draws", "draws_per_s", DRAWS * FRAMES / total, "draws/s");
  for (int i = 0; i < 200 + N; i++) {
    double t0 = now_s();
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    if (i >= 200)
      lat[i - 200] = now_s() - t0;
  }
  stats("gl-draws", "finish_rt", lat, N, 1e6, "us");
  TRY("no GL error", glGetError() == GL_NO_ERROR);
  glDeleteVertexArrays(1, &vao);
  glDeleteProgram(prog);
  glDeleteFramebuffers(1, &fbo);
  glDeleteTextures(1, &tex);
  return 0;
}

/* ───────── RM, raw (nvos.h layouts, x86-64) ───────── */

#define NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, 'F', (nr), (size))
#define NV_ESC_CARD_INFO 200
#define NV_ESC_REGISTER_FD 201
#define NV_ESC_SYS_PARAMS 214
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B
#define NV_ESC_RM_MAP_MEMORY 0x4E
#define NV_ESC_RM_UNMAP_MEMORY 0x4F
#define NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO 0x5E

struct nvos64 {
  uint32_t hRoot, hObjectParent, hObjectNew, hClass;
  uint64_t pAllocParms, pRightsRequested;
  uint32_t paramsSize, flags, status, _pad;
};
struct nvos00 {
  uint32_t hRoot, hObjectParent, hObjectOld, status;
};
struct nvos54 {
  uint32_t hClient, hObject, cmd, flags;
  uint64_t params;
  uint32_t paramsSize, status;
};
struct nvos33 {
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t offset, length, pLinearAddress;
  uint32_t status, flags;
  int32_t fd, _pad2;
};
struct nvos34 {
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t pLinearAddress;
  uint32_t status, flags;
};
struct nvos56 {
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t pOldCpuAddress, pNewCpuAddress;
  uint32_t status, _pad2;
};
struct mem_alloc {
  uint32_t owner, type, flags, width, height;
  int32_t pitch;
  uint32_t attr, attr2, format, comprCovg, zcullCovg, _pad;
  uint64_t rangeLo, rangeHi, size, alignment, offset, limit, address;
  uint32_t ctagOffset, hVASpace, internalflags, tag;
  int32_t numaNode, _pad2;
};

static struct {
  int ctl;
  uint32_t client, device, subdevice;
} rm = {.ctl = -1};

static uint32_t rm_alloc(uint32_t parent, uint32_t handle, uint32_t cls, void *params,
                         uint32_t size) {
  struct nvos64 p = {rm.client, parent, handle, cls, (uintptr_t)params, 0, size, 0, 0, 0};
  if (ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(p)), &p) < 0 || p.status)
    return 0;
  return p.hObjectNew;
}

static int rm_free(uint32_t parent, uint32_t h) {
  struct nvos00 p = {rm.client, parent, h, 0};
  return ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_FREE, sizeof(p)), &p) < 0 || p.status ? -1 : 0;
}

static int rm_open_gpu(void) {
  int gpu = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
  int32_t reg = rm.ctl;
  if (gpu < 0 || ioctl(gpu, NV_IOWR(NV_ESC_REGISTER_FD, sizeof(reg)), &reg) < 0) {
    if (gpu >= 0)
      close(gpu);
    return -1;
  }
  return gpu;
}

static int rm_ready(void) {
  static uint8_t sys[8], card[2304];

  if (rm.ctl >= 0)
    return 0;
  rm.ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  TRY("open /dev/nvidiactl", rm.ctl >= 0);
  ioctl(rm.ctl, NV_IOWR(NV_ESC_SYS_PARAMS, sizeof(sys)), sys);
  ioctl(rm.ctl, NV_IOWR(NV_ESC_CARD_INFO, sizeof(card)), card);
  int gpu = rm_open_gpu();
  TRY("open /dev/nvidia0", gpu >= 0);
  struct nvos64 p = {0, 0, 0, 0x41, 0, 0, 0, 0, 0, 0};
  TRY("root client", ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(p)), &p) == 0 && !p.status);
  rm.client = p.hObjectNew;
  uint8_t devp[56] = {0};
  memcpy(devp + 4, &rm.client, 4);
  rm.device = rm_alloc(rm.client, 0x5c000001, 0x80, devp, 0);
  uint32_t sub_id = 0;
  rm.subdevice = rm.device ? rm_alloc(rm.device, 0x5c000002, 0x2080, &sub_id, 0) : 0;
  TRY("device and subdevice", rm.subdevice);
  return 0;
}

static int t_rm_ctl(void) {
  enum { N = 20000 };
  static double lat[N];
  uint32_t ids[32];

  if (rm_ready())
    return -1;
  for (int i = 0; i < N + 500; i++) {
    struct nvos54 c = {rm.client, rm.client, 0x00000201, 0, (uintptr_t)ids, sizeof(ids), 0};
    double t0 = now_s();
    TRY("RM_CONTROL GPU_GET_ATTACHED_IDS",
        ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_CONTROL, sizeof(c)), &c) == 0 && !c.status);
    if (i >= 500)
      lat[i - 500] = now_s() - t0;
  }
  stats("rm-ctl", "control", lat, N, 1e6, "us");
  for (int i = 0; i < 2000; i++) {
    double t0 = now_s();
    int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    TRY("open", fd >= 0);
    close(fd);
    lat[i] = now_s() - t0;
  }
  stats("rm-ctl", "open_close", lat, 2000, 1e6, "us");
  return 0;
}

static int t_rm_map(void) {
  enum { N = 400 };
  static double lat[N], lat2[N];
  const uint64_t len = 2u << 20;

  if (rm_ready())
    return -1;
  for (int i = 0; i < N; i++) {
    struct mem_alloc m = {0};
    m.owner = 0x6e766d63;
    m.attr = (1u << 23); /* LOCATION_VIDMEM, PAGE_SIZE_4KB */
    m.size = len;
    double t0 = now_s();
    uint32_t h = rm_alloc(rm.device, 0xbe000000u + i, 0x40, &m, sizeof(m));
    TRY("RM_ALLOC video memory", h);
    TRY("RM_FREE", rm_free(rm.device, h) == 0);
    lat[i] = now_s() - t0;
  }
  stats("rm-map", "alloc_free_2m", lat, N, 1e6, "us");

  struct mem_alloc m = {0};
  m.owner = 0x6e766d63;
  m.attr = (1u << 23);
  m.size = len;
  uint32_t mem = rm_alloc(rm.device, 0xbf000000u, 0x40, &m, sizeof(m));
  TRY("RM_ALLOC video memory", mem);
  /* A descriptor per mapping, as the user-mode driver opens one: RM arms
   * one mapping per file. */
  for (int i = 0; i < N; i++) {
    double t0 = now_s();
    int mfd = rm_open_gpu();
    TRY("map descriptor", mfd >= 0);
    struct nvos33 map = {rm.client, rm.subdevice, mem, 0, 0, len, 0, 0, 0x03080002, mfd, 0};
    TRY("RM_MAP_MEMORY",
        ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_MAP_MEMORY, sizeof(map)), &map) == 0 && !map.status);
    void *va = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    TRY("mmap", va != MAP_FAILED);
    ((volatile uint32_t *)va)[0] = i;
    struct nvos56 upd = {rm.client, rm.subdevice, mem, 0, map.pLinearAddress, (uintptr_t)va, 0,
                         0};
    TRY("UPDATE_DEVICE_MAPPING_INFO",
        ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO, sizeof(upd)), &upd) == 0 &&
            !upd.status);
    double t1 = now_s();
    munmap(va, len);
    struct nvos34 un = {rm.client, rm.subdevice, mem, 0, (uintptr_t)va, 0, 0};
    TRY("RM_UNMAP_MEMORY",
        ioctl(rm.ctl, NV_IOWR(NV_ESC_RM_UNMAP_MEMORY, sizeof(un)), &un) == 0 && !un.status);
    close(mfd);
    lat[i] = now_s() - t0;
    lat2[i] = t1 - t0;
  }
  stats("rm-map", "map_unmap_2m", lat, N, 1e6, "us");
  stats("rm-map", "map_touch_2m", lat2, N, 1e6, "us");
  rm_free(rm.device, mem);
  return 0;
}

/* ───────── Wayland: a wl_shm client, full-frame commits ───────── */

static struct {
  struct wl_display *d;
  struct wl_compositor *comp;
  struct wl_shm *shm;
  struct xdg_wm_base *wm;
  int w, h, configured;
  int busy[4];
} wl;

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface,
                       uint32_t ver) {
  if (!strcmp(iface, "wl_compositor"))
    wl.comp = wl_registry_bind(r, name, &wl_compositor_interface, 4);
  else if (!strcmp(iface, "wl_shm"))
    wl.shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
  else if (!strcmp(iface, "xdg_wm_base"))
    wl.wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
}
static void reg_remove(void *data, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener reg_l = {reg_global, reg_remove};
static void wm_ping(void *data, struct xdg_wm_base *wm, uint32_t serial) {
  xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_l = {wm_ping};
static void xs_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
  xdg_surface_ack_configure(xs, serial);
  wl.configured = 1;
}
static const struct xdg_surface_listener xs_l = {xs_configure};
static void tl_configure(void *data, struct xdg_toplevel *t, int32_t w, int32_t h,
                         struct wl_array *s) {
  if (w > 0 && h > 0) {
    wl.w = w;
    wl.h = h;
  }
}
static void tl_close(void *data, struct xdg_toplevel *t) {}
static const struct xdg_toplevel_listener tl_l = {tl_configure, tl_close, NULL, NULL};
static void buf_release(void *data, struct wl_buffer *b) { wl.busy[(intptr_t)data] = 0; }
static const struct wl_buffer_listener buf_l = {buf_release};

static int t_wl_shm(void) {
  enum { NB = 3 };
  const int W = 1920, H = 1080, stride = W * 4;
  const size_t fsz = (size_t)stride * H;
  struct wl_buffer *bufs[NB];

  wl.d = wl_display_connect(NULL);
  TRY("wl_display_connect (WAYLAND_DISPLAY)", wl.d);
  struct wl_registry *reg = wl_display_get_registry(wl.d);
  wl_registry_add_listener(reg, &reg_l, NULL);
  wl_display_roundtrip(wl.d);
  TRY("wl_compositor, wl_shm, xdg_wm_base", wl.comp && wl.shm && wl.wm);
  xdg_wm_base_add_listener(wl.wm, &wm_l, NULL);
  struct wl_surface *s = wl_compositor_create_surface(wl.comp);
  struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wl.wm, s);
  xdg_surface_add_listener(xs, &xs_l, NULL);
  struct xdg_toplevel *tl = xdg_surface_get_toplevel(xs);
  xdg_toplevel_add_listener(tl, &tl_l, NULL);
  xdg_toplevel_set_title(tl, "nvgpu-bench wl-shm");
  xdg_toplevel_set_app_id(tl, "nvgpu-bench");
  wl_surface_commit(s);
  while (!wl.configured && wl_display_dispatch(wl.d) >= 0)
    ;
  int fd = memfd_create("nvgpu-bench", MFD_CLOEXEC);
  TRY("memfd", fd >= 0 && ftruncate(fd, fsz * NB) == 0);
  uint8_t *pix = mmap(NULL, fsz * NB, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  TRY("mmap", pix != MAP_FAILED);
  struct wl_shm_pool *pool = wl_shm_create_pool(wl.shm, fd, fsz * NB);
  for (int i = 0; i < NB; i++) {
    bufs[i] = wl_shm_pool_create_buffer(pool, i * fsz, W, H, stride, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(bufs[i], &buf_l, (void *)(intptr_t)i);
  }
  double t0 = 0, c0 = 0, end = 0;
  int frames = 0, warm = 60;
  for (int n = 0;; n++) {
    int i;
    for (;;) {
      for (i = 0; i < NB && wl.busy[i]; i++)
        ;
      if (i < NB)
        break;
      if (wl_display_dispatch(wl.d) < 0)
        return -1;
    }
    memset(pix + i * fsz, n & 0xff, fsz);
    wl_surface_attach(s, bufs[i], 0, 0);
    wl_surface_damage_buffer(s, 0, 0, W, H);
    wl_surface_commit(s);
    wl.busy[i] = 1;
    wl_display_flush(wl.d);
    wl_display_dispatch_pending(wl.d);
    if (n == warm) {
      t0 = now_s();
      c0 = cpu_s();
      end = t0 + 5.0;
    } else if (n > warm) {
      frames++;
      if (now_s() >= end)
        break;
    }
  }
  double dt = now_s() - t0;
  bench("wl-shm", "fps", frames / dt, "frames/s");
  bench("wl-shm", "MBps", frames * fsz / dt / 1e6, "MB/s");
  bench("wl-shm", "client_cpu", (cpu_s() - c0) / dt * 100, "%");
  wl_display_disconnect(wl.d);
  return 0;
}

/* ───────── main ───────── */

static const struct {
  const char *name;
  int (*fn)(void);
  char group;
} tests[] = {
    {"vk-init", t_vk_init, 'v'},   {"vk-submit", t_vk_submit, 'v'}, {"vk-draws", t_vk_draws, 'v'},
    {"vk-cost", t_vk_cost, 'c'},
    {"vk-alloc", t_vk_alloc, 'v'}, {"vk-copy", t_vk_copy, 'v'},     {"vk-cpu", t_vk_cpu, 'v'},
    {"gl-init", t_gl_init, 'g'},   {"gl-xfer", t_gl_xfer, 'g'},     {"gl-draws", t_gl_draws, 'g'},
    {"rm-ctl", t_rm_ctl, 'r'},     {"rm-map", t_rm_map, 'r'},       {"wl-shm", t_wl_shm, 'w'},
    /* Only by name: rig/rig-heavy.sh's, not the suite's. */
    {"vk-stream", t_vk_stream, 's'}, {"vk-stream-pool", t_vk_stream_pool, 's'},
};
#define NTESTS (sizeof(tests) / sizeof(tests[0]))

int main(int argc, char **argv) {
  int failed = 0;

  if (argc < 2) {
    fprintf(stderr, "usage: nvgpu-bench <test>... | all | vk | gl | rm | wl\n");
    for (unsigned i = 0; i < NTESTS; i++)
      fprintf(stderr, "  %s\n", tests[i].name);
    return 2;
  }
  for (int a = 1; a < argc; a++) {
    int ran = 0;
    for (unsigned i = 0; i < NTESTS; i++) {
      int match = !strcmp(argv[a], tests[i].name) ||
                  (!strcmp(argv[a], "all") && tests[i].group != 'w' && tests[i].group != 's') ||
                  (!strcmp(argv[a], "vk") && tests[i].group == 'v') ||
                  (!strcmp(argv[a], "gl") && tests[i].group == 'g') ||
                  (!strcmp(argv[a], "rm") && tests[i].group == 'r') ||
                  (!strcmp(argv[a], "wl") && tests[i].group == 'w');
      if (!match)
        continue;
      ran = 1;
      if (tests[i].fn()) {
        printf("BENCH-FAIL %s\n", tests[i].name);
        failed++;
      }
    }
    if (!ran) {
      fprintf(stderr, "nvgpu-bench: no test %s\n", argv[a]);
      failed++;
    }
  }
  vk_close();
  gl_close();
  return failed;
}
