# Architecture

How a Linux VM gets an NVIDIA GPU while the host keeps the card.

There's no code here. The code is in [`driver/`](driver/), [`device/`](device/)
and [`protocol/`](protocol/).

## The idea

The guest runs NVIDIA's real user-mode driver: the real `libvulkan_nvidia.so`,
the real NVENC, the real CUDA. That driver talks to the kernel through
`/dev/nvidia*`. We only change what sits behind those device nodes.

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
                                            │  checks, then translates
                                            │  handles, pointers and fds
                                            ▼
                                          host NVIDIA driver → GPU
```

Two things cross the boundary:

- **ioctls**, mostly while a device is being set up
- **memory mappings**, set up once and then used directly

The work itself doesn't cross. The guest submits a frame by writing to memory
it has already mapped, and that memory is the host's. Over 813,691 measured
frames, the backend handled one message per 59 frames.

## Why not Venus?

Venus forwards every Vulkan call. That means thousands of crossings per frame.
The host owns the buffers, so a compositor in the guest can't track them. And
the guest never holds a real GPU pointer, so it can't encode locally.

Intel and AMD solve this with _DRM native context_: the guest runs the real
driver and only submissions cross. NVIDIA has nothing like it, and outsiders
can't build one because the user-mode driver is closed source.

NVIDIA does expose a **kernel ABI**, though. gVisor's `nvproxy` already
forwards it, with Vulkan, CUDA and NVENC working. We do the same thing, but
across a real VM boundary: a guest kernel module and a virtio transport.

## The pieces

| piece        | where                  | what it does                                                                                                                                                                        |
| ------------ | ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| guest driver | `driver/` (GPL)        | Creates `/dev/nvidiactl`, `/dev/nvidia0…N`, `/dev/nvidia-uvm`, `/dev/nvidia-modeset` and a DRM render node. Copies requests across. Doesn't understand the ABI on purpose.          |
| backend      | `device/` (Apache-2.0) | Holds the real host descriptors. Understands the ABI, decides what's allowed, translates, and makes the real calls. Depends on no VMM: a VMM plugs in by implementing a few traits. |
| protocol     | `protocol/`            | The wire format, shared by both halves.                                                                                                                                             |

The device has a **control queue** (requests and replies), an **event queue**
(host to guest, for wakeups), and two shared memory regions: a **window** for
GPU mappings and a **UVM aperture** for CUDA.

Each VM gets its own backend process which refuses to run as root, and
locks itself down with seccomp and Landlock before it opens the GPU.

## How a call travels

The guest driver copies the ioctl's parameters and sends them over the control
queue. The backend checks them, makes the real call, and sends the result
back.

Three things can't cross unchanged:

- **Pointers.** A guest address means nothing on the host. The memory it points
  to travels with the request, and the backend points the call at its own copy.
- **Handles.** NVIDIA handles belong to one open file. When a compositor uses a
  client's buffer, the handle is translated to the file that owns it.
- **File descriptors.** An fd number from the guest would point at some random
  file in the backend. The guest driver swaps it for a handle, and the backend
  swaps in its own fd.

## How memory travels

Frames are never copied. When the guest maps GPU memory, the backend maps the
same memory on the host and places it in the **window**, which the VMM exposes
in guest physical memory. Then the guest maps those pages into the process and
both sides see the same bytes, at full speed.

The window has a fixed size, set when the VM is created. Mappings are tracked
and handed back when they go away.

### CUDA and the UVM aperture

CUDA needs one mapping the window can't provide. A CUDA context maps a
semaphore pool at an address the program chose, and UVM only accepts that
mapping at exactly that host address.

So each pool gets its own memory slot in a second region, the UVM aperture
(1 GiB), whereby the VMM maps the pool at the address it asked for, without replacing
anything of its own. Pools are capped at 64 MiB each, and at 64 pools or
256 MiB per VM. The backend checks those limits, and the VMM checks them again.

CUDA is opt-in: without `--caps ...,compute` the backend refuses
`/dev/nvidia-uvm`.

## How buffers get shared

Rendering doesn't need this, but... showing a frame does.

A client hands its buffer to a compositor as a dma-buf, through the DRM render
node. The guest's own DRM code handles the export and import. Behind each
buffer is a **proxy object** pointing at the real host object, so the memory
underneath is still the host's, reachable through the window.

`/dev/nvidia-modeset` isn't a display, it's how NVIDIA's stack names
shareable memory, so nothing is shown on a screen.

## How a guest waits

At the end of a frame, the driver waits for the GPU by polling a descriptor.
The interrupt fires on the host, so the backend watches its descriptors and
sends an **event** to the guest when one becomes ready. The guest driver then
wakes whoever is waiting.

If `poll` were missing, the kernel would report the device as always ready and
the driver would spin a whole CPU core and still look slightly _faster_ than
bare metal. Waiting properly costs about 0.02 ms per wake which is nothing
against a 16 ms frame, but it shows on a smaller 0.05 ms one.

## Driver versions

NVIDIA's kernel ABI changes between releases. The backend reads the host's
driver version at startup and picks the matching table, so a request of the wrong
size is refused, never guessed at.

The tables are generated from NVIDIA's open kernel modules and checked in, so a
build doesn't need NVIDIA source. The generator is in [`gen/`](gen/), which
lists the supported versions. Adding a release takes one generator run.

## Getting a frame out

Today that's done with **Vulkan Video**. A capture layer in the guest encodes
the app's swapchain image on the app's own device as H.264, then sends it out
over a socket. There's no CPU copy and no second device. On the driver we tested, Vulkan
Video encodes don't count against NVENC's 12-session cap on GeForce cards.

CUDA → NVENC (encoding from a GPU pointer) is forwarded, but hasn't been tried
end to end yet.

## What it can't do

- **NVIDIA only.** It forwards one vendor's kernel ABI.
- **It isn't hardware isolation.** There's no IOMMU between guest GPU work and
  the host. The GPU's own MMU separates them, so the host NVIDIA driver is part
  of the trusted base. The backend narrows what a guest can reach (allowlists
  generated from NVIDIA's own privilege tables), but passthrough or vGPU is
  stronger. See [`SECURITY.md`](SECURITY.md).
- **The window size is fixed** when the VM is created.
- **No unified memory** (`cudaMallocManaged`), MIG or SR-IOV (out of scope for now).
- **VRAM limits are approximate.** `--vram-limit-mib` misses memory RM
  allocates internally, which is tens of MiB per guest (might be fixed in a future version).

## Prior art

- **gVisor `nvproxy`** came first. It showed that forwarding NVIDIA's kernel
  ABI works, and its versioned tables are our model.
- **[`nvkvm-pv`](https://github.com/reindertpelsma/nvkvm-pv)** reached the same
  design independently. We checked our mapping, event and quirk handling
  against it. Its sibling **[`kayfabe`](https://github.com/reindertpelsma/kayfabe)**
  emulates the device itself instead.
- **DRM native context** (Intel, AMD) is what we're matching, but for NVIDIA.
- **`chromeos/virtio-media`** is where the repo layout comes from.
