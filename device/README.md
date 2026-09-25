# `device/` — VMM-agnostic device crate

**License: Apache-2.0** (`LICENSE-APACHE-2.0`).

The virtio device implementation, as a Rust library with **no VMM in its
dependency list**. Every VMM-specific concern is a trait that the embedding
VMM implements — descriptor chains as `Read`/`Write`, event queues, guest
memory mapping, host memory mapping.

The intent, borrowed wholesale from `virtio-media`: implementing this device
and adding support for it in a given VMM are two orthogonal tasks. Anyone on
QEMU, cloud-hypervisor, or a VMM of their own should be able to implement the
traits and get the whole device without patching this crate.

Buffer and window bookkeeping lives here, not in the VMM. The VMM supplies raw
map and unmap and nothing more: SHMEM_MAP and SHMEM_UNMAP on the window
(region 1), and on the UVM aperture (region 2), where a UVM semaphore pool is
mapped at its own host address with a memory slot of its own
(`src/uvmmap.rs`).

Optional capabilities **degrade rather than fail to build** — a no-op
implementation for `()` returns `ENOTTY` — so a VMM can adopt the device before
it supports every feature.

ABI-aware translation of ioctl parameters (pointer and file-descriptor
rewriting) happens here, driven by the tables in `gen/`.

The same crate holds the host half of the Wayland proxy (`src/wl/`), and the
vhost-user backend binary that serves it all to a VMM.

**Nothing on protocol v2 has run against a GPU.** The crate's tests run the
dispatcher, the IOCTL2 interpreter, the policies and the Wayland connection
against fake kernels and a fake compositor; `nvgpu-wl-guest`'s loopback test
(ignored by default, run by `scripts/wl-loopback-test.sh`) drives the
dispatcher against a real headless sway or weston. The display paths have not
met real hardware ([`TESTING.md`](../TESTING.md)).

## Files

| file | contents |
|---|---|
| `src/nvidia.rs` | `NvidiaBackend`: the dispatcher. Opens, closes, v1 ioctls routed by handle kind, RM escapes with nested and deep blocks, mmap placement, GET_PROC/SYS_FILES, and the hooks every other module is called from |
| `src/session.rs` | protocol v2: the session and its reset, HELLO, TIME_SYNC, WATCH, HOST_OP, and IOCTL2 split into prepare, execute and finish so the host ioctl runs without the backend's lock |
| `src/xfer.rs` | the IOCTL2 interpreter: walks the backend's own schema over what the guest sent, refuses any disagreement, builds what the host kernel is handed, re-homes GEM handles, and keeps each VM's framebuffer records |
| `src/schema.rs` | ties the generated schema tables (`abi::schema`) to handle kinds |
| `src/policy.rs` | `BackendHooks`: the judgements IOCTL2 leaves to its caller, routed to the KMS, fence and NVKMS sections |
| `src/kms.rs` | KMS properties classified by name, the host hotplug/lease uevent listener, lease re-checks, scanout checksums |
| `src/nvkms.rs` | NVKMS and nvidia-drm grant policy: grant records, head gates for FLIP and SET_MODE, refusals and rewrites, run-time revocation checks |
| `src/fence.rs` | syncobj waits turned into polls, and the shared, capped SYNCOBJ_EVENTFD registrations the guest sleeps on |
| `src/semsurf.rs` | semaphore-surface fence contexts (nvidia-drm 0x54): index bound by the host's layout, the VM's RM clients, per-file and per-session caps; OS events named inside RM parameters |
| `src/rmmem.rs` | records of RM system memory and doorbells, the coherency rewrite, and the Intel guest-PAT warning |
| `src/guestptr.rs` | every pointer the host would follow in RM, NVKMS, nvidia-drm and UVM parameters is relocated or zeroed, or the call refused; memory named by CPU address refused; the UVM command allowlist |
| `src/uvmfd.rs` | the descriptors inside UVM parameters, translated like RM's |
| `src/uvmmap.rs` | the UVM semaphore pools a guest may map, and where each sits in the UVM aperture: recorded from UVM's own replies, matched exactly, bounded per file and per VM, withdrawn on the last MUNMAP, the file's close and a session reset |
| `src/rmctl.rs` | RM controls answered without asking RM (the ones that list every GPU process on the host) |
| `src/hostfd.rs` | handle kinds, classification of a descriptor by what the kernel says it is, HOST_OP helpers, commands refused on every handle |
| `src/handle_table.rs` | backend handles: u32, cyclic, bounded |
| `src/privfd.rs` | the registry of descriptors the backend holds for itself, which an IOCTL2 must never adopt |
| `src/exec.rs` | per-file serial executors for host calls that may wait |
| `src/closer.rs` | `nvgpu-closer`: the last close of a display file, off every thread that must not wait |
| `src/pump.rs` | the event pump: v1 EVENT_READY and v2 EVENT_DATA records, DRM event budgets, the level sweep |
| `src/posture.rs` | refusing root and `CAP_SYS_ADMIN`, dropping capabilities, the socket's directory and path |
| `src/ratelimit.rs`, `src/tally.rs` | a rate limit per log call site, and bounded RM class and control tallies |
| `src/shm.rs`, `src/mmap.rs`, `src/replay.rs` | the shared window's zones and allocator, live mappings, and a replay of real mapping lifetimes against the allocator; `WindowPlacer`, what a transport implements to place into the window and the UVM aperture |
| `src/guarded.rs` | host-written buffers with a guard page behind them |
| `src/virtio.rs` | device config and feature layout, asserted against `driver/nvgpu_wire.h` |
| `src/host.rs`, `src/userspace.rs` | what the host's driver is (from `/proc/driver/nvidia`), and which host userspace files a guest must mount |
| `src/i2_e2e.rs` | test only: `driver/nvgpu_i2.c` transliterated to Rust, run against the whole backend |
| `src/wl/` | the Wayland proxy's host half: one compositor connection per channel (`conn.rs`), the dispatcher's side (`serve.rs`), which lease devices are this GPU's (`probe.rs`), export mode (`export.rs`) |
| `bin/vhost-user-nvgpu.rs` | the vhost-user backend: transport, epochs, executors, pump, hotplug listener, and every command-line flag (`--help`) |
| `bin/nvgpu-userspace.rs` | stages the host's NVIDIA user-mode driver for a guest to mount |
| `bin/test-harness.rs`, `bin/test_client.rs` | an early socket harness and its client |
