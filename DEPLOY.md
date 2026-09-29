# Deploying virtio-nvgpu

How to run the backend for real guests: what the host must have, which
combinations are supported, how each VM gets a user of its own, which flags
exist and which are only for diagnosis, what a guest image must contain, and
what to watch for in the logs. [`SECURITY.md`](SECURITY.md) says what each of
these protects against; [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md) is how the project
itself runs guests on its dev box, with a launcher that is not meant for
production.

## Host requirements

**Kernel.** Linux with KVM, and, for the backend's sandbox, which refuses to
start without every layer in force (`sandbox: DEGRADED`):

- **Landlock, ABI 9 or later**, enabled (`CONFIG_SECURITY_LANDLOCK=y` and
  `landlock` in the `lsm=` list). ABI 6 scopes signals and abstract sockets,
  ABI 9 pathname UNIX sockets; below either the backend reports DEGRADED. The
  backend logs the ABI it found (`sandbox: landlock: ABI N, ...`).
- **seccomp filters** (`CONFIG_SECCOMP_FILTER`).
- **A network namespace for the backend.** The shipped unit gives it one
  (`PrivateNetwork=yes`), which the backend keeps. Started any other way, the
  backend makes its own, which needs unprivileged user namespaces
  (`user.max_user_namespaces` > 0, and no `kernel.unprivileged_userns_clone=0`
  or AppArmor userns restriction on it).
- `/dev/udmabuf` (`CONFIG_UDMABUF`, the `udmabuf` module) for the Wayland
  modes' shm buffers.

**The NVIDIA driver.** NVIDIA's **open** kernel modules, at a release the
backend's tables were measured at (below), with **`nvidia_drm.modeset=1`**:
NVKMS is how a buffer becomes shareable and what the display paths and
nvidia-drm's semaphore-surface fences go through. For `--allow-compute`,
`nvidia_uvm` loaded at boot: the unit's `ProtectKernelModules=` and the
backend's `no_new_privs` keep it from being loaded on demand. The host RM
must keep each client to the file it was made on (the default): the backend
asks at start and refuses to run otherwise (SECURITY.md §11, R3).

**The exact-measured-release rule.** The backend starts only on a host driver
release every one of its tables was measured at (`device/src/release.rs`):
the RM allowlist and the NVKMS schema of that very release, a UVM table whose
range holds it, and an ABI profile measured through it. Today that is
**535.129.03, 580.178.04, 595.71.05, 595.99.02, 610.57.04 and 615.71.09**.
Any other release is refused at start with a line naming what it lacks.
`--allow-unmeasured-release` (diagnostic) runs a newer or in-between host on
the nearest older tables without compute; it is for getting through an
upgrade, not for running tenants. The guest's NVIDIA userspace must be the
host's own release, exactly, as it must natively.

**IOMMU.** The GPU stays with the host's driver, in the host's IOMMU domain;
the guest gets the driver's interface, not the device, so the IOMMU is not a
boundary between a guest and the host here (SECURITY.md §2). Leaving DMA
translation on (`amd_iommu=on` / `intel_iommu=on`, not `iommu=pt`) still
holds the GPU to what the host driver mapped for it, which is defence in
depth against a bad GPU page table; nothing in this project needs either
setting.

**Intel hosts: the guest-PAT quirk.** On Intel, KVM maps guest RAM write-back
whatever the guest's page attributes say, unless the VMM disables
`KVM_X86_QUIRK_IGNORE_GUEST_PAT` (`KVM_CAP_DISABLE_QUIRKS2`). The backend
makes guest system memory GPU-coherent to cover that, but display memory a
client allocated non-coherent stays exposed; the backend says so at every
start on an Intel host (`Intel host: KVM ignores guest PAT ...`). Neither VMM
below disables the quirk today. AMD hosts are not affected; every hardware
run so far was on AMD.

## Upgrading the host driver

A new NVIDIA release is refused until it is measured. Before upgrading a host:

1. Measure the release from its published sources (`gen/README.md`, "A new
   host release"): `gen/rmallow_extract.py extract`, `gen/nvkms_extract.py
   extract`, `gen/uvm_extract.py extract` and `scan`, `gen/rmctrl_extract.py
   extract`, then the renders and `gen/schema_gen.py`; move
   `MEASURED_THROUGH` in `gen/src/versions/mod.rs` once gVisor's nvproxy (or a
   capture in `gen/fixtures/`) shows the RM escapes unchanged, or add a
   profile (`gen/nvabi_gen.py`) if they moved.
2. Run `scripts/gen-check.sh` (network access; `GVISOR=<checkout>` to include
   the ABI profiles): every checked-in table must still be what its extractor
   produces.
3. `cargo test --workspace --features device/vhost-user`; then the guest
   side: rebuild the guest images with the new release's userspace.
4. Run the new release on a test host through the application workloads you
   serve, with `--rm-allowlist=log` only there, and read the teardown's
   `RM allowlist ... refused` lines: a control the new release's userspace
   needs and the allowlist lacks is a policy decision (SECURITY.md §12), not
   something to allow by default.
5. Ship the backend and the guest images together, then upgrade the host
   driver and reboot. A backend that starts logs the tables it chose on one
   warning line (`host driver X: RM allowlist X, ABI profile ..., NVKMS schema
   X, UVM table X`); anything marked `NEAREST OLDER, unmeasured` means step 1
   was skipped.

## What is supported

| | nesbox | crosvm |
|---|---|---|
| where | [github.com/nestrilabs/nesbox](https://github.com/nestrilabs/nesbox), branch `virtio-nvgpu-v3` (not yet merged upstream) | upstream crosvm `c0474109d64d` with [`patches/crosvm/`](patches/crosvm/) `0001`-`0009` |
| graphics (Vulkan, GL, EGL, Vulkan Video) | run on hardware | run on hardware |
| `--allow-compute` (CUDA, NVENC/NVDEC through CUDA, OpenCL) | run on hardware | run on hardware (needs `0007`-`0009`; its nvgpu frontend runs jailed) |
| Wayland client of the host compositor, direct scanout | run on hardware | run on hardware |
| DRM lease → guest KMS, `VK_KHR_display` | run on hardware | run on hardware |
| compositor VM (`--kms-card`), export mode | **not run on hardware** | **not run on hardware** |
| capture injection (`--inject-socket`), with the rig's own helper and daemon | run on hardware | run on hardware |

"Run on hardware" means an RTX 5090 with 595.99.02 on an AMD host, with the
probes and the application pass of [`rig/TESTING-RIG.md`](rig/TESTING-RIG.md) green,
the RM allowlist enforcing and the sandbox on. The lease modes need the
patched Hyprland 0.56.2 ([`patches/`](patches/)) as the host compositor. The
compositor-VM and export modes (rig/TESTING-RIG.md, "Group C") need the host
desktop stopped, and have run only in unit and loopback tests: treat them as
unsupported until they have run. What each VMM checks of the backend's
mapping requests: SECURITY.md §16.

A VMM other than these two must do what README.md, "What a VMM must do, over
vhost-user", lists.

## Per-VM users

Each VM's host processes are users of their own, so that no two VMs are one
principal to the kernel or to RM (SECURITY.md §4, "One uid per VM"). Slot N
is two system users:

- `nvgpu-vmN`, in group `nvgpu-vmN`: the backend. It needs no supplementary
  group of its own; the unit hands it `video`, `render` and `kvm` for the
  render and card nodes and `/dev/udmabuf` (`/dev/nvidia*` are 0666).
- `nvgpu-vmmN`, in group `nvgpu-vmN` and no other: the VMM. The backend's
  socket, `/run/nvgpu/vmN/nvgpu.sock`, is opened to that group alone.

```sh
for i in 0 1 2 3; do
  useradd --system --no-create-home --shell /usr/sbin/nologin --user-group nvgpu-vm$i
  useradd --system --no-create-home --shell /usr/sbin/nologin -g nvgpu-vm$i nvgpu-vmm$i
done
```

On NixOS, [`nix/module.nix`](nix/module.nix) declares the pool, the groups
and the unit (`services.virtio-nvgpu = { enable = true; package = ...; slots
= 4; }`; the root flake's `nixosModules.default` sets the package).

**The backend.** [`contrib/systemd/vhost-user-nvgpu@.service`](contrib/systemd/vhost-user-nvgpu@.service),
instance N, runs `vhost-user-nvgpu --socket /run/nvgpu/vmN/nvgpu.sock` as
`nvgpu-vmN` in a cgroup of its own (`MemoryMax=2G`, `MemorySwapMax=0`,
`TasksMax=256`, `OOMScoreAdjust=500`, `LimitCORE=0`, `Restart=no`), with no
capabilities, `NoNewPrivileges`, a network namespace of its own, and the file
system read-only (`ProtectSystem=strict`, `ProtectHome=yes`). Its
`ExecStartPost` helper, [`contrib/systemd/nvgpu-socket-open`](contrib/systemd/nvgpu-socket-open)
(install it at `/usr/libexec/virtio-nvgpu/`), makes the socket's directory
root's and opens the socket to the slot's group once the backend has bound
it. Per-VM flags go in `/etc/virtio-nvgpu/vmN.env` as
`NVGPU_BACKEND_ARGS="--allow-compute"` (one word per flag: systemd splits
the variable at spaces). Raise `MemoryMax` with `--wayland-shm-budget` and
`--wayland-queue-budget`. The backend is never restarted alone: the VM's
device goes with it, so restart the VMM with it.

**The VMM**, as `nvgpu-vmmN`, with a unit that has
`Requires=vhost-user-nvgpu@N.service` and `After=vhost-user-nvgpu@N.service`
and a network namespace of its own:

- nesbox, a config with `"gpu-forward": { "socket": "/run/nvgpu/vmN/nvgpu.sock" }`,
  under nesbox's jailer, with `"unshare-network": true` or a network
  namespace from its unit;
- crosvm, `crosvm run ... --vhost-user type=nvgpu,socket=/run/nvgpu/vmN/nvgpu.sock,max-queue-size=256 --no-pci-hotplug-port`,
  with its sandbox on (the default) and a `--pivot-root` directory that
  exists and is empty. crosvm publishes the UVM aperture when the backend
  reports it (`--allow-compute`).

Both need `/dev/kvm` for the VMM user, and both take the shared window's
size from the backend (GET_SHMEM_CONFIG): crosvm always has, nesbox from
branch `virtio-nvgpu-v4` (an older nesbox publishes 1 GiB whatever the
backend's `--window-size` says, and every placement past it fails). Both
prefault what they place in the window -- nesbox from branch
`virtio-nvgpu-v5`, crosvm with `patches/crosvm/0010` -- on a host kernel
with `KVM_PRE_FAULT_MEMORY` (6.11 or later): without it a guest's first
write to fresh video memory runs at about a tenth of the host's speed, one
second-level fault a page (BENCHMARKS.md; SECURITY.md §21). Guest
RAM is committed at boot. The window is the configured size (`--window-size`,
1 GiB by default), and with compute the UVM aperture another 1 GiB; what
the window costs in host memory depends on the VMM ("Sizing the window").

**The VMM of a compute VM** (`--allow-compute`) checks each UVM pool the
backend places with `mincore` on its own descriptor of `/dev/nvidia-uvm`
before giving the pool a memory slot (crosvm's `patches/crosvm/0007`,
nesbox's `fds.rs`). Two things a unit can take away break every pool with
EACCES:

- the `mincore` syscall, which is not in systemd's `@system-service` set:
  a VMM unit with `SystemCallFilter=@system-service` needs `mincore` added;
- **write** access to `/dev/nvidia-uvm` in the device cgroup: the check
  runs `access("/proc/self/fd/N", W_OK)`, and the kernel's
  `can_do_mincore` asks `file_permission(MAY_WRITE)`; both go through
  `devcgroup_inode_permission`. Under `DevicePolicy=closed`, give
  `DeviceAllow=/dev/nvidia-uvm w` (write alone is enough; the check goes
  through the descriptor the backend sent, so the path itself can stay
  hidden with `InaccessiblePaths=`).

**Device cgroup names.** NVIDIA's open kernel modules register character
major 195 as `nvidia` in `/proc/devices`, not `nvidia-frontend`: a unit that
restricts devices must say `DeviceAllow=char-nvidia` (or the nodes by path),
and `char-nvidia-frontend` alone denies `/dev/nvidia0`. The shipped
backend unit sets no device policy (Landlock holds the backend to the GPU's
nodes), so it is unaffected; a VMM unit that adds one is not.

**The Wayland modes.** With `--wayland-socket` the backend connects to the
host compositor's socket, which lives in the desktop user's
`$XDG_RUNTIME_DIR` (0700). Give the VM's user access to that one socket, not
to the directory's other files, and let the unit see it:

```sh
setfacl -m u:nvgpu-vm0:x  /run/user/1000
setfacl -m u:nvgpu-vm0:rw /run/user/1000/wayland-1
```

```ini
# /etc/systemd/system/vhost-user-nvgpu@0.service.d/wayland.conf
[Service]
ProtectHome=tmpfs
BindReadOnlyPaths=/run/user/1000/wayland-1
Environment="NVGPU_BACKEND_ARGS=--wayland-socket /run/user/1000/wayland-1"
```

The compositor makes its socket anew each session, so the ACL goes with
it: set it from the session's own start-up. The rig's launcher instead runs
the backend as the socket's owner (the
desktop user), which puts a compromised backend one step from the desktop
session; the per-VM user above avoids that. `--wayland-lease` needs the
patched Hyprland and a monitor marked `leasable` ([`patches/`](patches/)).
Export mode admits only host clients of the backend's own uid, so there it
is run as the owner of the export socket's directory; like the compositor VM
it has not run on hardware.

## Sizing the window

Device memory a guest process maps for the CPU is placed in the VM's
**shared window**, in one of three zones by the memory type the host maps
it with: uncached (registers, the usermode doorbell), write-combining (video
memory, through BAR1) and write-back (GPU-coherent system memory). A mapping
that the zone, or the process's share of it, cannot take fails with ENOMEM
(`SHM alloc failed` in the backend's log); a write-back one falls back to
write-combining first, which is correct but slow to read. The default, 1 GiB
at half a zone per process, is enough for every application the project has
run; a VM for one heavy application (a large game, a big 3D scene) can be
given more with `--window-size` and `--window-owner-share`.

**What the zones get.** UC stays at 32 MiB, and WC and WB split the rest
24:7 as the default does:

| `--window-size` | UC | WC | WB |
|---|---|---|---|
| 256 | 8 | 192 | 56 |
| 1024 (default) | 32 | 768 | 224 |
| 4096 | 32 | 3148 | 916 |
| 16384 | 32 | 12660 | 3692 |

**What applications use**, the most in use at once, on an RTX 5090 (595.99.02)
under nesbox, one VM per application, as an unprivileged guest user (MiB;
the backend logs the same line, `window use:`, when the VM stops):

| application | UC | WC | WB | largest mapping |
|---|---|---|---|---|
| SuperTuxKart (GL) | 0.1 | 4.5 | 34.6 | 4 |
| SuperTuxKart in gamescope | 0.4 | 15.5 | 60.2 | 7.9 |
| Blender EEVEE, GL | 0.1 | 29.0 | 86.3 | 21.3 |
| Blender EEVEE, Vulkan | 0.1 | 34.5 | 82.7 | 32 |
| glmark2 | 0.1 | 2.5 | 4.3 | 2 |
| vkmark | 0.1 | 2.5 | 14.2 | 2 |
| Chromium, animation | 0.1 | 2.5 | 23.2 | 4 |
| render probe with CUDA | 0.1 | 2.0 | 54.0 | 40 |

UC never passed half a MiB, so it does not grow. Every application here used
more write-back than write-combining, and Blender's 86 MiB is three quarters
of a process's default WB share, so WB keeps its proportion rather than
staying put; WC takes most of the growth because it has to hold its own
and WB's overflow, and because the one workload known to exhaust a default
window did it in WC (a Minecraft launcher on this GPU, in part through a
backend defect since fixed, SECURITY.md §19). On an older T4, CUDA peaked
at 68 MiB and an NVENC encode at 116 MiB, before system memory was told
apart from video memory.

**Choosing it.** Keep the default unless the backend logs `SHM alloc
failed` for a VM, or its `window use:` line shows a process near its share
(`by one process` against half the zone). Then raise `--window-size` in
steps of 64 MiB; `--window-owner-share` above 50 lets one process have
most of each zone, which suits a VM that runs one application and takes
from the VM's other processes the chance to map much at once
(SECURITY.md §19: from 88 %, one process can leave the others only the
reserve).

**What it costs the host.**

- *BAR1.* Video memory a guest maps is mapped through the GPU's BAR1, which
  the host's desktop and every other VM share. One VM can hold at most its
  WC zone there. The backend warns at start when that is more than half of
  a GPU's BAR1 (`nvidia-smi -q -d MEMORY`, "BAR1 Memory Usage"; 32 GiB on an
  RTX 5090 with resizable BAR, 256 MiB without it). Keep the WC zones of
  the VMs that run at once, plus the desktop's own use, below the BAR1
  total. VRAM itself is not bounded by the window, only how much of it is
  CPU-mapped at once.
- *Host memory.* The backend's copy of the window is a sparse memfd that
  its own pages never fill: the backend's `MemoryMax=2G` needs no change.
  nesbox reserves the guest-visible window `PROT_NONE`: it costs nothing
  but the placements, which are device memory. crosvm's is shared anonymous
  memory: a page of it the guest touches with nothing placed there is
  faulted in and charged, like guest RAM, to the VMM's cgroup, up to the
  window's size. No guest driver path does that, but a guest kernel can, so
  under crosvm size the VMM's `MemoryMax` as guest RAM plus the window (plus
  its own overhead).
- *Address space.* crosvm puts the window and the UVM aperture in one BAR,
  the next power of two above both (16 GiB plus the aperture is a 32 GiB
  BAR) and refuses past 64 GiB; nesbox puts each in a BAR of its own in a
  64 GiB MMIO window and refuses a window past 32 GiB. Both refuse at start,
  with the size named.

## Frame pacing

Measured on the rig (RTX 5090, 595.99.02, Ryzen 9 9950X, Hyprland 0.56.2,
`render:direct_scanout` 0 unless said), 2026-09-29, with
`rig/rig-framepace.sh` (rig/TESTING-RIG.md, "Frame pacing"): the same
workload natively and in a nesbox guest (4 vCPUs, 4 GiB), fullscreen on a
240 Hz monitor, MangoHud logging every frame's present interval, 20 s after
a 6 s warm-up, three runs each, interleaved. "Stutter" is frames longer than
1.5x the refresh period (6.25 ms), each a missed vblank; frames counted
over all three runs. The loaded rows run a stress-ng host load through each
run (32 workers at 60 %: every CPU busy, as on a desktop compiling or
encoding). "Before" is `b693254`; "after" is this branch with its defaults.

Frame times in ms (every FIFO run's mean is 4.166, its p50 4.16-4.17);
stutter out of about 14,400 frames. Native was measured in each session,
before / after:

| workload | native p99 / p99.9 / stutter | VM before | VM after |
|---|---|---|---|
| vkcube, FIFO | 4.63 / 8.49 / 53; 4.52 / 8.32 / 23 | 4.83 / 8.43 / 30 | 4.64 / 8.40 / 34 |
| vkcube, mailbox: mean (fps), p99, p99.9 | 0.096 (10,452), 0.281, 1.82; 0.094 (10,628), 0.261, 1.79 | 0.468 (2,138), 0.833, 1.31 | 0.267 (3,743), 0.432, 0.62 |
| SuperTuxKart (GL), FIFO | 4.48 / 5.62 / 1; 4.46 / 5.22 / 0 | 4.50 / 5.19 / 0 | 4.44 / 4.90 / 0 |
| vkcube in gamescope (guest only) | -- | 4.54 / 5.26 / 0 | 4.37 / 4.43 / 0 |
| vkcube, FIFO, host loaded | 4.87 / 8.47 / 96; 5.73 / 8.54 / 121 | 5.68 / 8.44 / 88 | 5.11 / 8.46 / 72 |
| SuperTuxKart, host loaded | 4.86 / 6.03 / 7; 4.90 / 6.03 / 8 | 5.20 / 6.60 / 38 | 4.72 / 5.88 / 5 |

On an idle host the guest already paced as natively before this work; what
it paid was per-frame cost, which a mailbox (uncapped) vkcube shows: 0.37 ms
a frame more than natively, now 0.17 ms (the guest's three runs averaged
0.22 to 0.30 ms a frame). With the host loaded, the guest missed five times as many vblanks
as native SuperTuxKart; now fewer. The "before" loaded rows ran the same
code as `b693254` with the counters added.

**Where the time went.** A presented frame took 17 round trips to the
backend (11 IOCTL2 -- the syncobj and semaphore-surface calls of explicit
sync -- two fence WATCHes, two CLOSEs, two Wayland messages), each about
15 µs from the guest's side, and RM posted 55-85 events a frame on the
guest's device files, each forwarded as a record and most as a guest
interrupt (18,000 a second under a mailbox vkcube), for descriptors no guest
thread ever polled. The guest also kicked the event queue for every batch it
handed back, and the queue thread slept between the requests of one
present. Syncobj waits were not it: they were ready at the first poll or
woken by their registration; none reached the registration caps
(SECURITY.md §17) or fell back to the polling backoff, and the pump's 1 ms
sweep found almost nothing. Nor was the present path: FIFO pacing on an idle
host matched native, and so did gamescope's.

**Under a host load**, the difference is scheduling: a vCPU, the queue thread
or the event pump that wakes waits behind the host's ~3 ms EEVDF slice,
and a frame's dozen of such wakeups make a missed vblank likely where the
native game's one thread rarely misses. With the load confined to the other
CCD the guest paced as natively (SuperTuxKart: no missed vblank in 14,400
frames in the guest, one natively); with it everywhere, it missed 38 against native's 7.

**What changed** (each measured separately, rig/TESTING-RIG.md):

| change | where | effect |
|---|---|---|
| armed RM readiness | protocol (`GCAP_ARMS_READY`), backend pump, guest poll | the per-event records go (86,000 a second under a mailbox vkcube, not sent); throughput about the same |
| `--queue-poll-us 50` (default) | backend | the ring is found busy for 78 % of requests: mailbox vkcube 2,300 -> 3,000 fps |
| reply spin, 20 µs (`rt_spin_us`) | guest | with the poll, 3,000 -> 4,400 fps; with arming and the poll, a FIFO vkcube's VM threads took 64 % of a core instead of 89 % (the vCPUs no longer halt-poll between replies) |
| fence WATCH from a work item (`async_fence_watch`) | guest | two round trips a frame off the presenting thread (4,400 -> about 4,500 fps, two runs) |
| no event-queue kicks while it holds buffers | backend | 7,000 guest exits a second fewer under a mailbox vkcube |
| 100 µs EEVDF slice (`NVGPU_SLICE_US`) | launcher | under load, SuperTuxKart's missed vblanks 22 -> 2 in 14,400 (native 7), p99 5.08 -> 4.77 ms |

**Recommended configuration for a VM that runs games:**

- The defaults of this branch: a 100 µs EEVDF slice for the VMM's and the
  backend's threads (the backend sets its own, `--sched-slice-us 100`; the
  rig launcher runs both under `chrt --other --sched-runtime 100000 0`,
  `NVGPU_SLICE_US`; a VMM unit should put the same `chrt` in front of its
  `ExecStart`), the backend's `--queue-poll-us 50`, and the guest module's
  defaults (`rt_spin_us=20`, `arm_ready=1`, `async_fence_watch=1`).
- **Do not pin or confine the vCPUs on a host whose other work can land on
  the same CPUs.** Under the load above, `NVGPU_CPU_AFFINITY=8-15` (one CCD)
  missed 166 vblanks where the free placement missed 28, and one pinned CPU
  per vCPU (`NVGPU_VCPU_PINS`) missed 1,053 with p99 9 ms: a pinned vCPU
  cannot escape a busy CPU. Pin only CPUs set aside for the guest -- an
  isolated cpuset partition (root; nesbox's `vcpu_cgroup_fd`,
  `io_cgroup_fd`), then `vcpu_pins` and `dedicated` -- which the confined-load
  runs stand in for: there the guest paced as natively.
- **Guest RAM on huge pages is already the case under nesbox**: it prefaults
  guest RAM and collapses it into THP (`ShmemPmdMapped` covered all 4 GiB in
  every run). `NVGPU_PREFAULT=0` brought 10 ms first-touch stalls back.
  crosvm's `--hugepages` (`NVGPU_HUGEPAGES=transparent`) made no measurable
  difference.
- **crosvm**: its default per-vCPU core scheduling cost the most of any
  setting tried -- under the load, SuperTuxKart missed 178 vblanks against
  5 with `--core-scheduling=false` (`NVGPU_CROSVM_CORE_SCHED=0`), and on an
  idle host its mailbox p99 was 1.4 ms against 0.44. It is a side-channel
  mitigation between the guest and host tasks on SMT siblings
  (SECURITY.md §20): turn it off only on a single-tenant desktop. Even so
  crosvm ran a mailbox vkcube at about 3,300 fps to nesbox's 4,500.
- **vCPUs**: 2, 4 and 8 paced the same for these workloads; give a game what
  it uses in parallel, no more.
- **Direct scanout** (Hyprland `render:direct_scanout = 1`) scanned the
  fullscreen guest window out directly in every run and removed the FIFO
  vkcube's remaining missed vblanks (33 -> 0 in 9,600 frames; native 10 -> 4).
- Fullscreen, and FIFO or mailbox as the game prefers: both pace as natively
  now on an idle host; mailbox's cost per frame is what the table's
  mailbox row shows.

**What remains.** A frame still makes about 17 round trips, two of them now
off the presenting thread (the explicit-sync plumbing is the application's
own ioctls, each one the host's to run), and a mailbox vkcube's frame costs
about 0.17 ms more than natively. Under a
host load the guest now paces better than a native game that keeps the
default slice (give the native game the same slice and the comparison
would even out); isolation, not placement, is what removes the rest. The
fence signal-to-queue latency the backend reports includes nvidia-drm's own
timer-driven semaphore-surface signalling, the same natively. Not measured:
vkmark, native gamescope (it cannot start inside the Claude sandbox), a
120 or 165 Hz monitor, and games heavier than SuperTuxKart.

## Capture injection

A guest application's screen share, zero-copy (ARCHITECTURE.md §17,
SECURITY.md §18). The backend provides one primitive: a host buffer made a
guest dma-buf. The two programs around it are the integrator's: a **capture
helper** on the host, one per VM, and a **capture daemon** in the guest. The
rig's `rig/rig-tools/nvgpu-inject-test.c` and
`rig/guest-image/tools/nvgpu-capture-import.c` are the reference for each.

**Host side.** Each VM that shares screens gets a helper user of its own
(never the backend's, the VMM's, or the desktop user's uid, and never one
shared with another VM: whoever has a VM's helper uid can inject into it).
In `/etc/virtio-nvgpu/vmN.env`:

```sh
NVGPU_BACKEND_ARGS="--inject-socket /run/nvgpu/vmN/inject.sock --inject-uid 950"
NVGPU_INJECT_GROUP=nvgpu-cap0     # the helper's group: the socket is opened to it
```

or, on NixOS, `services.virtio-nvgpu.vms."N".inject = { enable = true;
helperUid = 950; helperGroup = "nvgpu-cap0"; }`. The backend binds the socket
0600 before its sandbox; the unit's `nvgpu-socket-open` makes it 0660 in
that group once it exists. The helper then runs as that user, in that
group, with the portal and PipeWire of the desktop session it serves.

**The helper's side of the socket** (`protocol/src/inject.rs` is
normative): `AF_UNIX`, `SOCK_SEQPACKET`, one request per packet, each
answered in order by one 48-byte reply `{u32 op, i32 status, u32 id, u32
version, u8 token[16], u32 max_buffers, u32 max_syncobjs, u64 max_bytes}`;
all integers little-endian.

| request | bytes | descriptors | reply |
|---|---|---|---|
| HELLO `{1, version=1, flags=0, 0}` | 16 | none | `version`, the VM's bounds (32 buffers, 16 syncobjs, 1 GiB) |
| IMPORT `{2, nplanes, width, height, drm_fourcc, flags, u64 modifier, u32 offsets[4], u32 strides[4]}` | 64 | one dma-buf per plane (`spa_data[i].fd`), in plane order | `id`, `token`; or -EBADF (not a dma-buf), -ENODEV (not this GPU's nvidia-drm memory), -EINVAL (layout, format, planes of two objects), -ENOSPC, -EDQUOT |
| IMPORT_SYNCOBJ `{4, flags=0}` | 8 | one syncobj file (`drmSyncobjHandleToFD`, no flags) | `id`, `token` of the syncobj |
| RELEASE `{3, id}` | 8 | none | 0, or -ENOENT for an id this connection did not import |

`flags` bit 0 is "rows bottom to top", carried to the guest. A malformed
packet ends the connection; closing it releases everything it imported.
The helper, in order:

1. HELLO once per connection.
2. When PipeWire adds a buffer to the stream (a DMA-BUF stream: the
   helper's format offer asks for `SPA_DATA_DmaBuf` with modifiers, as a
   GPU consumer's does), IMPORT it and keep `pw_buffer -> (id, token)`;
   send the daemon `(id, token)` and the reply's layout. RELEASE it when
   PipeWire removes the buffer.
3. On each frame: if the producer attaches `SPA_META_SyncTimeline`, wait
   for its acquire point on the host first (xdg-desktop-portal-hyprland
   1.4.1 attaches none: a buffer it queues is complete). Then tell the
   daemon `(id, sequence, timestamps, damage, crop, cursor)` and keep the
   buffer: do not give it back to the stream yet.
4. When the daemon says `done(id, sequence)` -- or after a timeout of a few
   frame periods, whatever the daemon does -- give the buffer back
   (`pw_stream_queue_buffer`), and signal the producer's release point if
   it has one. The guest never signals anything of the producer's.
5. On a new daemon connection, send every live `(id, token)` again.

With explicit sync instead of step 4's message: make a timeline syncobj on
the helper's render node, IMPORT_SYNCOBJ it once per stream and send the
daemon its `(id, token)`; signal point `2k-1` for frame `k` in step 3 and
wait for `2k` (with the same timeout) in step 4. Points the guest signals
are hints: early, late or never, they change only this stream.

**Tokens stay secret.** A token is what keeps one stream's buffers from
every other process that can open the guest node, and it works only while
no one else can read it. The helper sends it to the daemon over their own
channel, and neither writes it to a command line (a process's arguments,
and the kernel's, are readable by every user of the machine), an
environment variable a child inherits, a log, or a file anyone else can
read. The rig's capture test puts tokens on the guest's kernel command
line because it has no vsock; that is a test's shortcut, not a model.

**Guest side.** The node exists when the backend has `--inject-socket`:
`/dev/nvgpu-capture`, root:root 0660 (module parameter `capture_mode`, which
refuses anything for "other"). Give it to the daemon's account alone, with
`contrib/udev/70-nvgpu-capture.rules` and a group `nvgpu-capture`; never to
an application, which gets the daemon's dma-bufs through PipeWire.

**The daemon's side** (`driver/uapi/nvgpu_capture.h`):

- Open `/dev/nvgpu-capture` and a render node of the GPU,
  `/dev/dri/renderD128`, `O_RDWR`.
- For each `(id, token)` from the helper: `NVGPU_CAPTURE_IOC_OPEN
  {render_fd, id, token, flags=0}` returns a read-only dma-buf
  `dmabuf_fd` (close-on-exec) and `width, height, fourcc, modifier,
  nplanes, offsets[4], strides[4], buf_flags, size`; offer it to consumers
  as a PipeWire buffer of type `SPA_DATA_DmaBuf` with that modifier (every
  plane is `dmabuf_fd` at its offset). ENOENT is a wrong token or an id the
  helper has released.
- Queue a buffer to consumers when the helper announces its frame; tell the
  helper `done` once the consumers have returned it and their work on it
  has finished. Close the dma-buf when the helper releases the id and the
  consumers are done with it.
- With explicit sync: `NVGPU_CAPTURE_IOC_OPEN_SYNCOBJ {render_fd, id,
  token, flags=0}` returns a syncobj `handle` of the render file: wait for
  `2k-1` with `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT` (`WAIT_FOR_SUBMIT`) before
  queueing frame `k`, or hand consumers a `SPA_META_SyncTimeline` of it
  (`drmSyncobjHandleToFD`), and signal `2k`
  (`DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`) when they are done.
- The helper's timestamps are the host's `CLOCK_MONOTONIC`, not the
  guest's: stamp frames on arrival, or keep an offset of the daemon's own.

## Backend flags

`vhost-user-nvgpu --help` has each one's full text; `--diagnostic --help`
shows the diagnostic ones too.

| flag | default | what it does |
|---|---|---|
| `--socket PATH` | `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` | the vhost-user socket; whoever connects gets the guest's memory. A file already there is removed only if it is this uid's socket |
| `--allow-compute` | off | serve CUDA and other compute: `/dev/nvidia-uvm`, the UVM aperture, memory registered by its pages. Graphics, Vulkan Video and display need none of it |
| `--window-size MIB` | 1024 | the shared window: how much GPU memory the VM's processes can have CPU-mapped at once. A multiple of 64, at least 256; with `--allow-compute` at most 64512 (window and aperture share crosvm's 64 GiB region cap), else 65536; nesbox takes at most 32768. Refused at start otherwise ("Sizing the window") |
| `--window-owner-share PERCENT` | 50 | the percent of each window zone one guest process may hold, 1-95. From 88 one process can take a zone down to its reserve (SECURITY.md §19) |
| `--kms-card` | off | compositor-VM mode: offer the host's card nodes to the guest. Only for a host with no compositor of its own. Not run on hardware |
| `--wayland-socket PATH` | none | the host compositor's socket, for the Wayland proxy |
| `--wayland-lease` | off | offer the compositor's `wp_drm_lease_device_v1` (this GPU's card only) |
| `--wayland-lease-interval SECS` | 5 | average spacing of one VM's lease requests (3 at once; 0 lifts it) |
| `--wayland-export PATH` | none | accept host Wayland clients here for a guest compositor. Not run on hardware |
| `--wayland-max-conns N` | 64 | Wayland channels per VM |
| `--wayland-shm-budget MIB` | 1024 | wl_shm buffer memory per VM |
| `--wayland-queue-budget MIB` | 256 | unread compositor output per VM |
| `--queue-poll-us US` | 50 | how long the queue thread keeps looking at the control ring after draining it (0 to 1000): requests that arrive meanwhile cost the guest no kick ("Frame pacing") |
| `--sched-slice-us US` | 100 | every backend thread's EEVDF slice, set at start (100-100000; 0 keeps the host's ~3 ms): how soon the queue thread and the pump run again after waking on a busy host ("Frame pacing") |
| `--pacing-stats SECS` | none | log the frame-pacing counters every SECS while the guest is busy (also `NVGPU_PACING_STATS`); they are logged once at teardown regardless |
| `--inject-socket PATH` | none | accept screen-share buffers here from the VM's capture helper (with `--inject-uid`; "Capture injection") |
| `--inject-uid UID` | none | the only uid `--inject-socket` serves: the VM's capture helper |
| `--rm-allowlist enforce` | `enforce` | the RM allowlist; `log` is diagnostic |
| `--sandbox on` | `on` | the process sandbox; `best-effort` and `off` are diagnostic |
| `--diagnostic` | off | allow the diagnostic flags below (or `NVGPU_DIAGNOSTIC=1`) |

**Diagnostic flags**, each refused without `--diagnostic`, hidden from
`--help`, and announced as `DIAGNOSTIC: <flag>: <what it takes away>` on
stderr and in the log at every start. None is for running a tenant:

| flag | what it takes away |
|---|---|
| `--allow-root-unsafe` | the refusal to run as root or with `CAP_SYS_ADMIN`: every guest process would be an RM administrator |
| `--proc-nvidia PATH` | the host driver's `/proc/driver/nvidia` (a fixture tree, for tests) |
| `--permissive-abi` | the refusal of ioctls the ABI profile does not describe |
| `--keep-guest-coherency` | GPU-coherent guest system memory (the Intel PAT cover) |
| `--rm-allowlist log` | the RM allowlist: what it would refuse is logged and forwarded |
| `--sandbox best-effort`, `--sandbox off` | the refusal to run with a sandbox layer missing; all of the sandbox |
| `--allow-unmeasured-release` | the exact-measured-release rule |
| `--allow-inject-self` | the refusal of an `--inject-uid` that is the backend's own uid (every process of that user could inject into the VM); the rig's, which is one user |

`RUST_LOG` sets the log level (`warn` by default, which is what production
should keep: the start-up lines worth reading are warnings, and every call
site is rate-limited).

`nvgpu-userspace` stages the host's NVIDIA user-mode driver as a directory
for a guest to mount read-only: `--stage DIR` builds it (only into an empty
directory or one it staged before), `--caps utility,compute,graphics,video`
chooses what to carry, `--driver-version` pins a build, `--verbose` lists
every file, and `--manifest PATH` names the driver's
`sandboxutils-filelist.json` where it is not at
`/usr/share/nvidia/files.d/` (NixOS, or any distribution that does not
install NVIDIA's manifest there). `--allow-version-mismatch` stages a
manifest whose build is not the loaded module's, which a guest will then fail
against: only for reproducing that failure.

## The guest

**Kernel and module.** A Linux 7.2 x86-64 guest kernel with the options
[`scripts/build-guest-kernel.sh`](scripts/build-guest-kernel.sh) sets (it
builds the kernel and the module together), `CONFIG_RUST=y` among them, and
the guest module `virtio_gpu_nv.ko`, built from [`driver/`](driver/)
against it. On a kernel with Rust the module's parsers of guest-process
input are the Rust ones, the default; `NVGPU_RUST=0` builds the C ones
instead, for a guest kernel that cannot have Rust (`modinfo -F parsers`
says which, and a C module on a Rust kernel says so at load;
[`driver/README.md`](driver/README.md)). One virtio-gpu-nv device per
guest. Its parameters (`/sys/module/virtio_gpu_nv/parameters/`):

| parameter | mode | default | |
|---|---|---|---|
| `wl_mode` | `0444` | `0660` | mode of `/dev/nvgpu-wl*`, created `root:root`; anything for "other" is refused |
| `capture_mode` | `0444` | `0660` | mode of `/dev/nvgpu-capture*` (only with `--inject-socket`), created `root:root`; anything for "other" is refused |
| `virtio_id` | `0444` | `45` | the virtio device ID to bind (another only for a VMM that cannot express 45) |
| `arm_ready` | `0444` | `Y` | armed RM readiness, offered at HELLO ([Frame pacing](#frame-pacing)); `N` only to measure |
| `rt_spin_us` | `0644` | `20` | microseconds a caller spins for its reply before sleeping; `0` never spins ([Frame pacing](#frame-pacing)) |
| `async_fence_watch` | `0644` | `Y` | a fence proxy's WATCH from a work item ([Frame pacing](#frame-pacing)); `N` only to measure |
| `pacing` | `0400` | -- | read-only, root only: the frame-pacing counters |

The `0444` ones are set at load (on the kernel command line as
`virtio_gpu_nv.<name>=`, or with `modprobe`); root may change the `0644`
ones at run time, and there is no reason to outside a measurement.

**NVIDIA userspace.** The host's own release, exactly: in the image, or
mounted from a share `nvgpu-userspace` staged. The module makes the device
nodes (`/dev/nvidiactl`, `/dev/nvidia0...`, `/dev/nvidia-modeset`,
`/dev/nvidia-uvm` with compute, a DRM card and render node per GPU); render
nodes to group `render`, card nodes to `video`, as usual.

**32-bit clients** (Steam's client, 32-bit games' GL and Vulkan) need
nothing from the host beyond what 64-bit ones do: the module answers their
ioctls on every node, as nvidia.ko does, and takes DRM structs of the size
their older headers have (a 16-byte `drm_syncobj_handle`) as `drm_ioctl()`
does ([`driver/README.md`](driver/README.md)). The guest image needs a
kernel with `CONFIG_IA32_EMULATION` (the one `scripts/build-guest-kernel.sh`
builds has it) and NVIDIA's 32-bit userspace of the same release: on NixOS
`hardware.graphics.enable32Bit = true` with the NVIDIA package's `lib32`
in `extraPackages32`, which is what `/run/opengl-driver-32` then holds (the
rig image builds the same, `rig/guest-image/nix/nvidia.nix`). A 32-bit
CUDA context is not served (the toolkit dropped 32-bit applications with
CUDA 12): a 32-bit process cannot map a UVM semaphore pool, which lives
above 4 GiB -- so neither is 32-bit NVENC/NVDEC through a CUDA context.

**The Wayland daemon.** `nvgpu-wl-guest` runs in each session and serves
`$XDG_RUNTIME_DIR/wayland-0` to the session's applications. It alone may open
`/dev/nvgpu-wl`: every open is a client of the host's compositor, and the
opener can charge channels to any guest process. So the node's group,
`nvgpu-wl`, goes to the daemon and never to an application account:

```sh
groupadd --system nvgpu-wl
chgrp nvgpu-wl "$(command -v nvgpu-wl-guest)"
chmod 2755 "$(command -v nvgpu-wl-guest)"        # setgid nvgpu-wl: not dumpable
install -m 0644 contrib/udev/70-nvgpu-wl.rules /etc/udev/rules.d/
udevadm control --reload && udevadm trigger --subsystem-match=misc
```

`nvgpu-wl-guest --help` lists its flags: `--socket`, `--device` (another
`/dev/nvgpu-wl*`), `--card` (the guest card node leases are made as),
`--export NAME` and `--render` (export mode, and the render node host
clients' dma-bufs are imported into), and `--log` or `NVGPU_WL_LOG` for its
log level (`info` by default).

**What the image must contain**, then: the kernel and `virtio_gpu_nv.ko`
loaded at boot; the host's NVIDIA userspace (Vulkan ICD, EGL and GBM
vendors, `libcuda` and the video libraries if compute is served); the
`video`, `render` and `nvgpu-wl` groups and the udev rule; `nvgpu-wl-guest`
setgid `nvgpu-wl`, started per session with `XDG_RUNTIME_DIR` set; for
capture injection, the capture daemon's account in `nvgpu-capture` and its
udev rule; and applications run as unprivileged users in `video` and
`render` only.
[`rig/guest-image/`](rig/guest-image/) builds the test image, which runs every probe
as root by default and is not a model for a production image.

## Not supported

- **Driver releases** other than the six measured ones; guest userspace of a
  release other than the host's.
- **The compositor-VM and export modes, and hotplug**, until they have run
  on hardware.
- MIG and SR-IOV; `cudaMallocManaged` and full unified memory.
- RM's `EXPORT_TO_DMABUF_FD` (dma-buf export through RM: NVIDIA's GBM export
  through RM, CUDA's `cuMemGetHandleForAddressRange` with a dma-buf handle),
  IMEX sessions and fabric memory, nvidia-drm's
  `GEM_IMPORT_USERSPACE_MEMORY`, DRM `GEM_FLINK`/`GEM_OPEN`: refused.
- Vulkan ray tracing without `--allow-compute`: NVIDIA's driver lists the
  extensions but cannot create a device with them, natively as well; an app
  that enables every one it is offered fails in a graphics-only guest.
- Two CUDA processes whose UVM semaphore pools want the same host address:
  the second context fails with ENOMEM (ARCHITECTURE.md §18).
- crosvm with a virtiofs share (no read-only mode), and crosvm
  `--allow-compute` without patches `0007`-`0009`.
- More than four guests on one card has not been tried.
- Mutually untrusted tenants needing hardware isolation: this is
  attack-surface reduction, not an IOMMU boundary (SECURITY.md). Use VFIO or
  vGPU for that.

## What to alert on

In the backend's log (the unit's journal):

| line | meaning |
|---|---|
| `refusing to start: ...` | the backend would not serve: an unmeasured release, root, a sandbox layer missing, a flag without `--diagnostic`, RM not keeping clients to their file |
| `sandbox: DEGRADED: ...` | a sandbox layer is not in force (fatal unless a diagnostic flag allowed it) |
| `DIAGNOSTIC: <flag>: ...` | a protection is switched off: never in production |
| `... (NEAREST OLDER, unmeasured)` in the `host driver ...` line | running on tables not measured at this release |
| exit status 159, `sandbox: syscall N is not on the seccomp allowlist` | the seccomp filter stopped the backend: a code path the list lacks, or an exploit attempt; report N |
| killed by SIGABRT | a panic: the release build aborts rather than leave a queue unserved |
| `RM control ... refused: not in the allowlist of host release ...`, `RM class ... refused`, and the teardown's `RM allowlist ... refused` summary | a guest workload asked for an RM call outside the allowlist: a missing entry, or a probe |
| `handle table full`, `OS descriptor ... refused: over the ...` | a guest at a budget: one app starving others, or misbehaving |
| `Intel host: KVM ignores guest PAT ...` | display memory may be incoherent on this Intel host |
| `NVKMS: ...; refused`, `OPEN of ... refused`, `... refused (guestptr.rs)` | a guest asked for something the policy refuses: expected occasionally, a pattern is worth a look |
| `inject: refusing a connection from uid ...` | something other than the VM's capture helper reached its inject socket: the socket's group is wrong, or a probe |
| `inject: ... is not nvidia-drm (NVKMS) memory of this GPU; refused` | the helper got a stream from another device (a desktop on another GPU) or shared memory: that stream cannot be injected, only copied |
| `INJECT_OPEN of id ... refused` | a guest asked for an id without its token, or after the helper released it; many in a row are a guest guessing |

In the VMM's log: crosvm's `refused a memory request` (the main process
refused a mapping the frontend passed on), a `SIGSYS`/seccomp kill of a
jailed device process; nesbox's refusals of a window placement.

In the guest's `dmesg`: `is not write-back in this guest's MTRRs` (the VMM's
MTRRs, see README.md), and the module's refusals.

In the host's kernel log: `NVRM: Xid` (GPU faults; 79, 119, 120 or 154 mean
the GPU needs a reset), `NVRM: API mismatch` (a guest userspace of another
release), any oops or `WARN` naming nvidia, nvidia-drm or nvidia-modeset.
