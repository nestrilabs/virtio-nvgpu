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
#include <linux/spinlock.h>
#include <linux/types.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/wait.h>

#include <drm/drm_gem.h>

#include "nvgpu_wire.h"

struct drm_file;

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
  struct nvgpu_device *dev;
  /* sysfs drm tree under the PCI device — required by Vulkan ICD */
  struct kobject *drm_kobj;      /* .../pci_addr/drm          */
  struct kobject *drm_node_kobj; /* .../pci_addr/drm/<name>   */
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
  struct nvgpu_event_buf *event_bufs; /* defined with the event queue, nvgpu_main.c */

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

/* ───────── nvgpu_main.c ───────── */

int nvgpu_send_recv(struct nvgpu_device *dev, void *req, int req_len,
                    void *resp, int resp_len);
long nvgpu_ioctl_fd(struct nvgpu_fd *nfd, unsigned int cmd, unsigned long arg);
long nvgpu_ioctl_flat_h(struct nvgpu_device *dev, u32 handle, unsigned int cmd,
                        void *kbuf, u32 sz);
int nvgpu_handle_for_fd(int guest_fd, u32 *handle);
void nvgpu_fd_register(struct nvgpu_device *dev, struct nvgpu_fd *nfd);
void nvgpu_fd_unregister(struct nvgpu_device *dev, struct nvgpu_fd *nfd);

/* ───────── nvgpu_drm.c ───────── */

int nvgpu_dri_init(struct nvgpu_device *dev);
void nvgpu_dri_cleanup(struct nvgpu_device *dev);

#endif /* NVGPU_H */
