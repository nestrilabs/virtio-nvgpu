# Testing the display paths

**This is the plan for the box with the GPU.** Nothing here runs in CI or on a
machine without a card: it drives real NVIDIA hardware, a real host compositor,
and real monitors. What runs without one is `scripts/ci.sh` (formatting, lints,
the unit and end-to-end tests, the guest module builds, fuzzing, Miri, the
Wayland loopback and the generated tables' check against their sources), and
the root flake's `checks`. It is written to be run in order — each stage assumes the one
before it passed — and every stage says what to type, what a pass looks like, and
what to grab when it does not.

The design under test is the display modes of the README's "Display" section and
[`ARCHITECTURE.md`](ARCHITECTURE.md) §10–§16; the claims each stage is checking
come from the on-device plan in the verification review
([`docs/review/NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5) and
the security review ([`docs/review/FINDINGS.md`](docs/review/FINDINGS.md)). The
runnable helpers live in [`rig/verify/`](rig/verify/).

**Where it stands (2026-09-26).** Stages 1–5, 8, the lease round trip of 9 and
the security negatives have run and pass on an **RTX 5090, driver 595.99.02**,
under nesbox and crosvm, against the live patched Hyprland;
[`rig/TESTING-RIG.md`](rig/TESTING-RIG.md) has the order they were run in, the results
and the application pass. Stages 6 and 7 (the compositor VM and export mode),
hotplug, and the performance stages have not run.

The machines this plan has been used on:

- **RTX 5090, driver 595.99.02** — the dev box, where every stage above ran
  ([`rig/TESTING-RIG.md`](rig/TESTING-RIG.md)).
- **RTX 3060, driver 595.99.02** — the box everything in [`BENCHMARKS.md`](BENCHMARKS.md)
  was measured on, before protocol v2.
- **RTX A2000, driver 615.71.09** — renders (before protocol v2), not yet
  exercised for display.

The reference driver tree and the envyhooks bindgen target **610.57.04**. That
mismatch matters for one stage (the envyhooks differential) and is called out
where it does; everywhere else the ABI profiles that carry the display work are
the same across 595, 610 and 615, so a 595 or 615 box is a valid target.

Throughout, three logs are the ones worth keeping on any failure, and each stage
names the ones specific to it on top:

- **the backend log** at `RUST_LOG=debug` (`sudo RUST_LOG=debug NVGPU_PREFIX=/root /root/bin/run-guest.sh …`;
  the launcher writes it per run, to `/root/logs/<tag>.backend.log`);
- **guest `dmesg`** (the guest kernel module logs the cause of each refusal);
- **host `dmesg`** (a host oops or an nvidia-drm/NVKMS `WARN` is always a FAIL).

---

## 0. Preconditions

### 0.1 Driver versions line up

```sh
cat /proc/driver/nvidia/version                 # host
# in the guest:
cat /proc/driver/nvidia/version ; nvidia-smi
```

- **PASS:** the host kernel module, host userspace and guest userspace are the
  same version. 610.57.04 is ideal because it is the reference tree; 595.99.02 or
  615.71.09 are fine for every stage except the envyhooks differential (stage 2),
  which needs an envyhooks built against the matching
  `open-gpu-kernel-modules` tag.
- If you stay on 595 or 615, rebuild envyhooks against that tag and confirm the
  logged `paramsSize` of `AMPERE_CHANNEL_GPFIFO_A` matches
  `size_of::<NV_CHANNEL_ALLOC_PARAMS>()` for that version (368 on 610).

### 0.2 KVM caching regime

```sh
grep -m1 vendor_id /proc/cpuinfo
```

The dev and benchmark boxes are AMD (NPT, guest PAT honoured), so the Intel-only
caching failures (stage "caching and coherency" below) cannot occur there — they
need an Intel host with `KVM_X86_QUIRK_IGNORE_GUEST_PAT` on. Record which you have.

### 0.3 Host configuration

- **nvidia-drm modeset on.** Boot the host with `nvidia_drm.modeset=1` (check
  `cat /sys/module/nvidia_drm/parameters/modeset` → `Y`). Without it, NVKMS is
  not available and `supports_semsurf` is 0.
- **vblank, optionally.** `nvidia_drm.vblank=1` is needed for guest vblank
  ioctls to deliver events (a documented limit of nvidia-drm's). Turn it on for the
  lease/compositor-VM stages if vblank waits hang.
- **the backend must not be root.** RM, DRM and NVKMS all take the guest's
  privilege from the backend's credentials ([`FINDINGS.md`](docs/review/FINDINGS.md) S-5), and the backend
  refuses to start as root. `rig/run-guest.sh` runs as root itself (for the
  VMM) and starts the backend through `setpriv` as an unprivileged user, with the
  groups `video`, `render` and `kvm`, no capabilities and `no_new_privs`: a
  user of the VM's own from the pool `nvgpu-vm0`, `nvgpu-vm1`, ... by default
  (the `useradd` loop is at the top of the script; the VMM runs under nesbox's
  jailer as the slot's `nvgpu-vmmN`), the owner of the compositor's socket in
  the Wayland modes, and the owner of the export socket's directory in export
  mode. The backend then sandboxes itself (network namespace, Landlock,
  seccomp); a line `sandbox: DEGRADED` in its log says a layer is missing, and
  `NVGPU_SANDBOX=off` turns it off to rule it out.
  `NVGPU_USER=…` picks another; `NVGPU_USER=root NVGPU_ALLOW_ROOT_UNSAFE=1` is
  the only way to run it as root, for ruling the credentials out, never for a
  test whose result you will keep. Every mode below is started through the
  launcher, with the flags the appendix lists.
- **As root, only root's files.** The launcher run as root refuses anything
  it would run, read or write that is not root's, or sits in a directory
  someone else can write -- itself included -- so it is not run from a user's
  checkout: install a copy of it and of its pieces beside it (`sudo install
  -D -o root -g root -m 0755 -t /root/bin rig/run-guest.sh` and `sudo install
  -D -o root -g root -m 0644 -t /root/bin/launcher rig/launcher/*.sh`; as
  root it checks each piece before it reads it), with the tree it expects
  under `/root` (the launcher's header), and name the layout:
  `sudo NVGPU_PREFIX=/root /root/bin/run-guest.sh …` (or `NVGPU_RIG=` a
  root-owned rig). The VMM runs jailed unless `NVGPU_VMM_JAIL=auto|off`, and
  `NVGPU_SANDBOX=off`, `NVGPU_ALLOW_ROOT_UNSAFE=1` and every diagnostic
  backend flag also need `NVGPU_DIAGNOSTIC=1`. As root the launcher binds the
  backend's socket itself (`systemd-socket-activate`) and hands it over, so
  the backend in the tree must be one that takes it (`LISTEN_FDS`, from the
  2026-09-29 review on: SECURITY.md §22); it starts when the VMM connects.

### 0.4 Tools

On this box only `vkcube` and `eglgears_wayland` are on PATH by default. Get the
rest through nix; the verify scripts already do this where they can:

```sh
nix shell nixpkgs#drm_info nixpkgs#libdrm nixpkgs#kmscube \
          nixpkgs#vulkan-tools nixpkgs#mesa-demos nixpkgs#bpftrace
```

`modetest` ships inside `nixpkgs#libdrm`. `drm_info`, `kmscube`, `vulkaninfo`,
`eglinfo` and `modetest` are used below.

### 0.5 Building the verify helpers

Two of the helpers are C. Build them where they will run — on the dev box for the
host-side pieces, and **inside the guest** for the guest-side ones, so the binary
matches the guest's glibc:

```sh
rig/verify/build.sh              # -> rig/verify/bin/{sec-negative,lease-flip}
```

It uses a system `cc` and `pkg-config libdrm` if present, and falls back to nix
otherwise. The tests link only libc and libdrm; nothing NVIDIA.

---

## Stage 1 — the module loads and HELLO negotiates v2

The foundation: the transport is up, the nodes are present, and the driver's own
libraries advertise the extensions the later stages need.

**Config:** any mode. Boot the guest with `rig/run-guest.sh` as usual; each
display mode adds its flags to that command line (see the appendix), and the
launcher passes them to the backend.

**Run (in the guest):**

```sh
rig/verify/guest-check.sh
```

**Expected:**

```
== protocol ==
  ok    protocol v2 negotiated
        virtio-gpu-nv: protocol v2, backend caps 0x…, requests up to … card node(s)
== device nodes ==
  ok    /dev/nvidiactl
  …
== Vulkan ==
  ok    VK_EXT_physical_device_drm
  …
```

**PASS:** protocol v2 is negotiated, `/dev/nvidiactl`, `/dev/nvidia0`,
`/dev/nvidia-uvm`, `/dev/nvidia-modeset` and a render node all exist, and
`VK_EXT_physical_device_drm` is advertised. `VK_KHR_display` /
`VK_EXT_acquire_drm_display` (needed only for the direct-display stage) and
`EGL_ANDROID_native_fence_sync` (explicit-sync EGL clients) are reported as
warnings if missing, so you know before you reach the stage that needs them.

**FAIL:** the check prints `backend is v1 only` (update the backend) or a node is
missing. **Capture:** the whole guest `dmesg`, and the backend log — the HELLO
line is logged at `info`.

---

## Stage 2 — submission fidelity (bare metal vs guest)

The claim under test is the one the whole project rests on: a guest submits the
*same* GPU work as bare metal, and display passthrough adds no per-submission
crossing ([`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) U2, U3, M-6, L-7). We prove it by running the same
binary on bare metal and in the guest under envyhooks and diffing the RM ioctl
sequence and the pushbuffers.

**Precondition:** matching driver versions everywhere (0.1); envyhooks built and
installed as `$EHKS`; `nv_push_dump` built ([`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5.2 / §5.3); no
other GPU clients.

**Run, once per side, for each of three workloads:**

```sh
export EHKS=/opt/ehks/libenvyhooks.so
# W1 offscreen, W2 Wayland present, W3 display present:
rig/verify/envy-capture.sh bare  W1 ~/ehks -- nesprobe --device 0 --cost 400 --seconds 10 --warmup 0
rig/verify/envy-capture.sh guest W1 ~/ehks -- nesprobe --device 0 --cost 400 --seconds 10 --warmup 0
# … and again for W2 (vkcube --wsi wayland --c 300) and W3 (vkcube --wsi display --c 300)
```

Then diff:

```sh
rig/verify/envy-diff.sh ~/ehks/bare/W1 ~/ehks/guest/W1
```

`envy-diff.sh` normalises away the differences that are *expected* — RM-assigned
handles, CPU pointers, `pLinearAddress` (a window offset in the guest), fds and
timer values — and shows what is left. Decode any unmatched pushbuffer with
`nv_push_dump file.bin AMPERE_B` (take the arch from the 3D class id in the RM
log).

**PASS:**

- the normalised RM sequence is identical apart from handle/pointer/fd/time
  values;
- the pushbuffer content-hash sets match, or differ only in address-like fields
  when decoded;
- **crossings per frame** (stage "performance" below) are ~0 for W1 on both sides
  and equal.

**FAIL:** any class, control command or escape added, missing or reordered; a
nonzero status on one side only; a different `MAP_MEMORY` length or caching flag;
a different `MAP_MEMORY_DMA` `dmaOffset` (GPU-VA divergence, U2); or an extra
host-class (c56f) `SEM_EXECUTE`/`WFI` in the guest's pushbuffers.
**Capture:** both `rm.log`s, the decoded diverging pushbuffers, guest `dmesg`, the
backend log. Trace a divergence back to the first differing earlier
`RM_ALLOC`/`RM_CONTROL`.

> Do **not** combine envyhooks with `strace -e trace=ioctl` in one run: both use
> `SIGTRAP`. Run the strace pass separately when you want the raw syscall counts.

---

## Stage 3 — Wayland-client mode with direct scanout

The default mode (README, "Display"; ARCHITECTURE.md §14, §16): a guest app is a Wayland client of host
Hyprland through the proxy, and a fullscreen buffer reaches the host as the host's
own NVKMS object, so it is eligible for direct scanout exactly as bare metal is.

**Host config (Hyprland Lua):**

```lua
hl.config({ render = { direct_scanout = 2 } })   -- 2 = auto
hl.config({ debug  = { enable_stdout_logs = true } })
```

**Launch:** from the desktop session, so the variables are its own —
`sudo NVGPU_PREFIX=/root /root/bin/run-guest.sh --wayland-socket "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" <probe>`.
The backend then runs as the session's user (the socket's owner), which is who
may connect to it.
**Guest:** run the daemon, then a fullscreen client through it:

```sh
nvgpu-wl-guest --socket wayland-0 &          # serves $XDG_RUNTIME_DIR/wayland-0
WAYLAND_DEBUG=1 WAYLAND_DISPLAY=wayland-0 vkcube --wsi wayland
```

**Expected / PASS:**

1. In the `WAYLAND_DEBUG` trace, `zwp_linux_buffer_params_v1.add` carries a
   `modifier_hi/lo` with vendor `0x03`, bit 4 set, `g=2, s=1, c=0`.
2. In the guest, `vulkaninfo`'s `VkDrmFormatModifierPropertiesEXT` for
   `B8G8R8A8_UNORM`/`XRGB8888` and `gbm_bo_get_modifier` equal the host's
   `drm_info -j /dev/dri/cardN` `IN_FORMATS` for the primary plane.
3. While the client is fullscreen, the host Hyprland log shows
   `Entered a direct scanout to <ptr>: "<title>"`, and leaving fullscreen logs
   `Left a direct scanout.` (`hyprctl monitors all` also reports
   `directScanoutTo` non-zero and `directScanoutBlockedBy: null`.)

**FAIL:** the modifier does not match host `IN_FORMATS`; the host never logs a
direct scanout while the client is fullscreen (it composited instead); or host
`dmesg` shows `Cannot create FB from compressible surface` / `Invalid format
modifier`. **Capture:** the `WAYLAND_DEBUG` trace, the Hyprland stdout log, host
`dmesg`, `drm_info -j` from both sides.

---

## Stage 4 — DRM lease → guest KMS (kmscube / modetest)

The lease mode (ARCHITECTURE.md §11): the host leases a connector and the guest drives it through
the adopted lease fd.

**Host config:** apply the `patches/hyprland` and `patches/aquamarine` patches
(`patches/README.md`), and mark a monitor leasable:

```lua
-- used as a normal monitor until a client leases it:
hl.monitor({ output = "DP-2", mode = "preferred", position = "auto", leasable = true })
-- or, for an untrusted guest, never used by Hyprland itself:
-- hl.monitor({ output = "HDMI-A-1", disabled = true, leasable = true })
```

`hyprctl monitors all` should show `leasable: 1` for that output.

**Launch:** `sudo NVGPU_PREFIX=/root /root/bin/run-guest.sh --wayland-socket … --wayland-lease <probe>`
(`--wayland-lease-interval SECS` spaces the guest's lease requests, 5 s by
default; 0 lifts it while iterating on this stage).
**Guest:** the daemon exposes the host's `wp_drm_lease_device_v1`; a lease client
acquires the connector and hands its fd to a KMS app.

```sh
drm_info /dev/dri/card1                 # lists the leased connector
modetest -D /dev/dri/card1 -c           # connectors/modes visible
kmscube -D /dev/dri/card1               # renders to the leased output
# or the smoke helper (modeset + a few page flips, reports flip pacing):
rig/verify/lease-flip.sh --device /dev/dri/card1 --frames 120
```

`lease-flip.sh` / `rig/verify/kms-smoke.sh` are the minimal libdrm drivers if
`kmscube` is not to hand; `kms-smoke.sh <dev> DP-2@crtc:preferred` does an
explicit `modetest` modeset.

**PASS:** the leased connector appears and scans out; a blocking commit completes
within 3 s; `lease-flip` reports a mean flip interval at the monitor's refresh
period; a nonblocking commit returns `-EBUSY` only while a flip is pending.

**FAIL:** the connector is missing; `ADDFB2` returns `-EINVAL` where bare metal
succeeds; a commit never completes; or `lease-flip` reports a flip that never
arrived in 3 s. **Capture:** `drm_info` from both sides, the backend log, guest
and host `dmesg`.

---

## Stage 5 — vkAcquireDrmDisplayEXT / VK_KHR_display

The `VK_KHR_display` mode (ARCHITECTURE.md §13): the direct Vulkan display path over the same lease plus
nvidia-drm `GRANT_PERMISSIONS(MODESET)`.

**Config:** as stage 4 (a leasable monitor, `--wayland-lease`). Needs
`VK_KHR_display` and `VK_EXT_acquire_drm_display` (stage 1 reports these).

**Run (guest):**

```sh
vkcube --wsi display          # or: vkcube --wsi display --c 300
```

If the installed `vkcube` lacks a display WSI, a minimal
`vkAcquireDrmDisplayEXT` → `vkGetDrmDisplayEXT` → `vkCreateDisplayPlaneSurfaceKHR`
program does the same; the acquire path is what is being checked.

**PASS:** the leased display is acquired and rendered to; the picture is stable
at the monitor's refresh. **FAIL:** `vkAcquireDrmDisplayEXT` returns an error, or
`GRANT_PERMISSIONS` is refused for `MODESET` (it must be *allowed* for MODESET and
refused only for SUB_OWNER — see the security stage). **Capture:** the backend log
(the grant is logged), host `dmesg`.

---

## Stage 6 — compositor-VM mode (Hyprland in the guest)

Compositor-VM mode (ARCHITECTURE.md §11): the guest compositor drives the host card directly; the host
runs no compositor of its own.

**Host:** no compositor running on the card. **Launch:**
`sudo NVGPU_PREFIX=/root /root/bin/run-guest.sh --kms-card <probe>` (the backend warns loudly that the
host card nodes are now offered to the guest). This
offers `DEV_DRI_CARD_*` nodes and `num_cards > 0` in HELLO.

**Guest:**

```sh
rig/verify/guest-check.sh          # now expects /dev/dri/card* to be present
rig/verify/kms-smoke.sh /dev/dri/card0            # enumerate
rig/verify/kms-smoke.sh /dev/dri/card0 DP-1@crtc:preferred   # drive it
Hyprland                                # a full guest compositor on the host card
```

**PASS:** `modetest`/`kms-smoke` enumerate the host's connectors through the guest
card; a guest compositor lights the physical output; master arbitration works
(the guest DRM core grants master to the first opener, and the host card file
becomes host master only while the guest file is guest master). **FAIL:** the card
enumerates nothing; a guest `SET_MASTER` does not take; or a probe steals host
master. **Capture:** backend log, guest and host `dmesg`, `hyprctl monitors all`
from the guest compositor.

---

## Stage 7 — export mode (apps in a second VM)

Compositor-VM mode's other half (ARCHITECTURE.md §14): host or second-VM clients reach the guest
compositor through the proxy in export mode.

**Config:** on the compositor-VM's launch add `--wayland-export /path/sock`
(socket 0600, peer-uid checked: the launcher runs the backend as the owner of
`/path`, so that user's programs are the ones that may connect). A host client connects at that socket; the guest
daemon in `--export` mode carries it to the guest compositor.

```sh
# guest side:
nvgpu-wl-guest --export wayland-1 --card /dev/dri/card0 &
# host side (or second VM), against the export socket:
WAYLAND_DISPLAY=/path/sock foot            # a host client shown by the guest compositor
```

**PASS:** the host client's window appears on the guest compositor's output; its
dmabufs import into the channel's render handle and present. **FAIL:** the client
cannot connect (peer-uid refused when it should pass, or vice versa), or its
buffers do not import. **Capture:** the daemon's stderr, the backend log.

> Export mode widens the attack surface ([`FINDINGS.md`](docs/review/FINDINGS.md) S-4, S-10): confirm the
> export socket is 0600 and that a guest process cannot beat the daemon to
> `ACCEPT`. The security stage covers the guest-reachable refusals.

---

## Stage 8 — explicit sync

ARCHITECTURE.md §12, [`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5.9: fences live on the host, the guest holds
proxies, and no host thread parks on a guest wait.

**Config:** `CAP_FENCES` is on by default (the backend reports it in HELLO). The
semsurf path needs `nvidia_drm.modeset=1`.

**Run (guest):**

1. **semsurf device creation.** `vkcube --wsi wayland`; a `WAYLAND_DEBUG` trace
   shows `wp_linux_drm_syncobj` timelines carried to the host.
   - On a `nvidia_drm.modeset=0` host, `GET_DEV_INFO` should report
     `supports_semsurf=0` and device creation should still succeed on the
     non-semsurf path.
2. **`EGL_ANDROID_native_fence_sync`.** An EGL client using
   `eglCreateSyncKHR(EGL_SYNC_NATIVE_FENCE_ANDROID)` + `eglDupNativeFenceFDANDROID`
   then polls/merges the fd. `strace` shows `SYNC_IOC_FILE_INFO` and no
   unexpected CPU `poll()` where the host uses semsurf-wait.
3. **timeline import.** `vkWaitSemaphores` (timeline, 1 s) on a value the GPU
   signals ~100 ms later returns on signal, not on the 5 s force-signal; the
   proxy `FILE_INFO` status is 1, not `-110`.

**PASS:** all three as described. **FAIL:** device creation fails with
`VK_ERROR_INITIALIZATION_FAILED`; the wait returns only at the force-signal; or a
host thread is seen parked on a guest wait. **Capture:** backend log at
`RUST_LOG=debug` (log `EV_FENCE` statuses), the strace, guest `dmesg`.

---

## Stage 9 — hotplug and lease round-trip

ARCHITECTURE.md §11, [`patches/README.md`](patches/README.md); [`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5.8.

**Lease round-trip (mode 3/4).** With a leased desktop monitor, take a lease in
the guest and drive it, then close it:

- **PASS:** the guest enumerates the leased output through
  `wp_drm_lease_device_v1`, acquires and drives it; on lease close the host
  reclaims the connector and re-enables its own use of it **without a stuck or
  dark output** (this also exercises M-9). The guest can no longer `FLIP`/`SET_MODE`
  the head after the lease ends.
- **FAIL:** the host output stays dark after the guest closes the lease, or the
  guest can still drive the head after lease end.

**Hotplug (compositor-VM).** Unplug/replug a monitor (or toggle `leasable` at
runtime, which withdraws/offers without a modeset). The guest compositor should
see the connector change through an `EV_HOTPLUG` uevent.

**Capture:** host and guest `dmesg`, `hyprctl monitors all` before/after, the
backend log (the hotplug listener logs lease re-checks).

---

## Security negative tests

The security review ([`FINDINGS.md`](docs/review/FINDINGS.md), and the C-1/H-1/M-10 findings in
[`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §3) says the backend must turn away a set of guest-reachable
requests that would otherwise reach past the VM. The **authoritative** proof of
each refusal is a backend unit test that asserts the host ioctl is never issued
([`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5.1, run with `cargo test`). This suite is the
**end-to-end** confirmation that the same refusal holds when a real guest process
makes the request, and that neither the guest nor the host dies doing it.

**Run (guest):**

```sh
rig/verify/sec-negative.sh                       # ctl + render tests
rig/verify/sec-negative.sh -- --kms /dev/dri/card1   # add the KMS/lease tests
```

The tests, each an ioctl a hostile guest would use:

| test | what it attempts | refusal expected | finding |
|---|---|---|---|
| `os-descriptor RM_ALLOC 0x71` | allocate `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` / `VID_HEAP ALLOC_OS_DESCRIPTOR` | RM would pin memory named by a VMM address; refused by class/function when sent with an address alone (the supported form sends the guest-physical pages, BCAP_OS_DESC) | S-1 (3) |
| `raw-pointer RM_CONTROL` | an embedded-pointer control with a guest pointer the guest table does not list | backend relocates every `NvP64` to its own buffer, or refuses | S-1 |
| `semsurf 0x54 huge index` | `SEMSURF_FENCE_CTX_CREATE` with an overflowing `index` | index bounded to the host layout; refused | C-1 |
| `GRANT_PERMISSIONS SUB_OWNER` | nvidia-drm `0x52` with `type=3` | only `MODESET` allowed; SUB_OWNER refused | H (GRANT) |
| `ADDFB2 non-NVKMS handle` | scan out a handle not IDENTIFYed as NVKMS | refused | L6, S-6 |
| `GETFB foreign fb` | fetch a GEM handle for an fb this file did not create | handle returned as 0 | S-6, RV:getfb |
| `RM_SHARE type ALL` | share an object with every RM client on the host | refused before RM, `NV_ERR_INSUFFICIENT_PERMISSIONS` in the status | S-35 |
| `DUP other process` | a forked child makes a client and a VA space; the parent duplicates it into its own client | refused before RM (the clients' guest processes differ), `NV_ERR_INSUFFICIENT_PERMISSIONS` | S-35 |
| `DUP same process` | **positive control**: the same duplicate between two clients of one process, on two files | **made** — a FAIL here is the backend refusing what RM allows, and makes the row above inconclusive | S-35 |

**PASS:** every test reports `PASS refused` (or `SKIP` where the mode is not
offered), the program exits 0, and — checked separately — **host `dmesg` is clean
and the backend is still serving** after the run. The wrapper prints the guest
`dmesg` tail so a guest oops is visible too.

**FAIL:** any test reports `FAIL accepted` (a dangerous request went through), or
the host oopses / the backend dies. A host oops is a FAIL of the *fix*, not a pass
of the test. **Capture:** the test output, the backend log at `RUST_LOG=debug`
(the refusal is logged there), host `dmesg`, guest `dmesg`.

The `sec-negative` program uses only public UAPI — the NVIDIA escape numbers and
NVOS layouts, and the nvidia-drm command numbers from the repo's own
`gen/schema/nvidia_drm.py` — so it builds against nothing but libc and libdrm.

---

## Performance

*In the style of [`BENCHMARKS.md`](BENCHMARKS.md): every number here is one you
ran, on the box named by the GPU in it, and the method is stated so it can be
re-taken. Where a figure is derived rather than observed, it says so.*

Two numbers matter for the display work, and both are the backend's own count,
not an estimate from a log.

### Per-present crossings

The render loop crosses the VM boundary essentially never — that is the whole
design, and BENCHMARKS.md already publishes ~0.02 crossings per frame for an
offscreen load. **Presentation is where a per-present crossing could hide**, and
this is the number to publish for it ([`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) L-7).

For the Wayland-present (W2) and display-present (W3) workloads of stage 2, take
the backend's served-message tally before and after a **30 s run after an 8 s
discard**, and divide by frames presented. The backend prints its tally by
message type at teardown (`NvidiaBackend::teardown: served N message(s): …`), so
the delta split by type (WL_SEND, syncobj/semsurf, ATOMIC/NVKMS FLIP, EV_DRM) is
the per-present breakdown. Cross-check against a separate

```sh
strace -f -c -e trace=ioctl,write,sendmsg -p $(pidof vhost-user-nvgpu)
```

over the same window.

- **Report** the per-present crossing count for W2 and W3 separately, and split by
  message type. Do **not** report W1's ~0 as the per-present figure — W1 does not
  present.
- **PASS:** the count is stable run to run and matches the strace-derived
  per-frame syscall count. A small fixed per-present count is expected (a present
  is a real commit); a count that grows with frame rate, or a per-*draw* crossing,
  is the failure.

### Frame pacing

A present path is only right if it holds cadence. Two ways to see it:

- **Leased output:** `rig/verify/lease-flip.sh --device /dev/dri/cardN
  --frames 300` reports mean/min/max flip interval; the mean should sit at the
  monitor's refresh period (16.67 ms at 60 Hz) with a tight spread.
- **Wayland present:** the whole-chain measurement BENCHMARKS.md already uses —
  arrival spacing at the receiver, `gap_ms p50 ≈ 16.67`, and **zero** frames
  arriving more than ~25 ms after the one before. Report the spacing, not just a
  frame count: a count cannot tell a slow pipeline from an on-time one that
  started late.

State the driver, card, host CPU and guest vCPU/mem for every figure, and compare
a guest only against its own host on the same machine, minutes apart — the same
discipline as BENCHMARKS.md.

---

## Caching and coherency (Intel host only)

[`NVK_VERIFICATION.md`](docs/review/NVK_VERIFICATION.md) §5.11–5.12, findings H-4, M-1, M-2. These failures need an
Intel host with the guest-PAT quirk on; on the AMD dev/benchmark boxes only the
M-1 performance tail shows (slow WB-intended reads through a WC mapping), not the
H-4 correctness failure.

- **H-4 (coherency):** read a GPU-written value through a
  `HOST_VISIBLE|HOST_COHERENT`, non-`HOST_CACHED` allocation. PASS (post-fix, or
  with the quirk disabled): the value is correct. FAIL (pre-fix on Intel): stale
  value or hang.
- **M-1 (doorbell latency):** tight empty-submit + wait, 1e5 iterations, p50/p99/max,
  WC doorbell vs a build mapping the usermode page UC. A heavy tail only on WC
  confirms it. Confirm the effective type in
  `/sys/kernel/debug/x86/pat_memtype_list`.
- **M-2 (read-only mapping VM-kill):** in the guest, allocate an RUSD (`NV00DE`)
  object, `RM_MAP_MEMORY`, `mmap(PROT_READ|PROT_WRITE)`, write one byte. PASS
  (post-fix): the write faults inside the guest (SIGSEGV) or `mprotect(PROT_WRITE)`
  returns `EACCES`, and the VM keeps running. FAIL (pre-fix): `KVM_RUN` returns
  `-EFAULT` and the VMM exits.

The `--keep-guest-coherency` backend flag exists to rule the coherency rewrite in
or out while chasing one of these.

---

## Appendix — configuration by mode

| mode | `run-guest.sh` flags | host | guest |
|---|---|---|---|
| Wayland client + direct scanout | `--wayland-socket $SOCK` | Hyprland, `render:direct_scanout=2` | `nvgpu-wl-guest --socket wayland-0` |
| DRM lease → guest KMS | `--wayland-socket $SOCK --wayland-lease` | Hyprland patched, a `leasable` monitor | lease client + kmscube/modetest/`lease-flip` |
| VK_KHR_display | `--wayland-socket $SOCK --wayland-lease` | as lease | `vkcube --wsi display` |
| compositor-VM | `--kms-card` | **no** host compositor on the card, `nvidia_drm.modeset=1` | guest Hyprland, `/dev/dri/card*` |
| export | `--wayland-export /path/sock` (+ compositor-VM) | a host/2nd-VM client | `nvgpu-wl-guest --export …` |

The launcher also takes the Wayland limits, for the stages that push them:
`--wayland-max-conns N` (channels per VM, 64), `--wayland-shm-budget MIB` (1024),
`--wayland-queue-budget MIB` (256) and `--wayland-lease-interval SECS` (5).
Anything else goes to the backend after `--`, e.g. the diagnostic flags:
`--permissive-abi` (forward unchecked ioctls, loudly — for finding what a
workload needs, never for running one), `--keep-guest-coherency` (caching
stage), `--proc-nvidia PATH` (test against a fixture tree). The backend
refuses each of these, and `--rm-allowlist=log`, `--sandbox=off|best-effort`,
`--allow-root-unsafe` and `--allow-unmeasured-release`, unless `--diagnostic`
is given too (they are hidden from its `--help` without it); the launcher adds
`--diagnostic` when one is passed, or asked for by `NVGPU_SANDBOX=off` or
`NVGPU_ALLOW_ROOT_UNSAFE=1`:

```sh
sudo NVGPU_PREFIX=/root /root/bin/run-guest.sh --kms-card shell.sh kms1 -- --keep-guest-coherency
```

Guest packages the stages assume: NVIDIA userspace (Vulkan ICD, EGL), `nvgpu-wl-guest`,
and for the checks `vulkan-tools`, `mesa-demos`, `libdrm` (modetest), `kmscube`,
`drm_info`. On this box, get them through `nix shell` as in §0.4.

Host boot parameters: `nvidia_drm.modeset=1` always; `nvidia_drm.vblank=1` for the
lease/compositor-VM stages if vblank waits stall.
