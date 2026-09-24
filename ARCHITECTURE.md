# virtio-nvgpu Architecture

How a Linux guest gets an NVIDIA GPU without the host giving the card away, and
without anything in the middle pretending to be a GPU driver.

This document explains the design. It contains no code: the code is in
[`driver/`](driver/), [`device/`](device/) and [`protocol/`](protocol/), and it
moves faster than prose can follow.

> **Built, as of 2026-09-24:** the forwarding path, the shared memory window,
> the DRM render node, buffer sharing between a client and a compositor inside
> the guest, the event queue that lets a guest wait, and encoding on the GPU. A
> guest renders, presents and encodes H.264, and costs within 2% of bare metal
> ([`BENCHMARKS.md`](BENCHMARKS.md)).
>
> **Designed but not built:** the isolate, CUDA beyond enumeration, several
> guests on one card, MIG and SR-IOV. Each is called out where it appears.

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
  │  copies the request, adds nothing
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
host to guest, and carries exactly one kind of message. The *shared window* is
a region of host memory published into guest physical address space, where
every GPU mapping is placed.

**The guest driver** (`driver/`, GPL, because it touches kernel symbols)
registers character devices that look exactly like the real ones —
`/dev/nvidiactl`, `/dev/nvidia0…N`, `/dev/nvidia-uvm`, `/dev/nvidia-modeset` —
plus a DRM render node. It implements open, release, ioctl, mmap and poll, and
it is deliberately **not ABI-aware**: it copies the parameter bytes a program
passed and forwards them. Every decision about what those bytes *mean* is made
on the other side.

**The backend** (`device/`, Apache-2.0, with no VMM in its dependency list)
holds the real host descriptors, understands the ABI, translates what has to be
translated, and issues the real ioctls. A VMM adopts it by implementing a few
traits — descriptor chains, guest memory, a way to place host memory in the
window — and gets the device without patching the crate.

**The isolate** is the part that is not built. The intent is one sandboxed,
unprivileged helper process per guest, holding the device descriptors so that a
compromised VMM does not hold them. Today the backend holds them itself.
[`isolate/`](isolate/) is the design.

---

## 4. How a call travels

A program in the guest calls `ioctl` on what it believes is the NVIDIA driver.
The guest driver copies the parameter block out of userspace, tags it with the
handle that identifies which open device it came from, and puts it on the
control queue. The backend picks it up, decides what the bytes mean, fixes the
parts that cannot survive the crossing, calls the real ioctl, and sends the
result back. The guest driver copies the reply into the caller's buffer and
returns.

Three kinds of thing cannot survive the crossing unchanged, and finding each of
them was its own bug:

**Pointers.** Many NVIDIA parameter structs carry a pointer to a second block of
memory. A guest address means nothing in the backend's process, so the block
travels alongside the request, and the backend rewrites the pointer to its own
copy before the call and copies the result back afterwards.

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
write-back for system memory — or the mapping is correct and unusably slow.
Placement has to be done by whoever owns the guest's address space, which is
the VMM and not the backend, so it travels as a request rather than a call.
And anything that fails to give space back fails *later*, in whatever mapping
happens to be next, which is why the accounting is explicit rather than
implicit.

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

`/dev/nvidia-drm` and `/dev/nvidia-modeset` are part of this, and they are not a
display. They are how NVIDIA's stack names shareable memory. There is no scanout
from the guest and there is not meant to be: on a streaming box the frame leaves
as video.

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

---

## 8. Versioning against a moving ABI

NVIDIA's kernel ABI is not stable: structure layouts change between driver
releases, and there is no compatibility promise to hold them still. Support is
therefore explicit per version range — the backend knows which layouts belong
to which driver, learns the host's version during initialisation, and uses the
right table. A request whose size does not match what that version expects is
refused rather than guessed at.

Adding a new driver version means diffing the structures against the open
kernel modules, updating the tables, and naming the range as supported. It is
the same maintenance burden `nvproxy` carries, and their work can be followed
directly.

The tables are generated and checked in, so a build needs no NVIDIA source, and
the generator is in the repository so the tables can be regenerated rather than
trusted.

---

## 9. Getting a frame out

What runs today is **Vulkan Video**. A capture layer inside the guest takes the
client's swapchain image and encodes it on the client's own device — no second
device, no CPU copy, no CUDA. The encoded stream leaves over a socket. This is
the path the measurements come from: a 60 Hz H.264 stream that decodes without
an error.

The CUDA route — importing a rendered image into CUDA, encoding it with NVENC
from a GPU pointer — is the one the project was originally designed around, and
the ioctls it needs are forwarded. It is untested beyond enumeration, and it is
a second path rather than a fallback.

What is deliberately out of scope is unified memory: `cudaMallocManaged` and
page-fault-driven migration need fault handling across the VM boundary and
precise virtual address matching. Device allocations and graphics interop do not,
and they are what an encode pipeline uses.

---

## 10. What this design cannot do

- **NVIDIA only.** It proxies one vendor's kernel ABI. Nothing here generalises.
- **Version-locked**, as §8 describes.
- **A real security surface.** Forwarded ioctls reach the host's NVIDIA module,
  so bugs in those paths are reachable from a guest. This is the same trade-off
  `nvproxy` and VFIO passthrough make, and it is the reason the isolate is in
  the design at all.
- **One guest, so far.** Nothing here prevents several guests sharing a card,
  and nothing here demonstrates it either.
- **No unified memory**, and no MIG or SR-IOV.
- **A window that must be sized in advance.** It is fixed when the VM is
  created, and a workload that needs more mapped memory than was provisioned
  will fail to map it.

---

## 11. Prior art

**gVisor `nvproxy`** is the direct ancestor: it established that forwarding the
NVIDIA kernel ABI is viable, and its versioned ABI tables are the model for §8.
The difference is the guest kernel — nvproxy has none to bridge.

**DRM native context** (Intel, AMD) is the design being matched: the guest runs
a real driver and only submissions cross. This project reaches the same place by
a different road, because the NVIDIA driver cannot be opened up the way Mesa can.

**Venus** is the alternative being rejected, for the reasons in §2.

**`chromeos/virtio-media`** is the model for the repository layout — one tree
holding a GPL guest driver beside a permissively licensed, VMM-agnostic device
crate.
