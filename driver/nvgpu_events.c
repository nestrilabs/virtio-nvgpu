// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the event queue -- legacy readiness, EVENT_DATA records
 * dispatched to their consumers, EV_HOTPLUG's uevents -- and the consumer
 * registry (nvgpu.h, "Event consumers"). Lock ordering is nvgpu_xfer.c's.
 */

#include <drm/drm_device.h>
#include <drm/drm_file.h>
#include <drm/drm_sysfs.h>
#include <linux/bitops.h>
#include <linux/hashtable.h>
#include <linux/kobject.h>
#include <linux/scatterlist.h>
#include <linux/virtio.h>

#include "nvgpu_xfer.h"

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

int nvgpu_event_post(struct nvgpu_events *ev, void *buf) {
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
void nvgpu_hotplug_work(struct work_struct *work) {
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
