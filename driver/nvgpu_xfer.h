/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * virtio-gpu-nv: the transport's own state, shared by the files it is made of
 * and by nothing else -- nvgpu_xfer.c (requests, CLOSE and the async ops, the
 * reaper, HOST_OP / WATCH, HELLO, probe and remove), nvgpu_tbuf.c (transport
 * buffers), nvgpu_clock.c (the host's clock) and nvgpu_events.c (the event
 * queue and its consumer registry). Lock ordering is nvgpu_xfer.c's header.
 */

#ifndef NVGPU_XFER_H
#define NVGPU_XFER_H

#include <linux/atomic.h>
#include <linux/hashtable.h>
#include <linux/scatterlist.h>
#include <linux/seqlock.h>
#include <linux/sizes.h>
#include <linux/spinlock.h>
#include <linux/wait.h>
#include <linux/workqueue.h>

#include "nvgpu.h"

/*
 * Largest piece of a transport buffer. Order 4 is the largest allocation the
 * page allocator still makes readily on a fragmented guest, and 64 KiB pieces
 * put 4 MiB -- the most a v2 backend accepts -- in 64 scatter-gather entries.
 */
#define NVGPU_TBUF_CHUNK SZ_64K

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

/* How often the clock is synced again. */
#define NVGPU_TIME_RESYNC (5 * HZ)

#define NVGPU_EVENT_BUFS 16

struct nvgpu_tbuf {
  size_t len;
  unsigned int nents;
  bool inline_data; /* the data follows sg[0] in this allocation */
  /* nvgpu_tbuf_on_free(): run when the buffer is freed, whoever frees it. */
  void (*release)(void *arg);
  void *release_arg;
  struct scatterlist sg[];
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

  /* Callers spinning for a reply, and callers asleep waiting for one (both
   * under `lock`): while there are spinners and no sleepers, the control
   * queue's interrupt is off and the spinners take replies off the ring
   * themselves (nvgpu_ctrl_poll_enter()). */
  unsigned int pollers;
  unsigned int sleepers;

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

struct nvgpu_times {
  u64 t0; /* just before the request went on the ring */
  u64 t1; /* in the callback that took it off */
};

/* nvgpu_tbuf.c: a buffer of `len` bytes in at most `max_sg` pieces; NULL on
 * failure. */
struct nvgpu_tbuf *nvgpu_tbuf_alloc_sg(size_t len, gfp_t gfp,
                                       unsigned int max_sg);

/* nvgpu_xfer.c: one request from plain kernel memory (see there). */
int nvgpu_call(struct nvgpu_device *dev, const void *req, size_t req_len,
               void *resp, size_t resp_len, u32 flags, u32 *used_len,
               struct nvgpu_times *tm, bool *sent);
/* The status of a reply that must carry a header: 0, a -errno, -EIO for less
 * than a header, -EPROTO for a status that is neither. */
int nvgpu_hdr_status(const void *resp, u32 used);

/* nvgpu_clock.c: sync the clock (`initial`: no slew, nothing translated
 * yet), and the resync work, re-queued every NVGPU_TIME_RESYNC. */
int nvgpu_time_sync(struct nvgpu_device *dev, bool initial);
void nvgpu_time_sync_work(struct work_struct *work);

/* nvgpu_events.c: post an event buffer (under ev->vq_lock), and the
 * EV_HOTPLUG uevent work. */
int nvgpu_event_post(struct nvgpu_events *ev, void *buf);
void nvgpu_hotplug_work(struct work_struct *work);

#endif /* NVGPU_XFER_H */
