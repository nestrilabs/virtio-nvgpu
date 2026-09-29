// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the misc nodes a device has besides the NVIDIA ones --
 * /dev/nvgpu-wl (nvgpu_wl.c) and /dev/nvgpu-capture (nvgpu_capture.c) -- and
 * what they share: registration, their lifetime, and the rule for their mode.
 *
 * A node is refcounted: remove() deregisters it, but files opened before that
 * still name it until they are released. An open takes its reference under
 * misc_mtx, which misc_deregister() also takes (drivers/char/misc.c:125-165,
 * 284-293), so no open can find a node once unregister has dropped the
 * initial one. And the node holds the nvgpu_device, which such a file's
 * release and ioctls use, until it goes itself: remove() alone would free it
 * under them (S-26).
 */

#include <linux/kernel.h>
#include <linux/miscdevice.h>
#include <linux/moduleparam.h>

#include "nvgpu.h"

/*
 * A node's mode: within 0770, nothing for "other" (the module does not load
 * with more). Every open of either node reaches the host's compositor or an
 * injected capture stream, and only the guest's own daemon needs that; its
 * group comes from udev (contrib/udev/).
 */
static int nvgpu_misc_mode_set(const char *val,
                               const struct kernel_param *kp) {
  u16 mode;
  int ret = kstrtou16(val, 0, &mode);

  if (ret)
    return ret;
  if (mode & ~0770)
    return -EINVAL;
  *(ushort *)kp->arg = mode;
  return 0;
}

const struct kernel_param_ops nvgpu_misc_mode_ops = {
    .set = nvgpu_misc_mode_set,
    .get = param_get_ushort,
};

static void nvgpu_misc_node_release(struct kref *ref) {
  struct nvgpu_misc_node *node =
      container_of(ref, struct nvgpu_misc_node, ref);
  struct nvgpu_device *dev = node->dev;

  nvgpu_dev_put(dev);
  node->free(node);
}

int nvgpu_misc_node_register(struct nvgpu_misc_node *node,
                             struct nvgpu_device *dev, const char *name,
                             ushort mode, const struct file_operations *fops,
                             void (*free)(struct nvgpu_misc_node *node)) {
  int ret;

  node->dev = dev;
  nvgpu_dev_get(dev);
  kref_init(&node->ref);
  node->free = free;
  node->misc.minor = MISC_DYNAMIC_MINOR;
  node->misc.name = name;
  node->misc.fops = fops;
  node->misc.parent = &dev->vdev->dev;
  node->misc.mode = mode & 0777;
  ret = misc_register(&node->misc);
  if (ret)
    nvgpu_misc_node_put(node);
  return ret;
}

void nvgpu_misc_node_unregister(struct nvgpu_misc_node *node) {
  misc_deregister(&node->misc);
  nvgpu_misc_node_put(node);
}

struct nvgpu_misc_node *nvgpu_misc_node_open(struct file *filp) {
  struct miscdevice *m = filp->private_data;

  return container_of(m, struct nvgpu_misc_node, misc);
}

void nvgpu_misc_node_get(struct nvgpu_misc_node *node) {
  kref_get(&node->ref);
}

void nvgpu_misc_node_put(struct nvgpu_misc_node *node) {
  kref_put(&node->ref, nvgpu_misc_node_release);
}
