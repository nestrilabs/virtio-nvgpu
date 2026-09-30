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

For the VMMs: crosvm's sandbox puts every device it emulates, and the nvgpu
frontend, in a minijail with a user namespace of its own, even as root
(`jail/src/helpers.rs`), so crosvm needs unprivileged user namespaces too,
and a crosvm unit must not set `RestrictNamespaces=`. nesbox's jailer needs
Linux 5.8 or later (it mounts a `/proc` of the jail's own with
`hidepid=invisible`, `virtio-nvgpu-v6`). Keep `vm.max_map_count` at its
default of 65,530 or above (NixOS sets 1,048,576): each VMM allows 16,384
window placements, which can split the window into about 32,769 mappings,
and a VMM near the limit stops its VM on a placement rather than leave a hole
in the window.

**The NVIDIA driver.** NVIDIA's **open** kernel modules, at a release the
backend's tables were measured at (below), with **`nvidia_drm.modeset=1`**:
NVKMS is how a buffer becomes shareable and what the display paths and
nvidia-drm's semaphore-surface fences go through. For `--allow-compute`,
`nvidia_uvm` loaded at boot: the unit's `ProtectKernelModules=` and the
backend's `no_new_privs` keep it from being loaded on demand. The host RM
must keep each client to the file it was made on (the default): the backend
asks at start and refuses to run otherwise (SECURITY.md, R3).

**The exact-measured-release rule.** The backend starts only on a host driver
release every one of its tables was measured at (`device/src/release.rs`):
the RM allowlist and the NVKMS schema of that very release, a UVM table whose
range holds it, and an ABI profile measured through it. Today that is
**535.129.03, 580.178.04, 595.71.05, 595.99.02, 610.57.04 and 615.71.09**:

| host release | RM allowlist, NVKMS schema | ABI profile | UVM table |
|---|---|---|---|
| `535.129.03` | its own | `535.129.03` | its own |
| `580.178.04` | its own | `580.178.04` | its own |
| `595.71.05` | its own | `595.71.05` | its own |
| `595.99.02` | its own | `595.71.05` | its own |
| `610.57.04` | its own | `595.71.05` | its own |
| `615.71.09` | its own | `595.71.05` (measured through 615.71.09) | its own |

Any other release is refused at start with a line naming what it lacks; a
host older than 535.129.03 is refused whatever the flags.
`--allow-unmeasured-release` (diagnostic) runs a newer or in-between host on
the nearest older tables without compute; it is for getting through an
upgrade, not for running tenants. The guest's NVIDIA userspace must be the
host's own release, exactly, as it must natively.

**IOMMU.** The GPU stays with the host's driver, in the host's IOMMU domain;
the guest gets the driver's interface, not the device, so the IOMMU is not a
boundary between a guest and the host here (SECURITY.md, "Threat model"). Leaving DMA
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

1. Measure the release from its published sources: `gen/README.md`, "A new
   host release", has the extractors to run and when to move
   `MEASURED_THROUGH` (or add a profile, `gen/nvabi_gen.py`).
2. Run `scripts/gen-check.sh` (network access; `GVISOR=<checkout>` to include
   the ABI profiles): every checked-in table must still be what its extractor
   produces.
3. `cargo test --workspace --features device/vhost-user`; then the guest
   side: rebuild the guest images with the new release's userspace.
4. Run the new release on a test host through the application workloads you
   serve, with `--rm-allowlist=log` only there, and read the teardown's
   `RM allowlist ... refused` lines: a control the new release's userspace
   needs and the allowlist lacks is a policy decision (SECURITY.md, "The RM allowlist"), not
   something to allow by default.
5. Ship the backend and the guest images together, then upgrade the host
   driver and reboot. A backend that starts logs the tables it chose on one
   warning line (`host driver X: RM allowlist X, ABI profile ..., NVKMS schema
   X, UVM table X`); anything marked `NEAREST OLDER, unmeasured` means step 1
   was skipped.

## What is supported

| | nesbox | crosvm |
|---|---|---|
| where | [github.com/nestrilabs/nesbox](https://github.com/nestrilabs/nesbox), branch `virtio-nvgpu-v6` (not yet merged upstream; `v5` and later size the window from the backend and prefault it, `v6` adds the jailer's own `/proc` and `/sys`) with [`patches/nesbox/`](patches/nesbox/) `0001`, which no branch has yet: without it a guest's block queue can lose a completion's interrupt for good, and everything that reads or writes the disk through that CPU stops, unkillable (rig/TESTING-RIG.md, "Wine start-up stalls") | upstream crosvm `c0474109d64d` with [`patches/crosvm/`](patches/crosvm/) `0001`-`0011` (`0010` and `0011`, the window and guest RAM prefaults, are optional: performance only) |
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
mapping requests: SECURITY.md, "The VMMs".

A VMM other than these two must do what README.md, "What a VMM must do, over
vhost-user", lists.

## Per-VM users

Each VM's host processes are users of their own, so that no two VMs are one
principal to the kernel or to RM (SECURITY.md, "One uid per VM"). Slot N
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

On NixOS, [`nix/module.nix`](nix/module.nix) declares the pool, with fixed
ids (`uidBase`, 64000 by default: slot N's backend user and group are
64000+2N, its VMM user 64000+2N+1), and the groups, and installs the
socket units and the unit below as they are, for its package
([`nix/units.nix`](nix/units.nix); the root flake's `units` package is the
same), with what is the configuration's -- flags, `memoryMax`, `tasksMax`,
the Wayland socket, the inject socket's group -- in a drop-in per slot
(`services.virtio-nvgpu = { enable = true; package = ...; slots = 4; }`;
the root flake's `nixosModules.default` sets the package). Its flags are
its own: it does not read `/etc/virtio-nvgpu/vmN.env`. Its assertions
refuse a slot's group with anyone else in it, a pool user with other
groups, another user on a pool id, and a flag with whitespace, a quote, a
backslash or a `%` in it (systemd would split, unquote or expand it).
With `vms.<n>.vmm.kind` (`crosvm` or `nesbox`) and `vmmPackages` it runs the
VM's VMM too, from contrib/systemd's template, with the VMM's arguments
and its tuning settings ("Tuning") in a drop-in of its own instead of
`vmmN.env`.

A host that runs both these units and the rig's root launcher shares one
pool between them: the launcher skips a slot whose socket unit listens,
but a unit takes no launcher lock, so start one only for a slot no
launcher run holds.

**The backend.** [`contrib/systemd/vhost-user-nvgpu@.service`](contrib/systemd/vhost-user-nvgpu@.service),
instance N, runs `vhost-user-nvgpu` as `nvgpu-vmN` in a cgroup of its own
(`MemoryMax=2G`, `MemorySwapMax=0`, `TasksMax=256`, `OOMScoreAdjust=500`,
`LimitCORE=0`, `Restart=no`), with no capabilities, `NoNewPrivileges`, a
network namespace of its own, no namespaces of its making
(`RestrictNamespaces=yes`), the file system read-only
(`ProtectSystem=strict`, `ProtectHome=yes`) and nothing executable but
itself and its libraries (`NoExecPaths=/`). Its socket is
[`vhost-user-nvgpu@.socket`](contrib/systemd/vhost-user-nvgpu@.socket)'s:
systemd binds `/run/nvgpu/vmN/nvgpu.sock`, root's and 0660 to the slot's
group, in a directory of root's, and hands it to the backend (socket
activation; the backend takes `LISTEN_FDS`, or `--socket-fd N`). The
backend's user owns neither the socket nor its directory, so nothing else
running as that user can replace the socket or intercept the VMM's
connection (SECURITY.md, "The backend's socket"). The service starts the socket unit
(`Requires=`); install both. Per-VM flags go in `/etc/virtio-nvgpu/vmN.env`
as `NVGPU_BACKEND_ARGS="--allow-compute"` (one word per flag: systemd
splits the variable at spaces). Raise `MemoryMax` with
`--wayland-shm-budget` and `--wayland-queue-budget`. The backend is never
restarted alone: the VM's device goes with it, so restart the VMM with it.

**The VMM**, as `nvgpu-vmmN`, with a unit that has
`BindsTo=vhost-user-nvgpu@N.service` and `After=vhost-user-nvgpu@N.service`
and a network namespace of its own. `BindsTo=`, not `Requires=`: a backend
that stops on its own -- a panic, a seccomp kill (exit 159), the OOM killer
-- does not stop a unit that only requires it, and the VM would run on with
a dead device. [`contrib/systemd/nvgpu-vmm-nesbox@.service`](contrib/systemd/nvgpu-vmm-nesbox@.service)
and [`nvgpu-vmm-crosvm@.service`](contrib/systemd/nvgpu-vmm-crosvm@.service)
are such units, templates that have passed `systemd-analyze verify` and not
yet run on hardware:

- nesbox, a config with `"gpu-forward": { "socket": "/run/nvgpu/vmN/nvgpu.sock" }`
  and `"unshare-network": false`, under nesbox's jailer, in the unit's
  network namespace (`PrivateNetwork=yes`). The jailer chroots, and the
  kernel refuses a user namespace to a chrooted process, so nesbox's own
  `"unshare-network": true` cannot start under it (it exits with EPERM): it
  is for unjailed runs only. The unit starts the jailer as root (it becomes
  `nvgpu-vmmN` itself), and the jailer clears supplementary groups, so
  `/dev/kvm` must be 0666 (systemd's default rule);
- crosvm, `crosvm run ... --vhost-user type=nvgpu,socket=/run/nvgpu/vmN/nvgpu.sock,max-queue-size=256 --no-pci-hotplug-port`,
  with its sandbox on (the default) and a `--pivot-root` directory that
  exists and is empty. crosvm publishes the UVM aperture when the backend
  reports it (`--allow-compute`). Its main process accepts the device's
  queue-notification ioevents only at the addresses BAR0 had when the BARs
  were laid out: a guest kernel that reassigns BARs before the driver binds
  (`pci=realloc`, or a resource conflict) has them refused, and the device
  is marked NEEDS_RESET. That fails closed, and Linux keeps
  firmware-assigned BARs by default: do not boot a guest with
  `pci=realloc`.

Both need `/dev/kvm` for the VMM user, and both take the shared window's
size from the backend (GET_SHMEM_CONFIG): crosvm always has, nesbox from
branch `virtio-nvgpu-v4` (an older nesbox publishes 1 GiB whatever the
backend's `--window-size` says, and every placement past it fails). Both
prefault what they place in the window -- nesbox from branch
`virtio-nvgpu-v5`, crosvm with `patches/crosvm/0010` -- on a host kernel
with `KVM_PRE_FAULT_MEMORY` (6.11 or later): without it a guest's first
write to fresh video memory runs at about a tenth of the host's speed, one
second-level fault a page (BENCHMARKS.md; SECURITY.md, "Prefaulting the window"). Guest
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
```

and the flags in the VM's environment file, beside its others -- not in the
drop-in: systemd lets an `EnvironmentFile=` override `Environment=`, so a
drop-in's flags are lost the moment the VM has a `vm0.env`:

```sh
# /etc/virtio-nvgpu/vm0.env
NVGPU_BACKEND_ARGS="--allow-compute"
NVGPU_WAYLAND_ARGS="--wayland-socket /run/user/1000/wayland-1 --wayland-lease"
```

On NixOS, `services.virtio-nvgpu.vms."0".wayland = { socket =
"/run/user/1000/wayland-1"; lease = true; }` does all three.

The compositor makes its socket anew each session, so the ACL goes with
it: set it from the session's own start-up. The rig's launcher instead runs
the backend as the socket's owner (the
desktop user), which puts a compromised backend one step from the desktop
session; the per-VM user above avoids that. `--wayland-lease` needs the
patched Hyprland and a monitor marked `leasable` ([`patches/`](patches/)).

**The lease, on the compositor's side.** Three knobs of the patched
Hyprland, all off by default, which leave a `leasable` monitor as it was:
any client of the compositor that sees the lease device may lease it, as
often as it likes, until it lets go. Their full text is in
[`patches/README.md`](patches/README.md), "Who may lease it, how often, and
taking it back"; what each exposes is in SECURITY.md, "The host desktop".

| monitor rule key or dispatcher | default | gain | cost |
|---|---|---|---|
| `lessee = USER` | any client | only the VM's backend (its per-VM user) is offered the monitor or may lease it; the desktop's own clients never see it | none, with a per-VM user; with the rig's launcher (the backend as the desktop user) it separates nothing |
| `lease_interval = MS` | 0 | a monitor is released (a blocking modeset) at most once per interval, whoever asks | a lessee that lets go and asks again sooner is refused, and has to ask later |
| `revokelease MONITOR` / `hl.dsp.revoke_lease` | none | the host takes a leased monitor back without stopping the VM | none: the guest sees its display go, as on an unplug |

The recommended rule for a VM's monitor, with the backend running as
`nvgpu-vm0`:

```lua
hl.monitor({ output = "DP-2", disabled = true, leasable = true, lessee = "nvgpu-vm0", lease_interval = 2000 })
```

```ini
# hyprland.conf (0.56.2)
monitor = DP-2, disable, leasable, 1, lessee, nvgpu-vm0, lease_interval, 2000
```

`disabled = true` keeps the desktop off the monitor, so a lease or its end
never moves windows. With 2 s a guest compositor that restarts leases again
at once, and a client that asks in a loop gets the monitor released once
every 2 s at most; a second request within 2 s of the first is refused, and
the guest has to ask again. The backend's own limit
(`--wayland-lease-interval`) is per VM and holds only the VM; this one is
per monitor and holds every client. To take the monitor back and keep it,
withdraw the offer, then revoke:

```sh
hyprctl eval 'hl.monitor({ output = "DP-2", leasable = false }); hl.dispatch(hl.dsp.revoke_lease({ monitor = "DP-2" }))'
# hyprlang: hyprctl keyword monitor DP-2, disable, leasable, 0 && hyprctl dispatch revokelease DP-2
```

`hyprctl monitors all` shows `lessee` (a uid, `any`, or `nobody` for a user
name that did not resolve) and `leaseInterval` for each monitor. Hyprland's
`permission` rules cannot stand in for `lessee`: they match a client by its
executable, which the compositor cannot read for the backend (it is
undumpable, and a per-VM user besides).
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
given more with `--window-size` and `--window-owner-share` (on NixOS,
`services.virtio-nvgpu.vms."N".windowMiB` and `.windowOwnerShare`).

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
| Blender EEVEE, GL, heavy scene | 0.1 | 1952.0 | 90.8 | 72.1 |
| Blender EEVEE, Vulkan, heavy scene | 0.1 | 994.5 | 89.7 | 256 |

The heavy scenes are BENCHMARKS.md's ("Heavy workloads": 150 subdivided
meshes, ray tracing and volumetrics, 64 samples at 1920x1080), with a 16 GiB
window at 90%; at the default window Blender is refused its mappings
(`SHM WriteCombine zone: guest process ... may not take`) and segfaults.

UC never passed half a MiB, so it does not grow. Every application here used
more write-back than write-combining, and Blender's 86 MiB is three quarters
of a process's default WB share, so WB keeps its proportion rather than
staying put; WC takes most of the growth because it has to hold its own
and WB's overflow, and because the one workload known to exhaust a default
window did it in WC (a Minecraft launcher on this GPU, in part through a
backend defect since fixed, SECURITY.md, "The window's size and share"). On an older T4, CUDA peaked
at 68 MiB and an NVENC encode at 116 MiB, before system memory was told
apart from video memory.

**Choosing it.** Keep the default unless the backend logs `SHM alloc
failed` for a VM, or its `window use:` line shows a process near its share
(`by one process` against half the zone). Then raise `--window-size` in
steps of 64 MiB; `--window-owner-share` above 50 lets one process have
most of each zone, which suits a VM that runs one application and takes
from the VM's other processes the chance to map much at once
(SECURITY.md, "The window's size and share": from 88 %, one process can leave the others only the
reserve).

**A VM for creative applications** (Blender, a 3D editor, a large scene
in an engine's editor) wants `--window-size 8192` at the default share:
a WC zone of 6,318 MiB, of which one process may hold 3,159 -- the heavy
scene above needs 1,952 -- and a WB zone of 1,842. Blender's GL and
Vulkan backends both ran the heavy scene at 8192 and 50% (BENCHMARKS.md,
"Heavy workloads"); 4096 at 50% (1,574 MiB of WC a process) is too small
for its GL backend. The launcher's `NVGPU_WINDOW_PRESET=creative` and the
module's `windowPreset = "creative"` give this size. The default stays at
1 GiB because a window's size is
also what one VM may take from the host: up to its WC zone of the GPU's
BAR1, shared with the desktop and every other VM (6.2 GiB of an RTX 5090's
32 at 8192), and under crosvm up to the whole window in host memory (below).
A games VM uses a few tens of MiB of it.

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
  its own overhead). Guest RAM itself is committed whole as the VM starts
  under both VMMs as the launcher runs them (nesbox's prefault, crosvm's
  `--prefault-memory`, `patches/crosvm/0011`), not as the guest first
  touches it; `NVGPU_PREFAULT=0` turns that off for either, at the cost of
  frame-time stalls in a guest that reaches new memory long after boot.
- *Address space.* crosvm puts the window and the UVM aperture in one BAR,
  the next power of two above both (16 GiB plus the aperture is a 32 GiB
  BAR) and refuses past 64 GiB; nesbox puts each in a BAR of its own in a
  64 GiB MMIO window and refuses a window past 32 GiB. Both refuse at start,
  with the size named.

## Video memory limit

Without a limit a VM allocates as much of the GPU's video memory as RM
gives it: the window bounds only what it has CPU-mapped at once, so one VM
can take all of it, and the host's desktop and the other VMs then fail
their allocations. `--vram-limit MIB` holds the VM to MIB (on NixOS
`services.virtio-nvgpu.vms."N".vramLimitMiB`; the rig's launcher,
`NVGPU_VRAM_LIMIT`). Off by default, and then nothing below applies.

- **What it does.** Video memory the guest allocates by name -- what
  `vkAllocateMemory` in a device-local heap, `cuMemAlloc` and GL's buffers
  and textures come down to -- is counted at the size RM gives it, and an
  allocation past the limit is refused before RM sees it, as RM refuses one
  it cannot back: Vulkan returns `VK_ERROR_OUT_OF_DEVICE_MEMORY`, CUDA
  `CUDA_ERROR_OUT_OF_MEMORY`, as on a smaller GPU. One guest process holds
  at most `--window-owner-share` percent of the limit (half by default),
  with the last eighth kept for processes that hold little, as in the
  window; a VM for one application wants the share raised with it.
- **What the guest sees.** nvidia-smi's "FB Memory Usage" (Total the limit,
  Reserved 0, Used what the VM holds, Free the rest), NVML, Vulkan's
  device-local heap and its `VK_EXT_memory_budget` budget, and CUDA's
  `cuDeviceTotalMem` and `cuMemGetInfo` all say the limit, and games that
  size their streaming from the budget size it to the VM. Free is never
  more than the host has free. A limit above the GPU's memory shows the
  GPU's.
- **What it gains.** One VM can no longer exhaust the GPU's memory for the
  host and the other VMs by allocating it (SECURITY.md, "Video memory
  limit").
- **What it costs.** A guest that asks for more than its limit fails where
  it would have run. Memory RM allocates on the VM's behalf -- channels'
  and contexts' buffers, page tables, GSP's -- and what nvidia-uvm migrates
  for CUDA managed memory are not counted, nor nvidia-drm's own
  allocations (`GEM_ALLOC_NVKMS_MEMORY`, dumb buffers), nor memory a GEM
  object or an NVKMS surface keeps alive after the VM freed its last RM
  handle of it. So a limit holds a VM's ordinary workload to its budget
  but is not a hard partition against a hostile guest (SECURITY.md, "Video
  memory limit"): leave headroom between the sum of the limits and the
  GPU's memory. With a limit, each RM control's parameters
  are copied once more in the backend to be looked at.
- **Choosing it.** The backend logs, when the VM stops, `video memory: at
  most X of Y MiB held, Z MiB by one process; ...`, and each refusal as
  `video memory: N bytes for guest process ... refused`. On the rig (RTX
  5090, 595.99.02) a CUDA context alone holds 498 MiB before its first
  `cuMemAlloc`; the peaks the applications reached are in rig/TESTING-RIG.md,
  "Video memory limit".
- **Where it runs.** Only on a host release gen/ measured exactly
  (`gen/vidmem_extract.py`): the replies it rewrites move between releases,
  and the backend refuses to start with a limit on any other.

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
(SECURITY.md, "Resource caps") or fell back to the polling backoff, and the pump's 1 ms
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
  `ExecStart`, as contrib/systemd's templates do), the backend's `--queue-poll-us 50`, and the guest module's
  defaults (`rt_spin_us=20`, `arm_ready=1`, `async_fence_watch=1`).
- **Do not pin or confine the vCPUs on a host whose other work can land on
  the same CPUs.** Under the load above, `NVGPU_CPU_AFFINITY=8-15` (one CCD)
  missed 166 vblanks where the free placement missed 28, and one pinned CPU
  per vCPU (`NVGPU_VCPU_PINS`) missed 1,053 with p99 9 ms: a pinned vCPU
  cannot escape a busy CPU. Pin only CPUs set aside for the guest -- an
  isolated cpuset partition (root; nesbox's `vcpu_cgroup_fd`,
  `io_cgroup_fd`), then `vcpu_pins` and `dedicated` -- which the confined-load
  runs stand in for: there the guest paced as natively. The topology-aware
  layouts, and what they gain and cost with the desktop's load on the
  other CCD, are in "vCPU placement".
- **Guest RAM prefaulted and on huge pages**: nesbox prefaults guest RAM
  and collapses it into THP by default (`ShmemPmdMapped` covered all 4 GiB
  in every run), and crosvm does with `--prefault-memory`
  (`patches/crosvm/0011`), which the launcher passes. `NVGPU_PREFAULT=0`
  brings the first-touch stalls back: 10 ms frames under nesbox, and under
  crosvm bursts of 20-40 ms frames long after boot that halved a 4K game's
  0.1% low (BENCHMARKS.md, "Heavy workloads"). crosvm's `--hugepages`
  (`NVGPU_HUGEPAGES=transparent`) alone makes no difference: a memfd's huge
  pages are the shmem policy's.
- **crosvm**: its default per-vCPU core scheduling cost the most of any
  setting tried -- under the load, SuperTuxKart missed 178 vblanks against
  5 with `--core-scheduling=false` (`NVGPU_CORE_SCHED=off`), and on an
  idle host its mailbox p99 was 1.4 ms against 0.44. It is a side-channel
  mitigation between the guest and host tasks on SMT siblings
  (SECURITY.md, "The tuning knobs"): turn it off only on a single-tenant
  desktop, or share one cookie between the VM and its backend
  (`NVGPU_CORE_SCHED=shared`, "Tuning"), which keeps every other task off
  the guest's cores. Even so crosvm ran a mailbox vkcube at about 3,300 fps
  to nesbox's 4,500.
- **vCPUs**: 2, 4 and 8 paced the same for these workloads, but a game's
  throughput is the vCPUs it can use: a job-system engine ran 58 fps on 4,
  99 on 8 and 158 on 16, 7-10% under the same program on as many host CPUs,
  and a 4-vCPU guest is within a few percent of the program confined to 4
  host CPUs (BENCHMARKS.md, "Steam-like games"). Give a modern game 8 or
  more; a guest's vCPUs are host CPUs other work cannot have while the game
  runs.
- **The guest kernel**: transparent huge pages (`always`) and ntsync, as
  `scripts/build-guest-kernel.sh` now sets them: without huge pages a
  game's heap is all 4 KiB pages (10% in the job-system loop), and without
  `/dev/ntsync` Wine and Proton fall back to slower synchronisation (11%
  in a D3D11 game under Wine's own).
- **Guest haltpoll** (`cpuidle_haltpoll.force=1` on the guest's command
  line) makes a wakeup between vCPUs faster than natively (0.95 µs against
  7.6 without it), but a polling vCPU runs no game thread: a D3D11 game
  under Wine lost 9%. Not recommended by default.
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

## Tuning

Each setting here trades something -- host CPU, host power, memory, or a
side-channel mitigation -- for frame rate or latency. Every one keeps
today's behaviour unless it is set; what each gives up, and what still
holds when it is on, is SECURITY.md's ("The tuning knobs"). The same
setting has one name at each layer:

| setting | backend or VMM | launcher (`rig/run-guest.sh`) | units (`/etc/virtio-nvgpu/`) | NixOS (`vms.<n>.`) | default |
|---|---|---|---|---|---|
| core scheduling | crosvm `--core-scheduling=false`, `--per-vm-core-scheduling`; `coresched` | `NVGPU_CORE_SCHED` | `NVGPU_CORE_SCHED` in `vmmN.env` | `vmm.coreScheduling` | crosvm `per-vcpu`, nesbox `off` |
| the queue thread's poll | `--queue-poll-us` | `NVGPU_QUEUE_POLL_US` | `NVGPU_BACKEND_ARGS` in `vmN.env` | `queuePollUs` | 50 |
| the backend's CPU quota | -- | -- | `CPUQuota=` in a drop-in | `cpuQuota` | none |
| the EEVDF slice | `--sched-slice-us`; `chrt` for the VMM | `NVGPU_SLICE_US` | `NVGPU_SLICE_US` in `vmmN.env`, `--sched-slice-us` in `vmN.env` | `sliceUs` | 100 |
| the window for creative applications | `--window-size 8192` | `NVGPU_WINDOW_PRESET=creative` | `NVGPU_BACKEND_ARGS` | `windowPreset = "creative"` | 1024 ("Sizing the window") |
| guest RAM prefaulted | crosvm `--prefault-memory`; nesbox's `"prefault"` | `NVGPU_PREFAULT` | `NVGPU_PREFAULT` in `vmmN.env` (crosvm) | `vmm.prefaultMemory` | on |
| the VMM's file-size limit | `RLIMIT_FSIZE` | `NVGPU_VMM_FSIZE_MIB` | `LimitFSIZE=` in a drop-in | `vmm.limitFSizeMiB` | none |
| a host C-state cap | `/dev/cpu_dma_latency` | `NVGPU_CPU_LATENCY_US` (root) | `nvgpu-cpu-latency@US.service` | `vmm.cpuLatencyUs` | none |
| channel-disable rates | `--fifo-disable-{proc,vm}-{rate,burst}` | `NVGPU_FIFO_DISABLE_RATES` | `NVGPU_BACKEND_ARGS` | `fifoDisable.*` | 50/40, 200/160 |
| the guest's knobs | its kernel command line | `NVGPU_GUEST_*` | the VMM's `-p` / config | the VMM's arguments | "The guest" |
| the Wayland daemon's surface buffers | `nvgpu-wl-guest --surface-buffers` | -- | -- | -- | 16 ("The guest") |

The launcher's header lists its variables; `nix/module.nix` its options,
whose assertions refuse what the backend or VMM would. The VMM units apply
theirs through `contrib/systemd/nvgpu-vmm-exec` as they exec the VMM. With
`vms.<n>.vmm.kind` (and `vmmPackages`) the NixOS module runs the VMM's
unit itself, its settings in a drop-in, and `vmmN.env` is not read.

**Core scheduling** (crosvm, and nesbox through `coresched`). Which host
tasks may run on the SMT sibling of a CPU running a guest vCPU:

- `per-vcpu`, crosvm's default: a core-scheduling cookie per vCPU thread.
  Nothing else -- not another vCPU of the same VM, not the backend thread
  answering it -- shares the core while a vCPU runs, so the sibling idles.
- `vm`: one cookie for the whole VMM (crosvm `--per-vm-core-scheduling`;
  nesbox under `coresched new`). The VM's own threads may share a core;
  the backend and everything else may not.
- `shared`: one cookie for the VMM and its backend, made before the guest runs
  (the launcher starts both from a process holding it; the units exec the
  VMM with a new one, and give it to the backend as root once the VMM has
  it -- until then the backend has none, as without the setting). The
  guest never runs outside it, and the backend's thread answering a vCPU
  may run on that vCPU's sibling.
- `off`, nesbox's default (nesbox sets no cookie): the host scheduler
  places every thread anywhere.

The rig (Ryzen 9 9950X; MDS, L1TF, TAA, MMIO and RFDS not affected, STIBP
on) measured, under crosvm, three runs each, interleaved (BENCHMARKS.md,
"crosvm's core scheduling, by mode"):

| workload (crosvm, 4 vCPUs, 8 GiB) | `per-vcpu` | `shared` | `off` |
|---|---|---|---|
| SuperTuxKart, Vulkan, 4K: average fps / 1% low | 984 / 255 | 1,110 / 359 | 1,130 / 452 |
| the same: an IOCTL2's round trip, from the guest | 13.0 µs | 7.9 µs | 7.4 µs |
| Unigine Heaven under Wine (D3D11 on DXVK): average fps / 1% low | 433 / 134 | 492 / 248 | 497 / 255 |
| `gameloop` (every vCPU busy with jobs): average fps / 1% low | 56.4 / 40.8 | 57.1 / 47.9 | 57.8 / 48.5 |

`shared` recovered 86-92% of what `per-vcpu` costs the average frame rate
and 91% of its round trip, and most of its 1% lows; the backend's CPU was
the same in all three.

`per-vcpu` stays the default. `shared` keeps every other VM and host task
off the guest's cores, as `per-vcpu` does, and lets only this VM's own
backend -- which holds only this VM's state -- onto them; take it where
the frame rate matters and the host runs more than one tenant. `off` on a
single-tenant desktop.

**The queue thread's poll** (`--queue-poll-us`, default 50). After
draining the control ring the backend's queue thread keeps looking at it
this long, so a request that arrives meanwhile costs the guest no kick
and the thread no wakeup: +30% on a mailbox vkcube. The price is up to a
host core at 100% for a guest that sends every 50 µs -- about ten times
the CPU the guest itself spends sending (about 40% of a core in the
heavy workloads). On a host with many VMs, 10 or 0; or bound a VM's
backend with a CPU quota (the unit's `CPUQuota=`, NixOS `cpuQuota`):
held below what it needs, a backend answers its guest late, so size the
quota from the backend's CPU in `rig/rig-heavy.sh`'s `.cpu` lines, not
below it.

**The EEVDF slice** (`--sched-slice-us`, `NVGPU_SLICE_US`, default 100).
Every backend and VMM thread runs with a 100 µs slice instead of the
host's ~3 ms, at the same weight: on a loaded host a woken vCPU or queue
thread gets a CPU back sooner. Under a 32-worker host load SuperTuxKart's
missed vblanks went from 22 to 2 in 14,400 frames. The host's other
threads see up to about 100 µs more wake-up jitter under load, and are
not starved. 0 keeps the host's slice; 100 to 100000 otherwise ("Frame
pacing" has the measurement).

**The window for creative applications**: "Sizing the window" (`creative`
is 8192 MiB: up to 6.2 GiB of a 32 GiB BAR1 per VM, and under crosvm up to
8 GiB of host memory in the VMM's cgroup).

**Guest RAM prefaulted** (default on). nesbox prefaults guest RAM and
collapses it into 2 MiB pages by default; crosvm does with
`--prefault-memory` (`patches/crosvm/0011`), which the launcher and the
crosvm unit pass. Off, a guest reaching memory it has not used since boot
stalls on host page faults: crosvm's 0.1% low went from 80 fps to 35 in a
4K game. On, all of guest RAM is committed as the VM starts, which undoes
a balloon or free-page reporting meant to give memory back early: turn it
off (`NVGPU_PREFAULT=0`, `vmm.prefaultMemory = false`, nesbox's
`"prefault": false`) for a VM whose RAM is overcommitted that way.

**The VMM's file-size limit** (`LimitFSIZE=`, none by default). A VMM its
guest has taken over can grow any file it can write -- the disk above all
-- with `ftruncate` or `fallocate` until the file system is full.
`RLIMIT_FSIZE` bounds that, and also every memfd the VMM sizes: it must be
at least the disk, guest RAM and the window (and, under the launcher, the
console log cap), or the VMM is killed (`SIGXFSZ`) as it starts or when
the guest writes its disk's last block. The launcher and the module refuse
one below any of them. Size it per deployment: the largest of those, plus
nothing.

**A host C-state cap** (none by default). An idle host CPU sleeps in a
deep C-state; waking from C3 took 350 µs on the rig, which a vCPU or queue
thread woken by an event pays. `NVGPU_CPU_LATENCY_US=N` (the root
launcher), `nvgpu-cpu-latency@N.service` wanted by a VMM unit, or
`vmm.cpuLatencyUs` holds `/dev/cpu_dma_latency` at N µs while the VM runs,
so no CPU enters a C-state slower to leave than that. The cost is the
host's idle power and heat, on every core, for as long as a VM that wants
it runs; the kernel takes the lowest of several requests. Not measured on
the rig (it needs root): start at 50 and compare frame times with and
without.

**Channel-disable rates** (`--fifo-disable-{proc,vm}-{rate,burst}`, default
50 a second per guest process after 40 at once, 200 per VM after 160).
`NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS` preempts the caller's channels off
the GPU and with them the runlist every VM shares; the rates are how often
one VM may make the others wait, and several times what any workload
measured makes. The backend refuses rates outside 1 to 1000, a process's
rate at or above the VM's, and a process's burst above the VM's less its
reserve of 40 or at most 8. Raise them only for a workload whose log shows
`FIFO_DISABLE_CHANNELS refused: ... over its rate`.

**The host's SRSO mitigation: a host kernel choice, not a knob here.** On
the rig's Zen 5 the kernel defaults `spec_rstack_overflow` to "IBPB on
VMEXIT only" (`/sys/devices/system/cpu/vulnerabilities/spec_rstack_overflow`),
and VMSCAPE's mitigation rides on it ("IBPB on VMEXIT"): every VM exit
flushes the branch predictors. A whole exit, a CPUID round trip, took
1.31 µs here with it, and Wine's 55,000 exits a second each pay it. The host kernel's
command line decides it: `spec_rstack_overflow=off` drops the per-exit
barrier (VMSCAPE then falls back to "IBPB before exit to userspace", on
the exits the VMM handles itself), and `vmscape=off` drops that too. Off,
a guest can train the return-address and branch predictors to steer the
host kernel's (SRSO, CVE-2023-20569) and the VMM's (VMSCAPE,
CVE-2025-40300) speculation after an exit and read host memory through a
side channel. Leave both at their defaults on any host that runs a guest
it does not trust; nothing in this project turns them off.

## vCPU placement

Off by default: the host scheduler places every vCPU, VMM and backend
thread, and the guest is told its VMM's own topology (nesbox: every vCPU a
core; crosvm: vCPUs 2k and 2k+1 SMT siblings, for an even count). Placement
changes where threads run, never what they may do (SECURITY.md, "vCPU
placement").

**The pieces, at each layer.** `rig/pin-layout.sh <vcpus> <layout>` prints
a layout for this host in each place's words.

| what | launcher (`rig/run-guest.sh`) | crosvm | nesbox (`machine-config`) | units / NixOS module |
|---|---|---|---|---|
| a layout from the host's topology | `NVGPU_PIN=cores\|smt\|spread\|l3\|core-sets[:l3=CPU][:avoid=LIST][:io=WHERE]` | (what it prints) | (what it prints) | (what it prints) |
| one host CPU, or a CPU list, per vCPU | `NVGPU_VCPU_PINS=8,9,..` or `8,24:9,25:..` | `--cpu-affinity 0=8:1=9:..` | `"vcpu_pins": [8, 9, ..]`, or `[[8, 24], ..]` with `patches/nesbox/0002` | `NVGPU_CROSVM_ARGS`; the nesbox config |
| one set for every vCPU | `NVGPU_CPU_AFFINITY=8-15` | `--cpu-affinity 8-15` | `"cpu_affinity"` | the same |
| the VMM's other threads | `NVGPU_IO_AFFINITY` | started under it (`taskset`; the unit's `CPUAffinity=`) | `"io_affinity"` | `nvgpu-vmm-crosvm@.service` `CPUAffinity=` |
| the backend's threads | `NVGPU_BACKEND_CPUS` (default: the I/O set) | -- | -- | `vhost-user-nvgpu@.service` `CPUAffinity=`; `vms.<n>.backendCpus` |
| threads per guest core | `NVGPU_GUEST_SMT=1\|2` | `--no-smt` for 1 | `"threads_per_core"` | the same |

The layouts read `/sys/devices/system/cpu`: each core's threads
(`thread_siblings_list`), each L3 domain (`cache/index3`: a CCD on a
Ryzen), and the host scheduler's preference (`amd_pstate_prefcore_ranking`,
or `acpi_cppc/highest_perf`), which is where the desktop's busiest threads
land. A layout takes the least-preferred L3 domain first and its
least-preferred cores first, and never CPU 0's core unless `avoid=none`.
On the rig's 9950X that is CCD1 (CPUs 8-15, 24-31):

| layout | 8 vCPUs on | the guest is told |
|---|---|---|
| `cores` | 8-15, one thread of each core; 24-31 left idle | 8 cores |
| `smt` | 10/26, 13/29, 14/30, 15/31 (vCPUs 2k, 2k+1 on one core) | 4 cores of 2 threads |
| `spread` | 15, 6, 14, 7, 10, 1, 13, 4 (the CCDs in turn) | 8 cores |
| `l3` | any of 8-15, 24-31 | the VMM's default |
| `core-sets` | vCPU i on either thread of core 8+i | 8 cores |
| `cores:io=siblings` | as `cores`; the VMM's other threads and the backend on 24-31 | 8 cores |
| `cores:io=other` | as `cores`; those on CCD0 (0-7, 16-23) | 8 cores |
| `smt:io=rest` | as `smt`; those on the four CCD1 cores it leaves (8, 9, 11, 12 and their siblings) | 4 cores of 2 threads |

`io=` is `none` (the default: where the host puts them), `siblings`,
`rest`, `other` or a CPU list.

`smt` needs one core-scheduling cookie for the VM, not one per vCPU
(crosvm's default), or a core's two vCPUs never run at once: the launcher
asks for it (`NVGPU_CORE_SCHED=vm`) and refuses a cookie per vCPU with
`smt`. A crosvm unit says `--per-vm-core-scheduling`. With pins and no I/O
set, the launcher gives nesbox its own CPUs as the I/O set, so that the
worker a vCPU thread starts for the GPU window does not stay on that
vCPU's CPU.

**Isolation.** A VM given whole cores -- `cores`, `smt`, `core-sets` --
shares no core with another VM that is kept off them, which closes the
SMT side channels between the two, and `l3` or a CCD of its own also
keeps the L3 apart: more isolation than the default placement, not less.
Pinning only places this VM; give each VM its own cores (`l3=`, `avoid=`)
and keep host work off them. Two VMs pinned to the two threads of one core
are the opposite: they share it all the time.

**What each layout gains and costs** (BENCHMARKS.md, "vCPU pinning";
nesbox, 8 vCPUs, the rig's 9950X, three runs or more a cell): on an idle
host no layout changes much. `cores` is within 2% of unpinned in Heaven and
gameloop and loses 6-8% in stk-vk and Godot. `smt` loses 3-8% of average fps
(it runs 8 vCPUs on 4 cores) and has the best lows of any layout:
Heaven's 1% / 0.1% lows 282 / 138 fps against 233 / 104 unpinned, and
native's 296 / 144. With the desktop's load on CCD0 (every thread of it
busy), `smt` is the only layout that keeps its lows -- Heaven 234 / 112
against 175 / 65 unpinned and 122 / 46 with `cores`, gameloop's 0.1% low 67
against 20 and 17 -- at an average within 3% of unpinned. What it has that
`cores`, `l3` and `cores:io=siblings` do not is four whole idle cores on the
vCPUs' own CCD, where the host puts what else must run (the compositor, the
backend's queue thread); the same program natively does the same (on 8-15,
Heaven's lows under the load 155 / 51; on `smt`'s CPUs 235 / 95). `spread`
puts half its vCPUs on the loaded CCD (Heaven 193 fps), and `io=other` puts
the backend there (an IOCTL2 takes 38-45 µs instead of 10). Pinning does
not make a wakeup between vCPUs cheaper (a futex hand-off 7.8 µs unpinned,
8.0 with `cores`, 8.7 between two `smt` siblings). A busy CCD0 costs even a
native game confined to CCD1 3-14% of its average: the two CCDs share a
power budget, which no placement changes.

**Recommended, for a gaming VM on this host:** `NVGPU_PIN=smt` (8 vCPUs on
four CCD1 cores, the guest told they are pairs; `rig/pin-layout.sh 8 smt`
for crosvm and nesbox configs), when the desktop's own work -- a build, an
encode, a browser -- can fill the other CCD. It trades 3-8% of average fps on
an idle host for lows that hold under load, and it costs no isolation: the
VM has four whole cores no other VM is placed on, and with one
core-scheduling cookie for the VM (crosvm, and nesbox with
`NVGPU_CORE_SCHED=vm`) no host task shares a core with a running vCPU
either. `smt:io=rest` (the backend and the VMM's other threads on the four
cores `smt` leaves) measured the same as `smt`. Keep the host's own
work off CCD1 (a slice or cpuset of its own) for the full effect. On a host
that is otherwise idle, the default -- nothing pinned -- is as good or
better on average, and it stays the default.

## Capture injection

A guest application's screen share, zero-copy (ARCHITECTURE.md and
SECURITY.md, "Capture injection"). The backend provides one primitive: a host buffer made a
guest dma-buf. The two programs around it are the integrator's: a **capture
helper** on the host, one per VM, and a **capture daemon** in the guest. The
rig's `rig/rig-tools/nvgpu-inject-test.c` and
`rig/guest-image/tools/nvgpu-capture-import.c` are the reference for each.

**Host side.** Each VM that shares screens gets a helper user of its own
(never the backend's, the VMM's, or the desktop user's uid, and never one
shared with another VM: whoever has a VM's helper uid can inject into it),
and a group of its own with the helper alone in it. The socket is
[`vhost-user-nvgpu-inject@.socket`](contrib/systemd/vhost-user-nvgpu-inject@.socket)'s:
systemd binds `/run/nvgpu/vmN/inject.sock`, root's and 0660 to the group
`nvgpu-capN` (a drop-in's `SocketGroup=` names another), and hands it to the
backend. In `/etc/virtio-nvgpu/vmN.env`:

```sh
NVGPU_BACKEND_ARGS="--inject-uid 950"
```

and the backend's unit pulls the socket in:

```ini
# /etc/systemd/system/vhost-user-nvgpu@N.service.d/inject.conf
[Unit]
Requires=vhost-user-nvgpu-inject@N.socket
```

On NixOS, `services.virtio-nvgpu.vms."N".inject = { enable = true;
helperUid = 950; helperGroup = "nvgpu-cap0"; }`, and the module refuses a
helper uid of the pool or of a login user, a helper group that does not
exist, is a pool or shared group (wheel, users, video, ...) or holds anyone
but the helper, and one helper or group for two VMs. The backend serves the
socket only for its `--inject-uid`, and refuses to start with the socket
and no `--inject-uid`. The helper runs as that user, in that group, with the
portal and PipeWire of the desktop session it serves.

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

## Opt-in RM groups

`--rm-allow-group NAME[,NAME...]` (rig: `NVGPU_RM_ALLOW_GROUP`; NixOS:
`vms.<n>.rmAllowGroups`) adds groups of RM controls the default allowlist
refuses, none by default. Each is held to a rule of its own before RM sees
it; SECURITY.md, "Opt-in RM groups", has what each exposes and what still
holds, and what was left out even as a group. The backend's start-up line
after the tables names the groups it serves and what each adds on the
host's release.

| group | gain | cost |
|---|---|---|
| `thermal` | temperatures through THERMAL_SYSTEM_EXECUTE_V2 (host 580 and later), for an NVML, MangoHud or nvtop that reads them that way; on the rig's 595.99.02 NVML reads them through controls the default list has, and this group changes nothing a tool shows | every guest process reads the GPU's temperatures, which other tenants' load moves |
| `health` | `nvidia-smi -q`'s ECC mode, InfoROM, retired pages and black-box flush time (N/A, as natively, on a GPU without ECC or InfoROM) | the host GPU's health, to every guest process |
| `memacct` | the GPU memory limits and use of the backend's cgroup (host 610 and later) | this VM's own GPU memory use, to its processes; nothing on older hosts |
| `debug` | cuda-gdb's and compute-sanitizer's debugger memory reads and writes (at most 256 KiB a call) and debug modes; needs `--allow-compute` | a debugger over the caller's own memory objects; the modes act on the debugged context inside GSP-RM |
| `profiling` | context-switched Nsight and CUPTI counters of the caller's own context; needs `--allow-compute`, and a host whose nvidia.ko has `NVreg_RmProfilingAdminOnly=0` (RM refuses an unprivileged profiler otherwise, as it does natively); register writes are refused, so a profiler that programs counters that way does not work | the caller's own context's counters; a host that sets `RmProfilingAdminOnly=0` opens profiling to every host user too |

The backend refuses to start with a name it does not know, or with `debug`
or `profiling` and no `--allow-compute`; the NixOS module says the same at
evaluation.

## dma-buf export through RM

`--allow-dmabuf-export` (the NixOS module's `vms.<n>.dmabufExport`, the
launcher's `NVGPU_DMABUF_EXPORT=1`). **Off by default**, and then RM's
`EXPORT_TO_DMABUF_FD` is refused as it always was.

**What it does.** A guest process may export video memory of its own RM
client as a dma-buf, the way CUDA's `cuMemGetHandleForAddressRange` with a
dma-buf handle and NVIDIA's GBM export through RM ask nvidia.ko to. The
guest gets a dma-buf of its own device: its render nodes import it, it can
be passed to another guest process (which can import it too), and NVIDIA's
userspace in the guest takes it as it takes the same dma-buf natively.

**What it does not.** The dma-buf never leaves the VM: it cannot be sent to
the host compositor as a Wayland buffer (the client sees
`zwp_linux_buffer_params_v1.failed`), made a framebuffer on a lease or card,
or handed out as a host dma-buf. No CPU maps it (as natively on a discrete
GPU: `mmap` fails, EOPNOTSUPP in the guest, ENOTSUPP natively). One call
makes the whole dma-buf, of at most 128 allocations: the append form
(`fd` >= 0), which CUDA uses only past 128 allocations, is refused. Only
memory of a client the calling process made. A VM holds at most 256
exports and 8 GiB of them at once, a process a quarter of the count and
half the bytes; past that the export fails with
`NV_ERR_INSUFFICIENT_RESOURCES`.

**Gain.** Workloads that export video memory through RM stop failing as on
a driver without dma-buf export. On the rig's RTX 5090 (595.99.02) that is
less than it sounds: CUDA offers no dma-buf export on a GeForce card
(`CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED` is 0, natively too), and NVIDIA's
EGL and Vulkan take no dma-buf of RM's on a discrete GPU natively either
(`EGL_BAD_ALLOC`; no memory type for it). What works there is the export
itself and a render node's import of it; the rest is where the host's own
driver takes it.

**Cost.** nvidia.ko's dma-buf exporter becomes reachable from the guest
(`nv-dmabuf.c`: the export, and the attach and map of the backend's own
import of it into the render file), and each live export holds a render
file of the host's, its import, and BAR1 space while mapped on a GPU
without static BAR1 (SECURITY.md, "dma-buf export through RM"). Needs
nothing of the units or the sandbox.

## Backend flags

`vhost-user-nvgpu --help` has each one's full text; `--diagnostic --help`
shows the diagnostic ones too.

| flag | default | what it does |
|---|---|---|
| `--socket PATH` | `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` | the vhost-user socket; whoever connects gets the guest's memory. A file already there is removed only if it is this uid's socket. Not with the units: systemd binds the socket and hands it over (`LISTEN_FDS`, descriptors named `vhost-user` and `inject`) |
| `--socket-fd N` | none | serve the vhost-user socket already listening on descriptor N, bound by whoever started the backend (the root launcher does this) |
| `--allow-compute` | off | serve CUDA and other compute: `/dev/nvidia-uvm`, the UVM aperture, memory registered by its pages. Graphics, Vulkan Video and display need none of it |
| `--allow-dmabuf-export` | off | serve RM's `EXPORT_TO_DMABUF_FD`: a guest process's own video memory as a guest dma-buf that never leaves the VM ("dma-buf export through RM") |
| `--window-size MIB` | 1024 | the shared window: how much GPU memory the VM's processes can have CPU-mapped at once. A multiple of 64, at least 256; with `--allow-compute` at most 64512 (window and aperture share crosvm's 64 GiB region cap), else 65536; nesbox takes at most 32768. Refused at start otherwise ("Sizing the window") |
| `--window-owner-share PERCENT` | 50 | the percent of each window zone one guest process may hold, 1-95. From 88 one process can take a zone down to its reserve (SECURITY.md, "The window's size and share"). The same percent of `--vram-limit` |
| `--vram-limit MIB` | none | hold the VM to MIB of video memory, at least 64: allocations past it fail as on a smaller GPU, and nvidia-smi, NVML, Vulkan and CUDA in the guest are told it as the GPU's size ("Video memory limit") |
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
| `--fifo-disable-proc-rate N`, `--fifo-disable-proc-burst N` | 50, 40 | FIFO_DISABLE_CHANNELS calls a second one guest process may make, and at once ("Tuning") |
| `--fifo-disable-vm-rate N`, `--fifo-disable-vm-burst N` | 200, 160 | the same for the whole VM, the last 40 kept for quiet processes |
| `--pacing-stats SECS` | none | log the frame-pacing counters every SECS while the guest is busy (also `NVGPU_PACING_STATS`); they are logged once at teardown regardless |
| `--inject-socket PATH` | none | accept screen-share buffers here from the VM's capture helper (with `--inject-uid`; "Capture injection"); the units hand the socket over instead |
| `--inject-uid UID` | none | the only uid the inject socket serves: the VM's capture helper |
| `--rm-allowlist enforce` | `enforce` | the RM allowlist; `log` is diagnostic |
| `--rm-allow-group NAME[,NAME...]` | none | add opt-in groups to the RM allowlist: `thermal`, `health`, `memacct`, `debug`, `profiling` ("Opt-in RM groups") |
| `--osdesc-populate on` | `off` | fault memory a guest registers by its pages (cuMemHostRegister, VK_EXT_external_memory_host) into the backend with one `MADV_POPULATE_WRITE` (`_READ` for read-only memory) before RM pins it, instead of page by page inside the pin (rig: `NVGPU_OSDESC_POPULATE=1`; NixOS: `vms.<n>.osdescPopulate`). Gain: none measured. Cost: on the rig (nesbox, RTX 5090, 595.99.02) the first cuMemHostRegister of fresh guest memory took 49.8 ms for 1 GiB and 13.2 ms for 256 MiB with it, 47.2 and 12.2 ms without (three boots each): the pin finds guest RAM's pages cheap to fault, and the populate walks them once more. For a host where a pin's faults are costlier |
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
| `--allow-inject-self` | the refusal of an `--inject-uid` that is the backend's own uid, and of a helper connection from the VMM's uid (every process of that user could inject into the VM); the rig's, which is one user |

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

**The guest's tuning.** Each is a word of the guest kernel's command line
(the VMM's `-p`, nesbox's `boot_args`); the rig's launcher adds it from its
variable. None reaches the host: what each trades is the guest's own CPU.

| setting | launcher | default | what it does |
|---|---|---|---|
| `cpuidle_haltpoll.force=1` | `NVGPU_GUEST_HALTPOLL=1` | off | an idle vCPU polls before it halts: a wakeup between vCPUs took 0.95 µs against 7.6, and KVM saw 47% fewer exits under Wine -- but a polling vCPU runs no other thread, and a D3D11 game under Wine lost 9%. **Not recommended**; it also keeps host CPUs busy while the guest idles |
| `virtio_gpu_nv.rt_spin_us=N` | `NVGPU_GUEST_RT_SPIN_US` | 20 | how long a caller spins for its reply before sleeping (0 to 1000; "Frame pacing"). 20 saves an interrupt and a wakeup a call (idle guest: 5-6 µs a call against 18 at 0) and keeps the vCPU busy that long even when another guest task could run: with 4 hogs and 4 callers on 4 vCPUs the hogs lost about half. That is fairness inside one VM, not between VMs. Wine and the heavy workloads were neutral to 0 |
| `virtio_gpu_nv.async_fence_watch=Y\|N` | `NVGPU_GUEST_ASYNC_FENCE_WATCH=1\|0` | Y | a fence proxy's WATCH from a work item: two round trips a frame off the presenting thread |
| `transparent_hugepage=always\|madvise\|never` | `NVGPU_GUEST_THP` | always (the kernel's config) | the guest kernel's huge pages: `always` because a game's heap never asks for them (10% in the job-system loop) |

The guest kernel `scripts/build-guest-kernel.sh` builds has transparent
huge pages (`always`) and ntsync (`/dev/ntsync`, which Wine 10+ and Proton
use: 11% in a D3D11 game, and half the halts) built in; the launcher's
`NVGPU_GUEST_THP` is for measuring without them. The Wayland daemon's
`--surface-buffers N` (below) is the guest's too.

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
clients' dma-bufs are imported into), `--log` or `NVGPU_WL_LOG` for its
log level (`info` by default), and `--surface-buffers N` (1 to 256,
default 16): how many of one surface's `wl_shm` buffers have their damage
tracked. A client that cycles through more (some toolkits keep a pool of
them) has each buffer copied whole when it next shows it, not just what
changed; each tracked buffer is visited at every commit, on the one thread
every other application's messages wait on (64 cost about 3 µs a commit
more than 16). Raise it for such a client, and not past what it cycles
through.

**The Vulkan layer, for a guest without compute.** Without
`--allow-compute` the guest has no `/dev/nvidia-uvm`, and NVIDIA's Vulkan
driver there -- as natively without the UVM device -- still lists the
extensions that need it (acceleration structures and every ray-tracing
extension on them, `VK_NVX_binary_import` that DLSS uses,
`VK_NV_cuda_kernel_launch`, `VK_NV_optical_flow` that Frame Generation
uses) and reports their features, but fails `vkCreateDevice` with
`VK_ERROR_INITIALIZATION_FAILED` when any of them is enabled. An
application that enables what it is offered does not start: Godot's Vulkan
renderer dies with SIGILL, and a D3D12 game through vkd3d-proton hangs
(BENCHMARKS.md, "Steam-like games"). `VK_LAYER_NVGPU_no_uvm`
([`nvgpu-vk-layer/`](nvgpu-vk-layer/)), an implicit layer, makes such a
guest look like a driver without them: it drops them from the device's
extension list, clears their features in `vkGetPhysicalDeviceFeatures2`,
and answers a `vkCreateDevice` that asks for them with
`VK_ERROR_EXTENSION_NOT_PRESENT` or `VK_ERROR_FEATURE_NOT_PRESENT`, so the
application falls back (vkd3d-proton without DXR, a game without DLSS). It
does nothing when `/dev/nvidia-uvm` exists, for another vendor's device, or
with `NVGPU_VK_NO_UVM_DISABLE=1`, and it reaches nothing outside the
process. Install its manifest where the guest's Vulkan loader searches for
implicit layers, naming the library by its absolute path: `make PREFIX=/usr
install` puts it in `/usr/share/vulkan/implicit_layer.d`, which every loader
searches while `XDG_DATA_DIRS` is unset; nixpkgs' loader does not search
`/etc/vulkan`, and on NixOS a package in the system profile is found through
`XDG_DATA_DIRS`. The 64- and 32-bit builds of
`rig/guest-image/nix/vk-layer.nix` carry `library_arch` in their manifests,
so each loader takes its own.

**What the image must contain**, then: the kernel and `virtio_gpu_nv.ko`
loaded at boot; the host's NVIDIA userspace (Vulkan ICD, EGL and GBM
vendors, `libcuda` and the video libraries if compute is served); the
`video`, `render` and `nvgpu-wl` groups and the udev rule; `nvgpu-wl-guest`
setgid `nvgpu-wl`, started per session with `XDG_RUNTIME_DIR` set;
`VK_LAYER_NVGPU_no_uvm` if the guest may run without compute; for
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
- RM's `EXPORT_TO_DMABUF_FD` without `--allow-dmabuf-export`, and with it
  its append form and any export of it out of the VM ("dma-buf export
  through RM"); IMEX sessions and fabric memory, nvidia-drm's
  `GEM_IMPORT_USERSPACE_MEMORY`, DRM `GEM_FLINK`/`GEM_OPEN`: refused.
- Vulkan ray tracing without `--allow-compute`: NVIDIA's driver lists the
  extensions but cannot create a device with them, natively as well; an app
  that enables every one it is offered fails in a graphics-only guest.
- Two CUDA processes whose UVM semaphore pools want the same host address:
  the second context fails with ENOMEM (ARCHITECTURE.md, "What this design cannot do").
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
| `the VMM never asked the shared window's size (GET_SHMEM_CONFIG) ...` | a VMM older than the window it serves (nesbox before `virtio-nvgpu-v4`): placements past 1 GiB will fail |
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
