// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-map-churn: map, move and unmap video memory the way NVIDIA's
 * user-mode driver does, many times in one process, and say how far it got.
 *
 *   nvgpu-map-churn [iterations [MiB per mapping]]    (default 400 x 2)
 *
 * Each iteration allocates video memory (NV01_MEMORY_LOCAL_USER), arms an
 * RM_MAP_MEMORY of it on a fresh /dev/nvidia0 descriptor, mmap()s that
 * descriptor, tells RM where with UPDATE_DEVICE_MAPPING_INFO (old: the
 * address the map returned, new: the mmap address), munmap()s it and
 * RM_UNMAP_MEMORYs it by the new address -- the sequence a client that
 * streams through mapped buffers repeats. The descriptors and the memory
 * are kept, as a client that caches them does.
 *
 * Natively, and on a backend that follows the UPDATE, every iteration
 * succeeds and nothing accumulates. On one that does not, each unmap misses
 * ("UNMAP_MEMORY: no mapping for pLinearAddress"), the window extent stays
 * charged to this process, and once it holds its share of the
 * write-combining zone the next map fails with ENOMEM ("SHM alloc failed").
 *
 * Exit status: 0 if every iteration and every unmap succeeded, 1
 * otherwise. Raw ioctls,
 * no libraries: the parameter layouts are RM's (nvos.h) for x86-64.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_CARD_INFO 200
#define NV_ESC_REGISTER_FD 201
#define NV_ESC_SYS_PARAMS 214
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_ALLOC 0x2B
#define NV_ESC_RM_MAP_MEMORY 0x4E
#define NV_ESC_RM_UNMAP_MEMORY 0x4F
#define NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO 0x5E

#define NV01_ROOT_CLIENT 0x41
#define NV01_DEVICE_0 0x80
#define NV20_SUBDEVICE_0 0x2080
#define NV01_MEMORY_LOCAL_USER 0x40

#define NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, (nr), (size))

struct nvos64 { /* NVOS64_PARAMETERS */
  uint32_t hRoot, hObjectParent, hObjectNew, hClass;
  uint64_t pAllocParms, pRightsRequested;
  uint32_t paramsSize, flags, status, _pad;
};

struct nvos00 { /* NVOS00_PARAMETERS: RM_FREE */
  uint32_t hRoot, hObjectParent, hObjectOld, status;
};

struct nvos33 { /* NVOS33_PARAMETERS_WITH_FD */
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t offset, length, pLinearAddress;
  uint32_t status, flags;
  int32_t fd, _pad2;
};

struct nvos34 { /* NVOS34_PARAMETERS */
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t pLinearAddress;
  uint32_t status, flags;
};

struct nvos56 { /* NVOS56_PARAMETERS */
  uint32_t hClient, hDevice, hMemory, _pad;
  uint64_t pOldCpuAddress, pNewCpuAddress;
  uint32_t status, _pad2;
};

struct mem_alloc { /* NV_MEMORY_ALLOCATION_PARAMS */
  uint32_t owner, type, flags, width, height;
  int32_t pitch;
  uint32_t attr, attr2, format, comprCovg, zcullCovg, _pad;
  uint64_t rangeLo, rangeHi, size, alignment, offset, limit, address;
  uint32_t ctagOffset, hVASpace, internalflags, tag;
  int32_t numaNode, _pad2;
};

_Static_assert(sizeof(struct nvos64) == 48, "NVOS64");
_Static_assert(sizeof(struct nvos33) == 56, "NVOS33 with fd");
_Static_assert(sizeof(struct nvos34) == 32, "NVOS34");
_Static_assert(sizeof(struct nvos56) == 40, "NVOS56");
_Static_assert(sizeof(struct mem_alloc) == 128, "NV_MEMORY_ALLOCATION_PARAMS");

static int ctl = -1;

static uint32_t rm_alloc(uint32_t root, uint32_t parent, uint32_t handle, uint32_t cls,
                         void *params, uint32_t size) {
  struct nvos64 p = {root, parent, handle, cls, (uintptr_t)params, 0, size, 0, 0, 0};
  if (ioctl(ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(p)), &p) < 0) {
    fprintf(stderr, "RM_ALLOC class %#x: %s\n", cls, strerror(errno));
    return 0;
  }
  if (p.status) {
    fprintf(stderr, "RM_ALLOC class %#x: RM status %#x\n", cls, p.status);
    return 0;
  }
  return p.hObjectNew;
}

int main(int argc, char **argv) {
  int iterations = argc > 1 ? atoi(argv[1]) : 400;
  uint64_t len = (uint64_t)(argc > 2 ? atoi(argv[2]) : 2) << 20;

  ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  if (ctl < 0) {
    perror("/dev/nvidiactl");
    return 1;
  }
  /* What the driver always issues first; without them the device
   * allocation is refused. */
  static uint8_t sys[8], card[2304];
  ioctl(ctl, NV_IOWR(NV_ESC_SYS_PARAMS, sizeof(sys)), sys);
  ioctl(ctl, NV_IOWR(NV_ESC_CARD_INFO, sizeof(card)), card);
  int gpu = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
  if (gpu < 0) {
    perror("/dev/nvidia0");
    return 1;
  }
  int32_t reg = ctl;
  if (ioctl(gpu, NV_IOWR(NV_ESC_REGISTER_FD, sizeof(reg)), &reg) < 0) {
    perror("REGISTER_FD");
    return 1;
  }

  uint32_t client = rm_alloc(0, 0, 0, NV01_ROOT_CLIENT, NULL, 0);
  uint8_t devp[56] = {0};
  memcpy(devp + 4, &client, 4); /* hClientShare */
  uint32_t device = client ? rm_alloc(client, client, 0x5c000001, NV01_DEVICE_0, devp, 0) : 0;
  uint32_t sub_id = 0;
  uint32_t subdevice =
      device ? rm_alloc(client, device, 0x5c000002, NV20_SUBDEVICE_0, &sub_id, 0) : 0;
  if (!subdevice)
    return 1;
  printf("client %#x device %#x subdevice %#x; %d x %llu MiB\n", client, device, subdevice,
         iterations, (unsigned long long)(len >> 20));

  int done = 0, unmap_failed = 0;
  for (int i = 0; i < iterations; i++) {
    struct mem_alloc m = {0};
    m.type = 0;                             /* NVOS32_TYPE_IMAGE */
    m.attr = (0u << 25) | (1u << 23);       /* LOCATION_VIDMEM, PAGE_SIZE_4KB */
    m.size = len;
    uint32_t mem = rm_alloc(client, device, 0xcaf00000u + i, NV01_MEMORY_LOCAL_USER, &m,
                            sizeof(m));
    if (!mem)
      break;

    int mfd = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
    int32_t r = ctl;
    if (mfd < 0 || ioctl(mfd, NV_IOWR(NV_ESC_REGISTER_FD, sizeof(r)), &r) < 0) {
      fprintf(stderr, "iteration %d: map descriptor: %s\n", i, strerror(errno));
      break;
    }

    struct nvos33 map = {client, subdevice, mem, 0, 0, len, 0, 0, 0x03080002, mfd, 0};
    if (ioctl(ctl, NV_IOWR(NV_ESC_RM_MAP_MEMORY, sizeof(map)), &map) < 0) {
      fprintf(stderr, "iteration %d: RM_MAP_MEMORY: %s\n", i, strerror(errno));
      break;
    }
    if (map.status) {
      fprintf(stderr, "iteration %d: RM_MAP_MEMORY: RM status %#x\n", i, map.status);
      break;
    }
    void *va = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if (va == MAP_FAILED) {
      fprintf(stderr, "iteration %d: mmap: %s\n", i, strerror(errno));
      break;
    }
    ((volatile uint32_t *)va)[0] = 0x600df00d + i;

    struct nvos56 upd = {client, subdevice, mem, 0, map.pLinearAddress, (uintptr_t)va, 0, 0};
    if (ioctl(ctl, NV_IOWR(NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO, sizeof(upd)), &upd) < 0 ||
        upd.status) {
      fprintf(stderr, "iteration %d: UPDATE_DEVICE_MAPPING_INFO: %s, RM status %#x\n", i,
              strerror(errno), upd.status);
      break;
    }
    munmap(va, len);
    /* A failed unmap is counted and passed over, as the user-mode driver
     * does: it has nothing to do about one. */
    struct nvos34 un = {client, subdevice, mem, 0, (uintptr_t)va, 0, 0};
    if (ioctl(ctl, NV_IOWR(NV_ESC_RM_UNMAP_MEMORY, sizeof(un)), &un) < 0 || un.status) {
      if (unmap_failed++ == 0)
        fprintf(stderr, "iteration %d: RM_UNMAP_MEMORY by %p: %s, RM status %#x\n", i, va,
                strerror(errno), un.status);
    }
    done++;
  }
  printf("NVGPU_MAP_CHURN iterations=%d of %d unmaps_failed=%d (%llu MiB mapped in all)\n",
         done, iterations, unmap_failed, (unsigned long long)((uint64_t)done * (len >> 20)));
  return done == iterations && unmap_failed == 0 ? 0 : 1;
}
