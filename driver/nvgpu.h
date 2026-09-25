/* SPDX-License-Identifier: GPL-2.0 */
/*
 * virtio-gpu-nv: state and prototypes shared between the objects that make up
 * virtio_gpu_nv.ko. Anything used by one file only stays in that file.
 */

#ifndef NVGPU_H
#define NVGPU_H

#include <linux/atomic.h>
#include <linux/cdev.h>
#include <linux/completion.h>
#include <linux/fs.h>
#include <linux/kobject.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/refcount.h>
#include <linux/spinlock.h>
#include <linux/types.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/wait.h>
#include <linux/xarray.h>

#include <drm/drm_gem.h>

#include "nvgpu_wire.h"

struct drm_file;
struct nvgpu_kms_file;

/* ───────── Driver state ───────── */

struct nvgpu_device;

/* Module parameters read outside nvgpu_main.c; see their definitions there. */
extern int nvgpu_claim_alloc;
extern int nvgpu_claim_sync_fd;

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
  /* The registered DRM device, which owns the node and its sysfs tree. */
  struct drm_device *drm;
  bool registered;
  u32 index;
  /* Index into nvgpu_device.cards of this device's host card node, or -1. */
  int card_index;
  struct nvgpu_device *dev;
  /* sysfs drm tree under the PCI device — required by Vulkan ICD */
  struct kobject *drm_kobj;      /* .../pci_addr/drm          */
  struct kobject *drm_node_kobj; /* .../pci_addr/drm/<name>   */
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

  /* GPU slots read from config space at probe */
  struct virtio_gpu_nv_gpu_slot gpu_slots[8];

  /* FD translation table received from backend */
  struct nvgpu_fd_translation_entry fd_translations[16];
  u32 num_fd_translations;

  /* Every open descriptor, so an event naming a handle can find its file. */
  struct list_head fds;
  spinlock_t fds_lock;

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
};

#define to_nvgpu_gem(o) container_of(o, struct nvgpu_gem_object, base)

/* ───────── nvgpu_main.c ───────── */

long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd, unsigned long arg);
long nvgpu_ioctl_flat_h(struct nvgpu_device *dev, u32 handle, unsigned int cmd,
                        void *kbuf, u32 sz);
/*
 * The backend handle standing for one of this module's open files: an
 * /dev/nvidia* character device, a DRM node of ours (its render handle), or a
 * host-handle file (nvgpu_hostfile_handle()). -EBADF for anything else --
 * never a guess at another driver's private_data.
 */
int nvgpu_handle_for_fd(int guest_fd, u32 *handle);
/* The nvgpu_fd behind a character device or DRM file of ours, else NULL. */
struct nvgpu_fd *nvgpu_fd_from_file(struct file *f);
void nvgpu_fd_register(struct nvgpu_device *dev, struct nvgpu_fd *nfd);
void nvgpu_fd_unregister(struct nvgpu_device *dev, struct nvgpu_fd *nfd);
void nvgpu_fd_get(struct nvgpu_fd *nfd);
/* Drops a reference; the last one CLOSEs the backend handle and frees. */
void nvgpu_fd_put(struct nvgpu_fd *nfd);

/* ───────── nvgpu_drm.c ───────── */

int nvgpu_dri_init(struct nvgpu_device *dev);
void nvgpu_dri_cleanup(struct nvgpu_device *dev);
/* The nvgpu_fd of a DRM file of this driver, else NULL. */
struct nvgpu_fd *nvgpu_drm_file_nfd(struct file *f);
/*
 * Stand a guest GEM object in front of host object `host_handle` of `owner`'s
 * host file, and return a handle for it in `file`. The proxy takes a
 * reference on `owner`. On failure the host handle has already been closed
 * (by the proxy's own free, where one was made): the caller must not close it
 * again -- except for -EEXIST, which means a proxy for that host handle
 * already exists and the handle is left alone, being that proxy's.
 */
int nvgpu_gem_proxy_create(struct drm_file *file, struct nvgpu_fd *owner,
                           u32 host_handle, size_t size, u32 *guest_handle);
/* Guest handle in `file` -> (host GEM handle, owner backend handle). */
int nvgpu_gem_to_host(struct drm_file *file, u32 guest_handle,
                      u32 *host_handle, u32 *owner_handle);
/*
 * The proxy already standing for host GEM handle `host_handle` of `owner`'s
 * render file, with a reference taken, or NULL. nvgpu_gem_proxy_create()
 * refuses (-EEXIST, leaving the handle alone: it is that proxy's) to make a
 * second one, so a caller whose host handle may be one the file already had
 * -- anything that PRIME-imports on the host -- looks here first.
 */
struct drm_gem_object *nvgpu_gem_proxy_find(struct nvgpu_fd *owner,
                                            u32 host_handle);
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
 * DRM file of ours), as a new guest dma-buf descriptor (@o_flags: O_CLOEXEC |
 * O_RDWR), or -errno. Owns @host_gem unless it returns -EBADF (not our file).
 */
int nvgpu_dmabuf_from_host(struct file *drm_filp, u32 host_gem, u64 size,
                           int o_flags);

/* Guest handle in `file` -> the proxy itself, referenced (drop it with
 * drm_gem_object_put(&ng->base)), or NULL for anything that is not one. */
struct nvgpu_gem_object *nvgpu_gem_lookup(struct drm_file *file,
                                          u32 guest_handle);

/* ───────── nvgpu_kms.c ───────── */

/*
 * The KMS side of a guest DRM file (DESIGN §4). A file has one if it is an
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

/* ───────── nvgpu_nvkms.c ───────── */

/* /dev/nvidia-modeset on a v2 device: NVKMS through IOCTL2, and the
 * readiness NVKMS keeps until GET_NEXT_EVENT/CLEAR_UNICAST_EVENT. */
long nvgpu_nvkms_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                       void __user *uarg);
__poll_t nvgpu_nvkms_poll(struct nvgpu_fd *nfd, struct file *filp,
                          struct poll_table_struct *wait);

/* ───────── nvgpu_hostfile.c ───────── */

/* The backend handle behind a host-handle file, or -EBADF if `f` is not one. */
int nvgpu_hostfile_handle(struct file *f, u32 *handle);
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

/* ───────── nvgpu_fence.c ───────── */

/*
 * Fences live on the host (DESIGN §6): a guest sync_file this driver makes
 * wraps an nvgpu host fence, a proxy dma_fence for a host sync_file that
 * signals (with the host's error, if any) when the host's does.
 */

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
 *      our fences) -- pass it, never close it; *owned true: a new handle made
 *      for this call (a merge of our fences) -- pass it with
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
                          bool *owned);
/* The core syncobj ioctls (0xBF-0xCF), when nvgpu_fences_enabled(). */
bool nvgpu_fence_is_syncobj_ioctl(unsigned int cmd);
long nvgpu_fence_syncobj_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, unsigned long arg);
/* nvidia-drm SEMSURF_FENCE_* (0x54-0x57), when nvgpu_fences_enabled(). */
long nvgpu_fence_semsurf_ioctl(struct nvgpu_fd *nfd, struct drm_file *file,
                               unsigned int cmd, void __user *uarg);
/* A GEM proxy is going: close what SEMSURF_FENCE_ATTACH moved into other
 * files for it. */
void nvgpu_fence_gem_free(struct nvgpu_gem_object *ng);

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
 * Ownership: nvgpu_xfer.c (transport, HELLO, TIME_SYNC, HOST_OP, WATCH, event
 * dispatch), nvgpu_i2.c (the schema-driven IOCTL2 interpreter),
 * nvgpu_hostfile.c (backend handles as guest files), nvgpu_kms.c,
 * nvgpu_fence.c, nvgpu_nvkms.c, nvgpu_wl.c. Wire layouts are nvgpu_wire.h.
 */

struct nvgpu_xfer;
struct nvgpu_events;
struct nvgpu_schema_set;

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
/* After del_vqs (remove, or a probe that failed): free what init allocated. */
void nvgpu_xfer_destroy(struct nvgpu_device *dev);
void nvgpu_ctrl_vq_cb(struct virtqueue *vq);
void nvgpu_event_vq_cb(struct virtqueue *vq);

/* ── HOST_OP / WATCH / CLOSE ── */
int nvgpu_host_op(struct nvgpu_device *dev, u32 op, const u64 *args,
                  u32 nargs, u64 *res, u32 nres);
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
void nvgpu_gem_close_async(struct nvgpu_device *dev, u32 file_handle,
                           u32 gem);
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
  const struct nvgpu_i2_ops *ops;
  void *priv;
  /* filled in: */
  s32 ret;                 /* host ioctl result */
  struct nvgpu_i2_state *st; /* interpreter-private, valid during hooks */
};

/* Is there a schema for this call? (Used to decide whether to intercept.) */
bool nvgpu_i2_has_schema(struct nvgpu_device *dev, u32 sclass,
                         unsigned int cmd, const void *arg_prefix,
                         size_t prefix_len);
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

/* Selects the schema set for the host driver version (NULL: none). */
const struct nvgpu_schema_set *nvgpu_schema_select(const char *driver_version);

#endif /* NVGPU_H */
