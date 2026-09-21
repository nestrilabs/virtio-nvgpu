# `driver/` — guest kernel module

**Licence: GPL-2.0** (`LICENSE-GPL-2.0`), required for kernel symbol access.

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
