# Running the display stages on the dev box

[`TESTING.md`](../TESTING.md) says what each stage checks and what a pass looks
like. This file says **in what order to run them on this machine**, with the
unprivileged launcher, and **which ones touch the monitors**.

The machine: AMD host (`kvm_amd`), an **RTX 5090 on the open 595.99.02 modules
with every monitor plugged into it**, an AMD iGPU with none, and the user's
Hyprland desktop running on the 5090. So there is no spare card and no spare
monitor. Every stage that does KMS takes a monitor away from the desktop, or
needs the desktop stopped. The stages are grouped below by that cost.

**Before the first GPU stage, know what can reach the desktop and what to do
if it freezes.** This host has `panic_on_oops=1` and `panic=0`, so a host oops
freezes the machine until you power it off. Save your work, keep an SSH
session open from another machine (the iGPU drives no monitor), and watch
`journalctl -kf` there for `Xid`, `NVRM` and `nvidia` lines. The rig's own
notes on this (`.rig/SAFETY-NOTES.md`, git-ignored, host-specific) go further.

## The rig

Everything is built into `.rig/` (git-ignored), laid out as
`rig/run-guest.sh` expects when it is not run as root:

| path | what |
|---|---|
| `.rig/bin/vhost-user-nvgpu` | the backend (release) |
| `.rig/bin/nesbox` | the VMM (release) |
| `.rig/bin/crosvm` | the other VMM, `--vmm crosvm` (release, static; below, "crosvm") |
| `.rig/bin/virtiofsd` | only with `NVGPU_NVIDIA_SHARE` |
| `.rig/kernel/vmlinux`, `.rig/kernel/nvgpu.ko` | guest kernel 7.2.7 (ELF `vmlinux`: nesbox enters it at `startup_64` with a `boot_params` page; QEMU, for the TCG smoke, through its PVH note), and the module built against it (Kbuild names it `virtio_gpu_nv.ko`; the rig installs it as `nvgpu.ko`) |
| `.rig/kernel-rust/vmlinux`, `.rig/kernel-rust/nvgpu.ko` | the same kernel with `CONFIG_RUST=y`, and the module with its parsers in Rust (`NVGPU_RUST=1`), built by `rig/rig-build-kernel-rust.sh`; to run one, see [`driver/rust/README.md`](../driver/rust/README.md) |
| `.rig/guest/rootfs.ext4` | the golden image: NVIDIA 595.99.02 userspace at `/run/opengl-driver`, probes at `/opt/nvgpu/<name>.sh`, the module at `/opt/nvgpu/nvgpu.ko` |
| `.rig/logs/` | one `<tag>.{backend.log,console.log,json}` per run |

Rebuild the backend with:

```sh
export NIX_CONFIG="experimental-features = nix-command flakes"
CARGO_TARGET_DIR=$PWD/.rig/target nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c \
  cargo build --release -p device --features vhost-user --bin vhost-user-nvgpu
install -m 0755 .rig/target/release/vhost-user-nvgpu .rig/bin/
```

A run boots a **copy** of the golden image and deletes it afterwards
(`NVGPU_KEEP_ROOTFS=1` keeps it), so a probe never leaves anything behind for
the next run. The probe is the guest's init. It prints `[probe] PASS|FAIL|SKIP`
lines, ends with `NVGPU_PROBE_DONE probe=<name> result=PASS|FAIL ...`, and
powers the VM off. `run-guest.sh` takes that verdict line as the result (the
tools a probe runs print PASS/FAIL lines of their own, some expected): 0 on
PASS, 1 on FAIL, and 124 when the guest did not power off in time
(`NVGPU_TIMEOUT`, 180 s by default). Without a verdict line it falls back to
counting PASS/FAIL words: 0 on a PASS and no FAIL, 1 on any FAIL, 2 on
neither. The other knobs are listed in
`rig/run-guest.sh --help`.

The rig runs one VM, as you: without root there is no other user to be, so
the backend, nesbox and your desktop share a uid (`rig/run-guest.sh`'s
header has what that gives up against a root run, where each VM takes users
of its own from a pool, and SECURITY.md §4 has what the uids separate). What
still applies: the backend's sandbox -- a user and network namespace of its
own, Landlock, seccomp (`device/src/sandbox.rs`) -- and nesbox leaving the
host's network (`"unshare-network"` in the run's config). A missing kernel
feature shows as `sandbox: DEGRADED` in `<tag>.backend.log`, and preflight
warns about both halves. To rule them out when something breaks:
`NVGPU_SANDBOX=off` for the backend, `NVGPU_VMM_NETNS=0` for nesbox. A
backend that meets a syscall its list lacks stops with status 159 and the
line `sandbox: syscall N is not on the seccomp allowlist` in its log; that
number, and the stage that hit it, are what to report.

To see which probes the image has:

```sh
nix shell nixpkgs#e2fsprogs -c debugfs -R 'ls -l /opt/nvgpu' .rig/guest/rootfs.ext4
```

`run-guest.sh stage1` boots `init=/opt/nvgpu/stage1.sh`. The image's probes
(`rig/guest-image/README.md` has what each checks) and the stages they cover:

| probe | stages |
|---|---|
| `stage1` | 1 (+0.1) |
| `render` | 2's offscreen part (nvidia-smi, Vulkan, EGL, CUDA); not the envyhooks differential. CUDA runs with `--allow-compute`; without it the probe checks there is no UVM device and CUDA fails cleanly (`nvgpu_compute`, which run-guest.sh sets) |
| `wayland` | 3 and 8 (the `WAYLAND_DEBUG` trace shows the modifiers and syncobj use) |
| `lease` | 4, and 9's lease round trip |
| `vkdisplay` | 5 |
| `compositor` | 6 (`nvgpu_comp=hyprland` for Hyprland; sway by default) |
| `export` | 7 |
| `secneg` | the security negatives (`nvgpu_secneg_kms=none|card|lease`) |
| `shell` | anything without a probe (caching M-2, hotplug): a shell on the console |
| `nodev` | none: the image without a device, for QEMU and for nesbox without `gpu-forward` (the rig's own helpers, below) |

Probe arguments are kernel command-line tokens, passed with
`NVGPU_CMDLINE_EXTRA="nvgpu_<key>=<value> ..."`.

Before the first real run, it is worth booting the same kernel, image and
command line without a GPU: under QEMU (TCG, or KVM when `/dev/kvm` is there)
with the `nodev` probe, and under nesbox with KVM but no `gpu-forward` section,
so the VMM's own boot path, disk, console and power-off are proven before the
device is added. The dev box keeps its helpers for both in `.rig/` (git-ignored,
not part of the repository). Neither opens anything on the host GPU.

## Before anything: preflight

```sh
rig/rig-preflight.sh
```

Every line is OK, WARN or FAIL, with a hint. A missing `/dev/nvidia-uvm` is
only a WARN: it matters only to `--allow-compute` runs. It exits non-zero only on a FAIL.
Inside the Claude sandbox, the rig needs these bound in: `/dev/kvm`,
`/dev/nvidiactl`, `/dev/nvidia0`, `/dev/nvidia-uvm`, `/dev/nvidia-modeset`,
`/dev/dri`, optionally `/dev/udmabuf`, and `/sys`. The backend needs `/sys`
itself, not only the checks: it reads each GPU's
`/sys/bus/pci/devices/<addr>/{config,drm}`. With the desktop up, preflight
expects the WARN "a compositor is running", which is the point of this file.
It also gives the `panic_on_oops` WARN in its "recovery" section.

## Group A: safe with the desktop running

These stages start no KMS client. They make no modeset and hold no master. They
never connect to the live compositor. Run them first, in this order.

| # | stage | run | backend flags |
|---|---|---|---|
| A1 | **1**: HELLO v2, nodes, extensions | `rig/run-guest.sh stage1 s1` | none |
| A2 | **2, W1 only**: offscreen render (the envyhooks differential has no probe), twice: without compute (nvidia-smi, Vulkan and EGL with no UVM device; CUDA must find no device and exit cleanly), then with it (CUDA must run) | `rig/run-guest.sh render s2` then `rig/run-guest.sh --allow-compute render s2c` | none, then `--allow-compute` |
| A3 | **security negatives**, ctl + render tests (no `--kms`). Run only after A1 and A2 pass, with your work saved: a regressed fix can oops the host (T1-T3 reach RM and nvidia-drm if the backend's refusal is gone) | `NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none rig/run-guest.sh secneg sec` | none |
| A4 | **3** against a **separate headless compositor** | see below | `--wayland-socket <headless socket>` |
| A5 | **8**: explicit sync (all three parts) | as A4 | `--wayland-socket <headless socket>` |
| A6 | caching **M-2** (read-only mapping; no probe yet) | `rig/run-guest.sh shell m2`, by hand | none (`-- --keep-guest-coherency` only to rule the rewrite out) |
| A7 | performance, W1 crossings; W2 crossings against the headless compositor | as A2 / A4 | as A2 / A4 |
| A8 | **compat**: DRM ioctls at older and newer struct sizes, and 32-bit processes (below) | `rig/run-guest.sh compat cmp` | none |

H-4 and M-1 in the caching stage are Intel-only and cannot occur on this AMD
host (`TESTING.md`, "Caching and coherency").

### Compat: struct sizes and 32-bit processes (A8)

`probes/compat.sh` needs an image built since the `drm-compat2` branch (it
has NVIDIA's 32-bit userspace at `/run/opengl-driver-32` and the i686
tools); an older one fails its first 32-bit check and says to rebuild. No
display, no compute, no KMS: the same cost as A2.

| check | what it shows |
|---|---|
| `nvgpu-drm-compat` | SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE with the 16-byte `drm_syncobj_handle` (the Steam runtime's libdrm) and a 32-byte one work as the native 24-byte call does -- same kind of file, same syncobj, EXPORT/IMPORT_SYNC_FILE too -- and nothing past the caller's struct is written; SYNCOBJ_WAIT (32 bytes) and TIMELINE_WAIT (40) from before `deadline_nsec` answer as the native sizes do. Exit 77 (SKIP) on a backend without fences |
| `nvgpu-drm-compat-32` | the same from a 32-bit process: the DRM node's compat path, and DRM_IOCTL_VERSION through the core's compat conversion |
| `nvgpu-rm-smoke`, `nvgpu-rm-smoke-32` | an RM client on `/dev/nvidiactl` from each width (NV01_ROOT_CLIENT, GET_ATTACHED_IDS, GET_BUILD_VERSION with its string pointers below 4 GiB, RM_FREE) and nvidia-drm GET_DEV_INFO; their `RESULT` lines must match |
| `vulkaninfo-32 --summary`, `eglinfo-32 -B` | NVIDIA's 32-bit Vulkan ICD and EGL vendor, with the loaders pinned to them |

The C module is in the image; the Rust one goes in with
`mkimage.sh --module-only --module <.ko> --out <image>` as for any probe.
Afterwards, host `dmesg` clean and the backend still serving, as for A3.

### The headless compositor (A4, A5, A7)

Stage 3's guest clients need a host compositor. **Do not point them at the
live Hyprland socket yet.** Hyprland exposes virtual keyboard and pointer,
screencopy and data-control to any client. The proxy's allowlist hides those
(`wlwire/src/policy_table.rs`), and these stages are where it is first shown to
hold against a real compositor. So the guest gets a compositor of its own,
where a hole in the allowlist costs nothing:

```sh
# terminal 1: sway, headless, rendering on the 5090's render node
rig/rig-headless-sway.sh                  # --renderer gles2 to try the other one
# terminal 2:
rig/run-guest.sh --wayland-socket "$(cat .rig/run/headless-sway.socket)" wayland s3
```

`rig-headless-sway.sh` runs `nix shell nixpkgs#sway` (sway 1.12, wlroots 0.20)
with:

- `WLR_BACKENDS=headless` and one `HEADLESS-1` output;
- `WLR_RENDERER=vulkan` (or `gles2`);
- `WLR_RENDER_DRM_DEVICE` set to the NVIDIA render node, which it finds by PCI
  vendor `0x10de`;
- `--unsupported-gpu`;
- a private 0700 `XDG_RUNTIME_DIR`, with `WAYLAND_DISPLAY` unset so it cannot
  nest into the desktop.

Inside the sandbox there is no `/run/opengl-driver`. The script then points
EGL, GBM and Vulkan at the host's `graphics-drivers` store path for the loaded
driver version. It needs no seat and no card node. The plumbing (sway comes
up, the socket appears, the launcher and backend accept it) was checked with
`--renderer pixman` in the sandbox; A4 and A5 then ran green with the Vulkan
renderer on the 5090 (results under "crosvm", below).

What a headless compositor **can** check in stage 3:

- PASS 1: the modifier in `zwp_linux_buffer_params_v1.add`.
- PASS 2: the modifier matches `drm_info -j /dev/dri/card1`'s `IN_FORMATS`.
  Reading the card's properties needs no master.
- The client presents.
- Stage 8's syncobj timelines. sway offers `wp_linux_drm_syncobj_manager_v1`
  only when the renderer supports timelines. For the Vulkan renderer that means
  sync_file import/export and DRM syncobj timelines on the node. The headless
  backend supports them. Before A4, check that both
  `zwp_linux_dmabuf_v1` and the syncobj manager are there:
  `WAYLAND_DISPLAY=$(cat .rig/run/headless-sway.socket) nix shell nixpkgs#wayland-utils -c wayland-info`.
  With pixman, neither is.

What it **cannot** check: PASS 3, direct scanout. It has no planes. That check
is B1. Frame pacing against it is timer-driven, not vblank, so publish only
crossings from it.

The patched Hyprland cannot be this compositor. In 0.56, aquamarine's headless
backend has no allocator. It needs the DRM backend, meaning the card through a
seat, which the desktop holds. Or it needs a parent Wayland compositor. If a
stage needs Hyprland's own behaviour without the desktop's session, run it
nested inside the headless sway
(`WAYLAND_DISPLAY=<headless socket> Hyprland`). That setup is untested.

## crosvm

Every Group A stage also runs under crosvm, with its sandbox on: add
`--vmm crosvm` (or set `NVGPU_VMM_KIND=crosvm`). A2's `--allow-compute` half
needs a crosvm with the UVM aperture (patches `0007`-`0009`, branch
`virtio-nvgpu-compute`); the launcher refuses `--allow-compute` with a crosvm
whose `run --help` does not name the `nvgpu-uvm-aperture`. The kernel, image, probes,
backend and logs are the same; `<tag>.json` records crosvm's command line,
as it has no config file.

```sh
rig/rig-build-crosvm.sh                     # .rig/src/crosvm -> .rig/bin/crosvm
                                                # (first time: the top of that script)
rig/run-guest.sh --vmm crosvm stage1 cv-s1
rig/run-guest.sh --vmm crosvm render cv-render
rig/run-guest.sh --vmm crosvm --wayland-socket "$(cat .rig/run/headless-sway.socket)" wayland cv-wl
NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none rig/run-guest.sh --vmm crosvm secneg cv-sec
NVGPU_VMM_KIND=crosvm NVGPU_APPS_EXTRA=nvgpu_user=1 rig/rig-app-check.sh \
  typing,pointer,clipboard,glxgears,gamescope,gtk,qt,firefox,mpv,vkmark,chromegpu cv-apps
```

`.rig/src/crosvm` is upstream crosvm (c0474109d64d, 2026-09-25) on branch
`virtio-nvgpu`, with patches `0001`-`0006` of `patches/crosvm/`;
`.rig/src/crosvm-compute` is a worktree on branch `virtio-nvgpu-compute`,
all nine (patches/README.md says what each is for). The compute build goes
to its own binary, so the graphics one is left alone:

```sh
CROSVM_SRC=.rig/src/crosvm-compute CROSVM_OUT=.rig/bin/crosvm-compute \
  CARGO_TARGET_DIR=.rig/target-crosvm-compute rig/rig-build-crosvm.sh
NVGPU_VMM=.rig/bin/crosvm-compute rig/run-guest.sh --vmm crosvm ...
``` It is built static and without
crosvm's default features: no virtio-gpu, virgl, virtio-wl, audio, USB or
network devices.

The guest driver gives the GPU the host's own PCI address, so its bus must
be free in the guest. The launcher checks before it starts anything: a host
GPU on bus 0 (crosvm's root bus) is refused, and so is one on bus 1 when
`.rig/bin/crosvm run --help` has no `--no-pci-hotplug-port` (patch 0003),
as crosvm's hot-plug root port takes that bus.

What crosvm's sandbox does here, unprivileged: every device crosvm emulates
(the disk, both consoles, rng) runs as a process of its own, each in new
user, pid, mount and network namespaces with no uid mapped, pivoted into the
empty `.rig/run/crosvm-empty`, under its seccomp policy (`Seccomp: 2` in
`/proc/<pid>/status`). With `0009` so does the nvgpu vhost-user frontend,
under `vhost_user_frontend_device` (the `vu-nvgpu` process); without it the
frontend stays in crosvm's main process, as upstream has it. The launcher
starts the main process in a user and network namespace of its own
(`NVGPU_VMM_NETNS=0` does not), and its summary line says
`nvgpu frontend jailed` when it is. `NVGPU_CROSVM_SANDBOX=off` runs
`--disable-sandbox`, and says so at the top of the console log; the
frontend is then in the main process, and the main process's checks
(SECURITY.md §16) still hold.

Not under crosvm yet: a virtiofs share, and the root layout.

Results on the 5090 (2026-09-26), against nesbox's from the regression
before the hardening merge (nesbox `stage1` and `render` re-run on this tree
match them):

| probe | crosvm | nesbox |
|---|---|---|
| `stage1` | 6 pass, 0 fail, 0 skip | 6/0/0 |
| `render`, no compute | 9/0/1 (vkcube: no WSI) | 9/0/1 |
| `wayland`, headless sway | 11/0/3 (the three natives also skip) | 11/0/3 |
| `secneg`, `kms=none` | 10 passed, 3 KMS skips | the same |
| apps, as uid 1000 | 19/0/0; captures render; chrome://gpu hardware accelerated, `Sandboxed: true`, one GPU (the 5090) | all PASS |

All but `secneg` ran with `-- --rm-allowlist=log` (for the app pass,
`NVGPU_APPS_BACKEND_ARGS=--rm-allowlist=log`): the backend they ran then
refused RM class `NV01_MEMORY_LOCAL_PRIVILEGED` (0x3f) on ALLOC_MEMORY,
which the 5090's Vulkan driver allocates, so `vulkaninfo` failed under
either VMM without it. That was the backend's, not the VMM's, and `6d5f0f7`
put the class on the list: from it on, neither VMM should need the flag.

Chromium needs `NVGPU_SLOT=40` to get to chrome://gpu when it runs alone;
at 25 s its capture is still the black window it opens with, under either
VMM.

### Compute under crosvm

Built (`0007`-`0009`), unit-tested, and run on the 5090 (2026-09-26): with
`.rig/bin/crosvm` built from `virtio-nvgpu-compute`, `render` with
`NVGPU_COMPUTE=1` passes 9/0/1 with `cuda-smoke` all PASS, and `secneg` with
compute passes, the frontend jailed (the launcher's summary says `nvgpu
frontend jailed`); see "Regression of the merged tree", below. The aperture is
region 2 after the window in the window's 64-bit BAR (2 GiB in all with
compute), with a shared-memory capability of its own. The jailed frontend
checks each pool and hands it to the main process, which checks it again,
maps `/dev/nvidia-uvm` at the pool's own address over the band it reserved
at start-up ([4 GiB, 32 TiB)), checks the pages with `mincore`, and adds the
slot; withdrawal removes the slot, then puts the reservation back
(SECURITY.md §16). What to run, in this order, each against nesbox's result
on the same tree:

```sh
V=NVGPU_VMM=.rig/bin/crosvm-compute
env $V rig/run-guest.sh --vmm crosvm stage1 cvc-s1
env $V NVGPU_COMPUTE=1 rig/run-guest.sh --vmm crosvm render cvc-render   # cuda-smoke too
NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none env $V NVGPU_COMPUTE=1 \
  rig/run-guest.sh --vmm crosvm secneg cvc-sec
env $V rig/run-guest.sh --vmm crosvm render cvc-render-nocompute       # graphics only, as before
```

and a CUDA workload of more than one context (the render probe's
`cuda-smoke` is one): each context maps a pool, and the pools of two
processes must be placed and withdrawn without leaving anything behind.
What to look at besides the probes' results:

- the launcher's summary says `nvgpu frontend jailed`, and
  `/proc/<vu-nvgpu pid>/status` shows `Seccomp: 2` and `NoNewPrivs: 1`;
- crosvm's log (the console log) has `nvgpu: reserved the UVM pool band`
  and `nvgpu: nvidia-uvm is character major N`, then an `nvgpu uvm: ...
  at aperture offset` line per pool and a `dropped` line per withdrawal, and
  no `refused a memory request` line in a run that passes;
- the guest's dmesg names the UVM aperture (`UVM aperture at ...,
  1073741824 bytes`) and does not warn that it is not write-back;
- no `SIGSYS`/`seccomp` kill of the `vu-nvgpu` process (the host's
  `dmesg`/audit log has one line per kill, if any);
- without `NVGPU_COMPUTE=1` the BAR is 1 GiB and there is one
  shared-memory capability, as before.

## Group B: takes one monitor, desktop keeps running

These stages need the **patched Hyprland as the live compositor**
([`patches/README.md`](../patches/README.md); `.rig/hypr-build` has a build). One
monitor must be marked leasable. The lease takes only that monitor. The rest of
the desktop keeps running. Pick a monitor you can lose, and prefer
`disabled = true, leasable = true` so Hyprland never puts windows on it:

```lua
hl.monitor({ output = "DP-2", disabled = true, leasable = true })
```

`hyprctl monitors all` shows `leasable: true` for it. The launcher must see the
session's socket. Outside the sandbox that means running from a terminal in the
desktop. Inside the sandbox it means binding `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`
in: the sandbox's own runtime directory is a different one, and preflight will
not find the socket there. The launcher prints a WARNING when the socket is the
session's own. That is expected here.

| # | stage | run | backend flags |
|---|---|---|---|
| B1 | **3, PASS 3**: direct scanout (no lease; guest windows appear on the desktop) | `run-guest.sh --wayland-socket "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" wayland s3live` | `--wayland-socket` |
| B2 | **4**: lease, then kmscube / modetest / lease-flip | `run-guest.sh --wayland-socket "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" --wayland-lease lease s4` | `--wayland-socket --wayland-lease` (`--wayland-lease-interval 0` while iterating) |
| B3 | **5**: `vkAcquireDrmDisplayEXT` / `VK_KHR_display` | as B2, probe `vkdisplay` | as B2 |
| B4 | **security negatives with `--kms`** on the leased card | as B2, probe `secneg`, `NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=lease` | as B2 |
| B5 | **9**: lease round-trip (the host gets the output back, not dark) | as B2, probe `lease` (its second lease is the round trip) | as B2 |
| B6 | **2 / performance, W3** (display present) | as B2 | as B2 |

Run B1 only after A4 has shown the allowlist holds. Watch the leased monitor
after each B stage. If it stays dark after the guest lets go, that is the M-9
FAIL (stage 9). Toggle it back with `hyprctl keyword`/`hyprctl reload`, or
replug it.

Results on the 5090 (2026-09-26, Hyprland 0.56.2 `efb50993` patched, DP-3
leasable), under nesbox and crosvm alike: B1 `wayland` 13/0/1, with direct
scanout confirmed (with `render:direct_scanout` on, a fullscreen guest
`weston-simple-egl` was scanned out on a desktop monitor, no blocker for the
whole run); B2 `lease` 9/0/1; B3 `vkdisplay` 8/0/1; B4 `secneg` on the lease
6/0/0 (inside it, KMS on the lease 15 passed, 0 failed); B5 the lease round
trip, DP-3 back to Hyprland after each lease. The skips: `kmscube` fails with
EINVAL and `vkcube --wsi display` crashes, both the same on the host natively.
B6 (performance) has not run.

## Application pass on the live desktop

Real applications in a guest, as uid 1000 with the browsers' sandboxes on,
shown on the live Hyprland (Group B's setup: the patched Hyprland, DP-3
leasable). Nothing is typed or clicked into the desktop -- input and the
clipboard were verified against the headless sway -- and every guest window
goes to DP-3's workspace, silently and without focus. DP-3 is an OLED: the
harness powers it (DPMS) only while a slot has a window, and leaves it off.

```sh
# graphics only (nesbox; NVGPU_VMM_KIND=crosvm for crosvm), in three batches
export NVGPU_MEM_MIB=8192 NVGPU_APPS_EXTRA=nvgpu_user=1
NVGPU_TIMEOUT=700  rig/rig-app-check.sh --live glxgears,xterm,gamescope,gtk,qt,firefox,mpv,glmark2,vkmark,chromeanim a0
NVGPU_TIMEOUT=1100 rig/rig-app-check.sh --live blender,stk,stkgs,neverball,godot,godotgl,blendervk,gimp,inkscape,krita a1
NVGPU_TIMEOUT=1300 rig/rig-app-check.sh --live lo,loskia,gte,gtevk,qml,qmlvk,electron,element,ffgl,crgl,mpvvk,ffvkdec,ffvkenc a2
# compute (nesbox only)
NVGPU_COMPUTE=1 NVGPU_TIMEOUT=1600 NVGPU_APPS_EXTRA="nvgpu_user=1 nvgpu_compute=1" rig/rig-app-check.sh --live \
  cuda,opencl,cycles,nvenc,ffnvdec,vainfo,mpvnvdec,mpvvaapi,ffvaapi,crvaapi,godot c1
```

`rig/rig-app-check.sh --live` (its header has the details) finds each
window with `hyprctl clients -j`, captures the monitor and the window with
grim into `.rig/logs/<tag>/`, and follows the guest's `APP_START`/`APP_END`
lines, so a slot that fails fast does not put it behind. What each slot
checks is in `rig/guest-image/probes/apps.sh`; the apps' logs stay in the run's
disk under `/var/log/nvgpu/apps` (`NVGPU_KEEP_ROOTFS=1`). The media are
built into the image (`/opt/nvgpu/apps`): the guest has no network.

To tell a failure of ours from the app's, `rig/rig-native-run.sh` runs
the same program on the host with the guest image's own userspace and
NVIDIA 595.99.02 files, against the headless sway (or `--live`), and
`--no-uvm` hides `/dev/nvidia-uvm` as a graphics-only guest has it.

### Results (RTX 5090, 595.99.02, 2026-09-26)

Every app ran as uid 1000; every window appeared on DP-3 and every capture
was looked at and shows the app's real content. Tags: nesbox `g0`, `n1`,
`n2`, `c2`; crosvm `x0`, `x1`, `x2`. "native" = shown the same on the host
with `rig-native-run.sh`.

| app | API / path | nesbox | crosvm | notes |
|---|---|---|---|---|
| glxgears, xterm/xeyes | GLX / X11 through rootful Xwayland | PASS | PASS | |
| gamescope + vkcube | Vulkan, nested compositor | PASS | PASS | |
| gnome-calculator, qalculate-qt | GTK4, Qt6 | PASS | PASS | |
| glmark2, vkmark, mpv (testsrc) | GL (EGL Wayland), Vulkan | PASS | PASS | |
| Firefox (webgl2 page), Chromium (animation) | WebGL2, GPU compositing | PASS | PASS | |
| SuperTuxKart | GL 4.6, SDL2, native Wayland | PASS, 116 fps avg | PASS, 116 fps | profile race, 4 karts |
| SuperTuxKart in gamescope | GL in gamescope's Xwayland, Vulkan compositor | PASS | PASS | gamescope 3.16 segfaults tearing down after its child exits (see below); the slot outlasts the race |
| Neverball | SDL2 + GLX, Xwayland | PASS | PASS | glxinfo in the same server: NVIDIA RTX 5090 |
| Godot 4 (Forward+) | Vulkan 1.4, native Wayland | SKIP (native) / PASS with compute, 213 fps | SKIP (native) | no UVM: see below |
| Godot 4 (compatibility) | GL 3.3, native Wayland | PASS | PASS | |
| Blender 5.2 UI + EEVEE frame | GL 4.6 / Vulkan backend | PASS, 8.6 s / 3.0 s | PASS, 8.4 s / 3.2 s | one EGL_BAD_ALLOC at start in the first run, not seen again in five |
| Blender Cycles | CUDA / OptiX | PASS, 2.08 s / 2.61 s (native 1.21 / 1.73) | n/a (compute) | 128 spp 1280x720, default scene |
| GIMP 3.2, Inkscape 1.4 | GTK3 | PASS | PASS | |
| Krita 6 | Qt6, OpenGL canvas, through Xwayland | PASS, canvas vendor NVIDIA | PASS | Krita picks xcb itself |
| LibreOffice Writer | GTK3 VCL, native Wayland | PASS | PASS | |
| LibreOffice, X11 VCL | Skia on Vulkan (skia.log: vulkan, 0x10de) | PASS | PASS | the probe supplies the Vulkan loader nixpkgs' LibreOffice lacks |
| gnome-text-editor | GTK4, GSK ngl / vulkan | PASS | PASS | |
| qml runtime | Qt Quick on GL / Vulkan (RHI) | PASS, 100 / 109 fps | PASS | |
| Electron 43 app | Chromium GPU process, WebGL1/2, video | PASS: gpu_compositing, webgl, vulkan, webgpu enabled; ANGLE on the RTX 5090 | PASS | |
| Element | Electron | PASS | PASS | a "System unsupported" (no keyring) dialog, native too |
| Firefox, page | WebGL 1/2, video (software decode) | PASS | PASS | Firefox reports its sanitised "GTX 980, or similar" |
| Chromium, page | WebGL 1/2 (ANGLE), video | PASS | PASS | |
| mpv | Vulkan Video decode, H.264/HEVC/AV1 | PASS, all three | PASS | |
| ffmpeg decode | Vulkan Video: HEVC, AV1, VP9 | PASS | PASS | H.264 stalls before the end in 2 of 5 runs (native) |
| ffmpeg encode | Vulkan Video: h264_vulkan, av1_vulkan | PASS, 12.8x / 12.3x real time | PASS | hevc_vulkan hangs finishing, every run (native) |
| ffmpeg encode | NVENC h264/hevc/av1 (CUDA) | PASS, 486 / 427 / 464 fps, valid files | n/a (compute) | needed two allowlist additions |
| ffmpeg decode | NVDEC via `-hwaccel cuda`, 4 codecs | PASS | n/a (compute) | |
| vainfo; mpv `--hwdec=vaapi` | VA-API on NVDEC (nvidia-vaapi-driver, CUDA) | PASS | n/a (compute) | needed the guest driver's DUMB_BUFFER cap fix |
| mpv `--hwdec=nvdec` | NVDEC (CUDA) | PASS, all three | n/a (compute) | |
| Chromium VA-API | VaapiVideoDecoder on nvidia-vaapi-driver | PASS, powerEfficient=true | n/a (compute) | |
| Firefox VA-API | RDD process, sandbox on | SKIP (native) | n/a (compute) | vaInitialize fails under the RDD sandbox, natively too |
| nvgpu-nbody | CUDA runtime | PASS, 24.4 TFLOP/s (native 24.4), 14.4 GB/s pinned | n/a (compute) | needed three allowlist additions |
| clinfo, clpeak | OpenCL (NVIDIA ICD) | PASS | n/a (compute) | |

Group B's lease stages under crosvm too (2026-09-26, `--vmm crosvm
--wayland-socket <session> --wayland-lease`): `lease` 9/0/1, `vkdisplay`
8/0/1 and `secneg` with `nvgpu_secneg_kms=lease` 6/0/0 (inside it: ctl and
render 10 passed, 5 skipped; KMS on the lease 15 passed, 0 failed), the same
as nesbox's, which were re-run the same day. After each lease DP-3 went back
to Hyprland; it was powered off again.

Under crosvm the compute slots were not run: the pass predates crosvm's
compute build, and CUDA under crosvm has run only in the regression's `render`
probe since. The video paths that need compute are every one that goes
through CUDA: NVENC as ffmpeg drives it (a CUDA context), NVDEC with
`-hwaccel cuda` and mpv's nvdec, and VA-API (nvidia-vaapi-driver is NVDEC on
CUDA) in vainfo, mpv, Chromium and Firefox. Vulkan Video, decode and encode,
is the graphics-only path and runs under both VMMs; NVIDIA's NVENC sits
behind it too, through the same session controls.

No run under either VMM stopped the backend's seccomp filter (status 159),
and no guest oops or host Xid was seen. Refusals and what was done with each
are in SECURITY.md §12, "Added from the application pass".

### Fixed on the way

- The CUDA runtime and NVENC: six GSS legacy RM controls the allowlist did
  not have (cudart's clock queries, NVENC's session setup); added, each held
  to its measured size (`16f9235`, SECURITY.md §12).
- VA-API: the guest's render node answered DRM_CAP_DUMB_BUFFER with
  -EOPNOTSUPP, which nvidia-vaapi-driver takes for `nvidia_drm.modeset=0`;
  it now answers 0, as a modeset device without dumb buffers (`0202233`).
- The probe side: Krita through Xwayland, SDL told X11 for Neverball,
  LibreOffice given the Vulkan loader, Qt's log on stderr, clpeak's options,
  Element's store, Electron's feature names; and the harness's live mode
  (window identity by stableId, following the guest).

### Known native failures, and how they were shown

| what | shown natively by |
|---|---|
| Godot Forward+ in a guest without `--allow-compute`: NVIDIA's Vulkan driver lists `VK_KHR_acceleration_structure`, `ray_query`, `ray_tracing_pipeline`, `VK_NV_ray_tracing`, `optical_flow`, `cuda_kernel_launch`, `VK_NVX_binary_import` without `/dev/nvidia-uvm` but `vkCreateDevice` fails with any of them | `rig-native-run.sh --no-uvm`: Godot fails the same way; a one-extension-at-a-time probe fails those seven and no other; with UVM, all 276 create |
| ffmpeg 9.0.1: H.264 Vulkan Video decode into GPU frames stalls at frame 252; with download, stalls in 2 of 5 runs; hevc_vulkan encode hangs finishing in 5 of 5 | the same commands on the host, five runs each |
| Firefox's RDD sandbox: nvidia-vaapi-driver's `vaInitialize` fails in the RDD process | the host with the same prefs: fails with the sandbox, works with `MOZ_DISABLE_RDD_SANDBOX=1` |
| Element's "System unsupported" dialog (no keyring) | the host, headless sway, captured |

Not settled natively: gamescope 3.16's segfault after its child exits
(status 139 at teardown, the game having run; it also logs "Compositor
released us but we were not acquired"). The proxy forwards `wl_buffer.release`
untouched (`wlwire/src/shm.rs`); gamescope cannot start natively inside the
Claude sandbox (its Xwayland cannot reach it there), so this was not compared.

### Skipped

- Xonotic (about 1 GB of game data) and vkQuake (needs id's pak files):
  SuperTuxKart, Neverball and Godot cover GL, SDL2 and Vulkan.
- NVIDIA's CUDA samples and `cudaPackages.saxpy`: they pull cuBLAS, cuFFT,
  cuSPARSE and more, about 2 GB from NVIDIA's servers at ~300 KB/s;
  `nvgpu-nbody` (`rig/guest-image/apps/cuda`) needs only nvcc and cudart.
- PyTorch (size), VS Code (Element covers Electron), anything needing the
  network, and the input-driven checks (chrome://gpu, about:support
  scrolling) on the live desktop.
- The compute slots under crosvm (the pass predates crosvm's compute build).

## Group C: desktop stopped, run from a TTY

**Not run yet.** The guest drives the card itself (`--kms-card`), so **no host
compositor may run on the 5090**. Every monitor goes to the guest.

1. Log out of Hyprland, or stop it. Locking the screen or switching to
   another VT is **not** enough. A compositor that is only switched away has
   dropped DRM master. The backend would then take master when it opens card1,
   and the desktop could not get the card back until the VM ends.
   `run-guest.sh --kms-card` refuses while a compositor socket is still in
   `$XDG_RUNTIME_DIR`. Inside the sandbox it cannot see that directory.
2. Log in on a text console. logind then gives the TTY user the card nodes.
3. Run the launcher from there, outside the sandbox or with the devices bound
   into it.

Set up a way back first: an SSH session from another machine, since the
iGPU drives no monitor. Keep `NVGPU_TIMEOUT` finite. The timeout kills the VM,
and when the VM goes the backend drops the card.

| # | stage | run | backend flags |
|---|---|---|---|
| C1 | **6**: compositor-VM (guest Hyprland on the host card) | `NVGPU_TIMEOUT=600 NVGPU_CMDLINE_EXTRA="nvgpu_comp=hyprland nvgpu_timeout=560" rig/run-guest.sh --kms-card compositor s6` | `--kms-card` |
| C2 | **7**: export mode (host client shown by the guest compositor) | `mkdir -m 0700 -p "$XDG_RUNTIME_DIR/nvgpu-export"`, then `rig/run-guest.sh --kms-card --wayland-export "$XDG_RUNTIME_DIR/nvgpu-export/wayland-x" export s7` | `--kms-card --wayland-export PATH` (PATH's directory must be yours) |
| C3 | **9**: hotplug (unplug/replug, or toggle `leasable`) | as C1, probe `shell` (no hotplug probe yet) | `--kms-card` |

## Capture injection

Safe with the desktop running: no KMS, no compositor, no picker. The host
half is `rig/rig-tools/nvgpu-inject-test` (built by
`rig/rig-tools/build.sh`), which allocates GBM buffers as
xdg-desktop-portal-hyprland does, paints a pattern into them and injects
them; `rig/rig-tools/inject-hook.sh` runs it as `run-guest.sh`'s
`NVGPU_BEFORE_VMM` hook, with the guest image's copy of the host's NVIDIA
userspace, and puts its ids and tokens on the guest's command line. The
guest half is the `capture` probe and `nvgpu-capture-import`.

```sh
rig/rig-tools/build.sh                         # .rig/bin/nvgpu-inject-test
NVGPU_BEFORE_VMM=$PWD/rig/rig-tools/inject-hook.sh rig/run-guest.sh --inject capture cap1
NVGPU_VMM_KIND=crosvm NVGPU_BEFORE_VMM=$PWD/rig/rig-tools/inject-hook.sh \
    NVGPU_INJECT_ARGS="--size 2560x1440" rig/run-guest.sh --inject capture cap1cv
```

The helper's side is in `.rig/logs/<tag>.hook.log` (its explicit-sync
timings at the end, after the guest has run). What the probe checks: the
node's mode, and that a user without its group cannot open it; each buffer
opened, its writable CPU mappings refused, imported into EGL and Vulkan
and every pixel checked against the frame the host painted, with the host's
checksum; the same with no CPU mapping at all (the backend's debug log
shows no window placement for it); a wrong token, a missing id and a
released id refused alike; a buffer the host keeps painting seen changing
without a new open; and 200 frames of explicit sync through an injected
syncobj. The host half checks the refusals it can cause: a memfd, a
udmabuf, a layout past the buffer, a memfd as a syncobj.

Results (RTX 5090, 595.99.02, 2026-09-26, sandbox on):

| | nesbox | crosvm (frontend jailed) |
|---|---|---|
| `capture`, 1280x720 | 16/0/0 | 16/0/0 |
| `capture`, 2560x1440 | 14/0/0 (before explicit sync) | 16/0/0 |
| IMPORT round trip (host) | median 16-22 us | median 16 us |
| OPEN (guest ioctl, INJECT_OPEN included) | 31-48 us | 27-79 us |
| explicit sync, announce to release, guest only waiting and signalling | median 55 us, p99 133 us | median 45 us, p99 109 us |
| the same with the guest reading each frame back through Vulkan | 1.2 ms (720p) | 4.4 ms (1440p) |

The rest of the regression on the same backend and images (`--inject` off),
and the capture probe with the Rust parsers (`NVGPU_KERNEL`, `NVGPU_ROOTFS`
of the Rust build):

| probe | nesbox, C module | nesbox, Rust module | crosvm (compute build) |
|---|---|---|---|
| `capture` | 16/0/0 (2560x1440) | 16/0/0 | 16/0/0 (1280x720) |
| `stage1` | 6/0/0 | 6/0/0 | 6/0/0 |
| `render` | 9/0/1 | 9/0/1 | 9/0/1 |
| `render`, `NVGPU_COMPUTE=1` | 9/0/1, cuda-smoke PASS | 9/0/1, cuda-smoke PASS | 9/0/1, cuda-smoke PASS |
| `wayland`, headless sway | 11/0/3 | 11/0/3 | 11/0/3 |
| `secneg`, `kms=none` | ctl + render 10 passed, 5 skipped | the same | the same |

**With a real screen share.** The tests never open the host's picker.
`rig/rig-tools/portal-identify.sh`, run by the desktop's user on the
desktop (not in a sandbox without the session bus and PipeWire), asks the
ScreenCast portal for a stream -- choose a monitor or a window in the
picker -- takes its first DMA-BUF frame and says whether it is NVKMS
memory, which is what the backend takes:

```sh
rig/rig-tools/portal-identify.sh --selftest        # the environment only
rig/rig-tools/portal-identify.sh                   # the picker appears
# and into a backend, as the capture helper would (the socket from a
# running `run-guest.sh --inject ... ` is $XDG_RUNTIME_DIR/nvgpu-run.*/inject.sock):
rig/rig-tools/portal-identify.sh --inject "$XDG_RUNTIME_DIR"/nvgpu-run.*/inject.sock
```

## Regression of the merged tree

The tree the 2026-09-26 review fixes were merged into (`integrate`) ran
Groups A and B on the 5090, three times over, all green, RM allowlist
enforcing and the backend's sandbox on:

| probe | nesbox, C module | nesbox, Rust module | crosvm (compute build) |
|---|---|---|---|
| `stage1` | 6/0/0 | 6/0/0 | 6/0/0 |
| `render` | 9/0/1 | 9/0/1 | 9/0/1 |
| `render`, `NVGPU_COMPUTE=1` | 9/0/1, cuda-smoke PASS | 9/0/1, cuda-smoke PASS | 9/0/1, cuda-smoke PASS, frontend jailed |
| `wayland`, live Hyprland | 13/0/1 | 13/0/1 | 13/0/1 |
| `secneg`, `kms=none` | ctl + render 10 passed, 5 skipped | the same | the same |
| `lease` | 9/0/1 | 9/0/1 | 9/0/1 |
| `vkdisplay` | 8/0/1 | 8/0/1 | 8/0/1 |
| `secneg`, `kms=lease` | 6/0/0; KMS 15 passed | the same | the same |

## Window size and share

`NVGPU_WINDOW_MIB` and `NVGPU_WINDOW_SHARE` give the backend
`--window-size` and `--window-owner-share`; the VMM follows the size
(nesbox from `virtio-nvgpu-v4`, which the launcher checks for). Each run's
backend log ends with a `window use:` line: each zone's peak, the most one
process held, the largest mapping and the refusals.

```sh
NVGPU_WINDOW_MIB=16384 NVGPU_WINDOW_SHARE=90 NVGPU_COMPUTE=1 rig/run-guest.sh render w16
NVGPU_WINDOW_MIB=16384 NVGPU_WINDOW_SHARE=90 NVGPU_APPS_EXTRA=nvgpu_user=1 \
  rig/rig-app-check.sh --live stk,stkgs,blender,blendervk,glmark2,vkmark,chromeanim w16-apps
# the map/UPDATE/unmap-by-address churn (rig/guest-image/tools/nvgpu-map-churn.c)
NVGPU_CMDLINE_EXTRA="nvgpu_cmd=$(printf %s 'nvgpu-map-churn 400 2' | base64 -w0)" rig/run-guest.sh run churn
```

Results (RTX 5090, 595.99.02, 2026-09-29, sandbox on, allowlist enforcing):

| run | nesbox | crosvm |
|---|---|---|
| default window: `stage1`; `render` with compute; `secneg` (kms=none) | 6/0/0; 9/0/1, cuda-smoke PASS; 10 passed, 5 skipped | 6/0/0 |
| default window: `wayland`, live Hyprland | 13/0/1 | |
| 4 GiB / 75 %: `stage1`; `render` with compute | 6/0/0; 9/0/1 | 6/0/0; 9/0/1 |
| 16 GiB / 90 %: `stage1`; `render` with compute | 6/0/0; 9/0/1 | 6/0/0; 9/0/1 |
| the seven apps above in one VM, 16 GiB / 90 % | 19/0/0 | 19/0/0 |
| the same, default window | 19/0/0 | |
| 48 GiB | refused: "shared window is 0xc00000000 bytes; this device takes whole pages up to 32 GiB" | |

The guest saw a 1, 4 or 16 GiB window in each (`virtio-gpu-nv: window at
..., N bytes`). No run logged `no mapping for pLinearAddress`, `SHM alloc
failed` or an Xid. Peaks of the seven-app VM: UC 0.4, WC 34.5, WB 90.3 MiB
(90.3 by one process: 81 % of its default WB share). Per application,
DEPLOY.md "Sizing the window". The churn, 400 x 2 MiB at the default window:
the backend before the fix (`vhost-user-nvgpu.47098c0`) refused the 193rd
map with `SHM WriteCombine zone: guest process ... holds 0x18000000 of
0x30000000 bytes and may not take 0x200000 more (Owner)`, every unmap having
missed; the fixed one ran 400/400 with the WC peak at 2 MiB.

## What to keep from every run

The launcher writes three files per run:

- `.rig/logs/<tag>.backend.log`, at `RUST_LOG=debug` when a stage fails;
- `.rig/logs/<tag>.console.log`, which holds the guest dmesg the probe prints;
- `.rig/logs/<tag>.json`.

Host `dmesg` is **not** in them. Take it after every B and C stage: a host
nvidia-drm/NVKMS `WARN` or oops is a FAIL whatever the guest printed.
