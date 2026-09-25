// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: fences (DESIGN §6, R:fences Design A).
 *
 * Every fence object a guest process holds is the host's. A semaphore-surface
 * fence is signalled by the host's RM from a GPU interrupt, a KMS out-fence by
 * the host's flip, a compositor's release fence by the host compositor -- and
 * the consumers that matter are on the host too (the compositor's GPU wait,
 * host scanout). So the host keeps the objects, and this side keeps proxies:
 *
 *  - a host sync_file is a guest sync_file around an nvgpu host fence, a
 *    dma_fence that signals (with the host's error, if any) when the host's
 *    does. It has to be a real sync_file: every guest-kernel consumer takes
 *    one through sync_file_get_fence(), and NVIDIA's userspace runs
 *    SYNC_IOC_MERGE and FILE_INFO on them (R:fences §2.1), which then need no
 *    round trip at all;
 *  - a host syncobj handle is the same number in the guest: each guest DRM
 *    file stands for exactly one host render-node file, and syncobj handles
 *    are per file, so there is nothing to translate. Syncobj *files* are
 *    host-handle files (nvgpu_hostfile.c);
 *  - a semaphore-surface fence context is a GEM object in the host file; the
 *    guest gets a proxy object of its own kind, which nothing can export or
 *    map (nvidia-drm-fence.c:439-456 makes it a GEM object; its only use is
 *    by handle, in the three ioctls that follow).
 *
 * Handing a guest fence back to a host consumer is "unwrap": our own proxy
 * gives its handle; a merge of our proxies becomes a host merge; anything the
 * host cannot see is waited for here -- by a callback for SEMSURF_FENCE_WAIT,
 * which returns at once as natively (nvgpu_semsurf_defer), and by sleeping
 * for syncobj IMPORT_SYNC_FILE and a committing IN_FENCE_FD.
 *
 * Nothing may wait on the host. A syncobj wait is forwarded as a poll (the
 * backend clamps its timeout to zero, device/src/fence.rs), and the sleeping
 * is done here, on a shared host SYNCOBJ_EVENTFD registration the backend
 * dedups and caps (HOST_OP SYNCOBJ_WATCH, RV:eventfd): one EV_READY wakes
 * every guest waiter on that (file, syncobj, point), and each re-polls. Over
 * the cap a waiter polls with a short backoff instead.
 *
 * Contexts and locks. Event delivery runs in hard IRQ under the event
 * registry's lock (nvgpu_xfer.c), and a dma_fence's last reference can be
 * dropped anywhere -- inside that very delivery, when signalling runs a
 * callback that puts a fence array holding our proxy. So nothing that frees a
 * proxy may touch the registry: a consumer that has to go is put on a
 * graveyard list (IRQ-safe) and unregistered later from process context, by
 * the next fence operation (nvgpu_fence_reap()). Order, outermost first:
 * the registry lock (nvgpu_events.lock) -> nvgpu_sowait_lock ->
 * nvgpu_fence_dead_lock / the proxy xarray's lock; none of ours is ever held
 * across nvgpu_ev_register/unregister.
 */

#include <drm/drm.h>
#include <drm/drm_device.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_utils.h>
#include <linux/dma-fence-unwrap.h>
#include <linux/dma-fence.h>
#include <linux/dma-resv.h>
#include <linux/err.h>
#include <linux/eventfd.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/hashtable.h>
#include <linux/hrtimer.h>
#include <linux/jiffies.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/refcount.h>
#include <linux/sched/signal.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/sync_file.h>
#include <linux/uaccess.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>

#include "nvgpu.h"

bool nvgpu_fences_enabled(struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_FENCES);
}

/* ───────── event consumers that outlive what they report on ───────── */

/*
 * A registry consumer owned by something that can die in any context. It is
 * unregistered only from process context: whatever frees its owner buries it
 * here instead, and nvgpu_fence_reap() finishes the job. A buried consumer
 * may still be called; its deliver() finds nothing and returns.
 */
struct nvgpu_fence_ev {
  struct nvgpu_ev_consumer c;
  struct nvgpu_device *dev;
  bool registered;
  u32 id; /* host fences: the index in nvgpu_host_fences */
  struct list_head dead;
  void (*free)(struct nvgpu_fence_ev *e);
};

static DEFINE_SPINLOCK(nvgpu_fence_dead_lock);
static LIST_HEAD(nvgpu_fence_dead);

static void nvgpu_fence_bury(struct nvgpu_fence_ev *e) {
  unsigned long flags;

  spin_lock_irqsave(&nvgpu_fence_dead_lock, flags);
  list_add_tail(&e->dead, &nvgpu_fence_dead);
  spin_unlock_irqrestore(&nvgpu_fence_dead_lock, flags);
}

/*
 * Process context only. A registered consumer holds its device (taken with
 * the registration): a buried one may be retired long after remove(), by
 * whoever reaps next, and a syncobj wait's can outlive every file (S-26).
 */
static void nvgpu_fence_ev_retire(struct nvgpu_fence_ev *e) {
  struct nvgpu_device *dev = e->dev;
  bool registered = e->registered;

  if (registered)
    nvgpu_ev_unregister(dev, &e->c);
  e->free(e);
  if (registered)
    nvgpu_dev_put(dev);
}

static void nvgpu_fence_reap(void) {
  struct nvgpu_fence_ev *e, *n;
  unsigned long flags;
  LIST_HEAD(dead);

  spin_lock_irqsave(&nvgpu_fence_dead_lock, flags);
  list_splice_init(&nvgpu_fence_dead, &dead);
  spin_unlock_irqrestore(&nvgpu_fence_dead_lock, flags);
  list_for_each_entry_safe(e, n, &dead, dead) {
    list_del(&e->dead);
    nvgpu_fence_ev_retire(e);
  }
}

static void nvgpu_fence_ev_kfree(struct nvgpu_fence_ev *e) { kfree(e); }

/* ───────── host fence proxies ───────── */

struct nvgpu_host_fence {
  struct dma_fence base;
  struct nvgpu_device *dev;
  /* The backend's sync_file. Open for as long as the proxy lives, signalled
   * or not: a host consumer handed this fence later still needs it. */
  u32 handle;
  struct nvgpu_fence_ev *ev;
  atomic_t signalled; /* the one EV_FENCE has been acted on */
};

/*
 * id -> proxy, for the event path. Looked up under RCU from hard IRQ; the
 * proxy's memory is freed with kfree_rcu (dma_fence_free()), and a reader
 * that finds one already at refcount zero is refused by dma_fence_get_rcu()
 * (RV:fencexa).
 */
static DEFINE_XARRAY_FLAGS(nvgpu_host_fences,
                           XA_FLAGS_ALLOC1 | XA_FLAGS_LOCK_IRQ);

static const char *nvgpu_host_fence_driver(struct dma_fence *f) {
  return "nvgpu";
}

static const char *nvgpu_host_fence_timeline(struct dma_fence *f) {
  return "host";
}

/*
 * The last reference is gone -- from anywhere, a hard IRQ included (our own
 * delivery below drops one). So: out of the xarray with the irqsave form
 * (xa_erase_irq() would turn interrupts back on inside an interrupt), the
 * consumer to the graveyard, the host's sync_file closed by a queued CLOSE
 * (which holds a module reference while queued), and the memory freed after
 * an RCU grace period. dma_fence_release() calls this under rcu_read_lock
 * (dma-fence.c:577-614), which also keeps the module from being freed under
 * the code below after its module_put().
 *
 * Having a .release at all keeps fence->ops set after signalling
 * (dma-fence.c:371-375), which is how unwrap still recognises our fences.
 */
static void nvgpu_host_fence_release(struct dma_fence *base) {
  struct nvgpu_host_fence *f = container_of(base, struct nvgpu_host_fence, base);
  unsigned long flags;

  xa_lock_irqsave(&nvgpu_host_fences, flags);
  __xa_erase(&nvgpu_host_fences, f->ev->id);
  xa_unlock_irqrestore(&nvgpu_host_fences, flags);
  /* Before the burial: the consumer's device reference is what keeps
   * f->dev alive, and a reap on another CPU may retire it at once. */
  nvgpu_close_handle_async(f->dev, f->handle);
  nvgpu_fence_bury(f->ev);
  dma_fence_free(base);
  module_put(THIS_MODULE);
}

static const struct dma_fence_ops nvgpu_host_fence_ops = {
    .get_driver_name = nvgpu_host_fence_driver,
    .get_timeline_name = nvgpu_host_fence_timeline,
    .release = nvgpu_host_fence_release,
};

/*
 * EV_FENCE for one proxy: the host's sync_file signalled, with this status
 * (SYNC_IOC_FILE_INFO: 1, or the fence's error -- a semaphore-surface fence
 * that timed out is -ETIMEDOUT, nvidia-drm-fence.c:890-898). Hard IRQ.
 */
static void nvgpu_host_fence_deliver(struct nvgpu_ev_consumer *c, u32 kind,
                                     u64 cookie, const void *payload,
                                     u32 len) {
  struct nvgpu_fence_ev *e = container_of(c, struct nvgpu_fence_ev, c);
  struct nvgpu_ev_fence ev = {};
  struct nvgpu_host_fence *f;
  s32 status;

  if (kind != NVGPU_EV_FENCE)
    return;
  memcpy(&ev, payload, min_t(u32, len, sizeof(ev)));
  status = (s32)le32_to_cpu(ev.status);

  rcu_read_lock();
  f = xa_load(&nvgpu_host_fences, e->id);
  if (f && !dma_fence_get_rcu(&f->base))
    f = NULL;
  rcu_read_unlock();
  if (!f)
    return; /* the proxy went first: nobody left to tell */
  /*
   * The id is the xarray's lowest free one (XA_FLAGS_ALLOC1), so the moment
   * a proxy is released its id can go to the next one -- while this
   * consumer, buried but not yet reaped, can still be handed the old
   * fence's one EV_FENCE (its CLOSE is queued behind other work). Only the
   * proxy this consumer belongs to is ours to signal: signalling the new
   * one would let its waiters -- a flip's IN_FENCE_FD, a sampler -- run
   * ahead of GPU work that has not finished (S-12).
   */
  if (READ_ONCE(f->ev) != e) {
    dma_fence_put(&f->base);
    return;
  }

  if (!atomic_xchg(&f->signalled, 1)) {
    if (status < 0 && status >= -MAX_ERRNO)
      dma_fence_set_error(&f->base, status);
    else if (status != 1)
      dev_warn_ratelimited(&e->dev->vdev->dev,
                           "virtio-gpu-nv: host fence %u reported status %d, "
                           "which is neither signalled nor an error; "
                           "signalling it anyway\n",
                           f->handle, status);
    /*
     * With the host's own signal time where the backend sent one: the ICD
     * reads a fence's timestamp back through FILE_INFO (glcore 0xa13870, the
     * sync-fd ops' slot 0x30), and the time this record arrived is the
     * host's plus the trip here, about a third of a millisecond. Moved into
     * the guest's clock, and never later than now -- the slewed offset could
     * otherwise put it a hair in the future, and a fence that signals after
     * it was seen signalled is a contradiction.
     */
    if (ev.timestamp_ns) {
      s64 at = nvgpu_host_to_guest_ns(e->dev,
                                      (s64)le64_to_cpu(ev.timestamp_ns));
      s64 now = ktime_get_ns();

      dma_fence_signal_timestamp(&f->base, ns_to_ktime(min(at, now)));
    } else {
      dma_fence_signal(&f->base);
    }
  }
  dma_fence_put(&f->base);
}

/*
 * A proxy dma_fence for host sync_file `handle`, with one reference, which
 * signals when the host's does. On failure `handle` is closed only if `own`;
 * an IOCTL2 fd_out hook passes false, because the interpreter closes what a
 * failing hook was given.
 */
static struct dma_fence *nvgpu_host_fence_new(struct nvgpu_device *dev,
                                              u32 handle, bool own) {
  struct nvgpu_host_fence *f;
  struct nvgpu_fence_ev *e;
  u64 cookie;
  int ret;

  nvgpu_fence_reap();
  if (!handle)
    return ERR_PTR(-EINVAL);
  f = kzalloc(sizeof(*f), GFP_KERNEL);
  e = kzalloc(sizeof(*e), GFP_KERNEL);
  if (!f || !e) {
    ret = -ENOMEM;
    goto fail_free;
  }
  /*
   * Reserved, not published: xa_load() finds NULL here until the proxy is
   * whole, so an event racing in (for a stale consumer that held this id
   * before) never sees a fence whose ev and refcount are not yet set.
   */
  ret = xa_alloc_irq(&nvgpu_host_fences, &e->id, NULL, xa_limit_32b,
                     GFP_KERNEL);
  if (ret)
    goto fail_free;
  e->dev = dev;
  e->free = nvgpu_fence_ev_kfree;
  e->c.deliver = nvgpu_host_fence_deliver;
  f->dev = dev;
  f->handle = handle;
  f->ev = e;
  /* Our ops are in every proxy; the module stays while one exists. */
  __module_get(THIS_MODULE);
  /*
   * A context of its own: two unrelated host fences must never be taken for
   * two points on one timeline, which dma_fence_unwrap_merge() and
   * dma_fence_is_later() would otherwise do (dma-fence.c:182).
   */
  dma_fence_init64(&f->base, &nvgpu_host_fence_ops, NULL,
                   dma_fence_context_alloc(1), 1);
  /* From here the fence's release owns the id, the consumer and the handle. */

  /* Published whole: the store is an rcu_assign_pointer. */
  ret = xa_err(xa_store_irq(&nvgpu_host_fences, e->id, f, GFP_KERNEL));
  if (ret)
    goto put;

  /*
   * Consumer first, WATCH second: once the backend watches, the report can
   * come at once (a host fence that has already signalled), and it must find
   * the consumer and the proxy in place.
   */
  cookie = nvgpu_ev_new_cookie(dev);
  ret = nvgpu_ev_register(dev, &e->c, NVGPU_EVKEY_COOKIE(cookie));
  if (ret)
    goto put;
  nvgpu_dev_get(dev); /* the registration's, put at retire */
  e->registered = true;
  ret = nvgpu_watch(dev, handle, NVGPU_W_FENCE | NVGPU_W_ONESHOT, cookie);
  if (ret) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: the backend would not watch host "
                         "fence %u: %d\n",
                         handle, ret);
    goto put;
  }
  return &f->base;

put:
  if (!own)
    f->handle = 0; /* the caller's to close; release closes nothing */
  dma_fence_put(&f->base);
  return ERR_PTR(ret);

fail_free:
  kfree(f);
  kfree(e);
  if (own)
    nvgpu_close_handle(dev, handle);
  return ERR_PTR(ret);
}

/* A guest sync_file for host sync_file `handle`; `own` as above. */
static int nvgpu_host_fence_fd(struct nvgpu_device *dev, u32 handle,
                               int o_flags, bool own) {
  struct dma_fence *base = nvgpu_host_fence_new(dev, handle, own);
  struct sync_file *sync;
  int fd, ret;

  if (IS_ERR(base))
    return PTR_ERR(base);
  sync = sync_file_create(base);
  if (!sync) {
    ret = -ENOMEM;
    goto put;
  }
  fd = get_unused_fd_flags(o_flags & O_CLOEXEC);
  if (fd < 0) {
    ret = fd;
    /* Before the fput: the sync_file's release may drop the last
     * reference first, and must then close nothing that is not ours. */
    if (!own)
      container_of(base, struct nvgpu_host_fence, base)->handle = 0;
    fput(sync->file);
    goto put;
  }
  fd_install(fd, sync->file);
  dma_fence_put(base); /* the sync_file holds its own */
  return fd;

put:
  if (!own)
    container_of(base, struct nvgpu_host_fence, base)->handle = 0;
  dma_fence_put(base);
  return ret;
}

int nvgpu_fence_from_handle(struct nvgpu_device *dev, u32 handle,
                            int o_flags) {
  return nvgpu_host_fence_fd(dev, handle, o_flags, true);
}

int nvgpu_fence_from_handle_noclose(struct nvgpu_device *dev, u32 handle,
                                    int o_flags) {
  return nvgpu_host_fence_fd(dev, handle, o_flags, false);
}

/* ───────── unwrap: a guest fence, for a host consumer ───────── */

/* nvgpu_fence_unwrap_ex()'s answer for a fence the host has no counterpart of,
 * not signalled yet, when the caller asked not to wait for it. */
#define NVGPU_UNWRAP_FOREIGN 2

static struct nvgpu_host_fence *nvgpu_host_fence_of(struct nvgpu_device *dev,
                                                    struct dma_fence *f) {
  struct nvgpu_host_fence *hf;

  if (rcu_access_pointer(f->ops) != &nvgpu_host_fence_ops)
    return NULL;
  hf = container_of(f, struct nvgpu_host_fence, base);
  return hf->dev == dev ? hf : NULL;
}

/*
 * One host sync_file for `n` of ours: HOST_OP SYNC_MERGE, five at a time,
 * folding the running result into the next merge and closing each
 * intermediate one. The result is a new handle, the caller's to close.
 */
static int nvgpu_fence_merge(struct nvgpu_device *dev, const u32 *hs, u32 n,
                             u32 *out) {
  u64 args[NVGPU_OP_MAX_ARGS], res[1];
  u32 acc = 0, i = 0;
  int ret;

  while (i < n) {
    u32 k = 0;

    if (acc)
      args[1 + k++] = acc;
    while (k < NVGPU_OP_MAX_ARGS - 1 && i < n)
      args[1 + k++] = hs[i++];
    args[0] = k;
    ret = nvgpu_host_op(dev, NVGPU_OP_SYNC_MERGE, args, k + 1, res, 1);
    if (acc)
      nvgpu_close_handle(dev, acc);
    if (ret)
      return ret;
    if (!res[0] || res[0] > U32_MAX)
      return -EPROTO;
    acc = (u32)res[0];
  }
  *out = acc;
  return 0;
}

/*
 * A guest fence as one host sync_file: 0 with its handle (*owned when it is a
 * new one, the caller's to consume), 1 when there is nothing left to wait
 * for, <0 on error. A fence only the guest can see is waited for here when
 * `wait`, else NVGPU_UNWRAP_FOREIGN is returned and the caller waits its own
 * way (0x56 without blocking, nvgpu_semsurf_defer).
 */
static int nvgpu_fence_unwrap_ex(struct nvgpu_device *dev, struct dma_fence *f,
                                 u32 *handle, bool *owned, bool wait) {
  struct nvgpu_host_fence *hf = nvgpu_host_fence_of(dev, f);
  long waited;

  *owned = false;
  if (hf) {
    *handle = hf->handle;
    return 0;
  }

  /*
   * NVIDIA's userspace merges its own render fences with SYNC_IOC_MERGE
   * before handing one on (R:fences §2.1), so a dma_fence_array of our
   * proxies is the common case, not a curiosity. Its unsignalled components,
   * if all ours, merge on the host into one fence the host consumer can
   * take; the array itself exists only here. However many there are: the
   * merge runs five at a time (nvgpu_fence_merge), so a large array costs
   * round trips, where a cap on it used to make the caller wait here for a
   * fence the host could have waited on.
   */
  if (dma_fence_is_array(f) || dma_fence_is_chain(f)) {
    struct dma_fence_unwrap cur;
    struct dma_fence *c;
    bool foreign = false;
    u32 *hs, n = 0, max = 0, i;
    int ret;

    /* Counted first, then collected: the components are fixed when the
     * array or chain is made, and one that signals between the two walks
     * only makes the second find fewer. Each walk runs to its end even once
     * the answer is known: the cursor holds a reference only the end drops. */
    dma_fence_unwrap_for_each(c, &cur, f) {
      if (foreign || dma_fence_is_signaled(c))
        continue;
      if (!nvgpu_host_fence_of(dev, c))
        foreign = true;
      else
        max++;
    }
    if (!foreign) {
      hs = kvmalloc_array(max ? max : 1, sizeof(*hs), GFP_KERNEL);
      if (!hs)
        return -ENOMEM;
      dma_fence_unwrap_for_each(c, &cur, f) {
        struct nvgpu_host_fence *hc;

        if (n == max || dma_fence_is_signaled(c))
          continue;
        hc = nvgpu_host_fence_of(dev, c);
        if (!hc)
          continue;
        for (i = 0; i < n && hs[i] != hc->handle; i++)
          ;
        if (i == n)
          hs[n++] = hc->handle;
      }
      if (n == 0) {
        ret = 1;
      } else if (n == 1) {
        *handle = hs[0];
        ret = 0;
      } else {
        ret = nvgpu_fence_merge(dev, hs, n, handle);
        *owned = !ret;
      }
      kvfree(hs);
      return ret;
    }
  }

  if (dma_fence_is_signaled(f))
    return 1;
  if (!wait)
    return NVGPU_UNWRAP_FOREIGN;
  /*
   * A fence the host has no counterpart of: another guest driver's, sw_sync,
   * a guest-CPU fence. The host cannot wait on it, so the guest does, here,
   * before the host is told there is nothing to wait for (R:fences §3.7,
   * step 4). Interruptible and unbounded, like the native waits on such a
   * fence; its error, if it has one, is not carried over. Native never
   * blocks here -- the host kernel takes a callback -- which is what 0x56
   * now does too; syncobj IMPORT_SYNC_FILE and a committing IN_FENCE_FD
   * still wait.
   */
  waited = dma_fence_wait(f, true);
  return waited < 0 ? (int)waited : 1;
}

int nvgpu_fence_unwrap_fd(struct nvgpu_device *dev, int fd, u32 *handle,
                          bool *owned) {
  struct dma_fence *f;
  int ret;

  nvgpu_fence_reap();
  *owned = false;
  f = sync_file_get_fence(fd);
  if (!f)
    return -EINVAL; /* as the kernel says for a descriptor that is not one */
  ret = nvgpu_fence_unwrap_ex(dev, f, handle, owned, true);
  dma_fence_put(f);
  return ret;
}

/* ───────── IOCTL2 calls this file makes ───────── */

/* What a call's hooks hand the interpreter. */
struct nvgpu_fence_call {
  struct nvgpu_fd *nfd;
  struct drm_file *file;
  /* FD_IN: the one descriptor field, resolved before the call. */
  bool has_in;
  bool in_used;
  u32 in_handle;
  u32 in_flags;
  /* FD_OUT: the kind the call must produce, and whether the sync_file is
   * wanted as a proxy fence (`fence`, referenced) rather than a descriptor. */
  u32 want_kind;
  bool want_fence;
  struct dma_fence *fence;
  /* GEM_IN: (offset in the argument) -> (owner file, host handle). */
  u32 ngem;
  struct {
    u32 off, owner, gem;
  } gem[2];
};

static int nvgpu_fence_hook_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off,
                                  s64 user_value, u32 kinds, u32 *handle,
                                  u32 *flags) {
  struct nvgpu_fence_call *p = call->priv;

  if (!p->has_in || buf)
    return -EINVAL;
  p->in_used = true;
  *handle = p->in_handle;
  *flags = p->in_flags;
  return 0;
}

static int nvgpu_fence_hook_gem_in(struct nvgpu_i2_call *call, u32 buf,
                                   u32 off, u32 guest_handle, u32 *owner,
                                   u32 *gem) {
  struct nvgpu_fence_call *p = call->priv;
  u32 i;

  for (i = 0; !buf && i < p->ngem; i++) {
    if (p->gem[i].off == off) {
      *owner = p->gem[i].owner;
      *gem = p->gem[i].gem;
      return 0;
    }
  }
  return -EINVAL;
}

/*
 * A descriptor the host made: a sync_file (SEMSURF_FENCE_CREATE, HANDLE_TO_FD
 * with EXPORT_SYNC_FILE) becomes a proxy sync_file, a syncobj file a
 * host-handle file. O_CLOEXEC, as the kernel opens both
 * (nvidia-drm-os-interface.c:244, drm_syncobj.c:677 and :759).
 */
static int nvgpu_fence_hook_fd_out(struct nvgpu_i2_call *call, u32 buf,
                                   u32 off, u32 handle, u32 kind,
                                   s64 *user_value) {
  struct nvgpu_fence_call *p = call->priv;
  int fd;

  if (kind != p->want_kind) {
    dev_warn_ratelimited(&call->dev->vdev->dev,
                         "virtio-gpu-nv: the host made a descriptor of kind "
                         "%u where kind %u was due; refused\n",
                         kind, p->want_kind);
    return -EPROTO;
  }
  if (kind == NVGPU_HK_SYNC_FILE && p->want_fence) {
    struct dma_fence *f = nvgpu_host_fence_new(call->dev, handle, false);

    if (IS_ERR(f))
      return PTR_ERR(f);
    p->fence = f;
    *user_value = -1;
    return 0;
  }
  if (kind == NVGPU_HK_SYNC_FILE)
    fd = nvgpu_fence_from_handle_noclose(call->dev, handle, O_CLOEXEC);
  else
    fd = nvgpu_hostfile_install(call->dev, handle, kind, O_CLOEXEC, false);
  if (fd < 0)
    return fd;
  *user_value = fd;
  return 0;
}

static int nvgpu_fence_ctx_create(struct nvgpu_fence_call *p, u32 host_handle,
                                  u32 *guest_handle);

static int nvgpu_fence_hook_gem_out(struct nvgpu_i2_call *call, u32 buf,
                                    u32 off, u32 gem, u64 size,
                                    u32 *guest_handle) {
  return nvgpu_fence_ctx_create(call->priv, gem, guest_handle);
}

static const struct nvgpu_i2_ops nvgpu_fence_i2_ops = {
    .fd_in = nvgpu_fence_hook_fd_in,
    .gem_in = nvgpu_fence_hook_gem_in,
    .fd_out = nvgpu_fence_hook_fd_out,
    .gem_out = nvgpu_fence_hook_gem_out,
};

/*
 * One render-class call on `target` (a render handle; the call's results land
 * there). `arg` is a kernel buffer when `kernel`, else the caller's pointer.
 * A handle resolved as NVGPU_I2_FD_CONSUME but never handed to the
 * interpreter (the call failed before the hook ran) is closed here.
 */
static long nvgpu_fence_call(struct nvgpu_fence_call *p, u32 target,
                             unsigned int cmd, void *arg, bool kernel) {
  struct nvgpu_i2_call call = {
      .dev = p->nfd->dev,
      .handle = target,
      .render = target,
      .sclass = NVGPU_SCLASS_RENDER,
      .cmd = cmd,
      .uarg = (void __user __force *)arg,
      .kernel = kernel,
      .ops = &nvgpu_fence_i2_ops,
      .priv = p,
  };
  long ret = nvgpu_i2_ioctl(&call);

  if (p->has_in && !p->in_used && (p->in_flags & NVGPU_I2_FD_CONSUME))
    nvgpu_close_handle(p->nfd->dev, p->in_handle);
  return ret;
}

/* A flat argument into kernel memory: `cmd` must be the native one. */
static int nvgpu_fence_copy_in(unsigned int cmd, unsigned int want,
                               void *karg, const void __user *uarg) {
  if (cmd != want)
    return -EINVAL;
  return copy_from_user(karg, uarg, _IOC_SIZE(cmd)) ? -EFAULT : 0;
}

/* ───────── syncobj waits, slept here ───────── */

/*
 * One shared host registration, as this guest sees it: the consumer for its
 * EV_READY cookie, whether that came, and the userspace eventfds that are to
 * be signalled when it does.
 *
 * References: every guest waiter holds one, and the subscriber list holds one
 * while it is not empty. The last put unhashes it; from process context it is
 * then unregistered and freed at once, from the delivery (IRQ) it is buried.
 */
struct nvgpu_sowait {
  struct nvgpu_fence_ev ev;
  u64 cookie;
  refcount_t ref;
  atomic_t fired;
  struct list_head subs; /* nvgpu_sowait_sub, under nvgpu_sowait_lock */
  bool subs_ref;
  struct hlist_node node; /* nvgpu_sowaits, under nvgpu_sowait_lock */
};

/* A userspace SYNCOBJ_EVENTFD. Whoever takes it off its registration's list
 * (under nvgpu_sowait_lock) signals and frees it. */
struct nvgpu_sowait_sub {
  struct list_head node;
  struct eventfd_ctx *ctx;
  u64 id; /* tells it from a later one at the same address */
};

static atomic64_t nvgpu_sowait_sub_ids = ATOMIC64_INIT(0);

static DEFINE_SPINLOCK(nvgpu_sowait_lock);
static DEFINE_HASHTABLE(nvgpu_sowaits, 6);
/* Every guest waiter sleeps here; any registration firing wakes them all,
 * and each looks at its own. Fences fire at frame rate, not faster. */
static DECLARE_WAIT_QUEUE_HEAD(nvgpu_sowait_wq);

/* Registrations one wait makes at most; past that it polls. */
#define NVGPU_SOWAIT_MAX_REG 64
/* Polling backoff: from this, doubling, to at most 1 ms (RV:eventfd). */
#define NVGPU_SOWAIT_BACKOFF_MIN_US 50
#define NVGPU_SOWAIT_BACKOFF_MAX_US 1000
/* TRANSFER's WAIT_FOR_SUBMIT waits this long (drm_syncobj.c:420). */
#define NVGPU_SOWAIT_SUBMIT_NS (5 * NSEC_PER_SEC)

/* The transport died: every guest syncobj waiter looks again (and finds
 * nvgpu_xfer_dead()). */
void nvgpu_fence_wake_waiters(void) { wake_up_all(&nvgpu_sowait_wq); }

static void nvgpu_sowait_free(struct nvgpu_fence_ev *e) {
  kfree(container_of(e, struct nvgpu_sowait, ev));
}

static bool nvgpu_sowait_unhash(struct nvgpu_sowait *s) {
  unsigned long flags;

  if (!refcount_dec_and_lock_irqsave(&s->ref, &nvgpu_sowait_lock, &flags))
    return false;
  hash_del(&s->node);
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
  return true;
}

/* Process context. */
static void nvgpu_sowait_put(struct nvgpu_sowait *s) {
  if (nvgpu_sowait_unhash(s))
    nvgpu_fence_ev_retire(&s->ev);
}

/* Any context. */
static void nvgpu_sowait_put_atomic(struct nvgpu_sowait *s) {
  if (nvgpu_sowait_unhash(s))
    nvgpu_fence_bury(&s->ev);
}

/*
 * EV_READY for a registration: its point is ready (or, WAIT_AVAILABLE, has a
 * fence). Hard IRQ, under the registry lock. Userspace eventfds are signalled
 * here, as the host kernel signals its own (drm_syncobj.c:1407-1414): once.
 */
static void nvgpu_sowait_deliver(struct nvgpu_ev_consumer *c, u32 kind,
                                 u64 cookie, const void *payload, u32 len) {
  struct nvgpu_sowait *s = container_of(c, struct nvgpu_sowait, ev.c);
  struct nvgpu_sowait_sub *sub, *n;
  unsigned long flags;
  LIST_HEAD(subs);
  bool drop;

  if (kind != NVGPU_EV_READY)
    return;
  spin_lock_irqsave(&nvgpu_sowait_lock, flags);
  atomic_set(&s->fired, 1);
  list_splice_init(&s->subs, &subs);
  drop = s->subs_ref;
  s->subs_ref = false;
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);

  list_for_each_entry_safe(sub, n, &subs, node) {
    eventfd_signal(sub->ctx);
    eventfd_ctx_put(sub->ctx);
    kfree(sub);
  }
  wake_up_all(&nvgpu_sowait_wq);
  if (drop)
    nvgpu_sowait_put_atomic(s);
}

/* A new registration object for `cookie`, registered and hashed. */
static struct nvgpu_sowait *nvgpu_sowait_new(struct nvgpu_device *dev,
                                             u64 cookie) {
  struct nvgpu_sowait *s;
  unsigned long flags;
  int ret;

  s = kzalloc(sizeof(*s), GFP_KERNEL);
  if (!s)
    return ERR_PTR(-ENOMEM);
  s->ev.dev = dev;
  s->ev.free = nvgpu_sowait_free;
  s->ev.c.deliver = nvgpu_sowait_deliver;
  s->cookie = cookie;
  refcount_set(&s->ref, 1);
  INIT_LIST_HEAD(&s->subs);
  ret = nvgpu_ev_register(dev, &s->ev.c, NVGPU_EVKEY_COOKIE(cookie));
  if (ret) {
    kfree(s);
    return ERR_PTR(ret);
  }
  nvgpu_dev_get(dev); /* the registration's, put at retire */
  s->ev.registered = true;
  spin_lock_irqsave(&nvgpu_sowait_lock, flags);
  hash_add(nvgpu_sowaits, &s->node, cookie);
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
  return s;
}

/* The live, unfired registration object for `cookie`, referenced. */
static struct nvgpu_sowait *nvgpu_sowait_find(struct nvgpu_device *dev,
                                              u64 cookie) {
  struct nvgpu_sowait *s, *found = NULL;
  unsigned long flags;

  spin_lock_irqsave(&nvgpu_sowait_lock, flags);
  hash_for_each_possible(nvgpu_sowaits, s, node, cookie) {
    if (s->ev.dev == dev && s->cookie == cookie &&
        !atomic_read(&s->fired) && refcount_inc_not_zero(&s->ref)) {
      found = s;
      break;
    }
  }
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
  return found;
}

/*
 * Be woken when (syncobj, point) of the file is ready: register it with the
 * backend under a fresh cookie, or join the registration the backend already
 * has for it. -EAGAIN: the VM is at its cap, poll instead.
 *
 * A joined cookie's object may be gone here (its last waiter left) or have
 * fired; then a fresh object is made for the cookie, and if its report has
 * already come and gone it simply never fires -- every caller polls once
 * more after this returns, which is what catches that.
 */
static struct nvgpu_sowait *nvgpu_sowait_get(struct nvgpu_fd *nfd,
                                             u32 syncobj, u64 point,
                                             u32 flags) {
  struct nvgpu_device *dev = nfd->dev;
  struct nvgpu_sowait *s, *t;
  u64 args[5], res[2];
  u64 cookie;
  int ret;

  cookie = nvgpu_ev_new_cookie(dev);
  s = nvgpu_sowait_new(dev, cookie);
  if (IS_ERR(s))
    return s;
  args[0] = nfd->handle;
  args[1] = syncobj;
  args[2] = point;
  args[3] = flags;
  args[4] = cookie;
  ret = nvgpu_host_op(dev, NVGPU_OP_SYNCOBJ_WATCH, args, 5, res, 2);
  if (ret) {
    nvgpu_sowait_put(s);
    return ERR_PTR(ret);
  }
  if (!res[1])
    return s; /* a registration of our own */

  nvgpu_sowait_put(s);
  if (res[0] <= U32_MAX)
    return ERR_PTR(-EPROTO); /* a legacy handle's cookie: not a registration */
  t = nvgpu_sowait_find(dev, res[0]);
  return t ? t : nvgpu_sowait_new(dev, res[0]);
}

/* One syncobj wait in progress. */
struct nvgpu_sowait_wait {
  struct nvgpu_fence_call p;
  unsigned int cmd; /* DRM_IOCTL_SYNCOBJ_(TIMELINE_)WAIT */
  void *karg;       /* its kernel argument, arrays in kernel memory */
  const u32 *handles;
  const u64 *points; /* NULL: all 0 */
  u32 count;
  u32 reg_flags; /* 0 or DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE */
  bool wait_all;
  /* One registration slot per point, or none at all (nregs 0) for a wait on
   * more points than it is worth registering: that one polls. */
  u32 nregs;
  struct nvgpu_sowait **regs;
  bool *seen;
};

/* The host's answer now: 0 ready, -ETIME not yet, or the call's error. */
static long nvgpu_sowait_poll(struct nvgpu_sowait_wait *w) {
  return nvgpu_fence_call(&w->p, w->p.nfd->handle, w->cmd, w->karg, true);
}

static bool nvgpu_sowait_fired(struct nvgpu_sowait *s) {
  return s && atomic_read(&s->fired);
}

/* A registration fired that this waiter has not looked at yet. */
static bool nvgpu_sowait_progress(struct nvgpu_sowait_wait *w) {
  u32 i;

  for (i = 0; i < w->nregs; i++)
    if (!w->seen[i] && nvgpu_sowait_fired(w->regs[i]))
      return true;
  return false;
}

static void nvgpu_sowait_mark_seen(struct nvgpu_sowait_wait *w) {
  u32 i;

  for (i = 0; i < w->nregs; i++)
    if (nvgpu_sowait_fired(w->regs[i]))
      w->seen[i] = true;
}

static void nvgpu_sowait_disarm(struct nvgpu_sowait_wait *w) {
  u32 i;

  for (i = 0; i < w->nregs; i++) {
    if (w->regs[i])
      nvgpu_sowait_put(w->regs[i]);
    w->regs[i] = NULL;
    w->seen[i] = false;
  }
}

/*
 * The poll said "not yet" after registrations fired. A fired registration is
 * spent, and one that fired while the wait is still unsatisfied is stale: a
 * binary syncobj whose fence was replaced (SIGNAL then RESET, or an import)
 * since. For "any of" that point is simply not ready now, so it is
 * registered afresh. For WAIT_ALL a fired point is still ready and waiting on
 * the others; it is registered afresh only once *every* point has fired and
 * the wait still fails, which again means something was replaced. Returns
 * whether anything was re-armed.
 */
static bool nvgpu_sowait_rearm(struct nvgpu_sowait_wait *w) {
  bool all = true, rearmed = false;
  u32 i;

  for (i = 0; i < w->nregs; i++)
    if (!nvgpu_sowait_fired(w->regs[i]))
      all = false;
  for (i = 0; i < w->nregs; i++) {
    if (!nvgpu_sowait_fired(w->regs[i]) || (w->wait_all && !all))
      continue;
    nvgpu_sowait_put(w->regs[i]);
    w->regs[i] = NULL;
    w->seen[i] = false;
    rearmed = true;
  }
  return rearmed;
}

/* Every point without a registration gets one. */
static int nvgpu_sowait_arm(struct nvgpu_sowait_wait *w) {
  u32 i;

  for (i = 0; i < w->nregs; i++) {
    struct nvgpu_sowait *s;

    if (w->regs[i])
      continue;
    s = nvgpu_sowait_get(w->p.nfd, w->handles[i],
                         w->points ? w->points[i] : 0, w->reg_flags);
    if (IS_ERR(s)) {
      if (PTR_ERR(s) != -EAGAIN)
        dev_warn_ratelimited(&w->p.nfd->dev->vdev->dev,
                             "virtio-gpu-nv: no wait registration for "
                             "syncobj %u: %ld; polling instead\n",
                             w->handles[i], PTR_ERR(s));
      return PTR_ERR(s);
    }
    w->regs[i] = s;
    w->seen[i] = false;
  }
  return 0;
}

static signed long nvgpu_sowait_left(bool forever, unsigned long end) {
  if (forever)
    return MAX_SCHEDULE_TIMEOUT;
  return time_before(jiffies, end) ? (signed long)(end - jiffies) : 0;
}

/* Sleep `us` microseconds (no longer than `left` jiffies), interruptibly. */
static long nvgpu_sowait_nap(u32 us, bool forever, signed long left) {
  u64 ns = (u64)us * NSEC_PER_USEC;
  ktime_t t;

  if (!forever)
    ns = min_t(u64, ns, jiffies_to_nsecs(left));
  t = ns_to_ktime(ns);
  set_current_state(TASK_INTERRUPTIBLE);
  schedule_hrtimeout_range(&t, 20 * NSEC_PER_USEC, HRTIMER_MODE_REL);
  return signal_pending(current) ? -ERESTARTSYS : 0;
}

/*
 * Wait until the host's poll succeeds, the deadline passes (-ETIME) or a
 * signal comes (-ERESTARTSYS), exactly what the native wait answers
 * (drm_syncobj.c:1136-1164). `timeout_nsec` is the caller's absolute
 * CLOCK_MONOTONIC deadline in *this* guest's clock, 0 for a single poll; it
 * is never sent to the host, whose clock it is not in.
 *
 * Each round: poll; register what has no registration; poll again (a point
 * that became ready before its registration is caught here); sleep until a
 * registration fires. A waiter over the VM's registration cap, or with too
 * many points to register, polls with a backoff of at most 1 ms.
 */
static long nvgpu_sowait_run(struct nvgpu_sowait_wait *w, s64 timeout_nsec) {
  signed long left = drm_timeout_abs_to_jiffies(timeout_nsec);
  bool forever = left >= MAX_SCHEDULE_TIMEOUT - 1;
  unsigned long end = jiffies + (forever ? 0 : left);
  bool polling = w->nregs < w->count;
  u32 backoff = NVGPU_SOWAIT_BACKOFF_MIN_US;
  bool rearmed_before = false;
  long ret;

  for (;;) {
    ret = nvgpu_sowait_poll(w);
    if (ret != -ETIME || !timeout_nsec)
      break;
    left = nvgpu_sowait_left(forever, end);
    if (!left)
      break;

    if (!polling) {
      bool rearmed = nvgpu_sowait_rearm(w);

      /* Re-arming twice running means the points keep flipping back; do not
       * spin on it. */
      if (rearmed && rearmed_before) {
        ret = nvgpu_sowait_nap(backoff, forever, left);
        if (ret)
          break;
        backoff = min_t(u32, backoff * 2, NVGPU_SOWAIT_BACKOFF_MAX_US);
      } else if (!rearmed) {
        backoff = NVGPU_SOWAIT_BACKOFF_MIN_US;
      }
      rearmed_before = rearmed;

      if (nvgpu_sowait_arm(w)) {
        nvgpu_sowait_disarm(w);
        polling = true;
        continue;
      }
      ret = nvgpu_sowait_poll(w);
      if (ret != -ETIME)
        break;
      /* Or the device is going: the next poll then fails at once, and
       * remove() is not kept waiting on this ioctl (S-26). */
      ret = wait_event_interruptible_timeout(
          nvgpu_sowait_wq,
          nvgpu_sowait_progress(w) || nvgpu_xfer_dead(w->p.nfd->dev),
          nvgpu_sowait_left(forever, end));
      if (ret < 0)
        break;
      nvgpu_sowait_mark_seen(w);
      continue;
    }

    ret = nvgpu_sowait_nap(backoff, forever, left);
    if (ret)
      break;
    backoff = min_t(u32, backoff * 2, NVGPU_SOWAIT_BACKOFF_MAX_US);
  }
  nvgpu_sowait_disarm(w);
  return ret;
}

static int nvgpu_sowait_alloc(struct nvgpu_sowait_wait *w) {
  w->nregs = w->count <= NVGPU_SOWAIT_MAX_REG ? w->count : 0;
  w->regs = kcalloc(max(w->nregs, 1u), sizeof(*w->regs), GFP_KERNEL);
  w->seen = kcalloc(max(w->nregs, 1u), sizeof(*w->seen), GFP_KERNEL);
  return w->regs && w->seen ? 0 : -ENOMEM;
}

static void nvgpu_sowait_release(struct nvgpu_sowait_wait *w) {
  kfree(w->regs);
  kfree(w->seen);
}

/* The largest SYNCOBJ_WAIT / TIMELINE_WAIT the schema forwards. */
#define NVGPU_SOWAIT_MAX_HANDLES 4096

/*
 * SYNCOBJ_WAIT and SYNCOBJ_TIMELINE_WAIT. The argument may be an older,
 * shorter struct (before deadline_nsec, drm.h:1000-1016); like drm_ioctl we
 * zero what the caller did not send and copy back only what it did.
 */
static long nvgpu_sowait_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg) {
  bool timeline = _IOC_NR(cmd) == _IOC_NR(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT);
  unsigned int kcmd = timeline ? DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT
                               : DRM_IOCTL_SYNCOBJ_WAIT;
  union {
    struct drm_syncobj_wait b;
    struct drm_syncobj_timeline_wait t;
  } a = {}, k;
  struct nvgpu_sowait_wait w = {.p.nfd = nfd, .cmd = kcmd, .karg = &k};
  size_t usize = _IOC_SIZE(cmd);
  u64 uhandles, upoints = 0, deadline;
  u32 *handles = NULL;
  u64 *points = NULL;
  s64 timeout;
  u32 flags;
  long ret;

  if (_IOC_DIR(cmd) != (_IOC_READ | _IOC_WRITE) ||
      usize > _IOC_SIZE(kcmd) || usize < 32)
    return -EINVAL;
  if (copy_from_user(&a, uarg, usize))
    return -EFAULT;
  k = a;
  if (timeline) {
    uhandles = a.t.handles;
    upoints = a.t.points;
    timeout = a.t.timeout_nsec;
    w.count = a.t.count_handles;
    flags = a.t.flags;
    deadline = a.t.deadline_nsec;
  } else {
    uhandles = a.b.handles;
    timeout = a.b.timeout_nsec;
    w.count = a.b.count_handles;
    flags = a.b.flags;
    deadline = a.b.deadline_nsec;
  }
  if (w.count > NVGPU_SOWAIT_MAX_HANDLES)
    return -E2BIG;

  if (w.count) {
    handles = kvmalloc_array(w.count, sizeof(*handles), GFP_KERNEL);
    if (!handles)
      return -ENOMEM;
    ret = -EFAULT;
    if (copy_from_user(handles, u64_to_user_ptr(uhandles),
                       w.count * sizeof(*handles)))
      goto out;
    if (upoints) {
      ret = -ENOMEM;
      points = kvmalloc_array(w.count, sizeof(*points), GFP_KERNEL);
      if (!points)
        goto out;
      ret = -EFAULT;
      if (copy_from_user(points, u64_to_user_ptr(upoints),
                         w.count * sizeof(*points)))
        goto out;
    }
  }
  w.handles = handles;
  w.points = points;
  w.wait_all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL;
  w.reg_flags = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE;

  /*
   * What the host is sent: our copies of the arrays, a zero timeout (the
   * backend makes it zero too), and the deadline hint in the host's clock --
   * it is applied even to a poll (drm_syncobj.c:1122-1129) and is what lets
   * the host boost the GPU for a frame that is late.
   */
  if (timeline) {
    k.t.handles = (u64)(uintptr_t)handles;
    k.t.points = (u64)(uintptr_t)points;
    k.t.timeout_nsec = 0;
    if (flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE)
      k.t.deadline_nsec = nvgpu_guest_to_host_ns(nfd->dev, deadline);
  } else {
    k.b.handles = (u64)(uintptr_t)handles;
    k.b.timeout_nsec = 0;
    if (flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE)
      k.b.deadline_nsec = nvgpu_guest_to_host_ns(nfd->dev, deadline);
  }

  ret = nvgpu_sowait_alloc(&w);
  if (!ret)
    ret = nvgpu_sowait_run(&w, timeout);
  nvgpu_sowait_release(&w);

  /* first_signaled is the host's, from the poll that succeeded; everything
   * else goes back as the caller sent it, as drm_ioctl copies it back. */
  if (!ret) {
    if (timeline)
      a.t.first_signaled = k.t.first_signaled;
    else
      a.b.first_signaled = k.b.first_signaled;
  }
  if (copy_to_user(uarg, &a, usize))
    ret = -EFAULT;
out:
  kvfree(handles);
  kvfree(points);
  return ret;
}

/*
 * One (syncobj, point) until it has a fence, as drm_syncobj_find_fence() with
 * WAIT_FOR_SUBMIT does (drm_syncobj.c:482-509): up to five seconds, -ETIME
 * after, -ERESTARTSYS on a signal.
 */
static long nvgpu_sowait_submitted(struct nvgpu_fd *nfd, u32 syncobj,
                                   u64 point) {
  struct drm_syncobj_timeline_wait k = {
      .handles = (u64)(uintptr_t)&syncobj,
      .points = (u64)(uintptr_t)&point,
      .count_handles = 1,
      .flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE |
               DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
  };
  struct nvgpu_sowait_wait w = {
      .p.nfd = nfd,
      .cmd = DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
      .karg = &k,
      .handles = &syncobj,
      .points = &point,
      .count = 1,
      .reg_flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE,
  };
  long ret;

  ret = nvgpu_sowait_alloc(&w);
  if (!ret)
    ret = nvgpu_sowait_run(&w, ktime_get_ns() + NVGPU_SOWAIT_SUBMIT_NS);
  nvgpu_sowait_release(&w);
  return ret;
}

/*
 * SYNCOBJ_TRANSFER. With WAIT_FOR_SUBMIT the kernel would wait in the ioctl
 * for the source point to be submitted; that wait happens here, and the
 * transfer goes to the host without the flag (the backend refuses it with).
 */
static long nvgpu_fence_transfer(struct nvgpu_fd *nfd, unsigned int cmd,
                                 void __user *uarg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_transfer a;
  long ret;

  ret = nvgpu_fence_copy_in(cmd, DRM_IOCTL_SYNCOBJ_TRANSFER, &a, uarg);
  if (ret)
    return ret;
  if (a.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT) {
    if (a.flags & ~DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT)
      return -EINVAL; /* drm_syncobj.c:446-449 */
    ret = nvgpu_sowait_submitted(nfd, a.src_handle, a.src_point);
    if (ret)
      return ret;
    a.flags &= ~DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT;
  }
  /* Nothing comes back: the kernel writes no field of this struct. */
  return nvgpu_fence_call(&p, nfd->handle, cmd, &a, true);
}

/*
 * SYNCOBJ_EVENTFD from a guest process (a compositor in the guest waiting on
 * an acquire point, SyncTimeline.cpp:80-90): the caller's eventfd is
 * signalled once when the point is ready, from the shared registration's
 * EV_READY. Never forwarded -- each forwarded one would leave a host kernel
 * entry nobody can remove.
 *
 * The registration may have fired before this subscriber joined it, so the
 * point is polled once after joining; ready then, the eventfd is signalled
 * here instead (exactly once either way: whoever takes the subscriber off the
 * list signals it). Over the VM's cap this fails -ENOMEM, the kernel's own
 * answer when an entry cannot be made (drm_syncobj.c:1487-1491).
 */
static long nvgpu_fence_eventfd(struct nvgpu_fd *nfd, unsigned int cmd,
                                void __user *uarg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_eventfd a;
  struct nvgpu_sowait_sub *sub, *it;
  struct nvgpu_sowait *s;
  struct eventfd_ctx *ctx;
  bool now = false, drop = false;
  unsigned long flags;
  u64 id;
  long ret;

  ret = nvgpu_fence_copy_in(cmd, DRM_IOCTL_SYNCOBJ_EVENTFD, &a, uarg);
  if (ret)
    return ret;
  if ((a.flags & ~DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE) || a.pad)
    return -EINVAL; /* drm_syncobj.c:1472-1476 */
  ctx = eventfd_ctx_fdget(a.fd);
  if (IS_ERR(ctx))
    return PTR_ERR(ctx);
  sub = kzalloc(sizeof(*sub), GFP_KERNEL);
  if (!sub) {
    eventfd_ctx_put(ctx);
    return -ENOMEM;
  }
  sub->ctx = ctx;
  sub->id = id = atomic64_inc_return(&nvgpu_sowait_sub_ids);

  s = nvgpu_sowait_get(nfd, a.handle, a.point, a.flags);
  if (IS_ERR(s)) {
    ret = PTR_ERR(s) == -EAGAIN ? -ENOMEM : PTR_ERR(s);
    kfree(sub);
    eventfd_ctx_put(ctx);
    return ret;
  }

  spin_lock_irqsave(&nvgpu_sowait_lock, flags);
  if (atomic_read(&s->fired)) {
    now = true;
  } else {
    list_add_tail(&sub->node, &s->subs);
    if (!s->subs_ref) {
      refcount_inc(&s->ref);
      s->subs_ref = true;
    }
  }
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);

  if (!now) {
    struct drm_syncobj_timeline_wait k = {
        .handles = (u64)(uintptr_t)&a.handle,
        .points = (u64)(uintptr_t)&a.point,
        .count_handles = 1,
        /* An unsubmitted point is "not yet" here, never an error. */
        .flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | a.flags,
    };

    if (nvgpu_fence_call(&p, nfd->handle, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
                         &k, true) == 0) {
      /* Still ours only if still listed: the delivery may have taken (and
       * freed) it meanwhile, so it is looked for, never looked at. */
      spin_lock_irqsave(&nvgpu_sowait_lock, flags);
      list_for_each_entry(it, &s->subs, node) {
        if (it == sub && it->id == id) {
          list_del(&sub->node);
          now = true;
          break;
        }
      }
      if (now && list_empty(&s->subs) && s->subs_ref) {
        s->subs_ref = false;
        drop = true;
      }
      spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
    }
  }
  if (now) {
    eventfd_signal(sub->ctx);
    eventfd_ctx_put(sub->ctx);
    kfree(sub);
  }
  if (drop)
    nvgpu_sowait_put(s);
  nvgpu_sowait_put(s);
  return 0;
}

/* ───────── the other syncobj ioctls ───────── */

/*
 * SYNCOBJ_HANDLE_TO_FD: a host syncobj file becomes a host-handle file, a
 * host sync_file (EXPORT_SYNC_FILE) a proxy sync_file.
 */
static long nvgpu_fence_handle_to_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                                     void __user *uarg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_handle a;
  long ret;

  ret = nvgpu_fence_copy_in(cmd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &a, uarg);
  if (ret)
    return ret;
  p.want_kind = a.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE
                    ? NVGPU_HK_SYNC_FILE
                    : NVGPU_HK_SYNCOBJ;
  ret = nvgpu_fence_call(&p, nfd->handle, cmd, &a, true);
  if (copy_to_user(uarg, &a, sizeof(a)))
    ret = -EFAULT;
  return ret;
}

/*
 * SYNCOBJ_FD_TO_HANDLE. Without IMPORT_SYNC_FILE the descriptor must be one
 * of our syncobj files (natively, a syncobj file, drm_syncobj.c:712); with
 * it, a sync_file, unwrapped. A sync_file that is already signalled leaves
 * the host nothing to import, and importing a signalled fence is exactly
 * SIGNAL on a binary syncobj or TIMELINE_SIGNAL of the point on a timeline
 * (drm_syncobj.c:728-757 against :1560-1647), so that is what is sent.
 */
static long nvgpu_fence_fd_to_handle(struct nvgpu_fd *nfd, unsigned int cmd,
                                     void __user *uarg) {
  const u32 valid = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE |
                    DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE;
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_handle a;
  struct file *held = NULL;
  bool owned;
  long ret;

  ret = nvgpu_fence_copy_in(cmd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &a, uarg);
  if (ret)
    return ret;
  if (a.pad || (a.flags & ~valid))
    return -EINVAL; /* drm_syncobj.c:897-901, before the rewrite hides it */

  if (a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE) {
    u64 point = a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE ? a.point : 0;

    ret = nvgpu_fence_unwrap_fd(nfd->dev, a.fd, &p.in_handle, &owned);
    if (ret < 0)
      return ret;
    if (ret == 1) {
      if (point) {
        struct drm_syncobj_timeline_array s = {
            .handles = (u64)(uintptr_t)&a.handle,
            .points = (u64)(uintptr_t)&point,
            .count_handles = 1,
        };

        return nvgpu_fence_call(&p, nfd->handle,
                                DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &s, true);
      } else {
        struct drm_syncobj_array s = {
            .handles = (u64)(uintptr_t)&a.handle,
            .count_handles = 1,
        };

        return nvgpu_fence_call(&p, nfd->handle, DRM_IOCTL_SYNCOBJ_SIGNAL,
                                &s, true);
      }
    }
    p.has_in = true;
    p.in_flags = owned ? NVGPU_I2_FD_CONSUME : 0;
  } else {
    /* Held across the call, so a close racing it cannot close the handle
     * before the host has taken its own reference. */
    held = nvgpu_hostfile_fget(nfd->dev, a.fd, NVGPU_HK_SYNCOBJ, &p.in_handle);
    if (IS_ERR(held))
      return -EINVAL;
    p.has_in = true;
  }

  ret = nvgpu_fence_call(&p, nfd->handle, cmd, &a, true);
  if (held)
    fput(held);
  if (copy_to_user(uarg, &a, sizeof(a)))
    ret = -EFAULT;
  return ret;
}

bool nvgpu_fence_is_syncobj_ioctl(unsigned int cmd) {
  if (_IOC_TYPE(cmd) != DRM_IOCTL_BASE)
    return false;
  switch (_IOC_NR(cmd)) {
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_CREATE):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_DESTROY):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_WAIT):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_RESET):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_SIGNAL):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_QUERY):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_TRANSFER):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_EVENTFD):
    return true;
  default:
    return false;
  }
}

/*
 * Every syncobj ioctl, on the file's render handle, where the syncobjs are
 * (a guest file's host render file is the one holding its syncobj handles,
 * so they are forwarded as they are). The guest core's own syncobj table
 * stays empty: nothing reaches drm_ioctl().
 */
long nvgpu_fence_syncobj_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, unsigned long arg) {
  void __user *uarg = (void __user *)arg;
  struct nvgpu_fence_call p = {.nfd = nfd, .file = file};

  nvgpu_fence_reap();
  switch (_IOC_NR(cmd)) {
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_WAIT):
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT):
    return nvgpu_sowait_ioctl(nfd, cmd, uarg);
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD):
    return nvgpu_fence_handle_to_fd(nfd, cmd, uarg);
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE):
    return nvgpu_fence_fd_to_handle(nfd, cmd, uarg);
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_TRANSFER):
    return nvgpu_fence_transfer(nfd, cmd, uarg);
  case _IOC_NR(DRM_IOCTL_SYNCOBJ_EVENTFD):
    return nvgpu_fence_eventfd(nfd, cmd, uarg);
  default:
    /* CREATE, DESTROY, RESET, SIGNAL, QUERY, TIMELINE_SIGNAL: handles and
     * points, nothing to translate and nothing that waits. */
    return nvgpu_fence_call(&p, nfd->handle, cmd, (void __force *)uarg,
                            false);
  }
}

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
static int nvgpu_fence_ctx_create(struct nvgpu_fence_call *p, u32 host_handle,
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
  u32 dmabuf;
  int ret, tries = 0;

again:
  mutex_lock(&nvgpu_rehome_lock);
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
  if (!res[0] || res[0] > U32_MAX) {
    ret = -EPROTO;
    goto out_free;
  }
  dmabuf = (u32)res[0];
  args[0] = file->handle;
  args[1] = dmabuf;
  ret = nvgpu_host_op(dev, NVGPU_OP_DMABUF_IMPORT, args, 2, res, 2);
  nvgpu_close_handle(dev, dmabuf);
  if (ret)
    goto out_free;
  if (!res[0] || res[0] > U32_MAX) {
    ret = -EPROTO;
    goto out_free;
  }
  r->borrowed = nvgpu_gem_proxy_find(file, (u32)res[0]);
  if (!r->borrowed && nvgpu_gem_dying(file, (u32)res[0])) {
    /* Not ours to use or close: wait out the proxy's close, outside the
     * lock its free takes (nvgpu_fence_gem_free()), and import again. */
    kfree(r);
    mutex_unlock(&nvgpu_rehome_lock);
    if (++tries > 3)
      return -EAGAIN;
    ret = nvgpu_gem_wait_gone(file, (u32)res[0]);
    if (ret)
      return ret;
    goto again;
  }
  r->ng = ng;
  nvgpu_fd_get(file);
  r->file = file;
  r->gem = (u32)res[0];
  hash_add(nvgpu_rehomes, &r->node, (unsigned long)ng);
  *gem = r->gem;
  ret = 0;
  goto out;

out_free:
  kfree(r);
out:
  mutex_unlock(&nvgpu_rehome_lock);
  if (ret)
    dev_warn_ratelimited(&dev->vdev->dev,
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
                                 void __user *uarg) {
  struct nvgpu_semsurf_attach a;
  struct nvgpu_fence_ctx *ctx;
  struct nvgpu_gem_object *ng;
  struct nvgpu_fd *target;
  u32 gem;
  long ret;

  ret = nvgpu_fence_copy_in(cmd, NVGPU_IOCTL_SEMSURF_ATTACH, &a, uarg);
  if (ret)
    return ret;
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
  if (!res[0] || res[0] > U32_MAX)
    return -EPROTO;
  p.has_in = true;
  p.in_handle = (u32)res[0];
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
    dev_warn_ratelimited(&ctx->dev->vdev->dev,
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

  queue_work(system_unbound_wq, &d->work);
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
    dev_warn_ratelimited(&ctx->dev->vdev->dev,
                         "virtio-gpu-nv: SEMSURF_FENCE_ATTACH done, but not "
                         "mirrored into the guest buffer's resv: %ld\n",
                         ret);
}

long nvgpu_fence_semsurf_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void __user *uarg) {
  struct nvgpu_fence_call p = {.nfd = nfd, .file = file};
  long ret;

  nvgpu_fence_reap();
  switch (_IOC_NR(cmd)) {
  case _IOC_NR(NVGPU_IOCTL_SEMSURF_CTX_CREATE):
    /*
     * RM handles of the caller's own client and a size, no descriptor
     * (nvkms-kapi-sync.c:182-306): forwarded from the caller's memory; the
     * new context comes back as a GEM handle and gets its proxy (gem_out).
     */
    return nvgpu_fence_call(&p, nfd->handle, cmd, (void __force *)uarg, false);

  case _IOC_NR(NVGPU_IOCTL_SEMSURF_CREATE): {
    struct nvgpu_semsurf_create a;

    ret = nvgpu_fence_copy_in(cmd, NVGPU_IOCTL_SEMSURF_CREATE, &a, uarg);
    if (ret)
      return ret;
    p.want_kind = NVGPU_HK_SYNC_FILE;
    ret = nvgpu_semsurf_on_ctx(&p, cmd, &a, a.fence_context_handle);
    if (copy_to_user(uarg, &a, sizeof(a)))
      ret = -EFAULT;
    return ret;
  }

  case _IOC_NR(NVGPU_IOCTL_SEMSURF_WAIT): {
    struct nvgpu_semsurf_wait a;
    struct nvgpu_fence_ctx *ctx;
    struct dma_fence *f;
    bool owned;

    ret = nvgpu_fence_copy_in(cmd, NVGPU_IOCTL_SEMSURF_WAIT, &a, uarg);
    if (ret)
      return ret;
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

  case _IOC_NR(NVGPU_IOCTL_SEMSURF_ATTACH):
    return nvgpu_semsurf_attach(&p, cmd, uarg);

  default:
    return -ENOTTY;
  }
}
