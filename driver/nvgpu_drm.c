// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the DRM side of the guest module.
 *
 * DRM device registration, open and release, the ioctl entry, and the
 * nvidia-drm driver-range ioctls. The GEM proxies that stand in front of the
 * host's objects (mmap, vmap, dma-buf export, PRIME import) are
 * nvgpu_gem.c's. An RM ioctl (any type but 'd') a render node receives
 * goes through nvgpu_ioctl_fd() like any other device's (nvgpu_rmio.c, or the
 * Rust build's dispatch.rs); the core's are drm_ioctl()'s, or a KMS file's
 * host card's (nvgpu_kms.c).
 */

#include <drm/drm.h>
#include <linux/atomic.h>
#include <linux/compat.h>
#include <linux/dma-buf.h>
#include <linux/fcntl.h>
#include <linux/fs.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/poll.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>
#include <linux/wait.h>

#include <drm/drm_auth.h>
#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_prime.h>

#include "nvgpu.h"

/* ───────── a DRM ioctl's argument, as drm_ioctl() takes it ───────── */

/*
 * drm_ioctl.c:848-915, for the ioctls this node answers or forwards itself
 * rather than through drm_ioctl() (which does the same for those it keeps).
 * Every DRM ioctl the node serves goes through here: nvidia-drm's range,
 * syncobjs, a KMS file's KMS calls, the dumb-buffer pair; so a struct that
 * grew (drm_syncobj_handle's `point`, 16 -> 24 bytes) reaches its handler
 * the way the native kernel hands it over, whichever size the caller's
 * headers had.
 */
int nvgpu_drm_arg_in(struct nvgpu_drm_arg *a, unsigned int ucmd,
                     unsigned int ncmd, void __user *u) {
  u32 in = _IOC_SIZE(ucmd), ksize;

  BUILD_BUG_ON(sizeof(a->stack) != 128);
  a->cmd = ncmd;
  a->u = u;
  a->out = _IOC_SIZE(ucmd);
  if (!(ucmd & ncmd & IOC_IN))
    in = 0;
  if (!(ucmd & ncmd & IOC_OUT))
    a->out = 0;
  ksize = max3(in, a->out, (u32)_IOC_SIZE(ncmd));
  if (ksize <= sizeof(a->stack)) {
    a->k = a->stack;
  } else {
    a->k = kmalloc(ksize, GFP_KERNEL);
    if (!a->k)
      return -ENOMEM;
  }
  if (copy_from_user(a->k, u, in)) {
    nvgpu_drm_arg_drop(a);
    return -EFAULT;
  }
  memset((u8 *)a->k + in, 0, ksize - in);
  return 0;
}

long nvgpu_drm_arg_out(struct nvgpu_drm_arg *a, long ret) {
  if (copy_to_user(a->u, a->k, a->out))
    ret = -EFAULT;
  nvgpu_drm_arg_drop(a);
  return ret;
}

void nvgpu_drm_arg_drop(struct nvgpu_drm_arg *a) {
  if (a->k != a->stack)
    kfree(a->k);
  a->k = NULL;
}

/* ───────── nvidia-drm's driver range ───────── */

/*
 * The largest nvidia-drm GEM parameter struct this driver forwards, and the
 * largest NVKMS block one of them may point at. The first is a stack buffer's
 * size, so it is small on purpose and checked against each descriptor that
 * uses it when the call runs (nvgpu_ioctl_drm_gem_nested()); the second only
 * has to refuse a length field that is garbage, since a real NVKMS
 * memory-import block is a few hundred bytes.
 */
#define NVGPU_GEM_OUTER_MAX 32
#define NVGPU_GEM_NESTED_MAX (64 * 1024)

static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void *karg,
                                       const struct nvgpu_gem_nested_desc *d);

/* Defined below; named here because the ioctls that test it come first. */
static const struct file_operations nvgpu_drm_fops;

/*
 * The nvidia-drm ioctls, answered here rather than through a drm_ioctl_desc
 * table. drm_ioctl() serves the core ones (VERSION and friends) and answers
 * -EINVAL for anything in the driver range, since this driver registers no
 * table of its own; nvgpu_drm_unlocked_ioctl() takes that range first and
 * leaves the rest to the core.
 *
 * All types used below are stable UAPI structs; we define only what we use.
 */

/* _IOC_TYPE byte for DRM ioctls */
#define DRM_IOCTL_BASE 'd'
#define DRM_COMMAND_BASE 0x40

/* DRM_NVIDIA_* are offsets from DRM_COMMAND_BASE */
#define DRM_NVIDIA_GET_DEV_INFO 0x03     /* abs nr 0x43 */
#define DRM_NVIDIA_FENCE_SUPPORTED 0x04  /* abs nr 0x44 */
#define DRM_NVIDIA_DMABUF_SUPPORTED 0x0f /* abs nr 0x4f */
#define DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID 0x18 /* abs nr 0x58 */
/* Semaphore-surface fences, nvgpu_semsurf.c. */
#define DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE 0x14 /* abs nr 0x54 */
#define DRM_NVIDIA_SEMSURF_FENCE_CREATE 0x15     /* abs nr 0x55 */
#define DRM_NVIDIA_SEMSURF_FENCE_WAIT 0x16       /* abs nr 0x56 */
#define DRM_NVIDIA_SEMSURF_FENCE_ATTACH 0x17     /* abs nr 0x57 */

/*
 * The GEM ioctls, which are what a swapchain is made of: allocate or import
 * memory, give it a fake mmap offset, hand it out as a dma-buf. They are
 * forwarded to the host's render node rather than answered here -- the memory
 * is the host's and so is the object that names it.
 *
 * Their GEM handles are translated: a handle the caller names is one of its
 * proxies, forwarded as the proxy's host handle on the proxy's owner file,
 * and a handle the host makes gets a proxy before the caller sees a number
 * (nvgpu_ioctl_drm_gem_nested()). A GEM handle is per drm_file, and each
 * open of this node holds one open of the host's node (nvgpu_drm_open), but
 * a proxy may stand for another file's object -- an imported buffer.
 */
#define DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY 0x01  /* abs nr 0x41 */
#define DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY 0x09  /* abs nr 0x49 */
#define DRM_NVIDIA_GEM_MAP_OFFSET 0x0a           /* abs nr 0x4a */
#define DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY 0x0b   /* abs nr 0x4b */
#define DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY 0x0d /* abs nr 0x4d */
#define DRM_NVIDIA_GEM_IDENTIFY_OBJECT 0x0e      /* abs nr 0x4e */

/*
 * struct drm_nvidia_gem_import_nvkms_memory_params:
 *   u64 mem_size; u64 nvkms_params_ptr; u64 nvkms_params_size;
 *   u32 handle; u32 __pad;
 */
static const struct nvgpu_gem_nested_desc nvgpu_gem_import_nvkms = {
    .size = 32,
    .ptr_offset = 8,
    .size_offset = 16,
    /* struct NvKmsKapiPrivImportMemoryParams { int memFd; ... } */
    .fd_offset = 0,
    .handle_offset = 24,
    .handle_is_out = true,
    .size_field_offset = 0, /* mem_size */
};

/*
 * struct drm_nvidia_gem_export_dmabuf_memory_params:
 *   u32 handle; u32 __pad; u64 nvkms_params_ptr; u64 nvkms_params_size;
 */
static const struct nvgpu_gem_nested_desc nvgpu_gem_export_dmabuf = {
    .size = 24,
    .ptr_offset = 8,
    .size_offset = 16,
    /* struct NvKmsKapiPrivExportMemoryParams { int memFd; } */
    .fd_offset = 0,
    .handle_offset = 0,
    .handle_is_out = false,
    .size_field_offset = NVGPU_GEM_NO_FIELD,
};

struct drm_nvidia_get_dev_info_params {
  __u32 gpu_id;
  __u32 mig_device;
  __u32 primary_index;
  __u32 supports_alloc;
  __u32 generic_page_kind;
  __u32 page_kind_generation;
  __u32 sector_layout;
  __u32 supports_sync_fd;
  __u32 supports_semsurf;
} __packed;

/*
 * GET_DEV_INFO, answered from the host's own node (the record the backend
 * sent in GET_SYS_FILES), in the layout the caller asked for.
 *
 * These fields describe how the card lays memory out, and the ICD matches a
 * DRM node to an RM device by the gpu_id among them, so none of them is ours
 * to invent -- the constants that used to be here reported gpu_id 0 where the
 * host says 0x100, and a page kind correct only on the two architectures the
 * comment named.
 *
 * The struct has had four layouts, and the size in the ioctl number is the
 * only thing that tells them apart (the backend's normalise_dev_info has the
 * table): 20 bytes on 535, 28 on 545.23, 32 on 545.29-570 (supports_alloc
 * after primary_index), 36 from 575 (mig_device after gpu_id). The record is
 * always the 36-byte one; each caller gets its own layout cut from it. This
 * used to write 36 bytes whatever the caller's struct was, which on a 535
 * guest is 16 bytes of its stack past a 20-byte struct, and the words in the
 * wrong places besides. Nothing is ever written past _IOC_SIZE:
 *
 *   20/28/32/36  that layout, from the record
 *   > 36         the 36-byte layout, the rest cleared -- what drm_ioctl does
 *                for a caller newer than its driver (drm_ioctl.c:911-912)
 *   otherwise    -EINVAL; there is no layout to answer in
 */
static long nvgpu_drm_get_dev_info(struct nvgpu_fd *nfd,
                                   struct nvgpu_dri_dev *dri,
                                   struct drm_file *file, unsigned int cmd,
                                   void __user *uarg) {
  struct drm_nvidia_get_dev_info_params r;
  unsigned int want = _IOC_SIZE(cmd);
  u32 out[NVGPU_DEV_INFO_WORDS];
  unsigned int n = 0;

  BUILD_BUG_ON(sizeof(r) != NVGPU_DEV_INFO_WORDS * sizeof(u32));
  /*
   * drm_ioctl() copies back only where the caller's command says OUT, and
   * _IOC_SIZE bytes of it (drm_ioctl.c:866-871): an _IO or _IOW caller, or
   * one of size 0, gets 0 and nothing written. This wrote the answer for an
   * _IOW, and refused size 0.
   */
  if (!(_IOC_DIR(cmd) & _IOC_READ) || !want)
    return 0;
  if (want != 20 && want != 28 && want != 32 && want < 36)
    return -EINVAL;
  memcpy(&r, dri->dev_info, sizeof(r));

  /*
   * The capability bits are the host's answer about the host's node, ANDed
   * with what this node forwards -- never set where the host says no. The
   * host clears all of them without an NVKMS device (nvidia_drm.modeset=0)
   * and the two fence bits without semaphore-surface support
   * (nvidia-drm-drv.c:1095-1116); a guest that claimed them anyway sent the
   * ICD to 0x54 on every vkCreateDevice, where the host answered -EOPNOTSUPP
   * (nvidia-drm-fence.c:1316-1318) and the device failed to create.
   *
   *   supports_alloc     GEM_ALLOC_NVKMS_MEMORY, GEM_MAP_OFFSET,
   *                      GEM_EXPORT_DMABUF_MEMORY (0x0b, 0x0a, 0x0d): the
   *                      host's
   *   supports_sync_fd   with fences on, the semsurf bit: nvidia-drm sets
   *   supports_semsurf   both from one condition, so no userspace has seen
   *                      one without the other, and 0x54..0x57 are forwarded
   *                      (nvgpu_semsurf.c). With fences off (a v1 backend),
   *                      neither: 0x54 is not served, nor the PRIME fence
   *                      pair behind sync_fd (0x45, 0x46).
   *
   * The page kinds are "only valid if supports_alloc is true"
   * (nv_drm_common_ioctl.h:219-221) and the host zeroes them when it is not;
   * zeroed here too when it is not set, in a layout that has the bit: 535's
   * and 545.23's report the kinds without one (supports_alloc there is the
   * backend's DMABUF_SUPPORTED answer, which says the same thing), and a
   * caller in those gets them as the host said.
   */
  if (want >= 32 && !r.supports_alloc) {
    r.generic_page_kind = 0;
    r.page_kind_generation = 0;
    r.sector_layout = 0;
  }
  if (nvgpu_fences_enabled(nfd->dev)) {
    r.supports_sync_fd = r.supports_semsurf;
  } else {
    r.supports_sync_fd = 0;
    r.supports_semsurf = 0;
  }

  /*
   * primary_index is the number of the DRM node this device is, and it has
   * to be *ours*. The host's number describes the host's /dev/dri, and the
   * ICD uses it to find the node in the guest's: it looks for card<N>,
   * does not find it, associates the device with no DRM node at all, and
   * then reports no dma-buf support -- so a compositor's only buffer path
   * is gone and nothing can present. The symptom is three steps from the
   * cause and names none of it:
   *
   *   drm props: hasPrimary=0 0:0  hasRender=0 0:0
   *   VK_EXT_external_memory_dma_buf absent
   *   vkcube: "Could not find both graphics and present queues"
   *
   * The two numbers agree only where the host's NVIDIA card is its card0; on
   * a host with an integrated GPU as well it is card1, and passing the
   * host's number through left nothing able to present.
   */
  if (file && file->minor && file->minor->dev && file->minor->dev->primary)
    r.primary_index = file->minor->dev->primary->index;

  out[n++] = r.gpu_id;
  if (want >= 36)
    out[n++] = r.mig_device;
  out[n++] = r.primary_index;
  if (want >= 32)
    out[n++] = r.supports_alloc;
  out[n++] = r.generic_page_kind;
  out[n++] = r.page_kind_generation;
  out[n++] = r.sector_layout;
  if (want >= 28) {
    out[n++] = r.supports_sync_fd;
    out[n++] = r.supports_semsurf;
  }

  /*
   * A caller whose layout is not the host's runs userspace from another
   * release than the host kernel. The answer above is still right for its
   * layout, but its RM calls will not be, and this is the one place the
   * mismatch is visible before they fail.
   */
  if (dri->dev_info_size && want != dri->dev_info_size)
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: GET_DEV_INFO asked in a %u-byte "
                         "layout, the host's is %u bytes: guest userspace "
                         "and host driver are different releases\n",
                         want, dri->dev_info_size);

  if (copy_to_user(uarg, out, n * sizeof(u32)))
    return -EFAULT;
  if (want > n * sizeof(u32) &&
      clear_user((u8 __user *)uarg + n * sizeof(u32), want - n * sizeof(u32)))
    return -EFAULT;
  return 0;
}

/*
 * The native command of each nvidia-drm ioctl this node answers or forwards
 * outside IOCTL2 (nv_drm_common_ioctl.h), which a caller's argument is
 * normalised to (struct nvgpu_drm_arg); 0 for the rest. GET_DEV_INFO is not
 * here: its layouts differ in the middle, not at the end, so it answers each
 * caller in its own (nvgpu_drm_get_dev_info()). FENCE_SUPPORTED and
 * DMABUF_SUPPORTED are _IO and touch no memory.
 */
static unsigned int nvgpu_drm_driver_cmd(unsigned int nr) {
#define NVGPU_DRV_IOWR(n, sz)                                                  \
  _IOC(_IOC_READ | _IOC_WRITE, DRM_IOCTL_BASE, DRM_COMMAND_BASE + (n), (sz))
  switch (nr - DRM_COMMAND_BASE) {
  case DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID, 8);
  case DRM_NVIDIA_GEM_IDENTIFY_OBJECT:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_IDENTIFY_OBJECT, 8);
  case DRM_NVIDIA_GEM_MAP_OFFSET:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_MAP_OFFSET, 16);
  case DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY, 24);
  case DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY,
                          nvgpu_gem_import_nvkms.size);
  case DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY,
                          nvgpu_gem_export_dmabuf.size);
  case DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY:
    return NVGPU_DRV_IOWR(DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY,
                          nvgpu_gem_export_dmabuf.size);
  case DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE:
  case DRM_NVIDIA_SEMSURF_FENCE_CREATE:
  case DRM_NVIDIA_SEMSURF_FENCE_WAIT:
  case DRM_NVIDIA_SEMSURF_FENCE_ATTACH:
    return nvgpu_fence_semsurf_cmd(nr);
  }
#undef NVGPU_DRV_IOWR
  return 0;
}

static long nvgpu_drm_driver_ioctl(struct nvgpu_fd *nfd,
                                   struct drm_file *file, unsigned int cmd,
                                   void *k);

/*
 * nvgpu_drm_handle_ioctl — nvidia-drm's driver range on our /dev/dri/..
 * nodes (DRM_IOCTL_VERSION is the core's, from nvgpu_drm_driver's fields).
 *
 * GET_DEV_INFO       (nr=0x43) — the host's record, in the caller's layout
 * FENCE_SUPPORTED    (nr=0x44) — -EINVAL: 0x45/0x46 are not forwarded
 * DMABUF_SUPPORTED   (nr=0x4f) — the host's answer (supports_alloc)
 * the rest of nvgpu_drm_driver_cmd()'s — on the caller's argument normalised
 *                    as drm_ioctl() would (nvgpu_drm_arg_in())
 *
 * Everything else → -ENOTTY.
 */
static long nvgpu_drm_handle_ioctl(struct nvgpu_fd *nfd,
                                   struct nvgpu_dri_dev *dri,
                                   struct drm_file *file, unsigned int cmd,
                                   unsigned long arg) {
  unsigned int nr = _IOC_NR(cmd), ncmd;
  void __user *uarg = (void __user *)arg;
  struct nvgpu_drm_arg a;
  long ret;

  /* ── Driver ioctls: DRM_COMMAND_BASE .. DRM_COMMAND_END ── */
  if (nr < DRM_COMMAND_BASE || nr >= DRM_COMMAND_END)
    return -ENOTTY;

  switch (nr - DRM_COMMAND_BASE) {
  case DRM_NVIDIA_GET_DEV_INFO:
    return nvgpu_drm_get_dev_info(nfd, dri, file, cmd, uarg);

  /*
   * FENCE_SUPPORTED answers for the PRIME fence pair behind it,
   * PRIME_FENCE_CONTEXT_CREATE and GEM_PRIME_FENCE_ATTACH (0x45, 0x46), and
   * this node forwards neither -- the render schema refuses them by absence.
   * Its old answer, 0, is the ioctl's "yes" (the host returns 0 with an
   * NVKMS device and -EINVAL without, nvidia-drm-fence.c:387-392), and the
   * 610 Xorg DDX (nvidia_drv.so) turns PRIME fencing on from it and then has
   * every attach fail. -EINVAL is the host's own "no".
   */
  case DRM_NVIDIA_FENCE_SUPPORTED:
    return -EINVAL;

  /*
   * DMABUF_SUPPORTED is the host's "is there an NVKMS device behind this
   * node" (nvidia-drm-drv.c:1127-1135: 0 or -EINVAL), which is exactly
   * supports_alloc in the record -- the backend fills that word from this
   * same ioctl on hosts whose GET_DEV_INFO predates it. It used to answer 0
   * everywhere, which on a modeset=0 host sent the ICD down a dma-buf path
   * the host then refused.
   */
  case DRM_NVIDIA_DMABUF_SUPPORTED:
    return dri->dev_info[3] ? 0 : -EINVAL;
  }

  ncmd = nvgpu_drm_driver_cmd(nr);
  if (!ncmd || !file) {
    /*
     * Named rather than silently refused. An ioctl this stub does not answer
     * is the ICD asking for something the node cannot do yet, and -ENOTTY on
     * its own turns up much later as a device that would not initialise.
     */
    dev_dbg_ratelimited(&nfd->dev->vdev->dev,
                        "virtio-gpu-nv: unhandled nvidia-drm ioctl "
                        "nr=0x%02x (DRM_NVIDIA_%u) size=%u dir=%u\n",
                        nr, nr - DRM_COMMAND_BASE, _IOC_SIZE(cmd),
                        _IOC_DIR(cmd));
    return -ENOTTY;
  }
  /* Semaphore-surface fences only with the backend serving them;
   * GET_DEV_INFO says they exist only then. */
  if (nr - DRM_COMMAND_BASE >= DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE &&
      nr - DRM_COMMAND_BASE <= DRM_NVIDIA_SEMSURF_FENCE_ATTACH &&
      !nvgpu_fences_enabled(nfd->dev))
    return -ENOTTY;

  ret = nvgpu_drm_arg_in(&a, cmd, ncmd, uarg);
  if (ret)
    return ret;
  ret = nvgpu_drm_driver_ioctl(nfd, file, a.cmd, a.k);
  return nvgpu_drm_arg_out(&a, ret);
}

/* The driver range past its three special cases, on the kernel copy `k` of
 * the argument, `cmd` the native command (nvgpu_drm_driver_cmd()). */
/*
 * GEM_ALLOC_NVKMS_MEMORY is flat -- every field is a value -- so the whole
 * struct goes across and the answer comes back into it; the host's handle
 * never reaches userspace, a proxy stands in for it.
 */
static long nvgpu_gem_alloc_nvkms(struct nvgpu_fd *nfd, struct drm_file *file,
                                  unsigned int cmd, void *k) {
  /*
   * u32 handle OUT, u8 block_linear, u8 compressible, u16 pad,
   * u64 memory_size IN, u32 flags, u32 pad
   */
  struct {
    __u32 handle;
    __u8 block_linear;
    __u8 compressible;
    __u16 pad0;
    __u64 memory_size;
    __u32 flags;
    __u32 pad1;
  } p;
  u32 guest_handle;
  long ret;

  BUILD_BUG_ON(sizeof(p) != 24);
  /* A copy of its own: a failed call leaves the caller's bytes as they
   * were, never a host handle number. */
  memcpy(&p, k, sizeof(p));
  ret = nvgpu_ioctl_flat(nfd->dev, nfd->handle, cmd, &p, sizeof(p),
                         NVGPU_FLAT_WHOLE, NULL);
  if (ret < 0)
    return ret;

  /* On failure the proxy code has closed the host handle already. */
  ret = nvgpu_gem_proxy_create_new(file, nfd, p.handle, p.memory_size,
                                   &guest_handle);
  if (ret)
    return ret;

  p.handle = guest_handle;
  memcpy(k, &p, sizeof(p));
  return 0;
}

static long nvgpu_drm_driver_ioctl(struct nvgpu_fd *nfd,
                                   struct drm_file *file, unsigned int cmd,
                                   void *k) {
  switch (_IOC_NR(cmd) - DRM_COMMAND_BASE) {
  case DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID:
    /* Fixed at open (nvgpu_drm_open()), as nvidia-drm's is. */
    put_unaligned(nfd->drm_unique_id, (u64 *)k);
    return 0;

  /* Semaphore-surface fences: the host's objects, proxied (nvgpu_semsurf.c). */
  case DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE:
  case DRM_NVIDIA_SEMSURF_FENCE_CREATE:
  case DRM_NVIDIA_SEMSURF_FENCE_WAIT:
  case DRM_NVIDIA_SEMSURF_FENCE_ATTACH:
    return nvgpu_fence_semsurf_ioctl(nfd, file, cmd, k);

  /* Answered from the proxy; see nvgpu_gem_identify(). */
  case DRM_NVIDIA_GEM_IDENTIFY_OBJECT:
    return nvgpu_gem_identify(file, k);

  case DRM_NVIDIA_GEM_MAP_OFFSET: {
    /*
     * u32 handle IN, u32 pad, u64 offset OUT -- answered here, not forwarded.
     *
     * The offset a caller gets has to be one it can mmap, and it will mmap
     * *this* node. The host's offset names a position in the host's node and
     * would land on whatever this node happens to have at that offset, which
     * is nothing. So the proxy gets an offset of its own, the core's mmap
     * finds it, and nvgpu_gem_object_mmap() maps the host's memory through the
     * shared window. The host's offset is still needed, but only inside the
     * driver, and it is fetched at placement time.
     */
    struct {
      __u32 handle;
      __u32 pad;
      __u64 offset;
    } *p = k;
    u64 offset;
    int ret;

    ret = nvgpu_gem_mmap_offset(file, p->handle, &offset);
    if (ret)
      return ret;
    p->offset = offset;
    return 0;
  }

  /* ── GEM: forwarded to the host's render node ── */
  case DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY:
    return nvgpu_gem_alloc_nvkms(nfd, file, cmd, k);

  /*
   * These two carry a pointer to an NVKMS parameter block. The guest's
   * address means nothing on the host, so the bytes travel alongside and the
   * backend gives them a host address before the call -- the same shape as
   * v1 nvidia-modeset's (nvgpu_ioctl_modeset()), though each has its own
   * forwarder: this one also translates the GEM handle and the descriptor.
   */
  case DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY:
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, k,
                                      &nvgpu_gem_import_nvkms);

  case DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY:
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, k,
                                      &nvgpu_gem_export_dmabuf);

  /*
   * Byte-identical to the one above -- u32 handle, pad, ptr, size, with an
   * NvKmsKapiPrivExportMemoryParams { int memFd; } on the end of the pointer
   * -- so it takes the same descriptor and the same fd swap.
   *
   * This is what the ICD asks after re-importing a descriptor the node itself
   * exported, to learn that the memory behind it is NVKMS memory it can use.
   * Refused, vkGetMemoryFdPropertiesKHR answers memoryTypeBits=0, and the
   * capture layer drops every frame for want of a memory type. It is the
   * paired half of PRIME_FD_TO_HANDLE: the import gives the handle back, this
   * says what is behind it, and neither is any use without the other.
   */
  case DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY:
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, k,
                                      &nvgpu_gem_export_dmabuf);
  }
  return -ENOTTY;
}

/* ───────── nvidia-drm GEM ioctls with a nested parameter block ─────────
 *
 * GEM_IMPORT_NVKMS_MEMORY and GEM_EXPORT_DMABUF_MEMORY both hold a userspace
 * pointer to an NVKMS parameter block and the block's length beside it. That
 * is the same shape as nvidia-modeset's outer struct, so the wire format is
 * the same one: the outer struct, then the pointed-to bytes, with the backend
 * putting a host address in the pointer field before it makes the call and the
 * caller's own value back in it before it answers.
 *
 * The two differ from modeset only in where the pointer and the length sit and
 * in the length being a u64, which is why they are described by a
 * nvgpu_gem_nested_desc rather than hard-coded.
 */
/*
 * NVKMS names the memory by an open file. Our descriptor is not the
 * backend's, so it goes across as the handle the backend issued when we
 * opened that file, and the backend turns it back into one of its own
 * descriptors before making the call -- the same round trip
 * EXPORT_OBJECT_TO_FD already makes for RM. The guest's own value is put
 * back by the backend before it answers, so userspace reads back the
 * descriptor it passed. `nested` is the request's copy of the block.
 */
static int nvgpu_gem_nested_fd_in(struct nvgpu_fd *nfd, unsigned int cmd,
                                  const struct nvgpu_gem_nested_desc *d,
                                  void *nested, u32 nested_size) {
  int guest_fd, ret;
  u32 handle;

  if (d->fd_offset == NVGPU_GEM_NO_FD || nested_size < (u32)d->fd_offset + 4)
    return 0;
  guest_fd = (int)get_unaligned_le32(nested + d->fd_offset);
  ret = nvgpu_handle_for_fd(nfd->dev, guest_fd, &handle);
  if (ret) {
    dev_dbg_ratelimited(&nfd->dev->vdev->dev,
                        "virtio-gpu-nv: nvidia-drm ioctl nr=0x%02x names "
                        "fd %d, which is not one of our devices\n",
                        _IOC_NR(cmd), guest_fd);
    return ret;
  }
  put_unaligned_le32(handle, nested + d->fd_offset);
  return 0;
}

/*
 * A reply whose status is not an errno: nothing goes back (-EPROTO), but a
 * positive one is not a refusal, and a GEM handle the host made for the
 * caller would stay open in the render file `fwd_handle` until it closes.
 * Closed here, unless it is a proxy's (a handle the file already had, S-11).
 */
static void nvgpu_gem_nested_unread(struct nvgpu_fd *nfd,
                                    const struct nvgpu_gem_nested_desc *d,
                                    u32 fwd_handle, const u8 *resp_buf,
                                    const struct nvgpu_ioctl_reply *r) {
  u32 h;

  if (r->raw <= 0 || !d->handle_is_out ||
      d->handle_offset == NVGPU_GEM_NO_FIELD || r->data_len > d->size ||
      (u32)d->handle_offset + 4 > r->data_len ||
      !nvgpu_resp_has(r->used, sizeof(struct nvgpu_ioctl_resp), r->data_len))
    return;
  h = get_unaligned_le32(resp_buf + sizeof(struct nvgpu_ioctl_resp) +
                         d->handle_offset);
  if (h && !nvgpu_gem_handle_held(nfd->dev, fwd_handle, h))
    nvgpu_gem_close(nfd->dev, fwd_handle, h);
}

/*
 * The GEM handle in the reply's outer struct `out`, as the caller is to read
 * it. A handle the host made gets a proxy before the caller sees anything:
 * the host's handle means nothing in this guest, and the core's PRIME and
 * GEM_CLOSE paths need an object of ours to work on (0, or the proxy's
 * failure, which has closed the host handle already). A handle the caller
 * passed is given back as it passed it, not as the host's.
 */
static int nvgpu_gem_nested_handle_out(struct nvgpu_fd *nfd,
                                       struct drm_file *file,
                                       const struct nvgpu_gem_nested_desc *d,
                                       u8 *out, u32 caller_handle) {
  u32 host_handle, guest_handle;
  u64 obj_size = 0;
  int ret;

  if (!d->handle_is_out) {
    put_unaligned_le32(caller_handle, out + d->handle_offset);
    return 0;
  }
  host_handle = get_unaligned_le32(out + d->handle_offset);
  if (d->size_field_offset != NVGPU_GEM_NO_FIELD)
    obj_size = get_unaligned_le64(out + d->size_field_offset);
  ret = nvgpu_gem_proxy_create_new(file, nfd, host_handle, (size_t)obj_size,
                                   &guest_handle);
  if (ret)
    return ret;
  put_unaligned_le32(guest_handle, out + d->handle_offset);
  return 0;
}

static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void *karg,
                                       const struct nvgpu_gem_nested_desc *d) {
  u8 outer[NVGPU_GEM_OUTER_MAX];
  void __user *user_nested;
  u64 nested_size64;
  u32 nested_size;
  unsigned int sz = _IOC_SIZE(cmd);
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  struct nvgpu_ioctl_reply r;
  int req_total, resp_max, ret;
  u32 fwd_handle = nfd->handle;
  u32 caller_handle = 0;
  u32 used, data_len, nested_len;
  struct drm_gem_object *held = NULL;

  /*
   * The argument has been normalised to this descriptor's size
   * (nvgpu_drm_driver_cmd()), so this holds unless the two disagree, which
   * would put the pointer somewhere other than where it is about to be read.
   */
  if (sz != d->size) {
    dev_dbg_ratelimited(&nfd->dev->vdev->dev,
                        "virtio-gpu-nv: nvidia-drm ioctl nr=0x%02x carries %u "
                        "bytes, this driver knows it as %u\n",
                        _IOC_NR(cmd), sz, d->size);
    return -EINVAL;
  }

  /*
   * `outer` is on the stack, so a descriptor larger than it would be a buffer
   * overflow rather than a wrong answer. Checked here rather than at the call
   * sites because the descriptors are data, and data is what gets edited.
   */
  if (d->size > NVGPU_GEM_OUTER_MAX ||
      d->ptr_offset + 8 > d->size || d->size_offset + 8 > d->size)
    return -EINVAL;

  memcpy(outer, karg, d->size);

  user_nested = (void __user *)(unsigned long)get_unaligned_le64(
      outer + d->ptr_offset);
  nested_size64 = get_unaligned_le64(outer + d->size_offset);

  if (nested_size64 > NVGPU_GEM_NESTED_MAX)
    return -EINVAL;
  nested_size = (u32)nested_size64;

  /*
   * A handle the caller supplies names one of our proxies. Swap in the host's
   * handle and forward on the file that owns it, which is not necessarily the
   * one asking -- a compositor acting on a client's imported buffer is the
   * case that matters.
   */
  if (d->handle_offset != NVGPU_GEM_NO_FIELD && !d->handle_is_out) {
    struct nvgpu_gem_object *ng;

    caller_handle = get_unaligned_le32(outer + d->handle_offset);
    /* Held until the host is done with the numbers (S-25). */
    ng = nvgpu_gem_lookup(file, caller_handle);
    if (!ng)
      return -ENOENT;
    held = &ng->base;
    fwd_handle = ng->owner_handle;
    put_unaligned_le32(ng->host_handle, outer + d->handle_offset);
  }

  req_total = sizeof(*req) + d->size + nested_size;
  resp_max = sizeof(struct nvgpu_ioctl_resp) + d->size + nested_size;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  nvgpu_ioctl_req_init(req, fwd_handle, cmd, d->size, d->size, nested_size, 0,
                       0);

  memcpy(req_buf + sizeof(*req), outer, d->size);

  if (user_nested && nested_size > 0) {
    if (copy_from_user(req_buf + sizeof(*req) + d->size, user_nested,
                       nested_size)) {
      ret = -EFAULT;
      goto out;
    }
    ret = nvgpu_gem_nested_fd_in(nfd, cmd, d, req_buf + sizeof(*req) + d->size,
                                 nested_size);
    if (ret)
      goto out;
  }

  if (held) {
    /* The request holds it from here, as long as the host may act on it. */
    ret = nvgpu_send_recv_holding(nfd->dev, req_buf, req_total, resp_buf,
                                  resp_max, &used, nvgpu_gem_put_ref, held);
    held = NULL;
  } else {
    ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf,
                               resp_max, &used);
  }
  if (ret < 0)
    goto out;
  ret = nvgpu_ioctl_reply_parse(resp_buf, used, &r);
  if (ret == -EPROTO)
    nvgpu_gem_nested_unread(nfd, d, fwd_handle, resp_buf, &r);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = r.status;
  data_len = r.data_len;
  nested_len = r.nested_len;

  /*
   * The outer struct carries the answer: GEM_IMPORT writes the new handle
   * into it. Copied back even when the call failed, because the host's
   * failure may have written a field too, and the caller reads what the host
   * driver would have left it -- but only what the device actually wrote.
   */
  if (data_len && data_len <= d->size &&
      nvgpu_resp_has(used, sizeof(*resp), data_len)) {
    u8 *out = resp_buf + sizeof(*resp);

    if (d->handle_offset != NVGPU_GEM_NO_FIELD && ret >= 0) {
      int cret = nvgpu_gem_nested_handle_out(nfd, file, d, out,
                                             caller_handle);

      if (cret) {
        ret = cret;
        goto out;
      }
    }
    memcpy(karg, out, data_len);
  }

  if (user_nested && nested_size > 0 && nested_len > 0) {
    u32 copy_back = min(nested_size, nested_len);

    if (nvgpu_resp_has(used, sizeof(*resp) + d->size, copy_back) &&
        copy_to_user(user_nested, resp_buf + sizeof(*resp) + d->size,
                     copy_back))
      ret = -EFAULT;
  }

out:
  if (held)
    drm_gem_object_put(held);
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/* ───────── DRI device nodes (host major:minor passthrough) ─────────────── */

/*
 * The DRM side of a render node.
 *
 * The guest's node exists so NVIDIA's Vulkan and EGL userspace can find the
 * GPU the way it insists on finding it. Only the pieces that enumeration
 * touches are here: the core answers DRM_IOCTL_VERSION out of the fields
 * below, and the driver-private range is forwarded like any other ioctl.
 *
 * `name` is what the ICD compares against, so it is the host driver's name and
 * not this module's.
 */
/*
 * GET_DRM_FILE_UNIQUE_ID's answer: a number that tells one open of a node
 * from another. The ICD asks for it once a device has been created and uses
 * it to recognise its own file; nothing outside this guest ever sees it, so a
 * counter is a real answer rather than a stub, and it must not restart while
 * the module is loaded or two live files would claim the same id. Given at
 * open: assigned on first ask, two threads asking at once on a new file could
 * each be told a different one (the 2026-09-29 review, S8).
 */
static atomic64_t nvgpu_drm_next_unique_id = ATOMIC64_INIT(0);

static int nvgpu_drm_open(struct drm_device *drm, struct drm_file *file) {
  struct nvgpu_dri_dev *dri = drm->dev_private;
  struct nvgpu_device *dev;
  struct nvgpu_fd *nfd = NULL;
  struct nvgpu_open_req_proc *reqp = NULL;
  struct nvgpu_open_req *req;
  u32 req_len;
  struct nvgpu_open_resp *resp = NULL;
  int ret;

  if (!dri || !dri->dev)
    return -ENODEV;
  dev = dri->dev;

  nfd = kzalloc(sizeof(*nfd), GFP_KERNEL);
  reqp = kzalloc(sizeof(*reqp), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !reqp || !resp) {
    ret = -ENOMEM;
    goto err;
  }

  nfd->dev = dev;
  nfd->device_type = NVGPU_DEV_DRI_BASE + dri->index;
  nfd->drm_unique_id = (u64)atomic64_inc_return(&nvgpu_drm_next_unique_id);
  /* The file's own reference; GEM proxies it owns add theirs. */
  refcount_set(&nfd->ref, 1);

  req = &reqp->req;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->device_type = cpu_to_le32(nfd->device_type);
  req->flags = cpu_to_le32(O_RDWR);

  /* The opener, for the backend's per-process share (quota.rs). */
  req_len = nvgpu_open_req_fill_proc(dev, reqp);
  ret = nvgpu_send_recv(dev, reqp, req_len, resp, sizeof(*resp));
  if (ret < 0)
    goto err;

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto err;
  }

  nfd->handle = le32_to_cpu(resp->hdr.handle);
  nfd->drm_file = file;
  xa_init(&nfd->gem_index);
  ret = xa_err(xa_store(&dev->renders, nfd->handle, nfd, GFP_KERNEL));
  if (ret) {
    nvgpu_close_handle(dev, nfd->handle);
    goto err;
  }

  /* A lease being adopted into this very open, or a card file that may
   * want the host's card later (nvgpu_kms.c). */
  ret = nvgpu_kms_open(dri, file, nfd);
  if (ret) {
    xa_erase(&dev->renders, nfd->handle);
    nvgpu_close_handle(dev, nfd->handle);
    goto err;
  }

  /* Put by the last nvgpu_fd_put(), which may come after remove(). */
  nvgpu_dev_get(dev);
  nvgpu_fd_register(nfd->dev, nfd);
  file->driver_priv = nfd;
  kfree(reqp);
  kfree(resp);
  return 0;

err:
  kfree(nfd);
  kfree(reqp);
  kfree(resp);
  return ret;
}

/*
 * The guest file is going: cut everything that could reach its drm_file.
 *
 * The core frees the drm_file right after postclose, and drains its event
 * list well before that (drm_file_free(), drm_file.c:250 against :267), while
 * the nvgpu_fd lives on for as long as GEM proxies need its render handle. So
 * the back-pointer is cleared under the event registry lock -- after which no
 * consumer can inject an event into this file -- and the KMS handle is closed
 * now, not when the last proxy goes: host master, framebuffers and a lease
 * have to end with the file, as they would on the host itself.
 *
 * Runs from our .release, before drm_release(). postclose calls it again for
 * safety, and the second call finds nothing to do.
 */
static void nvgpu_drm_detach(struct nvgpu_fd *nfd) {
  u32 kms;

  if (!nvgpu_fd_detach_drm(nfd, &kms))
    return;
  nvgpu_fd_unregister(nfd->dev, nfd);
  /* Its syncobjs' userspace eventfds, which end with its syncobjs. */
  nvgpu_fence_file_release(nfd);
  /* Its event consumers and reserved events, while the drm_file stands. */
  nvgpu_kms_detach(nfd);
  if (kms)
    nvgpu_close_handle(nfd->dev, kms);
}

/*
 * The render handle outlives the file if proxies still need it: the file's
 * reference goes here, and the backend CLOSE with the last one.
 * drm_gem_release() has already run (drm_file.c:261), so the proxies this
 * file held handles to that nobody else references are gone by now.
 */
static void nvgpu_drm_postclose(struct drm_device *drm, struct drm_file *file) {
  struct nvgpu_fd *nfd = file->driver_priv;

  if (!nfd)
    return;
  nvgpu_drm_detach(nfd);
  file->driver_priv = NULL;
  nvgpu_fd_put(nfd);
}

static int nvgpu_drm_release(struct inode *inode, struct file *filp) {
  struct drm_file *file = filp->private_data;

  if (file && file->driver_priv)
    nvgpu_drm_detach(file->driver_priv);
  return drm_release(inode, filp);
}

void nvgpu_drm_drop_master(struct file *f) {
  struct drm_file *file = f->private_data;

  /*
   * Through the core's own DROP_MASTER, so its bookkeeping is the one a
   * user's drop makes; the caller opened the file, which is what the
   * core's check asks (was_master, same tgid). No argument to copy.
   */
  if (file && drm_is_current_master(file))
    drm_ioctl(f, DRM_IOCTL_DROP_MASTER, 0);
}

struct nvgpu_fd *nvgpu_drm_file_nfd(struct file *f) {
  struct drm_file *file;

  if (f->f_op != &nvgpu_drm_fops)
    return NULL;
  file = f->private_data;
  return file ? file->driver_priv : NULL;
}

/*
 * The DRM node's ioctl entry point.
 *
 * Three kinds of ioctl arrive on /dev/dri/renderD128:
 *
 *   driver range (DRM_COMMAND_BASE..END)  nvidia-drm's own
 *                                         (nvgpu_drm_handle_ioctl(): the
 *                                         GET_DEV_INFO layouts, the SUPPORTED
 *                                         probes, the GEM calls, the unique
 *                                         id, semaphore-surface fences).
 *                                         drm_ioctl() answers -EINVAL for
 *                                         these because we register no
 *                                         drm_ioctl_desc table, so they are
 *                                         taken here first.
 *   other type 'd'                        core DRM: VERSION, GET_UNIQUE, ...
 *                                         left to drm_ioctl().
 *   type 'F'                              NVIDIA RM, proxied to the host like
 *                                         on any other node.
 *
 * Every type-'d' ioctl is taken by its number alone, as drm_ioctl() takes
 * one. The ones answered or forwarded here (the driver range, syncobjs, a
 * KMS file's KMS calls) run on the caller's argument normalised to the
 * native struct (nvgpu_drm_arg_in()), so the handler and the host see only
 * the native command; the rest go to drm_ioctl(), which does the same.
 */
static long __nvgpu_drm_unlocked_ioctl(struct file *filp, unsigned int cmd,
                                       unsigned long arg, bool compat) {
  struct drm_file *file = filp->private_data;
  struct nvgpu_fd *nfd;
  unsigned int nr = _IOC_NR(cmd);

  if (!file || !file->driver_priv)
    return -ENODEV;
  nfd = file->driver_priv;

  if (_IOC_TYPE(cmd) == DRM_IOCTL_BASE) {
    long kret;

    /*
     * A file with a KMS side (a lease, or a card file of a compositor-VM
     * guest) sends its KMS ioctls -- and nvidia-drm's KMS-class ones -- to
     * its host card or lease file, ahead of the core, which has no KMS of its
     * own to answer them with. Everything it leaves goes on as before.
     */
    if (nvgpu_kms_ioctl(filp, cmd, arg, &kret))
      return kret;

    if (nr >= DRM_COMMAND_BASE && nr < DRM_COMMAND_END) {
      struct nvgpu_dri_dev *dri = file->minor->dev->dev_private;

      if (!dri)
        return -ENODEV;
      return nvgpu_drm_handle_ioctl(nfd, dri, file, cmd, arg);
    }

    /*
     * Syncobjs are the host's, in the file's render node (nvgpu_syncobj.c).
     * Only with the backend serving them; otherwise the core answers, and
     * with DRIVER_SYNCOBJ cleared for this device it answers -EOPNOTSUPP.
     */
    if (nvgpu_fence_syncobj_cmd(cmd) && nvgpu_fences_enabled(nfd->dev)) {
      struct nvgpu_drm_arg a;
      long ret;

      ret = nvgpu_drm_arg_in(&a, cmd, nvgpu_fence_syncobj_cmd(cmd),
                             (void __user *)arg);
      if (ret)
        return ret;
      ret = nvgpu_fence_syncobj_ioctl(nfd, file, a.cmd, a.k);
      return nvgpu_drm_arg_out(&a, ret);
    }

    /*
     * DRM_CAP_DUMB_BUFFER on a file with no KMS side (those went to the host
     * above). The core answers it only for a DRIVER_MODESET device and says
     * -EOPNOTSUPP here, which is how nvidia-drm with modeset=0 answers --
     * and NVIDIA's userspace reads it that way: nvidia-vaapi-driver asks for
     * exactly this cap to learn whether nvidia_drm.modeset=1 and gives up
     * when it fails. This node stands for a modeset=1 nvidia-drm (the only
     * kind the guest's userspace is built for), so it answers as the core
     * would for a modeset device without dumb buffers: 0, which is true of
     * this node (MODE_CREATE_DUMB is not served on it). Answered here; the
     * host sees nothing.
     */
    if (nr == _IOC_NR(DRM_IOCTL_GET_CAP)) {
      struct nvgpu_drm_arg a;
      struct drm_get_cap *gc;
      long ret;

      ret = nvgpu_drm_arg_in(&a, cmd, DRM_IOCTL_GET_CAP, (void __user *)arg);
      if (ret)
        return ret;
      gc = a.k;
      if (gc->capability == DRM_CAP_DUMB_BUFFER) {
        gc->value = 0;
        return nvgpu_drm_arg_out(&a, 0);
      }
      /* Any other is the core's, which reads the caller's argument itself. */
      nvgpu_drm_arg_drop(&a);
    }

    /*
     * Core DRM, answered by the core against this node's own state. That is
     * right for VERSION and GET_UNIQUE and wrong for anything that names a GEM
     * object: the objects live in the host's drm_file, so the core looks them
     * up here, finds nothing, and refuses.
     *
     * Named for the same reason the driver range is (see below): a refusal
     * from the core carries no hint that it came from the wrong side of the
     * boundary, and turns up much later as a client that stopped asking.
     */
    {
      long ret;

#ifdef CONFIG_COMPAT
      /* The core's 32-bit layouts (VERSION, GET_UNIQUE, ...) are the core's
       * to translate, as for any DRM driver. */
      if (compat)
        ret = drm_compat_ioctl(filp, cmd, arg);
      else
#endif
        ret = drm_ioctl(filp, cmd, arg);
      if (ret < 0)
        dev_dbg_ratelimited(&nfd->dev->vdev->dev,
                            "virtio-gpu-nv: core DRM ioctl nr=0x%02x answered "
                            "locally with %ld\n",
                            nr, ret);
      return ret;
    }
  }

  return nvgpu_ioctl_fd(nfd, cmd, arg);
}

/*
 * Every ioctl inside drm_dev_enter(): remove() unplugs the node
 * (nvgpu_dri_cleanup()) after failing every waiter, once the transport is
 * dead (its state stays, dead, until the device's last reference), and waits
 * there for any ioctl still inside (S-26). A file opened before stays open
 * and gets -ENODEV.
 */
static long nvgpu_drm_unlocked_ioctl(struct file *filp, unsigned int cmd,
                                     unsigned long arg) {
  struct drm_file *file = filp->private_data;
  long ret;
  int idx;

  if (!file || !drm_dev_enter(file->minor->dev, &idx))
    return -ENODEV;
  ret = __nvgpu_drm_unlocked_ioctl(filp, cmd, arg, false);
  drm_dev_exit(idx);
  return ret;
}

#ifdef CONFIG_COMPAT
/*
 * A 32-bit caller. nvidia-drm's own fops take compat_ioctl = drm_compat_ioctl
 * (nvidia-drm-drv.c): the core translates its own ioctls whose layout
 * differs in 32 bits, and hands everything else -- the driver range, whose
 * structs carry pointers as u64 so no layout differs -- to the native
 * handler as it is. So here: the core's ioctls that the core answers go
 * through drm_compat_ioctl(); the ones forwarded (the driver range, KMS and
 * syncobj calls, RM) go the native way with the pointer widened, the IOCTL2
 * interpreter reading in_compat_syscall() where a layout does differ.
 */
static long nvgpu_drm_compat_ioctl(struct file *filp, unsigned int cmd,
                                   unsigned long arg) {
  struct drm_file *file = filp->private_data;
  long ret;
  int idx;

  if (!file || !drm_dev_enter(file->minor->dev, &idx))
    return -ENODEV;
  ret = __nvgpu_drm_unlocked_ioctl(
      filp, cmd, (unsigned long)compat_ptr((compat_uptr_t)arg), true);
  drm_dev_exit(idx);
  return ret;
}
#endif

/*
 * drm_poll(), and once the backend is gone (nvgpu_xfer_dead()) EPOLLHUP |
 * EPOLLERR: no host event will come for a flip or a vblank, and a reader must
 * not sleep through that. The file's nvgpu_fd wait queue is the one
 * nvgpu_xfer_reclaim() wakes.
 */
static __poll_t nvgpu_drm_poll(struct file *filp,
                               struct poll_table_struct *wait) {
  struct drm_file *file = filp->private_data;
  struct nvgpu_fd *nfd = file ? file->driver_priv : NULL;

  if (nfd) {
    poll_wait(filp, &nfd->wq, wait);
    if (nvgpu_xfer_dead(nfd->dev))
      return EPOLLHUP | EPOLLERR;
  }
  return drm_poll(filp, wait);
}

/*
 * The DRM core refuses to open a node whose fops do not declare
 * FOP_UNSIGNED_OFFSET:
 *
 *   if (WARN_ON_ONCE(!(filp->f_op->fop_flags & FOP_UNSIGNED_OFFSET)))
 *           return -EINVAL;            -- drm_open_helper(), drm_file.c
 *
 * DRM offsets are a mmap address space and are unsigned, so the core makes
 * every driver say so. Drivers that build their fops with DEFINE_DRM_GEM_FOPS
 * get it for free; ours are written out by hand and so must set it, or every
 * open of /dev/dri/renderD128 fails with EINVAL before .open is ever reached.
 *
 * Guarded because the flag postdates the kernels this module still builds
 * against; on those the check does not exist either.
 */
static const struct file_operations nvgpu_drm_fops = {
    .owner = THIS_MODULE,
#if defined(FOP_UNSIGNED_OFFSET)
    .fop_flags = FOP_UNSIGNED_OFFSET,
#endif
    .open = drm_open,
    .release = nvgpu_drm_release,
    .unlocked_ioctl = nvgpu_drm_unlocked_ioctl,
#ifdef CONFIG_COMPAT
    .compat_ioctl = nvgpu_drm_compat_ioctl,
#endif
    .mmap = drm_gem_mmap,
    .poll = nvgpu_drm_poll,
    .read = drm_read,
    .llseek = noop_llseek,
};

/*
 * The guest core arbitrates DRM master (drm_auth.c) exactly as it would for a
 * real card, and these follow its decisions onto the host card file behind a
 * compositor-VM guest's card node. Both run under the core's master_mutex,
 * after its own permission checks; neither can veto (void in 7.2,
 * drm_drv.h:268-275).
 */
static void nvgpu_drm_master_set(struct drm_device *drm, struct drm_file *file,
                                 bool new_master) {
  nvgpu_kms_master_set(file, new_master);
}

static void nvgpu_drm_master_drop(struct drm_device *drm,
                                  struct drm_file *file) {
  nvgpu_kms_master_drop(file);
}

/*
 * Every feature any device of ours may have. A device that cannot serve one
 * has it cleared in its own drm_device.driver_features (nvgpu_dri_init()),
 * which the core ANDs with these on every check (drm_drv.h,
 * drm_core_check_all_features()) and which exists for exactly that: one
 * shared driver, per-device limits. A per-device copy of this struct would
 * work too, but the drm_device keeps a pointer to it and can outlive the
 * nvgpu_device it would live in, since an open file holds the drm_device.
 */
static const struct drm_driver nvgpu_drm_driver = {
    .driver_features = DRIVER_GEM | DRIVER_RENDER | DRIVER_SYNCOBJ |
                       DRIVER_SYNCOBJ_TIMELINE,
    /* Without this, a buffer this node exported cannot be imported back. */
    .gem_prime_import = nvgpu_gem_prime_import,
    .open = nvgpu_drm_open,
    .postclose = nvgpu_drm_postclose,
    .master_set = nvgpu_drm_master_set,
    .master_drop = nvgpu_drm_master_drop,
    .fops = &nvgpu_drm_fops,
    .name = "nvidia-drm",
    .desc = "NVIDIA DRM driver",
    .major = 0,
    .minor = 0,
    .patchlevel = 0,
};

/*
 * The fake PCI device a DRI record's GPU is: the root whose address is the
 * config-space GPU slot with the record's minor. With several GPUs each node
 * must hang off its own -- the ICD matches a node to an RM device through
 * that parent -- where every one was hung off the first root. The first
 * registered root is the last resort, as before, for a record that names no
 * slot (a single-GPU backend that numbers it otherwise); NULL if none.
 */
static struct nvgpu_pci_root *nvgpu_dri_root(struct nvgpu_device *dev,
                                             const struct nvgpu_dri_dev *dri) {
  struct nvgpu_pci_root *first = NULL;
  u32 nslots = min_t(u32, dev->num_gpus, ARRAY_SIZE(dev->gpu_slots));
  int ri;
  u32 gi;

  for (ri = 0; ri < dev->num_pci_roots; ri++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[ri];

    if (!root->registered || !root->pdev)
      continue;
    if (!first)
      first = root;
    for (gi = 0; gi < nslots; gi++) {
      const struct virtio_gpu_nv_gpu_slot *s = &dev->gpu_slots[gi];

      if (!strncmp(s->pci_addr, root->slot.pci_addr, sizeof(s->pci_addr)) &&
          le32_to_cpu(s->minor) == dri->slot_index)
        return root;
    }
  }
  return first;
}

int nvgpu_dri_init(struct nvgpu_device *dev) {
  int i;

  if (dev->num_dri_devs == 0) {
    dev_info(&dev->vdev->dev,
             "virtio-gpu-nv: no DRI devices reported by VMM\n");
    return 0;
  }

  for (i = 0; i < dev->num_dri_devs; i++) {
    struct nvgpu_dri_dev *dri = &dev->dri_devs[i];

    /*
     * The pci_dev this DRI device's GPU is, as its parent: the kernel then
     * makes /sys/dev/char/M:N/device -> pci_dev and
     * /sys/bus/pci/devices/<addr>/drm/<name>, which is what Vulkan/EGL walk.
     */
    struct device *pci_parent = &dev->vdev->dev; /* fallback */
    struct nvgpu_pci_root *root = nvgpu_dri_root(dev, dri);
    struct drm_device *drm;

    if (root)
      pci_parent = &root->pdev->dev;

    /*
     * No sysfs is built by hand here any more.
     *
     * This used to create a `drm` kobject under the PCI device and a child
     * named after the node, because a character device gets no such tree and
     * the Vulkan ICD insists on walking one. Registering a real DRM device
     * makes the same tree properly -- and makes the hand-made one fatal: the
     * core tries to create `drm` under the same PCI device, finds the name
     * taken, and drm_dev_register() fails with -EEXIST.
     */

    dri->index = (u32)i;
    dri->dev = dev;

    /*
     * Register a real DRM device rather than a character device at the
     * host's numbers.
     *
     * A raw cdev cannot have them: major 226 belongs to the DRM core, which
     * claims it whenever CONFIG_DRM is built in, so register_chrdev_region()
     * on 226:129 fails with the node never appearing. That failure is quiet
     * -- /sys/bus/pci/.../drm/<name> still gets made, so the tree looks
     * half-right -- and it is fatal to Vulkan, because NVIDIA's userspace
     * enumerates the GPU through the render node and not through
     * /dev/nvidia*, which carry compute. The ICD stats the node, takes its
     * major, and wants /sys/dev/char/<major>:<minor>/device/drm to exist
     * before it will open it. With no node it declines to create an instance
     * and reports only that it found no drivers.
     *
     * The DRM core owns the minor it hands out, so the guest's node is not
     * necessarily the host's number. Nothing requires it to be: the ICD reads
     * whichever node exists.
     */
    drm = drm_dev_alloc(&nvgpu_drm_driver, pci_parent);
    if (IS_ERR(drm)) {
      dev_warn(&dev->vdev->dev, "virtio-gpu-nv: drm_dev_alloc %s failed: %ld\n",
               dri->name, PTR_ERR(drm));
      continue;
    }
    drm->dev_private = dri;

    /*
     * Syncobjs only where the backend serves them. With the feature on, the
     * core answers GET_CAP(SYNCOBJ) with 1 and creates guest-local syncobjs
     * whose handles mean nothing on the host (drm_ioctl.c:250-253); with it
     * off, every syncobj ioctl fails -EOPNOTSUPP, which is the truth.
     */
    if (!(dev->v2 && (dev->backend_caps & NVGPU_BCAP_FENCES)))
      drm->driver_features &= ~(DRIVER_SYNCOBJ | DRIVER_SYNCOBJ_TIMELINE);

    if (drm_dev_register(drm, 0) != 0) {
      dev_warn(&dev->vdev->dev, "virtio-gpu-nv: drm_dev_register %s failed\n",
               dri->name);
      drm_dev_put(drm);
      continue;
    }

    dri->drm = drm;
    dri->registered = true;
    dev_dbg(&dev->vdev->dev,
            "virtio-gpu-nv: registered render node for %s, host (%u:%u) "
            "gpu_id=0x%x\n",
            dri->name, dri->major, dri->minor, dri->dev_info[0]);
  }

  return 0;
}

void nvgpu_dri_cleanup(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_dri_devs; i++) {
    struct nvgpu_dri_dev *dri = &dev->dri_devs[i];

    if (!dri->registered)
      continue;

    /* The core owns the node and everything under it, including the sysfs
     * tree this used to build by hand. Unplugged, not just unregistered:
     * files opened before stay open, and from here every ioctl and mmap on
     * them fails -ENODEV instead of reaching a dead transport;
     * this waits for any still inside (drm_dev_enter()). */
    drm_dev_unplug(dri->drm);
    drm_dev_put(dri->drm);
    dri->drm = NULL;
    dri->registered = false;
  }
}
