# Host compositor patches

A guest reaches the host display through `wp_drm_lease_device_v1` (the lease mode: README, "Display";
ARCHITECTURE.md §11): the host compositor leases a connector, its CRTC and planes, and the guest drives
them through the lease fd. Stock Hyprland only leases outputs the kernel marks non-desktop (VR headsets),
so these patches let it lease a normal monitor too.

**Status.** The 0.56.2 pair (`…-efb5099.patch` + `…-1a10fe2.patch`) runs as the live desktop compositor
of the dev box, Hyprland 0.56.2 at `efb50993` from its flake, and carried every lease stage on an RTX 5090
(rig/TESTING-RIG.md, "Group B"): a desktop monitor leased to a guest, driven with KMS and `VK_KHR_display`,
and taken back after each lease. The `e368c13c` pair builds and passes Hyprland's and aquamarine's own
tests, and has not run as a desktop.

| Patch | Against | What it does |
|---|---|---|
| `hyprland/0001-lease-desktop-outputs.patch` | Hyprland `main` at `e368c13c` (0.56.0 + 203 commits, `v0.56.0-203-ge368c13c`) | `leasable` monitor rule; Hyprland lets go of a leased monitor and takes it back when the lease ends; sends `released`; fixes to the lease protocol |
| `hyprland/0001-lease-desktop-outputs-efb5099.patch` | Hyprland 0.56.2 (`efb50993`, tag `v0.56.2`, branch `v0.56.2-b`) | the same, rebased; also parses `leasable` in the hyprlang config (`monitor = …, leasable, 1`, `monitorv2 { leasable = 1 }`) |
| `aquamarine/0001-keep-leased-crtcs.patch` | aquamarine 0.15.1 (`f31c47a`) | stops aquamarine from reassigning, VT-restoring or restating a leased CRTC |
| `aquamarine/0001-keep-leased-crtcs-1a10fe2.patch` | aquamarine `1a10fe26` (0.14.0 + 6), which Hyprland's flake pins at `efb50993` | the same, rebased (no code change) |

Which pair to use:

| Hyprland | aquamarine it is built with | patches |
|---|---|---|
| Hyprland flake at `efb50993` (0.56.2) | `1a10fe26` (its `flake.lock`) | `…-efb5099.patch` + `…-1a10fe2.patch` |
| nixpkgs `hyprland` 0.56.2 | nixpkgs `aquamarine` 0.15.1 | `…-efb5099.patch` + `0001-keep-leased-crtcs.patch` |
| Hyprland `main` at `e368c13c` | 0.15.1 | `0001-lease-desktop-outputs.patch` + `0001-keep-leased-crtcs.patch` |

`e368c13c` is not 0.56.0 and not an ancestor of 0.56.2: 0.56.2 is 36 commits on the `v0.56.2-b` release
branch from the `v0.56.0` tag, `e368c13c` 203 commits on `main` from it. `main` has since moved hyprctl
to `src/ipc/s1`, added async commits (`CMonitor::m_commitCoordinator`, aquamarine 0.15) and dropped the
hyprlang config; 0.56.2 has none of these, and the rebased patch is adjusted for that. On NixOS,
`nixos/hyprland-lease.nix` applies the right pair to whichever Hyprland the config uses
([`nixos/README.md`](nixos/README.md)).

The Hyprland patch works on its own for the usual case. Without the aquamarine patch, a hotplug while a
lease is active can hand the leased CRTC to another monitor, whose modeset then takes the display from the
guest.

## Configuring

```lua
-- used as a normal monitor until a client leases it
hl.monitor({ output = "DP-2", mode = "preferred", position = "auto", leasable = true })

-- never used by Hyprland, only for the lessee
hl.monitor({ output = "HDMI-A-1", disabled = true, leasable = true })
```

With the 0.56.2 patch the hyprlang config (`hyprland.conf`) takes it too:

```ini
monitor = DP-2, preferred, auto, 1, leasable, 1
monitor = HDMI-A-1, disable, leasable, 1

monitorv2 {
    output = HDMI-A-1
    disabled = 1
    leasable = 1
}
```

`leasable` defaults to `false`. With no leasable monitor, Hyprland's behaviour is unchanged apart from the
protocol fixes listed in the Hyprland commit message. You can change `leasable` at runtime: that withdraws
or offers the connector without a modeset. `hyprctl monitors all` shows `leasable` and `leased`.

## What happens during a lease

1. A client submits a lease request for the monitor's connector.
2. Hyprland releases the monitor as if it had been unplugged. Workspaces and focus move to another monitor,
   and frame scheduling and queued commits stop. Then a blocking commit disables the output, so the CRTC and
   planes are off and our last flip has completed.
3. Hyprland creates the lease from the connector's current CRTC and sends the lease fd.
4. While the lease is active, nothing in Hyprland commits to that output. Rule reloads, DPMS, output
   management and aquamarine state requests are all refused or skipped.
5. The lease ends in one of three ways: the client destroys the lease, the lessee closes every copy of the
   fd (the kernel sends a `LEASE=1` uevent and aquamarine runs `scanLeases`), or the monitor is unplugged,
   which revokes the lease. Hyprland then reconnects the monitor with its current rule, using a full modeset
   as on hotplug.

## Applying

```sh
cd hyprland && git am ../patches/hyprland/0001-lease-desktop-outputs-efb5099.patch     # on efb50993
cd aquamarine && git am ../patches/aquamarine/0001-keep-leased-crtcs-1a10fe2.patch     # on 1a10fe26
```

With Nix, `nixos/hyprland-lease.nix` is a NixOS module that does both: it overrides the `aquamarine`
argument Hyprland was built with (the Hyprland flake takes aquamarine from its own input, not from
`pkgs`, so an overlay on `pkgs.aquamarine` would miss it) and appends the Hyprland patch. The aquamarine
patch changes no headers, so the ABI is unchanged.

# crosvm

`crosvm/` is a series of ten patches against upstream crosvm (`c0474109d64d`, 2026-09-25) that lets
crosvm be the VMM, as the frontend of `vhost-user-nvgpu` (README.md, "What a VMM must do"; rig/TESTING-RIG.md,
"crosvm"). All ten have run on an RTX 5090, graphics and compute, the frontend jailed; `0007` and `0010`
were changed by the 2026-09-29 review (SECURITY.md §22) after that run, and `scripts/ci.sh deploy` checks
that the series still applies:

| Patch | What it does |
|---|---|
| `0001-vhost_user_frontend-check-every-backend-mapping-agai.patch` | every `SHMEM_MAP` (and `GPU_MAP`, `EXTERNAL_MAP`) is checked against the region the backend reported, page-aligned, and refused if it overlaps a live mapping; an unmap must name one exactly; a reset, or the backend going away, unmaps everything; a refusal no longer stops the VM |
| `0002-devices-virtio-nvgpu-as-a-vhost-user-device-type.patch` | `--vhost-user type=nvgpu`: virtio ID 45, PCI class 0xff0000, indirect descriptors passed through, region 1 published when the backend reports more, `GPU_MAP` and `EXTERNAL_MAP` refused |
| `0003-x86_64-no-pci-hotplug-port.patch` | `--no-pci-hotplug-port`: no empty hot-plug root port on PCI bus 1, which the guest driver needs for the GPU's host address |
| `0004-vhost_user_frontend-tests-for-the-backend-mapping-ch.patch` | unit tests for 0001 and 0002 |
| `0005-base-place-an-arena-file-mapping-without-leaving-a-h.patch` | a file mapping into the shared-memory arena is placed atomically (map elsewhere, then `mremap(MREMAP_FIXED)`), so a descriptor the backend hands over that cannot be mapped fails without punching a hole in the KVM-slot-backed arena; a move that fails after the kernel dropped the target is refilled with `MAP_FIXED_NOREPLACE`, or the process aborts; a hugetlbfs file is refused; with `/proc/self/maps` tests |
| `0006-devices-bound-the-backend-reported-shared-memory-reg.patch` | the backend-reported shared memory region size is capped (64 GiB) and refused cleanly, instead of a `next_power_of_two().expect()` that a wild value would panic on |
| `0007-vm_control-hold-virtio-nvgpu-s-memory-tubes-and-map-.patch` | the main process holds virtio-nvgpu's two memory tubes to what the device needs, against the BAR layout it made itself (`vm_control::sys::linux::nvgpu`): the shared memory tube prepares only the window, maps only NVIDIA (major 195) and DRM (226) descriptors into it, opened to match, page-aligned, inside it, not overlapping, at most 16,384, and unmaps only its own; the ioevent tube registers ioevents only, at the device's own queue notification addresses. New requests `RegisterUvmPool`/`UnregisterUvmPool` place a UVM semaphore pool at the host address its file offset names, in [4 GiB, 32 TiB), from `/dev/nvidia-uvm` (major from `/proc/devices`, minor 0; no major, no pool) opened read-write, from a file whose `mincore` the kernel answers for crosvm, inside the aperture, not overlapping, at most 64 MiB, 64 pools and 256 MiB per device, pages checked with `mincore` before the slot; slot removed before the mapping; all withdrawn when the tube goes. `reserve_uvm_band` reserves the band `PROT_NONE` so pools are mapped over crosvm's own reservation only (UVM refuses an mremap'd mapping); a range a failed placement leaves unknown is never used again. Unit tests for every check |
| `0008-devices-virtio-nvgpu-s-UVM-aperture-in-the-window-s-.patch` | more than one shared memory region per virtio-pci device (`VirtioDevice::get_extra_shared_memory_regions`, empty by default), each with its own capability at its own offset of the one BAR; the transport reports the layout, and the queue notification addresses, to the main process. The nvgpu frontend takes regions by id and publishes region 2, the UVM aperture, when the backend reports it (`--allow-compute`), checks each pool itself first and sends it as `RegisterUvmPool`; crosvm wires the restricted tubes, and reserves the band at the start of `run_config` when a device is of type nvgpu |
| `0009-devices-run-the-nvgpu-vhost-user-frontend-in-a-jaile.patch` | with the sandbox on, the nvgpu vhost-user frontend runs in a minijail'd process of its own under `vhost_user_frontend_device.policy` (common device syscalls, `getrandom`, `prctl` names; `open` refused; no socket, no ioctl beyond vmm-swap's); a test forks the real frontend under the embedded policy against a fake backend and main process; `run --help` names the `nvgpu-uvm-aperture` for the launcher to detect. Other vhost-user types keep upstream's in-process frontend. No virtio device's control tube may ask for a hot-plug |
| `0010-vm_control-prefault-virtio-nvgpu-s-window-mappings.patch` | the main process prefaults each window mapping it has made (`KVM_PRE_FAULT_MEMORY`, Linux 6.11 or later) through one spare vCPU that never runs, made only for an nvgpu device and only when KVM has the capability, before the guest's vCPUs, with an id past every guest vCPU's (so past every APIC id the ACPI tables list, `--host-cpu-topology` included); a hint on a thread of its own with a 1,024-range queue, which stops for good at the first ENOSYS, ENOTTY or EOPNOTSUPP. Performance only: without it the guest faults the pages in itself (SECURITY.md §21) |

To apply and build: the top of `rig/rig-build-crosvm.sh` (`CROSVM_SRC`, `CROSVM_OUT` build another
checkout to another binary). `0001`-`0006` are graphics only, with the frontend in crosvm's main process
as upstream has it; `0007`-`0009` add compute and jail the frontend, and change no other device's
seccomp policy or minijail setting; `0010` is optional, for speed. What the main process checks:
SECURITY.md §16.

## Why each VMM carries its own checks

nesbox's fork (`virtio-devices/src/nvgpu/fds.rs`, `aperture.rs` and `nvgpu.rs`, branch `virtio-nvgpu-v6`)
and this series (the jailed frontend's checks in `0001` and `0008`, the main process's in `0007`) check the
backend's mapping requests to the same limits: the descriptor kinds (NVIDIA major 195, DRM 226,
`/dev/nvidia-uvm` minor 0 opened read-write, a `mincore` the kernel answers), whole pages inside the
window and at most 16,384 placements, and UVM pools in [4 GiB, 32 TiB), at most 64 MiB each, 64 at once,
on 2 MiB aperture boundaries. A crate shared by both VMMs was considered (the 2026-09-29 review) and not
made:

- What is the same is small: the constants and the descriptor classification, about 150 lines a side.
  The rest follows each VMM's own address space and differs by design: nesbox moves a placement over a
  `PROT_NONE` reservation with `mremap` and bounds its pools by its 1 GiB aperture; crosvm checks against
  the BAR layout its transport reported, reserves the pool band up front, and counts pool bytes (256 MiB,
  as the backend does). A shared crate would hold the constants and a few predicates, with each VMM's own
  code around them.
- Neither VMM can take it as a dependency: nesbox's branch is a fork of a public project, meant to go
  upstream, that depends on nothing of this repository's; this series would carry it in `third_party/`.
  Either way it would be a copy in each VMM, which is what there is now.
- Within crosvm, the frontend's checks and the main process's are two on purpose: the main process does
  not trust the jailed frontend, and checks each request again against its own record (both take their
  constants from `vm_control/src/nvgpu.rs`). Within nesbox they are already in one place: `fds.rs` for
  descriptors, `aperture.rs` for pools.

What keeps the copies saying the same thing instead is `scripts/vmm-parity.py`: it reads each limit from
each VMM's source and from the backend's, and `scripts/ci.sh deploy` fails when one differs (crosvm as
this series has it; nesbox at `NESBOX_BRANCH`, from `NESBOX_SRC` or the rig's checkout).
