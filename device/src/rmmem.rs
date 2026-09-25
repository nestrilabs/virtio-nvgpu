//! What the backend knows about the memory behind RM handles, and the
//! coherency rewrite that lets a guest cache system memory safely.
//!
//! **The rewrite.** RM allocates system memory with the CPU cache type the
//! client asks for in COHERENCY -- UNCACHED (the default, 0), WRITE_COMBINE or
//! a cached one (mem.c:203-221, 1201-1215) -- and the host maps it that way in
//! the VMM (nv-mmap.c:727-729). A guest cannot rely on that type. The pages are
//! ordinary RAM (vm_insert_page, nv-mmap.c:473-474), and on Intel KVM maps RAM
//! write-back with IPAT set whatever either side's PAT says, under the default
//! IGNORE_GUEST_PAT quirk (spte.c:109-128, vmx.c:7808-7830). So the guest
//! caches memory the GPU reads and writes without snooping, and sees stale
//! semaphores while the GPU sees stale pushbuffers. The GPU side is chosen
//! twice: RM-internal mappings take SYS_NONCOH unless the memory is CACHED
//! (virt_mem_allocator_gm107.c:2815-2822), and a client's own GPU mapping
//! takes it unless NVOS46 asks for CACHE_SNOOP (the memdesc defers the choice
//! to map time, gm107 :432-442, 1282-1291). Making both coherent makes a
//! cached guest view correct: the GPU snoops what the CPU has cached. That is
//! allowed exactly when the platform lets context DMAs snoop
//! (NV2080_CTRL_BUS_INFO_COHERENT_DMA_FLAGS_CTXDMA, the test nvkms-rm.c:226-247
//! makes), which RM derives from PDB_PROP_CL_IS_CHIPSET_IO_COHERENT
//! (kernel_bif.c:751-758). That property is set by default and cleared only by
//! Tegra chipset setup (chipset.c:58, chipset_info.c:950), so on the x86 hosts
//! this backend runs on it always holds; `coherent` is still a switch, for an
//! operator who has to rule this rewrite out.
//!
//! What is rewritten, when the switch is on: COHERENCY of NV01_MEMORY_SYSTEM
//! through RM_ALLOC, VID_HEAP_CONTROL (any function that allocates, located
//! PCI or ANY -- both become NV01_MEMORY_SYSTEM,
//! rmapi_deprecated_vidheapctrl.c:137-142) and ALLOC_MEMORY; NVOS46
//! CACHE_SNOOP on every GPU mapping of system memory; NVOS03 CACHE_SNOOP of a
//! client's context DMA over memory made coherent here. Memory the guest
//! registered by its pages (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, osdesc.rs) is
//! guest RAM like the rest, and its GPU mappings and context DMAs snoop the
//! same way; its COHERENCY is left as asked, because RM takes an OS
//! descriptor of ordinary pages write-back or not at all (`registered`).
//! Never rewritten:
//! display memory (ATTR2 ISO or NISO_DISPLAY). The display reads it through a
//! context DMA whose snoop flag its client chose to match the memory
//! (nvkms-rm.c:2585-2633), and on a platform where display must not snoop,
//! ISO memory is non-IO-coherent by definition (mem_mgr_gb20b.c:186). NVKMS's
//! ALLOC_DEVICE reply, which is what tells a client which model to use, is
//! narrowed to the coherent one instead (nvkms.rs), so display memory a client
//! allocates after reading it is coherent at the source. Every rewritten bit
//! is put back in the reply, so the caller reads what it sent, as it would
//! natively (RM leaves those bits alone).
//!
//! **The records.** Every RM object this backend saw allocated or
//! registered as system memory or as a usermode (doorbell) aperture, by
//! (hClient, handle), carried through DUP_OBJECT and dropped on FREE or with
//! the file its client lives on (which clients those are is the backend's
//! one client set, kept by semsurf.rs for H-1): what a later RM_MAP_MEMORY
//! of it is mapped as on the host (M-1), and whether a GPU mapping of it
//! must snoop.

use std::collections::HashMap;

use abi::ioctl::{
    NV_ESC_RM_ALLOC, NV_ESC_RM_ALLOC_MEMORY, NV_ESC_RM_DUP_OBJECT, NV_ESC_RM_FREE,
    NV_ESC_RM_MAP_MEMORY_DMA, NV_ESC_RM_VID_HEAP_CONTROL,
};

use crate::shm::PgprotKind;

const NV01_CONTEXT_DMA: u32 = 0x02;
/// NV01_ROOT, NV01_ROOT_NON_PRIV, NV01_ROOT_CLIENT: a new RM client, which
/// lives as long as the file it was allocated on (escape.c:471-481 forces
/// every one to _CLIENT; the host frees it when that file closes).
use crate::semsurf::ROOT_CLASSES;
const NV01_MEMORY_SYSTEM: u32 = 0x3e;
/// Memory the guest registered by its pages (osdesc.rs).
use crate::osdesc::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
/// VID_HEAP_CONTROL's NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR, and its hMemory.
const HEAP_ALLOC_OS_DESCRIPTOR: u32 = 27;
const HEAP_OS_DESC_MEMORY: usize = 40;
/// VOLTA..BLACKWELL_USERMODE_A: the doorbell aperture, a slice of BAR0
/// (ADDR_REGMEM, kernel_fifo_gv100.c:371-374), which the host maps UC
/// whatever the caching type (nv-mmap.c:589-596).
const USERMODE_CLASSES: [u32; 5] = [0xc361, 0xc461, 0xc561, 0xc661, 0xc761];

// NVOS32_ATTR / ATTR2 (nvos.h:1069-1092, 1200-1240).
const ATTR_LOCATION_SHIFT: u32 = 25;
const ATTR_LOCATION_VIDMEM: u32 = 0;
const ATTR_COHERENCY_SHIFT: u32 = 29;
const ATTR_COHERENCY_MASK: u32 = 7 << ATTR_COHERENCY_SHIFT;
const ATTR2_NISO_DISPLAY_YES: u32 = 1 << 16;
const ATTR2_ISO_YES: u32 = 1 << 18;
const NVOS32_ALLOC_FLAGS_VIRTUAL: u32 = 0x0008_0000;
// NVOS02_FLAGS (nvos.h:195-204): same COHERENCY values, bits 15:12.
const OS02_LOCATION_SHIFT: u32 = 8;
const OS02_LOCATION_PCI: u32 = 0;
const OS02_COHERENCY_SHIFT: u32 = 12;
const OS02_COHERENCY_MASK: u32 = 0xf << OS02_COHERENCY_SHIFT;
/// NVOS46_FLAGS_CACHE_SNOOP (4:4), ENABLE = 1.
const OS46_CACHE_SNOOP: u32 = 1 << 4;
/// NVOS03_FLAGS_CACHE_SNOOP (28:28), ENABLE = 0, DISABLE = 1.
const OS03_CACHE_SNOOP_DISABLE: u32 = 1 << 28;

/// NVOS32_ATTR_COHERENCY values (== NVOS02_FLAGS_COHERENCY, mem.c:1216-1221).
pub(crate) const COHERENCY_UNCACHED: u8 = 0;
pub(crate) const COHERENCY_WRITE_COMBINE: u8 = 2;
pub(crate) const COHERENCY_WRITE_BACK: u8 = 5;

/// A guest cannot make the table grow without bound: objects past this are
/// not recorded, and their mappings fall back to the old classification.
const MAX_OBJECTS: usize = 1 << 18;

// Parameter layouts (nvos.h), identical in every release profiled.
/// NVOS64 (RM_ALLOC, the 48-byte form the guest sends): the class
/// parameters follow at 48.
const ALLOC_ROOT: usize = 0;
const ALLOC_NEW: usize = 8;
const ALLOC_CLASS: usize = 12;
const ALLOC_STATUS: usize = 40;
const ALLOC_OUTER: usize = 48;
/// NV_MEMORY_ALLOCATION_PARAMS.attr/attr2; NV_CONTEXT_DMA_ALLOCATION_PARAMS.flags/hMemory.
const MEM_ATTR: usize = 24;
const MEM_ATTR2: usize = 28;
const CTXDMA_FLAGS: usize = 4;
const CTXDMA_MEMORY: usize = 8;
/// NVOS32 (VID_HEAP_CONTROL, 184 bytes).
const HEAP_SIZE: usize = 184;
const HEAP_ROOT: usize = 0;
const HEAP_FUNCTION: usize = 8;
const HEAP_STATUS: usize = 20;
/// NVOS02 with its fd (ALLOC_MEMORY, 56 bytes).
const OS02_SIZE: usize = 56;
const OS02_ROOT: usize = 0;
const OS02_NEW: usize = 8;
const OS02_CLASS: usize = 12;
const OS02_FLAGS: usize = 16;
const OS02_STATUS: usize = 40;
const OS02_FD: usize = 48;
/// NVOS46 (MAP_MEMORY_DMA: 56 bytes before 580, 64 since; flags at 32 in both).
const OS46_CLIENT: usize = 0;
const OS46_MEMORY: usize = 12;
const OS46_FLAGS: usize = 32;
/// NVOS55 (DUP_OBJECT).
const DUP_SIZE: usize = 28;
const DUP_CLIENT: usize = 0;
const DUP_OBJECT: usize = 8;
const DUP_CLIENT_SRC: usize = 12;
const DUP_OBJECT_SRC: usize = 16;
const DUP_STATUS: usize = 24;
/// NVOS00 (RM_FREE).
const FREE_SIZE: usize = 16;
const FREE_ROOT: usize = 0;
const FREE_OLD: usize = 8;
const FREE_STATUS: usize = 12;

/// What an RM handle names, as far as mapping it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mem {
    /// NV01_MEMORY_SYSTEM, allocated with this COHERENCY (after any rewrite).
    Sysmem {
        coherency: u8,
        /// ATTR2 ISO or NISO_DISPLAY: left as allocated.
        display: bool,
        /// Made coherent here: allocated write-back by the rewrite, or
        /// registered by its pages, which RM only takes write-back.
        made_coherent: bool,
    },
    /// A usermode aperture: registers.
    Regmem,
}

impl Mem {
    /// The memory type the host gives a mapping of this: UC for registers
    /// (nv-mmap.c:589-596), the allocation's own cache type for system
    /// memory (727-729 via nv_encode_caching; every cached COHERENCY is
    /// NV_MEMORY_CACHED, mem.c:203-221).
    pub(crate) fn pgprot(self) -> PgprotKind {
        match self {
            Mem::Regmem => PgprotKind::Uncached,
            Mem::Sysmem { coherency, .. } => match coherency {
                COHERENCY_UNCACHED => PgprotKind::Uncached,
                COHERENCY_WRITE_COMBINE => PgprotKind::WriteCombine,
                _ => PgprotKind::WriteBack,
            },
        }
    }
}

/// A call `RmMem::before` looked at: the bits it changed, to put back in the
/// reply, and what to record once the reply says whether it succeeded.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    /// (offset, mask, the caller's bits): at most two words per call.
    restore: Vec<(usize, u32, u32)>,
    record: Record,
}

#[derive(Debug, Default)]
enum Record {
    #[default]
    Nothing,
    /// A new object: (hClient, handle) are read from the reply, and `mem`
    /// (or nothing, for any other class) recorded against them. A handle
    /// reused for something else must not keep the old record.
    New {
        client: usize,
        handle: usize,
        status: usize,
        mem: Option<Mem>,
        /// ALLOC_MEMORY arms a mapping on this guest file (escape.c:415-431),
        /// which the mmap that follows arrives on with no RM_MAP_MEMORY.
        armed_on: Option<u32>,
        /// A new client: nothing to record here. The backend's client set
        /// (semsurf.rs) says which file it dies with.
        is_client: bool,
    },
    Dup,
    Free,
}

/// The records and the switch. Owned by the backend, touched only from the
/// queue thread.
#[derive(Debug)]
pub(crate) struct RmMem {
    coherent: bool,
    objects: HashMap<(u32, u32), Mem>,
    /// Guest file handle -> what an ALLOC_MEMORY armed on it.
    armed: HashMap<u32, Mem>,
    full_warned: bool,
}

impl Default for RmMem {
    fn default() -> Self {
        Self {
            coherent: true,
            objects: HashMap::new(),
            armed: HashMap::new(),
            full_warned: false,
        }
    }
}

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn wr32(b: &mut [u8], off: usize, v: u32) {
    if let Some(s) = b.get_mut(off..off + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

impl Pending {
    /// Replace `mask` bits of the word at `off` with `bits`, remembering the
    /// caller's, if they differ.
    fn rewrite(&mut self, params: &mut [u8], off: usize, mask: u32, bits: u32) {
        let Some(w) = rd32(params, off) else { return };
        if w & mask == bits & mask {
            return;
        }
        self.restore.push((off, mask, w & mask));
        wr32(params, off, (w & !mask) | (bits & mask));
    }
}

impl RmMem {
    /// The escapes `before` and `after` want to see.
    pub(crate) fn watches(escape: u32) -> bool {
        matches!(
            escape,
            NV_ESC_RM_ALLOC
                | NV_ESC_RM_ALLOC_MEMORY
                | NV_ESC_RM_VID_HEAP_CONTROL
                | NV_ESC_RM_MAP_MEMORY_DMA
                | NV_ESC_RM_DUP_OBJECT
                | NV_ESC_RM_FREE
        )
    }

    /// Whether guest system memory is made coherent (see the module docs).
    pub(crate) fn set_coherent(&mut self, on: bool) {
        self.coherent = on;
    }

    pub(crate) fn coherent(&self) -> bool {
        self.coherent
    }

    /// What (hClient, handle) is, if this backend saw it made.
    pub(crate) fn lookup(&self, client: u32, handle: u32) -> Option<Mem> {
        self.objects.get(&(client, handle)).copied()
    }

    /// What an ALLOC_MEMORY armed on guest file `handle`, if anything.
    pub(crate) fn armed(&self, handle: u32) -> Option<Mem> {
        self.armed.get(&handle).copied()
    }

    /// A guest file closed: nothing can be mapped through it any more, and
    /// `gone`, the clients allocated on it, are gone on the host with all
    /// they held. The host frees them with no RM_FREE for us to see; without
    /// this a guest that runs one process after another would fill the table
    /// with objects long gone. `gone` comes from the backend's client set
    /// ([`crate::semsurf::SemsurfPolicy::forget_handle`]).
    pub(crate) fn forget_fd(&mut self, handle: u32, gone: &[u32]) {
        self.armed.remove(&handle);
        if gone.is_empty() {
            return;
        }
        self.objects.retain(|(c, _), _| !gone.contains(c));
    }

    /// Session reset: every client is gone with the files that held them.
    pub(crate) fn clear(&mut self) {
        self.objects.clear();
        self.armed.clear();
    }

    /// COHERENCY for a new system-memory allocation asking for `asked`:
    /// write-back when this backend makes guest memory coherent and it is not
    /// display memory, otherwise what was asked.
    fn coherency_for(&self, asked: u8, display: bool) -> (u8, bool) {
        let cached = !matches!(asked, COHERENCY_UNCACHED | COHERENCY_WRITE_COMBINE);
        if self.coherent && !display && !cached {
            (COHERENCY_WRITE_BACK, true)
        } else {
            (asked, false)
        }
    }

    /// Look at, and where needed rewrite, the parameters of an RM escape on
    /// its way to the host, issued on guest file `file`. `params` is the
    /// whole block the host will see (for RM_ALLOC: the 48-byte NVOS64 and
    /// the class parameters after it).
    pub(crate) fn before(&self, escape: u32, params: &mut [u8]) -> Pending {
        let mut p = Pending::default();
        match escape {
            NV_ESC_RM_ALLOC if params.len() >= ALLOC_OUTER => {
                let class = rd32(params, ALLOC_CLASS).unwrap_or(0);
                let mem = if class == NV01_MEMORY_SYSTEM {
                    self.alloc_sysmem(
                        &mut p,
                        params,
                        ALLOC_OUTER + MEM_ATTR,
                        ALLOC_OUTER + MEM_ATTR2,
                    )
                } else if class == NV01_MEMORY_SYSTEM_OS_DESCRIPTOR {
                    Some(self.registered())
                } else if USERMODE_CLASSES.contains(&class) {
                    Some(Mem::Regmem)
                } else {
                    if class == NV01_CONTEXT_DMA {
                        self.ctxdma(&mut p, params);
                    }
                    None
                };
                p.record = Record::New {
                    client: ALLOC_ROOT,
                    handle: ALLOC_NEW,
                    status: ALLOC_STATUS,
                    mem,
                    armed_on: None,
                    is_client: ROOT_CLASSES.contains(&class),
                };
            }
            NV_ESC_RM_VID_HEAP_CONTROL if params.len() >= HEAP_SIZE => {
                // (hMemory, flags, attr, attr2) of the three allocating
                // functions: ALLOC_SIZE, ALLOC_TILED_PITCH_HEIGHT,
                // ALLOC_SIZE_RANGE (nvos.h:690-800).
                let at = match rd32(params, HEAP_FUNCTION) {
                    Some(2) => Some((44, 52, 56, 144)),
                    Some(6) => Some((44, 52, 64, 144)),
                    Some(14) => Some((44, 52, 56, 136)),
                    _ => None,
                };
                if rd32(params, HEAP_FUNCTION) == Some(HEAP_ALLOC_OS_DESCRIPTOR) {
                    p.record = Record::New {
                        client: HEAP_ROOT,
                        handle: HEAP_OS_DESC_MEMORY,
                        status: HEAP_STATUS,
                        mem: Some(self.registered()),
                        armed_on: None,
                        is_client: false,
                    };
                } else if let Some((handle, flags, attr, attr2)) = at {
                    let a = rd32(params, attr).unwrap_or(0);
                    let virt = rd32(params, flags).unwrap_or(0) & NVOS32_ALLOC_FLAGS_VIRTUAL != 0;
                    let vidmem = (a >> ATTR_LOCATION_SHIFT) & 3 == ATTR_LOCATION_VIDMEM;
                    let mem = if virt || vidmem {
                        None
                    } else {
                        self.alloc_sysmem(&mut p, params, attr, attr2)
                    };
                    p.record = Record::New {
                        client: HEAP_ROOT,
                        handle,
                        status: HEAP_STATUS,
                        mem,
                        armed_on: None,
                        is_client: false,
                    };
                }
            }
            NV_ESC_RM_ALLOC_MEMORY if params.len() >= OS02_SIZE => {
                let class = rd32(params, OS02_CLASS).unwrap_or(0);
                let flags = rd32(params, OS02_FLAGS).unwrap_or(0);
                let mem = if class == NV01_MEMORY_SYSTEM
                    && (flags >> OS02_LOCATION_SHIFT) & 0xf == OS02_LOCATION_PCI
                {
                    let asked = ((flags & OS02_COHERENCY_MASK) >> OS02_COHERENCY_SHIFT) as u8;
                    // NVOS02 has no display attributes.
                    let (coherency, rewritten) = self.coherency_for(asked, false);
                    if rewritten {
                        p.rewrite(
                            params,
                            OS02_FLAGS,
                            OS02_COHERENCY_MASK,
                            (coherency as u32) << OS02_COHERENCY_SHIFT,
                        );
                    }
                    Some(Mem::Sysmem {
                        coherency,
                        display: false,
                        made_coherent: rewritten,
                    })
                } else if class == NV01_MEMORY_SYSTEM_OS_DESCRIPTOR {
                    Some(self.registered())
                } else {
                    None
                };
                // Only NV01_MEMORY_SYSTEM arms a mapping (escape.c:415-431).
                let fd = rd32(params, OS02_FD)
                    .filter(|&fd| fd as i32 >= 0 && class == NV01_MEMORY_SYSTEM);
                p.record = Record::New {
                    client: OS02_ROOT,
                    handle: OS02_NEW,
                    status: OS02_STATUS,
                    mem,
                    armed_on: fd,
                    is_client: false,
                };
            }
            NV_ESC_RM_MAP_MEMORY_DMA if params.len() >= OS46_FLAGS + 4 => {
                // A GPU mapping of system memory snoops, so what the guest
                // has cached is what the GPU reads. Only system memory:
                // vidmem ignores the flag (gm107 :429-430), and leaving it
                // alone keeps those mappings byte-identical to native.
                let client = rd32(params, OS46_CLIENT).unwrap_or(0);
                let mem = rd32(params, OS46_MEMORY).unwrap_or(0);
                if self.coherent && matches!(self.lookup(client, mem), Some(Mem::Sysmem { .. })) {
                    p.rewrite(params, OS46_FLAGS, OS46_CACHE_SNOOP, OS46_CACHE_SNOOP);
                }
            }
            NV_ESC_RM_DUP_OBJECT if params.len() >= DUP_SIZE => p.record = Record::Dup,
            NV_ESC_RM_FREE if params.len() >= FREE_SIZE => p.record = Record::Free,
            _ => {}
        }
        p
    }

    /// Memory the guest registered by its pages. RM takes an OS descriptor
    /// of ordinary pages -- and the backend only ever hands it those, its
    /// own mapping of guest RAM -- write-back or not at all: an UNCACHED or
    /// WRITE_COMBINE request is NV_ERR_INVALID_FLAGS
    /// (osCreateOsDescriptorFromPageArray, osmemdesc.c:353-358), as it is
    /// natively, so there is no coherency to rewrite, and none would make
    /// the call succeed where the native one fails. What is left is the
    /// GPU's side, chosen at each mapping as for any system memory: every
    /// GPU mapping of it snoops, and so does a context DMA over it.
    fn registered(&self) -> Mem {
        Mem::Sysmem {
            coherency: COHERENCY_WRITE_BACK,
            display: false,
            made_coherent: self.coherent,
        }
    }

    /// NV01_MEMORY_SYSTEM through RM_ALLOC or VID_HEAP_CONTROL: rewrite
    /// COHERENCY in the attr word at `attr` if it should be, and say what the
    /// object will be.
    fn alloc_sysmem(
        &self,
        p: &mut Pending,
        params: &mut [u8],
        attr: usize,
        attr2: usize,
    ) -> Option<Mem> {
        let a = rd32(params, attr)?;
        let a2 = rd32(params, attr2)?;
        let display = a2 & (ATTR2_ISO_YES | ATTR2_NISO_DISPLAY_YES) != 0;
        let asked = ((a & ATTR_COHERENCY_MASK) >> ATTR_COHERENCY_SHIFT) as u8;
        let (coherency, rewritten) = self.coherency_for(asked, display);
        if rewritten {
            p.rewrite(
                params,
                attr,
                ATTR_COHERENCY_MASK,
                (coherency as u32) << ATTR_COHERENCY_SHIFT,
            );
        }
        Some(Mem::Sysmem {
            coherency,
            display,
            made_coherent: rewritten,
        })
    }

    /// A client's own context DMA over memory made coherent here snoops. It
    /// chose NVOS03 CACHE_SNOOP to match the memory it thinks it has.
    fn ctxdma(&self, p: &mut Pending, params: &mut [u8]) {
        let client = rd32(params, ALLOC_ROOT).unwrap_or(0);
        let Some(mem) = rd32(params, ALLOC_OUTER + CTXDMA_MEMORY) else {
            return;
        };
        if let Some(Mem::Sysmem {
            made_coherent: true,
            ..
        }) = self.lookup(client, mem)
        {
            p.rewrite(
                params,
                ALLOC_OUTER + CTXDMA_FLAGS,
                OS03_CACHE_SNOOP_DISABLE,
                0,
            );
        }
    }

    /// The reply to a call `before` looked at: put the caller's bits back and
    /// record what succeeded. `reply` has the same layout as the parameters.
    pub(crate) fn after(&mut self, p: Pending, reply: &mut [u8]) {
        for (off, mask, bits) in p.restore {
            if let Some(w) = rd32(reply, off) {
                wr32(reply, off, (w & !mask) | bits);
            }
        }
        match p.record {
            Record::Nothing => {}
            Record::New {
                client,
                handle,
                status,
                mem,
                armed_on,
                is_client,
            } => {
                if rd32(reply, status) != Some(0) {
                    return;
                }
                let (Some(c), Some(h)) = (rd32(reply, client), rd32(reply, handle)) else {
                    return;
                };
                if is_client {
                    // A root allocation: the new client itself holds no
                    // memory, and semsurf.rs keeps the client set.
                    return;
                }
                self.set(c, h, mem);
                if let (Some(fd), Some(m)) = (armed_on, mem) {
                    self.armed.insert(fd, m);
                }
            }
            Record::Dup => {
                if rd32(reply, DUP_STATUS) != Some(0) {
                    return;
                }
                let get = |o| rd32(reply, o).unwrap_or(0);
                let src = self.lookup(get(DUP_CLIENT_SRC), get(DUP_OBJECT_SRC));
                self.set(get(DUP_CLIENT), get(DUP_OBJECT), src);
            }
            Record::Free => {
                if rd32(reply, FREE_STATUS) != Some(0) {
                    return;
                }
                let (root, old) = (
                    rd32(reply, FREE_ROOT).unwrap_or(0),
                    rd32(reply, FREE_OLD).unwrap_or(0),
                );
                if old == root {
                    // The client, and with it everything it held.
                    self.objects.retain(|&(c, _), _| c != root);
                } else {
                    self.objects.remove(&(root, old));
                }
            }
        }
    }

    fn set(&mut self, client: u32, handle: u32, mem: Option<Mem>) {
        match mem {
            None => {
                self.objects.remove(&(client, handle));
            }
            Some(m) => {
                if self.objects.len() >= MAX_OBJECTS
                    && !self.objects.contains_key(&(client, handle))
                {
                    if !self.full_warned {
                        self.full_warned = true;
                        log::warn!(
                            "RM memory records: {MAX_OBJECTS} objects; later ones are not \
                             recorded and map with the old write-combining guess"
                        );
                    }
                    return;
                }
                self.objects.insert((client, handle), m);
            }
        }
    }
}

/// Say once, at start-up, what an Intel host means for guest memory types.
///
/// On VMX, KVM maps guest RAM write-back with IPAT, ignoring guest PAT,
/// unless the VMM disables KVM_X86_QUIRK_IGNORE_GUEST_PAT (vmx.c:7808-7830;
/// the quirk is only offered with self-snoop, vmx.c:8785-8797). System memory
/// the host allocated non-coherent is then cached by the guest while the GPU
/// does not snoop it. This backend makes guest system memory coherent, so
/// what remains exposed is display memory a client allocated non-coherent
/// (and everything, with `--keep-guest-coherency`). AMD's NPT honours guest
/// PAT and needs none of this.
pub fn warn_if_guest_pat_ignored(coherent: bool) {
    let intel = std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| {
            s.lines()
                .find(|l| l.starts_with("vendor_id"))
                .is_some_and(|l| l.contains("GenuineIntel"))
        })
        .unwrap_or(false);
    if !intel {
        return;
    }
    if coherent {
        log::warn!(
            "Intel host: KVM ignores guest PAT for guest RAM unless the VMM disables \
             KVM_X86_QUIRK_IGNORE_GUEST_PAT (KVM_CAP_DISABLE_QUIRKS2). Guest system memory \
             is made GPU-coherent here, so only display memory a client allocated \
             non-coherent is exposed; disable the quirk if a guest shows display corruption"
        );
    } else {
        log::warn!(
            "Intel host with guest coherency left as asked: KVM caches non-coherent GPU \
             system memory in the guest unless the VMM disables \
             KVM_X86_QUIRK_IGNORE_GUEST_PAT (KVM_CAP_DISABLE_QUIRKS2); expect stale GPU \
             data in the guest otherwise"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: u32 = 0xc1d0_0001;

    fn put(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// An RM_ALLOC of NV01_MEMORY_SYSTEM as the guest sends it.
    fn sysmem_alloc(handle: u32, coherency: u32, attr2: u32) -> Vec<u8> {
        let mut b = vec![0u8; ALLOC_OUTER + 128];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, handle);
        put(&mut b, ALLOC_CLASS, NV01_MEMORY_SYSTEM);
        // LOCATION_PCI, PHYSICALITY_NONCONTIGUOUS, some low attr bits the
        // rewrite must leave alone.
        put(
            &mut b,
            ALLOC_OUTER + MEM_ATTR,
            (coherency << ATTR_COHERENCY_SHIFT) | (1 << ATTR_LOCATION_SHIFT) | 0x15,
        );
        put(&mut b, ALLOC_OUTER + MEM_ATTR2, attr2);
        b
    }

    fn attr(b: &[u8]) -> u32 {
        rd32(b, ALLOC_OUTER + MEM_ATTR).unwrap()
    }

    /// Run a call through `before`, a host that succeeds, and `after`, and
    /// return (what the host saw, what the guest gets back).
    fn run(
        m: &mut RmMem,
        escape: u32,
        params: &[u8],
        status_at: Option<usize>,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut host = params.to_vec();
        let p = m.before(escape, &mut host);
        let seen = host.clone();
        if let Some(s) = status_at {
            put(&mut host, s, 0);
        }
        m.after(p, &mut host);
        (seen, host)
    }

    #[test]
    fn uncached_system_memory_is_allocated_write_back_and_the_guest_reads_back_what_it_asked() {
        let mut m = RmMem::default();
        for asked in [0u32, 2] {
            let req = sysmem_alloc(0x100 + asked, asked, 0);
            let (seen, back) = run(&mut m, NV_ESC_RM_ALLOC, &req, Some(ALLOC_STATUS));
            assert_eq!(
                attr(&seen) >> ATTR_COHERENCY_SHIFT,
                5,
                "host allocates WRITE_BACK"
            );
            assert_eq!(
                attr(&seen) & !ATTR_COHERENCY_MASK,
                attr(&req) & !ATTR_COHERENCY_MASK
            );
            assert_eq!(attr(&back), attr(&req), "caller reads back its own attr");
            assert_eq!(
                m.lookup(CLIENT, 0x100 + asked),
                Some(Mem::Sysmem {
                    coherency: 5,
                    display: false,
                    made_coherent: true
                })
            );
            assert_eq!(
                m.lookup(CLIENT, 0x100 + asked).unwrap().pgprot(),
                PgprotKind::WriteBack
            );
        }
    }

    #[test]
    fn display_memory_keeps_the_coherency_its_client_chose() {
        let mut m = RmMem::default();
        for (h, attr2) in [(1, ATTR2_ISO_YES), (2, ATTR2_NISO_DISPLAY_YES)] {
            let req = sysmem_alloc(h, 2, attr2);
            let (seen, _) = run(&mut m, NV_ESC_RM_ALLOC, &req, Some(ALLOC_STATUS));
            assert_eq!(seen, req, "display allocation reaches the host untouched");
            assert_eq!(
                m.lookup(CLIENT, h).unwrap().pgprot(),
                PgprotKind::WriteCombine
            );
        }
    }

    #[test]
    fn with_the_rewrite_off_memory_is_recorded_as_asked() {
        let mut m = RmMem::default();
        m.set_coherent(false);
        let req = sysmem_alloc(7, 0, 0);
        let (seen, _) = run(&mut m, NV_ESC_RM_ALLOC, &req, Some(ALLOC_STATUS));
        assert_eq!(seen, req);
        assert_eq!(m.lookup(CLIENT, 7).unwrap().pgprot(), PgprotKind::Uncached);
    }

    #[test]
    fn a_failed_allocation_records_nothing() {
        let mut m = RmMem::default();
        let req = sysmem_alloc(9, 0, 0);
        let mut host = req.clone();
        let p = m.before(NV_ESC_RM_ALLOC, &mut host);
        put(&mut host, ALLOC_STATUS, 0x1f);
        m.after(p, &mut host);
        assert_eq!(m.lookup(CLIENT, 9), None);
        assert_eq!(
            attr(&host),
            attr(&req),
            "the caller's bits come back on failure too"
        );
    }

    #[test]
    fn vid_heap_control_rewrites_pci_and_any_but_not_vidmem_or_virtual() {
        let mut m = RmMem::default();
        // (function, attr offset, attr2 offset)
        for (func, at, _a2) in [(2u32, 56usize, 144usize), (6, 64, 144), (14, 56, 136)] {
            for (loc, virt, want_rewrite) in [
                (1u32, false, true),
                (3, false, true),
                (0, false, false),
                (1, true, false),
            ] {
                let mut b = vec![0u8; HEAP_SIZE];
                put(&mut b, HEAP_ROOT, CLIENT);
                put(&mut b, HEAP_FUNCTION, func);
                put(&mut b, 44, 0x500 + func);
                put(
                    &mut b,
                    52,
                    if virt { NVOS32_ALLOC_FLAGS_VIRTUAL } else { 0 },
                );
                put(&mut b, at, loc << ATTR_LOCATION_SHIFT);
                let (seen, back) = run(&mut m, NV_ESC_RM_VID_HEAP_CONTROL, &b, Some(HEAP_STATUS));
                let coh = rd32(&seen, at).unwrap() >> ATTR_COHERENCY_SHIFT;
                assert_eq!(
                    coh == 5,
                    want_rewrite,
                    "function {func} location {loc} virtual {virt}"
                );
                assert_eq!(rd32(&back, at), rd32(&b, at));
                assert_eq!(
                    m.lookup(CLIENT, 0x500 + func).is_some(),
                    want_rewrite,
                    "function {func} location {loc} virtual {virt}"
                );
            }
        }
    }

    #[test]
    fn alloc_memory_is_rewritten_and_arms_the_file_it_names() {
        let mut m = RmMem::default();
        let mut b = vec![0u8; OS02_SIZE];
        put(&mut b, OS02_ROOT, CLIENT);
        put(&mut b, OS02_NEW, 0x77);
        put(&mut b, OS02_CLASS, NV01_MEMORY_SYSTEM);
        put(
            &mut b,
            OS02_FLAGS,
            0x8000_0000 | (2 << OS02_COHERENCY_SHIFT),
        );
        put(&mut b, OS02_FD, 42);
        let (seen, back) = run(&mut m, NV_ESC_RM_ALLOC_MEMORY, &b, Some(OS02_STATUS));
        assert_eq!(
            rd32(&seen, OS02_FLAGS),
            Some(0x8000_0000 | (5 << OS02_COHERENCY_SHIFT))
        );
        assert_eq!(rd32(&back, OS02_FLAGS), rd32(&b, OS02_FLAGS));
        assert_eq!(m.armed(42).map(Mem::pgprot), Some(PgprotKind::WriteBack));
        m.forget_fd(42, &[]);
        assert_eq!(m.armed(42), None);
    }

    fn map_dma(mem: u32, flags: u32) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        put(&mut b, OS46_CLIENT, CLIENT);
        put(&mut b, OS46_MEMORY, mem);
        put(&mut b, OS46_FLAGS, flags);
        b
    }

    #[test]
    fn a_gpu_mapping_of_system_memory_snoops_and_of_anything_else_is_untouched() {
        let mut m = RmMem::default();
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x10, 0, 0),
            Some(ALLOC_STATUS),
        );
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x11, 2, ATTR2_ISO_YES),
            Some(ALLOC_STATUS),
        );
        for mem in [0x10, 0x11] {
            let req = map_dma(mem, 0x0000_0001);
            let (seen, back) = run(&mut m, NV_ESC_RM_MAP_MEMORY_DMA, &req, None);
            assert_eq!(
                rd32(&seen, OS46_FLAGS),
                Some(0x11),
                "memory {mem:#x} snoops"
            );
            assert_eq!(rd32(&back, OS46_FLAGS), Some(1));
        }
        let vidmem = map_dma(0x99, 1);
        let (seen, _) = run(&mut m, NV_ESC_RM_MAP_MEMORY_DMA, &vidmem, None);
        assert_eq!(seen, vidmem);
    }

    #[test]
    fn a_context_dma_over_rewritten_memory_snoops() {
        let mut m = RmMem::default();
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x20, 0, 0),
            Some(ALLOC_STATUS),
        );
        let mut b = vec![0u8; ALLOC_OUTER + 32];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, 0x21);
        put(&mut b, ALLOC_CLASS, NV01_CONTEXT_DMA);
        put(
            &mut b,
            ALLOC_OUTER + CTXDMA_FLAGS,
            OS03_CACHE_SNOOP_DISABLE | 0x3,
        );
        put(&mut b, ALLOC_OUTER + CTXDMA_MEMORY, 0x20);
        let (seen, back) = run(&mut m, NV_ESC_RM_ALLOC, &b, Some(ALLOC_STATUS));
        assert_eq!(rd32(&seen, ALLOC_OUTER + CTXDMA_FLAGS), Some(0x3));
        assert_eq!(
            rd32(&back, ALLOC_OUTER + CTXDMA_FLAGS),
            Some(OS03_CACHE_SNOOP_DISABLE | 0x3)
        );
    }

    /// Memory registered by its pages, through each of the three calls.
    fn registered(m: &mut RmMem) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        // RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, attr UNCACHED.
        let mut b = vec![0u8; ALLOC_OUTER + 40];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, 0x71);
        put(&mut b, ALLOC_CLASS, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
        out.push((b.clone(), run(m, NV_ESC_RM_ALLOC, &b, Some(ALLOC_STATUS)).0));
        // VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR: hMemory at 40.
        let mut b = vec![0u8; HEAP_SIZE];
        put(&mut b, HEAP_ROOT, CLIENT);
        put(&mut b, HEAP_FUNCTION, HEAP_ALLOC_OS_DESCRIPTOR);
        put(&mut b, HEAP_OS_DESC_MEMORY, 0x72);
        out.push((
            b.clone(),
            run(m, NV_ESC_RM_VID_HEAP_CONTROL, &b, Some(HEAP_STATUS)).0,
        ));
        // ALLOC_MEMORY, COHERENCY UNCACHED, with a descriptor.
        let mut b = vec![0u8; OS02_SIZE];
        put(&mut b, OS02_ROOT, CLIENT);
        put(&mut b, OS02_NEW, 0x73);
        put(&mut b, OS02_CLASS, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
        put(&mut b, OS02_FD, 42);
        out.push((
            b.clone(),
            run(m, NV_ESC_RM_ALLOC_MEMORY, &b, Some(OS02_STATUS)).0,
        ));
        out
    }

    /// RM takes an OS descriptor of ordinary pages write-back or not at all,
    /// so its coherency is left as asked; but it is guest RAM, which the
    /// guest caches, and every GPU mapping of it snoops.
    #[test]
    fn registered_memory_is_write_back_system_memory_and_every_gpu_mapping_of_it_snoops() {
        let mut m = RmMem::default();
        for (req, seen) in registered(&mut m) {
            assert_eq!(seen, req, "the registration reaches RM untouched");
        }
        for h in [0x71, 0x72, 0x73] {
            assert_eq!(
                m.lookup(CLIENT, h),
                Some(Mem::Sysmem {
                    coherency: COHERENCY_WRITE_BACK,
                    display: false,
                    made_coherent: true
                }),
                "{h:#x}"
            );
            let req = map_dma(h, 0x1);
            let (seen, back) = run(&mut m, NV_ESC_RM_MAP_MEMORY_DMA, &req, None);
            assert_eq!(rd32(&seen, OS46_FLAGS), Some(0x11), "{h:#x} snoops");
            assert_eq!(rd32(&back, OS46_FLAGS), Some(0x1));
        }
        assert_eq!(m.armed(42), None, "RM arms no mapping for an OS descriptor");
        // A context DMA over it snoops too.
        let mut b = vec![0u8; ALLOC_OUTER + 32];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, 0x74);
        put(&mut b, ALLOC_CLASS, NV01_CONTEXT_DMA);
        put(&mut b, ALLOC_OUTER + CTXDMA_FLAGS, OS03_CACHE_SNOOP_DISABLE);
        put(&mut b, ALLOC_OUTER + CTXDMA_MEMORY, 0x71);
        let (seen, _) = run(&mut m, NV_ESC_RM_ALLOC, &b, Some(ALLOC_STATUS));
        assert_eq!(rd32(&seen, ALLOC_OUTER + CTXDMA_FLAGS), Some(0));
    }

    #[test]
    fn with_the_rewrite_off_registered_memory_is_mapped_as_asked() {
        let mut m = RmMem::default();
        m.set_coherent(false);
        registered(&mut m);
        let req = map_dma(0x71, 0x1);
        let (seen, _) = run(&mut m, NV_ESC_RM_MAP_MEMORY_DMA, &req, None);
        assert_eq!(seen, req);
    }

    #[test]
    fn usermode_apertures_are_registers() {
        let mut m = RmMem::default();
        let mut b = vec![0u8; ALLOC_OUTER];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, 0x30);
        put(&mut b, ALLOC_CLASS, 0xc461);
        run(&mut m, NV_ESC_RM_ALLOC, &b, Some(ALLOC_STATUS));
        assert_eq!(
            m.lookup(CLIENT, 0x30).map(Mem::pgprot),
            Some(PgprotKind::Uncached)
        );
    }

    #[test]
    fn dup_carries_the_record_and_free_drops_it() {
        let mut m = RmMem::default();
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x40, 0, 0),
            Some(ALLOC_STATUS),
        );
        let mut d = vec![0u8; DUP_SIZE];
        put(&mut d, DUP_CLIENT, 0xbeef);
        put(&mut d, DUP_OBJECT, 0x41);
        put(&mut d, DUP_CLIENT_SRC, CLIENT);
        put(&mut d, DUP_OBJECT_SRC, 0x40);
        run(&mut m, NV_ESC_RM_DUP_OBJECT, &d, Some(DUP_STATUS));
        assert_eq!(m.lookup(0xbeef, 0x41), m.lookup(CLIENT, 0x40));
        assert!(m.lookup(0xbeef, 0x41).is_some());

        let mut f = vec![0u8; FREE_SIZE];
        put(&mut f, FREE_ROOT, CLIENT);
        put(&mut f, FREE_OLD, 0x40);
        run(&mut m, NV_ESC_RM_FREE, &f, Some(FREE_STATUS));
        assert_eq!(m.lookup(CLIENT, 0x40), None);
        assert!(
            m.lookup(0xbeef, 0x41).is_some(),
            "the duplicate is its own object"
        );

        put(&mut f, FREE_ROOT, 0xbeef);
        put(&mut f, FREE_OLD, 0xbeef);
        run(&mut m, NV_ESC_RM_FREE, &f, Some(FREE_STATUS));
        assert_eq!(
            m.lookup(0xbeef, 0x41),
            None,
            "freeing a client drops what it held"
        );
    }

    #[test]
    fn closing_the_file_a_client_was_allocated_on_drops_what_it_held() {
        let mut m = RmMem::default();
        let mut root = vec![0u8; ALLOC_OUTER];
        put(&mut root, ALLOC_NEW, CLIENT);
        put(&mut root, ALLOC_CLASS, 0x41);
        let mut host = root.clone();
        let p = m.before(NV_ESC_RM_ALLOC, &mut host);
        m.after(p, &mut host);
        assert_eq!(m.lookup(CLIENT, CLIENT), None, "a client is not memory");
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x60, 0, 0),
            Some(ALLOC_STATUS),
        );
        assert!(m.lookup(CLIENT, 0x60).is_some());
        m.forget_fd(8, &[]);
        assert!(m.lookup(CLIENT, 0x60).is_some(), "another file's close");
        m.forget_fd(7, &[CLIENT]);
        assert_eq!(m.lookup(CLIENT, 0x60), None);
    }

    #[test]
    fn a_handle_reused_for_another_class_loses_its_old_record() {
        let mut m = RmMem::default();
        run(
            &mut m,
            NV_ESC_RM_ALLOC,
            &sysmem_alloc(0x50, 0, 0),
            Some(ALLOC_STATUS),
        );
        let mut b = vec![0u8; ALLOC_OUTER + 8];
        put(&mut b, ALLOC_ROOT, CLIENT);
        put(&mut b, ALLOC_NEW, 0x50);
        put(&mut b, ALLOC_CLASS, 0x40); // NV01_MEMORY_LOCAL_USER
        run(&mut m, NV_ESC_RM_ALLOC, &b, Some(ALLOC_STATUS));
        assert_eq!(m.lookup(CLIENT, 0x50), None);
    }
}
