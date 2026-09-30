// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: RM's EXPORT_TO_DMABUF_FD, when the backend serves it
 * (--allow-dmabuf-export, NVGPU_BCAP_DMABUF_EXPORT; device/src/rmexport.rs).
 *
 * Natively nvidia.ko makes a dma-buf of the caller's video memory and
 * installs it in the caller's descriptor table. Here the caller's table is
 * the guest's and the dma-buf is the host's, so the escape is carried as the
 * backend asks: the block as it is, the calling process, and a render file
 * of the GPU's node the host imports the new dma-buf into -- one opened for
 * the purpose, headless (nvgpu_render_open_headless()), as the caller. The
 * backend answers with the block RM left and, when RM made the dma-buf, the
 * GEM object it became in that file; that becomes a proxy and a guest
 * dma-buf (nvgpu_dmabuf_from_headless()), installed in the caller's table
 * and written to the block's fd, as nvidia.ko writes its own.
 *
 * What the caller holds is a dma-buf of this device: its render nodes
 * import it (any process it is passed to, too), and NVIDIA's userspace
 * asks of it what it asks of any imported buffer. No CPU maps it and no
 * guest device attaches to it, as nvidia.ko's own is mapped by no CPU on a
 * discrete GPU; and the backend lets no export of it leave the VM, so the
 * host compositor refuses it as a Wayland buffer. The file is released with
 * the dma-buf's last reference: the proxy goes, its host object and the
 * headless file with it, and the host's dma-buf with them.
 *
 * The block's contents are the backend's to parse and check (the caller's
 * client, one call for the whole dma-buf, the budgets). All this reads of
 * it is the first word, the descriptor, which says whether a dma-buf is
 * being made (-1) or appended to (which the backend refuses).
 *
 * A dma-buf the host made is undone, whatever fails after: the descriptor
 * number is taken before anything is asked, and installed after the copy
 * back succeeded; until then the dma-buf's put, or the headless file's,
 * takes everything back (the backend closes what it recorded with the file).
 */

#include <linux/dma-buf.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include <drm/drm_drv.h>

#include "nvgpu.h"

#define NVGPU_RM_IOCTL_TYPE 'F'
#define NV_ESC_EXPORT_TO_DMABUF_FD 0xd9

/* nv_ioctl_export_to_dma_buf_fd_t is 2600 or 2608 bytes; the backend holds
 * the size to the host's. This only bounds what is copied. */
#define NVGPU_RMX_MAX_BLOCK 4096

/* The largest dma-buf a proxy is made for, as for the Wayland imports. */
#define NVGPU_RMX_MAX_SIZE (1ull << 36)

bool nvgpu_rm_dmabuf_export_ours(const struct nvgpu_fd *nfd, unsigned int cmd) {
  const struct nvgpu_device *dev = nfd->dev;

  return _IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE &&
         _IOC_NR(cmd) == NV_ESC_EXPORT_TO_DMABUF_FD &&
         nfd->device_type < NVGPU_DEV_CTL && dev->v2 &&
         (dev->backend_caps & NVGPU_BCAP_DMABUF_EXPORT) &&
         nvgpu_proc_ids(dev);
}

/* The DRI device of GPU file @nfd's GPU, registered, or NULL. */
static struct nvgpu_dri_dev *nvgpu_rmx_dri(struct nvgpu_fd *nfd) {
  struct nvgpu_device *dev = nfd->dev;
  int i;

  for (i = 0; i < dev->num_dri_devs; i++) {
    struct nvgpu_dri_dev *d = &dev->dri_devs[i];

    if (d->registered && d->drm && d->slot_index == nfd->device_type)
      return d;
  }
  return NULL;
}

/*
 * One exchange: @block (@sz bytes) on @nfd's handle, with the caller and
 * render handle @render after it; the block RM left back into @block, and
 * the export's answer into *@out when there is one (*@have). The host
 * call's status, or a transport error.
 */
static long nvgpu_rmx_exchange(struct nvgpu_fd *nfd, unsigned int cmd,
                               void *block, u32 sz, u32 render,
                               struct nvgpu_dmabuf_export_resp *out,
                               bool *have) {
  struct nvgpu_device *dev = nfd->dev;
  struct nvgpu_dmabuf_export_req x = {.render = cpu_to_le32(render)};
  size_t req_len = sizeof(struct nvgpu_ioctl_req) + sz +
                   sizeof(struct nvgpu_proc_id) + sizeof(x);
  size_t resp_max = sizeof(struct nvgpu_ioctl_resp) + sz + sizeof(*out);
  struct nvgpu_ioctl_reply r;
  u8 *req, *resp;
  long ret;

  *have = false;
  req = kvzalloc(req_len, GFP_KERNEL);
  resp = kvzalloc(resp_max, GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out;
  }
  nvgpu_ioctl_req_init((struct nvgpu_ioctl_req *)req, nfd->handle, cmd, sz, 0,
                       0, 0, 0);
  memcpy(req + sizeof(struct nvgpu_ioctl_req), block, sz);
  nvgpu_proc_id_fill(dev, req + sizeof(struct nvgpu_ioctl_req) + sz);
  memcpy(req + sizeof(struct nvgpu_ioctl_req) + sz +
             sizeof(struct nvgpu_proc_id),
         &x, sizeof(x));

  ret = nvgpu_ioctl_exchange(dev, req, req_len, resp, resp_max, &r);
  if (ret < 0)
    goto out;
  ret = r.status;
  if (ret < 0)
    goto out;
  /* A success brings the block back whole: one without it is not one this
   * side can answer the caller with (its own bytes would read as RM's). */
  if (!r.full || r.data_len != sz || r.nested_len ||
      !nvgpu_resp_has(r.used, sizeof(struct nvgpu_ioctl_resp), sz)) {
    ret = -EPROTO;
    goto out;
  }
  memcpy(block, resp + sizeof(struct nvgpu_ioctl_resp), sz);
  if (ret >= 0 && r.deep_len == sizeof(*out) &&
      nvgpu_resp_has(r.used, sizeof(struct nvgpu_ioctl_resp) + sz,
                     sizeof(*out))) {
    memcpy(out, resp + sizeof(struct nvgpu_ioctl_resp) + sz, sizeof(*out));
    *have = true;
  } else if (ret >= 0 && r.deep_len) {
    ret = -EPROTO;
  }
out:
  kvfree(req);
  kvfree(resp);
  return ret;
}

long nvgpu_rm_dmabuf_export(struct nvgpu_fd *nfd, unsigned int cmd,
                            void __user *uarg) {
  struct nvgpu_dmabuf_export_resp out;
  struct nvgpu_fd *render = NULL;
  struct dma_buf *buf = NULL;
  struct nvgpu_dri_dev *dri = NULL;
  u32 sz = _IOC_SIZE(cmd), gem = 0;
  int fd = -1, idx;
  bool have = false;
  s32 fd_in;
  u8 *block;
  long ret;

  if (sz < sizeof(fd_in) || sz > NVGPU_RMX_MAX_BLOCK)
    return -EINVAL;
  block = kvmalloc(sz, GFP_KERNEL);
  if (!block)
    return -ENOMEM;
  if (copy_from_user(block, uarg, sz)) {
    ret = -EFAULT;
    goto out;
  }
  fd_in = (s32)get_unaligned_le32(block);

  /*
   * A dma-buf to make: a headless render file for it, and the descriptor
   * number, both before anything is asked, so that nothing but the copy
   * back can fail once the host has made it. Any other descriptor is the
   * append form, which the backend answers without RM.
   */
  if (fd_in == -1) {
    dri = nvgpu_rmx_dri(nfd);
    if (!dri) {
      ret = -ENODEV;
      goto out;
    }
    render = nvgpu_render_open_headless(dri);
    if (IS_ERR(render)) {
      ret = PTR_ERR(render);
      render = NULL;
      goto out;
    }
    fd = get_unused_fd_flags(O_CLOEXEC);
    if (fd < 0) {
      ret = fd;
      goto out;
    }
  }

  ret = nvgpu_rmx_exchange(nfd, cmd, block, sz, render ? render->handle : 0,
                           &out, &have);
  if (ret < 0)
    goto out;
  /* The caller's own descriptor field goes back as it sent it, unless an
   * export is made below: the backend sends -1 there, never a number of
   * its own. */
  put_unaligned_le32((u32)fd_in, block);

  if (have) {
    /*
     * Checked before anything is made of it: a GEM handle is non-zero, the
     * object a dma-buf one, of a size a proxy can stand for. A refused one
     * needs no close of its own: nothing else is in the headless file,
     * whose close below takes it.
     */
    if (!render || !dri || !nvgpu_res_u32(le32_to_cpu(out.gem), &gem) ||
        le32_to_cpu(out.object_type) != NVGPU_GEM_OBJECT_DMABUF ||
        !le64_to_cpu(out.size) || le64_to_cpu(out.size) > NVGPU_RMX_MAX_SIZE) {
      ret = -EPROTO;
      goto out;
    }
    if (!drm_dev_enter(dri->drm, &idx)) {
      ret = -ENODEV;
      goto out;
    }
    buf = nvgpu_dmabuf_from_headless(dri->drm, render, gem,
                                     le64_to_cpu(out.size),
                                     NVGPU_GEM_OBJECT_DMABUF,
                                     O_RDWR | O_CLOEXEC);
    drm_dev_exit(idx);
    if (IS_ERR(buf)) {
      ret = PTR_ERR(buf);
      buf = NULL;
      goto out;
    }
    put_unaligned_le32((u32)fd, block);
  }

  if (copy_to_user(uarg, block, sz)) {
    ret = -EFAULT;
    goto out;
  }
  if (buf) {
    /* The descriptor is the dma-buf's from here, and the dma-buf the
     * caller's. */
    fd_install(fd, buf->file);
    fd = -1;
    buf = NULL;
  }
out:
  if (buf)
    dma_buf_put(buf);
  if (fd >= 0)
    put_unused_fd(fd);
  /* The proxy holds the file if one was made; otherwise this closes it, and
   * the backend closes whatever it imported there. */
  if (render)
    nvgpu_fd_put(render);
  kvfree(block);
  return ret;
}
