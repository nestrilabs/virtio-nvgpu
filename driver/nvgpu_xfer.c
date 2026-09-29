// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the transport -- request contexts on the control queue,
 * transport buffers, HELLO and the host clock, HOST_OP / WATCH / CLOSE, and
 * the event queue with its consumer registry.
 *
 * Lock ordering, outermost first. Nothing here sleeps under any of them.
 *
 *   nvgpu_events.vq_lock   event virtqueue add/get/kick_prepare; held alone,
 *                          around those calls only, never across dispatch
 *   nvgpu_events.lock      consumer registry and nvgpu_fd.drm_file; every
 *                          consumer's deliver() runs under it
 *   nvgpu_device.fds_lock  legacy handle -> nvgpu_fd list (taken inside
 *                          nvgpu_events.lock by legacy EV_READY delivery)
 *   drm_device.event_lock  taken by a consumer inside deliver()
 *
 *   nvgpu_xfer.lock        control virtqueue add/kick_prepare/get_buf, the
 *                          per-request completed/abandoned flags and `dead`.
 *                          Innermost: a consumer may queue a CLOSE from
 *                          deliver() (nvgpu_close_handle_async() takes it
 *                          under nvgpu_events.lock), but nothing is taken
 *                          while it is held except the internal locks of
 *                          complete(), queue_work() and the ring itself.
 *
 * The clock's seqlock stands apart: its writer (the resync work) takes no
 * other lock, and readers may be anywhere, a deliver() included.
 *
 * All four spinlocks are taken with interrupts off, because the virtqueue
 * callbacks that take them run in hard interrupt context (vring_interrupt).
 * A consumer's deliver() must not call nvgpu_ev_register/unregister (it holds
 * the registry lock) nor anything that submits a request (that sleeps).
 */

#include <drm/drm.h>
#include <drm/drm_device.h>
#include <drm/drm_file.h>
#include <drm/drm_sysfs.h>
#include <linux/bitops.h>
#include <linux/hashtable.h>
#include <linux/kobject.h>
#include <linux/ktime.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/overflow.h>
#include <linux/scatterlist.h>
#include <linux/seqlock.h>
#include <linux/sizes.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/virtio_ring.h>
#include <linux/workqueue.h>

#include "nvgpu.h"

/* ───────── Limits ───────── */

/*
 * Largest piece of a transport buffer. Order 4 is the largest allocation the
 * page allocator still makes readily on a fragmented guest, and 64 KiB pieces
 * put 4 MiB -- the most a v2 backend accepts -- in 64 scatter-gather entries.
 */
#define NVGPU_TBUF_CHUNK SZ_64K
#define NVGPU_TBUF_CHUNK_ORDER get_order(NVGPU_TBUF_CHUNK)
/* Below this a buffer is one kmalloc with its header: most messages are tiny,
 * and a page apiece for a 40-byte CLOSE would be pure overhead. */
#define NVGPU_TBUF_INLINE_MAX 1024

/*
 * Scatter-gather entries per direction.
 *
 * Without indirect descriptors every entry is a ring slot, and a chain longer
 * than the free slots is refused with -ENOSPC -- forever, if it is longer than
 * the ring (virtio_ring.c:688 WARNs and :706 refuses). 32 a side keeps one
 * request to a quarter of a 256-slot ring. With them the chain takes one slot
 * and its table is allocated per request, so the cap is only there to keep
 * that table to a page and a chain within the ring if the table cannot be had
 * (virtqueue_use_indirect() falls back to direct slots, virtio_ring.c:684).
 */
#define NVGPU_SG_MAX_DIRECT 32
#define NVGPU_SG_MAX_INDIRECT 128

/*
 * A v1 backend writes its whole reply into the *last* writable descriptor,
 * and never more than 64 KiB of it (vhost-user-nvgpu.rs RESP_MAX). So until
 * HELLO says otherwise a response is one physically contiguous piece of at
 * most that: a second descriptor would have the reply land in the wrong place.
 */
#define NVGPU_V1_RESP_MAX SZ_64K

/*
 * How long a caller waits. Host display calls are bounded (nvidia-drm's
 * longest wait is 2 x 3 s, nvidia-drm-modeset.c:770,921) but uninterruptible,
 * so the waits are killable-only, like the native ioctls they stand for.
 */
#define NVGPU_INLINE_TIMEOUT (30 * HZ)
#define NVGPU_EXECUTOR_TIMEOUT (60 * HZ)

/* Clock: samples per sync, the RTT we trust, how often, how fast to slew. */
#define NVGPU_TIME_SAMPLES 8
#define NVGPU_TIME_RTT_GOOD_NS (200 * NSEC_PER_USEC)
#define NVGPU_TIME_RESYNC (5 * HZ)
/* 50 us per second, i.e. one part in 20000. */
#define NVGPU_TIME_SLEW_DIV 20000

#define NVGPU_EVENT_BUFS 16

/* WATCH cookies start above every u32, so none can be mistaken for a handle
 * in an EV_READY record (legacy watches use cookie == handle). */
#define NVGPU_COOKIE_BASE (1ull << 32)

/* ───────── State ───────── */

struct nvgpu_tbuf {
  size_t len;
  unsigned int nents;
  bool inline_data; /* the data follows sg[0] in this allocation */
  /* nvgpu_tbuf_on_free(): run when the buffer is freed, whoever frees it. */
  void (*release)(void *arg);
  void *release_arg;
  struct scatterlist sg[];
};

/*
 * One control-queue request, and the virtqueue token for it.
 *
 * Ownership is decided under xf->lock by two flags. `completed` is set by
 * whoever gets the buffers back from the ring (the callback, or reclaim after a
 * reset); `abandoned` by a waiter that gave up first. The waiter frees the
 * context if it sees `completed`; otherwise it sets `abandoned` and walks away,
 * and the callback, finding `abandoned`, hands the context to `work` -- which
 * reaps whatever handles the late reply created and frees everything. One of
 * the two always happens, exactly once, so no reference count is needed.
 */
struct nvgpu_req {
  struct nvgpu_device *dev;
  struct nvgpu_tbuf *req;
  struct nvgpu_tbuf *resp;
  struct completion done;
  struct work_struct work;
  u64 t1; /* ktime_get_ns() in the callback that returned it */
  u32 used_len;
  u32 exec_units; /* executor budget held until the device gives it back */
  bool completed;
  bool abandoned;
  bool dead; /* reclaimed after a device reset: never answered */
};

struct nvgpu_xfer {
  struct nvgpu_device *dev;
  spinlock_t lock;
  bool dead;
  /* Bumped whenever the device returns buffers: what an -ENOSPC sleeper
   * waits to see change. */
  unsigned long space_gen;
  wait_queue_head_t space_wq;

  bool indirect;
  unsigned int vring_size;
  unsigned int max_sg; /* per direction */

  /* Callers spinning for a reply (under `lock`): while there are any, the
   * control queue's interrupt is off and they take replies off the ring
   * themselves (nvgpu_ctrl_poll_enter()). */
  unsigned int pollers;

  /* Ring slots executor-class requests may hold between them. */
  atomic_t exec_avail;
  int exec_budget;
  wait_queue_head_t exec_wq;

  atomic_t next_id;
  /* Ordered: CLOSEs, reaping of abandoned replies, hotplug uevents and the
   * clock resync run one at a time, in the order they were queued. */
  struct workqueue_struct *wq;

  /* host_ns = guest_ns + offset; see nvgpu_clk_offset(). */
  seqlock_t clk_lock;
  bool clk_valid;
  s64 off_base;
  s64 off_target;
  u64 clk_anchor;
  /* The host's CLOCK_REALTIME and CLOCK_MONOTONIC_RAW, each less its
   * CLOCK_MONOTONIC, as of the last sync (TIME_SYNC's long form); valid only
   * with clk_ext. See nvgpu_host_clock_to_guest(). */
  bool clk_ext;
  s64 host_real_mono;
  s64 host_raw_mono;
  struct delayed_work sync_work;
};

struct nvgpu_events {
  struct nvgpu_device *dev;
  spinlock_t lock;
  DECLARE_HASHTABLE(consumers, 6);
  atomic64_t next_cookie;
  spinlock_t vq_lock;
  void *bufs[NVGPU_EVENT_BUFS];
  struct work_struct hotplug_work;
  unsigned long hotplug_pending; /* card indices, NVGPU_EV_HOTPLUG_F_HOTPLUG */
  unsigned long lease_pending;   /* card indices, NVGPU_EV_HOTPLUG_F_LEASE */
};

/* ───────── Transport buffers ───────── */

static struct nvgpu_tbuf *nvgpu_tbuf_alloc_sg(size_t len, gfp_t gfp,
                                              unsigned int max_sg) {
  struct nvgpu_tbuf *tb;
  size_t remaining = len;
  unsigned int n = 0;

  gfp &= ~__GFP_HIGHMEM; /* every piece must have a kernel address */
  if (!len || !max_sg)
    return NULL;

  if (len <= NVGPU_TBUF_INLINE_MAX) {
    tb = kmalloc(struct_size(tb, sg, 1) + len, gfp);
    if (!tb)
      return NULL;
    tb->len = len;
    tb->nents = 1;
    tb->inline_data = true;
    tb->release = NULL;
    sg_init_one(&tb->sg[0], &tb->sg[1], len);
    return tb;
  }

  if (len > (size_t)max_sg * NVGPU_TBUF_CHUNK)
    return NULL;

  tb = kzalloc(struct_size(tb, sg, max_sg), gfp);
  if (!tb)
    return NULL;
  sg_init_table(tb->sg, max_sg);

  /*
   * Largest pieces first, falling back an order at a time, but never below
   * the order at which what is left would no longer fit the entries left:
   * that one is asked for without __GFP_NORETRY, since failing it fails the
   * buffer. Each piece's order is get_order() of its length, which is what
   * nvgpu_tbuf_free() relies on -- a short last piece is allocated at the
   * order of its own length, not the chunk's.
   */
  while (remaining) {
    size_t need = DIV_ROUND_UP(remaining, max_sg - n);
    unsigned int lo = get_order(need);
    unsigned int hi = min_t(unsigned int, get_order(remaining),
                            NVGPU_TBUF_CHUNK_ORDER);
    struct page *pg = NULL;
    unsigned int o;
    size_t clen;

    if (lo > hi)
      goto fail;
    for (o = hi;; o--) {
      gfp_t g = o > lo ? gfp | __GFP_NORETRY | __GFP_NOWARN : gfp;

      pg = alloc_pages(g, o);
      if (pg || o == lo)
        break;
    }
    if (!pg)
      goto fail;

    clen = min_t(size_t, remaining, PAGE_SIZE << o);
    sg_set_page(&tb->sg[n++], pg, clen, 0);
    remaining -= clen;
  }

  if (n < max_sg) {
    sg_unmark_end(&tb->sg[max_sg - 1]);
    sg_mark_end(&tb->sg[n - 1]);
  }
  tb->nents = n;
  tb->len = len;
  return tb;

fail:
  tb->nents = n;
  nvgpu_tbuf_free(tb);
  return NULL;
}

/*
 * NULL on failure. The entry budget depends on the length, not the device:
 * up to 2 MiB the buffer fits the 32 entries a ring without indirect
 * descriptors allows, and larger buffers can only be sent with them anyway.
 */
struct nvgpu_tbuf *nvgpu_tbuf_alloc(size_t len, gfp_t gfp) {
  unsigned int max_sg = len <= (size_t)NVGPU_SG_MAX_DIRECT * NVGPU_TBUF_CHUNK
                            ? NVGPU_SG_MAX_DIRECT
                            : NVGPU_SG_MAX_INDIRECT;

  return nvgpu_tbuf_alloc_sg(len, gfp, max_sg);
}

void nvgpu_tbuf_free(struct nvgpu_tbuf *tb) {
  unsigned int i;

  if (!tb)
    return;
  if (tb->release)
    tb->release(tb->release_arg);
  if (!tb->inline_data)
    for (i = 0; i < tb->nents; i++)
      __free_pages(sg_page(&tb->sg[i]), get_order(tb->sg[i].length));
  kfree(tb);
}

size_t nvgpu_tbuf_len(const struct nvgpu_tbuf *tb) { return tb->len; }

/*
 * What a request's numbers stand for, kept until the request is done with:
 * a request buffer is freed by its caller once the reply is in, or by the
 * transport once a request its caller gave up on (-ETIMEDOUT, -EINTR) has
 * been answered late or is known never to run (nvgpu_req_free_orphan()) --
 * the moment the host can no longer act on what the request names. Always
 * process context. One per buffer.
 */
void nvgpu_tbuf_on_free(struct nvgpu_tbuf *tb, void (*fn)(void *arg),
                        void *arg) {
  WARN_ON(tb->release);
  tb->release = fn;
  tb->release_arg = arg;
}

enum nvgpu_tbuf_op {
  NVGPU_TB_WRITE,
  NVGPU_TB_WRITE_USER,
  NVGPU_TB_READ,
  NVGPU_TB_READ_USER,
  NVGPU_TB_ZERO,
};

/* Walks the pieces [off, off + len) falls in and does `op` on each. */
static int nvgpu_tbuf_copy(struct nvgpu_tbuf *tb, size_t off, void *kbuf,
                           void __user *ubuf, size_t len,
                           enum nvgpu_tbuf_op op) {
  size_t pos = 0;
  unsigned int i;

  if (len > tb->len || off > tb->len - len)
    return -EINVAL;

  for (i = 0; i < tb->nents && len; i++) {
    size_t seglen = tb->sg[i].length, in, n;
    u8 *va;

    if (off >= pos + seglen) {
      pos += seglen;
      continue;
    }
    in = off - pos;
    n = min(seglen - in, len);
    va = (u8 *)sg_virt(&tb->sg[i]) + in;

    switch (op) {
    case NVGPU_TB_WRITE:
      memcpy(va, kbuf, n);
      kbuf = (u8 *)kbuf + n;
      break;
    case NVGPU_TB_READ:
      memcpy(kbuf, va, n);
      kbuf = (u8 *)kbuf + n;
      break;
    case NVGPU_TB_WRITE_USER:
      if (copy_from_user(va, ubuf, n))
        return -EFAULT;
      ubuf = (u8 __user *)ubuf + n;
      break;
    case NVGPU_TB_READ_USER:
      if (copy_to_user(ubuf, va, n))
        return -EFAULT;
      ubuf = (u8 __user *)ubuf + n;
      break;
    case NVGPU_TB_ZERO:
      memset(va, 0, n);
      break;
    }
    off += n;
    len -= n;
    pos += seglen;
  }
  return 0;
}

int nvgpu_tbuf_write(struct nvgpu_tbuf *tb, size_t off, const void *src,
                     size_t len) {
  return nvgpu_tbuf_copy(tb, off, (void *)src, NULL, len, NVGPU_TB_WRITE);
}

int nvgpu_tbuf_write_user(struct nvgpu_tbuf *tb, size_t off,
                          const void __user *src, size_t len) {
  return nvgpu_tbuf_copy(tb, off, NULL, (void __user *)src, len,
                         NVGPU_TB_WRITE_USER);
}

int nvgpu_tbuf_read(const struct nvgpu_tbuf *tb, size_t off, void *dst,
                    size_t len) {
  return nvgpu_tbuf_copy((struct nvgpu_tbuf *)tb, off, dst, NULL, len,
                         NVGPU_TB_READ);
}

int nvgpu_tbuf_read_user(const struct nvgpu_tbuf *tb, size_t off,
                         void __user *dst, size_t len) {
  return nvgpu_tbuf_copy((struct nvgpu_tbuf *)tb, off, NULL, dst, len,
                         NVGPU_TB_READ_USER);
}

int nvgpu_tbuf_zero(struct nvgpu_tbuf *tb, size_t off, size_t len) {
  return nvgpu_tbuf_copy(tb, off, NULL, NULL, len, NVGPU_TB_ZERO);
}

/* ───────── Frame-pacing counters ───────── */

/*
 * Per message type: calls and the sum of their round trips; for all of them
 * two histograms in powers of two of a microsecond (bucket 0 under 1 us, the
 * last everything past 2^(n-2) us): the round trip (request on the ring ->
 * the callback that took the answer off), and the wake (that callback -> the
 * caller running again), which is this guest's scheduler and the vCPU's.
 * Relaxed atomics: a few increments against a VM exit per call.
 */
#define NVGPU_PACE_TYPES 18
#define NVGPU_PACE_BUCKETS 18

static struct {
  atomic64_t calls[NVGPU_PACE_TYPES];
  atomic64_t rtt_ns[NVGPU_PACE_TYPES];
  atomic64_t rtt[NVGPU_PACE_BUCKETS];
  atomic64_t wake[NVGPU_PACE_BUCKETS];
  atomic64_t ctr[NVGPU_PACE_CTRS];
} nvgpu_pace;

void nvgpu_pace_inc(enum nvgpu_pace_ctr c) {
  atomic64_inc(&nvgpu_pace.ctr[c]);
}

static unsigned int nvgpu_pace_bucket(u64 ns) {
  u64 us = div_u64(ns, NSEC_PER_USEC);

  return us ? min_t(unsigned int, fls64(us), NVGPU_PACE_BUCKETS - 1) : 0;
}

static void nvgpu_pace_call(u32 type, u64 t0, u64 t1, u64 t2) {
  u32 i = type < NVGPU_PACE_TYPES ? type : 0;

  if (!t0 || t1 < t0 || t2 < t1)
    return;
  atomic64_inc(&nvgpu_pace.calls[i]);
  atomic64_add(t1 - t0, &nvgpu_pace.rtt_ns[i]);
  atomic64_inc(&nvgpu_pace.rtt[nvgpu_pace_bucket(t1 - t0)]);
  atomic64_inc(&nvgpu_pace.wake[nvgpu_pace_bucket(t2 - t1)]);
}

static int nvgpu_pace_get(char *buf, const struct kernel_param *kp) {
  static const char *const ctr[NVGPU_PACE_CTRS] = {
      "sowait_waits", "sowait_polls",  "sowait_sleeps",
      "sowait_woken", "sowait_naps",   "sowait_overcap",
      "ev_batches",   "ev_records",    "ev_legacy",
      "ev_legacy_set", "rm_polls",     "rm_polls_ready"};
  int n = 0, i;

  for (i = 0; i < NVGPU_PACE_TYPES; i++) {
    s64 c = atomic64_read(&nvgpu_pace.calls[i]);

    if (c)
      n += scnprintf(buf + n, PAGE_SIZE - n, "type %d: %lld calls, rtt %lld ns mean\n",
                     i, c, div64_s64(atomic64_read(&nvgpu_pace.rtt_ns[i]), c));
  }
  n += scnprintf(buf + n, PAGE_SIZE - n, "rtt_us_log2:");
  for (i = 0; i < NVGPU_PACE_BUCKETS; i++)
    n += scnprintf(buf + n, PAGE_SIZE - n, " %lld", atomic64_read(&nvgpu_pace.rtt[i]));
  n += scnprintf(buf + n, PAGE_SIZE - n, "\nwake_us_log2:");
  for (i = 0; i < NVGPU_PACE_BUCKETS; i++)
    n += scnprintf(buf + n, PAGE_SIZE - n, " %lld", atomic64_read(&nvgpu_pace.wake[i]));
  n += scnprintf(buf + n, PAGE_SIZE - n, "\n");
  for (i = 0; i < NVGPU_PACE_CTRS; i++)
    n += scnprintf(buf + n, PAGE_SIZE - n, "%s %lld\n", ctr[i],
                   atomic64_read(&nvgpu_pace.ctr[i]));
  return n;
}

static const struct kernel_param_ops nvgpu_pace_ops = {
    .get = nvgpu_pace_get,
};
/* Root only: the calls of every process in the guest, as they happen. */
module_param_cb(pacing, &nvgpu_pace_ops, NULL, 0400);
MODULE_PARM_DESC(pacing, "frame-pacing counters: round trips per message "
                         "type, their latency, syncobj waits, events");

/* ───────── Control queue ───────── */

/*
 * How long a caller spins for its reply before it sleeps, in microseconds.
 * Most replies come back in 5-20 us (the backend's service time is a few);
 * sleeping for one costs the reply an interrupt that has to wake a task,
 * and often a vCPU that halted meanwhile -- a few microseconds each, on
 * every one of the dozen or so round trips a presented frame makes. A
 * spinning vCPU does not halt, so it costs the host no more than KVM's own
 * halt polling would have. 0 sleeps at once, as before. Executor-class
 * requests (a commit, a modeset) never spin: they take milliseconds.
 */
static unsigned int nvgpu_rt_spin_us = 20;
module_param_named(rt_spin_us, nvgpu_rt_spin_us, uint, 0644);
MODULE_PARM_DESC(rt_spin_us, "microseconds to spin for a reply before "
                             "sleeping (default 20, 0 never spins)");

/*
 * Whether HELLO offers to arm device readiness (NVGPU_GCAP_ARMS_READY): the
 * backend then reports an RM descriptor's events once per wait here instead
 * of once per event. 0 is for measuring what that saves.
 */
static bool nvgpu_arm_ready = true;
module_param_named(arm_ready, nvgpu_arm_ready, bool, 0444);
MODULE_PARM_DESC(arm_ready, "arm RM descriptor readiness per wait (default "
                            "on; takes effect at HELLO)");

struct nvgpu_times {
  u64 t0; /* just before the request went on the ring */
  u64 t1; /* in the callback that took it off */
};

static void nvgpu_req_reap_work(struct work_struct *work);
static unsigned int nvgpu_release_consumed(struct nvgpu_device *dev,
                                           const struct nvgpu_tbuf *req);

/* Budget units go back when the device returns the buffers, not when a
 * waiter gives up: until then the slots are still on the ring. */
static void nvgpu_exec_release(struct nvgpu_xfer *xf, u32 units) {
  if (!units)
    return;
  atomic_add(units, &xf->exec_avail);
  wake_up_all(&xf->exec_wq);
}

static bool nvgpu_exec_take(struct nvgpu_xfer *xf, u32 units) {
  int avail = atomic_read(&xf->exec_avail);

  do {
    if (avail < (int)units)
      return false;
  } while (!atomic_try_cmpxchg(&xf->exec_avail, &avail, avail - units));
  return true;
}

/*
 * Take every reply the device has returned off the control ring and complete
 * its request. Under xf->lock; returns the executor budget units the replies
 * give back (for nvgpu_exec_release(), after the lock), and says whether any
 * buffer came back (*freed).
 */
static u32 nvgpu_ctrl_harvest(struct nvgpu_xfer *xf, struct virtqueue *vq,
                              bool *freed) {
  u64 now = ktime_get_ns();
  struct nvgpu_req *r;
  unsigned int len;
  u32 units = 0;

  while ((r = virtqueue_get_buf(vq, &len)) != NULL) {
    r->used_len = len;
    r->t1 = now;
    r->completed = true;
    units += r->exec_units;
    r->exec_units = 0;
    *freed = true;
    if (r->abandoned)
      queue_work(xf->wq, &r->work);
    else
      complete(&r->done);
  }
  if (*freed)
    WRITE_ONCE(xf->space_gen, xf->space_gen + 1);
  return units;
}

static void nvgpu_ctrl_harvested(struct nvgpu_xfer *xf, u32 units,
                                 bool freed) {
  if (freed)
    wake_up_all(&xf->space_wq);
  nvgpu_exec_release(xf, units);
}

void nvgpu_ctrl_vq_cb(struct virtqueue *vq) {
  struct nvgpu_device *dev = vq->vdev->priv;
  struct nvgpu_xfer *xf = dev->xfer;
  unsigned long flags;
  bool freed = false;
  u32 units;

  if (!xf)
    return;

  spin_lock_irqsave(&xf->lock, flags);
  units = nvgpu_ctrl_harvest(xf, vq, &freed);
  spin_unlock_irqrestore(&xf->lock, flags);
  nvgpu_ctrl_harvested(xf, units, freed);
}

/*
 * A caller spinning for its reply takes replies off the ring itself, and
 * while any caller spins the control queue's interrupt is off. A reply then
 * costs the host no eventfd signal and this guest no interrupt -- about a
 * microsecond of the three or four a round trip takes -- and the caller sees
 * it as soon as the device writes it. The last caller to stop spinning turns
 * the interrupt back on and takes whatever arrived meanwhile, so a caller
 * that sleeps (an executor-class request, or one that spun out) is still
 * woken, by the next spinner or by the interrupt.
 */
static void nvgpu_ctrl_poll_enter(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  unsigned long flags;

  spin_lock_irqsave(&xf->lock, flags);
  /* A dead transport's queue is being taken apart (nvgpu_xfer_reclaim()
   * sets `dead` under this lock first): nothing here touches it then. */
  if (!xf->pollers++ && !xf->dead)
    virtqueue_disable_cb(dev->ctrl_vq);
  spin_unlock_irqrestore(&xf->lock, flags);
}

static void nvgpu_ctrl_poll(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  unsigned long flags;
  bool freed = false;
  u32 units;

  /* Another spinner holds it: it is harvesting for us. */
  if (!spin_trylock_irqsave(&xf->lock, flags))
    return;
  units = xf->dead ? 0 : nvgpu_ctrl_harvest(xf, dev->ctrl_vq, &freed);
  spin_unlock_irqrestore(&xf->lock, flags);
  nvgpu_ctrl_harvested(xf, units, freed);
}

static void nvgpu_ctrl_poll_exit(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  unsigned long flags;
  bool freed = false;
  u32 units = 0;

  spin_lock_irqsave(&xf->lock, flags);
  /* The last one out: interrupts back on, and anything that came back
   * before they were is taken now (enable_cb says so by returning false). */
  if (!--xf->pollers && !xf->dead && !virtqueue_enable_cb(dev->ctrl_vq))
    units = nvgpu_ctrl_harvest(xf, dev->ctrl_vq, &freed);
  spin_unlock_irqrestore(&xf->lock, flags);
  nvgpu_ctrl_harvested(xf, units, freed);
}

/*
 * The one path every request takes. See nvgpu_xfer() in nvgpu.h for the
 * contract; `sent` says whether the request reached the ring, which is what
 * the CLOSE helpers need to know to fall back to their async twins, and `tm`
 * is for TIME_SYNC.
 */
static int __nvgpu_xfer(struct nvgpu_device *dev, struct nvgpu_tbuf *req,
                        struct nvgpu_tbuf *resp, u32 flags, u32 *used_len,
                        struct nvgpu_times *tm, bool *sent, u32 *req_id) {
  struct nvgpu_xfer *xf = dev->xfer;
  struct scatterlist *sgs[2];
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_req *r;
  unsigned long irqf;
  u64 t0 = 0;
  u32 units = 0, id;
  long wret;
  bool kick = false;
  int ret;

  if (sent)
    *sent = false;
  if (req_id)
    *req_id = 0;
  if (used_len)
    *used_len = 0;
  if (!xf)
    return -ENODEV;
  if (!req || !resp || req->len < sizeof(hdr))
    return -EINVAL;

  if (req->nents > xf->max_sg || resp->nents > xf->max_sg) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: request of %u+%u pieces is more than "
                         "the ring takes (%u a side)\n",
                         req->nents, resp->nents, xf->max_sg);
    return -E2BIG;
  }
  if (dev->v2 && req->len > dev->max_req)
    return -E2BIG;
  if (!dev->v2 && resp->nents > 1) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: a %zu-byte reply in %u pieces cannot "
                         "be offered to a v1 backend, which writes only the "
                         "last one\n",
                         resp->len, resp->nents);
    return -EMSGSIZE;
  }

  r = kzalloc(sizeof(*r), GFP_KERNEL);
  if (!r)
    return -ENOMEM;
  r->dev = dev;
  r->req = req;
  r->resp = resp;
  init_completion(&r->done);
  INIT_WORK(&r->work, nvgpu_req_reap_work);

  /* A fresh non-zero id in every request; the backend echoes it. */
  do {
    id = (u32)atomic_inc_return(&xf->next_id);
  } while (!id);
  if (req_id)
    *req_id = id;
  nvgpu_tbuf_read(req, 0, &hdr, sizeof(hdr));
  hdr.req_id = cpu_to_le32(id);
  nvgpu_tbuf_write(req, 0, &hdr, sizeof(hdr));
  /* Zeroed, so a field the device did not write reads as zero, not as
   * whatever the page held before. */
  nvgpu_tbuf_zero(resp, 0, resp->len);

  sgs[0] = req->sg;
  sgs[1] = resp->sg;

  /*
   * Executor-class requests can sit on the ring for seconds (a blocking
   * commit, a modeset). Cap the slots they hold between them at half the ring
   * less a margin, so inline requests -- RM calls, CLOSE, HOST_OP -- always
   * find room.
   */
  if (flags & NVGPU_XF_EXECUTOR) {
    bool took = false;

    units = xf->indirect ? 1 : req->nents + resp->nents;
    units = min_t(u32, units, xf->exec_budget);
    /* The condition's last evaluation is the one that ended the wait, so
     * `took` says whether units are held -- never on an error return. */
    if (wait_event_killable(xf->exec_wq,
                            (took = nvgpu_exec_take(xf, units)) ||
                                READ_ONCE(xf->dead))) {
      units = 0;
      ret = -EINTR;
      goto unsent;
    }
    if (!took) {
      units = 0;
      ret = -ENODEV;
      goto unsent;
    }
    r->exec_units = units;
  }

  /*
   * A full ring is a reason to wait, never to fail: requests that hold their
   * slots for seconds are normal now. GFP_ATOMIC because the indirect table,
   * if there is one, is allocated under the lock; if that allocation fails
   * the ring falls back to direct slots, and max_sg keeps that possible.
   */
  for (;;) {
    unsigned long gen;

    spin_lock_irqsave(&xf->lock, irqf);
    if (xf->dead) {
      spin_unlock_irqrestore(&xf->lock, irqf);
      ret = -ENODEV;
      goto unsent_units;
    }
    t0 = ktime_get_ns();
    ret = virtqueue_add_sgs(dev->ctrl_vq, sgs, 1, 1, r, GFP_ATOMIC);
    if (!ret) {
      kick = virtqueue_kick_prepare(dev->ctrl_vq);
      spin_unlock_irqrestore(&xf->lock, irqf);
      break;
    }
    gen = xf->space_gen;
    spin_unlock_irqrestore(&xf->lock, irqf);

    if (ret != -ENOSPC)
      goto unsent_units;
    if (wait_event_killable(xf->space_wq,
                            READ_ONCE(xf->space_gen) != gen ||
                                READ_ONCE(xf->dead))) {
      ret = -EINTR;
      goto unsent_units;
    }
  }

  if (sent)
    *sent = true;
  if (kick)
    virtqueue_notify(dev->ctrl_vq);

  if (!(flags & NVGPU_XF_EXECUTOR)) {
    u32 spin_us = READ_ONCE(nvgpu_rt_spin_us);

    if (spin_us) {
      u64 until = ktime_get_ns() + (u64)min(spin_us, 1000u) * NSEC_PER_USEC;

      /* Replies are taken off the ring here while this caller spins, the
       * interrupt off meanwhile (nvgpu_ctrl_poll_enter()). */
      nvgpu_ctrl_poll_enter(dev);
      while (!completion_done(&r->done) && !need_resched() &&
             ktime_get_ns() < until) {
        nvgpu_ctrl_poll(dev);
        cpu_relax();
      }
      nvgpu_ctrl_poll_exit(dev);
    }
  }

  wret = wait_for_completion_killable_timeout(
      &r->done, (flags & NVGPU_XF_EXECUTOR) ? NVGPU_EXECUTOR_TIMEOUT
                                            : NVGPU_INLINE_TIMEOUT);
  if (wret <= 0) {
    spin_lock_irqsave(&xf->lock, irqf);
    if (!r->completed) {
      /* Ours no longer: the callback reaps and frees it when it comes back. */
      r->abandoned = true;
      __module_get(THIS_MODULE);
      spin_unlock_irqrestore(&xf->lock, irqf);
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: request %u (msg_type %u) %s; its "
                           "buffers stay with the transport until the device "
                           "returns them\n",
                           id, le32_to_cpu(hdr.msg_type),
                           wret ? "abandoned on a fatal signal"
                                : "timed out");
      return wret ? -EINTR : -ETIMEDOUT;
    }
    spin_unlock_irqrestore(&xf->lock, irqf);
  }

  ret = r->dead ? -ENODEV : 0;
  if (!ret)
    nvgpu_pace_call(le32_to_cpu(hdr.msg_type), t0, r->t1, ktime_get_ns());
  /*
   * The backend echoes the id (0 only in the bare header of a transport
   * refusal, which answers a request it could not read). It is not used to
   * route anything -- the context is the token -- but a different one here
   * means a reply landed in the wrong request's buffers, which is worth a
   * line in the log before it is worth a corrupted ioctl. The file streams
   * are the two replies with no header to look in.
   */
  if (!ret && min_t(u32, r->used_len, resp->len) >= sizeof(hdr) &&
      le32_to_cpu(hdr.msg_type) != NVGPU_MSG_GET_PROC_FILES &&
      le32_to_cpu(hdr.msg_type) != NVGPU_MSG_GET_SYS_FILES) {
    struct nvgpu_msg_hdr ah;

    if (!nvgpu_tbuf_read(resp, 0, &ah, sizeof(ah)) && ah.req_id &&
        le32_to_cpu(ah.req_id) != id && dev->v2)
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: request %u (msg_type %u) was "
                           "answered as request %u\n",
                           id, le32_to_cpu(hdr.msg_type),
                           le32_to_cpu(ah.req_id));
  }
  if (used_len)
    *used_len = min_t(u32, r->used_len, resp->len);
  if (tm) {
    tm->t0 = t0;
    tm->t1 = r->t1;
  }
  kfree(r);
  return ret;

unsent_units:
  nvgpu_exec_release(xf, units);
unsent:
  kfree(r);
  /* -EINTR hands the buffers to the transport, sent or not; see nvgpu.h.
   * Unsent, the backend never saw the request, so what it would have
   * consumed is released here, as the reaper does for a refused one. */
  if (ret == -EINTR) {
    nvgpu_release_consumed(dev, req);
    nvgpu_tbuf_free(req);
    nvgpu_tbuf_free(resp);
  }
  return ret;
}

int nvgpu_xfer(struct nvgpu_device *dev, struct nvgpu_tbuf *req,
               struct nvgpu_tbuf *resp, u32 flags, u32 *used_len) {
  return __nvgpu_xfer(dev, req, resp, flags, used_len, NULL, NULL, NULL);
}

/*
 * A request from plain kernel memory: copied into transport buffers so the
 * caller's memory is never on the ring and can be freed whatever happens.
 */
static int nvgpu_call_holding(struct nvgpu_device *dev, const void *req,
                              size_t req_len, void *resp, size_t resp_len,
                              u32 flags, u32 *used_len, struct nvgpu_times *tm,
                              bool *sent, u32 *req_id,
                              void (*release)(void *arg), void *arg) {
  struct nvgpu_xfer *xf = dev->xfer;
  struct nvgpu_tbuf *rq, *rs;
  size_t posted = resp_len;
  u32 used = 0;
  int ret;

  if (sent)
    *sent = false;
  if (req_id)
    *req_id = 0;
  if (used_len)
    *used_len = 0;
  if (!xf) {
    if (release)
      release(arg);
    return -ENODEV;
  }

  /* Nothing past what the backend will write is worth posting. */
  posted = min_t(size_t, posted, dev->v2 ? dev->max_resp : NVGPU_V1_RESP_MAX);
  rq = nvgpu_tbuf_alloc_sg(req_len, GFP_KERNEL, xf->max_sg);
  rs = nvgpu_tbuf_alloc_sg(posted, GFP_KERNEL, dev->v2 ? xf->max_sg : 1);
  if (release) {
    if (rq)
      nvgpu_tbuf_on_free(rq, release, arg);
    else
      release(arg);
  }
  if (!rq || !rs) {
    ret = -ENOMEM;
    goto out;
  }
  nvgpu_tbuf_write(rq, 0, req, req_len);

  ret = __nvgpu_xfer(dev, rq, rs, flags, &used, tm, sent, req_id);
  if (ret == -ETIMEDOUT || ret == -EINTR)
    return ret; /* the transport owns rq and rs now */
  if (!ret) {
    nvgpu_tbuf_read(rs, 0, resp, used);
    memset((u8 *)resp + used, 0, resp_len - used);
    if (used_len)
      *used_len = used;
  }
out:
  nvgpu_tbuf_free(rq);
  nvgpu_tbuf_free(rs);
  return ret;
}

static int nvgpu_call(struct nvgpu_device *dev, const void *req,
                      size_t req_len, void *resp, size_t resp_len, u32 flags,
                      u32 *used_len, struct nvgpu_times *tm, bool *sent) {
  return nvgpu_call_holding(dev, req, req_len, resp, resp_len, flags,
                            used_len, tm, sent, NULL, NULL, NULL);
}

int nvgpu_send_recv_used(struct nvgpu_device *dev, void *req, int req_len,
                         void *resp, int resp_len, u32 *used_len) {
  if (req_len <= 0 || resp_len <= 0)
    return -EINVAL;
  return nvgpu_call(dev, req, req_len, resp, resp_len, 0, used_len, NULL,
                    NULL);
}

int nvgpu_send_recv_holding(struct nvgpu_device *dev, void *req, int req_len,
                            void *resp, int resp_len, u32 *used_len,
                            void (*release)(void *arg), void *arg) {
  if (req_len <= 0 || resp_len <= 0) {
    release(arg);
    return -EINVAL;
  }
  return nvgpu_call_holding(dev, req, req_len, resp, resp_len, 0, used_len,
                            NULL, NULL, NULL, release, arg);
}

int nvgpu_send_recv_sent(struct nvgpu_device *dev, void *req, int req_len,
                         void *resp, int resp_len, u32 *used_len, bool *sent,
                         u32 *req_id) {
  if (req_len <= 0 || resp_len <= 0) {
    *sent = false;
    *req_id = 0;
    return -EINVAL;
  }
  return nvgpu_call_holding(dev, req, req_len, resp, resp_len, 0, used_len,
                            NULL, sent, req_id, NULL, NULL);
}

int nvgpu_send_recv(struct nvgpu_device *dev, void *req, int req_len,
                    void *resp, int resp_len) {
  u32 used;
  int ret = nvgpu_send_recv_used(dev, req, req_len, resp, resp_len, &used);

  if (!ret && used < sizeof(struct nvgpu_msg_hdr))
    ret = -EIO;
  return ret;
}

/* Status of a reply that must at least carry a header. */
static int nvgpu_hdr_status(const void *resp, u32 used) {
  const struct nvgpu_msg_hdr *h = resp;

  if (used < sizeof(*h))
    return -EIO;
  return (s32)le32_to_cpu(h->status);
}

/* ───────── CLOSE, GEM_CLOSE and their async twins ───────── */

/* What a queued close takes back. */
enum nvgpu_close_what {
  NVGPU_CLOSE_HANDLE, /* CLOSE `handle` */
  NVGPU_CLOSE_GEM,    /* GEM_CLOSE `id` in the file `handle` */
  NVGPU_CLOSE_MUNMAP, /* MUNMAP mapping `id`, made through `handle` */
  NVGPU_ARM_READY,    /* WATCH `handle` with NVGPU_W_ARM (not a close) */
};

struct nvgpu_close_work {
  struct work_struct work;
  struct nvgpu_device *dev;
  u32 handle; /* the handle to close, or the file the GEM handle is in */
  u32 id;     /* GEM handle or mapping id */
  enum nvgpu_close_what what;
  /* Run once the host can no longer act on the close: answered, abandoned
   * and then answered or reset, or never sent at all. Process context. */
  void (*release)(void *arg);
  void *arg;
};

static bool nvgpu_queue_close(struct nvgpu_device *dev, u32 handle, u32 id,
                              enum nvgpu_close_what what,
                              void (*release)(void *arg), void *arg);

static int __nvgpu_close_handle(struct nvgpu_device *dev, u32 handle,
                                bool fallback) {
  struct nvgpu_msg_hdr req = {}, resp;
  bool sent;
  u32 used;
  int ret;

  if (!handle)
    return 0;
  req.msg_type = cpu_to_le32(NVGPU_MSG_CLOSE);
  req.handle = cpu_to_le32(handle);
  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used,
                   NULL, &sent);
  if (!sent && fallback && ret != -ENODEV)
    nvgpu_queue_close(dev, handle, 0, NVGPU_CLOSE_HANDLE, NULL, NULL);
  return ret ? ret : nvgpu_hdr_status(&resp, used);
}

int nvgpu_close_handle(struct nvgpu_device *dev, u32 handle) {
  return __nvgpu_close_handle(dev, handle, true);
}

/* struct drm_gem_close, forwarded as a v1 IOCTL on the file that holds it. */
struct nvgpu_gem_close_msg {
  struct nvgpu_ioctl_req io;
  struct drm_gem_close arg;
} __packed;

struct nvgpu_gem_close_reply {
  struct nvgpu_ioctl_resp io;
  struct drm_gem_close arg;
} __packed;

/*
 * With `release`, which runs once the host can no longer act on the close
 * (see nvgpu_call_holding()), whatever happens. Only the queued close passes
 * one, and it has no fallback: a fallback would queue a close whose release
 * had already run with the unsent request.
 */
static int __nvgpu_gem_close(struct nvgpu_device *dev, u32 file, u32 gem,
                             bool fallback, void (*release)(void *arg),
                             void *arg) {
  struct nvgpu_gem_close_msg req = {};
  struct nvgpu_gem_close_reply resp;
  bool sent;
  u32 used;
  int ret;

  if (!file || !gem) {
    if (release)
      release(arg);
    return 0;
  }
  req.io.hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req.io.hdr.handle = cpu_to_le32(file);
  req.io.cmd = cpu_to_le32(DRM_IOCTL_GEM_CLOSE);
  req.io.data_len = cpu_to_le32(sizeof(req.arg));
  req.arg.handle = gem;
  ret = nvgpu_call_holding(dev, &req, sizeof(req), &resp, sizeof(resp), 0,
                           &used, NULL, &sent, NULL, release, arg);
  if (!sent && fallback && ret != -ENODEV)
    nvgpu_queue_close(dev, file, gem, NVGPU_CLOSE_GEM, NULL, NULL);
  return ret ? ret : nvgpu_hdr_status(&resp, used);
}

int nvgpu_gem_close(struct nvgpu_device *dev, u32 file_handle, u32 gem) {
  return __nvgpu_gem_close(dev, file_handle, gem, true, NULL, NULL);
}

/*
 * A window placement handed back. The backend counts one reference per MMAP
 * reply that named the placement, so this must be sent exactly once per such
 * reply -- a lost one leaks a slice of the window for the session, and a
 * second one takes a reference somebody else holds.
 */
static int __nvgpu_munmap(struct nvgpu_device *dev, u32 handle, u32 mapping_id,
                          bool fallback) {
  struct nvgpu_munmap_req req = {};
  struct nvgpu_munmap_resp resp;
  bool sent;
  u32 used;
  int ret;

  if (!mapping_id)
    return 0;
  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_MUNMAP);
  req.hdr.handle = cpu_to_le32(handle);
  req.mapping_id = cpu_to_le32(mapping_id);
  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used,
                   NULL, &sent);
  if (!sent && fallback && ret != -ENODEV)
    nvgpu_queue_close(dev, handle, mapping_id, NVGPU_CLOSE_MUNMAP, NULL, NULL);
  return ret ? ret : nvgpu_hdr_status(&resp, used);
}

int nvgpu_munmap(struct nvgpu_device *dev, u32 handle, u32 mapping_id) {
  return __nvgpu_munmap(dev, handle, mapping_id, true);
}

static void nvgpu_close_work_fn(struct work_struct *work) {
  struct nvgpu_close_work *cw =
      container_of(work, struct nvgpu_close_work, work);

  switch (cw->what) {
  case NVGPU_CLOSE_HANDLE:
    __nvgpu_close_handle(cw->dev, cw->handle, false);
    break;
  case NVGPU_CLOSE_GEM:
    __nvgpu_gem_close(cw->dev, cw->handle, cw->id, false, cw->release,
                      cw->arg);
    break;
  case NVGPU_CLOSE_MUNMAP:
    __nvgpu_munmap(cw->dev, cw->handle, cw->id, false);
    break;
  case NVGPU_ARM_READY:
    /* A handle closed meanwhile answers EBADF, which is fine. */
    nvgpu_watch(cw->dev, cw->handle, NVGPU_W_ARM, 0);
    break;
  }
  kfree(cw);
  module_put(THIS_MODULE);
}

/*
 * Queued rather than sent: callable from interrupt context (a fence's last
 * put, an event consumer) and from paths that must not wait. The item holds a
 * module reference so the code it runs cannot be unloaded under it; remove()
 * drains the queue before the device goes, and refuses new items after.
 * `release` (NVGPU_CLOSE_GEM only) runs here when the close is never queued,
 * so a caller passing one must be in process context.
 */
static bool nvgpu_queue_close(struct nvgpu_device *dev, u32 handle, u32 id,
                              enum nvgpu_close_what what,
                              void (*release)(void *arg), void *arg) {
  struct nvgpu_xfer *xf = dev->xfer;
  struct nvgpu_close_work *cw;
  unsigned long flags;

  if (!xf || !handle)
    goto unqueued;
  cw = kmalloc(sizeof(*cw), GFP_ATOMIC);
  if (!cw) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: no memory to release backend handle "
                         "%u (%s %u); it stays until the session resets\n",
                         handle,
                         what == NVGPU_CLOSE_MUNMAP ? "mapping" : "gem", id);
    goto unqueued;
  }
  INIT_WORK(&cw->work, nvgpu_close_work_fn);
  cw->dev = dev;
  cw->handle = handle;
  cw->id = id;
  cw->what = what;
  cw->release = release;
  cw->arg = arg;

  spin_lock_irqsave(&xf->lock, flags);
  if (xf->dead) {
    spin_unlock_irqrestore(&xf->lock, flags);
    kfree(cw);
    goto unqueued;
  }
  __module_get(THIS_MODULE);
  queue_work(xf->wq, &cw->work);
  spin_unlock_irqrestore(&xf->lock, flags);
  return true;

unqueued:
  /* Nothing will send it now, so nothing will act on it either. */
  if (release)
    release(arg);
  return false;
}

void nvgpu_close_handle_async(struct nvgpu_device *dev, u32 handle) {
  nvgpu_queue_close(dev, handle, 0, NVGPU_CLOSE_HANDLE, NULL, NULL);
}

bool nvgpu_arm_ready_async(struct nvgpu_device *dev, u32 handle) {
  return nvgpu_queue_close(dev, handle, 0, NVGPU_ARM_READY, NULL, NULL);
}

void nvgpu_gem_close_async(struct nvgpu_device *dev, u32 file_handle,
                           u32 gem) {
  if (gem)
    nvgpu_queue_close(dev, file_handle, gem, NVGPU_CLOSE_GEM, NULL, NULL);
}

void nvgpu_gem_close_then(struct nvgpu_device *dev, u32 file_handle, u32 gem,
                          void (*release)(void *arg), void *arg) {
  nvgpu_queue_close(dev, file_handle, gem, NVGPU_CLOSE_GEM, release, arg);
}

/* ───────── Replies nobody waited for ───────── */

/*
 * IOCTL2 descriptors the caller handed over to be consumed (NVGPU_I2_FD_CONSUME:
 * a fence unwrap that made a temporary). The backend closes them only when
 * the call runs; for one that never did -- refused, cancelled by a reset, or
 * never put on the ring -- they are still open there and nobody else knows.
 * Queued rather than sent: the unsent case runs with a fatal signal pending,
 * where a synchronous request would be abandoned before it was made.
 */
static unsigned int nvgpu_release_consumed(struct nvgpu_device *dev,
                                           const struct nvgpu_tbuf *req) {
  const size_t base = sizeof(struct nvgpu_msg_hdr);
  struct nvgpu_msg_hdr qh;
  struct nvgpu_i2_req q;
  u32 nbuf, nfd, i;
  size_t at;
  unsigned int n = 0;

  if (nvgpu_tbuf_read(req, 0, &qh, sizeof(qh)) ||
      le32_to_cpu(qh.msg_type) != NVGPU_MSG_IOCTL2 ||
      nvgpu_tbuf_read(req, base, &q, sizeof(q)))
    return 0;
  /* Our own request, so the counts are ours; bounded all the same. */
  nbuf = min_t(u32, le32_to_cpu(q.nbuf), NVGPU_I2_MAX_BUFS);
  nfd = min_t(u32, le32_to_cpu(q.nfd), NVGPU_I2_MAX_RECS);
  at = base + sizeof(q) + (size_t)nbuf * sizeof(__le32);
  for (i = 0; i < nfd; i++, at += sizeof(struct nvgpu_i2_fd_in)) {
    struct nvgpu_i2_fd_in fi;

    if (nvgpu_tbuf_read(req, at, &fi, sizeof(fi)))
      break;
    if (le32_to_cpu(fi.flags) & NVGPU_I2_FD_CONSUME) {
      nvgpu_close_handle_async(dev, le32_to_cpu(fi.handle));
      n++;
    }
  }
  return n;
}

/* IOCTL2: every descriptor the host produced, and every GEM handle it made in
 * the caller's render file. */
static unsigned int nvgpu_reap_ioctl2(struct nvgpu_device *dev,
                                      struct nvgpu_req *r, u32 used) {
  const size_t base = sizeof(struct nvgpu_msg_hdr);
  struct nvgpu_i2_resp ir;
  __le32 render_le;
  u32 nfd, ngem, render, i;
  size_t fds, gems;
  unsigned int n = 0;

  if (!nvgpu_resp_has(used, base, sizeof(ir)) ||
      nvgpu_tbuf_read(r->resp, base, &ir, sizeof(ir)) ||
      nvgpu_tbuf_read(r->req, base + offsetof(struct nvgpu_i2_req, render),
                      &render_le, sizeof(render_le)))
    return 0;

  nfd = min_t(u32, le32_to_cpu(ir.nfd), NVGPU_I2_MAX_RECS);
  ngem = min_t(u32, le32_to_cpu(ir.ngem), NVGPU_I2_MAX_RECS);
  render = le32_to_cpu(render_le);
  fds = base + sizeof(ir) + le32_to_cpu(ir.data_len);
  gems = fds + (size_t)nfd * sizeof(struct nvgpu_i2_fd_out);

  for (i = 0; i < nfd; i++) {
    struct nvgpu_i2_fd_out fo;
    size_t at = fds + (size_t)i * sizeof(fo);

    if (!nvgpu_resp_has(used, at, sizeof(fo)) ||
        nvgpu_tbuf_read(r->resp, at, &fo, sizeof(fo)))
      break;
    __nvgpu_close_handle(dev, le32_to_cpu(fo.handle), true);
    n++;
  }
  for (i = 0; i < ngem; i++) {
    struct nvgpu_i2_gem_out go;
    size_t at = gems + (size_t)i * sizeof(go);

    if (!nvgpu_resp_has(used, at, sizeof(go)) ||
        nvgpu_tbuf_read(r->resp, at, &go, sizeof(go)))
      break;
    /* A re-home can return a handle the file had: a proxy's (S-11). */
    if (nvgpu_gem_handle_held(dev, render, le32_to_cpu(go.gem)))
      continue;
    __nvgpu_gem_close(dev, render, le32_to_cpu(go.gem), true, NULL, NULL);
    n++;
  }
  return n;
}

/* HOST_OP: which results are handles the backend now holds for us. */
static unsigned int nvgpu_reap_host_op(struct nvgpu_device *dev,
                                       struct nvgpu_req *r, u32 used) {
  const size_t base = sizeof(struct nvgpu_msg_hdr);
  struct nvgpu_host_op_req q;
  struct nvgpu_host_op_resp a;
  u64 res0;

  if (!nvgpu_resp_has(used, base, sizeof(a)) ||
      nvgpu_tbuf_read(r->resp, base, &a, sizeof(a)) ||
      nvgpu_tbuf_read(r->req, base, &q, sizeof(q)) || !le32_to_cpu(a.nres))
    return 0;
  res0 = le64_to_cpu(a.res[0]);

  switch (le32_to_cpu(q.op)) {
  case NVGPU_OP_PRIME_EXPORT:
  case NVGPU_OP_SYNC_MERGE:
  case NVGPU_OP_NEW_EVENTFD:
  case NVGPU_OP_SIGNALED_SYNC_FILE:
  case NVGPU_OP_OPEN_KMS:
    __nvgpu_close_handle(dev, (u32)res0, true);
    return 1;
  case NVGPU_OP_DMABUF_IMPORT:
  case NVGPU_OP_INJECT_OPEN:
    /* A GEM handle in the render file named by the first argument -- unless
     * the file already had one for the buffer, which the host then returns
     * (drm_prime.c:306-310), and that is a proxy's to close (S-11). */
    if (nvgpu_gem_handle_held(dev, (u32)le64_to_cpu(q.args[0]), (u32)res0))
      return 0;
    __nvgpu_gem_close(dev, (u32)le64_to_cpu(q.args[0]), (u32)res0, true,
                      NULL, NULL);
    return 1;
  default:
    return 0;
  }
}

/*
 * An OS-descriptor registration (NVGPU_DEEP_PAGE_LIST): which registration
 * the late reply names, 0 for none -- refused, failed, or a backend that
 * names none -- so the pins its waiter handed over are kept under it or let
 * go (nvgpu_osdesc_late()). The same reading as nvgpu_osdesc_register()'s.
 */
static unsigned int nvgpu_reap_osdesc(struct nvgpu_device *dev,
                                      struct nvgpu_req *r, u32 used,
                                      const struct nvgpu_msg_hdr *ah) {
  struct nvgpu_ioctl_req q;
  struct nvgpu_ioctl_resp a;
  u32 data_len, nested_len;
  __le64 id = 0;

  if (nvgpu_tbuf_read(r->req, 0, &q, sizeof(q)) ||
      le32_to_cpu(q.deep_ptr_offset) != NVGPU_DEEP_PAGE_LIST)
    return 0;
  if ((s32)le32_to_cpu(ah->status) >= 0 &&
      nvgpu_resp_has(used, 0, sizeof(a)) &&
      !nvgpu_tbuf_read(r->resp, 0, &a, sizeof(a))) {
    data_len = le32_to_cpu(a.data_len);
    nested_len = data_len ? le32_to_cpu(a.nested_len) : 0;
    if (data_len && le32_to_cpu(a.deep_len) == sizeof(id) &&
        nvgpu_resp_has(used, sizeof(a) + (size_t)data_len + nested_len,
                       sizeof(id)) &&
        nvgpu_tbuf_read(r->resp, sizeof(a) + (size_t)data_len + nested_len,
                        &id, sizeof(id)))
      id = 0;
  }
  nvgpu_osdesc_late(dev, le32_to_cpu(q.hdr.req_id), le64_to_cpu(id));
  return 1;
}

/*
 * A reply whose caller gave up. Whatever it created on the backend has no
 * owner in the guest and would stay open until the session resets -- for a
 * CREATE_LEASE, that is a host lessee holding a CRTC nobody can lease again.
 * So everything it names is closed here, from process context.
 */
static void nvgpu_req_reap(struct nvgpu_req *r) {
  struct nvgpu_device *dev = r->dev;
  u32 used = min_t(u32, r->used_len, r->resp->len);
  struct nvgpu_msg_hdr qh, ah;
  unsigned int closed = 0;

  if (r->dead || !nvgpu_resp_has(used, 0, sizeof(ah)) ||
      nvgpu_tbuf_read(r->resp, 0, &ah, sizeof(ah)) ||
      nvgpu_tbuf_read(r->req, 0, &qh, sizeof(qh)))
    return;
  /*
   * A registration of the caller's pages, answered after its waiter left: the
   * pins it kept for it are this reply's to settle (nvgpu_osdesc.c). Before
   * the status, which says whether RM ever saw it.
   */
  if (le32_to_cpu(qh.msg_type) == NVGPU_MSG_IOCTL)
    closed += nvgpu_reap_osdesc(dev, r, used, &ah);
  if ((s32)le32_to_cpu(ah.status) < 0) {
    /* Refused before it ran -- the backend sets a status on nothing else,
     * bar a call whose session a reset already emptied: nothing was made,
     * but an IOCTL2's consumed handles are still open there. */
    closed += nvgpu_release_consumed(dev, r->req);
    goto out;
  }

  switch (le32_to_cpu(qh.msg_type)) {
  case NVGPU_MSG_OPEN:
    __nvgpu_close_handle(dev, le32_to_cpu(ah.handle), true);
    closed = 1;
    break;
  case NVGPU_MSG_MMAP: {
    struct nvgpu_mmap_resp m;

    if (nvgpu_resp_has(used, 0, sizeof(m)) &&
        !nvgpu_tbuf_read(r->resp, 0, &m, sizeof(m)) &&
        le32_to_cpu(m.mapping_id)) {
      nvgpu_munmap(dev, le32_to_cpu(qh.handle), le32_to_cpu(m.mapping_id));
      closed = 1;
    }
    break;
  }
  case NVGPU_MSG_IOCTL2:
    closed = nvgpu_reap_ioctl2(dev, r, used);
    break;
  case NVGPU_MSG_HOST_OP:
    closed = nvgpu_reap_host_op(dev, r, used);
    break;
  case NVGPU_MSG_WL_RECV:
    closed = nvgpu_wl_reap_recv(dev, r->resp, used);
    break;
  default:
    break;
  }

out:
  if (closed)
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: request %u (msg_type %u) came back "
                         "after its caller left; released %u thing(s) it made\n",
                         le32_to_cpu(qh.req_id), le32_to_cpu(qh.msg_type),
                         closed);
}

static void nvgpu_req_free_orphan(struct nvgpu_req *r) {
  nvgpu_tbuf_free(r->req);
  nvgpu_tbuf_free(r->resp);
  kfree(r);
  module_put(THIS_MODULE); /* taken when the waiter abandoned it */
}

static void nvgpu_req_reap_work(struct work_struct *work) {
  struct nvgpu_req *r = container_of(work, struct nvgpu_req, work);

  nvgpu_req_reap(r);
  nvgpu_req_free_orphan(r);
}

/* ───────── HOST_OP, WATCH, UNWATCH ───────── */

int nvgpu_host_op(struct nvgpu_device *dev, u32 op, const u64 *args,
                  u32 nargs, u64 *res, u32 nres) {
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_host_op_req body;
    struct nvgpu_proc_id proc;
  } __packed req = {};
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_host_op_resp body;
  } __packed resp;
  u32 used, got, i, req_len = sizeof(req);
  int ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  if (nargs > NVGPU_OP_MAX_ARGS || nres > NVGPU_OP_MAX_RES)
    return -EINVAL;

  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_HOST_OP);
  req.body.op = cpu_to_le32(op);
  req.body.nargs = cpu_to_le32(nargs);
  for (i = 0; i < nargs; i++)
    req.body.args[i] = cpu_to_le64(args[i]);
  /* The caller, whose share what the op makes counts against (quota.rs). */
  if (nvgpu_proc_ids(dev))
    nvgpu_proc_id_fill(dev, &req.proc);
  else
    req_len -= sizeof(req.proc);

  ret = nvgpu_call(dev, &req, req_len, &resp, sizeof(resp), 0, &used,
                   NULL, NULL);
  if (ret)
    return ret;
  ret = nvgpu_hdr_status(&resp, used);
  if (ret < 0)
    return ret;
  if (!nvgpu_resp_has(used, 0, sizeof(resp)))
    return -EIO;

  got = min_t(u32, le32_to_cpu(resp.body.nres), NVGPU_OP_MAX_RES);
  for (i = 0; i < nres; i++)
    res[i] = i < got ? le64_to_cpu(resp.body.res[i]) : 0;
  return 0;
}

int nvgpu_host_op_tail(struct nvgpu_device *dev, u32 op, const u64 *args,
                       u32 nargs, u64 *res, u32 nres, void *tail,
                       u32 tail_len, u32 *tail_used) {
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_host_op_req body;
    struct nvgpu_proc_id proc;
  } __packed req = {};
  const size_t fixed =
      sizeof(struct nvgpu_msg_hdr) + sizeof(struct nvgpu_host_op_resp);
  const struct nvgpu_host_op_resp *a;
  u32 used, got, i, req_len = sizeof(req);
  u8 *resp;
  int ret;

  *tail_used = 0;
  if (!dev->v2)
    return -EOPNOTSUPP;
  if (nargs > NVGPU_OP_MAX_ARGS || nres > NVGPU_OP_MAX_RES ||
      tail_len > PAGE_SIZE)
    return -EINVAL;
  resp = kzalloc(fixed + tail_len, GFP_KERNEL);
  if (!resp)
    return -ENOMEM;

  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_HOST_OP);
  req.body.op = cpu_to_le32(op);
  req.body.nargs = cpu_to_le32(nargs);
  for (i = 0; i < nargs; i++)
    req.body.args[i] = cpu_to_le64(args[i]);
  if (nvgpu_proc_ids(dev))
    nvgpu_proc_id_fill(dev, &req.proc);
  else
    req_len -= sizeof(req.proc);

  ret = nvgpu_call(dev, &req, req_len, resp, fixed + tail_len, 0, &used,
                   NULL, NULL);
  if (ret)
    goto out;
  ret = nvgpu_hdr_status(resp, used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, fixed)) {
    ret = -EIO;
    goto out;
  }
  a = (const void *)(resp + sizeof(struct nvgpu_msg_hdr));
  got = min_t(u32, le32_to_cpu(a->nres), NVGPU_OP_MAX_RES);
  for (i = 0; i < nres; i++)
    res[i] = i < got ? le64_to_cpu(a->res[i]) : 0;
  *tail_used = min_t(u32, used - fixed, tail_len);
  memcpy(tail, resp + fixed, *tail_used);
  ret = 0;
out:
  kfree(resp);
  return ret;
}

int nvgpu_watch(struct nvgpu_device *dev, u32 handle, u32 flags, u64 cookie) {
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_watch_req body;
  } __packed req = {};
  struct nvgpu_msg_hdr resp;
  u32 used;
  int ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_WATCH);
  req.hdr.handle = cpu_to_le32(handle);
  req.body.handle = cpu_to_le32(handle);
  req.body.flags = cpu_to_le32(flags);
  req.body.cookie = cpu_to_le64(cookie);
  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used,
                   NULL, NULL);
  return ret ? ret : nvgpu_hdr_status(&resp, used);
}

int nvgpu_unwatch(struct nvgpu_device *dev, u32 handle) {
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_unwatch_req body;
  } __packed req = {};
  struct nvgpu_msg_hdr resp;
  u32 used;
  int ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_UNWATCH);
  req.hdr.handle = cpu_to_le32(handle);
  req.body.handle = cpu_to_le32(handle);
  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used,
                   NULL, NULL);
  return ret ? ret : nvgpu_hdr_status(&resp, used);
}

/* ───────── The host's clock ───────── */

/*
 * Where host timestamps land in the guest's CLOCK_MONOTONIC.
 *
 * Vblank and flip events carry the host's time, and a compositor paces frames
 * by them, so an offset that jumps makes presentation times go backwards.
 * Each resync therefore sets a target, and the offset in use walks from where
 * it was towards it at no more than 50 us per second; two host timestamps a
 * frame apart can then never be translated out of order.
 */
static s64 nvgpu_slew(s64 base, s64 target, u64 anchor, u64 now) {
  s64 step = (s64)div_u64(now > anchor ? now - anchor : 0,
                          NVGPU_TIME_SLEW_DIV);
  s64 d = target - base;

  if (d > step)
    return base + step;
  if (d < -step)
    return base - step;
  return target;
}

static s64 nvgpu_clk_offset(struct nvgpu_xfer *xf, u64 now) {
  s64 base, target;
  unsigned int seq;
  u64 anchor;
  bool valid;

  do {
    seq = read_seqbegin(&xf->clk_lock);
    valid = xf->clk_valid;
    base = xf->off_base;
    target = xf->off_target;
    anchor = xf->clk_anchor;
  } while (read_seqretry(&xf->clk_lock, seq));

  return valid ? nvgpu_slew(base, target, anchor, now) : 0;
}

s64 nvgpu_host_to_guest_ns(struct nvgpu_device *dev, s64 host_ns) {
  if (!dev->xfer)
    return host_ns;
  return host_ns - nvgpu_clk_offset(dev->xfer, ktime_get_ns());
}

/*
 * A host CLOCK_REALTIME or CLOCK_MONOTONIC_RAW reading, in the guest's own
 * clock of the same id. Through the monotonic clocks, whose offset is the
 * slewed one above: host clock -> host monotonic by the host's distance
 * between the two at the last sync, -> guest monotonic, -> guest clock by
 * the guest's distance now. Realtime and monotonic are not the same clock on
 * either side (settimeofday, NTP steps), so no monotonic offset is ever
 * applied to a realtime value directly. False without TIME_SYNC's long form
 * (an older backend), in which case the caller leaves the value alone.
 */
bool nvgpu_host_clock_to_guest(struct nvgpu_device *dev, clockid_t clk,
                               s64 host_ns, s64 *guest_ns) {
  struct nvgpu_xfer *xf = dev->xfer;
  s64 host_delta, guest_delta, mono;
  unsigned int seq;
  bool ext;

  if (!xf)
    return false;
  do {
    seq = read_seqbegin(&xf->clk_lock);
    ext = xf->clk_valid && xf->clk_ext;
    host_delta = clk == CLOCK_REALTIME ? xf->host_real_mono : xf->host_raw_mono;
  } while (read_seqretry(&xf->clk_lock, seq));
  if (!ext || (clk != CLOCK_REALTIME && clk != CLOCK_MONOTONIC_RAW))
    return false;

  mono = nvgpu_host_to_guest_ns(dev, host_ns - host_delta);
  guest_delta = clk == CLOCK_REALTIME ? ktime_get_real_ns() - ktime_get_ns()
                                      : ktime_get_raw_ns() - ktime_get_ns();
  *guest_ns = mono + guest_delta;
  return true;
}

s64 nvgpu_guest_to_host_ns(struct nvgpu_device *dev, s64 guest_ns) {
  if (!dev->xfer)
    return guest_ns;
  return guest_ns + nvgpu_clk_offset(dev->xfer, ktime_get_ns());
}

/*
 * One round trip. The backend stamps its clock just before it hands the
 * chain back, t0 is taken before the request is on the ring and t1 in the
 * callback -- not when the waiter wakes, which would add a scheduler wakeup to
 * one leg only and bias the midpoint by half of it.
 */
static int nvgpu_time_sample(struct nvgpu_device *dev, s64 *offset,
                             u64 *rtt, s64 *real_mono, s64 *raw_mono,
                             bool *ext) {
  struct nvgpu_msg_hdr req = {};
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_time_sync_resp2 body;
  } __packed resp;
  struct nvgpu_times tm;
  u32 used;
  int ret;

  req.msg_type = cpu_to_le32(NVGPU_MSG_TIME_SYNC);
  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used, &tm,
                   NULL);
  if (ret)
    return ret;
  ret = nvgpu_hdr_status(&resp, used);
  if (ret < 0)
    return ret;
  if (!nvgpu_resp_has(used, 0,
                      sizeof(resp.hdr) + sizeof(struct nvgpu_time_sync_resp)) ||
      tm.t1 < tm.t0)
    return -EIO;

  *rtt = tm.t1 - tm.t0;
  *offset = (s64)le64_to_cpu(resp.body.host_mono_ns) -
            (s64)(tm.t0 + *rtt / 2);
  /* The long form, from a backend that knows it: the used length says. */
  *ext = nvgpu_resp_has(used, 0, sizeof(resp));
  if (*ext) {
    *real_mono = (s64)le64_to_cpu(resp.body.host_realtime_ns) -
                 (s64)le64_to_cpu(resp.body.host_mono_ns);
    *raw_mono = (s64)le64_to_cpu(resp.body.host_mono_raw_ns) -
                (s64)le64_to_cpu(resp.body.host_mono_ns);
  }
  return 0;
}

/*
 * Eight samples, keeping the one with the shortest round trip: the tighter
 * the bracket, the less the midpoint can be off. A round trip over 200 us has
 * a vCPU preemption or a busy backend in it; if every sample is that bad the
 * best of them is still the best estimate there is, so it is used anyway.
 */
static int nvgpu_time_sync(struct nvgpu_device *dev, bool initial) {
  struct nvgpu_xfer *xf = dev->xfer;
  u64 best_rtt = U64_MAX, rtt, now;
  s64 best_off = 0, off, real_mono = 0, raw_mono = 0, best_real = 0,
      best_raw = 0;
  bool ext = false, best_ext = false;
  unsigned long flags;
  int i, ret = -EIO;

  for (i = 0; i < NVGPU_TIME_SAMPLES; i++) {
    ret = nvgpu_time_sample(dev, &off, &rtt, &real_mono, &raw_mono, &ext);
    if (ret)
      return ret;
    if (rtt < best_rtt) {
      best_rtt = rtt;
      best_off = off;
      best_ext = ext;
      best_real = real_mono;
      best_raw = raw_mono;
    }
  }
  if (best_rtt > NVGPU_TIME_RTT_GOOD_NS)
    dev_dbg(&dev->vdev->dev,
            "virtio-gpu-nv: clock sync's best round trip was %llu ns\n",
            best_rtt);

  write_seqlock_irqsave(&xf->clk_lock, flags);
  now = ktime_get_ns();
  if (initial || !xf->clk_valid) {
    /* Nothing has been translated yet, so a step cannot be seen. */
    xf->off_base = best_off;
  } else {
    xf->off_base = nvgpu_slew(xf->off_base, xf->off_target, xf->clk_anchor,
                              now);
  }
  xf->off_target = best_off;
  xf->clk_anchor = now;
  xf->clk_valid = true;
  xf->clk_ext = best_ext;
  xf->host_real_mono = best_real;
  xf->host_raw_mono = best_raw;
  write_sequnlock_irqrestore(&xf->clk_lock, flags);
  return 0;
}

static void nvgpu_time_sync_work(struct work_struct *work) {
  struct nvgpu_xfer *xf =
      container_of(to_delayed_work(work), struct nvgpu_xfer, sync_work);
  int ret = nvgpu_time_sync(xf->dev, false);

  /* A backend that stops knowing the message is not going to learn it. */
  if (ret == -EPROTO || ret == -ENODEV || READ_ONCE(xf->dead))
    return;
  queue_delayed_work(xf->wq, &xf->sync_work, NVGPU_TIME_RESYNC);
}

/* ───────── HELLO ───────── */

/*
 * Ask for protocol v2. HELLO_F_FRESH tells the backend this is a new driver
 * instance, so whatever an earlier one left open there -- a master card file,
 * a lease -- is closed now rather than surviving a guest reboot.
 *
 * An old backend answers -EPROTO, as it does to any message it does not know
 * (nvidia.rs:658-662), and the guest simply stays on v1: nothing in v2 is
 * needed for what v1 already does.
 */
void nvgpu_xfer_hello(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_hello_req body;
  } __packed req = {};
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_hello_resp body;
  } __packed resp;
  u32 used, ring_max, guest_caps;
  int ret;

  if (!xf)
    return;
  req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_HELLO);
  req.body.proto = cpu_to_le32(NVGPU_PROTO_V2);
  req.body.flags = cpu_to_le32(NVGPU_HELLO_F_FRESH);
  /*
   * Which process makes each RM call, and its euid (nvgpu_main.c,
   * nvgpu_proc_id_fill).
   */
  guest_caps = NVGPU_GCAP_PROC_ID | NVGPU_GCAP_PROC_EUID;
  /* Readiness of a device descriptor once per wait on it (nvgpu_poll_mask),
   * not once per host event: RM posts dozens a frame that nobody here
   * waits for. */
  if (nvgpu_arm_ready)
    guest_caps |= NVGPU_GCAP_ARMS_READY;
  /* The backend hands out aperture offsets in 2 MiB granules, so a smaller
   * aperture is none. */
  if (dev->uvm_aperture.len >= SZ_2M) {
    guest_caps |= NVGPU_GCAP_UVM_APERTURE;
    req.body.uvm_aperture_mib =
        cpu_to_le32(min_t(u64, dev->uvm_aperture.len >> 20, U32_MAX));
  }
  req.body.guest_caps = cpu_to_le32(guest_caps);

  ret = nvgpu_call(dev, &req, sizeof(req), &resp, sizeof(resp), 0, &used,
                   NULL, NULL);
  if (!ret)
    ret = nvgpu_hdr_status(&resp, used);
  if (ret == -EPROTO) {
    dev_info(&dev->vdev->dev,
             "virtio-gpu-nv: backend speaks protocol v1 only; no display "
             "features\n");
    return;
  }
  if (ret < 0 || !nvgpu_resp_has(used, 0, sizeof(resp)) ||
      le32_to_cpu(resp.body.proto) != NVGPU_PROTO_V2) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: HELLO failed (%d, %u bytes, proto %u); staying "
             "on protocol v1\n",
             ret, used,
             nvgpu_resp_has(used, 0, sizeof(resp))
                 ? le32_to_cpu(resp.body.proto)
                 : 0);
    return;
  }

  /* What the ring can describe bounds what the backend may offer. */
  ring_max = xf->max_sg * NVGPU_TBUF_CHUNK;
  dev->backend_caps = le32_to_cpu(resp.body.backend_caps);
  dev->max_req = min_not_zero(le32_to_cpu(resp.body.max_req), ring_max);
  dev->max_resp = min_not_zero(le32_to_cpu(resp.body.max_resp), ring_max);
  dev->num_cards = le32_to_cpu(resp.body.num_cards);
  /*
   * The IOCTL2 tables. The DRM ones are the same for every host; an NVKMS
   * table only when the backend has one for its driver too, because both
   * halves must walk the same table or every modeset call is refused (or
   * worse, gathered by a layout the backend does not have). Before this the
   * interpreter falls back to the DRM-only set, and it never runs before v2.
   */
  dev->schema = nvgpu_schema_select(
      dev->backend_caps & NVGPU_BCAP_NVKMS_TABLE ? dev->driver_version : NULL);
  dev->v2 = true;

  dev_info(&dev->vdev->dev,
           "virtio-gpu-nv: protocol v2, backend caps 0x%x, requests up to %u "
           "and replies up to %u bytes%s, %u card node(s)\n",
           dev->backend_caps, dev->max_req, dev->max_resp,
           xf->indirect ? " (indirect descriptors)" : "", dev->num_cards);

  ret = nvgpu_time_sync(dev, true);
  if (ret) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: TIME_SYNC failed (%d); host timestamps pass "
             "through untranslated\n",
             ret);
    return;
  }
  queue_delayed_work(xf->wq, &xf->sync_work, NVGPU_TIME_RESYNC);
}

/* ───────── Event queue ───────── */

/*
 * Legacy readiness: the host says a descriptor has something to report. Wake
 * whoever is waiting on it.
 *
 * `pending` is a flag rather than a count: what the waiter does on waking is
 * ask the hardware's own semaphore, so two events and one event mean the same
 * thing to it. A wake with nothing behind it costs a wasted poll, and the host
 * re-sends while the descriptor stays readable, so a lost one costs a
 * millisecond rather than a hang.
 */
static void nvgpu_event_deliver(struct nvgpu_device *dev, u32 handle) {
  struct nvgpu_fd *nfd;
  unsigned long flags;

  spin_lock_irqsave(&dev->fds_lock, flags);
  list_for_each_entry(nfd, &dev->fds, node) {
    if (nfd->handle == handle) {
      /* This report answers the arm; the next poll arms again. */
      atomic_set(&nfd->armed, 0);
      if (!atomic_xchg(&nfd->pending, 1))
        nvgpu_pace_inc(NVGPU_PACE_EV_LEGACY_SET);
      wake_up_interruptible(&nfd->wq);
      break;
    }
  }
  spin_unlock_irqrestore(&dev->fds_lock, flags);
}

/* Every consumer registered under `key`. Caller holds ev->lock. */
static void nvgpu_ev_call(struct nvgpu_events *ev, u64 key, u32 kind,
                          u64 cookie, const void *payload, u32 len) {
  struct nvgpu_ev_consumer *c;

  hash_for_each_possible(ev->consumers, c, node, key)
    if (c->key == key)
      c->deliver(c, kind, cookie, payload, len);
}

/* One record. Caller holds ev->lock; hard IRQ context. */
static void nvgpu_ev_record(struct nvgpu_device *dev, u32 kind, u64 cookie,
                            const void *payload, u32 len) {
  struct nvgpu_events *ev = dev->events;

  nvgpu_pace_inc(NVGPU_PACE_EV_RECORDS);
  switch (kind) {
  case NVGPU_EV_DRM:
    nvgpu_ev_call(ev, NVGPU_EVKEY_HANDLE(cookie), kind, cookie, payload, len);
    break;
  case NVGPU_EV_FENCE:
    nvgpu_ev_call(ev, NVGPU_EVKEY_COOKIE(cookie), kind, cookie, payload, len);
    break;
  case NVGPU_EV_READY:
    if (cookie <= U32_MAX) {
      /* A legacy watch: every opened handle has one, cookie == handle. */
      nvgpu_pace_inc(NVGPU_PACE_EV_LEGACY);
      nvgpu_ev_call(ev, NVGPU_EVKEY_HANDLE(cookie), kind, cookie, payload,
                    len);
      nvgpu_event_deliver(dev, (u32)cookie);
    } else {
      nvgpu_ev_call(ev, NVGPU_EVKEY_COOKIE(cookie), kind, cookie, payload,
                    len);
    }
    break;
  case NVGPU_EV_HOTPLUG: {
    struct nvgpu_ev_hotplug hp = {};

    /* kobject_uevent_env() allocates and takes a mutex, so the uevent
     * itself is sent from a work item. */
    memcpy(&hp, payload, min_t(u32, len, sizeof(hp)));
    if (cookie < BITS_PER_LONG) {
      if (le32_to_cpu(hp.flags) & NVGPU_EV_HOTPLUG_F_HOTPLUG)
        set_bit(cookie, &ev->hotplug_pending);
      if (le32_to_cpu(hp.flags) & NVGPU_EV_HOTPLUG_F_LEASE)
        set_bit(cookie, &ev->lease_pending);
      queue_work(dev->xfer->wq, &ev->hotplug_work);
    }
    nvgpu_ev_call(ev, NVGPU_EVKEY_CARD(cookie), kind, cookie, payload, len);
    break;
  }
  default:
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: event record of unknown kind %u "
                         "(%u bytes) dropped\n",
                         kind, len);
    break;
  }
}

/*
 * One buffer off the event queue. Every length in it is the host's claim, so
 * each is checked against what the device says it wrote before it is used.
 */
static void nvgpu_event_dispatch(struct nvgpu_device *dev, const u8 *buf,
                                 u32 len) {
  struct nvgpu_events *ev = dev->events;
  const struct nvgpu_msg_hdr *hdr = (const void *)buf;
  const u8 *p, *end;
  unsigned long flags;
  u32 type, payload;

  if (len < sizeof(*hdr)) {
    if (len)
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: %u-byte event is shorter than a "
                           "header\n",
                           len);
    return;
  }
  type = le32_to_cpu(hdr->msg_type);

  if (type == NVGPU_MSG_EVENT_READY) {
    u32 handle = le32_to_cpu(hdr->handle);

    spin_lock_irqsave(&ev->lock, flags);
    nvgpu_ev_call(ev, NVGPU_EVKEY_HANDLE(handle), NVGPU_EV_READY, handle, NULL,
                  0);
    nvgpu_event_deliver(dev, handle);
    spin_unlock_irqrestore(&ev->lock, flags);
    return;
  }
  if (type != NVGPU_MSG_EVENT_DATA) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: event queue carried msg_type %u\n",
                         type);
    return;
  }

  payload = le32_to_cpu(hdr->req_id);
  if (payload > len - sizeof(*hdr)) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: EVENT_DATA claims %u bytes, the "
                         "device wrote %zu\n",
                         payload, len - sizeof(*hdr));
    payload = len - sizeof(*hdr);
  }
  p = buf + sizeof(*hdr);
  end = p + payload;

  nvgpu_pace_inc(NVGPU_PACE_EV_BATCHES);
  spin_lock_irqsave(&ev->lock, flags);
  while ((size_t)(end - p) >= sizeof(struct nvgpu_ev_rec)) {
    const struct nvgpu_ev_rec *rec = (const void *)p;
    u32 rlen = le32_to_cpu(rec->len);
    size_t room = end - p - sizeof(*rec);

    if (rlen > room) {
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: event record of %u bytes runs past "
                           "its buffer (%zu left); rest of batch dropped\n",
                           rlen, room);
      break;
    }
    nvgpu_ev_record(dev, le32_to_cpu(rec->kind), le64_to_cpu(rec->cookie),
                    p + sizeof(*rec), rlen);
    if (ALIGN((size_t)rlen, 8) >= room)
      break;
    p += sizeof(*rec) + ALIGN((size_t)rlen, 8);
  }
  spin_unlock_irqrestore(&ev->lock, flags);
}

static int nvgpu_event_post(struct nvgpu_events *ev, void *buf) {
  struct scatterlist sg;

  sg_init_one(&sg, buf, NVGPU_EVENT_BUF_SIZE);
  return virtqueue_add_inbuf(ev->dev->event_vq, &sg, 1, buf, GFP_ATOMIC);
}

/*
 * Only non-sleeping work happens here: registry lookups, waking pollers, and
 * whatever consumers do under the same rule (drm events with GFP_ATOMIC,
 * dma_fence_signal, eventfd_signal). Uevents and CLOSEs go to work items.
 */
void nvgpu_event_vq_cb(struct virtqueue *vq) {
  struct nvgpu_device *dev = vq->vdev->priv;
  struct nvgpu_events *ev = dev->events;
  unsigned long flags;
  bool kick = false;
  unsigned int len;
  void *buf;

  if (!ev)
    return;

  for (;;) {
    int ret;

    spin_lock_irqsave(&ev->vq_lock, flags);
    buf = virtqueue_get_buf(vq, &len);
    spin_unlock_irqrestore(&ev->vq_lock, flags);
    if (!buf)
      break;

    nvgpu_event_dispatch(dev, buf, min_t(u32, len, NVGPU_EVENT_BUF_SIZE));
    /* Back to zero, as it was posted: what the next batch does not write
     * reads as nothing, not as this one's records. */
    memset(buf, 0, min_t(u32, len, NVGPU_EVENT_BUF_SIZE));

    spin_lock_irqsave(&ev->vq_lock, flags);
    ret = nvgpu_event_post(ev, buf);
    spin_unlock_irqrestore(&ev->vq_lock, flags);
    if (ret)
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: event queue would not take a buffer "
                           "back: %d\n",
                           ret);
    else
      kick = true;
  }

  if (kick) {
    spin_lock_irqsave(&ev->vq_lock, flags);
    kick = virtqueue_kick_prepare(vq);
    spin_unlock_irqrestore(&ev->vq_lock, flags);
    if (kick)
      virtqueue_notify(vq);
  }
}

/*
 * EV_HOTPLUG, turned into the uevents a compositor on this side listens for.
 * drm_sysfs_lease_event() is not exported (drm_internal.h), so the LEASE one
 * is built the way it builds it (drm_sysfs.c:423-431).
 */
static void nvgpu_hotplug_work(struct work_struct *work) {
  struct nvgpu_events *ev =
      container_of(work, struct nvgpu_events, hotplug_work);
  struct nvgpu_device *dev = ev->dev;
  unsigned long hp = xchg(&ev->hotplug_pending, 0);
  unsigned long ls = xchg(&ev->lease_pending, 0);
  unsigned long any = hp | ls;
  unsigned int i;

  /* Card records without NVGPU_BCAP_KMS_CARD only name host numbers (the
   * Wayland devmap): no compositor here drives those cards. */
  if (!(dev->backend_caps & NVGPU_BCAP_KMS_CARD))
    return;

  for_each_set_bit(i, &any, BITS_PER_LONG) {
    struct nvgpu_dri_dev *dri;
    struct drm_device *drm;

    if ((int)i >= dev->num_card_recs ||
        dev->cards[i].render_index >= (u32)dev->num_dri_devs) {
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: hotplug for card %u, which this "
                           "guest was never told about\n",
                           i);
      continue;
    }
    dri = &dev->dri_devs[dev->cards[i].render_index];
    drm = dri->drm;
    if (!dri->registered || !drm || !drm->primary)
      continue;

    if (test_bit(i, &hp))
      drm_sysfs_hotplug_event(drm);
    if (test_bit(i, &ls)) {
      char *envp[] = {"LEASE=1", NULL};

      kobject_uevent_env(&drm->primary->kdev->kobj, KOBJ_CHANGE, envp);
    }
  }
}

/*
 * `c` must not be registered already; zero it before the first registration
 * so that unregistering one that never got this far is harmless. Several
 * consumers may share a key: each gets every record for it and filters on
 * `kind` (a handle key sees EV_DRM and legacy EV_READY alike).
 */
int nvgpu_ev_register(struct nvgpu_device *dev, struct nvgpu_ev_consumer *c,
                      u64 key) {
  struct nvgpu_events *ev = dev->events;
  unsigned long flags;

  if (!ev || !c->deliver)
    return -EINVAL;
  /* Nothing will ever be delivered again. */
  if (nvgpu_xfer_dead(dev))
    return -ENODEV;
  spin_lock_irqsave(&ev->lock, flags);
  c->key = key;
  hash_add(ev->consumers, &c->node, key);
  spin_unlock_irqrestore(&ev->lock, flags);
  return 0;
}

/*
 * Delivery happens under the same lock, so once this has taken and dropped it
 * no deliver() is running and none can start: the consumer may be freed.
 * Safe to call on a consumer that was never registered, if it was zeroed.
 */
void nvgpu_ev_unregister(struct nvgpu_device *dev,
                         struct nvgpu_ev_consumer *c) {
  struct nvgpu_events *ev = dev->events;
  unsigned long flags;

  if (!ev)
    return;
  spin_lock_irqsave(&ev->lock, flags);
  if (!hlist_unhashed(&c->node))
    hash_del(&c->node);
  spin_unlock_irqrestore(&ev->lock, flags);
}

/* 0 once the device is gone, which no WATCH or registration accepts. */
u64 nvgpu_ev_new_cookie(struct nvgpu_device *dev) {
  struct nvgpu_events *ev = dev->events;

  if (!ev || nvgpu_xfer_dead(dev))
    return 0;
  return (u64)atomic64_inc_return(&ev->next_cookie);
}

bool nvgpu_fd_detach_drm(struct nvgpu_fd *nfd, u32 *kms_handle) {
  struct nvgpu_events *ev = nfd->dev->events;
  unsigned long flags;
  bool attached;

  if (!ev) {
    /* The device is gone, and with it every event delivery. */
    attached = nfd->drm_file != NULL;
    WRITE_ONCE(nfd->drm_file, NULL);
    *kms_handle = nfd->kms_handle;
    nfd->kms_handle = 0;
    return attached;
  }
  spin_lock_irqsave(&ev->lock, flags);
  attached = nfd->drm_file != NULL;
  WRITE_ONCE(nfd->drm_file, NULL);
  *kms_handle = nfd->kms_handle;
  nfd->kms_handle = 0;
  spin_unlock_irqrestore(&ev->lock, flags);
  return attached;
}

/* ───────── Probe / remove ───────── */

int nvgpu_xfer_init(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf;
  struct nvgpu_events *ev;
  unsigned long flags;
  int i, posted = 0;

  xf = kzalloc(sizeof(*xf), GFP_KERNEL);
  ev = kzalloc(sizeof(*ev), GFP_KERNEL);
  if (!xf || !ev) {
    kfree(xf);
    kfree(ev);
    return -ENOMEM;
  }

  xf->wq = alloc_ordered_workqueue("nvgpu-xfer", 0);
  if (!xf->wq) {
    kfree(xf);
    kfree(ev);
    return -ENOMEM;
  }
  xf->dev = dev;
  spin_lock_init(&xf->lock);
  init_waitqueue_head(&xf->space_wq);
  init_waitqueue_head(&xf->exec_wq);
  seqlock_init(&xf->clk_lock);
  INIT_DELAYED_WORK(&xf->sync_work, nvgpu_time_sync_work);

  /*
   * VIRTIO_RING_F_INDIRECT_DESC is a transport feature: the ring code keeps
   * it if the device offers it (virtio_ring.c vring_transport_features), with
   * no entry in our feature table, and testing it cannot BUG the way an
   * unlisted device bit does.
   */
  xf->indirect = virtio_has_feature(dev->vdev, VIRTIO_RING_F_INDIRECT_DESC);
  xf->vring_size = virtqueue_get_vring_size(dev->ctrl_vq);
  xf->max_sg = xf->indirect
                   ? min_t(unsigned int, NVGPU_SG_MAX_INDIRECT,
                           xf->vring_size / 2)
                   : min_t(unsigned int, NVGPU_SG_MAX_DIRECT,
                           xf->vring_size / 4);
  xf->max_sg = max(xf->max_sg, 1u);
  xf->exec_budget = max_t(int, (int)xf->vring_size / 2 - 16, 1);
  atomic_set(&xf->exec_avail, xf->exec_budget);

  /* v1 until HELLO says otherwise. */
  dev->max_req = xf->max_sg * NVGPU_TBUF_CHUNK;
  dev->max_resp = NVGPU_V1_RESP_MAX;

  ev->dev = dev;
  spin_lock_init(&ev->lock);
  spin_lock_init(&ev->vq_lock);
  hash_init(ev->consumers);
  atomic64_set(&ev->next_cookie, NVGPU_COOKIE_BASE);
  INIT_WORK(&ev->hotplug_work, nvgpu_hotplug_work);

  dev->xfer = xf;
  dev->events = ev;

  /*
   * Somewhere for the host to put events. 8 KiB each whichever protocol the
   * backend speaks: a v1 backend writes its 16-byte EVENT_READY at the front
   * of whatever it is given, and a v2 one batches records up to the size.
   */
  /* Zeroed: a device that says it wrote more than it did must not have the
   * dispatch read whatever the heap held there before. */
  for (i = 0; i < NVGPU_EVENT_BUFS; i++)
    ev->bufs[i] = kzalloc(NVGPU_EVENT_BUF_SIZE, GFP_KERNEL);
  spin_lock_irqsave(&ev->vq_lock, flags);
  for (i = 0; i < NVGPU_EVENT_BUFS; i++) {
    if (!ev->bufs[i] || nvgpu_event_post(ev, ev->bufs[i]))
      break;
    posted++;
  }
  if (posted)
    virtqueue_kick(dev->event_vq);
  spin_unlock_irqrestore(&ev->vq_lock, flags);
  if (posted < NVGPU_EVENT_BUFS)
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: only %d of %d event buffers posted; waits may "
             "be woken late\n",
             posted, NVGPU_EVENT_BUFS);
  return 0;
}

/*
 * Whether nothing sent from here will ever be answered: after a reset or
 * remove(). For sleepers the transport cannot wake itself. Any holder of a
 * device reference may ask: the state lives until the last one goes.
 */
bool nvgpu_xfer_dead(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = READ_ONCE(dev->xfer);

  return !xf || READ_ONCE(xf->dead);
}

void nvgpu_xfer_quiesce(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;

  if (!xf)
    return;
  cancel_delayed_work_sync(&xf->sync_work);
  flush_workqueue(xf->wq);
}

void nvgpu_xfer_reclaim(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  struct nvgpu_events *ev = dev->events;
  struct nvgpu_req *r;
  unsigned long flags;
  int i;

  if (!xf)
    return;

  spin_lock_irqsave(&xf->lock, flags);
  xf->dead = true;
  spin_unlock_irqrestore(&xf->lock, flags);
  wake_up_all(&xf->space_wq);
  wake_up_all(&xf->exec_wq);
  nvgpu_fence_wake_waiters();
  cancel_delayed_work_sync(&xf->sync_work);

  /* After the reset nothing on the ring will ever be answered. */
  while ((r = virtqueue_detach_unused_buf(dev->ctrl_vq)) != NULL) {
    bool orphan;

    spin_lock_irqsave(&xf->lock, flags);
    r->completed = true;
    r->dead = true;
    orphan = r->abandoned;
    if (!orphan)
      complete(&r->done);
    spin_unlock_irqrestore(&xf->lock, flags);
    if (orphan)
      nvgpu_req_free_orphan(r);
  }
  drain_workqueue(xf->wq);

  if (ev)
    for (i = 0; i < NVGPU_EVENT_BUFS; i++) {
      kfree(ev->bufs[i]);
      ev->bufs[i] = NULL;
    }
}

/*
 * The queue goes; the state stays, dead, for whatever still holds the device
 * (an open file, a fence, a mapping): each of them finds nvgpu_xfer_dead()
 * rather than freed memory, and every path that could still queue work
 * checks that under xf->lock first (nvgpu_queue_close()), or ran on a
 * virtqueue that is gone (the callbacks, hotplug).
 */
void nvgpu_xfer_destroy(struct nvgpu_device *dev) {
  struct nvgpu_xfer *xf = dev->xfer;
  unsigned long flags;

  if (!xf)
    return;
  spin_lock_irqsave(&xf->lock, flags);
  xf->dead = true;
  spin_unlock_irqrestore(&xf->lock, flags);
  cancel_delayed_work_sync(&xf->sync_work);
  if (xf->wq) {
    destroy_workqueue(xf->wq);
    xf->wq = NULL;
  }
}

void nvgpu_xfer_free(struct nvgpu_device *dev) {
  struct nvgpu_events *ev = dev->events;
  int i;

  kfree(dev->xfer);
  dev->xfer = NULL;
  if (ev) {
    for (i = 0; i < NVGPU_EVENT_BUFS; i++)
      kfree(ev->bufs[i]);
    kfree(ev);
    dev->events = NULL;
  }
}
