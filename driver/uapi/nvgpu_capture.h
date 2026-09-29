/* SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0-or-later */
/*
 * /dev/nvgpu-capture: open a host buffer the host's capture helper injected
 * (ARCHITECTURE.md, "Capture injection"; SECURITY.md, "Capture injection").
 *
 * A screen share on the host is a stream of GPU buffers. The VM's capture
 * helper on the host hands each to the virtio-nvgpu backend, which answers
 * with an id and a 16-byte random token; the helper tells the guest's own
 * daemon both over a channel of its own (vsock). The daemon -- whoever may
 * open this node -- turns them into a local dma-buf with one ioctl:
 *
 *   render_fd  an open file of this device's DRM render node (or card
 *              node): the buffer is imported into its host file, and the
 *              dma-buf is a GEM proxy of that file, like any other export.
 *   id, token  what the helper was given. A wrong token and an id that is
 *              not live are the same -ENOENT.
 *   flags      0.
 *
 * On success `dmabuf_fd` is a new O_CLOEXEC dma-buf descriptor, opened
 * read-only: it cannot be mapped writable, and neither can the object
 * through the render node (the backend places it read-only, and a writable
 * mapping of a read-only placement is refused). The rest describes the
 * buffer as the helper did, checked by the backend against the object:
 * a DRM fourcc and format modifier, and each plane's offset and stride in
 * the one dma-buf (every plane is the same object). A GPU import of it
 * (EGL_EXT_image_dma_buf_import_modifiers, VK_EXT_image_drm_format_modifier)
 * is read-write to the GPU, as every NVIDIA import is.
 *
 * The dma-buf keeps the memory alive after the helper releases the id;
 * releasing only stops new opens. Nothing here says when a frame is ready
 * or when the guest is done with it: that is the helper's and the daemon's
 * own protocol (ARCHITECTURE.md, "Capture injection", sync).
 *
 * Errors: EBADF (render_fd is not a DRM file of this device), ENOENT, ENODEV
 * (render_fd is another GPU's), EAGAIN (this process holds its share of
 * opens), EINVAL (flags), EOPNOTSUPP (no capture helper configured).
 *
 * Dual licensed like nvgpu_wire.h.
 */

#ifndef _UAPI_NVGPU_CAPTURE_H
#define _UAPI_NVGPU_CAPTURE_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define NVGPU_CAPTURE_MAX_PLANES 4

/* nvgpu_capture_open.buf_flags: the rows run bottom to top. */
#define NVGPU_CAPTURE_F_Y_INVERT (1u << 0)

struct nvgpu_capture_open {
  /* in */
  __s32 render_fd;
  __u32 id;
  __u8 token[16];
  __u32 flags;
  /* out */
  __s32 dmabuf_fd;
  __u32 width;
  __u32 height;
  __u32 fourcc;
  __u32 nplanes;
  __u64 modifier;
  __u32 offsets[NVGPU_CAPTURE_MAX_PLANES];
  __u32 strides[NVGPU_CAPTURE_MAX_PLANES];
  __u32 buf_flags;
  __u32 pad;
  __u64 size; /* of the dma-buf */
};

#define NVGPU_CAPTURE_IOC_OPEN _IOWR('C', 0x40, struct nvgpu_capture_open)

/*
 * OPEN_SYNCOBJ: a syncobj the helper injected (explicit sync), imported into
 * render_fd's file. `handle` is a syncobj handle of that DRM file, used with
 * the ordinary syncobj ioctls on it (TIMELINE_WAIT, TIMELINE_SIGNAL,
 * HANDLE_TO_FD for a Vulkan or EGL import). What its points mean is the
 * helper's and the daemon's protocol; the host trusts none of them. Each call
 * makes a new handle, as SYNCOBJ_FD_TO_HANDLE does; SYNCOBJ_DESTROY it when
 * done (the file's close does too). Errors as OPEN's.
 */
struct nvgpu_capture_open_syncobj {
  /* in */
  __s32 render_fd;
  __u32 id;
  __u8 token[16];
  __u32 flags;
  /* out */
  __u32 handle;
};

#define NVGPU_CAPTURE_IOC_OPEN_SYNCOBJ                                          \
  _IOWR('C', 0x41, struct nvgpu_capture_open_syncobj)

#ifdef __KERNEL__
static_assert(sizeof(struct nvgpu_capture_open) == 104, "capture open");
static_assert(sizeof(struct nvgpu_capture_open_syncobj) == 32,
              "capture open syncobj");
#endif

#endif /* _UAPI_NVGPU_CAPTURE_H */
