// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: /dev/nvgpu-capture, a host capture buffer opened as a guest
 * dma-buf (ARCHITECTURE.md, "Capture injection"; UAPI in
 * uapi/nvgpu_capture.h).
 *
 * The host's capture helper injects buffers into the backend; the backend
 * keeps each under an id and a random token, and a guest process that knows
 * both asks here. Two ioctls, fixed-size, and nothing parsed but their own
 * structs. OPEN: HOST_OP INJECT_OPEN imports the host object into the
 * caller's render file on the host, and the GEM handle it answers with
 * becomes a proxy of that file and a dma-buf, exactly as a host client's
 * buffer does in export mode (nvgpu_wl.c, nvgpu_wl_import()). OPEN_SYNCOBJ:
 * HOST_OP INJECT_OPEN_SYNCOBJ imports an injected syncobj into the same
 * file, whose handle numbers are the host's. The backend checks the token,
 * the object and the caller's share of opens; what this adds is who may ask
 * at all (the node's mode, below) and a dma-buf opened read-only.
 *
 * The node follows /dev/nvgpu-wl: root:root 0660 by default (capture_mode),
 * a group for the guest's capture daemon from udev
 * (contrib/udev/70-nvgpu-capture.rules), never "other". The token is what
 * keeps one stream's buffers from another guest process that may open the
 * node; the mode is what keeps processes that have no business with any
 * stream from trying.
 */

#include <linux/dma-buf.h>
#include <linux/fdtable.h>
#include <linux/file.h>
#include <linux/kref.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include "nvgpu.h"
#include "uapi/nvgpu_capture.h"

#define NVGPU_CAPTURE_MAX_DEVS 8
/* The largest buffer a proxy is made for: as nvgpu_wl.c's imports. */
#define NVGPU_CAPTURE_MAX_SIZE (1ull << 36)

static ushort nvgpu_capture_mode = 0660;

static int nvgpu_capture_mode_set(const char *val,
                                  const struct kernel_param *kp) {
  u16 mode;
  int ret = kstrtou16(val, 0, &mode);

  if (ret)
    return ret;
  /* Nothing for "other": every guest process could try tokens. */
  if (mode & ~0770)
    return -EINVAL;
  *(ushort *)kp->arg = mode;
  return 0;
}

static const struct kernel_param_ops nvgpu_capture_mode_ops = {
    .set = nvgpu_capture_mode_set,
    .get = param_get_ushort,
};
module_param_cb(capture_mode, &nvgpu_capture_mode_ops, &nvgpu_capture_mode,
                0444);
MODULE_PARM_DESC(capture_mode, "permissions of /dev/nvgpu-capture* (default "
                               "0660, within 0770; the group comes from udev)");

/*
 * One node per virtio device, refcounted as nvgpu_wl.c's: files opened
 * before remove() still name it, and it holds the nvgpu_device they use.
 */
struct nvgpu_capture_dev {
  struct miscdevice misc;
  struct nvgpu_device *dev;
  struct kref ref;
  char name[20];
};

static struct nvgpu_capture_dev *nvgpu_capture_devs[NVGPU_CAPTURE_MAX_DEVS];
static DEFINE_MUTEX(nvgpu_capture_devs_lock);

static void nvgpu_capture_dev_free(struct kref *ref) {
  struct nvgpu_capture_dev *cd =
      container_of(ref, struct nvgpu_capture_dev, ref);

  nvgpu_dev_put(cd->dev);
  kfree(cd);
}

/*
 * INJECT_OPEN on @nfd's render handle, then a proxy and a read-only dma-buf
 * in @rf. Fills @a's outputs; returns the dma-buf (not yet a descriptor) or
 * an ERR_PTR.
 */
static struct dma_buf *nvgpu_capture_do_open(struct nvgpu_device *dev,
                                             struct file *rf,
                                             struct nvgpu_fd *nfd,
                                             struct nvgpu_capture_open *a) {
  struct nvgpu_inject_info info;
  struct dma_buf *buf;
  u64 args[4], res[3];
  u32 gem, tail, i;
  int ret;

  args[0] = nfd->handle;
  args[1] = a->id;
  args[2] = get_unaligned_le64(&a->token[0]);
  args[3] = get_unaligned_le64(&a->token[8]);
again:
  memset(&info, 0, sizeof(info));
  ret = nvgpu_host_op_tail(dev, NVGPU_OP_INJECT_OPEN, args, 4, res, 3, &info,
                           sizeof(info), &tail);
  if (ret < 0)
    return ERR_PTR(ret);
  /* A GEM handle is a non-zero u32 (none can be closed that is not). */
  if (!nvgpu_res_u32(res[0], &gem))
    return ERR_PTR(-EPROTO);
  /*
   * The rest checked before anything is made of it: a size a proxy can
   * stand for, NVKMS memory (nothing else is injected), and a whole
   * description with planes it can name.
   */
  if (!res[1] || res[1] > NVGPU_CAPTURE_MAX_SIZE ||
      res[2] != NVGPU_GEM_OBJECT_NVKMS || tail != sizeof(info) ||
      !le32_to_cpu(info.nplanes) ||
      le32_to_cpu(info.nplanes) > NVGPU_CAPTURE_MAX_PLANES) {
    nvgpu_gem_close_unheld(nfd, gem);
    return ERR_PTR(-EPROTO);
  }
  /*
   * Owns the host GEM handle from here: closed on failure, or left to the
   * proxy that already stands for it. Without O_RDWR the dma-buf's file is
   * read-only, so no process can map it writable (the backend's read-only
   * placement refuses what does get through, nvgpu_drm.c).
   */
  buf = nvgpu_dmabuf_from_host_buf(rf, gem, res[1], NVGPU_GEM_OBJECT_NVKMS,
                                   O_CLOEXEC);
  /* A proxy of the same handle on its way out: wait it out and import
   * again (as nvgpu_wl_import()). */
  if (buf == ERR_PTR(-EAGAIN)) {
    ret = nvgpu_gem_wait_gone(nfd, gem);
    if (!ret)
      goto again;
    return ERR_PTR(ret);
  }
  if (IS_ERR(buf))
    return buf;

  a->width = le32_to_cpu(info.width);
  a->height = le32_to_cpu(info.height);
  a->fourcc = le32_to_cpu(info.fourcc);
  a->nplanes = le32_to_cpu(info.nplanes);
  a->modifier = le64_to_cpu(info.modifier);
  for (i = 0; i < NVGPU_CAPTURE_MAX_PLANES; i++) {
    a->offsets[i] = le32_to_cpu(info.offsets[i]);
    a->strides[i] = le32_to_cpu(info.strides[i]);
  }
  a->buf_flags = le32_to_cpu(info.flags) & NVGPU_CAPTURE_F_Y_INVERT;
  a->pad = 0;
  a->size = PAGE_ALIGN(res[1]);
  return buf;
}

static long nvgpu_capture_open_ioctl(struct nvgpu_capture_dev *cd,
                                     void __user *uarg) {
  struct nvgpu_device *dev = cd->dev;
  struct nvgpu_capture_open a;
  struct nvgpu_fd *nfd;
  struct dma_buf *buf;
  struct file *rf;
  int fd;

  if (copy_from_user(&a, uarg, sizeof(a)))
    return -EFAULT;
  if (a.flags)
    return -EINVAL;
  /*
   * One lookup of the descriptor for the whole call, as nvgpu_wl_import():
   * the file whose render handle the host imports into is the file the
   * proxy is made in. It must be a DRM file of ours, on this device.
   */
  rf = fget(a.render_fd);
  nfd = rf ? nvgpu_drm_file_nfd(rf) : NULL;
  if (!nfd || nfd->dev != dev) {
    if (rf)
      fput(rf);
    return -EBADF;
  }
  /*
   * The number first, installed last: until fd_install() nothing else in
   * the process can see or close it, so a failed copy-out takes back only
   * what this call made (a close_fd() after installing could close a file
   * another thread had put there meanwhile).
   */
  fd = get_unused_fd_flags(O_CLOEXEC);
  if (fd < 0) {
    fput(rf);
    return fd;
  }
  buf = nvgpu_capture_do_open(dev, rf, nfd, &a);
  fput(rf);
  if (IS_ERR(buf)) {
    put_unused_fd(fd);
    dev_dbg_ratelimited(&dev->vdev->dev,
                        "virtio-gpu-nv: capture: id %u not opened: %ld\n",
                        a.id, PTR_ERR(buf));
    return PTR_ERR(buf);
  }
  a.dmabuf_fd = fd;
  if (copy_to_user(uarg, &a, sizeof(a))) {
    put_unused_fd(fd);
    dma_buf_put(buf);
    return -EFAULT;
  }
  fd_install(fd, buf->file);
  return 0;
}

/*
 * OPEN_SYNCOBJ: INJECT_OPEN_SYNCOBJ on the render file's handle. The handle
 * the host answers with is the guest file's own number (nvgpu_fence.c: a
 * host syncobj handle is the same number in the guest), so there is nothing
 * to make here: the caller uses it with the ordinary syncobj ioctls. One
 * whose reply comes after the caller gave up stays in the caller's file
 * until it closes, as a SYNCOBJ_FD_TO_HANDLE's would.
 */
static long nvgpu_capture_open_syncobj_ioctl(struct nvgpu_capture_dev *cd,
                                             void __user *uarg) {
  struct nvgpu_device *dev = cd->dev;
  struct nvgpu_capture_open_syncobj a;
  struct nvgpu_fd *nfd;
  struct file *rf;
  u64 args[4], res[1];
  int ret;

  if (copy_from_user(&a, uarg, sizeof(a)))
    return -EFAULT;
  if (a.flags)
    return -EINVAL;
  if (!nvgpu_fences_enabled(dev))
    return -EOPNOTSUPP;
  rf = fget(a.render_fd);
  nfd = rf ? nvgpu_drm_file_nfd(rf) : NULL;
  if (!nfd || nfd->dev != dev) {
    if (rf)
      fput(rf);
    return -EBADF;
  }
  args[0] = nfd->handle;
  args[1] = a.id;
  args[2] = get_unaligned_le64(&a.token[0]);
  args[3] = get_unaligned_le64(&a.token[8]);
  ret = nvgpu_host_op(dev, NVGPU_OP_INJECT_OPEN_SYNCOBJ, args, 4, res, 1);
  fput(rf);
  if (ret < 0)
    return ret;
  /* A syncobj handle is a non-zero u32 (idr_alloc from 1). */
  if (!nvgpu_res_u32(res[0], &a.handle))
    return -EPROTO;
  if (copy_to_user(uarg, &a, sizeof(a)))
    return -EFAULT;
  return 0;
}

static int nvgpu_capture_fopen(struct inode *inode, struct file *filp) {
  struct miscdevice *m = filp->private_data;
  struct nvgpu_capture_dev *cd =
      container_of(m, struct nvgpu_capture_dev, misc);

  if (!cd->dev->v2)
    return -ENODEV;
  kref_get(&cd->ref); /* under misc_mtx, as nvgpu_wl_open() */
  filp->private_data = cd;
  return 0;
}

static int nvgpu_capture_release(struct inode *inode, struct file *filp) {
  struct nvgpu_capture_dev *cd = filp->private_data;

  kref_put(&cd->ref, nvgpu_capture_dev_free);
  return 0;
}

static long nvgpu_capture_ioctl(struct file *filp, unsigned int cmd,
                                unsigned long arg) {
  struct nvgpu_capture_dev *cd = filp->private_data;

  switch (cmd) {
  case NVGPU_CAPTURE_IOC_OPEN:
    return nvgpu_capture_open_ioctl(cd, (void __user *)arg);
  case NVGPU_CAPTURE_IOC_OPEN_SYNCOBJ:
    return nvgpu_capture_open_syncobj_ioctl(cd, (void __user *)arg);
  default:
    return -ENOTTY;
  }
}

static const struct file_operations nvgpu_capture_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_capture_fopen,
    .release = nvgpu_capture_release,
    .unlocked_ioctl = nvgpu_capture_ioctl,
    .compat_ioctl = compat_ptr_ioctl,
    .llseek = noop_llseek,
};

int nvgpu_capture_init(struct nvgpu_device *dev) {
  struct nvgpu_capture_dev *cd;
  int slot, ret;

  if (!dev->v2 || !(dev->backend_caps & NVGPU_BCAP_INJECT))
    return 0;

  cd = kzalloc(sizeof(*cd), GFP_KERNEL);
  if (!cd)
    return -ENOMEM;

  mutex_lock(&nvgpu_capture_devs_lock);
  for (slot = 0; slot < NVGPU_CAPTURE_MAX_DEVS && nvgpu_capture_devs[slot];
       slot++)
    ;
  if (slot == NVGPU_CAPTURE_MAX_DEVS) {
    mutex_unlock(&nvgpu_capture_devs_lock);
    kfree(cd);
    return -ENOSPC;
  }
  if (slot == 0)
    strscpy(cd->name, "nvgpu-capture", sizeof(cd->name));
  else
    snprintf(cd->name, sizeof(cd->name), "nvgpu-capture%d", slot);
  cd->dev = dev;
  nvgpu_dev_get(dev);
  kref_init(&cd->ref);
  cd->misc.minor = MISC_DYNAMIC_MINOR;
  cd->misc.name = cd->name;
  cd->misc.fops = &nvgpu_capture_fops;
  cd->misc.parent = &dev->vdev->dev;
  cd->misc.mode = nvgpu_capture_mode & 0777;
  ret = misc_register(&cd->misc);
  if (ret) {
    mutex_unlock(&nvgpu_capture_devs_lock);
    kref_put(&cd->ref, nvgpu_capture_dev_free);
    return ret;
  }
  nvgpu_capture_devs[slot] = cd;
  mutex_unlock(&nvgpu_capture_devs_lock);
  dev_info(&dev->vdev->dev,
           "virtio-gpu-nv: /dev/%s for the host's capture helper\n",
           cd->name);
  return 0;
}

void nvgpu_capture_cleanup(struct nvgpu_device *dev) {
  int slot;

  mutex_lock(&nvgpu_capture_devs_lock);
  for (slot = 0; slot < NVGPU_CAPTURE_MAX_DEVS; slot++) {
    struct nvgpu_capture_dev *cd = nvgpu_capture_devs[slot];

    if (!cd || cd->dev != dev)
      continue;
    misc_deregister(&cd->misc);
    nvgpu_capture_devs[slot] = NULL;
    kref_put(&cd->ref, nvgpu_capture_dev_free);
  }
  mutex_unlock(&nvgpu_capture_devs_lock);
}
