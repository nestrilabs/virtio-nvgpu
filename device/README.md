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
| `src/semsurf.rs` | semaphore-surface fence contexts (nvidia-drm 0x54): index bound by the host's layout, the VM's RM clients (with the guest process that made each, and the grants RM took for their objects, for `rmshare.rs`), per-file and per-session caps; OS events named inside RM parameters |
| `src/rmmem.rs` | records of RM system memory and doorbells, the coherency rewrite, and the Intel guest-PAT warning |
| `src/guestptr.rs` | every pointer the host would follow in RM, NVKMS, nvidia-drm and UVM parameters is relocated or zeroed, or the call refused; memory named by CPU address refused; the UVM command allowlist |
| `src/osdesc.rs` | memory the guest registers by its guest-physical pages instead: the call and its page list checked, every page looked up in guest RAM (the vhost-user memory table, `GuestRam`), RM handed the backend's own mapping of exactly those pages (its mapping of guest RAM, or a range reserved and mapped run by run from the memfds), registrations kept until RM lets go and their releases read by the guest with HOST_OP OSDESC_REAP; bounded per file and per VM |
| `src/uvmfd.rs` | the descriptors inside UVM parameters, translated like RM's |
| `src/uvmmap.rs` | the UVM semaphore pools a guest may map, and where each sits in the UVM aperture: recorded from UVM's own replies, matched exactly, at host addresses in [4 GiB, 32 TiB), bounded per file and per VM, withdrawn on the last MUNMAP, the file's close and a session reset |
| `src/rmctl.rs` | RM controls answered without asking RM (the ones that list every GPU process on the host) |
| `src/rmshare.rs` | RM objects between clients: NV_ESC_RM_SHARE and the NV0000 share controls go to RM only when they narrow or grant inside the VM; RM_DUP_OBJECT's two clients must be the VM's and, for a guest that names the calling process (BCAP_PROC_ID), one guest process's unless a CLIENT grant in the list RM checks covers it (the object's own, or its client's while no other object of it has one; a free drops the client's object grants); a second client named in class or control parameters, lists up to their counts included, must be the VM's. Refusals are RM's NV_ERR_INSUFFICIENT_PERMISSIONS |
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
| `src/vring.rs` | a control-queue chain as the vhost-user transport takes it: summed before it is read, gathered, the reply scattered back (feature `vhost-user`) |
| `src/host.rs`, `src/userspace.rs` | what the host's driver is (from `/proc/driver/nvidia`), and which host userspace files a guest must mount |
| `src/i2_e2e.rs` | test only: `driver/nvgpu_i2.c` transliterated to Rust, run against the whole backend |
| `src/fuzzing/` | fuzzing only (`--cfg fuzzing`, never in the backend): the fuzz targets' entry points and the fake host they run against; see "Fuzzing" below |
| `src/fuzz_seeds.rs` | test only: with `NVGPU_FUZZ_SEEDS` set, every session a unit test serves is written out as a seed for the `backend` targets |
| `src/wl/` | the Wayland proxy's host half: one compositor connection per channel (`conn.rs`), the dispatcher's side (`serve.rs`), which lease devices are this GPU's (`probe.rs`), export mode (`export.rs`) |
| `bin/vhost-user-nvgpu.rs` | the vhost-user backend: transport, epochs, executors, pump, hotplug listener, guest RAM handed to the backend from each memory table, and every command-line flag (`--help`) |
| `bin/nvgpu-userspace.rs` | stages the host's NVIDIA user-mode driver for a guest to mount |
| `bin/test-harness.rs`, `bin/test_client.rs` | an early socket harness and its client |

## Fuzzing

`scripts/fuzz.sh` (its header has every command) builds and runs a
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) target for each place
the host takes bytes from the guest or a Wayland peer, on a pinned nightly
with AddressSanitizer, each in a bubblewrap sandbox with a `/dev` of its own.
The targets are in `fuzz/` (a workspace of its own); what they drive is
`src/fuzzing/`, compiled only under `--cfg fuzzing`.

```
scripts/fuzz.sh seeds            # build, record seeds from the unit tests
scripts/fuzz.sh run 1800         # every target, 30 minutes, side by side
scripts/fuzz.sh triage backend   # what each finding is
scripts/fuzz.sh repro backend fuzz/artifacts/backend/crash-...
scripts/fuzz.sh stats            # how far the backend corpora reach
scripts/fuzz.sh miri
```

| target | what the input is | what is checked beyond "no panic" |
|---|---|---|
| `backend` | a configuration byte (v2 or not, compute, guest RAM, process ids, compositor mode, fences, driver release), then whole messages as the guest's driver queues them, each with its response capacity; files of every kind already open | see below |
| `backend_v2` | the same, always a v2 session with compute, guest RAM and 610.57.04 | as `backend` |
| `vring` | guest memory holding a control queue: descriptor table, available ring, buffers; walked by `virtio-queue`, taken apart by `vring.rs`, served, the reply scattered back | as `backend`, and a reply never past the chain's writable bytes |
| `deepseg` | a deep-segment block for one of RM's measured controls | each relocated pointer is a buffer of ours holding exactly the guest's bytes, RM's own size; a refusal changes nothing |
| `osdesc` | an OS-descriptor call and its page list | the runs are the list; the mapping RM gets is those guest pages, in order |
| `rmshare` | share and duplicate parameters, named-client tables, locally answered controls; or a sequence of ownership operations | a forgotten client is in no list; no duplicate between two processes nothing shares; the grant count is the lists' |
| `nvkms` | v1 NVKMS messages against every release's policy | |
| `guestptr` | RM escapes, controls and UVM blocks; IDLE_CHANNELS lists | no pointer RM follows is left holding a guest value; the caller reads its own values back |
| `misc` | fence rewrites, uevents, KMS property names | |
| `wl_engine` | a sequence of: app and compositor messages (raw, or built from the protocol tables against live objects, with descriptors of every class), channel frames into either end (raw, or built from any record type: Wayland, stream data, EOF, credit, SHM_SYNC, blob, error, hangup), moving what is queued across, stream readiness | no descriptor left open once both ends are dropped |
| `wl_codec` | a frame, and a message against any signature | |

The `backend` targets run the whole dispatcher (`serve`, IOCTL2's
`execute` and `finish`) against a fake host (`src/fuzzing/host.rs`): RM,
UVM, NVKMS and nvidia-drm as far as their parameter blocks go. Every pointer
the real driver would follow is followed, for as many bytes as it would
copy, probed the way the kernel's copy would fault (EFAULT, the guard pages
of `guarded.rs` working) and checked with AddressSanitizer's shadow (a heap
buffer shorter than the copy is a finding); a pointer holding any 8 bytes of
the input, or a small number, is a finding. OS-descriptor registrations are
checked page by page against an independent reading of the page list, over
guest RAM whose every word holds its own address. IOCTL2's host calls walk
the schema tables as the kernel walks the struct. The window and the UVM
aperture are a fake VMM that refuses a placement outside them, over another,
of one of the backend's private descriptors, or a UVM pool outside UVM's
band; after teardown nothing may be left placed and no descriptor open.
Replies may not exceed their capacity or carry an address the host was
handed.

Nothing reaches a device: under `cfg(fuzzing)` every path the backend opens
is `/dev/null` (`nvidia.rs`, `session.rs`, `semsurf.rs`, `hostfd.rs`), the
harness refuses to start where `/dev/nvidiactl` exists, and the script's
sandbox has none.

The corpora start from the unit tests: `scripts/fuzz.sh seeds` runs them
with `NVGPU_FUZZ_SEEDS` set, and every session is a seed (again with its
first handle as each kind of file the harness holds, and as a control queue
for `vring`). That is most of the depth: from nothing the dispatcher target
reached 2,300 edges in a minute; from the tests, 15,000.

### What it found

Four campaigns, every target side by side (about 20 worker processes,
AddressSanitizer on): 10, 25, 30 and 25 minutes, the harness corrected
between them. The dispatcher pair ran about 30 million inputs, the
control queue 9 million, the Wayland engine 130 million, the parsers
between 70 million and 2 billion each.

- **A guest pointer reached RM** (critical; `20434fa`). The size field RM
  copies a control's parameters by was left as the guest wrote it, and the
  buffer made as long as what was sent. Sent one byte short of a pointer
  field, the parameters had RM read the guest's seven bytes over a zero of
  the buffer's slack -- an address of the guest's choosing in the backend,
  which the pointer scrub, reading only what was sent, never saw; RM copies
  in from it and out to it. The size the host copies must now be exactly
  what was sent (or zero, where the host takes the class's own), on every
  nested path. Found as a two-byte pointer, 0x4000, by `backend_v2`.
- `serve` answered a request too short for a header, or of no known type,
  with a 16-byte header whatever capacity the guest posted. The vhost-user
  transport never posts less, but `serve` promises no reply past `cap`;
  fixed for every path (`0b320bb`).

Everything else reported was the harness's own mistake, corrected: no other
crash, overflow, leak, stray placement or guest pointer.

## Miri

`scripts/fuzz.sh miri` runs every unit test of `device` and `wlwire` under
Miri, one per process, since Miri stops at the first system call it cannot
model. It models files, pipes, eventfds and anonymous memory; not memfds,
file-backed mappings, Unix sockets or device ioctls, so most of the
backend's tests end there, and are counted, not failed. Under `cfg(miri)`
the shared window's backing, `GuardedBuf` (Miri's own bounds checks stand
in for the guard page) and `wlwire`'s memfd, seals and holes have stand-ins,
so the allocator, the deep and nested buffers, and the Wayland engine's shm,
blob and stream paths run. A test Miri cannot run for another reason says
why in a `cfg_attr(miri, ignore = ...)`. Of 624 tests, 463 run to the end
(46 of `wlwire`'s 52, 417 of `device`'s 572), 128 stop at something Miri
cannot model, and 33 are ignored with their reason.

It found one thing, in ten tests (`5e62911`): every v1 path handed the host a pointer
taken from a slice of the guest's length, and the nested block's address
was taken before the writes that relocate its pointers. Under Stacked
Borrows the first covers only the guest's bytes, while the host copies
`_IOC_SIZE`, and the second is invalidated before the call. Not known to
miscompile, but undefined; now the host is handed the whole buffer
(`host_call`) and the address is taken last.
