// SPDX-License-Identifier: Apache-2.0
/*
 * VK_LAYER_NVGPU_no_uvm -- an implicit Vulkan layer for virtio-nvgpu guests
 * served without compute (the backend's --allow-compute off, so no
 * /dev/nvidia-uvm in the guest).
 *
 * NVIDIA's driver there still lists the device extensions that need UVM --
 * acceleration structures and every ray-tracing extension built on them,
 * VK_NVX_binary_import (DLSS), VK_NV_cuda_kernel_launch and
 * VK_NV_optical_flow (Frame Generation) -- and reports their features, but
 * vkCreateDevice with any of them fails with VK_ERROR_INITIALIZATION_FAILED.
 * An application that enables what it is offered then does not start
 * (Godot's Vulkan renderer dies, vkd3d-proton's D3D12 device hangs its
 * game), where on a driver without them it would have fallen back. This
 * layer makes the guest look like such a driver: it drops those extensions
 * from vkEnumerateDeviceExtensionProperties, clears their features in
 * vkGetPhysicalDeviceFeatures2, and refuses a vkCreateDevice that asks for
 * them with the codes the specification gives (VK_ERROR_EXTENSION_NOT_PRESENT,
 * VK_ERROR_FEATURE_NOT_PRESENT) instead of the driver's failure.
 *
 * It changes nothing when /dev/nvidia-uvm exists (compute is served: the
 * extensions work), for devices that are not NVIDIA's, or when
 * NVGPU_VK_NO_UVM_DISABLE=1 is set (the loader then does not load it). It
 * reaches nothing outside the process: no host surface, a filter on what
 * the application is told.
 */
/* VK_NV_cuda_kernel_launch and VK_NV_displacement_micromap are provisional. */
#define VK_ENABLE_BETA_EXTENSIONS
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vk_layer.h>
#include <vulkan/vulkan.h>

#define LAYER_NAME "VK_LAYER_NVGPU_no_uvm"
#define EXPORT __attribute__((visibility("default")))

/* The extensions a device cannot be made with here, and those that depend
 * on one of them (offering them would name an extension that cannot be
 * enabled without one that is gone). */
static const char *const hidden_ext[] = {
    "VK_KHR_acceleration_structure",
    "VK_KHR_ray_query",
    "VK_KHR_ray_tracing_pipeline",
    "VK_KHR_ray_tracing_maintenance1",
    "VK_KHR_ray_tracing_position_fetch",
    "VK_KHR_opacity_micromap",
    "VK_EXT_opacity_micromap",
    "VK_EXT_ray_tracing_invocation_reorder",
    "VK_NV_ray_tracing",
    "VK_NV_ray_tracing_motion_blur",
    "VK_NV_ray_tracing_invocation_reorder",
    "VK_NV_ray_tracing_linear_swept_spheres",
    "VK_NV_ray_tracing_validation",
    "VK_NV_displacement_micromap",
    "VK_NV_cluster_acceleration_structure",
    "VK_NV_partitioned_acceleration_structure",
    "VK_NVX_binary_import",
    "VK_NV_cuda_kernel_launch",
    "VK_NV_optical_flow",
};

/* Their feature structures: every VkBool32 after sType and pNext is
 * cleared (they are all booleans, to the end of the structure). */
#define FEAT(st, type) {st, sizeof(type)}
static const struct {
  VkStructureType st;
  size_t size;
} hidden_feat[] = {
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
         VkPhysicalDeviceAccelerationStructureFeaturesKHR),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR,
         VkPhysicalDeviceRayQueryFeaturesKHR),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_PIPELINE_FEATURES_KHR,
         VkPhysicalDeviceRayTracingPipelineFeaturesKHR),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_MAINTENANCE_1_FEATURES_KHR,
         VkPhysicalDeviceRayTracingMaintenance1FeaturesKHR),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_POSITION_FETCH_FEATURES_KHR,
         VkPhysicalDeviceRayTracingPositionFetchFeaturesKHR),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_OPACITY_MICROMAP_FEATURES_EXT,
         VkPhysicalDeviceOpacityMicromapFeaturesEXT),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_MOTION_BLUR_FEATURES_NV,
         VkPhysicalDeviceRayTracingMotionBlurFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_INVOCATION_REORDER_FEATURES_NV,
         VkPhysicalDeviceRayTracingInvocationReorderFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_LINEAR_SWEPT_SPHERES_FEATURES_NV,
         VkPhysicalDeviceRayTracingLinearSweptSpheresFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_VALIDATION_FEATURES_NV,
         VkPhysicalDeviceRayTracingValidationFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_CLUSTER_ACCELERATION_STRUCTURE_FEATURES_NV,
         VkPhysicalDeviceClusterAccelerationStructureFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PARTITIONED_ACCELERATION_STRUCTURE_FEATURES_NV,
         VkPhysicalDevicePartitionedAccelerationStructureFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_CUDA_KERNEL_LAUNCH_FEATURES_NV,
         VkPhysicalDeviceCudaKernelLaunchFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_OPTICAL_FLOW_FEATURES_NV,
         VkPhysicalDeviceOpticalFlowFeaturesNV),
    FEAT(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DISPLACEMENT_MICROMAP_FEATURES_NV,
         VkPhysicalDeviceDisplacementMicromapFeaturesNV),
};
#define N(a) (sizeof(a) / sizeof((a)[0]))

static int hidden_name(const char *name) {
  for (size_t i = 0; i < N(hidden_ext); i++)
    if (!strcmp(name, hidden_ext[i]))
      return 1;
  return 0;
}

/* ───────── what the layer keeps per instance and per device ───────── */

/* A dispatchable handle's first word is the loader's dispatch table; a
 * VkPhysicalDevice shares its instance's. */
static void *key_of(const void *h) { return *(void *const *)h; }

struct inst {
  void *key;
  VkInstance instance;
  int active; /* no UVM here: filter */
  PFN_vkGetInstanceProcAddr gipa;
  PFN_vkDestroyInstance destroy;
  PFN_vkEnumerateDeviceExtensionProperties enum_ext;
  PFN_vkGetPhysicalDeviceFeatures2 feat2;
  PFN_vkGetPhysicalDeviceFeatures2 feat2_khr;
  PFN_vkGetPhysicalDeviceProperties props;
};
struct dev {
  void *key;
  PFN_vkGetDeviceProcAddr gdpa;
  PFN_vkDestroyDevice destroy;
};
enum { MAXI = 32, MAXD = 64 };
static struct inst insts[MAXI];
static struct dev devs[MAXD];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static struct inst *inst_of(const void *h) {
  struct inst *r = NULL;
  pthread_mutex_lock(&lock);
  for (int i = 0; i < MAXI && !r; i++)
    if (insts[i].key && insts[i].key == key_of(h))
      r = &insts[i];
  pthread_mutex_unlock(&lock);
  return r;
}

static struct dev *dev_of(const void *h) {
  struct dev *r = NULL;
  pthread_mutex_lock(&lock);
  for (int i = 0; i < MAXD && !r; i++)
    if (devs[i].key && devs[i].key == key_of(h))
      r = &devs[i];
  pthread_mutex_unlock(&lock);
  return r;
}

/* Filter this physical device? Only NVIDIA's, only without UVM. */
static int filtered(struct inst *in, VkPhysicalDevice pd) {
  if (!in || !in->active)
    return 0;
  VkPhysicalDeviceProperties p;
  in->props(pd, &p);
  return p.vendorID == 0x10de;
}

/* ───────── the entry points ───────── */

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL layer_gipa(VkInstance instance, const char *name);
static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL layer_gdpa(VkDevice device, const char *name);

static VKAPI_ATTR VkResult VKAPI_CALL layer_CreateInstance(const VkInstanceCreateInfo *ci,
                                                           const VkAllocationCallbacks *alloc,
                                                           VkInstance *out) {
  VkLayerInstanceCreateInfo *link = (VkLayerInstanceCreateInfo *)ci->pNext;
  while (link && !(link->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO &&
                   link->function == VK_LAYER_LINK_INFO))
    link = (VkLayerInstanceCreateInfo *)link->pNext;
  if (!link)
    return VK_ERROR_INITIALIZATION_FAILED;
  PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
  link->u.pLayerInfo = link->u.pLayerInfo->pNext;
  PFN_vkCreateInstance create = (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
  VkResult r = create(ci, alloc, out);
  if (r != VK_SUCCESS)
    return r;
  struct inst in = {
      .key = key_of(*out),
      .instance = *out,
      .active = access("/dev/nvidia-uvm", F_OK) != 0,
      .gipa = gipa,
      .destroy = (PFN_vkDestroyInstance)gipa(*out, "vkDestroyInstance"),
      .enum_ext = (PFN_vkEnumerateDeviceExtensionProperties)gipa(
          *out, "vkEnumerateDeviceExtensionProperties"),
      .feat2 = (PFN_vkGetPhysicalDeviceFeatures2)gipa(*out, "vkGetPhysicalDeviceFeatures2"),
      .feat2_khr = (PFN_vkGetPhysicalDeviceFeatures2)gipa(*out, "vkGetPhysicalDeviceFeatures2KHR"),
      .props = (PFN_vkGetPhysicalDeviceProperties)gipa(*out, "vkGetPhysicalDeviceProperties"),
  };
  pthread_mutex_lock(&lock);
  for (int i = 0; i < MAXI; i++)
    if (!insts[i].key) {
      insts[i] = in;
      break;
    }
  pthread_mutex_unlock(&lock);
  return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL layer_DestroyInstance(VkInstance instance,
                                                        const VkAllocationCallbacks *alloc) {
  struct inst *in = inst_of(instance);
  if (!in)
    return;
  PFN_vkDestroyInstance destroy = in->destroy;
  pthread_mutex_lock(&lock);
  memset(in, 0, sizeof(*in));
  pthread_mutex_unlock(&lock);
  destroy(instance, alloc);
}

static VKAPI_ATTR VkResult VKAPI_CALL layer_EnumerateDeviceExtensionProperties(
    VkPhysicalDevice pd, const char *layer, uint32_t *count, VkExtensionProperties *props) {
  if (layer && !strcmp(layer, LAYER_NAME)) {
    *count = 0;
    return VK_SUCCESS;
  }
  struct inst *in = inst_of(pd);
  if (!in)
    return VK_ERROR_INITIALIZATION_FAILED;
  if (layer || !filtered(in, pd))
    return in->enum_ext(pd, layer, count, props);
  uint32_t n = 0;
  VkResult r = in->enum_ext(pd, NULL, &n, NULL);
  if (r != VK_SUCCESS)
    return r;
  VkExtensionProperties *all = calloc(n ? n : 1, sizeof(*all));
  if (!all)
    return VK_ERROR_OUT_OF_HOST_MEMORY;
  r = in->enum_ext(pd, NULL, &n, all);
  if (r < 0) {
    free(all);
    return r;
  }
  uint32_t kept = 0;
  for (uint32_t i = 0; i < n; i++)
    if (!hidden_name(all[i].extensionName))
      all[kept++] = all[i];
  if (!props) {
    *count = kept;
    free(all);
    return VK_SUCCESS;
  }
  uint32_t w = kept < *count ? kept : *count;
  memcpy(props, all, w * sizeof(*all));
  *count = w;
  free(all);
  return w < kept ? VK_INCOMPLETE : VK_SUCCESS;
}

static void scrub(VkPhysicalDeviceFeatures2 *f) {
  for (VkBaseOutStructure *s = (VkBaseOutStructure *)f->pNext; s; s = s->pNext)
    for (size_t i = 0; i < N(hidden_feat); i++)
      if (s->sType == hidden_feat[i].st)
        memset((char *)s + sizeof(VkBaseOutStructure), 0,
               hidden_feat[i].size - sizeof(VkBaseOutStructure));
}

static VKAPI_ATTR void VKAPI_CALL layer_GetPhysicalDeviceFeatures2(VkPhysicalDevice pd,
                                                                   VkPhysicalDeviceFeatures2 *f) {
  struct inst *in = inst_of(pd);
  if (!in)
    return;
  in->feat2(pd, f);
  if (filtered(in, pd))
    scrub(f);
}

static VKAPI_ATTR void VKAPI_CALL layer_GetPhysicalDeviceFeatures2KHR(VkPhysicalDevice pd,
                                                                      VkPhysicalDeviceFeatures2 *f) {
  struct inst *in = inst_of(pd);
  if (!in)
    return;
  (in->feat2_khr ? in->feat2_khr : in->feat2)(pd, f);
  if (filtered(in, pd))
    scrub(f);
}

/* True if a feature structure in the chain asks for any of its features. */
static int asks_hidden_feature(const void *chain) {
  for (const VkBaseInStructure *s = chain; s; s = s->pNext)
    for (size_t i = 0; i < N(hidden_feat); i++)
      if (s->sType == hidden_feat[i].st) {
        const VkBool32 *b = (const VkBool32 *)((const char *)s + sizeof(VkBaseInStructure));
        size_t nb = (hidden_feat[i].size - sizeof(VkBaseInStructure)) / sizeof(VkBool32);
        for (size_t k = 0; k < nb; k++)
          if (b[k] == VK_TRUE)
            return 1;
      }
  return 0;
}

static VKAPI_ATTR VkResult VKAPI_CALL layer_CreateDevice(VkPhysicalDevice pd,
                                                         const VkDeviceCreateInfo *ci,
                                                         const VkAllocationCallbacks *alloc,
                                                         VkDevice *out) {
  struct inst *in = inst_of(pd);
  if (!in)
    return VK_ERROR_INITIALIZATION_FAILED;
  if (filtered(in, pd)) {
    for (uint32_t i = 0; i < ci->enabledExtensionCount; i++)
      if (hidden_name(ci->ppEnabledExtensionNames[i]))
        return VK_ERROR_EXTENSION_NOT_PRESENT;
    if (asks_hidden_feature(ci->pNext))
      return VK_ERROR_FEATURE_NOT_PRESENT;
  }
  VkLayerDeviceCreateInfo *link = (VkLayerDeviceCreateInfo *)ci->pNext;
  while (link && !(link->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO &&
                   link->function == VK_LAYER_LINK_INFO))
    link = (VkLayerDeviceCreateInfo *)link->pNext;
  if (!link)
    return VK_ERROR_INITIALIZATION_FAILED;
  PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
  PFN_vkGetDeviceProcAddr gdpa = link->u.pLayerInfo->pfnNextGetDeviceProcAddr;
  link->u.pLayerInfo = link->u.pLayerInfo->pNext;
  PFN_vkCreateDevice create = (PFN_vkCreateDevice)gipa(in->instance, "vkCreateDevice");
  VkResult r = create(pd, ci, alloc, out);
  if (r != VK_SUCCESS)
    return r;
  struct dev d = {.key = key_of(*out),
                  .gdpa = gdpa,
                  .destroy = (PFN_vkDestroyDevice)gdpa(*out, "vkDestroyDevice")};
  pthread_mutex_lock(&lock);
  for (int i = 0; i < MAXD; i++)
    if (!devs[i].key) {
      devs[i] = d;
      break;
    }
  pthread_mutex_unlock(&lock);
  return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL layer_DestroyDevice(VkDevice device,
                                                      const VkAllocationCallbacks *alloc) {
  struct dev *d = dev_of(device);
  if (!d)
    return;
  PFN_vkDestroyDevice destroy = d->destroy;
  pthread_mutex_lock(&lock);
  memset(d, 0, sizeof(*d));
  pthread_mutex_unlock(&lock);
  destroy(device, alloc);
}

#define HOOK(fn)                                                                   \
  if (!strcmp(name, "vk" #fn))                                                     \
  return (PFN_vkVoidFunction)layer_##fn

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL layer_gdpa(VkDevice device, const char *name) {
  if (!strcmp(name, "vkGetDeviceProcAddr"))
    return (PFN_vkVoidFunction)layer_gdpa;
  HOOK(DestroyDevice);
  struct dev *d = dev_of(device);
  return d ? d->gdpa(device, name) : NULL;
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL layer_gipa(VkInstance instance, const char *name) {
  if (!strcmp(name, "vkGetInstanceProcAddr"))
    return (PFN_vkVoidFunction)layer_gipa;
  if (!strcmp(name, "vkGetDeviceProcAddr"))
    return (PFN_vkVoidFunction)layer_gdpa;
  HOOK(CreateInstance);
  HOOK(DestroyInstance);
  HOOK(EnumerateDeviceExtensionProperties);
  HOOK(GetPhysicalDeviceFeatures2);
  HOOK(GetPhysicalDeviceFeatures2KHR);
  HOOK(CreateDevice);
  HOOK(DestroyDevice);
  if (!instance)
    return NULL;
  struct inst *in = inst_of(instance);
  return in ? in->gipa(instance, name) : NULL;
}

EXPORT VKAPI_ATTR VkResult VKAPI_CALL
vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
  if (v->sType != LAYER_NEGOTIATE_INTERFACE_STRUCT || v->loaderLayerInterfaceVersion < 2)
    return VK_ERROR_INITIALIZATION_FAILED;
  v->loaderLayerInterfaceVersion = 2;
  v->pfnGetInstanceProcAddr = layer_gipa;
  v->pfnGetDeviceProcAddr = layer_gdpa;
  v->pfnGetPhysicalDeviceProcAddr = NULL;
  return VK_SUCCESS;
}
