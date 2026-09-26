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

Both need `/dev/kvm` for the VMM user. Guest RAM is committed at boot; the
shared window adds up to 1 GiB (sparse), and with compute the UVM aperture
another 1 GiB.

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

## Backend flags

`vhost-user-nvgpu --help` has each one's full text; `--diagnostic --help`
shows the diagnostic ones too.

| flag | default | what it does |
|---|---|---|
| `--socket PATH` | `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock` | the vhost-user socket; whoever connects gets the guest's memory. A file already there is removed only if it is this uid's socket |
| `--allow-compute` | off | serve CUDA and other compute: `/dev/nvidia-uvm`, the UVM aperture, memory registered by its pages. Graphics, Vulkan Video and display need none of it |
| `--kms-card` | off | compositor-VM mode: offer the host's card nodes to the guest. Only for a host with no compositor of its own. Not run on hardware |
| `--wayland-socket PATH` | none | the host compositor's socket, for the Wayland proxy |
| `--wayland-lease` | off | offer the compositor's `wp_drm_lease_device_v1` (this GPU's card only) |
| `--wayland-lease-interval SECS` | 5 | average spacing of one VM's lease requests (3 at once; 0 lifts it) |
| `--wayland-export PATH` | none | accept host Wayland clients here for a guest compositor. Not run on hardware |
| `--wayland-max-conns N` | 64 | Wayland channels per VM |
| `--wayland-shm-budget MIB` | 1024 | wl_shm buffer memory per VM |
| `--wayland-queue-budget MIB` | 256 | unread compositor output per VM |
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

**Kernel and module.** A Linux 7.2 guest kernel with the options
[`scripts/build-guest-kernel.sh`](scripts/build-guest-kernel.sh) sets (it
builds the kernel and the module together), and the guest module
`virtio_gpu_nv.ko`, built from [`driver/`](driver/) against it, C or Rust
parsers ([`driver/README.md`](driver/README.md)). Its parameters:

| parameter | default | |
|---|---|---|
| `wl_mode` | `0660` | mode of `/dev/nvgpu-wl*`, created `root:root`; anything for "other" is refused |
| `virtio_id` | `45` | the virtio device ID to bind (another only for a VMM that cannot express 45) |

**NVIDIA userspace.** The host's own release, exactly: in the image, or
mounted from a share `nvgpu-userspace` staged. The module makes the device
nodes (`/dev/nvidiactl`, `/dev/nvidia0...`, `/dev/nvidia-modeset`,
`/dev/nvidia-uvm` with compute, a DRM card and render node per GPU); render
nodes to group `render`, card nodes to `video`, as usual.

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
setgid `nvgpu-wl`, started per session with `XDG_RUNTIME_DIR` set; and
applications run as unprivileged users in `video` and `render` only.
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
  the second context fails with ENOMEM (ARCHITECTURE.md §17).
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

In the VMM's log: crosvm's `refused a memory request` (the main process
refused a mapping the frontend passed on), a `SIGSYS`/seccomp kill of a
jailed device process; nesbox's refusals of a window placement.

In the guest's `dmesg`: `is not write-back in this guest's MTRRs` (the VMM's
MTRRs, see README.md), and the module's refusals.

In the host's kernel log: `NVRM: Xid` (GPU faults; 79, 119, 120 or 154 mean
the GPU needs a reset), `NVRM: API mismatch` (a guest userspace of another
release), any oops or `WARN` naming nvidia, nvidia-drm or nvidia-modeset.
