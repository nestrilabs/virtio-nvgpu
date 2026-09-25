// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: backend handles materialised as guest files -- adopted DRM
 * files, syncobj files and host-fence sync_files.
 *
 * Only the lookup hook exists so far; the files themselves come with the KMS
 * and FENCES work, which will fill it in.
 */

#include "nvgpu.h"

/*
 * The backend handle a host-handle file stands for, so that a descriptor of
 * one can be named in a forwarded ioctl like any other file of ours
 * (nvgpu_handle_for_fd()). No such files exist yet: when their fops do, this
 * checks f->f_op against them and reads the handle from private_data. Until
 * then nothing is one of them.
 */
int nvgpu_hostfile_handle(struct file *f, u32 *handle) { return -EBADF; }
