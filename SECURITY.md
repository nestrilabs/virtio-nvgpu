# Security

Found a security hole? Skip to [Reporting](#reporting).

## The short version

virtio-nvgpu **reduces** what a guest can reach. It does **not** add hardware
isolation.

- A guest never touches the GPU directly. Its driver calls go to a backend on the
  host, which checks them and passes them to the host's NVIDIA driver.
- There's no IOMMU between the guest's GPU work and the host. The GPU's own MMU keeps them
  apart, so **the host NVIDIA driver and the backend are both trusted**.

If your tenants don't trust each other, use VFIO passthrough or NVIDIA vGPU.

## What's contained

- **Guest kernel bugs** stay in the VM. A guest can only reach the host through
  the virtio devices its VMM offers.
- **Each VM gets its own backend**, which refuses to run as root. Before it
  opens the GPU it drops its capabilities and locks itself down with seccomp
  and Landlock. It can't run programs, open other files, or use any socket other than
  AF_UNIX.

## What the backend refuses

None of these can be switched off from the command line.

- **Unknown calls.** Any ioctl or UVM call the host driver release doesn't
  define, or one of the wrong size. Drivers older than 535.129.03 are refused
  at startup.
- **Privileged RM calls.** NVIDIA's RM decides privilege by who called it, and
  that's the backend, not the guest. So the backend applies RM's rules itself.
  The allowlist is generated from RM's own tables, never hand-written.
- **Host snooping.** Eight calls RM allows anyone are refused anyway, including
  the host's process list and GPU accounting.
- **Guest pointers.** No guest address reaches RM. The backend supplies every
  buffer itself, at up to 1 MiB per pointer and 2 MiB per call.
- **Anything outside `--caps`.** For example, no CUDA (`nvidia-uvm`) without
  `compute`, no encoders without `video`, and no 3D without `graphics`.
  `nvidia-uvm-tools` is always refused.
- **Bad CUDA mappings.** UVM pools are checked for size, alignment, address and
  overlap, by the backend and again by the VMM.
- **VRAM past `--vram-limit-mib`.** Allocations over the limit fail, and the
  guest sees the limit as the card's size.

The backend counts every refusal and prints the totals when a guest exits.

## What isn't covered

- **Bugs in the NVIDIA driver itself.** If a guest is allowed to make a call and
  that call has a bug, the guest can reach it. Keep the host driver up to date.
- **DMA buffers aren't validated.** gVisor's `nvproxy` doesn't validate them
  either.
- **The VRAM limit is approximate.** Memory RM allocates internally (about
  34 MiB per encoding guest) isn't counted.
- **Processes inside one guest aren't isolated from each other.** One backend
  serves the whole VM.

## Reporting

Please report vulnerabilities privately, not in a public issue:

- Email **[security@nestri.io](mailto:security@nestri.io)**, or
- Open a private [GitHub security advisory](../../security/advisories/new) on
  this repository.

If you can, include the driver version, the GPU, and the steps to reproduce. Thank you in advance.
