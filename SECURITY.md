# Security

What a guest can reach on the host, before and after the display-passthrough
work, and what is still open.

This is the security review of branch `display-passthrough` at `ae182ab`
against `dev` at `50ff74a` (called **dev** below), brought up to date by the
audit of branch `harden` (§11), the RM allowlist of branch `rmallow` (§12),
the fuzzing of branch `fuzz` (§13), the memory-safety structure of
branch `dind` (§14), the review of `dind` for memory passing (§15), and the
VMMs it runs under, nesbox and crosvm (§16). It is written for the project's owner. The
code is the reference: where this document and the code disagree, the code
is right.

> **Hardware status.** The rig (`TESTING-RIG.md`) runs stage 1, render (with
> and without CUDA), the Wayland proxy against a headless sway, the security
> negatives and an application pass on an RTX 5090, under nesbox and crosvm,
> with the RM allowlist enforcing and the backend's sandbox on. KMS, leases and
> direct scanout have not run. Statements below about what the host kernel does
> with a request were read from source (NVIDIA's open modules 610.57.04, Linux
> 7.2.7) unless a section says it was observed.

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
  deny: 215 of 610.57.04's 1,370 controls (plus 24 GSP pass-through numbers
  seen on hardware) and 96 of its 222 classes reach RM. The list was built
  from 106 hardware runs and NVIDIA's sources, and has not itself run: a
  workload it misses breaks with RM's "not supported" and a log line naming
  the call.
- The host NVIDIA driver is in the TCB. There is no IOMMU boundary between
  guest GPU work and the host.
- The backend sandboxes itself before the first guest message -- a network
  namespace, Landlock, a seccomp allowlist (§4, "The backend's sandbox") --
  and, run as root, each VM's backend and VMM are host users of their own
  (§4, "One uid per VM"). What neither touches is the GPU (RM's page tables
  keep VMs apart there) and the host kernel the backend still calls. There is
  no cgroup of the backend's own (the launcher's `NVGPU_MEMORY_MAX` scope
  holds the whole run), and the per-guest isolate is not built.
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
(`guest-image/probes/render.sh`). Vulkan Video (NVENC and NVDEC) uses RM's
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
  32 TiB), where a 64-bit VMM has nothing: its executable and heap sit at
  two-thirds of the 47-bit space (85 TiB), and its mappings grow down from
  below the stack or, under a legacy layout (an unlimited stack rlimit), up
  from a third of it (42.7 TiB), which the 64 TiB top the band once had
  reached. The backend, the VMM and the guest driver all check the band, and
  the VMM maps with `MAP_FIXED_NOREPLACE`, so a collision fails rather than
  replacing anything. Two of the guest's own pools at one address are
  refused by the backend before the VMM is asked, with ENOMEM as any
  placement is (it was EEXIST). That does not hide everything (§11, B5 and
  F3): a refusal where the caller's budgets had room still says the address
  is taken -- by another guest process's pool, which one process can so find
  and squat on, or by the VMM's mapping of guest RAM, which with several GiB
  of RAM can lie in the band.
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
  - each UVM external mapping (MAP_EXTERNAL_ALLOCATION) of any of them,
    counted whatever UVM answered, until UNMAP_EXTERNAL has covered it on
    every GPU it names, UVM_FREE takes its external range, or its UVM file
    closes. On that close the backend takes the mappings down itself first,
    on its own descriptor, because the event pump's duplicate can make the
    file's last close later; a mapping that will not come down keeps the
    pages until the session ends.

  A free the backend cannot see (an ancestor above the parent), a range it
  did not record (past 65,536 per VM) or a mapping UVM never made makes the
  release late, never early.
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
  backend's; 65,536 UVM mappings of registered memory per VM.
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
| host peers | the VMM's vhost-user messages | plus the host compositor's Wayland messages, export peers' Wayland messages, and kernel uevents (messages from any sender but the kernel are dropped). Every descriptor received over a socket is classified by what the kernel says it is (`hostfd::classify`), not by what the message claims. |

### Host files, sockets and netlink

| | dev | HEAD |
|---|---|---|
| device files | `/dev/nvidia*`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset`, `/dev/dri/renderD*` | the same, the two UVM devices only with `--allow-compute`, plus `/dev/dri/card*` (`--kms-card` only, through OPEN_KMS; a plain OPEN of a card is refused), lessee files received from the compositor, which must classify as a lease of this GPU, and `/dev/udmabuf` |
| other files | `/proc/driver/nvidia`, PCI config in sysfs | the same, plus memfds for shm pools, blobs and a sealed page, and readlink of `/proc/self/fd` |
| vhost-user socket | default `/tmp/nvgpu.sock`, and a failed unlink was ignored, so another user could bind it first and receive the guest's memory | default `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` in a 0700 directory, refused if the directory is anyone else's; a file already at the path is removed only if it is a socket of the backend's uid, and anything else stops the start (`device/src/posture.rs`) |
| other sockets | none | with `--wayland-socket`, one connection to the compositor per channel plus a probe connection; with `--wayland-export`, a listener created 0600 that admits only peers of the backend's uid, with 16 pending |
| netlink | none | `NETLINK_KOBJECT_UEVENT`, receive only, with `--kms-card` or `--wayland-lease` |

### Privileges

| | dev | HEAD |
|---|---|---|
| identity | whoever started it. The shipped `scripts/run-guest.sh` ran it as root, which makes every guest process an RM administrator: all of BAR0 mappable read-write, the register allowlist skipped, and DRM files authenticated. | Refuses to start with euid 0 or CAP_SYS_ADMIN unless `--allow-root-unsafe`. The launcher, as root, gives each VM a slot of a user pool: the backend runs through `setpriv` as `nvgpu-vmN`, in the groups video, render and kvm, with no capabilities and no_new_privs, and the VMM under nesbox's jailer as `nvgpu-vmmN` (below, "One uid per VM"). With `--wayland-socket` the backend runs as the socket's owner (the desktop user), and with `--wayland-export` as the owner of the export socket's directory. Unprivileged (the rig), both are the invoking user. |
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
| window | the zones, first come first served | each zone (UC 32 MiB, WC 768 MiB, WB 224 MiB) at most half per guest process, the last eighth kept for processes holding at most a sixteenth; a mapping is charged to whoever opened the file it is armed on |
| Wayland caps | -- | 64 channels per VM. Shm: 1 GiB and 1,024 pools per VM, and 512 MiB and 256 pools per connection; the bytes are what live buffers cover (page-rounded, overlaps once), not pool sizes, since a pool's memfd is sparse, SHM_SYNC writes only inside a live buffer, and pages no live buffer covers are punched out. Unread output: 256 MiB per VM and 64 MiB per connection, half the VM's per guest process (the last quarter kept for processes holding at most a quarter). Per guest process -- the client a daemon connection is for (NVGPU_WL_IOC_CONNECT_FOR), else the opener -- a quarter of the channels (the last eighth kept for processes with at most two) and a quarter of the shm bytes and pools, shared by all its connections. 16 unfinished blobs per connection. 131,072 objects per connection. Lease submits: one per 5 s on average, 3 at once. Four are flags: the channel count (`--wayland-max-conns`), the shm byte budget (`--wayland-shm-budget`), the queue budget (`--wayland-queue-budget`) and the lease interval (`--wayland-lease-interval`). The 1,024 pools per VM and the burst of 3 are fixed. |
| not capped | -- | Memory outside the Wayland and window budgets has no limit, and no cgroup. Several VMs of one backend user share that user's host limits. |

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

Run as root, `scripts/run-guest.sh` takes a free slot N of a pool made once
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
- **Memory.** No cgroup is keyed to the uid; the launcher's scope
  (`NVGPU_MEMORY_MAX`) bounds a run.

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
- A lease file must classify as a lessee of this GPU.
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
| `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` | 0666 | 0666, unchanged: any guest user reaches everything §3 lists. The two UVM nodes exist only when the backend runs with `--allow-compute` |
| `/dev/nvidia-caps/*` | 0444 | 0444 |
| DRM node | a hand-made character device, 0666 | a real DRM device per host render node, whose render and primary nodes the guest's DRM core makes. Syncobjs are enabled only when the backend serves fences, and the primary node drives KMS only when the backend offers `--kms-card`. |
| `/dev/nvgpu-wl[N]` | -- | root:root 0660 by default (module parameter `wl_mode`), with `scripts/70-nvgpu-wl.rules` giving it to group `nvgpu-wl` for the daemon, which is to be setgid (or its own account), never an application's group. Five ioctls: HELLO, CONNECT, CONNECT_FOR (a connection charged to the client process the daemon names), SEND and RECV. One LISTEN per device, and ACCEPT only from the listener's effective uid or CAP_SYS_ADMIN. The daemon holds at most 4 MiB a client has not read, and closes a client that stays that far behind for 30 s. |
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

Which host surfaces each display mode turns on:

| mode | flag | host surfaces it adds |
|---|---|---|
| headless, v2 | none | IOCTL2 render class and NVKMS tables, fences, HOST_OP without OPEN_KMS, `/dev/udmabuf`, memfds |
| Wayland client, and the host's direct scanout of a guest buffer | `--wayland-socket` | compositor connections, `wlwire`, reader threads |
| DRM lease, VK_KHR_display | `--wayland-lease` | lessee files as KMS handles, nvidia-drm grants and NVKMS gates, uevent netlink, leasable monitors in the patched compositor |
| compositor VM | `--kms-card` | host card files as DRM master, OPEN_KMS and DROP_IF_MASTER, CREATE_LEASE, the three NVKMS commands above, uevent netlink. The guest owns the host's display and can show anything on it. |
| export | `--wayland-export` | the listener, host peers, import of host dma-bufs |

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

1. **The design** (`DESIGN.md` v1), by four reviewers. v2 resolved their
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
longer works in the code. None of it has been checked on hardware. No second
adversarial pass has been run over the fixes: each status is taken from its
fix commit and the code. S-35, which a later review of the RM path opened
(NV_ESC_RM_SHARE forwarded raw), is fixed in the code the same way.

### Verification findings (NVK_VERIFICATION)

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
| S-4 | high | shm memfds: host OOM the OOM killer cannot attribute | `86553c1`, `de95ad3` | fixed; refusing SHM_SYNC before commit, and a cgroup in the launcher, not done |
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

1. **RM_CONTROL and RM_ALLOC are allow-listed, but the list has not run.**
   215 of 610.57.04's 1,370 controls and 96 of its 222 classes reach RM
   (§12); each is checked by RM as it would check a user process without
   admin rights, and a bug in any of them is a host bug. The list covers
   what 106 hardware runs used, the other architectures' classes for the
   same objects, and named NVENC, NVDEC, Vulkan Video and compute paths that
   have not run. A call it misses fails with RM's own "not supported" and a
   log line naming it; `--rm-allowlist=log` finds them without failing.
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
4. **The isolate is not built.** `isolate/README.md` describes one helper per
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
   `scripts/verify/sec-negative.c`, the VID_HEAP_CONTROL half of T1 sets
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
    EPERM where it works natively. None of it has run on hardware.
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
    token checks is not given to guest root. None of it has run on hardware
    (sec-negative T8 and T10 are the on-device checks), and no workload of
    the 88 runs issued NV_ESC_RM_SHARE.

---

## 10. Hardening roadmap

In priority order. Cost is a judgement, not a measurement.

1. **Run [`TESTING.md`](TESTING.md) on the RTX 3060**, security stages
   included, after fixing `sec-negative` T1. This comes first because every
   refusal above is still a claim, and because the allowlist in item 5 needs
   the workloads run first.
2. **A memory cgroup per backend.** For example, `systemd-run --scope -p
   MemoryMax=… -p TasksMax=…` in the launcher, with `oom_score_adj` making the
   backend the preferred victim. Memfd shmem is charged to the writer's cgroup,
   so an OOM stays inside the VM that caused it. This covers memory no
   budget counts, and needs no backend code.
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
5. **Run the RM allowlist (§12).** Done in code, default deny. What is left
   is running item 1 with it on, reading the teardown's "RM allowlist
   refused" lines, and growing `WORKLOAD_CONTROLS`/`WORKLOAD_CLASSES` in
   `gen/rmallow_extract.py` from them -- NVENC, NVDEC and Vulkan Video first,
   which no run has exercised. After that, holding each control to the
   classes of the object it is sent to (§12, "What it does not do").
6. **seccomp and Landlock per backend: done** (§4, "The backend's
   sandbox"). What is left: run the device stages with it on and extend the
   list by what they report; an `ioctl` filter by request number per
   descriptor kind is not possible in a filter, and would take the isolate.
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
   guest RAM is mapped (F3); a caller's process on IOCTL2 (R4); fair queuing
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
fixes are unit-tested and the guest module builds clean, and none of it has
run on the GPU.

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
| W3 | medium | app vs app | at the VM's queue budget the connection whose output crossed it was dropped, often an innocent one; the rig's app pass (`6263e20`, on `display-passthrough`) puts the app user in `nvgpu-wl`, so any app can hold raw channels it never reads | `b5980e5` | fixed in the backend: the queue budget is per process, so the connection that crosses its share is its own. **To do on merge**: `guest-image/probes/apps.sh` (`nvgpu_user=1`) must not add the app user to `nvgpu-wl`; run the daemon setgid `nvgpu-wl` (as `scripts/70-nvgpu-wl.rules` now says) or as an account of its own. `harden` does not have that commit |
| F2 | low | app vs app | export/import to a descriptor is a second way to move an RM object between clients, outside the DUP_OBJECT gate | -- | native strength, documented (§6): after R1/R2 the descriptor must be a control file the caller has open, which another process can only have handed it |
| F3 | low | host surface | the aperture band [4 GiB, 32 TiB) can hold the VMM's mapping of a large guest RAM: a pool there fails, and says the address is taken | -- | open. It would take the VMM reserving the band (PROT_NONE, MAP_NORESERVE) before it maps guest RAM, and placing pools over its own reservation; a change to nesbox's start-up order that needs a run to trust |
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
   classes for NVENC, NVDEC, Vulkan Video and compute paths not yet run.

| release | controls allowed / exported | classes allowed / defined |
|---|---|---|
| 535.129.03 | 211 / 1,132 | 67 / 145 |
| 580.178.04 | 215 / 1,357 | 93 / 209 |
| 595.71.05, 595.99.02 | 215 / 1,349 | 93 / 209 |
| 610.57.04 | 215 / 1,370 | 96 / 222 |
| 615.71.09 | 217 / 1,389 | 96 / 224 |

Each release also allows the 24 observed controls RM passes to GSP-RM with no
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
sources. Only the 22 GSS legacy and 2 BINAPI numbers observed are allowed.

**What it does not do.** It keys on the command, not on the object it is sent
to: an allowed control sent to an NV2081_BINAPI handle still goes to GSP-RM
without CPU-RM's size check (the backend holds RM's size itself on a release
measured exactly). It does not look inside parameters beyond the fields
above. It has not run: the observed set says what Vulkan, GL, EGL, CUDA and
the browsers need, but NVENC, NVDEC and Vulkan Video rest on names read from
sources and gVisor's lists, and a missing one shows up as a failed workload
with a warning of the form

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
policy (`nvkms.rs`), fence waits (`fence::before`), SYS_PARAMS' and
CHECK_VERSION_STR's command byte -- only unable to reach a declared field.
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
unit-tested and the guest module builds clean; none of it has run on a GPU.

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
already safe (`MAP_FIXED_NOREPLACE`, a slot only on success).

**crosvm** (`patches/crosvm/`): every request is bounds-checked against the
region the backend reported (overflow-checked, page-aligned offsets,
overlaps refused, unmaps must name a live mapping, reset unmaps all); GPU and
external maps are refused for the nvgpu type; a region size above 64 GiB is
an error, not a panic; a refused request no longer stops the VM. No seccomp
or minijail policy changed.

**Where the mapping requests run.** crosvm jails each device it emulates in
a process of its own with a seccomp policy, but a vhost-user *frontend* runs
in its main process, which upstream does not seccomp-confine; the launcher
adds a user and network namespace and pivots into an empty root. nesbox
installs one baseline seccomp filter on every thread, including the one
that serves these requests. So for this device's mapping path nesbox is the
more confined of the two; for everything else crosvm emulates, crosvm is.
The backend -- where guest bytes are parsed -- is the same process, uid and
sandbox under both.

**Compute** runs under nesbox only: crosvm publishes one shared-memory
region per device, and the UVM aperture needs a second with mappings at
host addresses the backend names. The launcher refuses `--allow-compute`
with `--vmm crosvm`.

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
the log level, and in the log. `scripts/run-guest.sh` adds `--diagnostic`
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
