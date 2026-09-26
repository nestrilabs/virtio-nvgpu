// SPDX-License-Identifier: GPL-2.0
/*
 * The protocol-v1 IOCTL message, in C: RM escapes (flat ones, RM_CONTROL with
 * its nested block, intercepts and deep pointers, RM_ALLOC, IDLE_CHANNELS),
 * ioctls that carry a descriptor at a fixed offset, UVM, and a v1 backend's
 * NVKMS commands. Moved here from nvgpu_main.c unchanged.
 *
 * This is the C implementation of what driver/rust/core/src/guest/rm.rs and
 * dispatch.rs implement in Rust; the Makefile builds one or the other
 * (NVGPU_RUST). It stays the reference, and the default, until the Rust one
 * has passed the hardware regression; driver/rust/difftest runs both on the
 * same inputs.
 */

#include <linux/kernel.h>
#include <linux/math64.h>
#include <linux/mm.h>
#include <linux/minmax.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/time.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include "gen/nvgpu_rm_deep.h"
#include "gen/nvgpu_rmalloc_classes.h"
#include "gen/nvgpu_schema.h"
#include "gen/nvgpu_v1v2_rewrites.h"
#include "nvgpu_rm_intercepts.h"
#include "nvgpu.h"

/* NVIDIA ioctl numbers that require nested-pointer marshalling */
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_DUP_OBJECT 0x34
#define NV_ESC_RM_IDLE_CHANNELS 0x41
/* UVM_INITIALIZE ioctl nr */
#define UVM_INITIALIZE_NR 0x30

/*
 * RM control commands that name an open file by descriptor inside their
 * parameters (ctrl0000unix.h). RM looks the number up in the calling
 * process -- the backend, which holds every guest process's control files
 * -- so a number that is not translated names someone else's file there.
 */
#define NVGPU_RM_EXPORT_OBJECT_TO_FD 0x00003d05
#define NVGPU_RM_IMPORT_OBJECT_FROM_FD 0x00003d06
#define NVGPU_RM_GET_EXPORT_OBJECT_INFO 0x00003d08
#define NVGPU_RM_CREATE_EXPORT_OBJECT_FD 0x00003d0a
#define NVGPU_RM_EXPORT_OBJECTS_TO_FD 0x00003d0b
#define NVGPU_RM_IMPORT_OBJECTS_FROM_FD 0x00003d0c

/*
 * Where one of those keeps its descriptor, or -1: after the 16-byte object
 * for EXPORT_OBJECT_TO_FD, after hDevice, maxObjects and 64 bytes of
 * metadata (aligned to 4) for CREATE_EXPORT_OBJECT_FD, first for the rest.
 */
static int nvgpu_rm_unix_fd_offset(u32 ctl_cmd) {
  switch (ctl_cmd) {
  case NVGPU_RM_EXPORT_OBJECT_TO_FD:
    return 16;
  case NVGPU_RM_CREATE_EXPORT_OBJECT_FD:
    return 72;
  case NVGPU_RM_IMPORT_OBJECT_FROM_FD:
  case NVGPU_RM_GET_EXPORT_OBJECT_INFO:
  case NVGPU_RM_EXPORT_OBJECTS_TO_FD:
  case NVGPU_RM_IMPORT_OBJECTS_FROM_FD:
    return 0;
  default:
    return -1;
  }
}

/* Largest second-level buffer we will carry for one call. */
#define NVGPU_DEEP_MAX (64 * 1024)

/* Event classes whose allocation parameters name a file of the caller's.
 * NV0005_ALLOC_PARAMETERS is {hParentClient, hSrcResource, hClass,
 * notifyIndex, data}, with `data` 8-byte aligned at 16. */
#define NVGPU_CLASS_EVENT 0x05
#define NVGPU_CLASS_EVENT_OS_EVENT 0x79
#define NVGPU_NV0005_DATA_OFFSET 16

/*
 * Parameters that name an OS event -- the descriptor ALLOC_OS_EVENT was
 * given -- as a u64 `notificationHandle`, which RM looks up by (fd, hClient)
 * in the calling process (os.c:1789-1815): a semaphore surface's
 * REGISTER_WAITER at 24 and UNREGISTER_WAITER at 16 (ctrl00da.h:207-212,
 * 251-255), and NV_EVENT_BUFFER's allocation at 40 (cl90cd.h:164-180).
 */
#define NVGPU_RM_SEMSURF_REGISTER_WAITER 0x00da0003
#define NVGPU_RM_SEMSURF_UNREGISTER_WAITER 0x00da0005
#define NVGPU_CLASS_EVENT_BUFFER 0x90cd
#define NVGPU_EVENT_BUFFER_NOTIFICATION_OFFSET 40

/* ───────── NVIDIA ioctl parameter structs ───────── */

struct NVOS54_PARAMETERS {
  __le32 hClient;
  __le32 hObject;
  __le32 cmd;
  __le32 flags;
  __le64 params; /* pointer to sub-command data in guest VA */
  __le32 paramsSize;
  __le32 status;
} __packed;

struct NVOS64_PARAMETERS {
  __le32 hRoot;
  __le32 hObjectParent;
  __le32 hObjectNew;
  __le32 hClass;
  __le64 pAllocParms;      /* pointer to class-specific alloc params */
  __le64 pRightsRequested; /* usually NULL */
  __le32 paramsSize;
  __le32 flags;
  __le32 status;
  /*
   * Tail padding, and it is part of the ABI rather than an artefact.
   * NVIDIA's NVOS64_PARAMETERS is naturally aligned, and its NvP64 members
   * give the struct 8-byte alignment, so the compiler rounds 44 up to 48.
   * __packed here removed that, and the guest sent 44-byte RM_ALLOCs to a
   * host driver expecting 48 -- confirmed against a capture of 463 calls on
   * 615.71.09, every one of them 48 bytes.
   */
  __le32 reserved;
} __packed;

static_assert(sizeof(struct NVOS64_PARAMETERS) == 48,
              "RM_ALLOC parameter struct must match the host driver ABI");

/*
 * nvgpu_ioctl_simple — flat struct, no embedded pointers. `pre`: the block,
 * already copied in whole by a caller that decided on it (NULL: read here).
 */
static long nvgpu_ioctl_simple(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg, unsigned int sz,
                               const void *pre) {
  /* RM_DUP_OBJECT carries the calling process after the struct. */
  bool proc = _IOC_TYPE(cmd) == 'F' && _IOC_NR(cmd) == NV_ESC_RM_DUP_OBJECT &&
              nvgpu_proc_ids(nfd->dev);
  int req_total = sizeof(struct nvgpu_ioctl_req) + sz +
                  (proc ? sizeof(struct nvgpu_proc_id) : 0);
  int resp_max = sizeof(struct nvgpu_ioctl_resp) + sz;
  void *req_buf, *resp_buf;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  u32 used, data_len;
  int ret;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sz);
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  if (sz > 0) {
    if (pre)
      memcpy(req_buf + sizeof(*req), pre, sz);
    else if (copy_from_user(req_buf + sizeof(*req), uarg, sz)) {
      ret = -EFAULT;
      goto out;
    }
  }
  if (proc)
    nvgpu_proc_id_fill(nfd->dev, req_buf + sizeof(*req) + sz);

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /*
   * Only what the device wrote. A failed call comes back as a bare header,
   * and before the used length was kept the bytes after it were whatever the
   * kmalloc'd buffer held -- guest kernel heap, copied out to userspace.
   */
  data_len = nvgpu_resp_has(used, 0, sizeof(*resp))
                 ? le32_to_cpu(resp->data_len)
                 : 0;
  if (sz > 0 && data_len && data_len <= sz &&
      nvgpu_resp_has(used, sizeof(*resp), data_len)) {
    if (copy_to_user(uarg, resp_buf + sizeof(*resp), data_len))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * An OS event named by descriptor in a u64 of RM's parameters at `slot`.
 *
 * The backend's RM looks the event up by the descriptor in *its* table, so,
 * as for every other descriptor we forward, the number the backend is sent
 * is the handle it issued for our file, and the backend turns it into its
 * own descriptor. Left as the guest's number, a waiter found no event (or
 * another of the client's), and an NV_EVENT_BUFFER allocation -- which does
 * not fail when the lookup does, but keeps the raw number as the event
 * pointer (event_buffer.c:463-477) -- was a host oops waiting to happen; the
 * backend refuses one now, and this makes the honest caller's call work.
 * Zero is "no notification" and passes as it is. The caller's value is
 * saved in `*saved`, for the reply.
 */
static int nvgpu_rm_os_event_in(struct nvgpu_device *dev, void *slot,
                                u64 *saved) {
  u32 handle;
  u64 v;

  memcpy(&v, slot, sizeof(v));
  *saved = v;
  if (!v)
    return 0;
  if (v > INT_MAX || nvgpu_handle_for_fd(dev, (int)v, &handle))
    return -EBADF;
  v = handle;
  memcpy(slot, &v, sizeof(v));
  return 0;
}

/* Where an RM control keeps an OS event (see above), or -1. */
static int nvgpu_rm_os_event_offset(u32 ctl_cmd) {
  switch (ctl_cmd) {
  case NVGPU_RM_SEMSURF_REGISTER_WAITER:
    return 24;
  case NVGPU_RM_SEMSURF_UNREGISTER_WAITER:
    return 16;
  default:
    return -1;
  }
}

/*
 * nvgpu_ioctl_rm_control — NV_ESC_RM_CONTROL with nested params buffer.
 *
 * Handles three cases:
 *   1. Normal: nested params are flat data → marshal and forward
 *   2. Nested params hold a pointer of their own → send what it points at
 *      alongside, and let the backend give it a host address
 *   3. paramsSize == 0 or params == NULL → forward outer struct only
 */
/*
 * Commands that carry a second-level pointer and have no V2 twin.
 *
 * The generated table is built by pairing a V1 command with a V2 one in
 * NVIDIA's headers, because that is what the old rewrite needed. Carrying the
 * pointer instead of rewriting the command made that criterion wrong: what
 * matters now is only whether a parameter block holds an NvP64, and a command
 * with no V2 variant holds one just the same. Those are invisible to the
 * generator, so they are listed here by hand.
 *
 * Only `v1_cmd`, `v1_userptr_offset` and `info_style` are read; the V2 fields
 * are dead and left zero.
 */
static const struct nvgpu_v1v2_entry nvgpu_deep_only_table[] = {
    /*
     * NV0041_CTRL_CMD_GET_SURFACE_INFO — {u32 surfaceInfoListSize, pad,
     * NvP64 surfaceInfoList}, entries of NVXXXX_CTRL_XXX_INFO {index, data},
     * eight bytes each.
     *
     * The ICD asks this straight after exporting NVKMS memory, to learn the
     * surface's attributes. Forwarded with the guest's own pointer still in
     * it, RM answers NV_ERR_INVALID_ADDRESS (0x1e), and the only thing the
     * caller reports is vkGetMemoryFdPropertiesKHR returning VK_ERROR_UNKNOWN
     * several layers up.
     */
    {0x00410110, 0, 0, 8, 0, 0, 0, true},
};

static const struct nvgpu_v1v2_entry *nvgpu_find_deep_rewrite(u32 cmd) {
  const struct nvgpu_v1v2_entry *rw = nvgpu_find_v1v2_rewrite(cmd);
  int i;

  if (rw)
    return rw;
  for (i = 0; i < (int)ARRAY_SIZE(nvgpu_deep_only_table); i++)
    if (nvgpu_deep_only_table[i].v1_cmd == cmd)
      return &nvgpu_deep_only_table[i];
  return NULL;
}

/*
 * Deep segments: several pointers of one parameter block, each sent with
 * what it addresses (NVGPU_DEEP_SEGMENTED, nvgpu_wire.h).
 *
 * One deep block carries one pointer, and some calls hold more, every one of
 * which RM follows. FIFO_GET_CHANNELLIST copies numChannels handles in through
 * one list and channel ids both ways through another; cuCtxCreate asks for it,
 * and with one list zeroed RM answers NV_ERR_INVALID_ARGUMENT. The table of
 * such controls, and how much RM copies through each pointer, is generated
 * from RM's own sources with the backend's (gen/nvgpu_rm_deep.h); the backend
 * computes every size again from what it is sent and refuses the call if one
 * differs, so this side only has to be right, not trusted.
 */
struct nvgpu_deep_plan {
  u32 n;
  u32 bytes; /* the deep block: header, table and segments */
  struct {
    u64 uptr;
    u32 ptr; /* offset of the pointer in the block holding it */
    u32 len;
    u8 flags; /* NVGPU_RM_DEEP_IN / _OUT */
  } seg[NVGPU_DEEP_SEGS_MAX];
};

static bool nvgpu_deep_segs_ok(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_DEEP_SEGS);
}

/*
 * How much RM copies through pointer `p` of the `len`-byte block `blk`, as
 * RM computes it: the counts multiplied in NvU32, which wraps, then by the
 * element size, which RM checks (portSafeMulU32). The backend's copy of this
 * is DeepPtr::size in gen/src/rmctrl/mod.rs. False for a count outside the
 * block or an overflow.
 */
static bool nvgpu_rm_deep_size(const struct nvgpu_rm_deep_ptr *p,
                               const u8 *blk, u32 len, u32 *size) {
  u32 n = p->scale;
  unsigned int i;

  for (i = 0; i < p->ncounts && i < NVGPU_RM_DEEP_COUNTS_MAX; i++) {
    const struct nvgpu_rm_deep_count *c = &p->counts[i];
    u32 v;

    if ((u32)c->offset + c->width > len)
      return false;
    switch (c->width) {
    case 1:
      v = blk[c->offset];
      break;
    case 2:
      v = get_unaligned_le16(blk + c->offset);
      break;
    case 4:
      v = get_unaligned_le32(blk + c->offset);
      break;
    default:
      return false;
    }
    n *= v;
  }
  return !check_mul_overflow(n, p->elem, size);
}

/*
 * One segment for each pointer of `ctl` the caller set in `blk`, of RM's
 * size for it. A pointer whose size is not known, or is zero, gets none and
 * is zeroed by the backend, as every pointer was before. Past
 * NVGPU_DEEP_SEGS_MAX_BYTES in all, nothing is planned.
 */
static void nvgpu_deep_plan(const struct nvgpu_rm_deep_control *ctl,
                            const u8 *blk, u32 len,
                            struct nvgpu_deep_plan *plan) {
  u32 i, total = 0;

  plan->n = 0;
  plan->bytes = 0;
  for (i = 0; i < ctl->nptrs && i < NVGPU_RM_DEEP_PTRS_MAX; i++) {
    const struct nvgpu_rm_deep_ptr *p = &ctl->ptrs[i];
    u64 uptr;
    u32 size;

    /* A pointer this release's block does not have. */
    if ((u32)p->ptr + sizeof(u64) > len)
      continue;
    uptr = get_unaligned_le64(blk + p->ptr);
    if (!uptr || !nvgpu_rm_deep_size(p, blk, len, &size) || !size)
      continue;
    if (size > NVGPU_DEEP_SEGS_MAX_BYTES - total ||
        plan->n == NVGPU_DEEP_SEGS_MAX) {
      plan->n = 0;
      return;
    }
    total += size;
    plan->seg[plan->n].uptr = uptr;
    plan->seg[plan->n].ptr = p->ptr;
    plan->seg[plan->n].len = size;
    plan->seg[plan->n].flags = p->flags;
    plan->n++;
  }
  if (plan->n)
    plan->bytes = sizeof(struct nvgpu_deep_seg_hdr) +
                  plan->n * sizeof(struct nvgpu_deep_seg) + total;
}

/*
 * Lay the planned deep block out at `dst` (plan->bytes). Every segment goes
 * with the caller's bytes, a buffer RM only writes too: the reply carries
 * each back as RM left it, so one RM did not get to write -- a call that
 * failed first -- is copied back to the caller unchanged, as natively.
 */
static int nvgpu_deep_fill(const struct nvgpu_deep_plan *plan, u8 *dst) {
  struct nvgpu_deep_seg_hdr *h = (struct nvgpu_deep_seg_hdr *)dst;
  struct nvgpu_deep_seg *t = (struct nvgpu_deep_seg *)(h + 1);
  u8 *at = (u8 *)(t + plan->n);
  u32 i;

  h->count = cpu_to_le32(plan->n);
  h->reserved = 0;
  for (i = 0; i < plan->n; i++) {
    t[i].ptr_offset = cpu_to_le32(plan->seg[i].ptr);
    t[i].len = cpu_to_le32(plan->seg[i].len);
  }
  for (i = 0; i < plan->n; i++) {
    if (copy_from_user(at, u64_to_user_ptr(plan->seg[i].uptr),
                       plan->seg[i].len))
      return -EFAULT;
    at += plan->seg[i].len;
  }
  return 0;
}

/*
 * Copy back each segment RM writes, from `src`, the reply's deep block of
 * `len` bytes. One not laid out as ours was is not read at all.
 */
static int nvgpu_deep_copy_back(const struct nvgpu_deep_plan *plan,
                                const u8 *src, u32 len) {
  u32 i, at;

  if (len != plan->bytes)
    return 0;
  at = sizeof(struct nvgpu_deep_seg_hdr) +
       plan->n * sizeof(struct nvgpu_deep_seg);
  for (i = 0; i < plan->n; i++) {
    if ((plan->seg[i].flags & NVGPU_RM_DEEP_OUT) &&
        copy_to_user(u64_to_user_ptr(plan->seg[i].uptr), src + at,
                     plan->seg[i].len))
      return -EFAULT;
    at += plan->seg[i].len;
  }
  return 0;
}

/*
 * GPU/CPU time correlation, rebased: the CPU half of each sample is read on
 * the host, in the host's clock (nvgpu_rm_intercepts.h has the layout), and
 * the caller correlates the GPU's timer with its own clock of that id --
 * glcore asks for OSTIME at 0xa5d94d and 0xa6c54c. The host's realtime and
 * raw clocks are not the guest's: a guest booted later, or with its own NTP,
 * disagrees by anything from microseconds to its whole uptime. Each value is
 * moved into the guest's clock of the same id (nvgpu_host_clock_to_guest);
 * OSTIME is microseconds of realtime, PLATFORM_API nanoseconds of raw
 * monotonic, and fills only samples[0] whatever sampleCount says. Left
 * alone for a GSP-side clock, on a failed RM status, and against a backend
 * too old to report its other clocks.
 */
static void nvgpu_rebase_time_correlation(struct nvgpu_device *dev, u8 *p,
                                          u32 len) {
  clockid_t clk;
  u32 i, n, scale;
  u8 id;

  if (len < NVGPU_TCI_SAMPLES)
    return;
  id = p[NVGPU_TCI_CLK_ID];
  n = min_t(u32, p[NVGPU_TCI_SAMPLE_COUNT], NVGPU_TCI_MAX_SAMPLES);
  if (NVGPU_TCI_PROC(id) != NVGPU_TCI_PROC_CPU)
    return;
  switch (NVGPU_TCI_SRC(id)) {
  case NVGPU_TCI_SRC_OSTIME:
    clk = CLOCK_REALTIME;
    scale = NSEC_PER_USEC;
    break;
  case NVGPU_TCI_SRC_PLATFORM_API:
    clk = CLOCK_MONOTONIC_RAW;
    scale = 1;
    n = min_t(u32, n, 1);
    break;
  default:
    return;
  }

  for (i = 0; i < n; i++) {
    u32 at = NVGPU_TCI_SAMPLES + i * NVGPU_TCI_SAMPLE_SIZE;
    s64 guest;

    if (at + sizeof(u64) > len)
      return;
    if (!nvgpu_host_clock_to_guest(
            dev, clk, (s64)(get_unaligned_le64(p + at) * scale), &guest))
      return;
    if (guest < 0)
      guest = 0;
    put_unaligned_le64(div_u64((u64)guest, scale), p + at);
  }
}

static long nvgpu_ioctl_rm_control(struct nvgpu_fd *nfd, unsigned int cmd,
                                   void __user *uarg, unsigned int sz) {
  struct NVOS54_PARAMETERS params;
  void __user *user_nested;
  u32 nested_size;
  u32 ctl_cmd;
  const struct nvgpu_v1v2_entry *rw;

  /* A descriptor named inside the nested block, and where it sits. */
  int nested_fd = -1;
  u32 nested_fd_offset = 0;
  int unix_fd_off;

  /* An OS event named inside it (nvgpu_rm_os_event_in), and where. */
  int os_event_off = -1;
  u64 os_event_val = 0;

  /* Second-level pointer carried alongside the nested block. */
  u64 deep_user_ptr = 0;
  u32 deep_ptr_offset = 0;
  u32 deep_len = 0;

  /* Or several, as deep segments, and the nested block they were sized from. */
  const struct nvgpu_rm_deep_control *ctl_deep = NULL;
  struct nvgpu_deep_plan plan = {0};
  u8 *nested_copy = NULL;

  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  bool proc;
  u32 used;

  if (sz < sizeof(params))
    return -EINVAL;

  if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;

  user_nested = (void __user *)(unsigned long)le64_to_cpu(params.params);
  nested_size = le32_to_cpu(params.paramsSize);
  ctl_cmd = le32_to_cpu(params.cmd);

  if (nested_size > 1024 * 1024)
    return -EINVAL;

  /* Intercept multi-pointer commands that can't be forwarded */
  {
    long intercept_ret;
    if (nvgpu_try_intercept_rm_control(nfd, ctl_cmd, uarg, user_nested,
                                       nested_size, nfd->dev->driver_version,
                                       &intercept_ret))
      return intercept_ret;
  }

  /*
   * A size with no parameters. RM refuses it as it copies the parameters in
   * (param_copy.c rmapiParamsAcquire: NV_ERR_INVALID_ARGUMENT, in the
   * struct, the ioctl itself succeeding), so that is the answer here too.
   * Sent on, the request carried nested_size bytes this never wrote --
   * uninitialised guest kernel heap -- and the backend, finding a block,
   * handed RM those bytes as the parameters.
   */
  if (!user_nested && nested_size)
    return nvgpu_set_nvos54_status(uarg, NVGPU_NV_ERR_INVALID_ARGUMENT);

  /*
   * A second-level pointer, carried rather than rewritten.
   *
   * Some parameter blocks hold an NvP64 pointing at a buffer of the caller's.
   * This used to be handled by swapping the command for an inline "V2"
   * variant that has no pointer. That bound us to the struct layouts of one
   * driver release: against any other it sent requests of the wrong size and
   * shape, and some V2 variants are not served at all, which is what ended
   * every Vulkan run here -- RM answered NV_ERR_INVALID_ARGUMENT to a command
   * userspace had never asked for.
   *
   * The backend already solves this one level up: it allocates a host buffer
   * for the top-level pointer, copies the guest's bytes in, points the struct
   * at it, and copies the result back. Doing the same one level deeper sends
   * the caller's own command through untouched, and needs to know only where
   * the pointer sits and how much it addresses. Both are properties of the
   * layout that carries the pointer, which is the stable one.
   */
  /*
   * The nested block, read once: what every decision below is taken on --
   * the TSC refusal, the V1V2 count and pointer, the deep segments' sizes,
   * the descriptors in it -- and what is sent, whatever another thread of
   * the caller writes meanwhile. (The TSC clock byte and the V1V2 block were
   * each read apart from the bytes that were then sent.)
   */
  if (user_nested && nested_size > 0) {
    nested_copy = kmalloc(nested_size, GFP_KERNEL);
    if (!nested_copy)
      return -ENOMEM;
    if (copy_from_user(nested_copy, user_nested, nested_size)) {
      kfree(nested_copy);
      return -EFAULT;
    }
  }

  /* A host TSC reading means nothing in the guest (nvgpu_rm_intercepts.h). */
  if (ctl_cmd == NVGPU_RM_TIME_CORRELATION && nested_copy &&
      nvgpu_tci_is_tsc(nested_copy[NVGPU_TCI_CLK_ID])) {
    kfree(nested_copy);
    return nvgpu_set_nvos54_status(uarg, NVGPU_NV_ERR_NOT_SUPPORTED);
  }

  rw = nested_copy ? nvgpu_find_deep_rewrite(ctl_cmd) : NULL;

  /*
   * Several pointers, each with what it addresses (nvgpu_deep_plan): the
   * sizes the backend checks are computed from the same bytes they were
   * planned from.
   */
  if (nested_copy && nvgpu_deep_segs_ok(nfd->dev))
    ctl_deep = nvgpu_rm_deep_find(ctl_cmd);
  if (ctl_deep) {
    rw = NULL;
    nvgpu_deep_plan(ctl_deep, nested_copy, nested_size, &plan);
    if (plan.n) {
      deep_ptr_offset = NVGPU_DEEP_SEGMENTED;
      deep_len = plan.bytes;
    }
  }

  if (rw && nested_size >= rw->v1_userptr_offset + 8) {
    u32 count;

    memcpy(&deep_user_ptr, nested_copy + rw->v1_userptr_offset, sizeof(u64));
    memcpy(&count, nested_copy, sizeof(u32));

    /*
     * The leading field says how much the buffer holds: entries of eight
     * bytes for the list-style commands, plain bytes for the caps tables.
     */
    if (!rw->info_style)
      deep_len = count;
    else if (check_mul_overflow(count, 8u, &deep_len))
      deep_len = U32_MAX; /* too large, below: no deep block */
    deep_ptr_offset = rw->v1_userptr_offset;

    if (!deep_user_ptr || deep_len == 0 || deep_len > NVGPU_DEEP_MAX) {
      deep_user_ptr = 0;
      deep_ptr_offset = 0;
      deep_len = 0;
    }
  }

  /* ── Normal path (no V1→V2 rewrite) ── */

  /* The calling process after the blocks (nvgpu_proc_euid). */
  proc = nvgpu_proc_euid(nfd->dev);
  req_total = sizeof(struct nvgpu_ioctl_req) + sizeof(params) + nested_size +
              deep_len + (proc ? sizeof(struct nvgpu_proc_id) : 0);
  resp_max =
      sizeof(struct nvgpu_ioctl_resp) + sizeof(params) + nested_size + deep_len;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sizeof(params));
  req->nested_offset = cpu_to_le32(sizeof(params));
  req->nested_len = cpu_to_le32(nested_size);
  req->deep_ptr_offset = cpu_to_le32(deep_ptr_offset);
  req->deep_len = cpu_to_le32(deep_len);

  memcpy(req_buf + sizeof(*req), &params, sizeof(params));

  if (nested_copy) {
    memcpy(req_buf + sizeof(*req) + sizeof(params), nested_copy, nested_size);

    /*
     * Exporting objects to a descriptor, importing them back and asking
     * about an export name another of our open files inside the nested
     * parameters. The backend knows that file by the handle it issued, not
     * by our descriptor number, so swap one for the other here and swap it
     * back on the way out -- userspace gets its own descriptor returned,
     * which is what it passed in.
     *
     * A number that is not one of our files is refused here, never sent as
     * it is: the backend reads the field as one of its handles, and handles
     * are issued in order, so a small number is likely another process's
     * control file -- and RM would import that process's exported memory
     * into the caller's client. -1, which RM refuses itself (os.c), is the
     * one value that passes unchanged.
     */
    unix_fd_off = nvgpu_rm_unix_fd_offset(ctl_cmd);
    if (unix_fd_off >= 0 && nested_size >= unix_fd_off + sizeof(u32)) {
      void *slot = req_buf + sizeof(*req) + sizeof(params) + unix_fd_off;
      u32 handle;

      memcpy(&nested_fd, slot, sizeof(nested_fd));
      if (nested_fd != -1) {
        if (nvgpu_handle_for_fd(nfd->dev, nested_fd, &handle)) {
          dev_warn_ratelimited(&nfd->dev->vdev->dev,
                               "virtio-gpu-nv: RM control 0x%x names fd %d, "
                               "which is not one of our devices\n",
                               ctl_cmd, nested_fd);
          ret = -EBADF;
          goto out;
        }
        memcpy(slot, &handle, sizeof(handle));
        nested_fd_offset = unix_fd_off;
      }
    }

    /*
     * A parameter block too short to hold the field is the backend's to
     * refuse; only one that holds it is translated.
     */
    os_event_off = nvgpu_rm_os_event_offset(ctl_cmd);
    if (os_event_off >= 0 && nested_size >= os_event_off + sizeof(u64)) {
      ret = nvgpu_rm_os_event_in(nfd->dev,
                                 req_buf + sizeof(*req) + sizeof(params) +
                                     os_event_off,
                                 &os_event_val);
      if (ret) {
        dev_warn_ratelimited(&nfd->dev->vdev->dev,
                             "virtio-gpu-nv: RM control 0x%x names OS event "
                             "0x%llx, which is not one of our devices\n",
                             ctl_cmd, os_event_val);
        goto out;
      }
    } else {
      os_event_off = -1;
    }
  }

  if (plan.n) {
    ret = nvgpu_deep_fill(&plan,
                          req_buf + sizeof(*req) + sizeof(params) + nested_size);
    if (ret)
      goto out;
  } else if (deep_len > 0) {
    if (copy_from_user(req_buf + sizeof(*req) + sizeof(params) + nested_size,
                       (const void __user *)deep_user_ptr, deep_len)) {
      ret = -EFAULT;
      goto out;
    }
  }
  if (proc)
    nvgpu_proc_id_fill(nfd->dev, req_buf + sizeof(*req) + sizeof(params) +
                                     nested_size + deep_len);

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /*
   * RM's own verdict is in params.status, so a successful reply always
   * carries the struct back. A failed one is a bare header: nothing to copy,
   * and the caller's struct is left as it was rather than overwritten.
   */
  if (!nvgpu_resp_has(used, sizeof(*resp), sizeof(params)))
    goto out;
  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(params))) {
    ret = -EFAULT;
    goto out;
  }

  if (user_nested && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));

    if (!nvgpu_resp_has(used, sizeof(*resp) + sizeof(params), copy_back))
      goto out;
    if (nested_fd >= 0 && copy_back >= nested_fd_offset + sizeof(u32))
      memcpy(resp_buf + sizeof(*resp) + sizeof(params) + nested_fd_offset,
             &nested_fd, sizeof(nested_fd));
    if (os_event_off >= 0 && copy_back >= os_event_off + sizeof(u64))
      memcpy(resp_buf + sizeof(*resp) + sizeof(params) + os_event_off,
             &os_event_val, sizeof(os_event_val));
    if (ctl_cmd == NVGPU_RM_TIME_CORRELATION &&
        get_unaligned_le32(resp_buf + sizeof(*resp) + 28) == 0 /* NV_OK */)
      nvgpu_rebase_time_correlation(
          nfd->dev, resp_buf + sizeof(*resp) + sizeof(params), copy_back);

    if (copy_to_user(user_nested, resp_buf + sizeof(*resp) + sizeof(params),
                     copy_back))
      ret = -EFAULT;
  }

  if (plan.n && le32_to_cpu(resp->deep_len) > 0) {
    /* Each segment RM writes goes back to its own pointer. */
    u32 back = le32_to_cpu(resp->deep_len);
    size_t at = sizeof(*resp) + sizeof(params) +
                (size_t)le32_to_cpu(resp->nested_len);

    if (nvgpu_resp_has(used, at, back) &&
        nvgpu_deep_copy_back(&plan, resp_buf + at, back))
      ret = -EFAULT;
  } else if (deep_len > 0 && le32_to_cpu(resp->deep_len) > 0) {
    u32 copy_back = min(deep_len, le32_to_cpu(resp->deep_len));
    size_t at = sizeof(*resp) + sizeof(params) +
                (size_t)le32_to_cpu(resp->nested_len);

    if (nvgpu_resp_has(used, at, copy_back) &&
        copy_to_user((void __user *)deep_user_ptr, resp_buf + at, copy_back))
      ret = -EFAULT;
  }

out:
  kfree(nested_copy);
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * NV_ESC_RM_IDLE_CHANNELS: NVOS30 names three arrays of handles, which RM
 * reads only for a channel list -- flags' CHANNEL field LIST, and a nonzero
 * count (RmDeprecatedIdleChannels). The Vulkan and GL drivers idle a list of
 * three as a device goes. A list goes with its arrays as deep segments, to a
 * backend that takes them; anything else goes as the flat block it is, and
 * the backend zeroes the pointers of the one-channel form and refuses a list
 * it was not sent the arrays of.
 */
static long nvgpu_ioctl_idle_channels(struct nvgpu_fd *nfd, unsigned int cmd,
                                      void __user *uarg, unsigned int sz) {
  const struct nvgpu_rm_deep_control *rule = &nvgpu_rm_deep_idle_channels;
  u8 params[NVGPU_RM_IDLE_CHANNELS_SIZE];
  struct nvgpu_deep_plan plan;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  void *req_buf = NULL, *resp_buf = NULL;
  u32 channel, count, used, data_len;
  size_t req_total, resp_max;
  int ret;

  if (sz != sizeof(params) || !nvgpu_deep_segs_ok(nfd->dev))
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz, NULL);
  /* Read once: the flat form, if it comes to that, sends these bytes. */
  if (copy_from_user(params, uarg, sizeof(params)))
    return -EFAULT;
  channel = (get_unaligned_le32(params + NVGPU_RM_IDLE_CHANNELS_FLAGS) >>
             NVGPU_RM_IDLE_CHANNELS_LIST_LO) &
            GENMASK(NVGPU_RM_IDLE_CHANNELS_LIST_HI -
                        NVGPU_RM_IDLE_CHANNELS_LIST_LO,
                    0);
  count = get_unaligned_le32(params + rule->ptrs[0].counts[0].offset);
  if (channel != NVGPU_RM_IDLE_CHANNELS_LIST || !count ||
      count > NVGPU_IDLE_CHANNELS_MAX)
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz, params);
  nvgpu_deep_plan(rule, params, sizeof(params), &plan);
  if (!plan.n)
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz, params);

  req_total = sizeof(*req) + sizeof(params) + plan.bytes;
  resp_max = sizeof(*resp) + sizeof(params);
  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sizeof(params));
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = cpu_to_le32(NVGPU_DEEP_SEGMENTED);
  req->deep_len = cpu_to_le32(plan.bytes);
  memcpy(req_buf + sizeof(*req), params, sizeof(params));
  ret = nvgpu_deep_fill(&plan, req_buf + sizeof(*req) + sizeof(params));
  if (ret)
    goto out;

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }
  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* RM only reads the arrays: the block, with RM's status, is all that
   * comes back. */
  data_len = nvgpu_resp_has(used, 0, sizeof(*resp))
                 ? le32_to_cpu(resp->data_len)
                 : 0;
  if (data_len && data_len <= sizeof(params) &&
      nvgpu_resp_has(used, sizeof(*resp), data_len) &&
      copy_to_user(uarg, resp_buf + sizeof(*resp), data_len))
    ret = -EFAULT;

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * nvgpu_ioctl_rm_alloc — NV_ESC_RM_ALLOC, same pattern via NVOS64_PARAMETERS.
 *
 * Subtlety: when paramsSize == 0 but pAllocParms != NULL, the host RM
 * driver determines size from hClass.  We must look up the size ourselves
 * so we know how many bytes to copy_from_user.
 */
static long nvgpu_ioctl_rm_alloc(struct nvgpu_fd *nfd, unsigned int cmd,
                                 void __user *uarg, unsigned int sz,
                                 const void *pre) {
  struct NVOS64_PARAMETERS params;
  void __user *user_alloc;
  u32 nested_size;
  void *req_buf = NULL, *resp_buf = NULL, *nested;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  /* The event descriptor NV0005 names, once swapped for a handle. */
  int event_fd = -1;
  u32 used;
  /* NV_EVENT_BUFFER's OS event, translated, and the caller's value. */
  bool os_event = false;
  u64 os_event_val = 0;
  /* paramsSize as the caller wrote it, when it is not what is sent. */
  u32 caller_psize = 0;

  if (sz < sizeof(params))
    return -EINVAL;

  if (pre)
    memcpy(&params, pre, sizeof(params));
  else if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;

  user_alloc =(void __user *)(unsigned long)le64_to_cpu(params.pAllocParms);
  nested_size = le32_to_cpu(params.paramsSize);

  /*
   * When paramsSize == 0 but pAllocParms is non-NULL,
   * the host RM driver knows the size from hClass.  We need to copy
   * that many bytes from guest userspace so the VMM can forward them.
   */
  /*
   * No parameters: RM sizes them from the class (rmapiParamsCopyInit), and
   * with a NULL pointer takes none -- or refuses the class that needs them
   * -- whatever paramsSize says. So none are sent, and the host is told a
   * size of 0 (the caller's comes back as it was): sent on, the request
   * carried paramsSize bytes this never wrote -- uninitialised guest kernel
   * heap -- which the backend handed RM as the parameters.
   */
  if (!user_alloc && nested_size) {
    caller_psize = params.paramsSize;
    params.paramsSize = 0;
    nested_size = 0;
  }
  if (user_alloc && nested_size == 0) {
    u32 hClass = le32_to_cpu(params.hClass);
    nested_size = nvgpu_rmalloc_class_param_size(hClass);
    pr_debug(
        "virtio-gpu-nv: RM_ALLOC hClass=0x%04x paramsSize=0 → copy %u bytes\n",
        hClass, nested_size);
  }

  if (nested_size > 1024 * 1024)
    return -EINVAL;

  /* The calling process after the blocks: a client made here is its. */
  req_total = sizeof(*req) + sizeof(params) + nested_size +
              (nvgpu_proc_ids(nfd->dev) ? sizeof(struct nvgpu_proc_id) : 0);
  resp_max = sizeof(struct nvgpu_ioctl_resp) + sizeof(params) + nested_size;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sizeof(params));
  req->nested_offset = cpu_to_le32(sizeof(params));
  req->nested_len = cpu_to_le32(nested_size);
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  memcpy(req_buf + sizeof(*req), &params, sizeof(params));
  if (nvgpu_proc_ids(nfd->dev))
    nvgpu_proc_id_fill(nfd->dev,
                       req_buf + sizeof(*req) + sizeof(params) + nested_size);

  if (user_alloc && nested_size > 0) {
    nested = req_buf + sizeof(*req) + sizeof(params);
    if (copy_from_user(nested, user_alloc, nested_size)) {
      ret = -EFAULT;
      goto out;
    }

    /*
     * An event object names the file the event will be delivered on, and it
     * names it inside these parameters rather than at a fixed place in the
     * ioctl -- so the translation the device publishes for whole ioctls never
     * sees it, and the backend was handed a descriptor number that means
     * nothing in its process. RM looks it up, finds no event registered under
     * it, and answers NV_ERR_OBJECT_NOT_FOUND.
     *
     * Userspace reports that as "Failed to allocate semaphore event" and
     * abandons the device, which is what ended every run here after
     * enumeration started working: four allocations of these two classes fail,
     * and nothing else in the run does.
     *
     * NV0005_ALLOC_PARAMETERS keeps the descriptor in `data` at offset 16.
     * Rewrite it the way the fixed-position path does: to the handle the
     * backend issued for that file, which the backend turns back into one of
     * its own descriptors.
     */
    {
      u32 hclass = le32_to_cpu(params.hClass);

      if ((hclass == NVGPU_CLASS_EVENT || hclass == NVGPU_CLASS_EVENT_OS_EVENT) &&
          nested_size >= NVGPU_NV0005_DATA_OFFSET + sizeof(u32)) {
        memcpy(&event_fd, nested + NVGPU_NV0005_DATA_OFFSET, sizeof(event_fd));
        /* -1 is "no descriptor"; any other negative is refused, as below. */
        if (event_fd < -1) {
          ret = -EBADF;
          goto out;
        }
        if (event_fd >= 0) {
          u32 handle;

          /*
           * The descriptor is the one the event will be delivered on, so it
           * is always one of our devices. Anything else would reach the
           * backend as a number it reads as a handle of its own.
           */
          if (nvgpu_handle_for_fd(nfd->dev, event_fd, &handle)) {
            dev_warn_ratelimited(&nfd->dev->vdev->dev,
                                 "virtio-gpu-nv: RM_ALLOC of event class 0x%x "
                                 "names fd %d, which is not one of our "
                                 "devices\n",
                                 hclass, event_fd);
            ret = -EBADF;
            goto out;
          }
          memcpy(nested + NVGPU_NV0005_DATA_OFFSET, &handle, sizeof(handle));
        }
      }

      /* NV_EVENT_BUFFER's OS event, as for a waiter's (see there). */
      if (hclass == NVGPU_CLASS_EVENT_BUFFER &&
          nested_size >=
              NVGPU_EVENT_BUFFER_NOTIFICATION_OFFSET + sizeof(u64)) {
        ret = nvgpu_rm_os_event_in(nfd->dev, nested +
                                       NVGPU_EVENT_BUFFER_NOTIFICATION_OFFSET,
                                   &os_event_val);
        if (ret) {
          dev_warn_ratelimited(&nfd->dev->vdev->dev,
                               "virtio-gpu-nv: NV_EVENT_BUFFER names OS event "
                               "0x%llx, which is not one of our devices\n",
                               os_event_val);
          goto out;
        }
        os_event = true;
      }
    }
  }

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* As for RM_CONTROL: a failed reply is a bare header, nothing to copy. */
  if (!nvgpu_resp_has(used, sizeof(*resp), sizeof(params)))
    goto out;
  if (caller_psize)
    memcpy(resp_buf + sizeof(*resp) +
               offsetof(struct NVOS64_PARAMETERS, paramsSize),
           &caller_psize, sizeof(caller_psize));
  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(params))) {
    ret = -EFAULT;
    goto out;
  }

  if (user_alloc && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));

    if (os_event &&
        copy_back >= NVGPU_EVENT_BUFFER_NOTIFICATION_OFFSET + sizeof(u64))
      memcpy(resp_buf + sizeof(*resp) + sizeof(params) +
                 NVGPU_EVENT_BUFFER_NOTIFICATION_OFFSET,
             &os_event_val, sizeof(os_event_val));
    /*
     * The event's `data` comes back holding the backend's handle, which
     * the backend restored in place of its own descriptor; the caller passed
     * its descriptor, and RM leaves the field alone, so that is what goes
     * back -- on a failed RM status too.
     */
    if (event_fd >= 0 && copy_back >= NVGPU_NV0005_DATA_OFFSET + sizeof(u32))
      memcpy(resp_buf + sizeof(*resp) + sizeof(params) +
                 NVGPU_NV0005_DATA_OFFSET,
             &event_fd, sizeof(event_fd));
    if (nvgpu_resp_has(used, sizeof(*resp) + sizeof(params), copy_back) &&
        copy_to_user(user_alloc, resp_buf + sizeof(*resp) + sizeof(params),
                     copy_back))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * An ioctl the device's config names as carrying a descriptor at a fixed
 * offset of its argument. This swaps the caller's descriptor for the handle
 * the backend issued for that file; the backend swaps the handle for its own
 * descriptor for the call and puts the *handle* back (dispatch_fd_carrying,
 * dispatch_map_memory), since it never saw the caller's number. So the
 * caller's descriptor goes back here, on every reply that carries the struct,
 * an RM-status failure included: RM never writes the field (escape.c:393-428,
 * 584-624), so native userspace reads back exactly what it passed, and
 * envyhooks keys its mmap tracking by that value and unwrap()s the lookup.
 */
static long nvgpu_ioctl_translate_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                                     void __user *uarg, unsigned int sz,
                                     unsigned int payload_offset,
                                     const void *pre) {
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  void *req_buf = NULL, *resp_buf = NULL;
  int guest_fd;
  u32 host_handle, used, data_len;
  int req_total, resp_max, ret;

  if (sz < payload_offset + sizeof(guest_fd))
    return -EINVAL;

  req_total = sizeof(*req) + sz;
  resp_max = sizeof(*resp) + sz;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  /* The full payload: from userspace, or as the caller already read it */
  if (pre)
    memcpy(req_buf + sizeof(*req), pre, sz);
  else if (copy_from_user(req_buf + sizeof(*req), uarg, sz)) {
    ret = -EFAULT;
    goto out;
  }

  /* Extract guest fd from its position in the payload */
  memcpy(&guest_fd, req_buf + sizeof(*req) + payload_offset, sizeof(guest_fd));

  /*
   * A descriptor in these parameters is optional, and -1 is how a caller says
   * it is not using one.  NV_ESC_RM_ALLOC_MEMORY carries -1 for every ordinary
   * allocation -- only one that is to be mapped through another open file names
   * that file -- and the driver on the other side accepts it and allocates.
   *
   * Resolving it is meaningless and refusing it is worse: this path returned
   * -EBADF from fget(-1) before the request was ever sent, so the call failed
   * with nothing recorded anywhere on the far side.  The value only keeps its
   * meaning if it is forwarded unchanged.
   *
   * -1 only: any other negative number is no descriptor either, and sent
   * as it is the backend would read it as a handle of its own.
   */
  if (guest_fd < -1) {
    ret = -EBADF;
    goto out;
  }
  if (guest_fd >= 0) {
    /* Resolve guest fd → nvgpu_fd → VMM handle */
    ret = nvgpu_handle_for_fd(nfd->dev, guest_fd, &host_handle);
    if (ret)
      goto out;

    /* Patch payload: replace raw guest fd with VMM handle */
    memcpy(req_buf + sizeof(*req) + payload_offset, &host_handle,
           sizeof(host_handle));
  }

  /* Build request header */
  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sz);
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* Write the (possibly modified) payload back to userspace */
  data_len = nvgpu_resp_has(used, 0, sizeof(*resp))
                 ? le32_to_cpu(resp->data_len)
                 : 0;
  if (ret == 0 && data_len > 0 && data_len <= sz &&
      nvgpu_resp_has(used, sizeof(*resp), data_len)) {
    if (data_len >= payload_offset + sizeof(guest_fd))
      memcpy(resp_buf + sizeof(*resp) + payload_offset, &guest_fd,
             sizeof(guest_fd));
    if (copy_to_user(uarg, resp_buf + sizeof(*resp), data_len))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/* NV_IOCTL_MAGIC, the type byte of every RM escape. */
#define NVGPU_RM_IOCTL_TYPE 'F'

/*
 * The config's table names ioctls by number alone, and those numbers are
 * RM's: 0xC9, 0xCE and 0xCF are also DRM's REVOKE_LEASE, GETFB2 and
 * SYNCOBJ_EVENTFD (drm.h). Matched on the number only, a DRM ioctl that
 * reached this path would have four bytes of its argument rewritten as a
 * descriptor, so the type byte has to be RM's too.
 */
static const struct nvgpu_fd_translation_entry *
nvgpu_find_fd_translation(struct nvgpu_device *dev, unsigned int cmd) {
  u32 i;

  if (_IOC_TYPE(cmd) != NVGPU_RM_IOCTL_TYPE)
    return NULL;
  for (i = 0; i < dev->num_fd_translations; i++)
    if (le32_to_cpu(dev->fd_translations[i].nr) == _IOC_NR(cmd))
      return &dev->fd_translations[i];
  return NULL;
}

/*
 * The UVM commands that name another open file -- the RM control file a GPU,
 * VA space, channel or allocation belongs to, or the primary UVM file
 * MM_INITIALIZE pins -- as the config's table lists them (NVGPU_FDT_UVM).
 * UVM looks a descriptor up in the calling process, which on the host is the
 * backend: forwarded as the caller's number, it named whatever the backend
 * had open under that number.
 */
static const struct nvgpu_fd_translation_entry *
nvgpu_find_uvm_fd_translation(struct nvgpu_device *dev, unsigned int cmd) {
  u32 i;

  if (cmd & NVGPU_FDT_UVM)
    return NULL;
  for (i = 0; i < dev->num_fd_translations; i++)
    if (le32_to_cpu(dev->fd_translations[i].nr) == (NVGPU_FDT_UVM | cmd))
      return &dev->fd_translations[i];
  return NULL;
}

/* Main ioctl dispatcher */
/*
 * Split from nvgpu_ioctl so a DRM node can reach it. On a real DRM node
 * filp->private_data is a `struct drm_file *`, and ours hangs off its
 * driver_priv -- so the caller supplies the fd rather than this deriving it.
 */
long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                    unsigned long arg) {
  unsigned int nr = _IOC_NR(cmd);
  unsigned int sz = _IOC_SIZE(cmd);
  void __user *uarg = (void __user *)arg;
  const struct nvgpu_fd_translation_entry *fdt;
  void *pre = NULL;
  long ret;

  /*
   * Memory the caller already has, registered by its pages rather than its
   * address (nvgpu_osdesc.c), before ALLOC_MEMORY's descriptor translation:
   * RM reads no descriptor for this class. A call that may be one is read
   * once, and the same bytes go to whichever path it takes (the class word
   * was read apart from the block that was then sent). A block that does
   * not read goes on to the path it would take otherwise, which reads it
   * itself and fails as it does.
   */
  if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE && nvgpu_osdesc_ok(nfd->dev) &&
      nvgpu_osdesc_candidate(nr, sz)) {
    pre = kmalloc(sz, GFP_KERNEL);
    if (pre && copy_from_user(pre, uarg, sz)) {
      kfree(pre);
      pre = NULL;
    }
    if (pre && nvgpu_osdesc_ioctl(nfd, cmd, uarg, pre, sz, &ret))
      goto out;
  }

  fdt = nvgpu_find_fd_translation(nfd->dev, cmd);
  if (fdt) {
    ret = nvgpu_ioctl_translate_fd(nfd, cmd, uarg, sz,
                                   le32_to_cpu(fdt->payload_offset), pre);
    goto out;
  }

  switch (nr) {
  case NV_ESC_RM_CONTROL:
    ret = nvgpu_ioctl_rm_control(nfd, cmd, uarg, sz);
    break;
  case NV_ESC_RM_ALLOC:
    ret = nvgpu_ioctl_rm_alloc(nfd, cmd, uarg, sz, pre);
    break;
  case NV_ESC_RM_IDLE_CHANNELS:
    if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE)
      ret = nvgpu_ioctl_idle_channels(nfd, cmd, uarg, sz);
    else
      ret = nvgpu_ioctl_simple(nfd, cmd, uarg, sz, pre);
    break;
  case NV_ESC_RM_FREE:
    ret = nvgpu_ioctl_simple(nfd, cmd, uarg, sz, pre);
    /* An object RM freed may have been registered memory. */
    if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE)
      nvgpu_osdesc_reap(nfd->dev);
    break;
  default:
    ret = nvgpu_ioctl_simple(nfd, cmd, uarg, sz, pre);
    break;
  }
out:
  kfree(pre);
  return ret;
}

/*
 * The size of UVM command `cmd`'s parameter block on the host's release, or
 * -1 for a command the backend does not let through (or this release does
 * not have). UVM's numbers carry no size -- UVM_IOCTL_BASE(n) is plain n, and
 * the 0x3000 in UVM_INITIALIZE's is not the size of anything -- and the host
 * copies exactly sizeof(<cmd>_PARAMS) each way, so the size comes from the
 * table generated with the backend's (gen/schema/uvm.py).
 */
static int nvgpu_uvm_size(const struct nvgpu_uvm_table *t, unsigned int cmd) {
  u32 i;

  for (i = 0; t && i < t->ncmds; i++)
    if (t->cmds[i].cmd == cmd)
      return t->cmds[i].size;
  return -1;
}

long nvgpu_uvm_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                        unsigned long arg) {
  void __user *uarg = (void __user *)arg;
  const struct nvgpu_fd_translation_entry *fdt;
  int sz;

  /*
   * Exactly the command's block, both ways. With 0x3000 as a bound for every
   * command it did not know, this copied 12 KiB in and all of it back out:
   * bytes past a 4-byte struct in the caller's memory rewritten with what
   * they held when the call began, under any other thread of the caller
   * writing there meanwhile. A command with no size is not one the backend
   * lets through, and goes no further than here.
   */
  sz = nvgpu_uvm_size(nfd->dev->uvm, cmd);
  if (sz < 0)
    return -EPERM;

  /*
   * UVM_INITIALIZE's flags go as the caller set them. Pageable access is the
   * backend's to turn off, with the flags the host's release takes
   * (guestptr.rs); forcing a flag here would only hand an older host's UVM a
   * bit it refuses the whole call for, and rewrite the caller's own block.
   */

  /*
   * A command naming another file: the caller's descriptor becomes our
   * handle for that file (and one not of ours is refused), sent in a block of
   * the command's own size, and the caller reads back its own descriptor.
   * The backend states that size in the config too; one that disagrees with
   * the table is a backend of another release, and nothing is sent.
   */
  fdt = nvgpu_find_uvm_fd_translation(nfd->dev, cmd);
  if (fdt) {
    u32 packed = le32_to_cpu(fdt->payload_offset);

    if (packed >> 16 != sz)
      return -EPROTO;
    return nvgpu_ioctl_translate_fd(nfd, cmd, uarg, sz, packed & 0xffff,
                                    NULL);
  }

  /* DEINITIALIZE takes no argument, and sends and gets back no bytes. */
  return nvgpu_ioctl_simple(nfd, cmd, uarg, sz, NULL);
}

/* ───────── nvidia-modeset ioctl (/dev/nvidia-modeset, ioc_type 0x6d) ───────
 *
 * Outer struct (16 bytes):
 *   u32 cmd       — modeset sub-command
 *   u32 dataSize  — bytes pointed to by pData
 *   u64 pData     — USERSPACE pointer to the actual data buffer
 *
 * Same 2-level serialisation pattern as RM_CONTROL/RM_ALLOC.
 * The VMM side already handles pointer patching at offset 8 (see handler.rs).
 */

/* NvKmsIoctlCommand: the one that names memory by a descriptor. */
#define NVGPU_NVKMS_REGISTER_SURFACE 16
/* Byte offset of planes[0].u inside NvKmsRegisterSurfaceRequest. */
#define NVGPU_NVKMS_SURFACE_FD_OFFSET 16

struct nvidia_modeset_outer {
  __le32 cmd;
  __le32 dataSize; /* ← the nested buffer size! */
  __le64 pData;    /* ← userspace pointer to nested params */
};

/*
 * Forward one nvidia-modeset ioctl. The parameter block is an outer struct
 * holding a userspace pointer to the real payload, so both have to be copied.
 *
 * Called only from nvgpu_modeset_ioctl(), which has already checked the ioctl
 * type and size.
 */
long nvgpu_ioctl_modeset(struct nvgpu_fd *nfd, unsigned int cmd,
                         void __user *uarg, u32 sz) {
  struct nvidia_modeset_outer outer;
  void __user *user_nested;
  u32 nested_size;
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  u32 used;

  /*
   * NVKMS takes one ioctl, NVKMS_IOCTL_CMD with exactly NvKmsIoctlParams
   * behind it, and anything else is -ENOTTY (nvidia-modeset-linux.c
   * nvkms_ioctl). This read and wrote 16 bytes whatever the ioctl's size.
   */
  if (_IOC_NR(cmd) != 0 || sz != sizeof(outer))
    return -ENOTTY;
  if (copy_from_user(&outer, uarg, sizeof(outer)))
    return -EFAULT;

  user_nested = (void __user *)(unsigned long)le64_to_cpu(outer.pData);
  nested_size = le32_to_cpu(outer.dataSize);

  if (nested_size > 1024 * 1024)
    return -EINVAL;
  /*
   * A size with no parameters: NVKMS copies the request in from the address
   * and fails the call (nvkms.c nvKmsIoctl, -EPERM). Sent on, the request
   * carried nested_size bytes this never wrote -- guest kernel heap.
   */
  if (!user_nested && nested_size)
    return -EPERM;

  req_total = sizeof(*req) + sizeof(outer) + nested_size;
  resp_max = sizeof(struct nvgpu_ioctl_resp) + sizeof(outer) + nested_size;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sizeof(outer));
  req->nested_offset = cpu_to_le32(sizeof(outer));
  req->nested_len = cpu_to_le32(nested_size);
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  memcpy(req_buf + sizeof(*req), &outer, sizeof(outer));

  if (user_nested && nested_size > 0) {
    if (copy_from_user(req_buf + sizeof(*req) + sizeof(outer), user_nested,
                       nested_size)) {
      ret = -EFAULT;
      goto out;
    }

    /*
     * NVKMS_IOCTL_REGISTER_SURFACE names the memory it registers by a *file
     * descriptor* when useFd is set, and a descriptor number means nothing in
     * the backend's process -- forwarded verbatim it picks out whatever that
     * process happens to have open at that number. NVKMS answers EPERM, the
     * ICD concludes it cannot share buffers, drops
     * VK_EXT_external_memory_dma_buf, and a client is left unable to present
     * with no error anywhere that names the cause.
     *
     * This is rung 8 on a second path. The GEM import was translated when it
     * was found; this one is reached instead on driver 615, where the ICD
     * registers the surface with NVKMS directly rather than through the DRM
     * node, which is why one box presented and the other did not.
     *
     * struct NvKmsRegisterSurfaceRequest:
     *   0  NvKmsDeviceHandle deviceHandle
     *   4  NvBool            useFd
     *   8  NvU32             rmClient
     *  16  planes[0].u       union { NvU64 rmHandle; NvS32 fd; }
     *
     * The handle goes in where the descriptor was; the backend puts its own
     * descriptor back before the call.
     */
    if (le32_to_cpu(outer.cmd) == NVGPU_NVKMS_REGISTER_SURFACE &&
        nested_size >= NVGPU_NVKMS_SURFACE_FD_OFFSET + sizeof(u64)) {
      u8 *nested = req_buf + sizeof(*req) + sizeof(outer);
      u32 use_fd;

      memcpy(&use_fd, nested + 4, sizeof(use_fd));
      if (le32_to_cpu((__le32)use_fd)) {
        s32 guest_fd;
        u32 handle;

        memcpy(&guest_fd, nested + NVGPU_NVKMS_SURFACE_FD_OFFSET,
               sizeof(guest_fd));
        if (nvgpu_handle_for_fd(nfd->dev, guest_fd, &handle) == 0) {
          u64 as_u64 = handle;

          memcpy(nested + NVGPU_NVKMS_SURFACE_FD_OFFSET, &as_u64,
                 sizeof(as_u64));
        } else {
          /*
           * Refused, not forwarded: the backend would read the caller's
           * number as one of its handles -- another process's file.
           */
          dev_warn_ratelimited(
              &nfd->dev->vdev->dev,
              "virtio-gpu-nv: REGISTER_SURFACE names fd %d, which is not one "
              "of ours\n",
              guest_fd);
          ret = -EBADF;
          goto out;
        }
      }
    }
  }

  ret = nvgpu_send_recv_used(nfd->dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* Write back outer struct -- if the device sent one back. */
  if (!nvgpu_resp_has(used, sizeof(*resp), sizeof(outer)))
    goto out;
  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(outer))) {
    ret = -EFAULT;
    goto out;
  }

  /* Write back nested params */
  if (user_nested && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));

    if (nvgpu_resp_has(used, sizeof(*resp) + sizeof(outer), copy_back) &&
        copy_to_user(user_nested, resp_buf + sizeof(*resp) + sizeof(outer),
                     copy_back))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/* ───────── Memory registered by its pages (see nvgpu_osdesc.c) ───────── */

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

/*
 * Read the call from the caller's block. False for anything that is not one
 * of the three, well-formed, with the virtual address descriptor type: that
 * goes the old way (the backend refuses a registration by address). An
 * unreadable block sets *ret.
 */
static u32 nvgpu_osdesc_size(unsigned int nr);

static bool nvgpu_osdesc_describe(struct nvgpu_osdesc_call *c,
                                  unsigned int cmd, const void *outer,
                                  unsigned int sz, long *ret) {
  const u8 *o = c->outer;
  u64 limit, end;
  u32 psize;

  c->nr = _IOC_NR(cmd);
  c->outer_len = nvgpu_osdesc_size(c->nr);
  if (!c->outer_len || sz != c->outer_len)
    return false;
  memcpy(c->outer, outer, c->outer_len);

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
  /*
   * The pages the range spans, counted without rounding up past 2^64: with
   * DIV_ROUND_UP, a size within a page of it came to none, and a range of
   * 2^64 bytes was registered with a page list of one empty run.
   */
  if ((c->va & ~PAGE_MASK) + c->size >
      (u64)NVGPU_OSDESC_MAX_PAGES * PAGE_SIZE)
    return false;
  return true;
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

  ret = nvgpu_osdesc_send(dev, req, (int)req_len, resp, (int)resp_len, &used,
                          pages, npages, c->write);
  kvfree(req);
  req = NULL;
  if (ret == -EINTR || ret == -ETIMEDOUT) {
    /* The pins went with the request (nvgpu_osdesc_send()). */
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

/* The parameter block of one of the three calls, by _IOC_NR; 0 for others. */
static u32 nvgpu_osdesc_size(unsigned int nr) {
  switch (nr) {
  case NVGPU_ESC_RM_ALLOC_MEMORY:
    return NVGPU_OS02_SIZE;
  case NVGPU_ESC_RM_VID_HEAP_CONTROL:
    return NVGPU_OS32_SIZE;
  case NVGPU_ESC_RM_ALLOC:
    return NVGPU_OS64_SIZE;
  default:
    return 0;
  }
}

bool nvgpu_osdesc_candidate(unsigned int nr, unsigned int sz) {
  u32 len = nvgpu_osdesc_size(nr);

  return len && sz == len;
}

bool nvgpu_osdesc_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                        void __user *uarg, const void *outer, unsigned int sz,
                        long *ret) {
  struct nvgpu_osdesc_call *c;
  u32 want;
  size_t at;
  bool ours;

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
  if (sz < at + sizeof(want) ||
      get_unaligned_le32((const u8 *)outer + at) != want)
    return false;
  c = kzalloc(sizeof(*c), GFP_KERNEL);
  if (!c) {
    *ret = -ENOMEM;
    return true;
  }
  *ret = 0;
  ours = nvgpu_osdesc_describe(c, cmd, outer, sz, ret);
  if (ours && !*ret)
    *ret = nvgpu_osdesc_register(nfd, cmd, uarg, c);
  kfree(c);
  return ours;
}
