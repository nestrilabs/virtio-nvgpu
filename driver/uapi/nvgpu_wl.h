/* SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0+ */
/*
 * /dev/nvgpu-wl: the guest end of the virtio-nvgpu Wayland channel.
 *
 * Each open file is one channel. The guest daemon (nvgpu-wl-guest) opens one
 * per Wayland client it proxies, CONNECTs it, and then moves *frames* through
 * it with SEND and RECV; poll() reports POLLIN while the host has something
 * for the channel. A frame is the same bytes the virtqueue carries (WL_SEND /
 * WL_RECV): a header, a descriptor table, and typed records. The records are
 * the daemon's and the backend's business; the kernel only reads the header
 * and the descriptor table, and only where a descriptor needs the kernel:
 *
 *   SEND, DMABUF:   `fd` is a dma-buf of this device's GEM proxies. The
 *                   kernel replaces it with the proxy's (owner handle, host
 *                   GEM) in `a`/`b`, holds the dma-buf until the host has
 *                   exported it, and sets `fd` to -1. A dma-buf of anyone
 *                   else is sent as NVGPU_WL_DESC_F_INVALID (the compositor
 *                   then refuses that buffer, not the connection).
 *   SEND, SYNCOBJ:  (NVGPU_WL_CAP_SYNCOBJ) `fd` is a syncobj file of this
 *                   device (drmSyncobjHandleToFD on one of our DRM files: a
 *                   host-handle file). The kernel puts the backend handle of
 *                   its host syncobj in `a`, holds the file until the host has
 *                   duplicated it for the compositor, and sets `fd` to -1.
 *                   Any other file is sent as NVGPU_WL_DESC_F_INVALID, and
 *                   the backend ends that connection with the protocol's
 *                   fatal invalid_timeline error on the manager (the
 *                   compositor never sees it).
 *   RECV, DRM_FILE: `a` is a backend handle to a host DRM file of our GPU (a
 *                   lease). The kernel adopts it into a new guest DRM file,
 *                   cloned from the `card_fd` template, and stores the new
 *                   descriptor in `fd` (O_CLOEXEC).
 *   RECV, DMABUF:   (export mode) `a` is a backend dma-buf handle. The kernel
 *                   imports it into the `render_fd` file, makes a GEM proxy,
 *                   exports it, and stores the dma-buf descriptor in `fd`.
 *
 * Every other descriptor kind must have `fd` == -1 on SEND and gets -1 on
 * RECV. A RECV descriptor the kernel could not materialise comes back with
 * `fd` -1 and NVGPU_WL_DESC_F_INVALID set.
 *
 * Frame layout (little-endian), shared with wlwire/src/frame.rs:
 *
 *   struct nvgpu_wl_frame_hdr
 *   struct nvgpu_wl_desc      desc[ndesc]
 *   records, rec_len bytes: struct nvgpu_wl_rec + payload padded to 8
 *
 * Dual licensed like nvgpu_wire.h, so the Apache-2.0 daemon can mirror it
 * (nvgpu-wl-guest/src/uapi.rs, with the same size asserts).
 */

#ifndef _UAPI_NVGPU_WL_H
#define _UAPI_NVGPU_WL_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define NVGPU_WL_UAPI_VERSION 1

/* ── HELLO ── */

/* nvgpu_wl_hello.caps */
#define NVGPU_WL_CAP_WAYLAND (1u << 0)       /* the host has a compositor socket    */
#define NVGPU_WL_CAP_EXPORT (1u << 1)        /* the host exports a socket to us     */
#define NVGPU_WL_CAP_DRM_FILE (1u << 2)      /* RECV can adopt DRM files            */
#define NVGPU_WL_CAP_DMABUF_IMPORT (1u << 3) /* RECV can import host dma-bufs       */
#define NVGPU_WL_CAP_SYNCOBJ (1u << 4)       /* SEND can name host syncobjs         */

#define NVGPU_WL_MAX_DEVMAP 8

/* nvgpu_wl_devmap.flags */
#define NVGPU_WL_DEV_RENDER (1u << 0)
#define NVGPU_WL_DEV_CARD (1u << 1)

/* One DRM node as the host numbers it and as this guest does. */
struct nvgpu_wl_devmap {
  __u32 host_major;
  __u32 host_minor;
  __u32 guest_major;
  __u32 guest_minor;
  __u32 flags;
  __u32 pad;
};

struct nvgpu_wl_hello {
  __u32 version;         /* out: NVGPU_WL_UAPI_VERSION */
  __u32 caps;            /* out: NVGPU_WL_CAP_* */
  __s64 clock_offset_ns; /* out: host minus guest CLOCK_MONOTONIC, now */
  __u32 max_frame;       /* out: largest frame SEND takes / RECV returns */
  __u32 ndev;            /* out: valid entries in dev[] */
  struct nvgpu_wl_devmap dev[NVGPU_WL_MAX_DEVMAP];
};

/* ── CONNECT ── */

#define NVGPU_WL_CONNECT 0 /* a new connection to the host compositor        */
#define NVGPU_WL_LISTEN 1  /* export mode: poll() says a host client waits   */
#define NVGPU_WL_ACCEPT 2  /* export mode: take the next waiting host client */

struct nvgpu_wl_connect {
  __u32 mode; /* NVGPU_WL_CONNECT / _LISTEN / _ACCEPT */
  __u32 flags; /* 0 */
};

/* ── SEND / RECV ── */

/* nvgpu_wl_xfer.flags (RECV, out) */
#define NVGPU_WL_XFER_MORE (1u << 0) /* more is waiting: RECV again */

struct nvgpu_wl_xfer {
  __u64 frame;     /* user pointer to the frame */
  __u32 len;       /* SEND: frame bytes; RECV: buffer size in, frame bytes out */
  __u32 max_desc;  /* RECV: descriptor-table capacity the caller accepts */
  __s32 card_fd;   /* RECV: guest card-node file to clone DRM files from, or -1 */
  __s32 render_fd; /* RECV: guest render-node file to import dma-bufs into, or -1 */
  __u32 flags;     /* RECV out: NVGPU_WL_XFER_* */
  __u32 backlog;   /* SEND out: bytes still waiting for the host compositor */
};

#define NVGPU_WL_IOC_HELLO _IOR('W', 0x40, struct nvgpu_wl_hello)
#define NVGPU_WL_IOC_CONNECT _IOW('W', 0x41, struct nvgpu_wl_connect)
#define NVGPU_WL_IOC_SEND _IOWR('W', 0x42, struct nvgpu_wl_xfer)
#define NVGPU_WL_IOC_RECV _IOWR('W', 0x43, struct nvgpu_wl_xfer)

/* ── Frames ── */

#define NVGPU_WL_FRAME_MAGIC 0x4c57564eu /* "NVWL" */
#define NVGPU_WL_FRAME_VERSION 1
#define NVGPU_WL_MAX_DESC 256
/*
 * The smallest RECV buffer the host takes (wlwire::frame::MIN_FRAME): a
 * header, 32 descriptors and one record of the largest payload (64 KiB).
 * Anything smaller is refused, with nothing taken from the channel.
 */
#define NVGPU_WL_MIN_FRAME (16 + 32 * 24 + 16 + 65536)

/* nvgpu_wl_frame_hdr.flags */
#define NVGPU_WL_FRAME_F_MORE (1u << 0)

struct nvgpu_wl_frame_hdr {
  __u32 magic;
  __u16 version;
  __u16 ndesc;
  __u32 rec_len;
  __u32 flags;
};

/* nvgpu_wl_desc.kind */
#define NVGPU_WL_DESC_DMABUF 1
#define NVGPU_WL_DESC_SHM_POOL 2
#define NVGPU_WL_DESC_BLOB 3
#define NVGPU_WL_DESC_STREAM 4
#define NVGPU_WL_DESC_DRM_FILE 5
#define NVGPU_WL_DESC_SYNCOBJ 6

/* nvgpu_wl_desc.flags */
#define NVGPU_WL_DESC_F_INVALID (1u << 0)

struct nvgpu_wl_desc {
  __u16 kind;
  __u16 flags;
  __s32 fd;
  __u32 a;
  __u32 b;
  __u64 c;
};

/* Records (the kernel does not look at them). */
#define NVGPU_WL_REC_HELLO 1
#define NVGPU_WL_REC_WAYLAND 2
#define NVGPU_WL_REC_STREAM_DATA 3
#define NVGPU_WL_REC_STREAM_EOF 4
#define NVGPU_WL_REC_STREAM_CREDIT 5
#define NVGPU_WL_REC_SHM_SYNC 6
#define NVGPU_WL_REC_BLOB 7
#define NVGPU_WL_REC_ERROR 8
#define NVGPU_WL_REC_HANGUP 9

struct nvgpu_wl_rec {
  __u16 type;
  __u16 flags;
  __u32 len; /* payload bytes, before padding to 8 */
  __u32 id;
  __u32 arg;
};

#endif /* _UAPI_NVGPU_WL_H */
