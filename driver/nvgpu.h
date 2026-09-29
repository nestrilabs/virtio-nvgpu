/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * virtio-gpu-nv: state and prototypes shared between the objects that make up
 * virtio_gpu_nv.ko. Anything used by one file only stays in that file.
 */

#ifndef NVGPU_H
#define NVGPU_H

#include <linux/atomic.h>
#include <linux/cdev.h>
#include <linux/completion.h>
#include <linux/err.h>
#include <linux/fs.h>
#include <linux/kobject.h>
#include <linux/kref.h>
#include <linux/list.h>
#include <linux/miscdevice.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/refcount.h>
#include <linux/spinlock.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/wait.h>
#include <linux/xarray.h>

#include <drm/drm_gem.h>

#include "nvgpu_wire.h"

/*
 * x86-64 only (Kconfig's `depends on X86_64`, which an out-of-tree build
 * never reads, hence this): the fake PCI bus embeds x86's struct pci_sysdata,
 * the 32-bit refusals (ADDFB2's) are right only where drm_ioc32.c converts
 * what it does on x86, and memory registered by its pages goes as runs of
 * 4 KiB pages, which the Rust parsers count in too (osdesc.rs PAGE_SIZE).
 */
#ifndef CONFIG_X86_64
#error "virtio-gpu-nv supports x86-64 guests only (driver/Kconfig)"
#endif

struct drm_file;
struct nvgpu_kms_file;

/* ───────── Driver state ───────── */

struct nvgpu_device;

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
   * per-architecture and were previously hardcoded for Ampere. Always the
   * 36-byte (575 and later) layout: the backend normalises older hosts'. */
  u32 dev_info[NVGPU_DEV_INFO_WORDS];
  /* The host's own GET_DEV_INFO struct size (20/28/32/36; 0 unknown), from
   * GET_SYS_FILES section 4; 36 from a backend that predates it. */
  u32 dev_info_size;
  /* The registered DRM device, which owns the node and its sysfs tree. */
  struct drm_device *drm;
  bool registered;
  u32 index;
  /* Index into nvgpu_device.cards of this device's host card node, or -1. */
  int card_index;
  struct nvgpu_device *dev;
};

/* A host card node (GET_SYS_FILES section 3, struct nvgpu_card_record). */
struct nvgpu_card_rec {
  char name[32];
  u32 major;
  u32 minor;
  u32 render_index; /* which nvgpu_device.dri_devs[] it belongs to */
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
 * The PCI core reads a bus's sysdata as the architecture's own type, which on
 * x86 is `struct pci_sysdata`: pci_domain_nr() its `domain`, pcibus_to_node()
 * its `node`, pci_host_bridge_msi_domain() its `fwnode`, the ACPI glue its
 * `companion`. So the bus's sysdata is a whole one, embedded here, the rest
 * of it zero.
 *
 * It used to be mirrored, and a mirror goes wrong silently. First only
 * `domain`: everything after it was read as the NUMA node -- "0000" of the
 * PCI address string, 0x30303030 -- which oopsed in ___slab_alloc as soon as
 * CONFIG_NUMA was on. Then `domain` and `node` only, with `companion`,
 * `iommu` and `fwnode` falling on the address string and the config space
 * (the 2026-09-29 review, #19): harmless while nothing matched on them, and
 * a pointer of "0x...10de" to any IRQ domain that did. The module is x86-64
 * only (Kconfig), so the arch's own type is the one to use.
 */
struct nvgpu_pci_root {
  struct pci_sysdata sd; /* bus->sysdata; container_of() gets the rest */
  struct nvgpu_pci_slot slot;
  struct nvgpu_device *nvdev; /* back pointer        */
  struct pci_host_bridge *bridge;
  struct pci_dev *pdev; /* first (only) device on this bus */
  /* The bridge's one window, [bus_nr, bus_nr]. The bridge's resource list
   * points at it and never frees it, so it lives here rather than on the
   * heap, where it had no owner to free it. */
  struct resource bus_res;
  bool registered;
};

struct nvgpu_device {
  /*
   * Everything that names this struct and can outlive remove() holds a
   * reference (nvgpu_dev_get()): an open file of any of our nodes, a
   * Wayland device, a guest file standing for a backend handle, a host
   * fence's and a syncobj wait's event consumer, an RM mapping's vmas --
   * and each character device below, whose kobject is parented here
   * (cdev_set_parent()), so the cdevs embedded in this struct outlive the
   * last cdev_put() of an inode. remove() drops the probe's, and the last
   * put frees it, with the transport's state and a reference on the virtio
   * device held for its log lines. After remove() the transport is dead
   * (nvgpu_xfer_dead()), not freed, so what still uses it fails -ENODEV
   * instead of touching freed memory (S-26). Never added to sysfs.
   */
  struct kobject kobj;
  /*
   * Where the VMM placed the window, read out of this device's own shared
   * memory region. Zero-length when the VMM offers none, in which case device
   * memory can be mapped on the host but never reached from here.
   */
  struct virtio_shm_region window;
  /*
   * The UVM aperture (shared memory region NVGPU_SHM_ID_UVM): where the VMM
   * gives each UVM semaphore pool a memory slot of its own, since UVM maps a
   * pool only at the host address equal to its offset and the window cannot
   * be that. Zero-length when the VMM offers none; HELLO tells the backend.
   */
  struct virtio_shm_region uvm_aperture;
  struct virtio_device *vdev;
  struct virtqueue *ctrl_vq;
  struct virtqueue *event_vq;

  /* Character device registration */
  struct cdev cdev_gpu[248]; /* /dev/nvidia0 … nvidia247 */
  struct cdev cdev_ctl;      /* /dev/nvidiactl            */
  struct cdev cdev_uvm;      /* /dev/nvidia-uvm           */
  dev_t uvm_devno;           /* dynamic major, minors 0-1 */
  bool uvm_registered;       /* UVM served: nvgpu_uvm_offered() */
  struct cdev cdev_caps;     /* /dev/nvidia-caps */
  dev_t caps_devno;          /* dynamic major, minors 1-2 */
  struct cdev cdev_modeset;  /* /dev/nvidia-modeset */
  dev_t modeset_devno;

  /* Config read from VMM */
  char driver_version[32];
  u32 num_gpus;
  u32 caps;

  /* GPU slots read from config space at probe */
  struct virtio_gpu_nv_gpu_slot gpu_slots[8];

  /* FD translation table received from backend */
  struct nvgpu_fd_translation_entry fd_translations[16];
  u32 num_fd_translations;

  /* Every open descriptor, so an event naming a handle can find its file. */
  struct list_head fds;
  spinlock_t fds_lock;
  /*
   * DRM files by render handle, from open until their last nvgpu_fd_put()
   * -- past release, while proxies keep the render handle: the reaper asks
   * whether a host GEM handle is a proxy's (nvgpu_gem_handle_held()), and a
   * released file's proxies are just as alive as an open one's.
   */
  struct xarray renders;

  /* ── DRI device nodes ── */
#define NVGPU_MAX_DRI_DEVS 8
  struct nvgpu_dri_dev dri_devs[NVGPU_MAX_DRI_DEVS];
  int num_dri_devs;
  /*
   * Host card nodes, from GET_SYS_FILES section 3 (every mode; older
   * backends only with --kms-card). Each names the DRI record (and so the
   * guest DRM device) it is the primary node of. Openable only with
   * NVGPU_BCAP_KMS_CARD; otherwise only their numbers are used (the Wayland
   * devmap). EV_HOTPLUG's cookie indexes this.
   */
  struct nvgpu_card_rec cards[NVGPU_MAX_DRI_DEVS];
  int num_card_recs;

  /* ── PCI sysfs fake hierarchy ── */
  struct kobject *pci_bus_kobj;     /* /sys/bus/pci              */
  struct kobject *pci_devices_kobj; /* /sys/bus/pci/devices      */

#define NVGPU_MAX_PCI_SLOTS 8
  struct nvgpu_pci_root pci_roots[NVGPU_MAX_PCI_SLOTS];
  int num_pci_roots;

  /* ── protocol v2 (nvgpu_xfer.c owns these) ── */
  bool v2;            /* HELLO succeeded; every v2 feature is gated on this */
  u32 backend_caps;   /* NVGPU_BCAP_* */
  u32 max_req;        /* bytes, from HELLO, clamped by what the ring allows */
  u32 max_resp;
  u32 num_cards;
  struct nvgpu_xfer *xfer;     /* transport state: contexts, ring lock, clock */
  struct nvgpu_events *events; /* v2 event consumers: handle/cookie registry  */
  const struct nvgpu_schema_set *schema; /* selected at probe for driver_version */
  const struct nvgpu_uvm_table *uvm;     /* likewise, v1 or v2; NULL: none */

  /*
   * Memory the caller already has, registered with RM by its pages
   * (nvgpu_osdesc.c): what each registration pinned, until a reap names its
   * id. osdesc_lock covers the list, the ack, the early releases, and one
   * reap at a time; osdesc_count is read without it to skip a reap when there
   * is nothing to reap. osdesc_dead: remove() has unpinned everything, and a
   * registration that finishes after it unpins at once.
   */
  struct mutex osdesc_lock;
  struct list_head osdescs;
  /* Late replies to abandoned registrations read before their waiter had
   * handed the pins over (struct nvgpu_osdesc_late). */
  struct list_head osdesc_late;
  unsigned int osdesc_count;
  u64 osdesc_ack;
#define NVGPU_OSDESC_EARLY 64
  u64 osdesc_early[NVGPU_OSDESC_EARLY];
  unsigned int osdesc_early_next;
  bool osdesc_dead;

  /* /proc/driver/nvidia's files' data (nvgpu_procfs.c, struct nvgpu_proc_buf). */
  struct list_head proc_bufs;

  /* /dev/nvgpu-wl and /dev/nvgpu-capture, when registered (nvgpu_misc.c). */
  struct nvgpu_misc_node *wl_node;
  struct nvgpu_misc_node *capture_node;
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
  /*
   * With NVGPU_BCAP_ARMED_READY: an arm (W_ARM) was sent and its report has
   * not come. The backend then reports one event per arm, so a poller that
   * finds nothing pending arms before it sleeps, and one that takes a
   * report arms for the next.
   */
  atomic_t armed;
  struct list_head node; /* dev->fds, for finding this by handle */
  /* DRM files: the answer to GET_DRM_FILE_UNIQUE_ID, given at open, from 1. */
  u64 drm_unique_id;
  /*
   * Lifetime of `handle`. The opener holds one reference, and for a DRM file
   * every GEM proxy whose host object lives in this file holds one more: the
   * host GEM handle belongs to the host drm_file, so the backend handle has to
   * stay open for as long as any guest object stands in front of one of its
   * objects -- a compositor routinely outlives the client whose buffer it
   * imported. The backend CLOSE goes out when the last reference goes
   * (nvgpu_fd_put()), so for a DRM file this struct outlives the drm_file.
   */
  refcount_t ref;
  /*
   * DRM files only: the guest drm_file this is the private state of, or NULL
   * for a character device and from the moment the file is released. Written
   * under the event registry lock (nvgpu_fd_detach_drm()); an event consumer,
   * which runs under that lock, may dereference it there and only there.
   * Nothing may reach the drm_file through this after release: the core frees
   * it right after postclose, while this struct lives on.
   */
  struct drm_file *drm_file;
  /*
   * DRM files only: the backend handle of a host card or lease file used for
   * KMS ioctls, or 0. Set by the KMS code from the file's open or an ioctl on
   * it (the file is alive then, so release cannot run concurrently); taken
   * and CLOSEd at release, before drm_release(), so host master, framebuffers
   * and leases go away when the guest file does rather than when the last
   * GEM proxy does.
   */
  u32 kms_handle;
  /*
   * DRM files only: the KMS side of a file that has one (nvgpu_kms.c) -- an
   * adopted lease, or a primary-node file of a compositor-VM guest -- else
   * NULL. Made at open, freed at release (nvgpu_kms_detach()).
   */
  struct nvgpu_kms_file *kms;
  /*
   * DRM files only: host GEM handle -> the nvgpu_gem_object standing for it,
   * for every proxy whose host object lives in this file's render handle. A
   * PRIME import on the host hands back the handle a file already has for an
   * object, so a host handle that arrives twice must find its proxy rather
   * than grow a second one (which would GEM_CLOSE it twice).
   */
  struct xarray gem_index;
};

/*
 * A GEM parameter struct that carries a userspace pointer, described well
 * enough to forward: where the pointer sits and where the length beside it
 * does. Both are u64. A struct with no pointer is not described here at all --
 * it goes through nvgpu_ioctl_flat(), which copies the whole thing.
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
  /* The file the host object lives in; the proxy holds a reference on it. */
  struct nvgpu_fd *owner;
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
  u8 caching;     /* NVGPU_MMAP_CACHE_*, from the placement's reply */
  bool read_only; /* the host maps it read-only (NVGPU_MMAP_F_READ_ONLY) */
  bool window_valid;
  /* SEMSURF_FENCE_ATTACH moved it into another file at least once, so its
   * free has re-homes to close (nvgpu_fence_gem_free()). */
  bool rehomed;
};

#define to_nvgpu_gem(o) container_of(o, struct nvgpu_gem_object, base)

/* ───────── nvgpu_rmio.c, or nvgpu_rs.rs with NVGPU_RUST ─────────
 *
 * The protocol-v1 IOCTL message: an ioctl on a /dev/nvidia* file (or a DRM
 * file's RM ioctl, of any type but 'd'), a UVM command, and a v1 backend's
 * NVKMS command.
 */

long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd, unsigned long arg);
long nvgpu_uvm_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd,
                        unsigned long arg);
long nvgpu_ioctl_modeset(struct nvgpu_fd *nfd, unsigned int cmd,
                         void __user *uarg);

/* ───────── nvgpu_v1.c: the v1 IOCTL exchange, both builds ───────── */

/* A v1 IOCTL reply's header, as far as the device wrote it. */
struct nvgpu_ioctl_reply {
  s32 status; /* 0 or a -errno in [-MAX_ERRNO, -1]: the host call's result */
  s32 raw;    /* the status as the backend sent it */
  u32 used;   /* bytes the device wrote */
  bool full;  /* the whole nvgpu_ioctl_resp is there; else the lengths are 0 */
  u32 data_len;
  u32 nested_len;
  u32 deep_len;
};
void nvgpu_ioctl_req_init(struct nvgpu_ioctl_req *req, u32 handle, u32 cmd,
                          u32 data_len, u32 nested_off, u32 nested_len,
                          u32 deep_off, u32 deep_len);
/*
 * Read a reply of `used` bytes: 0 and *r, -EIO for less than a header, or
 * -EPROTO for a status that is neither 0 nor an errno. Nothing of an -EPROTO
 * reply goes back to the caller; *r is filled all the same (r->raw, the
 * lengths) for the one path that must still account for what the backend
 * may have made: an OS-descriptor registration's pins.
 */
int nvgpu_ioctl_reply_parse(const void *resp, u32 used,
                            struct nvgpu_ioctl_reply *r);
/* nvgpu_send_recv_used() and nvgpu_ioctl_reply_parse(): a transport error,
 * -EIO, -EPROTO, or 0 with the reply in *r. */
int nvgpu_ioctl_exchange(struct nvgpu_device *dev, void *req, size_t req_len,
                         void *resp, size_t resp_max,
                         struct nvgpu_ioctl_reply *r);
/* nvgpu_ioctl_flat() flags */
#define NVGPU_FLAT_PROC (1u << 0)  /* the calling process after the block */
#define NVGPU_FLAT_WHOLE (1u << 1) /* a success brings all `sz` back, or -EIO */
/*
 * One flat v1 IOCTL on backend handle `handle` (rather than an nvgpu_fd: a GEM
 * op goes to the file that owns the object, not always the caller's): the
 * `sz` bytes of `buf` out, with NVGPU_FLAT_PROC the calling process after
 * them if the backend takes it, and back into `buf` what the reply carries,
 * its length in *back (may be NULL). Without NVGPU_FLAT_WHOLE that is the
 * reply's block if it is no longer than `sz`, else nothing; with it, all
 * `sz` bytes or nothing, and a success that brings less back is -EIO. The
 * host call's status (0 or -errno), or a transport error, -EIO or -EPROTO.
 */
long nvgpu_ioctl_flat(struct nvgpu_device *dev, u32 handle, unsigned int cmd,
                      void *buf, u32 sz, u32 flags, u32 *back);

/* ───────── nvgpu_main.c ───────── */

/*
 * The backend handle standing for one of this module's open files of device
 * `dev`: an /dev/nvidia* character device, a DRM node of ours (its render
 * handle), or a host-handle file (nvgpu_hostfile_handle()). -EBADF for
 * anything else -- never a guess at another driver's private_data, nor a
 * handle of another device's backend, which is someone else's number here.
 */
int nvgpu_handle_for_fd(struct nvgpu_device *dev, int guest_fd, u32 *handle);
/*
 * Whether calls say which guest process makes them (NVGPU_BCAP_PROC_ID),
 * and that process, as struct nvgpu_proc_id at `dst`.
 */
bool nvgpu_proc_ids(const struct nvgpu_device *dev);
/* And its euid, with every RM_CONTROL too (NVGPU_BCAP_PROC_EUID). */
bool nvgpu_proc_euid(const struct nvgpu_device *dev);
void nvgpu_proc_id_fill(const struct nvgpu_device *dev, void *dst);
struct task_struct;
void nvgpu_proc_id_fill_task(const struct nvgpu_device *dev,
                             struct task_struct *t, void *dst);
/* Fill an OPEN's trailer when the backend wants one; the length to send. */
u32 nvgpu_open_req_fill_proc(const struct nvgpu_device *dev,
                             struct nvgpu_open_req_proc *r);
/* The nvgpu_fd behind a character device or DRM file of ours, else NULL. */
struct nvgpu_fd *nvgpu_fd_from_file(struct file *f);
/* nvgpu_xfer.c: nothing sent will be answered (after a reset, remove()). */
bool nvgpu_xfer_dead(struct nvgpu_device *dev);
/* nvgpu_syncobj.c: every guest syncobj waiter looks again. */
void nvgpu_fence_wake_waiters(void);

/*
 * Frame-pacing counters, nvgpu_xfer.c's (ARCHITECTURE.md, "Frame pacing"):
 * relaxed atomics, always kept, read as root from
 * /sys/module/virtio_gpu_nv/parameters/pacing. The syncobj waits of
 * nvgpu_syncobj.c count themselves here too.
 */
enum nvgpu_pace_ctr {
  NVGPU_PACE_SW_WAITS,    /* SYNCOBJ_(TIMELINE_)WAITs with a timeout */
  NVGPU_PACE_SW_POLLS,    /* host polls those made */
  NVGPU_PACE_SW_SLEEPS,   /* sleeps until a registration fired */
  NVGPU_PACE_SW_WOKEN,    /* ... that a registration ended */
  NVGPU_PACE_SW_NAPS,     /* backoff naps (over the cap, or re-arming) */
  NVGPU_PACE_SW_OVERCAP,  /* waits that fell back to polling */
  NVGPU_PACE_EV_BATCHES,  /* event-queue buffers taken back */
  NVGPU_PACE_EV_RECORDS,  /* records in them */
  NVGPU_PACE_EV_LEGACY,   /* legacy readiness (RM event fds) among them */
  NVGPU_PACE_EV_LEGACY_SET, /* ... that set `pending` (it was clear) */
  NVGPU_PACE_POLLS,       /* poll()s of an RM descriptor */
  NVGPU_PACE_POLLS_READY, /* ... that took a pending report */
  NVGPU_PACE_CTRS
};
void nvgpu_pace_inc(enum nvgpu_pace_ctr c);
/*
 * The module's own work queues (nvgpu_main.c), made at init and destroyed at
 * exit after the driver is unregistered: destroy_workqueue() waits for an
 * item still running, so no item returns into module text after it is gone
 * -- which the system queues did not guarantee for items that hold no module
 * reference, or drop the last one (S3, 2026-09-29). nvgpu_wq: short,
 * high-priority items (W_ARM, a fence proxy's WATCH); nvgpu_long_wq: items
 * that make round trips (a semaphore-surface wait's second half).
 */
extern struct workqueue_struct *nvgpu_wq;
extern struct workqueue_struct *nvgpu_long_wq;
void nvgpu_dev_get(struct nvgpu_device *dev);
/* Any context: the last put only frees memory. */
void nvgpu_dev_put(struct nvgpu_device *dev);
void nvgpu_fd_register(struct nvgpu_device *dev, struct nvgpu_fd *nfd);
void nvgpu_fd_unregister(struct nvgpu_device *dev, struct nvgpu_fd *nfd);
void nvgpu_fd_get(struct nvgpu_fd *nfd);
/* Drops a reference; the last one CLOSEs the backend handle and frees. */
void nvgpu_fd_put(struct nvgpu_fd *nfd);

/* ───────── nvgpu_procfs.c ───────── */

/*
 * GET_PROC_FILES, and GET_SYS_FILES' first section, are records of
 * {le32 path_len, le32 content_len, path, content}, unaligned, in a stream
 * with no header that ends where the device stopped writing, or at a record
 * with both lengths 0 (device/src/nvidia.rs, handle_get_files()).
 */
struct nvgpu_file_rec {
  const u8 *path;
  const u8 *content;
  u32 path_len;
  u32 content_len;
};
/*
 * The record at *p, with *p moved past it: true. False at the terminator (*p
 * past it), at the end of the stream, or for a record that runs past `end`:
 * then *truncated, *p past its lengths, and nothing of it read.
 */
bool nvgpu_file_rec_next(const u8 **p, const u8 *end,
                         struct nvgpu_file_rec *r, bool *truncated);
/* /proc/driver/nvidia from GET_PROC_FILES (probe; -ENOMEM is fatal). */
int nvgpu_proc_init(struct nvgpu_device *dev);
/* Take /proc/driver/nvidia down, readers waited out, then free its data. */
void nvgpu_proc_cleanup(struct nvgpu_device *dev);

/* ───────── nvgpu_pci.c ───────── */

/* GET_SYS_FILES: the GPUs' config space, the DRI devices, the host cards. */
int nvgpu_fetch_sys_files(struct nvgpu_device *dev);
/* The fake PCI bus and device of each GPU whose config space came. */
int nvgpu_pci_init(struct nvgpu_device *dev);
void nvgpu_pci_cleanup(struct nvgpu_device *dev);

/* ───────── nvgpu_drm.c ───────── */

int nvgpu_dri_init(struct nvgpu_device *dev);
void nvgpu_dri_cleanup(struct nvgpu_device *dev);
/* The nvgpu_fd of a DRM file of this driver, else NULL. */
struct nvgpu_fd *nvgpu_drm_file_nfd(struct file *f);
/* A DRM file of ours the caller just opened stops being the guest device's
 * master, if it had become it. */
void nvgpu_drm_drop_master(struct file *f);
/*
 * A DRM ioctl's argument as drm_ioctl() hands it to a handler
 * (drm_ioctl.c:848-915). The handler is picked by the ioctl's number alone,
 * whatever size and direction the caller's command says, and runs on a kernel
 * copy at least as large as the native struct: the caller's _IOC_SIZE bytes
 * copied in where both its command and the native one say IN, the rest
 * zeroed; afterwards the caller's _IOC_SIZE bytes copied back where both say
 * OUT, whatever the handler returned. A caller built against an older,
 * shorter struct (a 16-byte drm_syncobj_handle, before `point`) gets the
 * native call with the new fields zero; one built against a longer struct
 * gets its tail back as it sent it (zeroed, where nothing went IN). The
 * handler and the host see `cmd`, the native number, and nothing else: no
 * size a native caller could not send reaches the backend.
 *
 * Bounded as the core bounds it: _IOC_SIZE is 14 bits, so at most 16 KiB,
 * on the stack up to 128 bytes, kmalloc()ed past that.
 */
struct nvgpu_drm_arg {
  unsigned int cmd; /* native: what the handler and the host see */
  void *k;          /* max(caller's size, native size) bytes */
  void __user *u;
  u32 out;          /* bytes copied back */
  u64 stack[16];
};
/* Copy in; on failure (-ENOMEM, -EFAULT) nothing is left to free. */
int nvgpu_drm_arg_in(struct nvgpu_drm_arg *a, unsigned int ucmd,
                     unsigned int ncmd, void __user *u);
/* Copy back (-EFAULT replaces `ret` if that fails) and free. */
long nvgpu_drm_arg_out(struct nvgpu_drm_arg *a, long ret);
/* Free without copying back: the call was not ours after all. */
void nvgpu_drm_arg_drop(struct nvgpu_drm_arg *a);
/*
 * Stand a guest GEM object in front of host object `host_handle` of `owner`'s
 * host file, and return a handle for it in `file`. The proxy takes a
 * reference on `owner`. On failure the host handle has already been closed
 * (by the proxy's own free, where one was made): the caller must not close it
 * again -- except for -EEXIST, which means a proxy for that host handle
 * already exists and the handle is left alone, being that proxy's, and
 * -EAGAIN, which means one on its way out does (nvgpu_gem_dying()).
 */
int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *owner,
                           u32 host_handle, size_t size, u32 *guest_handle);
/* drm_gem_object_put() as a release callback (nvgpu_tbuf_on_free(),
 * nvgpu_i2_hold()): the reference that keeps a proxy's host handle its own
 * while a request names it. */
void nvgpu_gem_put_ref(void *obj);
/*
 * The proxy already standing for host GEM handle `host_handle` of `owner`'s
 * render file, with a reference taken, or NULL. nvgpu_gem_proxy_create()
 * refuses (-EEXIST, leaving the handle alone: it is that proxy's) to make a
 * second one, so a caller whose host handle may be one the file already had
 * -- anything that PRIME-imports on the host -- looks here first.
 */
struct drm_gem_object *nvgpu_gem_proxy_find(struct nvgpu_fd *owner,
                                            u32 host_handle);
/*
 * One proxy per host handle, until it is closed (S-11): a proxy whose last
 * reference is gone stays in `owner`'s index until the host has closed its
 * handle -- its GEM_CLOSE answered, or known never to run -- not merely until
 * the close was queued. A host handle it holds is neither found nor
 * adoptable -- a PRIME
 * import that returns it gets -EAGAIN -- and is nobody else's to close.
 * Whoever meets one waits it out and asks the host again.
 */
bool nvgpu_gem_dying(struct nvgpu_fd *owner, u32 h);
int nvgpu_gem_wait_gone(struct nvgpu_fd *owner, u32 h);
/* Whether a proxy, alive or dying, holds host GEM `gem` of backend handle
 * `render` (for the reaper, which has only the numbers). */
bool nvgpu_gem_handle_held(struct nvgpu_device *dev, u32 render, u32 gem);
/*
 * Host GEM handle `gem` of `owner`'s render file, which a reply just named
 * and nothing is to be made of: GEM_CLOSEd, unless a proxy (alive or dying)
 * holds the number -- the host hands back the handle a file already has for
 * an object -- which is then that proxy's to close and nobody else's (S-11).
 */
void nvgpu_gem_close_unheld(struct nvgpu_fd *owner, u32 gem);
/* A proxy's fake mmap offset in this node (MAP_DUMB, GEM_MAP_OFFSET). */
int nvgpu_gem_mmap_offset(struct drm_file *file, u32 guest_handle,
                          u64 *offset);

struct dma_buf;
/*
 * Wayland channel (nvgpu_wl.c). A dma-buf of one of this device's GEM
 * proxies -> its (owner backend handle, host GEM), else -EINVAL.
 */
int nvgpu_dmabuf_to_host(struct nvgpu_device *dev, struct dma_buf *buf,
                         u32 *owner, u32 *gem);
/*
 * Host GEM @host_gem, just imported into the render handle of @drm_filp (a
 * DRM file of ours), as a new guest dma-buf (@o_flags: O_CLOEXEC | O_RDWR),
 * not yet a descriptor: the caller installs it (dma_buf_fd(), or fd_install()
 * of buf->file) once nothing else can fail, or dma_buf_put()s it. An ERR_PTR
 * on failure. @obj_type is the host's IDENTIFY answer for it
 * (NVGPU_GEM_OBJECT_*), which a new proxy reports. Owns @host_gem unless it
 * returns -EBADF (not our file) or -EAGAIN (a dying proxy's handle: wait it
 * out with nvgpu_gem_wait_gone() and import again).
 */
struct dma_buf *nvgpu_dmabuf_from_host_buf(struct file *drm_filp,
                                           u32 host_gem, u64 size,
                                           u32 obj_type, int o_flags);

/* Guest handle in `file` -> the proxy itself, referenced (drop it with
 * drm_gem_object_put(&ng->base)), or NULL for anything that is not one. */
struct nvgpu_gem_object *nvgpu_gem_lookup(struct drm_file *file,
                                          u32 guest_handle);

/* ───────── nvgpu_kms.c ───────── */

/*
 * The KMS side of a guest DRM file (ARCHITECTURE.md §11). A file has one if it is an
 * adopted host lease, or a primary-node file on a backend offering card nodes
 * (NVGPU_BCAP_KMS_CARD), whose host card file is then opened lazily. Called
 * from nvgpu_drm_open() once the render handle is open; consumes a pending
 * adoption (nvgpu_adopt_drm_file()) meant for this open.
 */
int nvgpu_kms_open(struct nvgpu_dri_dev *dri, struct drm_file *file,
                   struct nvgpu_fd *nfd);
/*
 * Release, after nvgpu_fd_detach_drm() and before the KMS handle is CLOSEd:
 * stop event delivery, give back every reserved event, close a retired card
 * handle, free the state.
 */
void nvgpu_kms_detach(struct nvgpu_fd *nfd);
/*
 * A core-range ioctl on a DRM file: true if this file's KMS side answered it
 * (result in *ret), false to leave it to the caller (drm_ioctl() or the
 * driver range) -- always false for a file without a KMS side.
 */
bool nvgpu_kms_ioctl(struct file *filp, unsigned int cmd, unsigned long arg,
                     long *ret);
/* drm_driver.master_set / master_drop: mirror guest master onto the host. */
void nvgpu_kms_master_set(struct drm_file *file, bool new_master);
void nvgpu_kms_master_drop(struct drm_file *file);
/*
 * A host DRM file the backend holds as `kms_handle` (of backend kind `kind`,
 * NVGPU_HK_DRM_LEASE) as a new guest DRM file: a clone of `tmpl`, which must
 * be a primary-node (card) file of this module, opened O_RDWR plus
 * `o_flags & O_NONBLOCK`; returns a descriptor installed with
 * `o_flags & O_CLOEXEC`, or -errno. The clone opens a render handle of its
 * own for its GEM objects.
 *
 * Ownership of `kms_handle` passes to this call whenever `tmpl` is such a
 * file: on success the new file owns it (CLOSEd when it is released); on
 * failure it has been closed here, or by the clone's own release if the
 * clone got far enough to take it. -EBADF means exactly that `tmpl` is not a
 * card file of ours: the handle cannot be attributed to a device and is
 * still the caller's to close. No other failure returns -EBADF.
 */
int nvgpu_adopt_drm_file(struct file *tmpl, u32 kms_handle, u32 kind,
                         int o_flags);
/*
 * The same, as the new file itself (an ERR_PTR on failure, with the same
 * ownership of `kms_handle`: ERR_PTR(-EBADF) exactly when `tmpl` is not a
 * card file of ours), for a caller that installs its descriptor only once
 * nothing else can fail (nvgpu_wl.c's RECV).
 */
struct file *nvgpu_adopt_drm_filp(struct file *tmpl, u32 kms_handle,
                                  u32 kind, int o_flags);

/* ───────── nvgpu_nvkms.c ───────── */

/* /dev/nvidia-modeset on a v2 device: NVKMS through IOCTL2, and the
 * readiness NVKMS keeps until GET_NEXT_EVENT/CLEAR_UNICAST_EVENT. */
long nvgpu_nvkms_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                       void __user *uarg);
__poll_t nvgpu_nvkms_poll(struct nvgpu_fd *nfd, struct file *filp,
                          struct poll_table_struct *wait);

/* ───────── nvgpu_hostfile.c ───────── */

/* The backend handle behind a host-handle file of `dev`, or -EBADF if `f`
 * is not one. */
int nvgpu_hostfile_handle(struct nvgpu_device *dev, struct file *f,
                          u32 *handle);
/*
 * A backend handle of kind `kind` (NVGPU_HK_*; a host syncobj file, say) as a
 * guest file: its release CLOSEs the handle. `o_flags`: O_CLOEXEC /
 * O_NONBLOCK. Returns the new fd. On success the file owns `handle`; on
 * failure it is closed if `close_on_error`, else still the caller's (an
 * nvgpu_i2_ops.fd_out hook passes false: the interpreter closes it).
 */
int nvgpu_hostfile_install(struct nvgpu_device *dev, u32 handle, u32 kind,
                           int o_flags, bool close_on_error);
/* The handle behind guest fd `fd` if it is a host-handle file of `dev` and of
 * `kind`; -EINVAL for any other file (as the kernel refuses a file that is
 * not a syncobj, drm_syncobj.c:712), -EBADF for no file. The handle stays the
 * file's: the caller must not close it. */
int nvgpu_hostfile_lookup(struct nvgpu_device *dev, int fd, u32 kind,
                          u32 *handle);
/* The same, keeping the file referenced -- and so the handle open -- until
 * the caller's fput(): for a handle about to be sent in a call. ERR_PTR on
 * failure. */
struct file *nvgpu_hostfile_fget(struct nvgpu_device *dev, int fd, u32 kind,
                                 u32 *handle);

/* ───────── nvgpu_fence.c, nvgpu_syncobj.c, nvgpu_semsurf.c ───────── */

/*
 * Fences live on the host (ARCHITECTURE.md §12): a guest sync_file this driver makes
 * wraps an nvgpu host fence, a proxy dma_fence for a host sync_file that
 * signals (with the host's error, if any) when the host's does
 * (nvgpu_fence.c); the syncobj ioctls are nvgpu_syncobj.c's, the
 * semaphore-surface ones nvgpu_semsurf.c's.
 */

struct dma_fence;

/* v2 and the backend serves fences: the syncobj and semsurf paths are live. */
bool nvgpu_fences_enabled(struct nvgpu_device *dev);
/*
 * Host sync_file `handle` as a new guest sync_file fd, opened with `o_flags`
 * (O_CLOEXEC). Takes ownership of `handle` whatever happens: on failure it is
 * closed. The proxy is watched from the moment it exists, so it signals even
 * if the host's fence already has.
 */
int nvgpu_fence_from_handle(struct nvgpu_device *dev, u32 handle, int o_flags);
/* The same for an nvgpu_i2_ops.fd_out hook (an ATOMIC out-fence, say): on
 * failure `handle` is left alone, because the interpreter closes what a
 * failing hook was given. */
int nvgpu_fence_from_handle_noclose(struct nvgpu_device *dev, u32 handle,
                                    int o_flags);
/*
 * The host sync_file standing for guest sync_file `fd`, for handing to a
 * host consumer (IN_FENCE_FD, SEMSURF_FENCE_WAIT, a syncobj import, the
 * Wayland proxy). Returns:
 *   0  *handle names it. *owned false: it is the proxy's own handle (one of
 *      our fences) -- pass it, never close it, and hold *ref (the fence that
 *      keeps it open) until the host is done with the call, then
 *      nvgpu_fence_put_ref() it (nvgpu_i2_hold()); *owned true: a new handle
 *      made for this call (a merge of our fences), *ref NULL -- pass it with
 *      NVGPU_I2_FD_CONSUME, or close it.
 *   1  already signalled: nothing to wait for (the caller sends "no fence",
 *      or a signalled stand-in if the field cannot be empty).
 *  <0  -EINVAL for a descriptor that is not a sync_file; -ERESTARTSYS if a
 *      signal came while waiting (below); transport errors.
 * A fence the host cannot see -- another guest driver's, sw_sync -- is
 * waited for here, interruptibly and without a timeout, and then reported as
 * signalled: the host has no fence to wait on in its place.
 */
int nvgpu_fence_unwrap_fd(struct nvgpu_device *dev, int fd, u32 *handle,
                          bool *owned, struct dma_fence **ref);
/* dma_fence_put() as a release callback (nvgpu_i2_hold()). */
void nvgpu_fence_put_ref(void *fence);
/*
 * The core syncobj ioctls (0xBF-0xCF), when nvgpu_fences_enabled(): the
 * native command of the one numbered as `cmd` is (this kernel's, which the
 * render schema must agree with), 0 if `cmd` is none of them.
 */
unsigned int nvgpu_fence_syncobj_cmd(unsigned int cmd);
/* Run on the kernel copy (struct nvgpu_drm_arg): `cmd` is the native one. */
long nvgpu_fence_syncobj_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void *karg);
/* nvidia-drm SEMSURF_FENCE_* (0x54-0x57): the native command, as above. */
unsigned int nvgpu_fence_semsurf_cmd(unsigned int nr);
/* When nvgpu_fences_enabled(), on the kernel copy as above. */
long nvgpu_fence_semsurf_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void *karg);
/* A GEM proxy is going: close what SEMSURF_FENCE_ATTACH moved into other
 * files for it. */
void nvgpu_fence_gem_free(struct nvgpu_gem_object *ng);
/* A DRM file is going: the SYNCOBJ_EVENTFD subscribers made through it go
 * too, as its syncobjs do. Process context. */
void nvgpu_fence_file_release(struct nvgpu_fd *nfd);
/* Retire the event consumers buried so far (remove(), module exit). */
void nvgpu_fence_drain(void);
/* The transport of `dev` is dead: signal its host fences with -ENODEV and
 * its SYNCOBJ_EVENTFD subscribers, and wake its syncobj waiters. */
void nvgpu_fence_device_dead(struct nvgpu_device *dev);

/* ───────── nvgpu_misc.c ───────── */

/*
 * A misc node of a device besides the NVIDIA ones (/dev/nvgpu-wl,
 * /dev/nvgpu-capture), embedded in its subsystem's own struct. Refcounted:
 * registration holds one reference, and each open file one more (taken in
 * its open, under misc_mtx, so none is taken after unregister). The node
 * holds a reference on `dev` until its last put, which then calls `free` on
 * it.
 */
struct nvgpu_misc_node {
  struct miscdevice misc;
  struct nvgpu_device *dev;
  struct kref ref;
  void (*free)(struct nvgpu_misc_node *node);
};
/* For a node's mode parameter: within 0770, nothing for "other". */
extern const struct kernel_param_ops nvgpu_misc_mode_ops;
/* Register `node` as /dev/`name`; on failure it has been put (and freed). */
int nvgpu_misc_node_register(struct nvgpu_misc_node *node,
                             struct nvgpu_device *dev, const char *name,
                             ushort mode, const struct file_operations *fops,
                             void (*free)(struct nvgpu_misc_node *node));
/* remove(): no new opens, and registration's reference dropped. */
void nvgpu_misc_node_unregister(struct nvgpu_misc_node *node);
/* In a node's .open: the node, whose reference the file then takes. */
struct nvgpu_misc_node *nvgpu_misc_node_open(struct file *filp);
void nvgpu_misc_node_get(struct nvgpu_misc_node *node);
void nvgpu_misc_node_put(struct nvgpu_misc_node *node);

/* ───────── nvgpu_capture.c ───────── */

/* /dev/nvgpu-capture: probe (after HELLO and nvgpu_dri_init()) and remove. */
int nvgpu_capture_init(struct nvgpu_device *dev);
void nvgpu_capture_cleanup(struct nvgpu_device *dev);

/* ───────── nvgpu_wl.c ───────── */

/* /dev/nvgpu-wl: probe (after HELLO and nvgpu_dri_init()) and remove. */
int nvgpu_wl_init(struct nvgpu_device *dev);
void nvgpu_wl_cleanup(struct nvgpu_device *dev);
struct nvgpu_tbuf;
/*
 * A WL_RECV reply whose caller gave up: close the backend handles its
 * descriptors carry (a host lease among them). The transport's reaper only,
 * process context.
 */
unsigned int nvgpu_wl_reap_recv(struct nvgpu_device *dev,
                                const struct nvgpu_tbuf *resp, u32 used);

/* ═════════════════════════ Protocol v2 internal API ═════════════════════════
 *
 * Ownership: nvgpu_xfer.c (transport, HELLO, HOST_OP, WATCH, CLOSE),
 * nvgpu_tbuf.c (transport buffers), nvgpu_clock.c (TIME_SYNC and the host's
 * clock), nvgpu_events.c (event dispatch and the consumer registry; the
 * state they share is nvgpu_xfer.h), nvgpu_i2.c (the schema-driven IOCTL2
 * interpreter),
 * nvgpu_hostfile.c (backend handles as guest files), nvgpu_kms.c,
 * nvgpu_fence.c (with nvgpu_syncobj.c and nvgpu_semsurf.c; nvgpu_fence.h
 * what they share), nvgpu_nvkms.c, nvgpu_wl.c. Wire layouts are
 * nvgpu_wire.h.
 */

struct nvgpu_xfer;
struct nvgpu_events;
struct nvgpu_schema_set;
struct nvgpu_uvm_table;

/* ── Transport buffers ──
 *
 * Built from page chunks (at most 64 KiB each), never vmalloc, so a request of
 * any allowed size can be described to the ring in a bounded number of
 * scatter-gather entries whether or not indirect descriptors were negotiated.
 * nvgpu_tbuf_alloc() returns NULL on failure; the accessors return -EINVAL
 * for a range outside the buffer and -EFAULT for a bad user pointer.
 */
struct nvgpu_tbuf;
struct nvgpu_tbuf *nvgpu_tbuf_alloc(size_t len, gfp_t gfp);
void nvgpu_tbuf_free(struct nvgpu_tbuf *tb);
/* Run fn(arg) when `tb` is freed -- for a request buffer, once the host can
 * no longer act on it, even if its caller gave up (S-25). */
void nvgpu_tbuf_on_free(struct nvgpu_tbuf *tb, void (*fn)(void *arg),
                        void *arg);
size_t nvgpu_tbuf_len(const struct nvgpu_tbuf *tb);
int nvgpu_tbuf_write(struct nvgpu_tbuf *tb, size_t off, const void *src,
                     size_t len);
int nvgpu_tbuf_write_user(struct nvgpu_tbuf *tb, size_t off,
                          const void __user *src, size_t len);
int nvgpu_tbuf_read(const struct nvgpu_tbuf *tb, size_t off, void *dst,
                    size_t len);
int nvgpu_tbuf_read_user(const struct nvgpu_tbuf *tb, size_t off,
                         void __user *dst, size_t len);
int nvgpu_tbuf_zero(struct nvgpu_tbuf *tb, size_t off, size_t len);

/* nvgpu_xfer() flags */
#define NVGPU_XF_EXECUTOR (1u << 0) /* runs on a host executor: long timeout */

/*
 * Send one request and wait for its response. `req` starts with an
 * nvgpu_msg_hdr (req_id is filled in here). `resp` is zeroed before sending;
 * `*used_len` is what the device actually wrote. Returns 0, or -errno for a
 * transport failure (the response header's status is the caller's to read).
 *
 * If the wait is abandoned (timeout, fatal signal) the buffers stay owned by
 * the transport until the device returns them, and any backend handles the
 * late response created are closed (IOCTL2 fd/gem outs, HOST_OP results).
 * Callers must therefore not free `req`/`resp` themselves after -ETIMEDOUT or
 * -EINTR: ownership passes to the transport on those returns. (-EINTR is also
 * what a fatal signal while waiting for ring space returns; the transport
 * frees the unsent buffers then, so the rule has no exception.) -ENODEV: the
 * device is gone; the buffers are the caller's.
 *
 * On those two returns an IOCTL2's NVGPU_I2_FD_CONSUME handles are the
 * transport's too: it closes them if the call never ran (unsent, or answered
 * with a non-zero status), and the backend closes them if it did, so the
 * caller must not.
 *
 * NVGPU_XF_EXECUTOR waits up to 60 s, anything else 30 s, killable only:
 * host display calls are bounded but uninterruptible, like the native ioctls.
 */
int nvgpu_xfer(struct nvgpu_device *dev, struct nvgpu_tbuf *req,
               struct nvgpu_tbuf *resp, u32 flags, u32 *used_len);

/*
 * One request from plain kernel buffers, for the fixed-size and v1 messages.
 * The bytes are copied through transport buffers, so `req` and `resp` may be
 * any kernel memory (kvmalloc included) and are the caller's again whatever
 * this returns. `resp` reads as zero past what the device wrote.
 *
 * nvgpu_send_recv() fails with -EIO when the device wrote less than a header,
 * so a caller that reads only the header cannot mistake silence for success.
 * nvgpu_send_recv_used() leaves that to the caller, who gets the length and
 * must check every field it reads against it (nvgpu_resp_has()); a response
 * with no header at all (GET_PROC_FILES) has to use this one.
 */
int nvgpu_send_recv(struct nvgpu_device *dev, void *req, int req_len,
                    void *resp, int resp_len);
int nvgpu_send_recv_used(struct nvgpu_device *dev, void *req, int req_len,
                         void *resp, int resp_len, u32 *used_len);
/* nvgpu_send_recv_used(), with release(arg) run once the host can no longer
 * act on the request (see nvgpu_tbuf_on_free()); always run, failure or not. */
int nvgpu_send_recv_holding(struct nvgpu_device *dev, void *req, int req_len,
                            void *resp, int resp_len, u32 *used_len,
                            void (*release)(void *arg), void *arg);
/*
 * nvgpu_send_recv_used(), saying whether the request reached the ring
 * (`*sent`) and under which request id (`*req_id`, 0 if it never got one):
 * what a caller that hands something over on -EINTR/-ETIMEDOUT needs to know
 * to tell a request the host may still run from one it never will.
 */
int nvgpu_send_recv_sent(struct nvgpu_device *dev, void *req, int req_len,
                         void *resp, int resp_len, u32 *used_len, bool *sent,
                         u32 *req_id);
/* Does a response of `used` bytes contain all of [off, off + len)? */
static inline bool nvgpu_resp_has(u32 used, size_t off, size_t len) {
  return off <= used && len <= used - off;
}

/* ── Probe / remove (nvgpu_xfer.c), in the order they are called ── */
/* After virtio_find_vqs(): transport state, event buffers posted. */
int nvgpu_xfer_init(struct nvgpu_device *dev);
/* After virtio_device_ready(): HELLO, then TIME_SYNC and its resync work. */
void nvgpu_xfer_hello(struct nvgpu_device *dev);
/* Remove, device still live: stop the clock work, let queued CLOSEs out. */
void nvgpu_xfer_quiesce(struct nvgpu_device *dev);
/* Remove, after the device reset: fail waiters, reclaim every buffer. */
void nvgpu_xfer_reclaim(struct nvgpu_device *dev);
/* After del_vqs (remove, or a probe that failed): stop the work queue. The
 * state stays, dead, for whatever still holds the device. */
void nvgpu_xfer_destroy(struct nvgpu_device *dev);
/* The device's last reference: free what init allocated. Any context. */
void nvgpu_xfer_free(struct nvgpu_device *dev);
void nvgpu_ctrl_vq_cb(struct virtqueue *vq);
void nvgpu_event_vq_cb(struct virtqueue *vq);

/* ── Memory registered by its pages (nvgpu_osdesc.c) ── */
void nvgpu_osdesc_init(struct nvgpu_device *dev);
/*
 * An RM escape that registers memory the caller already has (ALLOC_MEMORY or
 * RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, VID_HEAP_CONTROL's
 * ALLOC_OS_DESCRIPTOR), on its block `outer` as the caller read it (`sz`
 * bytes, all of it): true if it was handled here, with *ret its result.
 * False for anything else, which goes the usual way with the same bytes.
 * nvgpu_rmio.c; with NVGPU_RUST, nvgpu_ioctl_fd() in Rust does this itself.
 */
bool nvgpu_osdesc_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                        void __user *uarg, const void *outer, unsigned int sz,
                        long *ret);
/* Whether an escape of number `nr` and size `sz` can be one of the three. */
bool nvgpu_osdesc_candidate(unsigned int nr, unsigned int sz);
/* Unpin what RM has let go of. Process context; cheap with nothing pinned. */
void nvgpu_osdesc_reap(struct nvgpu_device *dev);
/* remove(), after the reset: unpin everything. */
void nvgpu_osdesc_release_all(struct nvgpu_device *dev);
/* What a registration does with its pages, for whichever builds the call. */
bool nvgpu_osdesc_ok(const struct nvgpu_device *dev);
/* Pin the `npages` pages from the page-aligned `start`, all of them, into
 * `pages` (FOLL_LONGTERM, FOLL_WRITE for `write`), as RM would; 0 or -errno. */
int nvgpu_osdesc_pin(unsigned long start, unsigned long npages, bool write,
                     struct page **pages);
/* Keep them pinned under registration `id` (non-zero) until a reap names
 * it; the list and the array are then nvgpu_osdesc.c's. */
void nvgpu_osdesc_keep(struct nvgpu_device *dev, u64 id, struct page **pages,
                       unsigned long npages, bool write);
/*
 * Send a registration whose pages are pinned: nvgpu_send_recv_used(), except
 * that on -EINTR and -ETIMEDOUT the pins are no longer the caller's. A
 * request that never reached the ring had them unpinned at once; one that
 * did keeps them under its request id until its late reply says what RM
 * registered (nvgpu_osdesc_late()), or remove(). On any other return they
 * are still the caller's.
 */
int nvgpu_osdesc_send(struct nvgpu_device *dev, void *req, int req_len,
                      void *resp, int resp_len, u32 *used,
                      struct page **pages, unsigned long npages, bool write);
/* The transport's reaper: request `req_id`, a registration its caller gave
 * up on, came back naming registration `id` (0: none was made). */
void nvgpu_osdesc_late(struct nvgpu_device *dev, u32 req_id, u64 id);
/* Unpin them (dirtied for `write`) and free the array. */
void nvgpu_osdesc_unpin(struct page **pages, unsigned long n, bool write);

/* ── HOST_OP / WATCH / CLOSE ── */
/* A HOST_OP result that names a backend handle: nonzero and 32-bit, into
 * *h; false for anything else, which the caller answers -EPROTO. */
static inline bool nvgpu_res_u32(u64 v, u32 *h) {
  if (!v || v > U32_MAX)
    return false;
  *h = (u32)v;
  return true;
}
int nvgpu_host_op(struct nvgpu_device *dev, u32 op, const u64 *args,
                  u32 nargs, u64 *res, u32 nres);
/*
 * nvgpu_host_op() for an op whose reply carries more after the fixed part
 * (INJECT_OPEN): up to `tail_len` bytes of it into `tail`, and how many the
 * backend sent in `*tail_used`. A caller that gives up still has the op's
 * results reaped (nvgpu_reap_host_op()).
 */
int nvgpu_host_op_tail(struct nvgpu_device *dev, u32 op, const u64 *args,
                       u32 nargs, u64 *res, u32 nres, void *tail,
                       u32 tail_len, u32 *tail_used);
int nvgpu_watch(struct nvgpu_device *dev, u32 handle, u32 flags, u64 cookie);
int nvgpu_unwatch(struct nvgpu_device *dev, u32 handle);
/*
 * CLOSE / GEM_CLOSE, waiting for the answer; process context. Neither can be
 * lost: a request that never reached the ring (a fatal signal while the ring
 * was full, no memory) is handed to the _async variant instead.
 */
int nvgpu_close_handle(struct nvgpu_device *dev, u32 handle);
int nvgpu_gem_close(struct nvgpu_device *dev, u32 file_handle, u32 gem);
/* From any context: queued on a workqueue that holds a module reference. */
void nvgpu_close_handle_async(struct nvgpu_device *dev, u32 handle);
/* A W_ARM of `nfd`'s handle, sent from a work item on nvgpu_wq: poll()
 * cannot wait for it. False if it could not be queued. Process context. */
bool nvgpu_arm_ready_async(struct nvgpu_fd *nfd);
void nvgpu_gem_close_async(struct nvgpu_device *dev, u32 file_handle,
                           u32 gem);
/* The same, and release(arg) once the host can no longer act on the close:
 * after its answer, or when it is known never to run. Process context. */
void nvgpu_gem_close_then(struct nvgpu_device *dev, u32 file_handle, u32 gem,
                          void (*release)(void *arg), void *arg);
/*
 * MUNMAP: give back one window placement an MMAP reply handed out, through
 * the handle it was made on. The backend counts a reference per MMAP reply,
 * so exactly one of these per reply, never per vma. Process context; like
 * CLOSE, a request that never reached the ring is queued instead, not lost.
 */
int nvgpu_munmap(struct nvgpu_device *dev, u32 handle, u32 mapping_id);

/*
 * The memory type an MMAP reply asks a placement to be mapped with
 * (NVGPU_MMAP_CACHE_*), applied to `prot`; write-combining for a v1 backend,
 * which leaves the field zero. nvgpu_main.c.
 */
pgprot_t nvgpu_window_pgprot(struct nvgpu_device *dev, u8 caching,
                             pgprot_t prot);

/* ── Clock ── */
s64 nvgpu_host_to_guest_ns(struct nvgpu_device *dev, s64 host_ns);
s64 nvgpu_guest_to_host_ns(struct nvgpu_device *dev, s64 guest_ns);
/* A host CLOCK_REALTIME / CLOCK_MONOTONIC_RAW reading in the guest's clock of
 * the same id; false (value untouched) without TIME_SYNC's long form. */
bool nvgpu_host_clock_to_guest(struct nvgpu_device *dev, clockid_t clk,
                               s64 host_ns, s64 *guest_ns);

/* ── Event consumers ──
 *
 * EVENT_DATA records are dispatched from the event virtqueue callback, i.e.
 * hard IRQ context. A consumer's callback must not sleep; anything that does
 * (uevents, CLOSE, UNWATCH) goes to a work item. Registration and
 * unregistration are safe from process context; once nvgpu_ev_unregister()
 * returns the callback is not running and will not run again.
 */
struct nvgpu_ev_consumer {
  /* rec->kind, rec->cookie, payload of rec->len bytes */
  void (*deliver)(struct nvgpu_ev_consumer *c, u32 kind, u64 cookie,
                  const void *payload, u32 len);
  /* private to the registry */
  u64 key;
  struct hlist_node node;
};
/* EV_DRM and legacy EV_READY: keyed by backend handle. EV_FENCE / one-shot
 * EV_READY: keyed by WATCH cookie. EV_HOTPLUG: keyed by card index. */
#define NVGPU_EVKEY_HANDLE(h) ((u64)(h))
#define NVGPU_EVKEY_COOKIE(c) ((u64)(c) | (1ull << 63))
#define NVGPU_EVKEY_CARD(i) ((u64)(i) | (1ull << 62))
int nvgpu_ev_register(struct nvgpu_device *dev, struct nvgpu_ev_consumer *c,
                      u64 key);
void nvgpu_ev_unregister(struct nvgpu_device *dev, struct nvgpu_ev_consumer *c);
/* Never below 2^32, so a WATCH cookie can never be read as a legacy handle. */
u64 nvgpu_ev_new_cookie(struct nvgpu_device *dev);
/*
 * Release of a DRM file: under the registry lock, clear nfd->drm_file (so no
 * consumer can reach the dying drm_file) and take nfd->kms_handle. Returns
 * false if the file was already detached. The caller CLOSEs the KMS handle.
 */
bool nvgpu_fd_detach_drm(struct nvgpu_fd *nfd, u32 *kms_handle);

/* ── IOCTL2 interpreter (nvgpu_i2.c) ── */

/*
 * A reply header's status is the backend's word on what the call returned: 0
 * or a -errno. Anything else -- positive, or below -MAX_ERRNO -- would reach
 * the caller as an ioctl result no native driver returns, so every reader of
 * a status fails it with -EPROTO (IOCTL2 here and in nvgpu_kms.c, the v1
 * exchange in nvgpu_v1.c, the fixed messages in nvgpu_xfer.c).
 */
static inline bool nvgpu_status_valid(s32 status) {
  return status <= 0 && status >= -MAX_ERRNO;
}

/*
 * The fixed head of an IOCTL2 request and of its reply. The interpreter
 * builds the one and reads the other (nvgpu_i2.c; i2.rs's build() and
 * parse()), and so does nvgpu_kms.c for the calls it makes of its own.
 */
struct nvgpu_i2_head {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_req req;
} __packed;

struct nvgpu_i2_rhead {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_resp resp;
} __packed;

/* Everything of the head but the record counts, which start at 0. */
static inline void nvgpu_i2_head_init(struct nvgpu_i2_head *h, u32 handle,
                                      unsigned int cmd, u32 render, u32 nbuf,
                                      u32 data_len) {
  memset(h, 0, sizeof(*h));
  h->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL2);
  h->hdr.handle = cpu_to_le32(handle);
  h->req.cmd = cpu_to_le32(cmd);
  h->req.nbuf = cpu_to_le32(nbuf);
  h->req.data_len = cpu_to_le32(data_len);
  h->req.render = cpu_to_le32(render);
}

/* Schema classes: which kind of host file the call targets. */
#define NVGPU_SCLASS_RENDER 1
#define NVGPU_SCLASS_KMS 2
#define NVGPU_SCLASS_MODESET 3

struct nvgpu_i2_call;

/*
 * Hooks the calling subsystem supplies. Each may be NULL if the schema entry
 * cannot have that kind of field (the interpreter fails the call with -EINVAL
 * if one is needed and missing).
 */
struct nvgpu_i2_ops {
  /* A descriptor field. `user_value` is what the caller wrote there.
   * Return 0 and set *handle (and *flags, e.g. NVGPU_I2_FD_CONSUME) to send
   * it, 1 for "no descriptor" (the field is sent as -1), or -errno. */
  int (*fd_in)(struct nvgpu_i2_call *call, u32 buf, u32 off, s64 user_value,
               u32 kinds, u32 *handle, u32 *flags);
  /* A GEM handle field: guest handle → the proxy's (owner, host gem). */
  int (*gem_in)(struct nvgpu_i2_call *call, u32 buf, u32 off,
                u32 guest_handle, u32 *owner, u32 *gem);
  /* The host produced a descriptor, now backend handle `handle` of `kind`.
   * Materialise it and return the value to write in the caller's field. On
   * error the interpreter closes the handle. */
  int (*fd_out)(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
                u32 kind, s64 *user_value);
  /* The host produced a GEM handle, valid in call->render. Make a proxy. */
  int (*gem_out)(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 gem,
                 u64 size, u32 *guest_handle);
  /* Schema specials (e.g. "atomic"): called after gathering (phase 0, may add
   * dyn records / fd records) and after the reply (phase 1). */
  int (*special)(struct nvgpu_i2_call *call, u32 special_id, int phase);
  /*
   * Every call, whatever its entry. Phase 0: after gathering, translation and
   * special(0), before the request is built -- the kernel copies
   * (nvgpu_i2_buf()) are what will be sent and may be rewritten; an error
   * refuses the call unsent. Phase 1: as soon as a reply has been parsed,
   * before GEM/descriptor outputs, special(1) and copy-back; call->ret is the
   * host's result, and the hook may change it (it is what the caller gets
   * back) or rewrite the kernel copies that will be copied back. An error
   * there drops the reply's outputs and is returned. Not called in phase 1
   * when no reply was parsed (refused, transport failure, abandoned).
   */
  int (*phase)(struct nvgpu_i2_call *call, int phase);
};

struct nvgpu_i2_call {
  struct nvgpu_device *dev;
  u32 handle; /* target backend handle */
  u32 render; /* render handle of the calling guest file */
  u32 sclass; /* NVGPU_SCLASS_* */
  unsigned int cmd;
  void __user *uarg;
  u32 xflags; /* NVGPU_XF_* */
  /*
   * `uarg`, and every pointer in the argument, are kernel addresses: a call
   * the driver builds itself (a syncobj wait turned into a poll, a rewritten
   * import). Copy-back lands in that kernel memory, and the caller copies to
   * userspace what the native ioctl would have.
   */
  bool kernel;
  /*
   * `uarg` alone is a kernel address: the argument, copied in by the DRM
   * node's entry as drm_ioctl() does (struct nvgpu_drm_arg), with every
   * pointer in it the caller's. Its copy-back lands in that kernel copy.
   */
  bool karg;
  const struct nvgpu_i2_ops *ops;
  void *priv;
  /* filled in: */
  s32 ret;                 /* host ioctl result */
  struct nvgpu_i2_state *st; /* interpreter-private, valid during hooks */
};

/*
 * For a gem_in or fd_in hook: keep `obj` (a reference the hook took) until
 * the host is done with the request, then put(obj) -- after the reply, or,
 * for a request its caller gave up on, once the transport knows it can no
 * longer run. On failure put(obj) has run already.
 */
int nvgpu_i2_hold(struct nvgpu_i2_call *call, void (*put)(void *obj),
                  void *obj);

/*
 * For an fd_in hook: whether a file of ours of `device_type` (NVGPU_DEV_*)
 * may stand in a descriptor field that allows `kinds` (NVGPU_SKIND*), as the
 * backend's schema::kind_allowed() decides it. nvgpu_schema.c.
 */
bool nvgpu_fd_kind_allowed(u32 device_type, u32 kinds);

/* Is there a schema for this call? (Used to decide whether to intercept.) */
bool nvgpu_i2_has_schema(struct nvgpu_device *dev, u32 sclass,
                         unsigned int cmd, const void *arg_prefix,
                         size_t prefix_len);
/*
 * The DRM-class entry numbered as `cmd` (by type and number, the one of
 * exactly `cmd` if there is one, as the interpreter looks it up): its own
 * command, the native size and direction, which the caller's argument is
 * normalised to (struct nvgpu_drm_arg); 0 if there is none.
 */
unsigned int nvgpu_i2_native_cmd(struct nvgpu_device *dev, u32 sclass,
                                 unsigned int cmd);
/*
 * Run an ioctl through IOCTL2: find the schema, gather the caller's buffers,
 * translate fd/GEM fields through the hooks, send, copy every OUT buffer back
 * per the schema's copy-back rule (also when the host call failed), and
 * materialise fd/GEM outputs. Returns the host ioctl's result (0 or -errno) or
 * a transport/validation -errno.
 */
long nvgpu_i2_ioctl(struct nvgpu_i2_call *call);
/* For specials: add a dyn record / an fd record while gathering (phase 0). */
int nvgpu_i2_add_dyn(struct nvgpu_i2_call *call, u32 kind, u32 buf, u32 off,
                     u32 len);
int nvgpu_i2_add_fd(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
                    u32 flags);
/* For specials: the kernel copy of buffer `buf` (NULL if none). */
void *nvgpu_i2_buf(struct nvgpu_i2_call *call, u32 buf, u32 *len);

/* nvgpu_schema.c: the generated tables' one copy. */
/* The set to use before HELLO has picked the host's (DRM only). */
const struct nvgpu_schema_set *nvgpu_schema_default(void);
/* ── An atomic commit's arrays (nvgpu_atomic.c, or nvgpu_rs.rs) ── */

/* struct drm_mode_atomic, as the parse reads it (nvgpu_kms.c asserts it). */
#define NVGPU_ATOMIC_SIZE 56
#define NVGPU_ATOMIC_FLAGS 0
#define NVGPU_ATOMIC_COUNT_OBJS 4
#define NVGPU_ATOMIC_OBJS_PTR 8
#define NVGPU_ATOMIC_COUNT_PROPS_PTR 16
#define NVGPU_ATOMIC_PROPS_PTR 24
#define NVGPU_ATOMIC_VALUES_PTR 32
#define NVGPU_ATOMIC_USER_DATA 48
#define NVGPU_ATOMIC_FLIP_EVENT 0x01u  /* DRM_MODE_PAGE_FLIP_EVENT */
#define NVGPU_ATOMIC_TEST_ONLY 0x100u  /* DRM_MODE_ATOMIC_TEST_ONLY */
/* Events reserved per commit: "more CRTCs than any GPU has"; anything past
 * it is reserved when its event arrives instead. */
#define NVGPU_ATOMIC_MAX_EVENTS 32
/* CRTC_ID assignments one commit may teach nvgpu_kms.c. */
#define NVGPU_ATOMIC_MAX_LEARN 64

/* What a property is, as far as the parse cares (by name, like the backend). */
#define NVGPU_KPROP_PLAIN 1
#define NVGPU_KPROP_CRTC_ID 2  /* plane/connector CRTC_ID: a CRTC joins the commit */
#define NVGPU_KPROP_IN_FENCE 3 /* a sync_file descriptor, -1 none */
#define NVGPU_KPROP_OUT_PTR 4  /* a user pointer the kernel writes an fd to */
/* What an object is: a CRTC, or not -- then with the CRTC it is on. */
#define NVGPU_KOBJ_CRTC 1
#define NVGPU_KOBJ_OTHER 2

/* What the parse asks of nvgpu_kms.c. `st` is the interpreter's state for
 * the call, to be call->st while a hook that reaches the buffers runs. */
struct nvgpu_atomic_ops {
  /* NVGPU_KOBJ_* with the CRTC a non-CRTC is on (0: not known), or -errno. */
  int (*obj_class)(void *ctx, u32 obj, u32 *crtc);
  /* NVGPU_KPROP_*, or a negative error (the host's, for an unknown id). */
  int (*prop_class)(void *ctx, u32 id);
  /* IN_FENCE_FD with a value other than -1, at `off` of buffer `buf`. */
  int (*in_fence)(void *ctx, void *st, u32 buf, u32 off, s64 fd);
  /* A *_PTR property with a nonzero value. */
  int (*out_fence)(void *ctx, void *st, u32 buf, u32 off, u64 uptr);
  /* Object `obj` is put on CRTC `crtc` (0: off), if the commit goes. */
  void (*learn)(void *ctx, u32 obj, u32 crtc);
  /* A flip event for `crtc`. */
  int (*reserve)(void *ctx, u32 crtc, u64 user_data);
};

struct nvgpu_atomic_out {
  bool commit;    /* not TEST_ONLY */
  u32 values_buf; /* prop_values' buffer, 0 for none */
};

/*
 * From the ATOMIC special's phase 0: walk the commit's arrays in the call's
 * kernel copies, asking `ops` what its objects and properties are, reserving
 * the flip events and bridging the fences. 0 or -errno. `out->commit` is
 * written before any hook runs (both implementations: the C at once, the
 * Rust through atomic::Env::begin; the difftest checks what a hook sees),
 * and is the flag's one source -- nvgpu_kms.c's hooks read it through their
 * context; `out->values_buf` by the end.
 */
int nvgpu_atomic_parse(struct nvgpu_i2_call *call, bool fences,
                       const struct nvgpu_atomic_ops *ops, void *ctx,
                       struct nvgpu_atomic_out *out);

/* Selects the schema set for the host driver version (NULL: none). */
const struct nvgpu_schema_set *nvgpu_schema_select(const char *driver_version);
/* The UVM block sizes for the host driver version (NULL: UVM refused). */
const struct nvgpu_uvm_table *nvgpu_uvm_select(const char *driver_version);

#endif /* NVGPU_H */
