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

#define NVGPU_ESC_RM_ALLOC_MEMORY 0x27
#define NVGPU_ESC_RM_ALLOC 0x2b
#define NVGPU_ESC_RM_VID_HEAP_CONTROL 0x4a
#define NVGPU_CLASS_OS_DESCRIPTOR 0x71
#define NVGPU_OS32_ALLOC_OS_DESCRIPTOR 27
/* NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS: the only type registered here. */
#define NVGPU_DESCRIPTOR_VIRTUAL_ADDRESS 0
/* What RM answers when the pages would not pin (escape.c, os-mlock.c). */
#define NVGPU_NV_ERR_INVALID_ADDRESS 0x1e

/* Parameter layouts (nvos.h), as device/src/osdesc.rs reads them. */
#define NVGPU_OS02_SIZE 56 /* with its fd */
#define NVGPU_OS02_CLASS 12
#define NVGPU_OS02_FLAGS 16
#define NVGPU_OS02_MEMORY 24
#define NVGPU_OS02_LIMIT 32
#define NVGPU_OS02_STATUS 40
#define NVGPU_OS02_USER_READ_ONLY (1u << 21)
#define NVGPU_OS32_SIZE 184
#define NVGPU_OS32_FUNCTION 8
#define NVGPU_OS32_STATUS 20
#define NVGPU_OS32_ATTR2 56
#define NVGPU_OS32_DESCRIPTOR 64
#define NVGPU_OS32_LIMIT 72
#define NVGPU_OS32_DESCRIPTOR_TYPE 80
#define NVGPU_OS64_SIZE 48
#define NVGPU_OS64_CLASS 12
#define NVGPU_OS64_PARAMS 16
#define NVGPU_OS64_PARAMS_SIZE 32
#define NVGPU_OS64_STATUS 40
#define NVGPU_OSD_SIZE 40 /* NV_OS_DESC_MEMORY_ALLOCATION_PARAMS */
#define NVGPU_OSD_FLAGS 4
#define NVGPU_OSD_ATTR2 12
#define NVGPU_OSD_DESCRIPTOR 16
#define NVGPU_OSD_LIMIT 24
#define NVGPU_OSD_DESCRIPTOR_TYPE 32
/* NVOS32_ATTR2_PROTECTION_USER_READ_ONLY, NVOS32_ALLOC_FLAGS_USER_READ_ONLY */
#define NVGPU_ATTR2_USER_READ_ONLY (1u << 22)
#define NVGPU_OS32_FLAGS_USER_READ_ONLY 0x04000000u

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

/* What one of the three calls asks RM to pin, read from the caller's block. */
struct nvgpu_osdesc_call {
  unsigned int nr;
  u8 outer[NVGPU_OS32_SIZE]; /* the largest of the three */
  u32 outer_len;
  u8 nested[NVGPU_OSD_SIZE]; /* RM_ALLOC's class parameters */
  u32 nested_len;
  void __user *unested;
  u32 status_at;
  u64 va;
  u64 size;
  bool write;
};

static bool nvgpu_osdesc_ok(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_OS_DESC);
}

void nvgpu_osdesc_init(struct nvgpu_device *dev) {
  mutex_init(&dev->osdesc_lock);
  INIT_LIST_HEAD(&dev->osdescs);
}

static void nvgpu_osdesc_unpin(struct page **pages, unsigned long n,
                               bool write) {
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
static void nvgpu_osdesc_keep(struct nvgpu_device *dev, u64 id,
                              struct page **pages, unsigned long npages,
                              bool write) {
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

/*
 * Read the call from the caller's block. False for anything that is not one
 * of the three, well-formed, with the virtual address descriptor type: that
 * goes the old way (the backend refuses a registration by address). An
 * unreadable block sets *ret.
 */
static bool nvgpu_osdesc_describe(struct nvgpu_osdesc_call *c,
                                  unsigned int cmd, void __user *uarg,
                                  unsigned int sz, long *ret) {
  const u8 *o = c->outer;
  u64 limit, end;
  u32 psize;

  c->nr = _IOC_NR(cmd);
  switch (c->nr) {
  case NVGPU_ESC_RM_ALLOC_MEMORY:
    c->outer_len = NVGPU_OS02_SIZE;
    break;
  case NVGPU_ESC_RM_VID_HEAP_CONTROL:
    c->outer_len = NVGPU_OS32_SIZE;
    break;
  case NVGPU_ESC_RM_ALLOC:
    c->outer_len = NVGPU_OS64_SIZE;
    break;
  default:
    return false;
  }
  if (sz != c->outer_len)
    return false;
  if (copy_from_user(c->outer, uarg, c->outer_len)) {
    *ret = -EFAULT;
    return true;
  }

  switch (c->nr) {
  case NVGPU_ESC_RM_ALLOC_MEMORY:
    if (get_unaligned_le32(o + NVGPU_OS02_CLASS) != NVGPU_CLASS_OS_DESCRIPTOR)
      return false;
    c->va = get_unaligned_le64(o + NVGPU_OS02_MEMORY);
    limit = get_unaligned_le64(o + NVGPU_OS02_LIMIT);
    /* ALLOC_USER_READ_ONLY makes RM pin read-only (escape.c:260-261). */
    c->write = !(get_unaligned_le32(o + NVGPU_OS02_FLAGS) &
                 NVGPU_OS02_USER_READ_ONLY);
    c->status_at = NVGPU_OS02_STATUS;
    break;
  case NVGPU_ESC_RM_VID_HEAP_CONTROL:
    if (get_unaligned_le32(o + NVGPU_OS32_FUNCTION) !=
            NVGPU_OS32_ALLOC_OS_DESCRIPTOR ||
        get_unaligned_le32(o + NVGPU_OS32_DESCRIPTOR_TYPE) !=
            NVGPU_DESCRIPTOR_VIRTUAL_ADDRESS)
      return false;
    c->va = get_unaligned_le64(o + NVGPU_OS32_DESCRIPTOR);
    limit = get_unaligned_le64(o + NVGPU_OS32_LIMIT);
    c->write = !(get_unaligned_le32(o + NVGPU_OS32_ATTR2) &
                 NVGPU_ATTR2_USER_READ_ONLY);
    c->status_at = NVGPU_OS32_STATUS;
    break;
  default: /* NVGPU_ESC_RM_ALLOC */
    if (get_unaligned_le32(o + NVGPU_OS64_CLASS) != NVGPU_CLASS_OS_DESCRIPTOR)
      return false;
    psize = get_unaligned_le32(o + NVGPU_OS64_PARAMS_SIZE);
    c->unested = u64_to_user_ptr(get_unaligned_le64(o + NVGPU_OS64_PARAMS));
    if (!c->unested || (psize && psize != NVGPU_OSD_SIZE))
      return false;
    if (copy_from_user(c->nested, c->unested, NVGPU_OSD_SIZE)) {
      *ret = -EFAULT;
      return true;
    }
    if (get_unaligned_le32(c->nested + NVGPU_OSD_DESCRIPTOR_TYPE) !=
        NVGPU_DESCRIPTOR_VIRTUAL_ADDRESS)
      return false;
    c->nested_len = NVGPU_OSD_SIZE;
    c->va = get_unaligned_le64(c->nested + NVGPU_OSD_DESCRIPTOR);
    limit = get_unaligned_le64(c->nested + NVGPU_OSD_LIMIT);
    /* osdescConstruct takes either as read-only (os_desc_mem.c:75-84). */
    c->write = !(get_unaligned_le32(c->nested + NVGPU_OSD_ATTR2) &
                 NVGPU_ATTR2_USER_READ_ONLY) &&
               !(get_unaligned_le32(c->nested + NVGPU_OSD_FLAGS) &
                 NVGPU_OS32_FLAGS_USER_READ_ONLY);
    c->status_at = NVGPU_OS64_STATUS;
    break;
  }
  if (check_add_overflow(limit, 1ull, &c->size) ||
      check_add_overflow(c->va, c->size, &end))
    return false;
  if (DIV_ROUND_UP((c->va & ~PAGE_MASK) + c->size, PAGE_SIZE) >
      NVGPU_OSDESC_MAX_PAGES)
    return false;
  return true;
}

/* Pin the `npages` pages from `start`, all of them, into `pages`. */
static int nvgpu_osdesc_pin(unsigned long start, unsigned long npages,
                            bool write, struct page **pages) {
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

/* Runs of guest-physically contiguous pages, in order; how many, or -E2BIG. */
static long nvgpu_osdesc_runs(struct page **pages, unsigned long npages,
                              struct nvgpu_osdesc_run *out) {
  unsigned long i;
  long n = 0;
  u64 gpa = 0, len = 0;

  for (i = 0; i < npages; i++) {
    u64 pa = page_to_phys(pages[i]);

    if (len && pa == gpa + len * PAGE_SIZE && len < U32_MAX) {
      len++;
      continue;
    }
    if (len) {
      if (out) {
        out[n].gpa = cpu_to_le64(gpa);
        out[n].pages = cpu_to_le32((u32)len);
        out[n].reserved = 0;
      }
      n++;
    }
    gpa = pa;
    len = 1;
  }
  if (out) {
    out[n].gpa = cpu_to_le64(gpa);
    out[n].pages = cpu_to_le32((u32)len);
    out[n].reserved = 0;
  }
  n++;
  return n > NVGPU_OSDESC_MAX_RUNS ? -E2BIG : n;
}

static long nvgpu_osdesc_register(struct nvgpu_fd *nfd, unsigned int cmd,
                                  void __user *uarg,
                                  struct nvgpu_osdesc_call *c) {
  struct nvgpu_device *dev = nfd->dev;
  unsigned long off = c->va & ~PAGE_MASK;
  unsigned long npages = DIV_ROUND_UP(off + c->size, PAGE_SIZE);
  struct nvgpu_ioctl_req *req = NULL;
  struct nvgpu_ioctl_resp *resp = NULL;
  struct nvgpu_osdesc_hdr *h;
  struct page **pages;
  size_t req_len, resp_len, params = c->outer_len + c->nested_len;
  u32 used, data_len, nested_len, deep_len;
  long nruns, ret;
  u64 id = 0;
  u8 *at;

  /* What RM has let go of first, so its budget is back. */
  nvgpu_osdesc_reap(dev);

  pages = kvmalloc_array(npages, sizeof(*pages), GFP_KERNEL);
  if (!pages)
    return -ENOMEM;
  ret = nvgpu_osdesc_pin(c->va & PAGE_MASK, npages, c->write, pages);
  if (ret) {
    kvfree(pages);
    /*
     * RM would have failed to pin them too, and they are no device memory
     * either (os_lookup_user_io_memory): its status, in a call that
     * succeeded, as natively.
     */
    put_unaligned_le32(NVGPU_NV_ERR_INVALID_ADDRESS, c->outer + c->status_at);
    return copy_to_user(uarg + c->status_at, c->outer + c->status_at,
                        sizeof(u32))
               ? -EFAULT
               : 0;
  }

  nruns = nvgpu_osdesc_runs(pages, npages, NULL);
  if (nruns < 0) {
    ret = -ENOMEM;
    goto unpin;
  }
  req_len = sizeof(*req) + params + sizeof(*h) +
            nruns * sizeof(struct nvgpu_osdesc_run);
  resp_len = sizeof(*resp) + params + sizeof(u64);
  req = kvzalloc(req_len, GFP_KERNEL);
  resp = kzalloc(resp_len, GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto unpin;
  }
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(c->outer_len);
  req->nested_offset = cpu_to_le32(c->nested_len ? c->outer_len : 0);
  req->nested_len = cpu_to_le32(c->nested_len);
  req->deep_ptr_offset = cpu_to_le32(NVGPU_DEEP_PAGE_LIST);
  req->deep_len =
      cpu_to_le32(sizeof(*h) + nruns * sizeof(struct nvgpu_osdesc_run));
  at = (u8 *)(req + 1);
  memcpy(at, c->outer, c->outer_len);
  memcpy(at + c->outer_len, c->nested, c->nested_len);
  h = (struct nvgpu_osdesc_hdr *)(at + params);
  h->nruns = cpu_to_le32((u32)nruns);
  h->flags = cpu_to_le32(c->write ? NVGPU_OSDESC_F_WRITE : 0);
  nvgpu_osdesc_runs(pages, npages, (struct nvgpu_osdesc_run *)(h + 1));

  ret = nvgpu_send_recv_used(dev, req, (int)req_len, resp, (int)resp_len,
                             &used);
  kvfree(req);
  req = NULL;
  if (ret == -EINTR || ret == -ETIMEDOUT) {
    /* It may reach RM yet, and nothing will say so: pinned until remove(). */
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: an OS-descriptor registration was "
                         "abandoned in flight; its %lu pages stay pinned\n",
                         npages);
    nvgpu_osdesc_keep(dev, 0, pages, npages, c->write);
    kfree(resp);
    return ret;
  }
  if (ret < 0)
    goto unpin;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto unpin;
  }
  ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
  if (ret < 0)
    goto unpin; /* refused before RM saw it */

  data_len = nvgpu_resp_has(used, 0, sizeof(*resp))
                 ? le32_to_cpu(resp->data_len)
                 : 0;
  nested_len = data_len ? le32_to_cpu(resp->nested_len) : 0;
  deep_len = data_len ? le32_to_cpu(resp->deep_len) : 0;
  if (deep_len == sizeof(u64) &&
      nvgpu_resp_has(used, sizeof(*resp) + data_len + nested_len, deep_len))
    id = get_unaligned_le64((u8 *)(resp + 1) + data_len + nested_len);
  if (id)
    /* Registered: RM has the pages until a reap says otherwise. */
    nvgpu_osdesc_keep(dev, id, pages, npages, c->write);
  else
    nvgpu_osdesc_unpin(pages, npages, c->write);
  pages = NULL;

  /* The caller's block back, its own address in it, and RM's status. */
  if (data_len != c->outer_len || nested_len != c->nested_len ||
      !nvgpu_resp_has(used, sizeof(*resp), params)) {
    ret = -EIO;
    goto out;
  }
  if (copy_to_user(uarg, resp + 1, c->outer_len) ||
      (c->nested_len &&
       copy_to_user(c->unested, (u8 *)(resp + 1) + c->outer_len,
                    c->nested_len)))
    ret = -EFAULT;
  goto out;

unpin:
  nvgpu_osdesc_unpin(pages, npages, c->write);
out:
  kvfree(req);
  kfree(resp);
  return ret;
}

bool nvgpu_osdesc_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                        void __user *uarg, unsigned int sz, long *ret) {
  struct nvgpu_osdesc_call *c;
  bool ours;

  u32 which, want;
  size_t at;

  if (!nvgpu_osdesc_ok(nfd->dev))
    return false;
  /* The class or function alone first: every other RM_ALLOC passes here. */
  switch (_IOC_NR(cmd)) {
  case NVGPU_ESC_RM_ALLOC_MEMORY:
    at = NVGPU_OS02_CLASS;
    want = NVGPU_CLASS_OS_DESCRIPTOR;
    break;
  case NVGPU_ESC_RM_VID_HEAP_CONTROL:
    at = NVGPU_OS32_FUNCTION;
    want = NVGPU_OS32_ALLOC_OS_DESCRIPTOR;
    break;
  case NVGPU_ESC_RM_ALLOC:
    at = NVGPU_OS64_CLASS;
    want = NVGPU_CLASS_OS_DESCRIPTOR;
    break;
  default:
    return false;
  }
  if (sz < at + sizeof(which) || get_user(which, (u32 __user *)(uarg + at)) ||
      le32_to_cpu((__force __le32)which) != want)
    return false;
  c = kzalloc(sizeof(*c), GFP_KERNEL);
  if (!c) {
    *ret = -ENOMEM;
    return true;
  }
  *ret = 0;
  ours = nvgpu_osdesc_describe(c, cmd, uarg, sz, ret);
  if (ours && !*ret)
    *ret = nvgpu_osdesc_register(nfd, cmd, uarg, c);
  kfree(c);
  return ours;
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
