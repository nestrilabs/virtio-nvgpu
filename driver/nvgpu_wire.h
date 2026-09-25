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
/* Protocol v2 (see the v2 section at the end of this file). */
#define NVGPU_MSG_HELLO 9
#define NVGPU_MSG_IOCTL2 10
#define NVGPU_MSG_TIME_SYNC 11
/* Host → guest, on the event queue: a batch of nvgpu_ev_rec records. */
#define NVGPU_MSG_EVENT_DATA 12
#define NVGPU_MSG_WATCH 13
#define NVGPU_MSG_UNWATCH 14
#define NVGPU_MSG_HOST_OP 15
#define NVGPU_MSG_WL_SEND 16
#define NVGPU_MSG_WL_RECV 17

/* device_type values for OPEN */
#define NVGPU_DEV_CTL 255
#define NVGPU_DEV_UVM 256
#define NVGPU_DEV_UVM_TOOLS 257
#define NVGPU_DEV_MODESET 258
#define NVGPU_DEV_WAYLAND 259
#define NVGPU_DEV_DRI_BASE 512
/* Card (primary) nodes, only offered in compositor-VM mode. Decoded before the
 * render-node range, so an old backend reads 1024 as render node 512 and
 * answers ENODEV rather than opening something else. */
#define NVGPU_DEV_DRI_CARD_BASE 1024

/* capability bits */
#define NVGPU_CAP_COMPUTE (1 << 0)
#define NVGPU_CAP_GRAPHICS (1 << 1)
#define NVGPU_CAP_VIDEO (1 << 2)
#define NVGPU_CAP_UTILITY (1 << 3)

/* ───────── Wire protocol structs ───────── */

/*
 * Every message starts with this, in both directions.
 *
 * `req_id` was padding until protocol v2. A v2 guest puts a non-zero id in
 * every request and the backend echoes it; it is for logging and for telling a
 * late reply from a fresh one, and a v1 guest's zero there means "none".
 */
struct nvgpu_msg_hdr {
  __le32 msg_type;
  __le32 handle;
  __le32 status;
  __le32 req_id;
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

/*
 * deep_ptr_offset == NVGPU_DEEP_SEGMENTED: the deep block carries what several
 * pointers of one parameter block address, one segment each -- a
 * nvgpu_deep_seg_hdr, `count` nvgpu_deep_seg, then each segment's bytes back
 * to back in table order. The pointers are an RM control's (in the nested
 * block) or NV_ESC_RM_IDLE_CHANNELS' (in the top-level NVOS30). Sent only to
 * a backend with NVGPU_BCAP_DEEP_SEGS, which sizes every segment itself and
 * refuses the call if a length is not what RM will copy. A reply's deep
 * block, if any, is laid out as the request's, with the bytes RM left.
 * protocol/src/messages.rs, DEEP_SEGMENTED, has the rest.
 */
#define NVGPU_DEEP_SEGMENTED 0xffffffffu
#define NVGPU_DEEP_SEGS_MAX 4
#define NVGPU_DEEP_SEGS_MAX_BYTES (1u << 20)
/* Most channels an IDLE_CHANNELS list may name. */
#define NVGPU_IDLE_CHANNELS_MAX 4096

struct nvgpu_deep_seg_hdr {
  __le32 count; /* 1..NVGPU_DEEP_SEGS_MAX */
  __le32 reserved;
} __packed;

struct nvgpu_deep_seg {
  __le32 ptr_offset; /* of the pointer, in the block holding it */
  __le32 len;        /* bytes it addresses, after the table     */
} __packed;

/*
 * deep_ptr_offset == NVGPU_DEEP_PAGE_LIST: memory the caller already has,
 * registered with RM by the guest-physical pages behind it rather than by its
 * address -- NV_ESC_RM_ALLOC_MEMORY or NV_ESC_RM_ALLOC of
 * NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, or NV_ESC_RM_VID_HEAP_CONTROL's
 * ALLOC_OS_DESCRIPTOR, with the user virtual address descriptor type. The
 * deep block is a nvgpu_osdesc_hdr and `nruns` nvgpu_osdesc_run, nothing
 * after: the pages RM would pin, from the one holding the address to the one
 * holding its last byte, pinned here (for writing unless the call asks for
 * read-only memory: NVGPU_OSDESC_F_WRITE). Sent only to a backend with
 * NVGPU_BCAP_OS_DESC. A reply with RM's NV_OK carries an 8-byte deep block,
 * the registration id; the pages stay pinned until a reap
 * (NVGPU_OP_OSDESC_REAP) names it. protocol/src/messages.rs, DEEP_PAGE_LIST,
 * has the rest.
 */
#define NVGPU_DEEP_PAGE_LIST 0xfffffffeu
#define NVGPU_OSDESC_F_WRITE (1u << 0)
#define NVGPU_OSDESC_MAX_RUNS 8192
#define NVGPU_OSDESC_MAX_PAGES (1u << 20)

struct nvgpu_osdesc_hdr {
  __le32 nruns; /* 1..NVGPU_OSDESC_MAX_RUNS */
  __le32 flags; /* NVGPU_OSDESC_F_* */
} __packed;

struct nvgpu_osdesc_run {
  __le64 gpa;   /* page-aligned guest-physical address */
  __le32 pages; /* >= 1 */
  __le32 reserved;
} __packed;

struct nvgpu_mmap_req {
  struct nvgpu_msg_hdr hdr;
  __le64 size;
  __le64 offset;
  __le32 prot;
  __le32 padding;
} __packed;

/*
 * `caching` was padding before protocol v2, and a backend leaves it zero for a
 * v1 session, so zero keeps its old meaning: the guest's choice, which was
 * always write-combining. The host picks a memory type per mapping (nv-mmap.c
 * forces UC on registers and uses the allocation's own type for system
 * memory), and a guest mapping of another type is slow (WC reads of cached
 * memory) or wrong (a WC doorbell).
 */
#define NVGPU_MMAP_CACHE_DEFAULT 0
#define NVGPU_MMAP_CACHE_WB 1
#define NVGPU_MMAP_CACHE_WC 2
#define NVGPU_MMAP_CACHE_UC 3

/* The host mapping is read-only (nv-mmap.c:756-761 clears VM_WRITE and
 * VM_MAYWRITE). A guest write through the window would reach KVM as a write
 * fault it cannot resolve, which stops the VM; the guest refuses it instead. */
#define NVGPU_MMAP_F_READ_ONLY (1u << 0)
/* guest_phys_addr is an offset in the UVM aperture (shared memory region
 * NVGPU_SHM_ID_UVM), not in the window: a UVM semaphore pool the VMM maps at
 * the pool's own host address, as UVM requires, with a memory slot of its own.
 * Only in reply to an MMAP of a UVM file, after NVGPU_BCAP_UVM_MAP; always
 * write-back, never read-only. */
#define NVGPU_MMAP_F_UVM_APERTURE (1u << 1)

/* The shared memory region id of the UVM aperture. The window is id 1. */
#define NVGPU_SHM_ID_UVM 2

/* The band a UVM pool's host address must lie in, [4 GiB, 32 TiB): nothing of
 * a 64-bit VMM's own is there (protocol/src/messages.rs, UVM_HVA_MIN). The
 * backend and the VMM check it too; this only refuses sooner. */
#define NVGPU_UVM_HVA_MIN (1ull << 32)
#define NVGPU_UVM_HVA_MAX (1ull << 45)

struct nvgpu_mmap_resp {
  struct nvgpu_msg_hdr hdr;
  __le64 guest_phys_addr;
  __le64 size;
  __le32 mapping_id;
  __u8 caching; /* NVGPU_MMAP_CACHE_* */
  __u8 flags;   /* NVGPU_MMAP_F_* */
  __le16 reserved;
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

/*
 * An entry whose nr has this bit set names a UVM command (the whole command
 * number in the low bits, UVM numbers being plain) rather than an RM escape,
 * and its payload_offset is packed: the descriptor's offset in bits 0-15 and
 * the parameter block's size in bits 16-31, UVM numbers carrying no size and
 * one offset depending on the host release (device/src/uvmfd.rs). A driver
 * that predates it compares nr with an escape's 8-bit number and so never
 * matches one.
 */
#define NVGPU_FDT_UVM 0x80000000u

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

/* ═════════════════════════ Protocol v2 ═════════════════════════
 *
 * Negotiated at run time, not by feature bit: the VMM in front of the device
 * may not pass device feature bits through, and an old backend answers an
 * unknown message with -EPROTO, which is all the guest needs to stay on v1.
 * Only a successful HELLO switches a session to v2; without it the backend
 * sends nothing but 16-byte EVENT_READY and behaves exactly as before.
 */

#define NVGPU_PROTO_V2 2

/* HELLO.flags */
#define NVGPU_HELLO_F_FRESH (1u << 0) /* new driver instance: reset the session */

/* HELLO backend_caps */
#define NVGPU_BCAP_KMS_CARD (1u << 0)    /* card nodes offered (--kms-card)     */
#define NVGPU_BCAP_WAYLAND (1u << 1)     /* a host Wayland socket is configured */
#define NVGPU_BCAP_FENCES (1u << 2)      /* fence and syncobj schemas           */
#define NVGPU_BCAP_NVKMS_TABLE (1u << 3) /* NVKMS schema for this host version  */
#define NVGPU_BCAP_WL_EXPORT (1u << 4)   /* --wayland-export                    */
#define NVGPU_BCAP_DEEP_SEGS (1u << 5)   /* NVGPU_DEEP_SEGMENTED deep blocks    */
#define NVGPU_BCAP_UVM_MAP (1u << 6)     /* UVM pools map into the aperture     */
#define NVGPU_BCAP_OS_DESC (1u << 7)     /* NVGPU_DEEP_PAGE_LIST registrations  */
#define NVGPU_BCAP_PROC_ID (1u << 8)     /* nvgpu_proc_id on RM_ALLOC, RM_DUP   */
#define NVGPU_BCAP_PROC_EUID (1u << 9)   /* ... with euid, and on RM_CONTROL    */
#define NVGPU_BCAP_COMPUTE (1u << 10)    /* UVM served (--allow-compute)        */

/* HELLO guest_caps */
#define NVGPU_GCAP_UVM_APERTURE (1u << 0) /* region NVGPU_SHM_ID_UVM found */
#define NVGPU_GCAP_PROC_ID (1u << 1)      /* can send nvgpu_proc_id        */
#define NVGPU_GCAP_PROC_EUID (1u << 2)    /* ... with the caller's euid     */

/*
 * The guest process an IOCTL is made by. With NVGPU_BCAP_PROC_ID, every
 * NVGPU_MSG_IOCTL of NV_ESC_RM_ALLOC or NV_ESC_RM_DUP_OBJECT carries one after
 * its blocks (after the deep_len bytes), and with NVGPU_BCAP_PROC_EUID every
 * NV_ESC_RM_CONTROL too. The host sees every guest process's RM calls as the
 * backend's, one process; this is how the backend keeps RM objects to the
 * guest process that made their client, as RM keeps them to a host process,
 * and holds a second client a call names to RM's rule for it -- the same
 * process, or the same euid where RM's rule is its security token
 * (protocol/src/messages.rs, ProcId; device/src/rmshare.rs). The process is
 * the calling thread group's leader, by its PID in the initial namespace and
 * its start time, a pair no other process has for the guest's lifetime.
 */
struct nvgpu_proc_id {
  __le64 start_ns; /* group_leader->start_time, CLOCK_MONOTONIC */
  __le32 tgid;     /* task_tgid_nr(), initial PID namespace     */
  __le32 euid;     /* current_euid(), initial user namespace, with
                      NVGPU_BCAP_PROC_EUID; 0 otherwise */
} __packed;

struct nvgpu_hello_req {
  __le32 proto;            /* NVGPU_PROTO_V2 */
  __le32 flags;            /* NVGPU_HELLO_F_* */
  __le32 guest_caps;       /* NVGPU_GCAP_*; was reserved, 0 */
  __le32 uvm_aperture_mib; /* the UVM aperture's length, 0 if none; was
                              reserved, 0 */
} __packed;

struct nvgpu_hello_resp {
  __le32 proto;
  __le32 backend_caps;
  __le32 max_req;  /* largest request the backend accepts, bytes */
  __le32 max_resp; /* largest response it will build, bytes      */
  __le32 num_cards;
  __le32 reserved[3];
} __packed;

struct nvgpu_time_sync_resp {
  __le64 host_mono_ns; /* CLOCK_MONOTONIC, stamped just before add_used */
} __packed;

/*
 * The same reply, with the host's other two clocks read in the same instant:
 * what RM stamps GPU/CPU time correlation samples with (0x20800406: OSTIME is
 * CLOCK_REALTIME in us, PLATFORM_API is CLOCK_MONOTONIC_RAW in ns), which the
 * guest rebases per clock. Sent only when the reply buffer has room for it,
 * so a guest that posts the 8-byte form gets that; a backend that predates it
 * fills 8 bytes of a larger buffer, which the used length says.
 */
struct nvgpu_time_sync_resp2 {
  __le64 host_mono_ns;
  __le64 host_realtime_ns; /* CLOCK_REALTIME */
  __le64 host_mono_raw_ns; /* CLOCK_MONOTONIC_RAW */
  __le64 reserved;
} __packed;

/* ── IOCTL2: a vectored ioctl, laid out by a schema both halves share ──
 *
 * The request is the ioctl argument (buffer 0) and every buffer a pointer in
 * it reaches, in the schema's canonical traversal order. The backend does not
 * take the guest's word for any of that: it walks its own copy of the schema
 * over the bytes it received, recomputes every pointer, length, descriptor and
 * GEM field, and refuses a request that disagrees. See gen/schema/.
 */
#define NVGPU_I2_MAX_BUFS 256
#define NVGPU_I2_MAX_RECS 256

/* nvgpu_i2_fd_in.flags */
#define NVGPU_I2_FD_CONSUME (1u << 0) /* backend closes the handle after the call */

/* nvgpu_i2_dyn.kind */
#define NVGPU_I2_DYN_OUT_FENCE 1 /* ATOMIC OUT_FENCE_PTR: 4-byte OUT buffer + fd out */

struct nvgpu_i2_req {
  __le32 cmd;   /* the caller's ioctl number, verbatim */
  __le32 flags; /* reserved, 0 */
  __le32 nbuf;
  __le32 nfd;
  __le32 ngem;
  __le32 ndyn;
  __le32 data_len;
  /* Render handle of the calling guest file. GEM handles the call creates are
   * re-homed into it (host KMS files never own guest objects), and a GEM_IN
   * whose owner is this file needs no re-homing. Equal to hdr.handle for a
   * call made on a render handle. */
  __le32 render;
  /* followed by: __le32 buf_len[nbuf];
   *              struct nvgpu_i2_fd_in  fd[nfd];
   *              struct nvgpu_i2_gem_in gem[ngem];
   *              struct nvgpu_i2_dyn    dyn[ndyn];
   *              u8 data[data_len]: the IN bytes of every IN/INOUT buffer,
   *                                 in buffer order, each padded to 8 */
} __packed;

struct nvgpu_i2_fd_in {
  __le32 buf;    /* which buffer the descriptor field is in */
  __le32 off;    /* byte offset of the field in that buffer */
  __le32 handle; /* backend handle standing for the caller's descriptor */
  __le32 flags;  /* NVGPU_I2_FD_* */
} __packed;

struct nvgpu_i2_gem_in {
  __le32 buf;
  __le32 off;
  __le32 owner; /* backend handle of the file the host GEM handle lives in */
  __le32 gem;   /* host GEM handle in that file */
} __packed;

struct nvgpu_i2_dyn {
  __le32 kind; /* NVGPU_I2_DYN_* */
  __le32 buf;
  __le32 off;
  __le32 len;
} __packed;

struct nvgpu_i2_resp {
  __le32 ret; /* host ioctl result: 0 or -errno (signed) */
  __le32 nbuf;
  __le32 nfd;
  __le32 ngem;
  __le32 data_len;
  __le32 reserved[3];
  /* followed by: u8 data[data_len]: the OUT bytes of every OUT/INOUT buffer,
   *                                 full length, in buffer order, padded to 8;
   *              struct nvgpu_i2_fd_out  fd[nfd];
   *              struct nvgpu_i2_gem_out gem[ngem]; */
} __packed;

struct nvgpu_i2_fd_out {
  __le32 buf;
  __le32 off;
  __le32 handle; /* new backend handle now owning the host descriptor */
  __le32 kind;   /* NVGPU_HK_* */
} __packed;

struct nvgpu_i2_gem_out {
  __le32 buf;
  __le32 off;
  __le32 gem; /* host GEM handle, valid in the calling file's render handle */
  __le32 reserved;
  __le64 size;
} __packed;

/* ── Handle kinds, as the backend classifies a host descriptor ── */
#define NVGPU_HK_DEV 1
#define NVGPU_HK_DRI_RENDER 2
#define NVGPU_HK_DRM_CARD 3
#define NVGPU_HK_DRM_LEASE 4
#define NVGPU_HK_SYNC_FILE 5
#define NVGPU_HK_SYNCOBJ 6
#define NVGPU_HK_DMABUF 7
#define NVGPU_HK_EVENTFD 8
#define NVGPU_HK_MEMFD 9
#define NVGPU_HK_WAYLAND 10
#define NVGPU_HK_OTHER 11

/* ── WATCH ── */
#define NVGPU_W_ONESHOT (1u << 0)
#define NVGPU_W_FENCE (1u << 1) /* SyncFile: report EV_FENCE with its status */
#define NVGPU_W_DRM (1u << 2)   /* DRM card/lease: read events, EV_DRM       */
#define NVGPU_W_READY (1u << 3) /* readiness only, EV_READY                  */

struct nvgpu_watch_req {
  __le32 handle;
  __le32 flags;
  __le64 cookie;
} __packed;

struct nvgpu_unwatch_req {
  __le32 handle;
  __le32 pad;
} __packed;

/* ── HOST_OP ── */
#define NVGPU_OP_PRIME_EXPORT 1       /* (render file, gem) -> dmabuf handle         */
#define NVGPU_OP_DMABUF_IMPORT 2      /* (render file, dmabuf) -> (gem, size)        */
/* ... and a third result, the imported object's GEM_IDENTIFY_OBJECT type
 * (NVGPU_GEM_OBJECT_*); an older backend sends two, read as NVKMS (0). */
#define NVGPU_OP_SYNC_MERGE 3         /* (n, h0..) -> sync_file handle               */
#define NVGPU_OP_NEW_EVENTFD 4        /* () -> eventfd handle                        */
#define NVGPU_OP_FD_KIND 5            /* (handle) -> NVGPU_HK_*                      */
#define NVGPU_OP_SIGNALED_SYNC_FILE 6 /* () -> sync_file handle, already signalled   */
#define NVGPU_OP_OPEN_KMS 7           /* (render handle, card) -> card handle        */
#define NVGPU_OP_DROP_IF_MASTER 8     /* (card handle) -> 1 if it was master         */
#define NVGPU_OP_CLOSE_MANY 9         /* (n, h0..) -> ()                             */
/* (render, syncobj, point, flags, cookie) -> (reporting cookie, joined): one
 * shared SYNCOBJ_EVENTFD registration per (file, syncobj, point, flags),
 * reported once as EV_READY; -EAGAIN over the per-VM cap (device/src/fence.rs) */
#define NVGPU_OP_SYNCOBJ_WATCH 10
/* (ack) -> (last, count), then `count` __le64 registration ids after the
 * nvgpu_host_op_resp: OS-descriptor registrations RM has let go of, released
 * after `ack`, oldest first. `last` is the next ack; the backend forgets them
 * only then, so a lost reply is answered again. */
#define NVGPU_OP_OSDESC_REAP 11
#define NVGPU_OSDESC_REAP_MAX 256

#define NVGPU_OP_MAX_ARGS 6
#define NVGPU_OP_MAX_RES 4

struct nvgpu_host_op_req {
  __le32 op;
  __le32 nargs;
  __le64 args[NVGPU_OP_MAX_ARGS];
} __packed;

struct nvgpu_host_op_resp {
  __le32 nres;
  __le32 pad;
  __le64 res[NVGPU_OP_MAX_RES];
} __packed;

/* ── EVENT_DATA records (host → guest) ──
 *
 * The message is an nvgpu_msg_hdr whose req_id carries the payload length,
 * followed by records. A record never splits a struct drm_event.
 */
#define NVGPU_EV_DRM 1     /* cookie = backend handle; bytes = drm_event stream */
#define NVGPU_EV_FENCE 2   /* cookie = WATCH cookie; bytes = nvgpu_ev_fence     */
#define NVGPU_EV_READY 3   /* cookie = WATCH cookie (legacy watches: handle)     */
#define NVGPU_EV_HOTPLUG 4 /* cookie = card index; bytes = nvgpu_ev_hotplug      */

#define NVGPU_EVENT_BUF_SIZE 8192

struct nvgpu_ev_rec {
  __le32 kind;
  __le32 len; /* payload bytes that follow, before padding to 8 */
  __le64 cookie;
} __packed;

struct nvgpu_ev_fence {
  __le32 status; /* 1 signalled, <0 error (signed) */
  __le32 pad;
  /* When the host's fences signalled, host CLOCK_MONOTONIC ns (the latest
   * of a merge's), 0 unknown. An older backend sends the first 8 bytes only;
   * the record's len says, and the rest reads as 0. */
  __le64 timestamp_ns;
} __packed;

#define NVGPU_EV_HOTPLUG_F_HOTPLUG (1u << 0)
#define NVGPU_EV_HOTPLUG_F_LEASE (1u << 1)

struct nvgpu_ev_hotplug {
  __le32 flags;
  __le32 pad;
} __packed;

/*
 * ── GET_SYS_FILES section 3: card nodes ──
 * Sent in every mode (older backends: compositor-VM mode only). Openable only
 * with NVGPU_BCAP_KMS_CARD; otherwise they only say how the host numbers its
 * cards (the Wayland devmap).
 */
struct nvgpu_card_record {
  __le32 name_len;
  __le32 major;
  __le32 minor;
  __le32 render_index; /* the DRI record this card belongs to */
  /* followed by name_len bytes of name */
} __packed;

/* ── Wayland channel frames (WL_SEND / WL_RECV) ── */
struct nvgpu_wl_recv_req {
  __le32 max_bytes;
  __le32 max_desc;
} __packed;

struct nvgpu_wl_send_resp {
  __le32 accepted;
  __le32 backlog;
} __packed;

static_assert(sizeof(struct nvgpu_msg_hdr) == 16, "msg hdr");
static_assert(sizeof(struct nvgpu_mmap_resp) == 16 + 24, "mmap resp");
static_assert(offsetof(struct nvgpu_mmap_resp, caching) == 16 + 20,
              "mmap resp caching");
static_assert(sizeof(struct nvgpu_hello_req) == 16, "hello req");
static_assert(sizeof(struct nvgpu_hello_resp) == 32, "hello resp");
static_assert(sizeof(struct nvgpu_time_sync_resp) == 8, "time sync");
static_assert(sizeof(struct nvgpu_time_sync_resp2) == 32, "time sync 2");
static_assert(sizeof(struct nvgpu_i2_req) == 32, "i2 req");
static_assert(sizeof(struct nvgpu_i2_fd_in) == 16, "i2 fd in");
static_assert(sizeof(struct nvgpu_i2_gem_in) == 16, "i2 gem in");
static_assert(sizeof(struct nvgpu_i2_dyn) == 16, "i2 dyn");
static_assert(sizeof(struct nvgpu_i2_resp) == 32, "i2 resp");
static_assert(sizeof(struct nvgpu_i2_fd_out) == 16, "i2 fd out");
static_assert(sizeof(struct nvgpu_i2_gem_out) == 24, "i2 gem out");
static_assert(sizeof(struct nvgpu_watch_req) == 16, "watch");
static_assert(sizeof(struct nvgpu_unwatch_req) == 8, "unwatch");
static_assert(sizeof(struct nvgpu_host_op_req) == 56, "host op req");
static_assert(sizeof(struct nvgpu_host_op_resp) == 40, "host op resp");
static_assert(sizeof(struct nvgpu_osdesc_hdr) == 8, "osdesc hdr");
static_assert(sizeof(struct nvgpu_osdesc_run) == 16, "osdesc run");
static_assert(sizeof(struct nvgpu_proc_id) == 16, "proc id");
static_assert(sizeof(struct nvgpu_ev_rec) == 16, "ev rec");
static_assert(sizeof(struct nvgpu_ev_fence) == 16, "ev fence");
static_assert(sizeof(struct nvgpu_ev_hotplug) == 8, "ev hotplug");
static_assert(sizeof(struct nvgpu_card_record) == 16, "card record");

#endif /* NVGPU_WIRE_H */
