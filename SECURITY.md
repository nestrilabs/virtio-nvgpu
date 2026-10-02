# Security

This document says what virtio-nvgpu protects, what it does not, and what is
still being built. It describes the `feat/v0.2-compute` branch. Each limit is
stated next to the feature it limits.

## Model

A guest runs NVIDIA's own user-mode driver. Its ioctls are carried to a
backend process on the host, which issues them against the host's NVIDIA
kernel driver on the guest's behalf. The guest never touches the PCI device.

There is no IOMMU boundary between a guest's GPU work and the host. The card
belongs to the host driver and sits in the host's IOMMU domain. What keeps a
guest's GPU work out of host memory is the GPU's own MMU, with page tables the
host driver programs. So the host NVIDIA kernel driver is in the trusted
computing base, and so is the backend.

For mutually untrusted tenants, VFIO passthrough with an IOMMU, or NVIDIA
vGPU, is stronger. It confines the device to one guest's memory. virtio-nvgpu
reduces the attack surface a guest can reach. It does not add hardware
isolation.

## What a guest's kernel bug reaches

A guest runs its own Linux kernel under KVM. A bug in the guest kernel, or in
anything running in the guest, is contained by the virtual machine. It does
not reach the host kernel's system call interface. The only host interfaces a
guest can drive are the virtio devices its VMM offers, and for the GPU that is
the backend described below.

## What the backend refuses

These hold on this branch.

| check | what is refused |
|---|---|
| ABI profile | an NVIDIA escape the host release's profile does not list, or a parameter block of the wrong size |
| release | a host driver older than every profile; the backend does not start |
| privilege | running as root or with CAP_SYS_ADMIN; the backend does not start, and drops every capability and sets no_new_privs before it opens a device |
| capabilities | device nodes and RM classes outside `--caps`: `nvidia-uvm` without compute, `nvidia-modeset` and render nodes without graphics, 3D classes without graphics, NVENC, NVDEC, NVJPG and OFA classes without video |
| UVM tools | `nvidia-uvm-tools`, under every capability |
| the backend's own reach | every path but the GPU nodes, the driver's `/proc` and `/sys` trees and the socket's directory; every executable mapping; every new process; every socket but AF_UNIX; every system call outside the allowlist. A kernel without Landlock or without seccomp filtering is refused at start |
| UVM init | the guest's UVM_INITIALIZE flags; the backend sends its own, with HMM off and sharing mode on, and pageable access off where the release has the flag |
| embedded pointers | a guest address in a pointer RM dereferences inside an RM control's parameters: the backend supplies every such buffer itself, sized from the count in the guest's own block and bounded at 1 MiB per pointer and 2 MiB per call |
| allocation rights | `NVOS64.pRightsRequested`, a pointer RM dereferences when it is not null; the host sees null and the caller gets its own value back |
| release drift | on a host with no pointer table of its own, every control any release describes a pointer in that the selected table does not; the backend does not start when no table covers the release at all |
| half-sent buffers | a control whose pointer RM reads and whose bytes the guest did not send, or sent a different number of; refused rather than served with zeroes |
| undescribed controls | a control whose embedded pointers the generated table cannot describe, including one RM compiles in only under a build flag; refused with NV_ERR_NOT_SUPPORTED |
| RM privilege | an RM control or class RM does not serve to an unprivileged caller. RM applies its rule to whoever called it, and here that is the backend, not the guest -- so the backend applies RM's rule itself, from RM's own tables, before forwarding. All four of RM's dispatch paths are covered: the exported-method flags, the deprecated table (by the privilege of the command each one is rewritten into), the commands RM forwards to GSP firmware (by the bit RM tests in the command number), and a class whose control is a passthrough (by the privilege of the class) |
| the host, through RM | eight controls RM itself marks non-privileged and this backend refuses regardless: the host's process list through the GPU (GET_PIDS, GET_PID_INFO), GPU accounting state and the pids it recorded, and the two that let a caller rename itself inside the backend's client or switch off USERD isolation. On a host these answer a local user about their own machine; here they answer a guest about someone else's |
| oversized parameters | a parameter block larger than RM will copy for an unprivileged caller, on the paths where RM reads no struct and so declares no size. 535.129.03 applies no ceiling of its own there; the backend applies one anyway |

The RM privilege row rests on RM's tables and on nothing written by hand: the
generator reads each release's own flag values, parses its deprecated-control
table and the command each converter rewrites into, and asserts the shape of
RM's two privilege checks and of the dispatch order itself. If any of them
changes it stops rather than carry on with a rule that no longer describes the
driver. The numbers differ by release often enough that this is not caution:
535.129.03 numbers the control flags differently from 615.71.09, carries an
extra column in the deprecated table, and caps a GSP-forwarded parameter block
at nothing at all.

The rule never asks whether a privileged bit is clear. RMCTRL_FLAGS_KERNEL_-
PRIVILEGED and RS_FLAGS_NONE are both zero, so an entry with no bits set is the
most privileged thing in RM's table rather than the least -- 32 controls in
615.71.09 are exactly that. Everything tests for the non-privileged bit being
set.

The embedded-pointer rows hold for a host release with a table of its own
(535.129.03, 580.178.04, 595.71.05, 595.104.02, 615.71.09) and for one between
two of them or newer than all of them. In the second case the table is an older
release's, which cannot describe a control a newer release added, so a control
any release describes and that table does not is refused. Adding a release is
one generator run.

There is no command-line flag that switches any of these off.

A refused open returns ENODEV, as on a host without the device. A refused RM
allocation returns NV_ERR_INVALID_CLASS in RM's status word, as for an engine
the GPU does not have. The backend counts every refusal and prints the totals
when a guest exits.

## What is not done

These are open on this branch. Until they land, treat a guest as able to reach
the backend process itself, and treat the backend as able to reach everything
its user can.

- An RM allowlist. RM controls and classes inside a capability are forwarded
  without a per-release list of what is safe to expose.
- UVM size tables. UVM ioctls are not yet checked against per-release sizes,
  which is one reason compute is opt-in.
- Memory registered by CPU address. Allocations that name guest memory by
  address are not yet translated.
- Per-process isolation inside one guest. One backend serves every process
  in a guest. The design for one helper per guest process is in `isolate/`
  and is not built.

## Compared with gVisor's nvproxy

gVisor's nvproxy is the closest prior work and its security notes are a good
model. The comparison, item by item:

| | nvproxy | virtio-nvgpu |
|---|---|---|
| host kernel exposure from general code | gVisor's Sentry, behind a seccomp filter | a guest kernel under KVM |
| which ioctls reach the driver | an allowlist per driver release | an ABI profile per release, and an RM allowlist generated from RM's own privilege tables |
| process confining the forwarder | seccomp | seccomp and Landlock, entered before the first thread |
| driver bugs in forwarded calls | not mitigated | not mitigated |
| DMA buffer validation | none | none |

The last two rows are the same for both and cannot be fixed here. A
vulnerability in a part of the NVIDIA driver a guest is allowed to call is
reachable through this project. Keep the host driver current.

## Reporting

Report a vulnerability privately through GitHub's security advisories on this
repository, not in a public issue.
