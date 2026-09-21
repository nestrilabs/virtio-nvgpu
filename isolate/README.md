# `isolate/` — per-guest-process host helper

**Licence: Apache-2.0** (`LICENSE-APACHE-2.0`).

A sandboxed helper process, one per guest process, launched from a memfd. It
holds the real host `/dev/nvidia*` file descriptors and performs the forwarded
operations; it runs unprivileged, with empty capability sets and `NoNewPrivs`.

This is the part of the design that is **not** a trait the VMM implements. It is
a runtime artifact this repository ships, which means anyone integrating
`virtio-nvgpu` inherits a **process model**, not just a library.

That is a requirement, not a detail — it is documented here rather than left to
be discovered during integration. A VMM that cannot spawn helper processes
cannot use this device as designed.
