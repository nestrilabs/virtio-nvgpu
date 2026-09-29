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

**Those numbers are an RTX 3060's, on the code before protocol v2.** The
current code was measured again on an RTX 5090 (2026-09-29, the same
programs natively and in nesbox and crosvm guests,
[`BENCHMARKS.md`](BENCHMARKS.md)): frames of a millisecond or more within
3% of bare metal, 4.7 ms and above within 1%; CUDA, OpenCL, NVDEC, NVENC and
every copy between host and GPU at native speed. What crosses to the host
costs a round trip each -- an RM call about 2 µs more than natively, a CPU
mapping of GPU memory about 2-3 times as long, a wait that sleeps 12-16 µs more --
so starting a Vulkan device takes about 1.4 times as long.

### Several guests on one card

Four guests on one RTX 3060, the same load in each: **25.84, 26.49, 25.57, 25.79
fps** — 103.7 together, against 102.9 for a single guest — with p50 frame times
of 39.165, 39.164, 39.168 and 39.165 ms. The total does not move as guests are
added, and the split is even to four decimal places.

All four render correctly at the same time, and four of them **encode H.264 at
once**, each paced at exactly 60 Hz, with no NVENC session limit reached.

Four is what was run, not a limit found.

### What is known to work

This is the one status table; [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md),
"Where the runs stand", has every run with its date and its results. Every
run below was on an **RTX 5090 with driver 595.99.02**, on an AMD host,
with the RM allowlist enforcing and the backend's sandbox on, last on
2026-09-29.

| what | nesbox | crosvm |
|---|---|---|
| a guest enumerates the card (`nvidia-smi` with real power and memory, the host's `deviceUUID`); Vulkan, OpenGL and EGL render, offscreen draws pixel-correct | run | run |
| CUDA with `--allow-compute`; without it no UVM device, and CUDA finds no GPU, cleanly | run | run (the nvgpu frontend jailed) |
| Wayland client of the live host Hyprland, a fullscreen guest window scanned out directly | run | run |
| a monitor leased to the guest and driven with KMS, `VK_KHR_display` on it, the lease handed back | run | run |
| the security negatives, on the control and render nodes and on a leased card | run | run |
| about 35 applications as an unprivileged guest user, browsers' sandboxes on, on the live desktop: SuperTuxKart, Neverball, Godot, Blender (EEVEE, and Cycles on CUDA and OptiX), GIMP, Inkscape, Krita, LibreOffice, Firefox, Chromium, Electron, mpv, ffmpeg with Vulkan Video, NVENC, NVDEC and VA-API, OpenCL | run | run |
| capture injection, with the rig's own helper and daemon | run | run |
| the guest module with its Rust parsers (the default) and its C ones | run | run |
| frame pacing on a monitor (the Wayland mode) | measured | measured |
| the compositor VM (`--kms-card`), export mode, hotplug | **not run** | **not run** |
| a root run of the launcher, the socket-activated units, the VMM templates | **not run** | **not run** |

Earlier, on an RTX 3060 (595.99.02) before protocol v2: every number in
[`BENCHMARKS.md`](BENCHMARKS.md)'s section on it, four guests on one card,
and NVENC through Vulkan Video encoding on the client's own device. An RTX
A2000 on 615.71.09 enumerated and rendered on the code before protocol v2.
The driver releases the backend starts on are in [Driver versions](#driver-versions).

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

**Modes 1 to 3 have run on hardware**, against the live patched Hyprland
(0.56.2); **mode 4, and export mode, have not**: they need the desktop
stopped ([What is known to work](#what-is-known-to-work)). Only the Wayland
mode's frame pacing has been timed ([`DEPLOY.md`](DEPLOY.md), "Frame
pacing"). [`TESTING.md`](TESTING.md) is the plan, stage by stage; how to
turn each mode on: [Display](#display).

### What a guest can reach

Worth stating plainly, because it is the first question a security person asks
and the honest answer is not "nothing". [`SECURITY.md`](SECURITY.md) is the
full account, and its "Summary" the short one; this is shorter still.

- There is **no IOMMU boundary** between guest GPU work and the host. The
  guest gets the host NVIDIA driver's ioctl interface, not the device, and
  what separates a guest from host memory is the GPU's own MMU, with page
  tables RM programs: **the host NVIDIA driver is in the TCB**.
- The guest kernel is untrusted. Everything that protects the host is
  decided by the backend, from its own tables, never from a layout the guest
  sent: ioctls the tables do not describe are refused; every pointer field
  the tables, measured per release, say the host follows is pointed at a
  buffer of the backend's or zeroed; guest descriptor numbers are translated
  or refused, but for a few inside RM control parameters; and RM controls
  and classes are allow-listed per driver release, default deny.
- **The backend runs unprivileged**, because RM, DRM and NVKMS take a
  guest's privilege from its credentials: it refuses root and
  `CAP_SYS_ADMIN`, drops every capability, sandboxes itself (a network
  namespace, Landlock, seccomp) before the first guest message, and runs as
  a user of its own per VM ([`DEPLOY.md`](DEPLOY.md)).
- **Compute is opt-in** (`--allow-compute`): without it the guest has no
  UVM device and no memory registered by its pages.
- RM objects stay with the guest process that made them, the display paths
  are held to leases, grants and the VM's own framebuffers, and the Wayland
  allowlist is enforced on the host.
- Not done: the per-guest isolate, so the backend holds the host
  descriptors in the process that maps guest memory; the open items are
  [`SECURITY.md`](SECURITY.md)'s "Open items and residual risk".

**Guest system memory is GPU-coherent.** On an Intel host KVM ignores the
guest's page attributes unless the VMM disables a quirk, so the backend
makes every GPU mapping of guest system memory snoop; display memory a
client allocates non-coherent stays exposed there, and the backend says so
at start ([`ARCHITECTURE.md`](ARCHITECTURE.md), "Memory types, and
coherency"; the operator's side in [`DEPLOY.md`](DEPLOY.md)).

**This is attack-surface reduction, not hardware isolation.** VFIO passthrough
with an IOMMU is strictly stronger — it constrains the device to the guest's own
memory — and for mutually untrusted tenants that or vGPU is still the answer.

### What is not done

- **the compositor-VM and export modes on hardware**, and hotplug: they need
  the host desktop stopped, and have run only in unit and loopback tests.
- **per-present crossings on a lease**, and several guests on one card on
  the current code: the per-present count was taken only for the Wayland
  mode (17 round trips a frame, [`DEPLOY.md`](DEPLOY.md), "Frame pacing").
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
| [`scripts/`](scripts/) | **Apache-2.0** | The project's own tooling: `ci.sh` (the checks, in tiers), `check-unsafe.sh`, `fuzz.sh`, `gen-check.sh` (every generated table against its sources), `build-guest-kernel.sh` (the guest kernel and module), the Wayland loopback test, `verify-units.sh` (`systemd-analyze verify` of the units), `vmm-parity.py` (each VMM's limits on the backend's mapping requests against the backend's own). |
| [`contrib/`](contrib/) | **Apache-2.0** | What a deployment installs: the backend's systemd unit and socket units per VM, the helper that snapshots the GPU's PCI config for it, templates for the VMM's unit (nesbox, crosvm), and the guest's udev rules for `/dev/nvgpu-wl` and `/dev/nvgpu-capture` ([`DEPLOY.md`](DEPLOY.md)). |
| [`nix/`](nix/) | **Apache-2.0** | The NixOS module for the backend (`module.nix`: per-VM users, the units, a drop-in per slot), the units of `contrib/systemd` for a backend in the Nix store (`units.nix`), and the module's own checks (`module-test.nix`, `unit-diff.py`). The root `flake.nix` packages the backend, the daemon and the units, and runs the fast checks. |
| [`rig/`](rig/) | **Apache-2.0** | The dev box's test rig, not for production: the launcher (`run-guest.sh`, and its pieces in `launcher/`), preflight, the test guest image (`guest-image/`), the on-device helpers (`verify/`), and [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md). Its data lives in `.rig/`, git-ignored. |
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
- **Publish shared memory region 1, the window, at the size
  `GET_SHMEM_CONFIG` (request 44) reports for it** -- the backend's
  `--window-size`, 1 GiB by default and up to 64 GiB -- as a 64-bit
  prefetchable BAR above 4 GiB with a virtio shared-memory capability, and
  place what the backend asks for in it: `BACKEND_REQ` plus `SHMEM`,
  `SHMEM_MAP`/`UNMAP` (backend requests 9 and 10: region id, file offset,
  region offset, length, read/write flag, the file descriptor with the
  message), answered when `REPLY_ACK` is negotiated. `GET_SHMEM_CONFIG`
  reports region 1, and region 2 only with `--allow-compute`; a VMM that
  assumes 1 GiB instead breaks every VM given a larger window (the backend
  says so once, at the memory table, when the VMM never asked). **Check
  every request** against the region: the backend is another process and
  may be compromised.
- **Prefault what it places**, where the host kernel has
  `KVM_PRE_FAULT_MEMORY` (6.11): without it a guest's first write to fresh
  video memory runs at about a tenth of the host's speed, one second-level
  fault a page. Both VMMs below do it through a spare vCPU that never runs,
  with an id past every guest vCPU's (SECURITY.md, "Prefaulting the window").
- **Give the window write-back in the guest's MTRRs** (default type WB with
  the 32-bit PCI hole UC, on every vCPU), or the guest driver warns that
  the window "is not write-back in this guest's MTRRs".
- **Leave the host GPU's PCI bus free in the guest.** The guest driver gives
  the GPU the host's own PCI address (NVIDIA's userspace looks it up by
  that), on a PCI host bridge of its own, so a guest with a bridge on that
  bus number has no GPU device.
- For compute only, the **UVM aperture** (region 2): see
  [`ARCHITECTURE.md`](ARCHITECTURE.md), "The UVM aperture". A VMM
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
`virtio-nvgpu-v6`, not yet merged upstream; its jail's changes over `v5`
have not run on hardware) has its own frontend for the device. **crosvm** takes the ten patches in
[`patches/crosvm/`](patches/crosvm/): a vhost-user device type `nvgpu` whose
every mapping request the frontend checks against its region, the UVM
aperture after the window in the window's BAR, and, with crosvm's sandbox
on, the nvgpu frontend in a jailed process of its own, whose every request
the main process checks again before it maps anything.
[`patches/README.md`](patches/README.md) says what each patch does, and
[`SECURITY.md`](SECURITY.md), "The VMMs", what each VMM checks. crosvm's
vhost-user fork already implements the upstream `GET_SHMEM_CONFIG`,
`SHMEM_MAP` and `SHMEM_UNMAP` messages byte for byte as rust-vmm does, so the
protocol needed nothing new; nesbox asks `GET_SHMEM_CONFIG` from
`virtio-nvgpu-v4`. How to run it: [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md), "crosvm".

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
  and needs `/dev/nvgpu-wl`, which the module creates `root:root 0660` and
  which is for the daemon alone: every open of it is a client of the host
  compositor. [`DEPLOY.md`](DEPLOY.md), "The guest", has the setup (the
  daemon setgid `nvgpu-wl`, the udev rule); then
  `WAYLAND_DISPLAY=wayland-0 vkcube --wsi wayland`.

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

### Screen capture into a guest

A host screen share, handed to a guest application without a copy: the
VM's capture helper on the host gets the stream through the desktop's
portal and injects its GPU buffers into the backend (`--inject-socket PATH
--inject-uid UID`), and the guest's capture daemon opens each, with the id
and token the helper tells it, as a read-only guest dma-buf through
`/dev/nvgpu-capture`. PipeWire and the portal stay out of the backend; only
this GPU's own nvidia-drm memory is taken. The helper and the daemon are
the integrator's: [`DEPLOY.md`](DEPLOY.md), "Capture injection", has their
interface, [ARCHITECTURE.md](ARCHITECTURE.md) the design and
[SECURITY.md](SECURITY.md) what it trusts, each in its "Capture injection".

### Explicit sync

On whenever both sides speak protocol v2: fences live on the host, and the
guest holds proxies for them. Guest `sync_file`s are real ones around host
fences, syncobj waits sleep in the guest and never on a host thread, KMS
commits take in-fences and give out-fences, and `wp_linux_drm_syncobj_v1`
timelines reach the host compositor as the host's own syncobjs. Not in export
mode, yet.

### Limits

What one VM may hold through the Wayland proxy is bounded per VM and per
guest process: channels, shared-memory buffers and unread compositor output
(the flags and their defaults: [`DEPLOY.md`](DEPLOY.md), "Backend flags";
every cap: [`SECURITY.md`](SECURITY.md), "Resource caps").

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
hypervisor, because none was run). The current code, re-measured on an RTX
5090 across rendering, compute, memory, video, start-up and the control path,
keeps the GPU-bound row below; its full table is in the same file. The CPU
row still rests on the RTX 3060 run: the RTX 5090 tables have no CPU cost
per frame.

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

**The backend starts only on a host release its tables were measured at**
-- today 535.129.03, 580.178.04, 595.71.05, 595.99.02, 610.57.04 and
615.71.09 -- and refuses any other at start-up, with a line naming each table
it lacks. Forwarding an ioctl whose layout has never been seen is how you get
a plausible wrong answer instead of an error. Which table each release gets,
and the diagnostic way through an upgrade, is [`DEPLOY.md`](DEPLOY.md), "The
exact-measured-release rule"; measuring a new release is
[`gen/README.md`](gen/README.md), "A new host release".

The guest's NVIDIA userspace must be the host's own release, as it must
natively: RM refuses a client of another release (`NVRM: API mismatch`).

**Driver versions actually run:**

| version | card | how far it got |
|---|---|---|
| **595.99.02** | RTX 5090 | the current code: every graphics and compute path, three display modes, the application pass, under nesbox and crosvm ([`rig/TESTING-RIG.md`](rig/TESTING-RIG.md)) |
| **595.99.02** | RTX 3060 | the code before protocol v2: renders, presents, encodes, and the numbers of [`BENCHMARKS.md`](BENCHMARKS.md)'s section on it |
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
