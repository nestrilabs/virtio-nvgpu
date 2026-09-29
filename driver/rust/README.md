# `driver/rust/` — the guest module's parsers, in Rust

**License: GPL-2.0**, like the module.

The guest module is the boundary between mutually untrusting apps in one VM,
and between an app and the guest kernel. The code that reads what a guest
process hands the module is where a memory-safety bug becomes a guest kernel
compromise, so that code has a Rust implementation here, and it is what a
kernel with Rust gets:

```sh
make -C driver KDIR=<a CONFIG_RUST=y kernel's build tree>      # the Rust
make -C driver KDIR=<a kernel's build tree> NVGPU_RUST=0       # the C
```

Out of tree the Makefile takes the Rust when the target kernel has
`CONFIG_RUST=y` and the C (`nvgpu_i2.c`, `nvgpu_rmio.c`, `nvgpu_atomic.c`)
otherwise; in a kernel tree `CONFIG_VIRTIO_GPU_NV_RUST` (Kconfig, depends on
`CONFIG_RUST`, `default y`) decides. `NVGPU_RUST=1` insists on the Rust (a
build error against a kernel without `CONFIG_RUST`, and a plain one when no
`rustc` is on `PATH`), `NVGPU_RUST=0` on the C, which the module then says at
load on a kernel with Rust. The module records which it has (`modinfo -F
parsers`). The Makefile refuses to link a `nvgpu_rs.o` that names a panic
symbol. `scripts/build-guest-kernel.sh` builds a `CONFIG_RUST=y` kernel and
the Rust module in `scripts/guest-toolchain-rust` (the C fallback, with a
note, in a toolchain without Rust), and `driver/guest-kernel.defconfig` has
`CONFIG_RUST=y`. The C stays buildable and frozen -- fixes only, no new
parser features -- as the difftest's oracle and the fallback for a guest
kernel that cannot have Rust.

## What is in Rust, and what is not

| Rust (`core/src/guest/`) | replaces (C) |
|---|---|
| `i2.rs` — the IOCTL2 schema walk, the request, the reply, copy-back by the kernel's fill rules | `nvgpu_i2.c` |
| `schema.rs` — the tables' layout (the tables stay the generated C, read in place) and the entry lookup, which also names the native command a DRM caller's argument is normalised to (`i2::native_cmd`) | `nvgpu_i2_lookup()`, `nvgpu_i2_native_cmd()` |
| `dispatch.rs` — which path an ioctl on a `/dev/nvidia*` file, or a DRM file's RM (non-`'d'`) ioctl, takes; UVM | `nvgpu_ioctl_fd()`, `nvgpu_uvm_ioctl_fd()` |
| `rm.rs` — flat escapes, RM_CONTROL (GET_BUILD_VERSION and TIME_CORRELATION intercepts, OS_UNIX and OS-event descriptors, V1V2 deep pointers, the clock rebase), RM_ALLOC (event descriptors, class sizes), IDLE_CHANNELS, descriptor translation at a fixed offset, a v1 backend's NVKMS commands; the reply's header is `wire::IoctlResp::parse()`, the twin of `nvgpu_v1.c`'s (both builds) | `nvgpu_rmio.c` |
| `schema.rs`'s `fd_kind_allowed()` — whether one of the module's files may stand in a descriptor field (the backend's `kind_allowed()`); the C (`nvgpu_fd_kind_allowed()`, `nvgpu_schema.c`) is what both builds run, this the twin the difftest holds it to | -- |
| `deep.rs` — deep segments: the plan, the block, the copy-back | `nvgpu_deep_*()` |
| `osdesc.rs` — which calls register memory by its pages, the range, the page runs, the request, the reply's id | `nvgpu_osdesc_describe()`, `_runs()`, `_register()` |
| `atomic.rs` — an ATOMIC commit's object, property-count, property and value arrays: which CRTCs get flip events, what the commit teaches, which values are fences | `nvgpu_atomic.c` (was `nvgpu_kms_atomic()`) |

What stays C, and why:

- **Pinning** (`pin_user_pages_fast`, unpinning, the reap list,
  `nvgpu_osdesc.c`): kernel memory management with no Rust binding in 7.2
  that a module can use, and nothing in it reads guest input -- it is handed
  a range the Rust has already validated.
- **The IOCTL2 hooks** (`nvgpu_kms.c`, `nvgpu_fence.c`, `nvgpu_nvkms.c`,
  `nvgpu_drm.c`): DRM/KMS objects, `dma_fence`, `sync_file`, `dma-buf`, which
  7.2's Rust bindings do not cover for an out-of-tree module. They reach the
  kernel copies only through `nvgpu_i2_buf()` and friends, which Rust
  implements over buffers it owns. ATOMIC's arrays are parsed by
  `atomic.rs`, which asks `nvgpu_kms.c` (`struct nvgpu_atomic_ops`) what an
  object or a property is -- host queries behind caches -- and has it
  bridge the fences and reserve the events.
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
  carry canaries. The world's address space is split where x86-64's is, so
  the paths that read a kernel address -- IOCTL2's argument as the DRM
  node's entry copied it in (`nvgpu_i2_call.karg`) -- run too: the C
  through `nvgpu_i2_kread`/`_kwrite`, the Rust side through a store that
  follows `KStore::kern()`.
- `fuzz/` is cargo-fuzz (nightly, from fenix): `diff_rm` and `diff_i2` drive
  the differential test from libFuzzer's bytes, `i2_raw` feeds the Rust core
  raw bytes with debug assertions (overflow checks) on, `diff_atomic`
  drives ATOMIC commits through both.

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
rig/rig-build-kernel-rust.sh
```

The toolchain is `scripts/guest-toolchain-rust` (the rig's pinned nixpkgs:
gcc 15.3.0, rustc 1.98.1, bindgen 0.72.1, rust-src). Linux 7.2 asks for
rustc >= 1.85 and bindgen >= 0.71.1.

## Running the hardware regression on it

```sh
cp .rig/guest/rootfs.ext4 .rig/guest/rootfs-rust.ext4
rig/guest-image/mkimage.sh --module-only --module .rig/kernel-rust/nvgpu.ko \
  --out .rig/guest/rootfs-rust.ext4
NVGPU_KERNEL=.rig/kernel-rust/vmlinux NVGPU_ROOTFS=.rig/guest/rootfs-rust.ext4 \
  rig/run-guest.sh <probe>
```

On 2026-09-26 this ran the whole Group A and B regression on the RTX 5090
under nesbox, with results identical to the C module's (rig/TESTING-RIG.md,
"Regression of the merged tree").

Run the Rust module on the kernel it was built against. (It references no
symbol of the kernel's own Rust -- having no panic path, it needs no panic
handler and no `core` -- so it would also load into the C kernel of the same
vermagic; that is not what was tested.)

## Where it differed from the C

Nothing now, on purpose. The port read each block of the caller's once --
RM_CONTROL's nested block for the V1V2 count and pointer, the
TIME_CORRELATION clock (whole, before refusing a TSC clock: `-EFAULT` for a
block that does not all read) and the request; IDLE_CHANNELS' block for its
flat fallback; an escape that may register memory by its pages for
whichever path it takes -- where the C read the V1V2 block twice, the clock
byte and the class word apart from what it then sent, and IDLE_CHANNELS
twice. The C reads each once too since the 2026-09-26 review (SECURITY.md
§17), and the difftest's one recognised difference is gone.

Fixed in both since, each with a case in `difftest/tests/cases.rs` that
fails against the earlier C: a size with a NULL pointer (RM_CONTROL,
RM_ALLOC, v1 NVKMS) sent that much uninitialised guest kernel heap, and now
gets the native driver's answer; `nvgpu_i2_wr()` wrote 4 bytes for a field
of width 1 or 2, and the walk refuses a descriptor or GEM field of a width
the generator refuses; a GEM handle named again after a failing `gem_out`
hook was closed while an earlier proxy owned it; V1V2's count times 8
wrapped in u32; a v1 NVKMS call read and wrote 16 bytes whatever its size.
And from the 2026-09-26 review: an OS-descriptor registration abandoned in
flight hands its pins to the transport (`osdesc::Env::send_pinned`) instead
of keeping them under id 0 until remove(); a call that could be answered
with more descriptors than the state holds is refused before it is sent;
and the ATOMIC parse says commit/TEST_ONLY before any hook
(`atomic::Env::begin`), which the Rust wrapper once said only after the
parse -- so a hook reading it saw every commit as TEST_ONLY. The difftest
records what each in-fence hook sees. That flag, `out->commit`, is the only
source of commit-or-TEST_ONLY since the 2026-09-29 review: `nvgpu_kms.c`'s
in-fence hook reads it through its context, where it once computed the same
thing a second time from the same bytes.

From the 2026-09-29 review, in both: a v1 reply whose status is neither 0
nor an errno is `-EPROTO`, with nothing of it read (`nvgpu_v1.c`,
`wire::IoctlResp::parse`), where each path returned the raw status; and the
C's large blocks are `kvmalloc`'d, as the Rust's always were.

## Retiring the C

The Rust has passed, and is the default wherever the kernel has Rust (the
2026-09-29 review). The C stays for one more step: the fallback for a guest
kernel without `CONFIG_RUST` (a distribution kernel, or a toolchain that
does not match the kernel's), and the difftest's oracle. Once no supported
guest kernel lacks Rust: delete `nvgpu_i2.c`, `nvgpu_rmio.c` and
`nvgpu_atomic.c`, the `NVGPU_RUST` switch in `Makefile` (keep the Rust
objects unconditionally), the `NVGPU_RUST=0` path in
`scripts/build-guest-kernel.sh`, and `difftest/` (its `cases.rs` can become
core unit tests against a fake world; `renv.rs`, `backend.rs` and
`hooks.rs` are that world) -- after adding a smoke test that drives
`nvgpu_rs.rs`, the FFI, which the difftest never did and where the one
Rust-only bug was (the ATOMIC TEST_ONLY flag). The fuzz targets then drive
the core alone.
