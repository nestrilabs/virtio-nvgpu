// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: backend handles materialised as guest files.
 *
 * Some host objects reach a guest process as a descriptor and nothing else: a
 * host syncobj file from SYNCOBJ_HANDLE_TO_FD, which the process passes to a
 * compositor (wp_linux_drm_syncobj_v1's import_timeline) or back into
 * SYNCOBJ_FD_TO_HANDLE. The host file lives in the backend, which holds it as
 * a handle; the guest needs a file standing for that handle, so that closing
 * it closes the host's, passing it over a socket passes it, and handing it
 * back to one of our ioctls names the host object again.
 *
 * That file is an anonymous inode with the handle in private_data. Nothing
 * can be done with it but hold it, pass it and close it -- which is all a
 * native syncobj file offers too (drm_syncobj.c:651: release is its only
 * operation). Host sync_files are *not* made this way: a guest sync_file has
 * to be a real one, because every guest-kernel consumer takes it through
 * sync_file_get_fence(), which checks for sync_file's own fops
 * (sync_file.c:81-117); those are nvgpu_fence.c's proxy fences.
 */

#include <linux/anon_inodes.h>
#include <linux/err.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/module.h>
#include <linux/slab.h>

#include "nvgpu.h"

struct nvgpu_hostfile {
  struct nvgpu_device *dev;
  u32 handle;
  u32 kind; /* NVGPU_HK_* */
};

/*
 * The last reference to the guest file: the host's goes too. From fput(),
 * i.e. process context (task work or the delayed-fput worker), so the CLOSE
 * may wait -- and when it cannot be sent at all it is queued, not lost.
 */
static int nvgpu_hostfile_release(struct inode *inode, struct file *f) {
  struct nvgpu_hostfile *hf = f->private_data;

  nvgpu_close_handle(hf->dev, hf->handle);
  nvgpu_dev_put(hf->dev); /* S-26: the file may outlive the device */
  kfree(hf);
  return 0;
}

static const struct file_operations nvgpu_hostfile_fops = {
    .owner = THIS_MODULE,
    .release = nvgpu_hostfile_release,
    .llseek = noop_llseek,
};

int nvgpu_hostfile_install(struct nvgpu_device *dev, u32 handle, u32 kind,
                           int o_flags, bool close_on_error) {
  struct nvgpu_hostfile *hf;
  struct file *file;
  int fd;

  hf = kzalloc(sizeof(*hf), GFP_KERNEL);
  if (!hf) {
    if (close_on_error)
      nvgpu_close_handle(dev, handle);
    return -ENOMEM;
  }
  hf->dev = dev;
  hf->handle = handle;
  hf->kind = kind;

  fd = get_unused_fd_flags(o_flags & O_CLOEXEC);
  if (fd < 0) {
    kfree(hf);
    if (close_on_error)
      nvgpu_close_handle(dev, handle);
    return fd;
  }
  /*
   * Named as the kernel names a syncobj file ("anon_inode:syncobj_file",
   * drm_syncobj.c:677), so a tool that looks at /proc/<pid>/fd sees what it
   * would see natively. Open for reading only, as that one is.
   */
  file = anon_inode_getfile(kind == NVGPU_HK_SYNCOBJ ? "syncobj_file"
                                                     : "nvgpu_host_file",
                            &nvgpu_hostfile_fops, hf,
                            O_RDONLY | (o_flags & O_NONBLOCK));
  if (IS_ERR(file)) {
    put_unused_fd(fd);
    kfree(hf);
    if (close_on_error)
      nvgpu_close_handle(dev, handle);
    return PTR_ERR(file);
  }
  nvgpu_dev_get(dev); /* the file's, put at its release */
  fd_install(fd, file);
  return fd;
}

int nvgpu_hostfile_handle(struct file *f, u32 *handle) {
  const struct nvgpu_hostfile *hf;

  if (f->f_op != &nvgpu_hostfile_fops)
    return -EBADF;
  hf = f->private_data;
  *handle = hf->handle;
  return 0;
}

struct file *nvgpu_hostfile_fget(struct nvgpu_device *dev, int fd, u32 kind,
                                 u32 *handle) {
  const struct nvgpu_hostfile *hf;
  struct file *f;

  f = fget(fd);
  if (!f)
    return ERR_PTR(-EBADF);
  if (f->f_op != &nvgpu_hostfile_fops)
    goto not_ours;
  hf = f->private_data;
  if (hf->dev != dev || hf->kind != kind)
    goto not_ours;
  *handle = hf->handle;
  return f;

not_ours:
  fput(f);
  return ERR_PTR(-EINVAL);
}

int nvgpu_hostfile_lookup(struct nvgpu_device *dev, int fd, u32 kind,
                          u32 *handle) {
  struct file *f = nvgpu_hostfile_fget(dev, fd, kind, handle);

  if (IS_ERR(f))
    return PTR_ERR(f);
  fput(f);
  return 0;
}
