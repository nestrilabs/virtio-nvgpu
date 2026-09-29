# `gen/` — generated ABI tables

Checked in **and** reproducible. Both properties matter: a contributor must be
able to read the tables without running anything, and regenerate them without
asking anyone.

NVIDIA's kernel driver ABI is not stable — ioctl struct layouts change between
releases. The tables here map driver versions to struct layouts and to the set
of commands that exist and are safe to forward.

There are several, each with its own generator:

| table | from | what it is | section |
|---|---|---|---|
| `src/versions/*.rs` | `nvabi_gen.py`, gVisor's nvproxy | the ABI profiles: RM escape parameter sizes and kinds | [The generator](#the-generator) |
| `schema/*.py` → `src/schema/generated.rs`, `../driver/gen/nvgpu_schema.h` | `schema_gen.py` | the IOCTL2 schema both halves interpret: DRM render and KMS, nvidia-drm, NVKMS per release, UVM block sizes | [The IOCTL2 schema](#the-ioctl2-schema) |
| `nvkms/*.json` | `nvkms_extract.py` | NVKMS and nvidia-drm ioctl layouts, per release | [NVKMS and nvidia-drm layouts](#nvkms-and-nvidia-drm-layouts) |
| `rmctrl/*.json` → `src/rmctrl/generated.rs`, `../driver/gen/nvgpu_rm_deep.h` | `rmctrl_extract.py` | where RM follows a pointer inside a control's parameters, and how much it copies through it, per release | [RM control pointers](#rm-control-pointers) |
| `uvm/*.json` | `uvm_extract.py` | UVM parameter block sizes and descriptor offsets, per release | [UVM parameter blocks](#uvm-parameter-blocks) |
| `rmallow/*.json` → `src/rmallow/generated.rs` | `rmallow_extract.py` | the RM allowlist: which controls and classes a guest may reach, per release | [RM allowlist](#rm-allowlist) |
| `../driver/gen/nvgpu_rmalloc_classes.h`, `nvgpu_v1v2_rewrites.h` | `nvgpu_gen.py` | the guest module's RM class and rewrite tables | — |

The profiles key off a range of releases; the NVKMS, nvidia-drm, RM control and
UVM tables are measured per release, because the layouts they describe move
more often.

Two halves, with different risk:

- **The struct half is derived mechanically** from NVIDIA's published
  `open-gpu-kernel-modules` at each tag, by compiling a probe per field and
  reading back `sizeof`/`offsetof`. Nothing is transcribed by hand.
- **The judgement half** — which commands exist, and which are safe to expose —
  follows gVisor's `nvproxy` upstream.

The ABI profiles key off **ranges, not points**: a driver release between two
known versions selects the lower profile rather than requiring a new row. The
other tables do not: the backend starts only on a release every table was
measured at (DEPLOY.md, "The exact-measured-release rule"), so a new host
release still needs its NVKMS, RM control, UVM and allowlist tables ("A new
host release", below).

**Reference release.** The project reads NVIDIA's sources at
`open-gpu-kernel-modules` 610.57.04, the tree the display work was written
against, unless a table says otherwise. A citation names its release
(CONTRIBUTING.md, "Comments and documentation"); an older one that does
not may be of 595.99.02 or of 610.57.04, so check its line numbers against
both.

The generator must stay runnable by someone who does not work on this project.

---

## The generator

The ABI profiles come from two scripts, no build system, no Go toolchain. gVisor is a Bazel project and
`go build` on it fails without generated code, so these read its sources
directly rather than linking against it.

```sh
# one profile, from a gVisor checkout
./nvabi_gen.py --gvisor ~/forks/gvisor --version 580.178.04 \
    > src/versions/v580_178_04.rs

# just a struct size, when checking one thing by hand
./nvabi_sizes.py --gvisor ~/forks/gvisor NVOS46_PARAMETERS_V580
```

`nvabi_sizes.py` computes `sizeof` from the Go declarations in
`pkg/abi/nvgpu`. Those structs are `structs.HostLayout` — deliberately laid out
like the C structs they mirror — so applying natural alignment reproduces the
driver ABI.

`nvabi_gen.py` resolves the version chain. nvproxy records each driver release
as a delta against its parent, so the ABI for one version is the base map plus
every override along its lineage; the generated file names the chain it walked.

Adding a driver version is one command plus a line in `src/versions/mod.rs`.
If gVisor does not know the version, the generator says so and stops rather
than guessing.

### A new host release

The backend refuses to start on a host release the tables were not measured
at (`device/src/release.rs`): it needs this release's own RM allowlist
(`rmallow/<release>.json`) and NVKMS schema (`nvkms/<release>.json`), a UVM
table whose range holds it (the last one ends at the newest `uvm/*.json`),
and an ABI profile no newer than `MEASURED_THROUGH` in `src/versions/mod.rs`.
So measuring a release is: `rmallow_extract.py extract`, `nvkms_extract.py
extract`, `uvm_extract.py extract` (and `scan`), `rmctrl_extract.py extract`,
the renders, `schema_gen.py`, and moving `MEASURED_THROUGH` once gVisor's
nvproxy (or a capture in `fixtures/`) shows the frontend escapes unchanged --
or a new profile if they moved. Until then `--allow-unmeasured-release
--diagnostic` runs the host on the nearest older tables, without compute,
and says so at every start. `scripts/gen-check.sh` re-measures every
checked-in release.

## Fixtures

`fixtures/*.tsv` holds ioctl parameter sizes **observed on real hardware**,
captured with `nvidia_sniffer` under `LD_PRELOAD`.

The tables come from nvproxy. The fixtures come from a running GPU. They are
independent, and `src/fixtures.rs` asserts they agree — which is the only place
a wrong table is caught before it reaches a guest, where the symptom is a
silently truncated ioctl rather than an error.

`fixtures/580.178.04.tsv` was captured on a Tesla T4 (Turing) from
`nvidia-smi`, `vulkaninfo`, a CUDA driver-API probe and an `h264_nvenc` encode.
It found one real defect on arrival: the hand-written table had
`NV_ESC_RM_MAP_MEMORY_DMA` at 48 bytes, where 580 uses `NVOS46_PARAMETERS_V580`
at 64.

A fixture is only evidence for the driver version and architecture that
produced it. RM class IDs are per-architecture, so a Turing capture says
nothing about Ampere's channel classes.

## The IOCTL2 schema

Protocol v2 sends an ioctl whose argument points at more memory, or names a
descriptor or a GEM handle, as one IOCTL2 (ARCHITECTURE.md, "Protocol v2"). The guest
gathers the caller's buffers by a table; the backend walks its own copy of the
same table over what it received and refuses anything that disagrees. The two
copies must never differ, so they come from one source:

- `schema/lang.py` — the schema language: pointers with a direction, a length
  rule and a copy-back rule; inline arrays; descriptors in and out; GEM
  handles in and out; conditions; and the *canonical traversal* both sides use
  to number buffers
- `schema/drm_render.py`, `schema/drm_kms.py`, `schema/nvidia_drm.py`,
  `schema/nvkms.py` — the entries: DRM render-node and KMS ioctls, nvidia-drm's
  private ones, and NVKMS, one table per release converted from `nvkms/`
- `schema/formats.py` — plane counts per pixel format, for framebuffer checks
- `schema/uvm.py` — UVM block sizes per range of releases, from `uvm/`

```sh
./schema_gen.py                 # regenerate both tables in place
./schema_gen.py --out DIR       # write them under DIR instead
./schema_gen.py --probe DIR     # write C probes asserting every size and offset
```

The generator checks the schema before it writes anything — every field inside
its struct, no two fields overlapping unless their conditions exclude each
other, every count readable where it is read, every pointer capped — and a
schema that fails is a bug in the schema. The probes are `_Static_assert`s of
every size, offset, ioctl number and fourcc the schema states, to be compiled
against the kernel's uapi headers and the host release's
`open-gpu-kernel-modules`; the script's docstring has the command. The Rust
test `the_checked_in_tables_are_what_the_generator_writes` regenerates into a
temporary directory and compares, so an edit to the Python that was not
regenerated and committed fails the build's tests.

## NVKMS and nvidia-drm layouts

`nvkms_extract.py` compiles, for each release, where every pointer,
descriptor and GEM handle sits in NVKMS's and nvidia-drm's private ioctls, the
size of every parameter block and of its request and reply halves, and the
fields the backend's NVKMS policy has to read or rewrite. It reads the
release's own headers and dispatch tables, fetched from
`open-gpu-kernel-modules` at the tag, and writes `nvkms/<release>.json`. It
fails loudly rather than guess, and `selftest` proves the refusals still fire.

```sh
./nvkms_extract.py all        # fetch + extract every release
./nvkms_extract.py check      # regenerate and fail if nvkms/ is stale
./nvkms_extract.py selftest
```

Releases measured: 535.129.03, 580.178.04 and 595.71.05 (the ABI profiles),
595.99.02 (the RTX 3060 every benchmark comes from, and the RTX 5090 the
current code runs on), 610.57.04 (the tree the display work was written
against) and 615.71.09 (the RTX A2000). Only with `--allow-unmeasured-release`
does a host between two use the older table, and then an NVKMS command whose
layout moved in the next release measured runs only on the exact release. Command numbers are never
carried from one release to another: REGISTER_SURFACE is 16 in some and 17 in
others. [`nvkms/README.md`](nvkms/README.md) has the format and the method.

## RM control pointers

`rmctrl_extract.py` measures, per release, where RM follows a user pointer
inside an RM_CONTROL's parameters. The backend is the caller of every
forwarded control, so each such pointer is an address in *its* process, and
`device/src/guestptr.rs` zeroes every one the guest did not send the data
for; this is the table it does that from.

The commands and fields come from RM's own sources: each `case` of
`embeddedParamCopyIn` (embedded_param_copy.c) with its
`RMAPI_PARAM_COPY_INIT` calls, each converter of the deprecated V1 control
table (rmapi_deprecated_control.c) with its user copies, and a short
hand-written list of handlers that copy user memory themselves (`SELF_COPY`)
or whose pointers have no fixed place (`REFUSED`). The offsets come from a C
probe compiled against the release's SDK headers, and every field is checked
to be an 8-byte NvP64. The extractor refuses to write anything if the release
has an RM `.c` file that calls a user-copy primitive and none of those
entries account for it (`KNOWN_USER_COPY_FILES`): a new handler that follows
a pointer fails the run instead of being missed.

```sh
./rmctrl_extract.py all      # fetch + measure every release, render the table
./rmctrl_extract.py check    # re-measure and fail if gen/rmctrl/ is stale
./rmctrl_extract.py render   # gen/rmctrl/*.json -> src/rmctrl/generated.rs
```

Each pointer copied through `RMAPI_PARAM_COPY_INIT` also gets RM's size rule
for it — the count fields multiplied in NvU32, times the element size — read
from the call's arguments and measured by the same probe, with its direction
from the SKIP_COPYIN/SKIP_COPYOUT flags; RmDeprecatedIdleChannels is read the
same way for NV_ESC_RM_IDLE_CHANNELS. A control whose every pointer has a rule
is one the guest may send as deep segments, and the backend checks each
segment's length against the rule. A control with several pointers must have
a rule for each or be listed in `LEFT_ZEROED` with the reason (the ACPI
methods among them); the extractor fails otherwise.

`gen/rmctrl/<version>.json` holds each release's measurement;
`src/rmctrl/generated.rs` is their union (a command whose pointers differ
between releases lists all of them; one relocated in some releases and not
others is not relocated), and `../driver/gen/nvgpu_rm_deep.h` the guest's copy
of the multi-pointer rows, rendered from the same union. `render` is pure Python, and the Rust
test `the_checked_in_table_is_what_the_extractor_renders` runs it, as the
schema's does. Sources are cached in `$RMCTRL_EXTRACT_CACHE` (default
`$TMPDIR/ogkm-rm`).

## UVM parameter blocks

nvidia-uvm's ioctl numbers carry no size (`UVM_IOCTL_BASE(n)` is plain `n`,
and the `0x3000` in `UVM_INITIALIZE`'s is not the size of anything), and the
kernel copies exactly `sizeof(<cmd>_PARAMS)` each way. `uvm_extract.py`
measures that size for every UVM command the backend lets through
(`device/src/guestptr.rs`, `UVM_ALLOWED`), plus the offset of the descriptor
the six file-naming commands carry (`device/src/uvmfd.rs`), with a C probe
compiled against each release's own `uvm_linux_ioctl.h` and the SDK headers
it includes, fetched file by file from the tag.

```sh
./uvm_extract.py all      # fetch + measure every release in VERSIONS
./uvm_extract.py check    # re-measure and fail if gen/uvm/ is stale
./uvm_extract.py scan     # measure every tag since 535.129.03 (slow)
```

`gen/uvm/<version>.json` holds each release's measurement, and
`schema_gen.py` renders them (through `schema/uvm.py`) into both halves'
tables: `nvgpu_uvm_tables` in `driver/gen/nvgpu_schema.h`, which sizes every
UVM call the guest makes, and `UVM_TABLES` in `src/schema/generated.rs`,
which the backend holds each block to. A command with no row is refused by
both. The ranges run from each release to the next measured one, as the
NVKMS tables' do, but here that is checked rather than assumed: `scan`
measures every published tag and fails if one differs from the table it
would get. Besides the six measured releases (the three ABI profiles,
595.99.02, 610.57.04 and 615.71.09), `VERSIONS` has the four where it found a
change (550.40.53, 565.57.01, 580.65.06, 590.44.01). Sources are
cached in `$UVM_EXTRACT_CACHE` (default `$TMPDIR/ogkm-uvm`).

## RM allowlist

`rmallow_extract.py` decides which RM_CONTROL commands and RM_ALLOC classes
reach the host's RM at all; `device/src/rmallow.rs` answers everything else
before RM sees it, as RM answers a call it does not implement. Default deny,
per release.

The measured half, per release, from the tag's sources: every exported
control of every NVOC class (`src/nvidia/generated/g_*_nvoc.c`: method id,
RMCTRL flags read with that release's own `control.h` values -- they moved
between 535 and 580 -- the exporting class, and its parameter type, sized by
a C probe against the SDK headers); its FINN name; the deprecated V1
controls RM converts first, each with the V2 it becomes; what DEFERRED_API
may bundle; whether the parameters (nested structs included) carry a
pointer, a descriptor, a process id or an OS event; every class of
`resource_list.h` with its RS flags and implementing NVOC class, and whether
any GPU the release drives has it (`g_gpu_class_list.c`); and the offsets
of every field of the escapes' own blocks the backend reads or writes, from
`nvos.h` (`OS_BLOCKS`; `abi::rmallow::nvos`, re-exported by
`device/src/nvos.rs`), which render refuses to emit if a release moved one.

The judgement half is `POLICY` in the script. A control or class is allowed
only if RM would serve it to an unprivileged process (NON_PRIVILEGED, and not
PRIVILEGED, KERNEL_PRIVILEGED or INTERNAL; for classes ALLOC_NON_PRIVILEGED),
it names no host resource the backend does not translate, and a workload
asks for it:

- **observed**: `rmallow/observed.txt`, every control and class RM served
  across the rig's 106 hardware runs (Vulkan, GL, EGL, CUDA, Firefox,
  Chromium, mpv, gamescope, nvidia-smi);
- **own object**: every user-callable control of an object the guest itself
  allocated and RM confines to it (channel, channel group, context share,
  graphics context, memory, VA space, memory mapper, semaphore surface, user
  shared data, context DMA), with two scheduling controls left out; and for
  classes, every user-allocatable class of the NVOC classes observed --
  which is how every architecture's channel, usermode, 3D, compute, copy,
  NVDEC, NVENC, NVJPG and OFA classes come in, filtered to those some GPU
  the release drives has;
- **workload**: named controls and classes for NVENC, NVDEC, Vulkan Video,
  graphics and compute paths, read from gVisor nvproxy's compute, utility,
  graphics and video lists as a hint before the application pass ran them
  (SECURITY.md, "The RM allowlist", and the application pass in its
  review history).

Controls RM hands to GSP-RM without a CPU-side table -- the GSS legacy ones
(bit 15 of the command) and every control of an NV2081_BINAPI object -- have
no name or size in the open sources; only the observed ones are allowed, by
number.

```sh
./rmallow_extract.py all       # fetch + measure every release, render
./rmallow_extract.py check     # re-measure and fail if gen/rmallow/ is stale
./rmallow_extract.py render    # gen/rmallow/*.json + policy -> src/rmallow/generated.rs
./rmallow_extract.py report    # allowed and refused counts per release, by reason
```

`render` is pure Python, and the Rust test
`the_checked_in_table_is_what_the_extractor_renders` runs it, so a policy
edit that was not rendered and committed fails the tests; `render` also fails
if the policy names a control or class no release has, or refuses an
observed call RM would serve. Only with `--allow-unmeasured-release` does a
host between two releases get the older list; parameter sizes are held to
RM's only on a release measured exactly.
Sources are cached in `$RMALLOW_EXTRACT_CACHE` (default `$TMPDIR/ogkm-rmallow`).
