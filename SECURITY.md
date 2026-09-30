# Security

What a guest can reach on the host, what holds it there, and what is still
open.

**As of `492f29b` (branch `integrate30`), 2026-09-29.** Part I is the
current state and is normative. Part II is history and reference: the review
rounds (Appendix A), the comparison with the `dev` branch this work started
from (Appendix B), and the index of every finding (Appendix C).

---

# Part I: the current state

## 1. Status and how to read this

This is written for the project's owner. The code is the reference: where
this document and the code disagree, the code is right, and the document is
the thing to fix.

- **Findings.** Each review finding has an id (S-1, R3, FB-5, BE-1.7, ...).
  Its status is stated once, in Appendix C, with the fix commits and the
  appendix that tells the story. Part I cites ids and does not repeat a
  status.
- **Sources.** Statements about what the host kernel does with a request were
  read from source (NVIDIA's open kernel modules 610.57.04, Linux 7.2.7)
  unless a section says it was observed.
- **Hardware.** `rig/TESTING-RIG.md`, "Where the runs stand", is the dated
  record of every run, and README.md, "What is known to work", the summary.
  The last regression on hardware ran on an RTX 5090 with 595.99.02 on
  2026-09-29, on `492f29b` (the 2026-09-29 review's fixes and the
  restructuring after them), under nesbox with the C and the Rust parsers
  and under crosvm, with the backend's sandbox on and the RM allowlist
  enforcing. Never run on hardware: a root run of the launcher, the
  socket-activated units, the compositor-VM and export
  modes (they need the host desktop stopped) and hotplug. "How the claims
  are tested" says what each claim rests on.

## 2. Summary

A guest reaches the host only through the backend, one process per VM, and
through its VMM. This is attack-surface reduction in front of the host's
NVIDIA driver, not hardware isolation: the host driver and the GPU's MMU are
in the trusted computing base ("Threat model").

- **The host's NVIDIA driver**, only as the backend's own tables allow ("The
  host GPU surface"). RM escapes are size-checked against a profile chosen
  at start. RM controls and classes are on a default-deny list per release
  ("The RM allowlist"). NVKMS and DRM calls go through generated schemas.
  Every pointer field the backend's tables name is relocated to a buffer
  of the backend's or zeroed, and every descriptor field they name becomes
  the backend's own; what the tables miss reaches the host as the guest's
  bytes ("Memory safety", open item 11).
- **Compute paths** -- UVM, the UVM aperture, memory registered by its
  pages -- only with `--allow-compute` ("Compute is opt-in").
- **The host display and compositor**, only in the display modes the
  operator turns on ("Inside the guest", "NVKMS, KMS and leases", "The host
  desktop").
- **The backend**, which parses everything the guest sends. It is sandboxed
  before the first guest message and, run as root, is a host user of its own
  per VM ("The backend process", "Deployment").
- **The VMM**, through the device's PCI function and the backend's mapping
  requests, which each VMM checks itself ("The VMMs").
- **Other guest processes' RM objects**, by duplicate, share or name, only as
  RM would allow between two host processes ("RM objects between guest
  processes").

What is still open, in brief ("Open items and residual risk" has the whole
list):

- `RM_CONTROL` and `RM_ALLOC` are allow-listed per release, default deny, and
  enforcing by default: 216 of 610.57.04's 1,370 controls (plus 30 GSP
  pass-through numbers seen on hardware) and 97 of its 222 classes reach RM.
  The list was built from 106 hardware runs and NVIDIA's sources, and has run
  enforcing through the rig's regression and the application pass (Vulkan,
  GL, EGL, CUDA, NVENC, NVDEC, Vulkan Video, VA-API, OpenCL). A workload it
  still misses breaks with RM's "not supported" and a log line naming the
  call. What it allows is still host surface.
- The host NVIDIA driver is in the TCB. There is no IOMMU boundary between
  guest GPU work and the host.
- The backend's sandbox -- a network namespace, Landlock, a seccomp allowlist
  -- does not touch the GPU's nodes or the host kernel the backend still
  calls. The shipped unit gives each backend a cgroup (`MemoryMax`,
  `TasksMax`; [`DEPLOY.md`](DEPLOY.md)); the rig's launcher has one scope
  for the whole run (`NVGPU_MEMORY_MAX`). The per-guest isolate is not
  built.
- One VM's budgets are split among its guest processes ("Resource caps"),
  but a guest process that forks is a new process with a share of its own:
  the guest's own process limits are what bound a forking app.
- Memory registered by its pages is released to the guest only when no
  holder the backend knows of in the host kernel still has it. The holders
  were found by reading the 610 sources, not by a run; one missed would
  reopen a guest privilege escalation.

"Roadmap" is the order the remaining work should go in.

## 3. Threat model

| party | trusted? | why |
|---|---|---|
| **guest kernel and all guest userspace** | **no** | One VM is one trust domain. Anything that protects the host is enforced by the backend from its own tables, never from a layout the guest describes. Separating guest users from each other is the guest kernel's job; the checks the guest module makes (the `/dev/nvgpu-wl` mode, the ACCEPT uid check) hold only while the guest kernel does. |
| **host compositor** (Hyprland, with `patches/`) | yes | It decides what a proxied client may do within the allowlist, and it holds the leases it grants. The backend does not defend against it, but it does not trust it for descriptor types: every descriptor the compositor sends is classified by what the kernel says it is. |
| **export-mode peers** (host programs on the `--wayland-export` socket) | partly | They must have the backend's uid (`SO_PEERCRED`, `device/src/wl/export.rs`), and their messages go through the same allowlist. A program that connects makes the guest compositor its Wayland server, and a server can type into its clients: connect only programs the guest may drive. The backend is undumpable, so a same-uid peer cannot ptrace it. |
| **the backend** (`vhost-user-nvgpu`, one process per VM) | **in the TCB** | It maps all of the guest's RAM, holds every host descriptor the guest uses, and is the caller of every host ioctl. Whatever compromises it has that VM's host files and what the backend's sandbox leaves it ("The backend's sandbox"): the GPU's nodes, a few read-only files, the syscalls on its list, as a uid of that VM's own when run as root. |
| **the VMM** (nesbox, or crosvm with `patches/crosvm/`) | yes, with the guest's memory; not for what the backend asks of it | It hands the backend the guest's memory. It also carries out the backend's mapping requests (vhost-user `SHMEM_MAP`/`SHMEM_UNMAP`) and maps UVM pools into its own address space, and each VMM checks those requests against the window and the aperture itself ("The VMMs"). The guest reaches it through the device's PCI function. |
| **the host NVIDIA driver** (nvidia, nvidia-uvm, nvidia-modeset, nvidia-drm, GSP firmware) | **in the TCB** | The card is in the host's IOMMU domain. What keeps a guest's GPU work in its own memory is the GPU's MMU, with page tables RM programs for it. |
| **the host kernel** (DRM core, syncobj, dma-buf, memfd) | yes | The backend relies on its checks for everything "What the backend enforces, and what it leaves to the host" does not list. |
| **the capture helper** (`--inject-uid`, only with `--inject-socket`) | for consent, and nothing else | It injects only what the user agreed to share with this VM; the backend parses everything it sends as hostile ("Capture injection"). |

Not addressed: timing side channels between tenants on a shared GPU, a guest
saturating the GPU (scheduling is the host driver's), and physical access.

---

## 4. The host GPU surface

What a guest can make the host's NVIDIA driver run, by device and namespace.
"Host has" counts are from the 610.57.04 sources. The last column says how
each call is validated:

- **raw**: forwarded as sent;
- **size-checked**: the length is checked against the backend's profile;
- **table-sized**: the length is exact, from a per-release table;
- **schema-authoritative**: every pointer, length, descriptor and GEM field is
  recomputed from the backend's own copy against a generated schema;
- **refused**: the host is never called.

| entry point | host has | what reaches the host |
|---|---|---|
| **RM escapes**, type `F`, on `/dev/nvidiactl` and `/dev/nvidiaN` | -- | Only on GPU and control handles (`v1_route`, `device/src/nvidia/v1.rs`). The profile is chosen at start from `/proc/driver/nvidia/version`. **21 of 23 / 22 of 24** reach the host, **size-checked** except the three variable-length ones (CARD_INFO, ATTACH_GPUS_TO_FD, NUMA_INFO), which pass with no size check: EXPORT_TO_DMABUF_FD is **refused**, and IDLE_CHANNELS goes for one channel with its three array pointers zeroed, or for a list of at most 4,096 with the arrays as deep segments the backend sizes itself (a list without them is **refused**). XFER_CMD, I2C_ACCESS, ACCESS_REGISTRY, GET_EVENT_DATA and ADD_VBLANK_CALLBACK are **refused** under any ABI policy, `--permissive-abi` included. Pointer fields in the top-level blocks are zeroed. |
| **RM_CONTROL** commands | 1,370 controls | **Allow-listed** per release ("The RM allowlist"): 216 of 1,370 (and 30 GSP pass-through numbers seen on hardware) reach RM; the rest are answered NOT_SUPPORTED, or INVALID_PARAM_STRUCT for a size other than RM's, without RM. 3 controls whose pointers the tables cannot name one by one are **refused** whatever the list says (none is on it). 12 that list other clients' host PIDs are answered by the backend with RM's own "insufficient permissions" (`device/src/rmctl.rs`). For the 47 whose parameters hold pointers RM follows (measured per release, `gen/src/rmctrl/generated.rs`), each pointer is relocated to a guarded buffer or zeroed. Several of one control go as deep segments, each **table-sized**: its length is computed from the parameters RM is handed, as RM computes it, and must match exactly, at most 1 MiB in all. The ACPI-method controls and four others (`ZEROED_CONTROLS`) are never relocated. REGISTER_WAITER's OS-event descriptor is translated and must name a live event. NV0000's OS_UNIX controls (0x3dxx): the six that name a control file by descriptor (export, import, export info) get the backend's descriptor of the caller's own control file, and any other number is **refused** (EBADF), as is a descriptor field too short to hold; MEMACCT's cgroup descriptor and every OS_UNIX command RM does not define are answered NOT_SUPPORTED without RM (R1). |
| **RM_ALLOC** classes | 227 distinct numbers in `g_allclasses.h` | **Allow-listed** per release ("The RM allowlist"): 97 of 222 reach RM, on RM_ALLOC, ALLOC_MEMORY, ALLOC_OBJECT, ALLOC_CONTEXT_DMA2 and by VID_HEAP_CONTROL function; the rest are answered INVALID_CLASS without RM. **12 are refused** on RM_ALLOC, ALLOC_OBJECT and ALLOC_CONTEXT_DMA2 whatever the list says (OS-descriptor memory 0x71 named by address, kernel callbacks 0x78, 0x7e, 0x92 and 0x9010, memory lists 0x81-0x83, FB segments 0xc1, IMEX and fabric memory 0xf1, 0xf9 and 0xfd; `REFUSED_ALLOC_CLASSES`, `device/src/guestptr.rs`), and ALLOC_MEMORY refuses the four of them whose `pMemory` RM reads (`REFUSED_ALLOC_MEMORY_CLASSES`). An NV01_EVENT is held to both as the subclass its parameters name, which RM allocates in its place (`rm_alloc_class`). pRightsRequested is zeroed. NV_EVENT_BUFFER must name a live OS event and a header buffer of the caller's (`hBufferHeader`): one RM allocates itself comes back with its pages' host physical addresses. |
| **RM_SHARE, RM_DUP_OBJECT, and a second client named in parameters** | NV04 share and dup; 2 NV0000 share controls; 7 classes and 21 controls that name another client | Shares go to RM only when they narrow or grant inside the VM; the rest are **refused**. A duplicate's two clients must be this VM's, made by one guest process, unless the source was shared with the destination (RM's rule, guest processes for the backend's). A second client named in class or control parameters must be this VM's and pass RM's rule for that field, with guest processes and euids. A guest that does not say which process and euid make each call gets neither. Below, "RM objects between guest processes". |
| **memory named by CPU address** (OS descriptors through RM_ALLOC, ALLOC_MEMORY and VID_HEAP_CONTROL) | 3 paths | With an address alone, **refused**. Without `--allow-compute`, with pages too. With it, and the guest-physical pages behind it (BCAP_OS_DESC), **table-sized**: only the user-virtual-address descriptor type, a page list covering exactly what RM pins, every page in guest RAM, and RM handed the backend's own mapping of exactly those pages (below). |
| **nvidia-uvm**, `/dev/nvidia-uvm` | 38 commands | Without `--allow-compute` (the default), **0**: the open is **refused** before the host is asked, and the guest has no node. With it, **31** of 610.57.04's 38 (35 to 37 on the releases before 610 measured: 37 on 580 through 595.99.02, the rig's; 31 on 610.43.02), **table-sized** on both sides from `gen/uvm/`. Pageable access is forced off at UVM_INITIALIZE, so the GPU cannot fault in the backend's pages, and every file is put in multi-process sharing mode, which takes pageable access away on every release and ties the VA space to no process. The 6 descriptor fields are translated. Every command that copies through, pins or populates CPU memory is **refused**, and so are the two whose effect is the host's: TOOLS_FLUSH_EVENTS (the host-wide tools queue) and CLEAR_ALL_ACCESS_COUNTERS (the GPUs' own counters, which every tenant's migrations read). |
| **nvidia-uvm tools**, `/dev/nvidia-uvm-tools` | 7 | **0**: without `--allow-compute` the open is **refused**; with it the file opens, and every ioctl on it is **refused**. |
| **NVKMS**, `/dev/nvidia-modeset` | one ioctl carrying 66 commands (610.57.04) | 56 to 61 per release, **schema-authoritative** over IOCTL2. v1 carries only the commands with no pointer and no descriptor. 7 are **refused** by name, 3 run only with `--kms-card`, and 7 are gated on grants outside it. Everything else is in "NVKMS, KMS and leases". At most 64 opens per VM, and 16 per guest process (the VM's last 8 kept for processes holding at most 2). |
| **nvidia-drm and DRM core on a host render node** | 24 nvidia-drm ioctls (21 render-allowed), plus the core's render-allowed ones | v1: 6 full ioctl numbers. IOCTL2: 28 render-class entries (12 syncobj, 16 nvidia-drm), **schema-authoritative**. GEM_IMPORT_USERSPACE_MEMORY, GEM_FLINK and GEM_OPEN are **refused** on every handle. SEMSURF_FENCE_CTX_CREATE's index must lie inside the surface, and its client must be one this VM allocated, with at most 64 contexts per file, 96 per guest process and 256 per VM, the last 32 kept for processes holding at most 16 (`device/src/semsurf.rs`). Every argument buffer is at least `_IOC_SIZE` and guarded. |
| **DRM KMS on a host card or lease file** | the KMS core | 49 KMS-class entries, **schema-authoritative**, only on card handles (`--kms-card`) and lease handles. See "NVKMS, KMS and leases". |
| **HOST_OP** (backend-made host calls on the guest's behalf) | -- | 13 ops, each argument checked against the handle kind it must be: PRIME export and import on render files, sync_file merge (at most 5), eventfd, a signalled sync_file (by `/dev/udmabuf` when needed), a syncobj wait registration (at most 1,024 per VM), fd kind, close-many, OPEN_KMS and DROP_IF_MASTER, which are `--kms-card` only, and INJECT_OPEN and INJECT_OPEN_SYNCOBJ, which need `--inject-socket` and a live id's token ("Capture injection"). OSDESC_REAP calls nothing on the host: it reads which registrations of guest memory RM has let go of. No export leaves the backend of a live fence context, an injected object or a handle INJECT_OPEN made (`device/src/exportgate.rs`, asked by PRIME export, the Wayland proxy and the IOCTL2 re-home). |
| **mmap** | per device | Device, render, card and lease handles only. Each placement carries the host's memory type and whether it is writable, so a read-only host page is mapped read-only in the guest. A UVM file maps only a semaphore pool the same file was seen to create, asked for exactly, into the UVM aperture (below); anything else on it is **refused** before the VMM is asked. |
| **any other ioctl type** | -- | **Refused** (EPERM). |

### The RM allowlist

A default-deny list of the RM controls and classes a guest may reach, per
host release (`gen/rmallow_extract.py`, `gen/src/rmallow/`,
`device/src/rmallow.rs`).

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
   backend is never one (S-5).
3. It names no host resource the backend does not translate: every field of
   the parameters, nested structs included, that is a descriptor, a process
   id or an OS event refuses the control unless the backend translates it
   (the six OS_UNIX file controls, REGISTER_WAITER). Plus a short list: the
   host-PID controls (answered by `rmctl.rs` as before), ACPI methods,
   IMEX subscription, cgroup limits.
4. A workload asks for it: observed across the rig's 106 runs (all 156
   controls and 33 classes), the application pass and the heavy workloads
   (FIFO_DISABLE_CHANNELS, below); every user-callable control of an object the
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
| 535.129.03 | 212 / 1,132 | 68 / 145 |
| 580.178.04 | 216 / 1,357 | 94 / 209 |
| 595.71.05, 595.99.02 | 216 / 1,349 | 94 / 209 |
| 610.57.04 | 216 / 1,370 | 97 / 222 |
| 615.71.09 | 218 / 1,389 | 97 / 224 |

Each release also allows the 30 observed controls RM passes to GSP-RM with
no CPU-side table (below); the backend's start-up line counts them in (246
of 1,349 controls on 595.99.02). Of the ~750 controls RM would serve an
unprivileged caller in 610.57.04, the list keeps 216:
`NV2080_CTRL_CMD_GPU_SET_POWER`, `GPU_EXEC_REG_OPS`,
`PERF_RATED_TDP_SET_CONTROL` and several hundred more that any host user
process may call are refused. A host between two releases uses the older
list; RM's exact parameter size is held only on a release measured exactly.

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
sources. Only the 28 GSS legacy and 2 BINAPI numbers observed are allowed.
Six of them, added from the application pass (Appendix A.3), carry the size
measured on 595.99.02 (`GSS_LEGACY_SIZES` in `gen/rmallow_extract.py`), and
the backend refuses any other size there; on other releases they have none,
as the rest.

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

A refusal the driver does not check fails a workload later, not at the
call: the one below is how that was found.

**FIFO_DISABLE_CHANNELS.** `NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS` is
allowed, with a gate of its own (`device/src/rmchan.rs`). NVIDIA's Vulkan
driver stops its own channels with it around queue set-up and, in Blender,
around each frame it starts, and goes on the same way whatever RM answers.
Refused, Blender's Vulkan backend hung in 10 of 24 guest runs; natively,
the same refusal made by an LD_PRELOAD shim hung 5 of 14, a success
answered without the call 4 of 8, and the call forwarded none of 8
(BENCHMARKS.md, "Heavy workloads", has the table). Every field:

| field | handling |
|---|---|
| `hClientList[numChannels]` | each client must be the calling guest process's own (`rmshare.rs`, `CONTROL_CLIENT_LISTS`, `Rule::Process`); another VM's or another guest process's is refused with RM's NV_ERR_INSUFFICIENT_PERMISSIONS, RM not called. GSP-RM checks no token here (open-gpu-kernel-modules 595.99.02, src/nvidia/src/kernel/gpu/fifo/kernel_fifo_ctrl.c `subdeviceCtrlCmdFifoDisableChannels_IMPL` sends the parameters on as they are), so natively a process could name any client whose handle it knew |
| `numChannels` | at most 64 (the lists' length); more refuses the call whole (NV_ERR_INVALID_ARGUMENT) |
| `hChannelList[numChannels]` | looked up by RM under the matching client, so only the caller's own channels |
| `pRunlistPreemptEvent` | must be NULL, else NV_ERR_INSUFFICIENT_PERMISSIONS without RM: RM takes it only from a kernel client and refuses a user one the same way |
| `bDisable`, `bOnlyDisableScheduling`, `bRewindGpPut` | flags on the caller's own channels; forwarded |
| the size | 536 bytes, measured in every release; the allowlist holds it on a release measured exactly and the gate on every host |

The control never reaches RM inside a DEFERRED_API bundle: RM defers only
its own few, and the allowlist refuses a bundle of anything else.

What it costs other tenants: a disable without `bOnlyDisableScheduling`
preempts the channels off the GPU, a preemption of the runlist they share
with every other VM and host program, which natively any process may make as
often as it likes. So the gate has a rate: a token bucket per guest process
(50 calls a second after a burst of 40) and one per VM (200 a second after
160), a process charged for every call that reaches the rate, one the
VM's bucket then refuses included (a call refused for its size, its event
or its clients takes no token). The VM's last 40 tokens go only to a
process that has used fewer than 8 of its own, so processes asking past their rate cannot take the calls of one that
makes a few; it takes four processes at their whole rate to reach the VM's.
A call over the rate is answered NV_ERR_NOT_SUPPORTED, RM not called: what
CPU-RM answers for this control on a GPU without GSP, and what the driver
was seen to go on from. The busiest workload measured natively makes a
small fraction of it (BENCHMARKS.md, "Heavy workloads"); a process refused
for its rate may hang as Blender did, so the rate is for a guest that
misuses the call, not for one that uses it. As with the per-process
shares ("Resource caps"), the gate tells apart only the processes the guest
kernel does: a process that forks gets a bucket a child, and enough
children take the VM's rate from its other processes -- a denial within the
VM that the guest's own process limits bound. Against other VMs the VM's
bucket holds whatever the guest does.

gVisor's nvproxy (its master at `5f20848`, 2026-09-30) has allowed the control
since its first ABI, 535.104.05, to every container with the compute or
utility capability (pkg/sentry/devices/nvproxy/version.go,
frontend.go `ctrlSubdevFIFODisableChannels`): it takes RM's size exactly
and refuses a non-NULL `pRunlistPreemptEvent` (EINVAL), and checks neither
the clients the lists name nor a rate. Both of those are the backend's
alone.

### RM objects between guest processes

RM keeps a user client's objects to the process that made it. Its one
default share policy is `RS_SHARE_TYPE_PID` for DUP_OBJECT
(`serverInitGlobalSharePolicies`, `sharing.c`), and a PID policy matches
when the source client's ProcID is the duplicating client's
(`cliresShareCallback`; the policy's `target` is never read). Every RM call
a guest makes is the backend's, so all of a VM's clients were one process to
RM: any guest process could duplicate any other's objects by handle, and a
share a guest made RM applied to the host. The backend now judges three
things before RM sees the call (`device/src/rmshare.rs`):

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
  grants in them, are recorded, at most 4,096 together a session and a
  quarter of that per guest process; past that a share is refused, a revoke that would start a list included.
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
fields is RM's NV_ERR_INVALID_ARGUMENT in the status (BE-2.6).

Only the guest kernel knows which guest process makes a call, and as whom.
The guest module says, when the backend asks for it in HELLO
(`BCAP_PROC_ID`, `BCAP_PROC_EUID`): 16 bytes after the blocks of every
RM_ALLOC, RM_DUP_OBJECT and RM_CONTROL, the calling thread group's leader by
its PID in the initial namespace and its start time, and the caller's
effective uid in the initial user namespace -- what RM's token holds for a
host process (`os_get_euid`; RM never reads the fsuid). The pair is one
process for the guest's lifetime, whatever PID namespace it runs in and
however PIDs are reused. A fork is a new process, an exec or a setuid is
not, and a client passed to another process by its file stays its maker's,
as RM keys a client's ProcID and token when the client is made. The backend
takes the guest kernel's word, which is the threat model's line: separating
guest users is the guest kernel's job. A guest module that cannot say fails
closed: it gets no duplicate between two clients except by a recorded grant,
and names no client but the caller's own (logged once a session). One that
says the process but not the euid has every token rule held to the process.

### Memory registered by its pages

RM registers memory a caller already has by CPU address, and pins what that
address maps in the calling process, the backend. Sent that way the three
calls are refused (the critical finding on dev: RM pinned the VMM's memory at
a guest-chosen address). A guest that is offered BCAP_OS_DESC sends the
guest-physical pages behind the caller's range instead, and the backend hands
RM an address of its own (ARCHITECTURE.md, "How memory travels";
`device/src/osdesc.rs`). What
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
    external range, or its UVM file closes. On that close the backend takes
    the mappings down itself first, on its own descriptor, because the event
    pump's duplicate can make the file's last close later; a mapping that
    will not come down keeps the pages until the session ends.

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
  VM's last sixteenth kept for processes holding at most a sixty-fourth;
  32,768 separately mapped runs per VM, each a mapping of the backend's;
  65,536 UVM mappings of registered memory per VM, a quarter per guest
  process, the last sixteenth kept for processes holding at most a
  sixty-fourth.
- **Coherent on the GPU.** Guest RAM is cached write-back in the guest
  whatever RM thinks (ARCHITECTURE.md, "Memory types, and coherency"), so
  every GPU mapping of registered memory snoops, as for the other system
  memory the backend rewrites, and so does a context DMA over it
  (`device/src/rmmem.rs`). Its coherency needs no rewrite: RM takes an OS
  descriptor of ordinary pages write-back or refuses it, natively too.

What it does not cover: a holder in the host kernel the backend does not
know of. The list above comes from reading 610.57.04: every place RM
duplicates a caller's object into a client of its own (a semaphore surface,
a memory mapper, NV_MEMORY_EXPORT and the unix export, UVM through
nvUvmInterfaceDupMemory; an event buffer is freed with the memory it names,
an SM debugger's duplicate lasts one call, and the rest are video memory or
confidential compute), and every way NVKMS and nvidia-drm reach RM memory. A
semaphore-surface fence context (nvidia-drm 0x54) over a surface that holds
a registration is refused (EPERM) rather than tracked (BE-1.4). A
holder that list missed, or one a later release adds, would let the guest
unpin frames the GPU can still reach -- a guest privilege escalation, never
the host's memory. A free the backend itself makes (a closing file's
clients, the session's) releases only what RM confirms it freed; a client
RM would not free keeps its registrations until the session ends. An
ALLOC_MEMORY with a zero hObjectNew is refused: RM would make the object
under a handle it never reports. A registration abandoned in flight (a
fatal signal, a timeout) stays pinned in the guest until the device is
removed, since nothing can say whether RM took it.

### Compute is opt-in

CUDA needs paths nothing else does: the UVM device, UVM's multi-process
sharing mode, the UVM aperture (the VMM maps semaphore pools at guest-chosen
addresses in its own address space) and memory registered by its guest pages
(RM pins guest RAM for the GPU until a list of holders read from one release
lets go). All of them are served only when the backend is started with
`--allow-compute`, and none is by default: the guest then has no UVM device,
and its host surface is RM, NVKMS and DRM alone (the table below).

Every guest-reachable path that exists only for CUDA and other compute, and
what the backend does with it. Default is off.

| path | without `--allow-compute` (default) | with it |
|---|---|---|
| OPEN of `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools` | **refused** (ENODEV) before any host open; the guest makes neither node and does not register the `nvidia-uvm` major | opened on the host |
| UVM ioctls (`uvm_gate`, `device/src/guestptr.rs`) | unreachable: no UVM file exists | 31 of 610.57.04's 38 (35 to 37 on the releases before 610), table-sized; the tools device's all refused |
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

Without `--allow-compute`, NVIDIA's Vulkan driver still lists
`VK_KHR_acceleration_structure`, `VK_KHR_ray_query`,
`VK_KHR_ray_tracing_pipeline`, `VK_NV_ray_tracing`, `VK_NV_optical_flow`,
`VK_NV_cuda_kernel_launch` and `VK_NVX_binary_import`, but cannot create a
device with any of them (`VK_ERROR_INITIALIZATION_FAILED`: they run on its
CUDA stack, which needs `/dev/nvidia-uvm`). Natively the same with the node
hidden. An app that enables every ray tracing extension it is offered
(Godot's Forward+ renderer; likely vkd3d-proton's DXR) fails in a
graphics-only guest; with `--allow-compute` it runs. Hiding those extensions
in the guest (a Vulkan layer) would make such apps fall back instead; not
done.

### The UVM aperture

A CUDA context needs a UVM semaphore pool mapped at its own address, and UVM
maps one nowhere else, so each pool the guest maps is mapped by the VMM at
that address in the VMM's own address space and given a memory slot in a
second guest-physical region (ARCHITECTURE.md, "The UVM aperture"). The
address is the guest's choice. What bounds it:

- **Only a pool, only this VM's, only exactly.** The backend records a pool
  when UVM says ALLOC_SEMAPHORE_POOL succeeded on that UVM file, and asks the
  VMM to map only that range of that file, read-write, when the guest asks for
  exactly its base and length (`device/src/uvmmap.rs`). A UVM file not in
  sharing mode, another file's pool, a sub-range and a read-only request are
  refused. UVM checks the same range again when the VMM maps it.
- **Never over the VMM's own memory.** The address must lie in [4 GiB, 32
  TiB), where a 64-bit VMM normally has nothing: its executable and heap sit
  at two-thirds of the 47-bit space (85 TiB), and its mappings grow down
  from below the stack or, under the legacy layout (`vm.legacy_va_layout`,
  or the `ADDR_COMPAT_LAYOUT` personality), up from a third of it (42.7
  TiB), which the 64 TiB top the band once had reached. The exception is a
  stack rlimit that is unlimited, or above about 96 TiB: x86 puts the top of
  the mmap area below the stack by the rlimit, capped at five sixths of the
  address space (`arch/x86/mm/mmap.c`), so the VMM's mappings then start
  near 21 TiB, inside the band. (The comments said an unlimited stack rlimit
  meant the legacy layout; on x86 it does not.) The backend, the VMM and the
  guest driver all check the band. nesbox maps with `MAP_FIXED_NOREPLACE`,
  so a collision fails rather than replacing anything. crosvm reserves the
  whole band `PROT_NONE` at start-up, before it maps guest RAM or anything
  else, and maps a pool over its own reservation only, putting the
  reservation back when the pool goes ("The VMMs"): nothing of crosvm's can
  be in the band. Two of the guest's own pools at one address are refused by
  the backend before the VMM is asked, with ENOMEM as any placement is (it
  was EEXIST). That does not hide everything (B5 and F3): a refusal where
  the caller's budgets had room still says the address is taken -- by
  another guest process's pool, which one process can so find and squat on,
  or, under nesbox with a stack rlimit that moves its mmap area into the
  band (above), by one of nesbox's own mappings, guest RAM among them.
  nesbox's `docs/SECURITY.md` says the same since `virtio-nvgpu-v3`.
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
  (F1). Every placement goes when its
  last guest mapping does, when its file closes, and on a guest reboot or
  device reset; if the backend goes away, the VMM drops them all itself.

Sharing mode only takes things away (pageable access, the tie to the
backend's mm), no host driver change is needed, and the VMM's seccomp filter
already allows the calls involved (`mmap`, `mincore`, `ioctl`).

### NVKMS, KMS and leases

NVKMS itself checks no permission at all for SET_CURSOR_IMAGE, MOVE_CURSOR,
SET_LAYER_POSITION, SET_DPY_ATTRIBUTE, SET_DISP_ATTRIBUTE,
SET_FRAMELOCK_ATTRIBUTE and the overrides in QUERY_DPY_DYNAMIC_DATA (read
from `nvkms.c`), so the backend decides them from state of its own.

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
SETCRTC, SETPLANE and PAGE_FLIP) must be 0 or a framebuffer this VM made,
or, for SETCRTC only, -1, which keeps the CRTC's current framebuffer.
Otherwise the call is refused before the host sees it. The -1 case keeps
whatever the CRTC already shows, which on a newly leased CRTC may be a
framebuffer the host compositor made. Framebuffer ids are device-wide, and
without this a lessee could show the host compositor's or another VM's
screen. GETFB and GETFB2 return handles only for the calling file's own
framebuffers. ADDFB2 identifies every handle as NVKMS memory. The CRC32
ioctls run only on a card or a lessee, never on a render node, which on its
own would see every CRTC.

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

### The window's size and share

The shared window's zones are split among a VM's guest processes, and the
window's size and the share one process may hold are flags. The two defects
that made them necessary are in Appendix A ("`window-config2`").

**A mapping is found by who made it.** An RM mapping's entry records the
guest process that made it -- the owner of the file NV_ESC_RM_MAP_MEMORY ran
on -- and the address its last accepted UPDATE_DEVICE_MAPPING_INFO gave it
(`mmap.rs`, `MmapContext::find`). UPDATE and UNMAP find a mapping by
`(client, memory, process, address)`, the address matched against the
recorded virtual address first and the window offset second: never by an
address alone, and never another process's. RM itself answers both only for
the calling process's mappings (`serverutilMappingFilterCurrentUserProc`),
and every call reaches it from the backend's one process, so the filter is
kept here. The process the backend knows is the owner of the control file
the call is made on, not the thread that makes it: two processes that share
one control file (across fork, or by SCM_RIGHTS) are one process to this
filter, and either can UPDATE or UNMAP the other's mapping on it, where RM
would hold each to its own. The extent stays charged and placed while a
guest mapping of it lives, so what that reaches is the file's other holder
only. An UPDATE whose `pOld` names none of the caller's mappings of the
object moves its only one, and with two it guesses nothing (RM is handed 0
and says so). A later UPDATE to the same address takes it from a stale
entry. RM is handed only the backend's own address, as both `pOld` and
`pNew`; a guest address never reaches it.

**A mapping is made whole or not at all.** The window extent is reserved
before RM is asked to map, in the zone the mapping will most likely need
(rmmem's records; video memory is WC), so a refusal leaves the host with
nothing mapped. RM's answer decides the type: another type moves the
reservation, and if it does not fit there, or the VMM will not place it, or
there is no window, the host's mapping is undone with RM_UNMAP_MEMORY at the
host's address. The file stays spent (a host file carries one mapping in its
life) until the guest closes it. A length larger than any zone is refused
before a reservation, with RM's own NV_ERR_NO_MEMORY.

#### The flags

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

#### What a larger share gives up

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

#### What a larger window costs the host

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

---

## 5. The backend process

### What the backend parses

| input | what it parses |
|---|---|
| guest messages | 17 types, where every v2 type except HELLO is refused (EPROTO) until a HELLO succeeds; requests up to 256 KiB, or 4 MiB with indirect descriptors |
| guest payloads | the ioctl header with its nested and deep blocks; mmap and munmap requests; the IOCTL2 schema interpreter, HOST_OP, WATCH and UNWATCH; and Wayland frames through `wlwire` (codec generated from 39 vendored protocol XMLs, whose build fails if any reachable message carries a descriptor the policy does not classify) |
| host peers | the VMM's vhost-user messages, the host compositor's Wayland messages, export peers' Wayland messages, the capture helper's fixed-size packets (`--inject-socket`, "Capture injection"), and kernel uevents (messages from any sender but the kernel are dropped). Every descriptor received over a socket is classified by what the kernel says it is (`hostfd::classify`), not by what the message claims. |

### Host files, sockets and netlink

| | what the backend opens |
|---|---|
| device files | `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-modeset` and this GPU's render nodes; `/dev/nvidia-uvm` and `/dev/nvidia-uvm-tools` only with `--allow-compute`; `/dev/dri/card*` (`--kms-card` only, through OPEN_KMS; a plain OPEN of a card is refused); lessee files received from the compositor, which must classify as a lease of this GPU; and `/dev/udmabuf` |
| other files | `/proc/driver/nvidia`, each GPU's PCI directory in sysfs, the `--pci-config-dir` snapshot (read before the sandbox), memfds for shm pools, blobs and a sealed page, readlink of `/proc/self/fd` (through an `O_PATH` descriptor of the directory, kept), and reads of `/proc/self/fdinfo` |
| vhost-user socket | a listening socket handed to it (systemd's socket activation, or `--socket-fd`: "The backend's socket"); else by default `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` in a 0700 directory, refused if the directory is anyone else's, where a file already at the path is removed only if it is a socket of the backend's uid, and anything else stops the start (`device/src/posture.rs`) |
| other sockets | with `--wayland-socket`, one connection to the compositor per channel plus a probe connection; with `--wayland-export`, a listener created 0600 that admits only peers of the backend's uid, with 16 pending; with `--inject-socket`, a `SOCK_SEQPACKET` listener (created 0600 and opened to the capture helper's group by root after start, or handed over by the socket unit) that serves only `--inject-uid`, four connections at once ("Capture injection") |
| netlink | `NETLINK_KOBJECT_UEVENT`, receive only, with `--kms-card` or `--wayland-lease` |

### Privileges

| | now |
|---|---|
| identity | Refuses to start with euid 0 or CAP_SYS_ADMIN unless `--allow-root-unsafe`. The launcher, as root, gives each VM a slot of a user pool: the backend runs through `setpriv` as `nvgpu-vmN`, in the groups video, render and kvm, with no capabilities and no_new_privs, and the VMM under nesbox's jailer as `nvgpu-vmmN` (below, "One uid per VM"). With `--wayland-socket` the backend runs as the socket's owner (the desktop user), and with `--wayland-export` as the owner of the export socket's directory. Unprivileged (the rig), both are the invoking user. |
| capabilities | All dropped before the first thread exists (effective, permitted, inheritable, ambient, and the bounding set where it may), then no_new_privs, undumpable, umask 077. This holds under `--allow-root-unsafe` too, and RM decides administrator by `capable(CAP_SYS_ADMIN)` (`nv-linux.h`), so even then RM sees none. What root keeps is file access by uid. |
| sandbox | Before the first guest message and its second thread (`device/src/sandbox.rs`, below): a network namespace of its own; Landlock to the GPU's nodes, `/proc/driver/nvidia`, `/proc/self` and the GPUs' sysfs, read-only but for the nodes, plus the compositor's and its own export socket; a seccomp allowlist of 85 syscalls on x86_64 (72 whatever their arguments, 13 with a rule of their own), anything else stopping the process; RLIMIT_CORE 0. Each layer the kernel lacks is logged `sandbox: DEGRADED` and stops the start; `--sandbox=best-effort` and `--sandbox=off` are diagnostic flags. No cgroup of its own making; the shipped unit gives it one ("The units and the NixOS module"). RLIMIT_NOFILE is raised to its hard limit at start. The host RM must keep a client to the file it was made on: the backend asks it at start, and refuses to run on one that does not (R3) |

### Fail closed

Where the backend cannot hold what this document says, it refuses to start or
refuses the call. Appendix A.8 has what each of these replaced.

**Only measured driver releases.** The backend refuses to start unless every
table was measured at the host's release (`release_gate`,
`device/src/release.rs`): its own
RM allowlist and NVKMS schema, a UVM table whose range holds it (the last
range ends at the newest release measured, in both the backend's and the
guest's copy), and an ABI profile no newer than `MEASURED_THROUGH`
(`gen/src/versions/mod.rs`; 615.71.09, on gVisor's lineage, the A2000
capture, and every measured release's RM escape blocks). 580 is the
precedent for why: it added pointers to two controls the older list let
through. A version that does not parse is fatal. The tables chosen are named
on one warning line at every start. `--allow-unmeasured-release` (a
diagnostic flag) runs a newer or in-between host on the nearest older
tables, without compute (no UVM table on either side); a host older than
every release is refused regardless, and inside the backend it gets an
empty RM allowlist and every RM escape refused. The host's version is read
from `/proc/driver/nvidia/version` before any guest message and is never
learned from a guest's CHECK_VERSION_STR; with no host version set at all,
as a library user might leave it, every RM escape is refused (EINVAL)
outside tests and fuzzing (BE-1.15). The rig's 595.99.02 is measured
by all four.

**Start-up.** Three conditions stop the backend. A sandbox layer the kernel
lacks, or has only in part (`sandbox: DEGRADED`), with `--sandbox=on`, the
default; `--sandbox=best-effort` runs with what the kernel has, and it and
`--sandbox=off` are diagnostic flags. An error asking the host's RM whether
it keeps clients to their file (`probe_strict_clients`; R3): only an answer
of yes lets it start. And a second thread: the seccomp filter is installed
with `SECCOMP_FILTER_FLAG_TSYNC`, and `sandbox::apply` installs nothing at
all when the process already has a second thread (or its thread count
cannot be read), since Landlock and the user namespace reach only the
calling thread, and a thread made before them would be outside both. Tests:
a two-thread child gets no layer; a thread made before the filter is
stopped by it.

**The release profile.** The workspace's `[profile.release]` is
`panic = "abort"`, `overflow-checks = true`, thin LTO, one codegen unit,
line tables only: a panic ends the backend, and with it the VM's session,
rather than stalling one queue or leaving a lock poisoned or an executor
job mid-call. Nothing outside tests catches an unwind. What `abort()` does
-- block signals, `tgkill` its own thread with SIGABRT, reset a handler
that caught it -- is on the seccomp list, and a test panics a filtered child
the way a release build does (from the main thread and a worker) and sees
SIGABRT, not the 159 of a violation. The fuzz workspaces have profiles of
their own; the difftest runs under the release profile (`cargo test
--release`).

**Diagnostic flags.** Every flag that takes a protection away is a
diagnostic flag. DEPLOY.md, "Backend flags", lists them, and so does
`vhost-user-nvgpu --diagnostic --help`. They are hidden from `--help`, the
backend refuses to start with any of them unless `--diagnostic` or
`NVGPU_DIAGNOSTIC=1` is given too, and each in effect is announced as
`DIAGNOSTIC: <flag>: <what it takes away>` on stderr, whatever the log level,
and in the log. `rig/run-guest.sh` adds `--diagnostic` only when one of them
reached the backend's arguments (after `--`, or from `NVGPU_SANDBOX=off` or
`NVGPU_ALLOW_ROOT_UNSAFE=1`); as root it refuses every one of them without
`NVGPU_DIAGNOSTIC=1`, and it says each on the terminal.

**What the log holds.** No raw parameter dump reaches the log: not the
CARD_INFO reply (BAR physical addresses), SYS_PARAMS, RM_CONTROL and RM_ALLOC
replies, MAP/UNMAP_DMA and VID_HEAP_CONTROL replies, nor the parameter bytes
of a control RM refused (host addresses, and a guest's data). A refused
control is its command and status, at debug. The per-call lines a guest can
drive (UVM and window placements, read-only mappings, OPEN_KMS, SHM
restores) are debug; the RM allowlist's teardown report of what it refused
is a warning. The default level is `warn`, so the start-up lines worth
reading (the tables chosen, the sandbox layers not in force, the diagnostic
flags) are what a production log holds. The rate limit
(`device/src/ratelimit.rs`) says how many lines a site dropped when its next
line goes out, and a site that went quiet after its burst says so at
teardown.

**SYS_PARAMS and CHECK_VERSION_STR go as sent.** `NV_ESC_SYS_PARAMS` is
`{NvU64 memblock_size}`: nvidia.ko keeps the first caller's value (on the
control device, so host-wide) and answers EBUSY to any other
(`kernel-open/nvidia/nv.c`). The host's answer, EBUSY included, is the
guest's. What remains: a guest whose SYS_PARAMS is the first on the host
after the driver loads sets that host-wide value. RM uses it only to online
GPU memory as NUMA on coherent platforms, which this project does not
support; on a PCIe GPU it changes nothing.

`NV_ESC_CHECK_VERSION_STR` (`nv_ioctl_rm_api_version_t {cmd, reply,
versionString[64]}`) goes to RM with the command the guest sent. In the
strict (`0`) and relaxed (`'1'`) modes userspace sends, RM fails a caller
whose version is not its own (`RmPerformVersionCheck`, `osapi.c`), so
userspace must match the host's module, as natively (`nvgpu-userspace`
stages the host's own). A mismatch fails in the guest as RM's API-mismatch
error, with RM's reply word and version string copied back as nvidia.ko
copies them (BE-2.1), and with RM's usual `NVRM: API mismatch` line (the
backend's process name) in the host's kernel log. Test:
`sys_params_and_check_version_go_as_sent_and_come_back_as_answered`.

**Test binaries.** `test-harness`, which serves a backend on a socket with no
sandbox and no posture checks, is built only with `--features test-bins`
(which also brings in tokio and tracing, not dependencies of the backend),
and clears its socket path only if it holds this user's socket
(`posture::clear_socket_path`). `nvgpu-userspace --stage DIR` clears only a
directory that is empty or holds the marker it writes into every share it
stages, never through a symlink.

**The generators fail closed.** `rmctrl_extract.py` stops on a control RM's
pointer tables name whose command macro the release's headers do not define
(a control left out would be one whose pointer reaches RM as the guest's
bytes), unless it is in `UNDEFINED_IN_HEADERS` with its reason (one:
`NV0000_CTRL_CMD_OS_GET_CAPS`, whose case RM itself compiles only if the
macro exists). `nvabi_gen.py` stops on an escape whose struct it cannot lay
out, and on a fixed-size escape with no struct (only nvproxy's byte-copied
escapes are variable length). `scripts/gen-check.sh` runs every extractor's
`check` mode, the UVM tag scan, the schema render, and with `GVISOR=` the ABI
profiles, against the sources over the network (`scripts/ci.sh nightly`).
On 2026-09-26 every table matched, and gVisor master regenerated the three
profiles unchanged. Two scans do not yet cover everything: the UVM
extractor names descriptor fields by hand, and the RM extractor scans
control parameters for host fields but not class parameters (BE-1.19).

### The backend's sandbox

`device/src/sandbox.rs`, applied from `vhost-user-nvgpu`'s `main` after the
capability drop and before anything else: before the backend's second thread
(a user namespace can be entered only by a process with one, and Landlock and
seccomp reach the calling thread and what it creates), and before the first
guest message. What it cannot reopen later is opened first: the vhost-user
listener, the export listener, the uevent socket, and RLIMIT_NOFILE raised.

| layer | what it enforces | on a kernel without it |
|---|---|---|
| network | a network namespace with nothing but loopback. As root the launcher makes it (`unshare --net`); otherwise the backend unshares a user namespace and a network namespace together, maps only its own uid and gid onto themselves (so every uid check, its own and the kernel's, is unchanged; supplementary groups keep working, being kernel ids), and drops again the capabilities the new namespace gave it. Every socket the backend uses is a path or was opened before. The layer counts as in force when `/proc/self/net/dev` shows nothing but `lo`, as a launcher's namespace does; a host with no other interface shows the same, and then the backend keeps the host's namespace and reports it in force (open item 3) | unprivileged user namespaces off (`user.max_user_namespaces`, `kernel.unprivileged_userns_clone`, AppArmor's userns restriction): the backend keeps the host's network, `DEGRADED` |
| Landlock | opens only: `/dev/nvidiactl`, `/dev/nvidia-modeset`, `/dev/nvidiaN`, `/dev/nvidia-uvm{,-tools}` with `--allow-compute`, `/dev/udmabuf`, `/dev/null`, and this GPU's render nodes (card nodes with `--kms-card`), read, write and ioctl; `--proc-nvidia`, `/proc/self`, `/proc/cpuinfo` and each GPU's PCI directory in sysfs, read-only. Connects only to the compositor's socket and its own export socket (ABI 9). No file written, created or removed anywhere; no signal to, or abstract socket of, a process outside it (ABI 6); no TCP or UDP (ABI 4, 10). A node made after start is out of reach | no Landlock: every file and socket of the uid, `DEGRADED`; ABI below 9: other pathname sockets reachable, below 6: signals to the uid's other processes, `DEGRADED` either way |
| seccomp | 85 syscalls on x86_64: 72 whatever their arguments, and 13 with a rule of their own, three of which (`clone3`, `unlink`, `unlinkat`) are a fixed error answer. Threads but no processes (`clone` only with CLONE_THREAD and no namespace flag; `clone3` answered ENOSYS, which the C library falls back from); no PROT_EXEC in `mmap` or `mprotect`; `socket` and `socketpair` AF_UNIX only; `ioctl` anything but TIOCSTI and TIOCLINUX (which descriptor a call is on is not visible to a filter); `prctl` only to name threads and anonymous mappings, and to read; `tgkill` only this process; `prlimit64` only its own; the SIGSYS handler cannot be replaced (a kill); `unlink` and `unlinkat` answered EPERM. Absent: `execve`, `ptrace`, `process_vm_*`, `mount`, `unshare`, `setns`, `bpf`, `perf_event_open`, `userfaultfd`, `io_uring_*`, `keyctl`, `kill`, `bind`, `listen`, and the rest. A syscall not on the list stops the process: the log gets `sandbox: syscall N is not on the seccomp allowlist` and the exit status is 159 | no seccomp filters: every syscall, `DEGRADED` |
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
that `/dev/kvm`, other sysfs and the host's files do not. The list was taken
from the code, from strace of the unit tests and of a real start and vhost-user
session against the fake-host fixture; a path only the GPU exercises that
needs a syscall not on it shows up as that log line, on the device. What it
does not stop: a backend taken over still has the GPU's nodes and every
ioctl on them (RM, NVKMS, DRM, UVM: "The host GPU surface"), the guest's
memory, and the host kernel's syscalls on the list. It is a bound on
what one VM's compromised backend reaches outside that surface: other VMs'
files and sockets, the user's files, the network, other processes.

### Resource caps

| | cap |
|---|---|
| threads | the transport; one pump, which sweeps only handles it has reported readable; up to 16 executors; one closer thread for display files; one reader per Wayland channel (at most the channel cap, 64 by default); short-lived close threads; one export accept thread and one hotplug thread in those modes |
| handles | 65,536 per VM, or what RLIMIT_NOFILE backs (half of the hard limit less 1,024 kept for the backend's own); a quarter per guest process, the last sixteenth kept for processes holding at most a sixty-fourth (`device/src/quota.rs`) |
| RM memory records | 262,144 records, and as many parent links, per VM; a quarter each per guest process (`device/src/rmmem.rs`) |
| RM counters | counted only when RM said NV_OK, at most 4,096 keys (`device/src/tally.rs`) |
| logs | every call site limited to a burst of 50 and 10 a second (`device/src/ratelimit.rs`) |
| channel disables | FIFO_DISABLE_CHANNELS at 50 a second per guest process after a burst of 40, and 200 a second per VM after 160, the last 40 kept for processes that have used fewer than 8; at most 1,024 processes with a bucket not yet refilled (`device/src/rmchan.rs`, "The RM allowlist") |
| display caps | 64 NVKMS opens, 16 per guest process; 1,024 syncobj wait registrations; semaphore-surface contexts at 64 per file, 96 per guest process and 256 per VM; 4 KiB of undelivered DRM events per handle, past which the host's own backpressure applies |
| window | each zone (by default UC 32 MiB, WC 768 MiB, WB 224 MiB; `--window-size`) at most half per guest process (`--window-owner-share`), the last eighth kept for processes holding at most a sixteenth ("The window's size and share"); a mapping is charged to whoever opened the file it is armed on |
| Wayland caps | 64 channels per VM. Shm: 1 GiB and 1,024 pools per VM, and 512 MiB and 256 pools per connection; the bytes are what live buffers cover (page-rounded, overlaps once), not pool sizes, since a pool's memfd is sparse, SHM_SYNC writes only inside a live buffer, and pages no live buffer covers are punched out. Unread output: 256 MiB per VM and 64 MiB per connection, half the VM's per guest process (the last quarter kept for processes holding at most a quarter). Per guest process -- the client a daemon connection is for (NVGPU_WL_IOC_CONNECT_FOR), else the opener -- a quarter of the channels (the last eighth kept for processes with at most two) and a quarter of the shm bytes and pools (the last sixteenth kept for processes holding at most a sixty-fourth), shared by all its connections. In the guest daemon, per client process: a quarter of the descriptors it may hold for clients (its hard limit less 60) and of 64 MiB of stream-sink data, the last eighth of each kept for processes holding little. 16 unfinished blobs per connection. 131,072 objects per connection. Lease submits: one per 5 s on average, 3 at once. Four are flags: the channel count (`--wayland-max-conns`), the shm byte budget (`--wayland-shm-budget`), the queue budget (`--wayland-queue-budget`) and the lease interval (`--wayland-lease-interval`). The 1,024 pools per VM and the burst of 3 are fixed. |
| not capped | Memory outside the Wayland and window budgets has no limit of the backend's own; the shipped unit (`contrib/systemd/vhost-user-nvgpu@.service`, DEPLOY.md) puts each backend in a cgroup of its own with `MemoryMax`, `MemorySwapMax=0`, `TasksMax` and `OOMScoreAdjust=500`, the rig's launcher does not. Several VMs of one backend user share that user's host limits. |

**Per-process shares** (`device/src/quota.rs`). Each VM-wide pool keeps its
cap, and each guest process -- as the guest kernel names it, the tgid and
start time of the calling thread group (`ProcId`) -- may hold only a share,
with the last part of the pool kept for processes that hold little of it.
The owner of a handle is the process that opened it (OPEN) or asked for it
(HOST_OP), or the owner of the file the call that made it ran on (IOCTL2,
Wayland receives); a window mapping is charged to the owner of the file it
is armed on; a daemon connection to the client it is for. A guest that does
not say (a module without BCAP_PROC_ID) is held to the per-VM caps.

What it does not do is see through fork: every child is a new process with a
share of its own, so an app that forks four times can still take a pool a
quarter at a time. What bounds that is the guest's own process limits
(RLIMIT_NPROC, a pids cgroup, the sandbox's), as the per-process descriptor
limit does natively. It also rests on the guest kernel's word about which
process is which ("Threat model"), and `/dev/nvgpu-wl`'s group may charge a
channel to any process it names, which is why that group is the daemon's
alone.

### Memory safety

**Where `unsafe` is.** Only in the `sys` module of each crate:

| module | what |
|---|---|
| `device/src/sys/block.rs` | the arena every host call's parameter blocks are built in (below) |
| `device/src/sys/ioctl.rs` | the one `ioctl` with an argument, which takes only an argument an arena built; UDMABUF_CREATE; a no-argument `ioctl` |
| `device/src/sys/guarded.rs` | the guarded buffers the host writes into |
| `device/src/sys/mem.rs` | every mapping, each owned by a type that unmaps it once; every `MAP_FIXED` checked to land inside a range that type owns (the window, an OS-descriptor reservation); `HostSpan`, the only memory outside an arena a pointer field can name |
| `device/src/sys/fd.rs`, `net.rs`, `proc.rs` | descriptors (returned as `OwnedFd`), netlink, identity, limits, Landlock, seccomp |
| `device/src/sys/inherit.rs` | the listening sockets the backend is handed at exec (socket activation, `--socket-fd`), claimed once each |
| `device/src/sys/pod.rs` | wire structs as bytes, for the types that are all integers and no padding (checked field by field) |
| `wlwire/src/sys.rs`, `nvgpu-wl-guest/src/sys.rs` | the Wayland proxy's and the guest daemon's system calls |

Every crate root is `#![deny(unsafe_code)]` and
`#![deny(unsafe_op_in_unsafe_fn)]`, with only `mod sys` allowed; every other
file is `#![forbid(unsafe_code)]`. `scripts/check-unsafe.sh` fails a tree in
which `unsafe`, a raw address (`as_ptr`, a `*const`/`*mut` cast or type,
`transmute`, `from_raw_parts`), a descriptor claimed by number
(`from_raw_fd`, `borrow_raw`) or an `allow(unsafe_code)` appears outside
`sys`, a file has lost its attribute, or an `unsafe` inside `sys` has no
`SAFETY:` comment.

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
it.

The fuzz targets' fake host follows a pointer only into the call's own
blocks (`Arg::reach`). A single deep pointer the guest places across a
pointer RM follows is refused (`scrub_control`, EINVAL).

**What it does not do.** Which fields are pointers is still the tables' word
(`guestptr.rs`, `abi::rmctrl`, `schema.rs`, measured from the drivers'
sources): a pointer field they miss reaches the host as the guest's bytes.
Data rewrites are still edits of the host's copy -- the coherency attributes
(`rmmem.rs`, on a copy before it is built), NVKMS policy (`nvkms.rs`), fence
waits (`fence::before`) -- only unable to reach a declared field.
(SYS_PARAMS' and CHECK_VERSION_STR's rewrites are gone: "Fail closed".) RM's
top-level blocks are field offsets (`Plan`), not typed structs. Both are
"Roadmap" item 9. The raw-descriptor helpers (`read_raw`, `fstat`, ...) take
numbers: a wrong one is EBADF or another of the backend's own files, never
memory outside the buffers passed. This section is the host's; the guest
module's is "The guest module's parsers".

**Guarded blocks** (`device/src/sys/guarded.rs`). The blocks the host driver
reads and writes -- an RM call's parameter block and the block its pointer
reaches, and every block of an IOCTL2 -- are anonymous mappings with a
`PROT_NONE` guard page behind a page of zeroed slack, so a host write past a
block faults (EFAULT) rather than landing on other backend memory. A block of
up to 16 pages (60 KiB of parameters) goes back, when it is dropped, to a
pool of at most four per size -- 2.3 MiB in all, whatever the guest does --
and comes out again for the next block of its size. What makes that equal
to a fresh mapping:

- it is zeroed through its whole reach (the block and its slack, every byte
  the host could have written: the guard page cannot have been) before it
  enters the pool, so a block taken out is all zeros, as a fresh anonymous
  mapping is. Nothing one call left -- the host's output, a driver's write
  past the declared size into the slack -- reaches the next call, whichever
  guest process makes it;
- the guard page is the one `mmap` made and stays `PROT_NONE`: a gross
  overrun still faults, in the call that made it;
- the layout is the same (the block at the page's start, the slack, the
  guard), so what a driver that touches a few bytes past a block finds is
  what it would find in a fresh mapping: zeros;
- a larger block, or one the pool has no room for, is a fresh mapping,
  unmapped after the call. Miri builds keep the heap stand-in and no pool.

The pool is process memory the backend keeps mapped between calls; it is
never visible to a guest, and it holds no guest data between calls. Tests:
`a_reused_buffer_is_a_fresh_one` (dirty the reach, drop, take: all zero at
every size up to 60 KiB) and
`the_pool_is_bounded_and_takes_no_large_buffers`.

### Fuzzing and Miri

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

---

## 6. Deployment

### One uid per VM

Run as root, `rig/run-guest.sh` takes a free slot N of a pool made once with
`useradd` (`rig/launcher/root.sh`'s header): `nvgpu-vmN` runs the backend,
and `nvgpu-vmmN`, whose group is `nvgpu-vmN`'s, runs the VMM under nesbox's
jailer (chrooted into a jail image built for the run, a mount namespace of
its own, no supplementary groups, no_new_privs). The backend's socket is
0660 in the slot's group, so the VMM reaches it and nobody else does. Both
run in network namespaces of their own. A slot is free when no launcher
holds its lock, neither user has a live process, and `contrib/systemd`'s
socket unit for VM N is not listening (the units use the same users and
take no lock: start one only for a slot no launcher holds). Where there is no pool
the launcher falls back to the one user `nvgpu`, and without a jailer to a
root VMM, each with a warning. A uid of its own per VM separates two VMs'
host processes by the kernel's oldest rules, independent of anything this
project wrote:

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
- **Files and sockets.** The backend's socket is root's and open to its
  slot's group alone, in a directory of root's ("The backend's socket");
  the disk copy is 0600 the VMM's.
  An export socket admits only peers of the backend's uid (`SO_PEERCRED`),
  which with a uid per VM means only that VM's own. No VM can connect to
  another's vhost-user socket and take over its guest memory, or to its
  export socket and drive its compositor.
- **RM's security token.** RM's token for a host process is its uid (and
  PID). With one uid, every VM is one principal wherever RM compares tokens:
  OS_SECURITY_TOKEN shares, NV01_DEVICE_0's `hClientShare` fallback, the
  profiler's `hClientTarget` and the channel-ID controls ("RM objects
  between guest processes"). The backend already holds each of those to its
  own VM's clients, but with a uid per VM RM refuses the cross-VM case
  itself as well.
- **Per-user kernel counts.** Processes (RLIMIT_NPROC), user namespaces,
  inotify instances, pipe buffers and locked memory are counted per uid; one
  VM running into them does not do it for another.

What a uid per VM does not do:

- **The GPU.** Every VM's work runs on one GPU; what keeps one VM's GPU
  memory from another's is RM's per-client page tables and the GPU's MMU
  ("Threat model"), the same for every uid. The device nodes are 0666, so
  any uid opens them.
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

### The backend's socket

Nobody but root ever owns the backend's socket or its directory:

- the backend takes a listening socket handed to it: systemd's socket
  activation (`LISTEN_PID`, `LISTEN_FDS`, `LISTEN_FDNAMES`; `vhost-user` and
  `inject`) or `--socket-fd N`. Each is claimed once, made close-on-exec and
  blocking, and must be a listening `AF_UNIX` socket of its protocol's type
  (`device/src/sys/inherit.rs`; tests of the naming rules and the checks);
- `vhost-user-nvgpu@.socket` binds `/run/nvgpu/vmN/nvgpu.sock`, root's and
  0660 to `nvgpu-vmN`, in a directory of root's (0711), and
  `vhost-user-nvgpu-inject@.socket` the capture helper's, 0660 to its group;
- as root, the launcher's run directory, with the binary's copy in it, is
  root's 0711 from the start, and root binds the socket
  (`systemd-socket-activate`, which execs the backend with it once the VMM
  connects; the socket must be root's).

What stays: a backend user with other live processes can still signal the
backend (its own VM's availability), and, where `kernel.yama.ptrace_scope`
is 0, trace it in the moment between exec and its first
`PR_SET_DUMPABLE(0)`. Pool users have neither; keep the shared-user modes
for a single desktop (VD-H1).

### The launcher as root

`rig/run-guest.sh` as root:

- never assumes a layout (`NVGPU_PREFIX` or `NVGPU_RIG` is required), and
  runs from a root-owned copy of itself and `rig/launcher/`. The launcher,
  the binaries (the jailer and virtiofsd too), kernel, rootfs, share, jail
  image and logs directory, and every directory above them, must be root's
  and writable by no one else (a sticky one aside), and are used by their
  resolved paths, the launcher's own pieces too; every library it puts in
  the VMM's jail must be root's. Nothing it creates is writable by others,
  whatever umask it was started with (it ORs in 022): not the VMM's config
  between writing it and the VMM reading it, and not the backend's socket
  before it is opened to the slot's group;
- starts again under `env -i` with its own variables (`RUST_LOG`, `TERM`,
  `NESBOX_VIRTIOFSD`, checked as a path, the two the desktop warnings read)
  and a PATH of root's directories. The first shell still runs with what
  sudo let through: keep sudo's `env_reset`;
- takes a slot only when neither of its users runs anything and no
  systemd socket unit listens for it, holds the slot's lock, and kills
  nothing by pattern. A pool run's files carry its
  slot (`<tag>.vmN`), and a run whose `<tag>.json` another live run holds is
  refused, in every mode;
- runs the backend through `setpriv` with no capabilities and
  `no_new_privs`, in a network namespace root made, and refuses crosvm as
  root, `NVGPU_VMM_JAIL=off` without saying so (it defaults to `on`), and
  every diagnostic setting (`NVGPU_SANDBOX=off`, `--sandbox=off`,
  `NVGPU_ALLOW_ROOT_UNSAFE=1`, `--allow-root-unsafe`, and the rest) without
  `NVGPU_DIAGNOSTIC=1`;
- writes the backend's log through a writer that keeps
  `NVGPU_LOG_MAX_MIB` (64) and reads and drops the rest (the backend holds a
  pipe, not the file). The VMM writes the console log directly, and a
  watchdog stops the VM once the log passes the limit;
- strips control characters but tab and newline from what it echoes of the
  guest's console (the verdict, FAIL lines).

`rig/verify/launcher-dryrun/` runs the launcher as the root of a user
namespace with stub binaries and checks each of these (`scripts/ci.sh
deploy`).

### The units and the NixOS module

`contrib/systemd/` has the backend's service and socket units, the capture
helper's socket, and two VMM templates (`nvgpu-vmm-nesbox@.service`, the
jailer as root and the unit's network namespace; `nvgpu-vmm-crosvm@.service`,
`nvgpu-vmmN` with its sandbox on, so no `RestrictNamespaces=`). A VMM unit
says `BindsTo=` its backend, so it stops with it. Both set
`LockPersonality=`, `RestrictSUIDSGID=` and `SystemCallArchitectures=native`,
and the nesbox one `ProtectKernelModules=` and `ProtectKernelLogs=`; neither
has run on hardware.

The backend's unit runs it as `nvgpu-vmN` with no capabilities, in a cgroup
of its own (`MemoryMax`, `MemorySwapMax=0`, `TasksMax`,
`OOMScoreAdjust=500`), with `PrivateNetwork=`,
`RestrictAddressFamilies=AF_UNIX AF_NETLINK`, `RestrictNamespaces=yes` (the
unit's `PrivateNetwork=` gives it a namespace with only loopback, which it
keeps; it unshares nothing), `KeyringMode=private`, `ProtectHostname=yes`,
`ProtectKernelLogs=yes`, `ProtectSystem=strict`, `ProtectProc=invisible`,
and `NoExecPaths=/` with `ExecPaths=` the binary and the library directories
(it never execs). Left out, and said why in the unit: `RemoveIPC=`
(`PrivateIPC=` already keeps System V IPC and queues its own, and under a
login user it would remove that user's shared memory), and
`SystemCallFilter=` (the backend installs its own allowlist). Its
`ExecStartPre=` runs `nvgpu-pci-snapshot` as root, which copies the GPUs' PCI
config space for `--pci-config-dir`; the backend uses a snapshot only if it
is of the same device, and blanks what only the host should see before the
guest reads it: the MSI capability's address and data, and the device serial
number, with the capability lists still walking.

The NixOS module (`nix/module.nix`) installs these units, with what is per
slot in a drop-in. Its pool has fixed ids (`uidBase`, 64000: slot N is
uidBase+2N and uidBase+2N+1, group uidBase+2N), and its assertions refuse: a
helper uid of any slot or of a login user; a helper group that does not
exist, is a pool or shared group, or holds anyone but the helper; one helper
or group for two VMs; anyone else in a slot's group; pool users with other
groups; another user on a pool id; a flag with whitespace, a quote, a
backslash or a `%` (systemd would split, unquote or expand it on the way to
the backend), and a Wayland socket path with any of those or a colon.
`checks.module-eval` tries each with a configuration it must refuse,
compares the units it installs with `contrib/systemd`'s, and holds each
drop-in to the keys and variables that are per slot, every assignment of an
`Environment=` line included. The Wayland flags come from the environment file
(`$NVGPU_WAYLAND_ARGS` beside `$NVGPU_BACKEND_ARGS`; `vms.<n>.wayland`), since
systemd lets `EnvironmentFile=` override `Environment=`.

The backend holds the same rule at run time as far as it can see it: an
inject connection from the uid its vhost-user sockets are connected to (the
VMM's) is refused ("Capture injection").

---

## 7. The VMMs

The VMM maps all of guest RAM and the window, and it executes the backend's
mapping requests (vhost-user `SHMEM_MAP`/`SHMEM_UNMAP`). A compromised backend
talks to it; the guest reaches it through the device's PCI function.

### A placement never leaves a hole

A backend descriptor is placed into the window by mapping it where the
kernel chooses and then moving it over the reservation with
`mremap(MREMAP_FIXED)`; the reservation is put back if the move fails.
nesbox: `virtio-devices/src/nvgpu.rs` `WindowMapper::place`; crosvm:
`patches/crosvm/0005` (`base/src/sys/linux/mmap.rs`). Each has a test that
forces the failure and checks `/proc/self/maps`. nesbox's UVM aperture maps
with `MAP_FIXED_NOREPLACE` and adds a slot only on success. The move can
still fail after the kernel has dropped the target; the reservation is then
put back with `MAP_FIXED_NOREPLACE`, never `MAP_FIXED` (another thread's
mmap may have landed in the hole meanwhile), and if that cannot be done the
VMM aborts rather than leave something unknown behind the slot. A
hugetlbfs descriptor never gets this far: its mapping is a whole huge page,
and moving it in would replace up to a gigabyte past the checked range --
nesbox takes only NVIDIA and DRM character devices into the window (below),
crosvm's arena refuses hugetlbfs.

### nesbox

nesbox (`virtio-devices/src/nvgpu.rs`, branch `virtio-nvgpu-v6`, the one
DEPLOY.md names): a window placement must be an NVIDIA device (character
major 195) or a DRM node (226), opened read-write if the mapping is writable
-- the only files the backend places there -- page-aligned (its length taken
as whole pages), inside the window, clear of every live placement, and one
of at most 16,384 (each is a mapping of the VMM's, and `vm.max_map_count` is
the process's); a withdrawal must name a live placement exactly. When the
backend's request channel closes, every window placement is withdrawn as
well as every aperture one: each holds a host device file, and the GPU
memory it maps, in reach of the guest. A UVM pool must come from
`/dev/nvidia-uvm` itself (below, as crosvm). Guest RAM's memfd is sealed
against shrinking, growing and further seals once sized: the backend and
virtiofsd hold it, and a truncation would make every access past the new end
a SIGBUS in the VMM. The device config comes from the backend only
(`GET_CONFIG`; its length and GPU count are checked), the mapper is bound
only once the window is reserved, and the cgroup descriptors a config names
are checked (open for writing, on cgroup2) and made close-on-exec, so
virtiofsd does not inherit them. nesbox's baseline seccomp filter is
installed with TSYNC after all set-up, so threads made earlier (the prefault
thread among them) are covered.

The jailer (nesbox `virtio-nvgpu-v6`) mounts a `/proc` of the jail's own
with `hidepid=invisible` (not `subset=pid`: nesbox reads `/proc/devices` for
the UVM major), so the jailed user sees only its own processes, and binds
only `/sys/fs/cgroup`, `/sys/kernel/mm/transparent_hugepage` and
`/sys/devices/system/cpu` (with a network device, `/sys/class/net` and
`/sys/devices/virtual/net`; a virgl `gpu` device still gets all of `/sys`,
which Mesa walks). It refuses a kernel that does not know `invisible`
(before 5.8) rather than change the host's `/proc`. It runs in a private
mount namespace with a propagation cut, runs `setgroups(0)`, refuses a uid
that is live on the host, and binds the socket file alone. Under the jailer
the network namespace is the unit's (a chrooted process gets no user
namespace); `"unshare-network": true` is for unjailed runs.

### crosvm

`patches/crosvm/` (eleven patches; `patches/README.md` says what each is):
every request is bounds-checked against the region the backend reported
(overflow-checked, page-aligned offsets, overlaps refused, unmaps must name a
live mapping, reset unmaps all); GPU and external maps are refused for the
nvgpu type; a region size above 64 GiB is an error, not a panic; a refused
request fails the backend's request, not the VM. That is the frontend's own
check, and the main process checks every request again (`0007`-`0009`),
trusting nothing the frontend says.

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
nesbox: the code that parses the backend's requests is more confined
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
  any length, each registered once and unregistered only where it is. The
  addresses end with the settings BAR's 4 KiB notification area, whatever
  queue count the backend reported: past it are the device's MSI-X table and
  then other devices' MMIO (MP-1).
  Upstream lets any device's tube register an ioevent anywhere, and much
  more -- register memory anywhere, balloon.
- *Its shared memory tube* may prepare region 1, the window -- the window
  alone, not the whole BAR, so the aperture's slots never overlap it -- and
  map into it only: a descriptor (no other source), at its own BAR's
  allocation (no guest physical address, no other device's BAR), coherent,
  page-aligned, non-empty, inside the window, overflow-checked, clear of its
  other live mappings, at most 16,384 at once; from a character device of
  major 195 (`/dev/nvidia*`) or 226 (DRM) -- the only descriptors the
  backend places there (`nvidia/rmmap.rs`: RM_MAP_MEMORY's `/dev/nvidiaN`,
  and an mmap of an NVIDIA device or DRM node, `handle_mmap`) -- and
  writable only if the descriptor was opened read-write. It may unmap only
  what it mapped. Ballooning, `MmapAndRegisterMemory`, ioevents and external
  mappings are refused.
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

**Compute** runs under both VMMs. crosvm publishes region 2 after the window
in the same 64-bit prefetchable BAR, each region with its own shared-memory
capability, when the backend reports it (only with `--allow-compute`); the
guest driver finds regions by id. The launcher allows `--allow-compute` with
`--vmm crosvm` only when the binary's `run --help` names the
`nvgpu-uvm-aperture`. The crosvm side has unit tests for every check above
and the seccomp test, and has run on the RTX 5090: the render probe's CUDA
and the security negatives with `--allow-compute`, with the frontend jailed
(rig/TESTING-RIG.md, "crosvm"). The frontend takes the backend's regions by
id, however many it reports: a lone region that is not region 1 is no
window.

**Hot-plug.** A virtio device's control tube -- its PCI transport's,
whichever process that runs in -- takes power management events only;
`HotPlugVfioCommand` from it is refused (`AnyControlTube::DeviceNoHotplug`,
`0009`), so a compromised device process cannot have the main process add a
host VFIO device to the VM. Hot-plug ports keep theirs. (`run-guest.sh`
passes `--no-pci-hotplug-port`, so there is no port to plug into either.)

### Prefaulting the window

nesbox (from `virtio-nvgpu-v5`) and crosvm (`patches/crosvm/0010`). A
guest's first touch of each page placed in the window would be a
second-level page-table fault, about 3.1 µs a page for BAR1 video memory.
Each VMM makes one more vCPU that never runs and, on a
thread of its own, calls `KVM_PRE_FAULT_MEMORY` through it for each
placement it has made, after answering it. What this adds and does not:

- it maps only what the window's memory slot already backs, at the
  addresses just placed -- what the guest's own first access would have
  mapped, with the same permissions (KVM maps as a read fault would, so it
  never makes a mapping writable that a guest write would not). It gives the
  guest no memory, no address and no access it did not have;
- the extra vCPU is not the guest's: its id, which is its APIC id, is past
  every guest vCPU's -- nesbox's are 0 to n-1 and it is n; crosvm's are the
  host's APIC ids under `--host-cpu-topology`, and it is the largest of them
  plus one (VD-C3) -- so it is past every one the ACPI tables list, the
  guest never starts it, nothing ever runs it (no `KVM_RUN`), and an
  interrupt the guest sends it is never delivered to anything. Both VMMs
  make it only for an nvgpu device, only when KVM has
  `KVM_CAP_PRE_FAULT_MEMORY`, and stop trying at the first ENOSYS, ENOTTY or
  EOPNOTSUPP. It shares the guest's TDP root (the same CPUID, so the same
  paging depth), which is the point: nothing else about KVM's view of the VM
  changes;
- a range withdrawn before its turn is `PROT_NONE` reservation again, which
  KVM refuses (EFAULT), and the thread moves on; withdrawing is unchanged,
  and KVM's MMU notifiers still zap whatever was mapped when a placement is
  taken away, prefaulted or not;
- the work is bounded: at most 1,024 ranges wait, and past that a request is
  dropped (the guest then faults those pages itself, as before). The CPU it
  spends is about 0.13 µs a page placed, less than the faults it saves; a
  guest that maps and unmaps in a loop makes it spend that, as it made the
  placement thread spend its `mmap`s before;
- crosvm does this in its main process, which is where window mappings are
  made (patch 0007) and which holds the VM; the jailed frontend is
  unchanged. nesbox does it in the VMM process; the call is an `ioctl`,
  which both VMMs' filters already allow.

### Guest RAM under crosvm

`--prefault-memory` (`patches/crosvm/0011`), which the launcher passes when
the binary has it, brings crosvm to what nesbox does by default
(BENCHMARKS.md, "Heavy workloads": without it a game in a crosvm guest
stalled on host page faults). Guest RAM is faulted in
(`MADV_POPULATE_WRITE`) and collapsed into 2 MiB pages (`MADV_COLLAPSE`) by
a thread of crosvm's main process as the vCPUs start. It touches only the
memory crosvm already maps as guest RAM, writes nothing (a page the guest
reached first stays as it is), and gives the guest nothing it could not
reach; the collapse moves contents between pages the way khugepaged does.
It runs after every device process is forked, so no jail inherits the
thread. The cost is host memory: all of guest RAM is committed as the VM
starts, not as the guest first uses it, which a VMM unit's `MemoryMax` must
allow for already (it bounds what the guest could touch anyway). File
mappings are placed so their address is as far past a 2 MiB boundary as
their offset (`0011`'s change to base's `align`), which changes where, not
what, is mapped. `NVGPU_PREFAULT=0` leaves it off.

### The GPU's PCI address

The guest puts its fake PCI device at the host
GPU's address, because NVIDIA's userspace matches what RM reports against
sysfs and procfs; rewriting that address in every reply that carries it
would be new rewriting surface, so it is not done. A VMM with its own device
there collides: the guest driver fails cleanly and names the bridge in
the way, and `run-guest.sh` refuses a crosvm whose buses would hold the
address.

---

## 8. The host desktop

**The Wayland proxy** (`wlwire/src/policy_table.rs`).

- The allowlist has 42 globals: Hyprland's own set for clients it does not
  trust without `wl_drm`, plus `wl_output`, xdg-output, content-type and the
  lease device.
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

**What bounds a client.** Besides the caps in "Resource caps":

- Every lease submit of a frame must fit the lease throttle's rate; more
  than its burst in one frame ends the connection. The count follows every
  `new_id` through the protocol tables, and the engine lets through no more
  submits than were counted and admitted (`allow_lease_submits`): a miss is
  fatal, not a bypass (FW-1).
- The engine takes no more of a client's input while 4 MiB is queued for its
  channel (`CHANNEL_HIGH_WATER`); commit copies and blobs are read a record
  at a time as frames are made (`job::Job`), and the backlog is uncounted by
  what each step takes, whatever it read (FW-2, WL-S2). Export mode reads a
  host client only as the guest's queue has room.
- Past 1,024 untaken descriptors a client, or the backend's connection, is
  closed (FW-3).
- A stream sink grants a share that shrinks as streams are added, sent in
  the stream's descriptor to a peer that says `HELLO_STREAM_WINDOW`; what
  sinks hold is charged to the guest process's share of the VM queue
  budget, and past it the stream ends with `ENOBUFS`. Unfinished blobs are
  charged to the VM's and the process's shm budgets (FW-4).
- Fatal text a peer controls is escaped and cut to 512 bytes, and logged
  through the per-site rate limit (FW-5).
- A shm pool must be a regular file on tmpfs or hugetlbfs, or the client gets
  `wl_shm.error.invalid_fd`; a client's blob from anything else is sent as an
  invalid descriptor (FW-6). That, and whether a stream's descriptor is a
  pipe, is told without asking the file's filesystem: its cached type and
  device (`statx` with AT_STATX_DONT_SYNC), a memfd by the kernel's own shm
  device and any other file by a tmpfs or hugetlbfs mount of its device in
  `/proc/self/mountinfo` (`wlwire::sys::is_shmem`). A FUSE file handed over
  by a guest app (to the guest daemon, which serves every app), or by a host
  client of the compositor (a pipe for a guest's selection, to the backend),
  stalls nothing; a tmpfs mounted only in another mount namespace is
  refused.
- A surface collects damage for the last 16 buffers it showed, and a
  destroyed buffer visits only the surfaces that show it: a commit or a
  destroy costs the daemon a bounded number of steps, however many buffers
  and surfaces a client makes.
- In the guest daemon, a client's descriptors and stream-sink data are held
  to its process's share of the daemon's budgets, and a client is held by a
  pidfd from its accept and dropped if its process exits before or during
  CONNECT_FOR, so the process charged is the client's (WL-S3, WL-S6,
  WL-S7). An error is queued after what the client already has, where
  libwayland-server would post it (WL-S8, WL-C2).

A committed `wl_shm` buffer's rows are read straight into the record the
guest daemon sends (`frame::record_with`, the same bytes as
`frame::record`), and a frame is packed with one copy of its records. The
backend parses every record whatever the daemon did.

**The Hyprland and aquamarine patches** change the host compositor for every
client, not only VMs. They let a monitor marked `leasable` be leased, released
from the desktop, and taken back when the lease ends. With no monitor marked,
Hyprland's behaviour is unchanged apart from protocol fixes
(`patches/README.md`).

**Export mode.** Host programs of the backend's uid become clients of the
guest's compositor. Their dma-bufs are imported into the guest with their real
type. One listener is allowed per export.

---

## 9. Capture injection

Branch `capture-inject`: a screen share on the host reaches a guest
application without a copy. The desktop's portal gives a PipeWire stream of
GPU buffers to a per-VM **capture helper** on the host (built by the
integrator, not here: it runs the portal request, and the user picks what to
share in the host's own picker). The helper hands each buffer to the VM's
backend over `--inject-socket`; the backend checks it and answers with an id
and a random token; the helper tells the guest's capture daemon both over a
channel of its own (vsock); the daemon opens the buffer through
`/dev/nvgpu-capture` as a guest dma-buf of the same memory and publishes it
to the guest application as a PipeWire stream of its own. PipeWire, the
portal and every stream protocol stay outside the backend. Off by default;
ARCHITECTURE.md, "Capture injection", has the design, DEPLOY.md, "Capture
injection", the deployment.

### The trust boundary

| party | trusted for | not trusted for |
|---|---|---|
| the helper (`--inject-uid`) | injecting only what the user consented to share with this VM: the backend cannot tell a screen share from any other buffer, since a dma-buf says nothing of where its pixels came from | anything else: every packet is parsed as hostile, every descriptor classified by what the kernel says it is, every layout checked against the object, every count bounded |
| the guest (kernel and daemon) | nothing | ids are the helper's to make; a guest names one with a token it can only have been told |
| other host users | nothing | the socket is 0600, opened to the helper's group by root after start, and served only to `--inject-uid` (`SO_PEERCRED`) |

**What the backend checks** (`device/src/inject/`, each with a unit test
against a fake nvidia-drm, and the `inject` fuzz target):

- The packet: `SOCK_SEQPACKET`, exactly the size of its op (16, 64 or 8
  bytes), reserved words zero, HELLO first and once at version 1,
  descriptors only on IMPORT (exactly `nplanes`) and IMPORT_SYNCOBJ
  (exactly one), at most 8 per packet (`MSG_CTRUNC` ends the connection);
  a malformed packet ends the connection, a refused request is answered
  with its errno.
- Each plane's descriptor is a dma-buf, told as the rest of the backend
  tells one (`hostfd::classify`): by its cached attributes and the
  `exp_name:` line only a dma-buf's fdinfo has, not the link text a FUSE
  file could imitate (FB-13), and without asking any filesystem, so a
  helper that passes a FUSE file stalls nothing.
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
backlog; guest opens, one per (render file, object), 1024 and a quarter per
guest process (`quota::Share::quarter`). Refusals log through the per-site
rate limit. INJECT_OPEN checks the caller's share before it imports
anything, and on any later failure closes nothing: an import that finds the
object already in the file hands back the handle the file had, whatever made
it, which is not the backend's to close. What the bounds do not count: an
object a guest keeps open after the helper released it. That is memory the
helper allocated, held by at most 1024 guest opens, and a guest can allocate
GPU memory of its own without any of this ("Resource caps", "not capped").

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
is done with a buffer the same way (ARCHITECTURE.md, "Capture injection",
has the contract). Nothing on the host waits for the guest: a guest that
never says done costs the helper a timeout, after which it gives the buffer
back to the stream and the guest sees tearing at worst.

The second step, built and run: IMPORT_SYNCOBJ hands the backend a DRM
syncobj the helper made; INJECT_OPEN_SYNCOBJ imports it into the guest's
render file. The guest may then wait on and signal any point of it, early,
late, out of order or never. Its signals reach the helper's syncobj and
nothing else: the helper must treat every point as a hint, bound its own
waits, and never forward the guest's release points to anything that trusts
them. The guest's waits are the VM's ordinary syncobj waits, turned into
polls and counted against the 1024 wait registrations per VM and a quarter
per guest process ("Resource caps"; `device/src/fence.rs`); the render file
is marked as one that imported a syncobj, so its registrations wait out
their firing rather than go with a DESTROY, as for any syncobj someone else
holds. Each INJECT_OPEN_SYNCOBJ makes a new handle in the caller's file, as
SYNCOBJ_FD_TO_HANDLE and SYNCOBJ_CREATE do, which the guest can already make
without bound ("Resource caps").

### What a compromised helper could and could not do

**Could:** inject any NVKMS dma-buf it holds into its VM -- its own
allocations, the stream the portal gave it, any buffer another process
hands it -- which shows its VM pixels it could equally send over vsock as
a copy; hold host GPU memory through the backend, within the bounds above,
memory it allocated itself; make the backend import other devices'
dma-bufs it holds (refused after the import, which attaches the buffer to
its exporter for a moment); keep its four connections and threads.

**Could not:** be root, the backend's own uid or the VMM's (the backend
refuses the first two as `--inject-uid`, the second but for the diagnostic
`--allow-inject-self` a one-user rig needs, and refuses a connection from
the VMM's uid -- the one uid its vhost-user sockets are connected to when
the accept thread starts, which is after the VMM's first memory table; with
more than one such uid it refuses none and warns. The NixOS module, which
fixes the pool's uids, also refuses any VM's backend or VMM uid, a login
user's, a helper group that is shared, a pool group or holds anyone but the
helper, and one helper uid or group for two VMs: "The units and the NixOS
module"); reach another VM (another backend, another uid); make the backend
open, map or write anything (it imports, identifies, sizes and
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

**The guest module** reserves the dma-buf's descriptor number before it
builds anything and installs it only after the reply is copied out, so a
failed copy-out takes back only what the call made.

What is open is in "Open items and residual risk"; the tests and the
hardware runs are in Appendix A ("`capture-inject`").

---

## 10. Inside the guest

| node | in the guest |
|---|---|
| `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` | 0666, as on dev and natively: any guest user reaches everything "The host GPU surface" lists, from a 64- or a 32-bit process (below). The two UVM nodes exist only when the backend runs with `--allow-compute` |
| `/dev/nvidia-caps/*` | 0444 |
| DRM node | a real DRM device per host render node, whose render and primary nodes the guest's DRM core makes. Syncobjs are enabled only when the backend serves fences, and the primary node drives KMS only when the backend offers `--kms-card`. |
| `/dev/nvgpu-wl[N]` | root:root 0660 by default (module parameter `wl_mode`, which refuses any mode giving "other" access), with `contrib/udev/70-nvgpu-wl.rules` giving it to group `nvgpu-wl` for the daemon, which is to be setgid (or its own account), never an application's group. Five ioctls: HELLO, CONNECT, CONNECT_FOR (a connection charged to the client process the daemon names), SEND and RECV. One LISTEN per device, and ACCEPT only from the listener's effective uid or CAP_SYS_ADMIN. The daemon holds at most 4 MiB a client has not read, and closes a client that stays that far behind for 30 s. |
| `/dev/nvgpu-capture[N]` | only with `--inject-socket`: root:root 0660 by default (`capture_mode`, refusing "other" as `wl_mode` does), `contrib/udev/70-nvgpu-capture.rules` giving it to the capture daemon's group. OPEN (an injected buffer as a read-only dma-buf) and OPEN_SYNCOBJ, each needing the id's 128-bit token ("Capture injection") |
| adopted DRM files | a lease received from the host becomes a guest DRM file, cloned from a card-node file |
| `nvgpu-wl-guest` | a daemon listening at `$XDG_RUNTIME_DIR/wayland-0` in the guest |

Every guest user reaches RM, but not another guest process's RM objects: a
duplicate between clients two processes made is refused unless the source
was shared with the destination, and a second client named in parameters
must pass RM's rule for that field with guest processes and euids ("RM
objects between guest processes"). That rests on the guest module saying
which process and euid make each call, and so holds only while the guest
kernel does. RM's export and import of objects through a descriptor (NV0000
OS_UNIX) is a second way to move an object between clients, which the
duplicate rule does not see: it is held to native strength -- the descriptor
must be one of the caller's own open control files (R1, R2), so it takes a
file another process handed over, as it does natively. That the primary
client of every RM call is one of the calling file's own is RM's strict
client validation, which the backend checks the host has at start (R3).

### The guest module's parsers

What the module reads of a guest process's bytes -- IOCTL2's schema walk,
the v1 IOCTL marshalling with its nested blocks, deep pointers and segments
and the descriptors in them, OS-descriptor registrations -- is parsed in Rust
(`driver/rust/README.md`) on a kernel with Rust, unless `NVGPU_RUST=0` says
otherwise: a core with no `unsafe` and no panic path (the build checks the
object for panic symbols), which copies every byte of the caller's once and
decides on that copy, and one file of `unsafe` FFI around it. The guest
kernel the project builds has Rust (`driver/guest-kernel.defconfig`). The C
stays, frozen, as the fallback for a kernel without Rust and as the
difftest's oracle; a C module on a Rust kernel says so at load, and the
module says which parsers it has (`modinfo -F parsers`). Nothing moves across
the trust boundary: the backend still checks every request itself. The
ATOMIC special's parsing of the commit's arrays is in Rust too
(`nvgpu_atomic.c` in C); what it asks of `nvgpu_kms.c` -- object and property
classes, the fence bridge, event reservations -- stays C. The differential
test (`driver/rust/difftest`, the C compiled as it is and run with UBSan and
allocation canaries) holds the two equal.

The module is built for x86-64 with 4 KiB pages only (Kconfig depends on
`X86_64`; an out-of-tree build for anything else stops at `nvgpu.h`), and it
serves one virtio-gpu-nv device per guest. Every line a guest process can
cause in the guest's kernel log is rate-limited, most at debug level
(`dev_dbg_ratelimited`); the warnings kept name conditions an operator
should see, among them a thread killed while its request was in flight
(`nvgpu_xfer.c`) and GET_DEV_INFO asked in another release's layout
(`nvgpu_drm.c`: guest userspace and host driver differ). The host's device
details at probe are `dev_dbg`.

### 32-bit processes, and DRM structs of another size

Two kinds of caller the native kernel serves reach the guest module, and
neither brings the backend a byte it could not already be sent:

- *32-bit processes on the NVIDIA nodes.* `/dev/nvidiactl`, `/dev/nvidiaN`,
  `/dev/nvidia-modeset` and the two UVM nodes take `compat_ioctl =
  compat_ptr_ioctl`: the native handler, the pointer widened. nvidia.ko,
  nvidia-modeset and nvidia-uvm do the same -- each sets `.compat_ioctl` to
  its `.unlocked_ioctl` (nv.c:251-261, nvidia-modeset-linux.c:2015-2025,
  uvm.c:1074-1084, uvm_tools.c:2774-2784, 595.99.02), because RM, NVKMS and
  UVM parameter structs are fixed-width with 8-byte-aligned `NvP64`/`NvU64`
  fields, so a 32-bit caller's struct is the 64-bit one; none of the three
  reads `in_compat_syscall()`. A 32-bit process's ioctl is a 64-bit
  process's with its pointers below 4 GiB, which a 64-bit process can send
  too: the module's parsers (C and Rust) take every pointer as a u64 and
  copy through it, whatever its value, and nothing in them, in OS-descriptor
  pinning or in deep segments assumes an address above 4 GiB. What the
  backend holds each call to -- the RM allowlist's LP64 parameter sizes, the
  UVM table's sizes -- is what RM and UVM check natively, so a 32-bit build
  whose struct differed would be refused in the guest as on bare metal. The
  one thing a 32-bit process cannot do is map a UVM semaphore pool, which
  the module places only in [4 GiB, 32 TiB) (`nvgpu_mmap_uvm_check()`), so
  it gets no CUDA context (CUDA 12 dropped 32-bit applications). The DRM
  node already had its compat path (the core's ioctls through
  `drm_compat_ioctl()`, as nvidia-drm), and `/dev/nvgpu-wl` and
  `/dev/nvgpu-capture` had `compat_ptr_ioctl` with layouts identical at both
  widths (every `__u64` at an 8-byte offset, sizes multiples of 8);
  `/dev/nvidia-caps/*` answer no ioctl, as nv-caps.c's.
- *DRM ioctls whose struct grew.* The DRM node takes every ioctl it serves
  (nvidia-drm's range, syncobjs, semaphore-surface fences, a KMS file's
  KMS calls, the dumb-buffer pair) by its number alone and runs it on the
  caller's argument normalised as `drm_ioctl()` does
  (`nvgpu_drm_arg_in()`, drm_ioctl.c:848-915): the caller's size copied
  in where both its command and the native one say IN, zero-extended to
  the native struct, and the caller's size copied back, bounded by the
  14-bit size field (at most 16 KiB, on the stack up to 128 bytes). A
  16-byte `drm_syncobj_handle` (the Steam runtime's libdrm) is the native
  call with `point` zero. The handler and
  the backend see only the **native** command -- the kernel's own for
  syncobjs and dumb buffers, the release's schema entry for KMS
  (`nvgpu_i2_native_cmd()`, C and Rust, the difftest holding them equal),
  nvidia-drm's header for its range -- and the IOCTL2 interpreter still
  refuses any other size, so the backend is sent exactly the sizes a
  native-size caller sends. Only GET_DEV_INFO keeps its own rule: its four
  layouts differ in the middle, so each caller is answered in its own.

### Which host surfaces each display mode turns on

| mode | flag | host surfaces it adds |
|---|---|---|
| headless, v2 | none | IOCTL2 render class and NVKMS tables, fences, HOST_OP without OPEN_KMS, `/dev/udmabuf`, memfds |
| Wayland client, and the host's direct scanout of a guest buffer | `--wayland-socket` | compositor connections, `wlwire`, reader threads |
| DRM lease, VK_KHR_display | `--wayland-lease` | lessee files as KMS handles, nvidia-drm grants and NVKMS gates, uevent netlink, leasable monitors in the patched compositor |
| compositor VM | `--kms-card` | host card files as DRM master, OPEN_KMS and DROP_IF_MASTER, CREATE_LEASE, the three NVKMS commands above, uevent netlink. The guest owns the host's display and can show anything on it. |
| export | `--wayland-export` | the listener, host peers, import of host dma-bufs |
| capture injection | `--inject-socket`, `--inject-uid` | a listener for one helper uid, its fixed-size packets, PRIME import and IDENTIFY of its dma-bufs, import of its syncobjs; HOST_OP INJECT_OPEN and INJECT_OPEN_SYNCOBJ ("Capture injection") |

---

## 11. Frame pacing

Branch `frame-timing`. Games in a guest paced worse than natively; what was
found and changed is in DEPLOY.md, "Frame pacing". This section is what the
changes do to the boundary. None lifts a cap: the syncobj wait registrations
("Resource caps": 1,024 per VM, a quarter per process) were never reached in
any run (the counters below say so: no wait went over them, none backed
off), and they stand as they were.

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

**`--queue-poll-us`** (default 50 µs; 0 turns it off). The queue thread
keeps looking at the control ring for up to that long (capped at 1 ms) after
draining it. It is CPU the backend spends for the guest, as the drain itself
is: a guest that keeps sending keeps the thread busy either way, and the
poll adds at most the cap after each burst. It reads only the ring's avail
index, which the transport already reads. Leave it off where host CPU is
shared tightly between tenants.

**The guest's reply spin** (`rt_spin_us`, default 20 µs, 1 ms at most) and
**asynchronous fence WATCH** (`async_fence_watch`) are guest-internal. A
proxy's WATCH sent from a work item holds a reference to the proxy, so its
handle cannot be closed ahead of it; a WATCH the backend refuses signals the
proxy with the error -- a guest fence never
waits forever on a report that will not come.

**The event queue** does not ask the guest to kick while the pump holds
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
the default one). The backend sets its own threads' slice from
`--sched-slice-us` (default 100; 0 keeps the one it started with), keeping
a fair policy the launcher or unit chose and leaving a real-time one alone;
a launcher with a slice of its own passes it there too (BE-2.3). Nothing in the
guest can change it.
crosvm's `--core-scheduling=false` (`NVGPU_CROSVM_CORE_SCHED=0`) gives up
the per-vCPU core-scheduling cookies crosvm sets by default, which keep an
SMT sibling from running another task while a vCPU runs: a mitigation for
cross-thread side channels (L1TF/MDS-class) between the guest and host
tasks on the sibling. The launcher keeps crosvm's default; turn it off
only where no other tenant shares the cores.

**The guest's reply polling** (`driver/nvgpu_xfer.c`). While callers spin
for their replies (`rt_spin_us`, above) and none sleeps for one, the control
queue's interrupt is off and the spinning callers take replies off the ring
themselves. Guest-internal: the host sees fewer interrupts to deliver and
nothing else. Every ring operation that adds, harvests or toggles the
interrupt (`add_sgs`, `get_buf`, `disable_cb`, `enable_cb`) stays under the
transport's lock, as the interrupt handler's always did; the two that run
outside it are safe by design -- `virtqueue_notify()`, which the virtio core
allows unlocked, and reclaim's `virtqueue_detach_unused_buf()`, which runs
after the device reset and after `dead` has excluded every harvester. A
caller about to sleep for its reply -- an
executor-class request, or one that spun out -- turns the interrupt back on
first and takes whatever arrived while it was off (the virtio core's
`enable_cb` reports it), and no spinner turns it off while anyone sleeps: a
sleeper is woken by the interrupt, never by a spinner, so a spinner whose
vCPU the host deschedules delays only itself. Once the transport is dead (a
reset, a removal) nothing here touches the queue.

**Heavy workloads** (branch `heavyfix`; BENCHMARKS.md, "Heavy
workloads"). Four changes to the per-frame path, each holding what it
replaces to the same strength:

- *Classifying a descriptor the host made* (`hostfd::classify`) reads its
  `/proc/self/fd` link with `readlinkat` through an `O_PATH` descriptor of
  that directory kept for it, into a stack buffer, instead of formatting
  and resolving the path. The same text is matched against the same names
  (byte for byte; the names are ASCII, so exactly what the lossy
  conversion matched); a text that fills the 512-byte buffer, a directory
  that cannot be opened and a link that cannot be read through it all fall
  back to the path, and whatever cannot be read is still `Other`. The
  kept descriptor is registered as the backend's own (`privfd`), so no host
  call's answer can be adopted as it, and it is keyed by a fork count
  (`sys::proc::fork_generation`, a pthread_atfork handler): in a forked
  child, `/proc/self` as resolved in the parent would name the parent's
  table, so the child opens its own (and leaves the parent's number
  unclosed: it may be something else there now).
- *An IOCTL2 the schema says never waits* -- every render-node call but a
  few, all the syncobj calls -- runs from preparing to finishing under the
  one hold of the backend mutex that serving it takes, on the handle
  table's own descriptor for the target (`session.rs serve_ioctl2`). The
  duplicate it used to take was there so a CLOSE racing the host call could
  not pull the file away while the mutex was released; with the mutex held
  throughout, no CLOSE, bury or reset can change the table, so the
  descriptor is the one the call was checked against. Executor calls keep
  their duplicate and run unlocked as before, and a call without a
  duplicate that reached an executor would be answered as cancelled, never
  run on a bare number. What changes is who waits: the mutex's other
  holders, for the few microseconds such a call takes on the host.
- *A SYNCOBJ_DESTROY the guest can prove will succeed is posted*
  (`driver/nvgpu_syncobj.c`, "SYNCOBJ_DESTROY, posted"): the calling
  process is answered 0 at once and the request goes on the control ring
  ahead of anything sent after it, where the backend serves it exactly as
  a waited-for one -- the same IOCTL2, the same checks, its registrations
  orphaned, its reply reaped by the transport. Natively only a non-zero pad
  or a handle the file does not hold fails; the guest posts only for a zero
  pad and a handle its per-file map says the file holds, and that map never
  holds one the host does not (the invariant and why it holds are in the
  driver). Anything else goes synchronously and gets the host's own error,
  so a process still sees every failure it would natively. The backend's
  accounting is unchanged, since what it receives is unchanged; a posted
  request the host fails anyway is logged and counted by the guest's
  transport (`posted_failed`, `driver/nvgpu_xfer.c`).
- *The pump waits on an armed RM descriptor with `poll(2)`, and on an
  unarmed one not at all* (`pump.rs Pump::wait`). Such descriptors used to
  sit in the pump's epoll set whether armed or not, and a Vulkan game's
  driver woke the pump 150,000 times a second for events nobody waited on.
  They cannot be moved in and out of epoll, because RM's `poll` clears a
  dataless event as it reports it and epoll polls twice when a descriptor
  is added or modified with an event pending: an event arriving as the
  descriptor was re-armed would be lost, and a guest waiting on it would
  wait until its timeout. `poll(2)` polls each descriptor once a pass and
  returns what that pass found, and the arm's own look is a single poll
  too, so every event RM clears is one the pump reports. An unarmed
  descriptor's events stay in RM, where the next arm finds them. What is
  polled, and on whose behalf, is as before: the guest's own handles, only
  while it waits on them. At most 256 such watches of a VM are kept out of
  epoll (`POLLED_MAX`), so that no wake costs thousands of polls; any past
  that are in the epoll set for their life, as all were before.

---

## 12. What the backend enforces, and what it leaves to the host

**The backend enforces, from tables it holds:**

- which handle kinds take which calls;
- which RM controls, classes and VID_HEAP_CONTROL functions reach RM at all
  ("The RM allowlist"), and RM's parameter size for each control on a
  release measured exactly;
- sizes, from the RM profile, the UVM tables and the DRM and NVKMS schemas;
- that no guest pointer the tables name reaches the host: every such field
  is relocated to a buffer of the backend's or zeroed, and a buffer for
  several pointers of one block is exactly the size RM will copy, computed
  by the backend from that block (`device/src/deepseg.rs`). A pointer field
  the tables miss reaches the host as the guest's bytes ("Memory safety");
- that every descriptor field the tables name names a handle of an allowed
  kind, and becomes the backend's own descriptor (the ones no table names
  are open item 11);
- every refusal in "The host GPU surface";
- the NVKMS grants and gates, framebuffer ownership, KMS master, the CRC gate
  and the property rules in "NVKMS, KMS and leases";
- the semaphore-surface bounds, ownership and caps;
- that RM shares stay inside the VM, a duplicate's clients are the VM's and
  one guest process's (or shared between them), and a second client named
  in parameters is the VM's and passes RM's rule for its field ("RM objects
  between guest processes");
- that nothing only compute uses is reachable without `--allow-compute`
  ("Compute is opt-in");
- which UVM pools the VMM maps into the aperture, at what address, and how
  many;
- the host-PID answers;
- the Wayland allowlist, descriptor classes and budgets;
- every cap in "Resource caps", and each guest process's share of the
  VM-wide ones;
- that a descriptor RM resolves in the backend (NV0000's OS_UNIX
  controls, the event and fd-carrying escapes) is one of the caller's own
  files, or the call does not reach RM;
- its own privileges and socket.

**It leaves to the host:**

- **RM's checks on every control and class it forwards**, at user rather
  than admin privilege: the allow-listed ones only ("The RM allowlist").
  This is still the largest part.
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

## 13. Open items and residual risk

Every open item, in rough order of weight. Each names the finding ids it
carries; Appendix C has their history.

1. **RM_CONTROL and RM_ALLOC are allow-listed, and what is allowed is still
   a host surface.** 216 of 610.57.04's 1,370 controls and 97 of its 222
   classes reach RM ("The RM allowlist"); each is checked by RM as it would
   check a user process without admin rights, and a bug in any of them is a
   host bug. The list covers what 106 hardware runs and the application pass
   used, and the other architectures' classes for the same objects. A call
   it misses fails with RM's own "not supported" and a log line naming it;
   `--rm-allowlist=log`, a diagnostic flag, finds them without failing.
   30 controls go to GSP-RM with no CPU-side table and no public name; they
   are allowed by number because nvidia-smi and the drivers sent them. The
   list keys on the command, not on the object it is sent to, so an allowed
   control sent to an NV2081_BINAPI handle goes to GSP-RM without CPU-RM's
   size check (the backend holds RM's size itself on a release measured
   exactly). Several allowed controls change state other tenants and the
   host see (the ZBC setters 0x9096010{1,2,5}, TIMER_SET_GR_TICK_FREQ, the RC
   watchdog's enable and release, PERF_BOOST, MC_SERVICE_INTERRUPTS,
   SET_TPC_PARTITION_MODE, GPU_DETACH_IDS); none has a recorded decision yet
   (BE-1.20).
2. **The host NVIDIA driver is in the TCB.** There is no IOMMU boundary. A bug
   in any forwarded path, or in RM's page tables for a guest's GPU work, is a
   host bug. For mutually untrusted tenants, VFIO with an IOMMU or vGPU is
   still the answer.
3. **The backend's sandbox is only as strong as the kernel under it.** One
   process per VM maps all of that guest's RAM, holds every host descriptor,
   and parses everything "What the backend parses" lists. Its sandbox keeps
   a compromised one to the GPU's nodes and a few read-only files, without a
   network, on a list of syscalls; ioctl on those nodes is the host kernel's
   GPU surface, whole.
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
   - The network layer is reported in force whenever `/proc/self/net/dev`
     shows only `lo`. On a host with no other interface that is the host's
     own namespace, and below Landlock ABI 10 its loopback UDP stays
     reachable (BE-1.20).
4. **The isolate is not built.** ARCHITECTURE.md, "Future work: the
   isolate", describes one helper per guest process holding the descriptors.
   The display paths share objects across a VM's processes (a compositor
   holds its clients' buffers) through one VM-wide handle table, so a helper
   per VM is probably the first one to build. That is reasoned, not tried.
5. **Memory and descriptors.** Outside the Wayland and window budgets, the
   backend's memory is bounded only by the cgroup the shipped unit gives it
   (`MemoryMax`), and under the rig's launcher only by the run's scope. The
   handle table is sized from RLIMIT_NOFILE, with a share per guest process
   (B1), but the handle table, blobs, pools, streams and adopted compositor
   files do not yet draw from one descriptor budget (S-16, S-7). A guest
   process that forks enough can still take a pool, a share at a time.
6. **Registered guest memory is released by a list of holders.** The
   backend tells the guest to unpin memory registered by its pages only
   when the RM objects, duplicates and UVM external mappings holding it are
   gone, and refuses to export it ("Memory registered by its pages").
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
7. **RM objects between guest processes rest on the guest kernel and on
   reading.** Which guest process made a client, as which euid, and which
   makes a call is the guest kernel's word ("Threat model"); a guest module
   that cannot say fails closed. Grants are followed on the object and on
   its client only; a CLIENT grant on a device or other intermediate object
   that RM would honour for the objects under it is refused here, a client's
   grant stops counting once any of its objects has a list of its own, and
   any free in a client drops its objects' grants. Each second-client
   field's rule was read from 610.57.04's CPU-RM sources; where RM defers to
   GSP-RM the backend assumes GSP-RM checks nothing between host processes
   (its token is the GFID or none) and requires the calling process, which
   may refuse a cross-process tool (a profiler or debugger of another
   process of the same user) that works natively. RM's USER_ROOT bypass of
   the token checks is not given to guest root. The on-device checks,
   sec-negative T8 and T10, pass on the RTX 5090; none of the 88 rig runs
   checked before the application pass issued NV_ESC_RM_SHARE.
8. **RM objects of other clients, where the backend adds no rule.**
   SEMSURF_FENCE_CTX_CREATE accepts any client of the VM, not only the
   caller's own (R4; native strength: KAPI duplicates at kernel privilege;
   it needs the caller's process on IOCTL2, which carries none). IDLE_CHANNELS
   in list mode does not check that `phClients[]` are this VM's (native
   strength; BE-1.19). `uvm_client_ok` holds a UVM call's client to the
   control file it was made on, not to the calling process, so two processes
   sharing that file are one to it (D2, BE-1.20). The export and import of RM
   objects through a descriptor is held to native strength (F2).
9. **The UVM aperture.** Its safety rests on the VMM doing what "The UVM
   aperture" says: `MAP_FIXED_NOREPLACE` or crosvm's reserved band, the page
   check, slot before mapping, and reading an unslotted aperture address as
   nothing. The backend cannot see any of it. Two guest processes whose pools
   overlap cannot both have one mapped, so the second CUDA context fails, and
   one process can find another's pool address, or squat on it, by trying
   (B5). nesbox does not reserve the band: with a stack rlimit that moves its
   mmap area into the band, a pool can collide with nesbox's own mappings
   (F3).
10. **Backend posture.** Class 0x3f (RegisterMemory) is on the RM allowlist,
    since the Vulkan driver allocates one; RM maps its BAR0 only for an admin
    client, which the backend never is. Non-privileged RM clients are not
    forced, which matters only if the backend is ever given CAP_SYS_ADMIN
    (S-5). PID namespaces are left to the launcher, and the host-PID controls
    are answered by the backend (S-24). EXPORT_TO_DMABUF_FD is refused, so
    NVIDIA's GBM RM export path and `cuMemGetHandleForAddressRange(DMA_BUF)`
    fail (S-15).
11. **Untranslated descriptors.** Some RM controls carry a descriptor that
    nothing translates: CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL's devDescriptor, the
    NV00E0 export and NV00FD ATTACH_GPU devDescriptors, and NV00FD
    REGISTER_EVENT's pOsEvent. The guest's number reaches RM as a number in
    the backend's table. The fabric classes are refused, which keeps this low
    rather than none. (NV0000's OS_UNIX controls are translated or refused:
    R1.)
12. **KMS.**
    - One executor lane per class per VM, and a lower ALLOC_DEVICE cap, are
      not done (S-8).
    - Hotplug uevents do not clear the dpy probe cache.
    - The GET_LEASE check does not run again at run time (S-14).
    - S-11's three edge cases, not re-checked since: re-homing in the
      reverse direction, a host handle leaking into the render file on a KMS
      -EAGAIN, and a possible reference cycle between borrowing re-home
      entries.
    - Window pages stay mapped in guest processes after the device is
      removed: the guest module has no `unmap_mapping_range()` (S-26; calls in
      flight at removal are guarded since the device's lifetime was fixed).
    - The pump's duplicate of a display file can delay a master release
      until UNWATCH (S-33).
    - There is no single "open non-master" OPEN_KMS op, so the open and the
      drop are still two steps (S-34).
    - A revocation waits for a gated NVKMS call already running while it
      holds the backend's lock: one process's REVOKE or close during
      another's SET_MODE stalls every RM call of the VM (BE-1.20, with B6).
    - In compositor-VM mode the guest module's master hooks make round trips
      under the DRM core's `master_mutex`: a stalled backend holds every guest
      open of the card node and every SET/DROP_MASTER behind it -- the
      guest's own compositor and nothing else (GM-S7).
13. **Wayland.**
    - No per-interface object caps (S-7), no CONNECT churn limit, and no
      refusal of SHM_SYNC before commit (S-4).
    - No backpressure towards the compositor (S-18's optional item 3).
    - The lease cache is not cleared on compositor restart (S-30).
    - The Hyprland patches have no lease debounce (S-9), and
      `setLeaseOffered(false)` does not end an active lease. Short of ending
      the VM or unplugging the monitor, the host cannot take a leased
      monitor back.
    - A client may hold a stream sink's share of the queue budget by never
      reading its pipe, as it may hold a queue by never reading its socket;
      both are its own process's share (FW-4, WL-S6).
    - On a guest kernel without CONNECT_FOR (`ENOTTY`) the daemon's
      connections are charged to the daemon, which then holds one process's
      share of the VM's channels for all its clients (WL-S7).
    - Every guest app reaches the host compositor as the backend: a
      compositor permission given to one is given to all (W4).
14. **Fences.**
    - The optional guest hardening for S-13 is not done: the guest does not
      re-poll a joined wait's point before it signals the eventfd.
    - An 0x57 attach is mirrored into the guest's reservation object, but
      export mode is not bridged (L-1).
    - A process that forks children to make orphan wait registrations on
      shared syncobjs and exit can still fill the VM's pool; the cost is
      polling latency for the VM's other waits, not host memory.
15. **Fairness within a VM.** One queue thread serves every inline host
    call, and 16 executors every file (B6). Fence wait registrations,
    rmshare grants, rmmem records and the lease throttle are VM-wide (B7):
    each is a degradation (polling, a refused share, a write-combining
    guess), not a denial of the device.
16. **The guest module.** `osdesc_early` is a ring of 64: a reap that names
    more unrecorded ids than that before their registrations' replies are
    read loses the oldest, whose pins then stay until the device is removed
    (a list would let a backend grow it without bound). A registration
    abandoned in flight whose reply never comes (a garbage backend) keeps its
    pins until removal.
17. **Capture injection.** The helper and the guest's capture daemon are the
    integrator's, and so is how the helper proves consent; buffers whose
    planes are separate objects are refused (one dma-buf per buffer in the
    guest ABI); a guest open whose reply its caller abandoned leaves its GEM
    or syncobj handle in the caller's file until it closes; an object a guest
    still holds through a handle no open recorded (a GETFB of a framebuffer
    made from it) can be exported after its id and every open are gone; a
    real portal stream has not been injected by the project
    (`rig/rig-tools/portal-identify.sh` checks one, for the owner to run).
18. **The VMMs and the deployment.**
    - crosvm pins the device's ioevents to BAR0's first address: a guest
      kernel that moves BAR0 before the driver binds has its ioevents
      refused, and the device goes NEEDS_RESET. It fails closed; DEPLOY.md
      says not to boot a guest with `pci=realloc` (VD-C5).
    - A backend user with other live processes can signal the backend and,
      with `kernel.yama.ptrace_scope` 0, trace it before its first
      `PR_SET_DUMPABLE(0)` (VD-H1).
    - The backend's run-time refusal of the VMM's uid as a capture helper
      holds only when its vhost-user sockets are connected to one uid; with
      more than one it warns and refuses none (VD-H4).
    - The root launcher's first shell runs with what sudo let through
      (VD-H6).
19. **Behaviour that is correct and still worth knowing.**
   - In compositor-VM mode the guest owns the host's display, and could draw a
     convincing host login screen.
   - A guest client in Wayland mode reads the host clipboard when focused.
   - An export-mode peer is driven by the guest.
20. **The on-device negative test has a hole.** In
   `rig/verify/sec-negative.c`, the VID_HEAP_CONTROL half of T1 sets
   function 8 where ALLOC_OS_DESCRIPTOR is 27 (`nvos.h`), and sends a
   1,064-byte block where the escape is 184. The backend refuses it on size, so
   it passes without testing what it names. The backend's unit test
   (`device/src/guestptr.rs`) covers the real case.
21. **Parity and structure items left from the 2026-09-29 review.**
    Request fields the policy rewrote come back altered in the reply
    (ALLOC_DEVICE's scrubbed fields, QUERY_DPY_DYNAMIC_DATA's cleared
    overrides, DECLARE_EVENT_INTEREST's narrowed mask, SYNCOBJ_WAIT and
    TIMELINE_WAIT's `timeout_nsec`) where the native driver returns the
    caller's own (BE-2.5). RM's pointer tables are unions across releases:
    an offset that is a pointer in one release and data in another would get
    a backend address as data; nothing on the allowlist is affected today
    (BE-1.19). The UVM and class-parameter generators do not scan for new
    host fields (BE-1.19). Every IOCTL2's duplicate target descriptor is
    dropped on the closer thread, uncounted while the closer is stalled
    (BE-1.20). Without `--allow-compute`, NVIDIA's Vulkan driver still lists
    the ray-tracing and CUDA-backed extensions it cannot create a device with
    ("Compute is opt-in"); hiding them is not done. The Wayland review's
    structure items WL-R3 and WL-R5 to WL-R9 are left for later.
22. **Still to measure or run.**
    - The per-present crossings (L-7) are measured for the Wayland mode only:
      17 round trips per presented frame (DEPLOY.md, "Frame pacing"). No
      other display path has been timed.
    - The envyhooks run that checks the M-6 fix, descriptor fields no longer
      coming back holding backend handles, is not done (M-6).
    - Whether any caller reads a proxy fence's timestamp is still to be
      confirmed on the device (L-4).
    - Never run on hardware: the compositor-VM and export modes, hotplug.
    - Not yet run on hardware: the code at this document's commit as a
      whole (see "Status and how to read this"); a root run of the launcher
      (the backend started by `systemd-socket-activate` when the jailed
      nesbox connects; the capped console under a real guest); the
      socket-activated units, with and without the inject socket; the VMM
      templates; nesbox `v6`'s jail; crosvm `0010`'s spare vCPU under
      `--host-cpu-topology`; the 2026-09-29 guest-module fixes (the
      dead-transport path, the polls, the majors, the work queues, WL RECV).

---

## 14. Roadmap

In priority order. Cost is a judgement, not a measurement.

1. **Run the rest of [`TESTING.md`](TESTING.md)**: the regression of the
   code at this document's commit, the compositor-VM and export modes,
   hotplug, the deployment pieces not yet run (open item 22), and the
   performance stages. Groups A and B, the
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
   handle table sized from it (B1); what is left is to size one per-VM
   descriptor budget from it. The handle table, blobs,
   pools, streams and adopted compositor files would all draw from that
   budget, and MAX_HANDLES would come from it rather than a constant (S-16,
   S-7).
4. **A PID namespace per backend** (`PrivatePIDs=yes`, or `bwrap
   --unshare-pid`). With the namespace, RM filters the host-PID controls
   itself; `rmctl.rs` then stays as a second fence. (A uid per VM is done:
   "One uid per VM".) For the Wayland modes, give
   the backend a `wp_security_context_v1` socket from the desktop session
   rather than running it as the desktop user. Hyprland then filters globals
   for those clients itself (`filterGlobals`, `src/Compositor.cpp`). Its list
   lacks three of this proxy's additions: the lease device, xdg-output and
   content-type. It already has the syncobj manager (and `wl_drm`, which this
   proxy leaves out). Leasing would need a Hyprland patch; explicit sync would
   not.
5. **The RM allowlist: done and run.** Default deny, enforcing, run through
   Groups A and B and the application pass (NVENC, NVDEC and Vulkan Video
   included; six controls added from it). What is left: holding each control
   to the classes of the object it is sent to ("The RM allowlist", "What it
   does not do"), and growing the list from the teardown's "RM allowlist
   refused" lines as new workloads meet it.
6. **seccomp and Landlock per backend: done and run** ("The backend's
   sandbox"): no run of the regression or the application pass stopped the
   filter. An `ioctl` filter by request number per descriptor kind is not
   possible in a filter, and would take the isolate.
7. **The isolate**: the host descriptors held by an unprivileged helper apart
   from the process that maps guest RAM, so that a parser bug in the backend no
   longer yields the descriptors, and a driver-call bug no longer yields guest
   memory. Probably one per VM (open item 4).
8. **The rest of the open items.** In rough order:
   - one NVKMS and KMS executor lane per VM, and a lower ALLOC_DEVICE cap;
   - per-interface Wayland object caps and a CONNECT rate;
   - the Hyprland lease debounce, and `setLeaseOffered(false)` ending an
     active lease;
   - forcing non-privileged RM clients, as a second
     fence against a privileged backend;
   - translating EXPORT_TO_DMABUF_FD, once something needs it.
9. **Parse, don't patch: the rest ("Memory safety").** The data rewrites
   that are still edits of the host's copy (the coherency attributes, NVKMS
   policy, fence waits) made declared values; RM's top-level blocks as typed
   structs per release.
10. **What the `harden` audit left open.** (A uid per VM in the launcher is
    done.) The UVM aperture carved out of the VMM's address space before
    guest RAM is mapped (F3; done for crosvm, "The VMMs", open for nesbox);
    a caller's process on IOCTL2 (R4); fair queuing per guest file (B6); the
    remaining VM-wide pools split per process (B7); a security context per
    Wayland channel (W4).
11. **What the 2026-09-29 review left open.** A decision, recorded in "The
    RM allowlist", for each allowed control that changes GPU-wide state
    (BE-1.20); request fields restored in the reply as the native driver
    returns them (BE-2.5); the generators' scans for host fields in class
    parameters and UVM descriptor fields (BE-1.19); the network layer's
    check by namespace rather than by interface list (BE-1.20).

---

## 15. How the claims are tested

- **Unit tests** (`cargo test --workspace`, `scripts/ci.sh fast`): each fix
  of a finding has a test that fails without it, against fake kernels that
  answer as the real driver does (`device/src/testing/`, the fake nvidia-drm
  of `device/src/inject/fake.rs`, the fake VMM).
- **The guest module**: the difftest (`driver/rust/difftest`) holds the C and
  the Rust parsers equal, with UBSan and allocation canaries; `scripts/ci.sh
  kernel` builds both without a warning.
- **Fuzzing** ("Fuzzing and Miri"): a target for every parser of what a guest
  or a Wayland peer sends (`fuzz/fuzz_targets/`: backend, backend_v2,
  deepseg, guestptr, inject, misc, nvkms, osdesc, rmshare, vring, wl_codec,
  wl_engine), and the guest parsers' differential targets
  (`driver/rust/fuzz`: diff_i2, diff_rm, diff_atomic, i2_raw), nightly
  (`scripts/ci.sh nightly`).
- **Security negatives on the device** (`rig/verify/sec-negative.c`, run by
  the rig's `secneg` stage on the control, render and lease nodes): T1 an OS
  descriptor by address (with the hole in "Open items and residual risk"),
  T2 an RM control's embedded pointers, T3 0x54's index, T4 GRANT_PERMISSIONS
  SUB_OWNER, T5 ADDFB2 of handles the file did not make, T6 GETFB of another
  file's framebuffer, T7 RM_SHARE of type ALL, T8 and its positive control T9
  a duplicate between two guest processes, T10 a second client named in
  parameters of another guest user, T11 an export descriptor that is not the
  caller's file; and the KMS tests on a card or lease.
- **The deployment**: `scripts/ci.sh deploy` evaluates the NixOS module with
  the configurations it must refuse, checks the units with `systemd-analyze
  verify`, applies `patches/crosvm` to its base, holds the VMMs' limits to
  the backend's (`scripts/vmm-parity.py`) and runs the launcher's dry run.
- **Hardware** ("Status and how to read this"): the rig's stages
  (`rig/TESTING-RIG.md`) and the application pass. Appendix A says, round by
  round, what ran on the RTX 5090 and what did not.

---

# Part II: history and reference

The appendices are append-only. They say what each review round found and
did, as it stood then; a status in them is history, and Appendix C's is the
current one. Finding ids that collided between rounds carry a prefix (see
Appendix C).

## Appendix A. Review history

This document began as the security review of branch `display-passthrough`
at `ae182ab` against `dev` at `50ff74a` (A.1, and Appendix B), and each later
round was added to it: the audit of branch `harden` (A.2), the RM allowlist
of branch `rmallow` (A.3), the fuzzing of branch `fuzz` (A.4), the
memory-safety structure of branch `dind` and the review of it for memory
passing (A.5), the guest parsers in Rust (A.6), the VMMs it runs under (A.7),
the review of 2026-09-26 (A.8), capture injection (A.9), the window's size and
share (A.10), the frame-pacing changes ("Frame pacing"), the performance work
(A.11) and the review of 2026-09-29 (A.12). Part I was made from it on
2026-09-29. Section numbers the rounds cite are the document's numbers at the
time; they are given here as the Part I section or appendix they became.

### A.1 The design, verification and security reviews (`display-passthrough`)

Three reviews, each by independent reviewers, with every claim sent to
adversarial verifiers who tried to refute it (a fourth, of branch `harden`,
is A.2):

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

The findings of the verification review (C-1, H-1 to H-5, M-1 to M-10, L-1 to
L-9; [`docs/review/NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md))
and of the security review (S-1 to S-35; `docs/review/FINDINGS.md`) are
indexed in Appendix C with their fix commits.

Rejected by the security review:

- "There is no sandbox, and the planned per-process isolate cannot hold the
  display paths." The missing sandbox was already known (since built: "The
  backend's sandbox").
- "Host CLOCK_MONOTONIC and the host's DRM dev_t numbers are disclosed to
  guest users." The review did not count that as a finding. The dev_t numbers
  reach every guest user. The clock offset, in `/dev/nvgpu-wl`'s HELLO, now
  reaches only root and the `nvgpu-wl` group by default, because the node is
  0660 (S-10).

### A.2 The `harden` audit

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

#### Findings

| id | sev | lens | finding | fix | what was done |
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
| W3 | medium | app vs app | at the VM's queue budget the connection whose output crossed it was dropped, often an innocent one; the rig's app pass (`6263e20`, on `display-passthrough`) puts the app user in `nvgpu-wl`, so any app can hold raw channels it never reads | `b5980e5` | fixed in the backend: the queue budget is per process, so the connection that crosses its share is its own. The guest-image side is fixed too: `rig/guest-image/probes/apps.sh` (`nvgpu_user=1`) no longer adds the app user to `nvgpu-wl`, and gives the group to the daemon alone (A.8, "The Wayland proxy") |
| F2 | low | app vs app | export/import to a descriptor is a second way to move an RM object between clients, outside the DUP_OBJECT gate | -- | native strength, documented ("Inside the guest"): after R1/R2 the descriptor must be a control file the caller has open, which another process can only have handed it |
| F3 | low | host surface | the aperture band [4 GiB, 32 TiB) can hold the VMM's own mappings, guest RAM among them, when its stack rlimit is unlimited (or above about 96 TiB: x86 then starts the mmap area near 21 TiB): a pool there fails, and says the address is taken | crosvm `patches/crosvm/0007`, `0008` | **fixed for crosvm, open for nesbox.** crosvm reserves the band (PROT_NONE, MAP_NORESERVE) at the start of `run_config`, before it maps guest RAM, and maps each pool with MAP_FIXED over its own reservation ("The VMMs"); UVM refuses a pool moved in with mremap, so the window's map-then-move cannot be used. nesbox would need the same change to its start-up order, and a run to trust |
| F4 | low | host surface | CARD_INFO, ATTACH_GPUS_TO_FD and NUMA_INFO pass with no size check | -- | unchanged ("The host GPU surface"): the argument is never smaller than `_IOC_SIZE`, and ATTACH_GPUS_TO_FD, read again (nv.c), carries GPU ids only, no descriptor, once per file |
| R4 | low | app vs app | SEMSURF_FENCE_CTX_CREATE accepts any client of the VM, not the caller's own | -- | open, native strength (KAPI dups at kernel privilege). It needs the caller's process on IOCTL2, which carries none; the render file's opener is not the same thing |
| R5 | low | app vs app | negative descriptor values other than -1 forwarded, read by the backend as handles | `fc7a58b` | fixed: refused in the guest and the backend |
| B6 | low | app vs app | one queue thread serves every inline host call, and 16 executors every file | -- | open: needs fair queuing per guest file and Wayland traffic off the RM queue |
| B7 | low | app vs app | fence wait registrations, rmshare grants, rmmem records and the lease throttle are VM-wide | -- | open: each is a degradation (polling, a refused share, a write-combining guess), not a denial of the device; the same Ledger split applies when needed |
| W4 | low | app vs app, cross-VM | every guest app reaches the host compositor as the backend: a compositor permission given to one is given to all | -- | open: a `wp_security_context_v1` per channel needs a listening socket per channel from the backend and a compositor that keys permissions on it |

### A.3 The RM allowlist (`rmallow`) and the application pass

Branch `rmallow`: a default-deny list of the RM controls and classes a guest
may reach, per host release (`gen/rmallow_extract.py`, `gen/src/rmallow/`,
`device/src/rmallow.rs`). Before it, every control RM exports and every class
it can make, but 15 and 12, reached the host's RM from any guest process.

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

### A.4 Fuzzing (`fuzz`)

| id | sev | finding | fix |
|---|---|---|---|
| Z1 | critical | a nested parameter block sent shorter than the size field the host copies by (RM_CONTROL's paramsSize) had RM read a pointer field cut short as the guest's low bytes over the guarded buffer's zeroed slack, past the pointer scrub, which read only the bytes sent: a guest-chosen address in the backend that RM reads and writes through (FIFO_GET_CHANNELLIST and every control with a pointer). Default configuration, graphics included | `20434fa`: the size the host copies must equal the block sent, or be zero, on every nested path |
| Z2 | low | v1 host calls were handed a pointer taken from a slice of the guest's length, while the driver copies `_IOC_SIZE`; the nested block's address was taken before the writes that relocate its pointers. Undefined behaviour under Stacked Borrows (Miri), not known to miscompile | `5e62911` |
| Z3 | low | `serve` could answer a malformed request with more bytes than the capacity posted; the vhost-user transport never posts that little | `0b320bb` |

Nothing of it has run on the GPU; the fuzzers run with no device at all.

### A.5 Memory safety (`dind`) and its memory-passing review

Branch `dind`. A change of structure, not of behaviour: every test that
passed passes (717 with the new ones, 4 skipped), the guest module is
untouched, and the fuzz targets run against a stricter fake host.

**Where `unsafe` is.** Before, 376 uses of the keyword in 37 files: 333 in
`device` (the dispatcher, the window, OS descriptors, the sandbox, the pump,
the fakes of most test modules), 31 in `wlwire`, 12 in the guest daemon.
After it, 172 in 10 files, 15 of them test- or fuzz-only, all in the `sys`
modules "Memory safety" lists.

The fuzz targets' fake host now follows a pointer only into the call's own
blocks (`Arg::reach`), which caught one regression in the first commit of
this work (`f4cd317`): a single deep pointer the guest placed across a pointer RM
follows (FIFO_GET_CHANNELLIST's at 8 and 16, the deep one at 12) had the
scrub skip the overlapped field, so RM would have read four bytes of the
guest's and four of an address of ours as one pointer. The old in-place
scrub zeroed it by the order of its writes. Such a call is now refused
(`scrub_control`, EINVAL), with a unit test.

In IOCTL2, before this work, the guest's pointer bytes sat in the host's
copy until `aim_pointers` overwrote them; now each is declared as the walk
meets it. What went with it: a lifetime bug class (a relocated buffer
dropped before the call -- `_idle_segs` was held alive by name), an address leaking back
in a reply whose restore was skipped, a guest descriptor number reaching a
descriptor field, and an OwnedFd made of a number the kernel did not write.

An adversarial review of `dind` (at `69c3abc`) for how memory moves between
the guest, the backend, the VMM, the host kernel and other VMs, against the
product's requirement: no app less protected from another, no VM less
protected from another, than natively. Read from source; the fixes are
unit-tested and the guest module builds clean; at the time none of it had
run on a GPU (it has since: `rig/TESTING-RIG.md`).

| id | sev | lens | finding | what was done |
|---|---|---|---|---|
| D1 (M1) | high | app vs app | A second MMAP of a file the backend had placed without a record (the control file's ALLOC_MEMORY mappings, and every DRM object) got the first placement back whatever size it asked for, and the guest driver mapped the size it asked for from the placement's offset: an app could map past its own extent into the window's next ones -- other apps' device memory -- or into unplaced window, which stops the VM on the first touch | fixed: the backend refuses a request larger than the placement (`map_unrecorded`), and the guest driver refuses a vma, or a GEM object, larger than the placement the reply names (`nvgpu_mmap`, `nvgpu_gem_place_in_window`) |
| D2 (M2) | high | app vs app (`--allow-compute`) | UVM's REGISTER_GPU_VASPACE, REGISTER_CHANNEL, MAP_EXTERNAL_ALLOCATION and ALLOC_DEVICE_P2P name an RM client and object that UVM duplicates from a kernel client of its own; RM's check there is that the source client's process is the caller's (cliresShareCallback, PID policy), which every client of the VM passes, since all are the backend's. Any guest process could map another's GPU memory into its own UVM VA space, or register another's VA space or channel. Not cross-VM: another VM's clients are another process's | fixed: the client must be one this VM allocated on the control file `rmCtrlFd` names (what UVM's "Bug 1624521" TODO describes), so the caller holds the file the client was made on; a zero client passes |

What the review found holding, for the questions it was asked:

- **Guest RAM** is read once, from the chain copied out of the ring before
  anything is parsed (`vring.rs`); nothing parses guest RAM in place.
  Replies go only into the writable descriptors the guest posted.
- **Host blocks** are guarded mappings of their own with a readable slack
  page and a guard page, so a host write past a block faults (EFAULT) rather
  than landing on other backend memory; every pointer the tables name holds
  0 or an address the arena owns. What the tables miss still reaches the
  host as the guest's bytes ("Memory safety"): the fuzzers' fake host follows the same
  tables, so it tests the arena, not the tables.
- **The window** holds only descriptors of this VM's handle table, placed
  by the VMM inside its reservation; withdrawn ranges become PROT_NONE
  anonymous memory in the VMM, whose touch stops only this VM. A guest vma
  keeps its placement until its last vma (splits, forks, mremap) closes.
- **Registered memory and the UVM aperture** reach only this VM's guest RAM
  and this VM's UVM pools in the VMM's own address space; a holder missed in
  the release list (open item 6) exposes guest pages, never the host's.
- **Across VMs**, each VM has its own backend and VMM process; the host
  kernel's shared namespaces (RM clients, framebuffer ids, GEM names) are
  held to the VM by RM's per-file client validation (R3), the
  framebuffer rule (S-6), refusing FLINK and GEM_OPEN, and the PID policy
  above. A uid per VM ("One uid per VM") keeps one VM's processes from another's by the
  kernel's own rules, but it does nothing for a bug in the host kernel or
  the NVIDIA driver, which every uid reaches.

### A.6 The guest parsers in Rust (`rustguest`) and 32-bit callers (`drm-compat2`)

Branch `rustguest` added the guest module's parsers in Rust, built then
with `NVGPU_RUST=1`; they are the default on a kernel with Rust since
`1d33f59` ("The guest module's parsers"). Branch `drm-compat2` added the
32-bit and struct-size callers ("32-bit processes, and DRM structs of
another size"). Nothing moved across the trust boundary: the backend still
checks every request itself. The differential test
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

### A.7 The VMMs' placement fixes

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
VMM aborts rather than leave something unknown behind the slot (A.8). A
hugetlbfs descriptor never gets this far: its mapping is a whole huge page,
and moving it in would replace up to a gigabyte past the checked range --
nesbox takes only NVIDIA and DRM character devices into the window (below),
crosvm's arena refuses hugetlbfs.


### A.8 The 2026-09-26 review

A review of the guest module, the backend, the Wayland proxy and the VMM
launchers, each finding checked before it was fixed. The hardware status
at the time:

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
> things stood at the time; this note was the state then.

#### Fail closed, and production hardening

As written at the time; "Fail closed" in Part I is the current state.

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
`NVGPU_SANDBOX=off` or `NVGPU_ALLOW_ROOT_UNSAFE=1`); since VD-H3, as root it
refuses every one of them without `NVGPU_DIAGNOSTIC=1`, and it says each on
the terminal.

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
guest as RM's API-mismatch error -- since BE-2.1 with RM's reply word and
version string copied back as nvidia.ko copies them -- with RM's usual `NVRM: API mismatch`
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

#### Clean-up

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
semsurf, xfer and one site of nvidia/placement.rs's `map_unrecorded`, which a
concurrent isolation branch is rewriting. (Since then `scripts/ci.sh fast` runs clippy with `-D
warnings` over the whole workspace, so no such site is left.)

#### Backend isolation (branch `fix-backend`)

A review of the backend for cross-VM and app-to-app reach and for denial of
service (the review's own notes, not shipped; numbered as there). The
items below are the ones branch `fix-backend` took; the rest -- 8, 15-18 and
the logging and cleanup items -- are the hardening branch's. Each was checked
against the code and, where RM's behaviour decides it, NVIDIA's 610.57.04
sources; each fix has a unit test that fails without it. Each was tested
against fake kernels first; the hardware regression has run over all of them
since (below).

| id | sev | finding | what was done | commit |
|---|---|---|---|---|
| FB-1 | high, cross-VM | ALLOC_OS_EVENT and FREE_OS_EVENT reached RM with the guest's hClient unchecked. RM keeps OS events in one host-wide list matched by (hClient, fd), checking neither against the caller (osapi.c allocate_os_event, free_os_event; os.c osUserHandleToKernelPtr), and every backend's descriptor numbers are small: a neighbour's client (handed out in sequence) let a guest free that VM's events or take the key its next one needs | fixed: the client must be one this VM allocated and has not freed (the semsurf client record, `rm_share_gate`), else NV_ERR_INSUFFICIENT_PERMISSIONS without RM. Not "made on the calling file": RM posts an event to the file the call is made on (nv_post_event, `event->nvfp`), the file the caller then polls, which is not the one its client was made on -- that rule would refuse every legitimate event | 9a9d3aa |
| FB-2 | high, DoS | a fence context's GEM could be PRIME-exported (HOST_OP); the dma-buf kept the context -- a host kthread, a timer, an NVKMS duplicate -- alive after GEM_CLOSE gave its slot back to the caps | fixed on HOST_OP: PRIME_EXPORT of a live fence context is refused (EINVAL). Not on every path: the 2026-09-29 review found the Wayland proxy's PRIME export, of a GEM a DMABUF descriptor in WL_SEND names, made without this check (WL-S1). Nothing legitimate exports one: nvidia-drm's object has no sg table, Vulkan and EGL use a context only through 0x55-0x57, and the guest driver's own proxy refuses export (`nvgpu_fence_ctx_export`) | d7c41fc |
| FB-3 | medium, app vs app | RM_FREE and FREE_OS_EVENT wiped the backend's records whatever RM answered: one guest process freeing another's client, which RM refuses, broke the owner's duplicates, fence contexts, OS events and grants | fixed: forgotten on NV_OK, or when the free came through the file that made the client (or the event) | 9a9d3aa |
| FB-4 | medium, compute | UVM external mappings of registered memory were held whatever UVM answered and wherever they lay, against one VM-wide bound: holds nothing took down (pinned to the session's end), and one process filling the bound refused every other's | fixed: the mapping must lie in an external range the file made and the backend recorded (overflow-checked), or it is refused before UVM; held on NV_OK and on the failures of UVM's page-table wait, which leave mappings up (RC, ECC, GPU lost); a quarter of the bound per guest process; `UvmHold::within` does not wrap. The holds stay a list scanned per call, bounded by the cap | cf2c132 |
| FB-5 | medium, cross-VM | S-6's framebuffer check and the ioctl were not one step: an RMFB, CLOSEFB or file close on another executor in between freed the id, and the kernel gives the lowest free id to the next framebuffer anyone makes | fixed: each id a call names is in use from its check to the end of its ioctl; RMFB/CLOSEFB wait for it on their own executor (5 s at most: past the check the kernel holds the framebuffer by reference), and a KMS file whose framebuffers are in use is parked and closed after the last such call (close, lease burial, reset). Not one per-VM lock: that would have the queue thread wait on a blocking commit | 3034888 |
| FB-6 | medium, DoS | the S-8 probe limits kept a record per guest-chosen connector id and dpyId, made before the host saw the call: unbounded | fixed: a GETCONNECTOR probe the host refuses takes its record back; past 256 records the stale ones go, and a new id in a full window is reported, not probed. Past 256 dpy records the ones the host never answered go | 59f098d |
| FB-7 | low-medium | the one-pointer deep block was pointed at any 8 bytes of a control's or an allocation's parameters: where RM follows no pointer the address reached RM as data (SET_ZBC_COLOR_CLEAR put it in the GPU-wide ZBC table, an address of the backend's for any tenant to read) | fixed: relocated only at an offset `control_pointers(cmd)` names; elsewhere the field keeps the guest's bytes and the block goes back as sent (the guest driver still carries one for V1 GPU_GET_ID_INFO, whose szName RM 610 ignores). Refused (EINVAL) on anything but RM_CONTROL: no class takes one | e15e535 |
| FB-9 | low | IOCTL2's `after` hooks ran for a call finishing after its file closed or after a session reset: grants re-recorded for a closed file, an old REVOKE forgetting the new session's grants | fixed: `Finisher::records`; the reply is still rewritten, nothing is recorded | a0ee4ee |
| FB-10 | low | an OPEN_KMS card file was charged to whichever process the queue thread served last | fixed: the owner is taken when the call is served | f41a67b |
| FB-11 | low | pump instructions were forwarded after the backend lock was dropped, so a CLOSE's Unwatch could overtake an earlier Watch and leave the pump a duplicate of a closed file (a master, a lease) | fixed: the pump's lock is taken before the backend's is let go, at every site | 2643f5d |
| FB-12 | low, cross-VM | GETPROPBLOB read any blob by id: other VMs' MODE_ID and damage clips, the host desktop's EDIDs | fixed: only blobs a file of this VM made (until destroyed or closed) or saw as the value of a blob property of an object it can see (OBJ_GETPROPERTIES, GETCONNECTOR); ENOENT otherwise. A blob reported and freed since stays readable until asked again: another tenant would have to get that very id meanwhile | 20dbc70 |
| FB-13 | low | descriptors were classified by their `/proc/self/fd` link text, a path: a same-uid process with a mount namespace of its own could pass a FUSE file at `/dmabuf:x` and stall the reader | fixed in part: by `fstatfs`'s f_type first, then the link -- but `fstat` and `fstatfs` themselves still asked a FUSE server; fully fixed by BE-1.7 (cached `statx`, filesystem by device, dma-buf by fdinfo) | d97cfe1, 5726dff |
| FB-14 | low, DoS | a display file handed to the closer was refunded to the handle table at once, so a stuck closer let a guest queue host files without bound, to EMFILE for the whole VM | fixed: counted against the table and the owner's share until the closer has closed it | 69e8306 |
| FB-19 | low | (a) `map_unrecorded` reusing a placement keyed (handle, 0) on an RM file; (b) a freed parent left its children's memory records | (a) not a finding: RM keeps one mapping context per file (nv-usermap.c: a second is NV_ERR_STATE_IN_USE), so every mmap of the file maps the same memory. (b) fixed: each object's parent is kept and a free takes the subtree; a guest freeing devices in a loop could fill the table and leave other processes' memory unrecorded | c9ff8ef |

**On hardware.** What each needed was run on the RTX 5090 in the regression
of the merged tree (nesbox with the C and the Rust module, crosvm with
compute), all green: Vulkan and CUDA (OS events on the event's own file, FB-1;
fence contexts, FB-2; frees of clients, FB-3; CUDA's UVM mappings, FB-4); a lease
session (the lease and vkdisplay probes read MODE_ID, IN_FORMATS and EDID
blobs, FB-12; RMFB and page flips, FB-5; GETCONNECTOR probes, FB-6); descriptors
from the live host compositor, Wayland shm and dma-buf, classified (FB-13).

#### The VMMs and the launcher

What a review of nesbox, the crosvm series and `run-guest.sh` found, each
confirmed before it was fixed. nesbox: branch `virtio-nvgpu-v3` (e7a6548,
eb39060, c9d4391). crosvm: `patches/crosvm/0001`, `0005`, `0007`-`0009`,
regenerated (a fix goes in the patch that brought the code). "The VMMs"
says what each VMM now checks.

**nesbox took any descriptor into the window** (high, given a compromised
backend). A hugetlbfs memfd (`MFD_HUGE_1GB`) is mapped a whole huge page
long, and the `mremap` that moves a placement into the window then replaced
up to a gigabyte past the checked range -- past the window's end, since the
reservation is only 2 MiB-aligned; in the UVM aperture its `munmap` failed
and the mapping stayed. The backend only ever places NVIDIA devices (195)
and DRM nodes (226) in the window (`nvidia/rmmap.rs`: `NV_ESC_RM_MAP_MEMORY`'s
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
only after KVM let go of it; nesbox's `docs/SECURITY.md` and "The UVM aperture" here now
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

#### The Wayland proxy

A review of `wlwire`, `nvgpu-wl-guest` and `device/src/wl` against a guest
(and, in export mode, a host client) trying to take more than its share.
Each finding below was confirmed in the code, and each fix has a test that
fails without it. Branch `fix-wayland`.

| id | severity | what | fix |
|---|---|---|---|
| FW-1 | medium-high | The lease throttle checked only the first submit of a frame: a frame of a thousand submits went through on a full bucket, each a blocking modeset of a desktop monitor on the compositor's thread. The count made ahead of the engine also missed a registry made by `get_registry` in the same frame. | Every submit of a frame must fit the rate; more than the burst in one frame ends the connection. The count follows every `new_id` through the protocol tables, and the engine lets through no more submits than were counted and admitted (`allow_lease_submits`): a miss is fatal, not a bypass. `d08b673` |
| FW-2 | medium | The guest daemon read the whole buffer at every damaged commit, framed all of a client's input at once, and put no byte cap on a client's buffers: a client with a sparse 2 GiB pool committing in a loop grew the daemon, and every client with it, without bound. `set_icc_file` did the same at 16 MiB a message, and export mode in the backend with a host client's commits (dropping the connection past `max_queue`). | The client's side charges its buffers to `MAX_POOL_BYTES` as the server's side does. Commit copies and blobs are read a record at a time as frames are made (`SyncJob`, `BlobJob`, `take_units_upto`). The engine takes no more of a client's input with 4 MiB queued for the channel (`CHANNEL_HIGH_WATER`); the daemon frames a frame at a time and takes more as the host drains; export mode reads a host client only as the guest's queue has room (`EXPORT_QUEUE`). `2465bd1` |
| FW-3 | medium | The daemon queued every descriptor a client sent, and the backend's reader every one its peer sent, though only messages that carry one take one. | Past 1024 untaken (libwayland's own ring, `MAX_FDS_QUEUED`) the client, or the backend's connection, is closed. `9b9a274` |
| FW-4 | medium | Stream sinks held up to 256 KiB each, 256 streams a connection, and unfinished blobs up to 16 x 16 MiB of memfd pages that never expired: 64 MiB a connection outside every per-VM budget, times 64 connections. | A sink grants a share that shrinks as streams are added (`WINDOW` for sixteen, 4 MiB among more, 16 KiB at least), sent in the stream's descriptor to a peer that says `HELLO_STREAM_WINDOW`; what sinks hold is charged to the guest process's share of the VM queue budget, and past it the stream ends with `ENOBUFS`. Unfinished blobs are charged to the VM's and the process's shm budgets, and dropped if the record after them does not take them. An 8 MiB selection still crosses each way in about 220 ms (loopback test, sway). `062d8ea` |
| FW-5 | low | Error text a peer controls -- a far side's ERROR record, an interface name it bound -- went verbatim into the backend's log, the daemon's stderr and, in export mode, a host client's `wl_display.error`. | Fatal text is escaped (control and bidirectional-formatting characters) and cut to 512 bytes in the engine; the backend logs it quoted, through its per-site rate limit (`ratelimit.rs`); the daemon's client-caused lines are limited to 20 per 10 s with repeats counted, and its error list to 32. `ccce467` |
| FW-6 | low | Shm pools and a client's blobs were read with `pread` on the thread serving the connection (in export mode, under the lock WL_SEND and WL_RECV take): a file on FUSE stalled it for as long as its server liked. | A pool must be a regular file on tmpfs or hugetlbfs (every memfd is), or the client gets `wl_shm.error.invalid_fd`; a client's blob from anything else is sent as an invalid descriptor. `ac5cec3` |

Cleanups from the same review: the export socket is bound in a private
0700 directory and renamed into place, and a socket at the path is
replaced only if it is ours; its accept loop waits after `EMFILE` instead
of spinning (`bcb8617`). The `ext_image_copy_capture` value rewrite, of a
protocol the allowlist hides, is gone, and a test holds every rewrite to
what the allowlist reaches (`5201973`). "NVKMS, KMS and leases" now says what lease-file
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
never reading its socket; both are its own process's share. That was the
backend's side only: the guest daemon's sinks were unbudgeted until the
2026-09-29 review (WL-S6).

#### The guest module (`driver/`)

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
  host memory. Guest side (nvgpu_syncobj.c): userspace SYNCOBJ_EVENTFD
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
`dev_dbg_ratelimited`, the host's device details at probe `dev_dbg` -- with
three rate-limited warnings kept on purpose, each naming a condition an
operator should see: a thread killed while its request was in flight
(`nvgpu_xfer.c`), GET_DEV_INFO asked in another release's layout
(`nvgpu_drm.c`: guest userspace and host driver differ), and an ATOMIC
out-fence's write-back faulting (`nvgpu_kms.c`);
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

### A.9 Capture injection (`capture-inject`): tests and hardware

26 unit tests (`inject::tests`: every rule above, the socket's framing,
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

**Open:** the helper and the guest daemon are the integrator's, and so is
how the helper proves consent; buffers whose planes are separate objects
are refused (one dma-buf per buffer in the guest ABI); a guest open whose
reply its caller abandoned leaves its GEM or syncobj handle in the caller's
file until it closes; a real portal stream was not injected by the project
(the tests may not open the host's picker): `rig/rig-tools/portal-identify.sh`
checks one, for the owner to run.

### A.10 The window's size and share (`window-config2`)

Branch `window-config2`. Two defects in how RM mappings were tracked, which
made a game's process run out of its share of the window with the window
not full, and two flags for VMs that need more window than the default:
`--window-size` and `--window-owner-share`. Reported from a deployment (Prism
Launcher / Minecraft on an RTX 5090: about 950 refusals of the form `SHM
WriteCombine zone: guest process ... holds 0x17f82000 of 0x30000000 bytes
and may not take 0x200000 more (Owner)`, then Xid 69).

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

### A.11 Performance (`perf`)

Branch `perf`. What was measured and changed is in BENCHMARKS.md. None of
it lifts a cap or a check: every request is parsed, held to the allowlist
and the tables, and refused exactly as before. What each change does at the
boundary is in Part I: "Memory safety" (guarded blocks), "The host desktop"
(the Wayland proxy's copies), "Frame pacing" (the guest's reply polling) and
"Prefaulting the window".

The guarded blocks were each a fresh anonymous mapping, unmapped after the
call. That cost six system calls and two TLB shootdowns a call, 8.8 of the
10.2 µs the backend spent on an RM control; the pool replaced it.

The reply polling was first described as leaving the host fewer interrupts
to deliver. That held only once the backend honoured the guest's
suppression (BE-2.2): before, it signalled every completion however the
guest asked, and the gain measured on the branch was the spin alone.

### A.12 The 2026-09-29 review

A read-only review of `display-passthrough` at `416dc54`, in parts; each
part's fixes are on a branch of their own. Its notes are in the rig
(`.rig/notes/review-2026-09-29/`).

A final review of the branch before it is merged, by area.

#### VMMs and deployment

What a review of nesbox `virtio-nvgpu-v5`, the crosvm series (`0001`-`0010`),
`rig/run-guest.sh`, `contrib/systemd`, `nix/module.nix` and CI found, and
what was done. nesbox: branch `virtio-nvgpu-v6` (81a6c21, 10e96cc, fc65689).
crosvm: `0007` and `0010`, regenerated. Nothing here was run on the GPU;
what needs it is listed at the end.

**A root backend's binary and socket were its user's to swap** (VD-H1,
medium). As root, the launcher gave the run's directory to the backend's
user until the socket was bound, and put the backend's binary in it. When
that user has other live processes -- the desktop user with
`--wayland-socket`, the export directory's owner with `--wayland-export`,
anyone named by `NVGPU_USER` -- one of them could rename its own program
over the binary during the disk copy or the jail build (setpriv then ran it
as the backend, unsandboxed, handed the guest's RAM), or put a socket of its
own at the path before root closed the directory (the jailed VMM then
connected to it). `nvgpu-socket-open` had the same window. Now nobody but
root ever owns the socket or its directory:

- the backend takes a listening socket handed to it: systemd's socket
  activation (`LISTEN_PID`, `LISTEN_FDS`, `LISTEN_FDNAMES`; `vhost-user` and
  `inject`) or `--socket-fd N`. Each is claimed once, made close-on-exec and
  blocking, and must be a listening `AF_UNIX` socket of its protocol's type
  (`device/src/sys/inherit.rs`; tests of the naming rules and the checks);
- `vhost-user-nvgpu@.socket` binds `/run/nvgpu/vmN/nvgpu.sock`, root's and
  0660 to `nvgpu-vmN`, in a directory of root's (0711), and
  `vhost-user-nvgpu-inject@.socket` the capture helper's, 0660 to its group.
  `nvgpu-socket-open`, its root `ExecStartPost=` and its 10 s wait are gone;
- as root, the launcher's run directory, with the binary's copy in it, is
  root's 0711 from the start, and root binds the socket
  (`systemd-socket-activate`, which execs the backend with it once the VMM
  connects; the socket must now be root's).

What stays: a backend user with other live processes can still signal the
backend (its own VM's availability), and, where `kernel.yama.ptrace_scope`
is 0, trace it in the moment between exec and its first
`PR_SET_DUMPABLE(0)`. Pool users have neither; keep the shared-user modes
for a single desktop. The launcher's dry run shows the backend handed
descriptor 3 named `vhost-user`, running from a `0:711` directory, and no
change of ownership but the socket's group and the disk copy's.

**The console log was unbounded** (VD-H2, low-medium). A guest writing its
console in a loop filled root's logs filesystem. The backend's log goes
through a writer that keeps `NVGPU_LOG_MAX_MIB` (64) and reads and drops
the rest; the backend holds a pipe, not the file. The VMM writes the
console log directly -- through the same pipe, guests stalled (mpv's
Vulkan output froze within a second, found by the app pass) -- and a
watchdog stops the VM once the log passes the limit. (The units' journald
already capped and rate-limited.)

**Diagnostic flags went through as root unannounced** (VD-H3, low-medium). The
launcher added `--diagnostic` for any of them, which is the backend's
second opt-in, but asked for `NVGPU_DIAGNOSTIC=1` only for the sandbox and
root. As root each now needs it, and each in effect is said on the
terminal, as root or not.

**The module's helper-uid check was vacuous** (VD-H4, low-medium). Its users
had no fixed uids, so the check against the VM's own backend and VMM uids
compared with null. The pool now has fixed ids (`uidBase`, 64000: slot N is
uidBase+2N and uidBase+2N+1, group uidBase+2N), and the assertions refuse: a
helper uid of any slot or of a login user; a helper group that does not
exist, is a pool or shared group, or holds anyone but the helper; one
helper or group for two VMs; anyone else in a slot's group; pool users with
other groups; another user on a pool id. `checks.module-eval` tries each
with a configuration it must refuse. Not done: the backend refusing an
inject peer whose uid is the one that connected the vhost-user socket (a
run-time check of the same rule; since done: `c880911`, `a33521b`).

**Guest text reached root's terminal raw** (VD-H5, low). What the launcher
echoes from the console (the verdict, FAIL lines) now loses its control
characters but tab and newline; an escape sequence in a verdict line
reaches the terminal as text.

**The root launcher trusted its environment** (VD-H6, low). As root it now
starts again under `env -i` with its own variables, `RUST_LOG`, `TERM`,
`NESBOX_VIRTIOFSD` (checked as a path), the two the desktop warnings read,
and a PATH of root's directories; every library it puts in the VMM's jail
must be root's. The first shell still ran with what sudo let through: keep
sudo's `env_reset`.

**Two root runs with one tag shared their files** (VD-H7, low). A pool run's
files now carry its slot (`<tag>.vmN`), and a run whose `<tag>.json`
another live run holds (flock) is refused, in every mode.

**The nesbox jail showed the host's `/proc` and `/sys`** (VD-H8, low). The
jailer (`virtio-nvgpu-v6`) mounts a `/proc` of the jail's own with
`hidepid=invisible` (not `subset=pid`: nesbox reads `/proc/devices` for the
UVM major), so the jailed user sees only its own processes, and binds only
`/sys/fs/cgroup`, `/sys/kernel/mm/transparent_hugepage` and
`/sys/devices/system/cpu` (with a network device, `/sys/class/net` and
`/sys/devices/virtual/net`; a virgl `gpu` device still gets all of `/sys`,
which Mesa walks). It refuses a kernel that does not know `invisible`
(before 5.8) rather than change the host's `/proc`.

**The backend unit could be tighter** (VD-H9, low). Added, each against what
the backend does: `RestrictNamespaces=yes` (the unit's `PrivateNetwork=`
gives it a namespace with only loopback, which it keeps; it unshares
nothing), `KeyringMode=private`, `ProtectHostname=yes`,
`ProtectKernelLogs=yes`, `NoExecPaths=/` with `ExecPaths=` the binary and
the library directories (it never execs). Left out, and said why in the
unit: `RemoveIPC=` (`PrivateIPC=` already keeps System V IPC and queues its
own, and under a login user it would remove that user's shared memory),
`SystemCallFilter=` (the backend installs its own allowlist).

**The 100 µs slice** (VD-H10): nothing to fix -- it changes when a thread runs,
not its share. The VMM templates carry a commented `CPUWeight=` for a VMM
that must never beat the desktop ("Frame pacing").

**`Requires=` left a VMM running on a dead backend** (VD-C1, low-medium).
Every place that tells a VMM unit what to say now says `BindsTo=`, and
`contrib/systemd` has two VMM templates, `nvgpu-vmm-nesbox@.service` (the
jailer as root, the unit's network namespace) and
`nvgpu-vmm-crosvm@.service` (`nvgpu-vmmN`, its sandbox on, so no
`RestrictNamespaces=`), not yet run.

**DEPLOY's nesbox recipe could not start** (VD-C2, low-medium): a chrooted
process gets no user namespace, so under the jailer the network namespace is
the unit's; `"unshare-network": true` is for unjailed runs.

**crosvm's prefault vCPU id could be a guest vCPU's** (VD-C3, low). Under
`--host-cpu-topology` KVM's vCPU ids are host APIC ids, and `vcpu_count`
could be one of them. The spare vCPU is now the largest guest id plus one,
made before the guest's vCPUs (`0010`; a test of the review's 4-7 case).
nesbox's ids are 0 to n-1 and the spare n: no collision ("Prefaulting the window" said
this wrongly for crosvm).

**The Wayland drop-in lost to the environment file** (VD-C4, low): systemd
lets `EnvironmentFile=` override `Environment=`. The units take
`$NVGPU_WAYLAND_ARGS` beside `$NVGPU_BACKEND_ARGS`, both from the file, and
the module has `vms.<n>.wayland`.

**crosvm pins the ioevents to BAR0's first address** (VD-C5, low, not
changed): a guest kernel that moves BAR0 before the driver binds has its
ioevents refused, and the device goes NEEDS_RESET. It fails closed; DEPLOY
says not to boot a guest with `pci=realloc`. Following a move would need
the transport to report it after the one-time layout report.

**The two VMMs made the prefault vCPU under different rules** (VD-C6, low):
both now make it only for an nvgpu device and only with the capability, and
stop at the first ENOSYS, ENOTTY or EOPNOTSUPP. **Both logged every UVM
pool at info** (VD-C7, low): debug, as the backend's guest-driven lines are.
**Nothing said a VMM ignored the window's size** (VD-C8, low): the backend now
warns once, at the memory table, when the window is not 1 GiB and the VMM
never asked `GET_SHMEM_CONFIG`.

**CI missed the deployment** (VD-C10, B9): `scripts/ci.sh fast` (and `deploy`
alone) now evaluates the module with every option and each configuration
it must refuse, compares its units with `contrib/systemd`'s key by key,
runs `systemd-analyze verify` over the units, applies `patches/crosvm` in
order to `c0474109d64d`, runs the launcher's dry run, and shellchecks the
launcher. Five rig script bugs made some results weaker than reported: the
dry run's stale-backend case was never set up, `apps.sh` counted a dead
Wayland daemon as FAIL then PASS, preflight assumed a 1 GiB window,
`rig-bench.sh` and `rig-framepace.sh` could sample another user's backend,
and `rig-build-kernel-rust.sh` read `RIG`. All fixed.

**Not yet run on hardware:** a root run of the launcher (the backend
started by `systemd-socket-activate` when the jailed nesbox connects; the
capped console under a real guest); the socket-activated units, with and
without the inject socket (whose descriptor reaches the backend through
`Service=`); the VMM templates; nesbox `v6`'s jail (no other user's
processes in its `/proc`, the UVM major still found, cgroup placement,
virtiofsd); crosvm `0010`'s spare vCPU under `--host-cpu-topology`, and
prefault speed unchanged.


#### Wayland

A review of the Wayland proxy, capture injection and the guest daemon at
`416dc54`, each finding checked against the code before it was fixed, and
the review's reproductions made tests that fail without their fix.

`wlwire`, `nvgpu-wl-guest` and `device/src/wl`, branch `fix29-wayland`. WL-S1
-- the Wayland path's PRIME export of a fence context's GEM (FB-2) --
is the backend's, with the other export paths, and not in this branch.

| # | severity | what | fix |
|---|---|---|---|
| WL-S2 | medium | A client that truncated its own shm pool before a commit's copy was read left the rest of the buffer counted on the channel's backlog for good: the engine gave back only the bytes a short `pread` returned. Past the input limit the client's input was never read again, the daemon asked for `EPOLLIN` it would not read (376k turns in 500 ms), and after the client hung up its slot, its channel and the host's compositor client stayed. About eight short-lived processes held all 64 of a VM's channels. | The two lazy reads, a commit's copy and a blob's rest, are one `job::Job`, and the engine uncounts each step by what it takes off `remaining()`, whatever the step read (WL-R2). The daemon asks for readability only while it takes input, and closes a client that hangs up while its input is not being taken. The fuzzer checks the backlog against the queue after every operation. `1302c78`, `bd4cbe4` |
| WL-S3 | medium | The daemon ran at the session's soft descriptor limit with only a per-connection cap: one client queueing about a thousand descriptors, or pools on a few connections, took every descriptor; every later client stayed unaccepted while the listener woke every wait and logged each failed accept (117,024 lines in 300 ms at a 128 limit), and every client that sent a descriptor was cut off by `MSG_CTRUNC`. | The soft limit is raised to the hard one. What clients may hold -- sockets, channels, queued descriptors, frames for the host, pools, streams, blobs (`Engine::held_fds`, a running count) -- is the limit less the daemon's own and one read's worth, shared among client processes a quarter each with the last eighth kept for processes that hold little (`budget.rs`, the rule of `quota.rs`); past its share a client is closed with `no_memory`, or refused on connecting. One read takes at most libwayland's 28 descriptors, as libwayland's own receiver does. Out of descriptors, accept rests 100 ms instead of spinning, and its line is metered. The daemon also reads a client four times a turn, not until empty, so the channel's replies are not held behind a megabyte of requests. The fuzzer checks that every descriptor open between operations is one the engines count. `a2fe967`, `ece759f` |
| WL-S4, WL-S9 | medium-low | Streams were re-armed one-shot each time the slot was touched, even with no interest; epoll reports ERR and HUP regardless, so a clipboard pipe whose reader went away with nothing to write spun the daemon (250k turns in 500 ms) until the host's source moved. A sink's descriptor shares its file with the client, so closing it left its registration behind. | Streams are in epoll, level-triggered, only while there is interest. The engine keeps an ended stream's descriptor until its owner takes it (`take_closed_streams`), so the daemon takes it out of epoll while it is still open; a client's socket, channel and streams all leave epoll before it is dropped. `311a171` |
| WL-S5 | medium-low | A guest process's shm budget was a flat quarter of the VM's with no reserve: four processes (one forking three times) took every pool or byte at no cost to themselves -- sparse buffers are charged by the pages they cover -- and every other process's first pool was fatal to it. | `ShmShares`: the VM's budget shared through `quota::Share::quarter` and a `Ledger` for bytes and for pools, as `QueueBudget` shares the queue; a quarter each, the last sixteenth kept for processes holding at most a sixty-fourth (at least 8 pools). `9417596` |
| WL-S6 | low-medium | The daemon's stream sinks were unbudgeted: about 15 MiB per connection, 16 connections per process, the daemon's memory, which the guest OOM killer weighs against every app's display. | A 64 MiB daemon-wide budget for what sinks hold, shared among client processes as above; what a client's sinks hold counts toward the stuck-client rule. `a2fe967` |
| WL-S7 | low | CONNECT_FOR resolved the client by pid when asked, and a process it could not find fell back to a plain CONNECT charged to the daemon; a reused pid charged a stranger. | The daemon holds the client's process by a pidfd from the accept (`SO_PEERPIDFD`, or `pidfd_open` on a kernel without it) and drops a client whose process has exited before or during CONNECT_FOR: a pid is reused only after its process is gone, so a process alive after the ioctl is the one charged. `ESRCH` is refused, not charged to the daemon. Still so: a kernel without CONNECT_FOR (`ENOTTY`) charges every client to the daemon, which then holds one process's share of the VM's channels for all of them (ARCHITECTURE.md, "The Wayland proxy"); `driver/uapi/nvgpu_wl.h` does not say so yet. `484b44c` |
| WL-S8 | low | `fatal()` flushed the client and then wrote `wl_display.error` straight to the socket: after a flush that stopped inside an event, a client reading meanwhile got the error in that event's arguments, and the channel went on being read after it. | `Engine::end_with` (WL-R4) queues the error after what the client already has and returns the ERROR record for the far side; the backend's `fail` uses it too, and no `Fatal` is built by hand past `Fatal::new`. The daemon reads nothing more from the channel for a client it is closing. `a46807a` |
| WL-S10 | low | The daemon removed whatever was at its socket path and bound: a second daemon, or a guest compositor on `wayland-0`, lost its socket, and the first's exit removed the second's. | The name is taken as libwayland takes one: `<name>.lock` flocked first, and only then a leftover socket removed; a second daemon is refused, and the socket and lock go with the daemon that holds them. `484b44c` |
| WL-S11 | low | One log limit for the whole daemon: one client's errors used its burst and hid every other line. | A limit per call site, as the backend's. `484b44c` |
| WL-S12 | low | Export mode connected to the guest compositor blocking, on the daemon's one thread. | A non-blocking connect: a full backlog turns the host client away. `484b44c` |

**Native parity.**

- WL-C2: errors are posted where libwayland-server posts them -- the generic
  ones (`invalid_object`, `invalid_method`, `no_memory`, `implementation`)
  on the display, a bad bind on the registry, an interface's own
  (`wl_shm.invalid_fd`, `invalid_stride`, the syncobj manager's
  `invalid_timeline`) on its object. Before, every error named the object the
  message did: "invalid object 99" with object 99, which the client cannot
  dispatch, and `no_memory` on `wl_shm`, which reads as `invalid_fd`. The
  review had `invalid_method` on the resource; libwayland 1.26 posts it on
  the display (`wl_client_connection_data`), and so does this. `4ac50f8`
- WL-C3: a buffer destroyed while its surface shows it keeps its pages in the
  compositor's pool, and their charge, until the surface commits another
  attach or goes -- `wl_buffer.destroy` leaves a pool alone natively, and a
  compositor that reads shm when it paints (wlroots' pixman) painted zeros.
  At most one buffer per surface. `566b34b`
- WL-C4, the two places the proxy was stricter than libwayland:
  - an event newer than its object's version: libwayland-client checks no
    version for events, so one from the host's compositor now passes, as it
    would natively. One from a guest's compositor to a host client (export
    mode) is still refused, on security grounds: the host client would call
    past the end of a listener made for the object's version, a guest
    steering a host process's calls. Requests stay checked, as
    libwayland-server checks them. `7cee84c`
  - a NUL inside a string: no longer stricter. libwayland 1.26's
    demarshaller refuses it too ("string has embedded nul"); either way a
    string the proxy judges -- an interface bound or offered -- must be the
    one the peer's C code reads.

**Documents.** Corrected where the review found them contradicted: "The
host desktop"'s allowlist names `wl_output`; FB-2 says the fence-context fix
held on HOST_OP and not on the Wayland path; A.8's "still open" says it was
the backend's sinks alone; "Capture injection" counts 26 `inject` tests;
ARCHITECTURE.md, "The Wayland proxy", says a
client that truncates its pool goes on, when a destroyed buffer's pages go,
how the daemon charges a client and what it falls back to, and that the
daemon shares its descriptors and sink memory among processes;
`wlwire/protocols/README.md` says why three unallowed protocols are vendored;
`nvgpu-wl-guest/src/log.rs`, `daemon.rs` (two frames per client, not one) and
`device/src/wl/mod.rs` (the legacy watch only) match the code.

**Tests and fuzzing.** Every fix above has a test that fails without it: the
review's reproductions (`leak.rs` into `wlwire/src/tests.rs`, `spin.rs` into
the daemon's tests, `nofile.rs` as `nvgpu-wl-guest/tests/nofile.rs`, which
sets the test process's limit to 128 and puts the second client in a process
of its own), and one per remaining finding. `wl_engine` gained two oracles:
after every operation each engine's backlog is what its queue holds, and
nothing once it has nothing left to take (WL-S2 -- the fuzzer reaches it by
itself, with a pool descriptor smaller than the pool says); and every
descriptor open between operations is one the engines count in
`held_fds` (what the daemon's budget counts, WL-S3). Ten minutes each on
2026-09-29 (`scripts/fuzz.sh run 600`): `wl_engine` 10.6 million runs,
`wl_codec` 469 million, no finding. The loopback test
(`scripts/wl-loopback-test.sh`, a headless sway and weston started by the
test, real clients through the daemon and the backend's connection, directly
and through the dispatcher) passes: an 8 MiB selection crosses each way in
about 222 ms, as before.

**Left for later** (the review's structure items): WL-R5, one budget type for
the three "all or none" charge loops and the per-owner policies (the daemon's
`budget.rs` repeats `quota.rs`'s rule because it cannot link the backend);
WL-R3, the engine owning local input, which the backend reader's lease probe
reading the raw input makes less than natural; WL-R6 to WL-R9.

#### Guest module

Branch `fix29-driver`. The review found no kernel memory corruption, info
leak or cross-process reach in the module: every length taken from a caller
or the backend was bounded before use. What it did find, all Low or Info,
and what was done:

**Security (app vs app, kernel integrity, races).**

- **GM-S1. A failed W_ARM disarmed a file for good.** An arm that failed before
  or at the backend left `armed` set, and no report would ever clear it:
  every later poll armed nothing, and the file's RM event waits ran to their
  timeouts. The arm now holds its file, and any failure but `-EBADF` (the
  handle is gone) or `-ETIMEDOUT` (the request still arrives) makes the file
  ready, so the next poll arms again. Only the caller's own file was ever
  affected.
- **GM-S2. The reaper could close a live proxy's host handle.** It asked
  whether a host GEM handle was a proxy's by walking the open files, which
  lose a file at release while its proxies -- held by whoever it shared
  buffers with -- live on; a late reply naming one of their handles was
  then closed under them. DRM files are now found by render handle in
  `dev->renders` until their last reference.
- **GM-S3. Work items could outlive the module text.** The async fence WATCH
  and the semaphore-surface defer ran on system queues holding no module
  reference (the WATCH could drop the module's last). They run on the
  module's own queues, which `module_exit` destroys after the driver is
  unregistered, waiting them out. Root-only (`rmmod` racing a last put).
- **GM-S4. WL RECV installed descriptors before its copy-out** and took them
  back with `close_fd()` by number, which another thread of the daemon may
  have closed and seen reused; and a RECV that stopped early leaked the
  backend handles of the descriptors it never reached (a lease among them
  keeps an output leased). Descriptors are now reserved, built as files and
  installed only after the copy-out, as capture's always were; the rest of
  a frame's handles are closed on any early exit.
- **GM-S5. A fence proxy's WATCH could reach the backend after its CLOSE.** The
  WATCH was queued before the proxy owned its handle for good; a later
  failure handed the handle back to a caller whose CLOSE went by another
  queue. Harmless while backend handles are monotonic, a cross-app fence
  theft if they were ever reused. The WATCH now goes only once the proxy
  owns the handle (after `fd_install()`, or when the call keeps the fence).
- **GM-S6. Latency interference.** W_ARM queued behind every process's CLOSE,
  GEM_CLOSE, MUNMAP and reaper items on the transport's ordered queue, some
  of them a synchronous CLOSE and an osdesc reap; and every GEM proxy free
  in the VM took one global mutex a re-home holds across HOST_OPs. W_ARM
  runs on the module's own unordered high-priority queue, and a proxy's free
  takes the mutex only if it was ever re-homed.
- **GM-S7. Master hooks under the DRM core's `master_mutex`.** Accepted, and
  stated here: in compositor-VM mode (not yet run on hardware) `master_set`
  and `master_drop` make OPEN_KMS, DROP_IF_MASTER and SET_MASTER round trips
  (up to 60 s each while the backend stalls) under the core's lock, so a
  stalled backend holds every guest open of the card node and every
  SET/DROP_MASTER behind it -- the guest's own compositor and nothing else;
  no other VM and no host state waits. The emulated blocking WAIT_VBLANK,
  which `remove()`'s `drm_dev_unplug()` waited out for up to 3 s, now looks
  at the transport every 50 ms and answers `-ENODEV` once it is dead.
- **GM-S8. GET_DRM_FILE_UNIQUE_ID** was assigned on first ask, racily; it is
  given at open, as nvidia-drm's is.

**Correctness and native parity.**

- **GM-C1.** Nothing restricted the module to x86-64 with 4 KiB pages, where
  the Rust counts registered memory in 4 KiB pages while the C pins kernel
  pages. Kconfig depends on `X86_64`, an out-of-tree build for anything else
  stops at `nvgpu.h`, and `nvgpu_osdesc.c` asserts the page size.
- **GM-C2.** The C parsers kmalloc'd request and reply blocks of up to 1 MiB
  (order 8), which a fragmented guest fails with a page-allocation warning
  any user can cause; they are kvmalloc'd, as the Rust's are.
- **GM-C3.** The syncobj commands and structs the fence code builds are the
  guest kernel's, and nothing tied them to the schema the interpreter holds
  them to. The generator emits the DRM table's numbers
  (`NVGPU_SCHEMA_CMD_*`) and the fence code asserts them at build time.
- **GM-C4.** The difftest never drove the `karg` path (the DRM entry's kernel
  copy of the argument as buffer 0). It does now: the shim splits the
  address space as x86-64 does, and cases cover a kernel argument, one at a
  user address (refused), and a user pointer into the kernel half (refused).
- **GM-C5.** The ATOMIC commit/TEST_ONLY flag had two sources; the parse's
  `out->commit`, written before any hook in both builds, is now the only one.
- **GM-C6.** `/dev/nvidia-modeset` sent any ioctl type but NVKMS's down the RM
  path; it answers `-ENOTTY`, as `nvkms_ioctl` does, and its mmap `-EPERM`,
  as `nvkms_mmap` does.
- **GM-C7, PA-66.** `/dev/nvidia-uvm` had a `.poll` that never became ready (the
  backend cannot epoll the host's UVM file); it has none, as `uvm_fops`, so
  the VFS reports it ready. `/dev/nvidia-uvm-tools` has its own fops, with a
  `.poll` and no `.mmap`, as `uvm_tools_fops`.
- **GM-C8.** GET_DEV_INFO wrote its answer for an `_IOW` caller and refused
  size 0; it follows `drm_ioctl()`'s copy-back rule.
- **GM-C9.** `/proc/driver/nvidia`'s buffers leaked on remove and on a refused
  `proc_create_data()`; they are freed after the subtree is taken down.
- **GM-C10.** A second virtio-gpu-nv device failed half-way through probe; it
  is refused first, with a line saying the module serves one per guest.
- **GM-C11.** NVKMS's PRIME_EXPORT result was not checked like every other
  HOST_OP result (nonzero, 32-bit); it is.

**From the parity review.**

- **PA-40, PA-B1.** The v1 IOCTL exchange was written out nine times, and each
  copy returned the backend's status raw: a positive value, or one past
  `-MAX_ERRNO`, reached the caller as an ioctl result no native driver
  returns, with the payload copied back beside it. One exchange
  (`nvgpu_v1.c`, compiled into the difftest too) and its Rust twin
  (`wire::IoctlResp::parse`) now fail such a reply with `-EPROTO`, unread,
  as IOCTL2 always did. Defence in depth: only a faulty backend sends one.
- **PA-B6.** The KMS and NVKMS descriptor hooks each re-implemented the
  backend's `kind_allowed()`, and the copies disagreed (NVKMS's ignored the
  GPU and any-device bits). One `nvgpu_fd_kind_allowed()`, and its Rust
  twin, held equal to each other by the difftest and to the host's cases by
  a unit test.
- **PA-62, PA-65.** After a reset or `remove()` no event comes, and fence
  proxies were never signalled: a sync_file poll, an IN_FENCE_FD or a
  SYNCOBJ_EVENTFD waited forever. The dead transport now signals every
  proxy with `-ENODEV` and every eventfd subscriber, and every RM, modeset
  and DRM file's poll answers `EPOLLHUP | EPOLLERR`, as a lost GPU's does.
- **PA-64.** RM nodes report events as `EPOLLPRI | EPOLLIN`, as `nv.c` does.
- **PA-47.** mmap of an RM node at a nonzero offset is `-EINVAL`, as
  `nv-mmap.c` has it; it silently mapped from the start before.
- **PA-4, PA-17, PA-19.** nvidia-uvm and nvidia-caps take dynamic majors, as
  theirs do (the fixed 237 and 240 sat in the dynamic range, where a clash
  failed the probe); `/sys/module/nvidia_uvm` exists only with
  `/dev/nvidia-uvm`; and the fake PCI bus's sysdata is a whole x86
  `struct pci_sysdata` instead of a mirror of its first two fields, whose
  `companion`, `iommu` and `fwnode` fell on the PCI address and config space.

**The parsers' default.** The Rust parsers are now what a kernel with Rust
gets unless `NVGPU_RUST=0` says otherwise, and the guest kernel the project
builds has Rust (`driver/guest-kernel.defconfig`). The C stays, frozen, as
the fallback for a kernel without Rust and as the difftest's oracle; a C
module on a Rust kernel says so at load.

**Tests.** The difftest (cases for PA-40 and the karg path; PA-B6 over every
device type and kind bit), the Rust core's unit tests, and the builds of
both modules without a warning. The rest -- the dead-transport path, the
polls, the majors, the work queues, WL RECV -- is verified by reading and
waits for the hardware regression.

#### Backend

Finding ids carry the review's numbers: `BE-1.x` and `BE-2.x` from its
backend part, `PA-n` from its parity table, `WL-S1`/`WL-R1` from its
Wayland part.

| id | what | what was done | commit |
|---|---|---|---|
| BE-1.1 (high) | an app's RM_MAP_MEMORY length near u64::MAX overflowed the window's `align_up` and aborted the backend for the whole VM | fixed: checked rounding, a length larger than any zone refused before a reservation, as RM's NV_ERR_NO_MEMORY | 3dde74c |
| BE-1.10 | the same overflow from a guest kernel's MMAP size | fixed with 1.1 | 3dde74c |
| BE-1.2 | past 2^31 handle allocations a handle read as a negative descriptor, failing event, fd and UVM registrations VM-wide | fixed: handles issued in [1, i32::MAX] | 284ef3c |
| WL-S1, WL-R1 (high) | the Wayland dma-buf path and the IOCTL2 re-home PRIME-exported fence contexts, bypassing the 09-26 fix | fixed: one export gate (`device/src/exportgate.rs`) asked by HOST_OP, WL_SEND and the re-home: no fence context, no INJECT_OPEN handle, no tainted dma-buf | 2e4dfaa |
| BE-1.3 | with `--allow-compute`, a client allocated under a registration's object handle released the registration while RM still pinned its pages | fixed: root classes end no registration | d85a279 |
| BE-1.19 (in part) | a registration answered with object 0 could end another | fixed: object 0 ends nothing; both go with their client | d85a279 |
| BE-1.4 | a fence context over registered memory was a holder osdesc did not know | fixed, failing closed: 0x54 over a surface that holds a registration is EPERM | c605af3 |
| BE-1.5 | a KMS call finishing after a session reset recorded a blob under a dead serial, readable once the id was reused | fixed: reset keeps the retired marks; a file's drop purges its blobs | 35b47e0 |
| BE-1.6 | syncobj-watch eventfd handles were charged to no process | fixed: charged to the watcher | c8b5700 |
| BE-1.7 | classification still called `fstat`/`fstatfs`, which a FUSE server answers on its own schedule (FB-13 incomplete) | fixed: cached `statx`, filesystem by device against the kernel's own mounts, dma-buf by fdinfo | 5726dff |
| BE-1.8 | the OS-descriptor mapped-run and external-range budgets were per VM only | fixed: a quarter per process | e4de813 |
| BE-1.9 | a blob property could be committed to another tenant's blob id | fixed: 0, or a blob this VM made or sees | f6ad038 |
| BE-1.11 | the pump's outbox grew by a record a re-watch under a fresh cookie | fixed: a fired one-shot record goes on re-watch or close | 8c5506d |
| BE-1.12 | modeset files still closing did not count against the NVKMS open caps | fixed | bb0eae7 |
| BE-1.13 | a refused `MAP_FIXED` file mapping left a hole the owner later unmapped whole | fixed: map anywhere, `mremap(MREMAP_FIXED)` into place | 386a4c2 |
| BE-1.14 | the private-descriptor scan could take the accept threads' peer descriptors | fixed: those threads start after the scan | 5dab3bb |
| BE-1.15 | with no host version set, RM escapes went unchecked and the guest's CHECK_VERSION_STR chose the profile | fixed: never learned from a guest; no version refuses every RM escape outside tests; test-harness reads the host's | 18f4be9 |
| BE-1.16 | a v1 reply's room was checked after the host call | fixed: the least a success returns is checked first | e00592e |
| BE-1.17 | an event's parameters too short for its descriptor skipped the translation | fixed: EINVAL, as OS_UNIX | 1ea62b9 |
| BE-1.18 | the always-refused classes held on RM_ALLOC only | fixed: ALLOC_OBJECT and ALLOC_CONTEXT_DMA2 too | 8555606 |
| BE-1.19 (rest) | IDLE_CHANNELS clients, pointer-table unions, generator scans | open (phase 2) | |
| BE-1.20 | GPU-wide-state controls, revocation under the mutex, IOCTL2 duplicate drops, loopback-only netns, UNMAP keyed by opener | open: documentation and decisions for phase 3 | |
| BE-2.1, PA-23 | a failed host ioctl came back with no parameters, so a CHECK_VERSION_STR mismatch hid RM's version | fixed: the block as the host left it, with the errno (EFAULT excepted) | 120eea7 |
| BE-2.2 | the backend ignored EVENT_IDX and NO_INTERRUPT (A.11's claim did not hold) | fixed: an interrupt only when the guest asks | d50b5c5 |
| BE-2.3 | `--sched-slice-us` reset a unit's batch or idle policy | fixed: the policy is kept, a real-time one left alone; the launcher passes its slice through the flag | 1ea77f9 |
| BE-2.4 | RM_UNMAP_MEMORY zeroed the caller's pLinearAddress when found | fixed: the caller's own, found or not | fd02302 |
| BE-2.5 | rewritten request fields echoed back altered | open (phase 2, with typed replies) | |
| BE-2.6 | DRI count, `abs` on i32::MIN, protocol distinctness, the unmeasured-release log, a short named-client block's status | fixed | f81fee3, 18f4be9, 32d70a3 |
| PA-14 | the guest's PCI config space held only the 64 bytes an unprivileged reader gets | fixed: `--pci-config-dir`, a root launcher's snapshot, used only if it is of the same device | a73e1a2 |
| PA-29 | budget refusals on RM and UVM escapes were errnos | fixed: RM's statuses in the caller's block (NO_MEMORY, NOT_SUPPORTED, UVM's rmStatus) | 3dde74c, da05be7 |
| PA-31, PA-32 | an unserved UVM command and a foreign ioctl type were EPERM | fixed: ENOSYS, and the device's own EINVAL or ENOTTY | cf17437 |
| PA-40 | a positive header status would reach userspace as a result | fixed: every header 0 or -1..-4095, else EPROTO | af599c1 |
| — | the ring workers had no exit event, so a backend whose VMM hung up never exited | fixed | 2f09bb6 |
| — | a helper of the VMM's own uid could inject | fixed: the VMM's uid is refused (not with `--allow-inject-self`) | c880911 |

Every fix has a test that fails without it. On the RTX 5090 (595.99.02,
sandbox on, allowlist enforcing), under nesbox and crosvm alike: `stage1`
6/0/0, `compat` 12/0/0, `render` with compute 9/0/1 with `cuda-smoke` all
PASS, `wayland` on the live Hyprland 13/0/1, `secneg` 10 passed 5 skipped,
`lease` 9/0/1, `vkdisplay` 8/0/1, `secneg` on the lease 6/0/0 (KMS 15
passed), and the live app batch (glxgears, vkmark, stk, chromeanim) 11/0/0.
The PCI config path was shown with a synthetic snapshot (the rig has no
root): the guest's `lspci -vvv` reads `Capabilities: [40] Null` without it
and the snapshot's PCIe capability and link with it.

### A.13 Heavy workloads, the fixes (`heavyfix`)

Branch `heavyfix`: the fixes for what BENCHMARKS.md, "Heavy workloads"
measured. What each does at the boundary is in Part I -- "Frame pacing"
(descriptor classification through a kept directory, render calls under one
hold of the mutex without a duplicate, the guest's posted SYNCOBJ_DESTROY,
the pump's `poll(2)` for armed RM descriptors) and "Guest RAM under crosvm"
(`patches/crosvm/0011`). None lifts a cap or a check, and none changes what
the backend accepts: the classification matches the same texts, every call
is parsed and refused as before, and the backend receives the same DESTROY
whether or not the guest waits for it.

Tried and not kept: a crosvm option turning off the kernel PIT's
re-injection, which KVM pairs with an AVIC inhibit. It made no measurable
difference to crosvm's round trips or frame rates, so it adds nothing to
weigh against being one more thing that differs from upstream. What does
close crosvm's remaining gap to nesbox is its per-vCPU core scheduling
(`NVGPU_CROSVM_CORE_SCHED=0`); that stays on by default, a security choice
(DEPLOY.md, "Frame pacing"), and BENCHMARKS.md, "Heavy workloads" has what
it costs and a middle way for the deployment to weigh.

Proposed, not made: a host kernel flag for `KVM_PRE_FAULT_MEMORY` that maps
for write (`patches/linux/`, a draft applied nowhere).

## Appendix B. Against `dev`

`dev` at `50ff74a`, the branch this work started from, against the code
today. Everything here is true, but only as a comparison.

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
had. All of these are fixed in the code; see Appendix C.

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

### The host GPU surface on dev

| entry point | dev |
|---|---|
| **RM escapes**, type `F`, on `/dev/nvidiactl` and `/dev/nvidiaN` | Any `F` ioctl on any open handle. **Size-checked** against the profile (23 escapes for 535, 24 for 580 and 595) once the host version had been learned from the guest's first CHECK_VERSION_STR; **raw** before that. |
| **RM_CONTROL** commands | All, **raw** past the 32-byte outer check. One embedded pointer was relocated, at an offset the guest named, into a heap buffer sized from what the guest sent. Every other embedded pointer reached RM as a guest address, which RM dereferences in the backend. |
| **RM_ALLOC** classes | All, **raw**. |
| **RM_SHARE, RM_DUP_OBJECT, and a second client named in parameters** | **Raw**. RM saw every client of a VM as one process, the backend's, so a guest could share an object with every client on the host (type ALL) or every process of the backend's uid (OS_SECURITY_TOKEN), and any guest process could duplicate any other's objects by handle. |
| **memory named by CPU address** (OS descriptors through RM_ALLOC, ALLOC_MEMORY and VID_HEAP_CONTROL) | **Raw**: RM pinned the backend's pages at a guest-chosen address and mapped them for the GPU. |
| **nvidia-uvm**, `/dev/nvidia-uvm` | All, **raw**. The guest copied 12 KiB each way for every command but the two it knew the size of, and pointers and descriptors went as sent. |
| **nvidia-uvm tools**, `/dev/nvidia-uvm-tools` | All, **raw**. |
| **NVKMS**, `/dev/nvidia-modeset` | Every command, **raw**, with **no policy**. One descriptor, REGISTER_SURFACE's, was translated at a fixed offset. |
| **nvidia-drm and DRM core on a host render node** | Any `d` ioctl. Three nested GEM calls translated `memFd`; the rest were **raw** in a buffer sized by the guest, while the host copies `_IOC_SIZE` back (a heap overflow in the backend). |
| **DRM KMS on a host card or lease file** | None: no such file existed. |
| **HOST_OP** (backend-made host calls on the guest's behalf) | None. |
| **mmap** | Any handle. A UVM file went to the window, where the VMM's mmap of it failed and closed the window's request channel for the rest of the VM. |
| **any other ioctl type** | **Raw**, to whatever host file the handle was. |

What the dev column shows is that a guest's reach into the host driver on dev
was bounded mostly by what the guest's own libraries happened to send.

### The backend on dev

| | dev |
|---|---|
| guest messages | 8 types; responses capped at 64 KiB |
| guest payloads | the ioctl header with its nested and deep blocks; mmap and munmap requests |
| host peers | the VMM's vhost-user messages |

| | dev |
|---|---|
| device files | `/dev/nvidia*`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset`, `/dev/dri/renderD*` |
| other files | `/proc/driver/nvidia`, PCI config in sysfs |
| vhost-user socket | default `/tmp/nvgpu.sock`, and a failed unlink was ignored, so another user could bind it first and receive the guest's memory |
| other sockets | none |
| netlink | none |

| | dev |
|---|---|
| identity | whoever started it. The shipped `rig/run-guest.sh` ran it as root, which makes every guest process an RM administrator: all of BAR0 mappable read-write, the register allowlist skipped, and DRM files authenticated. |
| capabilities | whatever it was given |
| sandbox | none |

| | dev |
|---|---|
| threads | the transport, and one event pump that polled every open handle every millisecond |
| handles | no cap but RLIMIT_NOFILE |
| RM counters | one map entry per distinct guest value, unbounded |
| logs | unbounded; the launcher wrote them to an unrotated file |
| display caps | -- |
| window | the zones, first come first served |
| Wayland caps | -- |
| not capped | -- |

### The host desktop on dev

dev had one path to the host display, and it was unfiltered. NVKMS checks no
permission at all for SET_CURSOR_IMAGE, MOVE_CURSOR, SET_LAYER_POSITION,
SET_DPY_ATTRIBUTE, SET_DISP_ATTRIBUTE, SET_FRAMELOCK_ATTRIBUTE and the
overrides in QUERY_DPY_DYNAMIC_DATA (read from `nvkms.c`), and dev forwarded
all of them. Everything else in "NVKMS, KMS and leases" and "The host desktop"
is new.

### Inside the guest on dev

| node | dev |
|---|---|
| `/dev/nvidiaN`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` | 0666 |
| `/dev/nvidia-caps/*` | 0444 |
| DRM node | a hand-made character device, 0666 |
| `/dev/nvgpu-wl[N]` | -- |
| `/dev/nvgpu-capture[N]` | -- |
| adopted DRM files | -- |
| `nvgpu-wl-guest` | -- |

## Appendix C. Finding index

One row per finding: its severity, what it was, the commits that fixed it,
and its status now. "Fixed" means the finding's scenario no longer works in
the code. Where something is left, the status names the Part I section or the
"Open items and residual risk" item where it lives. Appendix A has each
round's story.

Ids that collided between rounds carry a prefix: **D** for the `dind`
memory-passing review (M1 and M2 there), **FB** and **FW** for the
2026-09-26 backend and Wayland reviews (#1, #2, ... there), and **VD**,
**WL**, **GM**, **BE** and **PA** for the 2026-09-29 review's VMM and
deployment, Wayland, guest-module, backend and parity parts.

### The verification review (A.1)

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
| L-7 | low | "no crossings per frame" measured only without presenting | -- | partly: the Wayland mode measured, 17 round trips per presented frame (DEPLOY.md, "Frame pacing"); no other display path (open item 22) |
| L-8 | low | stale proxy-size comment | `388288c` | fixed |
| L-9 | low | useSyncpt refused when not specified | `b6d58a8`, `3d6e281` | fixed |

### The security review (A.1)

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
| S-11 | medium | a host GEM handle gets two guest owners | `f264599` | fixed; three edge cases open, not re-checked since (open item 12) |
| S-12 | medium | a dead fence's consumer signals a reused id | `037c20f` | fixed |
| S-13 | medium | a destroyed syncobj's wait registration is joined | `2e4c715` | fixed |
| S-14 | medium | NVKMS gates checked at prepare, not at run | `5baeb66` | fixed; the GET_LEASE check at run time not done |
| S-15 | medium | EXPORT_TO_DMABUF_FD forwarded raw | `db4d736` | fixed by refusal; translation is future work |
| S-16 | low | blobs hold memfds with no count limit | `86553c1` | **partly**: count cap done; one descriptor budget and RLIMIT_NOFILE not |
| S-17 | low | RM counter maps grow without bound | `d773f45` | fixed |
| S-18 | low | no per-session channel limit | `de95ad3` | fixed; its optional item 3, backpressure towards the compositor, not done (open item 13) |
| S-19 | low | a guest can seize a leasable monitor, or churn it | `6e4c501` | churn fixed; holding is by design |
| S-20 | low | guest-triggered log flooding | `d773f45` | fixed |
| S-21 | low | the 1 ms sweep scales with open handles | `07971a3` | fixed |
| S-22 | low | any guest user can exhaust host memory through the proxy | `86553c1`, `de95ad3` | fixed |
| S-23 | low | `/tmp/nvgpu.sock` can be squatted | `b3c126b` | fixed |
| S-24 | low | host PIDs of every GPU client readable | `32fd293` | fixed by answering locally; a PID namespace is the real fix |
| S-25 | low | GEM-in drops the proxy reference before the host call | `bc2fcc1` | fixed |
| S-26 | low | device removal leaves objects pointing at freed memory | `3e930d3` | **partly**: calls in flight at removal are guarded since the device's lifetime was fixed (A.8, "The guest module"); window pages stay mapped after removal (open item 12) |
| S-27 | low | executor pool loses a wake-up | `7dad490` | fixed |
| S-28 | low | a failed lease probe hides the device for good | `5cf36ad` | fixed |
| S-29 | low | guest daemon busy-loops on a hung-up client | `2e003c5` | fixed |
| S-30 | low | lease probe under the connection lock; timeout cached | `5cf36ad` | fixed; the cache is not cleared on compositor restart |
| S-31 | low | every WL_RECV allocates the frame limit | `cfd63c3` | fixed |
| S-32 | low | an unresolvable syncobj becomes a placeholder | `a6d3d2f` | fixed |
| S-33 | low | final closes of display files block the VM's threads | `03b3539` | fixed; the pump's duplicate can delay a master release until UNWATCH |
| S-34 | low | OPEN_KMS and DROP_IF_MASTER run on the queue thread | `03b3539` | fixed; a single "open non-master" op not added |
| S-35 | high | RM_SHARE forwarded raw: a guest shares an object with every host client; DUP_OBJECT from a client not the VM's; every guest process duplicates every other's objects | `f550116` | fixed; opened by the review after these, not by the 34. Its review then found a grant outliving an object freed with its parent, an object's own list not overriding its client's, and the FIFO controls' client lists unchecked; fixed in the commit after. Grants on intermediate objects are not followed (refused where RM allows) (open item 7). A later pass dropped the caller-made-the-source allowance (RM's rule is the two clients' makers), made a guest without process ids fail closed, and held each second client to RM's rule for its field with the guest's euids |

### The `harden` audit (A.2)

| id | sev | finding | fix | status |
|---|---|---|---|---|
| R1 | critical | NV0000 OS_UNIX controls reached RM with the guest's descriptor number | `fc7a58b` | fixed (sec-negative T11) |
| R2 | critical | EXPORT/IMPORT_OBJECT_FROM_FD fell back to the caller's raw number | `fc7a58b` | fixed |
| B1 | high | the handle table was one pool, bounded by the inherited RLIMIT_NOFILE | `6b75a82` | fixed; one descriptor budget not done (open item 5) |
| B2 | high | the shared window was one pool | `6b75a82` | fixed |
| F1 | medium | ALLOC_SEMAPHORE_POOL's length reached UVM uncapped | `6b75a82` | fixed |
| R3 | medium | isolation rested on RM's strict client validation, which a registry key turns off | `fbe3f22` | fixed by refusal: the backend will not run on a host without it |
| B3 | medium | fence contexts: four files of one app took them all | `6b75a82` | fixed |
| B4 | medium | 64 NVKMS opens per VM, all takeable by one app | `6b75a82` | fixed |
| B5 | medium | UVM placements, pools and registrations capped per VM; pools of two processes collide | `6b75a82`, `fbe3f22` | partly: every budget per process; finding or squatting on another process's pool address is not fixable here (open item 9) |
| W1 | medium | 64 channels and 1 GiB of shm per VM, no per-process limit | `b5980e5` | fixed |
| W2 | medium | the daemon's per-client output buffer had no bound | `b5980e5` | fixed |
| W3 | medium | at the queue budget an innocent connection was dropped | `b5980e5` | fixed, the guest image's side too (A.8, "The Wayland proxy") |
| F2 | low | export/import to a descriptor moves an RM object outside the DUP_OBJECT gate | -- | native strength, documented (open item 8) |
| F3 | low | the aperture band can hold the VMM's own mappings | crosvm `0007`, `0008` | fixed for crosvm; open for nesbox (open item 9) |
| F4 | low | CARD_INFO, ATTACH_GPUS_TO_FD and NUMA_INFO pass with no size check | -- | accepted: the argument is never smaller than `_IOC_SIZE`, and ATTACH_GPUS_TO_FD carries GPU ids only |
| R4 | low | SEMSURF_FENCE_CTX_CREATE accepts any client of the VM | -- | open, native strength (open item 8) |
| R5 | low | negative descriptor values other than -1 forwarded | `fc7a58b` | fixed |
| B6 | low | one queue thread and 16 executors serve every file | -- | open (open item 15) |
| B7 | low | fence waits, rmshare grants, rmmem records and the lease throttle are VM-wide | -- | open (open item 15) |
| W4 | low | every guest app reaches the host compositor as the backend | -- | open (open item 13) |

### Fuzzing (A.4) and `dind` (A.5)

| id | sev | finding | fix | status |
|---|---|---|---|---|
| Z1 | critical | a nested block shorter than the size the host copies by let a guest address reach RM | `20434fa` | fixed |
| Z2 | low | undefined behaviour under Stacked Borrows in v1 host calls | `5e62911` | fixed |
| Z3 | low | `serve` could answer with more bytes than the capacity posted | `0b320bb` | fixed |
| D1 | high | a second MMAP of an unrecorded placement mapped past it | `139a9e9` | fixed |
| D2 | high | UVM duplicated another guest process's RM objects by client and handle | `139a9e9` | fixed; the client is held to the control file it was made on, not to the process (open item 8) |

### The 2026-09-26 review (A.8)

FB-8 and FB-15 to FB-18, and the review's logging and clean-up items, went to
the hardening branch; "Fail closed" is what it did.

| id | sev | finding | fix | status |
|---|---|---|---|---|
| FB-1 | high, cross-VM | ALLOC_OS_EVENT and FREE_OS_EVENT reached RM with the guest's hClient unchecked | `9a9d3aa` | fixed |
| FB-2 | high, DoS | a fence context's GEM could be PRIME-exported and outlive its caps | `d7c41fc`, `2e4dfaa` | fixed on every export path (the Wayland path by WL-S1) |
| FB-3 | medium | RM_FREE and FREE_OS_EVENT wiped the backend's records whatever RM answered | `9a9d3aa` | fixed |
| FB-4 | medium, compute | UVM external mappings of registered memory held whatever UVM answered | `cf2c132` | fixed |
| FB-5 | medium, cross-VM | S-6's framebuffer check and the ioctl were not one step | `3034888` | fixed |
| FB-6 | medium, DoS | the S-8 probe limits kept unbounded records | `59f098d` | fixed |
| FB-7 | low-medium | the one-pointer deep block was pointed at any 8 bytes | `e15e535` | fixed |
| FB-9 | low | IOCTL2's `after` hooks ran for a call finishing after a close or a reset | `a0ee4ee` | fixed |
| FB-10 | low | an OPEN_KMS card file was charged to the wrong process | `f41a67b` | fixed |
| FB-11 | low | a CLOSE's Unwatch could overtake an earlier Watch | `2643f5d` | fixed |
| FB-12 | low, cross-VM | GETPROPBLOB read any blob by id | `20dbc70`, `35b47e0`, `f6ad038` | fixed (with BE-1.5 and BE-1.9) |
| FB-13 | low | descriptors were classified by their link text, which a FUSE file can fake | `d97cfe1`, `5726dff` | fixed (finished by BE-1.7) |
| FB-14 | low, DoS | a display file handed to the closer was refunded at once | `69e8306` | fixed |
| FB-19 | low | (a) `map_unrecorded` reusing a placement; (b) a freed parent left its children's records | `c9ff8ef` | (a) not a finding; (b) fixed |
| FW-1 | medium-high | the lease throttle checked only a frame's first submit | `d08b673` | fixed |
| FW-2 | medium | the daemon, `set_icc_file` and export mode held a client's buffers without bound | `2465bd1` | fixed |
| FW-3 | medium | descriptors queued without bound | `9b9a274` | fixed |
| FW-4 | medium | stream sinks and unfinished blobs held memory outside every budget | `062d8ea` | fixed; a sink's share can be held by not reading (open item 13) |
| FW-5 | low | peer-controlled error text reached logs verbatim | `ccce467` | fixed |
| FW-6 | low | shm pools read with `pread` on the serving thread, which FUSE could stall | `ac5cec3` | fixed |

### The 2026-09-29 review (A.12)

| id | sev | finding | fix | status |
|---|---|---|---|---|
| VD-H1 | medium | a root backend's binary and socket were its user's to swap | `1e07849`, `ca81267`, `5c31fff` | fixed; signal and trace by a shared user remain (open item 18) |
| VD-H2 | low-medium | the console log was unbounded | `ca81267`, `cd1b5f9`, `f462409` | fixed |
| VD-H3 | low-medium | diagnostic flags went through as root unannounced | `ca81267` | fixed |
| VD-H4 | low-medium | the NixOS module's helper-uid check was vacuous | `5c31fff`, `c880911`, `a33521b` | fixed; the backend's run-time check needs one VMM uid (open item 18) |
| VD-H5 | low | guest text reached root's terminal raw | `ca81267` | fixed |
| VD-H6 | low | the root launcher trusted its environment | `ca81267` | fixed; the first shell runs with what sudo passes (open item 18) |
| VD-H7 | low | two root runs with one tag shared their files | `ca81267` | fixed |
| VD-H8 | low | the nesbox jail showed the host's `/proc` and `/sys` | nesbox `virtio-nvgpu-v6` | fixed |
| VD-H9 | low | the backend unit could be tighter | `5c31fff` | fixed |
| VD-H10 | -- | the 100 µs slice | -- | not a finding |
| VD-C1 | low-medium | `Requires=` left a VMM running on a dead backend | `5c31fff` | fixed (`BindsTo=`) |
| VD-C2 | low-medium | DEPLOY's nesbox recipe could not start | `1eba209` | fixed in DEPLOY.md |
| VD-C3 | low | crosvm's prefault vCPU id could be a guest vCPU's | `c66e698` | fixed |
| VD-C4 | low | the Wayland drop-in lost to the environment file | `5c31fff` | fixed |
| VD-C5 | low | crosvm pins the ioevents to BAR0's first address | -- | open, fails closed (open item 18) |
| VD-C6 | low | the two VMMs made the prefault vCPU under different rules | `c66e698`, nesbox `virtio-nvgpu-v6` | fixed |
| VD-C7 | low | both VMMs logged every UVM pool at info | `c66e698`, nesbox `virtio-nvgpu-v6` | fixed |
| VD-C8 | low | nothing said a VMM ignored the window's size | see A.12 | fixed: the backend warns |
| VD-C10 | -- | CI missed the deployment; five rig script bugs | `cf3d083`, `cecad2c` | fixed |
| WL-S1, WL-R1 | high | the Wayland dma-buf path and the IOCTL2 re-home PRIME-exported fence contexts | `2e4dfaa` | fixed: one export gate |
| WL-S2 | medium | a truncated shm pool left its backlog counted for good | `1302c78`, `bd4cbe4` | fixed |
| WL-S3 | medium | the daemon's descriptors were one pool any client could take | `a2fe967`, `ece759f` | fixed |
| WL-S4, WL-S9 | medium-low | streams re-armed without interest spun the daemon | `311a171` | fixed |
| WL-S5 | medium-low | a process's shm budget had no reserve | `9417596` | fixed |
| WL-S6 | low-medium | the daemon's stream sinks were unbudgeted | `a2fe967` | fixed |
| WL-S7 | low | CONNECT_FOR resolved the client by pid | `484b44c` | fixed; a kernel without CONNECT_FOR charges the daemon (open item 13) |
| WL-S8 | low | `fatal()` wrote the error inside an event | `a46807a` | fixed |
| WL-S10 | low | the daemon removed whatever was at its socket path | `484b44c` | fixed |
| WL-S11 | low | one log limit for the whole daemon | `484b44c` | fixed |
| WL-S12 | low | export mode connected to the guest compositor blocking | `484b44c` | fixed |
| WL-C2 | parity | errors posted on the wrong object | `4ac50f8` | fixed |
| WL-C3 | parity | a destroyed buffer's pages went while a surface showed it | `566b34b` | fixed |
| WL-C4 | parity | the proxy was stricter than libwayland about event versions and NULs | `7cee84c` | fixed |
| WL-R2, WL-R4 | structure | one lazy-job type; one "end with an error" helper | with WL-S2, WL-S8 | done |
| WL-R3, WL-R6 to WL-R9 | structure | local input in the engine; socket paths, message walks, descriptor patching, `inject.rs` in parts | `246df62`, `d891ad1`, `6105be8`, `39951bb`, `b30768a` | done |
| WL-R5 | structure | one budget type | `8461070` | partly: the daemon's `budget.rs` still repeats `quota.rs`'s rule (open item 21) |
| GM-S1 to GM-S6 | low | a failed W_ARM, the reaper's walk, work items past module text, WL RECV installs, WATCH before ownership, latency interference | `c701438` | fixed |
| GM-S7 | low | master hooks round-trip under the DRM core's `master_mutex` | -- | accepted (open item 12) |
| GM-S8 | low | GET_DRM_FILE_UNIQUE_ID assigned racily | `366c621` | fixed |
| GM-C1 to GM-C11 | correctness, parity | architecture, kvmalloc, syncobj numbers, the karg difftest, one commit flag, modeset refusals, UVM polls, GET_DEV_INFO, `/proc` buffers, one device, PRIME_EXPORT's result | `366c621`, `043486e`, `dab7062`, `82c16e9`, `a34c0f3` | fixed |
| BE-1.1, BE-1.10 | high | a mapping length near u64::MAX aborted the backend | `3dde74c` | fixed |
| BE-1.2 | medium | past 2^31 handles a handle read as a negative descriptor | `284ef3c` | fixed |
| BE-1.3 | medium, compute | a client under a registration's handle released the registration | `d85a279` | fixed |
| BE-1.4 | medium, compute | a fence context over registered memory was an unknown holder | `c605af3` | fixed, failing closed |
| BE-1.5 | low-medium, cross-VM | a KMS call after a reset recorded a blob under a dead serial | `35b47e0` | fixed |
| BE-1.6 | low-medium | syncobj-watch eventfds were charged to no process | `c8b5700` | fixed |
| BE-1.7 | low-medium | classification still asked a FUSE server | `5726dff` | fixed |
| BE-1.8 | low-medium | two OS-descriptor budgets were per VM only | `e4de813` | fixed |
| BE-1.9 | low, cross-VM | a blob property could name another tenant's blob | `f6ad038` | fixed |
| BE-1.11 | low | the pump's outbox had no bound | `8c5506d` | fixed |
| BE-1.12 | low | modeset files still closing did not count against the NVKMS caps | `bb0eae7` | fixed |
| BE-1.13 | low | a refused `MAP_FIXED` file mapping left a hole | `386a4c2` | fixed |
| BE-1.14 | low | the private-descriptor scan could take the accept threads' descriptors | `5dab3bb` | fixed |
| BE-1.15 | low | with no host version set, a guest chose the ABI profile | `18f4be9` | fixed |
| BE-1.16 | low | a v1 reply's room was checked after the host call | `e00592e` | fixed |
| BE-1.17 | low | a short event block skipped the descriptor translation | `1ea62b9` | fixed |
| BE-1.18 | low | the always-refused classes held on RM_ALLOC only | `8555606` | fixed |
| BE-1.19 | low | smaller gaps in the RM path | `d85a279` (in part) | partly: the zero-handle key fixed; IDLE_CHANNELS clients, pointer-table unions and the generators' scans open (open items 8, 21) |
| BE-1.20 | low | GPU-wide-state controls, revocation under the lock, IOCTL2 duplicate drops, the loopback-only network check, mappings keyed by opener | -- | open (open items 1, 3, 8, 12, 21; "The window's size and share") |
| BE-2.1 | low-medium | a failed host ioctl came back with no parameters | `120eea7` | fixed |
| BE-2.2 | low | the backend ignored EVENT_IDX and NO_INTERRUPT | `d50b5c5` | fixed |
| BE-2.3 | low | `--sched-slice-us` reset a unit's scheduling policy | `1ea77f9` | fixed |
| BE-2.4 | low | RM_UNMAP_MEMORY zeroed the caller's pLinearAddress | `fd02302` | fixed |
| BE-2.5 | low | rewritten request fields come back altered | -- | open (open item 21) |
| BE-2.6 | low | DRI count, `abs` on i32::MIN, protocol distinctness, the unmeasured-release log, a short named-client block's status | `f81fee3`, `18f4be9`, `32d70a3` | fixed |
| PA-14 | parity | the guest's PCI config space held 64 bytes | `a73e1a2` | fixed (`--pci-config-dir`) |
| PA-23 | parity | a CHECK_VERSION_STR mismatch hid RM's version | `120eea7` | fixed (with BE-2.1) |
| PA-29 | parity | budget refusals on RM and UVM escapes were errnos | `3dde74c`, `da05be7` | fixed |
| PA-31, PA-32 | parity | an unserved UVM command and a foreign ioctl type were EPERM | `cf17437` | fixed |
| PA-40, PA-B1 | parity | a positive or out-of-range status reached userspace | `af599c1`, `043486e` | fixed on both sides |
| PA-B6 | structure | the descriptor-kind check was written three times | `3b83f3a` | fixed |
| PA-62, PA-64, PA-65, PA-66 | parity | fences and polls after a dead transport; RM's poll bits; UVM's poll | `a34c0f3` | fixed |
| PA-4, PA-17, PA-19, PA-47 | parity | fixed majors, `/sys/module/nvidia_uvm`, `pci_sysdata`, mmap at an offset | `82c16e9` | fixed |
