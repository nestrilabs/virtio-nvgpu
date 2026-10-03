# gen

Tables of NVIDIA's kernel ABI, one per driver release, and the scripts that
make them. The tables are checked in, so they can be read without running
anything, and every one can be regenerated from public sources.

NVIDIA's kernel interface changes between releases. Struct sizes, field
offsets, command numbers and index values move. The backend selects the table
for the host's release at start. A release with no profile of its own gets the
nearest older one, and where an older table would be wrong rather than
incomplete, the backend refuses instead.

## Generators

| script | reads | writes | what the backend does with it |
|---|---|---|---|
| `nvabi_gen.py` | gVisor's nvproxy | `src/versions/` | escape sizes, the ABI check on every forwarded ioctl |
| `rmctrl_extract.py` | open-gpu-kernel-modules | `src/rmctrl/`, `../driver/rmctrl/` | RM controls whose parameters hold pointers |
| `rmallow_extract.py` | open-gpu-kernel-modules | `src/rmallow/` | the RM controls and classes a guest may use |
| `uvm_extract.py` | open-gpu-kernel-modules | `src/uvm/` | UVM commands, their sizes and descriptor fields |
| `osdesc_extract.py` | open-gpu-kernel-modules | `src/osdesc/` | the calls that name memory by CPU address |
| `vidmem_extract.py` | open-gpu-kernel-modules | `src/vidmem/` | video memory allocations and the figures that report memory, for `--vram-limit-mib` |
| `nvgpu_gen.py` | open-gpu-kernel-modules | `../driver/gen/` | RM allocation sizes and V1 to V2 rewrites, for the guest module |

The `*_extract.py` scripts compile a small C probe against the release's own
headers and print what it reports. Nothing is transcribed by hand. Where a
script has to classify something, a release that adds an unclassified entry
stops the script, so a person reads it first.

```sh
./vidmem_extract.py --ogkm ~/forks/ogkm-615.71.09 --version 615.71.09 \
    > src/vidmem/v615_71_09.rs
```

`--ogkm` is an open-gpu-kernel-modules checkout at the release's tag. The
scripts need Python 3 and a C compiler. `nvabi_gen.py` reads a gVisor checkout
instead, with no Go toolchain.

## Adding a release

1. Check out open-gpu-kernel-modules at the release tag.
2. Run each generator for it, and add the new file to that table's `mod.rs`.
3. Run `cargo test -p abi`. The tests check every table for internal
   consistency and against the releases around it.
4. Run the rig's probe set on a host with that release before shipping it.

## Fixtures

`fixtures/*.tsv` holds ioctl parameter sizes captured on real hardware with
`nvidia_sniffer`. `src/fixtures.rs` checks the tables against them. A fixture
is evidence only for the release and GPU architecture that produced it.
`580.178.04.tsv` came from a Tesla T4.
