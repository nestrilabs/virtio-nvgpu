// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the host's clock -- TIME_SYNC, and where host timestamps
 * land in the guest's clocks, slewed so they never go backwards.
 */

#include <linux/ktime.h>
#include <linux/math64.h>
#include <linux/seqlock.h>
#include <linux/timekeeping.h>
#include <linux/workqueue.h>

#include "nvgpu_xfer.h"

/* Samples per sync, the RTT we trust, how fast to slew. */
#define NVGPU_TIME_SAMPLES 8
#define NVGPU_TIME_RTT_GOOD_NS (200 * NSEC_PER_USEC)
/* 50 us per second, i.e. one part in 20000. */
#define NVGPU_TIME_SLEW_DIV 20000

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
int nvgpu_time_sync(struct nvgpu_device *dev, bool initial) {
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

void nvgpu_time_sync_work(struct work_struct *work) {
  struct nvgpu_xfer *xf =
      container_of(to_delayed_work(work), struct nvgpu_xfer, sync_work);
  int ret = nvgpu_time_sync(xf->dev, false);

  /* A backend that stops knowing the message is not going to learn it. */
  if (ret == -EPROTO || ret == -ENODEV || READ_ONCE(xf->dead))
    return;
  queue_delayed_work(xf->wq, &xf->sync_work, NVGPU_TIME_RESYNC);
}
