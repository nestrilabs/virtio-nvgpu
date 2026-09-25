/* SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0+ */
/*
 * virtio-gpu-nv wire protocol: what crosses the virtqueue and the device
 * config space, as the guest sees it.
 *
 * The Rust half is protocol/src/messages.rs (messages) and device/src/virtio.rs
 * (identity and config layout). Neither side negotiates, so a change here is a
 * change there too. Dual licensed like protocol/ (see protocol/README.md), so
 * the definitions can be shared with the Apache-2.0 host side.
 */

#ifndef NVGPU_WIRE_H
#define NVGPU_WIRE_H

#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/types.h>

/* ───────── virtio device identity ───────── */

#define VIRTIO_ID_GPU_NV 45

/* Feature bits */
#define VIRTIO_GPU_NV_F_UVM 0
#define VIRTIO_GPU_NV_F_ENCODE 1
#define VIRTIO_GPU_NV_F_GRAPHICS 2

/* ───────── Wire protocol constants ───────── */

#define NVGPU_MSG_OPEN 1
#define NVGPU_MSG_CLOSE 2
#define NVGPU_MSG_IOCTL 3
#define NVGPU_MSG_MMAP 4
#define NVGPU_MSG_MUNMAP 5
#define NVGPU_MSG_GET_PROC_FILES 6
#define NVGPU_MSG_GET_SYS_FILES 7
/* Host → guest, on the event queue: this handle's descriptor is readable. */
#define NVGPU_MSG_EVENT_READY 8

/* device_type values for OPEN */
#define NVGPU_DEV_CTL 255
#define NVGPU_DEV_UVM 256
#define NVGPU_DEV_UVM_TOOLS 257
#define NVGPU_DEV_MODESET 258
#define NVGPU_DEV_DRI_BASE 512

/* capability bits */
#define NVGPU_CAP_COMPUTE (1 << 0)
#define NVGPU_CAP_GRAPHICS (1 << 1)
#define NVGPU_CAP_VIDEO (1 << 2)
#define NVGPU_CAP_UTILITY (1 << 3)

/* ───────── Wire protocol structs ───────── */

struct nvgpu_msg_hdr {
  __le32 msg_type;
  __le32 handle;
  __le32 status;
  __le32 padding;
} __packed;

struct nvgpu_open_req {
  struct nvgpu_msg_hdr hdr;
  __le32 device_type;
  __le32 flags;
} __packed;

struct nvgpu_open_resp {
  struct nvgpu_msg_hdr hdr;
} __packed;

struct nvgpu_ioctl_req {
  struct nvgpu_msg_hdr hdr;
  __le32 cmd;
  __le32 data_len;
  __le32 nested_offset;
  __le32 nested_len;
  __le32 deep_ptr_offset;
  __le32 deep_len;
  /* followed by: data_len bytes top-level struct,
   *              nested_len bytes nested data,
   *              deep_len bytes of what a pointer inside the nested data
   *                  points at, at deep_ptr_offset within it       */
} __packed;

struct nvgpu_ioctl_resp {
  struct nvgpu_msg_hdr hdr;
  __le32 data_len;
  __le32 nested_len;
  __le32 deep_len;
  /* followed by: data_len bytes modified top-level,
   *              nested_len bytes modified nested,
   *              deep_len bytes modified second-level data   */
} __packed;

struct nvgpu_mmap_req {
  struct nvgpu_msg_hdr hdr;
  __le64 size;
  __le64 offset;
  __le32 prot;
  __le32 padding;
} __packed;

struct nvgpu_mmap_resp {
  struct nvgpu_msg_hdr hdr;
  __le64 guest_phys_addr;
  __le64 size;
  __le32 mapping_id;
  __le32 padding;
} __packed;

struct nvgpu_munmap_req {
  struct nvgpu_msg_hdr hdr;
  __le32 mapping_id;
  __le32 padding;
} __packed;

struct nvgpu_munmap_resp {
  struct nvgpu_msg_hdr hdr;
} __packed;

/* VMM response: stream of nvgpu_proc_file_entry records,
 * terminated by an entry with path_len == 0 */
struct nvgpu_proc_file_entry {
  __le32 path_len;    /* bytes in path[], 0 = end of stream */
  __le32 content_len; /* bytes in content[] */
  /* followed by: path_len bytes of path (no NUL),
   *              content_len bytes of content          */
} __packed;

/* Per-GPU slot in VMM config space — 476 bytes.
 *
 * info_text was 1060, which made this struct 1088 and the whole config 8912.
 * That cannot be delivered: virtio_pci_modern_dev.c maps the device config
 * capability with PAGE_SIZE as its maximum and silently truncates anything
 * longer ("length > size" -> "length = size"), so every field past 4096 read
 * back out of range and BUG'd in virtio_cread_bytes. The whole config must fit
 * in one page, and 448 bytes leaves room for the ~278 these files actually
 * contain while keeping all eight slots.
 */
struct virtio_gpu_nv_gpu_slot {
  char pci_addr[16];    /*    0.. 16  directory name          */
  __le32 minor;         /*   16.. 20  /dev/nvidia<minor>      */
  __le32 info_len;      /*   20.. 24  valid bytes in info_text */
  __le32 padding[1];    /*   24.. 28                          */
  char info_text[448];  /*   28.. 476 raw information content  */
} __packed;             /* 476 bytes */

struct nvgpu_fd_translation_entry {
  __le32 nr;
  __le32 payload_offset;
} __packed;

/* VMM config space layout */
struct virtio_gpu_nv_config {
  char driver_version[32];               /* 0.. 32  */
  __le32 num_gpus;                       /* 32.. 36 */
  __le32 caps;                           /* 36.. 40 */
  __le32 gpu_device_ids[8];              /* 40.. 72 */
  struct virtio_gpu_nv_gpu_slot gpus[8]; /* 72..    */
  __le32 num_fd_translations;
  __le32 _pad;
  struct nvgpu_fd_translation_entry fd_translations[16];
} __packed;

static_assert(sizeof(struct virtio_gpu_nv_gpu_slot) == 476,
              "gpu_slot size mismatch");
static_assert(sizeof(struct virtio_gpu_nv_config) == 4016,
              "virtio_gpu_nv_config size mismatch with VMM");
static_assert(offsetof(struct virtio_gpu_nv_config, num_fd_translations) ==
                  3880,
              "fd_translations offset mismatch with VMM");

/* The reason every number above is what it is. A guest cannot see past one
 * page of device config, so a layout that does not fit is not a tight fit --
 * it is unreadable. */
static_assert(sizeof(struct virtio_gpu_nv_config) <= 4096,
              "config space must fit in one page; see virtio_pci_modern_dev.c");

#endif /* NVGPU_WIRE_H */
