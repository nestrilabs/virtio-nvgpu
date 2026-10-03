/*
 * Is a Vulkan guest held to its video memory limit?
 *
 * The CUDA probe (vramprobe) covers RM_ALLOC; NVIDIA's Vulkan driver takes
 * video memory through VID_HEAP_CONTROL instead, so this asks the same
 * question on that route: device-local memory, 64 MiB at a time, until the
 * driver says no. With VRAM_LIMIT_MIB set, allocation must stop at the
 * limit, not past it and not far short, and the refusal must be
 * VK_ERROR_OUT_OF_DEVICE_MEMORY (-2).
 *
 *   cc -O2 -o vkalloc vkalloc.dyn.c -ldl
 */
#define VK_NO_PROTOTYPES
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>

#define STEP (64ull << 20)
#define MAX_STEPS 4096
#define MARGIN_MIB 384

static PFN_vkGetInstanceProcAddr gipa;
#define LOAD(inst, name) PFN_##name name = (PFN_##name)gipa(inst, #name)

static int failures;
static void check(int ok, const char *what, const char *detail) {
  printf("%-6s %s  %s\n", ok ? "PASS" : "FAIL", what, detail);
  if (!ok)
    failures++;
}

int main(void) {
  const char *env = getenv("VRAM_LIMIT_MIB");
  unsigned long long limit = env ? strtoull(env, NULL, 10) : 0;
  void *lib = dlopen("libvulkan.so.1", RTLD_NOW);
  if (!lib) {
    printf("FAIL   libvulkan.so.1 loads  %s\n", dlerror());
    return 1;
  }
  gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
  LOAD(NULL, vkCreateInstance);
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .apiVersion = VK_API_VERSION_1_1};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                              .pApplicationInfo = &app};
  VkInstance inst;
  if (vkCreateInstance(&ici, NULL, &inst)) {
    printf("FAIL   vkCreateInstance\n");
    return 1;
  }
  LOAD(inst, vkEnumeratePhysicalDevices);
  LOAD(inst, vkGetPhysicalDeviceProperties);
  LOAD(inst, vkGetPhysicalDeviceMemoryProperties);
  LOAD(inst, vkGetPhysicalDeviceQueueFamilyProperties);
  LOAD(inst, vkCreateDevice);
  LOAD(inst, vkGetDeviceProcAddr);
  VkPhysicalDevice pds[8], pd = NULL;
  uint32_t n = 8;
  vkEnumeratePhysicalDevices(inst, &n, pds);
  for (uint32_t i = 0; i < n; i++) {
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties(pds[i], &p);
    if (p.vendorID == 0x10de)
      pd = pds[i];
  }
  if (!pd) {
    printf("FAIL   an NVIDIA device\n");
    return 1;
  }
  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  uint32_t type = UINT32_MAX;
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) &&
        !(mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) {
      type = i;
      break;
    }
  if (type == UINT32_MAX) {
    printf("FAIL   a device-local memory type\n");
    return 1;
  }
  unsigned long long heap = mp.memoryHeaps[mp.memoryTypes[type].heapIndex].size;

  float prio = 1.0f;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                               .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .queueCreateInfoCount = 1, .pQueueCreateInfos = &q};
  VkDevice dev;
  VkResult r = vkCreateDevice(pd, &dci, NULL, &dev);
  if (r) {
    printf("FAIL   vkCreateDevice  %d\n", r);
    return 1;
  }
  PFN_vkAllocateMemory alloc = (PFN_vkAllocateMemory)vkGetDeviceProcAddr(dev, "vkAllocateMemory");
  static VkDeviceMemory mem[MAX_STEPS];
  unsigned long long got = 0;
  for (int i = 0; i < MAX_STEPS; i++) {
    VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .allocationSize = STEP, .memoryTypeIndex = type};
    r = alloc(dev, &ai, NULL, &mem[i]);
    if (r)
      break;
    got += STEP;
  }
  char line[160];
  printf("INFO   limit %llu MiB; device-local heap %llu MiB; allocated %llu MiB, then %d\n", limit,
         heap >> 20, got >> 20, r);
  if (!limit) {
    snprintf(line, sizeof line, "%llu MiB, then %d", got >> 20, r);
    check(r != 0, "allocation stops somewhere", line);
    return failures != 0;
  }
  snprintf(line, sizeof line, "%llu MiB against %llu", heap >> 20, limit);
  check((heap >> 20) <= limit, "the heap is at most the limit", line);
  snprintf(line, sizeof line, "%llu MiB against %llu", got >> 20, limit);
  check((got >> 20) <= limit, "allocation stops at the limit", line);
  check((got >> 20) + MARGIN_MIB >= limit, "and not far short of it", line);
  snprintf(line, sizeof line, "%d", r);
  check(r == VK_ERROR_OUT_OF_DEVICE_MEMORY, "the refusal is out of device memory", line);
  return failures != 0;
}
