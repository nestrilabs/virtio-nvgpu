# virtio-nvgpu

**A virtio device for near-native NVIDIA GPU access in KVM virtual machines.**

`virtio-nvgpu` forwards NVIDIA kernel driver ioctls between a Linux guest and
the host at the **driver ABI level**, bypassing API-level translation entirely.
The guest runs NVIDIA's own user-mode drivers, unmodified.

The primary target is **headless streaming**: a compositor inside the VM renders,
composites, and encodes frames on the GPU, then sends compressed video to a
remote display. The VM has no physical monitor, and the host keeps the card.

> **Status: design settled, implementation in progress.** The architecture below
> is stable and the repository is being restructured onto the layout in
> [Repository layout](#repository-layout). Existing work lives on feature
> branches and is moving into this tree. Interfaces will change.

---

## Repository layout

Four components, three license zones. The split is deliberate: the guest half
must be GPL to touch kernel symbols, the host half should be permissive so that
other people can build on it, and the definitions both halves share must be
includable from both.

| directory | license | what it is |
| --- | --- | --- |
| [`driver/`](driver/) | **GPL-2.0** | Guest kernel module. Registers `/dev/nvidia*`, forwards ioctl and mmap over the virtqueue. Deliberately not ABI-aware. |
| [`device/`](device/) | **Apache-2.0** | The virtio device, as a Rust crate with **no VMM in its dependency list**. Every VMM concern is a trait. |
| [`isolate/`](isolate/) | **Apache-2.0** | Per-guest-process sandboxed host helper, launched from a memfd, holding the real device FDs. Unprivileged. |
| [`gen/`](gen/) | — | Generated ABI tables. Checked in *and* reproducible. |
| [`protocol/`](protocol/) | **BSD-3-Clause OR GPL-2.0+** | Wire format and ABI definitions shared by both halves. Dual licensed so the GPL driver and the Apache crate can include the same headers. |

The layout follows [`chromeos/virtio-media`](https://chromium.googlesource.com/chromiumos/platform/virtio-media/),
which solves the same problem — one repository holding a GPL guest driver beside
a permissively licensed, VMM-agnostic device crate.

### Using it from a VMM

`device/` depends on no virtual machine monitor. A VMM adopts the device by
implementing a small set of traits — descriptor chains as `Read`/`Write`, an
event queue, guest memory mapping, host memory mapping — and gets the whole
device without patching the crate. Optional capabilities degrade rather than
fail to build, so a VMM can adopt it before supporting every feature.

Buffer and window bookkeeping lives in `device/`. The VMM supplies raw map and
unmap and nothing more.

One thing that is **not** a trait: the isolate. `virtio-nvgpu` runs one sandboxed
helper process per guest process, so integrating it means inheriting a **process
model**, not just a library dependency. See [`isolate/`](isolate/).

---

## Why

### The streaming pipeline we want

```text
Guest VM (headless, no physical display)
──────────────────────────────────────────

  Game / application
    │ Vulkan or OpenGL
    ▼
  Wayland compositor (guest-side)
    │ composites all windows
    │ CUDA zero-copy import of composed frame
    ▼
  NVENC hardware encoder (guest-side)
    │ H.264 / H.265 bitstream (~100 KB per frame)
    ▼
  Stream to remote client
```

The entire render → composite → encode pipeline runs **on the GPU, inside the
guest**. Only the compressed bitstream leaves. This requires the guest to have
**real, driver-level access** to GPU resources: buffer handles, fences, CUDA
device pointers, NVENC sessions.

### Why existing approaches fall short

**virtio-gpu + Venus (API-level translation).** Venus serializes every Vulkan or
OpenGL call in the guest, transports it over virtio, and replays it host-side.
Three problems for this use case:

1. **Latency compounds on draw-call-heavy workloads.** Games issue 1,000–5,000
   draw calls per frame plus binds, descriptor updates and render pass
   transitions, each serialized and replayed individually. At 60 fps the frame
   budget is 16.6 ms; 1–3 ms of serialization is 6–18% gone before any GPU work.
2. **CPU overhead is significant.** Serialization, transport and replay burn host
   CPU the application needs. Where compute is billed and finite, that waste is
   the product.
3. **Guest-side encoding is not viable.** GPU buffers are owned by the *host*.
   The guest compositor cannot see or import them, so there is no practical path
   to a `CUdeviceptr` in the guest pointing at a Venus-managed buffer — which
   means no NVENC without a full CPU readback and copy.

**DRM native context (Intel / AMD).** The guest runs the real Mesa driver, builds
command buffers locally, and only submissions cross the boundary. Guest-side
buffer ownership and encoding work correctly. **This does not exist for NVIDIA.**

**VFIO passthrough.** Native performance and a complete driver stack in the
guest, but it dedicates the whole GPU to one VM. In multi-tenant environments
that is often not an option.

### What virtio-nvgpu does differently

Translation happens at the **kernel driver** level (ioctls to `/dev/nvidia*`),
not the **graphics API** level. The guest runs NVIDIA's real user-mode libraries,
which build GPU command buffers **locally in the guest** — individual draw calls
are never serialized:

```text
                    Venus                  virtio-nvgpu
                    ──────────────         ──────────────────────

Per draw call:      serialize +            local function call
                    transport +            (no VM exit)
                    deserialize +
                    replay

Per frame           ~2,000 messages        ~5–20 messages
  boundary          (one per API call)     (queue submits + allocs)
  crossings

GPU command         generated on HOST      generated in GUEST
  buffers           after replay           by NVIDIA's own compiler

CPU overhead        serialization +        near zero for rendering
                    deserialization        (only ioctl forwarding)

Guest buffer        HOST owns buffers      GUEST owns buffers
  ownership         compositor can't       compositor has full
                    track them             visibility and control

Guest NVENC         not viable             works (real CUDA interop)
```

---

## How it works

**Guest kernel driver.** Registers `/dev/nvidiactl`, `/dev/nvidia0…N` and
`/dev/nvidia-uvm`. On `ioctl()` it serializes the request onto a control
virtqueue. On `mmap()` it maps the appropriate shared-memory region into the
calling process with the correct caching attributes. It copies raw bytes and
makes no ABI decisions.

**Device crate.** Receives requests, maps guest handles to host device file
descriptors, performs ABI-aware translation of ioctl parameters — rewriting
embedded pointers and file descriptors — and drives the isolate. Buffer and
window bookkeeping lives here.

**Isolate.** Holds the real host device FDs and issues the `ioctl(2)` calls,
unprivileged and sandboxed, one per guest process. For GPU memory it maps the
host device FD into the shared region so the guest can reach it directly.

```text
┌─ Guest ─────────────────────────────────────────────────┐
│  Application → NVIDIA Vulkan / GL / CUDA                │
│                     │ ioctl(/dev/nvidia*)               │
│  driver/ (GPL)      ▼                                   │
│    serialize → virtqueue                                │
│    mmap → shared region                                 │
└───────────────────────────┬─────────────────────────────┘
                            │ VM exit
┌───────────────────────────▼─────────────────────────────┐
│  VMM  (implements the device traits)                    │
│                                                         │
│  device/ (Apache-2.0)                                   │
│    ├─ guest handles → host FDs                          │
│    ├─ translate embedded FDs and pointers               │
│    └─ buffer + window bookkeeping                       │
│                     │                                   │
│  isolate/ ──────────▼── unprivileged, per guest process │
│    └─ ioctl(host /dev/nvidia*) · mmap → shared region   │
│                                                         │
│  Host NVIDIA driver → GPU                               │
└─────────────────────────────────────────────────────────┘
```

---

## Scope

**Targeted**

- Vulkan rendering (everything that works without `/dev/nvidia-drm`)
- OpenGL rendering (headless EGL)
- CUDA device memory allocation
- CUDA ↔ Vulkan/GL interop, zero-copy, GPU-side pointers
- NVENC encoding from CUDA device pointers; NVDEC decoding

**Out of scope**

- `cudaMallocManaged()` / full unified virtual memory
- `/dev/nvidia-drm` and `/dev/nvidia-modeset` — physical display output is not
  needed for headless streaming
- MIG, SR-IOV
- Arbitrary NVIDIA driver versions — each supported range is explicit, as with
  `nvproxy`

---

## Performance targets

Design targets, not measurements. Nothing here has been benchmarked.

| | Venus | virtio-nvgpu (target) | Bare metal |
| --- | --- | --- | --- |
| GPU-bound (heavy shaders) | 90–97% | 97–100% | 100% |
| CPU-bound (many draw calls) | 65–85% | 95–99% | 100% |
| Shader compilation stutter | +10–50 ms per shader | <1 ms overhead | 0 |
| Frame latency overhead | +1–3 ms | +0.05–0.1 ms | 0 |
| CPU overhead for rendering | high (serialize/replay) | near zero | zero |
| Guest-side NVENC | not viable | works (zero-copy) | works |

The difference is structural: Venus crosses the VM boundary **per API call**,
thousands of times a frame. `virtio-nvgpu` crosses it **per ioctl**, tens of
times a frame.

---

## Driver ABI versioning

NVIDIA's kernel driver ABI is not stable; ioctl struct layouts change between
releases. Support is explicit per version range, handled the way gVisor's
`nvproxy` handles it.

The cost is bounded, for three reasons. Profiles key off **ranges, not points**,
so a release between two known versions selects the lower profile. The struct
half is **derived mechanically** from NVIDIA's published `open-gpu-kernel-modules`
at each tag — compile a probe per field, read back `sizeof` and `offsetof` —
rather than transcribed by hand. And the judgement half, which commands exist and
which are safe, tracks `nvproxy` upstream.

See [`gen/`](gen/).

---

## Prior art

**gVisor `nvproxy`** — the direct inspiration. It forwards NVIDIA ioctls from
sandboxed containers to the host driver, handling ABI versioning, pointer and FD
translation, and GPU mmap management, and it supports Vulkan, OpenGL, CUDA and
NVENC in production today. Its ABI definitions (`pkg/abi/nvgpu`) and handler
logic (`pkg/sentry/devices/nvproxy`) are the primary reference. `nvproxy` also
demonstrates that Vulkan and NVENC work **without** `/dev/nvidia-drm` or
`/dev/nvidia-modeset`.

**`chromeos/virtio-media`** — the layout template. A GPL guest driver beside a
VMM-agnostic Rust device crate, with every VMM concern behind a trait.

**WSL2 `/dev/dxg`** — a production driver-level GPU proxy across a real
virtualization boundary, proving the general approach at scale. Different
problem: it targets a Windows host and a Microsoft-defined kernel abstraction.

**DRM native context (Intel / AMD)** — the same goal, achieved for other vendors:
the guest runs the real driver and builds command buffers locally, with only
submissions crossing the boundary. `virtio-nvgpu` aims at equivalent capability
for NVIDIA, where no native context exists.

---

## License

Three zones, listed in [Repository layout](#repository-layout).
Full texts: [`LICENSE-APACHE-2.0`](LICENSE-APACHE-2.0),
[`LICENSE-GPL-2.0`](LICENSE-GPL-2.0),
[`LICENSE-BSD-3-Clause`](LICENSE-BSD-3-Clause).

Code ported from other projects keeps its original terms.

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — guest driver, device, virtio protocol,
  memory model, ABI handling, CUDA and NVENC integration.
