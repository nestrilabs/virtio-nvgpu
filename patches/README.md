# Host compositor patches

A guest reaches the host display through `wp_drm_lease_device_v1` (DESIGN §0 mode 3): the host compositor
leases a connector, its CRTC and planes, and the guest drives them through the lease fd. Stock Hyprland
only leases outputs the kernel marks non-desktop (VR headsets), so these patches let it lease a normal
monitor too.

| Patch | Against | What it does |
|---|---|---|
| `hyprland/0001-lease-desktop-outputs.patch` | Hyprland 0.56.0 (`e368c13c`) | `leasable` monitor rule; Hyprland lets go of a leased monitor and takes it back when the lease ends; sends `released`; fixes to the lease protocol |
| `aquamarine/0001-keep-leased-crtcs.patch` | aquamarine 0.15.1 (`f31c47a`) | stops aquamarine from reassigning, VT-restoring or restating a leased CRTC |

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
cd hyprland && git am ../patches/hyprland/0001-lease-desktop-outputs.patch
cd aquamarine && git am ../patches/aquamarine/0001-keep-leased-crtcs.patch
```

With Nix, point Hyprland's `aquamarine` flake input at the patched tree, or add the patch to
`aquamarine.patches` in an overlay. The aquamarine patch changes no headers, so the ABI is unchanged.

# crosvm

`crosvm/` is a series against upstream crosvm (`c0474109d64d`, 2026-09-25) that lets crosvm be the
VMM, as the frontend of `vhost-user-nvgpu` (README.md, "What a VMM must do"; TESTING-RIG.md,
"crosvm"):

| Patch | What it does |
|---|---|
| `0001-vhost_user_frontend-check-every-backend-mapping-agai.patch` | every `SHMEM_MAP` (and `GPU_MAP`, `EXTERNAL_MAP`) is checked against the region the backend reported, page-aligned, and refused if it overlaps a live mapping; an unmap must name one exactly; a reset unmaps everything; a refusal no longer stops the VM |
| `0002-devices-virtio-nvgpu-as-a-vhost-user-device-type.patch` | `--vhost-user type=nvgpu`: virtio ID 45, PCI class 0xff0000, indirect descriptors passed through, region 1 published when the backend reports more, `GPU_MAP` and `EXTERNAL_MAP` refused |
| `0003-x86_64-no-pci-hotplug-port.patch` | `--no-pci-hotplug-port`: no empty hot-plug root port on PCI bus 1, which the guest driver needs for the GPU's host address |
| `0004-vhost_user_frontend-tests-for-the-backend-mapping-ch.patch` | unit tests for 0001 and 0002 |
| `0005-base-place-an-arena-file-mapping-without-leaving-a-h.patch` | a file mapping into the shared-memory arena is placed atomically (map elsewhere, then `mremap(MREMAP_FIXED)`), so a descriptor the backend hands over that cannot be mapped fails without punching a hole in the KVM-slot-backed arena; with a `/proc/self/maps` test |
| `0006-devices-bound-the-backend-reported-shared-memory-reg.patch` | the backend-reported shared memory region size is capped (64 GiB) and refused cleanly, instead of a `next_power_of_two().expect()` that a wild value would panic on |

To apply and build: the top of `scripts/rig-build-crosvm.sh`. No seccomp policy and no minijail
setting changes: the vhost-user frontend runs in crosvm's main process, as upstream has it.
