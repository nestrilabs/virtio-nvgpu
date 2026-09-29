// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-rm-smoke: the smallest RM client, and a DRM ioctl, from a process of
 * either width. Built 64- and 32-bit (nvgpu-rm-smoke-32) from this one file,
 * so a 32-bit run exercises the guest module's compat_ioctl on
 * /dev/nvidiactl and the DRM node with nothing but the width changed:
 *
 *   /dev/nvidiactl  CHECK_VERSION_STR (query), RM_ALLOC of an
 *                   NV01_ROOT_CLIENT (NVOS64), RM_CONTROL
 *                   NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS (flat) and
 *                   SYSTEM_GET_BUILD_VERSION (three string pointers, below
 *                   4 GiB in a 32-bit process), RM_FREE;
 *   render node     nvidia-drm GET_DEV_INFO (the 36-byte layout) and
 *                   DRM_IOCTL_GET_CAP.
 *
 * The last line, "RESULT ...", is the same for both widths on one guest,
 * which the probe compares. Exit status: the number of failed steps.
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_CHECK_VERSION_STR (200 + 10)
#define NV01_ROOT_CLIENT 0x41u
#define NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION 0x101u
#define NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS 0x201u
#define NV_RM_API_VERSION_CMD_QUERY '2'

/* nvos.h and nv-ioctl.h, 595.99.02: fixed-width, 8-byte-aligned pointers, so
 * one layout for both widths -- which is why nvidia.ko's compat_ioctl is its
 * native handler. */
struct nvos64 {
  uint32_t hRoot, hObjectParent, hObjectNew, hClass;
  uint64_t pAllocParms, pRightsRequested;
  uint32_t paramsSize, flags, status, pad;
};
struct nvos54 {
  uint32_t hClient, hObject, cmd, flags;
  uint64_t params;
  uint32_t paramsSize, status;
};
struct nvos00 {
  uint32_t hRoot, hObjectParent, hObjectOld, status;
};
struct rm_api_version {
  uint32_t cmd, reply;
  char versionString[64];
};
struct build_version {
  uint32_t sizeOfStrings, pad;
  uint64_t pDriverVersionBuffer, pVersionBuffer, pTitleBuffer;
  uint32_t changelistNumber, officialChangelistNumber;
};
struct dev_info36 {
  uint32_t gpu_id, mig_device, primary_index, supports_alloc;
  uint32_t generic_page_kind, page_kind_generation, sector_layout;
  uint32_t supports_sync_fd, supports_semsurf;
};
struct get_cap {
  uint64_t capability, value;
};

_Static_assert(sizeof(struct nvos64) == 48, "NVOS64");
_Static_assert(sizeof(struct nvos54) == 32, "NVOS54");
_Static_assert(sizeof(struct build_version) == 40, "GET_BUILD_VERSION");

#define NVIOC(nr, size) _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, nr, size)

static int fails;
static void step(int ok, const char *what, const char *detail) {
  printf("%s %s%s%s\n", ok ? "PASS" : "FAIL", what, detail ? ": " : "",
         detail ? detail : "");
  fflush(stdout);
  fails += !ok;
}

static int control(int fd, uint32_t client, uint32_t cmd, void *p, uint32_t size,
                   uint32_t *status) {
  struct nvos54 c = {.hClient = client, .hObject = client, .cmd = cmd,
                     .params = (uintptr_t)p, .paramsSize = size};
  int r = ioctl(fd, NVIOC(NV_ESC_RM_CONTROL, sizeof(c)), &c);

  *status = c.status;
  return r ? -errno : 0;
}

int main(int argc, char **argv) {
  const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
  char buf[160], drv[64] = "", ver[64] = "", title[64] = "";
  uint32_t ids[32], status, client = 0, gpu0 = 0xffffffffu, gpu_id = 0;
  struct rm_api_version q = {.cmd = NV_RM_API_VERSION_CMD_QUERY};
  struct build_version bv = {0};
  struct nvos64 a = {.hClass = NV01_ROOT_CLIENT};
  struct nvos00 f;
  int ctl, r;

  printf("nvgpu-rm-smoke: %zu-bit\n", sizeof(void *) * 8);
  ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
  if (ctl < 0) {
    snprintf(buf, sizeof(buf), "%s", strerror(errno));
    step(0, "open /dev/nvidiactl", buf);
    return 1;
  }

  r = ioctl(ctl, NVIOC(NV_ESC_CHECK_VERSION_STR, sizeof(q)), &q);
  q.versionString[sizeof(q.versionString) - 1] = 0;
  snprintf(buf, sizeof(buf), "%s, reply %u, \"%s\"", r ? strerror(errno) : "ok",
           q.reply, q.versionString);
  step(r == 0, "CHECK_VERSION_STR (query)", buf);

  r = ioctl(ctl, NVIOC(NV_ESC_RM_ALLOC, sizeof(a)), &a);
  client = a.hObjectNew;
  snprintf(buf, sizeof(buf), "%s, status 0x%x, hClient 0x%x",
           r ? strerror(errno) : "ok", a.status, client);
  step(r == 0 && a.status == 0 && client, "RM_ALLOC NV01_ROOT_CLIENT", buf);

  if (client) {
    memset(ids, 0xff, sizeof(ids));
    r = control(ctl, client, NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS, ids,
                sizeof(ids), &status);
    gpu0 = ids[0];
    snprintf(buf, sizeof(buf), "%s, status 0x%x, first gpuId 0x%x",
             r ? strerror(-r) : "ok", status, gpu0);
    step(r == 0 && status == 0 && gpu0 != 0xffffffffu,
         "RM_CONTROL GPU_GET_ATTACHED_IDS", buf);

    bv.sizeOfStrings = sizeof(drv);
    bv.pDriverVersionBuffer = (uintptr_t)drv;
    bv.pVersionBuffer = (uintptr_t)ver;
    bv.pTitleBuffer = (uintptr_t)title;
    r = control(ctl, client, NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION, &bv,
                sizeof(bv), &status);
    snprintf(buf, sizeof(buf), "%s, status 0x%x, \"%s\" \"%s\" \"%s\"",
             r ? strerror(-r) : "ok", status, drv, ver, title);
    step(r == 0 && status == 0 && drv[0] && !strcmp(drv, ver),
         "RM_CONTROL SYSTEM_GET_BUILD_VERSION (string pointers)", buf);

    f = (struct nvos00){.hRoot = client, .hObjectParent = client,
                        .hObjectOld = client};
    r = ioctl(ctl, NVIOC(NV_ESC_RM_FREE, sizeof(f)), &f);
    snprintf(buf, sizeof(buf), "%s, status 0x%x", r ? strerror(errno) : "ok",
             f.status);
    step(r == 0 && f.status == 0, "RM_FREE of the client", buf);
  }
  close(ctl);

  int dfd = open(node, O_RDWR | O_CLOEXEC);
  if (dfd < 0) {
    snprintf(buf, sizeof(buf), "%s: %s", node, strerror(errno));
    step(0, "open the render node", buf);
  } else {
    struct dev_info36 di = {0};
    struct get_cap cap = {.capability = 0x5 /* DRM_CAP_PRIME */};

    r = ioctl(dfd, _IOWR('d', 0x43, struct dev_info36), &di);
    gpu_id = di.gpu_id;
    snprintf(buf, sizeof(buf), "%s, gpu_id 0x%x, supports_alloc %u",
             r ? strerror(errno) : "ok", di.gpu_id, di.supports_alloc);
    step(r == 0 && di.gpu_id, "nvidia-drm GET_DEV_INFO", buf);
    r = ioctl(dfd, _IOWR('d', 0x0c, struct get_cap), &cap);
    snprintf(buf, sizeof(buf), "%s, value 0x%" PRIx64, r ? strerror(errno) : "ok",
             cap.value);
    step(r == 0 && cap.value, "DRM_IOCTL_GET_CAP(PRIME)", buf);
    close(dfd);
  }

  printf("RESULT gpu=0x%x drm_gpu=0x%x version=%s failed=%d\n", gpu0, gpu_id,
         drv[0] ? drv : "-", fails);
  return fails;
}
