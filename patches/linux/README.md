# Host kernel patches (drafts, not applied)

Nothing here is applied to any kernel the project runs, builds or ships. Each
patch is a proposal for the host kernel, written up where a measured cost has
no fix inside the project.

| Patch | Against | What it does | State |
|---|---|---|---|
| `0001-KVM-x86-KVM_PRE_FAULT_MEMORY_WRITE-draft.patch` | Linux 7.2.7 | a `flags` bit for `KVM_PRE_FAULT_MEMORY` that maps as a write fault would | draft: applies to 7.2.7 (`patch --dry-run`); the objects it touches compile against an x86 KVM config; never booted |

## 0001: a prefault that maps for write

**What it fixes.** A guest writing system memory it has just allocated
through the GPU driver writes it at about 1 GB/s against the host's 20
(BENCHMARKS.md, "Heavy workloads": `vk-stream`, a quarter of native speed).
The VMMs map every window placement into the guest with
`KVM_PRE_FAULT_MEMORY` (nesbox's memslot code, crosvm's
`patches/crosvm/0010`), which maps as a read fault would; KVM makes a page
writable on a read fault only when GUP-fast can prove the host mapping
writable, and with `CONFIG_SECRETMEM` GUP-fast refuses every order-0 folio
whose `->mapping` is NULL -- every page the NVIDIA driver maps with
`vm_insert_page()` (`nvidia_mmap_sysmem()`). So system memory is prefaulted
read-only and the first write to each 4 KiB page is a second stage-2 fault
through KVM's slow path. Video memory (a PFNMAP, whose writability KVM reads
from the PTE) and the driver's compound pages (allocations of order above 0)
are prefaulted writable already.

**What the VMMs would do with it.** For a placement the backend made
writable -- only those; a read-only placement must stay a read fault -- ask
with `KVM_PRE_FAULT_MEMORY_WRITE`, and on `EINVAL` (a kernel without it)
drop the flag for good and prefault as today. The write fault dirties the
pages in KVM's dirty log (neither VMM logs dirty pages of the window) and
would break CoW of a private mapping (the window's are shared).

**Why not inside the project.** Checked and rejected:

- *RM allocating the memory in larger pages*, so the driver allocates
  compound pages that GUP-fast accepts. The page size is the guest's
  `NVOS32_ATTR_PAGE_SIZE`; rewriting it to 64 KiB changes what RM reports
  back (the attribute, and a size and alignment rounded to the page), makes
  every such allocation need 64 KiB of contiguous memory -- a failure under
  fragmentation that a native process of the same request does not see --
  and can change which GPU page size maps it. The guest would see its
  allocations differ from what it asked for, which the project does not do.
- *The VMM or the backend touching the pages first* moves the fault, it does
  not remove it: whoever writes a page first takes the same stage-2 write
  fault (a VMM-side write would also race the guest and the GPU for the
  contents). `MADV_POPULATE_WRITE` in the VMM makes its own page tables
  writable, which KVM's read fault does not look at.
- *Another kind of mapping*: the driver maps imported and carve-out memory
  with `remap_pfn_range()` (a PFNMAP, which would be prefaulted writable), but
  only those; which path a guest allocation takes is RM's, from the class the
  guest asked for.

The alternative in the kernel is to let GUP-fast accept an order-0 folio with
a NULL mapping when the call neither pins long-term for write nor could be
reaching secretmem (a secretmem folio is unmapped before its mapping is
cleared, and GUP-fast re-checks the PTE after taking the reference). That
helps every such driver mapping, not only prefaults, but it is in `mm/gup.c`,
whose conservatism is deliberate, and it would be much harder to argue safe.

**Reviewed (2026-09-30, against the 7.2.7 source; not booted).** Whether the
flag lets a VMM write what it may only read:

- *Read-only memslot*: the write fault's `__gfn_to_hva_many()` returns
  `KVM_HVA_ERR_RO_BAD`, the fault `RET_PF_EMULATE`, the ioctl `ENOENT`;
  nothing is mapped.
- *A host mapping without write permission* (`PROT_READ`, a file opened
  read-only, an `mprotect`ed range, a sealed memfd): GUP is asked for
  `FOLL_WRITE` and never `FOLL_FORCE`, so it fails (`EFAULT`); a
  `VM_PFNMAP` whose entry is present and read-only gives
  `KVM_PFN_ERR_RO_FAULT` (`ENOENT`). A placement withdrawn and put back
  read-only between the request and the prefault is judged by what is
  mapped at the time of the call, not by what the VMM asked for.
- *CoW, KSM, the zero page*: the write breaks CoW or the merge, as a guest
  write or the VMM's own write to its own mapping would. No page is written.
- *userfaultfd write-protect*: the fault is delivered to the handler, as a
  guest write's would be.
- *Private memory (guest_memfd, TDX, SEV-SNP)*: refused (`EINVAL`) before
  any fault. A guest_memfd-only slot's shared pages take their writability
  from the slot (`kvm_mmu_faultin_pfn_gmem()`), not from the flag.
- *Write-tracked gfns* (shadowed guest page tables, KVMGT): the write fault
  stops at `page_fault_handle_page_track()` (`RET_PF_WRITE_PROTECTED`),
  which the prefault counts as done: nothing is made writable.
- *Nested*: a vCPU in guest mode is not on the TDP MMU, and the ioctl
  refuses it (`EOPNOTSUPP`), flag or not.

So the flag gives a VMM no access its own mapping and the memslot do not
already give it, and gives the guest nothing its own first write would not.
What it does change: the page is dirtied (in the dirty log, and for a
shared file mapping, in the page cache, so it is written back), and with a
dirty ring the entries go to the ring of the vCPU the ioctl runs on, which
only `KVM_RUN` drains; a never-run prefault vCPU over a dirty-logged slot
would overflow its ring (a `WARN_ON_ONCE`, then entries overwritten). That
is upstream's behaviour for a read prefault that maps a writable page too,
and neither VMM logs the window's pages; the draft's `api.rst` now says so.
The risk of applying it is a kernel change never booted: three lines on a
path taken only with the flag set (with `flags` 0 the code is unchanged),
reviewed by reading, not by `tools/testing/selftests/kvm` runs.
