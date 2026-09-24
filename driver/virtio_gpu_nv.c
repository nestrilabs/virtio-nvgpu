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
 * Place in: drivers/virtio/virtio_gpu_nv.c (libkrunfw tree)
 */

#include <drm/drm.h>
#include <linux/cdev.h>
#include <linux/completion.h>
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
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
#include <linux/scatterlist.h>
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

/*
 * module_kset lives in kernel/module/sysfs.c and is NOT exported to modules,
 * so it can only be named directly in an in-tree build.
 */
#ifndef MODULE
extern struct kset *module_kset;
#endif

/* ───────── virtio device identity ───────── */

#define VIRTIO_ID_GPU_NV 45

/* Feature bits */
#define VIRTIO_GPU_NV_F_UVM 0
#define VIRTIO_GPU_NV_F_ENCODE 1
#define VIRTIO_GPU_NV_F_GRAPHICS 2

/* ───────── NVIDIA device node numbers ───────── */

#define NV_MAJOR 195
#define NV_CTL_MINOR 255
#define NV_UVM_MAJOR 237
#define NV_CAPS_MAJOR 240

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
#define NV_MODESET_MINOR 254
#define NVGPU_DEV_MODESET 258

/* capability bits */
#define NVGPU_CAP_COMPUTE (1 << 0)
#define NVGPU_CAP_GRAPHICS (1 << 1)
#define NVGPU_CAP_VIDEO (1 << 2)
#define NVGPU_CAP_UTILITY (1 << 3)

/* NVIDIA ioctl numbers that require nested-pointer marshalling */
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_ALLOC 0x2b
/* UVM_INITIALIZE ioctl nr */
#define UVM_INITIALIZE_NR 0x30

#define NVGPU_DEV_DRI_BASE 512

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

/*
 * RM control commands that name an open file by descriptor inside their
 * parameters. The export form carries it after the 16-byte object it names.
 */
#define NVGPU_RM_EXPORT_OBJECT_TO_FD 0x00003d05
#define NVGPU_RM_IMPORT_OBJECT_FROM_FD 0x00003d06
#define NVGPU_RM_EXPORT_FD_OFFSET 16

/* Largest second-level buffer we will carry for one call. */
#define NVGPU_DEEP_MAX (64 * 1024)

/*
 * The largest nvidia-drm GEM parameter struct this driver forwards, and the
 * largest NVKMS block one of them may point at. The first is a stack buffer's
 * size, so it is small on purpose and BUILD_BUG_ON'd against the descriptors
 * that use it; the second only has to refuse a length field that is garbage,
 * since a real NVKMS memory-import block is a few hundred bytes.
 */
#define NVGPU_GEM_OUTER_MAX 32
#define NVGPU_GEM_NESTED_MAX (64 * 1024)

/* Event classes whose allocation parameters name a file of the caller's.
 * NV0005_ALLOC_PARAMETERS is {hParentClient, hSrcResource, hClass,
 * notifyIndex, data}, with `data` 8-byte aligned at 16. */
#define NVGPU_CLASS_EVENT 0x05
#define NVGPU_CLASS_EVENT_OS_EVENT 0x79
#define NVGPU_NV0005_DATA_OFFSET 16

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

struct nvgpu_device;

/* The shared memory region device memory is placed in, id 1. */
#define NVGPU_SHM_ID 1

/*
 * Which capability bits GET_DEV_INFO claims. Parameters rather than constants
 * because what the ICD asks for next depends on them, and the cost of being
 * wrong is a device that will not initialise -- cheaper to sweep than to
 * rebuild. Both default off: the ioctls behind them are not forwarded.
 */
/*
 * supports_alloc is on by default now, because the ioctls behind it work: a
 * Wayland client presents and 616 frames were encoded and decoded clean with
 * it set. It stays a parameter so it can be turned off to tell a GEM problem
 * from everything else in one boot.
 */
static int nvgpu_claim_alloc = 1;
module_param_named(claim_alloc, nvgpu_claim_alloc, int, 0444);
MODULE_PARM_DESC(claim_alloc, "GET_DEV_INFO reports supports_alloc");
/*
 * supports_sync_fd stays off: PRIME_FENCE_CONTEXT_CREATE and
 * GEM_PRIME_FENCE_ATTACH (0x05, 0x06) are still not forwarded. Nothing has
 * asked for them -- no unhandled-ioctl line names either -- so the encode path
 * does not need them, and claiming a capability nothing serves is what rung 5
 * cost us.
 */
static int nvgpu_claim_sync_fd;
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

/* struct drm_nvidia_get_dev_info_params is nine u32s. */
#define NVGPU_DEV_INFO_WORDS 9
/* name_len, major, minor, slot_index, then the dev_info words. */
#define NVGPU_DRI_RECORD_BYTES (16 + 4 * NVGPU_DEV_INFO_WORDS)

struct nvgpu_dri_dev {
  char name[32];
  u32 major;
  u32 minor;
  /* Which GPU slot this node hangs off. Ours, not NVIDIA's -- it is matched
   * against the GPU's minor, and is not the gpu_id GET_DEV_INFO reports. */
  u32 slot_index;
  /* GET_DEV_INFO as the host's own node answered it. Passed through rather
   * than reconstructed here: the gpu_id in it is what the ICD matches a DRM
   * node to an RM device by, and the page-kind and sector-layout fields are
   * per-architecture and were previously hardcoded for Ampere. */
  u32 dev_info[NVGPU_DEV_INFO_WORDS];
  struct cdev cdev;
  /* The registered DRM device, which owns the node and its sysfs tree. */
  struct drm_device *drm;
  bool registered;
  u32 index;
  struct nvgpu_device *dev;
  /* sysfs drm tree under the PCI device — required by Vulkan ICD */
  struct kobject *drm_kobj;      /* .../pci_addr/drm          */
  struct kobject *drm_node_kobj; /* .../pci_addr/drm/<name>   */
};

struct nvgpu_numa_attr {
  struct kobj_attribute kattr;
  struct nvgpu_device *dev;
  char *(*get_buf)(struct nvgpu_device *);
};

/* ── Fake PCI device state ── */
struct nvgpu_pci_slot {
  char pci_addr[16]; /* "0000:08:00.0"      */
  /*
   * Raw config space, the full extended 4 KiB of it and not the first 256
   * bytes.
   *
   * PCIe puts the extended capabilities above 256, and NVIDIA's userspace
   * reads them: with a 256-byte window nvidia-smi reports the PCIe link width
   * as an error while every neighbouring field is right, because the
   * capability it comes from is past the end of what this serves. The host
   * sends all 4096 bytes; there is no reason to keep only the first
   * sixteenth.
   */
  u8 config[4096];   /* raw config space    */
  bool config_valid;
  u16 domain;
  u8 bus_nr;
  u8 slot;
  u8 func;
};

/*
 * The PCI core reads a bus's sysdata as the architecture's own type. On x86
 * that is `struct pci_sysdata`, and the fields below have to line up with the
 * front of it:
 *
 *     struct pci_sysdata { int domain; int node; ... };
 *
 * `domain` was mirrored here from the start, for pci_domain_nr(). `node` was
 * not, and everything after `domain` in this struct was therefore read as the
 * bus's NUMA node -- that is, the first four bytes of the PCI address string,
 * "0000", or 0x30303030. It went unnoticed because it is only ever read under
 * CONFIG_NUMA, which the guest kernel did not have; turn it on and the first
 * allocation the DRM core makes against this device oopses in ___slab_alloc,
 * indexing a node array a billion entries past its end.
 *
 * Mirrored rather than embedded so the struct stays buildable where
 * `struct pci_sysdata` is not the arch's sysdata type; the layout is what
 * matters, and a wrong one is silent.
 */
struct nvgpu_pci_root {
  int domain; /* MUST be first — x86 pci_domain_nr()
               * reads domain from sysdata offset 0 */
  int node;   /* MUST be second — x86 pcibus_to_node()
               * reads the NUMA node from sysdata offset 4 */
  struct nvgpu_pci_slot slot;
  struct nvgpu_device *nvdev; /* back pointer        */
  struct pci_host_bridge *bridge;
  struct pci_dev *pdev; /* first (only) device on this bus */
  bool registered;
};

struct nvgpu_device {
  /*
   * Where the VMM placed the window, read out of this device's own shared
   * memory region. Zero-length when the VMM offers none, in which case device
   * memory can be mapped on the host but never reached from here.
   */
  struct virtio_shm_region window;
  struct virtio_device *vdev;
  struct virtqueue *ctrl_vq;
  struct virtqueue *event_vq;

  /* Character device registration */
  struct cdev cdev_gpu[248]; /* /dev/nvidia0 … nvidia247 */
  struct cdev cdev_ctl;      /* /dev/nvidiactl            */
  struct cdev cdev_uvm;      /* /dev/nvidia-uvm           */
  dev_t uvm_devno;           /* dynamic major for UVM     */
  struct cdev cdev_caps;     /* /dev/nvidia-caps */
  dev_t caps_devno;          /* dynamic major for nvidia-caps */
  struct cdev cdev_modeset;  /* /dev/nvidia-modeset */
  dev_t modeset_devno;

  /* Config read from VMM */
  char driver_version[32];
  u32 num_gpus;
  u32 caps;

  /* Serialise virtqueue access */
  struct mutex vq_lock;

  /* Completion for synchronous request */
  struct completion req_done;
  void *resp_buf;
  int resp_len;

  /* GPU slots read from config space at probe */
  struct virtio_gpu_nv_gpu_slot gpu_slots[8];

  /* FD translation table received from backend */
  struct nvgpu_fd_translation_entry fd_translations[16];
  u32 num_fd_translations;

  /* Every open descriptor, so an event naming a handle can find its file. */
  struct list_head fds;
  spinlock_t fds_lock;
  struct nvgpu_event_buf *event_bufs; /* defined with the event queue below */

  /* ── DRI device nodes ── */
#define NVGPU_MAX_DRI_DEVS 8
  struct nvgpu_dri_dev dri_devs[NVGPU_MAX_DRI_DEVS];
  int num_dri_devs;

  /* ── PCI sysfs fake hierarchy ── */
  struct kobject *pci_bus_kobj;     /* /sys/bus/pci              */
  struct kobject *pci_devices_kobj; /* /sys/bus/pci/devices      */

#define NVGPU_MAX_PCI_SLOTS 8
  struct nvgpu_pci_root pci_roots[NVGPU_MAX_PCI_SLOTS];
  int num_pci_roots;
};

/*
 * Per-open-fd state.
 * Every open("/dev/nvidia*") creates one nvgpu_fd.
 * The VMM keeps a matching host FD identified by handle.
 */
struct nvgpu_fd {
  struct nvgpu_device *dev;
  u32 handle;      /* VMM-assigned handle from OPEN response */
  u32 device_type; /* NVGPU_DEV_*                            */
  /*
   * Waiting for the GPU.
   *
   * NVIDIA's user-mode driver blocks on an RM event by polling the descriptor
   * the event is delivered on. The interrupt is the host's and so is the
   * descriptor that becomes readable, so the host tells us on the event queue
   * and this is where that lands: `pending` is set, `wq` is woken, and a
   * waiter in nvgpu_poll() returns.
   *
   * Before this existed there was no `.poll` at all, and a file_operations
   * with a NULL `.poll` is reported ready by the VFS every single time. The
   * driver's wait returned instantly, forever, so it spun -- a whole core per
   * guest at 100 frames a second.
   */
  wait_queue_head_t wq;
  atomic_t pending;
  struct list_head node; /* dev->fds, for finding this by handle */
  /* Answer to GET_DRM_FILE_UNIQUE_ID, assigned on first ask. Zero means
   * "not yet asked", which is why the counter starts at one. */
  u64 drm_unique_id;
};

/* Posted on the event queue for the host to fill; see NVGPU_EVENT_BUFS. */
struct nvgpu_event_buf;

/* class for device_create() */
static struct class *nvgpu_class;

/* ───────── nvidia-drm stub — no DRM subsystem headers needed ───────── */

static long nvgpu_ioctl(struct file *filp, unsigned int cmd, unsigned long arg);
static long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                           unsigned long arg);
static long nvgpu_ioctl_simple(struct nvgpu_fd *nfd, unsigned int cmd,
                               void __user *uarg, unsigned int sz);

/*
 * A GEM parameter struct that carries a userspace pointer, described well
 * enough to forward: where the pointer sits and where the length beside it
 * does. Both are u64. A struct with no pointer is not described here at all --
 * it goes through nvgpu_ioctl_simple(), which copies the whole thing.
 */
struct nvgpu_gem_nested_desc {
  u32 size;        /* sizeof the parameter struct */
  u32 ptr_offset;  /* byte offset of the u64 userspace pointer */
  u32 size_offset; /* byte offset of the u64 length beside it */
  /*
   * Byte offset, inside the *nested* block, of an `int` file descriptor, or
   * NVGPU_GEM_NO_FD. NVKMS names the memory to import or export by an open
   * file rather than by a handle, and a descriptor number means nothing in the
   * backend's process -- forwarded verbatim it picks out whatever that process
   * happens to have open at that number, which is how this arrived as an
   * NVKMS import that simply refused.
   */
  s32 fd_offset;
  /*
   * Where the GEM handle sits in the outer struct, and which way it travels.
   * `handle_is_out` means the host creates the object and we stand a proxy in
   * front of it before the caller ever sees a number; otherwise the caller
   * names a proxy and we translate it to the host's handle on the way in.
   */
  s32 handle_offset;
  bool handle_is_out;
  /* Offset of the u64 buffer size, used to size the proxy. -1 if none. */
  s32 size_field_offset;
};

#define NVGPU_GEM_NO_FD (-1)
#define NVGPU_GEM_NO_FIELD (-1)

static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg,
                                       const struct nvgpu_gem_nested_desc *d);

/*
 * ───────── GEM objects, proxied ─────────
 *
 * The host owns the memory and the object that names it. The guest needs an
 * object too, because the ioctls that make a swapchain usable are *core* DRM:
 * PRIME_HANDLE_TO_FD, PRIME_FD_TO_HANDLE and GEM_CLOSE are served by
 * drm_ioctl() out of this node's own object space, and that space was empty.
 * Forwarding those to the host cannot work either -- the dma-buf the host
 * would hand back is a file in the *backend's* process, and a Wayland client
 * has to pass its buffer to a compositor as a descriptor in this guest.
 *
 * So each host GEM object gets a guest object standing in front of it, and the
 * core's own PRIME and handle machinery does the work. Only three things cross
 * the boundary: creating the host object, closing it, and the GEM ioctls that
 * act on it -- each translated from the guest handle to the host one.
 *
 * `owner_handle` is the backend handle of the drm_file the host object belongs
 * to, not of whichever file is asking now. A GEM handle is per drm_file on the
 * host, so an op on this object has to go back to the file that created it,
 * whatever guest process is holding the proxy. That is what lets a compositor
 * PRIME-import a client's buffer and have the forwarded ops still land on the
 * right host object.
 */

/* drm_nvidia_gem_object_type, as GEM_IDENTIFY_OBJECT reports it. */
#define NVGPU_GEM_OBJECT_NVKMS 0
#define NVGPU_GEM_OBJECT_DMABUF 1
#define NVGPU_GEM_OBJECT_USERMEMORY 2
#define NVGPU_GEM_OBJECT_UNKNOWN 0x7fffffff

struct nvgpu_gem_object {
  struct drm_gem_object base;
  struct nvgpu_device *dev;
  u32 owner_handle; /* backend handle of the drm_file owning the host object */
  u32 host_handle;  /* the GEM handle in the host's drm_file */
  u32 obj_type;     /* what GEM_IDENTIFY_OBJECT answers */
  /*
   * Where the host's memory for this object sits in the shared window, and
   * whether it has been put there yet. Placed on the first map and not before:
   * most buffers are only ever touched by the GPU, and a placement costs a
   * round trip and a slice of a window that is finite.
   *
   * These are the buffer. Everything that hands the memory out -- the node's
   * mmap, the dma-buf's, its vmap, and the addresses an importer gets -- comes
   * from this one placement, so they all name the same bytes on the host.
   */
  struct mutex map_lock;
  u64 window_off;
  u32 mapping_id; /* what the backend takes back in MUNMAP */
  bool window_valid;
};

#define to_nvgpu_gem(o) container_of(o, struct nvgpu_gem_object, base)

/* Defined below; named here because the ioctls that test it come first. */
static const struct drm_gem_object_funcs nvgpu_gem_funcs;

static int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *nfd,
                                  u32 host_handle, size_t size,
                                  u32 *guest_handle);
static int nvgpu_gem_to_host(struct drm_file *file, u32 guest_handle,
                             u32 *host_handle, u32 *owner_handle);
static long nvgpu_gem_identify(struct drm_file *file, void __user *uarg);
static long nvgpu_ioctl_flat_h(struct nvgpu_device *dev, u32 handle,
                               unsigned int cmd, void *kbuf, u32 sz);

/* struct drm_gem_close — UAPI, include/uapi/drm/drm.h */
struct nvgpu_drm_gem_close {
  __u32 handle;
  __u32 pad;
};

/*
 * The nvidia-drm ioctls, answered here rather than through a drm_ioctl_desc
 * table. drm_ioctl() serves the core ones (VERSION and friends) and answers
 * -EINVAL for anything in the driver range, since this driver registers no
 * table of its own; nvgpu_drm_unlocked_ioctl() takes that range first and
 * leaves the rest to the core.
 *
 * All types used below are stable UAPI structs; we define only what we use.
 */

/* _IOC_TYPE byte for DRM ioctls */
#define DRM_IOCTL_BASE 'd'
#define DRM_COMMAND_BASE 0x40

/* DRM_NVIDIA_* are offsets from DRM_COMMAND_BASE */
#define DRM_NVIDIA_GET_DEV_INFO 0x03     /* abs nr 0x43 */
#define DRM_NVIDIA_FENCE_SUPPORTED 0x04  /* abs nr 0x44 */
#define DRM_NVIDIA_DMABUF_SUPPORTED 0x0f /* abs nr 0x4f */
#define DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID 0x18 /* abs nr 0x58 */

/*
 * The GEM ioctls, which are what a swapchain is made of: allocate or import
 * memory, give it a fake mmap offset, hand it out as a dma-buf. They are
 * forwarded to the host's render node rather than answered here -- the memory
 * is the host's and so is the object that names it.
 *
 * Nothing translates the handles in these structs. A GEM handle is per
 * drm_file, and each open of this node holds exactly one open of the host's
 * node (nvgpu_drm_open), so the handle the host issues is already scoped to
 * the file that will use it and means the same thing on both sides.
 */
#define DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY 0x01  /* abs nr 0x41 */
#define DRM_NVIDIA_GEM_MAP_OFFSET 0x0a           /* abs nr 0x4a */
#define DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY 0x0b   /* abs nr 0x4b */
#define DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY 0x0d /* abs nr 0x4d */
#define DRM_NVIDIA_GEM_IDENTIFY_OBJECT 0x0e      /* abs nr 0x4e */

/*
 * struct drm_nvidia_gem_import_nvkms_memory_params:
 *   u64 mem_size; u64 nvkms_params_ptr; u64 nvkms_params_size;
 *   u32 handle; u32 __pad;
 */
static const struct nvgpu_gem_nested_desc nvgpu_gem_import_nvkms = {
    .size = 32,
    .ptr_offset = 8,
    .size_offset = 16,
    /* struct NvKmsKapiPrivImportMemoryParams { int memFd; ... } */
    .fd_offset = 0,
    .handle_offset = 24,
    .handle_is_out = true,
    .size_field_offset = 0, /* mem_size */
};

/*
 * struct drm_nvidia_gem_export_dmabuf_memory_params:
 *   u32 handle; u32 __pad; u64 nvkms_params_ptr; u64 nvkms_params_size;
 */
static const struct nvgpu_gem_nested_desc nvgpu_gem_export_dmabuf = {
    .size = 24,
    .ptr_offset = 8,
    .size_offset = 16,
    /* struct NvKmsKapiPrivExportMemoryParams { int memFd; } */
    .fd_offset = 0,
    .handle_offset = 0,
    .handle_is_out = false,
    .size_field_offset = NVGPU_GEM_NO_FIELD,
};

/*
 * struct drm_version — UAPI, stable since DRM was upstreamed.
 * Copy of include/uapi/drm/drm.h:struct drm_version so we need
 * no DRM kernel headers.
 */
struct nvgpu_drm_version {
  int version_major;
  int version_minor;
  int version_patchlevel;
  size_t name_len;
  char __user *name;
  size_t date_len;
  char __user *date;
  size_t desc_len;
  char __user *desc;
};

struct drm_nvidia_get_dev_info_params {
  __u32 gpu_id;
  __u32 mig_device;
  __u32 primary_index;
  __u32 supports_alloc;
  __u32 generic_page_kind;
  __u32 page_kind_generation;
  __u32 sector_layout;
  __u32 supports_sync_fd;
  __u32 supports_semsurf;
} __packed;

/*
 * nvgpu_drm_handle_ioctl — handle all DRM-layer ioctls on our /dev/dri/..
 * nodes.
 *
 * DRM_IOCTL_VERSION  (nr=0x00) — core ioctl, returns name="nvidia-drm"
 * GET_DEV_INFO       (nr=0x43) — driver ioctl, returns gpu_id etc.
 * FENCE_SUPPORTED    (nr=0x44) — driver ioctl, returns 0
 * DMABUF_SUPPORTED   (nr=0x4f) — driver ioctl, returns 0
 *
 * Everything else → -ENOTTY.
 */
static long nvgpu_drm_handle_ioctl(struct nvgpu_fd *nfd,
                                   struct nvgpu_dri_dev *dri,
                                   struct drm_file *file, unsigned int cmd,
                                   unsigned long arg) {
  unsigned int nr = _IOC_NR(cmd);
  void __user *uarg = (void __user *)arg;

  /* ── DRM_IOCTL_VERSION (type='d', nr=0x00) ── */
  if (nr == 0x00) {
    struct nvgpu_drm_version v;

    if (copy_from_user(&v, uarg, sizeof(v)))
      return -EFAULT;

    v.version_major = 0;
    v.version_minor = 1;
    v.version_patchlevel = 0;

#define FILL_DRM_STR(field, str)                                               \
  do {                                                                         \
    const char *_s = (str);                                                    \
    size_t _sl = strlen(_s);                                                   \
    if (v.field##_len >= _sl && v.field)                                       \
      if (copy_to_user(v.field, _s, _sl))                                      \
        return -EFAULT;                                                        \
    v.field##_len = _sl;                                                       \
  } while (0)

    FILL_DRM_STR(name, "nvidia-drm");
    FILL_DRM_STR(date, "20240101");
    FILL_DRM_STR(desc, "NVIDIA DRM stub");
#undef FILL_DRM_STR

    if (copy_to_user(uarg, &v, sizeof(v)))
      return -EFAULT;

    return 0;
  }

  /* ── Driver ioctls: DRM_COMMAND_BASE .. DRM_COMMAND_END ── */
  if (nr < DRM_COMMAND_BASE || nr >= DRM_COMMAND_END)
    return -ENOTTY;

  switch (nr - DRM_COMMAND_BASE) {

  case DRM_NVIDIA_GET_DEV_INFO: {
    /*
     * Straight from the host's own node. These fields describe how the card
     * lays memory out, and the ICD matches a DRM node to an RM device by the
     * gpu_id among them, so none of them is ours to invent -- the constants
     * that used to be here reported gpu_id 0 where the host says 0x100, and a
     * page kind correct only on the two architectures the comment named.
     */
    u32 info[NVGPU_DEV_INFO_WORDS];

    BUILD_BUG_ON(sizeof(struct drm_nvidia_get_dev_info_params) !=
                 NVGPU_DEV_INFO_WORDS * sizeof(u32));

    memcpy(info, dri->dev_info, sizeof(info));

    /*
     * The three capability bits are the host's answer about the host's node,
     * and this node is not that node: it answers four ioctls and forwards
     * nothing else. Passing them through unchanged is a promise this stub
     * cannot keep -- with supports_semsurf set, the ICD asks for
     * SEMSURF_FENCE_CTX_CREATE (nr 0x54) on every device creation, gets
     * -ENOTTY, and fails the whole vkCreateDevice with
     * ERROR_INITIALIZATION_FAILED.
     *
     * Each stays zero until the ioctls behind it are forwarded:
     *
     *   supports_alloc     GEM_ALLOC_NVKMS_MEMORY, GEM_MAP_OFFSET,
     *                      GEM_EXPORT_DMABUF_MEMORY (0x0b, 0x0a, 0x0d)
     *   supports_sync_fd   PRIME_FENCE_CONTEXT_CREATE, GEM_PRIME_FENCE_ATTACH
     *                      (0x05, 0x06)
     *   supports_semsurf   SEMSURF_FENCE_CTX_CREATE and the three that follow
     *                      it (0x14..0x17)
     *
     * gpu_id, primary_index and the page-kind and sector-layout fields stay
     * as the host reported them: they describe the card, which is genuinely
     * the host's, and the ICD matches a DRM node to an RM device by gpu_id.
     */
    info[3] = nvgpu_claim_alloc;   /* supports_alloc */
    info[7] = nvgpu_claim_sync_fd; /* supports_sync_fd */
    info[8] = 0;                   /* supports_semsurf */

    if (copy_to_user(uarg, info, sizeof(info)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_GET_DRM_FILE_UNIQUE_ID: {
    /*
     * A number that tells one open of this node from another. The ICD asks
     * for it once a device has been created and uses it to recognise its own
     * file; nothing outside this guest ever sees it, so a counter is a real
     * answer rather than a stub, and it must not restart while the module is
     * loaded or two live files would claim the same id.
     */
    static atomic64_t next_unique_id = ATOMIC64_INIT(1);
    u64 id;

    if (!nfd->drm_unique_id)
      nfd->drm_unique_id = (u64)atomic64_inc_return(&next_unique_id);
    id = nfd->drm_unique_id;

    if (copy_to_user(uarg, &id, sizeof(id)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_FENCE_SUPPORTED:
    return 0; /* not supported, no payload */

  case DRM_NVIDIA_DMABUF_SUPPORTED:
    return 0; /* not supported, no payload */

  /*
   * ── GEM: forwarded to the host's render node ──
   *
   * These three are flat -- every field is a value -- so the whole struct
   * goes across and the answer comes back into it.
   */
  case DRM_NVIDIA_GEM_IDENTIFY_OBJECT:
    /* Answered from the proxy; see nvgpu_gem_identify(). */
    if (!file)
      return -ENOTTY;
    return nvgpu_gem_identify(file, uarg);

  case DRM_NVIDIA_GEM_MAP_OFFSET: {
    /*
     * u32 handle IN, u32 pad, u64 offset OUT -- answered here, not forwarded.
     *
     * The offset a caller gets has to be one it can mmap, and it will mmap
     * *this* node. The host's offset names a position in the host's node and
     * would land on whatever this node happens to have at that offset, which
     * is nothing. So the proxy gets an offset of its own, the core's mmap
     * finds it, and nvgpu_gem_object_mmap() maps the host's memory through the
     * shared window. The host's offset is still needed, but only inside the
     * driver, and it is fetched at placement time.
     */
    struct {
      __u32 handle;
      __u32 pad;
      __u64 offset;
    } p;
    struct drm_gem_object *obj;
    int ret;

    if (!file)
      return -ENOTTY;
    if (_IOC_SIZE(cmd) != sizeof(p))
      return -EINVAL;
    if (copy_from_user(&p, uarg, sizeof(p)))
      return -EFAULT;

    obj = drm_gem_object_lookup(file, p.handle);
    if (!obj)
      return -ENOENT;
    if (obj->funcs != &nvgpu_gem_funcs) {
      drm_gem_object_put(obj);
      return -ENOENT;
    }

    ret = drm_gem_create_mmap_offset(obj);
    if (!ret)
      p.offset = drm_vma_node_offset_addr(&obj->vma_node);
    drm_gem_object_put(obj);
    if (ret)
      return ret;

    if (copy_to_user(uarg, &p, sizeof(p)))
      return -EFAULT;
    return 0;
  }

  case DRM_NVIDIA_GEM_ALLOC_NVKMS_MEMORY: {
    /*
     * u32 handle OUT, u8 block_linear, u8 compressible, u16 pad,
     * u64 memory_size IN, u32 flags, u32 pad
     */
    struct {
      __u32 handle;
      __u8 block_linear;
      __u8 compressible;
      __u16 pad0;
      __u64 memory_size;
      __u32 flags;
      __u32 pad1;
    } p;
    u32 guest_handle;
    long ret;

    if (!file)
      return -ENOTTY;
    if (_IOC_SIZE(cmd) != sizeof(p))
      return -EINVAL;
    if (copy_from_user(&p, uarg, sizeof(p)))
      return -EFAULT;

    ret = nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, cmd, &p, sizeof(p));
    if (ret < 0)
      return ret;

    /* The host's handle never reaches userspace; a proxy stands in for it. */
    ret = nvgpu_gem_proxy_create(file, nfd, p.handle, p.memory_size,
                                 &guest_handle);
    if (ret) {
      struct nvgpu_drm_gem_close close = {.handle = p.handle};

      nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, DRM_IOCTL_GEM_CLOSE, &close,
                         sizeof(close));
      return ret;
    }

    p.handle = guest_handle;
    if (copy_to_user(uarg, &p, sizeof(p)))
      return -EFAULT;
    return 0;
  }

  /*
   * These two carry a pointer to an NVKMS parameter block. The guest's
   * address means nothing on the host, so the bytes travel alongside and the
   * backend gives them a host address before the call -- the same shape as
   * nvidia-modeset, which is why both go through one forwarder.
   */
  case DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY:
    if (!file)
      return -ENOTTY;
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, uarg,
                                      &nvgpu_gem_import_nvkms);

  case DRM_NVIDIA_GEM_EXPORT_DMABUF_MEMORY:
    if (!file)
      return -ENOTTY;
    return nvgpu_ioctl_drm_gem_nested(nfd, file, cmd, uarg,
                                      &nvgpu_gem_export_dmabuf);

  default:
    /*
     * Named rather than silently refused. An ioctl this stub does not answer
     * is the ICD asking for something the node cannot do yet, and -ENOTTY on
     * its own turns up much later as a device that would not initialise.
     */
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: unhandled nvidia-drm ioctl "
                         "nr=0x%02x (DRM_NVIDIA_%u) size=%u dir=%u\n",
                         nr, nr - DRM_COMMAND_BASE, _IOC_SIZE(cmd),
                         _IOC_DIR(cmd));
    return -ENOTTY;
  }
}

/*
 * nvgpu_dri_ioctl — combined ioctl for /dev/dri/.. nodes.
 *
 *   type 'd' → handled locally (DRM VERSION + nvidia-drm driver ioctls)
 *   type 'F' → proxied to host via virtqueue (NVIDIA RM ioctls)
 */
static long nvgpu_dri_ioctl(struct file *filp, unsigned int cmd,
                            unsigned long arg) {
  if (_IOC_TYPE(cmd) == DRM_IOCTL_BASE) {
    struct nvgpu_fd *nfd = filp->private_data;
    struct nvgpu_dri_dev *dri = NULL;
    int i;

    for (i = 0; i < nfd->dev->num_dri_devs; i++) {
      if ((u32)i == nfd->device_type - NVGPU_DEV_DRI_BASE) {
        dri = &nfd->dev->dri_devs[i];
        break;
      }
    }

    if (!dri)
      return -ENODEV;

    return nvgpu_drm_handle_ioctl(nfd, dri, NULL, cmd, arg);
  }

  /* NVIDIA-type and everything else → proxy to host */
  return nvgpu_ioctl(filp, cmd, arg);
}

/* ───────── Virtqueue communication ───────── */

/*
 * nvgpu_send_recv — submit one request to controlq and block until the VMM
 * returns the response.  Caller must supply pre-allocated resp buffer.
 */
static int nvgpu_send_recv(struct nvgpu_device *dev, void *req, int req_len,
                           void *resp, int resp_len) {
  struct scatterlist sg_out, sg_in;
  struct scatterlist *sgs[2] = {&sg_out, &sg_in};
  int ret;

  mutex_lock(&dev->vq_lock);

  reinit_completion(&dev->req_done);
  dev->resp_buf = resp;
  dev->resp_len = resp_len;

  sg_init_one(&sg_out, req, req_len);
  sg_init_one(&sg_in, resp, resp_len);

  ret = virtqueue_add_sgs(dev->ctrl_vq, sgs, 1, 1, resp, GFP_KERNEL);
  if (ret < 0) {
    mutex_unlock(&dev->vq_lock);
    return ret;
  }

  mutex_unlock(&dev->vq_lock);
  virtqueue_kick(dev->ctrl_vq);

  ret = wait_for_completion_killable_timeout(&dev->req_done, 10 * HZ);
  if (ret == 0) {
    return -ETIMEDOUT;
  }
  if (ret < 0) {
    return ret;
  }

  return 0;
}

/* Virtqueue callback: VMM has written the response buffer */
static void nvgpu_ctrl_vq_cb(struct virtqueue *vq) {
  struct nvgpu_device *dev = vq->vdev->priv;
  void *buf;
  unsigned int len;

  while ((buf = virtqueue_get_buf(vq, &len)) != NULL) {
    complete(&dev->req_done);
  }
}

/* ───────── Events from the host ───────── */

/*
 * Buffers posted on the event queue for the host to fill.
 *
 * The queue carries one message type and it is fixed-size, so the buffers are
 * allocated once at probe and handed straight back after each event. A guest
 * that posts none simply never hears about a readable descriptor, which is
 * what every build before this one did.
 */
#define NVGPU_EVENT_BUFS 16

struct nvgpu_event_buf {
  struct nvgpu_msg_hdr hdr;
};

static void nvgpu_event_post(struct nvgpu_device *dev,
                             struct nvgpu_event_buf *buf) {
  struct scatterlist sg;
  int ret;

  sg_init_one(&sg, buf, sizeof(*buf));
  ret = virtqueue_add_inbuf(dev->event_vq, &sg, 1, buf, GFP_ATOMIC);
  if (ret < 0)
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: event queue would not take a buffer: %d\n",
                         ret);
}

/*
 * The host says a descriptor has something to report. Wake whoever is waiting
 * on it.
 *
 * `pending` is a flag rather than a count: what the waiter does on waking is
 * ask the hardware's own semaphore, so two events and one event mean the same
 * thing to it. A wake with nothing behind it costs a wasted poll, and the host
 * re-sends while the descriptor stays readable, so a lost one costs a
 * millisecond rather than a hang.
 */
static void nvgpu_event_deliver(struct nvgpu_device *dev, u32 handle) {
  struct nvgpu_fd *nfd;
  unsigned long flags;

  spin_lock_irqsave(&dev->fds_lock, flags);
  list_for_each_entry(nfd, &dev->fds, node) {
    if (nfd->handle == handle) {
      atomic_set(&nfd->pending, 1);
      wake_up_interruptible(&nfd->wq);
      break;
    }
  }
  spin_unlock_irqrestore(&dev->fds_lock, flags);
}

static void nvgpu_event_vq_cb(struct virtqueue *vq) {
  struct nvgpu_device *dev = vq->vdev->priv;
  struct nvgpu_event_buf *buf;
  unsigned int len;

  while ((buf = virtqueue_get_buf(vq, &len)) != NULL) {
    if (len >= sizeof(buf->hdr) &&
        le32_to_cpu(buf->hdr.msg_type) == NVGPU_MSG_EVENT_READY)
      nvgpu_event_deliver(dev, le32_to_cpu(buf->hdr.handle));
    else if (len)
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: event queue carried msg_type %u\n",
                           len >= sizeof(buf->hdr)
                               ? le32_to_cpu(buf->hdr.msg_type)
                               : 0);
    nvgpu_event_post(dev, buf);
  }
  virtqueue_kick(dev->event_vq);
}

/*
 * poll() on a device descriptor.
 *
 * Reports nothing until the host says otherwise, which is the whole point:
 * without this the VFS reported every one of these descriptors as permanently
 * ready and the user-mode driver's wait never waited.
 */
static __poll_t nvgpu_poll(struct file *filp, struct poll_table_struct *wait) {
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
    return EPOLLIN | EPOLLRDNORM;
  return 0;
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
  req->hdr.padding = 0;
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

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  if (sz > 0 && resp->data_len && le32_to_cpu(resp->data_len) <= sz) {
    if (copy_to_user(uarg, resp_buf + sizeof(*resp),
                     le32_to_cpu(resp->data_len)))
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
static int nvgpu_handle_for_fd(int guest_fd, u32 *handle) {
  struct file *f;
  struct nvgpu_fd *other;

  if (guest_fd < 0)
    return -EBADF;

  f = fget(guest_fd);
  if (!f)
    return -EBADF;

  other = f->private_data;
  if (!other) {
    fput(f);
    return -EINVAL;
  }

  *handle = other->handle;
  fput(f);
  return 0;
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
  rw = (user_nested && nested_size > 0) ? nvgpu_find_v1v2_rewrite(ctl_cmd)
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
  req->hdr.padding = 0;
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

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(params))) {
    ret = -EFAULT;
    goto out;
  }

  if (user_nested && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));

    if (nested_fd >= 0 && copy_back >= nested_fd_offset + sizeof(u32))
      memcpy(resp_buf + sizeof(*resp) + sizeof(params) + nested_fd_offset,
             &nested_fd, sizeof(nested_fd));

    if (copy_to_user(user_nested, resp_buf + sizeof(*resp) + sizeof(params),
                     copy_back))
      ret = -EFAULT;
  }

  if (deep_len > 0 && le32_to_cpu(resp->deep_len) > 0) {
    u32 copy_back = min(deep_len, le32_to_cpu(resp->deep_len));
    if (copy_to_user((void __user *)deep_user_ptr,
                     resp_buf + sizeof(*resp) + sizeof(params) +
                         le32_to_cpu(resp->nested_len),
                     copy_back))
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
  req->hdr.padding = 0;
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
          struct file *ev_file = fget(event_fd);

          if (ev_file) {
            struct nvgpu_fd *ev_nfd = ev_file->private_data;

            if (ev_nfd) {
              u32 handle = ev_nfd->handle;

              memcpy(nested + NVGPU_NV0005_DATA_OFFSET, &handle, sizeof(handle));
            }
            fput(ev_file);
          }
        }
      }
    }
  }

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(params))) {
    ret = -EFAULT;
    goto out;
  }

  if (user_alloc && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));
    if (copy_to_user(user_alloc, resp_buf + sizeof(*resp) + sizeof(params),
                     copy_back))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * NOTE: The only subtlety worth noting: copy_to_user on the way back writes the
 * VMM handle value (not a host fd number) back into the guest's buffer. That's
 * fine — nvidia-smi doesn't read the payload back after REGISTER_FD, it only
 * checks the return code. If a future fd-carrying ioctl does need the response
 * payload, the VMM would need to translate back from host fd → handle before
 * returning.
 */
static long nvgpu_ioctl_translate_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                                     void __user *uarg, unsigned int sz,
                                     unsigned int payload_offset) {
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  void *req_buf = NULL, *resp_buf = NULL;
  struct file *other_file;
  struct nvgpu_fd *other_nfd;
  int guest_fd;
  u32 host_handle;
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
    other_file = fget(guest_fd);
    if (!other_file) {
      ret = -EBADF;
      goto out;
    }

    other_nfd = other_file->private_data;
    if (!other_nfd) {
      fput(other_file);
      ret = -EINVAL;
      goto out;
    }

    host_handle = other_nfd->handle;
    fput(other_file);

    /* Patch payload: replace raw guest fd with VMM handle */
    memcpy(req_buf + sizeof(*req) + payload_offset, &host_handle,
           sizeof(host_handle));
  }

  /* Build request header */
  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->hdr.status = 0;
  req->hdr.padding = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sz);
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* Write the (possibly modified) payload back to userspace */
  if (ret == 0 && le32_to_cpu(resp->data_len) > 0) {
    if (copy_to_user(uarg, resp_buf + sizeof(*resp),
                     le32_to_cpu(resp->data_len)))
      ret = -EFAULT;
  }

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

static const struct nvgpu_fd_translation_entry *
nvgpu_find_fd_translation(struct nvgpu_device *dev, unsigned int nr) {
  u32 i;
  for (i = 0; i < dev->num_fd_translations; i++)
    if (le32_to_cpu(dev->fd_translations[i].nr) == nr)
      return &dev->fd_translations[i];
  return NULL;
}

/* Main ioctl dispatcher */
/*
 * Split from nvgpu_ioctl so a DRM node can reach it. On a real DRM node
 * filp->private_data is a `struct drm_file *`, and ours hangs off its
 * driver_priv -- so the caller supplies the fd rather than this deriving it.
 */
static long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd,
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

  fdt = nvgpu_find_fd_translation(nfd->dev, nr);
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

static void nvgpu_vma_close(struct vm_area_struct *vma) {
  struct nvgpu_fd *nfd = vma->vm_file->private_data;
  u32 mapping_id = (u32)(unsigned long)vma->vm_private_data;
  struct nvgpu_munmap_req *req;
  struct nvgpu_munmap_resp *resp;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (req && resp) {
    req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MUNMAP);
    req->hdr.handle = cpu_to_le32(nfd->handle);
    req->mapping_id = cpu_to_le32(mapping_id);
    nvgpu_send_recv(nfd->dev, req, sizeof(*req), resp, sizeof(*resp));
  }
  kfree(req);
  kfree(resp);
}

static const struct vm_operations_struct nvgpu_vm_ops = {
    .close = nvgpu_vma_close,
};

static int nvgpu_mmap(struct file *filp, struct vm_area_struct *vma) {
  struct nvgpu_fd *nfd = filp->private_data;
  u64 size = vma->vm_end - vma->vm_start;
  u64 offset = (u64)vma->vm_pgoff << PAGE_SHIFT;
  u64 window_off;
  struct nvgpu_mmap_req *req;
  struct nvgpu_mmap_resp *resp;
  int ret;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out;
  }

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MMAP);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->size = cpu_to_le64(size);
  req->offset = cpu_to_le64(offset);
  req->prot = cpu_to_le32((vma->vm_flags & VM_WRITE) ? 3 : 1);

  ret = nvgpu_send_recv(nfd->dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0)
    goto out;

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto out;
  }

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

  vma->vm_ops = &nvgpu_vm_ops;
  vma->vm_private_data = (void *)(unsigned long)le32_to_cpu(resp->mapping_id);

out:
  kfree(req);
  kfree(resp);
  return ret;
}

/* ───────── open / release ───────── */

/* Make a new descriptor waitable, and findable by the handle an event names. */
static void nvgpu_fd_register(struct nvgpu_device *dev, struct nvgpu_fd *nfd) {
  unsigned long flags;

  init_waitqueue_head(&nfd->wq);
  atomic_set(&nfd->pending, 0);
  spin_lock_irqsave(&dev->fds_lock, flags);
  list_add(&nfd->node, &dev->fds);
  spin_unlock_irqrestore(&dev->fds_lock, flags);
}

static void nvgpu_fd_unregister(struct nvgpu_device *dev,
                                struct nvgpu_fd *nfd) {
  unsigned long flags;

  spin_lock_irqsave(&dev->fds_lock, flags);
  list_del(&nfd->node);
  spin_unlock_irqrestore(&dev->fds_lock, flags);
  /* Anyone still in poll_wait() is woken so they can see the file go. */
  wake_up_interruptible_all(&nfd->wq);
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
  struct nvgpu_msg_hdr *req;
  struct nvgpu_msg_hdr *resp;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (req && resp) {
    req->msg_type = cpu_to_le32(NVGPU_MSG_CLOSE);
    req->handle = cpu_to_le32(nfd->handle);
    nvgpu_send_recv(nfd->dev, req, sizeof(*req), resp, sizeof(*resp));
  }
  kfree(req);
  kfree(resp);
  nvgpu_fd_unregister(nfd->dev, nfd);
  kfree(nfd);
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
  req->hdr.padding = 0;
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
  }

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /* Write back outer struct */
  if (copy_to_user(uarg, resp_buf + sizeof(*resp), sizeof(outer))) {
    ret = -EFAULT;
    goto out;
  }

  /* Write back nested params */
  if (user_nested && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));
    if (copy_to_user(user_nested, resp_buf + sizeof(*resp) + sizeof(outer),
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
static long nvgpu_ioctl_flat_h(struct nvgpu_device *dev, u32 handle,
                               unsigned int cmd, void *kbuf, u32 sz) {
  int req_total = sizeof(struct nvgpu_ioctl_req) + sz;
  int resp_max = sizeof(struct nvgpu_ioctl_resp) + sz;
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
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
  req->hdr.padding = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(sz);
  req->nested_offset = 0;
  req->nested_len = 0;
  req->deep_ptr_offset = 0;
  req->deep_len = 0;
  memcpy(req_buf + sizeof(*req), kbuf, sz);

  ret = nvgpu_send_recv(dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (long)(s32)le32_to_cpu((__le32)resp->hdr.status);
  if (le32_to_cpu(resp->data_len) >= sz)
    memcpy(kbuf, resp_buf + sizeof(*resp), sz);

out:
  kfree(req_buf);
  kfree(resp_buf);
  return ret;
}

/*
 * The last reference to a proxy is gone, so the host's object can go too.
 *
 * Forwarded on the owner's handle rather than the caller's: the host object
 * belongs to the drm_file that created it, and that file may well have closed
 * first -- a compositor can outlive the client whose buffer it imported.
 */
static void nvgpu_gem_free(struct drm_gem_object *obj) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);

  if (ng->dev && ng->host_handle) {
    struct nvgpu_drm_gem_close close = {.handle = ng->host_handle};

    nvgpu_ioctl_flat_h(ng->dev, ng->owner_handle, DRM_IOCTL_GEM_CLOSE, &close,
                       sizeof(close));
  }

  /*
   * Give the window space back. The window is a gigabyte and a swapchain is
   * megabytes at a time, so a guest that allocates and frees buffers for an
   * hour exhausts it otherwise -- and the failure lands on whichever mapping
   * happens to be next, not on the one that leaked.
   */
  if (ng->window_valid && ng->mapping_id) {
    struct nvgpu_munmap_req *req = kzalloc(sizeof(*req), GFP_KERNEL);
    struct nvgpu_munmap_resp *resp = kzalloc(sizeof(*resp), GFP_KERNEL);

    if (req && resp) {
      req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MUNMAP);
      req->hdr.handle = cpu_to_le32(ng->owner_handle);
      req->mapping_id = cpu_to_le32(ng->mapping_id);
      nvgpu_send_recv(ng->dev, req, sizeof(*req), resp, sizeof(*resp));
    }
    kfree(req);
    kfree(resp);
  }

  drm_gem_object_release(obj);
  kfree(ng);
}

/*
 * ───────── the host's memory, reached through the shared window ─────────
 *
 * The object's memory is the host's and cannot be copied here: a swapchain
 * image is written by the host's GPU. What can travel is the *address*. The
 * backend maps the host's DRM node at the object's own mmap offset and places
 * that mapping in the shared window -- the same window that already carries
 * every RM mapping -- and the guest reaches it at a physical address it works
 * out from its own PCI configuration.
 *
 * Until this existed, the sg_table handed to an importer described freshly
 * allocated zeroed pages, which got the import accepted and made anything that
 * read the buffer read zeroes.
 */

/* drm_nvidia_gem_map_offset_params, as the host's nvidia-drm defines it. */
struct nvgpu_gem_map_offset_params {
  __u32 handle;
  __u32 pad;
  __u64 offset;
};

#define NVGPU_IOCTL_GEM_MAP_OFFSET                                             \
  _IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_NVIDIA_GEM_MAP_OFFSET,          \
        struct nvgpu_gem_map_offset_params)

/*
 * Put this object's memory in the window, once.
 *
 * Two steps, both on the *owner's* handle: ask the host for the mmap offset of
 * its GEM object, then ask the backend to map the node there and place it. The
 * offset is the host's and never reaches guest userspace -- what a client gets
 * from GEM_MAP_OFFSET is an offset into this node, answered below.
 */
static int nvgpu_gem_place_in_window(struct nvgpu_gem_object *ng) {
  struct drm_gem_object *obj = &ng->base;
  struct nvgpu_gem_map_offset_params mo = {};
  struct nvgpu_mmap_req *req;
  struct nvgpu_mmap_resp *resp;
  u64 window_off;
  long ret;

  if (READ_ONCE(ng->window_valid))
    return 0;

  if (!ng->dev->window.len) {
    dev_warn_once(&ng->dev->vdev->dev,
                  "virtio-gpu-nv: no shared memory region, so a buffer's "
                  "memory cannot be reached from the guest\n");
    return -ENOTSUPP;
  }

  mutex_lock(&ng->map_lock);
  if (ng->window_valid) {
    mutex_unlock(&ng->map_lock);
    return 0;
  }

  mo.handle = ng->host_handle;
  ret = nvgpu_ioctl_flat_h(ng->dev, ng->owner_handle,
                           NVGPU_IOCTL_GEM_MAP_OFFSET, &mo, sizeof(mo));
  if (ret < 0) {
    dev_warn(&ng->dev->vdev->dev,
             "virtio-gpu-nv: the host would not give object %u an mmap "
             "offset: %ld\n",
             ng->host_handle, ret);
    goto out;
  }

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out_free;
  }

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_MMAP);
  req->hdr.handle = cpu_to_le32(ng->owner_handle);
  req->size = cpu_to_le64(obj->size);
  req->offset = cpu_to_le64(mo.offset);
  req->prot = cpu_to_le32(3); /* read-write: the host's GPU writes it */

  ret = nvgpu_send_recv(ng->dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0)
    goto out_free;
  ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
  if (ret < 0)
    goto out_free;

  window_off = le64_to_cpu(resp->guest_phys_addr);
  if (window_off + obj->size > ng->dev->window.len) {
    dev_warn(&ng->dev->vdev->dev,
             "virtio-gpu-nv: a buffer at %llu+%zu runs past the %llu-byte "
             "window\n",
             window_off, obj->size, ng->dev->window.len);
    ret = -ERANGE;
    goto out_free;
  }

  ng->window_off = window_off;
  ng->mapping_id = le32_to_cpu(resp->mapping_id);
  smp_wmb(); /* the offset is readable before the flag says it is */
  WRITE_ONCE(ng->window_valid, true);
  ret = 0;

out_free:
  kfree(req);
  kfree(resp);
out:
  mutex_unlock(&ng->map_lock);
  return (int)ret;
}

/* Guest physical address of the object's memory. Valid only after placement. */
static phys_addr_t nvgpu_gem_phys(struct nvgpu_gem_object *ng) {
  return (phys_addr_t)(ng->dev->window.addr + ng->window_off);
}

/*
 * Map the object into a process, for the node's own mmap and for an importer
 * that maps the dma-buf. Write-combining, because it is device memory across a
 * PCI window and a client writing a buffer streams it.
 */
static int nvgpu_gem_object_mmap(struct drm_gem_object *obj,
                                 struct vm_area_struct *vma) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  unsigned long size = vma->vm_end - vma->vm_start;
  unsigned long node_start = drm_vma_node_start(&obj->vma_node);
  u64 within;
  int ret;

  /*
   * vm_pgoff arrives absolute -- it still carries the object's fake offset,
   * whether it came through the node's mmap or the dma-buf's, which adds the
   * offset back before calling this. Neither subtracts it, so this does.
   */
  if (vma->vm_pgoff < node_start)
    return -EINVAL;
  within = (u64)(vma->vm_pgoff - node_start) << PAGE_SHIFT;
  if (within + size > obj->size)
    return -EINVAL;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ret;

  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = pgprot_writecombine(vm_get_page_prot(vma->vm_flags));

  return io_remap_pfn_range(vma, vma->vm_start,
                            (nvgpu_gem_phys(ng) + within) >> PAGE_SHIFT, size,
                            vma->vm_page_prot);
}

/*
 * A kernel mapping of the buffer, which is what a CPU consumer of a dma-buf
 * asks for. It is iomem -- there is no struct page behind a PCI window -- so
 * it goes into the iosys_map as such and a caller that cannot handle iomem
 * will say so rather than dereference it.
 */
static int nvgpu_gem_vmap(struct drm_gem_object *obj, struct iosys_map *map) {
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  void __iomem *vaddr;
  int ret;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ret;

  vaddr = ioremap_wc(nvgpu_gem_phys(ng), obj->size);
  if (!vaddr)
    return -ENOMEM;

  iosys_map_set_vaddr_iomem(map, vaddr);
  return 0;
}

static void nvgpu_gem_vunmap(struct drm_gem_object *obj,
                             struct iosys_map *map) {
  if (map->is_iomem && map->vaddr_iomem)
    iounmap(map->vaddr_iomem);
  iosys_map_clear(map);
}

/*
 * The dma-buf an importer gets.
 *
 * Not the core's exporter: drm_gem_map_dma_buf() asks for an sg_table of
 * struct pages and then dma-maps it, and there are no pages here. What an
 * importer needs is a DMA address for the window, which dma_map_resource()
 * gives for exactly this case -- memory that is addressable but not backed by
 * pages.
 */
static struct sg_table *nvgpu_dmabuf_map(struct dma_buf_attachment *attach,
                                         enum dma_data_direction dir) {
  struct drm_gem_object *obj = attach->dmabuf->priv;
  struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);
  struct sg_table *sgt;
  dma_addr_t addr;
  int ret;

  ret = nvgpu_gem_place_in_window(ng);
  if (ret)
    return ERR_PTR(ret);

  sgt = kzalloc(sizeof(*sgt), GFP_KERNEL);
  if (!sgt)
    return ERR_PTR(-ENOMEM);
  if (sg_alloc_table(sgt, 1, GFP_KERNEL)) {
    kfree(sgt);
    return ERR_PTR(-ENOMEM);
  }

  addr = dma_map_resource(attach->dev, nvgpu_gem_phys(ng), obj->size, dir,
                          DMA_ATTR_SKIP_CPU_SYNC);
  if (dma_mapping_error(attach->dev, addr)) {
    sg_free_table(sgt);
    kfree(sgt);
    return ERR_PTR(-EIO);
  }

  sg_dma_address(sgt->sgl) = addr;
  sg_dma_len(sgt->sgl) = obj->size;
  sgt->nents = 1;
  return sgt;
}

static void nvgpu_dmabuf_unmap(struct dma_buf_attachment *attach,
                               struct sg_table *sgt,
                               enum dma_data_direction dir) {
  dma_unmap_resource(attach->dev, sg_dma_address(sgt->sgl), sg_dma_len(sgt->sgl),
                     dir, DMA_ATTR_SKIP_CPU_SYNC);
  sg_free_table(sgt);
  kfree(sgt);
}

static const struct dma_buf_ops nvgpu_dmabuf_ops = {
    .map_dma_buf = nvgpu_dmabuf_map,
    .unmap_dma_buf = nvgpu_dmabuf_unmap,
    .release = drm_gem_dmabuf_release,
    .mmap = drm_gem_dmabuf_mmap,
    .vmap = drm_gem_dmabuf_vmap,
    .vunmap = drm_gem_dmabuf_vunmap,
};

static struct dma_buf *nvgpu_gem_export(struct drm_gem_object *obj, int flags) {
  DEFINE_DMA_BUF_EXPORT_INFO(exp_info);

  exp_info.ops = &nvgpu_dmabuf_ops;
  exp_info.size = obj->size;
  exp_info.flags = flags;
  exp_info.priv = obj;
  exp_info.resv = obj->resv;

  return drm_gem_dmabuf_export(obj->dev, &exp_info);
}

/*
 * Both mmap paths take a reference on the object for the vma and leave it to
 * the vma to give back, through the vm_ops they copy out of the object's funcs.
 * Without these the reference is never dropped: the object outlives its last
 * handle, its free never runs, and the window placement it holds is never
 * returned -- which showed up as a guest exhausting a gigabyte of window in 191
 * buffers it had already closed.
 */
static const struct vm_operations_struct nvgpu_gem_vm_ops = {
    .open = drm_gem_vm_open,
    .close = drm_gem_vm_close,
};

static const struct drm_gem_object_funcs nvgpu_gem_funcs = {
    .free = nvgpu_gem_free,
    .vm_ops = &nvgpu_gem_vm_ops,
    .export = nvgpu_gem_export,
    .mmap = nvgpu_gem_object_mmap,
    .vmap = nvgpu_gem_vmap,
    .vunmap = nvgpu_gem_vunmap,
};

/*
 * Stand a guest object in front of a host one and return the guest handle.
 *
 * `size` is what the core reports for the object and what it validates
 * framebuffer dimensions against, so it has to be at least the real buffer.
 * Page-aligned because the core rejects an object smaller than a page.
 */
static int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *nfd,
                                  u32 host_handle, size_t size,
                                  u32 *guest_handle) {
  struct nvgpu_gem_object *ng;
  int ret;

  size = PAGE_ALIGN(size);
  if (!size)
    size = PAGE_SIZE;

  ng = kzalloc(sizeof(*ng), GFP_KERNEL);
  if (!ng)
    return -ENOMEM;

  mutex_init(&ng->map_lock);
  drm_gem_private_object_init(file->minor->dev, &ng->base, size);
  ng->base.funcs = &nvgpu_gem_funcs;
  ng->dev = nfd->dev;
  ng->owner_handle = nfd->handle;
  ng->host_handle = host_handle;
  ng->obj_type = NVGPU_GEM_OBJECT_NVKMS;

  ret = drm_gem_handle_create(file, &ng->base, guest_handle);
  /* The handle holds the only reference now, or nothing does and it is freed. */
  drm_gem_object_put(&ng->base);
  return ret;
}

/*
 * Guest handle → the host handle it stands for, and the backend handle to
 * forward on. Fails for anything that is not one of our proxies rather than
 * forwarding a number that would name some unrelated host object.
 */
static int nvgpu_gem_to_host(struct drm_file *file, u32 guest_handle,
                             u32 *host_handle, u32 *owner_handle) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, guest_handle);
  int ret = -ENOENT;

  if (!obj)
    return -ENOENT;

  if (obj->funcs == &nvgpu_gem_funcs) {
    struct nvgpu_gem_object *ng = to_nvgpu_gem(obj);

    *host_handle = ng->host_handle;
    if (owner_handle)
      *owner_handle = ng->owner_handle;
    ret = 0;
  }

  drm_gem_object_put(obj);
  return ret;
}

/*
 * GEM_IDENTIFY_OBJECT, answered here.
 *
 * NVIDIA's userspace asks this straight after a PRIME import, to learn what
 * kind of object it just took. The proxy already knows, and the host's answer
 * would be about a handle the importing file does not hold. Unknown for
 * anything that is not ours, which is what the host driver reports too.
 */
static long nvgpu_gem_identify(struct drm_file *file, void __user *uarg) {
  struct {
    __u32 handle;
    __u32 object_type;
  } p;
  struct drm_gem_object *obj;

  if (copy_from_user(&p, uarg, sizeof(p)))
    return -EFAULT;

  obj = drm_gem_object_lookup(file, p.handle);
  if (obj && obj->funcs == &nvgpu_gem_funcs)
    p.object_type = to_nvgpu_gem(obj)->obj_type;
  else
    p.object_type = NVGPU_GEM_OBJECT_UNKNOWN;
  if (obj)
    drm_gem_object_put(obj);

  if (copy_to_user(uarg, &p, sizeof(p)))
    return -EFAULT;
  return 0;
}

/* ───────── nvidia-drm GEM ioctls with a nested parameter block ─────────
 *
 * GEM_IMPORT_NVKMS_MEMORY and GEM_EXPORT_DMABUF_MEMORY both hold a userspace
 * pointer to an NVKMS parameter block and the block's length beside it. That
 * is the same shape as nvidia-modeset's outer struct, so the wire format is
 * the same one: the outer struct, then the pointed-to bytes, with the backend
 * putting a host address in the pointer field before it makes the call and the
 * caller's own value back in it before it answers.
 *
 * The two differ from modeset only in where the pointer and the length sit and
 * in the length being a u64, which is why they are described by a
 * nvgpu_gem_nested_desc rather than hard-coded.
 */
static long nvgpu_ioctl_drm_gem_nested(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg,
                                       const struct nvgpu_gem_nested_desc *d) {
  u8 outer[NVGPU_GEM_OUTER_MAX];
  void __user *user_nested;
  u64 nested_size64;
  u32 nested_size;
  unsigned int sz = _IOC_SIZE(cmd);
  void *req_buf = NULL, *resp_buf = NULL;
  struct nvgpu_ioctl_req *req;
  struct nvgpu_ioctl_resp *resp;
  int req_total, resp_max, ret;
  u32 fwd_handle = nfd->handle;
  u32 caller_handle = 0;

  /*
   * The caller's struct has to be the one this descriptor describes, or the
   * pointer is not where we are about to read it from. Named rather than
   * clamped: a size that is not the expected one means the guest's userspace
   * driver and this table disagree about a UAPI struct, and reading a pointer
   * out of the wrong offset would forward a plausible-looking address.
   */
  if (sz != d->size) {
    dev_warn_ratelimited(&nfd->dev->vdev->dev,
                         "virtio-gpu-nv: nvidia-drm ioctl nr=0x%02x carries %u "
                         "bytes, this driver knows it as %u\n",
                         _IOC_NR(cmd), sz, d->size);
    return -EINVAL;
  }

  /*
   * `outer` is on the stack, so a descriptor larger than it would be a buffer
   * overflow rather than a wrong answer. Checked here rather than at the call
   * sites because the descriptors are data, and data is what gets edited.
   */
  if (d->size > NVGPU_GEM_OUTER_MAX ||
      d->ptr_offset + 8 > d->size || d->size_offset + 8 > d->size)
    return -EINVAL;

  if (copy_from_user(outer, uarg, d->size))
    return -EFAULT;

  user_nested = (void __user *)(unsigned long)get_unaligned_le64(
      outer + d->ptr_offset);
  nested_size64 = get_unaligned_le64(outer + d->size_offset);

  if (nested_size64 > NVGPU_GEM_NESTED_MAX)
    return -EINVAL;
  nested_size = (u32)nested_size64;

  /*
   * A handle the caller supplies names one of our proxies. Swap in the host's
   * handle and forward on the file that owns it, which is not necessarily the
   * one asking -- a compositor acting on a client's imported buffer is the
   * case that matters.
   */
  if (d->handle_offset != NVGPU_GEM_NO_FIELD && !d->handle_is_out) {
    u32 host_handle;

    caller_handle = get_unaligned_le32(outer + d->handle_offset);
    ret = nvgpu_gem_to_host(file, caller_handle, &host_handle, &fwd_handle);
    if (ret)
      return ret;
    put_unaligned_le32(host_handle, outer + d->handle_offset);
  }

  req_total = sizeof(*req) + d->size + nested_size;
  resp_max = sizeof(struct nvgpu_ioctl_resp) + d->size + nested_size;

  req_buf = kmalloc(req_total, GFP_KERNEL);
  resp_buf = kmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  req = (struct nvgpu_ioctl_req *)req_buf;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(fwd_handle);
  req->hdr.status = 0;
  req->hdr.padding = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(d->size);
  req->nested_offset = cpu_to_le32(d->size);
  req->nested_len = cpu_to_le32(nested_size);
  req->deep_ptr_offset = 0;
  req->deep_len = 0;

  memcpy(req_buf + sizeof(*req), outer, d->size);

  if (user_nested && nested_size > 0) {
    if (copy_from_user(req_buf + sizeof(*req) + d->size, user_nested,
                       nested_size)) {
      ret = -EFAULT;
      goto out;
    }

    /*
     * NVKMS names the memory by an open file. Our descriptor is not the
     * backend's, so it goes across as the handle the backend issued when we
     * opened that file, and the backend turns it back into one of its own
     * descriptors before making the call -- the same round trip
     * EXPORT_OBJECT_TO_FD already makes for RM.
     *
     * The guest's own value is put back by the backend before it answers, so
     * userspace reads back the descriptor it passed.
     */
    if (d->fd_offset != NVGPU_GEM_NO_FD &&
        nested_size >= (u32)d->fd_offset + 4) {
      void *nested = req_buf + sizeof(*req) + d->size;
      int guest_fd = (int)get_unaligned_le32(nested + d->fd_offset);
      u32 handle;

      ret = nvgpu_handle_for_fd(guest_fd, &handle);
      if (ret) {
        dev_warn_ratelimited(&nfd->dev->vdev->dev,
                             "virtio-gpu-nv: nvidia-drm ioctl nr=0x%02x names "
                             "fd %d, which is not one of our devices\n",
                             _IOC_NR(cmd), guest_fd);
        goto out;
      }
      put_unaligned_le32(handle, nested + d->fd_offset);
    }
  }

  ret = nvgpu_send_recv(nfd->dev, req_buf, req_total, resp_buf, resp_max);
  if (ret < 0)
    goto out;

  resp = (struct nvgpu_ioctl_resp *)resp_buf;
  ret = (int)(s32)le32_to_cpu((__le32)resp->hdr.status);

  /*
   * The outer struct carries the answer: GEM_IMPORT writes the new handle
   * into it. Copied back even when the call failed, because the host's
   * failure may have written a field too, and the caller reads what the host
   * driver would have left it.
   */
  if (le32_to_cpu(resp->data_len) &&
      le32_to_cpu(resp->data_len) <= d->size) {
    u8 *out = resp_buf + sizeof(*resp);

    if (d->handle_offset != NVGPU_GEM_NO_FIELD && ret >= 0) {
      if (d->handle_is_out) {
        /*
         * The host made an object. Stand a proxy in front of it before the
         * caller sees anything: the host's handle means nothing in this
         * guest, and the core's PRIME and GEM_CLOSE paths need an object of
         * ours to work on.
         */
        u32 host_handle = get_unaligned_le32(out + d->handle_offset);
        u64 obj_size = 0;
        u32 guest_handle;
        int cret;

        if (d->size_field_offset != NVGPU_GEM_NO_FIELD)
          obj_size = get_unaligned_le64(out + d->size_field_offset);

        cret = nvgpu_gem_proxy_create(file, nfd, host_handle, (size_t)obj_size,
                                      &guest_handle);
        if (cret) {
          struct nvgpu_drm_gem_close close = {.handle = host_handle};

          nvgpu_ioctl_flat_h(nfd->dev, fwd_handle, DRM_IOCTL_GEM_CLOSE, &close,
                             sizeof(close));
          ret = cret;
          goto out;
        }
        put_unaligned_le32(guest_handle, out + d->handle_offset);
      } else {
        /* The caller reads back the handle it passed, not the host's. */
        put_unaligned_le32(caller_handle, out + d->handle_offset);
      }
    }

    if (copy_to_user(uarg, out, le32_to_cpu(resp->data_len)))
      ret = -EFAULT;
  }

  if (user_nested && nested_size > 0 && le32_to_cpu(resp->nested_len) > 0) {
    u32 copy_back = min(nested_size, le32_to_cpu(resp->nested_len));

    if (copy_to_user(user_nested, resp_buf + sizeof(*resp) + d->size,
                     copy_back))
      ret = -EFAULT;
  }

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

  /* nvidia-modeset ioctls use type 0x6d ('m') */
  if (ioc_type == 0x6d)
    return nvgpu_ioctl_modeset(nfd, cmd, uarg, sz);

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
    .poll = nvgpu_poll,
};

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
  int ret = 0;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  if (!req)
    return -ENOMEM;

  resp_buf = kvmalloc(resp_size, GFP_KERNEL);
  if (!resp_buf) {
    kfree(req);
    return -ENOMEM;
  }

  req->msg_type = cpu_to_le32(NVGPU_MSG_GET_PROC_FILES);
  req->handle = 0;
  req->status = 0;
  req->padding = 0;

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp_buf, resp_size);
  if (ret < 0) {
    dev_err(&dev->vdev->dev, "virtio-gpu-nv: GET_PROC_FILES failed: %d\n", ret);
    goto out;
  }

  p = resp_buf;
  end = resp_buf + resp_size;

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

/* ───────── DRI device nodes (host major:minor passthrough) ─────────────── */

static int nvgpu_dri_open(struct inode *inode, struct file *filp) {
  struct nvgpu_dri_dev *dri =
      container_of(inode->i_cdev, struct nvgpu_dri_dev, cdev);
  struct nvgpu_device *dev = dri->dev;
  struct nvgpu_fd *nfd = NULL;
  struct nvgpu_open_req *req = NULL;
  struct nvgpu_open_resp *resp = NULL;
  int ret;

  if (!dev)
    return -ENODEV;

  nfd = kzalloc(sizeof(*nfd), GFP_KERNEL);
  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !req || !resp) {
    ret = -ENOMEM;
    goto err;
  }

  nfd->dev = dev;
  nfd->device_type = NVGPU_DEV_DRI_BASE + dri->index;

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->hdr.handle = 0;
  req->hdr.status = 0;
  req->hdr.padding = 0;
  req->device_type = cpu_to_le32(NVGPU_DEV_DRI_BASE + dri->index);
  req->flags = cpu_to_le32(filp->f_flags);

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0)
    goto err;

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto err;
  }

  nfd->handle = le32_to_cpu(resp->hdr.handle);
  nvgpu_fd_register(nfd->dev, nfd);
  filp->private_data = nfd;
  kfree(req);
  kfree(resp);
  return 0;

err:
  kfree(nfd);
  kfree(req);
  kfree(resp);
  return ret;
}

static const struct file_operations nvgpu_dri_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_dri_open,
    .release = nvgpu_release,
    .unlocked_ioctl = nvgpu_dri_ioctl,
    .mmap = nvgpu_mmap,
    .poll = nvgpu_poll,
};

/*
 * The DRM side of a render node.
 *
 * The guest's node exists so NVIDIA's Vulkan and EGL userspace can find the
 * GPU the way it insists on finding it. Only the pieces that enumeration
 * touches are here: the core answers DRM_IOCTL_VERSION out of the fields
 * below, and the driver-private range is forwarded like any other ioctl.
 *
 * `name` is what the ICD compares against, so it is the host driver's name and
 * not this module's.
 */
static int nvgpu_drm_open(struct drm_device *drm, struct drm_file *file) {
  struct nvgpu_dri_dev *dri = drm->dev_private;
  struct nvgpu_device *dev;
  struct nvgpu_fd *nfd = NULL;
  struct nvgpu_open_req *req = NULL;
  struct nvgpu_open_resp *resp = NULL;
  int ret;

  if (!dri || !dri->dev)
    return -ENODEV;
  dev = dri->dev;

  nfd = kzalloc(sizeof(*nfd), GFP_KERNEL);
  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (!nfd || !req || !resp) {
    ret = -ENOMEM;
    goto err;
  }

  nfd->dev = dev;
  nfd->device_type = NVGPU_DEV_DRI_BASE + dri->index;

  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_OPEN);
  req->device_type = cpu_to_le32(nfd->device_type);
  req->flags = cpu_to_le32(O_RDWR);

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp, sizeof(*resp));
  if (ret < 0)
    goto err;

  if ((s32)le32_to_cpu((__le32)resp->hdr.status) < 0) {
    ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
    goto err;
  }

  nfd->handle = le32_to_cpu(resp->hdr.handle);
  nvgpu_fd_register(nfd->dev, nfd);
  file->driver_priv = nfd;
  kfree(req);
  kfree(resp);
  return 0;

err:
  kfree(nfd);
  kfree(req);
  kfree(resp);
  return ret;
}

static void nvgpu_drm_postclose(struct drm_device *drm, struct drm_file *file) {
  struct nvgpu_fd *nfd = file->driver_priv;
  struct nvgpu_msg_hdr *req, *resp;

  if (!nfd)
    return;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  resp = kzalloc(sizeof(*resp), GFP_KERNEL);
  if (req && resp) {
    req->msg_type = cpu_to_le32(NVGPU_MSG_CLOSE);
    req->handle = cpu_to_le32(nfd->handle);
    nvgpu_send_recv(nfd->dev, req, sizeof(*req), resp, sizeof(*resp));
  }
  kfree(req);
  kfree(resp);
  nvgpu_fd_unregister(nfd->dev, nfd);
  kfree(nfd);
  file->driver_priv = NULL;
}

/*
 * The DRM core refuses to open a node whose fops do not declare
 * FOP_UNSIGNED_OFFSET:
 *
 *   if (WARN_ON_ONCE(!(filp->f_op->fop_flags & FOP_UNSIGNED_OFFSET)))
 *           return -EINVAL;            -- drm_open_helper(), drm_file.c
 *
 * DRM offsets are a mmap address space and are unsigned, so the core makes
 * every driver say so. Drivers that build their fops with DEFINE_DRM_GEM_FOPS
 * get it for free; ours are written out by hand and so must set it, or every
 * open of /dev/dri/renderD128 fails with EINVAL before .open is ever reached.
 *
 * Guarded because the flag postdates the kernels this module still builds
 * against; on those the check does not exist either.
 */
/*
 * The DRM node's ioctl entry point.
 *
 * Three kinds of ioctl arrive on /dev/dri/renderD128:
 *
 *   driver range (DRM_COMMAND_BASE..END)  nvidia-drm's own -- GET_DEV_INFO and
 *                                         the two SUPPORTED probes. drm_ioctl()
 *                                         answers -EINVAL for these because we
 *                                         register no drm_ioctl_desc table, so
 *                                         they are taken here first.
 *   other type 'd'                        core DRM: VERSION, GET_UNIQUE, ...
 *                                         left to drm_ioctl().
 *   type 'F'                              NVIDIA RM, proxied to the host like
 *                                         on any other node.
 */
static long nvgpu_drm_unlocked_ioctl(struct file *filp, unsigned int cmd,
                                     unsigned long arg) {
  struct drm_file *file = filp->private_data;
  struct nvgpu_fd *nfd;
  unsigned int nr = _IOC_NR(cmd);

  if (!file || !file->driver_priv)
    return -ENODEV;
  nfd = file->driver_priv;

  if (_IOC_TYPE(cmd) == DRM_IOCTL_BASE) {
    if (nr >= DRM_COMMAND_BASE && nr < DRM_COMMAND_END) {
      struct nvgpu_dri_dev *dri = file->minor->dev->dev_private;

      if (!dri)
        return -ENODEV;
      return nvgpu_drm_handle_ioctl(nfd, dri, file, cmd, arg);
    }

    /*
     * Core DRM, answered by the core against this node's own state. That is
     * right for VERSION and GET_UNIQUE and wrong for anything that names a GEM
     * object: the objects live in the host's drm_file, so the core looks them
     * up here, finds nothing, and refuses.
     *
     * Named for the same reason the driver range is (see below): a refusal
     * from the core carries no hint that it came from the wrong side of the
     * boundary, and turns up much later as a client that stopped asking.
     */
    {
      long ret = drm_ioctl(filp, cmd, arg);

      if (ret < 0)
        dev_warn_ratelimited(&nfd->dev->vdev->dev,
                             "virtio-gpu-nv: core DRM ioctl nr=0x%02x answered "
                             "locally with %ld\n",
                             nr, ret);
      return ret;
    }
  }

  return nvgpu_ioctl_fd(nfd, cmd, arg);
}

static const struct file_operations nvgpu_drm_fops = {
    .owner = THIS_MODULE,
#if defined(FOP_UNSIGNED_OFFSET)
    .fop_flags = FOP_UNSIGNED_OFFSET,
#endif
    .open = drm_open,
    .release = drm_release,
    .unlocked_ioctl = nvgpu_drm_unlocked_ioctl,
    .compat_ioctl = nvgpu_drm_unlocked_ioctl,
    .mmap = drm_gem_mmap,
    .poll = drm_poll,
    .read = drm_read,
    .llseek = noop_llseek,
};

static const struct drm_driver nvgpu_drm_driver = {
    .driver_features = DRIVER_GEM | DRIVER_RENDER,
    .open = nvgpu_drm_open,
    .postclose = nvgpu_drm_postclose,
    .fops = &nvgpu_drm_fops,
    .name = "nvidia-drm",
    .desc = "NVIDIA DRM driver",
    .major = 0,
    .minor = 0,
    .patchlevel = 0,
};

static char *nvgpu_devnode(const struct device *dev, umode_t *mode) {
  if (mode)
    *mode = 0666;
  return NULL;
}

static struct class *nvgpu_dri_class;

static char *nvgpu_dri_devnode(const struct device *dev, umode_t *mode) {
  if (mode)
    *mode = 0666;
  return kasprintf(GFP_KERNEL, "dri/%s", dev_name(dev));
}

static int nvgpu_dri_init(struct nvgpu_device *dev) {
  int i;

  if (dev->num_dri_devs == 0) {
    dev_info(&dev->vdev->dev,
             "virtio-gpu-nv: no DRI devices reported by VMM\n");
    return 0;
  }

  nvgpu_dri_class = class_create("nvgpu_dri");
  if (IS_ERR(nvgpu_dri_class)) {
    dev_err(&dev->vdev->dev, "virtio-gpu-nv: failed to create dri class: %ld\n",
            PTR_ERR(nvgpu_dri_class));
    nvgpu_dri_class = NULL;
    return PTR_ERR(nvgpu_dri_class);
  }
  nvgpu_dri_class->devnode = nvgpu_dri_devnode;

  for (i = 0; i < dev->num_dri_devs; i++) {
    struct nvgpu_dri_dev *dri = &dev->dri_devs[i];

    /*
     * Find the pci_dev that owns this DRI device so we can:
     *   a) Use it as the parent of the device_create() call — this causes
     *      the kernel to create /sys/dev/char/M:N/device → pci_dev, which
     *      is what Vulkan/EGL reads when it traverses the sysfs char-dev tree.
     *   b) Create drm/<name> kobjects under the PCI device, which gives
     *      /sys/bus/pci/devices/<addr>/drm/<name> — required by the NVIDIA
     *      Vulkan ICD when it enumerates display engines.
     *
     * We match by gpu_id (minor number) against the GPU slots in config space.
     */
    struct device *pci_parent = &dev->vdev->dev; /* fallback */
    struct kobject *pci_kobj = NULL;
    struct drm_device *drm;
    int gi;

    for (gi = 0; gi < dev->num_pci_roots; gi++) {
      struct nvgpu_pci_root *root = &dev->pci_roots[gi];

      if (!root->registered || !root->pdev)
        continue;

      /* Match: the DRI device belongs to this GPU if the GPU's minor number
       * (which equals the /dev/nvidia<minor> index) matches the gpu_id field
       * set from the host.  gpu_id is the 32-bit RM client GPU identifier,
       * but we stored minor there from the VMM side — see device.rs. */
      {
        u32 slot_minor = le32_to_cpu(dev->gpu_slots[gi].minor);
        if (slot_minor != dri->slot_index && gi != 0)
          continue; /* only fall through for GPU 0 as a last resort */
      }

      pci_parent = &root->pdev->dev;
      pci_kobj = &root->pdev->dev.kobj;
      break;
    }

    /*
     * No sysfs is built by hand here any more.
     *
     * This used to create a `drm` kobject under the PCI device and a child
     * named after the node, because a character device gets no such tree and
     * the Vulkan ICD insists on walking one. Registering a real DRM device
     * makes the same tree properly -- and makes the hand-made one fatal: the
     * core tries to create `drm` under the same PCI device, finds the name
     * taken, and drm_dev_register() fails with -EEXIST.
     */

    dri->index = (u32)i;
    dri->dev = dev;

    /*
     * Register a real DRM device rather than a character device at the
     * host's numbers.
     *
     * A raw cdev cannot have them: major 226 belongs to the DRM core, which
     * claims it whenever CONFIG_DRM is built in, so register_chrdev_region()
     * on 226:129 fails with the node never appearing. That failure is quiet
     * -- /sys/bus/pci/.../drm/<name> still gets made, so the tree looks
     * half-right -- and it is fatal to Vulkan, because NVIDIA's userspace
     * enumerates the GPU through the render node and not through
     * /dev/nvidia*, which carry compute. The ICD stats the node, takes its
     * major, and wants /sys/dev/char/<major>:<minor>/device/drm to exist
     * before it will open it. With no node it declines to create an instance
     * and reports only that it found no drivers.
     *
     * The DRM core owns the minor it hands out, so the guest's node is not
     * necessarily the host's number. Nothing requires it to be: the ICD reads
     * whichever node exists.
     */
    drm = drm_dev_alloc(&nvgpu_drm_driver, pci_parent);
    if (IS_ERR(drm)) {
      dev_warn(&dev->vdev->dev, "virtio-gpu-nv: drm_dev_alloc %s failed: %ld\n",
               dri->name, PTR_ERR(drm));
      continue;
    }
    drm->dev_private = dri;

    if (drm_dev_register(drm, 0) != 0) {
      dev_warn(&dev->vdev->dev, "virtio-gpu-nv: drm_dev_register %s failed\n",
               dri->name);
      drm_dev_put(drm);
      continue;
    }

    dri->drm = drm;
    dri->registered = true;
    dev_info(&dev->vdev->dev,
             "virtio-gpu-nv: registered render node for %s, host (%u:%u) "
             "gpu_id=0x%x\n",
             dri->name, dri->major, dri->minor, dri->dev_info[0]);
  }

  return 0;
}

static void nvgpu_dri_cleanup(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_dri_devs; i++) {
    struct nvgpu_dri_dev *dri = &dev->dri_devs[i];

    if (!dri->registered)
      continue;

    /* The core owns the node and everything under it, including the sysfs
     * tree this used to build by hand. */
    drm_dev_unregister(dri->drm);
    drm_dev_put(dri->drm);
    dri->drm = NULL;
    dri->registered = false;
  }

  if (nvgpu_dri_class) {
    class_destroy(nvgpu_dri_class);
    nvgpu_dri_class = NULL;
  }
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

static int nvgpu_fetch_sys_files(struct nvgpu_device *dev) {
  struct nvgpu_msg_hdr *req;
  u8 *resp_buf;
  u8 *p, *end;
  const int resp_max = 128 * 1024;
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
  req->padding = 0;

  ret = nvgpu_send_recv(dev, req, sizeof(*req), resp_buf, resp_max);
  if (ret < 0)
    goto out;

  p = resp_buf;
  end = resp_buf + resp_max;

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

    num_dri = min(num_dri, (u32)NVGPU_MAX_DRI_DEVS);
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

      if (name_len == 0 || p + name_len > end) {
        dev_warn(&dev->vdev->dev,
                 "virtio-gpu-nv: DRI entry %u bad name_len %u\n", i, name_len);
        break;
      }

      idx = dev->num_dri_devs;
      nl = min(name_len, (u32)(sizeof(dev->dri_devs[idx].name) - 1));
      memset(dev->dri_devs[idx].name, 0, sizeof(dev->dri_devs[idx].name));
      memcpy(dev->dri_devs[idx].name, p, nl);
      dev->dri_devs[idx].major = major;
      dev->dri_devs[idx].minor = minor;
      dev->dri_devs[idx].slot_index = slot_index;
      memcpy(dev->dri_devs[idx].dev_info, info, sizeof(info));
      dev->num_dri_devs++;

      dev_info(&dev->vdev->dev,
               "virtio-gpu-nv: DRI %s (%u:%u) slot %u, nvidia gpu_id=0x%x, "
               "page kind %u/%u, sector layout %u\n",
               dev->dri_devs[idx].name, major, minor, slot_index, info[0],
               info[4], info[5], info[6]);
      p += name_len;
    }
  }

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
  mutex_init(&dev->vq_lock);
  init_completion(&dev->req_done);
  INIT_LIST_HEAD(&dev->fds);
  spin_lock_init(&dev->fds_lock);

  /* Find virtqueues */
  ret = virtio_find_vqs(vdev, 2, vqs, vqs_info, NULL);
  if (ret)
    return ret;

  dev->ctrl_vq = vqs[0];
  dev->event_vq = vqs[1];

  /*
   * Give the host somewhere to put an event. Until this existed the queue was
   * negotiated and empty, so the host had no way to say a descriptor had
   * become readable and the guest's poll() had nothing to report.
   */
  dev->event_bufs =
      kcalloc(NVGPU_EVENT_BUFS, sizeof(*dev->event_bufs), GFP_KERNEL);
  if (dev->event_bufs) {
    int i;

    for (i = 0; i < NVGPU_EVENT_BUFS; i++)
      nvgpu_event_post(dev, &dev->event_bufs[i]);
    virtqueue_kick(dev->event_vq);
  } else {
    dev_warn(&vdev->dev,
             "virtio-gpu-nv: no event buffers; waits will not be woken\n");
  }

  /* Read config space written by the VMM at device creation */
  virtio_cread_bytes(vdev, 0, dev->driver_version, 32);
  dev->driver_version[31] = '\0';
  virtio_cread(vdev, struct virtio_gpu_nv_config, num_gpus, &dev->num_gpus);
  virtio_cread(vdev, struct virtio_gpu_nv_config, caps, &dev->caps);

  if (dev->num_gpus == 0 || dev->num_gpus > 248) {
    dev_err(&vdev->dev, "virtio-gpu-nv: bad num_gpus %u\n", dev->num_gpus);
    return -EINVAL;
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

  /* Create device class once */
  nvgpu_class = class_create("nvidia");
  if (IS_ERR(nvgpu_class)) {
    ret = PTR_ERR(nvgpu_class);
    nvgpu_class = NULL;
    return ret;
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
    goto err_gpu_modeset;

  cdev_init(&dev->cdev_modeset, &nvgpu_modeset_fops);
  dev->cdev_modeset.owner = THIS_MODULE;
  ret = cdev_add(&dev->cdev_modeset, dev->modeset_devno, 1);
  if (ret)
    goto err_gpu_modeset;

  device_create(nvgpu_class, &vdev->dev, dev->modeset_devno, NULL,
                "nvidia-modeset");
  dev_info(&vdev->dev,
           "virtio-gpu-nv: registered /dev/nvidia-modeset (%u:%u)\n",
           MAJOR(dev->modeset_devno), MINOR(dev->modeset_devno));

  /* Register /dev/nvidia-caps/nvidia-cap{1,2} */
  dev->caps_devno = MKDEV(NV_CAPS_MAJOR, 1);
  ret = register_chrdev_region(dev->caps_devno, 2, "nvidia-caps");
  if (ret) {
    dev_warn(&vdev->dev, "virtio-gpu-nv: cannot register nvidia-caps: %d\n",
             ret);
    /* non-fatal — continue without caps */
  } else {
    nvgpu_caps_class = class_create("nvidia-caps");
    if (!IS_ERR(nvgpu_caps_class)) {
      nvgpu_caps_class->devnode = nvgpu_caps_devnode;

      cdev_init(&dev->cdev_caps, &nvgpu_caps_fops);
      dev->cdev_caps.owner = THIS_MODULE;
      if (cdev_add(&dev->cdev_caps, MKDEV(NV_CAPS_MAJOR, 1), 2) == 0) {
        device_create(nvgpu_caps_class, &vdev->dev, MKDEV(NV_CAPS_MAJOR, 1),
                      NULL, "nvidia-cap1");
        device_create(nvgpu_caps_class, &vdev->dev, MKDEV(NV_CAPS_MAJOR, 2),
                      NULL, "nvidia-cap2");
        dev_info(
            &vdev->dev,
            "virtio-gpu-nv: registered /dev/nvidia-caps/nvidia-cap{1,2}\n");
      }
    }
  }

  /* Create /proc/driver/nvidia/version */
  ret = nvgpu_proc_init(dev);
  if (ret)
    goto err_uvm_cdev;

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

  dev_info(&vdev->dev, "virtio-gpu-nv: %u GPU(s), driver %s\n", dev->num_gpus,
           dev->driver_version);
  return 0;

err_gpu_modeset:
  unregister_chrdev_region(dev->modeset_devno, 1);
err_uvm_region:
  unregister_chrdev_region(dev->uvm_devno, 2);
err_uvm_cdev:
err_ctl_cdev:
  cdev_del(&dev->cdev_ctl);
  device_destroy(nvgpu_class, MKDEV(NV_MAJOR, NV_CTL_MINOR));
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
  return ret;
}

static void nvgpu_remove(struct virtio_device *vdev) {
  struct nvgpu_device *dev = vdev->priv;
  int i;

  vdev->config->reset(vdev);
  /* After the reset: the queue is quiet, so the buffers cannot be in use. */
  kfree(dev->event_bufs);
  dev->event_bufs = NULL;

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

  /* nvidia-caps cleanup */
  if (nvgpu_caps_class) {
    device_destroy(nvgpu_caps_class, MKDEV(NV_CAPS_MAJOR, 1));
    device_destroy(nvgpu_caps_class, MKDEV(NV_CAPS_MAJOR, 2));
    cdev_del(&dev->cdev_caps);
    unregister_chrdev_region(MKDEV(NV_CAPS_MAJOR, 1), 2);
    class_destroy(nvgpu_caps_class);
    nvgpu_caps_class = NULL;
  }

  if (nvgpu_class) {
    class_destroy(nvgpu_class);
    nvgpu_class = NULL;
  }

  vdev->config->del_vqs(vdev);

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
