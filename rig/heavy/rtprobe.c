// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-rtprobe -- does hardware ray tracing work, and how fast: a Vulkan
 * device with VK_KHR_acceleration_structure and VK_KHR_ray_query, a bottom
 * level structure of GL_TRIS random triangles built on the GPU, a top level
 * of 64 instances of it, and a compute shader casting a ray a pixel of a
 * 1920x1080 image into them, submitted and waited for, for 5 s. The same
 * binary runs natively and in a guest (rig/heavy/extras.nix).
 *
 * Output: HEAVY_RT lines -- what the device offers, whether it could be
 * created with the extensions (the VkResult if not), the build time, and
 * rays a second. Exit status 0 only if rays were cast.
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#include "rtprobe-spv.h"

#define VK(x)                                                                      \
  do {                                                                             \
    VkResult r_ = (x);                                                             \
    if (r_ != VK_SUCCESS) {                                                        \
      printf("HEAVY_RT fail %s = %d (line %d)\n", #x, r_, __LINE__);               \
      exit(1);                                                                     \
    }                                                                              \
  } while (0)

static double now_s(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static VkDevice dev;
static VkPhysicalDeviceMemoryProperties mp;

static uint32_t mtype(uint32_t bits, VkMemoryPropertyFlags want) {
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
      return i;
  printf("HEAVY_RT fail no memory type\n");
  exit(1);
}

struct buf {
  VkBuffer b;
  VkDeviceMemory m;
  VkDeviceAddress a;
  void *p;
};

static struct buf mkbuf(VkDeviceSize size, VkBufferUsageFlags usage, int host) {
  struct buf r = {0};
  VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                            .size = size,
                            .usage = usage | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT};
  VK(vkCreateBuffer(dev, &bci, NULL, &r.b));
  VkMemoryRequirements mr;
  vkGetBufferMemoryRequirements(dev, r.b, &mr);
  VkMemoryAllocateFlagsInfo fi = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
                                  .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT};
  VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &fi,
      .allocationSize = mr.size,
      .memoryTypeIndex = mtype(mr.memoryTypeBits, host ? VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                             VK_MEMORY_PROPERTY_HOST_COHERENT_BIT
                                                       : VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
  VK(vkAllocateMemory(dev, &mai, NULL, &r.m));
  VK(vkBindBufferMemory(dev, r.b, r.m, 0));
  if (host)
    VK(vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, &r.p));
  VkBufferDeviceAddressInfo ai = {.sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
                                  .buffer = r.b};
  r.a = vkGetBufferDeviceAddress(dev, &ai);
  return r;
}

int main(void) {
  enum { W = 1920, H = 1080, INST = 64 };
  const char *e = getenv("GL_TRIS");
  uint32_t ntri = e ? (uint32_t)atoi(e) : 200000;
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .pApplicationName = "nvgpu-rtprobe",
                           .apiVersion = VK_API_VERSION_1_3};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                              .pApplicationInfo = &app};
  VkInstance inst;
  VK(vkCreateInstance(&ici, NULL, &inst));
  VkPhysicalDevice pds[8], pd = VK_NULL_HANDLE;
  uint32_t n = 8;
  VK(vkEnumeratePhysicalDevices(inst, &n, pds));
  for (uint32_t i = 0; i < n && !pd; i++) {
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties(pds[i], &p);
    if (p.vendorID == 0x10de)
      pd = pds[i];
  }
  if (!pd) {
    printf("HEAVY_RT fail no NVIDIA device\n");
    return 1;
  }
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  /* What the device lists, for the extensions DLSS, Frame Generation and
   * ray tracing need. */
  static const char *const watch[] = {
      "VK_KHR_acceleration_structure", "VK_KHR_ray_query",       "VK_KHR_ray_tracing_pipeline",
      "VK_NVX_binary_import",          "VK_NVX_image_view_handle", "VK_NV_cuda_kernel_launch",
      "VK_NV_optical_flow",            "VK_NV_low_latency2",     "VK_KHR_deferred_host_operations"};
  VkExtensionProperties ext[512];
  uint32_t ne = 512;
  VK(vkEnumerateDeviceExtensionProperties(pd, NULL, &ne, ext));
  printf("HEAVY_RT extensions=%u listed:", ne);
  for (size_t w = 0; w < sizeof(watch) / sizeof(watch[0]); w++) {
    int have = 0;
    for (uint32_t i = 0; i < ne; i++)
      have |= !strcmp(ext[i].extensionName, watch[w]);
    printf(" %s=%d", watch[w], have);
  }
  printf("\n");

  uint32_t qfam = 0, nq = 16;
  VkQueueFamilyProperties qf[16];
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
  for (uint32_t i = 0; i < nq; i++)
    if (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) {
      qfam = i;
      break;
    }
  float prio = 1;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = qfam,
                                 .queueCount = 1,
                                 .pQueuePriorities = &prio};
  /* The features the device reports for them. */
  VkPhysicalDeviceRayTracingPipelineFeaturesKHR fp = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_PIPELINE_FEATURES_KHR};
  VkPhysicalDeviceRayQueryFeaturesKHR fq = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR, .pNext = &fp};
  VkPhysicalDeviceAccelerationStructureFeaturesKHR fa = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR, .pNext = &fq};
  VkPhysicalDeviceFeatures2 f2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
                                  .pNext = &fa};
  vkGetPhysicalDeviceFeatures2(pd, &f2);
  printf("HEAVY_RT features accelerationStructure=%u rayQuery=%u rayTracingPipeline=%u\n",
         fa.accelerationStructure, fq.rayQuery, fp.rayTracingPipeline);
  /* Each of them alone (with what it depends on): which ones a device can
   * be made with. A game enables what is listed, and a device that cannot
   * be made is a game that does not start or falls back. */
  printf("HEAVY_RT device_with:");
  for (size_t w = 0; w < sizeof(watch) / sizeof(watch[0]); w++) {
    const char *one[3] = {watch[w], "VK_KHR_deferred_host_operations",
                          "VK_KHR_acceleration_structure"};
    uint32_t n1 = 1;
    int rt = !strncmp(watch[w], "VK_KHR_ray", 10);
    if (!strcmp(watch[w], "VK_KHR_acceleration_structure"))
      n1 = 2;
    else if (rt)
      n1 = 3;
    VkPhysicalDeviceVulkan12Features bda = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .bufferDeviceAddress = VK_TRUE};
    VkDeviceCreateInfo d1 = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                             .pNext = &bda,
                             .queueCreateInfoCount = 1,
                             .pQueueCreateInfos = &qci,
                             .enabledExtensionCount = n1,
                             .ppEnabledExtensionNames = one};
    VkDevice dv;
    VkResult r1 = vkCreateDevice(pd, &d1, NULL, &dv);
    if (r1 == VK_SUCCESS)
      vkDestroyDevice(dv, NULL);
    printf(" %s=%d", watch[w], r1);
  }
  printf("\n");
  VkPhysicalDeviceRayQueryFeaturesKHR rq = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR, .rayQuery = VK_TRUE};
  VkPhysicalDeviceAccelerationStructureFeaturesKHR asf = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
      .pNext = &rq,
      .accelerationStructure = VK_TRUE};
  VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                                          .pNext = &asf,
                                          .bufferDeviceAddress = VK_TRUE};
  const char *dext[] = {"VK_KHR_acceleration_structure", "VK_KHR_ray_query",
                        "VK_KHR_deferred_host_operations"};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .pNext = &f12,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qci,
                            .enabledExtensionCount = 3,
                            .ppEnabledExtensionNames = dext};
  VkResult dr = vkCreateDevice(pd, &dci, NULL, &dev);
  printf("HEAVY_RT device_with_ray_query=%s (%d)\n", dr == VK_SUCCESS ? "ok" : "FAILED", dr);
  if (dr != VK_SUCCESS)
    return 2;
  VkQueue q;
  vkGetDeviceQueue(dev, qfam, 0, &q);
#define PFN(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
  PFN(vkGetAccelerationStructureBuildSizesKHR);
  PFN(vkCreateAccelerationStructureKHR);
  PFN(vkCmdBuildAccelerationStructuresKHR);
  PFN(vkGetAccelerationStructureDeviceAddressKHR);

  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                 .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                 .queueFamilyIndex = qfam};
  VkCommandPool pool;
  VK(vkCreateCommandPool(dev, &cpi, NULL, &pool));
  VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                     .commandPool = pool,
                                     .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                     .commandBufferCount = 1};
  VkCommandBuffer cb;
  VK(vkAllocateCommandBuffers(dev, &cai, &cb));
  VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
  VkFence fence;
  VK(vkCreateFence(dev, &fci, NULL, &fence));
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                 .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                     .commandBufferCount = 1,
                     .pCommandBuffers = &cb};

  /* Random triangles in a 100-unit cube. */
  struct buf vb = mkbuf((VkDeviceSize)ntri * 9 * sizeof(float),
                        VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR, 1);
  float *v = vb.p;
  uint32_t s = 12345;
  for (uint32_t t = 0; t < ntri; t++) {
    float c[3];
    for (int k = 0; k < 3; k++) {
      s = s * 1664525u + 1013904223u;
      c[k] = (float)(s >> 8) / (float)(1 << 24) * 100.0f - 50.0f;
    }
    for (int p = 0; p < 3; p++)
      for (int k = 0; k < 3; k++) {
        s = s * 1664525u + 1013904223u;
        v[t * 9 + p * 3 + k] = c[k] + (float)(s >> 8) / (float)(1 << 24) * 2.0f - 1.0f;
      }
  }
  VkAccelerationStructureGeometryKHR geo = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
      .geometryType = VK_GEOMETRY_TYPE_TRIANGLES_KHR,
      .geometry.triangles = {.sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_TRIANGLES_DATA_KHR,
                             .vertexFormat = VK_FORMAT_R32G32B32_SFLOAT,
                             .vertexData.deviceAddress = vb.a,
                             .vertexStride = 12,
                             .maxVertex = ntri * 3 - 1,
                             .indexType = VK_INDEX_TYPE_NONE_KHR},
      .flags = VK_GEOMETRY_OPAQUE_BIT_KHR};
  VkAccelerationStructureBuildGeometryInfoKHR bg = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
      .flags = VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR,
      .mode = VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR,
      .geometryCount = 1,
      .pGeometries = &geo};
  VkAccelerationStructureBuildSizesInfoKHR sz = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_SIZES_INFO_KHR};
  vkGetAccelerationStructureBuildSizesKHR(dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &bg,
                                          &ntri, &sz);
  struct buf blasb = mkbuf(sz.accelerationStructureSize,
                           VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR, 0);
  struct buf scr = mkbuf(sz.buildScratchSize, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 0);
  VkAccelerationStructureCreateInfoKHR aci = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR,
      .buffer = blasb.b,
      .size = sz.accelerationStructureSize,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR};
  VkAccelerationStructureKHR blas;
  VK(vkCreateAccelerationStructureKHR(dev, &aci, NULL, &blas));
  bg.dstAccelerationStructure = blas;
  bg.scratchData.deviceAddress = scr.a;
  VkAccelerationStructureBuildRangeInfoKHR br = {.primitiveCount = ntri};
  const VkAccelerationStructureBuildRangeInfoKHR *brp = &br;
  double t0 = now_s();
  VK(vkBeginCommandBuffer(cb, &bi));
  vkCmdBuildAccelerationStructuresKHR(cb, 1, &bg, &brp);
  VK(vkEndCommandBuffer(cb));
  VK(vkQueueSubmit(q, 1, &si, fence));
  VK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
  VK(vkResetFences(dev, 1, &fence));
  double t_blas = now_s() - t0;

  /* 64 instances of it, on a grid. */
  VkAccelerationStructureDeviceAddressInfoKHR dai = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_DEVICE_ADDRESS_INFO_KHR,
      .accelerationStructure = blas};
  VkDeviceAddress blas_a = vkGetAccelerationStructureDeviceAddressKHR(dev, &dai);
  struct buf ib = mkbuf(INST * sizeof(VkAccelerationStructureInstanceKHR),
                        VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR, 1);
  VkAccelerationStructureInstanceKHR *in = ib.p;
  for (int i = 0; i < INST; i++) {
    memset(&in[i], 0, sizeof(in[i]));
    in[i].transform.matrix[0][0] = in[i].transform.matrix[1][1] = in[i].transform.matrix[2][2] = 1;
    in[i].transform.matrix[0][3] = (float)(i % 8) * 110.0f - 385.0f;
    in[i].transform.matrix[1][3] = (float)(i / 8) * 110.0f - 385.0f;
    in[i].transform.matrix[2][3] = 400.0f;
    in[i].mask = 0xff;
    in[i].flags = VK_GEOMETRY_INSTANCE_TRIANGLE_FACING_CULL_DISABLE_BIT_KHR;
    in[i].accelerationStructureReference = blas_a;
  }
  VkAccelerationStructureGeometryKHR tgeo = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
      .geometryType = VK_GEOMETRY_TYPE_INSTANCES_KHR,
      .geometry.instances = {.sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_INSTANCES_DATA_KHR,
                             .data.deviceAddress = ib.a}};
  VkAccelerationStructureBuildGeometryInfoKHR tg = bg;
  tg.type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR;
  tg.pGeometries = &tgeo;
  tg.dstAccelerationStructure = VK_NULL_HANDLE;
  uint32_t ninst = INST;
  VkAccelerationStructureBuildSizesInfoKHR tsz = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_SIZES_INFO_KHR};
  vkGetAccelerationStructureBuildSizesKHR(dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &tg,
                                          &ninst, &tsz);
  struct buf tlasb = mkbuf(tsz.accelerationStructureSize,
                           VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR, 0);
  struct buf tscr = mkbuf(tsz.buildScratchSize, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 0);
  aci.buffer = tlasb.b;
  aci.size = tsz.accelerationStructureSize;
  aci.type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR;
  VkAccelerationStructureKHR tlas;
  VK(vkCreateAccelerationStructureKHR(dev, &aci, NULL, &tlas));
  tg.dstAccelerationStructure = tlas;
  tg.scratchData.deviceAddress = tscr.a;
  br.primitiveCount = INST;
  VK(vkBeginCommandBuffer(cb, &bi));
  vkCmdBuildAccelerationStructuresKHR(cb, 1, &tg, &brp);
  VK(vkEndCommandBuffer(cb));
  VK(vkQueueSubmit(q, 1, &si, fence));
  VK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
  VK(vkResetFences(dev, 1, &fence));

  /* The compute pipeline: binding 0 the TLAS, binding 1 the output. */
  struct buf out = mkbuf((VkDeviceSize)W * H * 4, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 0);
  VkDescriptorSetLayoutBinding bnd[2] = {
      {0, VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL},
      {1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL}};
  VkDescriptorSetLayoutCreateInfo dli = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                                         .bindingCount = 2,
                                         .pBindings = bnd};
  VkDescriptorSetLayout dsl;
  VK(vkCreateDescriptorSetLayout(dev, &dli, NULL, &dsl));
  VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                    .setLayoutCount = 1,
                                    .pSetLayouts = &dsl};
  VkPipelineLayout pl;
  VK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
  VkShaderModuleCreateInfo smi = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                  .codeSize = sizeof(rt_cs),
                                  .pCode = rt_cs};
  VkShaderModule sm;
  VK(vkCreateShaderModule(dev, &smi, NULL, &sm));
  VkComputePipelineCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
                                      .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                                                .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                                                .module = sm,
                                                .pName = "main"},
                                      .layout = pl};
  VkPipeline pipe;
  VK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));
  VkDescriptorPoolSize ps[2] = {{VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR, 1},
                                {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1}};
  VkDescriptorPoolCreateInfo dpi = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                                    .maxSets = 1,
                                    .poolSizeCount = 2,
                                    .pPoolSizes = ps};
  VkDescriptorPool dp;
  VK(vkCreateDescriptorPool(dev, &dpi, NULL, &dp));
  VkDescriptorSetAllocateInfo dsai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                                      .descriptorPool = dp,
                                      .descriptorSetCount = 1,
                                      .pSetLayouts = &dsl};
  VkDescriptorSet ds;
  VK(vkAllocateDescriptorSets(dev, &dsai, &ds));
  VkWriteDescriptorSetAccelerationStructureKHR was = {
      .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_ACCELERATION_STRUCTURE_KHR,
      .accelerationStructureCount = 1,
      .pAccelerationStructures = &tlas};
  VkDescriptorBufferInfo dbi = {out.b, 0, VK_WHOLE_SIZE};
  VkWriteDescriptorSet wds[2] = {{.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                  .pNext = &was,
                                  .dstSet = ds,
                                  .dstBinding = 0,
                                  .descriptorCount = 1,
                                  .descriptorType = VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR},
                                 {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                  .dstSet = ds,
                                  .dstBinding = 1,
                                  .descriptorCount = 1,
                                  .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                  .pBufferInfo = &dbi}};
  vkUpdateDescriptorSets(dev, 2, wds, 0, NULL);
  VkCommandBufferBeginInfo rbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                  .flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT};
  VK(vkBeginCommandBuffer(cb, &rbi));
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
  vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
  vkCmdDispatch(cb, W / 8, H / 8, 1);
  VK(vkEndCommandBuffer(cb));
  double warm = now_s() + 1.0, end = warm + 5.0, t;
  long frames = 0;
  while ((t = now_s()) < end) {
    VK(vkQueueSubmit(q, 1, &si, fence));
    VK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
    VK(vkResetFences(dev, 1, &fence));
    if (t >= warm)
      frames++;
  }
  printf("HEAVY_RT ok triangles=%u instances=%d blas_build_ms=%.2f frames=%ld frame_ms=%.3f "
         "mrays_per_s=%.0f\n",
         ntri, INST, t_blas * 1e3, frames, 5000.0 / frames, frames * (double)W * H / 5.0 / 1e6);
  return 0;
}
