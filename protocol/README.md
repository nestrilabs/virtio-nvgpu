# `protocol/` — shared wire format and ABI definitions

**License: BSD-3-Clause OR GPL-2.0+** (`LICENSE-BSD-3-Clause`,
`LICENSE-GPL-2.0`) — dual, deliberately.

Apache-2.0 is not GPL-2.0-compatible, so a GPL kernel module cannot include an
Apache-2.0 header. Dual licensing the definitions **both halves must agree on**
is what lets one repository hold a GPL guest driver and an Apache-2.0 host crate
honestly.

This directory holds only definitions that cross the boundary: virtqueue message
layouts, request and response headers, and the ABI descriptions both sides read.
Nothing here should contain logic.

`src/messages.rs` is normative, and `driver/nvgpu_wire.h` mirrors it; both
assert their sizes. It defines both protocols:

- **v1**: OPEN, IOCTL, MMAP, MUNMAP, CLOSE, the proc/sys file listings and the
  16-byte EVENT_READY — everything a guest needs to render and encode. IOCTL
  carries what an RM pointer addresses as a nested and a deep block; with the
  v2 capability `BCAP_DEEP_SEGS`, the deep block may instead be a segment
  table (`DEEP_SEGMENTED`), one segment per pointer, which the backend sizes
  itself.
- **v2**, negotiated by HELLO: IOCTL2 (the schema-driven vectored ioctl),
  TIME_SYNC, EVENT_DATA records (readiness, fences, DRM events, hotplug),
  WATCH and UNWATCH, HOST_OP, and WL_SEND/WL_RECV for the Wayland channel; the
  handle kinds, the backend's capability bits, and each mapping's memory type
  and writability in the MMAP reply. A v1 guest or backend reads the fields v2
  added as zero, which keeps the old behaviour.

Two things both halves share live elsewhere, because they are generated or
belong to another component: the IOCTL2 schema tables (`gen/`, generated into
`gen/src/schema/` and `driver/gen/nvgpu_schema.h`), and the Wayland channel's
frame format (`wlwire/src/frame.rs`, which the guest kernel treats as opaque
apart from its descriptor table; the ioctls of `/dev/nvgpu-wl` are in
`driver/uapi/nvgpu_wl.h`, under this directory's dual license).

Files in this directory carry `SPDX-License-Identifier: BSD-3-Clause OR
GPL-2.0+`. The dual license binds **definitions authored here**; code ported
from other projects keeps its original terms and cannot be relicensed.
