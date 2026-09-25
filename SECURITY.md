# Security

What a guest can reach on the host, before and after the display-passthrough
work, and what is still open.

This is the security review of branch `display-passthrough` at `ae182ab`
against `dev` at `50ff74a` (called **dev** below). It is written for the
project's owner. The code is the reference: every count below was taken from
the tree at `ae182ab`, and where this document and the code disagree, the code
is right.

> **Nothing in the display work has run on a GPU or in a VM.** It is built and
> unit-tested: the Rust workspace's tests, and the guest module compiled
> against a 7.2.7 guest kernel. The Wayland proxy has also run real clients
> against a headless sway, with no VM and no GPU
> (`nvgpu-wl-guest/tests/loopback.rs`). Every statement below about what the
> host kernel does with a request -- that nvidia-drm adds an index to a kernel
> mapping without a bound, that RM follows a pointer in the backend -- was read
> from source (NVIDIA's open modules 610.57.04, Linux 7.2.7), not observed. The
> on-device plan is [`TESTING.md`](TESTING.md). The headless numbers in
> [`BENCHMARKS.md`](BENCHMARKS.md) were measured on dev, before any of this.

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

### What is still open

- `RM_CONTROL` and `RM_ALLOC` are still not allow-listed. All but 15 of
  610.57.04's 1,362 control method ids, and every allocation class but 12,
  reach RM, checked only by RM, as a caller without admin rights.
- The host NVIDIA driver is in the TCB. There is no IOMMU boundary between
  guest GPU work and the host.
- The backend is unsandboxed. There is no seccomp, no landlock, no namespace,
  no cgroup and no rlimit, and the per-guest isolate is not built.
- Four review findings are partly fixed, and two verification findings are
  partly fixed or open (§8). §9 lists every open item.
- Memory registered by its pages is released to the guest when its RM handle
  goes, even where UVM or NVKMS still holds it in the host kernel (§3,
  "Memory registered by its pages"): guest memory only, never the host's.

§10 is the order the remaining work should go in.

---

## 2. Threat model

| party | trusted? | why |
|---|---|---|
| **guest kernel and all guest userspace** | **no** | One VM is one trust domain. Anything that protects the host is enforced by the backend from its own tables, never from a layout the guest describes. Separating guest users from each other is the guest kernel's job; the checks the guest module makes (the `/dev/nvgpu-wl` mode, the ACCEPT uid check) hold only while the guest kernel does. |
| **host compositor** (Hyprland, with `patches/`) | yes | It decides what a proxied client may do within the allowlist, and it holds the leases it grants. The backend does not defend against it, but it does not trust it for descriptor types: every descriptor the compositor sends is classified by what the kernel says it is. |
| **export-mode peers** (host programs on the `--wayland-export` socket) | partly | They must have the backend's uid (`SO_PEERCRED`, `device/src/wl/export.rs`), and their messages go through the same allowlist. A program that connects makes the guest compositor its Wayland server, and a server can type into its clients: connect only programs the guest may drive. The backend is undumpable, so a same-uid peer cannot ptrace it. |
| **the backend** (`vhost-user-nvgpu`, one process per VM) | **in the TCB** | It maps all of the guest's RAM, holds every host descriptor the guest uses, and is the caller of every host ioctl. Whatever compromises it has that VM's host files and the backend user's privileges. |
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
| **RM_CONTROL** commands | 1,362 method ids | All, **raw** past the 32-byte outer check. One embedded pointer was relocated, at an offset the guest named, into a heap buffer sized from what the guest sent. Every other embedded pointer reached RM as a guest address, which RM dereferences in the backend. | Still all but 15, and **not allow-listed**. 3 are **refused** (pointers the table cannot name one by one). 12 that list other clients' host PIDs are answered by the backend with RM's own "insufficient permissions" (`device/src/rmctl.rs`). For the 47 whose parameters hold pointers RM follows (measured per release, `gen/src/rmctrl/generated.rs`), each pointer is relocated to a guarded buffer or zeroed. Several of one control go as deep segments, each **table-sized**: its length is computed from the parameters RM is handed, as RM computes it, and must match exactly, at most 1 MiB in all. The ACPI-method controls and four others (`ZEROED_CONTROLS`) are never relocated. REGISTER_WAITER's OS-event descriptor is translated and must name a live event. |
| **RM_ALLOC** classes | 227 distinct numbers in `g_allclasses.h` | All, **raw**. | All but **12, refused** (OS-descriptor memory 0x71 named by address, kernel callbacks 0x78, 0x7e, 0x92 and 0x9010, memory lists 0x81-0x83, FB segments 0xc1, IMEX and fabric memory 0xf1, 0xf9 and 0xfd; `REFUSED_ALLOC_CLASSES`, `device/src/guestptr.rs`). pRightsRequested is zeroed. NV_EVENT_BUFFER must name a live OS event. The rest reach RM **not allow-listed**. |
| **memory named by CPU address** (OS descriptors through RM_ALLOC, ALLOC_MEMORY and VID_HEAP_CONTROL) | 3 paths | **Raw**: RM pinned the backend's pages at a guest-chosen address and mapped them for the GPU. | With an address alone, **refused**. With the guest-physical pages behind it (BCAP_OS_DESC), **table-sized**: only the user-virtual-address descriptor type, a page list covering exactly what RM pins, every page in guest RAM, and RM handed the backend's own mapping of exactly those pages (below). |
| **nvidia-uvm**, `/dev/nvidia-uvm` | 38 commands | All, **raw**. The guest copied 12 KiB each way for every command but the two it knew the size of, and pointers and descriptors went as sent. | At most **33** (30 to 33 per release), **table-sized** on both sides from `gen/uvm/`. Pageable access is forced off at UVM_INITIALIZE, so the GPU cannot fault in the backend's pages, and every file is put in multi-process sharing mode, which takes pageable access away on every release and ties the VA space to no process. The 6 descriptor fields are translated. Every command that copies through, pins or populates CPU memory is **refused**. |
| **nvidia-uvm tools**, `/dev/nvidia-uvm-tools` | 7 | All, **raw**. | **0**: the file opens, and every ioctl on it is **refused**. |
| **NVKMS**, `/dev/nvidia-modeset` | one ioctl carrying 66 commands (610.57.04) | Every command, **raw**, with **no policy**. One descriptor, REGISTER_SURFACE's, was translated at a fixed offset. | 56 to 61 per release, **schema-authoritative** over IOCTL2. v1 carries only the commands with no pointer and no descriptor. 7 are **refused** by name, 3 run only with `--kms-card`, and 7 are gated on grants outside it. Everything else is in §5. At most 64 opens per VM. |
| **nvidia-drm and DRM core on a host render node** | 24 nvidia-drm ioctls (21 render-allowed), plus the core's render-allowed ones | Any `d` ioctl. Three nested GEM calls translated `memFd`; the rest were **raw** in a buffer sized by the guest, while the host copies `_IOC_SIZE` back (a heap overflow in the backend). | v1: 6 full ioctl numbers. IOCTL2: 28 render-class entries (12 syncobj, 16 nvidia-drm), **schema-authoritative**. GEM_IMPORT_USERSPACE_MEMORY, GEM_FLINK and GEM_OPEN are **refused** on every handle. SEMSURF_FENCE_CTX_CREATE's index must lie inside the surface, and its client must be one this VM allocated, with at most 16 contexts per file and 256 per VM (`device/src/semsurf.rs`). Every argument buffer is at least `_IOC_SIZE` and guarded. |
| **DRM KMS on a host card or lease file** | the KMS core | None: no such file existed. | 49 KMS-class entries, **schema-authoritative**, only on card handles (`--kms-card`) and lease handles. See §5. |
| **HOST_OP** (backend-made host calls on the guest's behalf) | -- | None. | 11 ops, each argument checked against the handle kind it must be: PRIME export and import on render files, sync_file merge (at most 5), eventfd, a signalled sync_file (by `/dev/udmabuf` when needed), a syncobj wait registration (at most 1,024 per VM), fd kind, close-many, and OPEN_KMS and DROP_IF_MASTER, which are `--kms-card` only. OSDESC_REAP calls nothing on the host: it reads which registrations of guest memory RM has let go of. |
| **mmap** | per device | Any handle. A UVM file went to the window, where the VMM's mmap of it failed and closed the window's request channel for the rest of the VM. | Device, render, card and lease handles only. Each placement carries the host's memory type and whether it is writable, so a read-only host page is mapped read-only in the guest. A UVM file maps only a semaphore pool the same file was seen to create, asked for exactly, into the UVM aperture (below); anything else on it is **refused** before the VMM is asked. |
| **any other ioctl type** | -- | **Raw**, to whatever host file the handle was. | **Refused** (EPERM). |

What the dev column shows is that a guest's reach into the host driver on dev
was bounded mostly by what the guest's own libraries happened to send.

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
  replacing anything. Two of the guest's own pools
  at one address are refused by the backend before the VMM is asked, so a
  failure never tells the guest anything about the VMM's layout.
- **No descriptor kept.** The UVM file travels to the VMM on the vhost-user
  request channel and its copy is closed when the request returns; the
  mapping's own file reference is all the VMM holds, and it goes with the
  mapping.
- **No slot over nothing.** The VMM checks the pages are present before it
  adds the slot, removes the slot before the mapping, and UVM refuses to free
  a pool that is still mapped. An aperture address with no slot reads zeros
  and ignores writes, in the VMM, never as a fault the host has to resolve.
- **Bounded.** Each pool at most 64 MiB; 16 placements and 64 MiB per UVM
  file; 64 placements and 256 MiB per VM; 256 recorded pools per file and
  4,096 per VM; an aperture of at most 1 GiB. Every placement goes when its
  last guest mapping does, when its file closes, and on a guest reboot or
  device reset; if the backend goes away, the VMM drops them all itself.

Sharing mode only takes things away (pageable access, the tie to the
backend's mm), no host driver change is needed, and the VMM's seccomp filter
already allows the calls involved (`mmap`, `mincore`, `ioctl`).

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
- **Pinned on both sides until RM lets go.** The guest keeps its pins until
  the backend reports the registration released, and the backend's range
  stays mapped until then: RM freed the object, its parent or its client
  (the backend frees a client holding one itself, on its own file, before
  that file closes), or the session ended. A duplicate made with DUP_OBJECT
  holds it too. A free the backend cannot see (an ancestor above the parent)
  makes the release late, never early.
- **Bounded.** 4,096 registrations per VM, released ones the guest has not
  yet read included, and 1,024 per guest file; 16 GiB per VM and 4 GiB per
  file; 32,768 separately mapped runs per VM, each a mapping of the
  backend's.

What it does not cover: a reference that only the host kernel holds. UVM
keeps its own duplicate of memory it maps as an external allocation, and
NVKMS of memory registered as a surface; RM keeps the pages pinned for those,
but the backend sees only the guest's handle, and reports the registration
released when that handle is freed. The guest then unpins, and pages it
reuses stay reachable by that GPU mapping until the process that made it
unmaps it or exits. Those are guest pages, never the host's: the host is
unaffected, but a guest process could reach memory its own kernel has
reused. Closing that needs the backend to follow those references too (or
refuse them for registered memory). A registration abandoned in flight (a
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
| device files | `/dev/nvidia*`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset`, `/dev/dri/renderD*` | the same, plus `/dev/dri/card*` (`--kms-card` only, through OPEN_KMS; a plain OPEN of a card is refused), lessee files received from the compositor, which must classify as a lease of this GPU, and `/dev/udmabuf` |
| other files | `/proc/driver/nvidia`, PCI config in sysfs | the same, plus memfds for shm pools, blobs and a sealed page, and readlink of `/proc/self/fd` |
| vhost-user socket | default `/tmp/nvgpu.sock`, and a failed unlink was ignored, so another user could bind it first and receive the guest's memory | default `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` in a 0700 directory, refused if the directory is anyone else's; a file already at the path is removed only if it is a socket of the backend's uid, and anything else stops the start (`device/src/posture.rs`) |
| other sockets | none | with `--wayland-socket`, one connection to the compositor per channel plus a probe connection; with `--wayland-export`, a listener created 0600 that admits only peers of the backend's uid, with 16 pending |
| netlink | none | `NETLINK_KOBJECT_UEVENT`, receive only, with `--kms-card` or `--wayland-lease` |

### Privileges

| | dev | HEAD |
|---|---|---|
| identity | whoever started it. The shipped `scripts/run-guest.sh` ran it as root, which makes every guest process an RM administrator: all of BAR0 mappable read-write, the register allowlist skipped, and DRM files authenticated. | Refuses to start with euid 0 or CAP_SYS_ADMIN unless `--allow-root-unsafe`. The launcher starts it through `setpriv` as the system user `nvgpu`, in the groups video, render and kvm, with no capabilities and no_new_privs. With `--wayland-socket` it runs as the socket's owner (the desktop user), and with `--wayland-export` as the owner of the export socket's directory. |
| capabilities | whatever it was given | All dropped before the first thread exists (effective, permitted, inheritable, ambient, and the bounding set where it may), then no_new_privs, undumpable, umask 077. This holds under `--allow-root-unsafe` too, and RM decides administrator by `capable(CAP_SYS_ADMIN)` (`nv-linux.h`), so even then RM sees none. What root keeps is file access by uid. |
| sandbox | none | **none**: no seccomp, landlock, namespaces, cgroup or rlimits |

### Threads and resource caps

| | dev | HEAD |
|---|---|---|
| threads | the transport, and one event pump that polled every open handle every millisecond | the transport; one pump, which sweeps only handles it has reported readable; up to 16 executors; one closer thread for display files; one reader per Wayland channel (at most the channel cap, 64 by default); short-lived close threads; one export accept thread and one hotplug thread in those modes |
| handles | no cap but RLIMIT_NOFILE | 65,536 per VM |
| RM counters | one map entry per distinct guest value, unbounded | counted only when RM said NV_OK, at most 4,096 keys (`device/src/tally.rs`) |
| logs | unbounded; the launcher wrote them to an unrotated file | every call site limited to a burst of 50 and 10 a second (`device/src/ratelimit.rs`) |
| display caps | -- | 64 NVKMS opens; 1,024 syncobj wait registrations; semaphore-surface contexts at 16 per file and 256 per VM; 4 KiB of undelivered DRM events per handle, past which the host's own backpressure applies |
| Wayland caps | -- | 64 channels per VM. Shm: 1 GiB and 1,024 pools per VM, and 512 MiB and 256 pools per connection; the bytes are what live buffers cover (page-rounded, overlaps once), not pool sizes, since a pool's memfd is sparse, SHM_SYNC writes only inside a live buffer, and pages no live buffer covers are punched out. Unread output: 256 MiB per VM and 64 MiB per connection. 16 unfinished blobs per connection. 131,072 objects per connection. Lease submits: one per 5 s on average, 3 at once. Four are flags: the channel count (`--wayland-max-conns`), the shm byte budget (`--wayland-shm-budget`), the queue budget (`--wayland-queue-budget`) and the lease interval (`--wayland-lease-interval`). The 1,024 pools per VM and the burst of 3 are fixed. |
| not capped | -- | RLIMIT_NOFILE is inherited and never set, and there is no shared descriptor budget. Memory outside the Wayland budgets has no limit, and no cgroup. |

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
| `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` | 0666 | 0666, unchanged: any guest user reaches everything §3 lists |
| `/dev/nvidia-caps/*` | 0444 | 0444 |
| DRM node | a hand-made character device, 0666 | a real DRM device per host render node, whose render and primary nodes the guest's DRM core makes. Syncobjs are enabled only when the backend serves fences, and the primary node drives KMS only when the backend offers `--kms-card`. |
| `/dev/nvgpu-wl[N]` | -- | root:root 0660 by default (module parameter `wl_mode`), with `scripts/70-nvgpu-wl.rules` giving it to group `nvgpu-wl` for the daemon. Four ioctls: HELLO, CONNECT, SEND and RECV. One LISTEN per device, and ACCEPT only from the listener's effective uid or CAP_SYS_ADMIN. |
| adopted DRM files | -- | a lease received from the host becomes a guest DRM file, cloned from a card-node file |
| `nvgpu-wl-guest` | -- | a daemon listening at `$XDG_RUNTIME_DIR/wayland-0` in the guest |

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
- which UVM pools the VMM maps into the aperture, at what address, and how
  many;
- the host-PID answers;
- the Wayland allowlist, descriptor classes and budgets;
- every cap in §4;
- its own privileges and socket.

**It leaves to the host:**

- **RM's checks on every control and class it forwards**, at user rather than
  admin privilege. This is the largest part.
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
adversarial verifiers who tried to refute it:

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
fix commit and the code.

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
| S-5 | high | a root backend makes every guest process an RM administrator | `b3c126b`, `ae182ab` | fixed; refusing class 0x3f and forcing non-privileged RM clients, not done |
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

1. **RM_CONTROL and RM_ALLOC are not allow-listed.** Every control but 15 and
   every class but 12 reach RM. RM checks them as it would a user process
   without admin rights. This is the largest GPU-side surface, and apart from
   the pointer, class and PID handling it is the same as dev. The instrument
   an allowlist would be written from exists: counts of what RM served, per
   control and class (`device/src/tally.rs`). No workload has been counted
   since the counts were fixed.
2. **The host NVIDIA driver is in the TCB.** There is no IOMMU boundary. A bug
   in any forwarded path, or in RM's page tables for a guest's GPU work, is a
   host bug. For mutually untrusted tenants, VFIO with an IOMMU or vGPU is
   still the answer.
3. **The backend is unsandboxed.** One process per VM maps all of that
   guest's RAM, holds every host descriptor, and parses everything in §4.
   - With `--wayland-socket` or `--wayland-export` the launcher runs it as the
     desktop user, so a compromised backend is that user.
   - In the default mode every VM's backend is the same user, `nvgpu`, so one
     can signal another.
   - No seccomp, landlock, namespace, cgroup or rlimit limits any of this.
4. **The isolate is not built.** `isolate/README.md` describes one helper per
   guest process holding the descriptors. The display paths share objects
   across a VM's processes (a compositor holds its clients' buffers) through
   one VM-wide handle table, so a helper per VM is probably the first one to
   build. That is reasoned, not tried.
5. **Descriptors and memory.** RLIMIT_NOFILE is inherited, never set, and
   usually far below the 65,536-handle cap. Running out shows up as EMFILE in
   whichever path hits it first, for the whole VM. The Wayland budgets bound
   shm and queues; nothing bounds the backend's memory as a whole.
6. **Open items after the fixes.**
   - **Backend posture.** Class 0x3f is not refused, and non-privileged RM
     clients are not forced, which matters only if the backend is ever given
     CAP_SYS_ADMIN. There is no `--socket-fd`. PID namespaces are left to the
     launcher. EXPORT_TO_DMABUF_FD is refused, so NVIDIA's GBM RM export path
     and `cuMemGetHandleForAddressRange(DMA_BUF)` fail.
   - **Untranslated descriptors.** Some RM controls carry a descriptor that
     nothing translates: CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL's devDescriptor, the
     NV00E0 export and NV00FD ATTACH_GPU devDescriptors, and NV00FD
     REGISTER_EVENT's pOsEvent. The guest's number reaches RM as a number in
     the backend's table. The fabric classes are refused, which keeps this low
     rather than none.
   - **UVM_INITIALIZE.** The guest driver writes the forced flags back into the
     caller's memory: a difference from native, not a host exposure.
   - **The UVM aperture.** Its safety rests on the VMM doing what §3 says:
     `MAP_FIXED_NOREPLACE`, the page check, slot before mapping, and reading
     an unslotted aperture address as nothing. The backend cannot see any of
     it. Two guest processes whose pools overlap cannot both have one mapped,
     so the second CUDA context fails. ALLOC_SEMAPHORE_POOL's length still
     sizes a host kernel allocation with no cap of the backend's (a pool over
     64 MiB is only never mapped).
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
     - No per-owner channel count in the guest.
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
11. **Registered guest memory outlives its handle in the host kernel.** A
    UVM external mapping or an NVKMS surface made from memory registered by
    its pages keeps RM's pin after the guest's handle is freed, and the
    backend, which follows only RM handles, then tells the guest to unpin
    (§3, "Memory registered by its pages"). Guest memory only: a guest
    process could reach pages its own kernel has since reused, never the
    host's. None of it has run on hardware.

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
3. **Set RLIMIT_NOFILE at start**: raise the soft limit to the hard one, log
   it, and size one per-VM descriptor budget from it. The handle table, blobs,
   pools, streams and adopted compositor files would all draw from that
   budget, and MAX_HANDLES would come from it rather than a constant (S-16,
   S-7).
4. **A PID namespace and a user per backend** (`PrivatePIDs=yes`, or `bwrap
   --unshare-pid`, and a uid per VM). With the namespace, RM filters the
   host-PID controls itself; `rmctl.rs` then stays as a second fence. With a
   uid per VM, one backend cannot signal another. For the Wayland modes, give
   the backend a `wp_security_context_v1` socket from the desktop session
   rather than running it as the desktop user. Hyprland then filters globals
   for those clients itself (`filterGlobals`, `src/Compositor.cpp`). Its list
   lacks three of this proxy's additions: the lease device, xdg-output and
   content-type. It already has the syncobj manager (and `wl_drm`, which this
   proxy leaves out). Leasing would need a Hyprland patch; explicit sync would
   not.
5. **Allow-list RM controls and classes from the counters.** Run the Vulkan,
   NVENC, CUDA and display workloads on each supported release, collect
   `rm_controls` and `rm_classes`, and generate per-release tables the way
   `gen/rmctrl`, `gen/uvm` and `gen/nvkms` are generated. Refuse the rest. This
   is the largest reduction left on the GPU side. Its risk is breaking a path
   the counted workloads did not take, which is why item 1 comes first.
6. **seccomp and landlock per backend.** The syscall set is small and known:
   ioctl, mmap, epoll, sendmsg and recvmsg, memfd_create, and a few more.
   Landlock can confine opens to `/dev/nvidia*`, `/dev/dri/*`, `/dev/udmabuf`,
   `/proc/driver/nvidia` and sysfs, plus the compositor and vhost-user
   sockets. Guest opens happen at run time, so these paths stay open for the
   whole run, and the policy is installed after start-up.
7. **The isolate**: the host descriptors held by an unprivileged helper apart
   from the process that maps guest RAM, so that a parser bug in the backend no
   longer yields the descriptors, and a driver-call bug no longer yields guest
   memory. Probably one per VM (§9, item 4).
8. **The rest of §9, item 6.** In rough order:
   - one NVKMS and KMS executor lane per VM, and a lower ALLOC_DEVICE cap;
   - per-interface Wayland object caps and a CONNECT rate;
   - the Hyprland lease debounce, and `setLeaseOffered(false)` ending an
     active lease;
   - refusing class 0x3f and forcing non-privileged RM clients, as a second
     fence against a privileged backend;
   - translating EXPORT_TO_DMABUF_FD, once something needs it.
