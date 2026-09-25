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

#include "gen/nvgpu_rmalloc_classes.h"
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

/* nvgpu_ioctl_simple — flat struct, no embedded pointers */
static long nvgpu_ioctl_simple(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg, unsigned int sz) {
  int req_total = sizeof(struct nvgpu_ioctl_req) + sz;
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

  /* Second-level pointer carried alongside the nested block. */
  u64 deep_user_ptr = 0;
  u32 deep_ptr_offset = 0;
  u32 deep_len = 0;

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
    if (copy_from_user(req_buf + sizeof(*req) + sizeof(params), user_nested,
                       nested_size)) {
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
  }

  if (deep_len > 0) {
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

    if (copy_to_user(user_nested, resp_buf + sizeof(*resp) + sizeof(params),
                     copy_back))
      ret = -EFAULT;
  }

  if (deep_len > 0 && le32_to_cpu(resp->deep_len) > 0) {
    u32 copy_back = min(deep_len, le32_to_cpu(resp->deep_len));
    size_t at = sizeof(*resp) + sizeof(params) +
                (size_t)le32_to_cpu(resp->nested_len);

    if (nvgpu_resp_has(used, at, copy_back) &&
        copy_to_user((void __user *)deep_user_ptr, resp_buf + at, copy_back))
      ret = -EFAULT;
  }

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
  u32 used;

  if (sz < sizeof(params))
    return -EINVAL;

  if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;

  user_alloc = (void __user *)(unsigned long)le64_to_cpu(params.pAllocParms);
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

  req_total = sizeof(*req) + sizeof(params) + nested_size;
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
        int event_fd;

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
 * offset of its argument. The backend puts its own descriptor there for the
 * call and our value back before it answers (nvidia.rs:2169-2171), so the
 * caller reads back what it passed.
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

  fdt = nvgpu_find_fd_translation(nfd->dev, cmd);
  if (fdt)
    return nvgpu_ioctl_translate_fd(nfd, cmd, uarg, sz,
                                    le32_to_cpu(fdt->payload_offset));

  switch (nr) {
  case NV_ESC_RM_CONTROL:
    return nvgpu_ioctl_rm_control(nfd, cmd, uarg, sz);
  case NV_ESC_RM_ALLOC:
    return nvgpu_ioctl_rm_alloc(nfd, cmd, uarg, sz);
  default:
    return nvgpu_ioctl_simple(nfd, cmd, uarg, sz);
  }
}

/* ───────── UVM ioctl ───────── */

static long nvgpu_ioctl(struct file *filp, unsigned int cmd,
                        unsigned long arg) {
  return nvgpu_ioctl_fd(filp->private_data, cmd, arg);
}

static long nvgpu_uvm_ioctl(struct file *filp, unsigned int cmd,
                            unsigned long arg) {
  struct nvgpu_fd *nfd = filp->private_data;
  unsigned int nr = _IOC_NR(cmd);
  unsigned int sz = _IOC_SIZE(cmd);
  void __user *uarg = (void __user *)arg;

  /*
   * UVM ioctls use _IOC(0, 0, nr, 0x3000) — type=0, size=0x3000.
   * _IOC_SIZE() returns 0x3000 which is the max buffer, not the
   * actual struct size. Use the real struct sizes instead.
   *
   * UVM_INITIALIZE     (nr=1): flags:u64 + rmStatus:u32 + pad = 16 bytes
   * UVM_MM_INITIALIZE  (nr=2): uvmFd:s32 + rmStatus:u32        =  8 bytes
   *
   * For all other UVM ioctls we use 0x3000 as an upper bound since
   * we don't know their sizes — the host driver will only read what
   * it needs.
   */
  if (sz == 0 || sz == 0x3000) {
    switch (nr) {
    case 1:
      sz = 16;
      break; /* UVM_INITIALIZE        */
    case 2:
      sz = 8;
      break; /* UVM_MM_INITIALIZE     */
    default:
      sz = 0x3000;
      break;
    }
  }

  if (sz > 0x3000)
    return -EINVAL;

  /*
   * UVM_MM_INITIALIZE passes arg=0 (NULL) because the uvmFd is
   * embedded in the ioctl struct on some driver versions, or the
   * kernel side doesn't need userspace params at all.
   * Forward with a zeroed buffer — host will fill rmStatus.
   */
  if (arg == 0) {
    /*
     * Can't copy_from_user a NULL pointer. Build a zeroed buffer
     * and send it; the host UVM driver will populate rmStatus.
     */
    int req_total = sizeof(struct nvgpu_ioctl_req) + sz;
    int resp_max = sizeof(struct nvgpu_ioctl_resp) + sz;
    void *req_buf, *resp_buf;
    struct nvgpu_ioctl_req *req;
    struct nvgpu_ioctl_resp *resp;
    int ret;

    req_buf = kzalloc(req_total, GFP_KERNEL);
    resp_buf = kzalloc(resp_max, GFP_KERNEL);
    if (!req_buf || !resp_buf) {
      kfree(req_buf);
      kfree(resp_buf);
      return -ENOMEM;
    }

    req = (struct nvgpu_ioctl_req *)req_buf;
    req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
    req->hdr.handle = cpu_to_le32(nfd->handle);
    req->cmd = cpu_to_le32(cmd);
    req->data_len = cpu_to_le32(sz);
    /* payload stays zeroed — no copy_from_user */

    ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);

    if (ret == 0) {
      resp = (struct nvgpu_ioctl_resp *)resp_buf;
      ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);
      /* arg=0 means no copy_to_user either */
    }

    kfree(req_buf);
    kfree(resp_buf);
    return ret;
  }

  /* UVM_INITIALIZE: inject MULTI_PROCESS_SHARING_MODE flag */
  if (nr == 1) {
    u64 flags;
    if (copy_from_user(&flags, uarg, sizeof(flags)))
      return -EFAULT;
    flags |= (1ULL << 2);
    if (copy_to_user(uarg, &flags, sizeof(flags)))
      return -EFAULT;
  }

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

static int nvgpu_mmap(struct file *filp, struct vm_area_struct *vma) {
  struct nvgpu_fd *nfd = filp->private_data;
  u64 size = vma->vm_end - vma->vm_start;
  u64 offset = (u64)vma->vm_pgoff << PAGE_SHIFT;
  u64 window_off;
  struct nvgpu_mmap_req *req;
  struct nvgpu_mmap_resp *resp;
  /* Allocated before asking, so that nothing can fail between the backend
   * placing the memory and this side owning the placement. */
  struct nvgpu_vma_map *m;
  u32 used, mapping_id = 0;
  int ret;

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

  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = pgprot_writecombine(vma->vm_page_prot);

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

  window_off = le64_to_cpu(resp->guest_phys_addr);
  if (window_off + size > nfd->dev->window.len) {
    dev_warn(&nfd->dev->vdev->dev,
             "virtio-gpu-nv: mapping at %llu+%llu runs past the %llu-byte "
             "window\n",
             window_off, size, nfd->dev->window.len);
    ret = -ERANGE;
    goto out;
  }

  ret = remap_pfn_range(vma, vma->vm_start,
                        (nfd->dev->window.addr + window_off) >> PAGE_SHIFT,
                        size, vma->vm_page_prot);
  if (ret)
    goto out;

  kref_init(&m->ref);
  m->dev = nfd->dev;
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

/*
 * The last reference: the host file goes too. Always from process context --
 * a file's release, a DRM postclose, or a GEM proxy's free, which the core
 * runs from the last handle close or dma-buf release.
 */
void nvgpu_fd_put(struct nvgpu_fd *nfd) {
  if (!refcount_dec_and_test(&nfd->ref))
    return;
  nvgpu_close_handle(nfd->dev, nfd->handle);
  kfree(nfd);
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

    parent = nvgpu_proc_mkdir_cached(built, NULL);

    if (next) {
      *next = '/';
      p = next + 1;
    } else {
      break;
    }
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

  dev = devm_kzalloc(&vdev->dev, sizeof(*dev), GFP_KERNEL);
  if (!dev)
    return -ENOMEM;

  dev->vdev = vdev;
  vdev->priv = dev;
  INIT_LIST_HEAD(&dev->fds);
  spin_lock_init(&dev->fds_lock);

  /* Find virtqueues */
  ret = virtio_find_vqs(vdev, 2, vqs, vqs_info, NULL);
  if (ret)
    return ret;

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
   * Which protocol, before anything else is said: the answer sizes every
   * request after it, and the DRM devices registered below advertise
   * features (syncobjs) only a v2 backend with the right caps can serve.
   */
  nvgpu_xfer_hello(dev);

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
