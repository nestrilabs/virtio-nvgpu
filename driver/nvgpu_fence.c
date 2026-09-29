// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: fences (ARCHITECTURE.md, "Fences").
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
 *    SYNC_IOC_MERGE and FILE_INFO on them, which then need no
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
 * This file has the proxies, unwrapping, and the IOCTL2 calls; the syncobj
 * ioctls and their waits are nvgpu_syncobj.c, semaphore-surface fences
 * nvgpu_semsurf.c, and what the three share nvgpu_fence.h.
 *
 * Contexts and locks. Event delivery runs in hard IRQ under the event
 * registry's lock (nvgpu_events.c), and a dma_fence's last reference can be
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
#include <linux/dma-fence-unwrap.h>
#include <linux/dma-fence.h>
#include <linux/err.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/module.h>
#include <linux/refcount.h>
#include <linux/sched/signal.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/sync_file.h>
#include <linux/uaccess.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>

#include "nvgpu_fence.h"

bool nvgpu_fences_enabled(struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_FENCES);
}

/* ───────── event consumers that outlive what they report on ───────── */

static DEFINE_SPINLOCK(nvgpu_fence_dead_lock);
static LIST_HEAD(nvgpu_fence_dead);

void nvgpu_fence_bury(struct nvgpu_fence_ev *e) {
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
void nvgpu_fence_ev_retire(struct nvgpu_fence_ev *e) {
  struct nvgpu_device *dev = e->dev;
  bool registered = e->registered;

  if (registered)
    nvgpu_ev_unregister(dev, &e->c);
  e->free(e);
  if (registered)
    nvgpu_dev_put(dev);
}

void nvgpu_fence_reap(void) {
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
  /*
   * The backend's sync_file. Open for as long as the proxy lives, signalled
   * or not: a host consumer handed this fence later still needs it. Cleared
   * (a failure handing it back to its caller) only before the WATCH is sent
   * or queued, which reads it (nvgpu_host_fence_watch()).
   */
  u32 handle;
  u64 cookie; /* the WATCH's, which the consumer is registered under */
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
 * Whether a new proxy's WATCH is sent from a work item rather than by the
 * caller. Every presented frame makes two host sync_files that become guest
 * ones (the render-done fence's export and its import), and each WATCH was a
 * round trip in the presenting thread; nothing it does needs the answer
 * first. 0 is for measuring what that saves.
 */
static bool nvgpu_async_fence_watch = true;
module_param_named(async_fence_watch, nvgpu_async_fence_watch, bool, 0644);
MODULE_PARM_DESC(async_fence_watch, "send a host fence proxy's WATCH from a "
                                    "work item (default on)");

struct nvgpu_fence_watch_work {
  struct work_struct work;
  struct nvgpu_host_fence *f; /* referenced until the WATCH is answered */
  u64 cookie;
};

/*
 * A WATCH the backend refused, or that never reached it: a fence nobody will
 * ever report must not be waited on forever, so it is signalled with the
 * error now.
 */
static void nvgpu_host_fence_unwatched(struct nvgpu_host_fence *f, int ret) {
  if (atomic_xchg(&f->signalled, 1))
    return;
  dev_warn_ratelimited(&f->dev->vdev->dev,
                       "virtio-gpu-nv: the backend would not watch host "
                       "fence %u: %d; signalled with the error\n",
                       f->handle, ret);
  dma_fence_set_error(&f->base, ret < 0 && ret >= -MAX_ERRNO ? ret : -EIO);
  dma_fence_signal(&f->base);
}

/*
 * The proxy's WATCH, answered. The reference held meanwhile keeps the proxy
 * -- and so its handle, which its release closes -- until the WATCH has gone
 * out ahead of any CLOSE; the handle is the proxy's for good by the time
 * this was queued (nvgpu_host_fence_watch()).
 */
static void nvgpu_fence_watch_fn(struct work_struct *work) {
  struct nvgpu_fence_watch_work *w =
      container_of(work, struct nvgpu_fence_watch_work, work);
  struct nvgpu_host_fence *f = w->f;
  int ret = nvgpu_watch(f->dev, f->handle, NVGPU_W_FENCE | NVGPU_W_ONESHOT,
                        w->cookie);

  if (ret)
    nvgpu_host_fence_unwatched(f, ret);
  kfree(w);
  /* Last: it may be the proxy's release, and with it the module's
   * reference; nvgpu_wq is destroyed at exit, which waits for this item to
   * return before the module text goes. */
  dma_fence_put(&f->base);
}

/*
 * Start watching the proxy's host fence: once it owns its handle for good --
 * installed as a descriptor, or kept by its caller -- and not before: queued
 * while a failure could still hand the handle back, the WATCH could reach the
 * backend after the caller's CLOSE of the same handle, on another queue. A
 * WATCH that fails signals the proxy with the error
 * (nvgpu_host_fence_unwatched()), sent here or from the work item alike.
 */
static void nvgpu_host_fence_watch(struct nvgpu_host_fence *f) {
  int ret;

  if (READ_ONCE(nvgpu_async_fence_watch)) {
    struct nvgpu_fence_watch_work *w = kmalloc(sizeof(*w), GFP_KERNEL);

    if (w) {
      INIT_WORK(&w->work, nvgpu_fence_watch_fn);
      w->f = f;
      w->cookie = f->cookie;
      dma_fence_get(&f->base);
      queue_work(nvgpu_wq, &w->work);
      return;
    }
  }
  ret = nvgpu_watch(f->dev, f->handle, NVGPU_W_FENCE | NVGPU_W_ONESHOT,
                    f->cookie);
  if (ret)
    nvgpu_host_fence_unwatched(f, ret);
}

/*
 * A proxy dma_fence for host sync_file `handle`, with one reference, which
 * will signal when the host's does -- once nvgpu_host_fence_watch() has been
 * called, when the proxy owns the handle for good. On failure `handle` is
 * closed only if `own`; an IOCTL2 fd_out hook passes false, because the
 * interpreter closes what a failing hook was given.
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
   * Consumer first, WATCH second (nvgpu_host_fence_watch()): once the
   * backend watches, the report can come at once (a host fence that has
   * already signalled), and it must find the consumer and the proxy in
   * place.
   */
  cookie = nvgpu_ev_new_cookie(dev);
  ret = nvgpu_ev_register(dev, &e->c, NVGPU_EVKEY_COOKIE(cookie));
  if (ret)
    goto put;
  nvgpu_dev_get(dev); /* the registration's, put at retire */
  e->registered = true;
  f->cookie = cookie;
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
  /* The descriptor owns the handle now: nothing hands it back after this. */
  nvgpu_host_fence_watch(container_of(base, struct nvgpu_host_fence, base));
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
    if (!nvgpu_res_u32(res[0], &acc))
      return -EPROTO;
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
int nvgpu_fence_unwrap_ex(struct nvgpu_device *dev, struct dma_fence *f,
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
   * before handing one on, so a dma_fence_array of our
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
   * before the host is told there is nothing to wait for. Interruptible
   * and unbounded, like the native waits on such a fence; its error, if it has one, is not carried over. Native never
   * blocks here -- the host kernel takes a callback -- which is what 0x56
   * now does too; syncobj IMPORT_SYNC_FILE and a committing IN_FENCE_FD
   * still wait.
   */
  waited = dma_fence_wait(f, true);
  return waited < 0 ? (int)waited : 1;
}

void nvgpu_fence_put_ref(void *fence) { dma_fence_put(fence); }

int nvgpu_fence_unwrap_fd(struct nvgpu_device *dev, int fd, u32 *handle,
                          bool *owned, struct dma_fence **ref) {
  struct dma_fence *f;
  int ret;

  nvgpu_fence_reap();
  *owned = false;
  *ref = NULL;
  f = sync_file_get_fence(fd);
  if (!f)
    return -EINVAL; /* as the kernel says for a descriptor that is not one */
  ret = nvgpu_fence_unwrap_ex(dev, f, handle, owned, true);
  /*
   * A proxy's own handle is open only as long as the proxy is: another
   * thread closing the sync_file could let its last reference go, and the
   * handle's CLOSE, before the call naming it ran -- and the host give the
   * number to someone else's object. So the caller keeps the fence (an
   * array's components with it) until the host is done with the call.
   */
  if (ret == 0 && !*owned) {
    *ref = f;
    return 0;
  }
  dma_fence_put(f);
  return ret;
}

/* ───────── IOCTL2 calls this file makes ───────── */

static int nvgpu_fence_hook_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off,
                                  s64 user_value, u32 kinds, u32 *handle,
                                  u32 *flags) {
  struct nvgpu_fence_call *p = call->priv;

  if (!p->has_in || buf)
    return -EINVAL;
  p->in_used = true;
  *handle = p->in_handle;
  *flags = p->in_flags;
  /* Held with the request: past a timeout too, until the host is done. */
  if (p->in_ref) {
    struct dma_fence *f = p->in_ref;

    p->in_ref = NULL;
    return nvgpu_i2_hold(call, nvgpu_fence_put_ref, f);
  }
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
    dev_dbg_ratelimited(&call->dev->vdev->dev,
                        "virtio-gpu-nv: the host made a descriptor of kind "
                        "%u where kind %u was due; refused\n",
                        kind, p->want_kind);
    return -EPROTO;
  }
  if (kind == NVGPU_HK_SYNC_FILE && p->want_fence) {
    struct dma_fence *f = nvgpu_host_fence_new(call->dev, handle, false);

    if (IS_ERR(f))
      return PTR_ERR(f);
    /* Kept by the call from here, whatever it returns: the handle is the
     * proxy's for good (its release closes it), so it may be watched. */
    nvgpu_host_fence_watch(container_of(f, struct nvgpu_host_fence, base));
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
 * there). `arg` is always kernel memory: with `built`, a struct the driver
 * built, every address in it a kernel one; else the caller's argument as the
 * DRM node's entry copied it in (struct nvgpu_drm_arg), every pointer in it
 * the caller's. A handle resolved as NVGPU_I2_FD_CONSUME but never handed to
 * the interpreter (the call failed before the hook ran) is closed here.
 */
long nvgpu_fence_call(struct nvgpu_fence_call *p, u32 target,
                      unsigned int cmd, void *arg, bool built) {
  struct nvgpu_i2_call call = {
      .dev = p->nfd->dev,
      .handle = target,
      .render = target,
      .sclass = NVGPU_SCLASS_RENDER,
      .cmd = cmd,
      .uarg = (void __user __force *)arg,
      .kernel = built,
      .karg = !built,
      .ops = &nvgpu_fence_i2_ops,
      .priv = p,
  };
  long ret = nvgpu_i2_ioctl(&call);

  if (p->has_in && !p->in_used && (p->in_flags & NVGPU_I2_FD_CONSUME))
    nvgpu_close_handle(p->nfd->dev, p->in_handle);
  /* Never handed to the request: the call never named the handle. */
  if (p->in_ref) {
    dma_fence_put(p->in_ref);
    p->in_ref = NULL;
  }
  return ret;
}

/*
 * The transport is dead (nvgpu_xfer_reclaim()): no EV_FENCE or EV_READY will
 * ever come for `dev`, so nothing waiting on one may go on waiting. A native
 * GPU loss ends its fences with an error too; here:
 *
 *  - every host-fence proxy of the device not yet signalled is signalled
 *    with -ENODEV, which a sync_file's poll, an IN_FENCE_FD and a
 *    dma_fence_wait all see;
 *  - every SYNCOBJ_EVENTFD subscriber of it is signalled, as the kernel
 *    signals one when its point gets a fence -- the waiter then finds the
 *    device gone on its next call;
 *  - guest syncobj waiters are woken, and find nvgpu_xfer_dead().
 *
 * A proxy made afterwards fails its WATCH with -ENODEV and is signalled with
 * that error then. Process context.
 */
void nvgpu_fence_device_dead(struct nvgpu_device *dev) {
  struct nvgpu_host_fence *f;
  unsigned long id;

  rcu_read_lock();
  xa_for_each(&nvgpu_host_fences, id, f) {
    if (f->dev != dev || !dma_fence_get_rcu(&f->base))
      continue;
    rcu_read_unlock();
    if (!atomic_xchg(&f->signalled, 1)) {
      dma_fence_set_error(&f->base, -ENODEV);
      dma_fence_signal(&f->base);
    }
    dma_fence_put(&f->base);
    rcu_read_lock();
  }
  rcu_read_unlock();

  nvgpu_syncobj_device_dead(dev);
}

void nvgpu_fence_drain(void) { nvgpu_fence_reap(); }
