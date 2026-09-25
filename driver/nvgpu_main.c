// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: NVIDIA GPU ioctl proxy for libkrun VMs.
 *
 * Each guest open("/dev/nvidia*") creates a new host FD via the VMM.
 * Ioctls are forwarded over the control virtqueue; mmap requests result
 * in KVM memory slots set up by the VMM so hot-path GPU writes go direct
 * through EPT — no VMM involvement in the render loop.
 *
 * Guest kernel driver — runs inside the VM.
 * Place in: drivers/virtio/ with the other nvgpu_* files (libkrunfw tree)
 */

#include <drm/drm.h>
#include <linux/cdev.h>
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/kref.h>
#include <linux/cpu.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pci-ecam.h>
#include <linux/numa.h>
#include <linux/pci.h>
#include <linux/poll.h>
#include <linux/proc_fs.h>
#include <linux/rcupdate.h>
#include <linux/sched.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/topology.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/virtio_ids.h>

#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_prime.h>

#include "gen/nvgpu_rm_deep.h"
#include "gen/nvgpu_rmalloc_classes.h"
#include "gen/nvgpu_schema.h"
#include "gen/nvgpu_v1v2_rewrites.h"
#include "nvgpu_rm_intercepts.h"
#include "nvgpu.h"

/*
 * module_kset lives in kernel/module/sysfs.c and is NOT exported to modules,
 * so it can only be named directly in an in-tree build.
 */
#ifndef MODULE
extern struct kset *module_kset;
#endif

/* ───────── NVIDIA device node numbers ───────── */

#define NV_MAJOR 195
#define NV_CTL_MINOR 255
#define NV_UVM_MAJOR 237
#define NV_CAPS_MAJOR 240
#define NV_MODESET_MINOR 254

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
 * parameters. The export form carries it after the 16-byte object it names.
 */
#define NVGPU_RM_EXPORT_OBJECT_TO_FD 0x00003d05
#define NVGPU_RM_IMPORT_OBJECT_FROM_FD 0x00003d06
#define NVGPU_RM_EXPORT_FD_OFFSET 16

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

/* ───────── Driver state ───────── */

/* The shared memory region device memory is placed in, id 1. */
#define NVGPU_SHM_ID 1

/*
 * Which capability bits GET_DEV_INFO claims. Parameters rather than constants
 * because what the ICD asks for next depends on them, and the cost of being
 * wrong is a device that will not initialise -- cheaper to sweep than to
 * rebuild. Each is ANDed with the host's own bit (nvgpu_drm_get_dev_info), so
 * it can only take a capability away, never claim one the host lacks.
 */
/*
 * supports_alloc is on by default now, because the ioctls behind it work: a
 * Wayland client presents and 616 frames were encoded and decoded clean with
 * it set. It stays a parameter so it can be turned off to tell a GEM problem
 * from everything else in one boot.
 */
int nvgpu_claim_alloc = 1;
module_param_named(claim_alloc, nvgpu_claim_alloc, int, 0444);
MODULE_PARM_DESC(claim_alloc, "GET_DEV_INFO reports supports_alloc");
/*
 * supports_sync_fd stays off: PRIME_FENCE_CONTEXT_CREATE and
 * GEM_PRIME_FENCE_ATTACH (0x05, 0x06) are still not forwarded. Nothing has
 * asked for them -- no unhandled-ioctl line names either -- so the encode path
 * does not need them, and claiming a capability nothing serves is what rung 5
 * cost us.
 */
int nvgpu_claim_sync_fd;
module_param_named(claim_sync_fd, nvgpu_claim_sync_fd, int, 0444);
MODULE_PARM_DESC(claim_sync_fd, "GET_DEV_INFO reports supports_sync_fd");

/*
 * Whether a wait on one of these descriptors can wait.
 *
 * On, `.poll` reports nothing until the host says a descriptor is readable, so
 * NVIDIA's user-mode driver sleeps between frames the way it does on bare
 * metal. Off, there is no `.poll` state to consult and the VFS reports every
 * descriptor ready -- the old behaviour, which spun.
 *
 * Measured on an RTX 3060, unpaced at ~100 fps: a guest cost 12.26 s of CPU
 * over a 12 s run with this off, and 0.64 s with it on, for the same frame
 * rate. The host itself costs 0.40 s. It is a parameter because the trade is
 * not free -- a paced encode run gives up ~11% of its frames to the wake, see
 * the write-up -- and because a switch is how the next person re-takes both
 * numbers without a rebuild.
 */
static int nvgpu_poll_events = 1;
module_param_named(poll_events, nvgpu_poll_events, int, 0444);
MODULE_PARM_DESC(poll_events, "a wait on a device descriptor really waits");

/*
 * Microseconds to look for the event before sleeping for it.
 *
 * Sleeping is not cheap here. The wake has to travel the host's epoll, the
 * event queue, an interrupt and a halted vCPU, and measured end to end that is
 * ~0.35 ms -- against 0.049 ms for a whole frame at cost 0. So a guest that
 * sleeps on every frame pays more in wake than it spends drawing, which is how
 * an encode run lost ~11% of its frames.
 *
 * Spinning first is the usual answer to that, and it was measured here rather
 * than assumed: **it does not help, and it is off.** At 80, 300 and 600 us the
 * late-frame count in a paced encode run went 33, 49 and 62 out of ~550, against
 * 53 with no spin at all -- noise, not a trend -- while CPU went from 0.39 s to
 * 0.99 s over a 12 s run. The waits that hurt are not the short ones.
 *
 * Kept as a knob because it is the obvious thing to try, and a number beats
 * trying it again.
 */
static int nvgpu_poll_spin_us;
module_param_named(poll_spin_us, nvgpu_poll_spin_us, int, 0644);
MODULE_PARM_DESC(poll_spin_us, "microseconds to spin before sleeping for an event");

struct nvgpu_numa_attr {
  struct kobj_attribute kattr;
  struct nvgpu_device *dev;
  char *(*get_buf)(struct nvgpu_device *);
};

/* class for device_create() */
static struct class *nvgpu_class;

static long nvgpu_ioctl(struct file *filp, unsigned int cmd, unsigned long arg);
static long nvgpu_ioctl_simple(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg, unsigned int sz);

/*
 * poll() on a device descriptor.
 *
 * Reports nothing until the host says otherwise, which is the whole point:
 * without this the VFS reported every one of these descriptors as permanently
 * ready and the user-mode driver's wait never waited.
 */
static __poll_t nvgpu_poll_mask(struct file *filp,
                                struct poll_table_struct *wait,
                                __poll_t ready) {
  struct nvgpu_fd *nfd = filp->private_data;

  if (!nfd)
    return EPOLLERR;

  /* Off: no state to consult, so say what the VFS said before this existed. */
  if (!nvgpu_poll_events)
    return EPOLLIN | EPOLLOUT | EPOLLRDNORM | EPOLLWRNORM;

  /*
   * A caller that passed a poll_table means to sleep if this says nothing, and
   * that sleep is what costs 0.35 ms. Look for the event first: an event that
   * arrives inside the spin is reported without the guest ever leaving the
   * CPU. A caller polling with no intention of sleeping passes no table and
   * gets the plain answer.
   */
  if (wait && nvgpu_poll_spin_us > 0 && !atomic_read(&nfd->pending)) {
    ktime_t deadline = ktime_add_us(ktime_get(), nvgpu_poll_spin_us);

    while (!atomic_read(&nfd->pending)) {
      if (ktime_after(ktime_get(), deadline))
        break;
      if (need_resched() || signal_pending(current))
        break;
      cpu_relax();
    }
  }

  poll_wait(filp, &nfd->wq, wait);

  /*
   * Taken, not read. Leaving it set until an ioctl consumed it was tried and
   * measured: a caller that polls without consuming then finds it ready every
   * time, which is the spin this whole path exists to end -- CPU went straight
   * back to a full core. One report per event it is.
   */
  if (atomic_xchg(&nfd->pending, 0))
    return ready;
  return 0;
}

static __poll_t nvgpu_poll(struct file *filp, struct poll_table_struct *wait) {
  return nvgpu_poll_mask(filp, wait, EPOLLIN | EPOLLRDNORM);
}

/*
 * nvidia-modeset reports a pending NVKMS event as POLLPRI | POLLIN
 * (nvidia-modeset-linux.c:2027-2029, nvkms_poll), so a caller may wait for
 * either. Answered like the RM devices, with EPOLLIN only, a poll for POLLPRI
 * would sleep through an event that had already arrived.
 */
static __poll_t nvgpu_modeset_poll(struct file *filp,
                                   struct poll_table_struct *wait) {
  struct nvgpu_fd *nfd = filp->private_data;

  /* v2: readable until consumed, as NVKMS is (nvgpu_nvkms.c). */
  if (nfd && nfd->dev->v2)
    return nvgpu_nvkms_poll(nfd, filp, wait);
  return nvgpu_poll_mask(filp, wait, EPOLLIN | EPOLLPRI | EPOLLRDNORM);
}

/* ───────── Ioctl forwarding ───────── */

/*
 * Whether RM_ALLOC and RM_DUP_OBJECT say which process makes them
 * (nvgpu_wire.h, struct nvgpu_proc_id): only to a backend that asked.
 */
static bool nvgpu_proc_ids(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_PROC_ID);
}

/*
 * The calling process, as the backend keeps RM clients to one: its thread
 * group, which every thread of it shares, by the group leader's PID in the
 * initial namespace and start time. A fork is a new pair; an exec keeps it
 * (de_thread gives the execing thread the leader's PID and start time), as
 * the host's RM keeps a process's PID across exec. Tasks are freed after an
 * RCU grace period, so the leader read here stays readable while a
 * concurrent exec replaces it.
 */
static void nvgpu_proc_id_fill(void *dst) {
  struct nvgpu_proc_id id = {};
  struct task_struct *leader;

  rcu_read_lock();
  leader = READ_ONCE(current->group_leader);
  id.start_ns = cpu_to_le64(leader->start_time);
  rcu_read_unlock();
  id.tgid = cpu_to_le32(task_tgid_nr(current));
  memcpy(dst, &id, sizeof(id));
}

/* nvgpu_ioctl_simple — flat struct, no embedded pointers */
static long nvgpu_ioctl_simple(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg, unsigned int sz) {
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
    if (copy_from_user(req_buf + sizeof(*req), uarg, sz)) {
      ret = -EFAULT;
      goto out;
    }
  }
  if (proc)
    nvgpu_proc_id_fill(req_buf + sizeof(*req) + sz);

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
 * nvgpu_handle_for_fd — the backend's handle for another of our open files.
 *
 * A guest descriptor means nothing on the other side. Anything that names an
 * open file has to name it by the handle the backend issued when we opened it.
 */
int nvgpu_handle_for_fd(int guest_fd, u32 *handle) {
  struct nvgpu_fd *other;
  struct file *f;
  int ret = 0;

  if (guest_fd < 0)
    return -EBADF;

  f = fget(guest_fd);
  if (!f)
    return -EBADF;

  /*
   * Only a file of ours has an nvgpu_fd behind it, and which field holds it
   * depends on which of ours it is. Reading private_data of any file as one
   * forwarded a number out of a drm_file, a pipe or a socket as if it were a
   * backend handle.
   */
  other = nvgpu_fd_from_file(f);
  if (other)
    *handle = other->handle;
  else
    ret = nvgpu_hostfile_handle(f, handle);
  fput(f);
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
static int nvgpu_rm_os_event_in(void *slot, u64 *saved) {
  u32 handle;
  u64 v;

  memcpy(&v, slot, sizeof(v));
  *saved = v;
  if (!v)
    return 0;
  if (v > INT_MAX || nvgpu_handle_for_fd((int)v, &handle))
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
  rw = (user_nested && nested_size > 0) ? nvgpu_find_deep_rewrite(ctl_cmd)
                                        : NULL;

  /*
   * Several pointers, each with what it addresses (nvgpu_deep_plan). The
   * nested block is read once, here, and that copy is what is sent: the sizes
   * the backend checks are computed from the same bytes they were planned
   * from, whatever another thread of the caller writes meanwhile.
   */
  if (user_nested && nested_size > 0 && nvgpu_deep_segs_ok(nfd->dev))
    ctl_deep = nvgpu_rm_deep_find(ctl_cmd);
  if (ctl_deep) {
    rw = NULL;
    nested_copy = kmalloc(nested_size, GFP_KERNEL);
    if (!nested_copy)
      return -ENOMEM;
    if (copy_from_user(nested_copy, user_nested, nested_size)) {
      kfree(nested_copy);
      return -EFAULT;
    }
    nvgpu_deep_plan(ctl_deep, nested_copy, nested_size, &plan);
    if (plan.n) {
      deep_ptr_offset = NVGPU_DEEP_SEGMENTED;
      deep_len = plan.bytes;
    }
  }

  if (rw && nested_size >= rw->v1_userptr_offset + 8) {
    void *pbuf = kmalloc(nested_size, GFP_KERNEL);
    u32 count;

    if (!pbuf)
      return -ENOMEM;

    if (copy_from_user(pbuf, user_nested, nested_size)) {
      kfree(pbuf);
      return -EFAULT;
    }

    memcpy(&deep_user_ptr, pbuf + rw->v1_userptr_offset, sizeof(u64));
    memcpy(&count, pbuf, sizeof(u32));
    kfree(pbuf);

    /*
     * The leading field says how much the buffer holds: entries of eight
     * bytes for the list-style commands, plain bytes for the caps tables.
     */
    deep_len = rw->info_style ? count * 8 : count;
    deep_ptr_offset = rw->v1_userptr_offset;

    if (!deep_user_ptr || deep_len == 0 || deep_len > NVGPU_DEEP_MAX) {
      deep_user_ptr = 0;
      deep_ptr_offset = 0;
      deep_len = 0;
    }
  }

  /* ── Normal path (no V1→V2 rewrite) ── */

  req_total =
      sizeof(struct nvgpu_ioctl_req) + sizeof(params) + nested_size + deep_len;
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

  if (user_nested && nested_size > 0) {
    if (nested_copy)
      memcpy(req_buf + sizeof(*req) + sizeof(params), nested_copy, nested_size);
    else if (copy_from_user(req_buf + sizeof(*req) + sizeof(params),
                            user_nested, nested_size)) {
      ret = -EFAULT;
      goto out;
    }

    /*
     * Exporting an object to a descriptor, and importing one back, name
     * another of our open files inside the nested parameters. The backend
     * knows that file by the handle it issued, not by our descriptor number,
     * so swap one for the other here and swap it back on the way out --
     * userspace gets its own descriptor returned, which is what it passed in.
     */
    if (ctl_cmd == NVGPU_RM_EXPORT_OBJECT_TO_FD ||
        ctl_cmd == NVGPU_RM_IMPORT_OBJECT_FROM_FD) {
      u32 off = (ctl_cmd == NVGPU_RM_EXPORT_OBJECT_TO_FD)
                    ? NVGPU_RM_EXPORT_FD_OFFSET
                    : 0;

      if (nested_size >= off + sizeof(u32)) {
        void *slot = req_buf + sizeof(*req) + sizeof(params) + off;
        u32 handle;

        memcpy(&nested_fd, slot, sizeof(nested_fd));
        if (nvgpu_handle_for_fd(nested_fd, &handle) == 0) {
          memcpy(slot, &handle, sizeof(handle));
          nested_fd_offset = off;
        } else {
          nested_fd = -1;
        }
      }
    }

    /*
     * A parameter block too short to hold the field is the backend's to
     * refuse; only one that holds it is translated.
     */
    os_event_off = nvgpu_rm_os_event_offset(ctl_cmd);
    if (os_event_off >= 0 && nested_size >= os_event_off + sizeof(u64)) {
      ret = nvgpu_rm_os_event_in(req_buf + sizeof(*req) + sizeof(params) +
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
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
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
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
  nvgpu_deep_plan(rule, params, sizeof(params), &plan);
  if (!plan.n)
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);

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
                                 void __user *uarg, unsigned int sz) {
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

  if (sz < sizeof(params))
    return -EINVAL;

  if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;

  user_alloc =(void __user *)(unsigned long)le64_to_cpu(params.pAllocParms);
  nested_size = le32_to_cpu(params.paramsSize);

  /*
   * When paramsSize == 0 but pAllocParms is non-NULL,
   * the host RM driver knows the size from hClass.  We need to copy
   * that many bytes from guest userspace so the VMM can forward them.
   */
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
    nvgpu_proc_id_fill(req_buf + sizeof(*req) + sizeof(params) + nested_size);

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
        if (event_fd >= 0) {
          u32 handle;

          /*
           * The descriptor is the one the event will be delivered on, so it
           * is always one of our devices. Anything else would reach the
           * backend as a number it reads as a handle of its own.
           */
          if (nvgpu_handle_for_fd(event_fd, &handle)) {
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
        ret = nvgpu_rm_os_event_in(nested +
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
                                     unsigned int payload_offset) {
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

  /* Copy the full payload from userspace */
  if (copy_from_user(req_buf + sizeof(*req), uarg, sz)) {
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
   */
  if (guest_fd >= 0) {
    /* Resolve guest fd → nvgpu_fd → VMM handle */
    ret = nvgpu_handle_for_fd(guest_fd, &host_handle);
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

  /* Hard cap only — sz == 0 is valid for several NVIDIA ioctls
   * (e.g. NV_ESC_RM_FREE on some driver versions, and any ioctl
   * that encodes parameters via _IOC_NR only with no struct). */
  if (sz > 65536)
    return -EINVAL;

  /*
   * Memory the caller already has, registered by its pages rather than its
   * address (nvgpu_osdesc.c), before ALLOC_MEMORY's descriptor translation:
   * RM reads no descriptor for this class.
   */
  if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE) {
    long ret;

    if (nvgpu_osdesc_ioctl(nfd, cmd, uarg, sz, &ret))
      return ret;
  }

  fdt = nvgpu_find_fd_translation(nfd->dev, cmd);
  if (fdt)
    return nvgpu_ioctl_translate_fd(nfd, cmd, uarg, sz,
                                    le32_to_cpu(fdt->payload_offset));

  switch (nr) {
  case NV_ESC_RM_CONTROL:
    return nvgpu_ioctl_rm_control(nfd, cmd, uarg, sz);
  case NV_ESC_RM_ALLOC:
    return nvgpu_ioctl_rm_alloc(nfd, cmd, uarg, sz);
  case NV_ESC_RM_IDLE_CHANNELS:
    if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE)
      return nvgpu_ioctl_idle_channels(nfd, cmd, uarg, sz);
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
  case NV_ESC_RM_FREE: {
    long ret = nvgpu_ioctl_simple(nfd, cmd, uarg, sz);

    /* An object RM freed may have been registered memory. */
    if (_IOC_TYPE(cmd) == NVGPU_RM_IOCTL_TYPE)
      nvgpu_osdesc_reap(nfd->dev);
    return ret;
  }
  default:
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
  }
}

/* ───────── UVM ioctl ───────── */

static long nvgpu_ioctl(struct file *filp, unsigned int cmd,
                        unsigned long arg) {
  return nvgpu_ioctl_fd(filp->private_data, cmd, arg);
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

static long nvgpu_uvm_ioctl(struct file *filp, unsigned int cmd,
                            unsigned long arg) {
  struct nvgpu_fd *nfd = filp->private_data;
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
    return nvgpu_ioctl_translate_fd(nfd, cmd, uarg, sz, packed & 0xffff);
  }

  /* DEINITIALIZE takes no argument, and sends and gets back no bytes. */
  return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
}

/* ───────── mmap ───────── */

/*
 * One window placement made through a character device, shared by every vma
 * that maps it.
 *
 * The backend counts a reference per MMAP reply, and a reply makes one vma --
 * but a vma is not what the kernel keeps. An munmap or mprotect of part of
 * the range splits it in two (__split_vma calls .open on the new half), fork
 * copies it into the child (dup_mmap, .open again), mremap moves it (copy_vma:
 * .open on the new one, .close on the old), and every one of those vmas is
 * closed on its own. With no .open and the mapping id kept bare in
 * vm_private_data, each of them sent MUNMAP: the second gave back a reference
 * the first had already given back, and with the backend counting, took the
 * placement away from whoever else held it -- another MMAP of the same host
 * object -- while this process still had the pages mapped.
 *
 * So the vmas share this, counted: .open takes a reference, .close drops one,
 * and the last sends the one MUNMAP the reply is owed. The handle stays valid
 * until then: every vma pins the file, and the file's release is what CLOSEs
 * the handle.
 */
struct nvgpu_vma_map {
  struct kref ref;
  struct nvgpu_device *dev;
  u32 handle;
  u32 mapping_id;
};

static void nvgpu_vma_map_release(struct kref *ref) {
  struct nvgpu_vma_map *m = container_of(ref, struct nvgpu_vma_map, ref);

  nvgpu_munmap(m->dev, m->handle, m->mapping_id);
  /* The vmas outlive the file, and may outlive the device. */
  nvgpu_dev_put(m->dev);
  kfree(m);
}

static void nvgpu_vma_open(struct vm_area_struct *vma) {
  struct nvgpu_vma_map *m = vma->vm_private_data;

  kref_get(&m->ref);
}

static void nvgpu_vma_close(struct vm_area_struct *vma) {
  struct nvgpu_vma_map *m = vma->vm_private_data;

  kref_put(&m->ref, nvgpu_vma_map_release);
}

static const struct vm_operations_struct nvgpu_vm_ops = {
    .open = nvgpu_vma_open,
    .close = nvgpu_vma_close,
};

/*
 * The host maps each placement with a memory type of its own choosing:
 * registers (the usermode doorbell) uncached, video memory through BAR1
 * write-combined, system memory with the cache type it was allocated with,
 * which the backend makes write-back and GPU-coherent (device/src/rmmem.rs).
 * Mapping everything write-combined here made the doorbell a write-combined
 * store where the host driver's own is uncached, and every read of cached
 * system memory an uncached one. A v1 backend leaves the field zero, and zero
 * keeps the old answer.
 */
pgprot_t nvgpu_window_pgprot(struct nvgpu_device *dev, u8 caching,
                             pgprot_t prot) {
  switch (dev->v2 ? caching : NVGPU_MMAP_CACHE_DEFAULT) {
  case NVGPU_MMAP_CACHE_WB:
    return prot;
  case NVGPU_MMAP_CACHE_UC:
    return pgprot_noncached(prot);
  default:
    return pgprot_writecombine(prot);
  }
}

/*
 * Whether the top of the window, where the backend keeps its write-back zone
 * (device/src/shm.rs: uncached, write-combining, then write-back), really is
 * write-back here. PAT grants a write-back request only where the MTRRs say
 * write-back too and quietly makes it UC- elsewhere (arch/x86/mm/pat/
 * memtype.c, pat_x_mtrr_type) -- and a PCI BAR is usually not write-back in
 * the MTRRs unless the VMM's firmware covered that zone with a variable MTRR
 * of its own. Nothing is wrong then, only slower: reads of coherent GPU system
 * memory go uncached. Said once, here, rather than found by a benchmark.
 */
static void nvgpu_region_check_wb(struct nvgpu_device *dev,
                                  const struct virtio_shm_region *r,
                                  const char *what) {
#ifdef CONFIG_X86
  void __iomem *p;
  unsigned int level;
  pte_t *pte;

  p = ioremap_cache(r->addr + r->len - PAGE_SIZE, PAGE_SIZE);
  if (!p)
    return;
  pte = lookup_address((unsigned long)p, &level);
  if (pte && level == PG_LEVEL_4K && (pte_flags(*pte) & _PAGE_CACHE_MASK))
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: %s is not write-back in this guest's MTRRs, so "
             "write-back mappings of GPU system memory will be uncached "
             "(slower, not wrong); the VMM can cover it with a write-back "
             "MTRR\n",
             what);
  iounmap(p);
#endif
}

/*
 * A UVM file maps one thing: a semaphore pool, which UVM takes only at the
 * address equal to the offset, only shared and read-write (uvm.c:792-806),
 * and only for the pool's exact range (the backend checks that one, against
 * the pools it saw this file make). Refused here as UVM would, and without
 * the aperture at all, as it always was: before it, a UVM mapping sent to
 * the window failed on the host and took the window with it.
 */
static int nvgpu_mmap_uvm_check(struct nvgpu_fd *nfd,
                                struct vm_area_struct *vma, u64 offset) {
  const vm_flags_t rw = VM_SHARED | VM_READ | VM_WRITE;
  struct nvgpu_device *dev = nfd->dev;

  if (!dev->v2 || !(dev->backend_caps & NVGPU_BCAP_UVM_MAP) ||
      dev->uvm_aperture.len < PAGE_SIZE)
    return -EINVAL;
  if (vma->vm_start != offset || (vma->vm_flags & rw) != rw)
    return -EINVAL;
  /* The band the backend and the VMM hold the pool's host address to. */
  if (offset < NVGPU_UVM_HVA_MIN || vma->vm_end > NVGPU_UVM_HVA_MAX)
    return -EINVAL;
  return 0;
}

static int nvgpu_mmap(struct file *filp, struct vm_area_struct *vma) {
  struct nvgpu_fd *nfd = filp->private_data;
  u64 size = vma->vm_end - vma->vm_start;
  u64 offset = (u64)vma->vm_pgoff << PAGE_SHIFT;
  bool uvm = nfd->device_type == NVGPU_DEV_UVM ||
             nfd->device_type == NVGPU_DEV_UVM_TOOLS;
  const struct virtio_shm_region *region;
  u64 window_off;
  struct nvgpu_mmap_req *req;
  struct nvgpu_mmap_resp *resp;
  /* Allocated before asking, so that nothing can fail between the backend
   * placing the memory and this side owning the placement. */
  struct nvgpu_vma_map *m;
  u32 used, mapping_id = 0;
  int ret;

  if (uvm) {
    ret = nvgpu_mmap_uvm_check(nfd, vma, offset);
    if (ret)
      return ret;
  }

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  m = kzalloc(sizeof(*m), GFP_KERNEL);
  if (!req || !resp || !m) {
    ret = -ENOMEM;
    goto out;
  }

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MMAP);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->size = cpu_to_le64(size);
  req->offset = cpu_to_le64(offset);
  req->prot = cpu_to_le32((vma->vm_flags & VM_WRITE) ? 3 : 1);

  ret = nvgpu_send_recv_used(nfd->dev, req, sizeof(*req), resp, sizeof(*resp),
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto out;
  }
  if (!nvgpu_resp_has(used, 0, sizeof(*resp))) {
    ret = -EIO;
    goto out;
  }
  /* The backend holds a placement for this reply from here on; every
   * failure below gives it back. */
  mapping_id = le32_to_cpu(resp->mapping_id);

  /*
   * Which region the offset is in is the backend's to say, and only for a
   * UVM file: an aperture reply to anything else, or a window reply to a
   * UVM mapping, is a backend this side does not understand.
   */
  if (uvm != !!(resp->flags & NVGPU_MMAP_F_UVM_APERTURE)) {
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: MMAP reply flags 0x%x for a %s file\n",
                         resp->flags, uvm ? "UVM" : "non-UVM");
    ret = -EIO;
    goto out;
  }
  region = uvm ? &nfd->dev->uvm_aperture : &nfd->dev->window;
  if (uvm) {
    /*
     * The pool's own pages, write-back as the host maps them
     * (uvm_mem_map_cpu_user), in a slot of their own. Not copied on fork,
     * as UVM's are not (uvm.c:830). The reply must be the whole vma: a
     * shorter placement would leave the rest of it reaching nothing.
     */
    if (le64_to_cpu(resp->size) != size) {
      ret = -EIO;
      goto out;
    }
    vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP |
                          VM_DONTCOPY);
    goto place;
  }

  /*
   * A mapping the host made read-only (the user-shared-data page; PTIMER and
   * MC for a non-admin) is read-only here too, by nvidia.ko's own rule for a
   * context without WRITEABLE (nv-mmap.c:756-761): the mmap succeeds, a write
   * faults in this process, and mprotect(PROT_WRITE) is refused. Left
   * writable, the first write would reach KVM as a fault on a read-only host
   * mapping it cannot resolve, and stop the whole VM.
   */
  if (nfd->dev->v2 && (resp->flags & NVGPU_MMAP_F_READ_ONLY)) {
    vm_flags_clear(vma, VM_WRITE | VM_MAYWRITE);
    vma->vm_page_prot = vm_get_page_prot(vma->vm_flags);
  }
  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot =
      nvgpu_window_pgprot(nfd->dev, resp->caching, vma->vm_page_prot);

  /*
   * What the backend returns is an offset within the shared window, not a
   * guest physical address. It cannot return an address: the bus decides
   * where the window sits, and the backend is a separate process that is
   * never told. This side knows, because the window is a region of this
   * device and the address came out of its own PCI configuration.
   */
  if (!nfd->dev->window.len) {
    dev_warn_once(&nfd->dev->vdev->dev,
                  "virtio-gpu-nv: no shared memory region, so device memory "
                  "cannot be mapped\n");
    ret = -ENOTSUPP;
    goto out;
  }

place:
  window_off = le64_to_cpu(resp->guest_phys_addr);
  if (window_off > region->len || size > region->len - window_off ||
      !PAGE_ALIGNED(window_off)) {
    dev_warn(&nfd->dev->vdev->dev,
             "virtio-gpu-nv: mapping at %llu+%llu runs past the %llu-byte "
             "%s\n",
             window_off, size, region->len, uvm ? "UVM aperture" : "window");
    ret = -ERANGE;
    goto out;
  }

  ret = remap_pfn_range(vma, vma->vm_start,
                        (region->addr + window_off) >> PAGE_SHIFT, size,
                        vma->vm_page_prot);
  if (ret)
    goto out;

  kref_init(&m->ref);
  m->dev = nfd->dev;
  nvgpu_dev_get(m->dev);
  m->handle = nfd->handle;
  m->mapping_id = mapping_id;
  vma->vm_ops = &nvgpu_vm_ops;
  vma->vm_private_data = m;
  m = NULL;

out:
  if (ret && mapping_id)
    nvgpu_munmap(nfd->dev, nfd->handle, mapping_id);
  kfree(m);
  kfree(req);
  kfree(resp);
  return ret;
}

/* ───────── open / release ───────── */

/* Make a new descriptor waitable, and findable by the handle an event names. */
void nvgpu_fd_register(struct nvgpu_device *dev, struct nvgpu_fd *nfd) {
  unsigned long flags;

  init_waitqueue_head(&nfd->wq);
  atomic_set(&nfd->pending, 0);
  spin_lock_irqsave(&dev->fds_lock, flags);
  list_add(&nfd->node, &dev->fds);
  spin_unlock_irqrestore(&dev->fds_lock, flags);
}

void nvgpu_fd_unregister(struct nvgpu_device *dev,
                         struct nvgpu_fd *nfd) {
  unsigned long flags;

  spin_lock_irqsave(&dev->fds_lock, flags);
  list_del(&nfd->node);
  spin_unlock_irqrestore(&dev->fds_lock, flags);
  /* Anyone still in poll_wait() is woken so they can see the file go. */
  wake_up_interruptible_all(&nfd->wq);
}

void nvgpu_fd_get(struct nvgpu_fd *nfd) { refcount_inc(&nfd->ref); }

void nvgpu_dev_get(struct nvgpu_device *dev) { kref_get(&dev->ref); }

static void nvgpu_dev_release(struct kref *ref) {
  kfree(container_of(ref, struct nvgpu_device, ref));
}

void nvgpu_dev_put(struct nvgpu_device *dev) {
  kref_put(&dev->ref, nvgpu_dev_release);
}

/*
 * The last reference: the host file goes too. Always from process context --
 * a file's release, a DRM postclose, or a GEM proxy's free, which the core
 * runs from the last handle close or dma-buf release.
 */
void nvgpu_fd_put(struct nvgpu_fd *nfd) {
  struct nvgpu_device *dev = nfd->dev;

  if (!refcount_dec_and_test(&nfd->ref))
    return;
  nvgpu_close_handle(dev, nfd->handle);
  /* RM clients the file held are gone, and what they registered with them. */
  nvgpu_osdesc_reap(dev);
  kfree(nfd);
  nvgpu_dev_put(dev); /* taken when the open succeeded */
}

static int nvgpu_open_common(struct inode *inode, struct file *filp,
                             u32 device_type) {
  struct nvgpu_device *dev;
  struct nvgpu_fd *nfd;
  struct nvgpu_open_req *req;
  struct nvgpu_open_resp *resp;
  int ret;

  /* Recover nvgpu_device pointer depending on which cdev was opened */
  if (device_type == NVGPU_DEV_UVM || device_type == NVGPU_DEV_UVM_TOOLS)
    dev = container_of(inode->i_cdev, struct nvgpu_device, cdev_uvm);
  else if (device_type == NVGPU_DEV_CTL)
    dev = container_of(inode->i_cdev, struct nvgpu_device, cdev_ctl);
  else if (device_type == NVGPU_DEV_MODESET)
    dev = container_of(inode->i_cdev, struct nvgpu_device, cdev_modeset);
  else
    dev = container_of(inode->i_cdev, struct nvgpu_device,
                       cdev_gpu[iminor(inode)]);

  nfd = kzalloc(sizeof(*nfd), GFP_KERNEL);
  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !req || !resp) {
    kfree(nfd);
    kfree(req);
    kfree(resp);
    return -ENOMEM;
  }

  nfd->dev = dev;
  nfd->device_type = device_type;
  refcount_set(&nfd->ref, 1);

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->device_type = cpu_to_le32(device_type);
  req->flags = cpu_to_le32(filp->f_flags);

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0 || (s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    kfree(nfd);
    kfree(req);
    kfree(resp);
    if (ret < 0)
      return ret;
    return (s32)le32_to_cpu((__le32)resp->hdr.status);
  }

  nfd->handle = le32_to_cpu(resp->hdr.handle);
  /* The file pins the device, cdevs included (they are in it), until its
   * last nvgpu_fd_put(). */
  nvgpu_dev_get(dev);
  nvgpu_fd_register(nfd->dev, nfd);
  filp->private_data = nfd;
  kfree(req);
  kfree(resp);
  return 0;
}

static int nvgpu_gpu_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, (u32)iminor(inode));
}

static int nvgpu_ctl_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, NVGPU_DEV_CTL);
}

static int nvgpu_uvm_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, NVGPU_DEV_UVM);
}

static int nvgpu_release(struct inode *inode, struct file *filp) {
  struct nvgpu_fd *nfd = filp->private_data;

  nvgpu_fd_unregister(nfd->dev, nfd);
  nvgpu_fd_put(nfd);
  return 0;
}

/* ───────── file_operations tables ───────── */

static const struct file_operations nvgpu_gpu_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_gpu_open,
    .release = nvgpu_release,
    .unlocked_ioctl = nvgpu_ioctl,
    .mmap = nvgpu_mmap,
    .poll = nvgpu_poll,
};

static const struct file_operations nvgpu_ctl_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_ctl_open,
    .release = nvgpu_release,
    .unlocked_ioctl = nvgpu_ioctl,
    .mmap = nvgpu_mmap,
    .poll = nvgpu_poll,
};

static const struct file_operations nvgpu_uvm_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_uvm_open,
    .release = nvgpu_release,
    .unlocked_ioctl = nvgpu_uvm_ioctl,
    .mmap = nvgpu_mmap,
    .poll = nvgpu_poll,
};

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
static long nvgpu_ioctl_modeset(struct nvgpu_fd *nfd, unsigned int cmd,
                                void __user *uarg, u32 sz) {
  struct nvidia_modeset_outer outer;
  void __user *user_nested;
  u32 nested_size;
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  u32 used;

  if (copy_from_user(&outer, uarg, sizeof(outer)))
    return -EFAULT;

  user_nested = (void __user *)(unsigned long)le64_to_cpu(outer.pData);
  nested_size = le32_to_cpu(outer.dataSize);

  if (nested_size > 1024 * 1024)
    return -EINVAL;

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
        if (nvgpu_handle_for_fd(guest_fd, &handle) == 0) {
          u64 as_u64 = handle;

          memcpy(nested + NVGPU_NVKMS_SURFACE_FD_OFFSET, &as_u64,
                 sizeof(as_u64));
        } else {
          dev_warn_ratelimited(
              &nfd->dev->vdev->dev,
              "virtio-gpu-nv: REGISTER_SURFACE names fd %d, which is not one "
              "of ours; forwarding it unchanged\n",
              guest_fd);
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

/*
 * One flat ioctl round trip on a named backend handle, in and out of a kernel
 * buffer. `handle` rather than an nvgpu_fd because a GEM op forwards on the
 * handle of the file that owns the object, which is not always the caller's.
 */
long nvgpu_ioctl_flat_h(struct nvgpu_device *dev, u32 handle,
                        unsigned int cmd, void *kbuf, u32 sz) {
  int req_total = sizeof(struct nvgpu_ioctl_req) + sz;
  int resp_max = sizeof(struct nvgpu_ioctl_resp) + sz;
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  u32 used;
  long ret;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sz);
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = 0;
  req->deep_len = 0;
  memcpy(req_buf + sizeof(*req), kbuf, sz);

  ret = nvgpu_send_recv_used(dev, req_buf, req_total, resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;
  if (!nvgpu_resp_has(used, 0, sizeof(resp->hdr))) {
    ret = -EIO;
    goto out;
  }

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (long)(s32)le32_to_cpu((__le32)resp->hdr.status);
  if (nvgpu_resp_has(used, 0, sizeof(*resp)) &&
      le32_to_cpu(resp->data_len) >= sz &&
      nvgpu_resp_has(used, sizeof(*resp), sz))
    memcpy(kbuf, resp_buf + sizeof(*resp), sz);

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

static long nvgpu_modeset_ioctl(struct file *filp, unsigned int cmd,
                                unsigned long arg) {
  struct nvgpu_fd *nfd = filp->private_data;
  unsigned int ioc_type = _IOC_TYPE(cmd);
  unsigned int sz = _IOC_SIZE(cmd);
  void __user *uarg = (void __user *)arg;

  if (sz > 65536)
    return -EINVAL;

  /* nvidia-modeset ioctls use type 0x6d ('m'); a v2 backend takes them
   * through the schema (nvgpu_nvkms.c), a v1 one as a flat block. */
  if (ioc_type == 0x6d) {
    if (nfd->dev->v2)
      return nvgpu_nvkms_ioctl(nfd, cmd, uarg);
    return nvgpu_ioctl_modeset(nfd, cmd, uarg, sz);
  }

  /* Anything else (unlikely) falls back to the standard path */
  return nvgpu_ioctl(filp, cmd, arg);
}

static int nvgpu_modeset_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, NVGPU_DEV_MODESET);
}

static const struct file_operations nvgpu_modeset_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_modeset_open,
    .release = nvgpu_release,
    .unlocked_ioctl = nvgpu_modeset_ioctl,
    .mmap = nvgpu_mmap,
    .poll = nvgpu_modeset_poll,
};

struct nvgpu_fd *nvgpu_fd_from_file(struct file *f) {
  if (f->f_op == &nvgpu_gpu_fops || f->f_op == &nvgpu_ctl_fops ||
      f->f_op == &nvgpu_uvm_fops || f->f_op == &nvgpu_modeset_fops)
    return f->private_data;
  return nvgpu_drm_file_nfd(f);
}

/* ───────── /proc/driver/nvidia ───────── */

static int nvgpu_proc_version_show(struct seq_file *m, void *v) {
  struct nvgpu_device *dev = m->private;

  seq_printf(m,
             "NVRM version: NVIDIA UNIX x86_64 Kernel Module  %s\n"
             "GCC version:  gcc version 12.2.0\n",
             dev->driver_version);
  return 0;
}

static int nvgpu_proc_version_open(struct inode *inode, struct file *filp) {
  return single_open(filp, nvgpu_proc_version_show, pde_data(inode));
}

static const struct proc_ops nvgpu_proc_version_ops = {
    .proc_open = nvgpu_proc_version_open,
    .proc_read = seq_read,
    .proc_lseek = seq_lseek,
    .proc_release = single_release,
};

static int nvgpu_proc_params_show(struct seq_file *m, void *v) {
  struct nvgpu_device *dev = m->private;

  seq_printf(m, "NVreg_EnablePCIeGen3=1\n"
                "NVreg_MemoryPoolSize=0\n");
  (void)dev;
  return 0;
}

static int nvgpu_proc_params_open(struct inode *inode, struct file *filp) {
  return single_open(filp, nvgpu_proc_params_show, pde_data(inode));
}

static const struct proc_ops nvgpu_proc_params_ops = {
    .proc_open = nvgpu_proc_params_open,
    .proc_read = seq_read,
    .proc_lseek = seq_lseek,
    .proc_release = single_release,
};

/* Generic heap-backed proc file — used for all passthrough files */
struct nvgpu_proc_buf {
  char *data;
  size_t len;
};

static int nvgpu_proc_buf_show(struct seq_file *m, void *v) {
  struct nvgpu_proc_buf *b = m->private;
  seq_write(m, b->data, b->len);
  return 0;
}

static int nvgpu_proc_buf_open(struct inode *inode, struct file *filp) {
  return single_open(filp, nvgpu_proc_buf_show, pde_data(inode));
}

static const struct proc_ops nvgpu_proc_buf_ops = {
    .proc_open = nvgpu_proc_buf_open,
    .proc_read = seq_read,
    .proc_lseek = seq_lseek,
    .proc_release = single_release,
};

/* Simple directory cache — avoids duplicate proc_mkdir calls */
#define NVGPU_PROC_MAX_DIRS 32

struct nvgpu_proc_dir_cache {
  char path[128];
  struct proc_dir_entry *entry;
};

static struct nvgpu_proc_dir_cache nvgpu_dir_cache[NVGPU_PROC_MAX_DIRS];
static int nvgpu_dir_cache_count;

static void nvgpu_dir_cache_reset(void) {
  memset(nvgpu_dir_cache, 0, sizeof(nvgpu_dir_cache));
  nvgpu_dir_cache_count = 0;
}

static struct proc_dir_entry *
nvgpu_proc_mkdir_cached(const char *path, struct proc_dir_entry *parent) {
  int i;

  /* Check cache first */
  for (i = 0; i < nvgpu_dir_cache_count; i++) {
    if (strcmp(nvgpu_dir_cache[i].path, path) == 0)
      return nvgpu_dir_cache[i].entry;
  }

  /* Not cached — create it */
  struct proc_dir_entry *entry = proc_mkdir(path, parent);

  /* Cache it even if NULL — so we don't retry failed creates */
  if (nvgpu_dir_cache_count < NVGPU_PROC_MAX_DIRS) {
    strscpy(nvgpu_dir_cache[nvgpu_dir_cache_count].path, path, 128);
    nvgpu_dir_cache[nvgpu_dir_cache_count].entry = entry;
    nvgpu_dir_cache_count++;
  }

  return entry;
}

static struct proc_dir_entry *nvgpu_proc_mkdir_parents(char *pathbuf,
                                                       char **leaf_name) {
  struct proc_dir_entry *parent = NULL;
  char built[256] = {};
  char *slash;
  char *p;

  slash = strrchr(pathbuf, '/');
  if (!slash) {
    *leaf_name = pathbuf;
    return NULL;
  }

  *leaf_name = slash + 1;
  *slash = '\0';

  /* Walk each component, building the full path as we go
   * so the cache key is always the full absolute component */
  p = pathbuf;
  while (*p) {
    char *next = strchr(p, '/');
    if (next)
      *next = '\0';

    /* Append component to built path */
    if (built[0])
      strlcat(built, "/", sizeof(built));
    strlcat(built, p, sizeof(built));

    /* The proc core makes /proc/driver itself (proc_root_init); a second
     * proc_mkdir of it WARNs "already registered". Every path is created
     * from the root by its full name, so there is nothing to look up. */
    if (strcmp(built, "driver") == 0)
      parent = NULL;
    else
      parent = nvgpu_proc_mkdir_cached(built, NULL);

    if (next) {
      *next = '/';
      p = next + 1;
    } else {
      break;
    }
  }

  /* No entry for the leaf's directory (it is /proc/driver, or its mkdir
   * failed): name the leaf in full, which the proc core resolves from the
   * root, rather than dropping it into /proc itself. */
  if (!parent) {
    *slash = '/';
    *leaf_name = pathbuf;
  }

  return parent;
}

static int nvgpu_proc_init(struct nvgpu_device *dev) {
  struct nvgpu_msg_hdr *req;
  u8 *resp_buf, *p, *end;
  /* 512 KiB — vastly more than needed, avoids any size guessing */
  const size_t resp_size = 512 * 1024;
  u32 used;
  int ret = 0;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  if (!req)
    return -ENOMEM;

  /*
   * Any memory will do: nvgpu_send_recv_used() copies through transport
   * buffers, so this is never put on the ring itself. It used to be, with
   * sg_init_one(), which is wrong for the vmalloc memory kvmalloc may return.
   */
  resp_buf = kvzalloc(resp_size, GFP_KERNEL);
  if (!resp_buf) {
    kfree(req);
    return -ENOMEM;
  }

  req->msg_type = cpu_to_le32(NVGPU_MSG_GET_PROC_FILES);
  req->handle = 0;
  req->status = 0;
  req->req_id = 0;

  ret = nvgpu_send_recv_used(dev, req, sizeof(*req), resp_buf, resp_size,
                             &used);
  if (ret < 0) {
    dev_err(&dev->vdev->dev, "virtio-gpu-nv: GET_PROC_FILES failed: %d\n", ret);
    goto out;
  }

  /* The stream has no header; what the device wrote is where it ends. */
  p = resp_buf;
  end = resp_buf + used;

  nvgpu_dir_cache_reset();

  while (p + 8 <= end) {
    u32 path_len, content_len;
    struct nvgpu_proc_buf *buf;
    char *pathbuf, *leaf;
    struct proc_dir_entry *parent = NULL;

    memcpy(&path_len, p, 4);
    path_len = le32_to_cpu((__le32)path_len);
    memcpy(&content_len, p + 4, 4);
    content_len = le32_to_cpu((__le32)content_len);
    p += 8;

    if (path_len == 0)
      break; /* terminator */

    if (p + path_len + content_len > end) {
      dev_warn(&dev->vdev->dev, "virtio-gpu-nv: proc stream truncated\n");
      break;
    }

    buf = kzalloc(sizeof(*buf), GFP_KERNEL);
    if (!buf) {
      ret = -ENOMEM;
      goto out;
    }

    buf->data = kmemdup(p + path_len, content_len, GFP_KERNEL);
    if (!buf->data) {
      kfree(buf);
      ret = -ENOMEM;
      goto out;
    }
    buf->len = content_len;

    pathbuf = kmalloc(path_len + 1, GFP_KERNEL);
    if (!pathbuf) {
      kfree(buf->data);
      kfree(buf);
      ret = -ENOMEM;
      goto out;
    }
    memcpy(pathbuf, p, path_len);
    pathbuf[path_len] = '\0';

    parent = nvgpu_proc_mkdir_parents(pathbuf, &leaf);
    proc_create_data(leaf, 0444, parent, &nvgpu_proc_buf_ops, buf);
    dev_dbg(&dev->vdev->dev, "virtio-gpu-nv: /proc/%s (%u bytes)\n", pathbuf,
            content_len);

    kfree(pathbuf);
    p += path_len + content_len;
  }

out:
  kvfree(resp_buf);
  kfree(req);
  return ret;
}

static char *nvgpu_devnode(const struct device *dev, umode_t *mode) {
  if (mode)
    *mode = 0666;
  return NULL;
}

/* --- SYSTEM BUS PCI DEVS --- */

static int nvgpu_pci_read(struct pci_bus *bus, unsigned int devfn, int where,
                          int size, u32 *val) {
  struct nvgpu_pci_root *root = bus->sysdata;
  u8 slot = PCI_SLOT(devfn);
  u8 func = PCI_FUNC(devfn);

  /* Only respond to our specific device */
  if (slot != root->slot.slot || func != root->slot.func) {
    *val = ~0u;
    return PCIBIOS_DEVICE_NOT_FOUND;
  }

  if (!root->slot.config_valid || where + size > (int)sizeof(root->slot.config)) {
    *val = ~0u;
    return PCIBIOS_BAD_REGISTER_NUMBER;
  }

  switch (size) {
  case 1:
    *val = root->slot.config[where];
    break;
  case 2:
    *val = le16_to_cpu(*(u16 *)&root->slot.config[where]);
    break;
  case 4:
    *val = le32_to_cpu(*(u32 *)&root->slot.config[where]);
    break;
  default:
    *val = ~0u;
    return PCIBIOS_BAD_REGISTER_NUMBER;
  }
  return PCIBIOS_SUCCESSFUL;
}

static int nvgpu_pci_write(struct pci_bus *bus, unsigned int devfn, int where,
                           int size, u32 val) {
  /* Config space is read-only from guest perspective */
  return PCIBIOS_FUNC_NOT_SUPPORTED;
}

static struct pci_ops nvgpu_pci_ops = {
    .read = nvgpu_pci_read,
    .write = nvgpu_pci_write,
};

/* Parse "DDDD:BB:SS.F" into components.
 * Returns 0 on success. */
static int nvgpu_parse_pci_addr(const char *addr, u16 *domain, u8 *bus,
                                u8 *slot, u8 *func) {
  unsigned int d, b, s, f;

  if (sscanf(addr, "%04x:%02x:%02x.%1x", &d, &b, &s, &f) != 4)
    return -EINVAL;

  *domain = (u16)d;
  *bus = (u8)b;
  *slot = (u8)s;
  *func = (u8)f;
  return 0;
}

static int nvgpu_pci_init(struct nvgpu_device *dev) {
  int i, ret = 0;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];
    struct pci_host_bridge *bridge;
    struct resource *bus_res;

    if (!root->slot.config_valid) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: no config space for %s, skipping\n",
               root->slot.pci_addr);
      continue;
    }

    bridge = pci_alloc_host_bridge(0);
    if (!bridge) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_alloc_host_bridge failed for %s\n",
              root->slot.pci_addr);
      ret = -ENOMEM;
      continue;
    }

    /* One bus resource covering exactly our bus number */
    bus_res = kzalloc(sizeof(*bus_res), GFP_KERNEL);
    if (!bus_res) {
      pci_free_host_bridge(bridge);
      ret = -ENOMEM;
      continue;
    }
    bus_res->start = root->slot.bus_nr;
    bus_res->end = root->slot.bus_nr;
    bus_res->flags = IORESOURCE_BUS;
    pci_add_resource(&bridge->windows, bus_res);

    bridge->dev.parent = &dev->vdev->dev;
    root->domain = (int)root->slot.domain;
    /* No node to claim: the GPU is the host's, and the guest's idea of
     * distance to it means nothing. NUMA_NO_NODE lets every allocation made
     * against this device fall back to the caller's node. */
    root->node = NUMA_NO_NODE;
    bridge->sysdata = root;
    bridge->ops = &nvgpu_pci_ops;
    bridge->busnr = root->slot.bus_nr;
    bridge->domain_nr = root->slot.domain; /* parsed u16, not ASCII bytes */
    root->nvdev = dev;

    ret = pci_scan_root_bus_bridge(bridge);
    if (ret) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_scan_root_bus_bridge %s: %d\n",
              root->slot.pci_addr, ret);
      pci_free_host_bridge(bridge);
      kfree(bus_res);
      continue;
    }

    pci_bus_add_devices(bridge->bus);
    root->bridge = bridge;
    root->registered = true;

    /* Save the one pci_dev on this bus so DRI init can use it as a parent */
    {
      struct pci_dev *pdev;
      list_for_each_entry(pdev, &bridge->bus->devices, bus_list) {
        root->pdev = pdev;
        break;
      }
    }

    dev_info(&dev->vdev->dev, "virtio-gpu-nv: registered fake PCI device %s\n",
             root->slot.pci_addr);
  }

  return ret;
}

static void nvgpu_pci_cleanup(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];

    if (!root->registered)
      continue;

    pci_remove_root_bus(root->bridge->bus);
    /* pci_remove_root_bus frees the bridge */
    root->bridge = NULL;
    root->registered = false;
  }
}

/* ───────── GET_SYS_FILES handler (guest side) ──────────────────────────── */

/*
 * Section 3: the host card nodes. A backend sends it in every mode, for the
 * host card numbers (the Wayland devmap maps a compositor's scanout dev_t by
 * them); the cards are openable only when it also says NVGPU_BCAP_KMS_CARD
 * (nvgpu_kms_open() checks, and the backend refuses OPEN_KMS otherwise). An
 * older backend sends it only with --kms-card, or ends the stream after
 * section 2, and then there is nothing between `p` and `end` and no card is
 * recorded; the parse never runs past what the device wrote.
 */
static const u8 *nvgpu_parse_card_section(struct nvgpu_device *dev,
                                          const u8 *p, const u8 *end) {
  struct nvgpu_card_record rec;
  __le32 raw_count;
  u32 count, i;

  dev->num_card_recs = 0;
  if (end - p < (ptrdiff_t)sizeof(raw_count))
    return NULL;
  memcpy(&raw_count, p, sizeof(raw_count));
  count = le32_to_cpu(raw_count);
  p += sizeof(raw_count);

  for (i = 0; i < count; i++) {
    struct nvgpu_card_rec *c;
    u32 name_len, render_index;

    if (end - p < (ptrdiff_t)sizeof(rec)) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card section truncated at entry %u\n", i);
      return NULL;
    }
    memcpy(&rec, p, sizeof(rec));
    p += sizeof(rec);
    name_len = le32_to_cpu(rec.name_len);
    render_index = le32_to_cpu(rec.render_index);
    if (name_len == 0 || name_len > end - p) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card entry %u bad name_len %u\n", i, name_len);
      return NULL;
    }

    /*
     * Kept in the order sent even when unusable, because EV_HOTPLUG names a
     * card by its position here; one that names no DRI device we registered
     * is recorded and never attached.
     */
    if (dev->num_card_recs >= NVGPU_MAX_DRI_DEVS) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card entry %u is past the %d this driver keeps\n",
               i, NVGPU_MAX_DRI_DEVS);
      return NULL;
    }
    c = &dev->cards[dev->num_card_recs];
    memset(c->name, 0, sizeof(c->name));
    memcpy(c->name, p, min_t(u32, name_len, sizeof(c->name) - 1));
    c->major = le32_to_cpu(rec.major);
    c->minor = le32_to_cpu(rec.minor);
    c->render_index = render_index;
    p += name_len;

    if (render_index < (u32)dev->num_dri_devs &&
        dev->dri_devs[render_index].card_index < 0)
      dev->dri_devs[render_index].card_index = dev->num_card_recs;
    else
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card %s names DRI record %u, which is not "
               "one this guest keeps or already has a card\n",
               c->name, render_index);

    dev_info(&dev->vdev->dev,
             "virtio-gpu-nv: host card node %s (%u:%u) for DRI record %u\n",
             c->name, c->major, c->minor, render_index);
    dev->num_card_recs++;
  }
  return p;
}

/*
 * Section 4: the size of each DRI record's host GET_DEV_INFO struct, in
 * section 2's order. The record's nine words are the 36-byte layout whatever
 * this says (the backend normalises a 535 host's 20 bytes, and 545's and
 * 550's 28 and 32); the size says which of them the host really had, which
 * nvgpu_drm_get_dev_info() uses to name a guest-userspace/host-kernel release
 * mismatch. Absent from an older backend, whose records stay at 36.
 */
static void nvgpu_parse_dev_info_sizes(struct nvgpu_device *dev, const u8 *p,
                                       const u8 *end) {
  __le32 raw;
  u32 count, i;

  if (!p || end - p < (ptrdiff_t)sizeof(raw))
    return;
  memcpy(&raw, p, sizeof(raw));
  count = le32_to_cpu(raw);
  p += sizeof(raw);
  if (count > (u32)((end - p) / sizeof(raw))) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: GET_DEV_INFO size section truncated\n");
    return;
  }
  for (i = 0; i < count && i < (u32)dev->num_dri_devs; i++) {
    memcpy(&raw, p + 4 * i, sizeof(raw));
    dev->dri_devs[i].dev_info_size = le32_to_cpu(raw);
  }
}

static int nvgpu_fetch_sys_files(struct nvgpu_device *dev) {
  struct nvgpu_msg_hdr *req;
  u8 *resp_buf;
  u8 *p, *end;
  const int resp_max = 128 * 1024;
  bool dri_complete = false;
  u32 used;
  int ret = 0;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  if (!req)
    return -ENOMEM;

  /* kvzalloc — zeroed so unwritten tail is never misread as data */
  resp_buf = kvzalloc(resp_max, GFP_KERNEL);
  if (!resp_buf) {
    kfree(req);
    return -ENOMEM;
  }

  req->msg_type = cpu_to_le32(NVGPU_MSG_GET_SYS_FILES);
  req->handle = 0;
  req->status = 0;
  req->req_id = 0;

  ret = nvgpu_send_recv_used(dev, req, sizeof(*req), resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;

  /* A headerless stream: it ends where the device stopped writing. */
  p = resp_buf;
  end = resp_buf + used;

  /* ── Section 1: sysfs files ─────────────────────────────────────── */
  while (p + 8 <= end) {
    /* Fix: memcpy for unaligned u32 reads, matching nvgpu_proc_init style */
    __le32 raw_path_len, raw_content_len;
    u32 path_len, content_len, copy_len;
    char path[256];

    memcpy(&raw_path_len, p, sizeof(__le32));
    memcpy(&raw_content_len, p + 4, sizeof(__le32));
    path_len = le32_to_cpu(raw_path_len);
    content_len = le32_to_cpu(raw_content_len);
    p += 8;

    if (path_len == 0 && content_len == 0)
      break; /* terminator */

    if (p + path_len + content_len > end) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: sys stream truncated at sysfs section\n");
      break;
    }

    /* Safe path extraction — explicit memset, no {} initialiser */
    memset(path, 0, sizeof(path));
    copy_len = min(path_len, (u32)(sizeof(path) - 1));
    memcpy(path, p, copy_len);
    p += path_len;

    if (p + content_len > end) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: sys stream truncated at content\n");
      break;
    }

    if (strncmp(path, "bus/pci/devices/", 16) == 0) {
      char *rest = path + 16; /* "<addr>/<filename>" */
      char *slash = strchr(rest, '/');

      if (slash && strcmp(slash + 1, "config") == 0) {
        char pci_addr[16] = {};
        int pi;

        memcpy(pci_addr, rest,
               min((size_t)(slash - rest), sizeof(pci_addr) - 1));

        /* Find existing slot or allocate new one */
        for (pi = 0; pi < dev->num_pci_roots; pi++)
          if (strcmp(dev->pci_roots[pi].slot.pci_addr, pci_addr) == 0)
            break;

        /* Match against known GPU slots to avoid creating
         * entries for unrelated PCI devices */
        if (pi == dev->num_pci_roots) {
          int gi;
          for (gi = 0; gi < (int)dev->num_gpus; gi++) {
            if (strcmp(dev->gpu_slots[gi].pci_addr, pci_addr) == 0) {
              pi = dev->num_pci_roots;
              if (pi < NVGPU_MAX_PCI_SLOTS) {
                memcpy(dev->pci_roots[pi].slot.pci_addr, pci_addr,
                       sizeof(pci_addr));
                if (nvgpu_parse_pci_addr(pci_addr,
                                         &dev->pci_roots[pi].slot.domain,
                                         &dev->pci_roots[pi].slot.bus_nr,
                                         &dev->pci_roots[pi].slot.slot,
                                         &dev->pci_roots[pi].slot.func) == 0)
                  dev->num_pci_roots++;
                else
                  pi = dev->num_pci_roots; /* parse failed */
              }
              break;
            }
          }
        }

        if (pi < dev->num_pci_roots) {
          struct nvgpu_pci_slot *ps = &dev->pci_roots[pi].slot;
          u32 copy = min(content_len, (u32)sizeof(ps->config));
          memcpy(ps->config, p, copy);
          ps->config_valid = true;
          dev_dbg(&dev->vdev->dev,
                  "virtio-gpu-nv: stored config space for %s (%u bytes)\n",
                  pci_addr, copy);
        }
      }
      /* Other PCI sysfs files (vendor, device, etc.) are handled
       * automatically by the kernel once the pci_dev is registered */
    }
    /* Unknown paths silently skipped */

    p += content_len;
  }

  /* ── Section 2: DRI devices ─────────────────────────────────────── */
  if (p + 4 > end) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: sys stream truncated before DRI section\n");
    goto out;
  }

  {
    __le32 raw_num_dri;
    u32 num_dri, i;

    memcpy(&raw_num_dri, p, sizeof(__le32));
    num_dri = le32_to_cpu(raw_num_dri);
    p += 4;

    /*
     * Every record is walked, and only the first NVGPU_MAX_DRI_DEVS kept:
     * section 3 starts after the last one, so stopping early would leave the
     * card records unreachable.
     */
    dev->num_dri_devs = 0;

    for (i = 0; i < num_dri; i++) {
      __le32 raw_name_len, raw_major, raw_minor, raw_slot, raw_info;
      u32 name_len, major, minor, slot_index, nl;
      u32 info[NVGPU_DEV_INFO_WORDS];
      int idx, w;

      /* name_len + major + minor + slot_index, then the dev_info words */
      if (p + NVGPU_DRI_RECORD_BYTES > end) {
        dev_warn(&dev->vdev->dev,
                 "virtio-gpu-nv: DRI section truncated at entry %u\n", i);
        break;
      }

      memcpy(&raw_name_len, p, sizeof(__le32));
      memcpy(&raw_major, p + 4, sizeof(__le32));
      memcpy(&raw_minor, p + 8, sizeof(__le32));
      memcpy(&raw_slot, p + 12, sizeof(__le32));
      name_len = le32_to_cpu(raw_name_len);
      major = le32_to_cpu(raw_major);
      minor = le32_to_cpu(raw_minor);
      slot_index = le32_to_cpu(raw_slot);
      for (w = 0; w < NVGPU_DEV_INFO_WORDS; w++) {
        memcpy(&raw_info, p + 16 + 4 * w, sizeof(__le32));
        info[w] = le32_to_cpu(raw_info);
      }
      p += NVGPU_DRI_RECORD_BYTES;

      if (name_len == 0 || name_len > end - p) {
        dev_warn(&dev->vdev->dev,
                 "virtio-gpu-nv: DRI entry %u bad name_len %u\n", i, name_len);
        break;
      }
      if (dev->num_dri_devs >= NVGPU_MAX_DRI_DEVS) {
        dev_warn(&dev->vdev->dev,
                 "virtio-gpu-nv: DRI entry %u is past the %d this driver "
                 "keeps\n",
                 i, NVGPU_MAX_DRI_DEVS);
        p += name_len;
        continue;
      }

      idx = dev->num_dri_devs;
      nl = min(name_len, (u32)(sizeof(dev->dri_devs[idx].name) - 1));
      memset(dev->dri_devs[idx].name, 0, sizeof(dev->dri_devs[idx].name));
      memcpy(dev->dri_devs[idx].name, p, nl);
      dev->dri_devs[idx].major = major;
      dev->dri_devs[idx].minor = minor;
      dev->dri_devs[idx].slot_index = slot_index;
      dev->dri_devs[idx].card_index = -1;
      memcpy(dev->dri_devs[idx].dev_info, info, sizeof(info));
      dev->dri_devs[idx].dev_info_size = sizeof(info);
      dev->num_dri_devs++;

      dev_info(&dev->vdev->dev,
               "virtio-gpu-nv: DRI %s (%u:%u) slot %u, nvidia gpu_id=0x%x, "
               "page kind %u/%u, sector layout %u\n",
               dev->dri_devs[idx].name, major, minor, slot_index, info[0],
               info[4], info[5], info[6]);
      p += name_len;
    }
    dri_complete = i == num_dri;
  }

  if (dri_complete)
    nvgpu_parse_dev_info_sizes(dev, nvgpu_parse_card_section(dev, p, end),
                               end);

out:
  kvfree(resp_buf);
  kfree(req);
  return ret;
}

/* ───────── /sys/module/nvidia{,_uvm} initstate fakes ───────── */

static struct kobject *nvgpu_module_kobj;     /* /sys/module/nvidia     */
static struct kobject *nvgpu_uvm_module_kobj; /* /sys/module/nvidia_uvm */
/*
 * /sys/module/nvidia_modeset
 *
 * This is a gate, not decoration. NVIDIA's userspace reads
 * /sys/module/nvidia_modeset/initstate before it will go near
 * /dev/nvidia-modeset, and with the file absent it never opens the device and
 * never issues an NVKMS call. Nothing fails visibly when that happens: the
 * Vulkan ICD simply stops short and reports that it found no driver.
 */
static struct kobject *nvgpu_modeset_module_kobj;

static ssize_t initstate_show(struct kobject *kobj, struct kobj_attribute *attr,
                              char *buf) {
  return sysfs_emit(buf, "live\n");
}

static struct kobj_attribute initstate_attr = __ATTR_RO(initstate);

/*
 * The kset behind /sys/module/.
 *
 * Built in-tree we can just name module_kset. Built as a loadable module we
 * cannot -- but this module is itself registered under /sys/module, and its
 * kobject's parent *is* module_kset's kobject, so the same kset is reachable
 * without the unexported symbol.
 */
static struct kset *nvgpu_module_kset(void) {
#ifdef MODULE
  struct kobject *parent = THIS_MODULE->mkobj.kobj.parent;

  if (!parent)
    return NULL;
  return container_of(parent, struct kset, kobj);
#else
  return module_kset;
#endif
}

static void nvgpu_module_sysfs_init(struct nvgpu_device *dev) {
  struct kobject *modules_kobj;
  struct kset *mkset = nvgpu_module_kset();

  if (!mkset) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: cannot reach /sys/module, skipping nvidia stubs\n");
    return;
  }

  /* /sys/module/ is the parent of all module kobjects */
  modules_kobj = kset_find_obj(mkset, "nvidia");
  if (modules_kobj) {
    /* nvidia.ko already loaded somehow — don't duplicate */
    kobject_put(modules_kobj);
    return;
  }

  nvgpu_module_kobj = kobject_create_and_add("nvidia", &mkset->kobj);
  if (!nvgpu_module_kobj) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: failed to create /sys/module/nvidia\n");
    return;
  }
  if (sysfs_create_file(nvgpu_module_kobj, &initstate_attr.attr))
    dev_warn(&dev->vdev->dev, "virtio-gpu-nv: failed initstate under nvidia\n");

  nvgpu_uvm_module_kobj =
      kobject_create_and_add("nvidia_uvm", &mkset->kobj);
  if (!nvgpu_uvm_module_kobj) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: failed to create /sys/module/nvidia_uvm\n");
    return;
  }
  if (sysfs_create_file(nvgpu_uvm_module_kobj, &initstate_attr.attr))
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: failed initstate under nvidia_uvm\n");

  nvgpu_modeset_module_kobj =
      kobject_create_and_add("nvidia_modeset", &mkset->kobj);
  if (!nvgpu_modeset_module_kobj) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: failed to create nvidia_modeset module kobj\n");
  } else if (sysfs_create_file(nvgpu_modeset_module_kobj,
                               &initstate_attr.attr)) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: failed initstate under nvidia_modeset\n");
  }

  dev_info(&dev->vdev->dev,
           "virtio-gpu-nv: created /sys/module/nvidia{,_uvm,_modeset}/initstate\n");
}

static void nvgpu_module_sysfs_cleanup(void) {
  if (nvgpu_modeset_module_kobj) {
    sysfs_remove_file(nvgpu_modeset_module_kobj, &initstate_attr.attr);
    kobject_put(nvgpu_modeset_module_kobj);
    nvgpu_modeset_module_kobj = NULL;
  }
  if (nvgpu_uvm_module_kobj) {
    sysfs_remove_file(nvgpu_uvm_module_kobj, &initstate_attr.attr);
    kobject_put(nvgpu_uvm_module_kobj);
    nvgpu_uvm_module_kobj = NULL;
  }
  if (nvgpu_module_kobj) {
    sysfs_remove_file(nvgpu_module_kobj, &initstate_attr.attr);
    kobject_put(nvgpu_module_kobj);
    nvgpu_module_kobj = NULL;
  }
}

/* ── nvidia-caps fops — proxy to host like everything else ── */

static int nvgpu_caps_open(struct inode *inode, struct file *filp) {
  /* caps devices are read-only capability checks.
   * NVIDIA userspace opens them, does a few ioctls, closes.
   * For now return success with a NULL private_data —
   * if actual ioctls are needed we'll add VMM proxying. */
  filp->private_data = NULL;
  return 0;
}

static int nvgpu_caps_release(struct inode *inode, struct file *filp) {
  return 0;
}

static long nvgpu_caps_ioctl(struct file *filp, unsigned int cmd,
                             unsigned long arg) {
  /* Most caps ioctls just query capability bits.
   * Return 0 (success) — tells userspace "no special capabilities"
   * which is correct for a non-MIG single GPU. */
  return 0;
}

static const struct file_operations nvgpu_caps_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_caps_open,
    .release = nvgpu_caps_release,
    .unlocked_ioctl = nvgpu_caps_ioctl,
};

static struct class *nvgpu_caps_class;

static char *nvgpu_caps_devnode(const struct device *dev, umode_t *mode) {
  if (mode)
    *mode = 0444;
  return kasprintf(GFP_KERNEL, "nvidia-caps/%s", dev_name(dev));
}

/*
 * /dev/nvidia-caps/nvidia-cap{1,2}. Optional: a guest without them only loses
 * MIG capability checks, so a failure here is logged and the rest goes on.
 * nvgpu_caps_class is set only once all of it is in place, which is what
 * cleanup keys on -- a half-made registration is undone here, not left for it.
 */
static void nvgpu_caps_init(struct nvgpu_device *dev) {
  struct class *cls;
  int ret;

  dev->caps_devno = MKDEV(NV_CAPS_MAJOR, 1);
  ret = register_chrdev_region(dev->caps_devno, 2, "nvidia-caps");
  if (ret) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: cannot register nvidia-caps: %d\n", ret);
    return;
  }

  cls = class_create("nvidia-caps");
  if (IS_ERR(cls)) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: cannot create the nvidia-caps class: %ld\n",
             PTR_ERR(cls));
    unregister_chrdev_region(dev->caps_devno, 2);
    return;
  }
  cls->devnode = nvgpu_caps_devnode;

  cdev_init(&dev->cdev_caps, &nvgpu_caps_fops);
  dev->cdev_caps.owner = THIS_MODULE;
  ret = cdev_add(&dev->cdev_caps, dev->caps_devno, 2);
  if (ret) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: cannot add the nvidia-caps cdev: %d\n", ret);
    class_destroy(cls);
    unregister_chrdev_region(dev->caps_devno, 2);
    return;
  }

  device_create(cls, &dev->vdev->dev, MKDEV(NV_CAPS_MAJOR, 1), NULL,
                "nvidia-cap1");
  device_create(cls, &dev->vdev->dev, MKDEV(NV_CAPS_MAJOR, 2), NULL,
                "nvidia-cap2");
  nvgpu_caps_class = cls;
  dev_info(&dev->vdev->dev,
           "virtio-gpu-nv: registered /dev/nvidia-caps/nvidia-cap{1,2}\n");
}

static void nvgpu_caps_cleanup(struct nvgpu_device *dev) {
  if (!nvgpu_caps_class)
    return;
  device_destroy(nvgpu_caps_class, MKDEV(NV_CAPS_MAJOR, 1));
  device_destroy(nvgpu_caps_class, MKDEV(NV_CAPS_MAJOR, 2));
  cdev_del(&dev->cdev_caps);
  unregister_chrdev_region(dev->caps_devno, 2);
  class_destroy(nvgpu_caps_class);
  nvgpu_caps_class = NULL;
}

/* ───────── Probe / remove ───────── */

static int nvgpu_probe(struct virtio_device *vdev) {
  struct nvgpu_device *dev;
  struct virtqueue_info vqs_info[] = {
      {"control", nvgpu_ctrl_vq_cb},
      {"event", nvgpu_event_vq_cb},
  };
  struct virtqueue *vqs[2];
  dev_t gpu_devno;
  int ret, i;

  /* Not devm: open files and objects may outlive remove(), and every one of
   * them names this (see struct nvgpu_device's ref). */
  dev = kzalloc(sizeof(*dev), GFP_KERNEL);
  if (!dev)
    return -ENOMEM;
  kref_init(&dev->ref);

  dev->vdev = vdev;
  vdev->priv = dev;
  INIT_LIST_HEAD(&dev->fds);
  spin_lock_init(&dev->fds_lock);
  nvgpu_osdesc_init(dev);

  /* Find virtqueues */
  ret = virtio_find_vqs(vdev, 2, vqs, vqs_info, NULL);
  if (ret) {
    vdev->priv = NULL;
    nvgpu_dev_put(dev);
    return ret;
  }

  dev->ctrl_vq = vqs[0];
  dev->event_vq = vqs[1];

  /*
   * Request contexts, and somewhere for the host to put an event. Until the
   * event queue had buffers it was negotiated and empty, so the host had no
   * way to say a descriptor had become readable and the guest's poll() had
   * nothing to report.
   */
  ret = nvgpu_xfer_init(dev);
  if (ret)
    goto err_vqs;

  /* Read config space written by the VMM at device creation */
  virtio_cread_bytes(vdev, 0, dev->driver_version, 32);
  dev->driver_version[31] = '\0';
  dev->uvm = nvgpu_uvm_select(dev->driver_version);
  if (!dev->uvm)
    dev_warn(&vdev->dev,
             "virtio-gpu-nv: no UVM table for host driver \"%s\"; "
             "/dev/nvidia-uvm refuses every command\n",
             dev->driver_version);
  virtio_cread(vdev, struct virtio_gpu_nv_config, num_gpus, &dev->num_gpus);
  virtio_cread(vdev, struct virtio_gpu_nv_config, caps, &dev->caps);

  if (dev->num_gpus == 0 || dev->num_gpus > 248) {
    dev_err(&vdev->dev, "virtio-gpu-nv: bad num_gpus %u\n", dev->num_gpus);
    ret = -EINVAL;
    goto err_xfer;
  }

  /* GPU info records */
  {
    u32 i;
    for (i = 0; i < dev->num_gpus && i < 8; i++) {
      size_t off = offsetof(struct virtio_gpu_nv_config, gpus[i]);
      virtio_cread_bytes(vdev, off, &dev->gpu_slots[i],
                         sizeof(dev->gpu_slots[i]));

      /* Safety: ensure pci_addr is NUL-terminated before logging */
      dev->gpu_slots[i].pci_addr[15] = '\0';

      dev_info(&vdev->dev,
               "virtio-gpu-nv: GPU%u  pci=%s  minor=%u  info_len=%u\n", i,
               dev->gpu_slots[i].pci_addr, le32_to_cpu(dev->gpu_slots[i].minor),
               le32_to_cpu(dev->gpu_slots[i].info_len));
    }
  }

  /* FD translation table */
  virtio_cread(vdev, struct virtio_gpu_nv_config, num_fd_translations,
               &dev->num_fd_translations);

  if (dev->num_fd_translations > 16)
    dev->num_fd_translations = 16;

  if (dev->num_fd_translations > 0) {
    size_t off = offsetof(struct virtio_gpu_nv_config, fd_translations);
    virtio_cread_bytes(vdev, off, dev->fd_translations,
                       dev->num_fd_translations *
                           sizeof(dev->fd_translations[0]));
  }

  dev_info(&vdev->dev, "virtio-gpu-nv: %u fd-translation ioctl(s) registered\n",
           dev->num_fd_translations);

  /* Ensure virtio is running before we open devices */
  virtio_device_ready(vdev);

  /*
   * The UVM aperture, before HELLO, which tells the backend how large it is.
   * A VMM that offers none leaves UVM files unmappable, as they always were.
   */
  if (virtio_get_shm_region(vdev, &dev->uvm_aperture, NVGPU_SHM_ID_UVM))
    dev_info(&vdev->dev, "virtio-gpu-nv: UVM aperture at %pa, %llu bytes\n",
             &dev->uvm_aperture.addr, dev->uvm_aperture.len);
  else
    dev->uvm_aperture.len = 0;

  /*
   * Which protocol, before anything else is said: the answer sizes every
   * request after it, and the DRM devices registered below advertise
   * features (syncobjs) only a v2 backend with the right caps can serve.
   */
  nvgpu_xfer_hello(dev);
  if ((dev->backend_caps & NVGPU_BCAP_UVM_MAP) &&
      dev->uvm_aperture.len >= PAGE_SIZE)
    nvgpu_region_check_wb(dev, &dev->uvm_aperture, "the UVM aperture");

  /* Create device class once */
  nvgpu_class = class_create("nvidia");
  if (IS_ERR(nvgpu_class)) {
    ret = PTR_ERR(nvgpu_class);
    nvgpu_class = NULL;
    goto err_ready;
  }

  /* Set more open permissions to device node */
  nvgpu_class->devnode = nvgpu_devnode;

  /* Register /dev/nvidia0 … /dev/nvidia<N-1> */
  gpu_devno = MKDEV(NV_MAJOR, 0);
  ret = register_chrdev_region(gpu_devno, dev->num_gpus, "nvidia");
  if (ret)
    goto err_class;

  for (i = 0; i < (int)dev->num_gpus; i++) {
    cdev_init(&dev->cdev_gpu[i], &nvgpu_gpu_fops);
    dev->cdev_gpu[i].owner = THIS_MODULE;
    ret = cdev_add(&dev->cdev_gpu[i], MKDEV(NV_MAJOR, i), 1);
    if (ret)
      goto err_gpu_cdevs;
    device_create(nvgpu_class, &vdev->dev, MKDEV(NV_MAJOR, i), NULL, "nvidia%d",
                  i);
  }

  /* Register /dev/nvidiactl */
  ret = register_chrdev_region(MKDEV(NV_MAJOR, NV_CTL_MINOR), 1, "nvidiactl");
  if (ret)
    goto err_gpu_cdevs;

  cdev_init(&dev->cdev_ctl, &nvgpu_ctl_fops);
  dev->cdev_ctl.owner = THIS_MODULE;
  ret = cdev_add(&dev->cdev_ctl, MKDEV(NV_MAJOR, NV_CTL_MINOR), 1);
  if (ret)
    goto err_ctl_region;

  device_create(nvgpu_class, &vdev->dev, MKDEV(NV_MAJOR, NV_CTL_MINOR), NULL,
                "nvidiactl");

  /* Register /dev/nvidia-uvm (major should match host) */
  dev->uvm_devno = MKDEV(NV_UVM_MAJOR, 0);
  ret = register_chrdev_region(dev->uvm_devno, 2, "nvidia-uvm");
  if (ret)
    goto err_ctl_cdev;

  cdev_init(&dev->cdev_uvm, &nvgpu_uvm_fops);
  dev->cdev_uvm.owner = THIS_MODULE;
  ret = cdev_add(&dev->cdev_uvm, dev->uvm_devno, 1);
  if (ret)
    goto err_uvm_region;

  device_create(nvgpu_class, &vdev->dev, dev->uvm_devno, NULL, "nvidia-uvm");
  device_create(nvgpu_class, &vdev->dev, MKDEV(NV_UVM_MAJOR, 1), NULL,
                "nvidia-uvm-tools");

  /* Register /dev/nvidia-modeset (match host, major 195, minor 254) */
  dev->modeset_devno = MKDEV(NV_MAJOR, NV_MODESET_MINOR);
  ret = register_chrdev_region(dev->modeset_devno, 1, "nvidia-modeset");
  if (ret)
    goto err_uvm_cdev;

  cdev_init(&dev->cdev_modeset, &nvgpu_modeset_fops);
  dev->cdev_modeset.owner = THIS_MODULE;
  ret = cdev_add(&dev->cdev_modeset, dev->modeset_devno, 1);
  if (ret)
    goto err_modeset_region;

  device_create(nvgpu_class, &vdev->dev, dev->modeset_devno, NULL,
                "nvidia-modeset");
  dev_info(&vdev->dev,
           "virtio-gpu-nv: registered /dev/nvidia-modeset (%u:%u)\n",
           MAJOR(dev->modeset_devno), MINOR(dev->modeset_devno));

  nvgpu_caps_init(dev);

  /* Create /proc/driver/nvidia/version */
  ret = nvgpu_proc_init(dev);
  if (ret)
    goto err_proc;

  /*
   * Where device memory will appear. The VMM publishes it as a virtio shared
   * memory region on this device, which is the only way this side can learn
   * an address the bus assigned after the backend was started.
   */
  if (virtio_get_shm_region(vdev, &dev->window, NVGPU_SHM_ID)) {
    dev_info(&vdev->dev, "virtio-gpu-nv: window at %pa, %llu bytes\n",
             &dev->window.addr, dev->window.len);
    /* Only a v2 backend ever asks for write-back. */
    if (dev->v2 && dev->window.len >= PAGE_SIZE)
      nvgpu_region_check_wb(dev, &dev->window, "the window's write-back zone");
  } else {
    dev->window.len = 0;
    dev_warn(&vdev->dev,
             "virtio-gpu-nv: no shared memory region; device memory will not "
             "be mappable\n");
  }

  /* Fetch host sysfs content + DRI device list from the VMM */
  ret = nvgpu_fetch_sys_files(dev);
  if (ret)
    dev_warn(&vdev->dev, "virtio-gpu-nv: GET_SYS_FILES failed: %d\n", ret);

  /* Register fake PCI devices — creates /sys/bus/pci/devices/<addr>/ */
  ret = nvgpu_pci_init(dev);
  if (ret)
    dev_warn(&vdev->dev, "virtio-gpu-nv: PCI sysfs init failed: %d\n", ret);

  nvgpu_module_sysfs_init(dev);

  /* Create /dev/dri/renderD128 etc. with host major:minor */
  nvgpu_dri_init(dev); /* non-fatal */

  /*
   * /dev/nvgpu-wl, when the backend offers a compositor: after HELLO (caps,
   * limits) and the DRM devices (its device map reads their minors).
   * Non-fatal too: GPU work does not need it.
   */
  ret = nvgpu_wl_init(dev);
  if (ret)
    dev_warn(&vdev->dev, "virtio-gpu-nv: /dev/nvgpu-wl: %d\n", ret);

  dev_info(&vdev->dev, "virtio-gpu-nv: %u GPU(s), driver %s\n", dev->num_gpus,
           dev->driver_version);
  return 0;

  /*
   * Each label undoes what was set up before the step that jumped to it, in
   * reverse order. The old ladder unregistered a modeset region it had never
   * registered, and a /proc failure left the modeset, uvm and caps devices
   * behind.
   */
err_proc:
  remove_proc_subtree("driver/nvidia", NULL);
  nvgpu_caps_cleanup(dev);
  device_destroy(nvgpu_class, dev->modeset_devno);
  cdev_del(&dev->cdev_modeset);
err_modeset_region:
  unregister_chrdev_region(dev->modeset_devno, 1);
err_uvm_cdev:
  device_destroy(nvgpu_class, dev->uvm_devno);
  device_destroy(nvgpu_class, MKDEV(NV_UVM_MAJOR, 1));
  cdev_del(&dev->cdev_uvm);
err_uvm_region:
  unregister_chrdev_region(dev->uvm_devno, 2);
err_ctl_cdev:
  device_destroy(nvgpu_class, MKDEV(NV_MAJOR, NV_CTL_MINOR));
  cdev_del(&dev->cdev_ctl);
err_ctl_region:
  unregister_chrdev_region(MKDEV(NV_MAJOR, NV_CTL_MINOR), 1);
err_gpu_cdevs:
  for (i = i - 1; i >= 0; i--) {
    cdev_del(&dev->cdev_gpu[i]);
    device_destroy(nvgpu_class, MKDEV(NV_MAJOR, i));
  }
  unregister_chrdev_region(MKDEV(NV_MAJOR, 0), dev->num_gpus);
err_class:
  class_destroy(nvgpu_class);
  nvgpu_class = NULL;
err_ready:
  nvgpu_xfer_quiesce(dev);
err_xfer:
  vdev->config->reset(vdev);
  nvgpu_xfer_reclaim(dev);
err_vqs:
  vdev->config->del_vqs(vdev);
  nvgpu_xfer_destroy(dev);
  vdev->priv = NULL;
  nvgpu_dev_put(dev);
  return ret;
}

static void nvgpu_remove(struct virtio_device *vdev) {
  struct nvgpu_device *dev = vdev->priv;
  int i;

  /*
   * The transport comes down in three steps around the reset. Before it, the
   * device still answers, so the clock work is stopped and any queued CLOSE
   * goes out. The reset stops the callbacks. After it, nothing on the ring
   * will ever be answered: waiters are failed and every buffer comes back.
   */
  /* First, so no new channel is opened against a device going away. */
  nvgpu_wl_cleanup(dev);

  nvgpu_xfer_quiesce(dev);
  vdev->config->reset(vdev);
  nvgpu_xfer_reclaim(dev);
  /* Nothing answers now, and the backend ends the session (freeing every
   * client) when it sees the reset: the pages RM held are the guest's. */
  nvgpu_osdesc_release_all(dev);

  nvgpu_dri_cleanup(dev);
  nvgpu_module_sysfs_cleanup();
  nvgpu_pci_cleanup(dev);

  device_destroy(nvgpu_class, dev->modeset_devno);
  cdev_del(&dev->cdev_modeset);
  unregister_chrdev_region(dev->modeset_devno, 1);

  for (i = 0; i < (int)dev->num_gpus; i++) {
    device_destroy(nvgpu_class, MKDEV(NV_MAJOR, i));
    cdev_del(&dev->cdev_gpu[i]);
  }
  unregister_chrdev_region(MKDEV(NV_MAJOR, 0), dev->num_gpus);

  device_destroy(nvgpu_class, MKDEV(NV_MAJOR, NV_CTL_MINOR));
  cdev_del(&dev->cdev_ctl);
  unregister_chrdev_region(MKDEV(NV_MAJOR, NV_CTL_MINOR), 1);

  device_destroy(nvgpu_class, dev->uvm_devno);
  device_destroy(nvgpu_class, MKDEV(NV_UVM_MAJOR, 1));
  cdev_del(&dev->cdev_uvm);
  unregister_chrdev_region(dev->uvm_devno, 2);

  nvgpu_caps_cleanup(dev);

  if (nvgpu_class) {
    class_destroy(nvgpu_class);
    nvgpu_class = NULL;
  }

  vdev->config->del_vqs(vdev);
  nvgpu_xfer_destroy(dev);

  remove_proc_subtree("driver/nvidia", NULL);
  /* Freed with the last open file or object that names it, if not now. */
  vdev->priv = NULL;
  nvgpu_dev_put(dev);
}

/* ───────── Module boilerplate ───────── */

static struct virtio_device_id id_table[] = {
    {VIRTIO_ID_GPU_NV, VIRTIO_DEV_ANY_ID},
    {0},
};
MODULE_DEVICE_TABLE(virtio, id_table);

/*
 * The virtio device ID to bind.
 *
 * VIRTIO_ID_GPU_NV is 45, which is what libkrun assigns. QEMU cannot express
 * it: its virtio_device_names table stops at 41, and a higher id trips an
 * assertion in virtio_id_to_name() before the device is even realised. Making
 * this a parameter lets the same module be tested under QEMU without changing
 * the identity it uses in production.
 *
 *     insmod virtio_gpu_nv.ko virtio_id=41
 */
static unsigned int virtio_id = VIRTIO_ID_GPU_NV;
module_param(virtio_id, uint, 0444);
MODULE_PARM_DESC(virtio_id, "virtio device ID to bind (default 45)");

static unsigned int features[] = {
    VIRTIO_F_VERSION_1,
    VIRTIO_GPU_NV_F_UVM,
    VIRTIO_GPU_NV_F_ENCODE,
    VIRTIO_GPU_NV_F_GRAPHICS,
};

static struct virtio_driver nvgpu_driver = {
    .driver.name = "virtio-gpu-nv",
    .driver.owner = THIS_MODULE,
    .id_table = id_table,
    .feature_table = features,
    .feature_table_size = ARRAY_SIZE(features),
    .probe = nvgpu_probe,
    .remove = nvgpu_remove,
};

static int __init nvgpu_init(void)
{
    if (virtio_id != VIRTIO_ID_GPU_NV) {
        id_table[0].device = virtio_id;
        pr_info("virtio-gpu-nv: binding virtio device id %u (default %u)\n",
                virtio_id, (unsigned int)VIRTIO_ID_GPU_NV);
    }
    return register_virtio_driver(&nvgpu_driver);
}

static void __exit nvgpu_exit(void)
{
    unregister_virtio_driver(&nvgpu_driver);
}

module_init(nvgpu_init);
module_exit(nvgpu_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("libkrun-nv contributors");
MODULE_DESCRIPTION("virtio-gpu-nv: NVIDIA GPU sharing for VMs via ioctl proxy");
