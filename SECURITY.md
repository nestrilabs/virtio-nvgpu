# Security

What a guest can reach on the host, before and after the display-passthrough
work, and what is still open.

This is the security review of branch `display-passthrough` at `ae182ab`
against `dev` at `50ff74a` (called **dev** below), brought up to date by the
audit of branch `harden` (§11), the RM allowlist of branch `rmallow` (§12),
the fuzzing of branch `fuzz` (§13), the memory-safety structure of
branch `dind` (§14), the review of `dind` for memory passing (§15), and the
VMMs it runs under, nesbox and crosvm (§16); by the review of 2026-09-26
(§17); by capture injection (§18); by the window's size and share, with the
RM mapping fixes that came with them (§19); and by the frame-pacing changes
(§20). It is written for the project's owner. The
code is the reference: where this document and the code disagree, the code
is right.

> **Hardware status (2026-09-26).** On an RTX 5090 with 595.99.02, under
> nesbox and crosvm, with the RM allowlist enforcing (the default) and the
> backend's sandbox on, the rig (`rig/TESTING-RIG.md`) runs green on the code this
> document describes: stage 1; render with and without `--allow-compute`
> (CUDA under both VMMs, crosvm's frontend jailed); the Wayland proxy against
> a headless sway and against the live patched Hyprland, with direct scanout;
> a lease driven with KMS, `VK_KHR_display` on it, and the lease round trip;
> the security negatives on the control and render nodes and on a leased card;
> the guest module with its parsers in C and in Rust; and about 35
> applications as an unprivileged guest user. **Not run on hardware:** the
> compositor-VM and export modes (they need the host desktop stopped), and
> hotplug. Statements below about what the host kernel does with a request
> were read from source (NVIDIA's open modules 610.57.04, Linux 7.2.7) unless
> a section says it was observed. Sections written before a run say where
> things stood at the time; this note is the current state.

---

## 1. Summary

### Does the display work add attack surface?

**Yes.** Most of it faces the host desktop and the backend process, not the
NVIDIA driver's entry points, and most of it exists only when the operator
turns a display mode on.

- **The host display.** With `--kms-card`, or with a lease from the host
  compositor, the backend holds host DRM files that are master of real
  outputs, and runs guest-chosen KMS ioctls on them: 49 schema entries,
  ATOMIC, SETCRTC, CREATE_LEASE and SET/DROP_MASTER among them. dev held no
  such file.
- **The host compositor.** With `--wayland-socket`, each guest client is a real
  client of the host compositor, limited to 42 globals. Its traffic passes
  through new parsers in the backend (`wlwire`: frame, wire codec, engine,
  shm, blob, stream). `--wayland-export` adds a listening socket that host
  programs connect to. `patches/` changes Hyprland and aquamarine so that a
  desktop monitor can be leased at all.
- **Fences.** In every v2 session: the 12 syncobj ioctls and nvidia-drm's
  semaphore-surface fences (0x54-0x57) on host render files, and HOST_OP's
  sync_file, dma-buf, eventfd and `/dev/udmabuf` operations.
- **The control plane.** Nine new wire messages (8 types become 17), an IOCTL2
  interpreter on both sides, up to 16 executor threads, a closer thread, one
  reader thread per Wayland channel, and a uevent netlink socket in the lease
  and compositor-VM modes.

The verification review found two ways a guest could make the host kernel
read out of bounds or oops, both on paths the fence work forwards.
SEMSURF_FENCE_CTX_CREATE passed on an index that nvidia-drm adds to a kernel
mapping without a bound (C-1). An OS-event number in NV_EVENT_BUFFER could make
RM keep a raw number as a pointer and oops (H-2); dev forwarded that
allocation raw as well. The worst findings the security review found new to
the display work were availability and isolation problems: host memory
exhaustion through the Wayland proxy, any guest user taking over export-mode
host programs, and a guest scanning out another tenant's framebuffer. Its worst
findings overall, S-1 (critical) and S-5 (high), were in paths dev already
had. All of these are fixed in the code; see §8.

### Does it reduce attack surface relative to dev?

**Yes. On the GPU side it removes more than it adds.** The work made the backend
the authority over what it forwards, and the review behind it found and closed
paths that dev already had. Measured against dev:

- **GPU-side, host kernel.** dev forwarded nvidia-uvm whole (both devices,
  every command, any length, guest pointers and descriptors as sent), every
  NVKMS command with no policy, and every nvidia-drm ioctl on a render node.
  RM escapes went unchecked until the guest itself had asked the driver's
  version. Now UVM takes at most 33 of the host's 38 commands at their exact
  sizes, and the tools device takes none. NVKMS goes through a table per
  release, and DRM through one generated schema, both held by the backend.
  Each RM escape is checked against a profile chosen when the backend starts.
- **GPU-side, the backend's own memory.** On dev, several paths handed RM,
  NVKMS or UVM a pointer the guest chose, and the host then read or wrote the
  backend at that address. They were the embedded pointers of RM controls, a
  parameter pointer with no block behind it, and the UVM buffers. Memory named
  by CPU address (OS descriptors) was pinned from the backend's address space
  and mapped for the GPU. None of these reaches the host now. The pointer
  fields come from tables measured per release (`device/src/guestptr.rs`,
  `gen/rmctrl/`). Memory the guest registers now travels as the
  guest-physical pages behind it, each checked to be guest RAM, and RM pins
  the backend's own mapping of exactly those pages (below).
- **The whole host.** dev's launcher ran the backend as root, so every guest
  process was an RM administrator with all of BAR0 mappable read-write. Its
  default socket was a fixed path in `/tmp` that another user could bind
  first. Now the backend refuses to start as root and drops every capability
  before its first thread. The socket lives in a private directory, and logs,
  counters and the event sweep are bounded.

Much of this is not display code. It is the forwarding path dev already had,
reviewed because the display work depended on it.

### Compute is opt-in

CUDA needs paths nothing else does: the UVM device, UVM's multi-process
sharing mode, the UVM aperture (the VMM maps semaphore pools at guest-chosen
addresses in its own address space) and memory registered by its guest pages
(RM pins guest RAM for the GPU until a list of holders read from one release
lets go). All of them are served only when the backend is started with
`--allow-compute`, and none is by default: the guest then has no UVM device,
and its host surface is RM, NVKMS and DRM alone ("Compute: the surface with
and without `--allow-compute`", §3).

### What is still open

- `RM_CONTROL` and `RM_ALLOC` are allow-listed per release (§12), default
  deny, and enforcing by default: 215 of 610.57.04's 1,370 controls (plus 30
  GSP pass-through numbers seen on hardware) and 96 of its 222 classes reach
  RM. The list was built from 106 hardware runs and NVIDIA's sources, and has
  run enforcing through the rig's regression and the application pass
  (Vulkan, GL, EGL, CUDA, NVENC, NVDEC, Vulkan Video, VA-API, OpenCL; §12). A
  workload it still misses breaks with RM's "not supported" and a log line
  naming the call.
- The host NVIDIA driver is in the TCB. There is no IOMMU boundary between
  guest GPU work and the host.
- The backend sandboxes itself before the first guest message -- a network
  namespace, Landlock, a seccomp allowlist (§4, "The backend's sandbox") --
  and, run as root, each VM's backend and VMM are host users of their own
  (§4, "One uid per VM"). What neither touches is the GPU (RM's page tables
  keep VMs apart there) and the host kernel the backend still calls. There is
  a cgroup of the backend's own in the shipped unit (`MemoryMax`,
  `TasksMax`; [`DEPLOY.md`](DEPLOY.md)), none under the rig's launcher, whose
  `NVGPU_MEMORY_MAX` scope holds the whole run, and the per-guest isolate is not built.
- One VM's budgets are split among its guest processes (§11, quota.rs), but
  a guest process that forks is a new process with a share of its own: the
  guest's own process limits are what bound a forking app.
- Four review findings are partly fixed, and two verification findings are
  partly fixed or open (§8). §9 lists every open item.
- Memory registered by its pages is now released to the guest only when no
  holder the backend knows of in the host kernel still has it: RM objects,
  UVM external mappings, and exports refused outright (§3, "Memory registered
  by its pages"). The holders were found by reading the 610 sources, not by a
  run; one missed would reopen a guest privilege escalation (§9, item 11).

§10 is the order the remaining work should go in.

---

## 2. Threat model

| party | trusted? | why |
|---|---|---|
| **guest kernel and all guest userspace** | **no** | One VM is one trust domain. Anything that protects the host is enforced by the backend from its own tables, never from a layout the guest describes. Separating guest users from each other is the guest kernel's job; the checks the guest module makes (the `/dev/nvgpu-wl` mode, the ACCEPT uid check) hold only while the guest kernel does. |
| **host compositor** (Hyprland, with `patches/`) | yes | It decides what a proxied client may do within the allowlist, and it holds the leases it grants. The backend does not defend against it, but it does not trust it for descriptor types: every descriptor the compositor sends is classified by what the kernel says it is. |
| **export-mode peers** (host programs on the `--wayland-export` socket) | partly | They must have the backend's uid (`SO_PEERCRED`, `device/src/wl/export.rs`), and their messages go through the same allowlist. A program that connects makes the guest compositor its Wayland server, and a server can type into its clients: connect only programs the guest may drive. The backend is undumpable, so a same-uid peer cannot ptrace it. |
| **the backend** (`vhost-user-nvgpu`, one process per VM) | **in the TCB** | It maps all of the guest's RAM, holds every host descriptor the guest uses, and is the caller of every host ioctl. Whatever compromises it has that VM's host files and what the backend's sandbox leaves it (§4): the GPU's nodes, a few read-only files, the syscalls on its list, as a uid of that VM's own when run as root. |
| **the VMM** | yes | It hands the backend the guest's memory. |
| **the host NVIDIA driver** (nvidia, nvidia-uvm, nvidia-modeset, nvidia-drm, GSP firmware) | **in the TCB** | The card is in the host's IOMMU domain. What keeps a guest's GPU work in its own memory is the GPU's MMU, with page tables RM programs for it. |
| **the host kernel** (DRM core, syncobj, dma-buf, memfd) | yes | The backend relies on its checks for everything §7 does not list. |

Not addressed: timing side channels between tenants on a shared GPU, a guest
saturating the GPU (scheduling is the host driver's), and physical access.

---

## 3. GPU-side host-kernel entry points

What a guest can make the host's NVIDIA driver run, by device and namespace.
"Host has" counts are from the 610.57.04 sources. The last column says how
each call is validated:

- **raw**: forwarded as sent;
- **size-checked**: the length is checked against the backend's profile;
- **table-sized**: the length is exact, from a per-release table;
- **schema-authoritative**: every pointer, length, descriptor and GEM field is
  recomputed from the backend's own copy against a generated schema;
- **refused**: the host is never called.

| entry point | host has | dev | HEAD |
|---|---|---|---|
| **RM escapes**, type `F`, on `/dev/nvidiactl` and `/dev/nvidiaN` | -- | Any `F` ioctl on any open handle. **Size-checked** against the profile (23 escapes for 535, 24 for 580 and 595) once the host version had been learned from the guest's first CHECK_VERSION_STR; **raw** before that. | Only on GPU and control handles (`v1_route`, `device/src/nvidia.rs`). The profile is chosen at start from `/proc/driver/nvidia/version`. **21 of 23 / 22 of 24** reach the host, **size-checked** except the three variable-length ones (CARD_INFO, ATTACH_GPUS_TO_FD, NUMA_INFO), which pass with no size check: EXPORT_TO_DMABUF_FD is **refused**, and IDLE_CHANNELS goes for one channel with its three array pointers zeroed, or for a list of at most 4,096 with the arrays as deep segments the backend sizes itself (a list without them is **refused**). XFER_CMD, I2C_ACCESS, ACCESS_REGISTRY, GET_EVENT_DATA and ADD_VBLANK_CALLBACK are **refused** under any ABI policy, `--permissive-abi` included. Pointer fields in the top-level blocks are zeroed. |
| **RM_CONTROL** commands | 1,362 method ids | All, **raw** past the 32-byte outer check. One embedded pointer was relocated, at an offset the guest named, into a heap buffer sized from what the guest sent. Every other embedded pointer reached RM as a guest address, which RM dereferences in the backend. | **Allow-listed** per release (§12): 215 of 1,370 (and 24 GSP pass-through numbers seen on hardware) reach RM; the rest are answered NOT_SUPPORTED, or INVALID_PARAM_STRUCT for a size other than RM's, without RM. Of those allowed, 3 are **refused** (pointers the table cannot name one by one). 12 that list other clients' host PIDs are answered by the backend with RM's own "insufficient permissions" (`device/src/rmctl.rs`). For the 47 whose parameters hold pointers RM follows (measured per release, `gen/src/rmctrl/generated.rs`), each pointer is relocated to a guarded buffer or zeroed. Several of one control go as deep segments, each **table-sized**: its length is computed from the parameters RM is handed, as RM computes it, and must match exactly, at most 1 MiB in all. The ACPI-method controls and four others (`ZEROED_CONTROLS`) are never relocated. REGISTER_WAITER's OS-event descriptor is translated and must name a live event. NV0000's OS_UNIX controls (0x3dxx): the six that name a control file by descriptor (export, import, export info) get the backend's descriptor of the caller's own control file, and any other number is **refused** (EBADF), as is a descriptor field too short to hold; MEMACCT's cgroup descriptor and every OS_UNIX command RM does not define are answered NOT_SUPPORTED without RM (§11, R1). |
| **RM_ALLOC** classes | 227 distinct numbers in `g_allclasses.h` | All, **raw**. | **Allow-listed** per release (§12): 96 of 222 reach RM, on RM_ALLOC, ALLOC_MEMORY, ALLOC_OBJECT, ALLOC_CONTEXT_DMA2 and by VID_HEAP_CONTROL function; the rest are answered INVALID_CLASS without RM. Of these, **12 refused** whatever the list says (OS-descriptor memory 0x71 named by address, kernel callbacks 0x78, 0x7e, 0x92 and 0x9010, memory lists 0x81-0x83, FB segments 0xc1, IMEX and fabric memory 0xf1, 0xf9 and 0xfd; `REFUSED_ALLOC_CLASSES`, `device/src/guestptr.rs`). pRightsRequested is zeroed. NV_EVENT_BUFFER must name a live OS event. |
| **RM_SHARE, RM_DUP_OBJECT, and a second client named in parameters** | NV04 share and dup; 2 NV0000 share controls; 7 classes and 21 controls that name another client | **Raw**. RM saw every client of a VM as one process, the backend's, so a guest could share an object with every client on the host (type ALL) or every process of the backend's uid (OS_SECURITY_TOKEN), and any guest process could duplicate any other's objects by handle. | Shares go to RM only when they narrow or grant inside the VM; the rest are **refused**. A duplicate's two clients must be this VM's, made by one guest process, unless the source was shared with the destination (RM's rule, guest processes for the backend's). A second client named in class or control parameters must be this VM's and pass RM's rule for that field, with guest processes and euids. A guest that does not say which process and euid make each call gets neither. Below, "RM objects between guest processes". |
| **memory named by CPU address** (OS descriptors through RM_ALLOC, ALLOC_MEMORY and VID_HEAP_CONTROL) | 3 paths | **Raw**: RM pinned the backend's pages at a guest-chosen address and mapped them for the GPU. | With an address alone, **refused**. Without `--allow-compute`, with pages too. With it, and the guest-physical pages behind it (BCAP_OS_DESC), **table-sized**: only the user-virtual-address descriptor type, a page list covering exactly what RM pins, every page in guest RAM, and RM handed the backend's own mapping of exactly those pages (below). |
| **nvidia-uvm**, `/dev/nvidia-uvm` | 38 commands | All, **raw**. The guest copied 12 KiB each way for every command but the two it knew the size of, and pointers and descriptors went as sent. | Without `--allow-compute` (the default), **0**: the open is **refused** before the host is asked, and the guest has no node. With it, at most **33** (30 to 33 per release), **table-sized** on both sides from `gen/uvm/`. Pageable access is forced off at UVM_INITIALIZE, so the GPU cannot fault in the backend's pages, and every file is put in multi-process sharing mode, which takes pageable access away on every release and ties the VA space to no process. The 6 descriptor fields are translated. Every command that copies through, pins or populates CPU memory is **refused**. |
| **nvidia-uvm tools**, `/dev/nvidia-uvm-tools` | 7 | All, **raw**. | **0**: without `--allow-compute` the open is **refused**; with it the file opens, and every ioctl on it is **refused**. |
| **NVKMS**, `/dev/nvidia-modeset` | one ioctl carrying 66 commands (610.57.04) | Every command, **raw**, with **no policy**. One descriptor, REGISTER_SURFACE's, was translated at a fixed offset. | 56 to 61 per release, **schema-authoritative** over IOCTL2. v1 carries only the commands with no pointer and no descriptor. 7 are **refused** by name, 3 run only with `--kms-card`, and 7 are gated on grants outside it. Everything else is in §5. At most 64 opens per VM, and 16 per guest process (the VM's last 8 kept for processes holding at most 2). |
| **nvidia-drm and DRM core on a host render node** | 24 nvidia-drm ioctls (21 render-allowed), plus the core's render-allowed ones | Any `d` ioctl. Three nested GEM calls translated `memFd`; the rest were **raw** in a buffer sized by the guest, while the host copies `_IOC_SIZE` back (a heap overflow in the backend). | v1: 6 full ioctl numbers. IOCTL2: 28 render-class entries (12 syncobj, 16 nvidia-drm), **schema-authoritative**. GEM_IMPORT_USERSPACE_MEMORY, GEM_FLINK and GEM_OPEN are **refused** on every handle. SEMSURF_FENCE_CTX_CREATE's index must lie inside the surface, and its client must be one this VM allocated, with at most 64 contexts per file, 96 per guest process and 256 per VM, the last 32 kept for processes holding at most 16 (`device/src/semsurf.rs`). Every argument buffer is at least `_IOC_SIZE` and guarded. |
| **DRM KMS on a host card or lease file** | the KMS core | None: no such file existed. | 49 KMS-class entries, **schema-authoritative**, only on card handles (`--kms-card`) and lease handles. See §5. |
| **HOST_OP** (backend-made host calls on the guest's behalf) | -- | None. | 11 ops, each argument checked against the handle kind it must be: PRIME export and import on render files, sync_file merge (at most 5), eventfd, a signalled sync_file (by `/dev/udmabuf` when needed), a syncobj wait registration (at most 1,024 per VM), fd kind, close-many, and OPEN_KMS and DROP_IF_MASTER, which are `--kms-card` only. OSDESC_REAP calls nothing on the host: it reads which registrations of guest memory RM has let go of. |
| **mmap** | per device | Any handle. A UVM file went to the window, where the VMM's mmap of it failed and closed the window's request channel for the rest of the VM. | Device, render, card and lease handles only. Each placement carries the host's memory type and whether it is writable, so a read-only host page is mapped read-only in the guest. A UVM file maps only a semaphore pool the same file was seen to create, asked for exactly, into the UVM aperture (below); anything else on it is **refused** before the VMM is asked. |
| **any other ioctl type** | -- | **Raw**, to whatever host file the handle was. | **Refused** (EPERM). |

What the dev column shows is that a guest's reach into the host driver on dev
was bounded mostly by what the guest's own libraries happened to send.

### Compute: the surface with and without `--allow-compute`

Every guest-reachable path that exists only for CUDA and other compute, and
what the backend does with it. Default is off.

| path | without `--allow-compute` (default) | with it |
|---|---|---|
| OPEN of `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools` | **refused** (ENODEV) before any host open; the guest makes neither node and does not register the `nvidia-uvm` major | opened on the host |
| UVM ioctls (`uvm_gate`, `device/src/guestptr.rs`) | unreachable: no UVM file exists | at most 33 of 38, table-sized; the tools device's all refused |
| UVM multi-process sharing mode, forced at UVM_INITIALIZE | unreachable | on every UVM file |
| range groups (CREATE/DESTROY/SET_RANGE_GROUP, PREVENT/ALLOW_MIGRATION_RANGE_GROUPS, MIGRATE_RANGE_GROUP) | unreachable | forwarded on releases that have them |
| UVM descriptor fields (`device/src/uvmfd.rs`) | unreachable | translated |
| the UVM aperture (BCAP_UVM_MAP): the VMM maps a pool at a guest-chosen address in [4 GiB, 32 TiB) of its own address space and gives it a memory slot | **never offered**; the aperture is length 0, and no UVM file exists to map | offered when the guest has the region, the VMM's request channel is up and the host's UVM takes sharing mode |
| memory registered by its pages (BCAP_OS_DESC): RM_ALLOC of 0x71, ALLOC_MEMORY and VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR with a page list, OSDESC_REAP | **never offered**; a page list is **refused** (EINVAL) before RM, and an address alone is refused (EPERM) as it always was | offered when the backend holds guest RAM |
| UVM external mappings holding registered memory | unreachable | tracked, and hold the pages |

Without the flag the host-reachable surface is RM (the escapes, controls and
classes of this section), NVKMS and DRM: less than before these features, since
UVM itself -- table-sized as it was -- is gone too. Nothing about the RM rules
changes with the flag.

What graphics needs of it: nothing. On the rig, the GL-then-Vulkan run, the
EGL device run and the app pass under sway (vkcube, GL and EGL clients)
issued no UVM command and allocated no class 0x71. The render probe's runs
each counted three UVM_INITIALIZE and DEINITIALIZE pairs, one of them
cuda-smoke's; the counts fit its two nvidia-smi runs, which were not traced.
A host whose nvidia-uvm is not loaded is an ordinary configuration for
NVIDIA's userspace, and the render probe checks that nvidia-smi, Vulkan and
EGL still pass without it and that CUDA finds no device
(`rig/guest-image/probes/render.sh`). Vulkan Video (NVENC and NVDEC) uses RM's
video classes, not UVM; NVENC through CUDA is compute. What does go without
the flag is registering existing host memory with RM:
`VK_EXT_external_memory_host` imports and `cuMemHostRegister` fail, as they
did before registration by pages existed. Whether anything on the graphics
side uses it is still to be seen on the rig, in the backend log's EPERM
refusals.

### The UVM aperture: what the guest can put in the VMM's address space

A CUDA context needs a UVM semaphore pool mapped at its own address, and UVM
maps one nowhere else, so each pool the guest maps is mapped by the VMM at
that address in the VMM's own address space and given a memory slot in a
second guest-physical region (ARCHITECTURE.md §5). The address is the
guest's choice. What bounds it:

- **Only a pool, only this VM's, only exactly.** The backend records a pool
  when UVM says ALLOC_SEMAPHORE_POOL succeeded on that UVM file, and asks the
  VMM to map only that range of that file, read-write, when the guest asks for
  exactly its base and length (`device/src/uvmmap.rs`). A UVM file not in
  sharing mode, another file's pool, a sub-range and a read-only request are
  refused. UVM checks the same range again when the VMM maps it.
- **Never over the VMM's own memory.** The address must lie in [4 GiB,
  32 TiB), where a 64-bit VMM normally has nothing: its executable and heap
  sit at two-thirds of the 47-bit space (85 TiB), and its mappings grow down
  from below the stack or, under the legacy layout (`vm.legacy_va_layout`,
  or the `ADDR_COMPAT_LAYOUT` personality), up from a third of it
  (42.7 TiB), which the 64 TiB top the band once had reached. The exception
  is a stack rlimit that is unlimited, or above about 96 TiB: x86 puts the
  top of the mmap area below the stack by the rlimit, capped at five sixths
  of the address space (`arch/x86/mm/mmap.c`), so the VMM's mappings then
  start near 21 TiB, inside the band. (The comments said an unlimited stack
  rlimit meant the legacy layout; on x86 it does not.) The backend, the VMM
  and the guest driver all check the band.
  nesbox maps with `MAP_FIXED_NOREPLACE`, so a collision fails rather than
  replacing anything. crosvm reserves the whole band `PROT_NONE` at start-up,
  before it maps guest RAM or anything else, and maps a pool over its own
  reservation only, putting the reservation back when the pool goes (§16):
  nothing of crosvm's can be in the band. Two of the guest's own pools at one
  address are refused by the backend before the VMM is asked, with ENOMEM as
  any placement is (it was EEXIST). That does not hide everything (§11, B5
  and F3): a refusal where the caller's budgets had room still says the
  address is taken -- by another guest process's pool, which one process can
  so find and squat on, or, under nesbox with a stack rlimit that moves its
  mmap area into the band (above), by one of nesbox's own mappings, guest
  RAM among them. nesbox's `docs/SECURITY.md` says the same since
  `virtio-nvgpu-v3`.
- **No descriptor kept.** The UVM file travels to the VMM on the vhost-user
  request channel and its copy is closed when the request returns; the
  mapping's own file reference is all the VMM holds, and it goes with the
  mapping.
- **No slot over nothing.** The VMM checks the pages are present before it
  adds the slot, removes the slot before the mapping, and UVM refuses to free
  a pool that is still mapped. An aperture address with no slot reads zeros
  and ignores writes, in the VMM, never as a fault the host has to resolve.
- **Bounded.** Each pool at most 64 MiB; 16 placements and 64 MiB per UVM
  file, and per guest process across its files; 64 placements and 256 MiB
  per VM, the last 8 and 32 MiB kept for processes holding at most 2 and
  8 MiB; 256 recorded pools per file and 4,096 per VM; an aperture of at
  most 1 GiB. A pool is host kernel memory from the moment UVM makes it, so
  ALLOC_SEMAPHORE_POOL is refused before UVM is asked when its length could
  never be mapped (0, or past 64 MiB) or the file (256 MiB), the process (a
  quarter) or the VM (1 GiB) has that much in pools already, mapped or not
  (§11, F1). Every placement goes when its
  last guest mapping does, when its file closes, and on a guest reboot or
  device reset; if the backend goes away, the VMM drops them all itself.

Sharing mode only takes things away (pageable access, the tie to the
backend's mm), no host driver change is needed, and the VMM's seccomp filter
already allows the calls involved (`mmap`, `mincore`, `ioctl`).

### RM objects between guest processes

RM keeps a user client's objects to the process that made it. Its one default
share policy is `RS_SHARE_TYPE_PID` for DUP_OBJECT (`serverInitGlobalSharePolicies`,
`sharing.c`), and a PID policy matches when the source client's ProcID is the
duplicating client's (`cliresShareCallback`; the policy's `target` is never
read). Every RM call a guest makes is the backend's, so all of a VM's clients
were one process to RM: any guest process could duplicate any other's objects
by handle, and a share a guest made RM applied to the host. The backend now
judges three things before RM sees the call (`device/src/rmshare.rs`):

- **Sharing.** NV_ESC_RM_SHARE and the two controls that do the same
  (NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT, SET_INHERITED_SHARE_POLICY) go to RM
  when they revoke or only add a REQUIRE, which can only narrow what RM
  shares, and when they grant to this VM: CLIENT naming one of its live
  clients or the owner itself, PID (the owner's own process, the backend's,
  whose other clients are this VM's and the backend's own), and NONE. A
  grant of type ALL, OS_SECURITY_TOKEN (the uid, shared with every host
  process of the backend's user, other VMs' backends included), GPU,
  SMC_PARTITION, FM_CLIENT, a type RM does not define, or CLIENT naming any
  other client is refused. The share lists RM modifies, and the CLIENT
  grants in them, are recorded, at most 4,096 together a session; past that
  a share is refused, a revoke that would start a list included.
- **Duplicating.** NV_ESC_RM_DUP_OBJECT's destination and source clients must
  both be clients this VM allocated and has not freed. Then RM's own rule, with
  guest processes for the backend's: the two clients were made by one guest
  process (RM's PID default compares the source client's maker with the
  destination client's, never the caller), or the list RM checks the object
  against grants the destination DUP_OBJECT: the object's own list once a
  share has modified it, else its client's while no other object of that
  client has a list of its own (it could be the object's parent, whose list
  RM would read). A free of any object of a client takes the grants of all
  its objects, since the backend cannot tell which went with it and a handle
  freed with its parent can be made again. Within one client there is
  nothing to keep apart.
- **Naming a second client.** Allocation classes and controls that name a
  client besides the caller's are checked by RM, where at all, against a
  security token every guest process shares. Each such field is held to the
  rule RM applies to it between two host processes, read from 610.57.04's
  sources, with the guest's processes and euids (`Rule` in
  `device/src/rmshare.rs`); the client must be this VM's first of all, and
  the caller's own client passes every rule, as in RM:

  | field | RM's check between host processes | rule here |
  |---|---|---|
  | NV01_DEVICE_0 `hClientShare` | clientValidate: the calling file (the default, strict; RM applies it itself to the backend's per-guest-file file), else the calling process's security token | **token**: made by the calling process, or one of the caller's euid |
  | MAXWELL_PROFILER_DEVICE `hClientTarget`; FIFO_GET_CHANNEL_GROUP_UNIQUE_ID_INFO; QUERY_CHANNEL_UNIQUE_ID's list | the two clients' tokens (profilerDevConstruct, `_kfifoValidateTargetClient`; skipped at USER_ROOT, which no guest process is to the backend) | **client token**: the caller's client and the named one made by one process, or by processes of one euid |
  | GT200_DEBUGGER `hAppClient` | RS_ACCESS_DEBUG on `hClass3dObject` from the object's share list (ksmdbgssnConstruct) | **shared**: the two clients made by one process (RM's PID default, which the backend's process matches for every guest client), or a recorded CLIENT grant of DEBUG on that object |
  | CLIENT_GET_ACCESS_RIGHTS `hClient` | none; the answer is the caller's rights on the object | **shared**, any right |
  | NV01_DEVICE_0 `hTargetClient`; the events' `hParentClient`; UVM_CHANNEL_RETAINER's and NV_CONFIDENTIAL_COMPUTE's `hClient`; NV503C REGISTER_PID; the five GR ctxsw binds; EXEC_REG_OPS; MIGRATABLE_OPS; PROMOTE, EVICT and INITIALIZE_CTX; FIFO_UPDATE_CHANNEL_INFO; DMA_INVALIDATE_TLB; DEFERRED_API's `hClientVA` and the control it bundles; the client lists of FIFO_DISABLE_CHANNELS (sent to GSP-RM as it is), DISABLE_CHANNELS_FOR_KEY_ROTATION and ROTATE_KEYS | none on the CPU side: looked up, stored, never read, kernel-only, or sent on to GSP-RM, whose token is the GFID or none | **process**: made by the calling guest process |

  "Process" is stricter than RM where RM checks nothing: natively any process
  of any user could name any client there, and a sandboxed app is not to be
  reachable that way from another. Every `NvHandle h*Client*` field of
  610.57.04's class and control headers, arrays included, was read for this;
  the rest are kernel-only, vGPU host, diagnostics or INTERNAL, which RM
  refuses the backend, or classes RM does not implement (NV_FB_SEGMENT,
  NV_EVENT_BUFFER's bind).

A refusal is RM's own answer to a caller without the right,
NV_ERR_INSUFFICIENT_PERMISSIONS in the parameters' status with the ioctl
succeeding, as for the host-PID controls. A block too short to hold the
fields is EINVAL.

Only the guest kernel knows which guest process makes a call, and as whom.
The guest module says, when the backend asks for it in HELLO
(`BCAP_PROC_ID`, `BCAP_PROC_EUID`): 16 bytes after the blocks of every
RM_ALLOC, RM_DUP_OBJECT and RM_CONTROL, the calling thread group's leader by
its PID in the initial namespace and its start time, and the caller's
effective uid in the initial user namespace -- what RM's token holds for a
host process (`os_get_euid`; RM never reads the fsuid). The pair is one
process for the guest's lifetime, whatever PID namespace it runs in and
however PIDs are reused. A fork is a new process, an exec or a setuid is not,
and a client passed to another process by its file stays its maker's, as RM
keys a client's ProcID and token when the client is made. The backend takes
the guest kernel's word, which is §2's line: separating guest users is the
guest kernel's job. A guest module that cannot say fails closed: it gets no
duplicate between two clients except by a recorded grant, and names no
client but the caller's own (logged once a session). One that says the
process but not the euid has every token rule held to the process.

### Memory registered by its pages: what the guest can make the GPU reach

RM registers memory a caller already has by CPU address, and pins what that
address maps in the calling process, the backend. Sent that way the three
calls are refused (the critical finding on dev: RM pinned the VMM's memory at
a guest-chosen address). A guest that is offered BCAP_OS_DESC sends the
guest-physical pages behind the caller's range instead, and the backend hands
RM an address of its own (ARCHITECTURE.md §5, `device/src/osdesc.rs`). What
bounds it:

- **Only guest RAM.** Every page of the list must lie in a region of the
  vhost-user memory table, which holds guest RAM and nothing else: the window,
  the UVM aperture and every other device region are not in it, and a page
  outside refuses the whole call before RM is called. The backend maps only
  from those regions' own memfds, at the page's own offset.
- **Only what RM pins, as RM pins it.** The list must cover exactly the
  pages from the one holding the caller's address to the one holding its last
  byte (`limit + 1` bytes on), in runs of whole pages, at most 8,192 runs and
  4 GiB, with the writability the call asks RM for; the backend maps
  read-only memory read-only. The caller's offset in its first page is kept,
  and RM refuses an unaligned one as it does natively.
- **Only a virtual address.** The descriptor type must be the user virtual
  address. A physical address, a page array, I/O memory, a dma-buf by
  descriptor and the kernel-only types are refused whatever came with them.
- **Pinned on both sides until the host kernel lets go.** The guest keeps
  its pins until the backend reports the registration released, and the
  backend's range stays mapped until then. It is released only when every
  holder is gone:
  - the RM object the call made, each DUP_OBJECT duplicate, and each object
    RM made over one of those and keeps a duplicate of its own for (a
    semaphore surface naming it, a memory mapper over such a surface), and
    each duplicate a semaphore surface hands back with REF_MEMORY. Each
    goes when RM frees it, its parent or its client (the backend frees a
    client holding one itself, on its own file, before that file closes), or
    with the session;
  - each UVM external mapping (MAP_EXTERNAL_ALLOCATION) of any of them --
    which must lie in an external range the file made and the backend
    recorded, or it is refused before UVM sees it -- counted when UVM
    answered NV_OK or one of the errors of its wait for the page-table
    writes, which leave the mappings up (RC, ECC, GPU lost), until
    UNMAP_EXTERNAL has covered it on every GPU it names, UVM_FREE takes its
    external range, or its UVM file closes. On that close the backend takes the mappings down itself first,
    on its own descriptor, because the event pump's duplicate can make the
    file's last close later; a mapping that will not come down keeps the
    pages until the session ends.

  A free the backend cannot see (an ancestor above the parent) or a mapping
  UVM never made makes the release late, never early; a range it did not
  record (past 65,536 per VM) takes no mapping of registered memory.
- **Never handed where the backend cannot follow.** RM's export to a
  descriptor (NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT(S)_TO_FD) and
  NV_MEMORY_EXPORT's EXPORT_MEM duplicate an object into a client of RM's
  own, and anything that imports it -- another RM client, NVKMS surface
  registration, nvidia-drm's GEM import -- makes another. Naming an object
  that holds a registration, both are refused (EPERM) before RM sees them,
  and so is UVM's ALLOC_DEVICE_P2P. NVKMS takes an RM handle directly only
  from kernel clients, and nvidia-drm names memory to NVKMS only by an
  export descriptor, so these refusals are what keep registered memory out
  of both.
- **Bounded.** 4,096 registrations per VM, released ones the guest has not
  yet read included, and 1,024 per guest file; 16 GiB per VM and 4 GiB per
  file; a guest process at most a file's budget across all its files, the
  VM's last sixteenth kept for processes holding at most a sixty-fourth; 32,768 separately mapped runs per VM, each a mapping of the
  backend's; 65,536 UVM mappings of registered memory per VM, a quarter
  per guest process, the last sixteenth kept for processes holding at most
  a sixty-fourth.
- **Coherent on the GPU.** Guest RAM is cached write-back in the guest
  whatever RM thinks (§15 of ARCHITECTURE.md), so every GPU mapping of
  registered memory snoops, as for the other system memory the backend
  rewrites, and so does a context DMA over it (`device/src/rmmem.rs`). Its
  coherency needs no rewrite: RM takes an OS descriptor of ordinary pages
  write-back or refuses it, natively too.

What it does not cover: a holder in the host kernel the backend does not
know of. The list above comes from reading 610.57.04: every place RM
duplicates a caller's object into a client of its own (a semaphore surface,
a memory mapper, NV_MEMORY_EXPORT and the unix export, UVM through
nvUvmInterfaceDupMemory; an event buffer is freed with the memory it names,
an SM debugger's duplicate lasts one call, and the rest are video memory or
confidential compute), and every way NVKMS and nvidia-drm reach RM memory. A
holder that list missed, or one a later release adds, would let the guest
unpin frames the GPU can still reach -- a guest privilege escalation, never
the host's memory. A free the backend itself makes (a closing file's
clients, the session's) releases only what RM confirms it freed; a client
RM would not free keeps its registrations until the session ends. An
ALLOC_MEMORY with a zero hObjectNew is refused: RM would make the object
under a handle it never reports. A registration abandoned in flight (a
fatal signal, a timeout) stays pinned in the guest until the device is
removed, since nothing can say whether RM took it.

---

## 4. The whole host

### What the backend parses

| | dev | HEAD |
|---|---|---|
| guest messages | 8 types; responses capped at 64 KiB | 17 types, where every v2 type except HELLO is refused (EPROTO) until a HELLO succeeds; requests up to 256 KiB, or 4 MiB with indirect descriptors |
| guest payloads | the ioctl header with its nested and deep blocks; mmap and munmap requests | those, plus the IOCTL2 schema interpreter, HOST_OP, WATCH and UNWATCH, and Wayland frames through `wlwire` (codec generated from 39 vendored protocol XMLs, whose build fails if any reachable message carries a descriptor the policy does not classify) |
| host peers | the VMM's vhost-user messages | plus the host compositor's Wayland messages, export peers' Wayland messages, the capture helper's fixed-size packets (`--inject-socket`, §18), and kernel uevents (messages from any sender but the kernel are dropped). Every descriptor received over a socket is classified by what the kernel says it is (`hostfd::classify`), not by what the message claims. |

### Host files, sockets and netlink

| | dev | HEAD |
|---|---|---|
| device files | `/dev/nvidia*`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset`, `/dev/dri/renderD*` | the same, the two UVM devices only with `--allow-compute`, plus `/dev/dri/card*` (`--kms-card` only, through OPEN_KMS; a plain OPEN of a card is refused), lessee files received from the compositor, which must classify as a lease of this GPU, and `/dev/udmabuf` |
| other files | `/proc/driver/nvidia`, PCI config in sysfs | the same, plus memfds for shm pools, blobs and a sealed page, and readlink of `/proc/self/fd` |
| vhost-user socket | default `/tmp/nvgpu.sock`, and a failed unlink was ignored, so another user could bind it first and receive the guest's memory | default `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` in a 0700 directory, refused if the directory is anyone else's; a file already at the path is removed only if it is a socket of the backend's uid, and anything else stops the start (`device/src/posture.rs`) |
| other sockets | none | with `--wayland-socket`, one connection to the compositor per channel plus a probe connection; with `--wayland-export`, a listener created 0600 that admits only peers of the backend's uid, with 16 pending; with `--inject-socket`, a `SOCK_SEQPACKET` listener created 0600 (opened to the capture helper's group by root after start) that serves only `--inject-uid`, four connections at once (§18) |
| netlink | none | `NETLINK_KOBJECT_UEVENT`, receive only, with `--kms-card` or `--wayland-lease` |

### Privileges

| | dev | HEAD |
|---|---|---|
| identity | whoever started it. The shipped `rig/run-guest.sh` ran it as root, which makes every guest process an RM administrator: all of BAR0 mappable read-write, the register allowlist skipped, and DRM files authenticated. | Refuses to start with euid 0 or CAP_SYS_ADMIN unless `--allow-root-unsafe`. The launcher, as root, gives each VM a slot of a user pool: the backend runs through `setpriv` as `nvgpu-vmN`, in the groups video, render and kvm, with no capabilities and no_new_privs, and the VMM under nesbox's jailer as `nvgpu-vmmN` (below, "One uid per VM"). With `--wayland-socket` the backend runs as the socket's owner (the desktop user), and with `--wayland-export` as the owner of the export socket's directory. Unprivileged (the rig), both are the invoking user. |
| capabilities | whatever it was given | All dropped before the first thread exists (effective, permitted, inheritable, ambient, and the bounding set where it may), then no_new_privs, undumpable, umask 077. This holds under `--allow-root-unsafe` too, and RM decides administrator by `capable(CAP_SYS_ADMIN)` (`nv-linux.h`), so even then RM sees none. What root keeps is file access by uid. |
| sandbox | none | Before the first guest message and its second thread (`device/src/sandbox.rs`, below): a network namespace of its own; Landlock to the GPU's nodes, `/proc/driver/nvidia`, `/proc/self` and the GPUs' sysfs, read-only but for the nodes, plus the compositor's and its own export socket; a seccomp allowlist of 84 syscalls, 13 with arguments checked, anything else stopping the process; RLIMIT_CORE 0. Each layer the kernel lacks is logged `sandbox: DEGRADED` and stops the start; `--sandbox=best-effort` and `--sandbox=off` are diagnostic flags. No cgroup of its own. RLIMIT_NOFILE is raised to its hard limit at start. The host RM must keep a client to the file it was made on: the backend asks it at start, and refuses to run on one that does not (§11, R3) |

### Threads and resource caps

| | dev | HEAD |
|---|---|---|
| threads | the transport, and one event pump that polled every open handle every millisecond | the transport; one pump, which sweeps only handles it has reported readable; up to 16 executors; one closer thread for display files; one reader per Wayland channel (at most the channel cap, 64 by default); short-lived close threads; one export accept thread and one hotplug thread in those modes |
| handles | no cap but RLIMIT_NOFILE | 65,536 per VM, or what RLIMIT_NOFILE backs (half of the hard limit less 1,024 kept for the backend's own); a quarter per guest process, the last sixteenth kept for processes holding at most a sixty-fourth (`device/src/quota.rs`) |
| RM counters | one map entry per distinct guest value, unbounded | counted only when RM said NV_OK, at most 4,096 keys (`device/src/tally.rs`) |
| logs | unbounded; the launcher wrote them to an unrotated file | every call site limited to a burst of 50 and 10 a second (`device/src/ratelimit.rs`) |
| display caps | -- | 64 NVKMS opens, 16 per guest process; 1,024 syncobj wait registrations; semaphore-surface contexts at 64 per file, 96 per guest process and 256 per VM; 4 KiB of undelivered DRM events per handle, past which the host's own backpressure applies |
| window | the zones, first come first served | each zone (by default UC 32 MiB, WC 768 MiB, WB 224 MiB; `--window-size`) at most half per guest process (`--window-owner-share`), the last eighth kept for processes holding at most a sixteenth (§19); a mapping is charged to whoever opened the file it is armed on |
| Wayland caps | -- | 64 channels per VM. Shm: 1 GiB and 1,024 pools per VM, and 512 MiB and 256 pools per connection; the bytes are what live buffers cover (page-rounded, overlaps once), not pool sizes, since a pool's memfd is sparse, SHM_SYNC writes only inside a live buffer, and pages no live buffer covers are punched out. Unread output: 256 MiB per VM and 64 MiB per connection, half the VM's per guest process (the last quarter kept for processes holding at most a quarter). Per guest process -- the client a daemon connection is for (NVGPU_WL_IOC_CONNECT_FOR), else the opener -- a quarter of the channels (the last eighth kept for processes with at most two) and a quarter of the shm bytes and pools, shared by all its connections. 16 unfinished blobs per connection. 131,072 objects per connection. Lease submits: one per 5 s on average, 3 at once. Four are flags: the channel count (`--wayland-max-conns`), the shm byte budget (`--wayland-shm-budget`), the queue budget (`--wayland-queue-budget`) and the lease interval (`--wayland-lease-interval`). The 1,024 pools per VM and the burst of 3 are fixed. |
| not capped | -- | Memory outside the Wayland and window budgets has no limit of the backend's own; the shipped unit (`contrib/systemd/vhost-user-nvgpu@.service`, DEPLOY.md) puts each backend in a cgroup of its own with `MemoryMax`, `MemorySwapMax=0`, `TasksMax` and `OOMScoreAdjust=500`, the rig's launcher does not. Several VMs of one backend user share that user's host limits. |

### The backend's sandbox

`device/src/sandbox.rs`, applied from `vhost-user-nvgpu`'s `main` after the
capability drop and before anything else: before the backend's second thread
(a user namespace can be entered only by a process with one, and Landlock and
seccomp reach the calling thread and what it creates), and before the first
guest message. What it cannot reopen later is opened first: the vhost-user
listener, the export listener, the uevent socket, and RLIMIT_NOFILE raised.

| layer | what it enforces | on a kernel without it |
|---|---|---|
| network | a network namespace with nothing but loopback. As root the launcher makes it (`unshare --net`); otherwise the backend unshares a user namespace and a network namespace together, maps only its own uid and gid onto themselves (so every uid check, its own and the kernel's, is unchanged; supplementary groups keep working, being kernel ids), and drops again the capabilities the new namespace gave it. Every socket the backend uses is a path or was opened before | unprivileged user namespaces off (`user.max_user_namespaces`, `kernel.unprivileged_userns_clone`, AppArmor's userns restriction): the backend keeps the host's network, `DEGRADED` |
| Landlock | opens only: `/dev/nvidiactl`, `/dev/nvidia-modeset`, `/dev/nvidiaN`, `/dev/nvidia-uvm{,-tools}` with `--allow-compute`, `/dev/udmabuf`, `/dev/null`, and this GPU's render nodes (card nodes with `--kms-card`), read, write and ioctl; `--proc-nvidia`, `/proc/self`, `/proc/cpuinfo` and each GPU's PCI directory in sysfs, read-only. Connects only to the compositor's socket and its own export socket (ABI 9). No file written, created or removed anywhere; no signal to, or abstract socket of, a process outside it (ABI 6); no TCP or UDP (ABI 4, 10). A node made after start is out of reach | no Landlock: every file and socket of the uid, `DEGRADED`; ABI below 9: other pathname sockets reachable, below 6: signals to the uid's other processes, `DEGRADED` either way |
| seccomp | 84 syscalls. Threads but no processes (`clone` only with CLONE_THREAD and no namespace flag; `clone3` answered ENOSYS, which the C library falls back from); no PROT_EXEC in `mmap` or `mprotect`; `socket` and `socketpair` AF_UNIX only; `ioctl` anything but TIOCSTI and TIOCLINUX (which descriptor a call is on is not visible to a filter); `prctl` only to name threads and read; `tgkill` only this process; `prlimit64` only its own; the SIGSYS handler cannot be replaced (a kill); `unlink` answered EPERM. Absent: `execve`, `ptrace`, `process_vm_*`, `mount`, `unshare`, `setns`, `bpf`, `perf_event_open`, `userfaultfd`, `io_uring_*`, `keyctl`, `kill`, `bind`, `listen`, and the rest. A syscall not on the list stops the process: the log gets `sandbox: syscall N is not on the seccomp allowlist` and the exit status is 159 | no seccomp filters: every syscall, `DEGRADED` |
| limits | RLIMIT_CORE 0 over undumpable and no_new_privs | -- |

Each start logs one line per layer, `sandbox: DEGRADED: ...` at warning level
for one not fully in force (and then, with `--sandbox=on`, refuses to start),
and `sandbox: every layer in force` when all are;
the unit tests (`sandbox::tests`) fork children that apply the filter, the
Landlock domain and the whole sandbox, check that the backend's own
operations still work under them, and that a forbidden call, a process
clone, an executable mapping, an IP socket, PR_SET_DUMPABLE, replacing the
SIGSYS handler and TIOCSTI each stop the child; on a host with the driver,
one also checks that the GPU's run-time paths stay open under the plan and
that `/dev/kvm`, other sysfs and the host's files do not. The list was taken from the
code, from strace of the unit tests and of a real start and vhost-user
session against the fake-host fixture; a path only the GPU exercises that
needs a syscall not on it shows up as that log line, on the device. What it
does not stop: a backend taken over still has the GPU's nodes and every
ioctl on them (RM, NVKMS, DRM, UVM: the host kernel's GPU surface, §3), the
guest's memory, and the host kernel's syscalls on the list. It is a bound on
what one VM's compromised backend reaches outside that surface: other VMs'
files and sockets, the user's files, the network, other processes.

### One uid per VM

Run as root, `rig/run-guest.sh` takes a free slot N of a pool made once
with `useradd` (the script's header): `nvgpu-vmN` runs the backend, and
`nvgpu-vmmN`, whose group is `nvgpu-vmN`'s, runs the VMM under nesbox's jailer
(chrooted into a jail image built for the run, a mount namespace of its own,
no supplementary groups, no_new_privs). The backend's socket is 0660 in the
slot's group, so the VMM reaches it and nobody else does. Both run in network
namespaces of their own. A slot is free when no launcher holds its lock and
neither user has a live process. Where there is no pool the launcher falls
back to the one user `nvgpu`, and without a jailer to a root VMM, each with a
warning. A uid of its own per VM separates two VMs' host processes by the
kernel's oldest rules, independent of anything this project wrote:

- **Signals.** kill(2) needs the sender's uid to match the target's (or
  CAP_KILL): one VM's backend or VMM cannot stop, or feed a signal to,
  another's.
- **ptrace and `/proc/<pid>/mem`.** Attaching, and reading or writing another
  process's memory through `/proc` or `process_vm_*`, needs every uid and gid
  of the two to match (or CAP_SYS_PTRACE), before the dumpable flag or Yama
  (`kernel.yama.ptrace_scope`, 1 on this host: descendants only) are even
  asked. The backend is also undumpable, which keeps a same-uid process out;
  nesbox is not, so with a shared uid only Yama keeps a VM's VMM, which maps
  all of its guest RAM, from being traced by a same-uid process that is its
  ancestor. With a uid of its own, neither depends on either.
- **`/proc/<pid>/fd`, and the memory behind it.** Guest RAM, the shared
  window, shm pools, blobs and dma-bufs are all anonymous: no path names them,
  and another process can reach one only by being handed the descriptor or
  through `/proc/<pid>/fd`, `/proc/<pid>/map_files` or `/proc/<pid>/mem`,
  all of which take the ptrace check above. The UVM aperture and memory
  registered by its pages live in the VMM's and the backend's own address
  spaces, behind the same check.
- **Files and sockets.** The backend's socket directory is 0700 its own; its
  socket is open to its slot's group alone; the disk copy is 0600 the VMM's.
  An export socket admits only peers of the backend's uid (`SO_PEERCRED`),
  which with a uid per VM means only that VM's own. No VM can connect to
  another's vhost-user socket and take over its guest memory, or to its
  export socket and drive its compositor.
- **RM's security token.** RM's token for a host process is its uid (and
  PID). With one uid, every VM is one principal wherever RM compares tokens:
  OS_SECURITY_TOKEN shares, NV01_DEVICE_0's `hClientShare` fallback, the
  profiler's `hClientTarget` and the channel-ID controls (§3). The backend
  already holds each of those to its own VM's clients, but with a uid per VM
  RM refuses the cross-VM case itself as well.
- **Per-user kernel counts.** Processes (RLIMIT_NPROC), user namespaces,
  inotify instances, pipe buffers and locked memory are counted per uid; one
  VM running into them no longer does it for another.

What a uid per VM does not do:

- **The GPU.** Every VM's work runs on one GPU; what keeps one VM's GPU
  memory from another's is RM's per-client page tables and the GPU's MMU (§2),
  the same for every uid. The device nodes are 0666, so any uid opens them.
- **The host kernel.** A bug in nvidia.ko, the DRM core, KVM, memfd or
  anything else a VM's processes can call crosses every uid.
- **The Wayland modes.** With `--wayland-socket` or `--wayland-export` the
  backend is the desktop user, as every such VM's is, so between them and the
  desktop only the backend's sandbox holds (undumpable; Landlock's signal
  scoping at ABI 6 and socket scoping at ABI 9, which is what keeps one such
  backend off another's export socket). Their VMMs still take slots.
- **The rig.** Run unprivileged, one VM's backend and VMM are the invoking
  user, and share it with the desktop: the VMM, with seccomp but no Landlock,
  can open whatever the user can; RM sees the user's own token; two VMs
  started by one user are one principal. Keep a rig for one VM at a time.
- **Memory.** No cgroup is keyed to the uid: the shipped unit's cgroup
  bounds each backend (DEPLOY.md), and the launcher's scope
  (`NVGPU_MEMORY_MAX`) bounds a rig run.

---

## 5. The host desktop

dev had one path to the host display, and it was unfiltered. NVKMS checks no
permission at all for SET_CURSOR_IMAGE, MOVE_CURSOR, SET_LAYER_POSITION,
SET_DPY_ATTRIBUTE, SET_DISP_ATTRIBUTE, SET_FRAMELOCK_ATTRIBUTE and the
overrides in QUERY_DPY_DYNAMIC_DATA (read from `nvkms.c`), and dev forwarded
all of them. Everything else below is new.

**NVKMS** (`device/src/nvkms.rs`), outside `--kms-card`:

- The cursor, LUT, layer-position and dpy-attribute commands must name a head
  or dpy the guest was granted through a lease.
- Every FLIP element, and every committed SET_MODE head, must fall inside a
  grant. NVKMS itself checks only the layers a flip dirties.
- A grant decided when a call is prepared is checked again when it runs, and a
  revocation waits for a gated call already running.
- `completionNotifier.awaken` is cleared, so a guest flip cannot make
  nvidia-drm WARN on the host.
- DECLARE_EVENT_INTEREST is cut to what a display client needs.
- QUERY_DPY_DYNAMIC_DATA probes a dpy at most once a second when the guest may
  drive it, and once every 30 s when it may not. In between, the last reply
  answers. Each probe is an EDID read under the global `nvkms_lock`.

GRAB_OWNERSHIP, SET_DISP_ATTRIBUTE and SET_FRAMELOCK_ATTRIBUTE run only with
`--kms-card`, and the head and dpy gates are lifted there, since that guest
owns the display.

**KMS master.** SET_MASTER and DROP_MASTER run only on a card file the backend
opened for the guest, never on a lease. A card opened for a guest file that is
not guest master drops host master at once. The card must be the same DRM
device as the guest file's render node. Opening a card and dropping master run
on executors, and display files close on their own thread, so a blocking
modeset does not stall the VM's RM calls.

**Framebuffers** (`device/src/xfer.rs`). A scanout source (FB_ID and
WRITEBACK_FB_ID in ATOMIC, SETPROPERTY and OBJ_SETPROPERTY, and the fb_id of
SETCRTC, SETPLANE and PAGE_FLIP) must be 0 or a framebuffer this VM made, or,
for SETCRTC only, -1, which keeps the CRTC's current framebuffer. Otherwise the
call is refused before the host sees it. The -1 case keeps whatever the CRTC
already shows, which on a newly leased CRTC may be a framebuffer the host
compositor made. Framebuffer ids are device-wide, and without this a lessee
could show the host compositor's or another VM's screen. GETFB and GETFB2 return handles only for the calling
file's own framebuffers. ADDFB2 identifies every handle as NVKMS memory. The
CRC32 ioctls run only on a card or a lessee, never on a render node, which on
its own would see every CRTC.

**Properties.** Fence and pointer properties are accepted only in ATOMIC,
through the fence hook, and are refused on SETPROPERTY and OBJ_SETPROPERTY. A
forced connector probe (GETCONNECTOR with count_modes 0) goes to the host at
most once a second per connector.

**Leases.**

- A guest sees the compositor's lease device only with `--wayland-lease`, and
  only when a probe shows that device hands out files of this GPU.
- A DRM file the compositor sends (the lease device's `drm_fd`, a lease)
  is carried only if it is a primary-node file of this GPU
  (`hostfd::classify`: major 226 below minor 128, nvidia-drm, one of this
  GPU's card nodes). That does not make it a lessee: the `drm_fd` is a plain
  file of the card, and classification cannot tell the two apart. What
  either may do is held by the gates above -- no SET_MASTER or DROP_MASTER
  on it, framebuffers the VM made, the NVKMS grants -- and by the CRC gate,
  which asks the kernel whether the file really is a lessee
  (`kms.rs`, `crc_gate`).
- The backend asks about leases on a timer and when the compositor hangs up.
  A lease that has ended for good has its file closed and its NVKMS grants
  forgotten.
- Lease submits are rate-limited per VM.
- How long a lease is held is not limited. That is what a lease is for.

**The Wayland proxy** (`wlwire/src/policy_table.rs`).

- The allowlist has 42 globals: Hyprland's own set for clients it does not
  trust without `wl_drm`, plus xdg-output, content-type and the lease device.
  The lease device needs `--wayland-lease` and this GPU's device. The syncobj
  manager, which Hyprland's set also has, is offered only when fences are
  served.
- Deliberately absent: security-context, input capture, gamma, data-control,
  virtual keyboard and pointer, input method, every capture protocol, layer
  shell, session lock, output management and `wl_drm`.
- Versions are the lowest of the host's, the vendored XML's and the table's.
- 13 descriptor-carrying messages are classified. Shm is copied into a
  backend memfd, blobs by value into sealed memfds, and streams through
  credit-controlled pipes. Dma-bufs must be host GEM objects of this VM's
  render files, DRM files leases of this GPU, and syncobjs this VM's.

A guest client within this list is still a host window. It can draw anywhere
the compositor places it, receive input when focused, and read and set the
clipboard (`wl_data_device_manager` and primary selection are on the list).

**The Hyprland and aquamarine patches** change the host compositor for every
client, not only VMs. They let a monitor marked `leasable` be leased, released
from the desktop, and taken back when the lease ends. With no monitor marked,
Hyprland's behaviour is unchanged apart from protocol fixes
(`patches/README.md`).

**Export mode.** Host programs of the backend's uid become clients of the
guest's compositor. Their dma-bufs are imported into the guest with their real
type. One listener is allowed per export.

---

## 6. Inside the guest

| node | dev | HEAD |
|---|---|---|
| `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` | 0666 | 0666, unchanged: any guest user reaches everything §3 lists, from a 64- or a 32-bit process (below). The two UVM nodes exist only when the backend runs with `--allow-compute` |
| `/dev/nvidia-caps/*` | 0444 | 0444 |
| DRM node | a hand-made character device, 0666 | a real DRM device per host render node, whose render and primary nodes the guest's DRM core makes. Syncobjs are enabled only when the backend serves fences, and the primary node drives KMS only when the backend offers `--kms-card`. |
| `/dev/nvgpu-wl[N]` | -- | root:root 0660 by default (module parameter `wl_mode`, which refuses any mode giving "other" access), with `contrib/udev/70-nvgpu-wl.rules` giving it to group `nvgpu-wl` for the daemon, which is to be setgid (or its own account), never an application's group. Five ioctls: HELLO, CONNECT, CONNECT_FOR (a connection charged to the client process the daemon names), SEND and RECV. One LISTEN per device, and ACCEPT only from the listener's effective uid or CAP_SYS_ADMIN. The daemon holds at most 4 MiB a client has not read, and closes a client that stays that far behind for 30 s. |
| `/dev/nvgpu-capture[N]` | -- | only with `--inject-socket`: root:root 0660 by default (`capture_mode`, refusing "other" as `wl_mode` does), `contrib/udev/70-nvgpu-capture.rules` giving it to the capture daemon's group. OPEN (an injected buffer as a read-only dma-buf) and OPEN_SYNCOBJ, each needing the id's 128-bit token (§18) |
| adopted DRM files | -- | a lease received from the host becomes a guest DRM file, cloned from a card-node file |
| `nvgpu-wl-guest` | -- | a daemon listening at `$XDG_RUNTIME_DIR/wayland-0` in the guest |

Every guest user reaches RM, as before, but no longer another guest
process's RM objects: a duplicate between clients two processes made is
refused unless the source was shared with the destination, and a second client
named in parameters must pass RM's rule for that field with guest processes
and euids (§3, "RM objects between guest processes"). That rests on the guest
module saying which process and euid make each call, and so holds only while
the guest kernel does. RM's export and import of objects through a
descriptor (NV0000 OS_UNIX) is a second way to move an object between
clients, which the duplicate rule does not see: it is held to native
strength -- the descriptor must be one of the caller's own open control
files (§11, R1, R2), so it takes a file another process handed over, as it
does natively. That the primary client of every RM call is one of the
calling file's own is RM's strict client validation, which the backend
checks the host has at start (§11, R3).

**The guest module's parsers in Rust** (branch `rustguest`). What the module
reads of a guest process's bytes -- IOCTL2's schema walk, the v1 IOCTL
marshalling with its nested blocks, deep pointers and segments and the
descriptors in them, OS-descriptor registrations -- can be built in Rust
(`NVGPU_RUST=1`, `driver/rust/README.md`) instead of C: a core with no
`unsafe` and no panic path (the build checks the object for panic symbols),
which copies every byte of the caller's once and decides on that copy, and
one file of `unsafe` FFI around it. Nothing moves across the trust boundary:
the backend still checks every request itself. The differential test
(`driver/rust/difftest`, the C compiled as it is and run with UBSan and
allocation canaries) and fuzzing found, in the C, now fixed or not carried
over: (1) an OS-descriptor range within a page of 2^64 passed its page bound
as zero pages (DIV_ROUND_UP wrapped) and was registered with one empty page
run -- fixed in the C too; (2) an RM_CONTROL, RM_ALLOC or v1 NVKMS call with
a size and a NULL pointer sent that many bytes of uninitialised guest kernel
heap to the backend, which handed them to RM as the parameters -- both now
answer as the native driver does (RM_CONTROL NV_ERR_INVALID_ARGUMENT in the
struct, RM_ALLOC with no parameters and a size of 0, NVKMS -EPERM); (3)
double fetches of the caller's memory, where the decision and the request
came from different reads: RM_CONTROL's V1V2 count and pointer,
TIME_CORRELATION's clock, the OS-descriptor class word, IDLE_CHANNELS'
flags -- the Rust reads each once. Also fixed in both, with a regression
case each: IOCTL2's `nvgpu_i2_wr()` wrote 4 bytes for a field of width 1 or
2 (no generated field has one; the walk now refuses a descriptor or GEM
field of a width the generator refuses); a V1V2 list count multiplied by 8
in u32 wrapped small (now checked: no deep block); a v1 NVKMS call read and
wrote 16 bytes whatever its ioctl's size (now -ENOTTY unless it is
NVKMS_IOCTL_CMD with 16, as nvkms_ioctl); and a reply naming a host GEM
handle again after a failing `gem_out` hook closed the handle an earlier
proxy of the same reply owned. That last stayed within the caller's own
file: GEM handles are per host DRM file, one per guest DRM file, so the
closed handle was the calling file's (a later GEM_CLOSE of the stale proxy
could then close an object the same file made meanwhile), which the file's
owner can close natively anyway; another process's objects are reached only
through its own files, or through dma-bufs whose import here is a reference
of this file's. And it needs a reply from the backend, which the guest does
not write. The ATOMIC special's parsing of the commit's arrays is in Rust
too (`nvgpu_atomic.c` in C); what it asks of `nvgpu_kms.c` -- object and
property classes, the fence bridge, event reservations -- stays C.

**32-bit processes, and DRM structs of another size** (branch
`drm-compat2`). Two kinds of caller the native kernel serves reach the
guest module now, and neither brings the backend a byte it could not
already be sent:

- *32-bit processes on the NVIDIA nodes.* `/dev/nvidiactl`,
  `/dev/nvidiaN`, `/dev/nvidia-modeset` and the two UVM nodes take
  `compat_ioctl = compat_ptr_ioctl`: the native handler, the pointer
  widened. nvidia.ko, nvidia-modeset and nvidia-uvm do the same -- each sets
  `.compat_ioctl` to its `.unlocked_ioctl` (nv.c:251-261,
  nvidia-modeset-linux.c:2015-2025, uvm.c:1074-1084, uvm_tools.c:2774-2784,
  595.99.02), because RM, NVKMS and UVM parameter structs are fixed-width
  with 8-byte-aligned `NvP64`/`NvU64` fields, so a 32-bit caller's struct
  is the 64-bit one; none of the three reads `in_compat_syscall()`. A
  32-bit process's ioctl is a 64-bit process's with its pointers below
  4 GiB, which a 64-bit process can send too: the module's parsers (C and
  Rust) take every pointer as a u64 and copy through it, whatever its
  value, and nothing in them, in OS-descriptor pinning or in deep segments
  assumes an address above 4 GiB. What the backend holds each call to --
  the RM allowlist's LP64 parameter sizes, the UVM table's sizes -- is what
  RM and UVM check natively, so a 32-bit build whose struct differed would
  be refused in the guest as on bare metal. The one thing a 32-bit process
  cannot do is map a UVM semaphore pool, which the module places only in
  [4 GiB, 32 TiB) (`nvgpu_mmap_uvm_check()`), so it gets no CUDA context
  (CUDA 12 dropped 32-bit applications). The DRM node already had its compat path (the core's ioctls through
  `drm_compat_ioctl()`, as nvidia-drm), and `/dev/nvgpu-wl` and
  `/dev/nvgpu-capture` had `compat_ptr_ioctl` with layouts identical at
  both widths (every `__u64` at an 8-byte offset, sizes multiples of 8);
  `/dev/nvidia-caps/*` answer no ioctl, as nv-caps.c's.
- *DRM ioctls whose struct grew.* The DRM node takes every ioctl it serves
  (nvidia-drm's range, syncobjs, semaphore-surface fences, a KMS file's
  KMS calls, the dumb-buffer pair) by its number alone and runs it on the
  caller's argument normalised as `drm_ioctl()` does
  (`nvgpu_drm_arg_in()`, drm_ioctl.c:848-915): the caller's size copied
  in where both its command and the native one say IN, zero-extended to
  the native struct, and the caller's size copied back, bounded by the
  14-bit size field (at most 16 KiB, on the stack up to 128 bytes). A
  16-byte `drm_syncobj_handle` (the Steam runtime's libdrm) used to get
  -EINVAL; now it is the native call with `point` zero. The handler and
  the backend see only the **native** command -- the kernel's own for
  syncobjs and dumb buffers, the release's schema entry for KMS
  (`nvgpu_i2_native_cmd()`, C and Rust, the difftest holding them equal),
  nvidia-drm's header for its range -- and the IOCTL2 interpreter still
  refuses any other size, so the backend is sent exactly the sizes a
  native-size caller sends. Only GET_DEV_INFO keeps its own rule: its four
  layouts differ in the middle, so each caller is answered in its own.

Which host surfaces each display mode turns on:

| mode | flag | host surfaces it adds |
|---|---|---|
| headless, v2 | none | IOCTL2 render class and NVKMS tables, fences, HOST_OP without OPEN_KMS, `/dev/udmabuf`, memfds |
| Wayland client, and the host's direct scanout of a guest buffer | `--wayland-socket` | compositor connections, `wlwire`, reader threads |
| DRM lease, VK_KHR_display | `--wayland-lease` | lessee files as KMS handles, nvidia-drm grants and NVKMS gates, uevent netlink, leasable monitors in the patched compositor |
| compositor VM | `--kms-card` | host card files as DRM master, OPEN_KMS and DROP_IF_MASTER, CREATE_LEASE, the three NVKMS commands above, uevent netlink. The guest owns the host's display and can show anything on it. |
| export | `--wayland-export` | the listener, host peers, import of host dma-bufs |
| capture injection | `--inject-socket`, `--inject-uid` | a listener for one helper uid, its fixed-size packets, PRIME import and IDENTIFY of its dma-bufs, import of its syncobjs; HOST_OP INJECT_OPEN and INJECT_OPEN_SYNCOBJ (§18) |

---

## 7. What the backend enforces, and what it leaves to the host

**The backend enforces, from tables it holds:**

- which handle kinds take which calls;
- which RM controls, classes and VID_HEAP_CONTROL functions reach RM at all
  (the allowlist, §12), and RM's parameter size for each control on a
  release measured exactly;
- sizes, from the RM profile, the UVM tables and the DRM and NVKMS schemas;
- that no guest pointer reaches the host, since every field the host would
  follow is relocated to a buffer of the backend's or zeroed, and a buffer
  for several pointers of one block is exactly the size RM will copy,
  computed by the backend from that block (`device/src/deepseg.rs`);
- that every descriptor field names a handle of an allowed kind, and becomes
  the backend's own descriptor;
- every refusal in §3;
- the NVKMS grants and gates, framebuffer ownership, KMS master, the CRC gate
  and the property rules in §5;
- the semaphore-surface bounds, ownership and caps;
- that RM shares stay inside the VM, a duplicate's clients are the VM's and
  one guest process's (or shared between them), and a second client named
  in parameters is the VM's and passes RM's rule for its field (§3, "RM
  objects between guest processes");
- that nothing only compute uses is reachable without `--allow-compute` (§3,
  "Compute");
- which UVM pools the VMM maps into the aperture, at what address, and how
  many;
- the host-PID answers;
- the Wayland allowlist, descriptor classes and budgets;
- every cap in §4, and each guest process's share of the VM-wide ones
  (§11);
- that a descriptor RM resolves in the backend (NV0000's OS_UNIX
  controls, the event and fd-carrying escapes) is one of the caller's own
  files, or the call does not reach RM;
- its own privileges and socket.

**It leaves to the host:**

- **RM's checks on every control and class it forwards**, at user rather than
  admin privilege: the allow-listed ones only (§12). This is still the
  largest part.
- The GPU's MMU and RM's page tables, which keep a guest's GPU work in its own
  memory.
- NVKMS's own checks on FLIP layers and SET_MODE (ValidateRequest), with the
  backend's gate in front of them.
- UVM's range checks, with pageable access off.
- The DRM core. `drm_ioctl_permit` on render files; lease filtering, which
  covers CRTCs, connectors and planes only (hence the framebuffer rule);
  master semantics.
- The kernel's syncobj, sync_file and dma-buf rules.
- The host compositor's handling of a client inside the allowlist. Hyprland
  has no per-client object limit (S-7, read from its source).

---

## 8. How it was reviewed

Three reviews, each by independent reviewers, with every claim sent to
adversarial verifiers who tried to refute it (a fourth, of branch `harden`,
is §11):

1. **The design** (its implementation spec, v1; not shipped), by four reviewers. v2 resolved their
   findings, chiefly by making the backend authoritative from its own
   generated tables rather than from buffer and descriptor lists the guest
   declares. That landed in `a555a99`, together with the v1 gating and the
   guarded argument buffers.
2. **Verification against NVK, NVIDIA and Linux** at `fbfa3fb`, through four
   lenses: layout and modifiers, sync, memory and caching, submission and
   display. **25 real findings** (1 critical, 5 high, 10 medium, 9 low); 7 of
   35 claims were rejected.
3. **Security and adversarial review** at `fbfa3fb`, through five lenses
   (parsers, GPU side, desktop, denial of service, posture) and correctness
   walks of the guest, the backend, Wayland and KMS. Each claim went to two
   verifiers. **34 real findings** (1 critical, 4 high, 10 medium, 19 low); 2
   were rejected.

Fixing them turned up five more problems, all fixed: parameter pointers
forwarded when no block came with them (`93283b4`), OS-descriptor memory
(`c16bddc`), UVM's descriptor fields (`c2aeb38`), UVM's sizes (`1c22889`) and
the IMEX and fabric classes (`50286f8`).

**Status: of the 34, 30 are fixed and 4 partly fixed. Of the 25, 23 are fixed,
1 is partly fixed and 1 is open.** "Fixed" means the finding's scenario no
longer works in the code. The fixes have since run on the RTX 5090 in the
rig's regression, security negatives included, which shows they break no
workload, not that each scenario is closed. No second adversarial pass has
been run over the fixes: each status is taken from its
fix commit and the code. S-35, which a later review of the RM path opened
(NV_ESC_RM_SHARE forwarded raw), is fixed in the code the same way.

### Verification findings ([NVK_VERIFICATION](docs/review/NVK_VERIFICATION.md))

| id | sev | finding | fix | status |
|---|---|---|---|---|
| C-1 | critical | 0x54 unbounded index: a host kernel read primitive | `f6c9a66` | fixed |
| H-1 | high | 0x54 imports any host client's semaphore surface | `f6c9a66`, `56628c8` | fixed |
| H-2 | high | OS-event fds in REGISTER_WAITER and NV_EVENT_BUFFER untranslated; 0x90cd can oops the host | `f6c9a66` | fixed |
| H-3 | high | NVKMS FLIP head fields skip every permission check | `b6d58a8`, `3d6e281` | fixed |
| H-4 | high | non-coherent sysmem is WB in the guest on Intel | `a0a71db` | fixed |
| H-5 | high | GET_DEV_INFO overruns a 20-byte caller on 535 | `4c0f556` | fixed |
| M-1 | medium | per-mapping memory type not carried to the guest | `a0a71db`, `2397f75`, `3175dad` | fixed |
| M-2 | medium | read-only host pages mapped writable; a write kills the VM | `a0a71db`, `2397f75` | fixed |
| M-3 | medium | window extent reused while guest PTEs still map it | `a0a71db` | fixed |
| M-4 | medium | capability bits overridden, not ANDed with the host's | `4c0f556` | fixed |
| M-5 | medium | every proxy answers IDENTIFY as NVKMS | `388288c` | fixed |
| M-6 | medium | fd fields come back holding backend handles | `b3450de` | fixed; the envyhooks run is still to do |
| M-7 | medium | CRC32 on a render node reaches every host CRTC | `d1f470d`, made sound by `19c3186` (S-6) | fixed |
| M-8 | medium | FLIP awaken floods host dmesg with WARNs | `b6d58a8`, `3d6e281` | fixed |
| M-9 | medium | NVKMS grants outlive a lease end | `3d6e281`, `b67733d` | fixed |
| M-10 | medium | no cap on semaphore-surface contexts | `f6c9a66` | fixed |
| L-1 | low | 0x57 not mirrored into the guest resv | `5c72866` | partly: export mode not bridged |
| L-2 | low | unwrapping a foreign fence blocks | `5c72866` | fixed |
| L-3 | low | GPU/CPU time correlation not rebased | `d0bf0b7` | fixed |
| L-4 | low | a proxy fence's timestamp is its delivery time | `6bf6cad` | fixed; whether anything reads it is to be confirmed on device |
| L-5 | low | FENCE_SUPPORTED answers yes | `4c0f556` | fixed |
| L-6 | low | UPDATE_DEVICE_MAPPING_INFO zeroes the caller's addresses | `8f6a075` | fixed |
| L-7 | low | "no crossings per frame" measured only without presenting | -- | open: needs the per-present measurement |
| L-8 | low | stale proxy-size comment | `388288c` | fixed |
| L-9 | low | useSyncpt refused when not specified | `b6d58a8`, `3d6e281` | fixed |

### Security findings (FINDINGS)

| id | sev | finding | fix | status |
|---|---|---|---|---|
| S-1 | critical | RM control pointers reach RM as guest addresses in the backend | `93283b4`, `c16bddc`, `f8405cf` | fixed (item 2, refusing non-table controls larger than their inline block, was judged unnecessary) |
| S-2 | high | Wayland shm capped only per connection | `86553c1`, `de95ad3` | fixed |
| S-3 | high | no per-VM cap on Wayland channels | `de95ad3` | fixed |
| S-4 | high | shm memfds: host OOM the OOM killer cannot attribute | `86553c1`, `de95ad3` | fixed; a cgroup per backend is the shipped unit's (DEPLOY.md); refusing SHM_SYNC before commit not done |
| S-5 | high | a root backend makes every guest process an RM administrator | `b3c126b`, `ae182ab` | fixed; class 0x3f allocatable (the Vulkan driver makes one) but its BAR0 mapping is RM-admin only and the backend is never admin; forcing non-privileged RM clients, not done |
| S-6 | medium | KMS commits can scan out any host framebuffer | `19c3186` | fixed |
| S-7 | medium | channels are full compositor clients, with no per-VM cap | `de95ad3` | **partly**: channel cap and queue budget done; per-interface object caps, a CONNECT rate limit and RLIMIT_NOFILE not |
| S-8 | medium | forced EDID reads hold `nvkms_lock` from up to 16 executors | `429d57c` | **partly**: probes rate-limited; one executor lane per class and a lower ALLOC_DEVICE cap not done |
| S-9 | medium | lease churn stalls the host compositor | `6e4c501` | fixed in the backend; the Hyprland-side debounce not done |
| S-10 | medium | `/dev/nvgpu-wl` 0666; any guest user can ACCEPT host programs | `e1e26d7`, `de95ad3` | fixed |
| S-11 | medium | a host GEM handle gets two guest owners | `f264599` | fixed; edge cases open (§9) |
| S-12 | medium | a dead fence's consumer signals a reused id | `037c20f` | fixed |
| S-13 | medium | a destroyed syncobj's wait registration is joined | `2e4c715` | fixed |
| S-14 | medium | NVKMS gates checked at prepare, not at run | `5baeb66` | fixed; the GET_LEASE check at run time not done |
| S-15 | medium | EXPORT_TO_DMABUF_FD forwarded raw | `db4d736` | fixed by refusal; translation is future work |
| S-16 | low | blobs hold memfds with no count limit | `86553c1` | **partly**: count cap done; one descriptor budget and RLIMIT_NOFILE not |
| S-17 | low | RM counter maps grow without bound | `d773f45` | fixed |
| S-18 | low | no per-session channel limit | `de95ad3` | fixed |
| S-19 | low | a guest can seize a leasable monitor, or churn it | `6e4c501` | churn fixed; holding is by design |
| S-20 | low | guest-triggered log flooding | `d773f45` | fixed |
| S-21 | low | the 1 ms sweep scales with open handles | `07971a3` | fixed |
| S-22 | low | any guest user can exhaust host memory through the proxy | `86553c1`, `de95ad3` | fixed |
| S-23 | low | `/tmp/nvgpu.sock` can be squatted | `b3c126b` | fixed |
| S-24 | low | host PIDs of every GPU client readable | `32fd293` | fixed by answering locally; a PID namespace is the real fix |
| S-25 | low | GEM-in drops the proxy reference before the host call | `bc2fcc1` | fixed |
| S-26 | low | device removal leaves objects pointing at freed memory | `3e930d3` | **partly**: calls in flight during teardown are unguarded, and window pages stay mapped |
| S-27 | low | executor pool loses a wake-up | `7dad490` | fixed |
| S-28 | low | a failed lease probe hides the device for good | `5cf36ad` | fixed |
| S-29 | low | guest daemon busy-loops on a hung-up client | `2e003c5` | fixed |
| S-30 | low | lease probe under the connection lock; timeout cached | `5cf36ad` | fixed; the cache is not cleared on compositor restart |
| S-31 | low | every WL_RECV allocates the frame limit | `cfd63c3` | fixed |
| S-32 | low | an unresolvable syncobj becomes a placeholder | `a6d3d2f` | fixed |
| S-33 | low | final closes of display files block the VM's threads | `03b3539` | fixed; the pump's duplicate can delay a master release until UNWATCH |
| S-34 | low | OPEN_KMS and DROP_IF_MASTER run on the queue thread | `03b3539` | fixed; a single "open non-master" op not added |
| S-35 | high | RM_SHARE forwarded raw: a guest shares an object with every host client; DUP_OBJECT from a client not the VM's; every guest process duplicates every other's objects | `f550116` | fixed; opened by the review after these, not by the 34. Its review then found a grant outliving an object freed with its parent, an object's own list not overriding its client's, and the FIFO controls' client lists unchecked; fixed in the commit after. Grants on intermediate objects are not followed (refused where RM allows) (§9). A later pass dropped the caller-made-the-source allowance (RM's rule is the two clients' makers), made a guest without process ids fail closed, and held each second client to RM's rule for its field with the guest's euids |

Rejected by the security review:

- "There is no sandbox, and the planned per-process isolate cannot hold the
  display paths." The missing sandbox is already known and is in §9.
- "Host CLOCK_MONOTONIC and the host's DRM dev_t numbers are disclosed to
  guest users." The review did not count that as a finding. The dev_t numbers
  reach every guest user. The clock offset, in `/dev/nvgpu-wl`'s HELLO, now
  reaches only root and the `nvgpu-wl` group by default, because the node is
  0660 (S-10).

---

## 9. Residual risk

In rough order of weight.

1. **RM_CONTROL and RM_ALLOC are allow-listed, and what is allowed is still
   a host surface.** 215 of 610.57.04's 1,370 controls and 96 of its 222
   classes reach RM (§12); each is checked by RM as it would check a user
   process without admin rights, and a bug in any of them is a host bug. The
   list covers what 106 hardware runs and the application pass used, and the
   other architectures' classes for the same objects. A call it misses fails
   with RM's own "not supported" and a log line naming it;
   `--rm-allowlist=log`, a diagnostic flag, finds them without failing.
   24 controls go to GSP-RM with no CPU-side table and no public name; they
   are allowed by number because nvidia-smi and the drivers sent them.
2. **The host NVIDIA driver is in the TCB.** There is no IOMMU boundary. A bug
   in any forwarded path, or in RM's page tables for a guest's GPU work, is a
   host bug. For mutually untrusted tenants, VFIO with an IOMMU or vGPU is
   still the answer.
3. **The backend's sandbox is only as strong as the kernel under it.** One
   process per VM maps all of that guest's RAM, holds every host descriptor,
   and parses everything in §4. Its sandbox (§4) keeps a compromised one to
   the GPU's nodes and a few read-only files, without a network, on a list of
   syscalls; ioctl on those nodes is the host kernel's GPU surface, whole.
   - Each layer degrades on a kernel without it, loudly. Below Landlock ABI 9,
     other pathname sockets the uid can reach stay reachable, which matters
     where backends share a uid.
   - With `--wayland-socket` or `--wayland-export` the launcher runs it as the
     desktop user, so a compromised backend is that user, within the sandbox.
   - The syscall list was built from the code, the unit tests and a start
     against the fake host, not from the device: a device path that needs a
     syscall it lacks stops the backend (status 159, the number logged).
   - nesbox's own filter allows `openat`, `socket` and `connect`, and nesbox
     is dumpable; under the jailer (root) that is inside a chroot as a uid of
     its own, but in the rig it is the user. Its baseline filter covers the
     UVM aperture's path (`mmap` with MAP_FIXED_NOREPLACE, `mincore`, the
     memory-slot ioctl); its tighter vCPU filter is defined but never
     installed, and if it were, the window-request thread a vCPU spawns at
     DRIVER_OK would inherit it without `clone` or `mincore`.
4. **The isolate is not built.** ARCHITECTURE.md, "Future work", describes one helper per
   guest process holding the descriptors. The display paths share objects
   across a VM's processes (a compositor holds its clients' buffers) through
   one VM-wide handle table, so a helper per VM is probably the first one to
   build. That is reasoned, not tried.
5. **Descriptors and memory.** RLIMIT_NOFILE is raised to its hard limit at
   start and the handle table sized from it, with a share per guest process
   (§11, B1). The Wayland and window budgets bound shm, queues and the
   window; nothing bounds the backend's memory as a whole. A guest process
   that forks enough can still take a pool, a share at a time.
6. **Open items after the fixes.**
   - **Backend posture.** Class 0x3f (RegisterMemory) is on the RM
     allowlist (§12), since the Vulkan driver allocates one; RM maps its BAR0
     only for an admin client, which the backend never is. Non-privileged RM clients are not forced, which matters only if the
     backend is ever given
     CAP_SYS_ADMIN. There is no `--socket-fd`. PID namespaces are left to the
     launcher. EXPORT_TO_DMABUF_FD is refused, so NVIDIA's GBM RM export path
     and `cuMemGetHandleForAddressRange(DMA_BUF)` fail.
   - **Untranslated descriptors.** Some RM controls carry a descriptor that
     nothing translates: CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL's devDescriptor, the
     NV00E0 export and NV00FD ATTACH_GPU devDescriptors, and NV00FD
     REGISTER_EVENT's pOsEvent. (NV0000's OS_UNIX controls, which this list
     once missed, are translated or refused now: §11, R1.) The guest's number reaches RM as a number in
     the backend's table. The fabric classes are refused, which keeps this low
     rather than none.
   - **UVM_INITIALIZE.** The guest driver writes the forced flags back into the
     caller's memory: a difference from native, not a host exposure.
   - **The UVM aperture.** Its safety rests on the VMM doing what §3 says:
     `MAP_FIXED_NOREPLACE`, the page check, slot before mapping, and reading
     an unslotted aperture address as nothing. The backend cannot see any of
     it. Two guest processes whose pools overlap cannot both have one mapped,
     so the second CUDA context fails, and one process can find another's
     pool address, or squat on it, by trying (§11, B5).
   - **KMS.**
     - One executor lane per class per VM, and a lower ALLOC_DEVICE cap
       (S-8), are not done.
     - Hotplug uevents do not clear the dpy probe cache.
     - The GET_LEASE check does not run again at run time (S-14).
     - S-11 has three edge cases left: re-homing in the reverse direction, a
       host handle leaking into the render file on a KMS -EAGAIN, and a
       possible reference cycle between borrowing re-home entries.
     - Chardev and Wayland ioctls in flight while the device is torn down are
       unguarded, and window pages stay mapped after removal (S-26).
     - The pump's duplicate of a display file can delay a master release
       until UNWATCH (S-33).
     - There is no single "open non-master" OPEN_KMS op, so the open and the
       drop are still two steps (S-34).
   - **Wayland.**
     - No per-interface object caps (S-7), no CONNECT churn limit, and no
       refusal of SHM_SYNC before commit (S-4).
     - No backpressure towards the compositor (S-18).
     - The lease cache is not cleared on compositor restart (S-30).
     - The Hyprland patches have no lease debounce, and
       `setLeaseOffered(false)` does not end an active lease. Short of ending
       the VM or unplugging the monitor, the host cannot take a leased
       monitor back.
   - **Fences.**
     - The optional guest hardening for S-13 is not done: the guest does not
       re-poll a joined wait's point before it signals the eventfd.
     - An 0x57 attach is mirrored into the guest's reservation object, but
       export mode is not bridged (L-1).
   - **Still to measure or run.**
     - "No crossings per frame" is measured only without presenting; the
       per-present measurement is not done (L-7).
     - The envyhooks run that checks the M-6 fix, descriptor fields no longer
       coming back holding backend handles, is not done (M-6).
     - Whether any caller reads a proxy fence's timestamp is still to be
       confirmed on the device (L-4).
7. **Behaviour that is correct and still worth knowing.**
   - In compositor-VM mode the guest owns the host's display, and could draw a
     convincing host login screen.
   - A guest client in Wayland mode reads the host clipboard when focused.
   - An export-mode peer is driven by the guest.
8. **A host driver older than 535.129.03** gets no RM profile. The backend
   then forwards RM escapes with no size check and logs a warning
   (`set_driver_version`, `device/src/nvidia.rs`). It does not refuse them.
   Pointer scrubbing still applies, and UVM and NVKMS fail closed there.
9. **The on-device negative test has a hole.** In
   `rig/verify/sec-negative.c`, the VID_HEAP_CONTROL half of T1 sets
   function 8 where ALLOC_OS_DESCRIPTOR is 27 (`nvos.h`), and sends a
   1,064-byte block where the escape is 184. The backend refuses it on size, so
   it passes without testing what it names. The backend's unit test
   (`device/src/guestptr.rs`) covers the real case.
10. **Nothing here has met a real workload.** A refusal that is too broad
    shows up as a broken application, and one that is too narrow shows up as
    nothing at all. Only running [`TESTING.md`](TESTING.md) tells the two
    apart. That applies to the headless path too: its UVM, RM and coherency
    changes are unmeasured.
11. **Registered guest memory is released by a list of holders.** The
    backend tells the guest to unpin memory registered by its pages only
    when the RM objects, duplicates and UVM external mappings holding it are
    gone, and refuses to export it (§3, "Memory registered by its pages").
    That list was read out of one release's sources. A holder it misses
    releases the pages early, and a guest process can then reach frames its
    own kernel has reused: never the host's memory, but a guest privilege
    escalation. The rest errs late: a mapping UVM refused, a range past the
    recorded bound or a mapping that will not come down when its UVM file
    closes keeps the pages pinned until a later free or the session, and
    counts against the registration budgets meanwhile. Exporting registered
    memory (Vulkan's external memory host exported onward, say) fails with
    EPERM where it works natively. CUDA's own registrations (the buffer
    `cuCtxCreate` registers) have run on the RTX 5090; that shows the holders
    a workload uses let go as expected, not that the list is complete.
12. **RM objects between guest processes rest on the guest kernel and on
    reading.** Which guest process made a client, as which euid, and which
    makes a call is the guest kernel's word (§2); a guest module that cannot
    say fails closed. Grants are followed on the object and on its client
    only; a CLIENT grant on a device or other intermediate object that RM
    would honour for the objects under it is refused here, a client's grant
    stops counting once any of its objects has a list of its own, and any
    free in a client drops its objects' grants. Each second-client field's
    rule was read from 610.57.04's CPU-RM sources; where RM defers to GSP-RM
    the backend assumes GSP-RM checks nothing between host processes (its
    token is the GFID or none) and requires the calling process, which may
    refuse a cross-process tool (a profiler or debugger of another process
    of the same user) that works natively. RM's USER_ROOT bypass of the
    token checks is not given to guest root. The on-device checks,
    sec-negative T8 and T10, pass on the RTX 5090; no workload of the 88
    runs before the application pass issued NV_ESC_RM_SHARE.

---

## 10. Hardening roadmap

In priority order. Cost is a judgement, not a measurement.

1. **Run the rest of [`TESTING.md`](TESTING.md)**: the compositor-VM and
   export modes, hotplug, and the performance stages. Groups A and B, the
   security negatives included, have run on the RTX 5090
   (`rig/TESTING-RIG.md`).
2. **A memory cgroup per backend: done in the shipped unit.**
   `contrib/systemd/vhost-user-nvgpu@.service` runs each VM's backend in a
   cgroup of its own with `MemoryMax`, `MemorySwapMax=0`, `TasksMax` and
   `OOMScoreAdjust=500`, making the backend the preferred victim
   ([`DEPLOY.md`](DEPLOY.md)). Memfd shmem is charged to the writer's
   cgroup, so an OOM stays inside the VM that caused it. The rig's launcher
   still uses one scope for the whole run (`NVGPU_MEMORY_MAX`).
3. **One descriptor budget.** RLIMIT_NOFILE is now raised at start and the
   handle table sized from it (§11, B1); what is left is to size one per-VM
   descriptor budget from it. The handle table, blobs,
   pools, streams and adopted compositor files would all draw from that
   budget, and MAX_HANDLES would come from it rather than a constant (S-16,
   S-7).
4. **A PID namespace per backend** (`PrivatePIDs=yes`, or `bwrap
   --unshare-pid`). With the namespace, RM filters the host-PID controls
   itself; `rmctl.rs` then stays as a second fence. (A uid per VM is done:
   §4, "One uid per VM".) For the Wayland modes, give
   the backend a `wp_security_context_v1` socket from the desktop session
   rather than running it as the desktop user. Hyprland then filters globals
   for those clients itself (`filterGlobals`, `src/Compositor.cpp`). Its list
   lacks three of this proxy's additions: the lease device, xdg-output and
   content-type. It already has the syncobj manager (and `wl_drm`, which this
   proxy leaves out). Leasing would need a Hyprland patch; explicit sync would
   not.
5. **The RM allowlist (§12): done and run.** Default deny, enforcing, run
   through Groups A and B and the application pass (NVENC, NVDEC and Vulkan
   Video included; six controls added from it). What is left: holding each
   control to the classes of the object it is sent to (§12, "What it does
   not do"), and growing the list from the teardown's "RM allowlist refused"
   lines as new workloads meet it.
6. **seccomp and Landlock per backend: done and run** (§4, "The backend's
   sandbox"): no run of the regression or the application pass stopped the
   filter. An `ioctl` filter by request number per descriptor kind is not
   possible in a filter, and would take the isolate.
7. **The isolate**: the host descriptors held by an unprivileged helper apart
   from the process that maps guest RAM, so that a parser bug in the backend no
   longer yields the descriptors, and a driver-call bug no longer yields guest
   memory. Probably one per VM (§9, item 4).
8. **The rest of §9, item 6.** In rough order:
   - one NVKMS and KMS executor lane per VM, and a lower ALLOC_DEVICE cap;
   - per-interface Wayland object caps and a CONNECT rate;
   - the Hyprland lease debounce, and `setLeaseOffered(false)` ending an
     active lease;
   - forcing non-privileged RM clients, as a second
     fence against a privileged backend;
   - translating EXPORT_TO_DMABUF_FD, once something needs it.
9. **Parse, don't patch: the rest (§14).** The data rewrites that are
   still edits of the host's copy (the coherency attributes, NVKMS policy,
   fence waits) made declared values; RM's top-level blocks as typed
   structs per release.
10. **What the `harden` audit left open (§11).** (A uid per VM in the
   launcher is done.) The UVM aperture carved out of the VMM's address space before
   guest RAM is mapped (F3; done for crosvm, §16, open for nesbox); a caller's process on IOCTL2 (R4); fair queuing
   per guest file (B6); the remaining VM-wide pools split per process (B7);
   a security context per Wayland channel (W4).

---

## 11. The `harden` audit

A review of branch `harden` (from `b6a508c`, with nesbox `virtio-nvgpu-v2`)
through two lenses, judged against the product's requirement: no host
attack surface a sandboxed app would not have on the host's own driver, and
no app less protected from another -- in one guest or across VMs -- than it
would be natively. **Host surface**: what `harden` added over `4c15f6e`
(the UVM device and aperture, sharing mode, pages for OS descriptors, deep
segments, IDLE_CHANNELS lists, rmshare) is all gated or bounded, and with
`--allow-compute` off (the default) no UVM, aperture or page-list path is
reachable; four findings. **App isolation**: two critical findings, where
one app could import, overwrite or clear another's exported GPU memory
through RM controls that name a file by descriptor, and a set of VM-wide
budgets one app could empty for all the others. Everything was read from
source (the guest module, the backend, the daemon, NVIDIA 610.57.04); the
fixes are unit-tested and the guest module builds clean. At the time none of
it had run on the GPU; it has since, in the rig's regression and application
pass (`rig/TESTING-RIG.md`).

### Findings

| id | sev | lens | finding | fix | status |
|---|---|---|---|---|---|
| R1 | critical | app vs app | NV0000 OS_UNIX GET_EXPORT_OBJECT_INFO, CREATE_EXPORT_OBJECT_FD, EXPORT_OBJECTS_TO_FD and IMPORT_OBJECTS_FROM_FD reached RM with the guest's descriptor number, which RM resolves among every guest process's files in the backend: import another app's exported memory, overwrite or clear its export slots | `fc7a58b` | fixed: all six descriptor-carrying OS_UNIX controls translated in the guest and the backend to the caller's own control file (the backend accepts only a control file, RM's own rule), any other number EBADF, -1 alone passed; MEMACCT (a cgroup descriptor) and undefined OS_UNIX commands answered NOT_SUPPORTED without RM; sec-negative T11 |
| R2 | critical | app vs app | EXPORT/IMPORT_OBJECT_FROM_FD fell back to the caller's raw number when it did not translate, which the backend read as one of its handles | `fc7a58b` | fixed: fail closed in the guest (EBADF), as the event path does |
| B1 | high | app vs app | the handle table was bounded in practice by the inherited RLIMIT_NOFILE (1,024), one pool for every guest process: one app opening /dev/nvidiactl ~1,000 times left every other app EMFILE | `6b75a82` | fixed: RLIMIT_NOFILE raised to the hard limit at start, the table sized from it with 1,024 descriptors kept for the backend; every handle charged to a guest process (OPEN and HOST_OP now carry the caller, derived handles go to the owner of the file the call ran on), a quarter each, the last sixteenth for processes holding little |
| B2 | high | app vs app | the shared window (UC 32, WC 768, WB 224 MiB) was one pool: one app mapping 768 MiB left every other app's mappings ENOMEM | `6b75a82` | fixed: each zone at most half per process, the last eighth for processes holding at most a sixteenth; a mapping is charged to whoever opened the file it is armed on |
| F1 | medium | host surface | ALLOC_SEMAPHORE_POOL's length reached UVM uncapped: host kernel memory allocated before the backend decided the pool would never be mapped | `6b75a82` | fixed: refused before UVM for a length that could never be mapped, or past the file's (256 MiB), the process's or the VM's (1 GiB) pool budget, mapped or not |
| R3 | medium | app vs app, cross-VM | no RM call's own client was checked by the backend; isolation rested on RM's strict client validation, which a registry key turns off, and every guest process and every VM of one backend user then share RM's fallback token | `fbe3f22` | fixed by refusal: the backend asks RM at start (a client made on one control file, used from another) and will not run on a host that serves it. A check of its own in the backend (the primary client issued on this file, or one registered to it with REGISTER_FD) is not done: RM, verified, already applies exactly that rule, and a copy that missed a path would break real workloads |
| B3 | medium | app vs app | fence contexts 64 per file and 256 per VM: four files of one app took them all | `6b75a82` | fixed: 96 per process, the last 32 for processes holding at most 16 |
| B4 | medium | app vs app | 64 NVKMS opens per VM, all takeable by one app | `6b75a82` | fixed: 16 per process, the last 8 for processes holding at most 2 |
| B5 | medium | app vs app | UVM placements, pools and OS-descriptor registrations capped per VM (four files took them); pools of two processes at one address collide in the one VMM address space, with a distinct errno | `6b75a82`, `fbe3f22` | **partly**: every one of those budgets now per process as well (a file's worth, with a reserve); a collision is ENOMEM like any refusal. Not fixable here: one process can still find another's pool address by trying, and squat on the address CUDA would use. UVM maps a pool only at the host address equal to its offset, so every guest process's pools share the VMM's one address space; only per-process VMM address spaces, or a UVM that maps at an offset, would end it |
| W1 | medium | app vs app | 64 channels per VM, and the daemon opened one per client with no per-peer limit; 1 GiB of shm per VM at 512 MiB a connection | `b5980e5` | fixed: the daemon opens each client's channel with CONNECT_FOR, naming the client (SO_PEERCRED), and the guest module charges the OPEN to it; the backend holds each process to a quarter of the channels, a quarter of the shm (one budget for all its connections) and half of the queue budget |
| W2 | medium | app vs app | the daemon's per-client output buffer had no bound: a client flooding requests whose replies it never read grew the daemon until the OOM killer took it, and every client with it | `b5980e5` | fixed: at 4 MiB unread the daemon stops reading that client's channel, and closes a client that stays behind for 30 s; the output waits in the backend, on that client's share (loopback test) |
| W3 | medium | app vs app | at the VM's queue budget the connection whose output crossed it was dropped, often an innocent one; the rig's app pass (`6263e20`, on `display-passthrough`) puts the app user in `nvgpu-wl`, so any app can hold raw channels it never reads | `b5980e5` | fixed in the backend: the queue budget is per process, so the connection that crosses its share is its own. The guest-image side is fixed too: `rig/guest-image/probes/apps.sh` (`nvgpu_user=1`) no longer adds the app user to `nvgpu-wl`, and gives the group to the daemon alone (§17, "The Wayland proxy") |
| F2 | low | app vs app | export/import to a descriptor is a second way to move an RM object between clients, outside the DUP_OBJECT gate | -- | native strength, documented (§6): after R1/R2 the descriptor must be a control file the caller has open, which another process can only have handed it |
| F3 | low | host surface | the aperture band [4 GiB, 32 TiB) can hold the VMM's own mappings, guest RAM among them, when its stack rlimit is unlimited (or above about 96 TiB: x86 then starts the mmap area near 21 TiB): a pool there fails, and says the address is taken | crosvm `patches/crosvm/0007`, `0008` | **fixed for crosvm, open for nesbox.** crosvm reserves the band (PROT_NONE, MAP_NORESERVE) at the start of `run_config`, before it maps guest RAM, and maps each pool with MAP_FIXED over its own reservation (§16); UVM refuses a pool moved in with mremap, so the window's map-then-move cannot be used. nesbox would need the same change to its start-up order, and a run to trust |
| F4 | low | host surface | CARD_INFO, ATTACH_GPUS_TO_FD and NUMA_INFO pass with no size check | -- | unchanged (§3): the argument is never smaller than `_IOC_SIZE`, and ATTACH_GPUS_TO_FD, read again (nv.c), carries GPU ids only, no descriptor, once per file |
| R4 | low | app vs app | SEMSURF_FENCE_CTX_CREATE accepts any client of the VM, not the caller's own | -- | open, native strength (KAPI dups at kernel privilege). It needs the caller's process on IOCTL2, which carries none; the render file's opener is not the same thing |
| R5 | low | app vs app | negative descriptor values other than -1 forwarded, read by the backend as handles | `fc7a58b` | fixed: refused in the guest and the backend |
| B6 | low | app vs app | one queue thread serves every inline host call, and 16 executors every file | -- | open: needs fair queuing per guest file and Wayland traffic off the RM queue |
| B7 | low | app vs app | fence wait registrations, rmshare grants, rmmem records and the lease throttle are VM-wide | -- | open: each is a degradation (polling, a refused share, a write-combining guess), not a denial of the device; the same Ledger split applies when needed |
| W4 | low | app vs app, cross-VM | every guest app reaches the host compositor as the backend: a compositor permission given to one is given to all | -- | open: a `wp_security_context_v1` per channel needs a listening socket per channel from the backend and a compositor that keys permissions on it |

### What the budgets per process are, and are not

`device/src/quota.rs`. Each VM-wide pool keeps its cap, and each guest
process -- as the guest kernel names it, the tgid and start time of the
calling thread group (`ProcId`) -- may hold only a share, with the last
part of the pool kept for processes that hold little of it. The owner of a
handle is the process that opened it (OPEN) or asked for it (HOST_OP), or
the owner of the file the call that made it ran on (IOCTL2, Wayland
receives); a window mapping is charged to the owner of the file it is armed
on; a daemon connection to the client it is for. A guest that does not
say (a module without BCAP_PROC_ID) is held to the per-VM caps, as before.

What it does not do is see through fork: every child is a new process with a
share of its own, so an app that forks four times can still take a pool a
quarter at a time. What bounds that is the guest's own process limits
(RLIMIT_NPROC, a pids cgroup, the sandbox's), as the per-process descriptor
limit does natively. It also rests on the guest kernel's word about which
process is which (§2), and `/dev/nvgpu-wl`'s group may charge a channel to
any process it names, which is why that group is the daemon's alone.

---

## 12. The RM allowlist

Branch `rmallow`: a default-deny list of the RM controls and classes a guest
may reach, per host release (`gen/rmallow_extract.py`, `gen/src/rmallow/`,
`device/src/rmallow.rs`). Before it, every control RM exports and every class
it can make, but 15 and 12, reached the host's RM from any guest process.

**Policy.** A control or class reaches RM only if all of these hold, per
release:

1. RM exports it (an NVOC method, a deprecated V1 control, an RS_ENTRY), read
   from that release's sources.
2. RM would serve it to an unprivileged process: NON_PRIVILEGED and not
   PRIVILEGED, KERNEL_PRIVILEGED or INTERNAL (flag values read from each
   release's `control.h`: they moved between 535 and 580); for classes,
   ALLOC_NON_PRIVILEGED. RM refuses the backend the rest anyway; not
   forwarding them keeps their lookup paths, and any bug before RM's check,
   out of reach, and holds if the backend is ever privileged. Class 0x3f,
   NV01_MEMORY_LOCAL_PRIVILEGED, is the exception: RM lets any user allocate
   it and the Vulkan driver does (vulkaninfo fails without it); the BAR0
   mapping that makes it dangerous RM gives only an admin client, and the
   backend is never one (S-5, §8).
3. It names no host resource the backend does not translate: every field of
   the parameters, nested structs included, that is a descriptor, a process
   id or an OS event refuses the control unless the backend translates it
   (the six OS_UNIX file controls, REGISTER_WAITER). Plus a short list: the
   host-PID controls (answered by `rmctl.rs` as before), ACPI methods,
   IMEX subscription, cgroup limits.
4. A workload asks for it: observed across the rig's 106 runs (all 156
   controls and 33 classes); every user-callable control of an object the
   guest itself made and RM confines to it (channel, group, context share,
   graphics context, memory, VA space, mapper, semaphore surface, context
   DMA), less MAKE_REALTIME and RESTART_RUNLIST, which reach other clients'
   work; every user-allocatable class of the kinds observed that some GPU the
   release drives has (so every Turing-to-Blackwell channel, usermode, 3D,
   compute, copy, NVDEC, NVENC, NVJPG and OFA class); and named controls and
   classes for NVENC, NVDEC, Vulkan Video and compute paths, read from sources
   before the application pass ran them (below).

| release | controls allowed / exported | classes allowed / defined |
|---|---|---|
| 535.129.03 | 211 / 1,132 | 67 / 145 |
| 580.178.04 | 215 / 1,357 | 93 / 209 |
| 595.71.05, 595.99.02 | 215 / 1,349 | 93 / 209 |
| 610.57.04 | 215 / 1,370 | 96 / 222 |
| 615.71.09 | 217 / 1,389 | 96 / 224 |

Each release also allows the 30 observed controls RM passes to GSP-RM with no
CPU-side table (below). Of the ~750 controls RM would serve an unprivileged
caller in 610.57.04, the list keeps 215: `NV2080_CTRL_CMD_GPU_SET_POWER`,
`GPU_EXEC_REG_OPS`, `PERF_RATED_TDP_SET_CONTROL` and several hundred more that
any host user process may call are refused. A host between two releases uses
the older list; RM's exact parameter size is held only on a release measured
exactly.

**Where it is enforced.** Every v1 RM escape that names a control or class,
before any other RM gate: RM_CONTROL (and the control a DEFERRED_API bundles,
which must also be one RM defers), RM_ALLOC, ALLOC_MEMORY, ALLOC_OBJECT,
ALLOC_CONTEXT_DMA2, VID_HEAP_CONTROL by function (a function is allowed when
every class it allocates is), and the page-list registrations. The controls
the backend answers itself (`rmctl.rs`) keep their answers. IOCTL2 carries no
RM call, and `--permissive-abi` does not change the gate. `--rm-allowlist=log`
logs what it would refuse and forwards it: for bring-up, not for running.

**Found on the way.** Two ways into GSP-RM skip CPU-RM's tables entirely: a
control with bit 15 set (the "GSS legacy" range, `RmGssLegacyRpcCmd`) is sent
to GSP-RM with any size, checked only against the PRIVILEGED mask 0xC000; and
every control on an NV2081_BINAPI object, which any user may allocate, is
sent on as it is (`binapiControl`). Neither has a name or size in the open
sources. Only the 28 GSS legacy and 2 BINAPI numbers observed are allowed (six of them since the application pass, below, each held to its measured size).

**Added from the application pass (2026-09-26).** The pass on the live
desktop (`rig/TESTING-RIG.md`, "Application pass") ran NVENC, NVDEC, Vulkan
Video and CUDA-runtime programs on the hardware for the first time. Every
refusal it met is listed here with what was done; six GSS legacy controls
were added to `observed.txt`, each held to its measured size:

| control | what (measured natively, LD_PRELOAD ioctl logger) | refused, what broke | decision |
|---|---|---|---|
| `0x20809001` (CLK legacy) | 8 bytes, zeros in, a clock-domain mask out | every CUDA runtime program: `cudaGetDeviceCount` "initialization error" | allowed, size 8 |
| `0x2080a026` (PERF legacy) | 532 bytes, a request in, the GPU and memory clocks (kHz) out | as above (cudart initialisation) | allowed, size 532 |
| `0x2080a084` (PERF legacy) | 4 bytes, zeros in and out | as above | allowed, size 4 |
| `0x20808165` (GPU legacy) | 1 byte, zero in and out, at NVENC encoder open | NVENC through Vulkan Video and through CUDA: "AuthorizeEncoderSession: Failed to authorize this encoder instance" | allowed, size 1 |
| `0x20808163` / `0x20808164` (GPU legacy) | 4 bytes, zeros in and out, at encoder open / close | as above | allowed, size 4 |
| `0x2080a028` (PERF legacy) | 2192 bytes, mostly the caller's uninitialised stack in, one clock out | nothing: NVENC and NVDEC work without it | **left refused** |
| `NV0000_CTRL_CMD_GPU_ATTACH_IDS` at 128 bytes | libnvcuvid/libnvidia-encode's first try; RM itself answers `NV_ERR_INVALID_PARAM_STRUCT` natively and the caller retries with 132 | nothing: the backend answers exactly what RM does | unchanged |
| `AMPERE_SMC_CONFIG_SESSION`, `AMPERE_SMC_MONITOR_SESSION` (MIG) | nvidia-smi's MIG queries | nothing (no MIG on a GeForce) | unchanged |

Why these meet the policy: all six are in NVIDIA's own
`NV2080_CTRL_*_LEGACY_NON_PRIVILEGED` ranges (`ctrl2080base.h`: 0x81 GPU,
0x90 CLK, 0xa0 PERF), below the privileged mask RM checks (rule 2); their
blocks carry no pointer, descriptor or process id -- three concurrent
encoder sessions all sent and got 0, so the NVENC value is no GPU-wide id one
client could name for another's session (rule 3); and real workloads need
them: every CUDA-runtime program (most CUDA software; Cycles only worked
because it uses the driver API) and every NVENC session (rule 4). What they
read is clock telemetry any host user reads (nvidia-smi shows the same).
RM's CPU side forwards a GSS legacy block of any size to GSP-RM
(`RmGssLegacyRpcCmd`), so unlike the 24 GSS entries before them, these six
carry the size measured on 595.99.02 (`GSS_LEGACY_SIZES` in
`gen/rmallow_extract.py`) and the backend refuses any other size there; on
other releases they have none, as the rest. Counts: the GSS pass-through numbers every
release allows go from 24 to 30 (the backend's log line for 595.99.02: 245
of 1,349 controls, 239 before); classes unchanged. No seccomp exit
(status 159) and no other backend refusal occurred in the pass.

Found alongside, not an allowlist matter: without `--allow-compute`, NVIDIA's
Vulkan driver still lists `VK_KHR_acceleration_structure`, `VK_KHR_ray_query`,
`VK_KHR_ray_tracing_pipeline`, `VK_NV_ray_tracing`, `VK_NV_optical_flow`,
`VK_NV_cuda_kernel_launch` and `VK_NVX_binary_import`, but cannot create a
device with any of them (`VK_ERROR_INITIALIZATION_FAILED`: they run on its
CUDA stack, which needs `/dev/nvidia-uvm`). Natively the same with the node
hidden. An app that enables every ray tracing extension it is offered (Godot's
Forward+ renderer; likely vkd3d-proton's DXR) fails in a graphics-only guest;
with `--allow-compute` it runs. Hiding those extensions in the guest (a
Vulkan layer) would make such apps fall back instead; not done.

**What it does not do.** It keys on the command, not on the object it is sent
to: an allowed control sent to an NV2081_BINAPI handle still goes to GSP-RM
without CPU-RM's size check (the backend holds RM's size itself on a release
measured exactly). It does not look inside parameters beyond the fields
above. It has run enforcing through the rig's regression and the application
pass, which covered Vulkan, GL, EGL, CUDA, the browsers, NVENC, NVDEC, Vulkan
Video, VA-API and OpenCL; a workload outside those may still meet a missing
entry, which shows up as a failed workload with a warning of the form

    RM control NV2080_CTRL_CMD_… (0x2080…) refused: not in the allowlist of host release 610.57.04

(or `RM class … refused`, `VID_HEAP_CONTROL function …`), rate-limited per
call site, and a teardown summary of every refusal by name.

---

## 13. Fuzzing and Miri

Every place the backend parses what a guest or a Wayland peer sent has a
fuzz target (`fuzz/`, `scripts/fuzz.sh`, `device/README.md` "Fuzzing"): the
v1 dispatcher and protocol v2 (HELLO, IOCTL2, HOST_OP, WATCH, MMAP and
MUNMAP) as whole message sequences, the control queue as guest memory,
deep segments, OS-descriptor page lists, RM share parsing and ownership,
NVKMS policy, the pointer scrub, and the Wayland engine both ways. The
host they run against is a fake kernel that follows every pointer the real
driver would, as far as it would copy, and fails on a guest value or an
unmapped address there; the window and aperture are a fake VMM that fails
on an overlap, a placement outside them or of a private descriptor. Miri
runs over the unit tests it can model (`scripts/fuzz.sh miri`).

| id | sev | finding | fix |
|---|---|---|---|
| Z1 | critical | a nested parameter block sent shorter than the size field the host copies by (RM_CONTROL's paramsSize) had RM read a pointer field cut short as the guest's low bytes over the guarded buffer's zeroed slack, past the pointer scrub, which read only the bytes sent: a guest-chosen address in the backend that RM reads and writes through (FIFO_GET_CHANNELLIST and every control with a pointer). Default configuration, graphics included | `20434fa`: the size the host copies must equal the block sent, or be zero, on every nested path |
| Z2 | low | v1 host calls were handed a pointer taken from a slice of the guest's length, while the driver copies `_IOC_SIZE`; the nested block's address was taken before the writes that relocate its pointers. Undefined behaviour under Stacked Borrows (Miri), not known to miscompile | `5e62911` |
| Z3 | low | `serve` could answer a malformed request with more bytes than the capacity posted; the vhost-user transport never posts that little | `0b320bb` |

Nothing of it has run on the GPU; the fuzzers run with no device at all.

---

## 14. Memory safety: `unsafe` in one module, host blocks built

Branch `dind`. A change of structure, not of behaviour: every test that
passed passes (717 with the new ones, 4 skipped), the guest module is untouched, and the fuzz
targets run against a stricter fake host.

**Where `unsafe` is.** Before, 376 uses of the keyword in 37 files: 333 in
`device` (the dispatcher, the window, OS descriptors, the sandbox, the pump,
the fakes of most test modules), 31 in `wlwire`, 12 in the guest daemon.
Now 172 in 10 files, 15 of them test- or fuzz-only:

| module | what |
|---|---|
| `device/src/sys/block.rs` | the arena every host call's parameter blocks are built in (below) |
| `device/src/sys/ioctl.rs` | the one `ioctl` with an argument, which takes only an argument an arena built; UDMABUF_CREATE; a no-argument `ioctl` |
| `device/src/sys/guarded.rs` | the guarded buffers the host writes into |
| `device/src/sys/mem.rs` | every mapping, each owned by a type that unmaps it once; every `MAP_FIXED` checked to land inside a range that type owns (the window, an OS-descriptor reservation); `HostSpan`, the only memory outside an arena a pointer field can name |
| `device/src/sys/fd.rs`, `net.rs`, `proc.rs` | descriptors (returned as `OwnedFd`), netlink, identity, limits, Landlock, seccomp |
| `device/src/sys/pod.rs` | wire structs as bytes, for the types that are all integers and no padding (checked field by field) |
| `wlwire/src/sys.rs`, `nvgpu-wl-guest/src/sys.rs` | the Wayland proxy's and the guest daemon's system calls |

Every crate root is `#![deny(unsafe_code)]` and `#![deny(unsafe_op_in_unsafe_fn)]`,
with only `mod sys` allowed; every other file is `#![forbid(unsafe_code)]`.
`scripts/check-unsafe.sh` fails a tree in which `unsafe`, a raw address
(`as_ptr`, a `*const`/`*mut` cast or type, `transmute`, `from_raw_parts`), a
descriptor claimed by number (`from_raw_fd`, `borrow_raw`) or an
`allow(unsafe_code)` appears outside `sys`, a file has lost its attribute,
or an `unsafe` inside `sys` has no `SAFETY:` comment.

**Built, not patched.** Every `ioctl` the backend makes -- a guest's v1
call, an IOCTL2, and its own (RM frees, UVM queries, PRIME, syncobjs,
sync_files, leases, semaphore-surface probes) -- goes through an
`Arena` (`sys/block.rs`). The guest's bytes are read once, from the request
already copied out of the ring, and never edited; the host's copy is a block
of the arena, the guest's bytes as data with every field the backend knows
to be more than data *declared* first: a pointer, a descriptor the host
resolves or creates, a value the backend decides. Declaring takes the
guest's value out (for the reply) and leaves 0 (-1 for a descriptor out);
after that the field holds only what the arena puts there:

- a pointer: 0, another block of the same arena, or a `HostSpan` the arena
  holds for the call (guest RAM as the transport mapped it, or an
  OS-descriptor reservation). A block cannot point at itself, a data write
  (`Arena::write`, and the `DataMut` view policy code rewrites through)
  cannot touch a declared field, and no code outside `sys` takes a
  pointer's address (the only addresses it sees are the numbers `sys`
  reports for its own mappings, for comparisons and logs);
- a descriptor: one of the handle table's, by `BorrowedFd`, never the
  guest's number; a descriptor out is claimed only once, only after a call
  that succeeded, and only from a field that held -1;
- the reply is a copy, with the caller's value back in every declared field
  that restores it and in every field the arena pointed: no address of the
  backend's leaves.

Declared on each path: RM escapes (`guestptr::rm_escape`, a plan instead of
edits: pRightsRequested, HW_ALLOC's two pointers, IDLE_CHANNELS' arrays; the
OUT addresses of ALLOC_MEMORY, MAP_MEMORY and VID_HEAP_CONTROL); RM_CONTROL's
and RM_ALLOC's parameter pointer, the single deep pointer, each deep segment
(`deepseg.rs`, sized from the host's copy) and every other pointer of a
control (`scrub_control`); the descriptors of the six OS_UNIX controls,
NV0005 events, OS events, REGISTER_FD, ALLOC_OS_EVENT, ALLOC_MEMORY,
MAP_MEMORY, NVKMS's and nvidia-drm's memFd and UVM's descriptor fields;
the address an OS descriptor pins (`osdesc.rs`); UVM_INITIALIZE's forced
flags; UNMAP_MEMORY's and UPDATE_DEVICE_MAPPING_INFO's keys; and in IOCTL2
every schema pointer, descriptor and GEM field, declared as the walk meets
it (before, the guest's pointer bytes sat in the host's copy until
`aim_pointers` overwrote them).

The fuzz targets' fake host now follows a pointer only into the call's own
blocks (`Arg::reach`), which caught one regression in the first commit of
this work (`f4cd317`): a single deep pointer the guest placed across a pointer RM
follows (FIFO_GET_CHANNELLIST's at 8 and 16, the deep one at 12) had the
scrub skip the overlapped field, so RM would have read four bytes of the
guest's and four of an address of ours as one pointer. The old in-place
scrub zeroed it by the order of its writes. Such a call is now refused
(`scrub_control`, EINVAL), with a unit test.

What went with it: a lifetime bug class (a relocated buffer dropped before
the call -- `_idle_segs` was held alive by name), an address leaking back
in a reply whose restore was skipped, a guest descriptor number reaching a
descriptor field, and an OwnedFd made of a number the kernel did not write.

**What it does not do.** Which fields are pointers is still the tables' word
(`guestptr.rs`, `abi::rmctrl`, `schema.rs`, measured from the drivers'
sources): a pointer field they miss reaches the host as the guest's bytes,
exactly as before. Data rewrites are still edits of the host's copy -- the
coherency attributes (`rmmem.rs`, on a copy before it is built), NVKMS
policy (`nvkms.rs`), fence waits (`fence::before`) -- only unable to reach a
declared field. (SYS_PARAMS' and CHECK_VERSION_STR's rewrites are gone, §17.)
RM's top-level blocks are field offsets (`Plan`), not typed structs. Both
are roadmap item 9. The raw-descriptor helpers (`read_raw`, `fstat`, ...)
take numbers: a wrong one is EBADF or another of the backend's own files,
never memory outside the buffers passed. The guest module, which is C, is
unchanged; §14 is the host's.

---

## 15. Memory passing: the review of `dind`

An adversarial review of `dind` (at `69c3abc`) for how memory moves between
the guest, the backend, the VMM, the host kernel and other VMs, against the
product's requirement: no app less protected from another, no VM less
protected from another, than natively. Read from source; the fixes are
unit-tested and the guest module builds clean; at the time none of it had
run on a GPU (it has since: `rig/TESTING-RIG.md`).

| id | sev | lens | finding | status |
|---|---|---|---|---|
| M1 | high | app vs app | A second MMAP of a file the backend had placed without a record (the control file's ALLOC_MEMORY mappings, and every DRM object) got the first placement back whatever size it asked for, and the guest driver mapped the size it asked for from the placement's offset: an app could map past its own extent into the window's next ones -- other apps' device memory -- or into unplaced window, which stops the VM on the first touch | fixed: the backend refuses a request larger than the placement (`map_unrecorded`), and the guest driver refuses a vma, or a GEM object, larger than the placement the reply names (`nvgpu_mmap`, `nvgpu_gem_place_in_window`) |
| M2 | high | app vs app (`--allow-compute`) | UVM's REGISTER_GPU_VASPACE, REGISTER_CHANNEL, MAP_EXTERNAL_ALLOCATION and ALLOC_DEVICE_P2P name an RM client and object that UVM duplicates from a kernel client of its own; RM's check there is that the source client's process is the caller's (cliresShareCallback, PID policy), which every client of the VM passes, since all are the backend's. Any guest process could map another's GPU memory into its own UVM VA space, or register another's VA space or channel. Not cross-VM: another VM's clients are another process's | fixed: the client must be one this VM allocated on the control file `rmCtrlFd` names (what UVM's "Bug 1624521" TODO describes), so the caller holds the file the client was made on; a zero client passes |

What the review found holding, for the questions it was asked:

- **Guest RAM** is read once, from the chain copied out of the ring before
  anything is parsed (`vring.rs`); nothing parses guest RAM in place.
  Replies go only into the writable descriptors the guest posted.
- **Host blocks** are guarded mappings of their own with a readable slack
  page and a guard page, so a host write past a block faults (EFAULT) rather
  than landing on other backend memory; every pointer the tables name holds
  0 or an address the arena owns. What the tables miss still reaches the
  host as the guest's bytes (§14): the fuzzers' fake host follows the same
  tables, so it tests the arena, not the tables.
- **The window** holds only descriptors of this VM's handle table, placed
  by the VMM inside its reservation; withdrawn ranges become PROT_NONE
  anonymous memory in the VMM, whose touch stops only this VM. A guest vma
  keeps its placement until its last vma (splits, forks, mremap) closes.
- **Registered memory and the UVM aperture** reach only this VM's guest RAM
  and this VM's UVM pools in the VMM's own address space; a holder missed in
  the release list (§9, item 11) exposes guest pages, never the host's.
- **Across VMs**, each VM has its own backend and VMM process; the host
  kernel's shared namespaces (RM clients, framebuffer ids, GEM names) are
  held to the VM by RM's per-file client validation (§11, R3), the
  framebuffer rule (S-6), refusing FLINK and GEM_OPEN, and the PID policy
  above. A uid per VM (§4) keeps one VM's processes from another's by the
  kernel's own rules, but it does nothing for a bug in the host kernel or
  the NVIDIA driver, which every uid reaches.

## 16. The VMMs: nesbox and crosvm

The VMM maps all of guest RAM and the window, and it executes the backend's
mapping requests (vhost-user `SHMEM_MAP`/`SHMEM_UNMAP`). A compromised backend
talks to it; the guest reaches it through the device's PCI function.

**A failed placement must not leave a hole.** Both VMMs placed a backend
descriptor into the window with one `mmap(MAP_FIXED)`, which drops the
`PROT_NONE` reservation before the descriptor's own mmap runs. A descriptor
that refuses (a pipe, a bad offset, a writable request on a read-only file)
left the range unmapped inside the KVM memory slot; a later mmap of the
VMM's own (heap, a stack) could land there and the guest would read and
write it. Fixed in both: the descriptor is mapped where the kernel chooses,
then moved over the reservation with `mremap(MREMAP_FIXED)`, and the
reservation is put back if the move fails. nesbox: `virtio-devices/src/
nvgpu.rs` `WindowMapper::place` (branch `virtio-nvgpu-v2`, d633165); crosvm:
`patches/crosvm/0005` (`base/src/sys/linux/mmap.rs`). Each has a test that
forces the failure and checks `/proc/self/maps`. nesbox's UVM aperture was
already safe (`MAP_FIXED_NOREPLACE`, a slot only on success). The move can
still fail after the kernel has dropped the target; the reservation is then
put back with `MAP_FIXED_NOREPLACE`, never `MAP_FIXED` (another thread's
mmap may have landed in the hole meanwhile), and if that cannot be done the
VMM aborts rather than leave something unknown behind the slot (§17). A
hugetlbfs descriptor never gets this far: its mapping is a whole huge page,
and moving it in would replace up to a gigabyte past the checked range --
nesbox takes only NVIDIA and DRM character devices into the window (below),
crosvm's arena refuses hugetlbfs.

**nesbox** (`virtio-devices/src/nvgpu.rs`, branch `virtio-nvgpu-v3`): a
window placement must be an NVIDIA device (character major 195) or a DRM
node (226), opened read-write if the mapping is writable -- the only files
the backend places there -- page-aligned (its length taken as whole pages),
inside the window, clear of every live placement, and one of at most
16,384 (each is a mapping of the VMM's, and `vm.max_map_count` is the
process's); a withdrawal must name a live placement exactly. When the
backend's request channel closes, every window placement is withdrawn as
well as every aperture one: each holds a host device file, and the GPU
memory it maps, in reach of the guest. A UVM pool must come from
`/dev/nvidia-uvm` itself (below, as crosvm). Guest RAM's memfd is sealed
against shrinking, growing and further seals once sized: the backend and
virtiofsd hold it, and a truncation would make every access past the new
end a SIGBUS in the VMM. The device config comes from the backend only (the
copy nesbox used to build from `/proc/driver/nvidia`, with a descriptor
table fixed to one release, is gone), the mapper is bound only once the
window is reserved, and the cgroup descriptors a config names are checked
(open for writing, on cgroup2) and made close-on-exec, so virtiofsd no
longer inherits them.

**crosvm** (`patches/crosvm/`): every request is bounds-checked against the
region the backend reported (overflow-checked, page-aligned offsets,
overlaps refused, unmaps must name a live mapping, reset unmaps all); GPU and
external maps are refused for the nvgpu type; a region size above 64 GiB is
an error, not a panic; a refused request no longer stops the VM. That is the
frontend's own check, and since `0007`-`0009` the main process checks every
request again, trusting nothing the frontend says.

**Where the mapping requests run.** Upstream, a vhost-user *frontend* runs
in crosvm's main process, which crosvm does not seccomp-confine (the
launcher adds a user and network namespace). With `0009` and the sandbox on,
the nvgpu frontend runs in a process of its own, jailed like every device
crosvm emulates -- user, pid, mount and network namespaces, pivoted into an
empty directory -- under a seccomp policy of its own,
`vhost_user_frontend_device`: crosvm's `common_device.policy` (the syscalls
every jailed device has: memory, futexes, epoll, eventfds, pipes,
`sendmsg`/`recvmsg` on the descriptors it holds, the vmm-swap `userfaultfd`
ioctls) plus `getrandom` and `prctl(PR_SET_NAME, PR_SET_PDEATHSIG)`, with
`open`/`openat` refused. No socket, no other ioctl, no KVM call: it maps
nothing into the guest itself. It keeps only its backend socket, the
backend's request channel, its crash tube and its memory tube, and passes
each mapping to the main process as a `VmMemoryRequest`, as a jailed
virtio-gpu does. A test forks the real frontend under that policy against a
fake backend and main process and runs it through config, activation,
window and pool mappings and a reset; another checks the policy stops a
socket and an ioctl. Other vhost-user types keep upstream's in-process
frontend. The KVM calls and the final `mmap` stay in the main process: KVM
refuses VM ioctls from any process but the VM's creator, and a memory slot
points into that process's memory. The child can still `mmap` the
descriptors the backend hands it into its own memory (crosvm's device
policies allow shared file mappings, which the transport needs); that
reaches the same driver `mmap` handlers the main process calls anyway.
`NVGPU_CROSVM_SANDBOX=off` (`--disable-sandbox`) puts the frontend back in
the main process; the main process's checks below hold either way. Against
nesbox: the code that parses the backend's requests is now more confined
under crosvm (a jailed process with a narrow policy, against nesbox's one
baseline filter on every thread), while the process that makes the KVM
calls and the final mappings -- the main process in both -- has no seccomp
filter under crosvm and nesbox's baseline one under nesbox.

**What the main process checks** (`vm_control::sys::linux::nvgpu`, `0007`).
The nvgpu device's two memory tubes are held to what the device needs,
against the BAR layout the PCI transport reported from the main process
before the device process was made; a tube whose layout never arrived gets
nothing.

- *Its ioevent tube* registers and unregisters ioevents, nothing else, and
  only at the device's own queue notification addresses in its settings BAR
  (as the transport laid it out, reported with the shared memory layout),
  any length, each registered once and unregistered only where it is. Before
  the 2026-09-26 review (§17) the address was not held: a compromised device
  process could have taken another device's doorbell, or any MMIO address,
  in the same VM. Upstream lets any device's tube do that and much more --
  register memory anywhere, balloon.
- *Its shared memory tube* may prepare region 1, the window -- the window
  alone, not the whole BAR, so the aperture's slots never overlap it -- and
  map into it only: a descriptor (no other source), at its own BAR's
  allocation (no guest physical address, no other device's BAR), coherent,
  page-aligned, non-empty, inside the window, overflow-checked, clear of
  its other live mappings, at most 16,384 at once; from a character device
  of major 195 (`/dev/nvidia*`) or 226 (DRM) -- the only descriptors the
  backend places there (`nvidia.rs`: RM_MAP_MEMORY's `/dev/nvidiaN`, and an
  mmap of an NVIDIA device or DRM node, `handle_mmap`) -- and writable only
  if the descriptor was opened read-write. It may unmap only what it mapped.
  Ballooning, `MmapAndRegisterMemory`, ioevents and external mappings are
  refused.
- *UVM pools* (region 2, compute only): the host address page-aligned and
  in [4 GiB, 32 TiB), the file offset equal to it (UVM's own rule), the
  length non-zero, whole pages and at most 64 MiB, the BAR offset inside
  region 2 as the main process laid it out and clear of every live pool (by
  offset and by host address), at most 64 pools and 256 MiB per device, and
  the descriptor `/dev/nvidia-uvm` itself -- a character device of
  nvidia-uvm's major (from `/proc/devices`; with no major known, nothing
  is) and minor 0, not `nvidia-uvm-tools` -- opened read-write, of a file
  whose `mincore` the kernel answers for this process (its owner, or
  writable by it: otherwise it reports every page present, and the check
  below would prove nothing). The pool is mapped over the
  band's reservation (below), `mincore` must show every page present or it
  is unmapped and refused, and only then is its KVM memory slot added.
  Withdrawal must name a live pool exactly: the slot goes first, then the
  mapping. When the device's tube goes away (its process ended), every pool
  and window mapping it made is withdrawn by the main process; when the
  backend's request channel closes or the backend hangs up, the frontend
  withdraws everything the backend mapped itself.
- Every refusal fails the backend's `SHMEM_MAP`, not the VM, and is logged
  (the first eight per tube, then every 1024th).

**The band (F3).** With an nvgpu device, crosvm reserves [4 GiB, 32 TiB)
`PROT_NONE`, `MAP_NORESERVE`, at the start of `run_config`, before guest RAM
or anything else is mapped: one VMA, not charged to overcommit (no
`VM_WRITE`), 28 TiB of address space against the 128 TiB a 47-bit process
has (an `RLIMIT_AS` would have to allow it). If it cannot be reserved, the
main process logs so and maps pools with `MAP_FIXED_NOREPLACE`, as nesbox
does. UVM will not take the window's atomic placement (map anywhere, then
`mremap` into place): it maps a pool only where the address equals the file
offset at `mmap` time, and a semaphore pool's `vm_open` disables a moved
vma (`uvm.c`). So a pool is mapped with `MAP_FIXED` over the reservation --
only over a range the band records as reservation: never a live pool (the
band tracks every one, across devices) and never a range a failure left
unknown. UVM expects `MAP_FIXED` (it disables rather than fails a vma when
it cannot take its power-management lock, "to safely handle MAP_FIXED"),
and `mincore` catches that case. If the file's `mmap` fails the kernel may
leave the range unmapped; the main process fills it again at once with
`MAP_FIXED_NOREPLACE`, and if that cannot be done -- the range is not a
hole, whether because the kernel kept the reservation or because another
thread mapped something there -- it never places there again: what
`/proc/self/maps` shows cannot tell a reservation from another thread's
`PROT_NONE` mapping. Past 256 such ranges the band takes no more pools. On
withdrawal the reservation is put back over the pool in the same
`mmap`. The hole is never guest-visible: the slot is added only after
success. UVM's mappings are `VM_DONTCOPY`, so a device process forked later
gets none of them.

**Compute** runs under both VMMs. crosvm publishes region 2 after the
window in the same 64-bit prefetchable BAR, each region with its own
shared-memory capability, when the backend reports it (only with
`--allow-compute`); the guest driver finds regions by id. The launcher
allows `--allow-compute` with `--vmm crosvm` only when the binary's
`run --help` names the `nvgpu-uvm-aperture`. The crosvm side has unit tests
for every check above and the seccomp test, and has run on the RTX 5090: the
render probe's CUDA and the security negatives with `--allow-compute`, with
the frontend jailed (rig/TESTING-RIG.md, "crosvm"). The frontend takes the backend's regions by id,
however many it reports: a lone region that is not region 1 is no window.

**Hot-plug.** A virtio device's control tube -- its PCI transport's,
whichever process that runs in -- takes power management events only;
`HotPlugVfioCommand` from it is refused (`AnyControlTube::DeviceNoHotplug`,
`0009`), so a compromised device process cannot have the main process add a
host VFIO device to the VM. Hot-plug ports keep theirs. (`run-guest.sh`
passes `--no-pci-hotplug-port`, so there is no port to plug into either.)

**The GPU's PCI address.** The guest puts its fake PCI device at the host
GPU's address, because NVIDIA's userspace matches what RM reports against
sysfs and procfs; rewriting that address in every reply that carries it
would be new rewriting surface, so it is not done. A VMM with its own device
there collides: the guest driver now fails cleanly and names the bridge in
the way, and `run-guest.sh` refuses a crosvm whose buses would hold the
address.


## 17. The 2026-09-26 review

### Fail closed, and production hardening

**Unmeasured driver releases.** A host release above the newest one
measured silently ran on the newest release's tables (RM allowlist, ABI
profile, NVKMS schema, and a UVM table open-ended at 999.999.999), and one
below the oldest on the oldest's allowlist with no ABI profile, forwarding
RM escapes unchecked. 580 is the precedent for why that matters: it added
pointers to two controls the older list let through. Now the backend refuses
to start unless every table was measured at the host's release
(`device/src/release.rs`): its own RM allowlist and NVKMS schema, a UVM
table whose range holds it (the last range now ends at the newest release
measured, in both the backend's and the guest's copy), and an ABI profile no
newer than `MEASURED_THROUGH` (`gen/src/versions/mod.rs`; 615.71.09, on
gVisor's lineage, the A2000 capture, and every measured release's RM escape
blocks). A version that does not parse is fatal. The tables chosen are named
on one warning line at every start. `--allow-unmeasured-release` (a
diagnostic flag) runs a newer or in-between host on the nearest older
tables, without compute (no UVM table on either side); a host older than
every release is refused regardless, and inside the backend it gets an
empty RM allowlist and every RM escape refused. The rig's 595.99.02 is
measured by all four.

**Start-up fails closed.** Three conditions the backend used to log and run
through now stop it. A sandbox layer the kernel lacks, or has only in part
(`sandbox: DEGRADED`), with `--sandbox=on`, the default; the new
`--sandbox=best-effort` runs with what the kernel has, and it and
`--sandbox=off` are diagnostic flags. An error asking the host's RM whether it
keeps clients to their file (`probe_strict_clients`; R3): only an answer of
yes lets it start. And the seccomp filter is installed with
`SECCOMP_FILTER_FLAG_TSYNC`, while `sandbox::apply` installs nothing at all
when the process already has a second thread (or its thread count cannot be
read): Landlock and the user namespace reach only the calling thread, so a
thread made before them would have been outside both. Tests: a two-thread
child gets no layer; a thread made before the filter is stopped by it.

**The release profile.** There was none: a panic unwound one thread, so a
vring worker's panic stalled its queue and the VM with it, a lock held
across it was poisoned for the rest, and an executor job left its file
mid-call. The workspace's `[profile.release]` is now `panic = "abort"`,
`overflow-checks = true`, thin LTO, one codegen unit, line tables only.
Nothing outside tests catches an unwind. What `abort()` does -- block
signals, `tgkill` its own thread with SIGABRT, reset a handler that caught
it -- is on the seccomp list, and a test panics a filtered child the way a
release build does (from the main thread and a worker) and sees SIGABRT, not
the 159 of a violation. The fuzz workspaces have profiles of their own and
are unchanged; the difftest runs under the new profile (`cargo test
--release`).

**Diagnostic flags.** `--allow-root-unsafe`, `--proc-nvidia`,
`--permissive-abi`, `--keep-guest-coherency`, `--rm-allowlist=log`,
`--sandbox=best-effort|off` and `--allow-unmeasured-release` each take a
protection away. They are hidden from `--help` (shown with `--diagnostic
--help`), the backend refuses to start with any of them unless
`--diagnostic` or `NVGPU_DIAGNOSTIC=1` is given too, and each in effect is
announced as `DIAGNOSTIC: <flag>: <what it takes away>` on stderr, whatever
the log level, and in the log. `rig/run-guest.sh` adds `--diagnostic`
only when one of them reached the backend's arguments (after `--`, or from
`NVGPU_SANDBOX=off` or `NVGPU_ALLOW_ROOT_UNSAFE=1`).

**What the log holds.** The raw dumps are gone: the CARD_INFO reply (BAR
physical addresses), SYS_PARAMS, RM_CONTROL and RM_ALLOC replies (whose
`&param_buf[4..]` also panicked on a block shorter than 4), MAP/UNMAP_DMA and
VID_HEAP_CONTROL replies, all at info, and the 64 parameter bytes of every
control RM refused, at warning (host addresses, and a guest's data, in the
host's log). A refused control is now its command and status, at debug.
The per-call info lines a guest can drive (UVM and window placements,
read-only mappings, OPEN_KMS, SHM restores) are debug; the RM allowlist's
teardown report of what it refused is a warning. The default level is
`warn`, so the start-up lines worth reading (the tables chosen, the sandbox
layers not in force, the diagnostic flags) are what a production log holds.
The rate limit (`device/src/ratelimit.rs`) already said how many lines a
site dropped when its next line went out; a site that went quiet after its
burst now says so at teardown too.

**SYS_PARAMS and CHECK_VERSION_STR go as sent.** `NV_ESC_SYS_PARAMS` is
`{NvU64 memblock_size}`: nvidia.ko keeps the first caller's value (on the
control device, so host-wide) and answers EBUSY to any other
(`kernel-open/nvidia/nv.c`). On EBUSY the backend wrote 2 into the value's
low byte, called again, and on a second EBUSY answered the guest success
with zeroed parameters; now the host's answer, EBUSY included, is the
guest's. What remains: a guest whose SYS_PARAMS is the first on the host
after the driver loads sets that host-wide value. RM uses it only to online
GPU memory as NUMA on coherent platforms, which this project does not
support; on a PCIe GPU it changes nothing.

`NV_ESC_CHECK_VERSION_STR` (`nv_ioctl_rm_api_version_t {cmd, reply,
versionString[64]}`) had its command rewritten to `'2'`, query mode, "based
on gVisor nvproxy". In query mode RM copies out its own version and returns
success without comparing (`RmPerformVersionCheck`, `osapi.c`); in the
strict (`0`) and relaxed (`'1'`) modes userspace sends, it fails a caller
whose version is not its own. The rewrite was not needed -- gVisor queries
the host's version once for itself, and the backend reads it from
`/proc/driver/nvidia/version` -- and what it did was let a guest userspace
of another release run against this RM, with its structures sized for the
other release. Userspace must match the host's module, as natively
(`nvgpu-userspace` stages the host's own); a mismatch now fails in the
guest as RM's API-mismatch error, with RM's usual `NVRM: API mismatch`
line (the backend's process name) in the host's kernel log. Test:
`sys_params_and_check_version_go_as_sent_and_come_back_as_answered`.

**Test binaries.** `test-harness` served a backend on a socket with no
sandbox and no posture checks, removing whatever was at its socket path
first (default `/tmp/nv-vhost.sock`); it was built by every `cargo build
-p device`. It is now built only with `--features test-bins` (which also
brings in tokio and tracing, no longer dependencies of the backend), and
clears its path only if it holds this user's socket
(`posture::clear_socket_path`). `test-client`, which carried its own stale
copy of the protocol, is deleted. `nvgpu-userspace --stage DIR` ran
`remove_dir_all(DIR)` on whatever it was given; it now clears only a
directory that is empty or holds the marker it writes into every share it
stages, never through a symlink.

**The generators fail closed.** `rmctrl_extract.py` left out, silently, a
control RM's pointer tables name whose command macro the release's headers
do not define; a control left out is one whose pointer reaches RM as the
guest's bytes. Now any such name stops the extraction unless it is in
`UNDEFINED_IN_HEADERS` with its reason (one: `NV0000_CTRL_CMD_OS_GET_CAPS`,
whose case RM itself compiles only if the macro exists). `nvabi_gen.py`
wrote `param_size: None` -- no size check -- for an escape whose struct it
could not lay out; that, and a fixed-size escape with no struct, now stop
it (only nvproxy's byte-copied escapes are variable length). Nothing ran the
extractors' `check` modes; `scripts/gen-check.sh` runs all of them, the UVM
tag scan (whose comparison had rotted: it no longer ran), the schema render,
and with `GVISOR=` the ABI profiles, against the sources over the network.
On 2026-09-26 every table matched, and gVisor master regenerates the three
profiles unchanged.

**Dead code and lint.** What bypassed a check and nothing called is gone:
`xfer::Prepared::finish` adopted host descriptors without asking whether the
number was already the backend's and without closing consumed handles (the
backend always used `finish_with`). The constructors that make policy state
with no owner -- `semsurf`'s `render_opened`/`client_allocated`,
`BackendHooks::new`/`shared`/`Default` -- are test-only, so production code
cannot build hooks sharing no state with the backend. The RM allowlist's
NVOS21 branch for RM_ALLOC is gone (every profile admits only NVOS64; a
32-byte block is refused as too short), with dead constants, an unused test
helper and orphaned doc comments. `cargo clippy --all-targets` is clean
outside the Wayland files (another branch) and hostfd, nvkms, osdesc,
semsurf, xfer and one site of nvidia.rs's `map_unrecorded`, which a
concurrent isolation branch is rewriting.

### Backend isolation (branch `fix-backend`)

A review of the backend for cross-VM and app-to-app reach and for denial of
service (the review's own notes, not shipped; numbered as there). The
items below are the ones branch `fix-backend` took; the rest -- 8, 15-18 and
the logging and cleanup items -- are the hardening branch's. Each was checked
against the code and, where RM's behaviour decides it, NVIDIA's 610.57.04
sources; each fix has a unit test that fails without it. Each was tested
against fake kernels first; the hardware regression has run over all of them
since (below).

| # | sev | finding | status | commit |
|---|---|---|---|---|
| 1 | high, cross-VM | ALLOC_OS_EVENT and FREE_OS_EVENT reached RM with the guest's hClient unchecked. RM keeps OS events in one host-wide list matched by (hClient, fd), checking neither against the caller (osapi.c allocate_os_event, free_os_event; os.c osUserHandleToKernelPtr), and every backend's descriptor numbers are small: a neighbour's client (handed out in sequence) let a guest free that VM's events or take the key its next one needs | fixed: the client must be one this VM allocated and has not freed (the semsurf client record, `rm_share_gate`), else NV_ERR_INSUFFICIENT_PERMISSIONS without RM. Not "made on the calling file": RM posts an event to the file the call is made on (nv_post_event, `event->nvfp`), the file the caller then polls, which is not the one its client was made on -- that rule would refuse every legitimate event | 9a9d3aa |
| 2 | high, DoS | a fence context's GEM could be PRIME-exported (HOST_OP); the dma-buf kept the context -- a host kthread, a timer, an NVKMS duplicate -- alive after GEM_CLOSE gave its slot back to the caps | fixed: HOST_OP PRIME_EXPORT of a live fence context is refused (EINVAL). Nothing legitimate exports one: nvidia-drm's object has no sg table, Vulkan and EGL use a context only through 0x55-0x57, and the guest driver's own proxy refuses export (`nvgpu_fence_ctx_export`) | d7c41fc |
| 3 | medium, app vs app | RM_FREE and FREE_OS_EVENT wiped the backend's records whatever RM answered: one guest process freeing another's client, which RM refuses, broke the owner's duplicates, fence contexts, OS events and grants | fixed: forgotten on NV_OK, or when the free came through the file that made the client (or the event) | 9a9d3aa |
| 4 | medium, compute | UVM external mappings of registered memory were held whatever UVM answered and wherever they lay, against one VM-wide bound: holds nothing took down (pinned to the session's end), and one process filling the bound refused every other's | fixed: the mapping must lie in an external range the file made and the backend recorded (overflow-checked), or it is refused before UVM; held on NV_OK and on the failures of UVM's page-table wait, which leave mappings up (RC, ECC, GPU lost); a quarter of the bound per guest process; `UvmHold::within` does not wrap. The holds stay a list scanned per call, bounded by the cap | cf2c132 |
| 5 | medium, cross-VM | S-6's framebuffer check and the ioctl were not one step: an RMFB, CLOSEFB or file close on another executor in between freed the id, and the kernel gives the lowest free id to the next framebuffer anyone makes | fixed: each id a call names is in use from its check to the end of its ioctl; RMFB/CLOSEFB wait for it on their own executor (5 s at most: past the check the kernel holds the framebuffer by reference), and a KMS file whose framebuffers are in use is parked and closed after the last such call (close, lease burial, reset). Not one per-VM lock: that would have the queue thread wait on a blocking commit | 3034888 |
| 6 | medium, DoS | the S-8 probe limits kept a record per guest-chosen connector id and dpyId, made before the host saw the call: unbounded | fixed: a GETCONNECTOR probe the host refuses takes its record back; past 256 records the stale ones go, and a new id in a full window is reported, not probed. Past 256 dpy records the ones the host never answered go | 59f098d |
| 7 | low-medium | the one-pointer deep block was pointed at any 8 bytes of a control's or an allocation's parameters: where RM follows no pointer the address reached RM as data (SET_ZBC_COLOR_CLEAR put it in the GPU-wide ZBC table, an address of the backend's for any tenant to read) | fixed: relocated only at an offset `control_pointers(cmd)` names; elsewhere the field keeps the guest's bytes and the block goes back as sent (the guest driver still carries one for V1 GPU_GET_ID_INFO, whose szName RM 610 ignores). Refused (EINVAL) on anything but RM_CONTROL: no class takes one | e15e535 |
| 9 | low | IOCTL2's `after` hooks ran for a call finishing after its file closed or after a session reset: grants re-recorded for a closed file, an old REVOKE forgetting the new session's grants | fixed: `Finisher::records`; the reply is still rewritten, nothing is recorded | a0ee4ee |
| 10 | low | an OPEN_KMS card file was charged to whichever process the queue thread served last | fixed: the owner is taken when the call is served | f41a67b |
| 11 | low | pump instructions were forwarded after the backend lock was dropped, so a CLOSE's Unwatch could overtake an earlier Watch and leave the pump a duplicate of a closed file (a master, a lease) | fixed: the pump's lock is taken before the backend's is let go, at every site | 2643f5d |
| 12 | low, cross-VM | GETPROPBLOB read any blob by id: other VMs' MODE_ID and damage clips, the host desktop's EDIDs | fixed: only blobs a file of this VM made (until destroyed or closed) or saw as the value of a blob property of an object it can see (OBJ_GETPROPERTIES, GETCONNECTOR); ENOENT otherwise. A blob reported and freed since stays readable until asked again: another tenant would have to get that very id meanwhile | 20dbc70 |
| 13 | low | descriptors were classified by their `/proc/self/fd` link text, a path: a same-uid process with a mount namespace of its own could pass a FUSE file at `/dmabuf:x` and stall the reader | fixed: by `fstatfs`'s f_type first (anon_inodefs, the dma-buf fs, shmem or hugetlbfs), then the link; fstatfs added to the seccomp list | d97cfe1 |
| 14 | low, DoS | a display file handed to the closer was refunded to the handle table at once, so a stuck closer let a guest queue host files without bound, to EMFILE for the whole VM | fixed: counted against the table and the owner's share until the closer has closed it | 69e8306 |
| 19 | low | (a) `map_unrecorded` reusing a placement keyed (handle, 0) on an RM file; (b) a freed parent left its children's memory records | (a) not a finding: RM keeps one mapping context per file (nv-usermap.c: a second is NV_ERR_STATE_IN_USE), so every mmap of the file maps the same memory. (b) fixed: each object's parent is kept and a free takes the subtree; a guest freeing devices in a loop could fill the table and leave other processes' memory unrecorded | c9ff8ef |

**On hardware.** What each needed was run on the RTX 5090 in the regression
of the merged tree (nesbox with the C and the Rust module, crosvm with
compute), all green: Vulkan and CUDA (OS events on the event's own file, #1;
fence contexts, #2; frees of clients, #3; CUDA's UVM mappings, #4); a lease
session (the lease and vkdisplay probes read MODE_ID, IN_FORMATS and EDID
blobs, #12; RMFB and page flips, #5; GETCONNECTOR probes, #6); descriptors
from the live host compositor, Wayland shm and dma-buf, classified (#13).

### The VMMs and the launcher

What a review of nesbox, the crosvm series and `run-guest.sh` found, each
confirmed before it was fixed. nesbox: branch `virtio-nvgpu-v3` (e7a6548,
eb39060, c9d4391). crosvm: `patches/crosvm/0001`, `0005`, `0007`-`0009`,
regenerated (a fix goes in the patch that brought the code). §16 says what
each VMM now checks.

**nesbox took any descriptor into the window** (high, given a compromised
backend). A hugetlbfs memfd (`MFD_HUGE_1GB`) is mapped a whole huge page
long, and the `mremap` that moves a placement into the window then replaced
up to a gigabyte past the checked range -- past the window's end, since the
reservation is only 2 MiB-aligned; in the UVM aperture its `munmap` failed
and the mapping stayed. The backend only ever places NVIDIA devices (195)
and DRM nodes (226) in the window (`nvidia.rs`: `NV_ESC_RM_MAP_MEMORY`'s
device file, and `handle_mmap`, which refuses every other handle kind and
sends UVM files to the aperture), so nesbox now takes those alone, opened to
match, with the page-aligned offsets and whole-page lengths crosvm already
required, and a pool only from `/dev/nvidia-uvm` (its major from
`/proc/devices`, read when the device is built). Tests: a memfd, a hugetlb
memfd, `/dev/null` and a pipe are refused before anything is mapped; the
classifier by mode and device number. **Placements were uncapped**
(low-medium): at most 16,384 now, overlaps refused, withdrawals exact;
tested to the cap. **Placements outlived the backend**: withdrawn when its
channel closes (tested). **Guest RAM was not sealed** (low-medium): a
backend or virtiofsd could `ftruncate` it and make the VMM SIGBUS; sealed
against shrink, grow and further seals (tested, through a second
descriptor). **The mapper was bound before the window was reserved**, which
the comment there denied, and **the cgroup descriptors** from the config
were neither checked nor close-on-exec, so virtiofsd inherited them: bound
after, and checked (cgroup2, writable) and made close-on-exec (tested).
Cleanup: the `/proc/driver/nvidia` config fallback and its fixed descriptor
table are gone (the backend always serves `CONFIG`; `gpu-forward.proc-nvidia`
is gone from the config and the jailer); `MemorySlots::unmap` forgets a slot
only after KVM let go of it; nesbox's `docs/SECURITY.md` and §3 here now
agree about the band (an unlimited stack rlimit puts nesbox's mappings in
it). The failed-move refill uses `MAP_FIXED_NOREPLACE` and aborts if it
cannot, as crosvm's does. Not unit-tested: the refill's abort (it needs a
move to fail after the kernel unmapped), and `MemorySlots` (it needs KVM).

**crosvm's ioevent tube took any address** (medium, `0007`/`0008`): held
now to the device's own notification addresses, reported by the transport
with the BAR layout, each once, any-length matches only (tested, and the
report's resolution). **The arena's refill after a failed move** (`0005`,
low) was `MAP_FIXED` and unchecked: `MAP_FIXED_NOREPLACE`, checked, abort if
the hole cannot be filled; hugetlbfs refused before mapping (tests: refill a
hole in a reservation no other thread can map into, refuse to replace a
mapping, refuse a hugetlb memfd). **`mincore` proved nothing without write
access** (low, `0007`): a pool's file must be the process's own or writable
by it, so the kernel answers (tested). **The `/proc/self/fd` fallback took
any character device linked at `/dev/nvidia-uvm`** (low): gone; no major, no
pool. **A virtio device's control tube accepted `HotPlugVfioCommand`**
(low, `0009`): refused on those tubes (`DeviceNoHotplug`; the predicate is
tested). Cleanup: the `/proc/self/maps` heuristic that judged a range still
reserved is test-only; a range that cannot be refilled is given up, and
past 256 of them the band takes no pools (tested); the frontend takes
regions by id however many there are, so a lone aperture is not published
as the window (tested); it withdraws the backend's mappings when the
backend's channel closes or it hangs up, as on reset.

**`run-guest.sh` as root killed other VMs** (medium): `pkill -u
$NVGPU_USER` matched every backend of a user all VMs share in the Wayland
modes, and the VMM pattern every root VMM. As root nothing is killed by
pattern now; a slot is taken only when its users run nothing anyway. **Root
changed a file through the backend user's symlink** (medium): the socket's
directory was the backend user's, so its `nvgpu.sock` could be swapped for
a symlink before root's `chgrp`/`chmod`. The directory is root's (0711) once
the socket exists, a symlink is refused, and `chgrp -h`. **Root ran, read
and wrote a user's files** (low): the launcher itself, the binaries (the
jailer and virtiofsd too), kernel, rootfs, share, jail image and logs
directory, and every directory above them, must be root's and writable by
no one else (a sticky one aside), and are used by their resolved paths.
`rig/verify/launcher-dryrun/run.sh` shows both against the launcher
before this: in a user namespace, as its root, with stub binaries, the old
one killed another VM's backend and VMM and turned a root daemon's 0600
socket 0660; this one kills neither and refuses the symlink. Cleanup:
`NVGPU_VMM_JAIL` defaults to `on`; as root, `NVGPU_SANDBOX=off`,
`--sandbox=off`, `NVGPU_ALLOW_ROOT_UNSAFE=1` and `--allow-root-unsafe` need
`NVGPU_DIAGNOSTIC=1`; `--permissive-abi` and `--rm-allowlist=log` are
announced; the root layout is never assumed (`NVGPU_PREFIX` or
`NVGPU_RIG`), and TESTING.md runs a root-owned copy. Also: the Claude
sandbox's NixOS snippet (`patches/nixos/README.md`) no longer binds
Hyprland's IPC by default (`hyprctl dispatch exec` runs anything on the
host); `envy-capture.sh` makes its output directory with `mktemp -d`;
`rig-preflight.sh` advises `kernel.sysrq=244`, not 1.

**On the GPU:** nesbox `virtio-nvgpu-v3` and the crosvm compute build ran the
probes green on the RTX 5090 -- the window's device check against every
descriptor the backend really places (RM mappings, DRM objects, leases),
compute (the UVM major and the `mincore` access check as the VMM's user),
and crosvm's ioevents at the reported addresses.


### The Wayland proxy

A review of `wlwire`, `nvgpu-wl-guest` and `device/src/wl` against a guest
(and, in export mode, a host client) trying to take more than its share.
Each finding below was confirmed in the code, and each fix has a test that
fails without it. Branch `fix-wayland`.

| # | severity | what | fix |
|---|---|---|---|
| 1 | medium-high | The lease throttle checked only the first submit of a frame: a frame of a thousand submits went through on a full bucket, each a blocking modeset of a desktop monitor on the compositor's thread. The count made ahead of the engine also missed a registry made by `get_registry` in the same frame. | Every submit of a frame must fit the rate; more than the burst in one frame ends the connection. The count follows every `new_id` through the protocol tables, and the engine lets through no more submits than were counted and admitted (`allow_lease_submits`): a miss is fatal, not a bypass. `d08b673` |
| 2 | medium | The guest daemon read the whole buffer at every damaged commit, framed all of a client's input at once, and put no byte cap on a client's buffers: a client with a sparse 2 GiB pool committing in a loop grew the daemon, and every client with it, without bound. `set_icc_file` did the same at 16 MiB a message, and export mode in the backend with a host client's commits (dropping the connection past `max_queue`). | The client's side charges its buffers to `MAX_POOL_BYTES` as the server's side does. Commit copies and blobs are read a record at a time as frames are made (`SyncJob`, `BlobJob`, `take_units_upto`). The engine takes no more of a client's input with 4 MiB queued for the channel (`CHANNEL_HIGH_WATER`); the daemon frames a frame at a time and takes more as the host drains; export mode reads a host client only as the guest's queue has room (`EXPORT_QUEUE`). `2465bd1` |
| 3 | medium | The daemon queued every descriptor a client sent, and the backend's reader every one its peer sent, though only messages that carry one take one. | Past 1024 untaken (libwayland's own ring, `MAX_FDS_QUEUED`) the client, or the backend's connection, is closed. `9b9a274` |
| 4 | medium | Stream sinks held up to 256 KiB each, 256 streams a connection, and unfinished blobs up to 16 x 16 MiB of memfd pages that never expired: 64 MiB a connection outside every per-VM budget, times 64 connections. | A sink grants a share that shrinks as streams are added (`WINDOW` for sixteen, 4 MiB among more, 16 KiB at least), sent in the stream's descriptor to a peer that says `HELLO_STREAM_WINDOW`; what sinks hold is charged to the guest process's share of the VM queue budget, and past it the stream ends with `ENOBUFS`. Unfinished blobs are charged to the VM's and the process's shm budgets, and dropped if the record after them does not take them. An 8 MiB selection still crosses each way in about 220 ms (loopback test, sway). `062d8ea` |
| 5 | low | Error text a peer controls -- a far side's ERROR record, an interface name it bound -- went verbatim into the backend's log, the daemon's stderr and, in export mode, a host client's `wl_display.error`. | Fatal text is escaped (control and bidirectional-formatting characters) and cut to 512 bytes in the engine; the backend logs it quoted, through its per-site rate limit (`ratelimit.rs`); the daemon's client-caused lines are limited to 20 per 10 s with repeats counted, and its error list to 32. `ccce467` |
| 6 | low | Shm pools and a client's blobs were read with `pread` on the thread serving the connection (in export mode, under the lock WL_SEND and WL_RECV take): a file on FUSE stalled it for as long as its server liked. | A pool must be a regular file on tmpfs or hugetlbfs (every memfd is), or the client gets `wl_shm.error.invalid_fd`; a client's blob from anything else is sent as an invalid descriptor. `ac5cec3` |

Cleanups from the same review: the export socket is bound in a private
0700 directory and renamed into place, and a socket at the path is
replaced only if it is ours; its accept loop waits after `EMFILE` instead
of spinning (`bcb8617`). The `ext_image_copy_capture` value rewrite, of a
protocol the allowlist hides, is gone, and a test holds every rewrite to
what the allowlist reaches (`5201973`). §5 now says what lease-file
classification does and does not establish. Clippy is clean on these
crates (`e296fac`).

**Fuzzing.** `wl_engine` now runs export mode as well as normal mode, sends
every frame into the host engine through `lease_submits` and a
`LeaseThrottle` on a clock the input moves, and checks after every
operation that neither engine holds more memory than its budgets allow
(`Engine::held_bytes`), that no more submits reach the compositor than were
admitted, and that the throttle never admits past its burst and rate nor
refuses once its named wait has passed. Five minutes each on 2026-09-26
(`scripts/fuzz.sh run 300`, three workers): `wl_engine` about 10 million
runs, `wl_codec` 174 million, no finding. The memory oracle was checked by
hand against a bound of 16 KiB, which a single blob chunk trips.

**W3's guest-image side: fixed.** `apps.sh` (`nvgpu_user=1`) no longer puts
the app user in `nvgpu-wl`: the apps are in `video` and `render` only, and
the daemon alone is started with `nvgpu-wl` added to its groups (checked in
`rig/guest-image/probes/apps.sh`). The rig runs the daemon as the app's own uid;
a production guest runs it setgid `nvgpu-wl` or as an account of its own
([`DEPLOY.md`](DEPLOY.md), "The guest").

**Still open.** A client may still hold a sink's share
of the queue budget by never reading its pipe, as it may hold a queue by
never reading its socket; both are its own process's share.

---


A review of the guest module, the backend, the Wayland proxy and the VMM
launchers, each finding checked before it was fixed.

### The guest module (`driver/`)

Every finding below was confirmed in the code before the fix; where a parser
has both implementations, both were fixed and `driver/rust/difftest` agrees.
None needed a new host surface; the backend change is the syncobj
registrations' accounting in `device/src/fence.rs`.

**Medium.**

- **OS-descriptor pins held until remove().** A registration abandoned in
  flight (a fatal signal, a timeout) kept its pages pinned under id 0 until
  the device went -- even one that never reached the ring -- so a guest
  process could pin memory without bound. `nvgpu_osdesc_send()` now takes
  the pins with the request: unsent, they are unpinned at once; sent, they
  are kept under the request id, and the transport's reaper reads the late
  reply as the registration would have -- kept under the id it names, or
  unpinned when it names none or was refused. A reply that beats the
  waiter's hand-over is remembered for it; one that never comes (a garbage
  backend) leaves the pins until remove(), as before. Rust:
  `osdesc::Env::send_pinned`; a difftest case for the hand-over.
- **Syncobj wait registrations fill the VM's cap.** A process polling
  never-signalled points, then destroying the syncobjs or closing the file,
  left orphans that held the 1024 slots until they fired -- never -- and
  every other process polled. Now (fence.rs) each registration is charged to
  the guest process that asked and a process holds at most a quarter
  (`quota::Share::quarter`); and a syncobj that never left its render file
  (not exported as a syncobj file, in a file that never imported one) has
  its registrations dropped when a DESTROY of it succeeds or the file
  closes: our syncobj file was its last reference, so the kernel frees its
  entries with it, as for a native process. A syncobj that did get out keeps
  its orphans counted until they fire, charged to their maker -- dropping
  them would let a guest leave uncounted host kernel entries on a syncobj it
  keeps alive elsewhere, the growth the cap exists to stop. **Residual:** a
  process that forks children to make orphans on shared syncobjs and exit
  can still fill the pool (`Share::owners_to_exhaust`, as quota.rs says of
  every pool); the cost is polling latency for the VM's other waits, not
  host memory. Guest side (nvgpu_fence.c): userspace SYNCOBJ_EVENTFD
  subscribers are charged per process (a quarter of 4096) and freed,
  unsignalled, when their syncobj handle is destroyed or their file closed,
  as the kernel frees a dead syncobj's entries.
- **GEM proxy tombstone erased before the host closed the handle.** When a
  proxy's GEM_CLOSE could only be queued, its gem_index entry went at once,
  and a GETFB or PRIME import that got the number back made a new proxy the
  queued close then closed. The close is now always queued with a release
  that erases the entry, wakes waiters and frees the tombstone once the host
  has answered it or it is known never to run.
- **Rust ATOMIC: IN_FENCE_FD dropped.** nvgpu_kms.c's in-fence hook read the
  commit/TEST_ONLY bit from the parse's out struct, which the Rust wrapper
  wrote only after the parse: every real commit's in-fence went to the host
  as -1. kms.c takes the bit from the argument's kernel copy before the
  parse; the core says it before any hook (`atomic::Env::begin`), written
  through a raw pointer, no `&mut` held while C reads; the difftest records
  the bit each hook sees, and fails with `begin` removed.

**Low-medium.** **Use after free on device remove.** The character devices
are embedded in `struct nvgpu_device`, freed with its last reference -- taken
in a file's release, before the VFS's `cdev_put()`; and remove() freed the
transport's state under files still holding the device. The device's count
is a kobject now, every cdev parented to it (`cdev_set_parent()`), so it is
freed after the last `cdev_put()`; the transport's state is freed with the
device and found dead, not gone, until then; the device holds the virtio
device its late log lines name. No SRCU was needed: every late path holds a
device reference and checks `nvgpu_xfer_dead()`.

**Low.**

- A Wayland SEND's dma-bufs and syncobj files ride on its request buffer, so
  one abandoned in flight keeps them until the host is done with it.
- A placement's offset is checked page-aligned and its end without
  overflow; `window_valid` is published and read with release/acquire; a
  dma-buf importer gets no device-writable mapping of a read-only
  placement, and the dma-buf's map and vmap run inside `drm_dev_enter()`.
- IOCTL2 refuses, before sending, a call whose reply could name more
  descriptors than its state holds (C wrote the excess over the GEM
  records, Rust skipped them and leaked their handles).
- The C reads each of the caller's blocks once, as the Rust does:
  RM_CONTROL's nested block (TIME_CORRELATION's TSC refusal, V1V2), the
  OS-descriptor class word, IDLE_CHANNELS' flat fallback, NVKMS's outer
  struct and GET_NEXT_EVENT's reply. The difftest's one recognised
  difference is gone.
- GET_PROC_FILES / GET_SYS_FILES lengths are checked against what is left of
  the stream, and the GPU lookup stays within the 8 config-space slots.
- A descriptor of another nvgpu device is refused (`nvgpu_handle_for_fd()`,
  `nvgpu_hostfile_handle()` take the device).
- An adopted lease file does not stay the guest device's master (it became
  it when none was), and its SET/DROP_MASTER are refused, as a lessee's.
- The KMS object-class cache keeps only CRTCs; -ENOENT (any number a commit
  names) was cached without bound. The caches are memcg-charged.
- DMABUF_IMPORT's results are checked (a non-zero u32 handle, a size below
  64 GiB); a proxy refuses a size `PAGE_ALIGN()` would wrap; a RECV whose
  frame fails its check closes the handles its descriptors carry.
- `nvgpu_fence_unwrap_fd()` hands back the fence that keeps a proxy's own
  host handle open, held with the request: dropping it first let a racing
  close of the sync_file free the number before the call naming it ran.
- A short success reply to a flat nvidia-drm call is -EIO rather than the
  caller's own bytes read back as the answer (ALLOC_NVKMS made a proxy for a
  handle number the caller chose).
- A driver-built IOCTL2 (`nvgpu_i2_call.kernel`) refuses a pointer in the
  user range instead of `memcpy()`ing through it (both builds).
- Event buffers are zeroed, when posted and after each batch.

**Correctness.** Each GPU's render node now hangs off its own fake PCI
device (every one was parented to the first); the DRM node's compat_ioctl
sends the core's ioctls through `drm_compat_ioctl()`, as nvidia-drm does.
`/dev/nvidia-uvm-tools` has a cdev behind it (the backend opens the host's
tools node); `/dev/nvidia-caps/*` answer no ioctl, as nv-caps.c's do.

**Cleanup.** Dead code removed; the experiment switches `poll_events`,
`poll_spin_us`, `claim_alloc`, `claim_sync_fd` are gone; `wl_mode` refuses a
mode giving "other" anything; lines a guest process can cause are
`dev_dbg_ratelimited`, the host's device details at probe `dev_dbg`;
Kconfig is a tristate depending on VIRTIO, DRM and PCI with
`VIRTIO_GPU_NV_RUST`, and the Makefile refuses to link a Rust object that
names a panic symbol. The module says which parsers it has
(`modinfo -F parsers`).

**Not done.** `osdesc_early` is still a ring of 64: a reap that names more
unrecorded ids than that before their registrations' replies are read
loses the oldest, whose pins then stay until remove() (a list would let a
backend grow it without bound). The Rust build has passed the hardware
regression (nesbox, 2026-09-26); the C parsers stay, as the difftest's
oracle and for a kernel built without Rust.

---

## 18. Capture injection

Branch `capture-inject`: a screen share on the host reaches a guest
application without a copy. The desktop's portal gives a PipeWire stream of
GPU buffers to a per-VM **capture helper** on the host (built by the
integrator, not here: it runs the portal request, and the user picks what
to share in the host's own picker). The helper hands each buffer to the
VM's backend over `--inject-socket`; the backend checks it and answers with
an id and a random token; the helper tells the guest's capture daemon both
over a channel of its own (vsock); the daemon opens the buffer through
`/dev/nvgpu-capture` as a guest dma-buf of the same memory and publishes
it to the guest application as a PipeWire stream of its own. PipeWire, the
portal and every stream protocol stay outside the backend. Off by default;
ARCHITECTURE.md §17 has the design, DEPLOY.md "Capture injection" the
deployment.

### The trust boundary

| party | trusted for | not trusted for |
|---|---|---|
| the helper (`--inject-uid`) | injecting only what the user consented to share with this VM: the backend cannot tell a screen share from any other buffer, since a dma-buf says nothing of where its pixels came from | anything else: every packet is parsed as hostile, every descriptor classified by what the kernel says it is, every layout checked against the object, every count bounded |
| the guest (kernel and daemon) | nothing | ids are the helper's to make; a guest names one with a token it can only have been told |
| other host users | nothing | the socket is 0600, opened to the helper's group by root after start, and served only to `--inject-uid` (`SO_PEERCRED`) |

**What the backend checks** (`device/src/inject.rs`, each with a unit test
against a fake nvidia-drm, and the `inject` fuzz target):

- The packet: `SOCK_SEQPACKET`, exactly the size of its op (16, 64 or 8
  bytes), reserved words zero, HELLO first and once at version 1,
  descriptors only on IMPORT (exactly `nplanes`) and IMPORT_SYNCOBJ
  (exactly one), at most 8 per packet (`MSG_CTRUNC` ends the connection);
  a malformed packet ends the connection, a refused request is answered
  with its errno.
- Each plane's descriptor is a dma-buf (`fstatfs`'s magic, not the link
  text a same-uid FUSE file could imitate: §17 #13).
- It imports (PRIME_FD_TO_HANDLE) into a render file of this GPU list the
  backend opens for the purpose, GEM_IDENTIFY_OBJECT there says **NVKMS**,
  and the import is a **self-import**: exported back from the backend's
  file (PRIME_HANDLE_TO_FD) it is the helper's very dma-buf, the same file
  (`fstat`'s device and inode). nvidia-drm imports any other device's
  buffer -- an iGPU's, a udmabuf, a camera's -- as a dma-buf object, which
  IDENTIFY names; but it imports another NVIDIA device's buffer by
  duplicating it into an NVKMS object of its own device
  (`nv_drm_gem_prime_import` -> prime_dup -> `dupMemory`), which IDENTIFY
  calls NVKMS too, and only the self-import proof tells them apart. Each
  render node is tried; the buffer is the node whose import is itself, or
  it is refused (ENODEV). Only a GPU's own nvidia-drm memory is injected,
  and only into guest files of that GPU.
- Every plane is the same object (the guest gets one dma-buf and the planes'
  offsets into it); the format is one of thirteen capture formats with its
  plane count; width and height are 1..16384; the modifier is not INVALID;
  unknown flags are refused; and for each plane, `stride >= width * cpp`
  and `offset + stride * rows <= size` of the object, in u64 with every
  step checked.
- A syncobj (IMPORT_SYNCOBJ) must classify as a syncobj file and import
  into a render file of the backend's (the DRM core checks its file
  operations); the handle is destroyed at once and the file kept.

**Bounds**, per VM (one backend): 32 buffer ids and 1 GiB of them at once,
16 syncobj ids, 4 helper connections (a thread each), a 4-connection
backlog; guest opens, one per (render file, object), 1024 and a quarter
per guest process (`quota::Share::quarter`). Refusals log through the
per-site rate limit. INJECT_OPEN checks the caller's share before it
imports anything, and on any later failure closes nothing: an import that
finds the object already in the file hands back the handle the file had,
whatever made it, which is not the backend's to close. What the bounds do not count: an object a guest keeps
open after the helper released it. That is memory the helper allocated,
held by at most 1024 guest opens, and a guest can allocate GPU memory of
its own without any of this (§4, "not capped").

**Why a token and a node permission.** The node (`/dev/nvgpu-capture`,
root:root 0660, `capture_mode`, never "other"; `contrib/udev/70-nvgpu-
capture.rules` gives it to the daemon's group) decides who may try at all:
applications never open it, they get the daemon's dma-bufs through
PipeWire. The token decides which stream: one daemon, or several of
several guest users, may open the node, and a buffer opens only for the
one told its 128-bit token, compared in constant time; a wrong token and a
missing id are the same ENOENT, and a released id's token opens nothing
again (each IMPORT draws a new one from `getrandom`). No guest message
lists, enumerates or makes an id. A token is a secret only while nobody
else can read it: it must never be on a command line or the kernel's
(readable by every user), in a log or in a world-readable file (DEPLOY.md;
the rig's test puts it on the guest's kernel command line, for want of
vsock, and is no model).

### What a guest can do with a buffer

**Read it**, which is what it is for, from the moment its daemon opens it
until it closes its last handle; the helper's RELEASE only stops new opens,
and a guest's GEM handle keeps the memory as any importer's does.

**Not map it writable for the CPU through the proxy.** Every window
placement of the object's mmap range (its `drm_vma_node` offset, the same
from every file) is read-only whichever of the VM's files maps it, for as
long as the id lives or a guest open of it does
(`BackendInject::read_only`); the guest module refuses a writable mapping
of a read-only placement, and the dma-buf it hands out is opened without
`O_RDWR`. On the RTX 5090 a `PROT_WRITE` mapping fails with EACCES
(dma-buf) and EINVAL (render node), and an `mprotect` to writable with
EACCES.

**Write it with the GPU**, and there is no way to stop that. An NVIDIA
import is read-write: NVIDIA's userspace turns the imported object into an
RM handle of its own (GEM_EXPORT_NVKMS_MEMORY, then RM's OS_UNIX
export/import, a full duplicate of the memory descriptor into the guest's
client), and RM's read-only GPU mapping (`NVOS46_FLAGS_ACCESS_READ_ONLY`)
is the mapper's choice, forced only for memory allocated
`MEMDESC_FLAGS_DEVICE_READ_ONLY`, which NVKMS never asks for
(nvidia-drm-gem-nvkms-memory.c, mem_mgr/mem.c, virt_mem_allocator_gm107.c,
610.57.04). The same duplicate can be CPU-mapped writable through RM (BAR1)
unless the memory was allocated `ATTR2_PROTECTION_USER=READ_ONLY`, which it
is not. So the read-only placement is hygiene, not a boundary: a guest
process holding the buffer can write it. What it can write is its own
stream's buffers, which only it, its daemon, the helper and the compositor
filling them see; the compositor overwrites them each frame, nothing on the
host reads them, and they are no host scanout surface. Natively, a
PipeWire consumer of the same stream can do the same.

**Not hand it on.** No export of an injected object leaves the backend:
HOST_OP PRIME_EXPORT of a handle INJECT_OPEN made is refused (EINVAL)
before the host is asked, and so is any export whose dma-buf is an injected
object's, from any handle and any path -- PRIME_EXPORT, a Wayland buffer
for the host compositor, an IOCTL2 re-home into a KMS file for a
framebuffer -- recognised by its identity: an export of the object from any
file is the helper's own dma-buf (nvidia-drm keeps the object's
`dma_buf`), which the backend holds, and keeps open while an id or a guest
open does (`inject::Taint`). Otherwise a guest could send the portal's
buffer to the host compositor as its own window, scan it out on a leased
output, or make host dma-buf handles of it outside the open's bounds.
Inside the guest nothing changes: the guest's dma-buf is its own proxy's,
shared between guest processes without the host. Not covered: an object a
guest still holds through a handle no open recorded (a GETFB of a
framebuffer made from it) after its id and every open are gone -- by then
the export makes a new dma-buf of an object the helper has let go.

**Nothing else.** INJECT_OPEN's host calls on the guest's render file are
calls the guest can already cause there -- a PRIME import (the self-import
of NVKMS memory; HOST_OP DMABUF_IMPORT does the same), IDENTIFY, MAP_OFFSET,
SYNCOBJ_FD_TO_HANDLE -- on descriptors the backend holds; the guest's
bytes are an id and a token. A guest's GEM handle is a handle of its own
render file, one per (file, object), closed with the file.

### Across VMs, and apps within one

Each VM's backend has its own socket and registry: another VM's ids and
tokens mean nothing there (tested with two backends), and a helper reaches
another VM only if that VM's backend admits its uid -- which is why each VM
needs a helper user of its own (DEPLOY.md; the NixOS module refuses two VMs
one helper uid). The injected object is the helper's allocation; no VM's
backend but its own ever receives the dma-buf. Inside a guest, the daemon
decides which application gets which stream, as a desktop's PipeWire does;
an application given the dma-buf can read and GPU-write it, as natively.

### Sync, and what the guest can do with a syncobj

The first cut has no host fence at all: the helper announces a frame over
its own channel only once the frame is complete, and learns that the guest
is done with a buffer the same way (ARCHITECTURE.md §17 has the contract).
Nothing on the host waits for the guest: a guest that never says done
costs the helper a timeout, after which it gives the buffer back to the
stream and the guest sees tearing at worst.

The second step, built and run: IMPORT_SYNCOBJ hands the backend a DRM
syncobj the helper made; INJECT_OPEN_SYNCOBJ imports it into the guest's
render file. The guest may then wait on and signal any point of it, early,
late, out of order or never. Its signals reach the helper's syncobj and
nothing else: the helper must treat every point as a hint, bound its own
waits, and never forward the guest's release points to anything that
trusts them. The guest's waits are the VM's ordinary syncobj waits, turned
into polls and counted against the 1024 wait registrations per VM and a
quarter per guest process (§12's fence work, fence.rs); the render file is
marked as one that imported a syncobj, so its registrations wait out their
firing rather than go with a DESTROY, as for any syncobj someone else
holds. Each INJECT_OPEN_SYNCOBJ makes a new handle in the caller's file, as
SYNCOBJ_FD_TO_HANDLE and SYNCOBJ_CREATE do, which the guest can already
make without bound (§4).

### What a compromised helper could and could not do

**Could:** inject any NVKMS dma-buf it holds into its VM -- its own
allocations, the stream the portal gave it, any buffer another process
hands it -- which shows its VM pixels it could equally send over vsock as
a copy; hold host GPU memory through the backend, within the bounds above,
memory it allocated itself; make the backend import other devices'
dma-bufs it holds (refused after the import, which attaches the buffer to
its exporter for a moment); keep its four connections and threads.

**Could not:** be root or the backend's own uid (the backend refuses
either as `--inject-uid`, the latter but for the diagnostic
`--allow-inject-self` a one-user rig needs; the NixOS module also refuses
the VM's backend and VMM uids where they are fixed, and one helper uid for
two VMs); reach another VM (another backend, another uid); make the
backend open, map or write anything (it imports, identifies, sizes and
keeps descriptors it was given; nothing is mapped into the backend); reach
guest memory, the window or the virtqueue; stall the VM: its imports hold
a lock of their own that no guest message takes, and the registry's state
lock, which INJECT_OPEN takes, is never held across a kernel call on a
helper's descriptor (a test stalls an import and opens meanwhile); make
the guest open anything the guest's daemon does not ask for.

### What it adds to the host

- **Backend surface:** one listener, at most four connection threads, a
  fixed-size parser (`protocol/src/inject.rs`), and the host calls above.
- **Sandbox:** no change. The listener is bound before the sandbox, like
  the export socket; `accept4`, `recvmsg` (SCM_RIGHTS), `fstatfs`, `lseek`,
  `readlinkat` of `/proc/self/fd` and `getrandom` were on the seccomp list
  already; the render node the backend imports into is one Landlock already
  opens. The sandbox ran enforcing ("every layer in force") in every
  hardware run below.
- **Guest surface:** HOST_OP INJECT_OPEN and INJECT_OPEN_SYNCOBJ, which
  refuse everything without a live id's token, and the guest node's two
  fixed-size ioctls (`driver/nvgpu_capture.c`, C in both builds; it parses
  only its own structs).

### Tests and hardware

24 unit tests (`inject::tests`: every rule above, the socket's framing,
uid and peer limits, release on hangup, two backends, a stalled import, the
read-only placement through the dispatcher, the per-process open share),
the protocol's own, and the `inject` fuzz target (packets, imports with
descriptors of every kind, opens with right and altered tokens, syncobjs,
closes; bounds, token and descriptor-leak oracles): ten minutes, no
finding.

On the RTX 5090 (595.99.02), under nesbox and under crosvm (its nvgpu
frontend jailed), with the backend's sandbox on (`rig/TESTING-RIG.md`,
"Capture injection"): GBM buffers allocated as xdg-desktop-portal-hyprland
allocates them (`gbm_bo_create_with_modifiers`, block-linear modifier
`0x0300000000606014`) are NVKMS memory and accepted; a udmabuf (ENODEV), a
memfd (EBADF) and a layout past the buffer (EINVAL) are refused; each
buffer's pixels, painted on the host, read back identical in the guest
through EGL and through Vulkan at 1280x720 and 2560x1440, checksums equal
to the host's; writable CPU mappings are refused; a wrong token, a missing
id and a released id are ENOENT; a user without the node's group cannot
open it; a buffer the host keeps painting is seen changing without a new
open; and 200 frames of explicit sync through an injected syncobj arrive
whole and in order, the host's announce-to-release round trip 55 us
(nesbox) and 45 us (crosvm) at the median. A GPU-only import costs the
shared window nothing (no placement is made); a CPU mapping of a 1440p
buffer places 15 MiB, read-only.

**The guest module** reserves the dma-buf's descriptor number before it
builds anything and installs it only after the reply is copied out, so a
failed copy-out takes back only what the call made.

**Open:** the helper and the guest daemon are the integrator's, and so is
how the helper proves consent; buffers whose planes are separate objects
are refused (one dma-buf per buffer in the guest ABI); a guest open whose
reply its caller abandoned leaves its GEM or syncobj handle in the caller's
file until it closes; a real portal stream was not injected by the project
(the tests may not open the host's picker): `rig/rig-tools/portal-identify.sh`
checks one, for the owner to run.

---

## 19. The window's size and share

Branch `window-config2`. Two defects in how RM mappings were tracked, which
made a game's process run out of its share of the window with the window
not full, and two flags for VMs that need more window than the default:
`--window-size` and `--window-owner-share`. Reported from a deployment (Prism
Launcher / Minecraft on an RTX 5090: about 950 refusals of the form `SHM
WriteCombine zone: guest process ... holds 0x17f82000 of 0x30000000 bytes
and may not take 0x200000 more (Owner)`, then Xid 69).

### The two defects

**UPDATE_DEVICE_MAPPING_INFO was not followed.** The NVIDIA library maps
a file an RM_MAP_MEMORY armed and then tells RM where
(`RmUpdateDeviceMappingInfo`, osapi.c: RM moves its record of the mapping
from `pOld` to `pNew`); from then on it names the mapping by that virtual
address, in a later UPDATE and in RM_UNMAP_MEMORY. The backend translated
`pOld` by `(client, memory)` alone -- which of two mappings of one object
it moved was whichever it found first -- and never recorded `pNew`, so
the unmap, which it looked up by the window offset alone, missed (`UNMAP_MEMORY:
no mapping for pLinearAddress=0x7f...`), and the extent stayed charged to
the process until the file it was armed on closed. The rig's own logs had
6,448 such misses in 225 of 306 runs, every one a guest virtual address;
there, they were at application exit and the files closed right after. A
client that keeps the files (`rig/guest-image/tools/nvgpu-map-churn.c`
does, mapping 2 MiB of video memory and unmapping it by address in a loop)
was refused after exactly 192 iterations with the report's own line, its
share (0x18000000 of 0x30000000) full of mappings it had unmapped.

Now (`mmap.rs` `MmapContext::find`): an entry records the process that
made it (the owner of the file the map ran on) and the address its last
accepted UPDATE gave it. UPDATE and UNMAP find a mapping by `(client,
memory, process, address)`, the address matched against the recorded
virtual address first and the window offset second -- never by an address
alone, and never another process's: RM itself answers both only for the
calling process's mappings (`serverutilMappingFilterCurrentUserProc`), and
every call reaches it from the backend's one process, so the filter is
kept here. An UPDATE whose `pOld` names none of the caller's mappings of
the object moves its only one, as before, and with two it guesses nothing
(RM is handed 0 and says so). A later UPDATE to the same address takes it
from a stale entry. RM is still handed only the backend's own address, as
both `pOld` and `pNew`; a guest address never reaches it. The same
churn, 400 iterations, finished with the zone's peak at one mapping (2
MiB).

**RM_MAP_MEMORY was not transactional.** The extent was allocated after
RM had mapped, and a refused extent returned ENOMEM with the host mapping
made and recorded nowhere until the memory was freed. Now the extent is
reserved first, in the zone the mapping will most likely need (rmmem's
records; video memory is WC), so a refusal leaves the host with nothing
mapped. RM's answer decides the type: another type moves the reservation,
and if it does not fit there, or the VMM will not place it, or there is no
window, the host's mapping is undone with RM_UNMAP_MEMORY at the host's
address. The file stays spent (a host file carries one mapping in its life)
until the guest closes it.

Neither defect gave one process reach into another's memory: the lookups
were keyed by the caller's own client, and RM refused a call about another
client's objects. Both were availability, within one VM.

### The flags

`--window-size <MiB>` (default 1024) and `--window-owner-share <percent>`
(default 50) make the one `ZoneConfig` that sizes the allocator and answers
the VMM's GET_SHMEM_CONFIG, so the guest-visible region and the allocator
cannot disagree. Refused at start, with the reason: not a multiple of 64
MiB, under 256 MiB, a share outside 1-95 %, or the window with the UVM
aperture (with `--allow-compute`) past crosvm's 64 GiB shared-region cap.
nesbox (branch `virtio-nvgpu-v4`) asks the backend for the size and refuses
one past 32 GiB, what fits its 64-bit MMIO window beside the aperture;
before v4 it published 1 GiB whatever the backend said, and the rig's
launcher refuses that pairing. crosvm sized its BAR from the backend
already (the next power of two; 16 GiB plus the aperture is a 32 GiB BAR).
The guest takes the size from the shared-memory capability.

The zones: UC stays at 32 MiB, and WC and WB split the rest 24:7, as in the
default (DEPLOY.md, "Sizing the window", has the measurements behind it).
At 1024 MiB this is exactly the default's UC 32, WC 768, WB 224 MiB.

### What a larger share gives up

A share is `Share::percent(size, p)` (quota.rs): an owner may hold `p` %
of a zone, and the last eighth is kept for owners holding at most a
sixteenth -- exactly `Share::half` at 50. Past 87.5 % the reserve shrinks
to what is left beside a whole share, and the floor with it, so the share
asked for can be had.

| window, share | WC zone | one process | reserve | each other process, in the reserve | processes to fill it to the reserve |
|---|---|---|---|---|---|
| 1 GiB, 50 % | 768 MiB | 384 MiB | 96 MiB | 48 MiB | 2 |
| 4 GiB, 75 % | 3148 MiB | 2361 MiB | 394 MiB | 197 MiB | 2 |
| 16 GiB, 90 % | 12660 MiB | 11394 MiB | 1266 MiB | 791 MiB | 1 |

From 88 %, one process can take a zone down to its reserve, and every
other process of the VM keeps only the reserve, each at most the floor of
it -- still more, at 16 GiB, than a whole default share. This is
availability within one guest only: each VM has its own backend, window
and zones, and nothing one VM's processes hold is charged to another VM.
It is the operator's choice for a VM that runs one heavy application.

### What a larger window costs the host

- **BAR1.** Video memory a guest CPU-maps is mapped through the GPU's BAR1,
  which the host's desktop and every other VM share; the window bounds how
  much one VM can hold at once (its WC zone: 12.4 GiB at 16 GiB). The window
  is the per-VM cap; there is no host-wide one, as natively there is none
  per process, and a cross-VM budget would be a channel between VMs'
  backends, which this design has none of. The backend warns at start when
  the WC zone is more than half of a GPU's BAR1 (read from sysfs). The RTX
  5090's BAR1 is 32 GiB with resizable BAR; without it 256 MiB, and even
  the default window then warns. Video memory itself was never bounded by
  the window, only how much of it is mapped at once.
- **Host memory.** The backend's copy of the window is a sparse memfd its
  own pages never fill (`MemoryMax=2G` is unaffected). nesbox reserves the
  guest-visible window `PROT_NONE`, so it costs nothing until a placement.
  crosvm's is shared anonymous memory: pages of it a guest touches with
  nothing placed there are faulted in and charged to the VMM's cgroup, up to
  the window's size -- as guest RAM is -- so its memory limit must cover
  guest RAM plus the window (DEPLOY.md). Only a guest kernel touches such
  pages; no guest driver path does.

### Tests and hardware

Unit tests against a fake RM that tracks its mappings and finds them by
object and address: map, UPDATE, unmap by the virtual address returns the
zone and the process's share exactly; two mappings of one object; another
process's address and window offset name nothing; a refused extent makes
no host mapping and no charge; a retyped mapping moves zone or is undone;
a refused placement undoes both. `Share::percent` against `half` at 50,
and its reserve; every window size from 256 MiB to 64 GiB in whole 2 MiB
zones; the flags' refusals; GET_SHMEM_CONFIG from the flags; nesbox's
window from the backend's size.

On the RTX 5090 (595.99.02), sandbox on, RM allowlist enforcing
(`rig/TESTING-RIG.md`, "Window size and share"): with the default window,
`stage1` 6/0/0, `render` with compute 9/0/1 (cuda-smoke), `wayland` on the
live Hyprland 13/0/1, `secneg` ctl + render 10 passed, 5 skipped; at 4 GiB /
75 % and 16 GiB / 90 %, `stage1` and `render` with compute under nesbox and
crosvm, the guest seeing a 4 or 16 GiB window; SuperTuxKart (also in
gamescope), Blender GL and Vulkan, glmark2, vkmark and Chromium in one VM,
19/0/0, at 16 GiB / 90 % under both VMMs and at the default. No run logged
`no mapping for pLinearAddress` or `SHM alloc failed`, and no Xid. The
churn reproduced the report against the backend before the fix and ran
400/400 after it. A 48 GiB window was refused by nesbox with its size named,
and the backend warned that its WC zone was more than half of BAR1.

## 20. Frame pacing

Branch `frame-timing`. Games in a guest paced worse than natively; what was
found and changed is in DEPLOY.md, "Frame pacing". This section is what the
changes do to the boundary. None lifts a cap: the syncobj wait registrations
of §17 (1,024 per VM, a quarter per process) were never reached in any run
(the counters below say so: no wait went over them, none backed off), and
they stand as they were.

**Armed readiness** (`GCAP_ARMS_READY`, `BCAP_ARMED_READY`, `W_ARM`). A guest
that says it arms an RM device's readiness gets one report per `W_ARM`
instead of one per host event. A `W_ARM` is a WATCH naming one of the
guest's own handles; it is refused unless the session negotiated it and
the handle is an RM device (control, GPU or UVM file -- not the modeset
device, not a Wayland channel), and it costs the backend a pump
instruction and two booleans per watch that already exist. A guest that
arms nothing, or arms wrongly, gets fewer reports on its own files; it
reaches nobody else's, and the host descriptors are polled as before. A
guest that does not negotiate it keeps a report per event.

**`--queue-poll-us`** (off by default). The queue thread keeps looking at
the control ring for up to that long (capped at 1 ms) after draining it.
It is CPU the backend spends for the guest, as the drain itself is: a guest
that keeps sending keeps the thread busy either way, and the poll adds at
most the cap after each burst. It reads only the ring's avail index, which
the transport already reads. Leave it off where host CPU is shared tightly
between tenants.

**The guest's reply spin** (`rt_spin_us`, default 20 µs, 1 ms at most) and
**asynchronous fence WATCH** (`async_fence_watch`) are guest-internal. A
proxy's WATCH sent from a work item holds a reference to the proxy, so its
handle cannot be closed ahead of it; a WATCH the backend refuses signals the
proxy with the error, where the call used to fail -- a guest fence never
waits forever on a report that will not come.

**The event queue** no longer asks the guest to kick while the pump holds
buffers (it asks again when it runs out, as before): fewer guest exits, the
same delivery.

**Counters.** `device::pacing` is fixed-size relaxed atomics, and the IOCTL2
table is keyed by the schema's own static names (a closed set), so a guest
cannot grow it; the report is logged at teardown at `warn` (rate-limited, as
every call site) and, with `--pacing-stats`, periodically. The guest's
counters are readable by root alone (`/sys/module/virtio_gpu_nv/parameters/
pacing`, 0400): they count every process's calls as they happen, which an
unprivileged process should not be able to watch.

**The launcher's placement knobs** (`NVGPU_CPU_AFFINITY`, `NVGPU_VCPU_PINS`,
`NVGPU_IO_AFFINITY`, `NVGPU_BACKEND_CPUS`, `NVGPU_HUGEPAGES`,
`NVGPU_SLICE_US`) change where and when threads run and which pages back
guest RAM, nothing about what they may do; `taskset` and `chrt` run before
the backend's sandbox and the VMM's jail, which are unchanged. The launcher's
default 100 µs EEVDF slice for the VMM's and backend's threads changes how
soon they run after waking, not their share of the CPU (their weight is
the default one), and it is the launcher's to set: nothing in the guest can
change it.
crosvm's `--core-scheduling=false` (`NVGPU_CROSVM_CORE_SCHED=0`) gives up
the per-vCPU core-scheduling cookies crosvm sets by default, which keep an
SMT sibling from running another task while a vCPU runs: a mitigation for
cross-thread side channels (L1TF/MDS-class) between the guest and host
tasks on the sibling. The launcher keeps crosvm's default; turn it off
only where no other tenant shares the cores.
