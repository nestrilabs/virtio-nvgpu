# Benchmarks

**Every number here was produced by running something**, on the hardware
named below, against the same machine's bare metal. Where a figure is
derived rather than observed, it says so.

The short version, on an RTX 5090 with the current code:

- **What runs on the GPU runs at native speed in a guest**: rendering
  whose frames take a millisecond or more is within 3% (within 1% at 4.7 ms
  and above), CUDA, OpenCL, NVDEC, NVENC and every host-GPU copy within the
  runs' spread, and 10,000 Vulkan draws a frame cost what they cost
  natively. A render loop crosses nothing.
- **What crosses to the host costs a round trip each**: an RM call, a CPU
  mapping of GPU memory, a fence wait that sleeps, a Wayland message.
  This work cut those: an RM control from a guest went from 7.9 to 2.4
  times native, a fresh GPU mapping is written at native speed instead of a
  tenth of it, and a mailbox Vulkan client presents twice as fast, within
  10% of native. What is left is quantified below; most of it is inherent
  in a device the host serves.
- **Start-up is set-up**: thousands of those calls, so a Vulkan device takes
  about 1.4 times as long to create, and CUDA's first context 1.3 times.

[Table](#native-against-a-guest) ·
[How it was measured](#how-it-was-measured) ·
[Where the time went](#where-the-time-went) ·
[What remains](#what-remains-and-why) ·
[What these numbers do not support](#what-these-numbers-do-not-support) ·
[Earlier, on an RTX 3060](#earlier-an-rtx-3060-before-protocol-v2) ·
[Re-taking these](#re-taking-these)

## Native against a guest

RTX 5090, 595.99.02, Ryzen 9 9950X, 2026-09-29. **Before** is
`display-passthrough` at `4e284e7` (with nesbox `virtio-nvgpu-v4` and
crosvm `patches/crosvm` 0001-0009); **after** is branch `perf` (nesbox
`virtio-nvgpu-v5`, crosvm with 0010). Each cell is the mean of three runs ±
half their range (native: of six, taken through the day). **×native** is
the guest after against native, oriented so that above 1 is the guest's
loss: its time over native's, or native's rate over its.

**GPU-bound rendering**, nothing presented: one 1920x1080 triangle of dialled fragment cost, submitted and waited for per frame (median frame time); glmark2's heavier scenes off screen

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| cost 32768 (18.6 ms natively) | ms | 18.6 ± 0.015 | 18.6 ± 0.015 | 18.6 ± 0.01 | 1.00 | 18.6 ± 0.005 | 18.6 ± 0.005 | 1.00 |
| cost 8192 | ms | 4.69 ± 0.005 | 4.74 ± 0.008 | 4.73 ± 0.012 | 1.01 | 4.72 ± 0.006 | 4.74 ± 0.015 | 1.01 |
| cost 2048 | ms | 1.19 ± 0.0055 | 1.22 ± 0.0085 | 1.22 ± 0.004 | 1.03 | 1.22 ± 0.0035 | 1.23 ± 0.0015 | 1.03 |
| cost 512 | ms | 0.323 ± 0.00675 | 0.349 ± 0.00295 | 0.347 ± 0.0038 | 1.08 | 0.347 ± 0.0052 | 0.356 ± 0.00295 | 1.10 |
| cost 64 | ms | 0.0623 ± 0.00101 | 0.0775 ± 0.00501 | 0.0759 ± 0.00422 | 1.22 | 0.079 ± 0.00466 | 0.0781 ± 0.0015 | 1.25 |
| cost 0: the round trip alone | ms | 0.0306 ± 0.00202 | 0.0413 ± 0.00096 | 0.0415 ± 0.00018 | 1.36 | 0.0456 ± 0.00374 | 0.0467 ± 0.0026 | 1.53 |
| glmark2 terrain | fps | 2955 ± 7.00 | 2860 ± 57.5 | 2825 ± 17.5 | 1.05 | 2820 ± 36.0 | 2802 ± 23.5 | 1.05 |
| glmark2 refract | fps | 7736 ± 54.0 | 7474 ± 21.5 | 7443 ± 71.0 | 1.04 | 7510 ± 122 | 7344 ± 53.0 | 1.05 |
| glmark2 jellyfish | fps | 39,731 ± 651 | 41,332 ± 1072 | 41,959 ± 130 | 0.95 | 40,993 ± 1052 | 41,898 ± 392 | 0.95 |
| glmark2 score (13 scenes) | points | 41,618 ± 406 | 46,002 ± 187 | 46,426 ± 266 | 0.90 | 45,774 ± 193 | 46,228 ± 490 | 0.90 |

**Compute**

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| CUDA n-body | GFLOP/s | 24,220 ± 249 | 24,280 ± 17.5 | 24,318 ± 37.5 | 1.00 | 24,278 ± 9.50 | 24,278 ± 6.00 | 1.00 |
| OpenCL fp32 (clpeak) | gflops | 118,354 ± 328 | 118,199 ± 267 | 118,351 ± 170 | 1.00 | 118,228 ± 212 | 118,236 ± 223 | 1.00 |
| OpenCL global memory bandwidth | gbps | 1661 ± 5.08 | 1659 ± 1.80 | 1662 ± 3.23 | 1.00 | 1660 ± 3.12 | 1666 ± 3.89 | 1.00 |
| Blender Cycles, CUDA (720p, 128 samples) | s | 1.20 ± 0.02 | 2.08 ± 0.08 | 2.00 ± 0.005 | 1.67 | 2.86 ± 0.025 | 2.92 ± 0.01 | 2.44 |
| Blender Cycles, OptiX | s | 1.27 ± 0.015 | 2.55 ± 0.035 | 2.52 ± 0 | 1.99 | 3.47 ± 0.135 | 3.26 ± 0.045 | 2.57 |
| kernel launch + sync, spinning (p50) | us | 4.98 ± 0.0345 | 4.93 ± 0.0105 | 4.97 ± 0.0395 | 1.00 | 5.00 ± 0.0145 | 4.95 ± 0.025 | 0.99 |
| kernel launch + sync, blocking (p50) | us | 7.12 ± 0.325 | 19.0 ± 0.46 | 19.0 ± 0.395 | 2.67 | 20.9 ± 0.25 | 24.2 ± 4.39 | 3.40 |
| kernel launches, pipelined | launches/s | 780,433 ± 21,750 | 775,800 ± 4450 | 750,067 ± 18,250 | 1.04 | 744,300 ± 37,650 | 733,133 ± 34,150 | 1.06 |
| cuMemAlloc + cuMemFree, 1 MiB (p50) | us | 43.3 ± 1.02 | 103 ± 4.67 | 53.5 ± 0.995 | 1.24 | 110 ± 1.05 | 65.6 ± 18.2 | 1.52 |
| cuMemAlloc + cuMemFree, 64 MiB (p50) | us | 174 ± 9.00 | 195 ± 6.40 | 189 ± 7.10 | 1.09 | 206 ± 10.2 | 191 ± 6.40 | 1.10 |
| cuMemHostRegister, 64 MiB (p50) | ms | 1.29 ± 0.086 | 5.33 ± 0.894 | 4.17 ± 1.31 | 3.24 | 4.20 ± 0.547 | 5.79 ± 0.146 | 4.50 |
| cuCtxCreate | ms | 52.6 ± 2.75 | 68.1 ± 0.81 | 67.0 ± 1.27 | 1.27 | 107 ± 1.45 | 108 ± 8.15 | 2.05 |

**Memory**

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| cuMemcpy host to device, pageable | GB/s | 14.0 ± 0.165 | 14.2 ± 0.025 | 14.1 ± 0.1 | 0.99 | 13.9 ± 0.12 | 14.1 ± 0.105 | 0.99 |
| cuMemcpy host to device, pinned | GB/s | 14.4 ± 0.005 | 14.4 ± 0.005 | 14.4 ± 0.005 | 1.00 | 14.4 ± 0.005 | 14.4 ± 0 | 1.00 |
| cuMemcpy device to host, pinned | GB/s | 14.3 ± 0.01 | 14.3 ± 0.005 | 14.3 ± 0.01 | 1.00 | 14.3 ± 0.005 | 14.3 ± 0 | 1.00 |
| vkCmdCopyBuffer staging to device, 256 MiB | GB/s | 13.3 ± 0.04 | 13.3 ± 0.005 | 13.3 ± 0.03 | 1.00 | 13.3 ± 0.02 | 13.3 ± 0.045 | 1.00 |
| vkCmdCopyBuffer device to staging | GB/s | 13.1 ± 0.05 | 13.1 ± 0.035 | 13.0 ± 0.035 | 1.01 | 13.1 ± 0.045 | 13.1 ± 0.02 | 1.00 |
| glTexSubImage2D, 4096x4096 RGBA8 | GB/s | 8.24 ± 0.064 | 5.94 ± 0.263 | 5.91 ± 0.268 | 1.39 | 6.04 ± 0.237 | 5.78 ± 0.106 | 1.43 |
| glReadPixels, 4096x4096 RGBA8 | GB/s | 13.5 ± 0.255 | 13.5 ± 0.065 | 13.6 ± 0.01 | 0.99 | 13.6 ± 0 | 13.6 ± 0.01 | 0.99 |
| CPU write, device-local host-visible (BAR1) | GB/s | 11.1 ± 0.2 | 11.1 ± 0.05 | 11.0 ± 0.125 | 1.01 | 11.1 ± 0.075 | 10.9 ± 0.205 | 1.02 |
| CPU first write, fresh 64 MiB BAR1 mapping | GB/s | 11.1 ± 0.26 | 1.09 ± 0.143 | 11.1 ± 0.11 | 1.00 | 1.21 ± 0.148 | 11.1 ± 0.155 | 1.01 |
| CPU read, BAR1 (uncached) | GB/s | 0.0848 ± 0.0105 | 0.0856 ± 0.0106 | 0.081 ± 0.00924 | 1.05 | 0.0832 ± 0.0101 | 0.079 ± 0.0109 | 1.07 |
| CPU write, host-cached system memory | GB/s | 23.0 ± 1.65 | 24.6 ± 0.605 | 24.3 ± 0.4 | 0.95 | 24.1 ± 0.515 | 23.5 ± 0.49 | 0.98 |
| CPU first write, fresh host-cached system memory | GB/s | 21.0 ± 0.895 | 1.05 ± 0.142 | 1.00 ± 0.149 | 20.92 | 1.15 ± 0.139 | 1.02 ± 0.148 | 20.69 |
| CPU read, host-cached system memory | GB/s | 23.9 ± 0.52 | 24.0 ± 0.53 | 23.7 ± 0.445 | 1.01 | 23.3 ± 0.335 | 22.7 ± 0.645 | 1.05 |
| CPU read, host-coherent (uncached) system memory | GB/s | 0.669 ± 0.0156 | 32.2 ± 2.92 | 27.2 ± 4.42 | 0.02 | 28.6 ± 2.27 | 25.0 ± 9.39 | 0.03 |
| CPU first write, fresh malloc (guest RAM) | GB/s | 3.58 ± 0.178 | 8.30 ± 0.65 | 5.95 ± 2.55 | 0.60 | 0.505 ± 0.067 | 0.466 ± 0.0512 | 7.68 |

**Driver overhead and the control path**

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| 10,000 Vulkan draws, recorded, submitted, waited | ms | 0.35 ± 0.00565 | 0.337 ± 0.00615 | 0.331 ± 0.0063 | 0.95 | 0.339 ± 0.0062 | 0.35 ± 0.0118 | 1.00 |
| vkQueueSubmit, pipelined | submits/s | 254,850 ± 1100 | 255,500 ± 500 | 254,967 ± 750 | 1.00 | 253,133 ± 1900 | 255,533 ± 550 | 1.00 |
| vkQueueSubmit + vkWaitForFences (p50) | us | 20.2 ± 1.01 | 34.6 ± 2.64 | 36.1 ± 1.36 | 1.78 | 38.1 ± 2.41 | 38.4 ± 5.57 | 1.90 |
| glClear + glFinish (p50) | us | 7.71 ± 0.1 | 6.99 ± 0.015 | 6.98 ± 0.005 | 0.90 | 6.99 ± 0 | 7.00 ± 0.0045 | 0.91 |
| RM control, GPU_GET_ATTACHED_IDS (p50) | us | 1.46 ± 0.0155 | 11.5 ± 0.685 | 3.44 ± 0.0255 | 2.36 | 13.4 ± 1.43 | 4.97 ± 2.17 | 3.41 |
| open + close /dev/nvidiactl (p50) | us | 1.32 ± 0.015 | 8.60 ± 1.30 | 6.94 ± 0.606 | 5.24 | 13.4 ± 4.87 | 10.7 ± 6.13 | 8.06 |
| RM video memory, alloc + free 2 MiB (p50) | us | 14.8 ± 0.12 | 32.5 ± 1.78 | 19.1 ± 0.165 | 1.29 | 35.6 ± 2.78 | 21.8 ± 4.17 | 1.47 |
| RM map, mmap, touch, unmap 2 MiB (p50) | us | 48.0 ± 0.585 | 166 ± 6.65 | 143 ± 0.75 | 2.99 | 214 ± 13.7 | 197 ± 13.5 | 4.12 |
| vkAllocateMemory + free, 1 MiB device-local (p50) | us | 119 ± 0.35 | 181 ± 10.2 | 139 ± 6.05 | 1.17 | 201 ± 10.9 | 146 ± 15.5 | 1.23 |
| the same, host-visible device-local, mapped (p50) | us | 147 ± 2.10 | 324 ± 18.6 | 262 ± 6.90 | 1.79 | 394 ± 18.0 | 305 ± 27.3 | 2.08 |
| the same, host-cached, mapped (p50) | us | 125 ± 1.70 | 356 ± 40.5 | 290 ± 33.1 | 2.32 | 390 ± 6.60 | 322 ± 18.9 | 2.57 |

**Start-up** (a process each, second of three)

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| vkCreateInstance + vkCreateDevice | ms | 96.4 ± 1.55 | 142 ± 0.75 | 137 ± 1.45 | 1.42 | 164 ± 28.2 | 137 ± 0.65 | 1.42 |
| EGL display and context | ms | 0.809 ± 0.0375 | 0.954 ± 0.0459 | 0.863 ± 0.0031 | 1.07 | 1.06 ± 0.132 | 1.44 ± 0.763 | 1.78 |
| cuInit | ms | 50.9 ± 1.34 | 59.7 ± 1.94 | 57.9 ± 0.92 | 1.14 | 64.2 ± 5.62 | 62.1 ± 2.71 | 1.22 |
| nvidia-smi -L | ms | 17.0 ± 0.255 | 13.9 ± 0.21 | 13.1 ± 0.275 | 0.77 | 18.5 ± 6.19 | 13.4 ± 0.245 | 0.79 |
| vulkaninfo --summary | ms | 1066 ± 6.00 | 433 ± 10.8 | 411 ± 5.15 | 0.39 | 501 ± 64.6 | 421 ± 8.80 | 0.39 |
| vkcube, start to first frame presented | ms | 169 ± 3.55 | 218 ± 7.15 | 205 ± 2.05 | 1.21 | 355 ± 93.8 | 243 ± 20.5 | 1.44 |

**Video** (ffmpeg, a 1080p clip looped to 3,000 frames, frames kept on the GPU; process start included)

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| NVDEC H.264 | fps | 1814 ± 16.5 | 1674 ± 31.0 | 1684 ± 12.5 | 1.08 | 1554 ± 32.5 | 1563 ± 33.5 | 1.16 |
| NVDEC AV1 | fps | 1598 ± 14.0 | 1506 ± 10.5 | 1489 ± 24.5 | 1.07 | 1406 ± 10.5 | 1424 ± 40.5 | 1.12 |
| Vulkan Video decode, HEVC | fps | 1457 ± 4.00 | 1406 ± 7.00 | 1418 ± 12.5 | 1.03 | 1285 ± 35.0 | 1327 ± 10.0 | 1.10 |
| Vulkan Video decode, AV1 | fps | 1390 ± 12.0 | 1303 ± 17.0 | 1318 ± 5.50 | 1.05 | 1126 ± 52.5 | 1179 ± 30.0 | 1.18 |
| NVENC H.264, from NVDEC | fps | 832 ± 5.00 | 823 ± 8.05 | 831 ± 5.75 | 1.00 | 787 ± 15.1 | 803 ± 0.85 | 1.04 |
| NVENC AV1, from NVDEC | fps | 860 ± 2.60 | 852 ± 3.25 | 845 ± 2.35 | 1.02 | 795 ± 8.65 | 791 ± 18.9 | 1.09 |

**Wayland**, through the proxy to a headless sway

| figure | unit | native | nesbox before | nesbox after | ×native | crosvm before | crosvm after | ×native |
|---|---|---|---|---|---|---|---|---|
| wl_shm client, full 1080p frames | frames/s | 1881 ± 55.0 | 266 ± 5.55 | 273 ± 8.65 | 6.88 | 253 ± 4.75 | 273 ± 17.2 | 6.88 |
| vkmark "clear", mailbox | fps | 22,780 ± 676 | 4658 ± 154 | 9541 ± 210 | 2.39 | 2991 ± 548 | 2755 ± 501 | 8.27 |
| vkmark "cube", mailbox | fps | 11,471 ± 801 | 5015 ± 44.0 | 10,488 ± 624 | 1.09 | 2583 ± 485 | 3219 ± 750 | 3.56 |
| the backend's CPU meanwhile | % | -- | 115 ± 1.05 | 125 ± 1.25 |  | 98.2 | -- |  |

Some rows are faster in a guest. Reading host-coherent system memory is
uncached natively (0.67 GB/s) but cached in the guest, whose memory type
for it is write-back by design (ARCHITECTURE.md §15). `vulkaninfo` and
`nvidia-smi` have more to enumerate on the host. glmark2's light scenes
(tens of thousands of frames a second, a `glFinish` each) run up to 20%
faster in a guest, which is not explained. And 10,000 GL draws a frame
(not in the table) took 1.7 ms natively against 0.44 ms in a guest, which
turned out to be the native driver's: natively they slow down five-fold
once the same process has opened a Vulkan device, as the suite's does
first; alone they take 0.35 ms natively (0.28 ms on 4 CPUs), so a guest
pays about 1.25 times for them, on the CPU.

## How it was measured

**Hardware and software.** RTX 5090 (32 GiB), NVIDIA open modules and
userspace 595.99.02, Ryzen 9 9950X (16 cores, 32 threads), 91 GiB RAM,
host Linux 7.2.7 (NixOS) with `kvm_amd` (AVIC on, halt polling 200 µs).
Guest: Linux 7.2.7, 4 vCPUs, 4 GiB, the guest module (C parsers) at its
defaults, as root in the rig's image; the backend at its defaults
(`--queue-poll-us 50`, `--sched-slice-us 100`), RM allowlist enforcing,
sandbox on, `--allow-compute` (the CUDA rows need it; nothing else changes
with it). nesbox prefaults guest RAM onto huge pages (its default); crosvm
keeps its default per-vCPU core scheduling and jails its frontend.

**Native** is the same programs on the host: the guest image's own
binaries and its NVIDIA userspace (the same 595.99.02 files), through
`rig/rig-native-run.sh`, in the environment the guest's probes have (no
`XDG_DATA_DIRS`: with it the Vulkan loader finds MangoHud's and gamescope's
implicit layers, which the guest never loads, and `vulkaninfo` took twice as
long).

**Nothing is presented to a monitor.** Every Wayland client, native or in a
guest, is a client of the rig's headless sway (`rig/rig-headless-sway.sh`,
Vulkan renderer on the same 5090), which a guest reaches through the Wayland
proxy. The rendering rows draw off screen.

**Runs.** `rig/rig-bench.sh` (rig/TESTING-RIG.md, "Benchmarks") boots a
guest per run and runs `nvgpu-bench-suite` in it, or runs the same suite
natively; runs of each configuration were interleaved, with nothing else
running on the host (no build, the desktop idle). Inside a run each latency
is the mean, median or 99th percentile of thousands of calls after a
warm-up; each rendering figure is 3 s after a 1.5 s warm-up (the GPU's clock
ramp); each bandwidth is several 64-256 MiB transfers after a first one. The
programs: `nvgpu-bench` (Vulkan, GL, RM and `wl_shm`) and `nvgpu-cubench`
(CUDA driver API), both in `rig/guest-image/tools/`, with glmark2, clpeak,
Blender's Cycles, ffmpeg and vkmark.

## Where the time went

**1. Six system calls a call, in the backend** (fixed). Every RM call and
every IOCTL2 built its parameter blocks as fresh guarded mappings -- a
`PROT_NONE` page behind a page of zeroed slack, so a driver writing past a
block faults at once (SECURITY.md §14) -- and unmapped them afterwards.
`strace -c` of the backend serving 21,003 RM controls counted 42,019
`mmap`, 42,008 `mprotect` and 42,006 `munmap` beside the 21,003 `ioctl`s,
and in a process of many threads every `munmap` is a TLB shootdown. The
backend took **10.2 µs** to serve an RM control the host driver answers in
1.4 (`bench_rm_control_service`, a test that drives the real driver through
the backend's own dispatch). Small blocks now come from a bounded pool,
zeroed through their reach with the guard intact (SECURITY.md §21): **2.6
µs**. Every figure that crosses moved with it, a presented frame most of
all, since it is a dozen IOCTL2s.

**2. One second-level fault per page of fresh GPU memory** (fixed for video
memory). A guest writing 64 MiB of video memory it had just mapped ran at
1.1 GB/s where the host writes 11. KVM's per-vCPU statistics, read around a
bare guest writing such a mapping (a small KVM program of our own), count
16,385 `pf_fixed` for 16,384 pages, 3.1 µs each: the window is mapped into
the VMM at once, but into the guest's nested page tables one fault at a
time. Both VMMs now prefault each placement with `KVM_PRE_FAULT_MEMORY`
from a spare vCPU that never runs, 0.13 µs a page, after answering it
(SECURITY.md §21). A fresh device-local mapping is now written at the host's
speed. System memory is prefaulted the same way, but the call maps as a
read fault would, and KVM maps the driver's system memory read-only on a
read fault: a guest's first **read** of it is now free, its first **write**
still faults, once a page (row "first write, fresh host-cached system
memory"), and nothing short of mapping it writable up front, which KVM
offers no call for, would change that.

**3. A reply interrupt per round trip** (fixed). A guest caller spins up to
20 µs for its reply (DEPLOY.md, "Frame pacing"), but each reply still came
as an interrupt: the host signalled an eventfd, KVM injected, the guest's
handler took the reply off the ring. Now, while callers spin and none sleeps,
the spinners take replies off the ring themselves with the control queue's
interrupt off; a caller that sleeps turns it back on first (SECURITY.md §21).
Three runs each, the same backend: an RM control from a guest 4.7 -> 3.4 µs,
a mailbox Vulkan client +30%.

**4. Copies of every `wl_shm` frame** (fewer). A committed 1080p
shared-memory buffer was copied seven times on its way to the host
compositor: four times in the guest daemon, into the transport, out of it,
into the compositor's memfd. Two of the daemon's are gone (266 frames a
second before, 273-289 across the runs after). The rest
is the design: a guest's pool is guest RAM, which the host compositor
cannot map (ARCHITECTURE.md §14), so its pixels must travel, and a 4 MiB
frame is about 0.5 ms of the backend's queue thread.

**5. The floor of a crossing.** A request that does almost nothing on the
host (TIME_SYNC) takes a guest about 3.7 µs there and back: its copy and
descriptors, the ring, the queue thread (which polls the ring 50 µs after
each drain, so a burst of requests costs no kick), and now no interrupt.
An RM control is that plus the host driver's 1.4 µs and the backend's
checking, translating and scrubbing. Every control-path row is a count of
crossings times that.

**6. A wait that sleeps** (not changed). A Vulkan fence the guest sleeps on
costs about 16 µs more than natively (36 against 20 µs a submit-and-wait),
a CUDA context with blocking sync 12 µs more (19 against 7 µs a launch):
the guest arms the descriptor (`W_ARM`, from a work item), the backend's
event pump wakes on the host's event and sends one record, and an interrupt
wakes the guest's poller. Keeping the pump polling 30 µs after each arm
(`--pump-poll-us`, tried) found 90% of those events without a host wakeup
but saved only 2 µs, for up to 30 µs of a host core per wait; not kept.
Waits that spin -- CUDA's default schedule, GL's `glFinish` -- cost what
they cost natively.

**7. Placing memory is the VMM's** (inherent). Every CPU mapping of GPU
memory is a request to the VMM, which owns guest physical space
(ARCHITECTURE.md §5); every unmap is another, with a nested-page-table
invalidation and a TLB shootdown of every vCPU before RM may free the
memory. A 2 MiB map, touch and unmap costs a guest about 3 times what it
costs natively; allocating and mapping 1 MiB of host-visible Vulkan memory,
about 1.8 times. The order cannot be relaxed: memory RM has freed must not
stay reachable from a guest.

**8. CPUs, not virtualisation.** Blender's Cycles took 1.7 times as long in
a guest (CUDA 2.0 s against 1.2), and the syscalls around it take the same
wall time both ways (`strace -f -w -c`: 0.38 s in the guest, 0.41 s
natively). It builds its scene on the CPU: natively, confined to 4 CPUs
like the guest (`taskset -c 0-3`), it takes 2.10 s with CUDA and 2.16 s
with OptiX. A guest given the host's cores would close it.

**9. crosvm** is 10-50% slower than nesbox on anything that crosses, and
presents at a third of nesbox's rate or less: by default it gives each vCPU
a core-scheduling cookie of its own, a side-channel mitigation that keeps anything else off
a running vCPU's SMT sibling, so the backend's threads and the vCPU they
answer never share a core. With `--core-scheduling=false`
(`NVGPU_CROSVM_CORE_SCHED=0`, two runs, the same build) a mailbox Vulkan
client went from 2,755 to 4,962 fps ("clear") and from 3,219 to 5,954
("cube"). That is a security trade for the deployment to make (DEPLOY.md,
"Frame pacing"; SECURITY.md §20); the default stays. crosvm also does not prefault guest RAM: a guest's first
touch of its own memory runs at 0.4 GB/s against nesbox's 8
(`--hugepages` made no difference).

## What remains, and why

- **Per crossing, about 2 µs more than natively** for the cheapest RM
  call, most of it the round trip itself (ring, queue thread, the guest's
  copy). A game or a compositor makes a few crossings a frame; CUDA and a
  render loop make none. Batching would need the guest to know which calls
  may be deferred, which is the user-mode driver's knowledge, not ours.
- **A sleeping wait, 12-16 µs more**: a host event must wake a host
  thread and then a vCPU. Spinning longer in the guest trades a vCPU for it.
- **Mapping GPU memory, about 2-3 times native**: the VMM places every
  mapping, and an unmap must reach every vCPU before the host frees the
  memory.
- **The first write to fresh system memory**, a fault a page (see 2).
- **`wl_shm`, about 7 times fewer 1080p frames a second** than natively,
  still 270-290 a second: shared memory is copied because it is guest RAM. A
  dma-buf client (every GPU-rendered one) crosses no pixels.
- **Start-up, 1.2-1.4 times native**: thousands of crossings.
- **crosvm's core scheduling**: a security choice, above.

## What these numbers do not support

- **One card, one driver, one host.** RTX 5090, 595.99.02, a 9950X.
- **One guest at a time.** Several guests on one card were last measured
  on an RTX 3060, on the code before protocol v2 (below).
- **No other hypervisor.** nesbox and crosvm run the same device; nothing
  here compares this project with Venus, vGPU or passthrough.
- **Nothing presented.** Frame pacing on a monitor is in DEPLOY.md, "Frame
  pacing" (measured the same day, before this work).
- **Synthetic loads**, and Blender. No game was timed here; SuperTuxKart's
  frame pacing is in DEPLOY.md.

## Earlier: an RTX 3060, before protocol v2

What this file said before, measured on an RTX 3060 (595.99.02, a Ryzen 7
9850X3D, Ubuntu 26.04) on the code before protocol v2 and the security work
that came with it, with nesbox's `nesprobe` (a headless load of dialled
cost, 30 s after an 8 s warm-up, median of three). It is kept because two
claims elsewhere rest on it and nothing since has contradicted it.

- **The frame time.** Within 2% of the host at 2 ms a frame and above
  (+1.7% at 1.98 ms, -0.4% at 39 ms), +7.1% at 0.5 ms, +41% at 0.05 ms --
  the shape the RTX 5090 rows above show again.
- **The CPU.** Unpaced at about 100 fps for 12 s, one guest used 0.37 s of
  CPU where the host used 0.40 s: over 813,691 frames the backend served
  13,792 messages, nearly all of them device set-up.
- **Several guests on one card.** One, two and four guests running the same
  load took 102.9, 102.4 and 103.7 fps between them, split evenly (four at
  25.84, 26.49, 25.57 and 25.79 fps), against 100.9 bare metal; four
  rendered correctly at once, and four encoded H.264 at once, each at 60 Hz.
  Four is what was run, not a limit found; it has not been repeated on the
  current code.
- **The whole chain.** A Wayland client presenting inside the guest,
  captured and encoded on its own device by Vulkan Video and received at the
  far end of a socket, decoded without an error: about 600 frames at a
  median spacing of 16.67 ms, 60 Hz.

## Re-taking these

```sh
rig/rig-headless-sway.sh &                 # the compositor nothing is shown on
export NVGPU_BENCH_GROUPS="micro cuda gpu video startup wl"
rig/rig-bench.sh native n 3
NVGPU_COMPUTE=1 rig/rig-bench.sh vm g 3
NVGPU_COMPUTE=1 NVGPU_VMM_KIND=crosvm rig/rig-bench.sh vm c 3
rig/bench-stats.py native='.rig/logs/bench/n/*.bench' nesbox='.rig/logs/bench/g/*.bench' \
    crosvm='.rig/logs/bench/c/*.bench'
# the backend's own cost of an RM control, on the real driver
cargo test --release -p device --lib bench_rm_control_service -- --ignored --nocapture
```

Each guest run also records the backend's and the VMM's CPU per group, both
sides' pacing counters, and the backend's log (rig/TESTING-RIG.md,
"Benchmarks"). **Check for a stray VM and a running build before believing
anything**: either moves every figure, and looks like an ordinary result.
