# virtio-nvgpu

**Share one NVIDIA GPU across several VMs, at bare-metal speed.**

Run NVIDIA's own unmodified drivers (Vulkan, NVENC, and CUDA) inside multiple microVMs, while
the host keeps the card. No vGPU licenses or VFIO passthrough needed.

## Why

1. Passthrough locks the whole GPU to one VM.
2. vGPU costs real money and only works on datacenter cards.
3. API forwarding (Venus, virgl) is hella slow, and doesn't support NVENC or CUDA.

virtio-nvgpu forwards the NVIDIA **driver** instead of the graphics API. The
guest builds its own GPU commands and writes them straight to GPU memory, so
a frame being drawn doesn't cross the VM boundary. Nothing gets serialized and replayed on the host the way
Venus and virgl do it.

## How fast

On an RTX 3060:

- **Within 2% of bare metal** for any real game frame (>2 ms)
- **Same CPU cost** as running on the host
- **12 VMs on one card**, splitting it evenly, each encoding 720p60 H.264 with no dropped frames

> [!NOTE]
> 12 is what we tested, not a hard cap. Like containers, you're limited by:
>
> 1. **VRAM.** A game needs gigabytes, so this usually runs out first. Cap each guest with `--vram-limit-mib`.
> 2. **Host CPU and RAM** for each guest.
> 3. **NVENC's 12-session cap** on GeForce cards. Our [Nestri](https://github.com/nestrilabs/nestri) capture layer encodes with Vulkan Video, which
>    doesn't count against it (16 at once worked fine).

[Numbers and method →](BENCHMARKS.md)

## Who it's for

People who stream games or GPU apps from headless VMs (cloud gaming, remote
desktops, CI with a real GPU, selfhosted "serverless" AI) on consumer NVIDIA cards.

## Before you use it

- **It isn't hardware isolation.** The host NVIDIA driver is the trust boundary.
  If your tenants don't trust each other, use passthrough or vGPU. (It's roughly the same trust boundary Docker containers have.)
  [Details →](SECURITY.md)
- Linux guests and hosts only. Supports driver 535.129.03 and newer.
- This is an early version, tested on only two cards. CUDA works but hasn't been benchmarked.

## Learn more

- [How it works](ARCHITECTURE.md)
- [Benchmarks](BENCHMARKS.md)
- [Security](SECURITY.md)
- [`driver/`](driver/) is the guest kernel module (GPL-2.0), [`device/`](device/)
  is the VMM-agnostic Rust device (Apache-2.0), and [`protocol/`](protocol/) is
  shared by both (BSD-3/GPL-2.0).

Inspired by gVisor's `nvproxy`, and [`kayfabe`](https://github.com/reindertpelsma/kayfabe) by Reindert Pelsma.
