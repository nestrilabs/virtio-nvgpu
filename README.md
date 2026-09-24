# virtio-nvgpu

**Near-native NVIDIA GPU access inside a KVM guest. A guest renders within 2% of
the machine it is running on, and costs the same CPU.**

`virtio-nvgpu` forwards NVIDIA kernel driver ioctls between a Linux guest and
the host at the **driver ABI level**, bypassing API-level translation entirely.
The guest runs NVIDIA's own user-mode drivers, unmodified — the same libraries,
the same Vulkan and NVENC, talking to the same card.

The target is **headless streaming**: a compositor inside the VM renders,
composites and encodes frames on the GPU, then sends compressed video out. The
VM has no monitor, and the host keeps the card.

## Where it stands

**It works, and it has been measured.** A Wayland client presents inside a
guest, the capture layer encodes on the game's own device, and the H.264 comes
out the other side — 618 frames that `ffmpeg` decodes without an error.

Measured on an RTX 3060 (driver 595.99.02), guest against **the same host, bare
metal**, with an identical headless Vulkan load:

| what the host takes for one frame | guest frame time | |
|---|---|---|
| 39 ms | **−0.3%** | faster than bare metal, within noise |
| 9.9 ms | **−0.8%** | |
| 2.0 ms | **+1.9%** | |
| 0.5 ms | +120% | a wake costs ~0.35 ms, and the frame is 0.5 |
| 0.05 ms | +727% | |

**Above about 2 ms a frame — which is every frame a game draws — a guest is
within 2% of bare metal.** Below that, the cost of waiting for the GPU starts to
dominate a frame that barely exists.

CPU is the other half of it, because a shared GPU is only worth sharing if the
guests are cheap. Unpaced at ~100 fps for 12 s, one guest:

| | CPU used |
|---|---|
| host, bare metal | 0.40 s |
| **guest** | **0.39 s** |

**A guest costs what the host costs.** Nothing is spent on forwarding in a
render loop, because nothing is forwarded: NVIDIA's user-mode driver submits
through memory it has mapped, and that memory is the host's. Over 813,691
frames the backend served 13,792 messages — one crossing per 59 frames, nearly
all of it device setup.

Full method, raw runs and the things these numbers do **not** support:
[`BENCHMARKS.md`](BENCHMARKS.md).

### What is known to work

- a guest enumerates the card — `nvidia-smi` reports real power and memory, and
  the `deviceUUID` is the host's
- Vulkan renders: `vulkaninfo` exits 0, offscreen draws are pixel-correct
- a Wayland client presents through a compositor in the guest
- NVENC through Vulkan Video, encoding on the client's own device
- imported buffers are the host's memory, mapped through a shared window

### What is not done

- **one guest at a time.** Two guests have never shared a card in any
  measurement here.
- **two cards, two driver versions.** RTX 3060 / 595.99.02 is where the numbers
  come from; an RTX A2000 / 615.71.09 has rendered but is not benchmarked.
- **jitter.** Frames arriving more than 25 ms apart in a 60 Hz encode run: 53
  out of ~600. Nothing is dropped and the mean is exactly 60 Hz, but the tail
  is real and unexplained.
- CUDA is forwarded but untested beyond enumeration; the jailer, per-version
  driver shares and the multi-tenant envelope are unbuilt.

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
| [`isolate/`](isolate/) | **Apache-2.0** | **A design note, not code yet.** The sandboxed per-guest helper that will hold the real device FDs. Today the backend holds them itself, in the VMM's own process. |
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

One thing that will **not** be a trait: the isolate. The intended design runs
one sandboxed helper process per guest process, so adopting it eventually means
inheriting a **process model**, not just a library dependency. That helper is
not written — the backend holds the device descriptors itself today — and
[`isolate/`](isolate/) is where the design lives until it is.

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
embedded pointers and file descriptors — and issues them against the host's
devices. Buffer and window bookkeeping lives here.

**Events.** A second virtqueue runs the other way. The host watches each
descriptor it has opened and says when one becomes readable, which is how a
guest waiting for the GPU is woken. Without it the guest cannot wait at all —
it polls a descriptor the kernel reports as permanently ready, and spins.

**Isolate — not built yet.** The plan is a sandboxed helper per guest process,
holding the real device FDs and issuing the `ioctl(2)` calls unprivileged.
Today the backend does that itself, inside the VMM's process.
[`isolate/`](isolate/) holds the design and no code.

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
│    └─ ioctl(host /dev/nvidia*) · mmap → shared window   │
│       (an unprivileged per-guest isolate is planned,     │
│        and is not what runs today)                       │
│                                                         │
│  Host NVIDIA driver → GPU                               │
└─────────────────────────────────────────────────────────┘
```

---

## Scope

**Targeted**

- Vulkan rendering, including presentation to a compositor inside the guest —
  which needs `/dev/nvidia-drm` and `/dev/nvidia-modeset`, both of which are
  implemented and neither of which is a display: they are how a buffer becomes
  shareable
- OpenGL rendering (headless EGL)
- CUDA device memory allocation
- CUDA ↔ Vulkan/GL interop, zero-copy, GPU-side pointers
- NVENC encoding from CUDA device pointers; NVDEC decoding

**Out of scope**

- `cudaMallocManaged()` / full unified virtual memory
- **scanout.** No physical display output: there is no monitor on a streaming
  box, and the frame leaves as video rather than as pixels on a wire
- MIG, SR-IOV
- Arbitrary NVIDIA driver versions — each supported range is explicit, as with
  `nvproxy`

---

## Performance

Measured, on one card, by one synthetic load — see [`BENCHMARKS.md`](BENCHMARKS.md)
for the method and the raw runs, and for what this does not support (it does not
support a comparison with any other hypervisor, because none was run).

| | virtio-nvgpu, measured | Venus, by design |
| --- | --- | --- |
| GPU-bound (≥2 ms a frame) | **98–100% of bare metal** | 90–97% |
| Very light frames (≤0.5 ms) | 45–14% of bare metal | — |
| CPU cost of a rendering guest | **same as bare metal** | high (serialize and replay) |
| Host crossings per frame | **~0.02** | thousands |
| Guest-side NVENC | works, zero-copy | not viable |

The difference is structural: Venus crosses the VM boundary **per API call**,
thousands of times a frame. `virtio-nvgpu` crosses it **per ioctl** — and a
render loop issues none, because submission is a write to mapped memory. What
is left at very light frames is not forwarding but *waiting*: the guest sleeps
for the GPU, and the wake costs ~0.35 ms however small the frame was.

The Venus column is that project's design envelope, not something measured
here.

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

- [`BENCHMARKS.md`](BENCHMARKS.md) — what it costs against bare metal, how that
  was measured, and what the numbers do not support.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — guest driver, device, virtio protocol,
  memory model, ABI handling. Part design document, part description; it opens
  by saying which part is which.
