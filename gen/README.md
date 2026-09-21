# `gen/` — generated ABI tables

Checked in **and** reproducible. Both properties matter: a contributor must be
able to read the tables without running anything, and regenerate them without
asking anyone.

NVIDIA's kernel driver ABI is not stable — ioctl struct layouts change between
releases. The tables here map driver versions to struct layouts and to the set
of commands that exist and are safe to forward.

Two halves, with different risk:

- **The struct half is derived mechanically** from NVIDIA's published
  `open-gpu-kernel-modules` at each tag, by compiling a probe per field and
  reading back `sizeof`/`offsetof`. Nothing is transcribed by hand.
- **The judgement half** — which commands exist, and which are safe to expose —
  follows gVisor's `nvproxy` upstream.

Profiles key off **ranges, not points**: a driver release between two known
versions selects the lower profile rather than requiring a new row. This is why
the per-release cost is small rather than open-ended.

The generator must stay runnable by someone who does not work on this project.

---

## The generator

Two scripts, no build system, no Go toolchain. gVisor is a Bazel project and
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
