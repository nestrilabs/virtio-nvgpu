# NixOS: Hyprland with a leasable monitor

`hyprland-lease.nix` is a NixOS module that runs Hyprland 0.56.2 with
virtio-nvgpu's DRM-lease patch, linked against aquamarine with its lease
patch, so that one monitor can be marked `leasable` and a guest VM can lease
it over `wp_drm_lease_device_v1` (the lease and `VK_KHR_display` display
modes; [`../README.md`](../README.md) says what the patches do). Nothing here
changes the kernel, the NVIDIA driver, or any other package.

For the backend's own service, per-VM users and groups, see
[`nix/module.nix`](../../nix/module.nix) and [`DEPLOY.md`](../../DEPLOY.md).

## Files

Keep these together in one directory; the module finds the patches next to
itself. In this repository the three patches are symlinks to
`patches/hyprland/` and `patches/aquamarine/`, so copy with `cp -L`:

| file | for |
|---|---|
| `hyprland-lease.nix` | the NixOS module |
| `0001-lease-desktop-outputs-efb5099.patch` | Hyprland 0.56.2 (`efb50993`, tag `v0.56.2`) |
| `0001-keep-leased-crtcs-1a10fe2.patch` | aquamarine `1a10fe26`, what Hyprland's flake pins for 0.56.2 |
| `0001-keep-leased-crtcs-0.15.1.patch` | aquamarine 0.15.1, what nixpkgs builds Hyprland 0.56.2 with |

The module picks the aquamarine patch by the version of the aquamarine that
Hyprland is built with.

```sh
DEST=<config>/modules/hyprland-lease
mkdir -p "$DEST"
cp -L patches/nixos/hyprland-lease.nix patches/nixos/*.patch "$DEST"/
git -C <config> add "$DEST"          # a flake only sees files git tracks
```

## Importing it

```nix
{ inputs, pkgs, ... }:
{
  imports = [ ./modules/hyprland-lease/hyprland-lease.nix ];

  programs.hyprland.enable = true;
  programs.hyprland.lease = {
    enable = true;
    # The Hyprland to patch, with any override it already has. Default: pkgs.hyprland.
    # With the Hyprland flake:
    basePackage = inputs.hyprland.packages.${pkgs.stdenv.hostPlatform.system}.hyprland;
  };
}
```

- The module sets `programs.hyprland.package` at priority 90, which wins over
  a plain definition and over the Hyprland flake module's `mkDefault`; it
  loses only to `mkForce`. Move whatever `programs.hyprland.package` was into
  `basePackage`.
- `basePackage.override` receives the arguments Hyprland was built with, so
  the aquamarine it links is replaced by that same aquamarine plus the patch.
  The Hyprland flake takes aquamarine from its own flake input, not from
  `pkgs`, so an overlay on `pkgs.aquamarine` would miss it.
- The Hyprland patch is appended to the package's own `patches`, with its
  `tests/` and `hyprtester/` hunks filtered out (the flake's source leaves
  those directories out). Anything the base already does (a `postPatch`,
  other `overrideAttrs`) stays.
- It asserts that the base is 0.56.2.
- The patched build is also `config.programs.hyprland.lease.finalPackage`.
  With home-manager, point `wayland.windowManager.hyprland.package` at it (or
  set that option to `null`, so the NixOS one is used).
- The Hyprland flake at `efb50993`, built with its own `flake.lock`, fails at
  CMake configure where its nixpkgs has glaze 8 and `CMakeLists.txt` asks for
  `glaze 7...<8`. A configuration that already builds it has a fix for that;
  keep it on `basePackage`. If there is none, this is one:

  ```nix
  programs.hyprland.lease.basePackage =
    inputs.hyprland.packages.${pkgs.stdenv.hostPlatform.system}.hyprland.overrideAttrs (o: {
      postPatch = (o.postPatch or "") + ''
        substituteInPlace CMakeLists.txt start/CMakeLists.txt hyprpm/CMakeLists.txt \
          --replace-fail "glaze 7...<8" "glaze"
      '';
    });
  ```

No binary cache has the patched build, so it is compiled locally: aquamarine
plus Hyprland, 10–20 minutes.

## The monitor rule

Mark the monitor the guest may have. For a guest you do not trust, prefer
`disabled = true, leasable = true`: Hyprland then never puts windows or
workspaces on it, and only the lessee lights it. A plain `leasable = true`
also works: the desktop gives the monitor up when it is leased and takes it
back afterwards. 0.56.2 reads `hyprland.lua` if it exists, else
`hyprland.conf`; write the rule in whichever is in use, changing the output's
existing rule rather than adding a second.

```lua
-- hyprland.lua
hl.monitor({ output = "DP-2", disabled = true, leasable = true })
```

```ini
# hyprland.conf: either form
monitor = DP-2, disable, leasable, 1

monitorv2 {
    output = DP-2
    disabled = 1
    leasable = 1
}
```

```nix
# home-manager settings (generates hyprland.conf)
wayland.windowManager.hyprland.settings.monitor = [ "DP-2, disable, leasable, 1" ];
```

### Who may lease it, and how often

With only `leasable`, any program of the desktop user may lease the monitor,
not only the VM. Two more keys narrow that; both are off by default, which is
the behaviour above:

- `lessee`: the user the VM's backend runs as. Only that user's clients are
  offered the monitor, or may lease it. With `services.virtio-nvgpu.vms."0"`
  the backend runs as `nvgpu-vm0`. A name that does not exist leases to
  nobody.
- `lease_interval`: the least time between two leases of the monitor, in
  milliseconds; a request that comes sooner is refused before the desktop
  lets go of the monitor.

Recommended, for VM 0:

```lua
hl.monitor({ output = "DP-2", disabled = true, leasable = true, lessee = "nvgpu-vm0", lease_interval = 2000 })
```

```ini
monitor = DP-2, disable, leasable, 1, lessee, nvgpu-vm0, lease_interval, 2000
```

```nix
wayland.windowManager.hyprland.settings.monitor = [ "DP-2, disable, leasable, 1, lessee, nvgpu-vm0, lease_interval, 2000" ];
```

`lessee` only separates the VM from the desktop when the backend runs as a
user of its own, as the NixOS module does; with the rig's launcher, which runs
it as the desktop user, it separates nothing.

To take the monitor back while the VM runs, a dispatcher revokes the lease
(`revokelease DP-2` in hyprlang, `hl.dsp.revoke_lease({ monitor = "DP-2" })`
in Lua, or every lease without a monitor). The monitor stays offered, so
withdraw the offer first to keep it:

```sh
hyprctl eval 'hl.monitor({ output = "DP-2", leasable = false }); hl.dispatch(hl.dsp.revoke_lease({ monitor = "DP-2" }))'
# hyprland.conf: hyprctl keyword monitor DP-2, disable, leasable, 0 && hyprctl dispatch revokelease DP-2
```

or bind it to a key:

```nix
wayland.windowManager.hyprland.settings.bind = [ "SUPER SHIFT, L, revokelease, DP-2" ];
```

These are Hyprland settings, not options of this module: the module only
builds the patched Hyprland. [`../README.md`](../README.md), "Who may lease
it, how often, and taking it back", has the full text.

Unpatched Hyprland reports `leasable` as a config error (in Lua, in
`monitorv2`, and after a mode in `monitor =`), and silently ignores
`monitor = X, disable, leasable, 1`: change the config in the same rebuild
that installs the patched Hyprland.

The display paths also need `nvidia_drm.modeset=1`. On NixOS that is
`hardware.nvidia.modesetting.enable`, `true` by default for drivers from 535
on; it goes to modprobe options, not the kernel command line, so
`/proc/cmdline` does not show it. Do not set it `false`.

## Checking it

Rebuild, then **log out of Hyprland and back in**: a running Hyprland keeps
the old binary, and `hyprctl reload` is not enough.

```sh
hyprctl version | head -3
#   Hyprland 0.56.2 ... at commit efb50993...: upstream's commit either way,
#   since the patch is applied at build time; the next checks tell them apart.

hyprctl monitors all | grep -E '^Monitor|disabled:|leasable:|leased:|lessee:|leaseInterval:'
#   every monitor has "leasable:" and "leased:" lines (unpatched Hyprland has neither);
#   the chosen one shows disabled: true / leasable: true / leased: false, and with the
#   recommended rule lessee: <nvgpu-vm0's uid> / leaseInterval: 2000 (a build of the
#   patch from before 2026-09-30 has no lessee: line; "lessee: nobody" means the user
#   name did not resolve)

H=$(readlink -f /proc/$(pgrep -f -o 'Hyprland-wrapped|/bin/Hyprland')/exe)
P=$(echo "$H" | cut -d/ -f1-4)
nix derivation show -r "$(nix-store -q --deriver "$P")" \
  | grep -o -e 'hyprland-lease-src.patch' -e 'keep-leased-crtcs[^"/]*\.patch' | sort -u
#   hyprland-lease-src.patch, and keep-leased-crtcs-1a10fe2.patch (or -0.15.1 on nixpkgs' Hyprland)

sudo cat /sys/module/nvidia_drm/parameters/modeset      # Y
```

If the `leasable:` lines are missing, the old Hyprland is still running. If
the rule is missing, check that it is in the file Hyprland actually reads.

## How it was tested

A scratch flake imported the module from a copied directory, with a pinned
nixpkgs and `inputs.hyprland = github:hyprwm/Hyprland/efb50993` and its
NixOS module:

- the system evaluates with the flake's `hyprland` and with nixpkgs'
  `hyprland` as `basePackage`;
- `programs.hyprland.package` builds with both (the flake base with the glaze
  fix above; the nixpkgs base picks the 0.15.1 aquamarine patch), and each
  Hyprland binary links the patched aquamarine and contains the lease code;
- all 273 of Hyprland's unit tests pass on the rebased branch, the three
  lease tests included.

The 0.56.2 build has run as a live desktop compositor and carried every lease
stage of [`rig/TESTING-RIG.md`](../../rig/TESTING-RIG.md) ("Group B").

The patch was then extended (2026-09-30: `lessee`, `lease_interval`,
`revokelease`, and clearing the planes when a lease ends). That version builds
with the Hyprland flake at `efb50993` and the patched aquamarine, and passes
282 unit tests (one, resolving a user name, skipped where the build has no
user database); it has not yet run as a desktop or carried a lease.
