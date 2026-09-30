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
- **Games and renderers** ([Heavy workloads](#heavy-workloads)): GPU-bound
  ones run at native speed under nesbox, lows included, and pace like the
  host on a 240 Hz monitor. A game presenting a thousand frames a second
  loses 8-14% to explicit sync's eleven round trips a frame, and a frame
  that writes freshly allocated staging memory runs at a quarter of native
  speed, a page fault a page.
- **Steam-like games** ([Steam-like games](#steam-like-games)): D3D11 and
  D3D12 under Wine, a job-system engine and a native Vulkan game run within
  a few percent of the same program on as many host CPUs as the guest has
  vCPUs; what a 4-vCPU guest loses against the whole host is the other
  CPUs. The frame-pacing measures cost no throughput. The guest kernel
  lacked huge pages (10%) and ntsync (11-13% under Wine), and without compute a
  game that enables ray tracing or DLSS did not start; both are fixed.

[Table](#native-against-a-guest) ·
[How it was measured](#how-it-was-measured) ·
[Where the time went](#where-the-time-went) ·
[What remains](#what-remains-and-why) ·
[What these numbers do not support](#what-these-numbers-do-not-support) ·
[Heavy workloads](#heavy-workloads) ·
[Steam-like games](#steam-like-games) ·
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
for it is write-back by design (ARCHITECTURE.md, "Memory types, and coherency"). `vulkaninfo` and
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
block faults at once (SECURITY.md, "Memory safety") -- and unmapped them afterwards.
`strace -c` of the backend serving 21,003 RM controls counted 42,019
`mmap`, 42,008 `mprotect` and 42,006 `munmap` beside the 21,003 `ioctl`s,
and in a process of many threads every `munmap` is a TLB shootdown. The
backend took **10.2 µs** to serve an RM control the host driver answers in
1.4 (`bench_rm_control_service`, a test that drives the real driver through
the backend's own dispatch). Small blocks now come from a bounded pool,
zeroed through their reach with the guard intact (SECURITY.md, "Memory safety"): **2.6
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
(SECURITY.md, "Prefaulting the window"). A fresh device-local mapping is now written at the host's
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
interrupt off; a caller that sleeps turns it back on first (SECURITY.md, "Frame pacing").
Three runs each, the same backend: an RM control from a guest 4.7 -> 3.4 µs,
a mailbox Vulkan client +30%.

**4. Copies of every `wl_shm` frame** (fewer). A committed 1080p
shared-memory buffer was copied seven times on its way to the host
compositor: four times in the guest daemon, into the transport, out of it,
into the compositor's memfd. Two of the daemon's are gone (266 frames a
second before, 273-289 across the runs after). The rest
is the design: a guest's pool is guest RAM, which the host compositor
cannot map (ARCHITECTURE.md, "The Wayland proxy"), so its pixels must travel, and a 4 MiB
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
(ARCHITECTURE.md, "How memory travels"); every unmap is another, with a nested-page-table
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
"Frame pacing"; SECURITY.md, "Frame pacing"); the default stays. crosvm also does not prefault guest RAM: a guest's first
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
- **Nothing presented** in the table above. Frame pacing on a monitor is in
  DEPLOY.md, "Frame pacing", and one heavy game's in "Heavy workloads".
- **Synthetic loads** in the table above; games, an engine, Blender and a
  streaming loop are in "Heavy workloads".

## Heavy workloads

RTX 5090, 595.99.02, Ryzen 9 9950X, 2026-09-29. Games, an engine, a
renderer and a texture-streaming loop, each run unpaced (vsync off, no frame
cap) against the rig's headless sway, and every frame's time recorded the
same way natively and in a guest (`rig/rig-heavy.sh`; rig/TESTING-RIG.md,
"Heavy workloads"): MangoHud's present-to-present time for SuperTuxKart,
the programs' own clocks for Godot, Blender and `vk-stream`. The code is
`display-passthrough` at `f462409` with the backend, nesbox and crosvm it
installed; the guest has 4 vCPUs, 8 GiB and `--allow-compute` (Godot's
Forward+ needs UVM), the backend its defaults, sandbox on, RM allowlist
enforcing. Three runs of each, interleaved, mean ± half the range; Blender
two. **1% low** and **0.1% low** are the average rate over the slowest 1%
and 0.1% of frames. **×native** is native's rate over the guest's.
**native, 4 CPUs** is the same program confined to as many CPUs as the
guest has (`taskset -c 0-3`), which separates what the guest's CPU count
costs from what the device costs.

**GPU-bound.** SuperTuxKart 1.5's `--benchmark` (a replayed race) at
3840x2160 with every effect on (dynamic lights, 2048 shadows, SSAO, glow,
light shafts, DoF, motion blur, MLAA, HD textures); Godot 4.7 Forward+ at
3840x2160 with SDFGI, volumetric fog, SSAO, SSIL, SSR and 48 shadowed omni
lights over 900 meshes; Blender 5 EEVEE, 1920x1080 at 64 samples, 150
subdivided meshes, ray tracing and volumetrics, from its UI (s a frame).

| workload | figure | native | nesbox | ×native | crosvm | ×native |
|---|---|---|---|---|---|---|
| SuperTuxKart, 4K ultra | avg fps | 198.9 ± 3.2 | 198.7 ± 4.4 | 1.00 | 197.4 ± 3.8 | 1.01 |
| | 1% / 0.1% low | 121.5 / 64.5 | 129.9 / 77.3 | | 97.0 / 31.4 | |
| | p99 / p99.9 ms | 6.73 / 9.59 | 6.37 / 10.25 | | 6.73 / 16.45 | |
| Godot, SDFGI 4K | avg fps | 228.7 ± 4.1 | 229.8 ± 5.2 | 1.00 | 225.7 ± 6.8 | 1.01 |
| | 1% / 0.1% low | 181.6 / 159.4 | 190.6 / 174.8 | | 151.4 / 119.4 | |
| Blender EEVEE, GL | s a frame | 2.56 | 2.56 | 1.00 | 3.04 | 1.19 |
| Blender EEVEE, Vulkan | s a frame | 2.47 | -- (hung, below) | | 2.77 (1 of 2) | 1.12 |

Blender ran with a 16 GiB window (`NVGPU_WINDOW_MIB=16384
NVGPU_WINDOW_SHARE=90`); at the default it does not run (4, below). Its
guest runs hung until FIFO_DISABLE_CHANNELS was allowed (below); since,
24 under nesbox averaged 2.53 s a frame and 10 under crosvm 2.56 s
(`hang-watch.sh`, not interleaved with native runs, so not in the table).

**CPU- and present-bound.** The same SuperTuxKart race at 1280x720 on the
legacy pipeline with every effect off (GL), and at 3840x2160 on its Vulkan
renderer, which has no advanced pipeline and so is light too; Godot's
"draws" scene, 20,000 meshes of 64 shapes and 97 materials the renderer
cannot merge, at 1280x720.

| workload | figure | native | native, 4 CPUs | nesbox | ×native (4 CPUs) | crosvm | ×native (4 CPUs) |
|---|---|---|---|---|---|---|---|
| SuperTuxKart 720p, GL | avg fps | 1201 ± 30 | 1176 ± 2 | 1088 ± 6 | 1.10 (1.08) | 979 ± 6 | 1.23 (1.20) |
| | p50 / p99 ms | 0.780 / 1.58 | 0.806 / 1.53 | 0.843 / 1.54 | | 0.950 / 2.08 | |
| | 1% / 0.1% low | 419 / 144 | 442 / 147 | 560 / 282 | | 336 / 172 | |
| SuperTuxKart 4K, Vulkan | avg fps | 1499 ± 6 | 1374 ± 7 | 1203 ± 11 | 1.25 (1.14) | 986 ± 20 | 1.52 (1.39) |
| | p50 / p99 ms | 0.628 / 1.07 | 0.689 / 1.13 | 0.797 / 1.24 | | 0.913 / 2.82 | |
| Godot, 20,000 draws, Vulkan | avg fps | 112.5 ± 1.7 | 116.5 ± 1.9 | 108.5 ± 6.4 | 1.04 (1.07) | 101.8 ± 6.5 | 1.11 (1.14) |
| | 1% low | 91.5 | 93.3 | 78.0 ± 30 | | 67.9 | |

The guest's 1% and 0.1% lows at 720p are better than native's: natively
the race's heaviest stretches run on more threads than the guest has, and
the frame time varies twice as much (CV 1.07 against 0.47). Godot's GL
"draws" scene has no row: it is bimodal natively too (61 or 28 fps from one
run to the next).

**Streaming.** `nvgpu-bench vk-stream`, a frame as an engine without a
sub-allocator streams textures: four fresh host-visible staging buffers of
256 KiB to 2 MiB allocated, mapped and written by the CPU, copied on one
submit, 0.6 ms of GPU work on a second submit that waits on the first
through a semaphore, two frames in flight. `vk-stream-pool` is the same
frame from staging memory allocated once and kept mapped.

| workload | figure | native | nesbox | ×native | crosvm | ×native |
|---|---|---|---|---|---|---|
| vk-stream | avg fps | 781 ± 7 | 212 ± 11 | 3.69 | 206 ± 8 | 3.79 |
| | CPU a frame: allocate + map / first write | 1.17 / 0.20 ms | 2.08 / 3.76 ms | | | |
| vk-stream-pool | avg fps | 1126 ± 4 | 1130 ± 1 | 1.00 | 1128 ± 1 | 1.00 |

**On a monitor.** The same SuperTuxKart race with every effect on
(`rig/rig-framepace.sh stk-ultra`, the profile race on "lighthouse") on
DP-3 at 3840x2160 and 240 Hz, vsync on, through Hyprland's composition
(`render:direct_scanout` off), two 20 s logs each: native 240.0 fps, p99
4.47 ms, p99.9 5.06 ms; nesbox 240.0 fps, p99 4.47 ms, p99.9 5.07 ms; no
frame missed a vblank either way.

### Where the time went

**1. GPU-bound frames run at native speed.** At 4-5 ms a frame a guest
under nesbox is within 1% of native, and so are its lows; on a monitor it
paces exactly as the host does. A frame crosses to the host about 17 times
(ten IOCTL2s of explicit sync, two closes, two fence watches, a Wayland
message each way): at 200 frames a second, off the GPU's critical path.

**2. Explicit sync costs about 80 µs a frame** (the largest per-frame cost
left). NVIDIA's EGL and Vulkan Wayland paths make eleven synchronous DRM
calls a present: two binary syncobjs created, transferred into and
destroyed, a semaphore-surface fence made and exported, one imported and
waited on, the release point waited for (`pacing: ioctl2:`). Each is a
round trip from the presenting thread, 7.0-7.5 µs under nesbox at a
thousand frames a second, 12 under crosvm. Against the program on four
CPUs that is the whole gap: SuperTuxKart at 720p loses 8% to it, and on its
Vulkan renderer, which presents 1,370 times a second, 14%; the median
frame is 40 and 110 µs longer. The backend's timing report (`pacing:
ioctl2 time`, new here) splits each call: of about 55 µs of backend time a
frame, the host ioctls are 15; preparing a call (the schema walk, copies,
translation, policy) is 0.7-2 µs; classifying the descriptor the two
exporting calls produce is 2.8 µs each (a `statx` and a `readlink` of
`/proc/self/fd`); and every call handed its duplicate of the target file to
the closer thread. That hand-off is now skipped when the handle table
provably still holds the file: IOCTL2 service 5.1 -> 4.6 µs, the guest's
round trip 7.5 -> 7.0 µs, which is inside the frame rate's run-to-run
spread. The calls come one at a time from NVIDIA's userspace, each using
the last one's result, so nothing below it can batch them.

**3. The first write to fresh system memory faults once a page** (the
largest loss measured). Streaming through fresh staging memory runs at a
quarter of native speed; from memory kept mapped, at native speed. A frame
spends 1.2 ms natively and 2.1 ms in a guest allocating and mapping its four
buffers (each map a request to the VMM, above), but 0.2 ms natively and 3.8 ms in a
guest writing them: 1.05 GB/s against 19.7, a second-level page fault on
every 4 KiB page. The cause, read in Linux 7.2.7 and open-gpu-kernel-modules
595.99.02: the driver maps its system memory into the VMM with
`vm_insert_page` (kernel-open/nvidia/nv-mmap.c `nvidia_mmap_sysmem()`),
on order-0 pages it allocated itself, whose `folio->mapping` is NULL. With
`CONFIG_SECRETMEM` (on here) GUP-fast refuses such a folio (mm/gup.c
`gup_fast_folio_allowed()`), and KVM maps a read fault -- which
`KVM_PRE_FAULT_MEMORY` is -- writable only if GUP-fast with `FOLL_WRITE`
succeeds (virt/kvm/kvm_main.c `hva_to_pfn_fast()`, `hva_to_pfn_slow()`).
So the prefault maps these pages read-only and each first write takes the
slow path. Video memory (a PFNMAP, whose writability KVM reads from the
PTE) and compound pages (the driver's allocations of order above 0,
kernel-open/nvidia/nv-vm.c `nv_compute_gfp_mask()`) are not affected.

**4. The default window is too small for a heavy application.** Blender's
EEVEE scene maps more write-combined and write-back memory than one process
may hold in the default 1 GiB window (384 MiB of the WC zone and 112 MiB
of the WB zone at the default 50% share): the backend refuses the mappings
(`SHM WriteCombine zone: guest process ... may not take`), Blender's Vulkan
backend reports staging buffers it cannot allocate, and Blender segfaults
on either backend. With a 16 GiB window at 90% it runs. DEPLOY.md, "Sizing
the window", has the sizes.

**5. crosvm stalls where nesbox does not.** Under crosvm the GPU-bound runs
keep their average and lose their lows: SuperTuxKart's 0.1% low is 31 fps
against nesbox's 77 and native's 65, from a dozen frames of 20-40 ms in
bursts where nesbox and native have one or none, and every Blender GL run
has one frame of about 6 s. Its crossings cost more (an IOCTL2 round trip
of 11.9 µs against 7.4, with a long tail: 21,000 calls over 64 µs in one
run against 800). Core scheduling explains the CPU-bound gap but not the
stalls: with `NVGPU_CROSVM_CORE_SCHED=0` SuperTuxKart on Vulkan went from
986 to 1,150 fps (nesbox 1,203), and the 4K race's 0.1% low only from 31 to
39.

**6. The event pump wakes for events nobody waits on.** A Vulkan game's
driver posts about 125,000 events a second on its device files; none reach
the guest, but the pump wakes for each (`pacing: legacy readiness ...
events nobody waited on`), and the backend used 45% of a core for a
1,200 fps Vulkan game against 32% for GL at the same rate. Taking an
unarmed descriptor out of epoll would save that, but not simply: NVIDIA's
`poll` clears a dataless event as it reports it, and epoll polls a
descriptor twice when it is added or modified with an event pending
(fs/eventpoll.c `ep_insert()` or `ep_modify()`, then
`ep_send_events()`), so an event arriving as the descriptor is re-armed
would be lost, and a guest waiting on it would hang.

### Failures met on the way

- **SuperTuxKart's Vulkan renderer aborts natively too.** "vkQueueSubmit
  failed" at the start of the race, then "Aborting SuperTuxKart": 1 of 20
  guest runs, and 3 of 6 native runs confined to four CPUs (none of 6
  unconfined). It is the program's or the driver's, made likelier by fewer
  CPUs, not the device's. An aborted process can go on rendering; the
  harness kills what is left of a native run before the next.
- **Blender's Vulkan backend hung because the RM allowlist refused
  FIFO_DISABLE_CHANNELS; it is allowed now.** NVIDIA's Vulkan driver calls
  `NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS` on its own client's channels: for
  about half a millisecond of queue set-up at every Vulkan start (16
  channels), and in Blender around each frame it starts, twice, on one
  channel (disable, about a millisecond, enable; 20-22 calls a run, seen
  natively with `rig/heavy/rmlog.c`, an LD_PRELOAD logger of RM controls).
  The allowlist refused it, the driver went on as if it had been done, and
  in 10 of 24 of Blender's runs a frame, nearly always the second, never
  completed: its main thread polls `/dev/nvidia0` every 10 ms for a GPU semaphore that
  does not move, and nothing crosses to the host but readiness arms and
  their reports, four a second (`rig/heavy/hang-watch.sh` dumped it on the
  30th second without progress, rig/TESTING-RIG.md, "Heavy workloads").
  The device is not in it: natively, the same refusal
  (`RMLOG_REFUSE=0x2080110b`) hangs Blender at the same point, and
  answering success without disabling anything hangs it as often, so the
  driver needs the channel really stopped.

  | Blender Vulkan, 16 GiB window, 8 frames a run | runs | hung |
  |---|---|---|
  | guest, nesbox, refused (the list before) | 24 | 10 |
  | guest, nesbox, forwarded (`--rm-allowlist=log`) | 10 | 0 |
  | guest, crosvm, forwarded | 10 | 0 |
  | guest, nesbox, allowed with its gate (`device/src/rmchan.rs`) | 24 | 0 |
  | guest, crosvm, allowed with its gate | 10 | 0 |
  | native, refused by `rmlog.so` | 14 | 5 |
  | native, answered success by `rmlog.so`, not done | 8 | 4 |
  | native, sent with `bOnlyDisableScheduling` set (`RMLOG_ONLY_SCHED`) | 8 | 3 |
  | native, forwarded (`rmlog.so` logging only) | 8 | 0 |

  Round one's figures agree (4 of 8 guest runs under either VMM, 0 of 5
  native). Stopping the channel's scheduling without taking it off the GPU
  is not enough either. SuperTuxKart's start-ups coped with the refusal
  (thirty refused, thirty forwarded, all reached the race). The user
  decided to allow it: SECURITY.md, "The RM allowlist", has how it is held
  (the calling process's own clients, no preemption event, RM's size, a
  rate). With it allowed, every one of the 752 calls the 34 Blender runs
  above made was served (status 0, `rmlog.so` in the guest), and ten
  SuperTuxKart Vulkan start-ups made 28, all served, none aborted.

  The rate (50 calls a second per guest process after a burst of 40, 200
  per VM after 160) against what every Vulkan workload here makes natively
  (`rmlog.so` logging only, two runs each):

  | workload | calls a run | most in 1 ms / 100 ms / 1 s |
  |---|---|---|
  | Blender EEVEE, Vulkan (8 frames) | 22 | 2 / 4 / 6 |
  | SuperTuxKart 4K, Vulkan | 4 | 2 / 2 / 4 |
  | Godot, SDFGI 4K and 20,000 draws | 4 | 2 / 2 / 4 |
  | vkmark, every scene | 0 | -- |
  | SuperTuxKart 4K, GL | 0 | -- |

  The busiest is Blender, 6 in its busiest second and 2 within a
  millisecond: an eighth of a process's rate, a twentieth of its burst.
- **Blender's Vulkan backend at the default 1 GiB window crashes; it does
  not hang.** Staging buffers it cannot map ("Unable to upload data to
  vertex buffer via a staging buffer"), then a crash report, in 4 of 4
  runs, none stalled (4, above).
- **Godot's GL scene hung once as it exited** (1 of 8 guest runs, crosvm),
  with no message crossing to the host until the watchdog. Not reproduced:
  36 more runs under crosvm, each watched by `hang-watch.sh`, all rendered
  their 20 s and exited. Still unexplained; at one in eight, 36 clean runs
  would happen by chance about one time in 120.

### What would close the rest

In the order of what each would buy:

1. **Map fresh system memory writable up front** (3: 3.7 times on
   streaming). Either a host kernel whose `KVM_PRE_FAULT_MEMORY` can map for
   write, or whose GUP-fast accepts a driver's unmapped order-0 folio for a
   non-pinning get; or RM allocating host-visible system memory in 64 KiB
   pages or larger, which the driver then allocates as compound pages that
   GUP-fast accepts. The last could be the backend's, by rewriting the
   allocation's page-size attribute, at the cost of rounding and of
   higher-order allocations that can fail under fragmentation; it belongs
   in the backend's RM allocation path (`device/src/nvidia/rm.rs`).
2. **Cut the backend's cost per IOCTL2** (2: up to about 40 µs a frame, 5%
   at a thousand frames a second). A cheaper exact classification of
   exported descriptors (a `readlinkat` on a directory descriptor kept for
   `/proc/self/fd`, 0.3 µs of the readlink's 1.0 in a microbenchmark, and no
   allocation), and a leaner prepare and finish for the schema's plain
   syncobj calls.
3. **Post the calls whose answer nothing waits for.** SYNCOBJ_DESTROY's
   only failure is a bad handle; sent without waiting, as closes already
   are (`nvgpu_close_handle_async()`), it would save two round trips a frame
   (about 10 µs). A guest driver and protocol change.
4. **crosvm's stalls** (5): not core scheduling; the next step is to see
   what its vCPU threads wait on during one.
5. **Size the window for the workload** (4): a desktop VM that runs
   creative applications wants 4 GiB or more.
6. **Quiet the pump** (6): host CPU rather than frame time, and only with
   the double-poll race solved.

## Steam-like games

RTX 5090, 595.99.02, Ryzen 9 9950X, 2026-09-30. What a Steam game under
Proton does that the heavy workloads above do not -- D3D11 and D3D12
through DXVK and vkd3d-proton, Wine's threads and its synchronisation, a
job system waking every core each frame -- measured the same way natively
and in a guest (`rig/rig-heavy.sh`; rig/TESTING-RIG.md, "Proton-like and
CPU-heavy workloads"). The code is `display-passthrough` at `8fe984f` with
its backend and nesbox (holding KVM's statistics, `virtio-nvgpu-v7`), the
rig's crosvm, a guest kernel of the rig's 7.2.7 config with THP (set to
`madvise`, as the host has it, unless a row says otherwise) and ntsync
built in, 4 vCPUs and 8 GiB unless a row says otherwise, `--allow-compute`,
everything else at its default. Wine 11.16 (staging, WoW64), DXVK 2.7.1,
vkd3d-proton 2.14.1, all unpaced against the rig's headless sway. Mean of
three runs ± half their range unless a row says fewer; **native, 4 CPUs**
is the program confined to the guest's CPU count (`taskset -c 0-3`).

- `gameloop`: `nvgpu-gameloop`, a synthetic frame: four fork-join phases
  of 64 jobs on as many threads as there are CPUs (particles integrated,
  random reads through a 512 MiB heap), then 4,000 draws presented.
- Godot D3D12: Godot 4.7.2's Windows build under Wine on vkd3d-proton,
  the "draws" scene (20,000 meshes) at 1280x720.
- Heaven: Unigine Heaven 4.0's 32-bit D3D11 build under Wine with DXVK,
  1280x720, low, no tessellation, its demo camera.
- Godot Vulkan: the same "draws" scene on Godot's Linux Vulkan renderer.
- 0 A.D. 0.28 on Vulkan, four AIs; its frame time varies a great deal from
  run to run (the AIs play differently), so its rows say little.

Wine synchronises through its server in both columns here: the host has no
`/dev/ntsync` loaded, and the guest's is removed for these runs.

| workload | figure | native | native, 4 CPUs | nesbox | ×native (4 CPUs) | crosvm | ×native (4 CPUs) |
|---|---|---|---|---|---|---|---|
| gameloop | avg fps | 220.9 ± 26 | 60.2 (4 runs) | 58.3 ± 0.8 | 3.79 (1.03) | 52.2 ± 0.5 | 4.23 (1.15) |
| | 1% / 0.1% low | 80.8 / 40.9 | 42.9 / 33.4 | 49.4 / 37.9 | | 36.0 / 28.4 | |
| Godot D3D12 | avg fps | 70.7 ± 0.7 | 70.0 ± 1.4 | 68.7 ± 2.1 (2) | 1.03 (1.02) | 67.0 ± 0.5 (2) | 1.06 (1.04) |
| | 1% / 0.1% low | 59.5 / 46.8 | 28.6 / 21.6 | 58.7 / 50.6 | | 48.2 / 34.2 | |
| Heaven, D3D11 | avg fps | 589.6 ± 6.7 | 480.2 ± 4.5 | 476 (4 runs) | 1.24 (1.01) | 413.4 ± 25 (2) | 1.43 (1.16) |
| | 1% / 0.1% low | 293 / 144 | 136 / 83 | 188 / 101 | | 109 / 62 | |
| Godot Vulkan | avg fps | 111.8 ± 2.4 | 113.8 ± 1.3 | 106.8 ± 6.2 | 1.05 (1.07) | 101.6 ± 0.7 | 1.10 (1.12) |
| 0 A.D. | avg fps | 1060 ± 132 | 1035 ± 348 | 1016 (1) | | 491 (1) | |

A guest of 4 vCPUs is within a few percent of the same program on 4 host
CPUs, and its lows are better than there (the host's 4 CPUs also serve the
desktop and the compositor). What it loses against the host is the other 28
CPUs: nothing a game runs on more than 4 threads can use them.

### Where the time went

**1. The guest's CPU count.** The same two programs with more vCPUs, each
against native confined to as many CPUs (mean of two runs; guest RAM
16 GiB at 16 vCPUs, 24 at 24):

| CPUs | gameloop, native | gameloop, nesbox | Heaven, native | Heaven, nesbox |
|---|---|---|---|---|
| 4 | 60.2 | 58.3 | 480 | 476 |
| 8 | 106.2 | 98.6 (-7%) | 581 | 494 (-15%) |
| 16 | 140 ± 36 | 158.1 | 574 | 472 (-18%) |
| 24 | 215.2 | 194.7 (-10%) | 590 | (below) |

The job system scales in a guest as it does natively, 7-10% under it. Heaven
does not: from 8 CPUs up it is bound by something per frame, not by CPUs,
natively at about 580 and in a guest at about 480 (2 below).

**2. Waking a thread on another vCPU.** `nvgpu-wakecost`, two threads on
two CPUs handing a token through a futex, 20,000 round trips (µs):

| | futex hand-off, p50 / p99 | spinning hand-off, p50 | one CPUID |
|---|---|---|---|
| native | 1.8-2.0 / 3.3-7.8 | 0.06 | 0.025 |
| nesbox | 7.6 / 15-17 | 0.06 | 1.31 |
| nesbox, vCPUs pinned (8-11) | 7.9-8.0 / 14 | 0.07-0.09 | 1.33 |
| nesbox, guest haltpoll | 0.95 / 1.1-2.1 | 0.07 | 1.31 |
| crosvm | 10.5-12.7 / 23-26 | 0.08-0.09 | 1.60 |

A wakeup that finds the other vCPU halted costs it an exit, the host's wake
and an entry: about four times native. A VM exit and entry here costs 1.3 µs
(CPUID, which always exits): the host kernel issues an IBPB on every VM exit
(its SRSO mitigation, "IBPB on VMEXIT only"; `vmscape` adds one on the way
to user space). Wine without ntsync is made of such wakeups: its events and
mutexes are round trips to the wineserver process. KVM's own counters
(`rig/heavy/kvmstat.py`, 12 s windows, two runs each, per second over the 4
vCPUs):

| | exits | of them halts | halts the host's poll caught | host CPU in halt polling | avg fps |
|---|---|---|---|---|---|
| gameloop | 15,732 | 1,454 | 816 | 0.09 s/s | 58.0 |
| Heaven, Wine server sync | 54,786 | 35,793 | 30,122 | 1.09 s/s | 485.3 |
| Heaven, ntsync | 36,055 | 17,716 | 13,938 | 0.99 s/s | 538.7 |
| Heaven, guest haltpoll | 29,151 | 3,697 | -- | (polling in the guest) | 441.5 |
| Godot D3D12 | 9,992 | 3,605 | 610 | 0.04 s/s | 67.4 |

Heaven halts a vCPU 36,000 times a second, and KVM's halt polling (200 µs)
spends more than a host CPU catching them. ntsync halves the halts and gains
11% (it is Wine's and Proton's own path when `/dev/ntsync` is there; the
guest kernel now has it). Polling in the guest instead (`cpuidle_haltpoll.
force=1`) removes nine halts in ten and makes a futex hand-off faster than
natively, but Heaven loses 9% to it: a polling vCPU is one Wine's other
runnable threads cannot have. It is not the default.

Wine on X11 through a rootful Xwayland in the guest, as Proton runs a game
on an X11 desktop, costs what its Wayland driver does: Heaven 476.0 fps
against 462.5 on 4 host CPUs the same way, Godot D3D12 69.6 (one run)
against 71.8 (two runs each otherwise).

**3. Guest pages.** The guest kernel had no transparent huge pages at all
(x86_64's defconfig leaves them out): every page of a game's heap was 4 KiB
under the host's 2 MiB, and each TLB miss walked two page tables of small
pages. With THP (`always`), `gameloop` went from 57.6 to 63.7 fps
(two runs each), past native on 4 CPUs, whose heap the host maps in 4 KiB
pages too (`madvise`, and malloc never asks). The guest kernel config now
has THP, `always` by default; see "What changed".

**4. What the frame-pacing measures cost.** The hypothesis was that the
reply spin, the queue poll, the short slice and the fence watch, which cut
stutter, cost a CPU-bound game throughput. One knob at a time against the
defaults, nesbox, 4 vCPUs, avg fps (mean of two runs unless marked; `(1)`
one, the other run of Godot D3D12 stopped, "What remains"):

| knob | gameloop (1% low) | Heaven | Godot D3D12 |
|---|---|---|---|
| defaults | 57.6 (40.5) | 487.7 | 67.3 |
| `rt_spin_us=0` | 58.0 (49.3) | 474.2 ± 21.5 | 70.1 (1) |
| spin only while the vCPU has nothing else to run (a patch, not kept) | 57.7 (49.2) | 481.2 (1) | -- |
| `--queue-poll-us 0` | 56.6 (46.1) | 474.4 | 69.7 |
| the host's slice (`NVGPU_SLICE_US=0`) | 54.5 (34.7) | 488.2 | 67.3 |
| `async_fence_watch=0` | 56.0 (40.0) | 484.8 | 69.1 |
| `nopvspin` | 57.5 (50.0) | -- | -- |
| vCPUs pinned to 8-11 | 57.0 (45.4) | 484.2 (1) | 69.1 (1) |
| guest haltpoll | 58.1 (46.3) | 441.5 | 70.7 |
| guest THP `always` | 63.7 (53.5) | 482.9 | 66.1 |
| ntsync kept | -- | 552.2 | 69.1 |

None of the pacing measures costs throughput beyond the runs' spread; the
slice and the queue poll gain a little in the job system and Heaven, and
the slice's absence costs the job system's lows. Huge pages help the job
system's heap and not the Wine games, which spend their time elsewhere;
ntsync is Heaven's largest single gain (+13%). The spin was the likeliest thief of a busy vCPU's time, and turning it
off changes nothing measurable: a caller spins only while its own reply is
on its way, a few microseconds a call, at a few hundred calls a frame. The
patch that stops a spin as soon as another task is queued on the vCPU
(including the lazy reschedule flag `need_resched()` does not read) is
correct but bought nothing here, and is not kept.

**5. crosvm** loses 10-16% more on these, as on the others (above, "Heavy
workloads"): its per-vCPU core scheduling, and a futex hand-off of 11 µs
against nesbox's 7.6.

**6. Without compute, a game that enables what it is offered does not
start.** Without `--allow-compute` there is no `/dev/nvidia-uvm` in the
guest, and NVIDIA's driver there -- natively too, without the UVM device --
lists every extension it lists with it (276), reports their features, and
fails `vkCreateDevice` (`VK_ERROR_INITIALIZATION_FAILED`) with any of
`VK_KHR_acceleration_structure`, `VK_KHR_ray_query`,
`VK_KHR_ray_tracing_pipeline`, `VK_NVX_binary_import` (DLSS),
`VK_NV_cuda_kernel_launch` or `VK_NV_optical_flow` (Frame Generation);
`VK_NVX_image_view_handle` and `VK_NV_low_latency2` work. Godot's Vulkan
renderer then dies with SIGILL; Godot's D3D12 through vkd3d-proton ran at
36.7-40.0 fps where it runs at 68.7 with compute (5 runs), or hung (2).
DXVK's D3D11 (Heaven) was unaffected. `VK_LAYER_NVGPU_no_uvm`
([`nvgpu-vk-layer/`](nvgpu-vk-layer/); DEPLOY.md, "The guest") hides those
extensions and their features without compute:

| without compute | without the layer | with it |
|---|---|---|
| extensions listed | 276 | 260 |
| `vkCreateDevice` with ray queries | -3 (initialization failed) | -7 (extension not present) |
| Godot, Vulkan | SIGILL | 108.5 fps (104.9 with compute) |
| Godot, D3D12 (vkd3d-proton) | 38.5 ± 0.2 fps, or hung | 69.8 ± 1.0 fps (2 runs) |

NVIDIA's 32-bit Vulkan driver lists none of these extensions; the layer's
32-bit build loads in 32-bit processes (`vulkaninfo-32`) and changes nothing
there.

With compute, ray tracing works in a guest: `nvgpu-rtprobe` (ray queries
into 64 instances of 200,000 triangles, a ray a pixel of 1920x1080,
submitted and waited for a frame) casts 7,331-7,579 Mrays/s against 8,807
natively, the difference being the frame's submit-and-wait round trip
(0.28 ms against 0.23), not the rays.

### Choices for the deployment, measured

None of these is changed here: each trades something the project does not
decide for the user.

| choice | gain measured | what it costs |
|---|---|---|
| more vCPUs (8-16 for a modern engine) | gameloop 58 -> 99 (8) -> 158 (16) fps | host CPUs the VM can occupy; nothing isolation-wise |
| `--allow-compute` for games | RT, DLSS and Frame Generation usable; without it, the layer | the UVM surface (SECURITY.md, "Compute is opt-in") |
| guest haltpoll (`cpuidle_haltpoll.force=1` on the guest's command line) | futex hand-off 7.6 -> 0.95 µs; exits -47% in Heaven | Heaven -9% (a polling vCPU is not running a game thread); host CPU while the guest idles |
| the host's SRSO mitigation (`spec_rstack_overflow=`; "IBPB on VMEXIT only" here) | not measured: an IBPB is in every one of the 15,000-55,000 exits a second above | the guest-to-host return-stack mitigation on this CPU |
| a lower C-state limit on the host (`/dev/cpu_dma_latency`, or C3 disabled: 350 µs exit latency here) | not measured (the sandbox has neither) | power, heat |
| crosvm `--core-scheduling=false` | 10-16% here, 16-50% above | the SMT side-channel mitigation between a vCPU and host tasks |
| pinning vCPUs | none measured (7.9 µs hand-off, 57.0 fps) | worse under a host load unless the CPUs are set aside (DEPLOY.md, "Frame pacing") |

### What changed

- **The guest kernel** (`scripts/build-guest-kernel.sh`,
  `driver/guest-kernel.defconfig`): transparent huge pages, `always`;
  ntsync; paravirtual spinlocks. All guest-internal.
- **`VK_LAYER_NVGPU_no_uvm`**, above, in the guest image and in DEPLOY.md,
  "The guest".
- **The harness**: the workloads above, KVM's counters without perf
  (nesbox `virtio-nvgpu-v7` holds each vCPU's statistics descriptor with
  `NESBOX_HOLD_KVM_STATS=1`), `nvgpu-wakecost`, `nvgpu-rtprobe`.

### What remains

- **Wine without ntsync or fsync**, the server's round trips: a Proton
  that finds `/dev/ntsync` (guest kernels built here now have it) or uses
  fsync avoids them.
- **Heaven's per-frame cost from 8 vCPUs up** (-15%): explicit sync's
  round trips a present (above, "Heavy workloads", 2), with Wine's.
- **Godot's D3D12 under Wine stops, now and then, in a freshly booted
  guest**: 7 of about 35 such runs (with and without compute, with and
  without the layer), none of 12 native, and none of 10 back to back in one
  guest under `hang-watch.sh` (which dumps what a stopped program waits
  on). Twice the killed process did not exit, which points below Wine. Not
  diagnosed.

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
The heavy workloads are `rig/rig-heavy.sh`'s (rig/TESTING-RIG.md, "Heavy
workloads"); a game process left over from an aborted run loads every run
after it the same way.
