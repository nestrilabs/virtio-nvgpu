// SPDX-License-Identifier: GPL-2.0
/*
 * Memory the caller already has, registered with RM by its guest-physical
 * pages.
 *
 * RM registers existing memory by CPU address -- NV01_MEMORY_SYSTEM_OS_
 * DESCRIPTOR (0x71) through NV_ESC_RM_ALLOC_MEMORY or NV_ESC_RM_ALLOC, or
 * NV_ESC_RM_VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR -- and pins whatever that
 * address maps in the calling process. The calling process is the backend,
 * so a guest address would name the VMM's memory, and the backend refuses it.
 * cuCtxCreate registers a 2 MiB buffer this way, and cuMemHostRegister and
 * VK_EXT_external_memory_host are built on it.
 *
 * So, with a backend that says NVGPU_BCAP_OS_DESC, the pages travel instead:
 * this pins the caller's range the way RM would (os_lock_user_pages:
 * FOLL_LONGTERM, and FOLL_WRITE unless the call asks for memory read-only to
 * the CPU), sends the guest-physical page list with the call
 * (NVGPU_DEEP_PAGE_LIST), and the backend hands RM an address of its own that
 * maps exactly those pages. The caller's address stays in the block as it
 * wrote it.
 *
 * The pages stay pinned for as long as RM may reach them. RM's lifetime rules
 * are the backend's to follow -- the object's free, its parent's, its
 * client's, the close of the client's file, a duplicate still holding it --
 * so a registration that succeeds carries an id, and the pages are unpinned
 * only when a reap (NVGPU_OP_OSDESC_REAP) names it: after an RM_FREE, after a
 * file's CLOSE, and before the next registration. A request abandoned in
 * flight keeps its pages until remove(): it may still have reached RM.
 *
 * Anything that is not exactly one of the three, with the user virtual
 * address descriptor type, goes the old way, and the backend refuses it.
 */

#include <linux/mm.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include "nvgpu.h"

/* Pages pinned at one go. */
#define NVGPU_OSDESC_PIN_CHUNK 4096

/* One registration's pins. `id` 0: a request abandoned in flight. */
struct nvgpu_osdesc {
  struct list_head node;
  u64 id;
  struct page **pages;
  unsigned long npages;
  bool write;
};

bool nvgpu_osdesc_ok(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_OS_DESC);
}

void nvgpu_osdesc_init(struct nvgpu_device *dev) {
  mutex_init(&dev->osdesc_lock);
  INIT_LIST_HEAD(&dev->osdescs);
}

void nvgpu_osdesc_unpin(struct page **pages, unsigned long n, bool write) {
  unpin_user_pages_dirty_lock(pages, n, write);
  kvfree(pages);
}

static void nvgpu_osdesc_free(struct nvgpu_osdesc *d) {
  nvgpu_osdesc_unpin(d->pages, d->npages, d->write);
  kfree(d);
}

/*
 * Whether a reap already named `id`: its reply beat the registration's own
 * (another thread freed the object first). Taken out if so. Under
 * osdesc_lock.
 */
static bool nvgpu_osdesc_released_early(struct nvgpu_device *dev, u64 id) {
  unsigned int i;

  for (i = 0; i < NVGPU_OSDESC_EARLY; i++) {
    if (dev->osdesc_early[i] == id) {
      dev->osdesc_early[i] = 0;
      return true;
    }
  }
  return false;
}

/* Keep `pages` pinned under `id` until a reap names it (or remove()). */
void nvgpu_osdesc_keep(struct nvgpu_device *dev, u64 id, struct page **pages,
                       unsigned long npages, bool write) {
  struct nvgpu_osdesc *d = kzalloc(sizeof(*d), GFP_KERNEL);
  bool now;

  if (!d) {
    /* Nothing to find them by later: better pinned for good than unpinned
     * while RM may still reach them. */
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: no memory to record registration "
                         "%llu; its %lu pages stay pinned\n",
                         id, npages);
    return;
  }
  d->id = id;
  d->pages = pages;
  d->npages = npages;
  d->write = write;
  mutex_lock(&dev->osdesc_lock);
  now = dev->osdesc_dead || (id && nvgpu_osdesc_released_early(dev, id));
  if (!now) {
    list_add_tail(&d->node, &dev->osdescs);
    WRITE_ONCE(dev->osdesc_count, dev->osdesc_count + 1);
  }
  mutex_unlock(&dev->osdesc_lock);
  if (now)
    nvgpu_osdesc_free(d);
}

/* Pin the `npages` pages from `start`, all of them, into `pages`. */
int nvgpu_osdesc_pin(unsigned long start, unsigned long npages, bool write,
                     struct page **pages) {
  unsigned int gup = FOLL_LONGTERM | (write ? FOLL_WRITE : 0);
  unsigned long done = 0;

  while (done < npages) {
    int n = pin_user_pages_fast(
        start + done * PAGE_SIZE,
        (int)min_t(unsigned long, npages - done, NVGPU_OSDESC_PIN_CHUNK), gup,
        pages + done);

    if (n <= 0) {
      if (done)
        unpin_user_pages(pages, done);
      return n ? n : -EFAULT;
    }
    done += n;
  }
  return 0;
}

/* NVGPU_OP_OSDESC_REAP's reply: header, the fixed part, then the ids. */
#define NVGPU_OSDESC_REAP_RESP                                                 \
  (sizeof(struct nvgpu_msg_hdr) + sizeof(struct nvgpu_host_op_resp) +         \
   NVGPU_OSDESC_REAP_MAX * sizeof(u64))

void nvgpu_osdesc_reap(struct nvgpu_device *dev) {
  struct {
    struct nvgpu_msg_hdr hdr;
    struct nvgpu_host_op_req op;
  } __packed req;
  const size_t fixed =
      sizeof(struct nvgpu_msg_hdr) + sizeof(struct nvgpu_host_op_resp);
  struct nvgpu_osdesc *d, *tmp;
  LIST_HEAD(done);
  unsigned int round;
  u8 *resp;

  if (!READ_ONCE(dev->osdesc_count) || !nvgpu_osdesc_ok(dev))
    return;
  resp = kmalloc(NVGPU_OSDESC_REAP_RESP, GFP_KERNEL);
  if (!resp)
    return;

  mutex_lock(&dev->osdesc_lock);
  for (round = 0; round < 64 && !dev->osdesc_dead; round++) {
    const struct nvgpu_host_op_resp *a =
        (const void *)(resp + sizeof(struct nvgpu_msg_hdr));
    u64 last, n, i;
    u32 used;

    memset(&req, 0, sizeof(req));
    req.hdr.msg_type = cpu_to_le32(NVGPU_MSG_HOST_OP);
    req.op.op = cpu_to_le32(NVGPU_OP_OSDESC_REAP);
    req.op.nargs = cpu_to_le32(1);
    req.op.args[0] = cpu_to_le64(dev->osdesc_ack);
    if (nvgpu_send_recv_used(dev, &req, sizeof(req), resp,
                             NVGPU_OSDESC_REAP_RESP, &used) ||
        used < fixed ||
        (s32)le32_to_cpu(((struct nvgpu_msg_hdr *)resp)->status) < 0 ||
        le32_to_cpu(a->nres) < 2)
      break;
    last = le64_to_cpu(a->res[0]);
    n = le64_to_cpu(a->res[1]);
    if (n > NVGPU_OSDESC_REAP_MAX || used < fixed + n * sizeof(u64))
      break;
    for (i = 0; i < n; i++) {
      u64 id = get_unaligned_le64(resp + fixed + i * sizeof(u64));
      bool found = false;

      list_for_each_entry(d, &dev->osdescs, node) {
        if (d->id == id) {
          list_move_tail(&d->node, &done);
          WRITE_ONCE(dev->osdesc_count, dev->osdesc_count - 1);
          found = true;
          break;
        }
      }
      /* Its registration's reply has not been read yet: it will look. */
      if (!found && id) {
        dev->osdesc_early[dev->osdesc_early_next] = id;
        dev->osdesc_early_next =
            (dev->osdesc_early_next + 1) % NVGPU_OSDESC_EARLY;
      }
    }
    dev->osdesc_ack = last;
    if (n < NVGPU_OSDESC_REAP_MAX)
      break;
  }
  mutex_unlock(&dev->osdesc_lock);
  kfree(resp);

  list_for_each_entry_safe(d, tmp, &done, node) {
    list_del(&d->node);
    nvgpu_osdesc_free(d);
  }
}

/*
 * The device is gone, and the session with it: every client the backend
 * held is freed, and nothing will reach these pages again.
 */
void nvgpu_osdesc_release_all(struct nvgpu_device *dev) {
  struct nvgpu_osdesc *d, *tmp;
  LIST_HEAD(done);

  mutex_lock(&dev->osdesc_lock);
  dev->osdesc_dead = true;
  list_splice_init(&dev->osdescs, &done);
  WRITE_ONCE(dev->osdesc_count, 0);
  mutex_unlock(&dev->osdesc_lock);
  list_for_each_entry_safe(d, tmp, &done, node) {
    list_del(&d->node);
    nvgpu_osdesc_free(d);
  }
}
