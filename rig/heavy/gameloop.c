// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-gameloop -- a game's frame, synthetic, for rig/rig-heavy.sh: a job
 * system's fork-join phases across every CPU, then a render of many draws
 * presented through Vulkan's Wayland WSI, unpaced. What a modern engine (or
 * a Proton game) does to a machine each frame, without its assets: a wakeup
 * of every worker per phase (futexes, and in a guest, IPIs between vCPUs),
 * memory streamed and gathered at random through a large heap (TLB misses,
 * which nested paging makes dearer), a few thousand draws recorded on the
 * main thread, and explicit sync on present.
 *
 * Every frame's time (ms, present to present) goes to $GL_FRAMES after
 * $GL_WARM seconds, for $GL_SECS seconds. Environment (defaults):
 *   GL_THREADS    threads taking jobs, the main thread included (the CPUs
 *                 it may run on)
 *   GL_PHASES     fork-join phases a frame (4)
 *   GL_JOBS       jobs a phase (64)
 *   GL_PARTICLES  particles integrated by each phase's jobs, 32 bytes each
 *                 (1048576)
 *   GL_GATHER     random 8-byte reads a job makes into GL_HEAP_MIB (2048)
 *   GL_HEAP_MIB   the table those reads land in (512)
 *   GL_SPIN_US    how long an idle worker spins before it sleeps (0)
 *   GL_DRAWS      draws a frame, a push constant each (4000)
 *   GL_PRESENT    immediate, mailbox or fifo (immediate, else mailbox)
 *   GL_WIDTH, GL_HEIGHT (1280 x 720), GL_WARM (6), GL_SECS (20)
 * Output: HEAVY_GAMELOOP lines (the settings, then the summary).
 */
#define _GNU_SOURCE
#include <linux/futex.h>
#include <math.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>
#include <wayland-client.h>

#include "gameloop-spv.h"
#include "xdg-shell-client-protocol.h"

#define DIE(...)                                                                   \
  do {                                                                             \
    fprintf(stderr, "nvgpu-gameloop: " __VA_ARGS__);                               \
    fputc('\n', stderr);                                                           \
    exit(1);                                                                       \
  } while (0)
#define VK(x)                                                                      \
  do {                                                                             \
    VkResult r_ = (x);                                                             \
    if (r_ != VK_SUCCESS)                                                          \
      DIE("%s: %d (line %d)", #x, r_, __LINE__);                                   \
  } while (0)

static double now_s(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static long env_l(const char *n, long d) {
  const char *v = getenv(n);
  return v && *v ? strtol(v, NULL, 0) : d;
}

/* ───────── the job system ───────── */

struct particle {
  float p[4], v[4];
};

static struct {
  int threads, phases, jobs, gather;
  long particles;
  uint64_t *heap;
  size_t heap_n;
  struct particle *pt;
  double spin_s;
  /* One phase at a time: `gen` moves when a phase starts, `next` hands out
   * its jobs, `done` counts them in, and `idle` is the main thread's word
   * to sleep on until the last one is. */
  _Atomic uint32_t gen;
  _Atomic int next, done;
  _Atomic uint32_t idle;
  _Atomic int quit;
  _Atomic uint64_t sink;
} js;

static void futex_wait(_Atomic uint32_t *w, uint32_t v) {
  syscall(SYS_futex, w, FUTEX_WAIT_PRIVATE, v, NULL, NULL, 0);
}
static void futex_wake(_Atomic uint32_t *w, int n) {
  syscall(SYS_futex, w, FUTEX_WAKE_PRIVATE, n, NULL, NULL, 0);
}

/* A job: its share of the particles integrated, then `gather` dependent
 * reads at random through the heap, as a game's scattered state is. */
static void job(int phase, int j, uint64_t seed) {
  long per = js.particles / js.jobs, lo = per * j, hi = lo + per;
  float dt = 0.001f * (float)(phase + 1);
  for (long i = lo; i < hi; i++) {
    struct particle *q = &js.pt[i];
    q->v[1] -= 9.81f * dt;
    for (int k = 0; k < 3; k++) {
      q->p[k] += q->v[k] * dt;
      if (q->p[k] < -100.0f || q->p[k] > 100.0f)
        q->v[k] = -q->v[k] * 0.9f;
    }
  }
  uint64_t x = seed * 0x9e3779b97f4a7c15ull + (uint64_t)j, acc = 0;
  for (int g = 0; g < js.gather; g++) {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    acc += js.heap[(x * 0x2545f4914f6cdd1dull ^ acc) % js.heap_n];
  }
  atomic_fetch_add_explicit(&js.sink, acc, memory_order_relaxed);
}

/* Take jobs of the current phase until there are none; the thread that
 * completes the last one wakes the main thread. */
static void drain(uint32_t gen) {
  int j;
  while ((j = atomic_fetch_add(&js.next, 1)) < js.jobs) {
    job((int)(gen % 16), j, gen);
    if (atomic_fetch_add(&js.done, 1) + 1 == js.jobs) {
      atomic_store(&js.idle, 1);
      futex_wake(&js.idle, 1);
    }
  }
}

static void *worker(void *arg) {
  uint32_t seen = 0;
  (void)arg;
  while (!atomic_load(&js.quit)) {
    uint32_t g = atomic_load(&js.gen);
    if (g == seen) {
      double until = now_s() + js.spin_s;
      while (js.spin_s > 0 && atomic_load(&js.gen) == seen && now_s() < until)
        __builtin_ia32_pause();
      if (atomic_load(&js.gen) == seen)
        futex_wait(&js.gen, seen);
      continue;
    }
    seen = g;
    drain(g);
  }
  return NULL;
}

/* One fork-join phase, the main thread taking jobs as well. */
static void phase(void) {
  atomic_store(&js.next, 0);
  atomic_store(&js.done, 0);
  atomic_store(&js.idle, 0);
  uint32_t g = atomic_fetch_add(&js.gen, 1) + 1;
  futex_wake(&js.gen, js.threads);
  drain(g);
  while (!atomic_load(&js.idle))
    futex_wait(&js.idle, 0);
}

/* ───────── Wayland and Vulkan ───────── */

static struct {
  struct wl_display *d;
  struct wl_compositor *comp;
  struct xdg_wm_base *wm;
  int configured;
} wl;

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface,
                       uint32_t ver) {
  (void)data;
  (void)ver;
  if (!strcmp(iface, "wl_compositor"))
    wl.comp = wl_registry_bind(r, name, &wl_compositor_interface, 4);
  else if (!strcmp(iface, "xdg_wm_base"))
    wl.wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
}
static void reg_remove(void *data, struct wl_registry *r, uint32_t name) {
  (void)data;
  (void)r;
  (void)name;
}
static const struct wl_registry_listener reg_l = {reg_global, reg_remove};
static void wm_ping(void *data, struct xdg_wm_base *wm, uint32_t serial) {
  (void)data;
  xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_l = {wm_ping};
static void xs_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
  (void)data;
  xdg_surface_ack_configure(xs, serial);
  wl.configured = 1;
}
static const struct xdg_surface_listener xs_l = {xs_configure};
static void tl_configure(void *data, struct xdg_toplevel *t, int32_t w, int32_t h,
                         struct wl_array *s) {
  (void)data;
  (void)t;
  (void)w;
  (void)h;
  (void)s;
}
static void tl_close(void *data, struct xdg_toplevel *t) {
  (void)data;
  (void)t;
}
static const struct xdg_toplevel_listener tl_l = {tl_configure, tl_close, NULL, NULL};

enum { MAXIMG = 8, INFLIGHT = 2 };

int main(void) {
  static double ft[400000];
  const uint32_t W = (uint32_t)env_l("GL_WIDTH", 1280), H = (uint32_t)env_l("GL_HEIGHT", 720);
  const int draws = (int)env_l("GL_DRAWS", 4000);
  const double warm = (double)env_l("GL_WARM", 6), secs = (double)env_l("GL_SECS", 20);
  const char *pm = getenv("GL_PRESENT") ? getenv("GL_PRESENT") : "immediate";

  /* As many threads as the CPUs this process may run on (taskset's, a
   * guest's), as an engine sizes its job system. */
  cpu_set_t aff;
  long ncpu = sched_getaffinity(0, sizeof(aff), &aff) ? sysconf(_SC_NPROCESSORS_ONLN)
                                                      : CPU_COUNT(&aff);
  js.threads = (int)env_l("GL_THREADS", ncpu);
  js.phases = (int)env_l("GL_PHASES", 4);
  js.jobs = (int)env_l("GL_JOBS", 64);
  js.particles = env_l("GL_PARTICLES", 1048576);
  js.gather = (int)env_l("GL_GATHER", 2048);
  js.spin_s = (double)env_l("GL_SPIN_US", 0) * 1e-6;
  size_t heap_b = (size_t)env_l("GL_HEAP_MIB", 512) << 20;
  if (js.threads < 1 || js.jobs < 1 || js.phases < 0 || js.particles < js.jobs)
    DIE("bad GL_THREADS / GL_JOBS / GL_PHASES / GL_PARTICLES");
  js.heap = mmap(NULL, heap_b, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (js.heap == MAP_FAILED)
    DIE("heap of %zu bytes", heap_b);
  js.heap_n = heap_b / 8;
  for (size_t i = 0; i < js.heap_n; i++)
    js.heap[i] = i * 0x9e3779b97f4a7c15ull;
  js.pt = calloc((size_t)js.particles, sizeof(*js.pt));
  if (!js.pt)
    DIE("particles");
  for (long i = 0; i < js.particles; i++)
    for (int k = 0; k < 3; k++) {
      js.pt[i].p[k] = (float)((i * (k + 3)) % 200) - 100.0f;
      js.pt[i].v[k] = (float)((i * (k + 7)) % 20) - 10.0f;
    }
  pthread_t th[256];
  int nw = js.threads - 1 < 256 ? js.threads - 1 : 256;
  for (int i = 0; i < nw; i++)
    pthread_create(&th[i], NULL, worker, NULL);

  wl.d = wl_display_connect(NULL);
  if (!wl.d)
    DIE("wl_display_connect (WAYLAND_DISPLAY)");
  struct wl_registry *reg = wl_display_get_registry(wl.d);
  wl_registry_add_listener(reg, &reg_l, NULL);
  wl_display_roundtrip(wl.d);
  if (!wl.comp || !wl.wm)
    DIE("no wl_compositor or xdg_wm_base");
  xdg_wm_base_add_listener(wl.wm, &wm_l, NULL);
  struct wl_surface *surf = wl_compositor_create_surface(wl.comp);
  struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wl.wm, surf);
  xdg_surface_add_listener(xs, &xs_l, NULL);
  struct xdg_toplevel *tl = xdg_surface_get_toplevel(xs);
  xdg_toplevel_add_listener(tl, &tl_l, NULL);
  xdg_toplevel_set_title(tl, "nvgpu-gameloop");
  wl_surface_commit(surf);
  while (!wl.configured && wl_display_dispatch(wl.d) >= 0)
    ;

  const char *iext[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME};
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .pApplicationName = "nvgpu-gameloop",
                           .apiVersion = VK_API_VERSION_1_3};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                              .pApplicationInfo = &app,
                              .enabledExtensionCount = 2,
                              .ppEnabledExtensionNames = iext};
  VkInstance inst;
  VK(vkCreateInstance(&ici, NULL, &inst));
  VkWaylandSurfaceCreateInfoKHR wsci = {.sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
                                        .display = wl.d,
                                        .surface = surf};
  VkSurfaceKHR vs;
  VK(vkCreateWaylandSurfaceKHR(inst, &wsci, NULL, &vs));
  VkPhysicalDevice pds[8], pd = VK_NULL_HANDLE;
  uint32_t n = 8;
  VK(vkEnumeratePhysicalDevices(inst, &n, pds));
  for (uint32_t i = 0; i < n && !pd; i++) {
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties(pds[i], &p);
    if (p.vendorID == 0x10de)
      pd = pds[i];
  }
  if (!pd)
    DIE("no NVIDIA physical device");
  uint32_t qfam = UINT32_MAX, nq = 16;
  VkQueueFamilyProperties qf[16];
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
  for (uint32_t i = 0; i < nq && qfam == UINT32_MAX; i++) {
    VkBool32 ok = VK_FALSE;
    vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, vs, &ok);
    if ((qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && ok)
      qfam = i;
  }
  if (qfam == UINT32_MAX)
    DIE("no graphics queue that presents");
  float prio = 1.0f;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = qfam,
                                 .queueCount = 1,
                                 .pQueuePriorities = &prio};
  VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .synchronization2 = VK_TRUE,
      .dynamicRendering = VK_TRUE};
  const char *dext[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .pNext = &f13,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qci,
                            .enabledExtensionCount = 1,
                            .ppEnabledExtensionNames = dext};
  VkDevice dev;
  VK(vkCreateDevice(pd, &dci, NULL, &dev));
  VkQueue q;
  vkGetDeviceQueue(dev, qfam, 0, &q);

  VkPresentModeKHR modes[8], mode = VK_PRESENT_MODE_FIFO_KHR, want = VK_PRESENT_MODE_IMMEDIATE_KHR;
  if (!strcmp(pm, "mailbox"))
    want = VK_PRESENT_MODE_MAILBOX_KHR;
  else if (!strcmp(pm, "fifo"))
    want = VK_PRESENT_MODE_FIFO_KHR;
  uint32_t nm = 8;
  VK(vkGetPhysicalDeviceSurfacePresentModesKHR(pd, vs, &nm, modes));
  for (uint32_t i = 0; i < nm; i++)
    if (modes[i] == want)
      mode = want;
  if (mode != want && want == VK_PRESENT_MODE_IMMEDIATE_KHR)
    for (uint32_t i = 0; i < nm; i++)
      if (modes[i] == VK_PRESENT_MODE_MAILBOX_KHR)
        mode = VK_PRESENT_MODE_MAILBOX_KHR;
  VkSurfaceCapabilitiesKHR caps;
  VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, vs, &caps));
  uint32_t nimg = caps.minImageCount + 1;
  if (caps.maxImageCount && nimg > caps.maxImageCount)
    nimg = caps.maxImageCount;
  const VkFormat fmt = VK_FORMAT_B8G8R8A8_UNORM;
  VkSwapchainCreateInfoKHR sci = {.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
                                  .surface = vs,
                                  .minImageCount = nimg,
                                  .imageFormat = fmt,
                                  .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
                                  .imageExtent = {W, H},
                                  .imageArrayLayers = 1,
                                  .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
                                  .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
                                  .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
                                  .presentMode = mode,
                                  .clipped = VK_TRUE};
  VkSwapchainKHR sc;
  VK(vkCreateSwapchainKHR(dev, &sci, NULL, &sc));
  VkImage imgs[MAXIMG];
  VkImageView views[MAXIMG];
  nimg = MAXIMG;
  VK(vkGetSwapchainImagesKHR(dev, sc, &nimg, imgs));
  for (uint32_t i = 0; i < nimg; i++) {
    VkImageViewCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                                .image = imgs[i],
                                .viewType = VK_IMAGE_VIEW_TYPE_2D,
                                .format = fmt,
                                .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    VK(vkCreateImageView(dev, &vi, NULL, &views[i]));
  }

  VkShaderModule vsm, fsm;
  VkShaderModuleCreateInfo smi = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                  .codeSize = sizeof(gl_vs),
                                  .pCode = gl_vs};
  VK(vkCreateShaderModule(dev, &smi, NULL, &vsm));
  smi.codeSize = sizeof(gl_fs);
  smi.pCode = gl_fs;
  VK(vkCreateShaderModule(dev, &smi, NULL, &fsm));
  VkPushConstantRange pcr = {VK_SHADER_STAGE_VERTEX_BIT, 0, 16};
  VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                    .pushConstantRangeCount = 1,
                                    .pPushConstantRanges = &pcr};
  VkPipelineLayout pl;
  VK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
  VkPipelineShaderStageCreateInfo st[2] = {
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_VERTEX_BIT,
       .module = vsm,
       .pName = "main"},
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
       .module = fsm,
       .pName = "main"}};
  VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
  VkViewport vp = {0, 0, (float)W, (float)H, 0, 1};
  VkRect2D scr = {{0, 0}, {W, H}};
  VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &scr};
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
                                      .layout = pl};
  VkPipeline pipe;
  VK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpi, NULL, &pipe));

  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                 .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                 .queueFamilyIndex = qfam};
  VkCommandPool pool;
  VK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
  VkCommandBuffer cb[INFLIGHT];
  VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                     .commandPool = pool,
                                     .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                     .commandBufferCount = INFLIGHT};
  VK(vkAllocateCommandBuffers(dev, &cai, cb));
  VkFence fence[INFLIGHT];
  VkSemaphore acq[INFLIGHT], done[MAXIMG];
  VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                           .flags = VK_FENCE_CREATE_SIGNALED_BIT};
  VkSemaphoreCreateInfo semi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
  for (int i = 0; i < INFLIGHT; i++) {
    VK(vkCreateFence(dev, &fci, NULL, &fence[i]));
    VK(vkCreateSemaphore(dev, &semi, NULL, &acq[i]));
  }
  for (uint32_t i = 0; i < nimg; i++)
    VK(vkCreateSemaphore(dev, &semi, NULL, &done[i]));

  printf("HEAVY_GAMELOOP threads=%d phases=%d jobs=%d particles=%ld gather=%d heap_mib=%zu "
         "spin_us=%.0f draws=%d present=%s images=%u size=%ux%u cpus=%ld\n",
         js.threads, js.phases, js.jobs, js.particles, js.gather, heap_b >> 20, js.spin_s * 1e6,
         draws, mode == VK_PRESENT_MODE_IMMEDIATE_KHR ? "immediate"
                : mode == VK_PRESENT_MODE_MAILBOX_KHR ? "mailbox"
                                                       : "fifo",
         nimg, W, H, ncpu);
  fflush(stdout);

  double start = now_s(), t0 = start + warm, end = t0 + secs, prev = start, t = start;
  double jobs_s = 0, rec_s = 0, wait_s = 0;
  int nf = 0;
  uint64_t fr = 0;
  while (t < end && nf < (int)(sizeof(ft) / sizeof(ft[0]))) {
    int f = (int)(fr % INFLIGHT);
    double a0 = now_s();
    VK(vkWaitForFences(dev, 1, &fence[f], VK_TRUE, UINT64_MAX));
    VK(vkResetFences(dev, 1, &fence[f]));
    double a1 = now_s();
    for (int p = 0; p < js.phases; p++)
      phase();
    double a2 = now_s();
    uint32_t ii;
    VkResult ar = vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acq[f], VK_NULL_HANDLE, &ii);
    if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR)
      DIE("vkAcquireNextImageKHR: %d", ar);
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                   .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    VK(vkResetCommandBuffer(cb[f], 0));
    VK(vkBeginCommandBuffer(cb[f], &bi));
    VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                              .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                              .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                              .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                              .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .image = imgs[ii],
                              .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    vkCmdPipelineBarrier(cb[f], VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                         VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &b);
    VkClearValue clr = {.color = {{0.05f, 0.05f, 0.08f * (float)(fr % 8), 1.0f}}};
    VkRenderingAttachmentInfo ca = {.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
                                    .imageView = views[ii],
                                    .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                    .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                    .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                    .clearValue = clr};
    VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                          .renderArea = {{0, 0}, {W, H}},
                          .layerCount = 1,
                          .colorAttachmentCount = 1,
                          .pColorAttachments = &ca};
    vkCmdBeginRendering(cb[f], &ri);
    vkCmdBindPipeline(cb[f], VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    for (int d = 0; d < draws; d++) {
      float pc[4] = {(float)(d % 100) * 0.02f - 1.0f, (float)(d / 100 % 100) * 0.02f - 1.0f,
                     0.015f, (float)(fr & 0xff)};
      vkCmdPushConstants(cb[f], pl, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(pc), pc);
      vkCmdDraw(cb[f], 3, 1, 0, 0);
    }
    vkCmdEndRendering(cb[f]);
    b.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    b.dstAccessMask = 0;
    b.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    b.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    vkCmdPipelineBarrier(cb[f], VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                         VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &b);
    VK(vkEndCommandBuffer(cb[f]));
    VkPipelineStageFlags ws = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                       .waitSemaphoreCount = 1,
                       .pWaitSemaphores = &acq[f],
                       .pWaitDstStageMask = &ws,
                       .commandBufferCount = 1,
                       .pCommandBuffers = &cb[f],
                       .signalSemaphoreCount = 1,
                       .pSignalSemaphores = &done[ii]};
    VK(vkQueueSubmit(q, 1, &si, fence[f]));
    VkPresentInfoKHR pi = {.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
                           .waitSemaphoreCount = 1,
                           .pWaitSemaphores = &done[ii],
                           .swapchainCount = 1,
                           .pSwapchains = &sc,
                           .pImageIndices = &ii};
    VkResult pr = vkQueuePresentKHR(q, &pi);
    if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR)
      DIE("vkQueuePresentKHR: %d", pr);
    fr++;
    t = now_s();
    if (prev >= t0) {
      ft[nf++] = (t - prev) * 1e3;
      wait_s += a1 - a0;
      jobs_s += a2 - a1;
      rec_s += t - a2;
    }
    prev = t;
    /* The compositor's events (configure, ping, releases). */
    wl_display_dispatch_pending(wl.d);
    wl_display_flush(wl.d);
  }
  VK(vkDeviceWaitIdle(dev));
  atomic_store(&js.quit, 1);
  atomic_fetch_add(&js.gen, 1);
  futex_wake(&js.gen, js.threads);
  for (int i = 0; i < nw; i++)
    pthread_join(th[i], NULL);

  const char *path = getenv("GL_FRAMES");
  FILE *fo = fopen(path ? path : "/tmp/gameloop-frames.txt", "w");
  if (fo) {
    for (int i = 0; i < nf; i++)
      fprintf(fo, "%.4f\n", ft[i]);
    fclose(fo);
  }
  double sum = 0;
  for (int i = 0; i < nf; i++)
    sum += ft[i];
  printf("HEAVY_GAMELOOP done frames=%d fps=%.1f mean_ms=%.4f jobs_ms=%.4f render_present_ms=%.4f "
         "fence_wait_ms=%.4f sink=%llu\n",
         nf, nf ? nf / (sum / 1e3) : 0, nf ? sum / nf : 0, nf ? jobs_s / nf * 1e3 : 0,
         nf ? rec_s / nf * 1e3 : 0, nf ? wait_s / nf * 1e3 : 0,
         (unsigned long long)(atomic_load(&js.sink) & 0xffff));
  return 0;
}
