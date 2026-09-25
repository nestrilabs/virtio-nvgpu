// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: the DRM side of the guest module.
 *
 * DRM device registration, the GEM proxies that stand in front of the host's
 * objects (mmap, vmap, dma-buf export, PRIME import), and the nvidia-drm
 * driver-range ioctls. Everything else a render node receives is forwarded
 * through nvgpu_ioctl_fd() in nvgpu_main.c like any other device's ioctls.
 */

#include <drm/drm.h>
#include <linux/atomic.h>
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/fcntl.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_prime.h>

#include "nvgpu.h"

/* ───────── nvidia-drm stub — no DRM subsystem headers needed ───────── */

/*
 * The largest nvidia-drm GEM parameter struct this driver forwards, and the
 * largest NVKMS block one of them may point at. The first is a stack buffer's
 * size, so it is small on purpose and BUILD_BUG_ON'd against the descriptors
 * that use it; the second only has to refuse a length field that is garbage,
 * since a real NVKMS memory-import block is a few hundred bytes.
 */
#define NVGPU_GEM_OUTER_MAX 32
#define NVGPU_GEM_NESTED_MAX (64 * 1024)

static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg,
                                       const struct nvgpu_gem_nested_desc *d);

/* Defined below; named here because the ioctls that test it come first. */
static const struct drm_gem_object_funcs nvgpu_gem_funcs;
static const struct file_operations nvgpu_drm_fops;

static long nvgpu_gem_identify(struct drm_file *file, void __user *uarg);

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

/*
 * The GEM ioctls, which are what a swapchain is made of: allocate or import
 * memory, give it a fake mmap offset, hand it out as a dma-buf. They are
 * forwarded to the host's render node rather than answered here -- the memory
 * is the host's and so is the object that names it.
 *
 * Nothing translates the handles in these structs. A GEM handle is per
 * drm_file, and each open of this node holds exactly one open of the host's
 * node (nvgpu_drm_open), so the handle the host issues is already scoped to
 * the file that will use it and means the same thing on both sides.
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

/*
 * struct drm_version — UAPI, stable since DRM was upstreamed.
 * Copy of include/uapi/drm/drm.h:struct drm_version so we need
 * no DRM kernel headers.
 */
struct nvgpu_drm_version {
  int version_major;
  int version_minor;
  int version_patchlevel;
  size_t name_len;
  char __user *name;
  size_t date_len;
  char __user *date;
  size_t desc_len;
  char __user *desc;
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
 * nvgpu_drm_handle_ioctl — handle all DRM-layer ioctls on our /dev/dri/..
 * nodes.
 *
 * DRM_IOCTL_VERSION  (nr=0x00) — core ioctl, returns name="nvidia-drm"
 * GET_DEV_INFO       (nr=0x43) — driver ioctl, returns gpu_id etc.
 * FENCE_SUPPORTED    (nr=0x44) — driver ioctl, returns 0
 * DMABUF_SUPPORTED   (nr=0x4f) — driver ioctl, returns 0
 *
 * Everything else → -ENOTTY.
 */
static long nvgpu_drm_handle_ioctl(struct nvgpu_fd *nfd,
                                   struct nvgpu_dri_dev *dri,
                                   struct drm_file *file, unsigned int cmd,
                                   unsigned long arg) {
  unsigned int nr = _IOC_NR(cmd);
  void __user *uarg = (void __user *)arg;

  /* ── DRM_IOCTL_VERSION (type='d', nr=0x00) ── */
  if (nr == 0x00) {
    struct nvgpu_drm_version v;

    if (copy_from_user(&v, uarg, sizeof(v)))
      return -EFAULT;

    v.version_major = 0;
    v.version_minor = 1;
    v.version_patchlevel = 0;

#define FILL_DRM_STR(field, str)                                               \
  do {                                                                         \
    const char *_s = (str);                                                    \
    size_t _sl = strlen(_s);                                                   \
    if (v.field##_len >= _sl && v.field)                                       \
      if (copy_to_user(v.field, _s, _sl))                                      \
        return -EFAULT;                                                        \
    v.field##_len = _sl;                                                       \
  } while (0)

    FILL_DRM_STR(name, "nvidia-drm");
    FILL_DRM_STR(date, "20240101");
    FILL_DRM_STR(desc, "NVIDIA DRM stub");
#undef FILL_DRM_STR

    if (copy_to_user(uarg, &v, sizeof(v)))
      return -EFAULT;

    return 0;
  }

  /* ── Driver ioctls: DRM_COMMAND_BASE .. DRM_COMMAND_END ── */
  if (nr < DRM_COMMAND_BASE || nr >= DRM_COMMAND_END)
    return -ENOTTY;

  switch (nr - DRM_COMMAND_BASE) {

  case DRM_NVIDIA_GET_DEV_INFO: {
    /*
     * Straight from the host's own node. These fields describe how the card
     * lays memory out, and the ICD matches a DRM node to an RM device by the
     * gpu_id among them, so none of them is ours to invent -- the constants
     * that used to be here reported gpu_id 0 where the host says 0x100, and a
     * page kind correct only on the two architectures the comment named.
     */
    u32 info[NVGPU_DEV_INFO_WORDS];

    BUILD_BUG_ON(sizeof(struct drm_nvidia_get_dev_info_params) !=
                 NVGPU_DEV_INFO_WORDS * sizeof(u32));

    memcpy(info, dri->dev_info, sizeof(info));

    /*
     * The ICD's idea of this struct's size, against ours. A newer driver can
     * grow it, and this handler answers the user pointer directly rather than
     * through drm_ioctl's buffer -- so a larger struct is filled to 36 bytes
     * and the rest is left as whatever the caller had there. The ICD then
     * reads rubbish for the fields it added, and the symptom is not an error:
     * it is a device that associates with no DRM node at all.
     */
    if (_IOC_SIZE(cmd) != sizeof(info))
      dev_warn(&nfd->dev->vdev->dev,
               "virtio-gpu-nv: GET_DEV_INFO size mismatch: caller wants %u "
               "bytes, this build answers %zu\n",
               _IOC_SIZE(cmd), sizeof(info));

    /*
     * The three capability bits are the host's answer about the host's node,
     * and this node is not that node: it answers four ioctls and forwards
     * nothing else. Passing them through unchanged is a promise this stub
     * cannot keep -- with supports_semsurf set, the ICD asks for
     * SEMSURF_FENCE_CTX_CREATE (nr 0x54) on every device creation, gets
     * -ENOTTY, and fails the whole vkCreateDevice with
     * ERROR_INITIALIZATION_FAILED.
     *
     * Each stays zero until the ioctls behind it are forwarded:
     *
     *   supports_alloc     GEM_ALLOC_NVKMS_MEMORY, GEM_MAP_OFFSET,
     *                      GEM_EXPORT_DMABUF_MEMORY (0x0b, 0x0a, 0x0d)
     *   supports_sync_fd   PRIME_FENCE_CONTEXT_CREATE, GEM_PRIME_FENCE_ATTACH
     *                      (0x05, 0x06)
     *   supports_semsurf   SEMSURF_FENCE_CTX_CREATE and the three that follow
     *                      it (0x14..0x17)
     *
     * gpu_id, primary_index and the page-kind and sector-layout fields stay
     * as the host reported them: they describe the card, which is genuinely
     * the host's, and the ICD matches a DRM node to an RM device by gpu_id.
     */
    info[3] = nvgpu_claim_alloc;   /* supports_alloc */
    info[7] = nvgpu_claim_sync_fd; /* supports_sync_fd */
    info[8] = 0;                   /* supports_semsurf */

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
     * This was invisible for as long as there was one test box, because its
     * NVIDIA card was card0 on the host and card0 in the guest, and passing
     * the host's number through was indistinguishable from getting it right.
     * The second box has an integrated GPU, so its NVIDIA node is card1 --
     * and nothing presented.
     */
    if (file && file->minor && file->minor->dev && file->minor->dev->primary)
      info[2] = file->minor->dev->primary->index;

    if (copy_to_user(uarg, info, sizeof(info)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID: {
    /*
     * A number that tells one open of this node from another. The ICD asks
     * for it once a device has been created and uses it to recognise its own
     * file; nothing outside this guest ever sees it, so a counter is a real
     * answer rather than a stub, and it must not restart while the module is
     * loaded or two live files would claim the same id.
     */
    static atomic64_t next_unique_id = ATOMIC64_INIT(1);
    u64 id;

    if (!nfd->drm_unique_id)
      nfd->drm_unique_id = (u64)atomic64_inc_return(&next_unique_id);
    id = nfd->drm_unique_id;

    if (copy_to_user(uarg, &id, sizeof(id)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_FENCE_SUPPORTED:
    return 0; /* not supported, no payload */

  case DRM_NVIDIA_DMABUF_SUPPORTED:
    return 0; /* not supported, no payload */

  /*
   * ── GEM: forwarded to the host's render node ──
   *
   * These three are flat -- every field is a value -- so the whole struct
   * goes across and the answer comes back into it.
   */
  case DRM_NVIDIA_GEM_IDENTIFY_OBJECT:
    /* Answered from the proxy; see nvgpu_gem_identify(). */
    if (!file)
      return -ENOTTY;
    return nvgpu_gem_identify(file, uarg);

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
    } p;
    struct drm_gem_object *obj;
    int ret;

    if (!file)
      return -ENOTTY;
    if (_IOC_SIZE(cmd) != sizeof(p))
      return -EINVAL;
    if (copy_from_user(&p, uarg, sizeof(p)))
      return -EFAULT;

    obj = drm_gem_object_lookup(file, p.handle);
    if (!obj)
      return -ENOENT;
    if (obj->funcs != &nvgpu_gem_funcs) {
      drm_gem_object_put(obj);
      return -ENOENT;
    }

    ret = drm_gem_create_mmap_offset(obj);
    if (!ret)
      p.offset = drm_vma_node_offset_addr(&obj->vma_node);
    drm_gem_object_put(obj);
    if (ret)
      return ret;

    if (copy_to_user(uarg, &p, sizeof(p)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY: {
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

    if (!file)
      return -ENOTTY;
    if (_IOC_SIZE(cmd) != sizeof(p))
      return -EINVAL;
    if (copy_from_user(&p, uarg, sizeof(p)))
      return -EFAULT;

    ret = nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, cmd, &p, sizeof(p));
    if (ret < 0)
      return ret;

    /* The host's handle never reaches userspace; a proxy stands in for it.
     * On failure the proxy code has closed the host handle already. */
    ret = nvgpu_gem_proxy_create(file, nfd, p.handle, p.memory_size,
                                 &guest_handle);
    if (ret)
      return ret;

    p.handle = guest_handle;
    if (copy_to_user(uarg, &p, sizeof(p)))
      return -EFAULT;
    return 0;
  }

  /*
   * These two carry a pointer to an NVKMS parameter block. The guest's
   * address means nothing on the host, so the bytes travel alongside and the
   * backend gives them a host address before the call -- the same shape as
   * nvidia-modeset, which is why both go through one forwarder.
   */
  case DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY:
    if (!file)
      return -ENOTTY;
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, uarg,
                                      &nvgpu_gem_import_nvkms);

  case DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY:
    if (!file)
      return -ENOTTY;
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, uarg,
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
    if (!file)
      return -ENOTTY;
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, uarg,
                                      &nvgpu_gem_export_dmabuf);

  default:
    /*
     * Named rather than silently refused. An ioctl this stub does not answer
     * is the ICD asking for something the node cannot do yet, and -ENOTTY on
     * its own turns up much later as a device that would not initialise.
     */
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: unhandled nvidia-drm ioctl "
                         "nr=0x%02x (DRM_NVIDIA_%u) size=%u dir=%u\n",
                         nr, nr - DRM_COMMAND_BASE, _IOC_SIZE(cmd),
                         _IOC_DIR(cmd));
    return -ENOTTY;
  }
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

  if (ng->dev && ng->host_handle)
    nvgpu_gem_close(ng->dev, ng->owner_handle, ng->host_handle);

  /*
   * Give the window space back. The window is a gigabyte and a swapchain is
   * megabytes at a time, so a guest that allocates and frees buffers for an
   * hour exhausts it otherwise -- and the failure lands on whichever mapping
   * happens to be next, not on the one that leaked.
   */
  if (ng->window_valid && ng->mapping_id) {
    struct nvgpu_munmap_req *req = kzalloc(sizeof(*req), GFP_KERNEL);
    struct nvgpu_munmap_resp *resp = kzalloc(sizeof(*resp), GFP_KERNEL);

    if (req && resp) {
      req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MUNMAP);
      req->hdr.handle = cpu_to_le32(ng->owner_handle);
      req->mapping_id = cpu_to_le32(ng->mapping_id);
      nvgpu_send_recv(ng->dev, req, sizeof(*req), resp, sizeof(*resp));
    }
    kfree(req);
    kfree(resp);
  }

  drm_gem_object_release(obj);
  if (ng->owner)
    nvgpu_fd_put(ng->owner);
  kfree(ng);
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
  u64 window_off;
  u32 used;
  long ret;

  if (READ_ONCE(ng->window_valid))
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
  ret = nvgpu_ioctl_flat_h(ng->dev, ng->owner_handle,
                           NVGPU_IOCTL_GEM_MAP_OFFSET, &mo, sizeof(mo));
  if (ret < 0) {
    dev_warn(&ng->dev->vdev->dev,
             "virtio-gpu-nv: the host would not give object %u an mmap "
             "offset: %ld\n",
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
  if (window_off + obj->size > ng->dev->window.len) {
    dev_warn(&ng->dev->vdev->dev,
             "virtio-gpu-nv: a buffer at %llu+%zu runs past the %llu-byte "
             "window\n",
             window_off, obj->size, ng->dev->window.len);
    ret = -ERANGE;
    goto out_free;
  }

  ng->window_off = window_off;
  ng->mapping_id = le32_to_cpu(resp->mapping_id);
  smp_wmb(); /* the offset is readable before the flag says it is */
  WRITE_ONCE(ng->window_valid, true);
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
 * that maps the dma-buf. Write-combining, because it is device memory across a
 * PCI window and a client writing a buffer streams it.
 */
static int nvgpu_gem_object_mmap(struct drm_gem_object *obj,
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

  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = pgprot_writecombine(vm_get_page_prot(vma->vm_flags));

  return io_remap_pfn_range(vma, vma->vm_start,
                            (nvgpu_gem_phys(ng) + within) >> PAGE_SHIFT, size,
                            vma->vm_page_prot);
}

/*
 * A kernel mapping of the buffer, which is what a CPU consumer of a dma-buf
 * asks for. It is iomem -- there is no struct page behind a PCI window -- so
 * it goes into the iosys_map as such and a caller that cannot handle iomem
 * will say so rather than dereference it.
 */
static int nvgpu_gem_vmap(struct drm_gem_object *obj, struct iosys_map *map) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  void __iomem *vaddr;
  int ret;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ret;

  vaddr = ioremap_wc(nvgpu_gem_phys(ng), obj->size);
  if (!vaddr)
    return -ENOMEM;

  iosys_map_set_vaddr_iomem(map, vaddr);
  return 0;
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
  int ret;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ERR_PTR(ret);

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
 * Stand a guest object in front of a host one and return the guest handle.
 *
 * `size` is what the core reports for the object and what it validates
 * framebuffer dimensions against, so it has to be at least the real buffer.
 * Page-aligned because the core rejects an object smaller than a page.
 *
 * The host handle is this function's from the moment it is called: every
 * failure closes it exactly once. Callers used to close it again after a
 * failed drm_gem_handle_create(), whose put had already freed the proxy and
 * sent GEM_CLOSE -- and a second close of a number the host may have reused
 * for a newer object closes that one.
 */
int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *owner,
                           u32 host_handle, size_t size, u32 *guest_handle) {
  struct nvgpu_gem_object *ng;
  int ret;

  size = PAGE_ALIGN(size);
  if (!size)
    size = PAGE_SIZE;

  ng = kzalloc(sizeof(*ng), GFP_KERNEL);
  if (!ng) {
    nvgpu_gem_close(owner->dev, owner->handle, host_handle);
    return -ENOMEM;
  }

  mutex_init(&ng->map_lock);
  drm_gem_private_object_init(file->minor->dev, &ng->base, size);
  ng->base.funcs = &nvgpu_gem_funcs;
  ng->dev = owner->dev;
  nvgpu_fd_get(owner);
  ng->owner = owner;
  ng->owner_handle = owner->handle;
  ng->host_handle = host_handle;
  ng->obj_type = NVGPU_GEM_OBJECT_NVKMS;

  ret = drm_gem_handle_create(file, &ng->base, guest_handle);
  /* The handle holds the only reference now, or nothing does and it is
   * freed -- which closes the host handle and drops the owner. */
  drm_gem_object_put(&ng->base);
  return ret;
}

/*
 * Guest handle → the host handle it stands for, and the backend handle to
 * forward on. Fails for anything that is not one of our proxies rather than
 * forwarding a number that would name some unrelated host object.
 */
int nvgpu_gem_to_host(struct drm_file *file, u32 guest_handle,
                      u32 *host_handle, u32 *owner_handle) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, guest_handle);
  int ret = -ENOENT;

  if (!obj)
    return -ENOENT;

  if (obj->funcs == &nvgpu_gem_funcs) {
    struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);

    *host_handle = ng->host_handle;
    if (owner_handle)
      *owner_handle = ng->owner_handle;
    ret = 0;
  }

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
static long nvgpu_gem_identify(struct drm_file *file, void __user *uarg) {
  struct {
    __u32 handle;
    __u32 object_type;
  } p;
  struct drm_gem_object *obj;

  if (copy_from_user(&p, uarg, sizeof(p)))
    return -EFAULT;

  obj = drm_gem_object_lookup(file, p.handle);
  if (obj && obj->funcs == &nvgpu_gem_funcs)
    p.object_type = to_nvgpu_gem(obj)->obj_type;
  else
    p.object_type = NVGPU_GEM_OBJECT_UNKNOWN;
  if (obj)
    drm_gem_object_put(obj);

  if (copy_to_user(uarg, &p, sizeof(p)))
    return -EFAULT;
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
static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg,
                                       const struct nvgpu_gem_nested_desc *d) {
  u8 outer[NVGPU_GEM_OUTER_MAX];
  void __user *user_nested;
  u64 nested_size64;
  u32 nested_size;
  unsigned int sz = _IOC_SIZE(cmd);
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  u32 fwd_handle = nfd->handle;
  u32 caller_handle = 0;
  u32 used, data_len, nested_len;

  /*
   * The caller's struct has to be the one this descriptor describes, or the
   * pointer is not where we are about to read it from. Named rather than
   * clamped: a size that is not the expected one means the guest's userspace
   * driver and this table disagree about a UAPI struct, and reading a pointer
   * out of the wrong offset would forward a plausible-looking address.
   */
  if (sz != d->size) {
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
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

  if (copy_from_user(outer, uarg, d->size))
    return -EFAULT;

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
    u32 host_handle;

    caller_handle = get_unaligned_le32(outer + d->handle_offset);
    ret = nvgpu_gem_to_host(file, caller_handle, &host_handle, &fwd_handle);
    if (ret)
      return ret;
    put_unaligned_le32(host_handle, outer + d->handle_offset);
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
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(fwd_handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(d->size);
  req->nested_offset = cpu_to_le32(d->size);
  req->nested_len = cpu_to_le32(nested_size);
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  memcpy(req_buf + sizeof(*req), outer, d->size);

  if (user_nested && nested_size > 0) {
    if (copy_from_user(req_buf + sizeof(*req) + d->size, user_nested,
                       nested_size)) {
      ret = -EFAULT;
      goto out;
    }

    /*
     * NVKMS names the memory by an open file. Our descriptor is not the
     * backend's, so it goes across as the handle the backend issued when we
     * opened that file, and the backend turns it back into one of its own
     * descriptors before making the call -- the same round trip
     * EXPORT_OBJECT_TO_FD already makes for RM.
     *
     * The guest's own value is put back by the backend before it answers, so
     * userspace reads back the descriptor it passed.
     */
    if (d->fd_offset != NVGPU_GEM_NO_FD &&
        nested_size >= (u32)d->fd_offset + 4) {
      void *nested = req_buf + sizeof(*req) + d->size;
      int guest_fd = (int)get_unaligned_le32(nested + d->fd_offset);
      u32 handle;

      ret = nvgpu_handle_for_fd(guest_fd, &handle);
      if (ret) {
        dev_warn_ratelimited(&nfd->dev->vdev->dev,
                             "virtio-gpu-nv: nvidia-drm ioctl nr=0x%02x names "
                             "fd %d, which is not one of our devices\n",
                             _IOC_NR(cmd), guest_fd);
        goto out;
      }
      put_unaligned_le32(handle, nested + d->fd_offset);
    }
  }

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);
  if (nvgpu_resp_has(used, 0, sizeof(*resp))) {
    data_len = le32_to_cpu(resp->data_len);
    nested_len = le32_to_cpu(resp->nested_len);
  } else {
    data_len = 0;
    nested_len = 0;
  }

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
      if (d->handle_is_out) {
        /*
         * The host made an object. Stand a proxy in front of it before the
         * caller sees anything: the host's handle means nothing in this
         * guest, and the core's PRIME and GEM_CLOSE paths need an object of
         * ours to work on.
         */
        u32 host_handle = get_unaligned_le32(out + d->handle_offset);
        u64 obj_size = 0;
        u32 guest_handle;
        int cret;

        if (d->size_field_offset != NVGPU_GEM_NO_FIELD)
          obj_size = get_unaligned_le64(out + d->size_field_offset);

        /* A failure has closed the host handle already. */
        cret = nvgpu_gem_proxy_create(file, nfd, host_handle, (size_t)obj_size,
                                      &guest_handle);
        if (cret) {
          ret = cret;
          goto out;
        }
        put_unaligned_le32(guest_handle, out + d->handle_offset);
      } else {
        /* The caller reads back the handle it passed, not the host's. */
        put_unaligned_le32(caller_handle, out + d->handle_offset);
      }
    }

    if (copy_to_user(uarg, out, data_len))
      ret = -EFAULT;
  }

  if (user_nested && nested_size > 0 && nested_len > 0) {
    u32 copy_back = min(nested_size, nested_len);

    if (nvgpu_resp_has(used, sizeof(*resp) + d->size, copy_back) &&
        copy_to_user(user_nested, resp_buf + sizeof(*resp) + d->size,
                     copy_back))
      ret = -EFAULT;
  }

out:
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
static int nvgpu_drm_open(struct drm_device *drm, struct drm_file *file) {
  struct nvgpu_dri_dev *dri = drm->dev_private;
  struct nvgpu_device *dev;
  struct nvgpu_fd *nfd = NULL;
  struct nvgpu_open_req *req = NULL;
  struct nvgpu_open_resp *resp = NULL;
  int ret;

  if (!dri || !dri->dev)
    return -ENODEV;
  dev = dri->dev;

  nfd = kzalloc(sizeof(*nfd), GFP_KERNEL);
  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !req || !resp) {
    ret = -ENOMEM;
    goto err;
  }

  nfd->dev = dev;
  nfd->device_type = NVGPU_DEV_DRI_BASE + dri->index;
  /* The file's own reference; GEM proxies it owns add theirs. */
  refcount_set(&nfd->ref, 1);

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->device_type = cpu_to_le32(nfd->device_type);
  req->flags = cpu_to_le32(O_RDWR);

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0)
    goto err;

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto err;
  }

  nfd->handle = le32_to_cpu(resp->hdr.handle);
  nfd->drm_file = file;
  nvgpu_fd_register(nfd->dev, nfd);
  file->driver_priv = nfd;
  kfree(req);
  kfree(resp);
  return 0;

err:
  kfree(nfd);
  kfree(req);
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

struct nvgpu_fd *nvgpu_drm_file_nfd(struct file *f) {
  struct drm_file *file;

  if (f->f_op != &nvgpu_drm_fops)
    return NULL;
  file = f->private_data;
  return file ? file->driver_priv : NULL;
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
/*
 * The DRM node's ioctl entry point.
 *
 * Three kinds of ioctl arrive on /dev/dri/renderD128:
 *
 *   driver range (DRM_COMMAND_BASE..END)  nvidia-drm's own -- GET_DEV_INFO and
 *                                         the two SUPPORTED probes. drm_ioctl()
 *                                         answers -EINVAL for these because we
 *                                         register no drm_ioctl_desc table, so
 *                                         they are taken here first.
 *   other type 'd'                        core DRM: VERSION, GET_UNIQUE, ...
 *                                         left to drm_ioctl().
 *   type 'F'                              NVIDIA RM, proxied to the host like
 *                                         on any other node.
 */
static long nvgpu_drm_unlocked_ioctl(struct file *filp, unsigned int cmd,
                                     unsigned long arg) {
  struct drm_file *file = filp->private_data;
  struct nvgpu_fd *nfd;
  unsigned int nr = _IOC_NR(cmd);

  if (!file || !file->driver_priv)
    return -ENODEV;
  nfd = file->driver_priv;

  if (_IOC_TYPE(cmd) == DRM_IOCTL_BASE) {
    if (nr >= DRM_COMMAND_BASE && nr < DRM_COMMAND_END) {
      struct nvgpu_dri_dev *dri = file->minor->dev->dev_private;

      if (!dri)
        return -ENODEV;
      return nvgpu_drm_handle_ioctl(nfd, dri, file, cmd, arg);
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
      long ret = drm_ioctl(filp, cmd, arg);

      if (ret < 0)
        dev_warn_ratelimited(&nfd->dev->vdev->dev,
                             "virtio-gpu-nv: core DRM ioctl nr=0x%02x answered "
                             "locally with %ld\n",
                             nr, ret);
      return ret;
    }
  }

  return nvgpu_ioctl_fd(nfd, cmd, arg);
}

static const struct file_operations nvgpu_drm_fops = {
    .owner = THIS_MODULE,
#if defined(FOP_UNSIGNED_OFFSET)
    .fop_flags = FOP_UNSIGNED_OFFSET,
#endif
    .open = drm_open,
    .release = nvgpu_drm_release,
    .unlocked_ioctl = nvgpu_drm_unlocked_ioctl,
    .compat_ioctl = nvgpu_drm_unlocked_ioctl,
    .mmap = drm_gem_mmap,
    .poll = drm_poll,
    .read = drm_read,
    .llseek = noop_llseek,
};

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
    .fops = &nvgpu_drm_fops,
    .name = "nvidia-drm",
    .desc = "NVIDIA DRM driver",
    .major = 0,
    .minor = 0,
    .patchlevel = 0,
};

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
     * Find the pci_dev that owns this DRI device so we can:
     *   a) Use it as the parent of the device_create() call — this causes
     *      the kernel to create /sys/dev/char/M:N/device → pci_dev, which
     *      is what Vulkan/EGL reads when it traverses the sysfs char-dev tree.
     *   b) Create drm/<name> kobjects under the PCI device, which gives
     *      /sys/bus/pci/devices/<addr>/drm/<name> — required by the NVIDIA
     *      Vulkan ICD when it enumerates display engines.
     *
     * We match by gpu_id (minor number) against the GPU slots in config space.
     */
    struct device *pci_parent = &dev->vdev->dev; /* fallback */
    struct kobject *pci_kobj = NULL;
    struct drm_device *drm;
    int gi;

    for (gi = 0; gi < dev->num_pci_roots; gi++) {
      struct nvgpu_pci_root *root = &dev->pci_roots[gi];

      if (!root->registered || !root->pdev)
        continue;

      /* Match: the DRI device belongs to this GPU if the GPU's minor number
       * (which equals the /dev/nvidia<minor> index) matches the gpu_id field
       * set from the host.  gpu_id is the 32-bit RM client GPU identifier,
       * but we stored minor there from the VMM side — see device.rs. */
      {
        u32 slot_minor = le32_to_cpu(dev->gpu_slots[gi].minor);
        if (slot_minor != dri->slot_index && gi != 0)
          continue; /* only fall through for GPU 0 as a last resort */
      }

      pci_parent = &root->pdev->dev;
      pci_kobj = &root->pdev->dev.kobj;
      break;
    }

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
    dev_info(&dev->vdev->dev,
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
     * tree this used to build by hand. */
    drm_dev_unregister(dri->drm);
    drm_dev_put(dri->drm);
    dri->drm = NULL;
    dri->registered = false;
  }
}
