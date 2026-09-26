// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-nvgpu guest driver: the NVIDIA character devices, DRM nodes and the
 * Wayland channel of a KVM guest, forwarded to the host's NVIDIA driver.
 *
 * Each guest open("/dev/nvidia*") creates a new host FD via the backend.
 * Ioctls are forwarded over the control virtqueue; mmap requests are placed
 * in the shared window the VMM maps into guest memory, so hot-path GPU
 * writes go direct through EPT/NPT -- no VMM involvement in the render loop.
 *
 * Built out of tree (driver/Makefile) or in a kernel tree under
 * drivers/virtio/ with the other nvgpu_* files (driver/Kconfig).
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

/* ───────── Driver state ───────── */

/* The shared memory region device memory is placed in, id 1. */
#define NVGPU_SHM_ID 1

/* class for device_create() */
static struct class *nvgpu_class;

static long nvgpu_ioctl(struct file *filp, unsigned int cmd, unsigned long arg);

/*
 * poll() on a device descriptor.
 *
 * Reports nothing until the host says a descriptor is readable (the event
 * queue sets `pending`), so NVIDIA's user-mode driver sleeps between frames
 * the way it does on bare metal. A file_operations with no .poll is reported
 * ready by the VFS every time, and the driver's wait never waited: it spun,
 * a whole core per guest.
 */
static __poll_t nvgpu_poll_mask(struct file *filp,
                                struct poll_table_struct *wait,
                                __poll_t ready) {
  struct nvgpu_fd *nfd = filp->private_data;

  if (!nfd)
    return EPOLLERR;

  poll_wait(filp, &nfd->wq, wait);

  /*
   * Taken, not read: a caller that polls without consuming would otherwise
   * find it ready every time, which is the spin this path exists to end. One
   * report per event.
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
bool nvgpu_proc_ids(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_PROC_ID);
}

/*
 * Whether the process says its euid too, and every RM_CONTROL carries it: a
 * control may name a second client, which the backend holds to RM's rule for
 * it -- the same process, or the same euid where RM's rule is its security
 * token (device/src/rmshare.rs).
 */
bool nvgpu_proc_euid(const struct nvgpu_device *dev) {
  return nvgpu_proc_ids(dev) && (dev->backend_caps & NVGPU_BCAP_PROC_EUID);
}

/*
 * Whether this guest has a UVM device at all. A v2 backend serves UVM only
 * with --allow-compute (NVGPU_BCAP_COMPUTE) and refuses every open of it
 * otherwise; then no /dev/nvidia-uvm is made and "nvidia-uvm" is not in
 * /proc/devices, which NVIDIA's userspace reads as a host whose nvidia-uvm
 * is not loaded (and nvidia-modprobe finds no major to make a node with).
 * A v1 backend knows nothing of the flag and keeps what it had.
 */
static bool nvgpu_uvm_offered(const struct nvgpu_device *dev) {
  return !dev->v2 || (dev->backend_caps & NVGPU_BCAP_COMPUTE);
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
void nvgpu_proc_id_fill(const struct nvgpu_device *dev, void *dst) {
  nvgpu_proc_id_fill_task(dev, current, dst);
}

/* The same for task `t`, which the caller holds a reference on. */
void nvgpu_proc_id_fill_task(const struct nvgpu_device *dev,
                             struct task_struct *t, void *dst) {
  struct nvgpu_proc_id id = {};
  struct task_struct *leader;

  rcu_read_lock();
  leader = READ_ONCE(t->group_leader);
  id.start_ns = cpu_to_le64(leader->start_time);
  rcu_read_unlock();
  id.tgid = cpu_to_le32(task_tgid_nr(t));
  /*
   * The effective uid, as RM's security token holds it for a host process
   * (os_get_euid: current->cred->euid, in the initial user namespace). Not
   * the fsuid: RM never reads it.
   */
  if (nvgpu_proc_euid(dev))
    id.euid = cpu_to_le32(__kuid_val(task_euid(t)));
  memcpy(dst, &id, sizeof(id));
}

/*
 * An OPEN's length, with the opener after the request when the backend
 * charges what a process opens to it (device/src/quota.rs).
 */
u32 nvgpu_open_req_fill_proc(const struct nvgpu_device *dev,
                             struct nvgpu_open_req_proc *r) {
  if (!nvgpu_proc_ids(dev))
    return sizeof(r->req);
  nvgpu_proc_id_fill(dev, &r->proc);
  return sizeof(*r);
}

/*
 * nvgpu_handle_for_fd — the backend's handle for another of our open files.
 *
 * A guest descriptor means nothing on the other side. Anything that names an
 * open file has to name it by the handle the backend issued when we opened it.
 */
int nvgpu_handle_for_fd(struct nvgpu_device *dev, int guest_fd, u32 *handle) {
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
  if (other && other->dev == dev)
    *handle = other->handle;
  else if (other)
    ret = -EBADF; /* another device's: its backend's number */
  else
    ret = nvgpu_hostfile_handle(dev, f, handle);
  fput(f);
  return ret;
}

/* ───────── UVM ioctl ───────── */

static long nvgpu_ioctl(struct file *filp, unsigned int cmd,
                        unsigned long arg) {
  return nvgpu_ioctl_fd(filp->private_data, cmd, arg);
}

static long nvgpu_uvm_ioctl(struct file *filp, unsigned int cmd,
                            unsigned long arg) {
  return nvgpu_uvm_ioctl_fd(filp->private_data, cmd, arg);
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
  /*
   * No more of the window than the placement holds. What follows it is the
   * next extent -- another process's device memory -- or unplaced window,
   * whose first touch stops the VM; a backend that answered a larger vma
   * with a smaller placement would hand this process either.
   */
  if (size > le64_to_cpu(resp->size)) {
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: a %llu-byte mapping of a %llu-byte "
                         "placement refused\n",
                         size, le64_to_cpu(resp->size));
    ret = -EINVAL;
    goto out;
  }

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

void nvgpu_dev_get(struct nvgpu_device *dev) { kobject_get(&dev->kobj); }

/*
 * The last reference: every open file, mapping, fence and character device
 * inode is done with the device. Only memory goes here -- the transport's
 * state, and the virtio device, kept for the lines logged against it.
 */
static void nvgpu_dev_release(struct kobject *kobj) {
  struct nvgpu_device *dev = container_of(kobj, struct nvgpu_device, kobj);

  nvgpu_xfer_free(dev);
  put_device(&dev->vdev->dev);
  kfree(dev);
}

static const struct kobj_type nvgpu_dev_ktype = {
    .release = nvgpu_dev_release,
};

void nvgpu_dev_put(struct nvgpu_device *dev) { kobject_put(&dev->kobj); }

/*
 * A character device embedded in the device: its kobject holds the device
 * from cdev_add() until the last inode's cdev_put(), which comes after the
 * file's release and so after its own device reference is gone.
 */
static int nvgpu_cdev_add(struct nvgpu_device *dev, struct cdev *c,
                          const struct file_operations *fops, dev_t devno,
                          unsigned int count) {
  cdev_init(c, fops);
  c->owner = THIS_MODULE;
  cdev_set_parent(c, &dev->kobj);
  return cdev_add(c, devno, count);
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
  struct nvgpu_open_req_proc *reqp;
  struct nvgpu_open_req *req;
  struct nvgpu_open_resp *resp;
  u32 req_len;
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
  reqp = kzalloc(sizeof(*reqp), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !reqp || !resp) {
    kfree(nfd);
    kfree(reqp);
    kfree(resp);
    return -ENOMEM;
  }
  req = &reqp->req;

  nfd->dev = dev;
  nfd->device_type = device_type;
  refcount_set(&nfd->ref, 1);

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->device_type = cpu_to_le32(device_type);
  req->flags = cpu_to_le32(filp->f_flags);
  req_len = nvgpu_open_req_fill_proc(dev, reqp);

  ret = nvgpu_send_recv(dev, reqp, req_len, resp, sizeof(*resp));
  if (ret < 0 || (s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    kfree(nfd);
    kfree(reqp);
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
  kfree(reqp);
  kfree(resp);
  return 0;
}

static int nvgpu_gpu_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, (u32)iminor(inode));
}

static int nvgpu_ctl_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp, NVGPU_DEV_CTL);
}

/*
 * One character device for both UVM nodes, told apart by minor, as
 * nvidia-uvm does: 0 is /dev/nvidia-uvm, 1 /dev/nvidia-uvm-tools, which the
 * backend opens as the host's own tools node (profilers, uvm_tools.c).
 */
static int nvgpu_uvm_open(struct inode *inode, struct file *filp) {
  return nvgpu_open_common(inode, filp,
                           iminor(inode) == 1 ? NVGPU_DEV_UVM_TOOLS
                                              : NVGPU_DEV_UVM);
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
  else if (ret >= 0)
    /*
     * A success that did not carry the struct back: `kbuf` still holds what
     * was sent, which a caller reading an answer out of it (ALLOC_NVKMS's
     * handle, MAP_OFFSET's offset) would take for the host's -- a proxy for
     * a handle number its caller chose.
     */
    ret = -EIO;

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

static long nvgpu_modeset_ioctl(struct file *filp, unsigned int cmd,
                                unsigned long arg) {
  struct nvgpu_fd *nfd = filp->private_data;
  unsigned int ioc_type = _IOC_TYPE(cmd);
  void __user *uarg = (void __user *)arg;

  /* nvidia-modeset ioctls use type 0x6d ('m'); a v2 backend takes them
   * through the schema (nvgpu_nvkms.c), a v1 one as a flat block. */
  if (ioc_type == 0x6d) {
    if (nfd->dev->v2)
      return nvgpu_nvkms_ioctl(nfd, cmd, uarg);
    return nvgpu_ioctl_modeset(nfd, cmd, uarg);
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

    /* Each length against what is left, never their sum past the end. */
    if (path_len > end - p || content_len > end - p - path_len) {
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

/*
 * Whether the bus the GPU's host address names is already in this guest. The
 * device can only be registered where the host has it (userspace finds the
 * GPU by that address), and a bus the VMM already populated cannot be made
 * again: pci_scan_root_bus_bridge() refuses it with -EEXIST, saying why only
 * at dev_dbg. The usual cause is a VMM bridge whose secondary bus is the GPU's:
 * crosvm's hot-plug root port sits on 00:xx and has bus 1 behind it, and many
 * hosts have their GPU at 0000:01:00.0. Asked first so the log names both.
 */
static bool nvgpu_pci_bus_taken(struct nvgpu_device *dev,
                                const struct nvgpu_pci_root *root) {
  struct pci_bus *b = pci_find_bus(root->slot.domain, root->slot.bus_nr);

  if (!b)
    return false;
  if (b->self)
    dev_err(&dev->vdev->dev,
            "virtio-gpu-nv: cannot put the GPU at its host address %s: bus "
            "%04x:%02x already exists in this guest, behind the VMM's bridge "
            "%s [%04x:%04x]. The VMM has a device at the host GPU's address; "
            "start it without that bridge (crosvm: --no-pci-hotplug-port)\n",
            root->slot.pci_addr, root->slot.domain, root->slot.bus_nr,
            pci_name(b->self), b->self->vendor, b->self->device);
  else
    dev_err(&dev->vdev->dev,
            "virtio-gpu-nv: cannot put the GPU at its host address %s: bus "
            "%04x:%02x is already one of this guest's root buses. The VMM "
            "has devices at the host GPU's address\n",
            root->slot.pci_addr, root->slot.domain, root->slot.bus_nr);
  return true;
}

static int nvgpu_pci_init(struct nvgpu_device *dev) {
  int i, ret, err = 0;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];
    struct pci_host_bridge *bridge;

    if (!root->slot.config_valid) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: no config space for %s, skipping\n",
               root->slot.pci_addr);
      continue;
    }

    if (nvgpu_pci_bus_taken(dev, root)) {
      err = err ?: -EEXIST;
      continue;
    }

    bridge = pci_alloc_host_bridge(0);
    if (!bridge) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_alloc_host_bridge failed for %s\n",
              root->slot.pci_addr);
      err = err ?: -ENOMEM;
      continue;
    }

    /* One bus resource covering exactly our bus number */
    root->bus_res = (struct resource){
        .start = root->slot.bus_nr,
        .end = root->slot.bus_nr,
        .flags = IORESOURCE_BUS,
    };
    pci_add_resource(&bridge->windows, &root->bus_res);

    bridge->dev.parent = &dev->vdev->dev;
    /* x86 reads the domain from the sysdata (pci_domain_nr()), not from
     * bridge->domain_nr, which stays PCI_DOMAIN_NR_NOT_SET: set, the bridge's
     * release takes it for a number this driver allocated from the PCI
     * core's emulated-domain IDA and frees it there, and ida_free() WARNs on
     * a number it never handed out. That was the WARN on every failed scan,
     * and the reason the bridge was never freed after a successful one. */
    root->domain = (int)root->slot.domain;
    /* No node to claim: the GPU is the host's, and the guest's idea of
     * distance to it means nothing. NUMA_NO_NODE lets every allocation made
     * against this device fall back to the caller's node. */
    root->node = NUMA_NO_NODE;
    bridge->sysdata = root;
    bridge->ops = &nvgpu_pci_ops;
    bridge->busnr = root->slot.bus_nr;
    root->nvdev = dev;

    ret = pci_scan_root_bus_bridge(bridge);
    if (ret) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_scan_root_bus_bridge %s: %d%s\n",
              root->slot.pci_addr, ret,
              ret == -EEXIST ? " (bus already present in this guest)" : "");
      pci_free_host_bridge(bridge);
      err = err ?: ret;
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

    dev_dbg(&dev->vdev->dev, "virtio-gpu-nv: registered fake PCI device %s\n",
            root->slot.pci_addr);
  }

  return err;
}

static void nvgpu_pci_cleanup(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];

    if (!root->registered)
      continue;

    /* Removing the root bus deletes the bridge's device and drops the bus's
     * reference to it; the one pci_alloc_host_bridge() gave is ours. */
    pci_remove_root_bus(root->bridge->bus);
    pci_free_host_bridge(root->bridge);
    root->bridge = NULL;
    root->pdev = NULL;
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

    dev_dbg(&dev->vdev->dev,
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

    if (path_len > end - p || content_len > end - p - path_len) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: sys stream truncated at sysfs section\n");
      break;
    }

    /* Safe path extraction — explicit memset, no {} initialiser */
    memset(path, 0, sizeof(path));
    copy_len = min(path_len, (u32)(sizeof(path) - 1));
    memcpy(path, p, copy_len);
    p += path_len;

    if (content_len > end - p) {
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

          /* The slots config space had: num_gpus may say up to 248. */
          for (gi = 0; gi < (int)min_t(u32, dev->num_gpus,
                                       ARRAY_SIZE(dev->gpu_slots));
               gi++) {
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

      dev_dbg(&dev->vdev->dev,
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

/*
 * /dev/nvidia-caps/nvidia-cap*: files to be opened and handed to RM as proof
 * of a capability (MIG config and monitor), as nvidia.ko's nv-caps.c makes
 * them -- open and release and nothing else, so an ioctl on one is -ENOTTY
 * natively and here. (This answered 0 to every ioctl number.)
 */
static int nvgpu_caps_open(struct inode *inode, struct file *filp) {
  filp->private_data = NULL;
  return 0;
}

static int nvgpu_caps_release(struct inode *inode, struct file *filp) {
  return 0;
}

static const struct file_operations nvgpu_caps_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_caps_open,
    .release = nvgpu_caps_release,
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

  ret = nvgpu_cdev_add(dev, &dev->cdev_caps, &nvgpu_caps_fops, dev->caps_devno,
                       2);
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
  kobject_init(&dev->kobj, &nvgpu_dev_ktype);

  /* For the lines logged against it by whatever outlives remove(). */
  get_device(&vdev->dev);
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

      dev_dbg(&vdev->dev,
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

  dev_dbg(&vdev->dev, "virtio-gpu-nv: %u fd-translation ioctl(s) registered\n",
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
    ret = nvgpu_cdev_add(dev, &dev->cdev_gpu[i], &nvgpu_gpu_fops,
                         MKDEV(NV_MAJOR, i), 1);
    if (ret)
      goto err_gpu_cdevs;
    device_create(nvgpu_class, &vdev->dev, MKDEV(NV_MAJOR, i), NULL, "nvidia%d",
                  i);
  }

  /* Register /dev/nvidiactl */
  ret = register_chrdev_region(MKDEV(NV_MAJOR, NV_CTL_MINOR), 1, "nvidiactl");
  if (ret)
    goto err_gpu_cdevs;

  ret = nvgpu_cdev_add(dev, &dev->cdev_ctl, &nvgpu_ctl_fops,
                       MKDEV(NV_MAJOR, NV_CTL_MINOR), 1);
  if (ret)
    goto err_ctl_region;

  device_create(nvgpu_class, &vdev->dev, MKDEV(NV_MAJOR, NV_CTL_MINOR), NULL,
                "nvidiactl");

  /*
   * Register /dev/nvidia-uvm (major should match host), when the backend
   * serves it (nvgpu_uvm_offered).
   */
  dev->uvm_devno = MKDEV(NV_UVM_MAJOR, 0);
  if (nvgpu_uvm_offered(dev)) {
    ret = register_chrdev_region(dev->uvm_devno, 2, "nvidia-uvm");
    if (ret)
      goto err_ctl_cdev;

    ret = nvgpu_cdev_add(dev, &dev->cdev_uvm, &nvgpu_uvm_fops, dev->uvm_devno,
                         2);
    if (ret) {
      unregister_chrdev_region(dev->uvm_devno, 2);
      goto err_ctl_cdev;
    }

    device_create(nvgpu_class, &vdev->dev, dev->uvm_devno, NULL, "nvidia-uvm");
    device_create(nvgpu_class, &vdev->dev, MKDEV(NV_UVM_MAJOR, 1), NULL,
                  "nvidia-uvm-tools");
    dev->uvm_registered = true;
  } else {
    dev_info(&vdev->dev,
             "virtio-gpu-nv: the backend serves no compute (no "
             "--allow-compute); no /dev/nvidia-uvm\n");
  }

  /* Register /dev/nvidia-modeset (match host, major 195, minor 254) */
  dev->modeset_devno = MKDEV(NV_MAJOR, NV_MODESET_MINOR);
  ret = register_chrdev_region(dev->modeset_devno, 1, "nvidia-modeset");
  if (ret)
    goto err_uvm_cdev;

  ret = nvgpu_cdev_add(dev, &dev->cdev_modeset, &nvgpu_modeset_fops,
                       dev->modeset_devno, 1);
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

  /* /dev/nvgpu-capture, when the backend has a capture helper's socket:
   * like the Wayland node, after the DRM devices, and not fatal. */
  ret = nvgpu_capture_init(dev);
  if (ret)
    dev_warn(&vdev->dev, "virtio-gpu-nv: /dev/nvgpu-capture: %d\n", ret);

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
  if (dev->uvm_registered) {
    device_destroy(nvgpu_class, dev->uvm_devno);
    device_destroy(nvgpu_class, MKDEV(NV_UVM_MAJOR, 1));
    cdev_del(&dev->cdev_uvm);
    unregister_chrdev_region(dev->uvm_devno, 2);
    dev->uvm_registered = false;
  }
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
  nvgpu_capture_cleanup(dev);

  nvgpu_xfer_quiesce(dev);
  vdev->config->reset(vdev);
  nvgpu_xfer_reclaim(dev);
  /* Nothing answers now, and the backend ends the session (freeing every
   * client) when it sees the reset: the pages RM held are the guest's. */
  nvgpu_osdesc_release_all(dev);
  /* Fence consumers buried so far; later ones go with the next reap. */
  nvgpu_fence_drain();

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

  if (dev->uvm_registered) {
    device_destroy(nvgpu_class, dev->uvm_devno);
    device_destroy(nvgpu_class, MKDEV(NV_UVM_MAJOR, 1));
    cdev_del(&dev->cdev_uvm);
    unregister_chrdev_region(dev->uvm_devno, 2);
    dev->uvm_registered = false;
  }

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
 * VIRTIO_ID_GPU_NV is 45, the number libkrun assigned it and the VMMs here
 * (nesbox, crosvm) present. QEMU cannot express it: its virtio_device_names
 * table stops at 41, and a higher id trips an assertion in
 * virtio_id_to_name() before the device is even realised. Making this a
 * parameter lets the same module be tested under QEMU without changing the
 * identity it uses in production.
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
    nvgpu_fence_drain();
}

module_init(nvgpu_init);
module_exit(nvgpu_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("Jacob Root");
MODULE_DESCRIPTION("virtio-gpu-nv: NVIDIA GPU sharing for VMs via ioctl proxy");
/* Which implementation parses what guest processes hand the module
 * (driver/Makefile, NVGPU_RUST; modinfo -F parsers). */
#ifdef NVGPU_RUST
MODULE_INFO(parsers, "rust");
#else
MODULE_INFO(parsers, "c");
#endif
