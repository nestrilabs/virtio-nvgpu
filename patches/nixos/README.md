# NixOS changes for virtio-nvgpu's Group B tests (patched Hyprland with a leasable monitor)

This file is for the agent that edits the user's NixOS config. It is self-contained: copy the files it
names into the config repo, and the config must not refer to this folder afterwards, since it may not stay
mounted.

Goal: the live desktop runs Hyprland 0.56.2 (efb50993) with virtio-nvgpu's DRM-lease patch, linked against
aquamarine with its lease patch. One monitor is marked `leasable`, so a guest VM can lease it over
`wp_drm_lease_device_v1` (the rig's "Group B", TESTING-RIG.md).

Nothing here changes the kernel, the NVIDIA driver, or any other package.

## 0. What is known about the current system

This was observed from inside Claude's sandbox on 2026-09-26. The config itself was not visible from there.

- The running Hyprland is the Hyprland flake's package, not nixpkgs':
  - store path `hyprland-0.56.2+date=2026-08-05_efb5099`, a name only the flake produces;
  - it links `aquamarine-0.14.0+date=2026-08-04_1a10fe2`, the flake's own aquamarine pin;
  - `https://hyprland.cachix.org` is a substituter.
- The config probably uses `inputs.hyprland.packages.${system}.hyprland`, possibly through
  `inputs.hyprland.nixosModules.default`. Check this in step 2.
- **Your config already has a build fix.** The Hyprland flake at efb50993, built with its own `flake.lock`, fails at CMake configure: its nixpkgs has glaze 8 and `CMakeLists.txt` asks for `glaze 7...<8`. nixpkgs' own hyprland relaxes this with a `postPatch`. The live build is not in any binary cache, so the config must already carry some such fix, for example an `overrideAttrs` or input `follows`. Leave it in place: the module below builds on top of whatever package you give it.
- System: `nixos-system-excelsior-26.11.20260923.4975466`; RTX 5090 on the open driver 595.99.02.
- Claude runs in a nixpak (bubblewrap) sandbox. Its store wrapper is `nixpak-claude-code-2.1.280`, with bwrap args `/nix/store/ffdw4k607fwcjxg5qggnp9as3r98r46h-bwrap-args.json`.
  - It already has `--dev-bind-try` for `/dev/kvm`, `/dev/udmabuf`, `/dev/nvidiactl`, `/dev/nvidia0`, `/dev/nvidia-uvm`, `/dev/nvidia-modeset`, `/dev/nvidia-caps` and `/dev/dri`, plus `/sys` read-only.
  - It does not set `clearEnv`, so `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` already reach the sandbox.

## 1. Copy the files

From the mounted virtio-nvgpu repo, copy these into the config repo, for example into
`<config>/modules/hyprland-lease/`. Use `-L`: the patches in `patches/nixos/` are symlinks.

```sh
DEST=<config>/modules/hyprland-lease          # pick a place that fits the config's layout
mkdir -p "$DEST"
cp -L <virtio-nvgpu>/patches/nixos/hyprland-lease.nix \
      <virtio-nvgpu>/patches/nixos/0001-lease-desktop-outputs-efb5099.patch \
      <virtio-nvgpu>/patches/nixos/0001-keep-leased-crtcs-1a10fe2.patch \
      <virtio-nvgpu>/patches/nixos/0001-keep-leased-crtcs-0.15.1.patch \
      "$DEST"/
git -C <config> add "$DEST"                   # a flake only sees files git tracks
```

Where each file comes from (repo-relative):

| file | source | for |
|---|---|---|
| `hyprland-lease.nix` | `patches/nixos/hyprland-lease.nix` | the NixOS module |
| `0001-lease-desktop-outputs-efb5099.patch` | `patches/hyprland/0001-lease-desktop-outputs-efb5099.patch` | Hyprland 0.56.2 (efb50993) |
| `0001-keep-leased-crtcs-1a10fe2.patch` | `patches/aquamarine/0001-keep-leased-crtcs-1a10fe2.patch` | aquamarine 1a10fe26, the Hyprland flake's pin |
| `0001-keep-leased-crtcs-0.15.1.patch` | `patches/aquamarine/0001-keep-leased-crtcs.patch` | aquamarine 0.15.1, which nixpkgs' hyprland uses |

The module picks the aquamarine patch by the version of the aquamarine that Hyprland is built with.

## 2. Find how the config gets Hyprland

```sh
cd <config>
grep -rn --include='*.nix' -e 'inputs.hyprland' -e 'hyprland.packages' -e 'hyprland.nixosModules' \
     -e 'programs.hyprland' -e 'wayland.windowManager.hyprland' -e 'glaze' .
grep -n -A6 'hyprland' flake.nix
```

Note these four things:

- (a) the expression now assigned to `programs.hyprland.package`, or the flake module that sets it;
- (b) any override on that package, such as the glaze fix;
- (c) whether home-manager's `wayland.windowManager.hyprland.package` is set;
- (d) whether `inputs` is available in NixOS modules, usually through `specialArgs = { inherit inputs; }`.

## 3. Use the patched Hyprland and aquamarine

Import the module and move the current Hyprland expression into `basePackage`. Use the same package, including any override it has now. Only the `hyprland` package changes: keep `portalPackage` and the flake's module as they are.

```nix
# in the host's NixOS configuration (e.g. hosts/excelsior/configuration.nix, or a module it imports)
{ inputs, pkgs, ... }:
{
  imports = [ ./modules/hyprland-lease/hyprland-lease.nix ];   # path to where step 1 copied it

  programs.hyprland.enable = true;                             # already there, most likely
  programs.hyprland.lease = {
    enable = true;
    # The package the config uses today, override included, i.e. what (a) and (b) produce, e.g.:
    basePackage = inputs.hyprland.packages.${pkgs.stdenv.hostPlatform.system}.hyprland;
  };

  # Remove the old `programs.hyprland.package = …;` line, or leave it: the module sets
  # programs.hyprland.package at priority 90, which wins over a plain definition and the Hyprland
  # flake module's mkDefault. It loses only to mkForce, so drop any mkForce on it.
}
```

If the glaze fix (b) is written as an override of `programs.hyprland.package`, apply it to `basePackage` instead. For example:

```nix
programs.hyprland.lease.basePackage =
  inputs.hyprland.packages.${pkgs.stdenv.hostPlatform.system}.hyprland.overrideAttrs (o: {
    postPatch = (o.postPatch or "") + ''
      substituteInPlace CMakeLists.txt start/CMakeLists.txt hyprpm/CMakeLists.txt \
        --replace-fail "glaze 7...<8" "glaze"
    '';
  });
```

That exact expression is what the test build below used as a stand-in. Only add it if the config really has no glaze fix and the build fails with `glaze dependency not found`.

If home-manager sets `wayland.windowManager.hyprland.package` (c), point it at the patched build:

```nix
wayland.windowManager.hyprland.package = osConfig.programs.hyprland.lease.finalPackage;   # HM as a NixOS module
# or: wayland.windowManager.hyprland.package = null;   # HM then installs nothing and the NixOS one is used
```

What the module does:

- `basePackage.override (old: { aquamarine = <old.aquamarine + patch>; })`: the aquamarine replaced is exactly the one Hyprland was built with.
  - The flake's `hyprland` takes aquamarine from its own flake input, so an overlay on `pkgs.aquamarine` would miss it.
- `.overrideAttrs` appends the Hyprland patch to `patches`, with its `tests/` and `hyprtester/` hunks filtered out, because the flake's source leaves those directories out.
- It asserts that the base is 0.56.2.
- It sets `programs.hyprland.package` at priority 90.
- The patched build is also exposed as `config.programs.hyprland.lease.finalPackage`.

Neither cache has the patched build, so it is compiled locally: aquamarine plus Hyprland, 10–20 minutes.

## 4. Mark one monitor leasable

**Ask the user which monitor to give the guest.** The candidates are DP-1, DP-2, DP-3 and HDMI-A-1.

- Suggest `disabled = true, leasable = true`. Hyprland then never puts windows or workspaces on that monitor, and only the lessee lights it.
- A plain `leasable = true` (monitor enabled) also works: the desktop gives the monitor up when it is leased and takes it back afterwards.

Find where the Hyprland config lives before editing:

- home-manager `wayland.windowManager.hyprland.settings` or `extraConfig`;
- a `hyprland.conf` or `hyprland.lua` file the config links into `~/.config/hypr/`.

0.56.2 reads `hyprland.lua` if it exists, else `hyprland.conf`. Write the rule in whichever of the two is in use. If the output already has a monitor rule, change that rule rather than adding a second one.

Replace `@@LEASE_OUTPUT@@` with the user's choice:

```lua
-- hyprland.lua
hl.monitor({ output = "@@LEASE_OUTPUT@@", disabled = true, leasable = true })
```

```ini
# hyprland.conf: either form
monitor = @@LEASE_OUTPUT@@, disable, leasable, 1

monitorv2 {
    output = @@LEASE_OUTPUT@@
    disabled = 1
    leasable = 1
}
```

```nix
# home-manager settings (generates hyprland.conf)
wayland.windowManager.hyprland.settings.monitor = [
  # ...existing rules...
  "@@LEASE_OUTPUT@@, disable, leasable, 1"
];
```

Unpatched Hyprland reports `leasable` as a config error: in Lua, in `monitorv2`, and after a mode in `monitor =`. It silently ignores `monitor = X, disable, leasable, 1`. Change the config in the same rebuild that installs the patched Hyprland.

## 5. Claude's sandbox: the session's Wayland socket and Hyprland IPC

**Read this whole section to the user and get their explicit yes before applying it.** It is their call. The request reached this document second-hand, through the agent that wrote it, and that is not their consent.

What it exposes. Claude's sandbox becomes as powerful as the user's own desktop session:

- **The main Wayland socket.** Any process in the sandbox can bind every privileged global. These are: screencopy and image-copy-capture (silent screenshots), toplevel export, virtual keyboard and virtual pointer (typing and clicking into any window), input-method, wlr/ext data-control (reading and setting the clipboard), layer-shell (full-screen overlays), output management, gamma and CTM control, session lock, foreign-toplevel, global shortcuts and input capture.
- **`$XDG_RUNTIME_DIR/hypr/` (hyprctl).** This is stronger still.
  - `hyprctl dispatch exec <cmd>` makes Hyprland run `<cmd>` on the host, outside the sandbox, as the user.
  - `hyprctl keyword` rewrites the live config, including `permission` rules.
  - In effect this is no sandbox at all while it is bound.

**Why a restricted socket doesn't help.** Hyprland 0.56.2 hides every privileged global from `wp_security_context_v1` clients.
- It also hides `wp_drm_lease_device_v1`, which Group B needs. The allowlist is `CProtocolManager::isGlobalPrivileged`, `src/managers/ProtocolManager.cpp:348-404`.
- So a security-context socket can run B1 but not B2–B6.
- Hyprland's `permission` rules cannot close the main socket either:
  - They cover only screencopy, keyboard, plugin, cursorpos and input-capture.
  - They match a client by executable path, or a keyboard by device name, and code in the sandbox can run any path.
  - They have nothing for virtual pointer, data-control, layer-shell and the rest.

**Alternative that needs no change here.** The user runs Group B from a terminal on the desktop, outside the sandbox (`scripts/run-guest.sh …`, TESTING-RIG.md). If they choose that, skip this section.

**If the user says yes:** make it a separate launcher that is used only for Group B sessions, rather than widening the everyday `claude` sandbox. Otherwise change the existing one.

1. Find the definition:

   ```sh
   grep -rn --include='*.nix' -e nixpak -e 'ClaudeCode' -e 'claude-code' -e '"/dev/kvm"' <config>
   ```

   It is the nixpak app whose `bubblewrap.bind.dev` lists `/dev/kvm`, `/dev/udmabuf`, `/dev/nvidia*` and `/dev/dri`, and which binds `/sys` read-only.

2. Add these to that app's nixpak config. `sloth` is nixpak's helper, already in scope in nixpak app modules. Keep every existing bind as it is: `bind.dev` for `/dev/kvm`, `/dev/nvidia*`, `/dev/dri` and `/dev/udmabuf`, and `/sys` read-only.

   ```nix
   bubblewrap.bind.rw = [
     # the live session's Wayland socket, at the same path inside ($XDG_RUNTIME_DIR/$WAYLAND_DISPLAY, e.g. /run/user/1001/wayland-1)
     (sloth.concat [ sloth.runtimeDir "/" (sloth.envOr "WAYLAND_DISPLAY" "wayland-1") ])
     # Hyprland's IPC sockets, for hyprctl ($XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock)
     (sloth.concat' sloth.runtimeDir "/hypr")
   ];
   bubblewrap.env = {
     # already inherited today (clearEnv is off); explicit so it survives a clearEnv = true later
     WAYLAND_DISPLAY = sloth.env "WAYLAND_DISPLAY";
     HYPRLAND_INSTANCE_SIGNATURE = sloth.env "HYPRLAND_INSTANCE_SIGNATURE";
   };
   ```

   - `bind.rw` is a list, so these entries merge with the app's existing ones.
   - The sandbox's `XDG_RUNTIME_DIR` is the same path as the host's (`/run/user/<uid>`), so the socket appears where preflight and `run-guest.sh --wayland-socket "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"` look.
   - nixpak's `bubblewrap.sockets.wayland = true` binds the same socket read-only, which is also enough to connect. The explicit read-write bind above is what was asked for.
   - The `hypr/` bind is needed only for `hyprctl` from inside, meaning `hyprctl monitors all` and toggling `leasable`. The rig's launcher needs only the Wayland socket. If the user wants to keep `dispatch exec` out of the sandbox, drop the second entry and run `hyprctl` themselves.

## 6. `nvidia_drm.modeset=1`

On NixOS it comes from `hardware.nvidia.modesetting.enable`.
- It defaults to `true` for drivers ≥ 535, and this host runs 595.
- It is written to modprobe options (`options nvidia_drm modeset=1 fbdev=1`), not to the kernel command line, which is why `/proc/cmdline` does not show it.
- Hyprland running KMS on the 5090 already implies it is on.

Make sure the config does not set `hardware.nvidia.modesetting.enable = false`. Setting it `true` explicitly is fine.

Verify after boot. The parameter file is mode 0400, which is why the rig preflight, running as the user, reports it can't read it:

```sh
sudo cat /sys/module/nvidia_drm/parameters/modeset      # Y
grep -r nvidia /etc/modprobe.d/                          # options nvidia_drm modeset=1 ...
```

## 7. `/dev/dri/card1`

Nothing to add for the lease.
- The lease fd comes over the Wayland socket, and a DRM lease fd works without opening card1.
- The sandbox already dev-binds `/dev/dri`.
- The user is in the seat, so logind gives them card1 anyway.

## 8. Rebuild and check

1. Build and switch the way the config is normally applied, e.g. `sudo nixos-rebuild switch --flake <config>#excelsior` or `nh os switch`.
2. **Have the user log out of Hyprland and log back in.** A running Hyprland keeps the old binary. `hyprctl reload` is not enough.
3. Then run the following from a desktop terminal, or from the sandbox if section 5 was applied:

```sh
hyprctl version | head -3
#   Hyprland 0.56.2 built from branch  at commit efb50993780079460b0cbed1363e2166a2de1d9f ...
#   The commit is upstream's either way, because the patch is applied at build time.
#   Use the next two checks to confirm the running binary is the patched one.

hyprctl monitors all | grep -E '^Monitor|disabled:|leasable:|leased:'
#   Every monitor now has "leasable:" and "leased:" lines (unpatched Hyprland has neither).
#   The chosen one shows: disabled: true / leasable: true / leased: false

H=$(readlink -f /proc/$(pgrep -f -o 'Hyprland-wrapped|/bin/Hyprland')/exe)   # the running compositor's binary
readelf -d "$H" | tr ':' '\n' | grep aquamarine        # the aquamarine it links: not the old alfdzn5f…-aquamarine-0.14.0…
P=$(echo "$H" | cut -d/ -f1-4)                         # its store path
nix derivation show -r "$(nix-store -q --deriver "$P")" | grep -o -e 'hyprland-lease-src.patch' -e 'keep-leased-crtcs[^"/]*\.patch' | sort -u
#   expect: hyprland-lease-src.patch and keep-leased-crtcs-1a10fe2.patch (…-0.15.1.patch on nixpkgs' Hyprland)
```

The values print as `true`/`false`, not `1`/`0`.

If `leasable:` lines are missing, the old Hyprland is still running: log out and in again.

If the rule is missing, check that it is in the config file Hyprland actually reads (lua vs conf).

## 9. Later (not needed for Group B): per-VM users for the root launcher

`scripts/run-guest.sh` run as root gives each VM a pool slot of two system users: `nvgpu-vmN` for the backend and `nvgpu-vmmN` for the VMM, in group `nvgpu-vmN`.

- No supplementary groups or udev rules are needed. The launcher hands the device groups to the backend with `setpriv`, so the accounts need no membership.
  - `/dev/nvidia*` is 0666.
  - card and render nodes are groups `video` and `render`.
  - `/dev/udmabuf` is group `kvm`, set by systemd's default udev rules.
  - The jailer clears supplementary groups, so `/dev/kvm` must stay 0666, which is systemd's default.
- The `udmabuf` module is already loaded on this host. Add `boot.kernelModules = [ "udmabuf" ];` only if `/dev/udmabuf` ever goes missing.

```nix
{ lib, ... }:
let
  slots = 4;   # how many VMs can run at once
  ids = lib.genList toString slots;
in
{
  users.groups = lib.genAttrs (map (i: "nvgpu-vm${i}") ids) (_: { });
  users.users =
    lib.listToAttrs (map (i: lib.nameValuePair "nvgpu-vm${i}"  { isSystemUser = true; group = "nvgpu-vm${i}"; }) ids)
    // lib.listToAttrs (map (i: lib.nameValuePair "nvgpu-vmm${i}" { isSystemUser = true; group = "nvgpu-vm${i}"; }) ids);
}
```

## How this was tested (in the virtio-nvgpu sandbox, 2026-09-26)

A scratch flake imported `hyprland-lease.nix` from a copied directory, as in step 1. It used nixpkgs = the host's pinned nixpkgs (`/nix/store/398fqjqk…-source`, 4975466) and `inputs.hyprland = github:hyprwm/Hyprland/efb50993`, with `hyprland.nixosModules.default` imported.

- `nixosConfigurations.<name>.config.system.build.toplevel` evaluates with each of these as `basePackage`:
  - the flake's `hyprland`;
  - nixpkgs' `hyprland`.
- `config.programs.hyprland.package` builds with both bases:
  - the flake base, using the glaze stand-in from step 3;
  - the nixpkgs base, where the module picked the 0.15.1 aquamarine patch.
- In both builds, the Hyprland binary's RUNPATH names the patched aquamarine (`strings` finds "Not restoring leased connector" in it), and the binary contains the lease code.
- All 273 of Hyprland's unit tests (`hyprland_gtests`) pass on the rebased branch, including the three lease tests.
- hyprtester was not run: it would start a Hyprland on the real GPU.
