// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: the /dev/nvgpu-wl misc device, the guest end of the Wayland
 * channel (DESIGN §7; UAPI in uapi/nvgpu_wl.h).
 *
 * The guest daemon (nvgpu-wl-guest) proxies guest Wayland clients to the host
 * compositor. It parses and translates everything itself; what it cannot do
 * from userspace is name a guest object to the host, or turn a host object
 * into a guest file. That is all this file does:
 *
 *   - SEND: a client's dma-buf becomes the host GEM object behind it, as the
 *     (owner handle, host GEM) of our GEM proxy, so the backend can PRIME
 *     export the very same object on the owner's render file. The dma-buf is
 *     held until the host has answered, so the proxy (and with it the owner's
 *     render handle) cannot go away while the export is in flight.
 *   - RECV: a host DRM file (a lease, or the lease device's query fd) arrives
 *     as a backend handle and is adopted into a new guest DRM file cloned
 *     from a card-node template the daemon passes; in export mode a host
 *     dma-buf arrives as a backend handle and is imported into the daemon's
 *     render file and exported as a guest dma-buf.
 *   - HELLO: what the daemon needs to translate the rest -- the device map
 *     (host dev_t ↔ guest dev_t of each DRM node) and the clock offset.
 *
 * Frames are otherwise opaque here: records, blobs, shm bytes and stream data
 * go through untouched, and the backend validates everything anyway.
 *
 * Each open file is one channel: CONNECT opens a DEV_WAYLAND backend handle,
 * which is one connection to the host compositor (or, in export mode, the
 * listener or one accepted host client). Readiness is the backend watching
 * the connection's eventfd: legacy EVENT_READY on the handle, or EV_READY
 * keyed by it, both of which land in `pending` here.
 */

#include <linux/dma-buf.h>
#include <linux/fdtable.h>
#include <linux/file.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/poll.h>
#include <linux/slab.h>
#include <linux/uaccess.h>

#include <drm/drm_device.h>
#include <drm/drm_file.h>
#include <drm/drm_ioctl.h>

#include "nvgpu.h"
#include "uapi/nvgpu_wl.h"

MODULE_IMPORT_NS("DMA_BUF");

/*
 * Provided by other files of the module. These prototypes belong in nvgpu.h;
 * they are here until the owners of those files add them there.
 *
 * nvgpu_drm.c: if @buf is one of our GEM proxies' dma-bufs (ops ==
 * nvgpu_dmabuf_ops and the object is on one of this device's DRM devices),
 * its (owner backend handle, host GEM handle); else -EINVAL.
 */
int nvgpu_dmabuf_to_host(struct dma_buf *buf, u32 *owner, u32 *gem);
/*
 * nvgpu_drm.c: make a GEM proxy for host GEM @host_gem, which lives in the
 * render handle of @drm_filp (a guest render-node file of ours), and return a
 * new dma-buf descriptor for it (@o_flags: O_CLOEXEC | O_RDWR), or -errno.
 */
int nvgpu_dmabuf_from_host(struct file *drm_filp, u32 host_gem, u64 size,
                           int o_flags);
/*
 * KMS workstream (DESIGN §4.2): adopt backend handle @kms_handle (a host DRM
 * file of kind @kind) into a new guest DRM file cloned from template @tmpl,
 * returning its descriptor. Ownership of the handle passes to it: on failure
 * it has closed the handle unless the clone consumed it.
 */
int nvgpu_adopt_drm_file(struct file *tmpl, u32 kms_handle, u32 kind,
                         int o_flags);
/* Called from probe (after HELLO and DRI registration) and remove. */
int nvgpu_wl_init(struct nvgpu_device *dev);
void nvgpu_wl_cleanup(struct nvgpu_device *dev);

#define NVGPU_WL_MAX_DEVS 8

struct nvgpu_wl_dev {
  struct miscdevice misc;
  struct nvgpu_device *dev;
  char name[16];
};

static struct nvgpu_wl_dev *nvgpu_wl_devs[NVGPU_WL_MAX_DEVS];
static DEFINE_MUTEX(nvgpu_wl_devs_lock);

struct nvgpu_wl_file {
  /* Handle, waitqueue and `pending`, and the registry entry legacy
   * EVENT_READY finds the handle by. */
  struct nvgpu_fd nfd;
  /* The same wake-up for EV_READY on the handle (protocol v2 pump). */
  struct nvgpu_ev_consumer ev;
  struct nvgpu_wl_dev *wl;
  /* One ioctl at a time: frames on one channel are ordered. */
  struct mutex lock;
  bool bound;
  u32 mode;
  /* The last RECV said more is waiting. */
  bool more;
};

/* ── helpers ── */

static u32 nvgpu_wl_max_frame(struct nvgpu_device *dev) {
  u32 m = min(dev->max_req, dev->max_resp);

  return m > sizeof(struct nvgpu_msg_hdr) ? m - sizeof(struct nvgpu_msg_hdr)
                                          : 0;
}

static void nvgpu_wl_ev_deliver(struct nvgpu_ev_consumer *c, u32 kind,
                                u64 cookie, const void *payload, u32 len) {
  struct nvgpu_wl_file *wf = container_of(c, struct nvgpu_wl_file, ev);

  /* IRQ context: flag and wake only. */
  atomic_set(&wf->nfd.pending, 1);
  wake_up_interruptible(&wf->nfd.wq);
}

static int nvgpu_wl_desc_get(struct nvgpu_tbuf *tb, size_t base, u32 i,
                             struct nvgpu_wl_desc *d) {
  return nvgpu_tbuf_read(tb, base + sizeof(struct nvgpu_wl_frame_hdr) +
                                 (size_t)i * sizeof(*d),
                         d, sizeof(*d));
}

static int nvgpu_wl_desc_put(struct nvgpu_tbuf *tb, size_t base, u32 i,
                             const struct nvgpu_wl_desc *d) {
  return nvgpu_tbuf_write(tb, base + sizeof(struct nvgpu_wl_frame_hdr) +
                                  (size_t)i * sizeof(*d),
                          d, sizeof(*d));
}

/*
 * Check a frame's header against its length: the descriptor table must fit
 * and the records take exactly the rest. Records themselves are not looked
 * at (the backend and the daemon do that).
 */
static int nvgpu_wl_frame_check(const struct nvgpu_wl_frame_hdr *h, u32 len,
                                u32 max_desc) {
  u64 need;

  if (le32_to_cpu((__le32)h->magic) != NVGPU_WL_FRAME_MAGIC ||
      le16_to_cpu((__le16)h->version) != NVGPU_WL_FRAME_VERSION)
    return -EPROTO;
  if (le16_to_cpu((__le16)h->ndesc) > max_desc)
    return -E2BIG;
  need = sizeof(*h) +
         (u64)le16_to_cpu((__le16)h->ndesc) * sizeof(struct nvgpu_wl_desc) +
         le32_to_cpu((__le32)h->rec_len);
  return need == len ? 0 : -EINVAL;
}

/* ── HELLO ── */

static long nvgpu_wl_hello(struct nvgpu_wl_file *wf, void __user *uarg) {
  struct nvgpu_device *dev = wf->wl->dev;
  struct nvgpu_wl_hello *h;
  s64 now;
  int i, n = 0;
  long ret = 0;

  h = kzalloc(sizeof(*h), GFP_KERNEL);
  if (!h)
    return -ENOMEM;

  h->version = NVGPU_WL_UAPI_VERSION;
  if (dev->backend_caps & NVGPU_BCAP_WAYLAND)
    h->caps |= NVGPU_WL_CAP_WAYLAND;
  if (dev->backend_caps & NVGPU_BCAP_WL_EXPORT)
    h->caps |= NVGPU_WL_CAP_EXPORT | NVGPU_WL_CAP_DMABUF_IMPORT;
  /* Adoption needs a card-node template, which the daemon brings. */
  h->caps |= NVGPU_WL_CAP_DRM_FILE;

  /*
   * The offset is what the kernel's slewed estimate says now; the daemon asks
   * again every few seconds, and a timestamp moved by a stale offset is off by
   * the slew since (at most 50 µs/s), not by the offset.
   */
  now = ktime_get_ns();
  h->clock_offset_ns = nvgpu_guest_to_host_ns(dev, now) - now;
  h->max_frame = nvgpu_wl_max_frame(dev);

  /*
   * The device map: host compositors name the GPU they render on by the
   * dev_t of its render node (linux-dmabuf main_device, tranche target
   * device), and a guest client looks that number up in *its* /dev. The host
   * numbers of each render node came with GET_SYS_FILES; the guest's are
   * whatever minor the DRM core gave our node. Card nodes are added when
   * the host's card numbers are known here (compositor-VM mode).
   */
  for (i = 0; i < dev->num_dri_devs && n < NVGPU_WL_MAX_DEVMAP; i++) {
    struct nvgpu_dri_dev *dri = &dev->dri_devs[i];

    if (!dri->registered || !dri->drm || !dri->drm->render)
      continue;
    h->dev[n].host_major = dri->major;
    h->dev[n].host_minor = dri->minor;
    h->dev[n].guest_major = DRM_MAJOR;
    h->dev[n].guest_minor = dri->drm->render->index;
    h->dev[n].flags = NVGPU_WL_DEV_RENDER;
    n++;
  }
  h->ndev = n;

  if (copy_to_user(uarg, h, sizeof(*h)))
    ret = -EFAULT;
  kfree(h);
  return ret;
}

/* ── CONNECT ── */

static long nvgpu_wl_connect(struct nvgpu_wl_file *wf, void __user *uarg) {
  struct nvgpu_device *dev = wf->wl->dev;
  struct nvgpu_wl_connect c;
  struct nvgpu_open_req req = {};
  struct nvgpu_open_resp resp = {};
  s32 status;
  int ret;

  if (copy_from_user(&c, uarg, sizeof(c)))
    return -EFAULT;
  if (c.flags || c.mode > NVGPU_WL_ACCEPT)
    return -EINVAL;
  if (wf->bound)
    return -EBUSY;

  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req.device_type = cpu_to_le32(NVGPU_DEV_WAYLAND);
  /* The OPEN flags word says which kind of channel (WL_OPEN_* in wlwire). */
  req.flags = cpu_to_le32(c.mode);
  ret = nvgpu_send_recv(dev, &req, sizeof(req), &resp, sizeof(resp));
  if (ret < 0)
    return ret;
  status = (s32)le32_to_cpu(resp.hdr.status);
  if (status < 0)
    return status;

  wf->nfd.dev = dev;
  wf->nfd.handle = le32_to_cpu(resp.hdr.handle);
  wf->nfd.device_type = NVGPU_DEV_WAYLAND;
  wf->mode = c.mode;
  /*
   * Findable by handle for legacy EVENT_READY. Not nvgpu_fd_register(): that
   * initialises the waitqueue, and a poller of the still-unbound file may be
   * on it already (open initialised it).
   */
  atomic_set(&wf->nfd.pending, 0);
  {
    unsigned long flags;

    spin_lock_irqsave(&dev->fds_lock, flags);
    list_add(&wf->nfd.node, &dev->fds);
    spin_unlock_irqrestore(&dev->fds_lock, flags);
  }
  wf->ev.deliver = nvgpu_wl_ev_deliver;
  ret = nvgpu_ev_register(dev, &wf->ev, NVGPU_EVKEY_HANDLE(wf->nfd.handle));
  if (ret < 0) {
    nvgpu_fd_unregister(dev, &wf->nfd);
    nvgpu_close_handle(dev, wf->nfd.handle);
    return ret;
  }
  wf->bound = true;
  /* The backend queues its HELLO record at once. */
  atomic_set(&wf->nfd.pending, 1);
  return 0;
}

/* ── SEND ── */

/*
 * A client's dma-buf plane, for the host: ours becomes (owner, host GEM) and
 * its reference is kept in @held until the host answered; anyone else's is
 * sent as invalid, which the host turns into a placeholder the compositor
 * refuses (the client sees zwp_linux_buffer_params_v1.failed).
 */
static void nvgpu_wl_resolve_dmabuf(struct nvgpu_device *dev,
                                    struct nvgpu_wl_desc *d,
                                    struct dma_buf **held) {
  struct dma_buf *buf;
  u32 owner, gem;

  if (d->flags & NVGPU_WL_DESC_F_INVALID)
    goto invalid;
  buf = dma_buf_get(d->fd);
  if (IS_ERR(buf))
    goto invalid;
  if (nvgpu_dmabuf_to_host(buf, &owner, &gem) < 0) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: wayland: a client's dma-buf is not "
                         "one of ours; the host gets a placeholder\n");
    dma_buf_put(buf);
    goto invalid;
  }
  d->a = owner;
  d->b = gem;
  d->fd = -1;
  *held = buf;
  return;

invalid:
  d->flags |= NVGPU_WL_DESC_F_INVALID;
  d->a = 0;
  d->b = 0;
  d->fd = -1;
}

static long nvgpu_wl_send(struct nvgpu_wl_file *wf, void __user *uarg) {
  struct nvgpu_device *dev = wf->wl->dev;
  const size_t H = sizeof(struct nvgpu_msg_hdr);
  struct nvgpu_wl_xfer x;
  struct nvgpu_msg_hdr hdr = {};
  struct nvgpu_wl_frame_hdr fh;
  struct nvgpu_wl_send_resp sr;
  struct nvgpu_tbuf *req = NULL, *resp = NULL;
  struct dma_buf **held = NULL;
  u32 ndesc = 0, used = 0, i;
  s32 status;
  long ret;

  if (copy_from_user(&x, uarg, sizeof(x)))
    return -EFAULT;
  if (!wf->bound || wf->mode == NVGPU_WL_LISTEN)
    return -ENOTCONN;
  if (x.len < sizeof(fh) || x.len > nvgpu_wl_max_frame(dev))
    return -EMSGSIZE;

  req = nvgpu_tbuf_alloc(H + x.len, GFP_KERNEL);
  resp = nvgpu_tbuf_alloc(H + sizeof(sr), GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out;
  }
  hdr.msg_type = cpu_to_le32(NVGPU_MSG_WL_SEND);
  hdr.handle = cpu_to_le32(wf->nfd.handle);
  ret = nvgpu_tbuf_write(req, 0, &hdr, H);
  if (!ret)
    ret = nvgpu_tbuf_write_user(req, H, u64_to_user_ptr(x.frame), x.len);
  if (!ret)
    ret = nvgpu_tbuf_read(req, H, &fh, sizeof(fh));
  if (!ret)
    ret = nvgpu_wl_frame_check(&fh, x.len, NVGPU_WL_MAX_DESC);
  if (ret)
    goto out;

  ndesc = le16_to_cpu((__le16)fh.ndesc);
  if (ndesc) {
    held = kcalloc(ndesc, sizeof(*held), GFP_KERNEL);
    if (!held) {
      ret = -ENOMEM;
      goto out;
    }
  }
  for (i = 0; i < ndesc; i++) {
    struct nvgpu_wl_desc d;

    ret = nvgpu_wl_desc_get(req, H, i, &d);
    if (ret)
      goto out;
    switch (d.kind) {
    case NVGPU_WL_DESC_DMABUF:
      nvgpu_wl_resolve_dmabuf(dev, &d, &held[i]);
      break;
    case NVGPU_WL_DESC_SHM_POOL:
    case NVGPU_WL_DESC_BLOB:
    case NVGPU_WL_DESC_STREAM:
      /* By value: no descriptor may ride along. */
      if (d.fd != -1) {
        ret = -EINVAL;
        goto out;
      }
      break;
    default:
      /* DRM files and syncobjs never go guest → host. */
      ret = -EINVAL;
      goto out;
    }
    ret = nvgpu_wl_desc_put(req, H, i, &d);
    if (ret)
      goto out;
  }

  ret = nvgpu_xfer(dev, req, resp, 0, &used);
  if (ret == -ETIMEDOUT || ret == -EINTR) {
    /* The transport owns the buffers now (nvgpu.h). */
    req = NULL;
    resp = NULL;
    goto out;
  }
  if (ret)
    goto out;
  ret = nvgpu_tbuf_read(resp, 0, &hdr, H);
  if (ret)
    goto out;
  status = (s32)le32_to_cpu(hdr.status);
  if (status < 0) {
    /* -EAGAIN: the host compositor is not reading; try again later. */
    ret = status;
    goto out;
  }
  if (used < H + sizeof(sr)) {
    ret = -EPROTO;
    goto out;
  }
  ret = nvgpu_tbuf_read(resp, H, &sr, sizeof(sr));
  if (ret)
    goto out;
  x.backlog = le32_to_cpu(sr.backlog);
  x.flags = 0;
  if (copy_to_user(uarg, &x, sizeof(x)))
    ret = -EFAULT;

out:
  if (held) {
    for (i = 0; i < ndesc; i++)
      if (held[i])
        dma_buf_put(held[i]);
    kfree(held);
  }
  if (req)
    nvgpu_tbuf_free(req);
  if (resp)
    nvgpu_tbuf_free(resp);
  return ret;
}

/* ── RECV ── */

/* A host DRM file (backend handle @handle of kind @kind) as a guest file. */
static int nvgpu_wl_adopt(struct nvgpu_device *dev, int card_fd, u32 handle,
                          u32 kind) {
  struct file *tmpl;
  int fd;

  tmpl = card_fd >= 0 ? fget(card_fd) : NULL;
  if (!tmpl) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: wayland: a DRM file arrived with no "
                         "card template to clone; dropping it\n");
    nvgpu_close_handle(dev, handle);
    return -EBADF;
  }
  /*
   * nvgpu_adopt_drm_file validates the template and owns the handle -- once
   * the template is a card file of ours, and of this device: a handle is a
   * number in one backend session, and adopted through another device's file
   * it would name whatever that session has under it. Anything else (-EBADF)
   * leaves the handle ours to close.
   */
  if (nvgpu_drm_file_nfd(tmpl) && nvgpu_drm_file_nfd(tmpl)->dev == dev)
    fd = nvgpu_adopt_drm_file(tmpl, handle, kind, O_RDWR | O_CLOEXEC);
  else
    fd = -EBADF;
  if (fd == -EBADF)
    nvgpu_close_handle(dev, handle);
  fput(tmpl);
  return fd;
}

/*
 * Export mode: a host dma-buf (backend handle @handle) imported into the
 * render handle of the daemon's render file, as a guest proxy and a guest
 * dma-buf descriptor. The backend's dma-buf handle is closed either way.
 */
static int nvgpu_wl_import(struct nvgpu_device *dev, int render_fd,
                           u32 handle) {
  struct file *rf = NULL;
  u32 render;
  u64 args[2], res[2];
  int ret;

  ret = render_fd >= 0 ? nvgpu_handle_for_fd(render_fd, &render) : -EBADF;
  if (ret < 0)
    goto out;
  args[0] = render;
  args[1] = handle;
  ret = nvgpu_host_op(dev, NVGPU_OP_DMABUF_IMPORT, args, 2, res, 2);
  if (ret < 0)
    goto out;
  rf = fget(render_fd);
  if (!rf) {
    nvgpu_gem_close_async(dev, render, (u32)res[0]);
    ret = -EBADF;
    goto out;
  }
  ret = nvgpu_dmabuf_from_host(rf, (u32)res[0], res[1], O_RDWR | O_CLOEXEC);
  if (ret < 0)
    nvgpu_gem_close_async(dev, render, (u32)res[0]);
  fput(rf);
out:
  nvgpu_close_handle(dev, handle);
  return ret;
}

static long nvgpu_wl_recv(struct nvgpu_wl_file *wf, void __user *uarg) {
  struct nvgpu_device *dev = wf->wl->dev;
  const size_t H = sizeof(struct nvgpu_msg_hdr);
  struct nvgpu_wl_xfer x;
  struct nvgpu_msg_hdr hdr = {};
  struct nvgpu_wl_recv_req rr;
  struct nvgpu_wl_frame_hdr fh;
  struct nvgpu_tbuf *req = NULL, *resp = NULL;
  int *installed = NULL;
  u32 cap, max_desc, flen, ndesc = 0, used = 0, i;
  s32 status;
  long ret;

  if (copy_from_user(&x, uarg, sizeof(x)))
    return -EFAULT;
  if (!wf->bound || wf->mode == NVGPU_WL_LISTEN)
    return -ENOTCONN;
  cap = min(x.len, nvgpu_wl_max_frame(dev));
  if (cap < sizeof(fh))
    return -EMSGSIZE;
  max_desc = min_t(u32, x.max_desc, NVGPU_WL_MAX_DESC);

  req = nvgpu_tbuf_alloc(H + sizeof(rr), GFP_KERNEL);
  resp = nvgpu_tbuf_alloc(H + cap, GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out;
  }
  hdr.msg_type = cpu_to_le32(NVGPU_MSG_WL_RECV);
  hdr.handle = cpu_to_le32(wf->nfd.handle);
  rr.max_bytes = cpu_to_le32(cap);
  rr.max_desc = cpu_to_le32(max_desc);
  ret = nvgpu_tbuf_write(req, 0, &hdr, H);
  if (!ret)
    ret = nvgpu_tbuf_write(req, H, &rr, sizeof(rr));
  if (ret)
    goto out;

  /*
   * Cleared before asking, so a wake-up that arrives while the request is in
   * flight is kept, at the cost of at most one empty RECV.
   */
  atomic_set(&wf->nfd.pending, 0);
  ret = nvgpu_xfer(dev, req, resp, 0, &used);
  if (ret == -ETIMEDOUT || ret == -EINTR) {
    /*
     * Abandoned: whatever the late reply carries (records, and backend
     * handles in its descriptors) is lost to this channel, which cannot
     * resynchronise after that; the daemon closes it on the error.
     */
    req = NULL;
    resp = NULL;
    goto out;
  }
  if (ret)
    goto out;
  ret = nvgpu_tbuf_read(resp, 0, &hdr, H);
  if (ret)
    goto out;
  status = (s32)le32_to_cpu(hdr.status);
  if (status < 0) {
    ret = status;
    goto out;
  }
  if (used < H + sizeof(fh) || used > H + cap) {
    ret = -EPROTO;
    goto out;
  }
  flen = used - H;
  ret = nvgpu_tbuf_read(resp, H, &fh, sizeof(fh));
  if (!ret)
    ret = nvgpu_wl_frame_check(&fh, flen, max_desc);
  if (ret)
    goto out;

  ndesc = le16_to_cpu((__le16)fh.ndesc);
  if (ndesc) {
    installed = kmalloc_array(ndesc, sizeof(*installed), GFP_KERNEL);
    if (!installed) {
      ret = -ENOMEM;
      goto out;
    }
  }
  for (i = 0; i < ndesc; i++) {
    struct nvgpu_wl_desc d;
    int fd = -1;

    ret = nvgpu_wl_desc_get(resp, H, i, &d);
    if (ret)
      goto out_close;
    if (!(d.flags & NVGPU_WL_DESC_F_INVALID)) {
      switch (d.kind) {
      case NVGPU_WL_DESC_DRM_FILE:
        fd = nvgpu_wl_adopt(dev, x.card_fd, d.a, d.b);
        break;
      case NVGPU_WL_DESC_DMABUF:
        fd = nvgpu_wl_import(dev, x.render_fd, d.a);
        break;
      default:
        break;
      }
      if (fd < 0 && (d.kind == NVGPU_WL_DESC_DRM_FILE ||
                     d.kind == NVGPU_WL_DESC_DMABUF)) {
        dev_warn_ratelimited(&dev->vdev->dev,
                             "virtio-gpu-nv: wayland: could not make a guest "
                             "file of a host descriptor (kind %u): %d\n",
                             d.kind, fd);
        d.flags |= NVGPU_WL_DESC_F_INVALID;
        fd = -1;
      }
    }
    /* Only descriptors installed here are ever reported as one. */
    d.fd = fd;
    installed[i] = fd;
    ret = nvgpu_wl_desc_put(resp, H, i, &d);
    if (ret) {
      i++;
      goto out_close;
    }
  }

  ret = nvgpu_tbuf_read_user(resp, H, u64_to_user_ptr(x.frame), flen);
  if (!ret) {
    wf->more = le32_to_cpu((__le32)fh.flags) & NVGPU_WL_FRAME_F_MORE;
    x.len = flen;
    x.flags = wf->more ? NVGPU_WL_XFER_MORE : 0;
    x.backlog = 0;
    if (copy_to_user(uarg, &x, sizeof(x)))
      ret = -EFAULT;
  }
  if (wf->more)
    atomic_set(&wf->nfd.pending, 1);
  if (!ret)
    goto out;
  /* The caller never learnt the descriptors: take them back. */
  i = ndesc;
out_close:
  while (i-- > 0)
    if (installed[i] >= 0)
      close_fd(installed[i]);
out:
  kfree(installed);
  if (req)
    nvgpu_tbuf_free(req);
  if (resp)
    nvgpu_tbuf_free(resp);
  return ret;
}

/* ── file operations ── */

static int nvgpu_wl_open(struct inode *inode, struct file *filp) {
  struct miscdevice *m = filp->private_data;
  struct nvgpu_wl_dev *wl = container_of(m, struct nvgpu_wl_dev, misc);
  struct nvgpu_wl_file *wf;

  if (!wl->dev->v2)
    return -ENODEV;
  wf = kzalloc(sizeof(*wf), GFP_KERNEL);
  if (!wf)
    return -ENOMEM;
  wf->wl = wl;
  mutex_init(&wf->lock);
  init_waitqueue_head(&wf->nfd.wq);
  filp->private_data = wf;
  return 0;
}

static int nvgpu_wl_release(struct inode *inode, struct file *filp) {
  struct nvgpu_wl_file *wf = filp->private_data;
  struct nvgpu_device *dev = wf->wl->dev;

  if (wf->bound) {
    /* Nothing can wake us once these return; then the host connection. */
    nvgpu_ev_unregister(dev, &wf->ev);
    nvgpu_fd_unregister(dev, &wf->nfd);
    nvgpu_close_handle(dev, wf->nfd.handle);
  }
  mutex_destroy(&wf->lock);
  kfree(wf);
  return 0;
}

static long nvgpu_wl_ioctl(struct file *filp, unsigned int cmd,
                           unsigned long arg) {
  struct nvgpu_wl_file *wf = filp->private_data;
  void __user *uarg = (void __user *)arg;
  long ret;

  if (mutex_lock_interruptible(&wf->lock))
    return -ERESTARTSYS;
  switch (cmd) {
  case NVGPU_WL_IOC_HELLO:
    ret = nvgpu_wl_hello(wf, uarg);
    break;
  case NVGPU_WL_IOC_CONNECT:
    ret = nvgpu_wl_connect(wf, uarg);
    break;
  case NVGPU_WL_IOC_SEND:
    ret = nvgpu_wl_send(wf, uarg);
    break;
  case NVGPU_WL_IOC_RECV:
    ret = nvgpu_wl_recv(wf, uarg);
    break;
  default:
    ret = -ENOTTY;
    break;
  }
  mutex_unlock(&wf->lock);
  return ret;
}

static __poll_t nvgpu_wl_poll(struct file *filp,
                              struct poll_table_struct *wait) {
  struct nvgpu_wl_file *wf = filp->private_data;
  __poll_t mask = EPOLLOUT | EPOLLWRNORM;

  poll_wait(filp, &wf->nfd.wq, wait);
  if (!wf->bound)
    return mask;
  /*
   * A channel stays readable until a RECV finds it empty: level, like a
   * socket, so a daemon that reads only part of what is waiting is woken
   * again. The export listener has no RECV to clear it, so there the report
   * is taken, as on the other devices; the backend re-reports while
   * connections are still waiting.
   */
  if (wf->mode == NVGPU_WL_LISTEN) {
    if (atomic_xchg(&wf->nfd.pending, 0))
      mask |= EPOLLIN | EPOLLRDNORM;
  } else if (atomic_read(&wf->nfd.pending) || READ_ONCE(wf->more)) {
    mask |= EPOLLIN | EPOLLRDNORM;
  }
  return mask;
}

static const struct file_operations nvgpu_wl_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_wl_open,
    .release = nvgpu_wl_release,
    .unlocked_ioctl = nvgpu_wl_ioctl,
    .compat_ioctl = compat_ptr_ioctl,
    .poll = nvgpu_wl_poll,
    .llseek = noop_llseek,
};

/* ── registration ── */

int nvgpu_wl_init(struct nvgpu_device *dev) {
  struct nvgpu_wl_dev *wl;
  int slot, ret;

  if (!dev->v2 ||
      !(dev->backend_caps & (NVGPU_BCAP_WAYLAND | NVGPU_BCAP_WL_EXPORT)))
    return 0;

  wl = kzalloc(sizeof(*wl), GFP_KERNEL);
  if (!wl)
    return -ENOMEM;

  mutex_lock(&nvgpu_wl_devs_lock);
  for (slot = 0; slot < NVGPU_WL_MAX_DEVS && nvgpu_wl_devs[slot]; slot++)
    ;
  if (slot == NVGPU_WL_MAX_DEVS) {
    mutex_unlock(&nvgpu_wl_devs_lock);
    kfree(wl);
    return -ENOSPC;
  }
  if (slot == 0)
    strscpy(wl->name, "nvgpu-wl", sizeof(wl->name));
  else
    snprintf(wl->name, sizeof(wl->name), "nvgpu-wl%d", slot);
  wl->dev = dev;
  wl->misc.minor = MISC_DYNAMIC_MINOR;
  wl->misc.name = wl->name;
  wl->misc.fops = &nvgpu_wl_fops;
  wl->misc.parent = &dev->vdev->dev;
  /* Like /dev/nvidia* and the render node: any guest process may use it.
   * Separating guest users from each other is the guest's own business. */
  wl->misc.mode = 0666;
  ret = misc_register(&wl->misc);
  if (ret) {
    mutex_unlock(&nvgpu_wl_devs_lock);
    dev_warn(&dev->vdev->dev, "virtio-gpu-nv: /dev/%s: misc_register: %d\n",
             wl->name, ret);
    kfree(wl);
    return ret;
  }
  nvgpu_wl_devs[slot] = wl;
  mutex_unlock(&nvgpu_wl_devs_lock);
  dev_info(&dev->vdev->dev, "virtio-gpu-nv: /dev/%s for the host compositor\n",
           wl->name);
  return 0;
}

void nvgpu_wl_cleanup(struct nvgpu_device *dev) {
  int slot;

  mutex_lock(&nvgpu_wl_devs_lock);
  for (slot = 0; slot < NVGPU_WL_MAX_DEVS; slot++) {
    struct nvgpu_wl_dev *wl = nvgpu_wl_devs[slot];

    if (!wl || wl->dev != dev)
      continue;
    misc_deregister(&wl->misc);
    nvgpu_wl_devs[slot] = NULL;
    kfree(wl);
  }
  mutex_unlock(&nvgpu_wl_devs_lock);
}
