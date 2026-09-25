# `driver/` — guest kernel module

**License: GPL-2.0** (`LICENSE-GPL-2.0`), required for kernel symbol access.

A Linux kernel module for the guest. It registers the NVIDIA character
devices — `/dev/nvidiactl`, `/dev/nvidia0`…`/dev/nvidiaN`, `/dev/nvidia-uvm`,
`/dev/nvidia-modeset` — a DRM device per GPU (render node and card node), and
`/dev/nvgpu-wl`, the guest end of the Wayland proxy's channel. It forwards
`ioctl()` and `mmap()` against them over the virtqueue.

The module is deliberately **not ABI-aware**. It moves bytes and manages
mappings; every decision about what an ioctl *means* belongs in `device/`.
For DRM KMS, syncobj and fence calls, nvidia-drm's permission grants and NVKMS,
where an ioctl's argument points at more memory or names a descriptor or GEM
handle, the module follows a table generated in `gen/` to gather it (IOCTL2),
and the backend holds the request to its own copy of the same table. RM
escapes, UVM and nvidia-drm's GEM calls go as protocol v1's IOCTL message from
every guest, with RM's embedded pointers carried as nested and deep blocks,
which the backend holds to its own RM control table; where one block holds
several pointers, a deep segment each (`gen/nvgpu_rm_deep.h`, rendered with
the backend's table), to a backend that offers them. Keeping the module dumb is
what keeps it stable across NVIDIA driver releases.

The one call it does more for is the registration of memory the caller already
has (`nvgpu_osdesc.c`): RM pins such memory by CPU address, which in the
backend would be the VMM's, so the module pins the caller's range itself, as
RM would, and sends the guest-physical page list with the call. The pages stay
pinned until the backend says RM has let go of them, which the module asks
after an RM_FREE, a close and before the next registration.

It also says which process makes each RM_ALLOC and RM_DUP_OBJECT, to a
backend that asks (`NVGPU_BCAP_PROC_ID`): 16 bytes after the call's blocks,
the thread group's leader by its initial-namespace PID and start time. On the
host every guest process is the backend, so this is how the backend keeps RM
objects to the guest process that made their client, as RM would
(`device/src/rmshare.rs`). The module makes no RM client of its own.

Shared wire-format and ABI definitions live in `protocol/` and are dual
licensed so this module can include the same headers the Rust side uses.

Built out of tree against the guest kernel, or copied/submoduled into a kernel
tree by whoever is assembling a guest image.

**Protocol v2 is untested in a guest.** The module speaks v2 when the backend
answers its HELLO, and falls back to v1 — no display features — when it does
not. The v2 code compiles against the 7.2 guest kernel and has never been
loaded; its IOCTL2 interpreter is exercised only through a Rust
transliteration (`device/src/i2_e2e.rs`) that has to be kept in step with
`nvgpu_i2.c` by hand, and through `rust/difftest`, which runs `nvgpu_i2.c`
itself against its Rust port.

**The parsers of guest-process input have a Rust implementation**
(`NVGPU_RUST=1`, needing a kernel with `CONFIG_RUST=y`): the IOCTL2 walk, the
v1 IOCTL marshalling and descriptor translation, deep segments and the
OS-descriptor registrations. The C is the default until the Rust has passed
the hardware regression; [`rust/README.md`](rust/README.md) has what differs,
how to build and test each, and how to delete the C afterwards.

One module, `virtio_gpu_nv.ko`, built from several objects (see `Makefile`):

| file | contents |
|---|---|
| `nvgpu.h` | internal header: shared structs, cross-file prototypes, module parameter `extern`s |
| `nvgpu_wire.h` | wire protocol and config-space layout (BSD-3-Clause OR GPL-2.0+, mirrors `protocol/`) |
| `nvgpu_main.c` | probe/remove, virtqueues, `/dev/nvidia*` cdevs, mmap with each placement's memory type and writability (UVM semaphore pools from the UVM aperture, shared memory region 2, found before HELLO and offered in it, at host addresses in [4 GiB, 32 TiB) only), `/proc`, sysfs, fake PCI, v1 nvidia-modeset |
| `nvgpu_rmio.c` | the protocol-v1 IOCTL message, in C: the ioctl dispatcher for `/dev/nvidia*` and DRM driver-range calls, RM forwarding (descriptors and OS events translated to backend handles, deep pointers and deep segments, GPU/CPU time correlation moved into the guest's clocks, the calling process on RM_ALLOC and RM_DUP_OBJECT), UVM, v1 nvidia-modeset, and what reads an OS-descriptor registration and builds its page list. Built with `NVGPU_RUST=0` (the default) |
| `nvgpu_osdesc.c` | memory the caller already has, registered with RM by its pages: ALLOC_MEMORY and RM_ALLOC of the OS-descriptor class and VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR pin the caller's range as RM would and send its guest-physical runs; the pins last until a reap (HOST_OP OSDESC_REAP) names the registration, or remove() |
| `nvgpu_drm.c` | DRM device registration, GEM proxies, PRIME, nvidia-drm driver-range ioctls |
| `nvgpu_xfer.c` | protocol v2 transport: request contexts and transport buffers, HELLO and the host clock, HOST_OP / WATCH / CLOSE, the event queue and its consumer registry, EV_HOTPLUG uevents |
| `nvgpu_i2.c` | the schema-driven IOCTL2 interpreter, in C: gathers a caller's buffers per `gen/nvgpu_schema.h`, translates descriptors and GEM handles through per-caller hooks, copies replies back by the kernel's own rules. Built with `NVGPU_RUST=0` |
| `nvgpu_schema.c` | the generated IOCTL2 and UVM tables' one copy, and which a host gets |
| `nvgpu_atomic.c` | an ATOMIC commit's arrays, in C: which CRTCs get flip events and which values are fences, asking `nvgpu_kms.c` what objects and properties are. Built with `NVGPU_RUST=0` |
| `nvgpu_rs.rs`, `nvgpu_rs_glue.c`, `nvgpu_rs.h`, `rust/` | with `NVGPU_RUST=1`, in place of `nvgpu_i2.c`, `nvgpu_rmio.c` and `nvgpu_atomic.c`: the same parsers in Rust (`rust/core`, no `unsafe`, no panic path), the one Rust file with `unsafe` around them, the C they call, and the ABI; `rust/difftest` runs both on the same inputs. See [`rust/README.md`](rust/README.md) |
| `nvgpu_hostfile.c` | backend handles as guest files (anonymous inodes, e.g. a host syncobj's `syncobj_file`): closing one closes the host's, passing one names it again |
| `nvgpu_kms.c` | the KMS side of guest DRM files: host card and lease handles, lease adoption (`nvgpu_adopt_drm_file`), KMS ioctls through IOCTL2, master mirroring, flip/vblank events and clocks, ATOMIC fence properties |
| `nvgpu_fence.c` | fences: host sync_files behind guest `dma_fence` proxies, unwrapping guest fences for the host, the syncobj ioctls (waits that sleep here, not on the host), nvidia-drm semaphore-surface fences |
| `nvgpu_nvkms.c` | `/dev/nvidia-modeset` on protocol v2: NVKMS through IOCTL2 by the host release's own tables, descriptor translation, and the level readiness NVKMS clients poll |
| `nvgpu_wl.c`, `uapi/nvgpu_wl.h` | `/dev/nvgpu-wl`, the guest end of the Wayland channel: HELLO (device map, clock offset), CONNECT, SEND/RECV frames, and the descriptors only the kernel can name (dma-bufs, syncobjs, adopted DRM files) |
| `nvgpu_rm_intercepts.h`, `gen/nvgpu_rmalloc_classes.h`, `gen/nvgpu_v1v2_rewrites.h` | RM command tables (generated / hand-kept) |
| `gen/nvgpu_rm_deep.h` | the RM controls, and IDLE_CHANNELS, whose several pointers go as deep segments, and how much RM copies through each; generated by `gen/rmctrl_extract.py` with the backend's copy; never edited by hand |
| `gen/nvgpu_schema.h` | IOCTL2 schema tables (DRM render/KMS, nvidia-drm, NVKMS per release) and UVM parameter block sizes per release, generated by `gen/schema_gen.py`; never edited by hand |

Other files: `Makefile` and `Kconfig` (in-tree and out-of-tree builds),
`guest-kernel.config` (the configuration of the guest kernel it is built
against).

## Module parameters

| parameter | default | what it does |
|---|---|---|
| `wl_mode` | `0660` | permissions of `/dev/nvgpu-wl*`, created `root:root`. Every open is a client of the host compositor, so the node is for the Wayland daemon alone; `scripts/70-nvgpu-wl.rules` gives it the `nvgpu-wl` group. `0666` is the old, open behaviour. |
| `poll_events` | `1` | a wait on a device descriptor really waits for the host's event; `0` is the old behaviour, which spins |
| `poll_spin_us` | `0` | microseconds to spin before sleeping for an event; measured not to help, kept as a knob |
| `claim_alloc` | `1` | GET_DEV_INFO reports `supports_alloc` (ANDed with the host's) |
| `claim_sync_fd` | `0` | GET_DEV_INFO reports `supports_sync_fd` (ANDed with the host's) — only without fences (a v1 backend); with fences it follows the host's semaphore-surface bit and this is ignored |
| `virtio_id` | `45` | virtio device ID to bind, for testing under another VMM's numbering |
