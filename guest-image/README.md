# guest-image: the test guest's root filesystem

A reproducible, nix-built ext4 root for the on-device stages in
[`TESTING.md`](../TESTING.md). It carries NVIDIA's 595.99.02 userspace (the
host's release, to the digit), the tools each stage runs, this repo's
`nvgpu-wl-guest` and verify helpers, and one probe script per stage that runs
as the guest's init and powers the VM off when it is done.

```sh
guest-image/mkimage.sh                 # -> .rig/guest/rootfs.ext4 (golden; boot a copy)
guest-image/mkimage.sh --module-only   # swap in .rig/kernel/nvgpu.ko, in place, seconds
guest-image/mkimage.sh --probes-only   # copy probes/*.sh into the image, in place
```

A full build is about 10 s warm (a few minutes cold: the 420 MB `.run`, a few
crates, weston's clients and the rest from cache.nixos.org). No root: the store
paths are copied from where the store keeps them (`~/.local/share/nix/root` for
a chroot store, else `/nix/store`; `NIX_STORE_PHYS` overrides), and
`mkfs.ext4 -d` runs in a user namespace where the caller is uid 0, so every
file in the image is `root:root`. Nothing is setuid.

## Layout

| path | what |
|---|---|
| `flake.nix`, `flake.lock` | the pinned nixpkgs and the guest root (`packages.x86_64-linux.guestRoot`) |
| `nix/nvidia.nix` | 595.99.02 via `nvidiaPackages.mkDriver` (userspace: no module, firmware, settings or persistenced; 64-bit only), and the `/run/opengl-driver` tree: the driver plus egl-wayland, egl-wayland2, egl-gbm and egl-x11. **No mesa**: the guest must use NVIDIA's ICDs and fail loudly without them. |
| `nix/nvgpu-wl-guest.nix` | the daemon, from the workspace `mkimage.sh` stages |
| `nix/tools.nix`, `tools/*.c` | the helpers below, plus `sec-negative` and `lease-flip` from `scripts/verify` built against nix's libdrm |
| `probes/*.sh` | the init scripts, installed to `/opt/nvgpu` |
| `mkimage.sh` | build, stage, image |

Outputs, all under `.rig/` (git-ignored): `.rig/guest/rootfs.ext4`,
`.rig/guest/result` (the guest root's out-link, a GC root),
`.rig/guest/flake/` (the staged flake), `.rig/logs/guest-image/` (build and
e2fsck logs).

A flake sees only its own tree, so the repo's sources reach it through
`mkimage.sh`: it copies this directory to `.rig/guest/flake/` with a filtered
`nvgpu-src/` beside it (`Cargo.toml`, `Cargo.lock`, the workspace members
without `target/`, and `scripts/verify`). Building `guest-image/` directly
works too, but without `nvgpu-wl-guest` and the verify helpers
(`/etc/nvgpu/manifest` says so).

## In the image

- `/etc/nvgpu/sw` → the tools' `buildEnv` (on `PATH`, also `/usr/bin/*`):
  bash, coreutils, util-linux, kmod, procps, findutils, grep, sed, gawk, jq,
  pciutils, strace, gdb; libdrm's test programs (`modetest`, `proptest`),
  `drm_info`, `kmscube`; `vulkaninfo`, `vkcube` (`--wsi wayland|display`);
  mesa-demos (`eglinfo`, `eglgears_wayland`, `es2gears_wayland`);
  `wayland-info`; weston's simple clients (`weston-simple-egl`, `-shm`,
  `-damage`, `-dmabuf-egl`, `-dmabuf-feedback`; nixpkgs' weston builds none);
  sway, swaybg, Hyprland 0.56, seatd, foot, wlr-randr; `nvidia-smi`;
  `nvgpu-wl-guest`.
- `/etc/nvgpu/opengl-driver` → the driver tree; `/run/opengl-driver` points
  at it (re-made at boot, since `/run` is a tmpfs). `/etc/egl/egl_external_platform.d`
  → its external platforms. `probes/env.sh` pins `VK_DRIVER_FILES`,
  `__EGL_VENDOR_LIBRARY_FILENAMES` and `GBM_BACKENDS_PATH` to NVIDIA's files.
- `/opt/nvgpu/bin`: `nvgpu-lease` (a `wp_drm_lease_device_v1` client: takes a
  lease through the daemon and runs a KMS tool with the fd),
  `vk-acquire-display` (`vkGetDrmDisplayEXT` → `vkAcquireDrmDisplayEXT` →
  display-plane surface → N presented frames), `cuda-smoke` (CUDA driver API:
  a 16 MiB round trip and a PTX-JIT kernel, `libcuda` dlopen()ed, no toolkit),
  `nvgpu-poweroff`. `/opt/nvgpu/libnvgpu-shim.so` is an `LD_PRELOAD` that
  (a) opens the lease fd for the path `/dev/dri/lease`, so `kmscube -D`,
  `drm_info` and `modetest -D` drive a lease, and (b) makes `modetest -D PATH`
  open a path: libdrm treats `-D` as a *bus id*, so `modetest -D /dev/dri/card0`
  as written in TESTING.md and `kms-smoke.sh` otherwise finds nothing.
- `/opt/nvgpu/verify/`: `scripts/verify/*.sh`, with `bin/sec-negative` and
  `bin/lease-flip` already built.
- `/opt/nvgpu/nvgpu.ko`: the guest module, put in at image time (never by
  nix), so a module change is `--module-only`.

## Probes

Boot with `init=/opt/nvgpu/<probe>.sh` (`run-guest.sh <probe>.sh`). Each
mounts proc/sys/dev/pts/shm/run/tmp, loads the module (passing
`virtio_gpu_nv.<param>=<v>` tokens from the kernel command line to insmod),
waits for `/dev/nvidiactl`, prints the driver's dmesg lines, runs its stage
with every step under a timeout, checks guest dmesg for an oops/WARN, and
powers off. A watchdog powers off a probe that overruns its budget.

| probe | TESTING.md | launch with |
|---|---|---|
| `stage1.sh` | 1 (+0.1): HELLO v2, nodes, `guest-check.sh` | any mode |
| `render.sh` | `nvidia-smi -L`/`-q`, `vulkaninfo --summary`, `eglinfo -B` per platform, `cuda-smoke` | any mode |
| `wayland.sh` | 3 (+8.1): daemon, `wayland-info`, `vkcube --wsi wayland` under `WAYLAND_DEBUG` (modifiers, syncobj use), weston/mesa-demos EGL clients (two fullscreen: direct-scanout candidates), shm clients | `--wayland-socket` |
| `lease.sh` | 4 (+9 round trip): `nvgpu-lease --list`, `lease-flip` on the lease fd, `drm_info`, `modetest`, `kmscube` on it, a second lease | `--wayland-socket … --wayland-lease` |
| `vkdisplay.sh` | 5: `vk-acquire-display` on the lease (or the card), then `vkcube --wsi display` | as lease, or `--kms-card` |
| `compositor.sh` | 6: `guest-check.sh`, `kms-smoke.sh`, `drm_info`, `lease-flip` on the card, then sway (or Hyprland) on it with a client, and a master-arbitration check | `--kms-card` |
| `export.sh` | 7, guest side: the compositor + `nvgpu-wl-guest --export`; PASS when a host client's window appears | `--kms-card --wayland-export PATH` |
| `secneg.sh` | security negatives: `sec-negative.sh`, plus the KMS tests on the card or on a lease | any; KMS tests need a card or lease |
| `shell.sh` | an interactive shell on `hvc0` | any |
| `nodev.sh` | none: the image with **no** virtio-nvgpu device (QEMU/TCG, `.rig/tcg-smoke.sh`). insmod/rmmod, the NVIDIA userspace failing cleanly (`nvidia-smi`, `vulkaninfo` finding the ICD, `cuda-smoke`), `nvgpu-wl-guest`, the module's probe-failure path on a virtio-rng decoy (`nvgpu_decoy=0` skips it). FAILs on purpose when a device is there | QEMU, no backend |

Arguments are kernel command-line tokens (`nvgpu_<key>=<value>`; with
`run-guest.sh`, put them in `NVGPU_CMDLINE_EXTRA`):
`nvgpu_timeout` (probe budget, s), `nvgpu_secs` (timed steps), `nvgpu_frames`,
`nvgpu_connector`, `nvgpu_card`, `nvgpu_comp=sway|hyprland`,
`nvgpu_wlr_renderer`, `nvgpu_lease_gap`, `nvgpu_secneg_kms=auto|card|lease|none`,
`nvgpu_wl=1` (shell: start the daemon), `nvgpu_hold=1` (a shell instead of
poweroff at the end), `nvgpu_loglevel`, `nvgpu_dryrun=1` (skip the module).

Output: one `[<probe>] PASS|FAIL|SKIP <what>` line per check, and last
`NVGPU_PROBE_DONE probe=<p> result=PASS|FAIL pass=N fail=N skip=N`
(`result=FAIL reason=timeout` from the watchdog). The console log, dmesg and
the tools' own logs (daemon, compositor, the `WAYLAND_DEBUG` trace) stay in
the run's copy of the image under `/var/log/nvgpu/`:
`debugfs -R 'cat /var/log/nvgpu/wayland.log' <run copy>` (`run-guest.sh` keeps the copy with `NVGPU_KEEP_ROOTFS=1`).

The budgets default to at most 170 s because `run-guest.sh` kills the VMM
after `NVGPU_TIMEOUT` (180 s); `export.sh` waits `nvgpu_secs` (120) for a host
client inside that. Raise both together (`nvgpu_timeout=…`).

## Changing the driver release

`nix/nvidia.nix`: `version` and `sha256_64bit` (prefetch the `.run` with
`nix store prefetch-file --hash-type sha256 <url>`). The guest userspace must
equal the host kernel module's release, and the backend needs `gen/` tables
for it.
