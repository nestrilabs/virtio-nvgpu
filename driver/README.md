# `driver/` — guest kernel module

**License: GPL-2.0** (`LICENSE-GPL-2.0`), required for kernel symbol access.

A Linux kernel module for the guest. It registers the NVIDIA character
devices — `/dev/nvidiactl`, `/dev/nvidia0`…`/dev/nvidiaN`, `/dev/nvidia-uvm`,
`/dev/nvidia-modeset` — a DRM device per GPU (render node and card node), and
`/dev/nvgpu-wl`, the guest end of the Wayland proxy's channel, and
`/dev/nvgpu-capture`, where a guest's capture daemon opens host buffers the
VM's capture helper injected. It forwards
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
tree by whoever is assembling a guest image. **x86-64 guests only**, with
4 KiB pages (`Kconfig`; an out-of-tree build for anything else stops at
`nvgpu.h`): the fake PCI bus is x86's `struct pci_sysdata`, the page runs of
registered memory are 4 KiB ones, and the 32-bit refusals are x86's. **One
virtio-gpu-nv device per guest**: what the module registers is global, as
nvidia.ko's is (the `nvidia` class, major 195, `/proc/driver/nvidia`, the
`/sys/module/nvidia*` stubs), and a second device is refused at probe
(`-EBUSY`, with a line saying why).

**Where it stands.** The module speaks v2 when the backend answers its HELLO,
and falls back to v1 — no display features — when it does not. It has run on
Linux 7.2.7 guests on an RTX 5090 (595.99.02), under nesbox and crosvm, built
both ways (C and Rust parsers): every graphics and compute path, the Wayland
channel, a lease driven with KMS, `VK_KHR_display`, the security negatives
and the application pass ([`rig/TESTING-RIG.md`](../rig/TESTING-RIG.md)). The
compositor-VM mode (the guest driving the host card) has not run on hardware.
Off the hardware, its IOCTL2 interpreter is exercised end to end against the
whole backend by `device/src/i2_e2e.rs`, which runs the module's own Rust
interpreter (`rust/core`), and `rust/difftest` runs `nvgpu_i2.c` itself
against that Rust port.

**The parsers of guest-process input are Rust by default** wherever the
kernel has `CONFIG_RUST=y`: the IOCTL2 walk, the v1 IOCTL marshalling and
descriptor translation, deep segments, the OS-descriptor registrations and
ATOMIC commits. Out of tree the Makefile picks them when the target kernel
has Rust and the rustc it was built with is at hand, and the C otherwise
(with a warning, when the kernel has Rust but the rustc differs); in a kernel tree `CONFIG_VIRTIO_GPU_NV_RUST`
does (default `y` with `CONFIG_RUST`). `NVGPU_RUST=1` insists on the Rust,
`NVGPU_RUST=0` on the C, which the module then says at load on a kernel
with Rust. The Rust passed the whole hardware regression on 2026-09-26 and
is the stronger boundary (`SECURITY.md` §6); the C stays, frozen, as the
fallback for a guest kernel without Rust and as the difftest's oracle, and
`scripts/build-guest-kernel.sh` builds a Rust kernel and module in
`scripts/guest-toolchain-rust` (and says it builds the C fallback in a
toolchain without Rust, or with `NVGPU_RUST=0`). [`rust/README.md`](rust/README.md) has how to build and test
each, and how to delete the C afterwards.

One module, `virtio_gpu_nv.ko`, built from several objects (see `Makefile`;
the test rig installs it in its image as `nvgpu.ko`):

| file | contents |
|---|---|
| `nvgpu.h` | internal header: shared structs, cross-file prototypes, module parameter `extern`s |
| `nvgpu_wire.h` | wire protocol and config-space layout (BSD-3-Clause OR GPL-2.0+, mirrors `protocol/`) |
| `nvgpu_main.c` | probe/remove, virtqueues, `/dev/nvidia*` cdevs (nvidia-uvm and nvidia-caps at dynamic majors, as theirs), their polls, mmap with each placement's memory type and writability (UVM semaphore pools from the UVM aperture, shared memory region 2, found before HELLO and offered in it, at host addresses in [4 GiB, 32 TiB) only), `/proc`, sysfs, fake PCI, the `/dev/nvidia-modeset` dispatcher and the module's work queues |
| `nvgpu_v1.c` | the protocol-v1 IOCTL exchange, in both builds: the request header, the reading of a reply's header, whose status is 0 or an errno (anything else is `-EPROTO`), and the flat round trip of a block with no pointer in it (`nvgpu_ioctl_flat()`: the C parsers' flat escapes, `nvgpu_drm.c`'s flat nvidia-drm calls); the Rust's twin is `wire::IoctlResp::parse()` |
| `nvgpu_rmio.c` | the protocol-v1 IOCTL message, in C: the ioctl dispatcher for `/dev/nvidia*` files and a DRM file's RM (non-`'d'`) ioctls, RM forwarding (descriptors and OS events translated to backend handles, deep pointers and deep segments, GPU/CPU time correlation moved into the guest's clocks, the calling process on RM_ALLOC and RM_DUP_OBJECT), UVM, v1 nvidia-modeset, and what reads an OS-descriptor registration and builds its page list. Built with `NVGPU_RUST=0`, or by default on a kernel without Rust |
| `nvgpu_osdesc.c` | memory the caller already has, registered with RM by its pages: ALLOC_MEMORY and RM_ALLOC of the OS-descriptor class and VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR pin the caller's range as RM would and send its guest-physical runs; the pins last until a reap (HOST_OP OSDESC_REAP) names the registration, or remove(); a registration abandoned in flight keeps them under its request id until its late reply names what RM registered, and one never sent unpins at once |
| `nvgpu_drm.c` | DRM device registration, GEM proxies, PRIME, nvidia-drm driver-range ioctls, and every DRM ioctl's argument normalised as `drm_ioctl()` does (`nvgpu_drm_arg_in()`) |
| `nvgpu_xfer.c` | protocol v2 transport: request contexts and transport buffers, reply polling (`rt_spin_us`) and the interrupt, HELLO and the host clock, HOST_OP / WATCH / CLOSE and their async twins, the async W_ARM of a file's readiness (on the module's own queue), the event queue and its consumer registry, EV_HOTPLUG uevents, the reaper of replies nobody waited for, and the end of a dead transport (fences failed, pollers woken to EPOLLHUP) |
| `nvgpu_i2.c` | the schema-driven IOCTL2 interpreter, in C: gathers a caller's buffers per `gen/nvgpu_schema.h`, translates descriptors and GEM handles through per-caller hooks, copies replies back by the kernel's own rules. The C build's |
| `nvgpu_schema.c` | the generated IOCTL2 and UVM tables' one copy, which a host gets, and the descriptor-kind test the fd_in hooks make (`nvgpu_fd_kind_allowed()`, the backend's `kind_allowed()`) |
| `nvgpu_atomic.c` | an ATOMIC commit's arrays, in C: which CRTCs get flip events and which values are fences, asking `nvgpu_kms.c` what objects and properties are. The C build's |
| `nvgpu_rs.rs`, `nvgpu_rs_glue.c`, `nvgpu_rs.h`, `rust/` | the Rust build's (the default on a kernel with Rust), in place of `nvgpu_i2.c`, `nvgpu_rmio.c` and `nvgpu_atomic.c`: the same parsers in Rust (`rust/core`, no `unsafe`, no panic path), the one Rust file with `unsafe` around them, the C they call, and the ABI; `rust/difftest` runs both on the same inputs. See [`rust/README.md`](rust/README.md) |
| `nvgpu_hostfile.c` | backend handles as guest files (anonymous inodes, e.g. a host syncobj's `syncobj_file`): closing one closes the host's, passing one names it again |
| `nvgpu_kms.c` | the KMS side of guest DRM files: host card and lease handles, lease adoption (`nvgpu_adopt_drm_file`), KMS ioctls through IOCTL2, master mirroring, flip/vblank events and clocks, ATOMIC fence properties |
| `nvgpu_fence.c` | fences: host sync_files behind guest `dma_fence` proxies, unwrapping guest fences for the host, the syncobj ioctls (waits that sleep here, not on the host), nvidia-drm semaphore-surface fences |
| `nvgpu_nvkms.c` | `/dev/nvidia-modeset` on protocol v2: NVKMS through IOCTL2 by the host release's own tables, descriptor translation, and the level readiness NVKMS clients poll |
| `nvgpu_capture.c`, `uapi/nvgpu_capture.h` | `/dev/nvgpu-capture` (only with the backend's `--inject-socket`): OPEN, an injected host buffer by id and token as a read-only guest dma-buf of a GEM proxy, and OPEN_SYNCOBJ, an injected syncobj as a handle of a render file; fixed-size structs, C in both builds |
| `nvgpu_misc.c` | what `/dev/nvgpu-wl` and `/dev/nvgpu-capture` share: misc-node registration, the node's reference count (an open file keeps it, and it keeps the device), and the mode rule of `wl_mode` and `capture_mode` |
| `nvgpu_wl.c`, `uapi/nvgpu_wl.h` | `/dev/nvgpu-wl`, the guest end of the Wayland channel: HELLO (device map, clock offset), CONNECT, SEND/RECV frames, and the descriptors only the kernel can name (dma-bufs, syncobjs, adopted DRM files) |
| `nvgpu_rm_intercepts.h`, `gen/nvgpu_rmalloc_classes.h`, `gen/nvgpu_v1v2_rewrites.h` | RM command tables (generated / hand-kept) |
| `gen/nvgpu_rm_deep.h` | the RM controls, and IDLE_CHANNELS, whose several pointers go as deep segments, and how much RM copies through each; generated by `gen/rmctrl_extract.py` with the backend's copy; never edited by hand |
| `gen/nvgpu_schema.h` | IOCTL2 schema tables (DRM render/KMS, nvidia-drm, NVKMS per release) and UVM parameter block sizes per release, generated by `gen/schema_gen.py`; never edited by hand |

Other files: `Makefile` and `Kconfig` (in-tree and out-of-tree builds),
`guest-kernel.defconfig` (the configuration of the guest kernel it is built
against, with `CONFIG_RUST=y`, as `make savedefconfig` writes it: copy it to
a build tree's `.config` and run `make olddefconfig` in
`scripts/guest-toolchain-rust` for the whole of it;
`scripts/build-guest-kernel.sh` makes and records it, and `CONFIG_ONLY=1`
does only that).

## Callers of another width or another struct size

The native kernel serves two kinds of caller the module did not, and it
serves them the native way now (SECURITY.md §6 has why neither sends the
backend anything new):

- **32-bit processes** on `/dev/nvidiactl`, `/dev/nvidiaN`,
  `/dev/nvidia-modeset`, `/dev/nvidia-uvm` and `/dev/nvidia-uvm-tools`:
  `.compat_ioctl = compat_ptr_ioctl`, the native handler, as nvidia.ko,
  nvidia-modeset and nvidia-uvm each set theirs (RM, NVKMS and UVM structs
  are one layout at both widths). Without it every such ioctl was
  `-ENOTTY`: Steam's client, a 32-bit game's GL and Vulkan driver. A 32-bit
  UVM client gets its ioctls but no semaphore pool, which is mapped only
  above 4 GiB, so no CUDA context (CUDA 12 dropped 32-bit applications). The DRM node's compat path
  sends the core's ioctls through `drm_compat_ioctl()`, as nvidia-drm does,
  and refuses the two KMS ioctls whose compat layouts the core converts
  (WAIT_VBLANK, ADDFB2) rather than misread them. `/dev/nvgpu-wl` and
  `/dev/nvgpu-capture` have `compat_ptr_ioctl` too: their structs are the
  same at both widths.
- **DRM structs of another size** than this kernel's, from older or newer
  headers (a 16-byte `drm_syncobj_handle`, before `point`: the Steam
  runtime's libdrm). Every DRM ioctl the node answers or forwards itself --
  nvidia-drm's range, syncobjs, semaphore-surface fences, a KMS file's KMS
  calls, the dumb-buffer pair, GET_CAP -- is taken by its number and run on
  the caller's argument normalised as `drm_ioctl()` does
  (`nvgpu_drm_arg_in()` in `nvgpu_drm.c`): the caller's bytes in,
  zero-extended to the native struct, the caller's size back; the handler
  and the host see the native command only (the kernel's for syncobjs, the
  release's schema entry for KMS, `nvgpu_i2_native_cmd()`). The IOCTL2
  interpreter runs on that kernel copy (`nvgpu_i2_call.karg`) and still
  refuses any size but the schema's. GET_DEV_INFO keeps answering each of
  its four layouts in its own.

`rig/guest-image/probes/compat.sh` tests both on hardware
([`rig/TESTING-RIG.md`](../rig/TESTING-RIG.md), A8).

## Module parameters

Every one, with its mode under `/sys/module/virtio_gpu_nv/parameters/`:

| parameter | mode | default | what it does |
|---|---|---|---|
| `wl_mode` | `0444` | `0660` | permissions of `/dev/nvgpu-wl*`, created `root:root`. Every open is a client of the host compositor, so the node is for the Wayland daemon alone; `contrib/udev/70-nvgpu-wl.rules` gives it the `nvgpu-wl` group. A mode outside `0770` (anything for "other") is refused and the module does not load. Load time only. |
| `capture_mode` | `0444` | `0660` | permissions of `/dev/nvgpu-capture*`, created `root:root`, for the guest's capture daemon alone (`contrib/udev/70-nvgpu-capture.rules` gives it the `nvgpu-capture` group). Anything for "other" is refused. Load time only. |
| `virtio_id` | `0444` | `45` | virtio device ID to bind: libkrun's number; another for a VMM that cannot express 45 (QEMU stops at 41). Load time only. |
| `arm_ready` | `0444` | `Y` | whether HELLO offers armed readiness (`NVGPU_GCAP_ARMS_READY`): the backend reports an RM descriptor's events once per wait here instead of once per event. `N` is for measuring what that saves. Load time only (read at HELLO). |
| `rt_spin_us` | `0644` | `20` | how long a caller spins for its reply before it sleeps, in microseconds; `0` sleeps at once. Executor-class requests never spin. Root may change it at run time. |
| `async_fence_watch` | `0644` | `Y` | send a host-fence proxy's WATCH from a work item rather than in the presenting thread. `N` is for measuring what that saves. Root may change it at run time; it applies to proxies made after. |
| `pacing` | `0400` | -- | read-only: the frame-pacing counters (round trips per message type and their latency, syncobj waits, events, polls). Root only: they are every guest process's calls as they happen. |

The three measurement switches (`arm_ready`, `rt_spin_us`,
`async_fence_watch`) are documented in DEPLOY.md's guest section too; leave
them at their defaults outside a measurement. The experiment switches of
earlier builds (`poll_events`, `poll_spin_us`, `claim_alloc`,
`claim_sync_fd`) are gone.

`modinfo -F parsers virtio_gpu_nv.ko` says which implementation reads
guest-process input: `rust` (the default on a kernel with `CONFIG_RUST=y`,
`NVGPU_RUST=1`, or `CONFIG_VIRTIO_GPU_NV_RUST=y` in-tree) or `c`
(`NVGPU_RUST=0`, or a kernel without Rust).
