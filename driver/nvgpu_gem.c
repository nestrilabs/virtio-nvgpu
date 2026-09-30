// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: GEM proxies, the guest objects that stand in front of the
 * host's (nvgpu.h, "GEM objects, proxied") -- their lifetime and
 * tombstones, the host's memory reached through the shared window (mmap,
 * vmap, dma-buf export, PRIME import), and the Wayland channel's dma-bufs to
 * and from host objects. The ioctls that make and name them are
 * nvgpu_drm.c's.
 */

#include <drm/drm.h>
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/fcntl.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <linux/unaligned.h>
#include <linux/wait.h>

#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_prime.h>

#include "nvgpu.h"

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
 * live on, and a late reply naming one of their handles would be taken for
 * nobody's and closed under them. The xarray's lock keeps
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

/* DRM_NVIDIA_GEM_MAP_OFFSET, 0x0a past DRM_COMMAND_BASE (nvgpu_drm.c). */
#define NVGPU_IOCTL_GEM_MAP_OFFSET                                             \
  _IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x0a,                               \
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

  /* Killable: another mapper of the object holds it across the two host
   * calls below, each up to the transport's timeout. */
  if (mutex_lock_killable(&ng->map_lock))
    return -EINTR;
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

  /* One segment, whose length is an unsigned int: a larger object (a
   * window of more than 4 GiB) would be handed over truncated. */
  if (obj->size > UINT_MAX)
    return ERR_PTR(-E2BIG);
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
struct drm_gem_object *nvgpu_gem_prime_import(struct drm_device *dev,
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
    /* An import's number may be another import's proxy's, made meanwhile
     * (the host hands the same buffer the same handle, drm_prime.c:306-310):
     * closed only if no proxy holds it, as above (S-11). */
    nvgpu_gem_close_unheld(owner, host_handle);
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
int nvgpu_gem_proxy_create_new(struct drm_file *file, struct nvgpu_fd *owner,
                               u32 host_handle, size_t size,
                               u32 *guest_handle) {
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
long nvgpu_gem_identify(struct drm_file *file, void *karg) {
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
