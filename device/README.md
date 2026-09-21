# `device/` — VMM-agnostic device crate

**Licence: Apache-2.0** (`LICENSE-APACHE-2.0`).

The virtio device implementation, as a Rust library with **no VMM in its
dependency list**. Every VMM-specific concern is a trait that the embedding
VMM implements — descriptor chains as `Read`/`Write`, event queues, guest
memory mapping, host memory mapping.

The intent, borrowed wholesale from `virtio-media`: implementing this device
and adding support for it in a given VMM are two orthogonal tasks. Anyone on
QEMU, cloud-hypervisor, or a VMM of their own should be able to implement the
traits and get the whole device without patching this crate.

Buffer and window bookkeeping lives here, not in the VMM. The VMM supplies raw
map and unmap and nothing more.

Optional capabilities **degrade rather than fail to build** — a no-op
implementation for `()` returns `ENOTTY` — so a VMM can adopt the device before
it supports every feature.

ABI-aware translation of ioctl parameters (pointer and file-descriptor
rewriting) happens here, driven by the tables in `gen/`.
