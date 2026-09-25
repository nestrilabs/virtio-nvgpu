# `driver/rust/` — the guest module's parsers, in Rust

**License: GPL-2.0**, like the module.

The guest module is the boundary between mutually untrusting apps in one VM,
and between an app and the guest kernel. The code that reads what a guest
process hands the module is where a memory-safety bug becomes a guest kernel
compromise, so that code has a Rust implementation here, selected at build
time:

```sh
make -C driver KDIR=<a CONFIG_RUST=y kernel's build tree> NVGPU_RUST=1
```

`NVGPU_RUST=0` (the default) builds the C (`nvgpu_i2.c`, `nvgpu_rmio.c`) as
before. The C stays the default until the Rust build has passed the hardware
regression (below).

## What is in Rust, and what is not

| Rust (`core/src/guest/`) | replaces (C) |
|---|---|
| `i2.rs` — the IOCTL2 schema walk, the request, the reply, copy-back by the kernel's fill rules | `nvgpu_i2.c` |
| `schema.rs` — the tables' layout (the tables stay the generated C, read in place) and the entry lookup | `nvgpu_i2_lookup()` |
| `dispatch.rs` — which path an ioctl on a `/dev/nvidia*` file or a DRM file's driver range takes; UVM | `nvgpu_ioctl_fd()`, `nvgpu_uvm_ioctl()` |
| `rm.rs` — flat escapes, RM_CONTROL (GET_BUILD_VERSION and TIME_CORRELATION intercepts, OS_UNIX and OS-event descriptors, V1V2 deep pointers, the clock rebase), RM_ALLOC (event descriptors, class sizes), IDLE_CHANNELS, descriptor translation at a fixed offset, a v1 backend's NVKMS commands | `nvgpu_rmio.c` |
| `deep.rs` — deep segments: the plan, the block, the copy-back | `nvgpu_deep_*()` |
| `osdesc.rs` — which calls register memory by its pages, the range, the page runs, the request, the reply's id | `nvgpu_osdesc_describe()`, `_runs()`, `_register()` |

What stays C, and why:

- **Pinning** (`pin_user_pages_fast`, unpinning, the reap list,
  `nvgpu_osdesc.c`): kernel memory management with no Rust binding in 7.2
  that a module can use, and nothing in it reads guest input -- it is handed
  a range the Rust has already validated.
- **The IOCTL2 hooks** (`nvgpu_kms.c`, `nvgpu_fence.c`, `nvgpu_nvkms.c`,
  `nvgpu_drm.c`): DRM/KMS objects, `dma_fence`, `sync_file`, `dma-buf`, which
  7.2's Rust bindings do not cover for an out-of-tree module. They reach the
  kernel copies only through `nvgpu_i2_buf()` and friends, which Rust
  implements over buffers it owns. Their own parsing (ATOMIC's object and
  property arrays, in `nvgpu_kms.c`) is still C: the next thing to port.
- **The transport, virtio, mmap/window placement, the Wayland device** (`nvgpu_xfer.c`,
  `nvgpu_main.c`, `nvgpu_wl.c`): as the task set out; the transport's reply
  reaper (`nvgpu_reap_ioctl2`) reads host input, not guest input.
- **The generated tables** (`gen/*.h`): one copy, the C; Rust reads the
  schema arrays in place through `#[repr(C)]` mirrors whose layout both sides
  assert, and asks the C for the RM tables (deep controls, V1V2, class sizes,
  UVM sizes).

## How it is put together

- `core/` is a `no_std`, `#![forbid(unsafe_code)]` crate with no kernel
  dependency. It reaches the caller's memory, the transport, the hooks and
  pinning only through its traits (`i2::Store`, `i2::Env`, `rm::Env`,
  `osdesc::Env`). Every byte of the caller's is copied in once, into a buffer
  it owns; every decision is taken on that copy, and that copy is what is
  sent. It has **no panic path**: no index, slice, `unwrap`, division or
  unchecked arithmetic that can panic -- the kernel build checks this (`nm
  -u nvgpu_rs.o` names no `core::panicking` symbol), since a Rust panic in
  the kernel is a `BUG()`.
- `../nvgpu_rs.rs` includes the same sources (`#[path]`, as out-of-tree
  kernel Rust cannot use crates) and is the only Rust with `unsafe`: the
  traits over the C services in `../nvgpu_rs_glue.c`, each `unsafe` block an
  FFI call, a C table or buffer made a slice, or the state an IOCTL2 hook was
  handed made a reference again, each with its `SAFETY` comment.
  `../nvgpu_rs.h` is the ABI. The Rust exports the functions `nvgpu.h`
  declares for the parsers, so nothing else in the module changes.
- `difftest/` compiles the C parsers as they are (`nvgpu_i2.c`,
  `nvgpu_rmio.c`, `nvgpu_schema.c`) against a userspace shim, runs them and
  the Rust core in the same simulated world -- the caller's memory, a fake
  backend whose replies are a function of the request, the hooks, pinning --
  and requires the same result, the same messages byte for byte, the same
  bytes left in the caller's memory, the same hooks called with the same
  arguments, the same handles closed and pages pinned and kept, the same
  lines logged. The C is built with UBSan trapping, and its allocations
  carry canaries.
- `fuzz/` is cargo-fuzz (nightly, from fenix): `diff_rm` and `diff_i2` drive
  the differential test from libFuzzer's bytes, `i2_raw` feeds the Rust core
  raw bytes with debug assertions (overflow checks) on.

## Building, testing, fuzzing

```sh
export NIX_CONFIG="experimental-features = nix-command flakes"
# Unit tests, the C behaviour's cases, the differential test, properties:
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c \
  cargo test -p nvgpu-guest-core -p nvgpu-guest-difftest
DIFFTEST_ITERS=100000 ... cargo test --release -p nvgpu-guest-difftest --test diff
# Fuzzing (nightly):
nix shell github:nix-community/fenix#minimal.toolchain nixpkgs#cargo-fuzz nixpkgs#gcc -c \
  bash -c 'export LD_LIBRARY_PATH=$(dirname $(gcc -print-file-name=libstdc++.so.6)); \
           cargo fuzz run --fuzz-dir driver/rust/fuzz diff_i2 -- -max_total_time=600'
# The kernel with CONFIG_RUST and the module with the Rust parsers, in
# .rig/kernel-rust/ (never .rig/kernel/):
scripts/rig-build-kernel-rust.sh
```

The toolchain is `scripts/guest-toolchain-rust` (the rig's pinned nixpkgs:
gcc 15.3.0, rustc 1.98.1, bindgen 0.72.1, rust-src). Linux 7.2 asks for
rustc >= 1.85 and bindgen >= 0.71.1.

## Running the hardware regression on it

```sh
cp .rig/guest/rootfs.ext4 .rig/guest/rootfs-rust.ext4
guest-image/mkimage.sh --module-only --module .rig/kernel-rust/nvgpu.ko \
  --out .rig/guest/rootfs-rust.ext4
NVGPU_KERNEL=.rig/kernel-rust/vmlinux NVGPU_ROOTFS=.rig/guest/rootfs-rust.ext4 \
  scripts/run-guest.sh <probe>
```

Run the Rust module on the kernel it was built against. (It references no
symbol of the kernel's own Rust -- having no panic path, it needs no panic
handler and no `core` -- so it would also load into the C kernel of the same
vermagic; that is not what was tested.)

## Where it differs from the C, on purpose

- **Read once.** RM_CONTROL's nested block is copied once and the V1V2
  count and pointer, the TIME_CORRELATION clock, the descriptors in it and
  the request are all that copy (the C read the V1V2 block twice, and the
  clock byte apart from the block); IDLE_CHANNELS' block likewise (the C
  read it twice when it fell back to the flat form); an escape that may
  register memory by its pages is copied once and the same bytes go to
  whichever path it takes (the C read the class word, then the block). With
  no other thread writing the caller's memory, the results are the same.
- **Unreadable before decided.** TIME_CORRELATION reads its whole block
  before refusing a TSC clock: a block that is not all readable is
  `-EFAULT`, where the C read one byte and answered NOT_SUPPORTED.
- **Zeroed.** Where the C sent `kmalloc()` bytes it never wrote (an
  RM_CONTROL, RM_ALLOC or v1 NVKMS call with a size and a NULL pointer: the
  size's worth of guest kernel heap, to the backend), the Rust sends zeroes.

## When the Rust has passed

Delete `nvgpu_i2.c` and `nvgpu_rmio.c`, the `NVGPU_RUST` switch in
`Makefile` (keep the Rust objects unconditionally), the `NVGPU_RUST`
handling in `scripts/build-guest-kernel.sh` (always enable `CONFIG_RUST`),
and `difftest/` (its `cases.rs` can become core unit tests against a fake
world; `renv.rs`, `backend.rs` and `hooks.rs` are that world). The fuzz
targets then drive the core alone.
