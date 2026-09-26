# virtio-nvgpu

**Near-native NVIDIA GPU access inside a KVM guest. A guest renders within 2% of
the machine it is running on, and costs the same CPU.**

`virtio-nvgpu` forwards NVIDIA kernel driver ioctls between a Linux guest and
the host at the **driver ABI level**, bypassing API-level translation entirely.
The guest runs NVIDIA's own user-mode drivers, unmodified — the same libraries,
the same Vulkan and NVENC, talking to the same card.

The first target is **headless streaming**: a compositor inside the VM
renders, composites and encodes frames on the GPU, then sends compressed video
out. The VM has no monitor, and the host keeps the card.

The second is **display**: a guest's frames on the host's own monitors —
through the host's compositor, through an output it lends the guest, or with
the guest driving the card. The first two have run on an RTX 5090 under both
supported VMMs; the third has not yet run on hardware ([Display](#display)).

## Where it stands

**It works, and it has been measured.** A Wayland client presents inside a
guest, the capture layer encodes on the game's own device, and the H.264 comes
out the other side — 618 frames that `ffmpeg` decodes without an error.

Measured on an RTX 3060 (driver 595.99.02), guest against **the same host, bare
metal**, with an identical headless Vulkan load:

| what the host takes for one frame | guest frame time | |
|---|---|---|
| 39 ms | **−0.4%** | faster than bare metal, within noise |
| 9.9 ms | **−0.7%** | |
| 2.0 ms | **+1.7%** | |
| 0.5 ms | +7.1% | a wake costs ~0.02 ms, and the frame is half of one |
| 0.05 ms | +40.8% | |

**Above about 2 ms a frame — which is every frame a game draws — a guest is
within 2% of bare metal.** Below that, the cost of waiting for the GPU starts to
show against a frame that barely exists.

CPU is the other half of it, because a shared GPU is only worth sharing if the
guests are cheap. Unpaced at ~100 fps for 12 s, one guest:

| | CPU used |
|---|---|
| host, bare metal | 0.40 s |
| **guest** | **0.37 s** |

**A guest costs what the host costs.** Nothing is spent on forwarding in a
render loop, because nothing is forwarded: NVIDIA's user-mode driver submits
through memory it has mapped, and that memory is the host's. Over 813,691
frames the backend served 13,792 messages — one crossing per 59 frames, nearly
all of it device setup.

Full method, raw runs and the things these numbers do **not** support:
[`BENCHMARKS.md`](BENCHMARKS.md).

**Every number in this section predates protocol v2.** It was measured on the
code before the display work and the security changes that came with it: the
new transport, a memory type per mapping, guest system memory made cacheable
and GPU-coherent, the pointer and descriptor scrubbing, the RM allowlist and
the backend's sandbox. None of it has been re-measured since; the current code
has been run for function (below), not timed. The design gives a render loop
nothing new to cross, which is a reason to expect the same numbers, not a
measurement of them.

### Several guests on one card

Four guests on one RTX 3060, the same load in each: **25.84, 26.49, 25.57, 25.79
fps** — 103.7 together, against 102.9 for a single guest — with p50 frame times
of 39.165, 39.164, 39.168 and 39.165 ms. The total does not move as guests are
added, and the split is even to four decimal places.

All four render correctly at the same time, and four of them **encode H.264 at
once**, each paced at exactly 60 Hz, with no NVENC session limit reached.

Four is what was run, not a limit found.

### Driver versions

Measured on **595.99.02**, and the current code run there on an RTX 5090; an
A2000 on **615.71.09** rendered on the code before protocol v2. The backend
starts only on a host release every one of its tables was measured at:
535.129.03, 580.178.04, 595.71.05, 595.99.02, 610.57.04 and 615.71.09.
Details below.

### What is known to work

Run on an **RTX 5090, driver 595.99.02**, on the current code (2026-09-26),
under both nesbox and crosvm, with the RM allowlist enforcing and the
backend's sandbox on ([`rig/TESTING-RIG.md`](rig/TESTING-RIG.md)):

- a guest enumerates the card — `nvidia-smi` reports real power and memory, and
  the `deviceUUID` is the host's
- Vulkan, OpenGL and EGL render; offscreen draws are pixel-correct
- CUDA with `--allow-compute`, under nesbox and under crosvm (whose nvgpu
  frontend runs jailed); without it the guest has no UVM device and CUDA
  finds no GPU, cleanly
- guest applications as clients of the live host Hyprland, with a fullscreen
  guest window scanned out directly; a monitor leased to the guest and driven
  with KMS; `VK_KHR_display` on that lease; the lease handed back
- the security negatives, on the control and render nodes and on a leased card
- about 35 real applications, as an unprivileged guest user with the browsers'
  sandboxes on, shown on the live desktop: games (SuperTuxKart, Neverball,
  Godot), Blender (EEVEE, and Cycles on CUDA/OptiX), GIMP, Inkscape, Krita,
  LibreOffice, Firefox, Chromium, Electron, mpv, ffmpeg with Vulkan Video,
  NVENC, NVDEC and VA-API, OpenCL
- the guest module with its parsers in Rust, through the same regression

Run earlier on an RTX 3060 (595.99.02), before protocol v2: every number in
[`BENCHMARKS.md`](BENCHMARKS.md), four guests on one card, and NVENC through
Vulkan Video encoding on the client's own device.

### Display

Four ways to put a guest's frames on a monitor the host drives, and explicit
sync to go with them:

1. **Wayland client of the host compositor.** Guest applications are clients
   of the host's compositor through a proxy. Their GPU buffers cross without a
   copy — a guest buffer is a host GPU object, and the compositor gets the host
   dma-buf of that same object — so a fullscreen guest window can be **scanned
   out directly** by the host, exactly as a native client's would.
2. **DRM lease → guest KMS.** The host compositor leases one of its outputs to
   the guest, which drives it with ordinary KMS once a Wayland lease client
   hands over the file: a compositor of its own, or `kmscube` and `modetest`
   behind such a client. None ships here.
3. **`VK_KHR_display` / `vkAcquireDrmDisplayEXT`** on that lease, through
   NVIDIA's own display path (nvidia-drm permission grants and NVKMS).
4. **A compositor VM.** The host runs no compositor; a compositor in the guest
   drives the host's card, and applications on the host or in other VMs reach
   it through the same proxy in *export* mode.

**Modes 1 to 3 have run on hardware**: an RTX 5090 on 595.99.02, under nesbox
and crosvm, against the live patched Hyprland (0.56.2), with explicit sync in
the Wayland mode and a lease handed back and taken again. **Mode 4, and export
mode, have not**: they need the desktop stopped (rig/TESTING-RIG.md, "Group C").
No mode has been timed yet.
[`TESTING.md`](TESTING.md) is the plan, stage by stage; how to turn each mode
on: [Display](#display).

### What a guest can reach

Worth stating plainly, because it is the first question a security person asks
and the honest answer is not "nothing". [`SECURITY.md`](SECURITY.md) is the
full account; this is the summary.

There is **no IOMMU boundary** between guest GPU work and the host. The card
belongs to the host's NVIDIA driver and sits in the host's IOMMU domain; the
guest gets the driver's ioctl interface, not the device. What separates a guest
from host memory is the GPU's own MMU, with page tables RM programs on the
guest's behalf — so **the host NVIDIA driver is in the TCB**. The guest also
authors its own command streams, which is exactly why there is no per-submission
cost. The guest kernel is untrusted: everything that protects the host is
decided by the backend, from its own tables, never from a layout the guest
sent.

**The backend must run unprivileged.** RM, DRM and NVKMS take a guest's
privilege from the credentials of the process that calls them, and that process
is the backend: run as root, every guest process would be an RM administrator
with all of BAR0 mappable — the host kernel, one DMA away. So the backend
refuses to start as root or with `CAP_SYS_ADMIN` unless told
`--allow-root-unsafe` (a diagnostic flag), and drops every capability before
its first thread in any case. In production each VM's backend runs as a user
of its own (`nvgpu-vm0`, `nvgpu-vm1`, ...), with the VMM as the slot's
`nvgpu-vmm0`, ..., from the shipped systemd unit or NixOS module, in a cgroup
of its own with a memory bound ([`DEPLOY.md`](DEPLOY.md)); the rig's launcher,
[`rig/run-guest.sh`](rig/run-guest.sh), does the same through
`setpriv` when run as root, and in the Wayland modes runs it as the owner of
the compositor's socket or of the export directory ([Display](#display)).
Its socket defaults to
`$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock`, in a directory only it can enter, because
whoever listens there is handed the guest's memory.

Before the first guest message the backend sandboxes itself
([`device/src/sandbox.rs`](device/src/sandbox.rs)): a network namespace of its
own, Landlock confining it to the GPU's nodes and the few files it reads, and a
seccomp syscall allowlist that kills on anything else. A layer the host kernel
lacks is logged as `sandbox: DEGRADED` and stops the start;
`--sandbox=best-effort` runs without it and `--sandbox=off` (or
`NVGPU_SANDBOX=off` for the launcher) without any, both diagnostic flags
(`--diagnostic`).
[SECURITY.md](SECURITY.md) §4 has what each layer and the per-VM uid do and do
not stop.

What narrows the surface today:

- ioctls the ABI profile does not describe are **refused**, not forwarded
  (`--permissive-abi` turns that off for diagnosis, and says so loudly)
- **no guest pointer reaches the host driver as a pointer.** The backend makes
  every host call itself, so a pointer the guest left in a parameter block
  would be an address in the VMM. Each pointer field the host follows -- RM's
  parameter blocks, the pointers RM follows inside control parameters, the
  NVKMS and nvidia-drm nested blocks -- is pointed at a buffer of the
  backend's or zeroed, whatever the ABI policy; escapes whose pointers cannot
  be relocated (IOCTL_XFER_CMD, IDLE_CHANNELS, ACCESS_REGISTRY, I2C_ACCESS,
  GET_EVENT_DATA) are refused (`device/src/guestptr.rs`)
- **compute is opt-in.** Everything only CUDA needs -- `/dev/nvidia-uvm`,
  UVM's sharing mode and aperture, memory registered by its pages -- is served
  only with `--allow-compute` (`rig/run-guest.sh --allow-compute`, or
  `NVGPU_COMPUTE=1`). Without it the guest has no UVM device and CUDA finds no
  device; Vulkan, OpenGL, EGL, Vulkan Video and display need none of it
  ([`SECURITY.md`](SECURITY.md), "Compute")
- **RM objects stay with the guest process that made them.** A duplicate
  between two clients follows RM's own rule with guest processes in place of
  the backend's one, and a second client named in parameters is held to RM's
  rule for that field (the same process, or the same euid where RM checks its
  security token). The guest kernel says which process and euid make each
  call; a guest that cannot is refused both (`device/src/rmshare.rs`)
- **memory the guest registers with the GPU travels as its pages, not its
  address.** RM pins what an address maps in the calling process -- the
  VMM -- so the guest driver pins the caller's range and sends the
  guest-physical pages instead; the backend checks each is guest RAM and hands
  RM its own mapping of exactly those pages, and the guest keeps them pinned
  until RM has let go (`device/src/osdesc.rs`; `--allow-compute` only)
- **guest descriptor numbers are translated, not forwarded.** A file named
  inside RM escape, UVM, NVKMS or DRM parameters becomes the backend's own
  descriptor for that file, or the call is refused; a few descriptors inside
  RM control parameters are not translated yet ([`SECURITY.md`](SECURITY.md))
- UVM (`--allow-compute` only) runs with pageable memory access forced off, so the GPU cannot fault in
  the VMM's own pages, and takes only the commands that name UVM's ranges, RM
  handles or GPU state, each held to a block size measured per release: the
  tools device, and every command that copies through, pins or populates CPU
  memory, is refused
- NVKMS and nvidia-drm go through **per-release tables** and a policy of the
  backend's own: a guest outside compositor-VM mode flips and sets modes only
  on heads the host granted it through a lease, and those grants end when the
  lease does
- KMS on a lease or card is schema-checked call by call, and a guest may scan
  out only framebuffers its own VM made — never the host compositor's
- the Wayland proxy enforces its allowlist **on the host**, whatever the guest
  daemon did, and bounds what each VM may hold (channels, shared memory,
  queued output, lease requests)
- the RM controls that list every GPU process on the host, with PIDs and
  memory use, are answered by the backend with the refusal RM gives an
  unprivileged caller; running each backend in its own PID namespace makes RM
  itself show the backend alone

- `RM_ALLOC` classes and RM control commands are **allow-listed per driver
  release**, default deny: what reaches RM is what NVIDIA's own userspace was
  seen to use and the sources show to be safe ([`SECURITY.md`](SECURITY.md)
  §12). The list enforces by default; `--rm-allowlist=log` is a diagnostic
  flag

What does not, yet:

- the backend holds the host descriptors itself, in the process that maps the
  guest's memory; the unprivileged per-guest isolate is designed and unbuilt
- the display paths have open items of their own, listed in
  [`SECURITY.md`](SECURITY.md)

**Guest system memory is GPU-coherent now.** RM allocates system memory with
the CPU cache type the client asks for, uncached by default. On an **Intel**
host, KVM maps guest RAM write-back whatever the guest's page attributes say,
unless the VMM disables `KVM_X86_QUIRK_IGNORE_GUEST_PAT` — so a guest would
cache memory the GPU reads and writes without snooping, and see stale data. The
backend therefore allocates guest system memory write-back, makes every GPU
mapping of it snoop, and puts the caller's own bits back in every reply.
`--keep-guest-coherency` turns that off, for ruling it out while chasing
something. Display memory is left as its client allocated it (NVKMS's answer
to which kind to allocate is narrowed to the coherent one), so on Intel the VMM
should still disable the quirk; the backend says so at startup. AMD's nested
paging honours the guest's page attributes and needs none of this.

**This is attack-surface reduction, not hardware isolation.** VFIO passthrough
with an IOMMU is strictly stronger — it constrains the device to the guest's own
memory — and for mutually untrusted tenants that or vGPU is still the answer.

### What is not done

- **the compositor-VM and export modes on hardware**, and hotplug: they need
  the host desktop stopped, and have run only in unit and loopback tests.
- **re-measuring on protocol v2.** Every number above predates it, and no
  display path has been timed.
- **more than four guests**, or several guests doing anything heavier than
  vkcube at 720p. Four share the card evenly; eight has not been tried.
- **one driver release on the current code.** 595.99.02 (RTX 5090 and, before
  protocol v2, RTX 3060) is the only release the current code has run on; the
  other five releases the backend accepts are served by tables read from
  NVIDIA's sources, not proven by a run.
- the per-guest isolate, per-version driver shares and the multi-tenant
  envelope are unbuilt.
- the Hyprland patches have no cooldown between leases of a desktop monitor
  (the backend rate-limits a VM's requests instead), and un-marking a monitor
  `leasable` does not end a lease already granted.
- explicit sync is not carried in export mode, which hides the syncobj
  protocol from host clients.

---

## Repository layout

Three license zones for our own code; patches and vendored protocol XML keep
their upstream terms. The split is deliberate: the guest half must be GPL to
touch kernel symbols, the host half should be permissive so that other people
can build on it, and the definitions both halves share must be includable from
both.

| directory | license | what it is |
| --- | --- | --- |
| [`driver/`](driver/) | **GPL-2.0-only** | Guest kernel module. Registers `/dev/nvidia*`, the DRM nodes and `/dev/nvgpu-wl`, forwards ioctl and mmap over the virtqueue. Deliberately not ABI-aware: what an ioctl carries comes from generated tables. |
| [`device/`](device/) | **Apache-2.0** | The virtio device, as a Rust crate with **no VMM in its dependency list**. Every VMM concern is a trait. Also the vhost-user backend binary, and the host half of the Wayland proxy. |
| [`wlwire/`](wlwire/) | **Apache-2.0** | The Wayland proxy's shared half: a codec generated from vendored protocol XML, the allowlist it is checked against at build time, the channel's frame format, and the translation engine both ends run. |
| [`nvgpu-wl-guest/`](nvgpu-wl-guest/) | **Apache-2.0** | The guest daemon of the Wayland proxy. Guest clients connect to it as to a compositor. |
| [`gen/`](gen/) | — | Generated tables: the ABI profiles, the IOCTL2 schema both halves interpret, NVKMS and nvidia-drm layouts, RM control pointers and UVM block sizes, each measured per driver release. Checked in *and* reproducible. |
| [`protocol/`](protocol/) | **BSD-3-Clause OR GPL-2.0-or-later** | Wire format and ABI definitions shared by both halves. Dual licensed so the GPL driver and the Apache crate can include the same headers. |
| [`patches/`](patches/) | the patched project's | Patches to Hyprland and aquamarine that let the host lease a desktop monitor to a guest, and to crosvm to run the device as a vhost-user frontend. |
| [`scripts/`](scripts/) | **Apache-2.0** | The project's own tooling: `ci.sh` (the checks, in tiers), `check-unsafe.sh`, `fuzz.sh`, `gen-check.sh` (every generated table against its sources), `build-guest-kernel.sh` (the guest kernel and module), the Wayland loopback test. |
| [`contrib/`](contrib/) | **Apache-2.0** | What a deployment installs: the systemd unit per VM and its socket helper, and the guest's udev rule for `/dev/nvgpu-wl` ([`DEPLOY.md`](DEPLOY.md)). |
| [`nix/`](nix/) | **Apache-2.0** | The NixOS module for the backend: per-VM users and the unit. The root `flake.nix` packages the backend and the daemon and runs the fast checks. |
| [`rig/`](rig/) | **Apache-2.0** | The dev box's test rig, not for production: the launcher (`run-guest.sh`), preflight, the test guest image (`guest-image/`), the on-device helpers (`verify/`), and [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md). Its data lives in `.rig/`, git-ignored. |
| [`docs/review/`](docs/review/) | — | Review records the security documents cite by finding id. |

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
[`ARCHITECTURE.md`](ARCHITECTURE.md), "Future work: the isolate", is where
the design lives until it is.

### What a VMM must do, over vhost-user

The backend binary (`vhost-user-nvgpu`) speaks the upstream vhost-user
protocol as rust-vmm's `vhost` 0.17 implements it. A VMM that is its frontend
must:

- **Present virtio device ID 45** over virtio-pci, with the **device
  configuration read from the backend** (`GET_CONFIG`; 4016 bytes) and two
  queues (`MQ`). Use **PCI class 0xff0000**: a display-class function makes
  guest userspace see a second GPU (Chromium did).
- **Pass `VIRTIO_RING_F_INDIRECT_DESC` through** to the guest. Without it
  requests are held to 256 KiB instead of 4 MiB.
- **Publish shared memory region 1, 1 GiB**, as a 64-bit prefetchable BAR
  above 4 GiB with a virtio shared-memory capability, and place what the
  backend asks for in it: `BACKEND_REQ` plus `SHMEM`, `SHMEM_MAP`/`UNMAP`
  (backend requests 9 and 10: region id, file offset, region offset,
  length, read/write flag, the file descriptor with the message), answered
  when `REPLY_ACK` is negotiated. A VMM that asks for the regions gets them
  from `GET_SHMEM_CONFIG` (request 44): region 1, and region 2 only with
  `--allow-compute`. **Check every request** against the region: the
  backend is another process and may be compromised.
- **Give the window write-back in the guest's MTRRs** (default type WB with
  the 32-bit PCI hole UC, on every vCPU), or the guest driver warns that
  the window "is not write-back in this guest's MTRRs".
- **Leave the host GPU's PCI bus free in the guest.** The guest driver gives
  the GPU the host's own PCI address (NVIDIA's userspace looks it up by
  that), on a PCI host bridge of its own, so a guest with a bridge on that
  bus number has no GPU device.
- For compute only, the **UVM aperture** (region 2): see
  [`ARCHITECTURE.md`](ARCHITECTURE.md) §5, "The UVM aperture". A VMM
  without it runs every graphics path, and the guest reports no compute.
  The guest finds each region by its id, so the aperture may have a BAR of
  its own or follow the window in the window's BAR, with a capability of
  its own. Each pool is mapped at the host address its file offset names,
  in [4 GiB, 32 TiB), without replacing anything of the VMM's, and given a
  memory slot only once its pages are present; the slot goes before the
  mapping.

Two VMMs do this today, both with the UVM aperture, and both have run every
graphics and compute path on the GPU. **nesbox**
([github.com/nestrilabs/nesbox](https://github.com/nestrilabs/nesbox), branch
`virtio-nvgpu-v3`, not yet merged upstream) has its own frontend for the
device. **crosvm** takes the patches in [`patches/crosvm/`](patches/crosvm/):
a vhost-user device type `nvgpu` (class 0xff0000, indirect descriptors, only
`SHMEM_MAP` of the backend's mapping requests); every backend mapping checked
against its region, overlaps and stray unmaps refused, mappings dropped on
reset; `--no-pci-hotplug-port`, as crosvm otherwise puts an empty hot-plug
root port on PCI bus 1; the UVM aperture after the window in the window's
BAR; and, with its sandbox on, the nvgpu frontend in a jailed process of its
own under a seccomp policy of its own, whose every mapping request the main
process checks again against the regions it laid out -- NVIDIA and DRM
descriptors only in the window, `/dev/nvidia-uvm` pools only in the aperture,
per-device limits -- before it maps anything (SECURITY.md §16). crosvm
reserves the pools' host address band at start-up, so nothing of its own is
ever there. crosvm needed nothing new in the protocol: its vhost-user fork
already implements the upstream `GET_SHMEM_CONFIG`, `SHMEM_MAP` and
`SHMEM_UNMAP` messages byte for byte as rust-vmm does; the backend answers
`GET_SHMEM_CONFIG`, which nesbox never asks. Every other device crosvm
emulates is a minijail'd process with its seccomp policy, none of which
changed. How to run it: [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md), "crosvm".

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

Per frame           ~2,000 messages        ~0.02 (measured: one per
  boundary          (one per API call)     59 frames, nearly all setup;
  crossings                                submits go through mapped
                                           memory)

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

**Guest kernel driver.** Registers `/dev/nvidiactl`, `/dev/nvidia0…N`,
`/dev/nvidia-uvm` and `/dev/nvidia-modeset`, a DRM device per GPU, and
`/dev/nvgpu-wl` for the Wayland proxy. On `ioctl()` it serializes the request
onto a control virtqueue. For DRM KMS, syncobj and fence calls, nvidia-drm's
permission grants and NVKMS, it gathers whatever the argument points at by a
generated table; RM escapes, UVM and nvidia-drm's GEM calls travel as the first
protocol's single message, with RM's embedded pointers carried alongside as
nested blocks. On `mmap()` it maps the appropriate shared-memory region into the
calling process with the memory type the host mapped it with. It makes no ABI
decisions of its own.

**Device crate.** Receives requests, maps guest handles to host device file
descriptors, performs ABI-aware translation of ioctl parameters — rewriting
embedded pointers and file descriptors, from its own tables and never from the
guest's layout — and issues them against the host's devices. Buffer and window
bookkeeping lives here. Host calls that can block on a display lock run on
per-file executor threads, so that one guest compositor's modeset cannot stall
another process's RM traffic.

**Events.** A second virtqueue runs the other way. The host watches each
descriptor it has opened and says when one becomes readable, which is how a
guest waiting for the GPU is woken. Without it the guest cannot wait at all —
it polls a descriptor the kernel reports as permanently ready, and spins. With
protocol v2 the same queue also carries fence completions, DRM flip and vblank
events, and hotplug.

**Isolate — not built yet.** The plan is a sandboxed helper per guest process,
holding the real device FDs and issuing the `ioctl(2)` calls. Today the backend
does that itself, unprivileged but in one process per VM.
[`ARCHITECTURE.md`](ARCHITECTURE.md), "Future work: the isolate", has the
design.

```text
┌─ Guest ─────────────────────────────────────────────────┐
│  Application → NVIDIA Vulkan / GL / CUDA                │
│                     │ ioctl(/dev/nvidia*)               │
│  driver/ (GPL)      ▼                                   │
│    serialize → virtqueue                                │
│    mmap → shared region                                 │
└───────────────────────────┬─────────────────────────────┘
                            │ VM exit: a virtqueue kick
┌─ VMM ───────────┐  ┌──────▼─ device/ (Apache-2.0) ──────────┐
│ guest RAM and   │  │ vhost-user backend: its own            │
│ the shared      │  │ process, unprivileged, one per VM      │
│ window; maps    │◄─┤   ├─ guest handles → host FDs          │
│ host memory     │  │   ├─ translate embedded FDs and        │
│ there when the  │  │   │  pointers                          │
│ backend asks    │  │   └─ buffer + window bookkeeping       │
└─────────────────┘  │                │                       │
 vhost-user socket   │   ioctl(host /dev/nvidia*) · mmap      │
                     │   (an unprivileged per-guest           │
                     │    isolate is planned, and is not      │
                     │    what runs today)                    │
                     └──────┬─────────────────────────────────┘
                            ▼
                Host NVIDIA driver → GPU
```

---

## Display

How each mode is turned on. The Wayland-client mode (with direct scanout and
explicit sync), the lease and `VK_KHR_display` have run on an RTX 5090 under
nesbox and crosvm against the live patched Hyprland; the compositor VM and
export mode have not run on hardware. [`TESTING.md`](TESTING.md) is the test
plan, and its appendix has the same configuration as a table;
[`DEPLOY.md`](DEPLOY.md) is how to run it in production.

Every mode needs the host booted with `nvidia_drm.modeset=1`, and a guest
driver and backend that both speak **protocol v2**. The guest module asks for it
at probe (`HELLO`); an older backend answers the way it answers any message it
does not know, and the guest stays on v1 with no display features at all.

The backend is started by [`rig/run-guest.sh`](rig/run-guest.sh),
which passes the display flags through and picks the unprivileged user it runs
as: a pool user of the VM's own by default, the socket's owner with
`--wayland-socket`, and the export directory's owner with `--wayland-export`.
Anything else for the backend goes after `--`.

### Wayland client of the host compositor, and direct scanout

The default display mode: each guest application is a client of the host's
compositor, one host connection per guest client.

- **Host:** start the guest with `--wayland-socket
  "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"`. The proxy is written against Hyprland
  0.56; the loopback test runs it against headless sway and weston.
- **Guest:** the daemon, `nvgpu-wl-guest`, serves `$XDG_RUNTIME_DIR/wayland-0`
  and needs `/dev/nvgpu-wl`, which the module creates `root:root 0660` (module
  parameter `wl_mode` to change it). Every open of it is a client of the host
  compositor, so it is meant for the daemon alone:

  ```sh
  groupadd --system nvgpu-wl
  usermod -aG nvgpu-wl <the daemon's account>
  install -m 0644 contrib/udev/70-nvgpu-wl.rules /etc/udev/rules.d/
  udevadm control --reload && udevadm trigger --subsystem-match=misc

  nvgpu-wl-guest &                      # as that account
  WAYLAND_DISPLAY=wayland-0 vkcube --wsi wayland
  ```

- **Direct scanout** needs nothing of ours. A guest client's buffer is a host
  GPU object, and the compositor imports the host's own dma-buf of it, so
  whether it goes straight to a plane is the compositor's decision, made as for
  a native client (Hyprland: `render:direct_scanout`). The proxy maps the
  device numbers in dma-buf feedback between host and guest, so a client can
  find the compositor's scanout tranche and pick a modifier the plane takes.

`wl_shm` buffers are the exception to zero-copy: guest memory is not something
the host compositor can map, so the damaged rows are copied into host memory
at each commit that shows the buffer.

### DRM lease → guest KMS

- **Host:** Hyprland 0.56.2 (`efb50993`) and its aquamarine with the patches
  in [`patches/`](patches/), and a monitor marked `leasable`; stock Hyprland
  leases only outputs the kernel marks non-desktop. For a guest you do not
  trust, lease a monitor Hyprland itself never uses (`disabled = true,
  leasable = true`). Add `--wayland-lease` to `--wayland-socket`.
  `--wayland-lease-interval` spaces one VM's lease requests (5 s on average,
  three at once; 0 lifts it), because a lease of a desktop monitor makes the
  compositor modeset it away and back.
- **Guest:** a Wayland client of the daemon that speaks
  `wp_drm_lease_device_v1` receives the lease as a guest DRM file and drives it
  with ordinary KMS. Only a lease device on this GPU's card is ever shown.

### `VK_KHR_display` / `vkAcquireDrmDisplayEXT`

The lease configuration above. An application acquires the display from the
lease file, and NVIDIA's driver does the rest through nvidia-drm's permission
grant and NVKMS, both forwarded. Outside compositor-VM mode the backend holds
NVKMS to the heads the grant covers.

### Compositor VM, and export mode

- **Host:** no compositor running on the card. Start the guest with
  `--kms-card`: the host's card nodes are offered to the guest, and the backend
  warns that it has done so. Host hotplug and lease uevents are carried to the
  guest's card as its own.
- **Guest:** a compositor (Hyprland, say) opens the guest's `/dev/dri/card*` as
  on bare metal. The guest's DRM core decides who is master; the host follows.
- **Export mode**, for applications outside the VM: add `--wayland-export
  PATH` on the host (a socket created `0600`, and only clients of the backend's
  own uid are accepted), and in the guest run `nvgpu-wl-guest --export NAME`,
  which connects each host client to the guest compositor at `NAME`.

### Explicit sync

On whenever both sides speak protocol v2: fences live on the host, and the
guest holds proxies for them. Guest `sync_file`s are real ones around host
fences, syncobj waits sleep in the guest and never on a host thread, KMS
commits take in-fences and give out-fences, and `wp_linux_drm_syncobj_v1`
timelines reach the host compositor as the host's own syncobjs. Not in export
mode, yet.

### Limits

What one VM may hold through the Wayland proxy, all per VM: `--wayland-max-conns`
channels (64), `--wayland-shm-budget` MiB of shared-memory buffers (1024), and
`--wayland-queue-budget` MiB of compositor output left unread (256).

---

## Scope

**Targeted**

- Vulkan rendering, including presentation to a compositor inside the guest —
  which needs `/dev/nvidia-drm` and `/dev/nvidia-modeset`: they are how a
  buffer becomes shareable
- OpenGL rendering (headless EGL)
- CUDA device memory allocation
- registering memory the guest already has with the GPU:
  `cuMemHostRegister`, `VK_EXT_external_memory_host`, and the buffer
  `cuCtxCreate` registers for itself. RM's OS-descriptor memory is pinned by
  CPU address, which here would be the VMM's, so the guest sends the
  guest-physical pages behind it and the backend maps exactly those (an
  address alone is still refused). `--allow-compute` only
- CUDA ↔ Vulkan/GL interop, zero-copy, GPU-side pointers
- NVENC encoding from CUDA device pointers; NVDEC decoding
- **display** on the host's monitors: the four modes under
  [Display](#display), and explicit sync — the compositor VM and export mode
  not yet run on hardware

**Out of scope**

- `cudaMallocManaged()` / full unified virtual memory
- MIG, SR-IOV
- Arbitrary NVIDIA driver versions — each supported range is explicit, as with
  `nvproxy`

**Refused, so unsupported** — each would let a guest reach past its own
objects on the host, and is turned away before the host driver sees it:

- RM's `EXPORT_TO_DMABUF_FD`, which installs the new dma-buf in the caller's
  descriptor table — the backend's (`EOPNOTSUPP`). NVIDIA's GBM export through
  RM and CUDA's `cuMemGetHandleForAddressRange` with a dma-buf handle fail as
  they would on a driver without dma-buf export. Translating it is future work;
  dma-buf export through the DRM render node is unaffected
- IMEX sessions and fabric memory (classes `0xf1`, `0xf9`, `0xfd`), which name
  an OS event by descriptor and need `/dev/nvidia-caps` files and host-wide
  fabric management that no VM should have (`EPERM`)
- nvidia-drm's `GEM_IMPORT_USERSPACE_MEMORY`, and DRM `GEM_FLINK` and
  `GEM_OPEN`, whose global names reach every file on the host device

---

## Performance

Measured, on one card, by one synthetic load, before protocol v2 — see
[`BENCHMARKS.md`](BENCHMARKS.md) for the method and the raw runs, and for what
this does not support (it does not support a comparison with any other
hypervisor, because none was run).

| | virtio-nvgpu, measured | Venus, by design |
| --- | --- | --- |
| GPU-bound (≥2 ms a frame) | **98–100% of bare metal** | 90–97% |
| Very light frames (≤0.5 ms) | 93–71% of bare metal | — |
| CPU cost of a rendering guest | **same as bare metal** | high (serialize and replay) |
| Host crossings per frame | **~0.02** | thousands |
| Guest-side NVENC | works, zero-copy | not viable |

The difference is structural: Venus crosses the VM boundary **per API call**,
thousands of times a frame. `virtio-nvgpu` crosses it **per ioctl** — and a
render loop issues none, because submission is a write to mapped memory. What
is left at very light frames is not forwarding but *waiting*: the guest sleeps
for the GPU, and the wake costs ~0.02 ms however small the frame was.

The Venus column is that project's design envelope, not something measured
here.

---

## Driver versions

NVIDIA's kernel driver ABI is not stable; ioctl struct layouts change between
releases. Support is explicit, and this is the whole list.

**The backend starts only on a host release its tables were measured at.**
Four tables stand between a guest and the host driver, each read from the
release's own sources (`device/src/release.rs`): the RM allowlist and the
NVKMS schema, measured at the very release; the ABI profile, a range; and the
UVM block sizes, a range proven over the releases in it. Today that is:

| host release | RM allowlist, NVKMS schema | ABI profile | UVM table |
|---|---|---|---|
| `535.129.03` | its own | `535.129.03` | its own |
| `580.178.04` | its own | `580.178.04` | its own |
| `595.71.05` | its own | `595.71.05` | its own |
| `595.99.02` | its own | `595.71.05` | its own |
| `610.57.04` | its own | `595.71.05` | its own |
| `615.71.09` | its own | `595.71.05` (measured through 615.71.09) | its own |

Any other release is **refused at start-up**, with a line naming each table
it lacks. `--allow-unmeasured-release`, a diagnostic flag, runs a newer or
in-between host on the nearest older tables without compute, and says so at
every start; a host older than 535.129.03 is refused regardless. Forwarding an
ioctl whose layout has never been seen is how you get a plausible wrong
answer instead of an error. Adding a release is described in
[`DEPLOY.md`](DEPLOY.md), "Upgrading the host driver", and
[`gen/README.md`](gen/README.md).

The guest's NVIDIA userspace must be the host's own release, as it must
natively: RM refuses a client of another release (`NVRM: API mismatch`).

**Driver versions actually run:**

| version | card | how far it got |
|---|---|---|
| **595.99.02** | RTX 5090 | the current code: every graphics and compute path, three display modes, the application pass, under nesbox and crosvm ([`rig/TESTING-RIG.md`](rig/TESTING-RIG.md)) |
| **595.99.02** | RTX 3060 | the code before protocol v2: renders, presents, encodes, and every number in [`BENCHMARKS.md`](BENCHMARKS.md) |
| **615.71.09** | RTX A2000 | the code before protocol v2: enumerates and renders; not benchmarked, and not re-tested since |

Anything else is untested.

### How a profile is built

The cost is bounded, for three reasons. ABI profiles key off **ranges, not
points**, so a new release needs a new profile only when an escape's block
moved. The struct
half is **derived mechanically** from NVIDIA's published `open-gpu-kernel-modules`
at each tag — compile a probe per field, read back `sizeof` and `offsetof` —
rather than transcribed by hand. And the judgement half, which commands exist and
which are safe, tracks `nvproxy` upstream.

See [`gen/`](gen/), and `Coverage` in `device/src/release.rs` for the rule in
code — that, not this table, is the thing that decides.

Protocol v2 and the work around it added tables measured per **release**
rather than per profile: NVKMS and nvidia-drm ioctl layouts, the pointers RM
follows inside control parameters (both for 535.129.03, 580.178.04, 595.71.05,
595.99.02, 610.57.04 and 615.71.09), and UVM's parameter block sizes (those six
and the four releases between them where a size changed). Each is extracted
from the release's own sources by a script in `gen/`. Under
`--allow-unmeasured-release` a host between two measured releases uses the
older NVKMS table, and an NVKMS command whose layout moved in the next release
measured runs only on a host of exactly its table's release; the RM control
pointer table is the union of every release's.

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

Code ported from other projects keeps its original terms; [`NOTICE`](NOTICE)
says what comes from gVisor's nvproxy, NVIDIA's open-gpu-kernel-modules and
the vendored Wayland protocols. Every source file names its licence in an
`SPDX-License-Identifier` line.

## See also

- [`DEPLOY.md`](DEPLOY.md) — how to run it: host requirements, what is
  supported, per-VM users and the systemd unit, the flags, the guest image,
  what to alert on, and upgrading the host driver.
- [`BENCHMARKS.md`](BENCHMARKS.md) — what it costs against bare metal, how that
  was measured, and what the numbers do not support.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — how it works, in prose: what crosses
  the VM boundary and what does not, how memory is shared, how a buffer becomes
  shareable, how a guest waits, how the display paths are built, and what the
  design cannot do.
- [`SECURITY.md`](SECURITY.md) — what a guest can reach on the host, what
  narrows it, how to run the backend, and what is still open.
- [`TESTING.md`](TESTING.md) — the plan for running the display paths on the
  GPU box, stage by stage, and what each stage counts as a pass.
- [`patches/README.md`](patches/README.md) — the Hyprland and aquamarine
  patches for leasing a desktop monitor, and how to configure them.
