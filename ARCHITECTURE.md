# virtio-nvgpu Architecture

How a Linux guest gets an NVIDIA GPU without the host giving the card away, and
without anything in the middle pretending to be a GPU driver.

This document explains the design. It contains no code: the code is in
[`driver/`](driver/), [`device/`](device/), [`protocol/`](protocol/),
[`wlwire/`](wlwire/) and [`nvgpu-wl-guest/`](nvgpu-wl-guest/), and it moves
faster than prose can follow.

> **Built and measured, as of 2026-09-24:** the forwarding path, the shared
> memory window, the DRM render node, buffer sharing between a client and a
> compositor inside the guest, the event queue that lets a guest wait, and
> encoding on the GPU. A guest renders, presents and encodes H.264, costs
> within 2% of bare metal, and four guests share one card evenly
> ([`BENCHMARKS.md`](BENCHMARKS.md)). All of that was measured before
> protocol v2, and has not been re-measured since.
>
> **Built since, and run on an RTX 5090 (595.99.02) under nesbox and crosvm,
> as of 2026-09-26:** protocol v2 (§10); guest DRM files that drive a leased
> output (§11); fences kept on the host (§12); NVKMS forwarding and its
> permission gates (§13); the Wayland proxy against the live host compositor
> (§14); a memory type per mapping, and guest system memory made GPU-coherent
> (§15); direct scanout of guest buffers by the host compositor (§16); CUDA,
> with the UVM aperture and memory registered by its pages (§5); the RM
> allowlist, enforcing; and the security changes that came with them
> ([`SECURITY.md`](SECURITY.md)). About 35 applications ran on the live
> desktop ([`rig/TESTING-RIG.md`](rig/TESTING-RIG.md)). None of it has been timed.
>
> **Built, not yet run on hardware:** a guest driving the host card itself
> (compositor-VM mode, §11) and export mode (§14), which need the host desktop
> stopped. [`TESTING.md`](TESTING.md) is the plan.
>
> **Designed but not built:** the isolate (Future work, below), MIG and
> SR-IOV.

---

## 1. The shape of it

The guest runs NVIDIA's own user-mode driver — the real `libvulkan_nvidia.so`,
the real NVENC. That driver does what it always does: it builds command buffers
in memory and talks to a kernel driver through `/dev/nvidia*`. The only thing
that differs is what is behind those device nodes.

```text
Guest                                     Host
────────────────────────────────          ──────────────────────────
Application
NVIDIA user-mode driver (unmodified)
  │
  │ ioctl / mmap on /dev/nvidia*
  ▼
virtio-nvgpu guest driver
  │  copies the request and names descriptors by handle
  ▼
  ═══ virtqueue ═══════════════════►      virtio-nvgpu backend
                                            │  translates handles, pointers,
                                            │  and file descriptors
                                            ▼
                                          host NVIDIA driver → GPU
```

Two things cross the boundary: **ioctls**, tens of times while a device is set
up, and **memory mappings**, which are set up once and then used directly. What
does *not* cross it is the work: a frame is submitted by writing to memory the
guest has already mapped, and that memory is the host's. This is why a render
loop costs nothing in forwarding — over 813,691 measured frames, the backend
served one message per 59 frames, nearly all of it setup.

---

## 2. Why this shape

### What Venus does, and why it is not enough here

The usual way to give a VM a GPU is virtio-gpu with Venus: every Vulkan call is
serialized in the guest, carried across, and replayed against the host driver.
Three consequences follow. The boundary is crossed **per API call**, thousands
of times a frame. The buffers belong to the **host**, so a compositor in the
guest cannot reliably know their state. And the guest never holds a real GPU
pointer, which means it cannot hand one to an encoder — so encoding has to
happen on the host, on a frame that has to get there first.

### What Intel and AMD have instead

Both have a *DRM native context* for virtio-gpu: the guest runs the real Mesa
driver, builds command buffers locally, and only submissions cross. That gets
95–99% of bare metal with correct buffer ownership in the guest. NVIDIA has no
equivalent in the open ecosystem, and cannot have one built from the outside,
because the guest-side driver is closed.

### What can be done instead

The one thing NVIDIA's stack does expose is its **kernel ABI** — the ioctls the
closed user-mode driver issues. gVisor's `nvproxy` already forwards exactly
those from a sandbox to the host driver, and Vulkan, CUDA and NVENC all work
through it. Under gVisor's KVM platform the application even runs in guest mode
and its ioctls arrive as VM exits, which is structurally what a virtual machine
does.

The difference is that gVisor has no guest kernel to bridge, so its sentry
catches the exits itself. A real VM needs a guest kernel module and a
transport — which is the ordinary virtio pattern, and is what this project is.

So: **forward the driver ABI, not the graphics API.** The guest keeps its real
driver, keeps ownership of its buffers, and can encode locally, because the
pointers it holds are real.

---

## 3. The pieces

**The virtio device** offers two queues and one memory region. A *control*
queue carries requests and their replies. An *event* queue runs the other way,
host to guest: in the first protocol it carries exactly one kind of message, a
host descriptor becoming readable, and protocol v2 adds fence completions, DRM
events and hotplug (§10). The *shared window* is a region of host memory
published into guest physical address space, where every GPU mapping is placed.

**The guest driver** (`driver/`, GPL, because it touches kernel symbols)
registers character devices that look exactly like the real ones —
`/dev/nvidiactl`, `/dev/nvidia0…N`, `/dev/nvidia-uvm`, `/dev/nvidia-modeset` —
plus a DRM device per GPU and `/dev/nvgpu-wl` for the Wayland proxy. It
implements open, release, ioctl, mmap and poll, and it is deliberately **not
ABI-aware**: it copies the parameter bytes a program passed and forwards them.
For the calls protocol v2 added — KMS, syncobjs and fences on the DRM nodes,
nvidia-drm's permission grants, and NVKMS — where those bytes point at more, it
follows a generated table to gather it, the same table the backend then holds
the request to (§10). RM escapes, UVM and nvidia-drm's own GEM calls go as the
first protocol's single message, with what an RM or nvidia-drm pointer reaches
carried alongside as a nested block, which the backend holds to tables of its
own (§8). Every decision about what those bytes *mean* is made on the other
side.

What the driver does read of a guest process's bytes -- the IOCTL2 walk,
RM's nested blocks and deep segments, the descriptors in them, the ranges
registered by their pages -- is where a memory-safety bug is a guest kernel
compromise, and one guest app's way into another's. So those parsers have a
Rust implementation (`driver/rust/`, built with `NVGPU_RUST=1` into a kernel
with `CONFIG_RUST`): a `no_std` core without `unsafe` or a panic path,
which copies each byte of the caller's once and decides on that copy, around
which one small file holds the `unsafe` FFI to the C that stays (transport,
pinning, DRM/KMS hooks). The Rust build has passed the same hardware
regression as the C. The C is kept, selectable (a kernel without Rust
builds it), and a differential test runs the two on the same inputs.

**The backend** (`device/`, Apache-2.0, with no VMM in its dependency list)
holds the real host descriptors, understands the ABI, translates what has to be
translated, and issues the real ioctls. A VMM adopts it by implementing a few
traits — descriptor chains, guest memory, a way to place host memory in the
window — and gets the device without patching the crate.

**The Wayland proxy** is two more pieces: a daemon in the guest that guest
applications connect to as to a compositor, and a host half inside the
backend, connected to the host's compositor. `wlwire/` is the code they share
(§14).

**The isolate** is the part that is not built (Future work, below). The intent is one sandboxed,
unprivileged helper process per guest, holding the device descriptors so that a
compromised backend, which maps all of the guest's memory, does not hold them. Today the backend holds them itself — as
an unprivileged process, which it insists on being (§17).

---

## 4. How a call travels

A program in the guest calls `ioctl` on what it believes is the NVIDIA driver.
The guest driver copies the parameter block out of userspace, tags it with the
handle that identifies which open device it came from, and puts it on the
control queue. The backend picks it up, decides what the bytes mean, fixes the
parts that cannot survive the crossing, calls the real ioctl, and sends the
result back. The guest driver copies the reply into the caller's buffer and
returns.

Four kinds of thing cannot survive the crossing unchanged, and finding each of
them was its own bug:

**Pointers.** Many NVIDIA parameter structs carry a pointer to a second block of
memory. A guest address means nothing in the backend's process, so the block
travels alongside the request, and the backend rewrites the pointer to its own
copy before the call and copies the result back afterwards. Some blocks hold
several — FIFO_GET_CHANNELLIST's two lists, which cuCtxCreate asks for, and
IDLE_CHANNELS' three arrays — and then each travels as a segment of its own.
How much each segment holds is not the guest's to say: the backend computes it
from the parameters it is about to hand RM, as RM will, and refuses a call
whose segments differ.

**Handles.** The driver's object handles are per-open-file. Each guest open of a
device holds exactly one host open, so a handle the host issues is already
scoped to the file that will use it — except where an object is named from a
*different* file than the one that created it, which is what a compositor does
with a client's buffer. Those are translated explicitly, and the owning file is
remembered with the object.

**File descriptors.** Some structures name a resource by an open descriptor
rather than a handle — NVKMS memory import is the notable one. A descriptor
number forwarded verbatim picks out whatever the backend's process happens to
have open at that number, which is not a failure with an error message; it is a
failure with a plausible wrong answer. The guest driver replaces those with the
handle of the file it refers to, and the backend puts its own descriptor back.

**Processes.** RM keeps an object to the process that made its client:
another process may duplicate it only if it was shared, and the one share RM
starts with is "the same process". Every guest process's calls are the
backend's, so to RM a whole VM is one process, and anything a guest shares RM
shares with the host. So the backend holds sharing to the VM — a share goes to
RM only when it narrows or grants to the VM's own clients — and applies RM's
own duplicate rule with guest processes in place of the backend's: the
source client and the destination client were made by one guest process, or
the source object's share list grants the destination (`device/src/rmshare.rs`).
Calls that name a second client in their parameters — a device sharing
another client's VA space, register operations or a profiler on another
client's context, other clients' channels — are held to the rule RM applies to
that field between host processes: the same process where RM checks nothing
(or checks only inside GSP-RM), the same process or the same euid where it
checks its security token, the share list where it checks a right. Which
process and which euid, only the guest kernel knows: its driver sends the
caller's identity — the thread group's PID and its leader's start time, one
process for the guest's lifetime, and the effective uid — with every
RM_ALLOC, RM_DUP_OBJECT and RM_CONTROL, to a backend that asks in HELLO
(`BCAP_PROC_ID`, `BCAP_PROC_EUID`). A guest that does not gets no duplicate
between two clients except by a grant, and names no client but the caller's
own: fail closed. A client handed to another process with its file stays its
maker's, as RM's does. Processes in one guest that share GPU work do it
through descriptors (dma-bufs, RM's export to a file) rather than handles, and
cross nothing here.

---

## 5. How memory travels

Nothing copies a frame. When the guest maps GPU memory, the backend maps the
same memory on the host and **places it in the shared window** — a region the
VMM has published into guest physical address space. The guest driver then maps
those guest-physical pages into the calling process. Both sides are looking at
the same memory, and the guest reaches it at full speed with no one in the
middle.

The window is a finite resource, so placements are tracked and given back when
the mapping goes away. Three things make that subtle. The caching attribute has
to match what the driver asked for — write-combining for device memory,
write-back for system memory — or the mapping is correct and unusably slow
(§15 says how the guest learns which, and why system memory is also a question
of correctness).
Placement has to be done by whoever owns the guest's address space, which is
the VMM and not the backend, so it travels as a request rather than a call.
And anything that fails to give space back fails *later*, in whatever mapping
happens to be next, which is why the accounting is explicit rather than
implicit.

Memory the guest already has travels the other way, and **cannot be handed
to the GPU by address.** RM registers existing memory by CPU address — an
NV01_MEMORY_SYSTEM_OS_DESCRIPTOR object (through RM_ALLOC or ALLOC_MEMORY)
or VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR — and pins whatever that address
maps in the calling process. The calling process is the backend, so a guest
address would name the VMM's memory, and the GPU would read and write it.
The backend refuses all three when they come with an address alone (EPERM).
That is how `cuMemHostRegister`/`cudaHostRegister` and Vulkan's
`VK_EXT_external_memory_host` import memory, and how `cuCtxCreate` registers
a 2 MiB buffer of its own.

So the pages travel instead, to a backend that says it takes them
(BCAP_OS_DESC, offered when it holds the vhost-user memory table and was
started with `--allow-compute`; see "Compute is opt-in" below). The guest
driver pins the caller's range the way RM would — long-term, and for
writing unless the call asks for memory read-only to the CPU — and sends its
guest-physical page list, as runs, with the call (`driver/nvgpu_osdesc.c`).
The backend checks the call is one of the three with the user-virtual-address
descriptor type (a physical address, a page array or a dma-buf by descriptor
is refused whatever came with it), that the list covers exactly the pages RM
would pin, with the same writability, and that every page lies in a region of
guest RAM. It turns each page into its own address through the memory table
it already holds (guest RAM is mapped into the backend for the virtqueues),
and hands RM an address it owns: directly when the pages are contiguous in
one region of that mapping, otherwise a range of its own, reserved
`PROT_NONE` and then mapped run by run, `MAP_FIXED | MAP_SHARED`, from the
regions' memfds (`device/src/osdesc.rs`). The caller's offset inside its
first page is kept. RM pins the VMM's view of exactly the guest's pages, and
the caller reads back its own address.

RM keeps nothing of the address once it has pinned (the escape layer hands
the rest of RM a page array), but the backend's range stays mapped until the
host kernel lets go all the same, and "lets go" means every holder, not just
the guest's handle. RM holds the pages for the object the call made, for
every duplicate of it, and for every object it made over one and keeps a
duplicate of its own for (a semaphore surface, a memory mapper over one),
and for every duplicate a semaphore surface hands back (REF_MEMORY):
each goes when RM frees it, its parent or its client, when the file the
client was made on closes — the backend frees the client itself first, so
the pages are released by then — or with the session. nvidia-uvm holds them
for every external mapping of any of those, in a client of its own, until
UNMAP_EXTERNAL has taken it off every GPU, UVM_FREE takes its external range,
or the UVM file closes; the backend takes the mappings down itself before it
lets that file go, since the event pump's duplicate of the file may outlive
the close. What the backend could not follow it refuses: an RM export to a
descriptor, an NV_MEMORY_EXPORT attach and UVM's ALLOC_DEVICE_P2P, naming
registered memory, are EPERM. Those exports are also the only way a user
process can name RM memory to NVKMS or to nvidia-drm, so the refusal keeps it
out of both.

Only when the last holder is gone does the guest unpin: the reply to a
registration carries an id, and the guest asks for the ids released
(HOST_OP OSDESC_REAP) after every RM_FREE, every close and before every new
registration, and unpins what is named. Until then the pages are out of
ballooning and migration, as RM would keep them. Every release the backend
cannot see happen is taken late, never early. Registrations, bytes,
separately mapped runs and UVM mappings are bounded per file, per guest
process and per VM.

Registered memory is guest RAM, which the guest caches write-back, so every
GPU mapping of it snoops, as for any system memory the guest can see (§15).
RM takes an OS descriptor of ordinary pages only write-back, so there is no
coherency to rewrite at registration.

### The UVM aperture

One mapping cannot go in the window at all. Creating a CUDA context makes a
UVM semaphore pool at an address the caller chose and then maps the UVM file
there, at an offset equal to that address. UVM takes the mapping only at the
host address equal to the offset, only for the pool's exact range, and — in
its default mode — only from the process that initialised the file, which is
the backend. The window is none of those: its host address is wherever the
VMM happened to reserve it, and the VMM is not the backend.

So each pool gets a slot of its own. The backend initialises every UVM file in
multi-process sharing mode, which lifts the one-process rule (and also turns
pageable memory access off on every release, which the backend wanted anyway).
When the guest maps a pool, the backend checks it is one this very file was
seen to create, asked for exactly, and hands the VMM the file with the
existing SHMEM_MAP request on a second shared-memory region, the **UVM
aperture** (region 2). The VMM maps the file at the pool's own address in its
own address space, never over anything of its own (nesbox with
`MAP_FIXED_NOREPLACE`; crosvm over a band it reserved for pools at start-up,
from its main process, after its jailed frontend and then the main process
have checked the request), checks the pages are really present, and gives
that range a memory slot inside the aperture at an offset the backend chose. The guest maps its vma from there,
write-back. Taking it out goes the other way round: the slot first, then the
mapping, so the guest never has a slot over nothing.

A second region rather than more window because the window is one slot over
one reservation, and a pool's host address is not ours to choose. nesbox
gives the aperture a BAR of its own; crosvm puts it after the window in the
window's BAR, with a capability of its own. The guest looks regions up by id
and does not care which. One slot
per pool because the address is fixed per pool; a slot costs about 0.7 ms to
add and 2 ms to remove, once per CUDA context. UVM itself keeps the pages
alive: it refuses to free a pool that is still mapped, and the VMM's mapping
counts.

### Compute is opt-in

Everything in this section that exists only for CUDA — the UVM device with
its sharing mode and range groups, the UVM aperture, and memory registered by
its pages — is served only when the backend is started with
`--allow-compute`, and is off by default. Without it the backend refuses
every open of `/dev/nvidia-uvm` and `/dev/nvidia-uvm-tools`, offers neither
BCAP_UVM_MAP nor BCAP_OS_DESC, and says so in HELLO (no `BCAP_COMPUTE`); the
guest driver then makes no UVM device and does not register the
`nvidia-uvm` major, which NVIDIA's userspace reads as a host whose
nvidia-uvm is not loaded. Vulkan, OpenGL, EGL, Vulkan Video and the display
paths use RM, NVKMS and nvidia-drm and none of this (SECURITY.md, "Compute").
`rig/run-guest.sh --allow-compute` (or `NVGPU_COMPUTE=1`) turns it on
for a run.

---

## 6. How a buffer becomes shareable

Rendering needs none of this. Presentation needs all of it.

A client renders into a buffer and hands it to a compositor. Inside a single
guest that is an ordinary dma-buf export and import — and both go through the
DRM render node, not through the NVIDIA character devices. The core DRM code
serves them out of the node's own object space, which for a forwarding driver
is empty unless something fills it.

So each host object gets a **proxy object** in the guest standing in front of
it. The guest's own DRM core does the export, the import and the handle
bookkeeping, exactly as it would for a real driver; only three things cross the
boundary — creating the host object, closing it, and the operations that act on
it, each translated to the host's handle and sent back to the file that owns it.
A compositor can outlive the client whose buffer it holds, which is why the
owner is remembered rather than assumed.

The memory behind such a buffer is the host's, reached through the window as in
§5. Every path to it — mapping the node, mapping the dma-buf, a kernel mapping,
the DMA address an importer receives — resolves to the same placement, so they
all name the same bytes.

`/dev/nvidia-drm` and `/dev/nvidia-modeset` are part of this, and nothing in
this section is a display: they are how NVIDIA's stack names shareable memory.
A guest rendering and composing for itself, on a streaming box whose frame
leaves as video, needs no more. The display paths are built from the same
objects — a guest buffer the host compositor scans out is one of these proxies'
host objects (§16), and a guest driving a leased output makes its framebuffers
from them (§11) — but they are separate paths, and the sections from §10 on
describe them.

---

## 7. How a guest waits

A frame ends with waiting for the GPU, and waiting is not free to arrange across
a VM boundary.

NVIDIA's user-mode driver waits by polling the descriptor its completion event
is delivered on. The interrupt belongs to the host, and so does the descriptor
that becomes readable when it fires. The guest's descriptor knows nothing about
it — so the backend watches each descriptor it has opened and, when one becomes
readable, sends a message on the **event queue** naming it. The guest driver
finds the file that handle belongs to and wakes whoever is sleeping on it.

This is worth stating plainly because its absence is invisible. A driver whose
`poll` implementation is missing is reported by the kernel as *permanently
ready*: every wait returns immediately, the user-mode driver finds nothing and
tries again, and the guest spins a whole CPU core while producing correct
frames slightly faster than bare metal. It reads as a performance win. It is a
core per guest, and on a machine whose business is guests per host, it is the
most expensive thing that can happen.

The cost of doing it properly is that a wake now takes about a third of a
millisecond — the host's poll, the queue, an interrupt, and a vCPU that has
gone idle. That is nothing against a 16.7 ms frame and everything against a
0.05 ms one, which is exactly what the benchmark shows.

Protocol v2 carries more on the same wake — a fence's status, a flip event —
and turns every wait a guest could otherwise ask a host thread to sit in into
a poll, with the sleeping done in the guest (§10, §12).

---

## 8. Versioning against a moving ABI

NVIDIA's kernel ABI is not stable: structure layouts change between driver
releases, and there is no compatibility promise to hold them still. Support is
therefore explicit — the backend knows which layouts belong to which driver,
learns the host's version during initialisation, and uses the right table. It
refuses to start on a release its tables were not measured at, and a request
whose size does not match what that version expects is refused rather than
guessed at.

Adding a new driver version means extracting the tables from that release's
open kernel modules, checking them, and naming the release as measured
([`DEPLOY.md`](DEPLOY.md), "Upgrading the host driver"). It is
the same maintenance burden `nvproxy` carries, and their work can be followed
directly.

The tables are generated and checked in, so a build needs no NVIDIA source, and
the generator is in the repository so the tables can be regenerated rather than
trusted.

Protocol v2 and the work around it need layouts that move more often than the
profiles do, so those are measured per **release**: NVKMS and nvidia-drm ioctl
layouts, the pointers RM follows inside control parameters and how much it
copies through each, and UVM's parameter block sizes, each extracted from a
release's own sources by compiling probes against them. Only under
`--allow-unmeasured-release`, a diagnostic flag, does a host between two
measured releases use the older tables, and then the NVKMS commands whose
layout changed in the next measured release run only on the exact release
their table came from; the RM control pointer table is the union of every
release's. The ioctl schema both
halves interpret (§10) is written once and generated into a C table for the
guest and a Rust table for the backend, and a test fails if either checked-in
copy is not what the generator writes; the RM copy sizes the guest sends
segments by are rendered the same way, with the backend's table, from the same
measurements.

---

## 9. Getting a frame out

What runs today is **Vulkan Video**. A capture layer inside the guest takes the
client's swapchain image and encodes it on the client's own device — no second
device, no CPU copy, no CUDA. The encoded stream leaves over a socket. This is
the path the measurements come from: a 60 Hz H.264 stream that decodes without
an error.

The CUDA route — importing a rendered image into CUDA, encoding it with NVENC
from a GPU pointer — is the one the project was originally designed around. It
needs `--allow-compute` (§5), and it has run: ffmpeg's NVENC and NVDEC through
CUDA, nvidia-vaapi-driver, Blender's Cycles and a CUDA n-body in a guest, on
the RTX 5090. It is a second path rather than a fallback.

What is deliberately out of scope is unified memory: `cudaMallocManaged` and
page-fault-driven migration need fault handling across the VM boundary and
precise virtual address matching. Device allocations and graphics interop do not,
and they are what an encode pipeline uses.

The other way out is a monitor: the display paths of §10–§16, which put the
guest's frames on an output the host drives rather than on a socket.

---

## 10. Protocol v2

The protocol so far — open, ioctl, mmap, close, one kind of event — is enough
to render, encode, and share a buffer inside the guest. Display needs more:
calls that carry several buffers, descriptors and GEM handles at once; answers
that arrive out of order, because a modeset takes a while; events with a
payload; and a clock both sides agree on. That is protocol v2, and it is
negotiated rather than assumed.

**The session.** The guest driver says HELLO when it probes, before it
registers its DRM devices. An older backend answers it the way it answers any
message it does not know, and the guest stays on the first protocol, with no
display features at all. The backend's reply says what it offers — the host's
card nodes, a Wayland socket, fences, an NVKMS table for this host's release,
export mode — and how large one request may be. The guest's side says what it
can: the UVM aperture it found, and that it can name the process behind each
RM call and its euid (§4). Whether compute is served is the backend's to say
(§5, "Compute is opt-in").

A HELLO also says whether this is a fresh driver instance, and a fresh one
resets the session: every host file the previous instance held is closed,
every watch dropped, every queued call withdrawn. A device reset does the same.
Without it, a guest that reboots leaves its card DRM master and its lease
granted, and the host's output stays dark. Every change to a ring moves an
epoch as well, and an answer computed under an old epoch is dropped rather
than written into whatever ring is there now. Until a HELLO succeeds, the
backend speaks only the first protocol: no IOCTL2, no v2 events, no memory type
per mapping in the MMAP reply. The security changes that came with v2 are not
part of the protocol, and a v1 session gets them too: the coherency rewrite
(§15), UVM held to its block sizes per release, and the refused classes and
escapes ([`SECURITY.md`](SECURITY.md)).

**IOCTL2, and who decides.** An ioctl whose argument points at more memory, or
names a descriptor or a GEM handle, travels as one IOCTL2: the argument, every
buffer its pointers reach in a fixed traversal order, and a record for each
descriptor and each GEM handle saying where it sits. Both halves read the same
generated table (§8). The guest reads it to know what to gather. The backend
reads it to decide what the request may be: it walks its own copy over the
bytes it received, recomputes every length, pointer, descriptor position and
handle position from them, and refuses a request that disagrees in any of
them. Then it builds what the host kernel is handed itself — each pointer
aimed at a buffer exactly as long as the kernel will read from the same bytes,
each descriptor one it duplicated for this call from a handle of an allowed
kind, each descriptor the kernel may write pre-filled with -1, so that a value
the kernel did not write is never taken for one it did. The guest kernel is
untrusted, so its layout never is either. Output buffers come back even when
the call failed, because DRM and NVKMS both copy back on error.

The first protocol's IOCTL message stays for RM escapes, UVM and nvidia-drm's
own GEM calls, from every guest, and for a v1 guest's NVKMS commands, of which
it takes only those with no pointer and no descriptor. RM's embedded pointers
travel as that message's nested and deep blocks, and the backend holds them to
its own RM control table (§8), not to the IOCTL2 schema. A deep block carries
one pointer, or, to a backend that says so in HELLO (`BCAP_DEEP_SEGS`), a
segment for each pointer of a control or of IDLE_CHANNELS; a guest that sees no
such bit sends one or none, and the backend zeroes the rest. The message is refused
on every kind of handle v2 introduced, so it cannot be used to go around
IOCTL2's checks.

**Where a call runs.** The backend's queue thread serves the control queue in
order, so whatever it waits on, everything behind it waits on too: RM calls of
unrelated guest processes, CUDA, other files' flips. Host display calls do
wait. A blocking atomic commit holds the modeset locks through two waits of up
to three seconds each; a connector probe waits behind it; NVKMS commands queue
behind a mode set. None of those waits can be interrupted, so they cannot be
cancelled either — they can only be moved. The rule is that the queue thread
never issues one. Every call on a card, lease or modeset file, and every call
the table marks as able to wait, runs on that host file's own **executor**: a
queue per file, at most one call running per file — the order a
single-threaded native process would see — and different files in parallel on
a pool of at most sixteen threads. An IOCTL2 is prepared under the backend's
lock, runs without it, and is finished under it again. Every other message
still runs under that lock on the queue thread — RM escapes, UVM, nvidia-drm's
GEM calls, a v1 guest's NVKMS commands — and so do the lease checks of §11.
The guest waits for an answer with a timeout of its own (30 s for a call the
queue thread runs, 60 s for one on an executor), and if it gives up, it still
cleans up whatever the late answer creates.

**The closer.** Closing is a host call too, and not a cheap one. The last
close of an nvidia-drm file, of a DRM master, of a file with framebuffers still
on a plane, or of an NVKMS file can run a modeset in the thread that closes it.
So the backend's last reference to a display file is dropped by one thread
whose only job that is — `nvgpu-closer` — and never by the queue thread, the
event pump, or an executor under the lock. A close therefore finishes a moment
after the guest was told it had. The one thing that depends on it having
finished, opening a card as master right after the previous master closed,
waits for the closer to be idle first.

**Events with payloads.** In v2 the event queue carries records instead of a
bare handle: readiness, a fence's completion and status, raw DRM events read
from a card or lease file, and hotplug. Nothing about it may grow without
bound. Readiness and fence records are coalesced per watch, so there are never
more of them than there are watches. Undelivered DRM events are held to 4 KiB
per file; past that the backend stops reading the file, and the host kernel's
own per-file event space pushes back on whoever queues events there — exactly
what happens to a native client that stops reading. Dropping them instead would
lose flip completions that a compositor waits for forever. A record never
splits a DRM event, and the pump never blocks on a read. Descriptors that stay
readable until the guest drains them get a sweep every millisecond, but only
over the handles reported readable and not yet drained; an idle handle costs
nothing.

**Two clocks.** Vblank and flip events, fence signal times, Wayland
presentation feedback and the GPU/CPU time correlation RM reports all carry the
host's time, and a guest compositor paces its frames by them. The guest
measures the offset between the clocks with a dedicated message: eight
samples, the one with the shortest round trip kept, the host's reading taken
as belonging to the midpoint. It measures again every five seconds, and walks
the offset in use toward each new measurement at no more than 50 µs a second,
so two timestamps a frame apart can never come out in the wrong order. The
host's realtime and raw monotonic readings are translated through each side's
monotonic clock, never by applying the monotonic offset to them directly:
realtime is not the same clock on either side.

---

## 11. A guest DRM file, and KMS

Every guest DRM file stands in front of a host render-node file, which owns
the file's GEM objects and serves its render ioctls, syncobjs included — §6,
unchanged. A file may also have a second host file behind it, used only for
KMS: a lease, or the host's card itself.

**Adopting a lease.** A lease reaches the guest from the host compositor over
the Wayland lease protocol (§14), or from a CREATE_LEASE on a card the guest
drives. Either way it arrives as a backend handle, and the guest kernel
*adopts* it: it opens a clone of one of its own card-node files, binds the
lease to it as the file's KMS side, opens a render file for it, and installs
the result as an ordinary descriptor. The program holds a guest DRM file whose
KMS ioctls go to the host lease. The backend takes a descriptor for a lease
only if the kernel says it is an nvidia-drm card node of this very GPU, never
because the compositor or the guest said so.

In compositor-VM mode (`--kms-card`), the guest's own card-node files get a
host card file behind them instead, opened the first time one issues a KMS
ioctl or becomes DRM master.

**Master.** The guest's DRM core decides who is master, exactly as on bare
metal: first opener, SET and DROP_MASTER with its own permission checks
against guest processes. The host cannot decide it, because every host file
belongs to the one backend process, and the host's own check would pass for any
guest process holding a descriptor that was ever master. So the host mirrors
the guest. Becoming guest master sets master on the host card file, and losing
it drops it there; a host card opened for a file that is not guest master is
dropped from host master at once, so a program that only probes a card can
never walk off with the display. A host file opened while another held master
can never become master itself, so if the guest later makes such a file master,
a fresh host file is opened for it, and the old one is kept until the guest
file goes, so that the framebuffers made on it stay valid.

**What travels.** KMS ioctls go as IOCTL2 to the KMS side, on its executor.
The few the DRM core serves itself — version, the magic pair, master, client
name — stay in the guest. GEM handles never live in a KMS file: a
framebuffer's buffer is moved into the card or lease file for the one call that
needs it, checked to be NVKMS memory, and closed again in the same job, so
nothing can replace it between the check and the use. The check matters
because nvidia-drm treats any object it is handed for a framebuffer as NVKMS
memory. Handles coming the other way, from dumb buffers or GETFB, are moved
into the file's render side and get proxies there.

**What a lease may name.** Framebuffer ids are global on a device, and a lease
does not filter them, so a lessee could name any framebuffer on the card — the
host's desktop, another VM's output — as a plane's source, and read it back.
The backend records which framebuffers each VM made and refuses every scanout
call that names another, and GETFB hands out handles only for framebuffers the
same file created. A property whose value is a descriptor or a pointer — the
fence properties — is refused on the legacy setters, and on an atomic commit
goes only through the fence path (§12); a property a newer driver names like
one is treated as one.

**Events.** Flip and vblank events come back as DRM records for the KMS side.
The guest reserves event space in the caller's DRM file when the request is
made, as the core would, matches each event to its reservation when it
arrives, moves its timestamp into the guest's clock, and delivers it. A vblank
wait that would block is sent as its event form, and the caller sleeps in the
guest, not on a host thread. In compositor-VM mode the host card's hotplug and
lease uevents — which a compositor learns of in no other way — are raised
again on the guest's card.

**When a lease ends.** A lease can end without its holder closing it: the
compositor exits, the monitor is unplugged. nvidia-drm does not always follow,
so the backend asks about every lease it holds on each lease uevent, and about
the leases NVKMS grants were made through before each NVKMS call, once a
second, and at once when a connection to the host compositor hangs up. All but
the check before an NVKMS call come from a listener that runs only with
`--kms-card` or `--wayland-lease`. The NVKMS grants made through a lease that is gone for good are ended (§13),
and outside compositor-VM mode, a lessee file that holds nothing any more and
had a grant made through it is closed.

---

## 12. Fences

Every fence a guest process holds is the host's. A semaphore-surface fence is
signalled by the host's RM from a GPU interrupt, a KMS out-fence by the host's
flip, a compositor's release fence by the host compositor — and the consumers
that matter are on the host too, the compositor's GPU wait and the host's
scanout. So the host keeps the objects, and the guest keeps proxies.

- A host `sync_file` becomes a real guest `sync_file` around a proxy fence,
  which signals when the host's does, with the host's error and at the host's
  signal time. It has to be a real one: every guest-kernel consumer takes a
  `sync_file` through the kernel's own checks, and NVIDIA's userspace merges and
  queries them — which then needs no round trip.
- A syncobj handle is the same number on both sides. A guest DRM file stands
  for exactly one host render file, and syncobj handles are per file, so there
  is nothing to translate. A syncobj exported as a file becomes a guest file
  standing for the host's.
- A semaphore-surface fence context is a GEM object on the host, and the guest
  gets a proxy of its own kind that nothing can export or map.

Handing a guest fence back to the host is *unwrapping*: a proxy gives its host
handle, a merge of proxies becomes a merge on the host, and anything the host
cannot see is waited for in the guest first.

**Nothing waits on the host.** A syncobj wait forwarded as it stands would
park a host thread for as long as the guest asks — on the queue thread, every
guest process with it. So the backend turns every forwarded wait into a poll,
and the guest does the sleeping, woken by the host kernel's syncobj eventfd,
which the event pump reports. Those host registrations cannot be taken back
once made; they last until the point signals. So there is one per (file,
syncobj, point, flags), shared by every guest waiter on it; the syncobj is kept
alive while it lasts; and one VM may have only so many unfired. Past that cap
the guest polls with a short backoff, which costs it latency and the host
nothing.

**Semaphore surfaces.** nvidia-drm's fence-context call names an RM object by
client and handle, takes it into NVKMS at kernel privilege, and then reads
kernel memory at an index the caller chose, unchecked. So before the host sees
one, the backend checks that the RM client is one this VM allocated, that the
index lies inside the surface by the layout the host itself reports, and that
the VM has not run past its count of live contexts.

---

## 13. NVKMS

`/dev/nvidia-modeset` is one ioctl multiplexing some sixty commands, whose
parameter blocks hold pointers three levels deep, surface and permission
descriptors, and nothing the host could take as it stands. It travels as
IOCTL2 by the table generated for the host's release (§8): the guest gathers
what the table names and turns each descriptor into the handle of one of its
own files of the right kind, and the backend walks its own copy. Without a
table for the host, the backend offers none, and NVKMS calls that would need
one are refused.

The table makes a call well formed. It cannot make it harmless, because NVKMS
trusts its callers far more than a VM boundary may. It has no process,
credential or capability checks, and several of its commands check no
permission at all though they reach every display on the GPU — the cursor,
layer positions, display attributes, and a "query" whose override flags are
stored and outlive the call. So the backend decides, from state of its own.

**Grants.** Outside compositor-VM mode, a guest holds a display only through a
lease. It asks nvidia-drm to grant one display on the lease to a fresh modeset
file, then acquires the grant through NVKMS — which is what
`vkAcquireDrmDisplayEXT` does. The backend records what was granted through
which lease and what each acquire gave to which file, and holds the unchecked
commands, every head a flip names and every head a committed mode set touches
to those records. The records go when nvidia-drm's do — a revoke, a close, the
device freed — and also when the lease under them ends, which nvidia-drm does
not always notice. A gated call carries the generation of revocations it was
decided under, and is refused when it comes to run if a grant was taken back
in between; a revocation the guest starts waits for a gated call already
running.

**Fresh files.** The first ioctl on an NVKMS file makes it an ioctl file for
good, and such a file can never receive a grant. The guest never issues one of
its own on a modeset file, and the backend refuses, in any descriptor field, a
modeset file an NVKMS call has already been made on.

**Refused and rewritten.** Taking ownership, display-wide attributes and
framelock only in compositor-VM mode, where the guest owns the display anyway;
the head gates are lifted there too. Kernel-client and device-wide commands
never. The query's overrides are always cleared. A flip's request to announce
its completion to every client with flip permission on the head is cleared
outside compositor-VM mode, because nvidia-drm's own client would receive one
for a flip it never queued and warn — a host log flood, or a panic under
`panic_on_warn`. Event interest is cut to what a display client needs, since
NVKMS keeps each file's event list without a bound. And how often a VM may make
the host probe a display is limited.

nvidia-drm's own grant and revoke ioctls are allowed only for the MODESET kind
— never for sub-ownership — with a fresh modeset file, and a revoke only for a
display granted through the same file.

---

## 14. The Wayland proxy

In the default display mode a guest application is a client of the host's
compositor.

**One host connection per guest client.** A guest client connects to the guest
daemon's socket as it would to any compositor. The daemon opens a channel
through `/dev/nvgpu-wl`, and each channel is one backend handle and one
connection from the backend to the host compositor. Between the two ends
travel frames: the Wayland messages themselves, and beside them a table saying
what each descriptor was and how the far side rebuilds it, because a
descriptor cannot cross a VM boundary as a descriptor. Object ids are not
translated and the proxy creates no objects; it is one client on each side,
one to one. The only messages it makes up itself are a protocol error to a
local peer that broke the rules, and the lease device's `released` event,
which the protocol promises and stock Hyprland 0.56 never sends.

**Everything is parsed.** Both ends run one engine, generated at build time
from vendored protocol XML, and every message in either direction is parsed
against it: the target object must exist and the opcode be known at that
object's version, or the connection ends with a protocol error. That is not
pedantry. libwayland hands out received descriptors from one queue per
connection, in the order messages consume them, so a single message whose
descriptors the proxy did not count would desynchronise every one after it.

**The allowlist is enforced on the host.** The guest daemon filters too, but a
guest kernel is free not to run it, so the backend applies the allowlist to
whatever the guest sends. A global is offered only if it is on the list and its
requirement is met, at the lowest of the version the compositor advertises,
the version the vendored XML knows and the list's cap; binding anything else is
a protocol error. The list starts from the set Hyprland gives a client it does
not trust — which is what a guest application is — and adds output information,
content type, and two opt-ins: the lease device (with `--wayland-lease`, only
for a device on this GPU, and only to a guest that can adopt DRM files) and
explicit sync (only when fences are served). Left out on purpose: screen
capture, clipboard managers, virtual input, input methods, layer shell,
session lock, output management and `wl_drm`. The build checks the list against
the XML: it walks every object reachable from an allowed global, and fails if
any of them can carry a descriptor that has no class.

**Descriptor classes.** Every descriptor a reachable message can carry is one
of six kinds:

- a **dma-buf plane**: guest to host, it is the host GPU object behind the
  guest's buffer, exported on the host itself, so the compositor receives the
  host's own dma-buf and nothing is copied; host to guest, in export mode, it is
  imported into the channel's render file;
- a **shared-memory pool**: the far side makes memory of its own, of the
  pool's size, and it is kept filled from the near side (below);
- a **blob**: a small read-only file — a keymap, a format table, a colour
  profile — copied by value into a sealed file on the far side;
- a **stream**: the write end of a pipe, for clipboard and drag-and-drop data,
  carried as a flow-controlled byte stream with an explicit end;
- a **DRM file**: a lease, or the lease device's query file, adopted by the
  guest kernel into a guest DRM file (§11);
- a **syncobj**: explicit sync, guest to host only, carried as the host syncobj
  behind the guest's.

A descriptor that cannot be carried — a dma-buf the guest kernel does not own,
an export that failed — becomes an empty file on the far side, so the message
still consumes one descriptor and the peer fails that one request in its own
way instead of losing the connection. A syncobj is the exception: a timeline
the guest kernel cannot name ends the client with the protocol's
`invalid_timeline` error, which the compositor would raise anyway.

**Shared memory is copied at commit.** A client's shared memory is guest RAM,
which the host compositor cannot map. So the host side makes memory of its own
for each pool and hands that over in the pool's place, and when a surface
commits a shared-memory buffer, the guest side first sends the rows of it that
may have changed. The commit is the right moment: a client may not touch a
committed buffer until the compositor releases it, so what is copied then is
exactly what the compositor may read, and the release is forwarded untouched,
so the client's pacing stays the compositor's. The guest side only ever reads
the client's pool, never maps it, so a client that shrinks its pool under the
proxy gets short copies rather than a crash. The host's memory is charged to a
budget per connection, per guest process and per VM before it can be
written, because it is
memory the host's OOM killer would not count as the backend's. What is charged
is what it can come to hold, not the pool's size: the host's copy of a pool is
made at full size but empty, only the parts a live buffer covers are ever
written, and those are charged when the buffer is made and freed again when
the last buffer over them goes. A terminal like foot, which makes a 512 MiB
pool and scrolls by moving its buffer through it, holds what its buffer takes.

**Bounded both ways.** The host compositor disconnects a client whose output
buffer fills, and the guest reads when it gets round to it. So the backend
reads every host connection eagerly, on a thread of its own, and queues what it
translated until the guest takes it, within a budget per connection, per guest
process and per VM; a guest that stops reading loses the connection rather than
the backend its memory, and a process that stops reading loses its own
connection, never another process's. The guest daemon charges each client's
connection to that client (NVGPU_WL_IOC_CONNECT_FOR), and holds at most 4 MiB
a client has not read before it stops taking that client's output. The other way, the backend takes no more from the guest while a
compositor that is not reading has too much waiting, and the daemon stops
reading its client, so the client's own library buffer is where it waits. The
number of channels one VM may have, and how often it may ask for a lease, are
limited as well.

**Numbers that name the machine.** Device numbers in dma-buf feedback are
mapped between host and guest nodes — a host node with no guest counterpart
becomes 0, never another device's number — and presentation timestamps are
moved between the two clocks (§10).

**Export mode.** In compositor-VM mode the roles turn round. The backend
listens on a socket of its own, created for its user alone and accepting only
peers of its own uid, because the guest compositor is no more trusted than the
VM. Each host client that connects becomes a channel toward the guest, where
the daemon, in export mode, connects it to the guest compositor. The same
engine runs with client and server swapped, and a host client's dma-bufs are
imported into the channel's render file. Only one daemon may listen per
device, and only its user may accept, so no other guest process can become a
host program's compositor. Explicit sync is not carried this way yet.

---

## 15. Memory types, and coherency

§5 says a mapping's caching attribute has to match what the driver asked for.
In the first protocol the guest could not know what that was, and mapped every
placement write-combining: the usermode doorbell became a write-combined store
where the host driver's own is uncached, and every read of cached system memory
an uncached one. Now each mapping's answer carries the type the host mapped it
with — uncached for registers and the doorbell, write-combining for video
memory, and for system memory the cache type it was allocated with — and the
window has a zone for each. A mapping the host made read-only is made
read-only in the guest too, so a write faults in the guest process as it would
natively, instead of reaching KVM as a fault nobody can resolve, which stops
the whole VM. And a placement stays placed until the last guest mapping of it
goes, even if its RM mapping was undone or its file closed first.

**Coherency.** For system memory the type is not only a question of speed. RM
allocates system memory uncached unless asked otherwise, and the GPU does not
snoop it. On an Intel host, KVM maps guest RAM write-back whatever the guest's
page attributes say, unless the VMM disables `KVM_X86_QUIRK_IGNORE_GUEST_PAT`.
So the guest caches memory the GPU reads and writes without looking at that
cache: the guest sees stale semaphores, and the GPU stale pushbuffers.

The GPU's view is chosen twice — once for RM's own mappings, once for each
mapping a client makes — so the backend makes both coherent. Guest system
memory is allocated write-back, every GPU mapping of it snoops, and so does a
context DMA over it; the caller reads back the bits it sent. Memory the guest
registered by its pages (§5) is write-back to RM already, and its GPU
mappings and context DMAs snoop the same way. A cached guest
view is then correct, because the GPU snoops what the CPU has cached. RM allows
that exactly where context DMAs may snoop, which it sets by default and clears
only for Tegra.

Display memory is left as its client allocated it: the display reads it
through a context DMA whose snoop setting its client chose to match, and on a
platform where display must not snoop, that memory is non-coherent by
definition. Instead, NVKMS's answer to which model a client should use is
narrowed to the coherent one where the device offers both, so display memory a
client allocates after asking is coherent from the start. What stays exposed on
Intel is display memory a client allocates non-coherent regardless; the
backend warns at startup, and the VMM should disable the quirk. AMD's nested
paging honours the guest's page attributes and needs none of this.
`--keep-guest-coherency` turns the rewrite off, for ruling it out while chasing
something.

Write-back also needs the guest's MTRRs to agree. Where the VMM's firmware does
not cover the window's write-back zone, the guest quietly maps it uncached
instead: slower, never wrong, and the guest says so once when it probes.

---

## 16. Direct scanout

A compositor scans a client's buffer straight out to a plane, with no
composition, when the window covers the output and the buffer is one the
display can read, in a format and modifier the plane takes. On NVIDIA hardware
that means NVKMS memory with a modifier from the plane's list.

A guest client's buffer is already that. Its GEM object is a proxy for a host
nvidia-drm object in the file's render node (§6), allocated by NVIDIA's own
driver exactly as it would be on the host. When the client hands the buffer to
the host compositor, the backend exports that very host object, and the
compositor imports the host's own dma-buf of the host's own memory. From there
the choice is the compositor's, made as it makes it for a native client.
Nothing on the path copies, converts or composites.

For the client to allocate such a buffer in the first place, two things must
hold. The dma-buf feedback the compositor sends names devices by number — its
main device, and the primary node a scanout tranche is for — and the guest's
numbers are not the host's; the proxy maps them both ways, card nodes included,
so a guest client finds the scanout tranche and picks a modifier from it. And
the modifiers the guest's driver can produce must be the host's, which they
are, because the guest runs the host's own build of the user-mode driver
against the host's GPU.

That is the reasoning, from the code and the host driver's sources. On the rig
it held: with Hyprland's `render:direct_scanout` on, a fullscreen guest
`weston-simple-egl` was scanned out directly on a desktop monitor, with no
blocker reported for the whole run (stage 3 of [`TESTING.md`](TESTING.md)).

---

## 17. What this design cannot do

- **NVIDIA only.** It proxies one vendor's kernel ABI. Nothing here generalises.
- **Version-locked**, as §8 describes — and the display tables per release.
- **A real security surface, and not the one VFIO gives you.** Forwarded ioctls
  reach the host's NVIDIA module, so bugs in those paths are reachable from a
  guest. There is **no IOMMU boundary between guest GPU work and the host**: the
  card belongs to the host's driver and sits in the host's IOMMU domain, and the
  guest gets the driver's interface rather than the device. What isolates one
  guest from the host is the GPU's own MMU, with page tables RM programs on the
  guest's behalf — so the host NVIDIA driver is in the TCB.

  VFIO passthrough with an IOMMU is **strictly stronger**: there the hardware is
  constrained to the guest's own memory. An earlier version of this document
  called the two "the same trade-off", which was wrong. `nvproxy` is the right
  comparison, and gVisor is explicit that it reduces attack surface rather than
  providing an isolation boundary.

  What narrows the surface here: ioctls the ABI profile does not describe are
  **refused**, not forwarded, and no guest pointer reaches the host driver as
  a pointer — every field the host would follow is pointed at a buffer of the
  backend's or zeroed, and what cannot be made so is refused
  (`device/src/guestptr.rs`); UVM runs with pageable memory access off and
  only its range-, handle- and GPU-level commands. RM shares stay inside the
  VM, and a duplicate or a second client named in parameters must be the VM's
  own (§4). IOCTL2 holds every display
  call to the backend's own table (§10), NVKMS is held to grants (§13), a lease
  may scan out only its own VM's framebuffers (§11), the Wayland allowlist is
  enforced on the host (§14), and the backend refuses to run with privileges
  the host drivers would hand on to every guest process: RM, DRM and NVKMS take
  a guest's privilege from the backend's credentials, so it will not start as
  root and drops every capability. `RM_ALLOC` classes and RM control
  commands are allow-listed per release, default deny. What does not, yet:
  the backend holds the host descriptors itself — the isolate is why that one
  is in the design at all.
  [`SECURITY.md`](SECURITY.md) has the rest of what is open.

  If you need mutually untrusted tenants isolated by hardware, this is not it:
  one card per guest with an IOMMU, or vGPU.
- **Four guests, so far.** Four have shared one card evenly
  ([`BENCHMARKS.md`](BENCHMARKS.md)); more has not been tried.
- **No unified memory**, and no MIG or SR-IOV.
- **Nothing the guest already has can be given to the GPU** (§5), and RM's own
  dma-buf export is refused, because it would put the new dma-buf in the
  backend's descriptor table.
- **No cancelling a display call.** A host modeset cannot be interrupted, so a
  guest killed in the middle of one leaves it to finish on its executor; what
  bounds it is the host driver's own timeout.
- **Display modes that have not been shown.** The compositor VM and export
  mode are built and tested without a GPU; they have not driven a monitor.
- **A window that must be sized in advance.** It is fixed when the VM is
  created, and a workload that needs more mapped memory than was provisioned
  will fail to map it.
- **One address space for every UVM pool of a VM.** Each pool the guest maps
  sits at its own address in the VMM (§5), so two guest processes whose pools
  overlap cannot both be mapped: the second CUDA context fails with ENOMEM
  (the errno of any placement that cannot be made).
  CUDA picks its addresses the same way in every process, so two CUDA
  processes at once may meet this; how often is still to be measured. A pool
  cannot be moved — its GPU address is the same number — so this needs a host
  driver change to lift. Pools are also bounded (16 placements and 64 MiB per
  UVM file and per guest process, 64 and 256 MiB per VM; pools made at all,
  mapped or not, 256 MiB per file and 1 GiB per VM), and the aperture is
  1 GiB.

---

## Future work: the isolate

A sandboxed helper process, one per guest process, launched from a memfd. It
would hold the real host `/dev/nvidia*` descriptors and perform the forwarded
operations, unprivileged, with empty capability sets and `NoNewPrivs`. Today
the backend holds the host descriptors itself, one backend process per VM.
What the isolate would add is the split: the descriptors held by a helper per
guest process rather than by the process that also maps all of the guest's
memory, so that a compromised backend does not hold them.

It is the part of the design that is **not** a trait the VMM implements. It
is a runtime artifact, so anyone integrating `virtio-nvgpu` would inherit a
**process model**, not just a library: a VMM that cannot spawn helper
processes could not use the device as designed. That is a requirement, stated
here rather than left to be discovered during integration.

Part of the posture already applies to the backend: it refuses to start as
root or with `CAP_SYS_ADMIN`, drops every capability and sets `no_new_privs`
before its first thread (`device/src/posture.rs`), sandboxes itself before
the first guest message (`device/src/sandbox.rs`), and runs as a user of its
own per VM ([`DEPLOY.md`](DEPLOY.md)).

The display work adds descriptors an isolate would have to hold too: DRM card
and lease files, sync files and syncobjs, dma-bufs, and connections to the
host's Wayland compositor.

---

## 18. Prior art

**gVisor `nvproxy`** is the direct ancestor: it established that forwarding the
NVIDIA kernel ABI is viable, and its versioned ABI tables are the model for §8.
The difference is the guest kernel — nvproxy has none to bridge.

**DRM native context** (Intel, AMD) is the design being matched: the guest runs
a real driver and only submissions cross. This project reaches the same place by
a different road, because the NVIDIA driver cannot be opened up the way Mesa can.

**Venus** is the alternative being rejected, for the reasons in §2.

**waypipe** and the **cross-domain channel** of crosvm's virtio-gpu, with
Sommelier, carry Wayland connections across a machine or VM boundary. The
proxy of §14 takes their topology — one host connection per guest client,
object ids passed through unchanged — rather than Sommelier's nested
compositor, and none of their code: waypipe is GPL-3.

**`chromeos/virtio-media`** is the model for the repository layout — one tree
holding a GPL guest driver beside a permissively licensed, VMM-agnostic device
crate.
