// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the core syncobj ioctls on a guest DRM file (nvgpu_fence.c's
 * header has the design). A syncobj is the host's, under the same handle;
 * the ioctls go to the host as IOCTL2 calls, except the waits: a wait is a
 * poll there, and the sleeping is done here, on a shared host
 * SYNCOBJ_EVENTFD registration per (file, syncobj, point), with userspace
 * SYNCOBJ_EVENTFDs riding on the same registrations.
 */

#include <drm/drm.h>
#include <drm/drm_file.h>
#include <drm/drm_utils.h>
#include <linux/dma-fence.h>
#include <linux/eventfd.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/hashtable.h>
#include <linux/hrtimer.h>
#include <linux/jiffies.h>
#include <linux/refcount.h>
#include <linux/sched/signal.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/sync_file.h>
#include <linux/uaccess.h>
#include <linux/wait.h>

#include "gen/nvgpu_schema.h"
#include "nvgpu_fence.h"

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
  /* What it waits on: syncobj handle `syncobj` of backend render handle
   * `render` (a DESTROY or the file's close ends its subscribers). */
  u32 render;
  u32 syncobj;
  refcount_t ref;
  atomic_t fired;
  struct list_head subs; /* nvgpu_sowait_sub, under nvgpu_sowait_lock */
  bool subs_ref;
  struct hlist_node node; /* nvgpu_sowaits, under nvgpu_sowait_lock */
};

/*
 * A guest process's userspace SYNCOBJ_EVENTFDs outstanding (quota.rs's
 * pattern, on this side: the backend's share bounds registrations, this the
 * subscribers riding on them, each an eventfd reference and a little
 * memory). Under nvgpu_sowait_lock.
 */
struct nvgpu_sowait_owner {
  struct hlist_node node;
  u64 start_ns;
  u32 tgid;
  u32 n;
};

/* A userspace SYNCOBJ_EVENTFD. Whoever takes it off its registration's list
 * (under nvgpu_sowait_lock) signals and frees it. */
struct nvgpu_sowait_sub {
  struct list_head node;
  struct eventfd_ctx *ctx;
  struct nvgpu_sowait_owner *owner; /* charged to, under nvgpu_sowait_lock */
  u64 id; /* tells it from a later one at the same address */
};

static atomic64_t nvgpu_sowait_sub_ids = ATOMIC64_INIT(0);

static DEFINE_SPINLOCK(nvgpu_sowait_lock);
static DEFINE_HASHTABLE(nvgpu_sowaits, 6);
static DEFINE_HASHTABLE(nvgpu_sowait_owners, 6);
static u32 nvgpu_sowait_nsubs;

/*
 * Subscribers outstanding in the guest, and each process's share: a quarter,
 * the last sixteenth kept for processes holding at most a sixty-fourth (the
 * split device/src/quota.rs calls Share::quarter). A compositor holds one per
 * pending acquire point, a handful per surface.
 */
#define NVGPU_SOWAIT_SUBS_MAX 4096
#define NVGPU_SOWAIT_SUBS_OWNER (NVGPU_SOWAIT_SUBS_MAX / 4)
#define NVGPU_SOWAIT_SUBS_RESERVE (NVGPU_SOWAIT_SUBS_MAX / 16)
#define NVGPU_SOWAIT_SUBS_FLOOR (NVGPU_SOWAIT_SUBS_MAX / 64)
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

/*
 * Charge a new subscriber to the calling process: its owner record (the
 * existing one, or `*spare`, which is then consumed), or NULL over its share
 * or the pool -- the caller answers -ENOMEM, the kernel's own answer when it
 * cannot make an entry. Under the lock.
 */
static struct nvgpu_sowait_owner *
nvgpu_sowait_charge_locked(struct nvgpu_sowait_owner **spare) {
  struct nvgpu_sowait_owner *it, *mine = NULL;
  u64 start_ns;
  u32 tgid, after;

  rcu_read_lock();
  start_ns = READ_ONCE(current->group_leader)->start_time;
  rcu_read_unlock();
  tgid = task_tgid_nr(current);
  hash_for_each_possible(nvgpu_sowait_owners, it, node, tgid) {
    if (it->tgid == tgid && it->start_ns == start_ns) {
      mine = it;
      break;
    }
  }
  after = (mine ? mine->n : 0) + 1;
  if (nvgpu_sowait_nsubs >= NVGPU_SOWAIT_SUBS_MAX ||
      after > NVGPU_SOWAIT_SUBS_OWNER ||
      (nvgpu_sowait_nsubs + 1 >
           NVGPU_SOWAIT_SUBS_MAX - NVGPU_SOWAIT_SUBS_RESERVE &&
       after > NVGPU_SOWAIT_SUBS_FLOOR))
    return NULL;
  if (!mine) {
    mine = *spare;
    *spare = NULL;
    mine->tgid = tgid;
    mine->start_ns = start_ns;
    mine->n = 0;
    hash_add(nvgpu_sowait_owners, &mine->node, tgid);
  }
  mine->n++;
  nvgpu_sowait_nsubs++;
  return mine;
}

/* A subscriber is gone: its process's charge back. Under the lock; any
 * context. */
static void nvgpu_sowait_uncharge_locked(struct nvgpu_sowait_sub *sub) {
  struct nvgpu_sowait_owner *o = sub->owner;

  sub->owner = NULL;
  if (!o)
    return;
  nvgpu_sowait_nsubs--;
  if (!--o->n) {
    hash_del(&o->node);
    kfree(o);
  }
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
  list_for_each_entry(sub, &subs, node)
    nvgpu_sowait_uncharge_locked(sub);
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
                                             u64 cookie, u32 render,
                                             u32 syncobj) {
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
  s->render = render;
  s->syncobj = syncobj;
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
  s = nvgpu_sowait_new(dev, cookie, nfd->handle, syncobj);
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
  return t ? t : nvgpu_sowait_new(dev, res[0], nfd->handle, syncobj);
}

/*
 * Syncobj `syncobj` of `nfd`'s render file was destroyed (`all`: the file
 * is going, and every one of its syncobjs with it): the userspace eventfds
 * subscribed through it are let go unsignalled, as the kernel frees a
 * syncobj's entries without signalling them (drm_syncobj.c:533-538). The
 * backend drops or orphans the registrations themselves; a guest waiter
 * inside SYNCOBJ_WAIT holds its own references and finds out on its next
 * poll.
 */
static void nvgpu_sowait_forget(struct nvgpu_fd *nfd, u32 syncobj, bool all) {
  struct nvgpu_sowait_sub *sub, *n;
  struct nvgpu_sowait *s, *found;
  unsigned long flags;
  unsigned int bkt;

  for (;;) {
    LIST_HEAD(subs);

    found = NULL;
    spin_lock_irqsave(&nvgpu_sowait_lock, flags);
    hash_for_each(nvgpu_sowaits, bkt, s, node) {
      if (s->ev.dev == nfd->dev && s->render == nfd->handle &&
          (all || s->syncobj == syncobj) && s->subs_ref) {
        found = s;
        break;
      }
    }
    if (found) {
      list_splice_init(&found->subs, &subs);
      list_for_each_entry(sub, &subs, node)
        nvgpu_sowait_uncharge_locked(sub);
      found->subs_ref = false;
    }
    spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
    if (!found)
      return;
    list_for_each_entry_safe(sub, n, &subs, node) {
      eventfd_ctx_put(sub->ctx);
      kfree(sub);
    }
    nvgpu_sowait_put(found);
  }
}

void nvgpu_fence_file_release(struct nvgpu_fd *nfd) {
  nvgpu_sowait_forget(nfd, 0, true);
  nvgpu_fence_reap();
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
  nvgpu_pace_inc(NVGPU_PACE_SW_POLLS);
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
        dev_dbg_ratelimited(&w->p.nfd->dev->vdev->dev,
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

  nvgpu_pace_inc(NVGPU_PACE_SW_NAPS);
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

  if (timeout_nsec)
    nvgpu_pace_inc(NVGPU_PACE_SW_WAITS);
  if (polling)
    nvgpu_pace_inc(NVGPU_PACE_SW_OVERCAP);
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
        nvgpu_pace_inc(NVGPU_PACE_SW_OVERCAP);
        continue;
      }
      ret = nvgpu_sowait_poll(w);
      if (ret != -ETIME)
        break;
      /* Or the device is going: the next poll then fails at once, and
       * remove() is not kept waiting on this ioctl (S-26). */
      nvgpu_pace_inc(NVGPU_PACE_SW_SLEEPS);
      ret = wait_event_interruptible_timeout(
          nvgpu_sowait_wq,
          nvgpu_sowait_progress(w) || nvgpu_xfer_dead(w->p.nfd->dev),
          nvgpu_sowait_left(forever, end));
      if (ret < 0)
        break;
      if (nvgpu_sowait_progress(w))
        nvgpu_pace_inc(NVGPU_PACE_SW_WOKEN);
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
 * SYNCOBJ_WAIT and SYNCOBJ_TIMELINE_WAIT, on the caller's argument as the
 * DRM node's entry normalised it: an older, shorter struct (before
 * deadline_nsec, drm.h:1000-1016) arrives zero-extended, as drm_ioctl()
 * hands it over, and the entry copies back only what the caller sent.
 */
static long nvgpu_sowait_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                               void *karg) {
  bool timeline = cmd == DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT;
  union {
    struct drm_syncobj_wait b;
    struct drm_syncobj_timeline_wait t;
  } a = {}, k;
  struct nvgpu_sowait_wait w = {.p.nfd = nfd, .cmd = cmd, .karg = &k};
  u64 uhandles, upoints = 0, deadline;
  u32 *handles = NULL;
  u64 *points = NULL;
  s64 timeout;
  u32 flags;
  long ret;

  memcpy(&a, karg, timeline ? sizeof(a.t) : sizeof(a.b));
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
  memcpy(karg, &a, timeline ? sizeof(a.t) : sizeof(a.b));
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
                                 void *karg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_transfer a;
  long ret;

  /* A copy: the flags sent are not the caller's, which go back unchanged. */
  memcpy(&a, karg, sizeof(a));
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
 * list signals it). Over the VM's cap, the caller's share of it, or the
 * caller's share of the guest's subscribers, this fails -ENOMEM, the
 * kernel's own answer when an entry cannot be made (drm_syncobj.c:1487-1491).
 * A subscriber lasts until it is signalled, or its syncobj handle is
 * destroyed or its file closed (nvgpu_sowait_forget()).
 */
static long nvgpu_fence_eventfd(struct nvgpu_fd *nfd, unsigned int cmd,
                                void *karg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_eventfd a;
  struct nvgpu_sowait_sub *sub, *it;
  struct nvgpu_sowait_owner *owner;
  struct nvgpu_sowait *s;
  struct eventfd_ctx *ctx;
  bool now = false, drop = false;
  unsigned long flags;
  u64 id;
  long ret;

  memcpy(&a, karg, sizeof(a));
  if ((a.flags & ~DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE) || a.pad)
    return -EINVAL; /* drm_syncobj.c:1472-1476 */
  ctx = eventfd_ctx_fdget(a.fd);
  if (IS_ERR(ctx))
    return PTR_ERR(ctx);
  sub = kzalloc(sizeof(*sub), GFP_KERNEL);
  owner = kzalloc(sizeof(*owner), GFP_KERNEL);
  if (!sub || !owner) {
    kfree(sub);
    kfree(owner);
    eventfd_ctx_put(ctx);
    return -ENOMEM;
  }
  sub->ctx = ctx;
  sub->id = id = atomic64_inc_return(&nvgpu_sowait_sub_ids);

  s = nvgpu_sowait_get(nfd, a.handle, a.point, a.flags);
  if (IS_ERR(s)) {
    ret = PTR_ERR(s) == -EAGAIN ? -ENOMEM : PTR_ERR(s);
    kfree(sub);
    kfree(owner);
    eventfd_ctx_put(ctx);
    return ret;
  }

  spin_lock_irqsave(&nvgpu_sowait_lock, flags);
  if (atomic_read(&s->fired)) {
    now = true;
  } else {
    sub->owner = nvgpu_sowait_charge_locked(&owner);
    if (sub->owner) {
      list_add_tail(&sub->node, &s->subs);
      if (!s->subs_ref) {
        refcount_inc(&s->ref);
        s->subs_ref = true;
      }
    }
  }
  spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
  kfree(owner); /* unless the charge took it */
  if (!now && !sub->owner) {
    /* Over the caller's share of the guest's subscribers. */
    kfree(sub);
    eventfd_ctx_put(ctx);
    nvgpu_sowait_put(s);
    return -ENOMEM;
  }

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
          nvgpu_sowait_uncharge_locked(sub);
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
                                     void *karg) {
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_handle *a = karg;

  p.want_kind = a->flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE
                    ? NVGPU_HK_SYNC_FILE
                    : NVGPU_HK_SYNCOBJ;
  /* No pointer in it: a struct of the driver's as far as IOCTL2 goes. */
  return nvgpu_fence_call(&p, nfd->handle, cmd, a, true);
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
                                     void *karg) {
  const u32 valid = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE |
                    DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE;
  struct nvgpu_fence_call p = {.nfd = nfd};
  struct drm_syncobj_handle a;
  struct file *held = NULL;
  bool owned;
  long ret;

  /* Read once; the reply goes back into the caller's argument. */
  memcpy(&a, karg, sizeof(a));
  if (a.pad || (a.flags & ~valid))
    return -EINVAL; /* drm_syncobj.c:897-901, before the rewrite hides it */

  if (a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE) {
    u64 point = a.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE ? a.point : 0;

    ret = nvgpu_fence_unwrap_fd(nfd->dev, a.fd, &p.in_handle, &owned,
                                &p.in_ref);
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
  memcpy(karg, &a, sizeof(a));
  return ret;
}

/*
 * The native commands and the structs this file builds are this kernel's
 * (drm.h), and the host's schema must agree: the interpreter refuses any
 * size but the schema's, so a guest kernel whose drm_syncobj_* grew would
 * have every syncobj call fail -EINVAL at run time, with one warning line to
 * say why. Asserted here instead, at build time, number (and so size) and
 * all.
 */
#define NVGPU_SYNCOBJ_SCHEMA(name)                                             \
  static_assert(DRM_IOCTL_##name == NVGPU_SCHEMA_CMD_##name,                  \
                "DRM_IOCTL_" #name " differs from the IOCTL2 schema's")
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_CREATE);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_DESTROY);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_HANDLE_TO_FD);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_FD_TO_HANDLE);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_WAIT);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_RESET);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_SIGNAL);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_TIMELINE_WAIT);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_QUERY);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_TRANSFER);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_TIMELINE_SIGNAL);
NVGPU_SYNCOBJ_SCHEMA(SYNCOBJ_EVENTFD);
#undef NVGPU_SYNCOBJ_SCHEMA

unsigned int nvgpu_fence_syncobj_cmd(unsigned int cmd) {
  static const unsigned int native[] = {
      DRM_IOCTL_SYNCOBJ_CREATE,          DRM_IOCTL_SYNCOBJ_DESTROY,
      DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,    DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
      DRM_IOCTL_SYNCOBJ_WAIT,            DRM_IOCTL_SYNCOBJ_RESET,
      DRM_IOCTL_SYNCOBJ_SIGNAL,          DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
      DRM_IOCTL_SYNCOBJ_QUERY,           DRM_IOCTL_SYNCOBJ_TRANSFER,
      DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_EVENTFD,
  };
  unsigned int i;

  if (_IOC_TYPE(cmd) != DRM_IOCTL_BASE)
    return 0;
  for (i = 0; i < ARRAY_SIZE(native); i++)
    if (_IOC_NR(native[i]) == _IOC_NR(cmd))
      return native[i];
  return 0;
}

/*
 * Every syncobj ioctl, on the file's render handle, where the syncobjs are
 * (a guest file's host render file is the one holding its syncobj handles,
 * so they are forwarded as they are). The guest core's own syncobj table
 * stays empty: nothing reaches drm_ioctl().
 */
long nvgpu_fence_syncobj_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void *karg) {
  struct nvgpu_fence_call p = {.nfd = nfd, .file = file};

  nvgpu_fence_reap();
  switch (cmd) {
  case DRM_IOCTL_SYNCOBJ_WAIT:
  case DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT:
    return nvgpu_sowait_ioctl(nfd, cmd, karg);
  case DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD:
    return nvgpu_fence_handle_to_fd(nfd, cmd, karg);
  case DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE:
    return nvgpu_fence_fd_to_handle(nfd, cmd, karg);
  case DRM_IOCTL_SYNCOBJ_TRANSFER:
    return nvgpu_fence_transfer(nfd, cmd, karg);
  case DRM_IOCTL_SYNCOBJ_EVENTFD:
    return nvgpu_fence_eventfd(nfd, cmd, karg);
  case DRM_IOCTL_SYNCOBJ_DESTROY: {
    struct drm_syncobj_destroy a;
    long ret;

    /* Read once: the handle whose subscribers go is the one destroyed. */
    memcpy(&a, karg, sizeof(a));
    ret = nvgpu_fence_call(&p, nfd->handle, cmd, &a, true);
    if (!ret)
      nvgpu_sowait_forget(nfd, a.handle, false);
    return ret;
  }
  case DRM_IOCTL_SYNCOBJ_CREATE:
  case DRM_IOCTL_SYNCOBJ_RESET:
  case DRM_IOCTL_SYNCOBJ_SIGNAL:
  case DRM_IOCTL_SYNCOBJ_QUERY:
  case DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL:
    /* Handles and points, nothing to translate and nothing that waits; the
     * arrays they point at are the caller's. */
    return nvgpu_fence_call(&p, nfd->handle, cmd, karg, false);
  default:
    return -ENOTTY;
  }
}

/* nvgpu_fence_device_dead(), for the syncobj waits: every SYNCOBJ_EVENTFD
 * subscriber of `dev` signalled, every guest syncobj waiter woken. */
void nvgpu_syncobj_device_dead(struct nvgpu_device *dev) {
  struct nvgpu_sowait *s, *found;
  unsigned long flags;
  unsigned int bkt;

  for (;;) {
    struct nvgpu_sowait_sub *sub, *n;
    LIST_HEAD(subs);

    found = NULL;
    spin_lock_irqsave(&nvgpu_sowait_lock, flags);
    hash_for_each(nvgpu_sowaits, bkt, s, node) {
      if (s->ev.dev == dev && s->subs_ref) {
        found = s;
        break;
      }
    }
    if (found) {
      atomic_set(&found->fired, 1);
      list_splice_init(&found->subs, &subs);
      list_for_each_entry(sub, &subs, node)
        nvgpu_sowait_uncharge_locked(sub);
      found->subs_ref = false;
    }
    spin_unlock_irqrestore(&nvgpu_sowait_lock, flags);
    if (!found)
      break;
    list_for_each_entry_safe(sub, n, &subs, node) {
      eventfd_signal(sub->ctx);
      eventfd_ctx_put(sub->ctx);
      kfree(sub);
    }
    nvgpu_sowait_put(found);
  }
  nvgpu_fence_wake_waiters();
}
