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
