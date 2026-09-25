# `driver/` — guest kernel module

**License: GPL-2.0** (`LICENSE-GPL-2.0`), required for kernel symbol access.

A Linux kernel module for the guest. It registers the NVIDIA character
devices — `/dev/nvidiactl`, `/dev/nvidia0`…`/dev/nvidiaN`, `/dev/nvidia-uvm` —
and forwards `ioctl()` and `mmap()` against them over the virtqueue.

The module is deliberately **not ABI-aware**. It moves bytes and manages
mappings; every decision about what an ioctl *means* belongs in `device/`.
Keeping it dumb is what keeps it stable across NVIDIA driver releases.

Shared wire-format and ABI definitions live in `protocol/` and are dual
licensed so this module can include the same headers the Rust side uses.

Built out of tree against the guest kernel, or copied/submoduled into a kernel
tree by whoever is assembling a guest image.

One module, `virtio_gpu_nv.ko`, built from several objects (see `Makefile`):

| file | contents |
|---|---|
| `nvgpu.h` | internal header: shared structs, cross-file prototypes, module parameter `extern`s |
| `nvgpu_wire.h` | wire protocol and config-space layout (BSD-3-Clause OR GPL-2.0+, mirrors `protocol/`) |
| `nvgpu_main.c` | probe/remove, virtqueues, `/dev/nvidia*` cdevs, RM forwarding, `/proc`, sysfs, fake PCI, nvidia-modeset |
| `nvgpu_drm.c` | DRM device registration, GEM proxies, PRIME, nvidia-drm driver-range ioctls |
| `nvgpu_xfer.c`, `nvgpu_hostfile.c`, `nvgpu_kms.c`, `nvgpu_fence.c`, `nvgpu_nvkms.c`, `nvgpu_wl.c` | placeholders for display passthrough work; each says what it will hold |
| `nvgpu_rm_intercepts.h`, `gen/` | RM command tables (generated / hand-kept) |
