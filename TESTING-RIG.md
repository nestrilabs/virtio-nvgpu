# Running the display stages on the dev box

[`TESTING.md`](TESTING.md) says what each stage checks and what a pass looks
like. This file says **in what order to run them on this machine**, with the
unprivileged launcher, and **which ones touch the monitors**.

The machine: AMD host (`kvm_amd`), an **RTX 5090 on the open 595.99.02 modules
with every monitor plugged into it**, an AMD iGPU with none, and the user's
Hyprland desktop running on the 5090. So there is no spare card and no spare
monitor. Every stage that does KMS takes a monitor away from the desktop, or
needs the desktop stopped. The stages are grouped below by that cost.

**Before the first GPU stage, read [`.rig/SAFETY-NOTES.md`](.rig/SAFETY-NOTES.md).**
It covers what can reach the desktop, the memory and VRAM limits, and what to
do if the desktop freezes. This host has `panic_on_oops=1` and `panic=0`, so a
host oops freezes the machine until you power it off. Save your work and keep
an SSH session open from another machine.

## The rig

Everything is built into `.rig/` (git-ignored), laid out as
`scripts/run-guest.sh` expects when it is not run as root:

| path | what |
|---|---|
| `.rig/bin/vhost-user-nvgpu` | the backend (release) |
| `.rig/bin/nesbox` | the VMM (release) |
| `.rig/bin/crosvm` | the other VMM, `--vmm crosvm` (release, static; below, "crosvm") |
| `.rig/bin/virtiofsd` | only with `NVGPU_NVIDIA_SHARE` |
| `.rig/kernel/vmlinux`, `.rig/kernel/nvgpu.ko` | guest kernel 7.2.7 (ELF `vmlinux`: nesbox enters it at `startup_64` with a `boot_params` page; QEMU, for the TCG smoke, through its PVH note), and the module built against it |
| `.rig/kernel-rust/vmlinux`, `.rig/kernel-rust/nvgpu.ko` | the same kernel with `CONFIG_RUST=y`, and the module with its parsers in Rust (`NVGPU_RUST=1`), built by `scripts/rig-build-kernel-rust.sh`; to run one, see [`driver/rust/README.md`](driver/rust/README.md) |
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
`scripts/run-guest.sh --help`.

The rig runs one VM, as you: without root there is no other user to be, so
the backend, nesbox and your desktop share a uid (`scripts/run-guest.sh`'s
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
(`guest-image/README.md` has what each checks) and the stages they cover:

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
| `nodev` | none: the image without a device, for QEMU (`.rig/tcg-smoke.sh`) and nesbox without `gpu-forward` (`.rig/nesbox-nodev.sh`) |

Probe arguments are kernel command-line tokens, passed with
`NVGPU_CMDLINE_EXTRA="nvgpu_<key>=<value> ..."`.

Before the first real run, `.rig/tcg-smoke.sh` boots the same kernel, image
and command line under QEMU (TCG, or KVM when `/dev/kvm` is there) without a
GPU, and `.rig/nesbox-nodev.sh` boots them under nesbox with KVM but no
`gpu-forward` section, so nesbox's own boot path, disk, console and power-off
are proven before the device is added (`.rig/RIG.md`). Neither opens anything
on the host GPU.

## Before anything: preflight

```sh
scripts/rig-preflight.sh
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
| A1 | **1**: HELLO v2, nodes, extensions | `scripts/run-guest.sh stage1 s1` | none |
| A2 | **2, W1 only**: offscreen render (the envyhooks differential has no probe), twice: without compute (nvidia-smi, Vulkan and EGL with no UVM device; CUDA must find no device and exit cleanly), then with it (CUDA must run) | `scripts/run-guest.sh render s2` then `scripts/run-guest.sh --allow-compute render s2c` | none, then `--allow-compute` |
| A3 | **security negatives**, ctl + render tests (no `--kms`). Run only after A1 and A2 pass, with your work saved: a regressed fix can oops the host (SAFETY-NOTES risk 2) | `NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none scripts/run-guest.sh secneg sec` | none |
| A4 | **3** against a **separate headless compositor** | see below | `--wayland-socket <headless socket>` |
| A5 | **8**: explicit sync (all three parts) | as A4 | `--wayland-socket <headless socket>` |
| A6 | caching **M-2** (read-only mapping; no probe yet) | `scripts/run-guest.sh shell m2`, by hand | none (`-- --keep-guest-coherency` only to rule the rewrite out) |
| A7 | performance, W1 crossings; W2 crossings against the headless compositor | as A2 / A4 | as A2 / A4 |

H-4 and M-1 in the caching stage are Intel-only and cannot occur on this AMD
host (`TESTING.md`, "Caching and coherency").

### The headless compositor (A4, A5, A7)

Stage 3's guest clients need a host compositor. **Do not point them at the
live Hyprland socket yet.** Hyprland exposes virtual keyboard and pointer,
screencopy and data-control to any client. The proxy's allowlist hides those
(`wlwire/src/policy_table.rs`), but the allowlist has never run against a real
compositor, and it is one of the things these stages test. So the guest gets a
compositor of its own, where a hole in the allowlist costs nothing:

```sh
# terminal 1: sway, headless, rendering on the 5090's render node
scripts/rig-headless-sway.sh                  # --renderer gles2 to try the other one
# terminal 2:
scripts/run-guest.sh --wayland-socket "$(cat .rig/run/headless-sway.socket)" wayland s3
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
`--renderer pixman` in the sandbox. With the NVIDIA renderer it has not run
yet.

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
scripts/rig-build-crosvm.sh                     # .rig/src/crosvm -> .rig/bin/crosvm
                                                # (first time: the top of that script)
scripts/run-guest.sh --vmm crosvm stage1 cv-s1
scripts/run-guest.sh --vmm crosvm render cv-render
scripts/run-guest.sh --vmm crosvm --wayland-socket "$(cat .rig/run/headless-sway.socket)" wayland cv-wl
NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none scripts/run-guest.sh --vmm crosvm secneg cv-sec
NVGPU_VMM_KIND=crosvm NVGPU_APPS_EXTRA=nvgpu_user=1 scripts/rig-app-check.sh \
  typing,pointer,clipboard,glxgears,gamescope,gtk,qt,firefox,mpv,vkmark,chromegpu cv-apps
```

`.rig/src/crosvm` is upstream crosvm (c0474109d64d, 2026-09-25) on branch
`virtio-nvgpu`, with patches `0001`-`0006` of `patches/crosvm/`;
`.rig/src/crosvm-compute` is a worktree on branch `virtio-nvgpu-compute`,
all nine (patches/README.md says what each is for). The compute build goes
to its own binary, so the graphics one is left alone:

```sh
CROSVM_SRC=.rig/src/crosvm-compute CROSVM_OUT=.rig/bin/crosvm-compute \
  CARGO_TARGET_DIR=.rig/target-crosvm-compute scripts/rig-build-crosvm.sh
NVGPU_VMM=.rig/bin/crosvm-compute scripts/run-guest.sh --vmm crosvm ...
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

Built (`0007`-`0009`), unit-tested, not yet run on the GPU. The aperture is
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
env $V scripts/run-guest.sh --vmm crosvm stage1 cvc-s1
env $V NVGPU_COMPUTE=1 scripts/run-guest.sh --vmm crosvm render cvc-render   # cuda-smoke too
NVGPU_CMDLINE_EXTRA=nvgpu_secneg_kms=none env $V NVGPU_COMPUTE=1 \
  scripts/run-guest.sh --vmm crosvm secneg cvc-sec
env $V scripts/run-guest.sh --vmm crosvm render cvc-render-nocompute       # graphics only, as before
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
([`patches/README.md`](patches/README.md); `.rig/hypr-build` has a build). One
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

## Group C: desktop stopped, run from a TTY

The guest drives the card itself (`--kms-card`), so **no host compositor may
run on the 5090**. Every monitor goes to the guest.

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
| C1 | **6**: compositor-VM (guest Hyprland on the host card) | `NVGPU_TIMEOUT=600 NVGPU_CMDLINE_EXTRA="nvgpu_comp=hyprland nvgpu_timeout=560" scripts/run-guest.sh --kms-card compositor s6` | `--kms-card` |
| C2 | **7**: export mode (host client shown by the guest compositor) | `mkdir -m 0700 -p "$XDG_RUNTIME_DIR/nvgpu-export"`, then `scripts/run-guest.sh --kms-card --wayland-export "$XDG_RUNTIME_DIR/nvgpu-export/wayland-x" export s7` | `--kms-card --wayland-export PATH` (PATH's directory must be yours) |
| C3 | **9**: hotplug (unplug/replug, or toggle `leasable`) | as C1, probe `shell` (no hotplug probe yet) | `--kms-card` |

## What to keep from every run

The launcher writes three files per run:

- `.rig/logs/<tag>.backend.log`, at `RUST_LOG=debug` when a stage fails;
- `.rig/logs/<tag>.console.log`, which holds the guest dmesg the probe prints;
- `.rig/logs/<tag>.json`.

Host `dmesg` is **not** in them. Take it after every B and C stage: a host
nvidia-drm/NVKMS `WARN` or oops is a FAIL whatever the guest printed.
