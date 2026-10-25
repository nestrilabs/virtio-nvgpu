/*
 * Does a guest make a Vulkan device with each extension the driver offers it?
 *
 * Without /dev/nvidia-uvm, NVIDIA's Vulkan driver is reported to list
 * extensions that need it -- acceleration structures and the ray tracing
 * built on them, binary import, optical flow, CUDA kernel launch -- and then
 * fail vkCreateDevice with VK_ERROR_INITIALIZATION_FAILED when one is asked
 * for. An application that enables what it is offered then does not start.
 * This asks, one extension at a time, each in its own process so a crash or
 * a hang costs that line and not the run.
 *
 * Dynamic, unlike the other probes: the Vulkan loader cannot be linked
 * statically. It opens libvulkan.so.1 itself, so it builds against headers
 * alone.
 *
 * Prints, per extension: offered=yes|no and, when offered, the vkCreateDevice
 * result (0 is VK_SUCCESS, -3 VK_ERROR_INITIALIZATION_FAILED, -7
 * VK_ERROR_EXTENSION_NOT_PRESENT, -8 VK_ERROR_FEATURE_NOT_PRESENT), or
 * "crashed sig=N" / "hung".
 */
#define VK_NO_PROTOTYPES
#define VK_ENABLE_BETA_EXTENSIONS
#include <dlfcn.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

static PFN_vkGetInstanceProcAddr gipa;

#define LOAD(inst, name) PFN_##name name = (PFN_##name)gipa(inst, #name)

static VkInstance instance(void) {
  void *lib = dlopen("libvulkan.so.1", RTLD_NOW);
  if (!lib) {
    printf("GUEST: vkrt: no libvulkan.so.1: %s\n", dlerror());
    exit(99);
  }
  gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
  LOAD(NULL, vkCreateInstance);
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .pApplicationName = "vkrt",
                           .apiVersion = VK_API_VERSION_1_3};
  VkInstanceCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                             .pApplicationInfo = &app};
  VkInstance inst;
  VkResult r = vkCreateInstance(&ci, NULL, &inst);
  if (r) {
    printf("GUEST: vkrt: vkCreateInstance rc=%d\n", r);
    exit(99);
  }
  return inst;
}

static VkPhysicalDevice nvidia(VkInstance inst) {
  LOAD(inst, vkEnumeratePhysicalDevices);
  LOAD(inst, vkGetPhysicalDeviceProperties);
  VkPhysicalDevice pd[8];
  uint32_t n = 8;
  vkEnumeratePhysicalDevices(inst, &n, pd);
  for (uint32_t i = 0; i < n; i++) {
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties(pd[i], &p);
    if (p.vendorID == 0x10de)
      return pd[i];
  }
  printf("GUEST: vkrt: no NVIDIA device among %u\n", n);
  exit(99);
}

static int offered(VkInstance inst, VkPhysicalDevice pd, const char *ext) {
  LOAD(inst, vkEnumerateDeviceExtensionProperties);
  uint32_t n = 0;
  vkEnumerateDeviceExtensionProperties(pd, NULL, &n, NULL);
  VkExtensionProperties *e = calloc(n, sizeof(*e));
  vkEnumerateDeviceExtensionProperties(pd, NULL, &n, e);
  int found = 0;
  for (uint32_t i = 0; i < n; i++)
    if (!strcmp(e[i].extensionName, ext))
      found = 1;
  free(e);
  return found;
}

/* One test: the extensions to enable, the first being the one reported. The
 * rest are what it requires. */
struct test {
  const char *ext[4];
  /* Ray tracing needs these features on, or a device is made without them
   * and the driver is never asked. */
  int as_features;
};

static const struct test tests[] = {
    {{NULL}, 0}, /* a plain device, the baseline */
    {{"VK_KHR_acceleration_structure", "VK_KHR_deferred_host_operations"}, 1},
    {{"VK_KHR_ray_query", "VK_KHR_acceleration_structure",
      "VK_KHR_deferred_host_operations"},
     1},
    {{"VK_KHR_ray_tracing_pipeline", "VK_KHR_acceleration_structure",
      "VK_KHR_deferred_host_operations"},
     1},
    {{"VK_NV_ray_tracing"}, 0},
    {{"VK_NVX_binary_import"}, 0},
    {{"VK_NV_optical_flow"}, 0},
    {{"VK_NV_cuda_kernel_launch"}, 0},
};

static int run(const struct test *t) {
  VkInstance inst = instance();
  VkPhysicalDevice pd = nvidia(inst);
  LOAD(inst, vkCreateDevice);
  LOAD(inst, vkDestroyDevice);

  uint32_t ne = 0;
  while (ne < 4 && t->ext[ne])
    ne++;
  for (uint32_t i = 0; i < ne; i++)
    if (!offered(inst, pd, t->ext[i]))
      return 100 + i; /* not offered: nothing to ask */

  float prio = 1.0f;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                               .queueFamilyIndex = 0,
                               .queueCount = 1,
                               .pQueuePriorities = &prio};
  VkPhysicalDeviceRayQueryFeaturesKHR rq = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR};
  VkPhysicalDeviceRayTracingPipelineFeaturesKHR rtp = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_PIPELINE_FEATURES_KHR};
  VkPhysicalDeviceAccelerationStructureFeaturesKHR as = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
      .accelerationStructure = VK_TRUE};
  VkPhysicalDeviceVulkan12Features v12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .bufferDeviceAddress = VK_TRUE};
  void *next = NULL;
  if (t->as_features) {
    as.pNext = &v12;
    next = &as;
    if (!strcmp(t->ext[0], "VK_KHR_ray_query")) {
      rq.rayQuery = VK_TRUE;
      rq.pNext = next;
      next = &rq;
    } else if (!strcmp(t->ext[0], "VK_KHR_ray_tracing_pipeline")) {
      rtp.rayTracingPipeline = VK_TRUE;
      rtp.pNext = next;
      next = &rtp;
    }
  }
  VkDeviceCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                           .pNext = next,
                           .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &q,
                           .enabledExtensionCount = ne,
                           .ppEnabledExtensionNames = t->ext};
  VkDevice dev;
  VkResult r = vkCreateDevice(pd, &ci, NULL, &dev);
  if (r == VK_SUCCESS)
    vkDestroyDevice(dev, NULL);
  /* Exit codes carry the result: 0..99 is -VkResult. */
  return -r;
}

int main(void) {
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    const char *name = tests[i].ext[0] ? tests[i].ext[0] : "(none)";
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0)
      _exit(run(&tests[i]));
    int st = 0, waited = 0;
    while (waitpid(pid, &st, WNOHANG) == 0) {
      if (waited++ >= 300) { /* 30 s */
        kill(pid, SIGKILL);
        waitpid(pid, &st, 0);
        printf("GUEST: vkrt %-32s offered=yes hung\n", name);
        goto next;
      }
      struct timespec ts = {0, 100 * 1000 * 1000};
      nanosleep(&ts, NULL);
    }
    if (WIFSIGNALED(st)) {
      printf("GUEST: vkrt %-32s offered=yes crashed sig=%d\n", name, WTERMSIG(st));
    } else {
      int c = WEXITSTATUS(st);
      if (c == 99)
        printf("GUEST: vkrt %-32s setup failed\n", name);
      else if (c >= 100)
        printf("GUEST: vkrt %-32s offered=no (%s)\n", name, tests[i].ext[c - 100]);
      else
        printf("GUEST: vkrt %-32s offered=yes vkCreateDevice rc=%d\n", name, -c);
    }
  next:;
  }
  printf("GUEST: vkrt uvm node %s\n",
         access("/dev/nvidia-uvm", F_OK) == 0 ? "present" : "absent");
  return 0;
}
