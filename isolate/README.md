# `isolate/` — per-guest-process host helper

**License: Apache-2.0** (`LICENSE-APACHE-2.0`).

A sandboxed helper process, one per guest process, launched from a memfd. It
holds the real host `/dev/nvidia*` file descriptors and performs the forwarded
operations; it runs unprivileged, with empty capability sets and `NoNewPrivs`.

This is the part of the design that is **not** a trait the VMM implements. It is
a runtime artifact this repository ships, which means anyone integrating
`virtio-nvgpu` inherits a **process model**, not just a library.

That is a requirement, not a detail — it is documented here rather than left to
be discovered during integration. A VMM that cannot spawn helper processes
cannot use this device as designed.

**Not built.** This directory holds no code. Today the backend holds the host
descriptors itself, one backend process per VM. Part of the posture described
above already applies to that process: it refuses to start as root or with
`CAP_SYS_ADMIN` (`--allow-root-unsafe` overrides), drops every capability and
sets `no_new_privs` before its first thread (`device/src/posture.rs`), and
`scripts/run-guest.sh` starts it through `setpriv`, with no capabilities and no
supplementary groups but those of `video`, `render` and `kvm` the host has: as
the system user `nvgpu` by default, or in the Wayland modes as the owner of the
compositor's socket or of the export directory. What the isolate would add is
the split: the descriptors held by a helper per guest process rather than by
the process that also maps all of the guest's memory, so that a compromised
backend does not hold them.

The display work adds descriptors an isolate would have to hold too: DRM card
and lease files, sync files and syncobjs, dma-bufs, and connections to the
host's Wayland compositor.
