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

## Where the runs stand

This file is the dated record of every hardware run; the README's "What is
known to work" is the summary the other documents link to. Newest first,
each on an RTX 5090 with 595.99.02, the sandbox on and the RM allowlist
enforcing:

| date | tree | what ran | section |
|---|---|---|---|
| 2026-09-30 | branch `orphanfix` (efdrace's pump fix plus wait registrations let go with their syncobj) | `nvgpu-syncobj-race` phase 2 five times in one guest, then fresh processes, eight orphan-making processes, the whole tool twice more, drm-compat; the same under the previous backend; the compat probe; phase 2 natively on the host | "Orphan wait registrations" |
| 2026-09-30 | branch `integrate35` (display-passthrough after this row): everything above plus the Steam-like workloads' guest changes and `patches/nesbox/0001`; the binaries installed in `.rig/` | Groups A and B under the patched nesbox (C and Rust modules) and crosvm, two batches of live applications: all passed (apps 18/0 and 23/0) | "Regression of the 2026-09-30 review" |
| 2026-09-30 | branch `winehang`: `patches/nesbox/0001` (virtio-blk interrupt barrier), killable locks in the guest module, hang-watch | fresh-boot Wine/Godot D3D12 starts under nesbox with and without the patch; stage1, compat, render (with and without compute) and wayland on the patched nesbox | "Wine start-up stalls" |
| 2026-09-30 | branch `integrate32`: the 2026-09-30 review's fixes (backend, guest module, Wayland, patches, deployment), the gVisor comparison, heavyfix and the fence-retire fix | Groups A and B under nesbox (C and Rust modules) and crosvm, the same probes on a KASAN+UBSAN+KFENCE+lockdep guest kernel with the unbind probe, and two batches of live applications | "Regression of the 2026-09-30 review" |
| 2026-09-30 | branch `heavyfix` on `8fe984f`: IOCTL2 path, posted SYNCOBJ_DESTROY, the pump, crosvm `0011` | the heavy workloads before and after; stage1, compat, render (with and without compute), wayland and secneg under nesbox and crosvm, C parsers, and stage1, render and wayland with the Rust parsers | "Heavy workloads" |
| 2026-09-29 | `492f29b` (branch `integrate30`): the review's fixes and the restructuring after them | Groups A and B under nesbox (C and Rust modules) and crosvm, and two batches of live applications | "Regression of the restructured tree" |
| 2026-09-29 | the 2026-09-29 review's backend fixes | Groups A and B under nesbox and crosvm, and a batch of live applications | "Regression of the 2026-09-29 review's backend fixes" |
| 2026-09-29 | branch `perf` (nesbox `virtio-nvgpu-v5`, crosvm with `0010`) | Groups A and B under both VMMs, C and Rust modules, the benchmarks, part of the application pass | "Benchmarks" |
| 2026-09-29 | branch `frame-timing` | frame pacing on a 240 Hz monitor, natively and in a guest; Group A and B1 (nesbox) | "Frame pacing" |
| 2026-09-29 | branch `window-config2` | window sizes 1, 4 and 16 GiB, the map churn, seven applications | "Window size and share" |
| 2026-09-26 | `integrate`, the 2026-09-26 review's fixes | Groups A and B under both VMMs, C and Rust modules | "Regression of the merged tree" |
| 2026-09-26 | branch `capture-inject` | capture injection under both VMMs | "Capture injection" |
| 2026-09-26 | the application pass | about 35 applications on the live desktop | "Application pass on the live desktop" |

Not run on hardware: Group C (the compositor VM and export mode), hotplug,
B6 (per-present crossings on a lease); a root run of the launcher, the
socket-activated units and the VMM templates of `contrib/systemd`, nesbox
`virtio-nvgpu-v6`'s jail, and crosvm `0010`'s spare vCPU under
`--host-cpu-topology`.

## The rig

Everything is built into `.rig/` (git-ignored), laid out as
`rig/run-guest.sh` expects when it is not run as root:

| path | what |
|---|---|
| `.rig/bin/vhost-user-nvgpu` | the backend (release) |
| `.rig/bin/nesbox` | the VMM (release, static; nesbox's `virtio-nvgpu-v5` or later, and with it its `jailer` for root runs; `.rig/bin/nesbox-v6` and `jailer-v6` are `virtio-nvgpu-v6`, the 2026-09-29 review's) |
| `.rig/bin/crosvm` | the other VMM, `--vmm crosvm` (release, static; below, "crosvm") |
| `.rig/bin/virtiofsd` | only with `NVGPU_NVIDIA_SHARE` |
| `.rig/kernel/vmlinux`, `.rig/kernel/nvgpu.ko` | guest kernel 7.2.7 (ELF `vmlinux`: nesbox enters it at `startup_64` with a `boot_params` page; QEMU, for the TCG smoke, through its PVH note), and the module built against it (Kbuild names it `virtio_gpu_nv.ko`; the rig installs it as `nvgpu.ko`). `.rig/build-kernel.sh` builds it in the C toolchain (`.rig/kernel/toolchain`), so without Rust and with the C parsers -- the fallback since the 2026-09-29 review made the Rust ones the default (`scripts/build-guest-kernel.sh` says so when it builds it); run that script in `scripts/guest-toolchain-rust` to have the rig's main kernel be the default one |
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
of its own from a pool, and SECURITY.md, "One uid per VM", has what the uids separate). What
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
needs a crosvm with the UVM aperture (patches `0007`-`0009`); the launcher
refuses `--allow-compute` with a crosvm whose `run --help` does not name the
`nvgpu-uvm-aperture`. The kernel, image, probes,
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

Build crosvm from upstream (c0474109d64d, 2026-09-25) with the whole series,
all eleven patches of `patches/crosvm/` (patches/README.md says what each is
for; `scripts/ci.sh deploy` checks they still apply), on a branch of its
own:

```sh
git -C .rig/src/crosvm checkout -b virtio-nvgpu-series c0474109d64d
git -C .rig/src/crosvm am "$PWD"/patches/crosvm/*.patch
CROSVM_OUT=.rig/bin/crosvm CARGO_TARGET_DIR=.rig/target-crosvm rig/rig-build-crosvm.sh
```

The branches already in `.rig/src` are older: `.rig/src/crosvm`'s
`virtio-nvgpu` has `0001`-`0006`, `.rig/src/crosvm-compute` (a worktree) has
`virtio-nvgpu-compute` with `0001`-`0009` as first written and
`virtio-nvgpu-compute-fixed` with them regenerated; none has `0010` (the
prefault) or the 2026-09-29 review's changes to `0007` and `0010`. Built
from the series applied as above, the review's build is `.rig/bin/crosvm-p11`.
`CROSVM_SRC` and `CROSVM_OUT` build another checkout to another binary, so
the one in use is left alone. It is built static and without crosvm's
default features: no virtio-gpu, virgl, virtio-wl, audio, USB or network
devices.

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
(SECURITY.md, "The VMMs") still hold.

The launcher passes `--prefault-memory` (`0011`) whenever the binary has
it, and says on the terminal when it does not: guest RAM faulted in and on
2 MiB pages as the VM starts, as nesbox does (`NVGPU_PREFAULT=0` leaves it
out; the console log has crosvm's `prefault:` line with how much went
huge). Without it a game in a crosvm guest stalls for a frame of 20-40 ms
whenever it reaches memory the guest has not used yet (BENCHMARKS.md,
"Heavy workloads").

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
`.rig/bin/crosvm` built from `virtio-nvgpu-compute` (`0001`-`0009`), `render` with
`NVGPU_COMPUTE=1` passes 9/0/1 with `cuda-smoke` all PASS, and `secneg` with
compute passes, the frontend jailed (the launcher's summary says `nvgpu
frontend jailed`); see "Regression of the merged tree", below. The aperture is
region 2 after the window in the window's 64-bit BAR (2 GiB in all with
compute), with a shared-memory capability of its own. The jailed frontend
checks each pool and hands it to the main process, which checks it again,
maps `/dev/nvidia-uvm` at the pool's own address over the band it reserved
at start-up ([4 GiB, 32 TiB)), checks the pages with `mincore`, and adds the
slot; withdrawal removes the slot, then puts the reservation back
(SECURITY.md, "The VMMs"). What to run, in this order, each against nesbox's result
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
B6 (per-present crossings on a lease) has not run. The Wayland present path's
count (W2) came out of the frame-pacing counters instead: 17 round trips per
presented frame (DEPLOY.md, "Frame pacing").

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
are in SECURITY.md, "The RM allowlist", and the application pass in its
review history.

### Fixed on the way

- The CUDA runtime and NVENC: six GSS legacy RM controls the allowlist did
  not have (cudart's clock queries, NVENC's session setup); added, each held
  to its measured size (`16f9235`, SECURITY.md, "The RM allowlist").
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

## Regression of the 2026-09-30 review

2026-09-30, branch `integrate32`, with the backend, both guest modules and the
image built from it, nesbox `virtio-nvgpu-v6`, and crosvm with the regenerated
`0001`-`0011`. Counts are pass/fail/skip.

| probe | nesbox, C | nesbox, Rust | nesbox, KASAN kernel | crosvm |
|---|---|---|---|---|
| stage1 | 6/0/0 | 6/0/0 | 6/0/0 | 6/0/0 |
| compat, with `nvgpu-syncobj-race` | 13/0/0 | 13/0/0 | 13/0/0 | 13/0/0 |
| render, and render with CUDA | 9/0/1, cuda-smoke ALL PASS | same | same | same |
| map churn | 3/0/0 | 3/0/0 | 3/0/0 | 3/0/0 |
| wayland | 13/0/1 | 13/0/1 | 13/0/1 | 13/0/1 |
| secneg (T1-T13) | 13 passed, 5 skipped | same | same | same |
| lease, vkdisplay, secneg with KMS | 9/0/1, 8/0/1, 18 passed | same | not run | same |
| unbind under load | | | 7/0/0, no sanitizer report | |

The compat probe's syncobj race first failed at full speed with EMFILE: a
fired wait registration stayed charged to its process for a second after it
fired, and a guest firing 16,000 a second reached its share of the handle
table (fence.rs, pump.rs). The KASAN kernel, slower, passed. The Rust row's
compat is from the fixed backend's own run; its other rows are from the
backend before that fix. Live applications, nesbox: 18 checks passed, none
failed; crosvm with `--allow-compute`: 23 passed, none failed.

## Regression of the restructured tree

2026-09-29, `492f29b` (branch `integrate30`): the 2026-09-29 review's fixes
and the restructuring after them (the backend's `nvidia/` and `inject/`
modules, the guest module's split files, the launcher in `rig/launcher/`),
with the backend, both guest modules and the images built from that commit.
Unprivileged launcher, nesbox `virtio-nvgpu-v6` and crosvm with `0001`-`0010`.

| probe | nesbox, C parsers | nesbox, Rust parsers | crosvm |
|---|---|---|---|
| stage1 | 6/0/0 | 6/0/0 | 6/0/0 |
| compat | 12/0/0 | 12/0/0 | 12/0/0 |
| render | 9/0/1 | 9/0/1 | 9/0/1 |
| render with CUDA (`--allow-compute`) | 9/0/1, cuda-smoke ALL PASS | 9/0/1, ALL PASS | 9/0/1, ALL PASS |
| map churn (`nvgpu-map-churn 400 2`) | 3/0/0 | 3/0/0 | 3/0/0 |
| wayland (live Hyprland) | 13/0/1 | 13/0/1 | 13/0/1 |
| secneg | 3/0/1 (10 passed, 5 skipped) | same | same |
| lease | 9/0/1 | 9/0/1 | 9/0/1 |
| vkdisplay | 8/0/1 | 8/0/1 | 8/0/1 |
| secneg with KMS (on a lease) | 6/0/0 (15 passed) | same | same |

Counts are pass/fail/skip. Live applications (`rig/rig-app-check.sh --live`):
glxgears, gamescope, GTK, Qt, Firefox, mpv, vkmark, SuperTuxKart, Blender and
a Chromium animation under nesbox, 18 checks passed and none failed; CUDA,
Cycles, NVENC, mpv with VA-API and mpv under crosvm with `--allow-compute`,
23 passed and none failed. Earlier on the same day, the launcher's piped
console froze mpv's Vulkan output within a second; the launcher now has the
VMM write its console log directly, with a watchdog for the size cap, and
mpv passes under both VMMs.

## Regression of the 2026-09-29 review's backend fixes

The backend with the 2026-09-29 review's fixes (every one with a test that
fails without it), under nesbox and crosvm alike, RM allowlist enforcing and
the backend's sandbox on:

| probe | result |
|---|---|
| `stage1` | 6/0/0 |
| `compat` | 12/0/0 |
| `render`, `NVGPU_COMPUTE=1` | 9/0/1, cuda-smoke PASS |
| `wayland`, live Hyprland | 13/0/1 |
| `secneg`, `kms=none` | ctl + render 10 passed, 5 skipped |
| `lease` | 9/0/1 |
| `vkdisplay` | 8/0/1 |
| `secneg`, `kms=lease` | 6/0/0; KMS 15 passed |
| live applications (glxgears, vkmark, stk, chromeanim) | 11/0/0 |

The guest's PCI config space from a root snapshot (`--pci-config-dir`) was
shown with a synthetic snapshot, since the rig has no root: the guest's
`lspci -vvv` reads `Capabilities: [40] Null` without it, and the snapshot's
PCIe capability and link with it.

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

## Frame pacing

`rig/rig-framepace.sh` runs one workload natively (`rig-native-run.sh
--live`: the guest image's programs and NVIDIA userspace on the host) or in
a guest (`run-guest.sh ... run`, against the live session), on one
monitor's workspace, fullscreen, with MangoHud (in the image since this
branch, the same build both ways) logging every frame's present interval.
`rig/framepace-stats.py` reads the logs: mean, p50, p99, p99.9 and the
frames longer than 1.5x the refresh period, each of which missed a vblank.

```sh
# the same workload, native then in a guest, three runs each, on DP-3 (240 Hz)
rig/rig-framepace.sh vkcube native fp1          # vkcube-mbox, vkmark, stk, gamescope
rig/rig-framepace.sh vkcube vm fp1
# a busy desktop, reproducibly: a stress-ng load through every run
NVGPU_FP_LOAD="--cpu 32 --cpu-load 60" rig/rig-framepace.sh stk vm fp-load
# a guest module parameter, a backend flag, a placement
NVGPU_FP_GUEST_PRE="echo 0 > /sys/module/virtio_gpu_nv/parameters/rt_spin_us" \
  NVGPU_FP_BACKEND_ARGS=--queue-poll-us=0 NVGPU_CPU_AFFINITY=8-15 \
  rig/rig-framepace.sh stk vm fp-ablate
```

The window goes to the monitor's active workspace silently (a window rule
on the workloads' classes and titles) and is made fullscreen by its address;
focus stays where it was and nothing is typed. The monitor (DP-3 by default,
an OLED) is powered only while a run measures. For each guest run, beside
the CSV: both sides' pacing counters (`<wl>-vm-<n>.pacing.txt`: the guest
driver's round-trip and wake histograms, syncobj waits and event counts,
and the backend's teardown report), and how the host scheduled the VMM's
and backend's threads while the log ran (`<wl>-vm-<n>.sched.txt`, from
`rig/framepace-sched.py`: CPU, run-queue wait, preemptions per thread
group). `placement.log` has where each window was and whether the monitor
scanned it out directly.

gamescope does not start natively inside the Claude sandbox (its Xwayland
fails authorisation there, with or without a network namespace of its
own), so it has guest runs only. In the guest it runs without
`CAP_SYS_NICE`, as a user runs it: as root it asks for a realtime queue,
whose RM control the allowlist refuses.

Results are in DEPLOY.md, "Frame pacing". The regression of branch
`frame-timing` on its installed backend, image and modules (2026-09-29,
nesbox, sandbox on, allowlist enforcing): `stage1` 6/0/0, `render` with
compute 9/0/1 (cuda-smoke PASS), `secneg` (kms=none) ctl + render 10
passed, 5 skipped, `compat` 12/0/0, `wayland` on the live Hyprland 13/0/1;
with the Rust module, `stage1`, `render` with compute, `secneg` and
`compat` the same. No Xid.

## Benchmarks

`rig/rig-bench.sh` runs the matrix of [`BENCHMARKS.md`](../BENCHMARKS.md)
natively (`rig-native-run.sh`: the guest image's programs and NVIDIA
userspace on the host, without `XDG_DATA_DIRS`, as the guest's probes run)
or in a guest (`run-guest.sh run`), both against the headless sway, so
nothing reaches a monitor and DP-3 stays off. The guest image carries the
programs (`rig/guest-image/tools/nvgpu-bench.c`, `nvgpu-cubench.c`,
`nvgpu-bench-suite.sh`); `rig/bench-stats.py` makes the table.

```sh
rig/rig-headless-sway.sh &                        # the compositor, in its own terminal
export NVGPU_BENCH_GROUPS="micro cuda gpu video startup wl"
rig/rig-bench.sh native n 3                       # three native runs
NVGPU_COMPUTE=1 rig/rig-bench.sh vm g 3           # three in nesbox (cuda needs compute)
NVGPU_COMPUTE=1 NVGPU_VMM_KIND=crosvm rig/rig-bench.sh vm c 3
rig/bench-stats.py native='.rig/logs/bench/n/*.bench' nesbox='.rig/logs/bench/g/*.bench' \
    crosvm='.rig/logs/bench/c/*.bench'
# one figure's ablation: a backend flag, a guest module parameter
NVGPU_BENCH_GROUPS=micro NVGPU_BENCH_BACKEND_ARGS="--queue-poll-us 0" rig/rig-bench.sh vm qp0 3
```

The regression of branch `perf` on its installed backend, VMMs (nesbox
`virtio-nvgpu-v5`, crosvm with `patches/crosvm/0010`), image and modules
(2026-09-29, sandbox on, allowlist enforcing), under nesbox and crosvm, each
with the C and the Rust module: `stage1` 6/0/0, `compat` 12/0/0, `render`
with compute 9/0/1 (cuda-smoke PASS), the map churn 400/400, `secneg`
(kms=none) 10 passed, 5 skipped, `wayland` on the live Hyprland 13/0/1,
`lease` 9/0/1, `vkdisplay` 8/0/1, `secneg` on the lease 15 passed. The
application pass's glxgears, vkmark, glmark2, SuperTuxKart, Chromium, mpv,
CUDA n-body, NVENC, NVDEC and Cycles, as uid 1000 with compute: 33/0/0 under
each VMM. One earlier `compat` run (crosvm, Rust module, before the final
module) failed its EXPORT_SYNC_FILE check once -- a sync_file not signalled
within 1 s -- and did not recur in 14 fresh boots, 200 runs in one guest,
or the final regression; the same image's predecessor passed 8 of 8.

Each guest run also writes the backend's and the VMM's CPU per group
(`vm-N.cpu`, from `/proc` every 100 ms), both sides' pacing counters
(`vm-N.pacing`) and the backend's log. A run takes about four minutes;
interleave native and guest runs, and build nothing meanwhile: a compile on
the same CPUs moves every figure.

## Heavy workloads

`rig/rig-heavy.sh` runs games, an engine and a renderer unpaced (vsync
off, no frame cap) natively or in a guest, against the headless sway (it
sets the output's mode per workload: 3840x2160 for the GPU-bound ones,
1280x720 for the CPU-bound ones), and records every frame's time the same
way both ways; `rig/heavy/heavy-stats.py` makes BENCHMARKS.md's "Heavy
workloads" rows (average, 1% and 0.1% lows, p50/p99/p99.9, variation).
The workloads are `rig/heavy/heavy-run.sh`'s; the guest image needs them
at `/opt/heavy`, which `rig/heavy/mkimage-heavy.sh` puts into a reflink
copy of a built image in seconds, with an nvgpu-bench that has
`vk-stream` (`--file`).

```sh
rig/heavy/mkimage-heavy.sh .rig/guest/rootfs.ext4 /tmp/rootfs.heavy.ext4 \
    --file <tools>/bin/nvgpu-bench:/opt/heavy/nvgpu-bench:755
rig/rig-heavy.sh stk-ultra native h 3
NVGPU_HEAVY_NATIVE_CPUS=0-3 rig/rig-heavy.sh stk-low native h4 3   # as many CPUs as the guest
NVGPU_ROOTFS=/tmp/rootfs.heavy.ext4 NVGPU_COMPUTE=1 rig/rig-heavy.sh stk-ultra vm hg 3
NVGPU_ROOTFS=/tmp/rootfs.heavy.ext4 NVGPU_COMPUTE=1 NVGPU_VMM_KIND=crosvm rig/rig-heavy.sh stk-ultra vm hc 3
# Blender's heavy scene needs a bigger window than the default
NVGPU_WINDOW_MIB=16384 NVGPU_WINDOW_SHARE=90 NVGPU_ROOTFS=... rig/rig-heavy.sh blender-vk vm hb 2
rig/heavy/heavy-stats.py native='.rig/logs/heavy/h/stk-ultra-*.frames' nesbox='.rig/logs/heavy/hg/stk-ultra-*.frames'
```

Guest runs need `NVGPU_COMPUTE=1` for Godot's Forward+ (its ray-tracing
extensions want UVM; probes/apps.sh, godot). Each guest run keeps the backend's log with
its periodic pacing report (`NVGPU_PACING_STATS`, 5 s by default here),
whose `ioctl2 time` line splits each IOCTL2's backend time into preparing
and the host ioctl, both sides' pacing counters, and the backend's and the
VMM's CPU over the run. Interleave native and guest runs and build
nothing meanwhile, as for the benchmarks. For a monitor,
`rig/rig-framepace.sh stk-ultra` runs the same effects on DP-3 with vsync.

Known, and not ours:

- **SuperTuxKart 1.5's Vulkan renderer aborts natively.** "vkQueueSubmit
  failed", then "Aborting SuperTuxKart", at the start of the race: 3 of 6
  native runs on four CPUs (`NVGPU_HEAVY_NATIVE_CPUS=0-3`), none of 6
  unconfined, and 1 of 20 in a guest. An aborted process can go on
  rendering on the compositor; `rig-heavy.sh` kills whatever is left of the
  image's programs after each native run, and says so in the run's log.

Fixed, and how to see it again:

- **Blender's Vulkan backend (`blender-vk`) hung in 10 of 24 guest runs
  while the allowlist refused `NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS`.**
  NVIDIA's Vulkan driver disables and re-enables one of its own channels
  around each frame it starts, and goes on the same way whether RM did it
  or not; without it, a frame (nearly always the second) sometimes never
  completed, and the main thread polled `/dev/nvidia0` every 10 ms for
  good. The same refusal made natively (`RMLOG_REFUSE`, below) hangs
  Blender the same way. The control is allowed now, with a gate of its own
  (SECURITY.md, "The RM allowlist"): 24 runs under nesbox and 10 under
  crosvm, none hung. The gate's refusals show in the backend's log
  (`FIFO_DISABLE_CHANNELS refused: ...`) and at teardown; the rig's
  workloads make none. `sec-negative` T12 and T13 fire the refusals from a
  guest (another process's client, a preemption event, and 400 calls back
  to back).

**A run that stops.** `rig/heavy/hang-watch.sh <workload> [runs]` (at
`/opt/heavy` in the image) runs a workload in a loop and, when its app.log
gains no `HEAVY_` line for `HANG_STALL` seconds (default 30: Blender prints
one a frame; Godot prints one at the start and one at the end, so give it
45), dumps what the program waits on -- every thread's state, kernel stack
and blocking syscall, a poll's descriptors and a futex's word decoded
(`rig/heavy/waits.py`) -- and the guest module's counters, kills it and
starts the next run:

```sh
NVGPU_CMDLINE_EXTRA="nvgpu_wl=1 nvgpu_timeout=900 nvgpu_cmd=$(printf %s \
    'bash /opt/heavy/hang-watch.sh blender-vk 10' | base64 -w0)" \
NVGPU_COMPUTE=1 NVGPU_WINDOW_MIB=16384 NVGPU_WINDOW_SHARE=90 NVGPU_ROOTFS=... \
    rig/run-guest.sh --wayland-socket "$(cat .rig/run/headless-sway.socket)" run hw1
```

**The fixes' runs** (branch `heavyfix`, BENCHMARKS.md, "Heavy workloads",
"The fixes"). The pacing report says what each change does: the guest's
`posted` and `posted_failed` (SYNCOBJ_DESTROYs sent without waiting, and any
the host then refused -- none in any run), `pump woke N/s` on the backend's
`legacy readiness` line, and crosvm's `prefault:` line in the console log
(how long guest RAM took, and how much went on 2 MiB pages). The regression
subset with the branch's backend, guest module and crosvm, sandbox on,
allowlist enforcing, 2026-09-30:

| probe | nesbox, C parsers | crosvm, C parsers | nesbox, Rust parsers |
|---|---|---|---|
| stage1 | 6/0/0 (7 boots; one more stalled, below) | 6/0/0 | 6/0/0 |
| compat | 12/0/0 | 12/0/0 | -- |
| render | 9/0/1 | 9/0/1 | 9/0/1 |
| render, `--allow-compute` | 9/0/1 | 9/0/1 | -- |
| wayland (headless sway) | 11/0/3 | 11/0/3 | 11/0/3 |
| secneg, `kms=none` | 3/0/1 | 3/0/1 | -- |

Counts are pass/fail/skip. The images were round one's heavy and base
images with the branch's module put in (`mkimage.sh --module-only`), so
`secneg` ran the tests that image has, not `8fe984f`'s T12 and T13.

Two starts under nesbox with the branch stalled before any device call:
one stage1 whose `vulkaninfo` never opened a device node (guest-check timed
out at 60 s; the backend saw nothing but TIME_SYNC), and one SuperTuxKart
start that opened three nodes and made no call. Neither came back in six
more stage1 boots each way, 60 `vulkaninfo` start-ups each way in one guest,
or some 40 other SuperTuxKart starts with the branch; nothing that reached
the backend differed. Most likely nesbox's lost block-queue interrupt, found
later ("Wine start-up stalls"): a first start in a fresh guest, stopped
with no device call outstanding, only under nesbox. One more SuperTuxKart start (nesbox,
the branch) quit at once with "Could not initialize display": its Wayland
connection to the headless sway failed before any device call was made
(only Wayland messages had crossed), which no change here touches.

`rig/heavy/rmlog.c` (LD_PRELOAD) logs the RM controls a program makes that
fail, and every DISABLE_CHANNELS with its parameters; natively,
`RMLOG_REFUSE=0x2080110b` answers those controls as the allowlist does
without RM seeing them (`RMLOG_REFUSE_STATUS=0` answers success instead),
which is how a refusal is told apart from the device.

### Proton-like and CPU-heavy workloads

What a Steam game under Proton does that the games above do not -- D3D11
and D3D12 through DXVK and vkd3d-proton, Wine's threads and its
synchronisation, many worker threads a frame -- comes from programs the
guest image does not carry: `rig/heavy/extras.nix` builds them from the
image's own nixpkgs (Wine 11 staging in WoW64 mode, DXVK, 0 A.D., and the
rig's `nvgpu-gameloop`, `nvgpu-rtprobe` and `nvgpu-wakecost`), and
`mkimage-heavy.sh --closure` puts that closure into a copy of the image, with
the Windows programs and a Wine prefix beside it (`--tree`):

```sh
X=$(nix build --no-link --print-out-paths -f rig/heavy/extras.nix)
# the prefix: wineboot once, then DXVK's and vkd3d-proton's DLLs into
# system32 (x64) and syswow64 (x86); Godot's and Unigine Heaven's Windows
# builds under win/godot and win/heaven (innoextract of its installer)
rig/heavy/mkimage-heavy.sh .rig/guest/rootfs.ext4 /tmp/rootfs.steam.ext4 --grow 12000 \
    --closure "$X:/opt/heavy/extras" --tree "$WIN:/opt/heavy/win" \
    --tree "$PREFIX:/opt/heavy/wine/prefix"
NVGPU_HEAVY_EXTRAS=$X NVGPU_HEAVY_WIN=$WIN NVGPU_HEAVY_WINE_PREFIX=$PREFIX \
    rig/rig-heavy.sh wine-heaven native h 3
NVGPU_ROOTFS=/tmp/rootfs.steam.ext4 NVGPU_COMPUTE=1 rig/rig-heavy.sh wine-heaven vm hg 3
```

- `gameloop`: `nvgpu-gameloop` (rig/heavy/gameloop.c), a synthetic frame --
  four fork-join phases of 64 jobs on as many threads as the process has
  CPUs (particles integrated, random reads through a 512 MiB heap), then
  4,000 draws presented through Vulkan's Wayland WSI, unpaced. Every phase
  wakes every worker, which is what makes a guest's vCPU wakeups show.
- `wine-godot-draws-d3d12`, `wine-godot-draws-vk`, `wine-godot-gpu-d3d12`:
  Godot 4.7.2's Windows build under Wine on vkd3d-proton (D3D12) or
  winevulkan, the same scenes as `godot-draws` and `godot-gpu`.
- `wine-heaven`: Unigine Heaven 4.0's 32-bit D3D11 build under Wine with
  DXVK, its demo camera, 1280x720, low, no tessellation; MangoHud's frame
  times.
- `0ad`: 0 A.D. 0.28 on its Vulkan renderer, four Petra AIs on a generated
  map, observed; MangoHud as a Vulkan layer only (its GL hook crashes the
  game).
- `probe-vk`: the Vulkan device extensions listed, and `nvgpu-rtprobe`: a
  device with `VK_KHR_ray_query` and a compute shader casting a ray a pixel
  into 64 instances of 200,000 triangles.
- `wakecost`: `nvgpu-wakecost`, two threads on two CPUs handing a token
  through a futex (and spinning), and one CPUID.

Wine synchronises through ntsync where the kernel has it (`/dev/ntsync`,
`CONFIG_NTSYNC`), else through its server. The host here has no
`/dev/ntsync` loaded, so native runs use the server, and a guest run
removes the guest's `/dev/ntsync` so that both sides do the same;
`NVGPU_HEAVY_ENV=HEAVY_WINE_NTSYNC=1` keeps it. `NVGPU_HEAVY_ENV` passes
`VAR=value,...` to heavy-run.sh both ways (`HEAVY_THP=always` sets a
guest's transparent huge pages for the run).

**KVM's counters.** perf needs `kernel.perf_event_paranoid` of 2 or less and
tracefs needs root; neither is available here. `NVGPU_HEAVY_KVMSTAT=1` runs
the launcher under `rig/heavy/kvmstat.py`, which takes duplicates of the
VMM's KVM statistics descriptors (pidfd_getfd, allowed to an ancestor) and
writes the exits, halts, halt polling and faults per second over a window of
the run (`.kvmstat`). KVM answers `KVM_GET_STATS_FD` only in the process
that made the VM, so nesbox must open them itself: its `virtio-nvgpu-v7`
branch does with `NESBOX_HOLD_KVM_STATS=1`, which the harness sets.

## Wine start-up stalls

Godot 4.7.2's Windows build on D3D12 (Wine 11 staging, vkd3d-proton
2.14.1; `wine-godot-draws-d3d12`) stopped in one freshly booted nesbox
guest in five to seven (7 of 35 for steamperf, 8 of 46 here), with or without `--allow-compute` and the Vulkan
layer, never natively and never in ten runs back to back in one guest; a
killed Wine process sometimes did not exit. The cause is nesbox's, not the
device's: its virtio-blk worker decided whether to interrupt from the
driver's `used_event` with no barrier after publishing the used index, so
the guest could sleep on a completed request and never hear of it. Under
`EVENT_IDX` that queue is then dead for good -- later completions no longer
cross the `used_event` the guest waits at -- and every task that reads or
writes through that CPU's queue sleeps in `io_schedule`, uninterruptible.
[`patches/nesbox/0001`](../patches/nesbox/) is the barrier (the one crosvm
has in `get_used_event()`), with a unit test that races the real ring code
against the driver's side of the handshake: without the barrier it lost 1
to 55 completions in each of five runs of a million rounds, with it none in
ten. It is on nesbox `virtio-nvgpu-v7` in a local branch
(`virtio-nvgpu-v7-blkfence`), not pushed.

What showed it (rig/heavy/hang-watch.sh does all of this now): the kernel's
blocked tasks at the stall were `jbd2/vda-8` (writing the journal
superblock), `ext4lazyinit` (reading a block bitmap) and the Wine
process's reads or page faults, all in `io_schedule` -- a disk that stopped,
in the first minute after boot, while ext4's lazy init and Wine's first
start (nothing in the page cache yet) read from every CPU at once, which is
why a second run in the same guest never stalled. The backend had nothing
outstanding (its pacing report: TIME_SYNC only), and no blocked stack had
a frame of the guest module's. One direct 4 KiB read from each CPU afterwards
completed on three queues and joined the D tasks on the fourth (CPU 3, 0
and 3 in the three stalls with the poke). The old hang-watch printed
nothing past STALL, since its own `pgrep` and `awk` could not be read from
the disk; and `pgrep -f` or `/proc/<pid>/cmdline` of a task faulting on
the dead queue waits behind its `mmap_lock`.

Fresh boots of `wine-godot-draws-d3d12`, one start each, under a watcher
that dumps the guest at a 40 s stall (steamperf's image, backend and THP
guest kernel; only the VMM differs), 2026-09-30:

| nesbox | runs | stalled |
|---|---|---|
| `virtio-nvgpu-v7` | 46 (20 alternating with the next row; the last 6 under this branch's hang-watch.sh, whose report came out for both of its stalls) | 8 |
| `virtio-nvgpu-v7` + `patches/nesbox/0001` | 44 (20 alternating with the row above; 8 with this branch's backend and module) | 0 |

The same patched nesbox with this branch's backend and module (below) ran
stage1 6/0/0, compat 12/0/0, render 9/0/1, render with `--allow-compute`
9/0/1 and wayland 11/0/3.

Found auditing the guest module for the same stall, and fixed though not
its cause: four mutexes held across a synchronous host call (a file's first
KMS call, SYNCOBJ_DESTROY, the SEMSURF rehome, a GEM object's window
placement) were taken uninterruptibly, so a killed second caller could sit
in D for up to the transport's 30 or 60 s timeout; they are killable now.
Every other wait a process can enter in the module already was killable or
interruptible, bar `master_set`/`master_drop`'s lock, which DRM gives no
way to fail.

## Syncobj eventfds that never fired

The compat probe's `nvgpu-syncobj-race` failed 2 of about 16 runs,
2026-09-30, both while other agents loaded the host, with one line per
owner thread: `FAIL owner N: eventfd on handle H point P never fired` --
six or all eight owners at once, each once, and the phase about a second
short of its usual rounds (80 and 88 thousand, against 97 to 121). A lost
wakeup, not a slow one: the event pump (device/src/pump.rs) drained its
wake eventfd *after* taking its instructions, so the kick of a Watch
queued between the last instruction taken and the drain went with the
drain, and the pump slept with the Watch in its channel. Every owner that
had sent its SYNCOBJ_EVENTFD's WATCH in that moment then slept on an
eventfd the host had signalled, until one of them gave up and made some
other call. The pump now drains the kick first (`Pump::step`), and
`an_instruction_queued_as_the_pump_takes_its_instructions_wakes_its_next_wait`
fails without that.

The tool now tells the two apart: an eventfd not fired within
`NVGPU_RACE_PATIENCE_MS` (1000) is waited for up to `NVGPU_RACE_DIAG_S`
(30) seconds, and the failure says LATE (and when) or LOST, with the
host's view of the point then (SYNCOBJ_QUERY, a TIMELINE_WAIT poll), the
event records the guest took meanwhile and the other owners' eventfds
that fired. Each phase prints its signal-to-fire latency.
`NVGPU_RACE_OWNERS_ONLY=1` runs the first phase alone, for many runs in
one guest: the second phase leaves registrations on exported syncobjs
whose points never come, which count against the VM's pool for good
(SECURITY.md, "Fences"), and after four runs of it every SYNCOBJ_EVENTFD
in the VM says -ENOMEM. (Fixed by `orphanfix`: "Orphan wait
registrations", below.)

Runs of the owners phase (5 s, 8 threads) under nesbox, 2026-09-30; host
load from stress-ng under the rig lock, guest load four busy loops.
"Window" is a diagnostic build (not in the tree) that sleeps in the pump
between the instructions and the old drain, as a preempted pump thread
does:

| backend | load | runs | stalled |
|---|---|---|---|
| rig's (c273c5d) | none, guest, host (64 CPU hogs, 12 GiB vm, disk), backend pinned to one CPU with two hogs | 91 (C) | 0 |
| rig's + 100 us window | none | 30 (10 C, 20 Rust) | 2: every owner LOST for the full 5 s wait, the point signalled on the host, 0 event records to the guest |
| rig's + 2 ms window | none | 3 (C) | 3, in the first two rounds |
| fixed + 100 us window | none | 20 (Rust) | 0 |
| fixed + 2 ms window | none | 5 (C) | 0 |
| fixed | host and guest, pinned | 60 (40 C, 20 Rust), and the whole compat probe 6 times (3 C, 3 Rust, 32 CPU hogs) | 0 |

Signal to eventfd, per 5 s phase: idle, p99 under 512 us and at most
5 ms; four busy guest CPUs, p99 under 4 ms, at most 7 ms; the backend
sharing one host CPU with two hogs, at most 15 ms; the host at a load of
140 (64 hogs, memory, disk) with the guest busy, p99 under 16 to 65 ms,
p99.9 under 262 ms, at most 240 ms. The one-second patience stands.

## Orphan wait registrations

Branch `orphanfix`, 2026-09-30, under nesbox with the C module; the
backend built from the branch, the image with the tool's phase 3
(SECURITY.md, A.14, has the cause and the fix). One guest ran, in order:
`nvgpu-syncobj-race 5 8` with phase 3 skipped five times (phases 1 and 2),
the owners phase alone as a fresh process, phase 3 alone with eight
orphan-making processes, the whole tool twice more (phase 3 six processes
each time), the owners phase again, and `nvgpu-drm-compat` (64- and 32-bit).

| backend | phase 2 (rounds, handles) | fresh owners after it | phase 3: orphans per process | fresh process after phase 3 |
|---|---|---|---|---|
| previous (efdrace's `be-fix`) | 55k/57k, 32k/34k, 24k/26k, 17k/19k, 16k/17k: stopped short at -ENOMEM, barely an import | passed, on the reserve | 16, 16, 16, then 0 (-ENOMEM on the first SYNCOBJ_EVENTFD, even on syncobjs let go at once) | -ENOMEM; the whole tool after it: every owner's SYNCOBJ_EVENTFD -ENOMEM |
| `orphanfix` | 126k-140k rounds, 202k-219k handles (about 75k imports each) | passed | 256 (its share) in each of 8, then 6 and 6 | 64 of 64; every later phase passed |

The compat probe on a fresh guest passed, 13/0/0. The earlier A/B of ten
runs of phases 1 and 2 alone: the previous backend ran out after eight (the
ninth's owners phase, a fresh process, -ENOMEM on every SYNCOBJ_EVENTFD:
three runs of 256, one of 192, four of 16, 1,024 in all).

Once phase 2 went on past -ENOMEM, its check that an owner's handle never
shows the owner's own older object failed in about one run in five. The
same tool on the host's own render node (no guest, no backend) failed
every run, several times each in 2.3 million rounds: an owner whose number
a guesser took exports and imports what now has it, and the import lands
on the lowest free number, another owner's. The check now fails only on a
point the owner has not signalled yet; native runs pass (3 of 3), and so
does phase 3 natively, where nothing refuses (4,096 orphans each).

A registration the backend lets go is asked about first when its point
might have a fence (`HostSyncobj::available`, a handle imported into a
render file of the session and destroyed in the same step): the ignored
test `the_host_says_whether_a_point_has_a_fence_and_leaves_no_handle` in
`device/src/fence.rs` checks that on the host's render node, and passed.

## What to keep from every run

The launcher writes three files per run:

- `.rig/logs/<tag>.backend.log`, at `RUST_LOG=debug` when a stage fails;
- `.rig/logs/<tag>.console.log`, which holds the guest dmesg the probe prints;
- `.rig/logs/<tag>.json`.

Host `dmesg` is **not** in them. Take it after every B and C stage: a host
nvidia-drm/NVKMS `WARN` or oops is a FAIL whatever the guest printed.
