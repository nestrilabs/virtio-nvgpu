// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: nvidia-drm's semaphore-surface fences (SEMSURF_FENCE_*) on
 * a guest DRM file (nvgpu_fence.c's header has the design): fence contexts
 * as proxy GEM objects, a buffer re-homed into the file a context lives in,
 * and a wait on a fence only the guest can see deferred to a callback, as
 * the host returns at once.
 */

#include <drm/drm.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <linux/dma-fence.h>
#include <linux/hashtable.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/sync_file.h>
#include <linux/workqueue.h>

#include "nvgpu_fence.h"

/* ───────── nvidia-drm semaphore-surface fences ───────── */

/* nv_drm_common_ioctl.h:355-423 (610.57.04). */
struct nvgpu_semsurf_ctx_create {
  __u64 index;
  __u64 nvkms_params_ptr;
  __u64 nvkms_params_size;
  __u32 handle;
  __u32 __pad;
};

struct nvgpu_semsurf_create {
  __u32 fence_context_handle;
  __u32 timeout_value_ms;
  __u64 wait_value;
  __s32 fd;
  __u32 __pad;
};

struct nvgpu_semsurf_wait {
  __u32 fence_context_handle;
  __s32 fd;
  __u64 pre_wait_value;
  __u64 post_wait_value;
};

struct nvgpu_semsurf_attach {
  __u32 handle;
  __u32 fence_context_handle;
  __u32 timeout_value_ms;
  __u32 shared;
  __u64 wait_value;
};

static_assert(sizeof(struct nvgpu_semsurf_ctx_create) == 32);
static_assert(sizeof(struct nvgpu_semsurf_create) == 24);
static_assert(sizeof(struct nvgpu_semsurf_wait) == 24);
static_assert(sizeof(struct nvgpu_semsurf_attach) == 24);

#define NVGPU_IOCTL_SEMSURF_CTX_CREATE                                         \
  _IOWR(DRM_IOCTL_BASE, 0x54, struct nvgpu_semsurf_ctx_create)
#define NVGPU_IOCTL_SEMSURF_CREATE                                             \
  _IOWR(DRM_IOCTL_BASE, 0x55, struct nvgpu_semsurf_create)
#define NVGPU_IOCTL_SEMSURF_WAIT                                               \
  _IOW(DRM_IOCTL_BASE, 0x56, struct nvgpu_semsurf_wait)
#define NVGPU_IOCTL_SEMSURF_ATTACH                                             \
  _IOW(DRM_IOCTL_BASE, 0x57, struct nvgpu_semsurf_attach)

/*
 * A fence context: a GEM object in the host file that made it
 * (nvidia-drm-fence.c:439-456), standing for a semaphore surface NVKMS
 * imported. The guest's proxy is not an nvgpu_gem_object: nothing may export
 * it (no dma-buf of it exists natively either) or map it, and GEM_IDENTIFY
 * answers UNKNOWN for it, as it does for anything not a buffer proxy
 * (nvgpu_gem_identify()). The host handle is closed with the proxy.
 */
struct nvgpu_fence_ctx {
  struct drm_gem_object base;
  struct nvgpu_device *dev;
  struct nvgpu_fd *owner; /* referenced: its render handle holds the object */
  u32 host_handle;
  /* SEMSURF_FENCE_WAITs still waiting on a guest-only fence, which the
   * context's end cancels (nvgpu_semsurf_defer). */
  spinlock_t lock;
  struct list_head pending;
};

static void nvgpu_semsurf_cancel_all(struct nvgpu_fence_ctx *ctx);

static void nvgpu_fence_ctx_free(struct drm_gem_object *obj) {
  struct nvgpu_fence_ctx *ctx = container_of(obj, struct nvgpu_fence_ctx, base);

  nvgpu_semsurf_cancel_all(ctx);

  if (ctx->host_handle)
    nvgpu_gem_close(ctx->dev, ctx->owner->handle, ctx->host_handle);
  drm_gem_object_release(obj);
  nvgpu_fd_put(ctx->owner);
  kfree(ctx);
}

static struct dma_buf *nvgpu_fence_ctx_export(struct drm_gem_object *obj,
                                              int flags) {
  return ERR_PTR(-EINVAL);
}

static const struct drm_gem_object_funcs nvgpu_fence_ctx_funcs = {
    .free = nvgpu_fence_ctx_free,
    .export = nvgpu_fence_ctx_export,
};

/* The gem_out hook of SEMSURF_FENCE_CTX_CREATE. On failure the interpreter
 * closes the host handle, so the proxy must not. */
int nvgpu_fence_ctx_create(struct nvgpu_fence_call *p, u32 host_handle,
                           u32 *guest_handle) {
  struct nvgpu_fence_ctx *ctx;
  int ret;

  ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
  if (!ctx)
    return -ENOMEM;
  drm_gem_private_object_init(p->file->minor->dev, &ctx->base, PAGE_SIZE);
  ctx->base.funcs = &nvgpu_fence_ctx_funcs;
  ctx->dev = p->nfd->dev;
  nvgpu_fd_get(p->nfd);
  ctx->owner = p->nfd;
  ctx->host_handle = host_handle;
  spin_lock_init(&ctx->lock);
  INIT_LIST_HEAD(&ctx->pending);
  ret = drm_gem_handle_create(p->file, &ctx->base, guest_handle);
  if (ret)
    ctx->host_handle = 0;
  drm_gem_object_put(&ctx->base);
  return ret;
}

/* Guest handle -> fence-context proxy, referenced; NULL if it is not one
 * (natively -EINVAL, nvidia-drm-fence.c:1490-1497). */
static struct nvgpu_fence_ctx *nvgpu_fence_ctx_lookup(struct drm_file *file,
                                                      u32 handle) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, handle);

  if (!obj)
    return NULL;
  if (obj->funcs != &nvgpu_fence_ctx_funcs) {
    drm_gem_object_put(obj);
    return NULL;
  }
  return container_of(obj, struct nvgpu_fence_ctx, base);
}

/*
 * SEMSURF_FENCE_ATTACH looks both of its handles up in the file it runs in
 * (nvidia-drm-fence.c:1764, 1774-1776), and the buffer is often another
 * file's: a compositor attaches its fence to a client's imported buffer,
 * whose proxy's host object lives in the client's file. So the buffer is
 * moved into the context's file first -- PRIME export from its owner, import
 * into the target, one drm_device so one object and one resv
 * (drm_prime.c:306-331) -- and kept there for as long as the proxy lives:
 * later attaches reuse it, and the proxy's free closes it
 * (nvgpu_fence_gem_free()).
 *
 * Not a temporary, as KMS's re-homing is: PRIME import into a file that
 * already holds the object returns that file's existing handle
 * (drm_prime.c:306-309), and closing it would take it from whoever holds it
 * -- here, another proxy's re-home of the same host object. So each (file,
 * host handle) is counted across proxies and closed with the last (RV:rehome).
 * Each entry holds a reference on the target file, whose render handle the
 * host handle lives in.
 *
 * The same goes for a handle a *proxy* of the target file stands for: the
 * import hands that back too, and it is the proxy's to close (S-11). Such an
 * entry borrows it -- holds a reference on the proxy, so the number stays
 * the object's for as long as the entry uses it, and never closes it. One
 * whose proxy is on its way out (closing it) waits for the close and imports
 * again.
 */
struct nvgpu_rehome {
  struct hlist_node node; /* keyed by the proxy */
  struct nvgpu_gem_object *ng;
  struct nvgpu_fd *file;
  u32 gem; /* in file's host render file */
  struct drm_gem_object *borrowed; /* the proxy of file that owns gem */
};

static DEFINE_MUTEX(nvgpu_rehome_lock);
static DEFINE_HASHTABLE(nvgpu_rehomes, 6);

static int nvgpu_fence_rehome(struct nvgpu_gem_object *ng,
                              struct nvgpu_fd *file, u32 *gem) {
  struct nvgpu_device *dev = file->dev;
  struct nvgpu_rehome *r;
  u64 args[2], res[2];
  u32 dmabuf, h;
  int ret, tries = 0;

again:
  /* Killable: its holder may be in the two host calls below, which each
   * wait up to the transport's timeout. The cleanup (nvgpu_fence_gem_free()),
   * which can run as a killed process exits, takes it plainly: it holds it
   * only for the table. */
  if (mutex_lock_killable(&nvgpu_rehome_lock))
    return -EINTR;
  hash_for_each_possible(nvgpu_rehomes, r, node, (unsigned long)ng) {
    if (r->ng == ng && r->file == file) {
      *gem = r->gem;
      mutex_unlock(&nvgpu_rehome_lock);
      return 0;
    }
  }

  r = kzalloc(sizeof(*r), GFP_KERNEL);
  if (!r) {
    ret = -ENOMEM;
    goto out;
  }
  args[0] = ng->owner_handle;
  args[1] = ng->host_handle;
  ret = nvgpu_host_op(dev, NVGPU_OP_PRIME_EXPORT, args, 2, res, 2);
  if (ret)
    goto out_free;
  if (!nvgpu_res_u32(res[0], &dmabuf)) {
    ret = -EPROTO;
    goto out_free;
  }
  args[0] = file->handle;
  args[1] = dmabuf;
  ret = nvgpu_host_op(dev, NVGPU_OP_DMABUF_IMPORT, args, 2, res, 2);
  nvgpu_close_handle(dev, dmabuf);
  if (ret)
    goto out_free;
  if (!nvgpu_res_u32(res[0], &h)) {
    ret = -EPROTO;
    goto out_free;
  }
  r->borrowed = nvgpu_gem_proxy_find(file, h);
  if (!r->borrowed && nvgpu_gem_dying(file, h)) {
    /* Not ours to use or close: wait out the proxy's close, outside the
     * lock its free takes (nvgpu_fence_gem_free()), and import again. */
    kfree(r);
    mutex_unlock(&nvgpu_rehome_lock);
    if (++tries > 3)
      return -EAGAIN;
    ret = nvgpu_gem_wait_gone(file, h);
    if (ret)
      return ret;
    goto again;
  }
  r->ng = ng;
  nvgpu_fd_get(file);
  r->file = file;
  r->gem = h;
  hash_add(nvgpu_rehomes, &r->node, (unsigned long)ng);
  /* Under the lock; read by the proxy's free, which can only come after
   * the reference this caller holds on it is gone. */
  WRITE_ONCE(ng->rehomed, true);
  *gem = r->gem;
  ret = 0;
  goto out;

out_free:
  kfree(r);
out:
  mutex_unlock(&nvgpu_rehome_lock);
  if (ret)
    dev_dbg_ratelimited(&dev->vdev->dev,
                        "virtio-gpu-nv: cannot move object %u of file %u "
                        "into file %u for SEMSURF_FENCE_ATTACH: %d\n",
                        ng->host_handle, ng->owner_handle, file->handle, ret);
  return ret;
}

void nvgpu_fence_gem_free(struct nvgpu_gem_object *ng) {
  struct nvgpu_rehome *r, *o;
  struct hlist_node *tmp;
  HLIST_HEAD(gone);
  unsigned int bkt;

  /*
   * Most proxies were never attached a fence in another file: they need not
   * wait on the one global mutex, which a re-home holds across two or three
   * HOST_OPs.
   */
  if (!READ_ONCE(ng->rehomed))
    return;
  mutex_lock(&nvgpu_rehome_lock);
  hash_for_each_possible_safe(nvgpu_rehomes, r, tmp, node, (unsigned long)ng) {
    bool shared = false;

    if (r->ng != ng)
      continue;
    hash_del(&r->node);
    /* Another proxy's entry for the same host handle keeps it open. */
    hash_for_each(nvgpu_rehomes, bkt, o, node)
      if (o->file == r->file && o->gem == r->gem)
        shared = true;
    if (shared || r->borrowed)
      r->gem = 0;
    hlist_add_head(&r->node, &gone);
  }
  mutex_unlock(&nvgpu_rehome_lock);

  hlist_for_each_entry_safe(r, tmp, &gone, node) {
    if (r->gem)
      nvgpu_gem_close(r->file->dev, r->file->handle, r->gem);
    /* Outside the lock: this may be that proxy's last reference, and its
     * free comes back here. */
    if (r->borrowed)
      drm_gem_object_put(r->borrowed);
    nvgpu_fd_put(r->file);
    kfree(r);
  }
}

static void nvgpu_semsurf_mirror(struct nvgpu_fence_ctx *ctx,
                                 struct nvgpu_gem_object *ng,
                                 const struct nvgpu_semsurf_attach *a);

static long nvgpu_semsurf_attach(struct nvgpu_fence_call *p, unsigned int cmd,
                                 void *karg) {
  struct nvgpu_semsurf_attach a;
  struct nvgpu_fence_ctx *ctx;
  struct nvgpu_gem_object *ng;
  struct nvgpu_fd *target;
  u32 gem;
  long ret = 0;

  memcpy(&a, karg, sizeof(a));
  ctx = nvgpu_fence_ctx_lookup(p->file, a.fence_context_handle);
  if (!ctx)
    return -EINVAL;
  ng = nvgpu_gem_lookup(p->file, a.handle);
  if (!ng) {
    drm_gem_object_put(&ctx->base);
    return -EINVAL;
  }
  target = ctx->owner;
  if (ng->owner == target)
    gem = ng->host_handle;
  else
    ret = nvgpu_fence_rehome(ng, target, &gem);
  if (!ret) {
    p->ngem = 2;
    p->gem[0].off = offsetof(struct nvgpu_semsurf_attach, handle);
    p->gem[0].owner = target->handle;
    p->gem[0].gem = gem;
    p->gem[1].off = offsetof(struct nvgpu_semsurf_attach, fence_context_handle);
    p->gem[1].owner = target->handle;
    p->gem[1].gem = ctx->host_handle;
    ret = nvgpu_fence_call(p, target->handle, cmd, &a, true);
  }
  if (!ret && (p->nfd->dev->backend_caps & NVGPU_BCAP_KMS_CARD))
    nvgpu_semsurf_mirror(ctx, ng, &a);
  drm_gem_object_put(&ng->base);
  drm_gem_object_put(&ctx->base);
  return ret;
}

/* A call on a fence context's file (which is the caller's: a context cannot
 * leave the file it was made in, since nothing exports it), the context's
 * handle in the argument at offset 0 in every struct that has one. */
static long nvgpu_semsurf_ctx_call(struct nvgpu_fence_call *p,
                                   unsigned int cmd, void *karg,
                                   struct nvgpu_fence_ctx *ctx) {
  p->ngem = 1;
  p->gem[0].off = 0; /* fence_context_handle @0 in both */
  p->gem[0].owner = ctx->owner->handle;
  p->gem[0].gem = ctx->host_handle;
  return nvgpu_fence_call(p, ctx->owner->handle, cmd, karg, true);
}

/* FENCE_CREATE: the fence context, then the call on its file. */
static long nvgpu_semsurf_on_ctx(struct nvgpu_fence_call *p, unsigned int cmd,
                                 void *karg, u32 ctx_handle) {
  struct nvgpu_fence_ctx *ctx = nvgpu_fence_ctx_lookup(p->file, ctx_handle);
  long ret;

  if (!ctx) {
    if (p->has_in && (p->in_flags & NVGPU_I2_FD_CONSUME))
      nvgpu_close_handle(p->nfd->dev, p->in_handle);
    return -EINVAL;
  }
  ret = nvgpu_semsurf_ctx_call(p, cmd, karg, ctx);
  drm_gem_object_put(&ctx->base);
  return ret;
}

/*
 * FENCE_WAIT with nothing left to wait for. The fence field cannot be empty
 * (the host looks the descriptor up, nvidia-drm-fence.c:1692), so an
 * already-signalled host sync_file stands in for it: the host's
 * pre-wait-then-post ordering holds unchanged.
 */
static long nvgpu_semsurf_wait_signalled(struct nvgpu_fence_ctx *ctx,
                                         unsigned int cmd,
                                         struct nvgpu_semsurf_wait *a) {
  struct nvgpu_fence_call p = {.nfd = ctx->owner};
  u64 res[1];
  long ret;

  ret = nvgpu_host_op(ctx->dev, NVGPU_OP_SIGNALED_SYNC_FILE, NULL, 0, res, 1);
  if (ret)
    return ret;
  if (!nvgpu_res_u32(res[0], &p.in_handle))
    return -EPROTO;
  p.has_in = true;
  p.in_flags = NVGPU_I2_FD_CONSUME;
  return nvgpu_semsurf_ctx_call(&p, cmd, a, ctx);
}

/*
 * FENCE_WAIT on a fence only the guest can see -- another guest driver's,
 * sw_sync, a CPU fence -- without blocking the caller, as the host does it
 * natively: a callback on the fence, and the second half (the post write)
 * when it fires (nvidia-drm-fence.c:1682-1729). Here the second half is the
 * forward itself, with a signalled stand-in fence, run from a work item
 * because it is a round trip. The ioctl returns 0 at once, as native does
 * whatever happens later; a failed forward is logged.
 *
 * The wait hangs off its context and does not keep it alive: natively a
 * context's end drops the waits pending on it (nv_drm_semsurf_fence_ctx
 * free), and so does this one's (nvgpu_semsurf_cancel_all). The work item
 * checks the context is still alive, under its lock, before touching
 * anything; the cancel either takes the callback back or waits the work
 * item out. A 0x55 fence was not used as the stand-in because it
 * force-signals after its timeout (5 s at most), which a guest-only fence
 * need not respect.
 */
struct nvgpu_semsurf_defer {
  struct dma_fence_cb cb;
  struct work_struct work;
  struct list_head node;       /* ctx->pending, under ctx->lock */
  struct nvgpu_fence_ctx *ctx; /* not referenced: its end cancels this */
  struct dma_fence *fence;     /* referenced */
  unsigned int cmd;
  struct nvgpu_semsurf_wait a;
};

static void nvgpu_semsurf_defer_work(struct work_struct *work) {
  struct nvgpu_semsurf_defer *d =
      container_of(work, struct nvgpu_semsurf_defer, work);
  struct nvgpu_fence_ctx *ctx = d->ctx;
  unsigned long flags;
  bool live;
  long ret;

  spin_lock_irqsave(&ctx->lock, flags);
  live = kref_get_unless_zero(&ctx->base.refcount);
  if (live)
    list_del(&d->node);
  spin_unlock_irqrestore(&ctx->lock, flags);
  if (!live)
    return; /* the context is ending: its cancel frees this */

  ret = nvgpu_semsurf_wait_signalled(ctx, d->cmd, &d->a);
  if (ret)
    dev_dbg_ratelimited(&ctx->dev->vdev->dev,
                        "virtio-gpu-nv: SEMSURF_FENCE_WAIT after a guest "
                        "fence signalled: the host refused it (%ld)\n",
                        ret);
  drm_gem_object_put(&ctx->base);
  dma_fence_put(d->fence);
  kfree(d);
}

static void nvgpu_semsurf_defer_cb(struct dma_fence *f,
                                   struct dma_fence_cb *cb) {
  struct nvgpu_semsurf_defer *d =
      container_of(cb, struct nvgpu_semsurf_defer, cb);

  queue_work(nvgpu_long_wq, &d->work);
}

/* Takes its own reference on `f`. */
static long nvgpu_semsurf_defer(struct nvgpu_fence_ctx *ctx, unsigned int cmd,
                                const struct nvgpu_semsurf_wait *a,
                                struct dma_fence *f) {
  struct nvgpu_semsurf_defer *d = kzalloc(sizeof(*d), GFP_KERNEL);
  unsigned long flags;
  long ret;

  if (!d)
    return -ENOMEM;
  INIT_WORK(&d->work, nvgpu_semsurf_defer_work);
  d->ctx = ctx;
  d->fence = dma_fence_get(f);
  d->cmd = cmd;
  d->a = *a;
  spin_lock_irqsave(&ctx->lock, flags);
  list_add_tail(&d->node, &ctx->pending);
  spin_unlock_irqrestore(&ctx->lock, flags);
  if (!dma_fence_add_callback(f, &d->cb, nvgpu_semsurf_defer_cb))
    return 0;

  /* Signalled meanwhile (or, as the host treats any failure here, as good
   * as): the second half now, as native does (nvidia-drm-fence.c:1717-1729). */
  spin_lock_irqsave(&ctx->lock, flags);
  list_del(&d->node);
  spin_unlock_irqrestore(&ctx->lock, flags);
  ret = nvgpu_semsurf_wait_signalled(ctx, cmd, &d->a);
  dma_fence_put(d->fence);
  kfree(d);
  return ret;
}

/* The context is ending (its last reference; process context): every wait
 * still pending on it is dropped, as natively. */
static void nvgpu_semsurf_cancel_all(struct nvgpu_fence_ctx *ctx) {
  struct nvgpu_semsurf_defer *d, *n;
  unsigned long flags;
  LIST_HEAD(gone);

  spin_lock_irqsave(&ctx->lock, flags);
  list_splice_init(&ctx->pending, &gone);
  spin_unlock_irqrestore(&ctx->lock, flags);
  list_for_each_entry_safe(d, n, &gone, node) {
    /* Fired already: its work item is queued or running, and finds the
     * context dead (refcount zero) without touching the list. */
    if (!dma_fence_remove_callback(d->fence, &d->cb))
      cancel_work_sync(&d->work);
    dma_fence_put(d->fence);
    kfree(d);
  }
}

/*
 * SEMSURF_FENCE_ATTACH, mirrored into the guest buffer's reservation object
 * in compositor-VM mode. The host attaches its fence to the host object's
 * resv (nvidia-drm-fence.c:1764-1813), which host scanout sees; a guest
 * compositor's implicit sync reads the guest proxy's resv (EXPORT_SYNC_FILE,
 * poll on the dma-buf), which stayed empty. A host SEMSURF_FENCE_CREATE on
 * the same context, value and timeout makes a fence that signals when the
 * attached one does; its proxy goes into the guest resv, shared or
 * exclusive as the attach said (READ / WRITE, nvidia-dma-resv-helper.h). One
 * more round trip per attach, so only where a guest compositor reads the
 * resv. A failure leaves the attach done and the resv as it was.
 */
static void nvgpu_semsurf_mirror(struct nvgpu_fence_ctx *ctx,
                                 struct nvgpu_gem_object *ng,
                                 const struct nvgpu_semsurf_attach *a) {
  struct nvgpu_semsurf_create c = {
      .timeout_value_ms = a->timeout_value_ms,
      .wait_value = a->wait_value,
      .fd = -1,
  };
  struct nvgpu_fence_call p = {
      .nfd = ctx->owner,
      .want_kind = NVGPU_HK_SYNC_FILE,
      .want_fence = true,
  };
  struct dma_resv *resv = ng->base.resv;
  long ret;

  ret = nvgpu_semsurf_ctx_call(&p, NVGPU_IOCTL_SEMSURF_CREATE, &c, ctx);
  if (!ret && !p.fence)
    ret = -EPROTO;
  if (!ret) {
    ret = dma_resv_lock_interruptible(resv, NULL);
    if (!ret) {
      ret = dma_resv_reserve_fences(resv, 1);
      if (!ret)
        dma_resv_add_fence(resv, p.fence,
                           a->shared ? DMA_RESV_USAGE_READ
                                     : DMA_RESV_USAGE_WRITE);
      dma_resv_unlock(resv);
    }
  }
  if (p.fence)
    dma_fence_put(p.fence);
  if (ret)
    dev_dbg_ratelimited(&ctx->dev->vdev->dev,
                        "virtio-gpu-nv: SEMSURF_FENCE_ATTACH done, but not "
                        "mirrored into the guest buffer's resv: %ld\n",
                        ret);
}

unsigned int nvgpu_fence_semsurf_cmd(unsigned int nr) {
  switch (nr) {
  case _IOC_NR(NVGPU_IOCTL_SEMSURF_CTX_CREATE):
    return NVGPU_IOCTL_SEMSURF_CTX_CREATE;
  case _IOC_NR(NVGPU_IOCTL_SEMSURF_CREATE):
    return NVGPU_IOCTL_SEMSURF_CREATE;
  case _IOC_NR(NVGPU_IOCTL_SEMSURF_WAIT):
    return NVGPU_IOCTL_SEMSURF_WAIT;
  case _IOC_NR(NVGPU_IOCTL_SEMSURF_ATTACH):
    return NVGPU_IOCTL_SEMSURF_ATTACH;
  }
  return 0;
}

long nvgpu_fence_semsurf_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void *karg) {
  struct nvgpu_fence_call p = {.nfd = nfd, .file = file};
  long ret;

  nvgpu_fence_reap();
  switch (cmd) {
  case NVGPU_IOCTL_SEMSURF_CTX_CREATE:
    /*
     * RM handles of the caller's own client and a size, no descriptor
     * (nvkms-kapi-sync.c:182-306): forwarded, the NVKMS block it points at
     * read from the caller's memory; the new context comes back as a GEM
     * handle and gets its proxy (gem_out).
     */
    return nvgpu_fence_call(&p, nfd->handle, cmd, karg, false);

  case NVGPU_IOCTL_SEMSURF_CREATE: {
    struct nvgpu_semsurf_create *a = karg;

    p.want_kind = NVGPU_HK_SYNC_FILE;
    return nvgpu_semsurf_on_ctx(&p, cmd, a, a->fence_context_handle);
  }

  case NVGPU_IOCTL_SEMSURF_WAIT: {
    struct nvgpu_semsurf_wait a;
    struct nvgpu_fence_ctx *ctx;
    struct dma_fence *f;
    bool owned;

    memcpy(&a, karg, sizeof(a));
    ctx = nvgpu_fence_ctx_lookup(file, a.fence_context_handle);
    if (!ctx)
      return -EINVAL;
    f = sync_file_get_fence(a.fd);
    if (!f) {
      drm_gem_object_put(&ctx->base);
      return -EINVAL; /* as the kernel says for a descriptor that is not one */
    }
    /*
     * The fence to wait on, as the host sees it: a host sync_file (ours, or
     * a host merge of ours), nothing at all (signalled already), or one only
     * the guest can see, which is waited for without blocking the caller.
     */
    ret = nvgpu_fence_unwrap_ex(nfd->dev, f, &p.in_handle, &owned, false);
    if (ret == 0) {
      p.has_in = true;
      p.in_flags = owned ? NVGPU_I2_FD_CONSUME : 0;
      /* A proxy's own handle: kept open with the request, as above. */
      if (!owned)
        p.in_ref = dma_fence_get(f);
      ret = nvgpu_semsurf_ctx_call(&p, cmd, &a, ctx);
    } else if (ret == 1) {
      ret = nvgpu_semsurf_wait_signalled(ctx, cmd, &a);
    } else if (ret == NVGPU_UNWRAP_FOREIGN) {
      ret = nvgpu_semsurf_defer(ctx, cmd, &a, f);
    }
    dma_fence_put(f);
    drm_gem_object_put(&ctx->base);
    return ret;
  }

  case NVGPU_IOCTL_SEMSURF_ATTACH:
    return nvgpu_semsurf_attach(&p, cmd, karg);

  default:
    return -ENOTTY;
  }
}
