// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the DRM side of the guest module.
 *
 * DRM device registration, the GEM proxies that stand in front of the host's
 * objects (mmap, vmap, dma-buf export, PRIME import), and the nvidia-drm
 * driver-range ioctls. An RM ioctl (any type but 'd') a render node receives
 * goes through nvgpu_ioctl_fd() like any other device's (nvgpu_rmio.c, or the
 * Rust build's dispatch.rs); the core's are drm_ioctl()'s, or a KMS file's
 * host card's (nvgpu_kms.c).
 */

#include <drm/drm.h>
#include <linux/atomic.h>
#include <linux/compat.h>
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/fcntl.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/poll.h>
#include <linux/scatterlist.h>
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
static const struct drm_gem_object_funcs nvgpu_gem_funcs;
static const struct file_operations nvgpu_drm_fops;

static long nvgpu_gem_identify(struct drm_file *file, void *karg);
static int nvgpu_gem_proxy_create_new(struct drm_file *file,
                                      struct nvgpu_fd *owner, u32 host_handle,
                                      size_t size, u32 *guest_handle);

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
/* Semaphore-surface fences, nvgpu_fence.c. */
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
   *                      (nvgpu_fence.c). With fences off (a v1 backend),
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

  /* Semaphore-surface fences: the host's objects, proxied (nvgpu_fence.c). */
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

/* Woken whenever a dying proxy leaves its owner's gem_index. */
static DECLARE_WAIT_QUEUE_HEAD(nvgpu_gem_gone_wq);

/*
 * Whether @owner's host handle @h is a tombstone: indexed by a proxy whose
 * last reference is gone and whose free has not yet closed it. The entry is
 * erased under the xarray lock before the proxy's memory goes, so what is
 * read here is still allocated.
 */
bool nvgpu_gem_dying(struct nvgpu_fd *owner, u32 h) {
  struct nvgpu_gem_object *ng;
  bool dying;

  xa_lock(&owner->gem_index);
  ng = xa_load(&owner->gem_index, h);
  dying = ng && !kref_read(&ng->base.refcount);
  xa_unlock(&owner->gem_index);
  return dying;
}

/* Until @owner's host handle @h is no dying proxy's; -ERESTARTSYS if killed. */
int nvgpu_gem_wait_gone(struct nvgpu_fd *owner, u32 h) {
  return wait_event_killable(nvgpu_gem_gone_wq, !nvgpu_gem_dying(owner, h));
}

/*
 * Whether host GEM handle @gem of the file with backend handle @render is a
 * proxy's, alive or dying: a number the host handed back for an object the
 * file already had, which is that proxy's to close and nobody else's. For
 * the reaper of abandoned replies, which knows only the numbers.
 *
 * Looked up in dev->renders, which keeps a file until its last reference,
 * not in dev->fds, which loses it at release: a killed process's file is
 * released while its proxies -- held by whoever it shared buffers with --
 * live on, and a late reply naming one of their handles was taken for
 * nobody's and closed under them (S2, 2026-09-29). The xarray's lock keeps
 * the nvgpu_fd from being freed while its index is read (nvgpu_fd_put()
 * erases it under that lock first).
 */
bool nvgpu_gem_handle_held(struct nvgpu_device *dev, u32 render, u32 gem) {
  struct nvgpu_fd *nfd;
  bool held;

  xa_lock(&dev->renders);
  nfd = xa_load(&dev->renders, render);
  held = nfd && xa_load(&nfd->gem_index, gem) != NULL;
  xa_unlock(&dev->renders);
  return held;
}

void nvgpu_gem_close_unheld(struct nvgpu_fd *owner, u32 gem) {
  if (!xa_load(&owner->gem_index, gem))
    nvgpu_gem_close(owner->dev, owner->handle, gem);
}

/*
 * The host has closed the proxy's handle, or never will (the close was never
 * sent): the number is the host's to give out again, and an importer waiting
 * on it (nvgpu_gem_wait_gone()) may ask again. Only our own entry, never a
 * successor's. Then the owner's reference, which the close needed, and the
 * tombstone itself. Process context (nvgpu_gem_close_then()).
 */
static void nvgpu_gem_tomb_gone(void *arg) {
  struct nvgpu_gem_object *ng = arg;

  if (ng->owner && ng->host_handle) {
    xa_cmpxchg(&ng->owner->gem_index, ng->host_handle, ng, NULL, 0);
    wake_up_all(&nvgpu_gem_gone_wq);
  }
  if (ng->owner)
    nvgpu_fd_put(ng->owner);
  kfree(ng);
}

/*
 * The last reference to a proxy is gone, so the host's object can go too.
 *
 * Forwarded on the owner's handle rather than the caller's: the host object
 * belongs to the drm_file that created it, and that file may well have closed
 * first -- a compositor can outlive the client whose buffer it imported. The
 * proxy's reference on the owner is what keeps that handle open until now,
 * and it is dropped last, after the GEM_CLOSE and MUNMAP that need it.
 */
static void nvgpu_gem_free(struct drm_gem_object *obj) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);

  /*
   * The owner's index keeps this proxy until its host handle is really
   * closed, as a tombstone: nvgpu_gem_proxy_find() refuses it (no
   * references), and nvgpu_gem_proxy_new() answers -EAGAIN for its number.
   * Taking it out first, as this once did, opened a window -- as long as
   * nvgpu_fence_gem_free() waits on its global mutex, held across two
   * HOST_OPs -- in which a PRIME import of the *same* object, which the host
   * answers with the handle the file already has (drm_prime.c:306-310),
   * found no proxy and made a second one; this free's GEM_CLOSE then left
   * that one naming a number the host hands to the next object, and two
   * clients' buffers were mixed up from there on (S-11).
   */

  /* The handles SEMSURF_FENCE_ATTACH gave this object in other files go
   * first: each holds the host object too, and a reference on its file. */
  nvgpu_fence_gem_free(ng);

  /*
   * Give the window space back. The window is a gigabyte and a swapchain is
   * megabytes at a time, so a guest that allocates and frees buffers for an
   * hour exhausts it otherwise -- and the failure lands on whichever mapping
   * happens to be next, not on the one that leaked.
   *
   * Exactly once per proxy: the placement is the object's, made by one MMAP
   * on first use, not per vma -- vmas of a GEM object only hold object
   * references (drm_gem_vm_open/close), and this runs after the last one.
   */
  if (ng->window_valid && ng->mapping_id)
    nvgpu_munmap(ng->dev, ng->owner_handle, ng->mapping_id);

  drm_gem_object_release(obj);

  /*
   * The GEM_CLOSE last, and the tombstone stays until the host has really
   * closed the number (nvgpu_gem_tomb_gone()). Erased when the close was
   * merely queued -- a fatal signal while the ring was full, no memory --
   * the index let a GETFB or a PRIME import that got the same number back
   * make a new proxy for it, which the queued close then closed under it.
   * Queued always, so the release has one path; the proxy's memory is the
   * tombstone's until then.
   */
  if (ng->dev && ng->host_handle)
    nvgpu_gem_close_then(ng->dev, ng->owner_handle, ng->host_handle,
                         nvgpu_gem_tomb_gone, ng);
  else
    nvgpu_gem_tomb_gone(ng);
}

/*
 * ───────── the host's memory, reached through the shared window ─────────
 *
 * The object's memory is the host's and cannot be copied here: a swapchain
 * image is written by the host's GPU. What can travel is the *address*. The
 * backend maps the host's DRM node at the object's own mmap offset and places
 * that mapping in the shared window -- the same window that already carries
 * every RM mapping -- and the guest reaches it at a physical address it works
 * out from its own PCI configuration.
 *
 * Until this existed, the sg_table handed to an importer described freshly
 * allocated zeroed pages, which got the import accepted and made anything that
 * read the buffer read zeroes.
 */

/* drm_nvidia_gem_map_offset_params, as the host's nvidia-drm defines it. */
struct nvgpu_gem_map_offset_params {
  __u32 handle;
  __u32 pad;
  __u64 offset;
};

#define NVGPU_IOCTL_GEM_MAP_OFFSET                                             \
  _IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_NVIDIA_GEM_MAP_OFFSET,          \
        struct nvgpu_gem_map_offset_params)

/*
 * Put this object's memory in the window, once.
 *
 * Two steps, both on the *owner's* handle: ask the host for the mmap offset of
 * its GEM object, then ask the backend to map the node there and place it. The
 * offset is the host's and never reaches guest userspace -- what a client gets
 * from GEM_MAP_OFFSET is an offset into this node, answered below.
 */
static int nvgpu_gem_place_in_window(struct nvgpu_gem_object *ng) {
  struct drm_gem_object *obj = &ng->base;
  struct nvgpu_gem_map_offset_params mo = {};
  struct nvgpu_mmap_req *req;
  struct nvgpu_mmap_resp *resp;
  u64 window_off, end;
  u32 used;
  long ret;

  /* Acquire: the placement's fields below are read after this says so. */
  if (smp_load_acquire(&ng->window_valid))
    return 0;

  if (!ng->dev->window.len) {
    dev_warn_once(&ng->dev->vdev->dev,
                  "virtio-gpu-nv: no shared memory region, so a buffer's "
                  "memory cannot be reached from the guest\n");
    return -ENOTSUPP;
  }

  mutex_lock(&ng->map_lock);
  if (ng->window_valid) {
    mutex_unlock(&ng->map_lock);
    return 0;
  }

  mo.handle = ng->host_handle;
  ret = nvgpu_ioctl_flat(ng->dev, ng->owner_handle, NVGPU_IOCTL_GEM_MAP_OFFSET,
                         &mo, sizeof(mo), NVGPU_FLAT_WHOLE, NULL);
  if (ret < 0) {
    dev_dbg_ratelimited(&ng->dev->vdev->dev,
                        "virtio-gpu-nv: the host would not give object %u an "
                        "mmap offset: %ld\n",
                        ng->host_handle, ret);
    goto out;
  }

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out_free;
  }

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MMAP);
  req->hdr.handle = cpu_to_le32(ng->owner_handle);
  req->size = cpu_to_le64(obj->size);
  req->offset = cpu_to_le64(mo.offset);
  req->prot = cpu_to_le32(3); /* read-write: the host's GPU writes it */

  ret = nvgpu_send_recv_used(ng->dev, req, sizeof(*req), resp, sizeof(*resp),
                             &used);
  if (ret < 0)
    goto out_free;
  ret = nvgpu_resp_has(used, 0, sizeof(resp->hdr))
            ? (s32)le32_to_cpu((__le32)resp->hdr.status)
            : -EIO;
  if (ret < 0)
    goto out_free;
  if (!nvgpu_resp_has(used, 0, sizeof(*resp))) {
    ret = -EIO;
    goto out_free;
  }

  window_off = le64_to_cpu(resp->guest_phys_addr);
  /* Every mapping of the object reaches up to obj->size past window_off:
   * the placement must hold that much, or the rest is the window's next
   * extent (another process's memory) or unplaced window. And start on a
   * page: every mapping is made in pages from window_off, so a placement
   * that does not would hand out the bytes before it. */
  if (le64_to_cpu(resp->size) < obj->size) {
    dev_warn_ratelimited(&ng->dev->vdev->dev,
                         "virtio-gpu-nv: object %u placed in %llu bytes, not "
                         "its %zu\n",
                         ng->host_handle, le64_to_cpu(resp->size), obj->size);
    nvgpu_munmap(ng->dev, ng->owner_handle, le32_to_cpu(resp->mapping_id));
    ret = -ERANGE;
    goto out_free;
  }
  if (!PAGE_ALIGNED(window_off) ||
      check_add_overflow(window_off, (u64)obj->size, &end) ||
      end > ng->dev->window.len) {
    dev_warn_ratelimited(&ng->dev->vdev->dev,
                         "virtio-gpu-nv: a buffer placed at %llu+%zu is not "
                         "a page run inside the %llu-byte window\n",
                         window_off, obj->size, ng->dev->window.len);
    /* Placed but unusable: the placement is still the backend's to free. */
    nvgpu_munmap(ng->dev, ng->owner_handle, le32_to_cpu(resp->mapping_id));
    ret = -ERANGE;
    goto out_free;
  }

  ng->window_off = window_off;
  ng->mapping_id = le32_to_cpu(resp->mapping_id);
  ng->caching = resp->caching;
  ng->read_only = ng->dev->v2 && (resp->flags & NVGPU_MMAP_F_READ_ONLY);
  /* Release: every field above is visible before the flag says it is (a
   * lockless reader pairs with the acquire at the top). */
  smp_store_release(&ng->window_valid, true);
  ret = 0;

out_free:
  kfree(req);
  kfree(resp);
out:
  mutex_unlock(&ng->map_lock);
  return (int)ret;
}

/* Guest physical address of the object's memory. Valid only after placement. */
static phys_addr_t nvgpu_gem_phys(struct nvgpu_gem_object *ng) {
  return (phys_addr_t)(ng->dev->window.addr + ng->window_off);
}

/*
 * Map the object into a process, for the node's own mmap and for an importer
 * that maps the dma-buf, with the memory type the host maps it with -- for an
 * nvidia-drm object that is write-combining (drm_gem_mmap_obj), which the
 * placement's reply says.
 */
static int __nvgpu_gem_object_mmap(struct drm_gem_object *obj,
                                   struct vm_area_struct *vma) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  unsigned long size = vma->vm_end - vma->vm_start;
  unsigned long node_start = drm_vma_node_start(&obj->vma_node);
  u64 within;
  int ret;

  /*
   * vm_pgoff arrives absolute -- it still carries the object's fake offset,
   * whether it came through the node's mmap or the dma-buf's, which adds the
   * offset back before calling this. Neither subtracts it, so this does.
   */
  if (vma->vm_pgoff < node_start)
    return -EINVAL;
  within = (u64)(vma->vm_pgoff - node_start) << PAGE_SHIFT;
  if (within + size > obj->size)
    return -EINVAL;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ret;

  /*
   * A read-only host object (a read-only GEM node) is refused a writable
   * mapping and may not be made writable later, as nvidia-drm does itself
   * (nvidia-drm-gem.c:292-299): a write through the window would stop the VM.
   */
  if (ng->read_only) {
    if (vma->vm_flags & VM_WRITE)
      return -EINVAL;
    vm_flags_clear(vma, VM_MAYWRITE);
  }
  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = nvgpu_window_pgprot(ng->dev, ng->caching,
                                          vm_get_page_prot(vma->vm_flags));

  return io_remap_pfn_range(vma, vma->vm_start,
                            (nvgpu_gem_phys(ng) + within) >> PAGE_SHIFT, size,
                            vma->vm_page_prot);
}

/* Not after remove(): the window is the device's (see the ioctl entry). */
static int nvgpu_gem_object_mmap(struct drm_gem_object *obj,
                                 struct vm_area_struct *vma) {
  int ret, idx;

  if (!drm_dev_enter(obj->dev, &idx))
    return -ENODEV;
  ret = __nvgpu_gem_object_mmap(obj, vma);
  drm_dev_exit(idx);
  return ret;
}

/*
 * A kernel mapping of the buffer, which is what a CPU consumer of a dma-buf
 * asks for. It is iomem -- there is no struct page behind a PCI window -- so
 * it goes into the iosys_map as such and a caller that cannot handle iomem
 * will say so rather than dereference it.
 */
static int __nvgpu_gem_vmap(struct drm_gem_object *obj,
                            struct iosys_map *map) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  void __iomem *vaddr;
  int ret;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ret;

  /*
   * A kernel mapping cannot be made read-only through an iosys_map, and a
   * kernel write into a read-only placement would stop the VM, so a
   * read-only object has none.
   */
  if (ng->read_only) {
    dev_dbg_ratelimited(&ng->dev->vdev->dev,
                        "virtio-gpu-nv: object %u is read-only on the host; "
                        "no kernel mapping\n",
                        ng->host_handle);
    return -EPERM;
  }

  /* The same memory type as every other mapping of the placement. */
  switch (ng->dev->v2 ? ng->caching : NVGPU_MMAP_CACHE_DEFAULT) {
  case NVGPU_MMAP_CACHE_WB:
    vaddr = ioremap_cache(nvgpu_gem_phys(ng), obj->size);
    break;
  case NVGPU_MMAP_CACHE_UC:
    vaddr = ioremap(nvgpu_gem_phys(ng), obj->size);
    break;
  default:
    vaddr = ioremap_wc(nvgpu_gem_phys(ng), obj->size);
    break;
  }
  if (!vaddr)
    return -ENOMEM;

  iosys_map_set_vaddr_iomem(map, vaddr);
  return 0;
}

/* Not after remove(): the window is the device's (see the ioctl entry). */
static int nvgpu_gem_vmap(struct drm_gem_object *obj, struct iosys_map *map) {
  int ret, idx;

  if (!drm_dev_enter(obj->dev, &idx))
    return -ENODEV;
  ret = __nvgpu_gem_vmap(obj, map);
  drm_dev_exit(idx);
  return ret;
}

static void nvgpu_gem_vunmap(struct drm_gem_object *obj,
                             struct iosys_map *map) {
  if (map->is_iomem && map->vaddr_iomem)
    iounmap(map->vaddr_iomem);
  iosys_map_clear(map);
}

/*
 * The dma-buf an importer gets.
 *
 * Not the core's exporter: drm_gem_map_dma_buf() asks for an sg_table of
 * struct pages and then dma-maps it, and there are no pages here. What an
 * importer needs is a DMA address for the window, which dma_map_resource()
 * gives for exactly this case -- memory that is addressable but not backed by
 * pages.
 */
static struct sg_table *nvgpu_dmabuf_map(struct dma_buf_attachment *attach,
                                         enum dma_data_direction dir) {
  struct drm_gem_object *obj = attach->dmabuf->priv;
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  struct sg_table *sgt;
  dma_addr_t addr;
  int ret, idx;

  /* Not after remove(): the window is the device's. */
  if (!drm_dev_enter(obj->dev, &idx))
    return ERR_PTR(-ENODEV);
  ret = nvgpu_gem_place_in_window(ng);
  drm_dev_exit(idx);
  if (ret)
    return ERR_PTR(ret);
  /*
   * A read-only placement is read by an importer's device and never
   * written: a DMA write into it would stop the VM, as a CPU one would
   * (nvgpu_gem_object_mmap(), nvgpu_gem_vmap()).
   */
  if (ng->read_only && dir != DMA_TO_DEVICE)
    return ERR_PTR(-EPERM);

  sgt = kzalloc(sizeof(*sgt), GFP_KERNEL);
  if (!sgt)
    return ERR_PTR(-ENOMEM);
  if (sg_alloc_table(sgt, 1, GFP_KERNEL)) {
    kfree(sgt);
    return ERR_PTR(-ENOMEM);
  }

  addr = dma_map_resource(attach->dev, nvgpu_gem_phys(ng), obj->size, dir,
                          DMA_ATTR_SKIP_CPU_SYNC);
  if (dma_mapping_error(attach->dev, addr)) {
    sg_free_table(sgt);
    kfree(sgt);
    return ERR_PTR(-EIO);
  }

  sg_dma_address(sgt->sgl) = addr;
  sg_dma_len(sgt->sgl) = obj->size;
  sgt->nents = 1;
  return sgt;
}

static void nvgpu_dmabuf_unmap(struct dma_buf_attachment *attach,
                               struct sg_table *sgt,
                               enum dma_data_direction dir) {
  dma_unmap_resource(attach->dev, sg_dma_address(sgt->sgl), sg_dma_len(sgt->sgl),
                     dir, DMA_ATTR_SKIP_CPU_SYNC);
  sg_free_table(sgt);
  kfree(sgt);
}

static const struct dma_buf_ops nvgpu_dmabuf_ops = {
    .map_dma_buf = nvgpu_dmabuf_map,
    .unmap_dma_buf = nvgpu_dmabuf_unmap,
    .release = drm_gem_dmabuf_release,
    .mmap = drm_gem_dmabuf_mmap,
    .vmap = drm_gem_dmabuf_vmap,
    .vunmap = drm_gem_dmabuf_vunmap,
};

static struct dma_buf *nvgpu_gem_export(struct drm_gem_object *obj, int flags) {
  DEFINE_DMA_BUF_EXPORT_INFO(exp_info);

  exp_info.ops = &nvgpu_dmabuf_ops;
  exp_info.size = obj->size;
  exp_info.flags = flags;
  exp_info.priv = obj;
  exp_info.resv = obj->resv;

  return drm_gem_dmabuf_export(obj->dev, &exp_info);
}

/*
 * The other half of the export, and the one that is easy to forget: a buffer
 * this node exported, coming back in through PRIME_FD_TO_HANDLE.
 *
 * The core's default importer (drm_gem_prime_import_dev) has a fast path for
 * exactly this round trip, but it recognises a dma-buf by
 * `ops == &drm_gem_prime_dmabuf_ops`. Ours carries nvgpu_dmabuf_ops, because
 * the memory is the host's and reached through the shared window, so the fast
 * path misses -- and with no gem_prime_import_sg_table the core then answers
 * -EINVAL for a buffer it is holding a reference to.
 *
 * What that costs is not obvious from the refusal. vkGetMemoryFdPropertiesKHR
 * asks the node whether it owns a descriptor; refused, the ICD reports
 * memoryTypeBits=0, the importer finds no memory type in common with the
 * image's, and the capture layer drops every frame with "No suitable memory
 * type for DMA-BUF import". The encoder is fine; nothing ever reaches it.
 *
 * So do what the core would do, against our own ops. A foreign dma-buf still
 * gets -EINVAL: importing memory this node does not own would mean giving the
 * host GPU a mapping of it, which is the one thing the proxy must not invent.
 */
static struct drm_gem_object *nvgpu_gem_prime_import(struct drm_device *dev,
                                                     struct dma_buf *dma_buf) {
  struct drm_gem_object *obj;

  if (dma_buf->ops != &nvgpu_dmabuf_ops)
    return ERR_PTR(-EINVAL);

  obj = dma_buf->priv;
  if (!obj || obj->dev != dev)
    return ERR_PTR(-EINVAL);

  drm_gem_object_get(obj);
  return obj;
}

/*
 * Both mmap paths take a reference on the object for the vma and leave it to
 * the vma to give back, through the vm_ops they copy out of the object's funcs.
 * Without these the reference is never dropped: the object outlives its last
 * handle, its free never runs, and the window placement it holds is never
 * returned -- which showed up as a guest exhausting a gigabyte of window in 191
 * buffers it had already closed.
 */
static const struct vm_operations_struct nvgpu_gem_vm_ops = {
    .open = drm_gem_vm_open,
    .close = drm_gem_vm_close,
};

static const struct drm_gem_object_funcs nvgpu_gem_funcs = {
    .free = nvgpu_gem_free,
    .vm_ops = &nvgpu_gem_vm_ops,
    .export = nvgpu_gem_export,
    .mmap = nvgpu_gem_object_mmap,
    .vmap = nvgpu_gem_vmap,
    .vunmap = nvgpu_gem_vunmap,
};

/*
 * The proxy object itself, with one reference and no handle, indexed in
 * @owner's gem_index (nvgpu_gem_proxy_find()). Owns @host_handle the same way
 * nvgpu_gem_proxy_create() does, except on -EEXIST: a proxy already stands for
 * that host handle, which stays that proxy's; and on -EAGAIN: a proxy on its
 * way out still does, and will close it (nvgpu_gem_free()).
 */
static struct nvgpu_gem_object *nvgpu_gem_proxy_new(struct drm_device *drm,
                                                    struct nvgpu_fd *owner,
                                                    u32 host_handle,
                                                    size_t size,
                                                    u32 obj_type) {
  struct nvgpu_gem_object *ng;
  int ret;

  /* A size from the host's reply, which PAGE_ALIGN() must not wrap to 0. */
  if (size > SIZE_MAX - PAGE_SIZE + 1) {
    nvgpu_gem_close_unheld(owner, host_handle);
    return ERR_PTR(-E2BIG);
  }
  size = PAGE_ALIGN(size);
  if (!size)
    size = PAGE_SIZE;

  ng = kzalloc(sizeof(*ng), GFP_KERNEL);
  if (!ng) {
    nvgpu_gem_close(owner->dev, owner->handle, host_handle);
    return ERR_PTR(-ENOMEM);
  }

  mutex_init(&ng->map_lock);
  drm_gem_private_object_init(drm, &ng->base, size);
  ng->base.funcs = &nvgpu_gem_funcs;
  ng->dev = owner->dev;
  nvgpu_fd_get(owner);
  ng->owner = owner;
  ng->owner_handle = owner->handle;
  ng->host_handle = host_handle;
  /* What the host said the object is (DMABUF_IMPORT's third word), for
   * GEM_IDENTIFY_OBJECT to answer; NVKMS for everything this node allocates
   * or imports as NVKMS memory itself. */
  ng->obj_type = obj_type;

  /*
   * One proxy per host handle of a file, ever. A second would GEM_CLOSE the
   * number again when it died, and the host reuses numbers (drm_gem.c idr,
   * lowest free), so that close would land on whatever object had it by
   * then. A host handle that is already someone's proxy is theirs: refused
   * with -EEXIST, and left alone. Indexed only once whole, since
   * nvgpu_gem_proxy_find() hands out what it finds there.
   */
  ret = xa_insert(&owner->gem_index, host_handle, ng, GFP_KERNEL);
  if (ret) {
    if (ret == -EBUSY) {
      ng->host_handle = 0; /* the free below must not close it */
      ret = nvgpu_gem_dying(owner, host_handle) ? -EAGAIN : -EEXIST;
    }
    drm_gem_object_put(&ng->base);
    return ERR_PTR(ret);
  }
  return ng;
}

/*
 * Stand a guest object in front of a host one and return the guest handle.
 *
 * `size` is the object's size as the guest core knows it, and this node has
 * no DRIVER_MODESET (nvgpu_drm_driver), so nothing validates framebuffer
 * dimensions against it here -- the host's ADDFB2 does that against the host
 * object. What it does bound is everything that hands the memory out: the
 * mmap of the fake offset, the window placement, and the size of the dma-buf
 * an importer gets. So it has to be the real buffer's: an allocation's
 * memory_size, an import's mem_size, a host dma-buf's lseek size; a nested
 * import with no size field would fall to the single page below and
 * under-report, which is why none is described without one. Page-aligned
 * because the core rejects an object smaller than a page.
 *
 * The host handle is this function's from the moment it is called: every
 * failure closes it exactly once, except -EEXIST (a proxy of @owner already
 * stands for it, and the number is that proxy's) and -EAGAIN (a proxy on its
 * way out does, and closes it; see nvgpu_gem_proxy_create_new() for a
 * number the host has just made, and nvgpu_gem_wait_gone()). Callers used to close it
 * again after a failed drm_gem_handle_create(), whose put had already freed
 * the proxy and sent GEM_CLOSE -- and a second close of a number the host may
 * have reused for a newer object closes that one.
 */
int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *owner,
                           u32 host_handle, size_t size, u32 *guest_handle) {
  struct nvgpu_gem_object *ng;
  int ret;

  ng = nvgpu_gem_proxy_new(file->minor->dev, owner, host_handle, size,
                           NVGPU_GEM_OBJECT_NVKMS);
  if (IS_ERR(ng)) {
    if (PTR_ERR(ng) == -EEXIST)
      dev_dbg_ratelimited(&owner->dev->vdev->dev,
                          "virtio-gpu-nv: host GEM handle %u of file %u "
                          "already has a proxy; not making a second\n",
                          host_handle, owner->handle);
    return PTR_ERR(ng);
  }

  ret = drm_gem_handle_create(file, &ng->base, guest_handle);
  /* The handle holds the only reference now, or nothing does and it is
   * freed -- which closes the host handle and drops the owner. */
  drm_gem_object_put(&ng->base);
  return ret;
}

/*
 * nvgpu_gem_proxy_create() for a host handle the host has just made -- an
 * allocation's, not an import's -- which is new whatever the index says: a
 * tombstone under the same number has closed it already, or the host could
 * not have given it out again. So the tombstone is waited out, not refused.
 */
static int nvgpu_gem_proxy_create_new(struct drm_file *file,
                                      struct nvgpu_fd *owner, u32 host_handle,
                                      size_t size, u32 *guest_handle) {
  int ret;

  for (;;) {
    ret = nvgpu_gem_proxy_create(file, owner, host_handle, size,
                                 guest_handle);
    if (ret != -EAGAIN)
      return ret;
    ret = nvgpu_gem_wait_gone(owner, host_handle);
    if (ret) {
      nvgpu_gem_close(owner->dev, owner->handle, host_handle);
      return ret;
    }
  }
}

/*
 * ───────── dma-bufs for the Wayland channel (nvgpu_wl.c) ─────────
 */

/*
 * A client's dma-buf, on its way to the host compositor: the backend exports
 * the very object the proxy stands for on the proxy's owner file, so what is
 * needed is exactly that pair. Only our own proxies' dma-bufs, and only this
 * device's: anything else has no host object behind it that this backend
 * could name, and a pair read out of someone else's priv would be a guess.
 */
int nvgpu_dmabuf_to_host(struct nvgpu_device *dev, struct dma_buf *buf,
                         u32 *owner, u32 *gem) {
  struct drm_gem_object *obj;
  struct nvgpu_gem_object *ng;

  if (buf->ops != &nvgpu_dmabuf_ops)
    return -EINVAL;
  obj = buf->priv;
  if (!obj || obj->funcs != &nvgpu_gem_funcs)
    return -EINVAL;
  ng = to_nvgpu_gem(obj);
  if (ng->dev != dev || !ng->host_handle)
    return -EINVAL;
  *owner = ng->owner_handle;
  *gem = ng->host_handle;
  return 0;
}

/*
 * A host dma-buf, already imported into @drm_filp's render handle as host GEM
 * @host_gem, as a guest dma-buf descriptor (export mode: a host client's
 * buffer for the guest compositor).
 *
 * The proxy is owned by the file's own nvgpu_fd, whose handle is the render
 * handle the import ran on. If a proxy already stands for that host handle --
 * the host import returned a handle the file already had -- it is reused, not
 * doubled (see nvgpu_gem_proxy_find()). The export goes through the core with
 * a handle held in @drm_filp just for the call, so obj->dma_buf stays what the
 * core expects when the compositor imports the descriptor back
 * (drm_gem_prime_fd_to_handle's WARN_ON, drm_prime.c:320-324); the dma-buf
 * alone keeps the proxy alive afterwards.
 *
 * @host_gem is this function's once @drm_filp is known to be ours: every later
 * failure closes it (or leaves it to the proxy already standing for it).
 * -EBADF for a file that is not one of our DRM files leaves it with the
 * caller. -EAGAIN: a proxy on its way out stands for @host_gem and will close
 * it, so the number may be about to name nothing, or something else; it is
 * not the caller's to close either. Wait (nvgpu_gem_wait_gone()) and import
 * again, which gives a handle that is really the caller's.
 */
struct dma_buf *nvgpu_dmabuf_from_host_buf(struct file *drm_filp,
                                           u32 host_gem, u64 size,
                                           u32 obj_type, int o_flags) {
  struct nvgpu_fd *nfd = nvgpu_drm_file_nfd(drm_filp);
  struct drm_gem_object *obj = NULL;
  struct nvgpu_gem_object *ng;
  struct drm_file *file;
  struct dma_buf *buf;
  int ret, tries;
  u32 handle;

  if (!nfd)
    return ERR_PTR(-EBADF);
  file = drm_filp->private_data;

  /*
   * The proxy already standing for host_gem, if any; else a new one. A
   * racing import of the same buffer may index its proxy between the two
   * (-EEXIST, host_gem left alone): that one is then found on the retry.
   */
  for (tries = 0; !obj && tries < 2; tries++) {
    obj = nvgpu_gem_proxy_find(nfd, host_gem);
    if (obj)
      break;
    ng = nvgpu_gem_proxy_new(file->minor->dev, nfd, host_gem, size,
                             obj_type);
    if (!IS_ERR(ng))
      obj = &ng->base;
    else if (PTR_ERR(ng) != -EEXIST)
      return ERR_CAST(ng);
  }
  if (!obj)
    return ERR_PTR(-EBUSY); /* host_gem is the proxy's that keeps winning */
  if (obj->dev != file->minor->dev) {
    /* Proxies of a file's objects are made in that file's device; one that
     * is not cannot get a handle here, and a second proxy would double-own
     * the host handle. Should never happen: refuse rather than guess. */
    drm_gem_object_put(obj);
    return ERR_PTR(-EINVAL);
  }
  ret = drm_gem_handle_create(file, obj, &handle);
  /* The handle's reference, or none: a proxy made here and never handled is
   * freed now, which closes the host handle. */
  drm_gem_object_put(obj);
  if (ret)
    return ERR_PTR(ret);
  buf = drm_gem_prime_handle_to_dmabuf(file->minor->dev, file, handle,
                                       o_flags & (O_CLOEXEC | O_RDWR));
  drm_gem_handle_delete(file, handle);
  return buf;
}

/*
 * There is no "guest handle -> host numbers" that lets go of the proxy (this
 * was nvgpu_gem_to_host()): the numbers are the proxy's only while it lives,
 * and a concurrent last close would GEM_CLOSE them and let the host hand
 * them to a new object before the request that names them ran (S-25).
 * Whoever sends them holds the proxy (nvgpu_gem_lookup()) until the host is
 * done with the request: nvgpu_i2_hold(), nvgpu_send_recv_holding().
 */
void nvgpu_gem_put_ref(void *obj) { drm_gem_object_put(obj); }

struct nvgpu_gem_object *nvgpu_gem_lookup(struct drm_file *file,
                                          u32 guest_handle) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, guest_handle);

  if (!obj)
    return NULL;
  if (obj->funcs != &nvgpu_gem_funcs) {
    drm_gem_object_put(obj);
    return NULL;
  }
  return to_nvgpu_gem(obj);
}

struct drm_gem_object *nvgpu_gem_proxy_find(struct nvgpu_fd *owner,
                                            u32 host_handle) {
  struct nvgpu_gem_object *ng;
  struct drm_gem_object *obj = NULL;

  /* nvgpu_gem_free() erases under this lock before the memory goes, so a
   * proxy seen here is still allocated; one already on its way out (count
   * zero) is not handed back. */
  xa_lock(&owner->gem_index);
  ng = xa_load(&owner->gem_index, host_handle);
  if (ng && kref_get_unless_zero(&ng->base.refcount))
    obj = &ng->base;
  xa_unlock(&owner->gem_index);
  return obj;
}

int nvgpu_gem_mmap_offset(struct drm_file *file, u32 guest_handle,
                          u64 *offset) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, guest_handle);
  int ret;

  if (!obj)
    return -ENOENT;
  if (obj->funcs != &nvgpu_gem_funcs) {
    drm_gem_object_put(obj);
    return -ENOENT;
  }
  ret = drm_gem_create_mmap_offset(obj);
  if (!ret)
    *offset = drm_vma_node_offset_addr(&obj->vma_node);
  drm_gem_object_put(obj);
  return ret;
}

/*
 * GEM_IDENTIFY_OBJECT, answered here.
 *
 * NVIDIA's userspace asks this straight after a PRIME import, to learn what
 * kind of object it just took. The proxy already knows, and the host's answer
 * would be about a handle the importing file does not hold. Unknown for
 * anything that is not ours, which is what the host driver reports too.
 */
static long nvgpu_gem_identify(struct drm_file *file, void *karg) {
  struct {
    __u32 handle;
    __u32 object_type;
  } *p = karg;
  struct drm_gem_object *obj;

  obj = drm_gem_object_lookup(file, p->handle);
  if (obj && obj->funcs == &nvgpu_gem_funcs)
    p->object_type = to_nvgpu_gem(obj)->obj_type;
  else
    p->object_type = NVGPU_GEM_OBJECT_UNKNOWN;
  if (obj)
    drm_gem_object_put(obj);
  return 0;
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
     * Syncobjs are the host's, in the file's render node (nvgpu_fence.c).
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
