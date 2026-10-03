# The rig

How every result in this repo's PRs and docs was produced, so it can be
produced again without whoever produced it. A workstation edits and relays; a
build host compiles; a GPU host boots guests.

## One-time setup

1. Copy `hosts.env.example` to `hosts-<target>.env` (not in git) and fill it in.
   Pick your own `TAG`, `BUILD_DIR`, `GPU_DIR`, `GPU_ROOTFS` and `GPU_LOGS`
   when someone else may be using the same box: two runs sharing an image or
   a log directory overwrite each other.
2. A base guest image (`GPU_ROOTFS_BASE`). See "The base image" below.
3. A VMM build with this release's device: `NESBOX_SRC=<nesbox checkout>
   rig.sh nesbox`. CUDA needs one that offers the UVM aperture (shm id 2).

## A full run

```sh
export RIG_TARGET=box1 TAG=me
./scripts/rig/rig.sh sync && ./scripts/rig/rig.sh build &&
./scripts/rig/rig.sh module && ./scripts/rig/rig.sh stage &&
BACKEND_ARGS="--caps graphics,compute,video,utility" \
  ./scripts/rig/rig.sh probe rig-probe-cuda.sh guest-probe-draw.sh \
  guest-probe-vulkan.sh guest-probe-encode.sh
```

Run it under bash. `probe` takes the init script's file name. Without
`compute` in `--caps` the backend refuses `/dev/nvidia-uvm` and CUDA reports
`CUDA_ERROR_UNKNOWN`, which names nothing, so check the caps before reading a
CUDA failure as a bug. Logs land in `GPU_LOGS` as
`<tag>-<probe>.{backend,console}.log`.

## The probes, and what passing looks like

Each `*.sh` in `guest/` runs as the guest's init. Each `*.c` is built on the
GPU host by `box-stage.sh` (`*.dyn.c` dynamically, the rest static) and
staged beside them in `/opt/nvgpu`.

| probe | passes when |
|---|---|
| `guest-probe-draw.sh` | every line PASS, and `red=11858 blue=53678 other=0`. The regression check for rendering: those numbers do not move |
| `guest-probe-vulkan.sh` | `nvidia-smi` and `vulkaninfo` name the card |
| `guest-probe-encode.sh` | a compositor, a Vulkan client presenting into it, the capture layer encoding H.264 on the GPU and a receiver (`nesrecv.c`) counting it: about 600 frames in the run, and the two `HOST:` lines PASS. `box-run.sh` pulls the stream out of the image and decodes it on the host, because a black or corrupt stream still arrives at full rate |
| `rig-probe-cuda.sh` | 10 of 10, ending in a 1 MiB round trip with 0 bytes different |
| `rig-probe-uvm.sh` | 7 of 7 UVM calls |
| `rig-probe-osdesc.sh` | memory registered by CPU address, read back |
| `rig-probe-caps.sh` | via `rig.sh caps`: each capability set shows exactly the nodes it should |
| `rig-probe-rmbench*.sh` | numbers, not pass/fail; compare with `rig.sh hostbench` |

Compare message counts in the backend's `served N message(s)` line with the
last run you trust. A change in the count with no change in the result is
worth one look before it is waved through.

## The base image

The probes need things the image carries: the Vulkan loader, `vulkaninfo`,
`vkcube`, and for the encode probe the compositor and
capture layer under `/opt/nescapture`. `image/stage-guest.sh` puts the
libraries the NVIDIA userspace opens by name (never in any NEEDED entry, so
no package pulls them in) and the probes into an image; `image/build-offscreen.sh`
builds `offscreen-draw`, which the draw probe runs. Missing `libdrm.so.2` in
the image does not fail anything: Vulkan silently reports no surface support
and presentation fails with no failing call anywhere.

## Traps

- `rig.sh nesbox` sends only the files git tracks. A checkout collects
  gigabytes of untracked images, and the workstation link is the slow one.
- Do not edit `rig.sh` while a run is executing it: bash reads it as it goes.
- A guest killed mid-run leaves the image's journal dirty; `box-stage.sh`
  checks it before writing.
- The build host's login shell may be fish; every remote command runs under
  `bash`.
