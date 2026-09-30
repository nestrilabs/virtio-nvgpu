// SPDX-License-Identifier: Apache-2.0
//! Memory the guest already has, registered with RM by its guest-physical
//! pages.
//!
//! RM registers existing memory by CPU address: NV01_MEMORY_SYSTEM_OS_DESCRIPTOR
//! (0x71) through NV_ESC_RM_ALLOC_MEMORY or NV_ESC_RM_ALLOC, or
//! NV_ESC_RM_VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR. The calling process is
//! this one, so a guest address would name the VMM's memory, and the GPU would
//! read and write it; `guestptr::rm_escape` refuses all three when they come
//! with an address. That is how cuMemHostRegister, VK_EXT_external_memory_host
//! and cuCtxCreate's own 2 MiB buffer register memory.
//!
//! A guest that says it can ([`BCAP_OS_DESC`]) sends the pages instead. Its
//! driver pins the caller's range, as RM would -- for writing unless the call
//! asks for read-only memory -- and sends the guest-physical page list
//! ([`DEEP_PAGE_LIST`]) with the call, the address left as the caller wrote
//! it. This module:
//!
//! - **Checks the call.** One of the three shapes, the user-virtual-address
//!   descriptor type and no other (a physical address, a page array, a
//!   dma-buf by descriptor are refused whatever the page list), and a list
//!   that covers exactly the pages RM would pin -- from the one holding the
//!   address to the one holding its last byte, `limit + 1` bytes on -- with
//!   the writability the call asks for.
//! - **Checks every page is guest RAM.** Each run is looked up in the
//!   vhost-user memory table ([`GuestRam`]); a page in no region refuses the
//!   call. The table holds RAM only: the window and the UVM aperture are
//!   device memory the VMM never puts in it.
//! - **Hands RM an address of its own.** Pages contiguous in one region are
//!   given as the backend's own mapping of that region (the one the
//!   virtqueues are read through). Anything else is mapped contiguously: a
//!   `PROT_NONE` reservation, then each run `MAP_FIXED | MAP_SHARED` from the
//!   region's memfd at its offset, read-only unless the call writes. The
//!   caller's offset inside its first page is kept, so RM sees the
//!   alignment the caller chose (and refuses an unaligned one, as it does
//!   natively: RmCreateOsDescriptor, escape.c:144-149).
//!
//! RM pins exactly the guest's pages. It keeps nothing of the address
//! afterwards: ALLOC_MEMORY and VID_HEAP_CONTROL pin in the escape layer and
//! hand RM a page array (escape.c:134-203), the OS descriptor's recorded CPU
//! mapping is that page array's NULL (os_desc_mem.c:140-145, 200-205), and a
//! CPU mapping of the object is refused (mapping_cpu.c:180-199); RM_ALLOC of
//! the virtual-address type is NV_ERR_NOT_SUPPORTED in every release measured
//! (osmemdesc.c:135-137, 535 through 610), as it is natively. The backend's
//! range stays mapped until RM has let go all the same, so a release that
//! did keep it would find it there.
//!
//! **Lifetime.** A registration ends only when nothing in the host kernel
//! can still reach its pages:
//!
//! - **RM objects that hold it**: the object the call made, keyed by
//!   (hClient, hObject); every duplicate of it (NV_ESC_RM_DUP_OBJECT); and
//!   every object RM made over one of those and keeps a duplicate of its
//!   own for (`holding_fields`: a semaphore surface, and a memory mapper
//!   over one), and every duplicate such a surface hands back into the
//!   caller's client (`SEMSURF_REF_MEMORY`). Each lets go when RM frees it, its parent, or its client (a
//!   successful NV_ESC_RM_FREE naming any of them), when the file its
//!   client was allocated on closes (the backend frees the client itself
//!   first, so RM has let go before the guest is told), or with the
//!   session. Freeing an ancestor further up (a device whose subdevice holds
//!   it) is not seen, and the object then counts until its client goes:
//!   late, never early.
//! - **nvidia-uvm external mappings** of any of those objects
//!   (MAP_EXTERNAL_ALLOCATION): UVM duplicates the memory into a client of
//!   its own for as long as the mapping lasts, whatever becomes of the
//!   guest's handle. One holds it -- even when UVM answered with a failure,
//!   which it can after making the mappings -- until UNMAP_EXTERNAL has
//!   covered it on every GPU it was mapped on, UVM_FREE takes the external
//!   range it lies in (every CREATE_EXTERNAL_RANGE is recorded, up to a
//!   bound), or its UVM file closes. A file's last reference may be dropped
//!   later than its close (the event pump holds a duplicate), so on close
//!   the backend takes the mappings down itself first, on its own
//!   descriptor: UVM_FREE of each recorded range holding one, then
//!   UNMAP_EXTERNAL of what is left. What will not come down stays held
//!   until the session ends.
//! - **Refused**: what would hand one of those objects to a holder the
//!   backend cannot follow. RM's export to a descriptor
//!   (NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT(S)_TO_FD) duplicates it into
//!   RM's export client until that file closes, and every import -- another
//!   RM client, NVKMS's REGISTER_SURFACE, nvidia-drm's GEM import through
//!   NVKMS -- makes a duplicate of its own; NV00E0_CTRL_CMD_EXPORT_MEM
//!   duplicates it into an internal client, importable by UUID (`exported`).
//!   UVM's ALLOC_DEVICE_P2P, which is for video memory, likewise. All are
//!   EPERM. NVKMS takes an RM object by (hClient, hObject) only from kernel
//!   clients (RegisterSurface, nvkms.c:2727-2730), and nvidia-drm names
//!   memory to NVKMS only by an export descriptor, so the refusal covers
//!   both; a dma-buf is the other way in, and RM's export of one is refused
//!   already.
//!
//! Ended, its mapping goes and its id joins a release log the guest reads
//! with [`OP_OSDESC_REAP`]; the guest unpins what it names. It reaps after
//! every RM_FREE, every close and before every registration, so a release a
//! UVM call causes is read at the next of those.
//!
//! **Bounds.** Registrations per VM (released ones not yet reaped
//! included), per guest file and bytes per both, separately mapped runs
//! per VM (each is a mapping of this process's, and there are
//! `vm.max_map_count` of those), and UVM mappings of registered memory and
//! recorded external ranges per VM.

#![forbid(unsafe_code)]

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::os::fd::RawFd;
use std::sync::Arc;

use abi::ioctl::{NV_ESC_RM_ALLOC, NV_ESC_RM_ALLOC_MEMORY, NV_ESC_RM_VID_HEAP_CONTROL};
#[cfg(doc)]
use protocol::messages::{BCAP_OS_DESC, DEEP_PAGE_LIST, OP_OSDESC_REAP};
use protocol::messages::{
    OSDESC_F_WRITE, OSDESC_MAX_PAGES, OSDESC_MAX_RUNS, OSDESC_REAP_MAX, OsDescHdr, OsDescRun,
};

use crate::hostfd;
use crate::le;
use crate::nvidia::{IoctlOut, NvidiaBackend, V1, ioctl_arg_len};
use crate::nvos::{
    self, NV_ERR_NO_MEMORY, NV_OK, NV_OS_DESC_MEMORY_ALLOCATION_ATTR2,
    NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR_TYPE,
    NV_OS_DESC_MEMORY_ALLOCATION_FLAGS, NV_OS_DESC_MEMORY_ALLOCATION_LIMIT,
    NV_OS_DESC_MEMORY_ALLOCATION_SIZE, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, NVOS00_H_OBJECT_OLD,
    NVOS00_H_ROOT, NVOS00_SIZE, NVOS00_STATUS, NVOS02_FLAGS, NVOS02_H_CLASS, NVOS02_H_OBJECT_NEW,
    NVOS02_H_OBJECT_PARENT, NVOS02_H_ROOT, NVOS02_LIMIT, NVOS02_P_MEMORY, NVOS02_STATUS,
    NVOS02_WITH_FD_FD, NVOS02_WITH_FD_SIZE, NVOS32_ALLOC_OS_DESC_ATTR2,
    NVOS32_ALLOC_OS_DESC_DESCRIPTOR, NVOS32_ALLOC_OS_DESC_DESCRIPTOR_TYPE,
    NVOS32_ALLOC_OS_DESC_H_MEMORY, NVOS32_ALLOC_OS_DESC_LIMIT, NVOS32_FUNCTION,
    NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR, NVOS32_H_OBJECT_PARENT, NVOS32_H_ROOT, NVOS32_SIZE,
    NVOS32_STATUS, NVOS54_CMD, NVOS54_H_CLIENT, NVOS54_H_OBJECT, NVOS54_SIZE, NVOS54_STATUS,
    NVOS55_H_CLIENT, NVOS55_H_CLIENT_SRC, NVOS55_H_OBJECT, NVOS55_H_OBJECT_SRC, NVOS55_H_PARENT,
    NVOS55_STATUS, NVOS64_H_CLASS, NVOS64_H_OBJECT_NEW, NVOS64_H_OBJECT_PARENT, NVOS64_H_ROOT,
    NVOS64_P_ALLOC_PARMS, NVOS64_P_RIGHTS_REQUESTED, NVOS64_PARAMS_SIZE, NVOS64_SIZE,
    NVOS64_STATUS, ROOT_CLASSES,
};
use crate::sys::block::{Arena, BufId, Restore, SlotKind};
use crate::sys::mem::{HostSpan, Reservation};

/// A refusal: the errno the guest's ioctl returns.
pub type Errno = i32;

pub const PAGE: u64 = 4096;

/// NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS: the only type taken.
pub const DESCRIPTOR_TYPE_VIRTUAL_ADDRESS: u32 = 0;
/// NVOS32_ATTR2_PROTECTION_USER (22:22), READ_ONLY.
const ATTR2_PROTECTION_USER_READ_ONLY: u32 = 1 << 22;
/// NVOS32_ALLOC_FLAGS_USER_READ_ONLY.
const NVOS32_ALLOC_FLAGS_USER_READ_ONLY: u32 = 0x0400_0000;
/// NVOS02_FLAGS_ALLOC_USER_READ_ONLY (21:21).
const NVOS02_FLAGS_ALLOC_USER_READ_ONLY: u32 = 1 << 21;

// ─────────────────────────── Guest RAM ───────────────────────────

/// One region of guest RAM, as the vhost-user memory table describes it.
#[derive(Clone, Debug)]
pub struct RamRegion {
    /// Guest-physical start.
    pub gpa: u64,
    pub len: u64,
    /// The backend's own mapping of the region (read-write, shared), which
    /// the span keeps mapped.
    pub host: HostSpan,
    /// The region's backing file and where in it the region starts. None
    /// for anonymous memory, which only a registration inside one region
    /// can use.
    pub file: Option<(Arc<File>, u64)>,
}

/// Guest RAM: the regions of the vhost-user memory table, and what keeps
/// their mappings alive.
#[derive(Clone)]
pub struct GuestRam {
    regions: Vec<RamRegion>,
    keep: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for GuestRam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestRam")
            .field("regions", &self.regions)
            .finish()
    }
}

impl GuestRam {
    /// `regions`, whose mappings `keep` holds. A region not page-aligned in
    /// every respect, or overlapping another, is left out (and said so).
    pub fn new(mut regions: Vec<RamRegion>, keep: Arc<dyn Any + Send + Sync>) -> Self {
        regions.retain(|r| {
            let aligned = r.gpa % PAGE == 0
                && r.len % PAGE == 0
                && r.len > 0
                && r.host.addr() % PAGE == 0
                && r.host.len() as u64 == r.len
                && r.file.as_ref().is_none_or(|(_, o)| o % PAGE == 0)
                && r.gpa.checked_add(r.len).is_some();
            if !aligned {
                log::warn!(
                    "guest RAM region at {:#x}+{:#x} is not page-aligned; no OS descriptor may \
                     name it",
                    r.gpa,
                    r.len
                );
            }
            aligned
        });
        regions.sort_by_key(|r| r.gpa);
        let mut out: Vec<RamRegion> = Vec::with_capacity(regions.len());
        for r in regions {
            if out.last().is_some_and(|p| p.gpa + p.len > r.gpa) {
                log::warn!(
                    "guest RAM region at {:#x} overlaps another; left out",
                    r.gpa
                );
                continue;
            }
            out.push(r);
        }
        Self { regions: out, keep }
    }

    /// The table vhost-user handed the backend: every region of `mem`.
    #[cfg(feature = "vhost-user")]
    pub fn from_vm_memory(mem: Arc<vm_memory::GuestMemoryMmap>) -> Self {
        let regions = HostSpan::of_vm_memory(mem.clone())
            .into_iter()
            .map(|(gpa, host, file)| RamRegion {
                gpa,
                len: host.len() as u64,
                host,
                file,
            })
            .collect();
        Self::new(regions, mem)
    }

    pub fn regions(&self) -> &[RamRegion] {
        &self.regions
    }

    fn find(&self, gpa: u64) -> Option<&RamRegion> {
        let i = self.regions.partition_point(|r| r.gpa <= gpa);
        let r = self.regions.get(i.checked_sub(1)?)?;
        (gpa - r.gpa < r.len).then_some(r)
    }
}

// ─────────────────────────── The call ───────────────────────────

/// Which of the three calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// NV_ESC_RM_ALLOC_MEMORY, NVOS02 and its fd (56 bytes).
    AllocMemory,
    /// NV_ESC_RM_VID_HEAP_CONTROL, NVOS32 (184 bytes).
    VidHeap,
    /// NV_ESC_RM_ALLOC, NVOS64 (48 bytes) and NV_OS_DESC_MEMORY_ALLOCATION_PARAMS
    /// (40 bytes) as the nested block.
    RmAlloc,
}

/// What one of the three calls asks RM to pin, read from the block the host
/// will be handed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Call {
    pub shape: Shape,
    /// The caller's address, which RM would pin from.
    pub va: u64,
    /// `limit + 1`.
    pub size: u64,
    /// Whether RM pins for writing.
    pub writable: bool,
    pub client: u32,
    pub parent: u32,
}

impl Call {
    /// The caller's offset inside its first page.
    pub fn in_page(&self) -> u64 {
        self.va % PAGE
    }

    /// Pages RM would pin: from the one holding `va` to the one holding its
    /// last byte.
    pub fn pages(&self) -> u64 {
        (self.in_page() + self.size).div_ceil(PAGE)
    }
}

/// Whether `cmd` with `outer` (and `nested`, for RM_ALLOC) is one of the
/// three calls. Says nothing about whether it is well-formed.
pub(crate) fn shape_of(cmd: u32, outer: &[u8]) -> Option<Shape> {
    if hostfd::ioc_type(cmd) != b'F' {
        return None;
    }
    match hostfd::ioc_nr(cmd) {
        NV_ESC_RM_ALLOC_MEMORY
            if le::u32_at(outer, NVOS02_H_CLASS) == Some(NV01_MEMORY_SYSTEM_OS_DESCRIPTOR) =>
        {
            Some(Shape::AllocMemory)
        }
        NV_ESC_RM_VID_HEAP_CONTROL
            if le::u32_at(outer, NVOS32_FUNCTION) == Some(NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR) =>
        {
            Some(Shape::VidHeap)
        }
        NV_ESC_RM_ALLOC
            if le::u32_at(outer, NVOS64_H_CLASS) == Some(NV01_MEMORY_SYSTEM_OS_DESCRIPTOR) =>
        {
            Some(Shape::RmAlloc)
        }
        _ => None,
    }
}

/// Read the call from its blocks, as sent: `outer` the top-level block and
/// `nested` what RM_ALLOC's pAllocParms addresses. EINVAL for anything that
/// is not exactly one of the three, EPERM for a descriptor type other than
/// the user virtual address.
pub(crate) fn describe(cmd: u32, outer: &[u8], nested: &[u8]) -> Result<Call, Errno> {
    let refuse = |why: &str, e: Errno| {
        log::warn!("OS descriptor {cmd:#x} refused: {why}");
        Err(e)
    };
    let Some(shape) = shape_of(cmd, outer) else {
        return refuse("not a call that registers memory", libc::EINVAL);
    };
    let size_is = |want: usize| hostfd::ioc_size(cmd) == want && outer.len() == want;
    let (va, limit, writable, client, parent) = match shape {
        Shape::AllocMemory => {
            if !size_is(NVOS02_WITH_FD_SIZE) || !nested.is_empty() {
                return refuse("not NVOS02 with its fd", libc::EINVAL);
            }
            // A zero handle has RM generate one it never writes back
            // (RmAllocOsDescriptor copies only the status out): the object
            // would live under a handle nothing here could key it by, and a
            // second such call -- or any ALLOC_MEMORY that makes an object
            // under a zero handle -- would end its registration while RM
            // still holds the pages.
            if le::u32_at(outer, NVOS02_H_OBJECT_NEW) == Some(0) {
                return refuse("a zero hObjectNew", libc::EINVAL);
            }
            // RmAllocOsDescriptor: ALLOC_USER_READ_ONLY makes ATTR2
            // PROTECTION_USER read-only, which RmCreateOsDescriptor pins for
            // (escape.c:260-261, 161).
            let flags = le::u32_at(outer, NVOS02_FLAGS).unwrap_or(0);
            (
                le::u64_at(outer, NVOS02_P_MEMORY),
                le::u64_at(outer, NVOS02_LIMIT),
                flags & NVOS02_FLAGS_ALLOC_USER_READ_ONLY == 0,
                le::u32_at(outer, NVOS02_H_ROOT),
                le::u32_at(outer, NVOS02_H_OBJECT_PARENT),
            )
        }
        Shape::VidHeap => {
            if !size_is(NVOS32_SIZE) || !nested.is_empty() {
                return refuse("not NVOS32", libc::EINVAL);
            }
            if le::u32_at(outer, NVOS32_ALLOC_OS_DESC_DESCRIPTOR_TYPE)
                != Some(DESCRIPTOR_TYPE_VIRTUAL_ADDRESS)
            {
                return refuse("a descriptor that is not a virtual address", libc::EPERM);
            }
            let attr2 = le::u32_at(outer, NVOS32_ALLOC_OS_DESC_ATTR2).unwrap_or(0);
            (
                le::u64_at(outer, NVOS32_ALLOC_OS_DESC_DESCRIPTOR),
                le::u64_at(outer, NVOS32_ALLOC_OS_DESC_LIMIT),
                attr2 & ATTR2_PROTECTION_USER_READ_ONLY == 0,
                le::u32_at(outer, NVOS32_H_ROOT),
                le::u32_at(outer, NVOS32_H_OBJECT_PARENT),
            )
        }
        Shape::RmAlloc => {
            let size = le::u32_at(outer, NVOS64_PARAMS_SIZE).unwrap_or(u32::MAX) as usize;
            if !size_is(NVOS64_SIZE)
                || nested.len() != NV_OS_DESC_MEMORY_ALLOCATION_SIZE
                || !(size == 0 || size == NV_OS_DESC_MEMORY_ALLOCATION_SIZE)
            {
                return refuse(
                    "not NVOS64 with NV_OS_DESC_MEMORY_ALLOCATION_PARAMS",
                    libc::EINVAL,
                );
            }
            if le::u32_at(nested, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR_TYPE)
                != Some(DESCRIPTOR_TYPE_VIRTUAL_ADDRESS)
            {
                return refuse("a descriptor that is not a virtual address", libc::EPERM);
            }
            // osdescConstruct: either marks it read-only (os_desc_mem.c:75-84).
            let attr2 = le::u32_at(nested, NV_OS_DESC_MEMORY_ALLOCATION_ATTR2).unwrap_or(0);
            let flags = le::u32_at(nested, NV_OS_DESC_MEMORY_ALLOCATION_FLAGS).unwrap_or(0);
            (
                le::u64_at(nested, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR),
                le::u64_at(nested, NV_OS_DESC_MEMORY_ALLOCATION_LIMIT),
                attr2 & ATTR2_PROTECTION_USER_READ_ONLY == 0
                    && flags & NVOS32_ALLOC_FLAGS_USER_READ_ONLY == 0,
                le::u32_at(outer, NVOS64_H_ROOT),
                le::u32_at(outer, NVOS64_H_OBJECT_PARENT),
            )
        }
    };
    let (Some(va), Some(limit), Some(client), Some(parent)) = (va, limit, client, parent) else {
        return refuse("a short block", libc::EINVAL);
    };
    // RM refuses the wrap itself (NV_ERR_INVALID_LIMIT); nothing to pin.
    let Some(size) = limit.checked_add(1) else {
        return refuse("a limit that wraps", libc::EINVAL);
    };
    if va.checked_add(size).is_none() {
        return refuse("a range past the end of the address space", libc::EINVAL);
    }
    let call = Call {
        shape,
        va,
        size,
        writable,
        client,
        parent,
    };
    if call.pages() > u64::from(OSDESC_MAX_PAGES) {
        return refuse("more pages than one registration may name", libc::E2BIG);
    }
    Ok(call)
}

/// The guest's page list, checked against `call`: its runs, `(gpa, pages)`.
pub(crate) fn parse_runs(call: &Call, deep: &[u8]) -> Result<Vec<(u64, u64)>, Errno> {
    let refuse = |why: String| {
        log::warn!("OS descriptor page list refused: {why}");
        Err(libc::EINVAL)
    };
    const HDR: usize = size_of::<OsDescHdr>();
    const RUN: usize = size_of::<OsDescRun>();
    let (Some(nruns), Some(flags)) = (le::u32_at(deep, 0), le::u32_at(deep, 4)) else {
        return refuse(format!("{} bytes, no header", deep.len()));
    };
    if nruns == 0 || nruns > OSDESC_MAX_RUNS {
        return refuse(format!("{nruns} runs"));
    }
    if deep.len() != HDR + nruns as usize * RUN {
        return refuse(format!("{nruns} runs in {} bytes", deep.len()));
    }
    if flags & !OSDESC_F_WRITE != 0 || (flags & OSDESC_F_WRITE != 0) != call.writable {
        return refuse(format!(
            "flags {flags:#x} for a call RM pins {}",
            if call.writable {
                "for writing"
            } else {
                "read-only"
            }
        ));
    }
    let mut runs = Vec::with_capacity(nruns as usize);
    let mut total = 0u64;
    for i in 0..nruns as usize {
        let at = HDR + i * RUN;
        let gpa = le::u64_at(deep, at).unwrap_or(0);
        let pages = u64::from(le::u32_at(deep, at + 8).unwrap_or(0));
        if le::u32_at(deep, at + 12) != Some(0) {
            return refuse(format!("run {i}: reserved bits"));
        }
        if !gpa.is_multiple_of(PAGE) || pages == 0 {
            return refuse(format!("run {i}: {pages} pages at {gpa:#x}"));
        }
        if gpa.checked_add(pages * PAGE).is_none() {
            return refuse(format!("run {i}: wraps"));
        }
        total += pages;
        runs.push((gpa, pages));
    }
    if total != call.pages() {
        return refuse(format!(
            "{total} pages for a call RM pins {} of",
            call.pages()
        ));
    }
    Ok(runs)
}

// ─────────────────────────── The mapping ───────────────────────────

/// A stretch of the list inside one region: where it is in the backend's
/// mapping and in the region's file.
#[derive(Clone, Debug)]
struct Piece {
    host: HostSpan,
    file: Option<(Arc<File>, u64)>,
    len: u64,
}

/// A page list turned into the backend's addresses, not yet mapped.
#[derive(Debug)]
pub(crate) struct Resolved {
    pieces: Vec<Piece>,
    keep: Arc<dyn Any + Send + Sync>,
}

impl Resolved {
    /// Separate mappings this will make: none when it is one stretch of the
    /// backend's own mapping of guest RAM.
    pub fn vmas(&self) -> usize {
        if self.pieces.len() == 1 {
            0
        } else {
            self.pieces.len()
        }
    }

    pub fn bytes(&self) -> u64 {
        self.pieces.iter().map(|p| p.len).sum()
    }
}

/// Every run of the list in guest RAM, split where a region ends and merged
/// where the next page follows in the same region. EFAULT for a page in no
/// region.
pub(crate) fn resolve(ram: &GuestRam, runs: &[(u64, u64)]) -> Result<Resolved, Errno> {
    let mut pieces: Vec<Piece> = Vec::new();
    for &(gpa, pages) in runs {
        let mut at = gpa;
        let end = gpa + pages * PAGE;
        while at < end {
            let Some(r) = ram.find(at) else {
                log::warn!("OS descriptor page list refused: {at:#x} is not guest RAM");
                return Err(libc::EFAULT);
            };
            let off = at - r.gpa;
            let len = (end - at).min(r.len - off);
            let Some(host) = r.host.sub(off, len) else {
                return Err(libc::EFAULT);
            };
            let file = r.file.as_ref().map(|(f, o)| (f.clone(), o + off));
            let joined = pieces.last().and_then(|p| {
                let same_file = match (&p.file, &file) {
                    (Some((pf, po)), Some((f, o))) => Arc::ptr_eq(pf, f) && po + p.len == *o,
                    (None, None) => true,
                    _ => false,
                };
                same_file.then(|| p.host.join(&host)).flatten()
            });
            match (joined, pieces.last_mut()) {
                (Some(j), Some(p)) => {
                    p.host = j;
                    p.len += len;
                }
                _ => pieces.push(Piece { host, file, len }),
            }
            at += len;
        }
    }
    Ok(Resolved {
        pieces,
        keep: ram.keep.clone(),
    })
}

/// The backend's range RM is handed, owned until RM has let go of it.
#[derive(Debug)]
pub(crate) enum Mapping {
    /// A stretch of the backend's own mapping of guest RAM, kept alive.
    Direct {
        #[allow(dead_code)]
        keep: Arc<dyn Any + Send + Sync>,
    },
    /// A range of its own, unmapped when the last holder of it goes.
    Reserved(#[allow(dead_code)] Arc<Reservation>),
}

// The ranges this thread has reserved and not yet unmapped, for tests: the
// address of one that is gone may be mapped again by another test's thread
// at any moment, so whether something is mapped there says nothing.
#[cfg(test)]
std::thread_local! {
    static RESERVED: std::cell::RefCell<HashSet<(usize, usize)>> =
        std::cell::RefCell::new(HashSet::new());
}

/// Whether a range this thread reserved still covers `addr`.
#[cfg(test)]
pub(crate) fn reserved_live(addr: u64) -> bool {
    let a = addr as usize;
    RESERVED.with(|r| r.borrow().iter().any(|&(b, l)| a >= b && a < b + l))
}

#[cfg(test)]
impl Drop for Mapping {
    fn drop(&mut self) {
        if let Mapping::Reserved(r) = self {
            RESERVED.with(|s| s.borrow_mut().remove(&(r.addr() as usize, r.len())));
        }
    }
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reservation({:#x}+{:#x})", self.addr(), self.len())
    }
}

/// What RM is handed: `at` bytes into `span` (the address `addr`), kept
/// mapped by `mapping`.
#[derive(Debug)]
pub(crate) struct Pinned {
    pub addr: u64,
    pub span: HostSpan,
    pub at: u64,
    pub mapping: Mapping,
    pub bytes: u64,
    pub vmas: usize,
}

impl Resolved {
    /// Give the pages an address of ours, `in_page` past the start of the
    /// first, readable and (if `writable`) writable.
    pub fn map(self, in_page: u64, writable: bool) -> Result<Pinned, Errno> {
        let bytes = self.bytes();
        let vmas = self.vmas();
        if self.pieces.len() == 1 {
            let span = self.pieces[0].host.clone();
            return Ok(Pinned {
                addr: span.addr() + in_page,
                span,
                at: in_page,
                mapping: Mapping::Direct { keep: self.keep },
                bytes,
                vmas,
            });
        }
        let len = usize::try_from(bytes).map_err(|_| libc::ENOMEM)?;
        let res = match Reservation::new(len) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                log::warn!("OS descriptor: reserving {len:#x} bytes: {e}");
                return Err(libc::ENOMEM);
            }
        };
        // From here the reservation is unmapped on any return.
        #[cfg(test)]
        RESERVED.with(|r| r.borrow_mut().insert((res.addr() as usize, len)));
        let mapping = Mapping::Reserved(res.clone());
        let mut at = 0usize;
        for p in &self.pieces {
            let Some((file, off)) = &p.file else {
                log::warn!(
                    "OS descriptor: pages of anonymous guest RAM that are not contiguous; \
                     nothing to map them from"
                );
                return Err(libc::EINVAL);
            };
            let Ok(plen) = usize::try_from(p.len) else {
                return Err(libc::ENOMEM);
            };
            // Over part of our own reservation; the file is guest RAM's
            // backing memfd.
            if let Err(e) = res.map_file(at, plen, file.as_ref(), *off, writable) {
                log::warn!(
                    "OS descriptor: mapping {plen:#x} bytes of guest RAM at file offset \
                     {off:#x}: {e}"
                );
                return Err(libc::ENOMEM);
            }
            at += plen;
        }
        Ok(Pinned {
            addr: res.addr() + in_page,
            span: res.span(),
            at: in_page,
            mapping,
            bytes,
            vmas,
        })
    }
}

// ──────────────────── What else can hold the pages ────────────────────

/// NV_SEMAPHORE_SURFACE (cl00da.h) and NV_MEMORY_MAPPER (cl00fe.h).
pub(crate) const NV_SEMAPHORE_SURFACE: u32 = 0xda;
pub(crate) const NV_MEMORY_MAPPER: u32 = 0xfe;

/// Where in class `class`'s allocation parameters RM is named objects of
/// the caller's client that it duplicates into a client of its own, and
/// holds for as long as the new object lives, not the named one: a
/// semaphore surface's hSemaphoreMem and hMaxSubmittedMem (sem_surf.c,
/// _semsurfDupMemory), a memory mapper's hSemaphoreSurface (mem_mapper.c:
/// 412). Neither is made a dependant of what it names, so freeing that
/// leaves the new object, and its duplicate, alive.
pub(crate) fn holding_fields(class: u32) -> &'static [usize] {
    match class {
        NV_SEMAPHORE_SURFACE => &[0, 4],
        NV_MEMORY_MAPPER => &[0],
        _ => &[],
    }
}

/// NV_SEMAPHORE_SURFACE_CTRL_CMD_REF_MEMORY (ctrl00da.h:65): RM duplicates
/// the memory a semaphore surface keeps in its own client back into the
/// caller's, at the handles in the reply's NV_SEMAPHORE_SURFACE_CTRL_REF_
/// MEMORY_PARAMS {hSemaphoreMem, hMaxSubmittedMem} (sem_surf.c,
/// semsurfCtrlCmdRefMemory). Each is registered memory the backend never saw
/// made, once the surface holds some (`OsDesc::referenced`).
pub(crate) const SEMSURF_REF_MEMORY: u32 = 0x00da_0001;

/// NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD, _EXPORT_OBJECTS_TO_FD, and
/// NV00E0_CTRL_CMD_EXPORT_MEM.
pub(crate) const EXPORT_OBJECT_TO_FD: u32 = 0x3d05;
pub(crate) const EXPORT_OBJECTS_TO_FD: u32 = 0x3d0b;
pub(crate) const EXPORT_MEM: u32 = 0x00e0_0101;

/// The objects of the calling client RM control `cmd` would hand to a
/// holder the backend cannot follow, read from `params`: an RM export
/// descriptor (the object's duplicate lives in RM's export client until
/// that file closes, and any importer, NVKMS and nvidia-drm among them,
/// gets a duplicate of its own), or an NV_MEMORY_EXPORT object (duplicated
/// into an internal client, and importable by UUID). None for any other
/// control. A block too short for its layout names every word in it.
pub(crate) fn exported(cmd: u32, params: &[u8]) -> Option<Vec<u32>> {
    let words = |from: usize, n: usize| -> Vec<u32> {
        (0..n)
            .filter_map(|i| le::u32_at(params, from + 4 * i))
            .collect()
    };
    let every = || words(0, params.len() / 4);
    let count16 = |off: usize| le::uint_at(params, off, 2).map(|n| n as usize);
    Some(match cmd {
        // NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS: object.type,
        // .rmObject.{hDevice, hParent, hObject} (hObject at 12), fd, flags.
        EXPORT_OBJECT_TO_FD if params.len() >= 24 => words(12, 1),
        // NV0000_CTRL_OS_UNIX_EXPORT_OBJECTS_TO_FD_PARAMS: objects[512] at
        // 76, numObjects at 2124 (RM exports the first numObjects).
        EXPORT_OBJECTS_TO_FD if params.len() >= 2128 => {
            words(76, count16(2124).unwrap_or(512).min(512))
        }
        // NV00E0_CTRL_EXPORT_MEM_PARAMS: handles[256] at 8, numHandles at
        // 1032.
        EXPORT_MEM if params.len() >= 1048 => words(8, count16(1032).unwrap_or(256).min(256)),
        EXPORT_OBJECT_TO_FD | EXPORT_OBJECTS_TO_FD | EXPORT_MEM => every(),
        _ => return None,
    })
}

/// nvidia-uvm's commands (uvm_ioctl.h, UVM_IOCTL_BASE(n) == n) that make,
/// take down or refuse a mapping of registered memory.
pub(crate) const UVM_MAP_EXTERNAL_ALLOCATION: u32 = 33;
pub(crate) const UVM_UNMAP_EXTERNAL: u32 = 66;
pub(crate) const UVM_CREATE_EXTERNAL_RANGE: u32 = 73;
pub(crate) const UVM_ALLOC_DEVICE_P2P: u32 = 78;

/// A MAP_EXTERNAL_ALLOCATION of registered memory on its way to UVM.
#[derive(Debug)]
pub(crate) struct UvmMap {
    pub base: u64,
    pub len: u64,
    pub gpus: Vec<GpuUuid>,
    pub client: u32,
    pub memory: u32,
    /// Where UVM leaves its status in the block.
    pub status_at: usize,
}

// ─────────────────────────── The registry ───────────────────────────

/// How much one VM, and one of its files, may have registered.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Registrations per VM, released ones the guest has not reaped
    /// included.
    pub regs_per_vm: usize,
    pub regs_per_file: usize,
    pub bytes_per_vm: u64,
    pub bytes_per_file: u64,
    /// Separately mapped runs per VM.
    pub vmas_per_vm: usize,
    /// UVM external mappings of registered memory per VM.
    pub uvm_maps_per_vm: usize,
    /// UVM external ranges recorded per VM (what a UVM_FREE takes).
    pub uvm_ranges_per_vm: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            regs_per_vm: 4096,
            regs_per_file: 1024,
            bytes_per_vm: 16 << 30,
            bytes_per_file: 4 << 30,
            vmas_per_vm: 32768,
            uvm_maps_per_vm: 65536,
            uvm_ranges_per_vm: 65536,
        }
    }
}

/// An RM object a registration lives through: (hClient, hObject).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    client: u32,
    object: u32,
}

/// A GPU's UUID, as UVM names one (NvProcessorUuid).
pub(crate) type GpuUuid = [u8; 16];

/// An nvidia-uvm external mapping of a registration: UVM keeps a
/// duplicate of the memory in an RM client of its own for as long as the
/// mapping lasts on any GPU.
#[derive(Debug)]
struct UvmHold {
    /// The guest's UVM file. None once that file closed without the
    /// mapping coming down: it lasts until the session does.
    file: Option<u32>,
    /// The guest process that file is charged to (quota.rs).
    owner: crate::quota::Owner,
    base: u64,
    len: u64,
    /// The GPUs it is mapped on.
    gpus: Vec<GpuUuid>,
    id: u64,
}

impl UvmHold {
    /// Whether it lies in `[base, base + len)`. The guest chose both
    /// lengths: the ends are computed without wrapping (a mapping whose end
    /// would wrap lies in nothing).
    fn within(&self, base: u64, len: u64) -> bool {
        self.base >= base
            && self
                .base
                .checked_add(self.len)
                .is_some_and(|e| e <= base.saturating_add(len))
    }
}

/// How the VM's UVM external mappings of registered memory are split among
/// its guest processes (quota.rs): one that filled the VM's bound with
/// mappings refused every other process's.
fn uvm_share(limit: usize) -> crate::quota::Share {
    crate::quota::Share::quarter(limit as u64, 16)
}

#[derive(Debug)]
struct Reg {
    /// The guest file the call was made on, for its budget.
    file: u32,
    /// The guest process that file is charged to, for its share.
    owner: crate::quota::Owner,
    /// The RM objects that hold it, each with the parent it was made under:
    /// the object the call made, its duplicates, and objects RM made over
    /// it that keep a duplicate of their own (a semaphore surface, and a
    /// memory mapper over one of those).
    keys: Vec<(Key, u32)>,
    /// Its UVM external mappings (`OsDesc::uvm`).
    uvm: usize,
    bytes: u64,
    vmas: usize,
    #[allow(dead_code)]
    mapping: Mapping,
}

impl Reg {
    /// Nothing the host kernel holds names it any more.
    fn unheld(&self) -> bool {
        self.keys.is_empty() && self.uvm == 0
    }
}

/// Every registration of the session, what holds each, and the releases the
/// guest has not yet read.
#[derive(Debug)]
pub struct OsDesc {
    limits: Limits,
    regs: HashMap<u64, Reg>,
    /// Which registrations each RM object holds: one, but for an object
    /// made over several (a semaphore surface's two memories).
    by_key: HashMap<Key, Vec<u64>>,
    uvm: Vec<UvmHold>,
    /// The external ranges each guest UVM file made, base to length: what
    /// a UVM_FREE of one takes with it.
    uvm_ranges: HashMap<u32, BTreeMap<u64, u64>>,
    uvm_range_count: usize,
    uvm_ranges_warned: bool,
    /// The guest process each UVM file with recorded ranges is charged
    /// to, and how many each process has recorded: a quarter of the VM's
    /// bound each.
    uvm_range_files: HashMap<u32, crate::quota::Owner>,
    uvm_ranges_by_owner: HashMap<crate::quota::Owner, usize>,
    per_file: HashMap<u32, (usize, u64)>,
    /// The guest process each file is charged to, and what each process
    /// holds (quota.rs): a process holds at most a file's budget, whatever
    /// number of files it has, and the VM's last sixteenth is kept for
    /// processes holding at most a sixty-fourth (B5).
    file_owners: HashMap<u32, crate::quota::Owner>,
    /// Registrations, bytes and separately mapped runs.
    per_owner: HashMap<crate::quota::Owner, (usize, u64, usize)>,
    bytes: u64,
    vmas: usize,
    /// (sequence, id), oldest first.
    released: VecDeque<(u64, u64)>,
    next_id: u64,
    next_seq: u64,
}

impl Default for OsDesc {
    fn default() -> Self {
        Self::with_limits(Limits::default())
    }
}

impl OsDesc {
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            regs: HashMap::new(),
            by_key: HashMap::new(),
            uvm: Vec::new(),
            uvm_ranges: HashMap::new(),
            uvm_range_count: 0,
            uvm_ranges_warned: false,
            uvm_range_files: HashMap::new(),
            uvm_ranges_by_owner: HashMap::new(),
            per_file: HashMap::new(),
            file_owners: HashMap::new(),
            per_owner: HashMap::new(),
            bytes: 0,
            vmas: 0,
            released: VecDeque::new(),
            next_id: 1,
            next_seq: 1,
        }
    }

    /// Live registrations.
    pub fn live(&self) -> usize {
        self.regs.len()
    }

    /// Released, not yet reaped.
    pub fn unreaped(&self) -> usize {
        self.released.len()
    }

    /// UVM external mappings of registrations, those of closed files
    /// included.
    pub fn uvm_held(&self) -> usize {
        self.uvm.len()
    }

    /// Whether RM object `object` of client `client` holds a registration:
    /// the object a registration made, a duplicate of one, or an object made
    /// over one.
    pub(crate) fn holds(&self, client: u32, object: u32) -> bool {
        self.by_key.contains_key(&Key { client, object })
    }

    /// `(client, object)` holds a registration, as far as `holds` is
    /// concerned.
    #[cfg(test)]
    pub(crate) fn held_for_test(&mut self, client: u32, object: u32) {
        self.by_key
            .entry(Key { client, object })
            .or_default()
            .push(u64::MAX);
    }

    /// Guest file `file` is charged to `owner` (quota.rs).
    pub(crate) fn set_file_owner(&mut self, file: u32, owner: crate::quota::Owner) {
        if owner == crate::quota::Owner::Unknown {
            self.file_owners.remove(&file);
        } else {
            self.file_owners.insert(file, owner);
        }
    }

    /// Guest file `file` closed. Its owner goes unless a registration made
    /// on it is still live (then `release` drops it with the last one): a
    /// call refused before it registered anything set it too, and a loop of
    /// open, refused registration, close would otherwise grow the map for
    /// the session.
    pub(crate) fn file_closed(&mut self, file: u32) {
        if !self.per_file.contains_key(&file) {
            self.file_owners.remove(&file);
        }
    }

    /// Whether a registration of `bytes`, mapped in `vmas` pieces, fits on
    /// guest file `file`. ENOMEM if not.
    pub(crate) fn admit(&self, file: u32, bytes: u64, vmas: usize) -> Result<(), Errno> {
        let (fregs, fbytes) = self.per_file.get(&file).copied().unwrap_or((0, 0));
        let l = &self.limits;
        let owner = self.file_owners.get(&file).copied().unwrap_or_default();
        let (oregs, obytes, ovmas) = self.per_owner.get(&owner).copied().unwrap_or((0, 0, 0));
        let in_use = (self.regs.len() + self.released.len()) as u64;
        let share = |per_file: u64, per_vm: u64| {
            crate::quota::Share::custom(per_file, per_vm / 16, per_vm / 64)
        };
        let why = if crate::quota::admits(
            &share(l.regs_per_file as u64, l.regs_per_vm as u64),
            owner,
            oregs as u64,
            1,
            in_use,
            l.regs_per_vm as u64,
        )
        .is_err()
            && in_use < l.regs_per_vm as u64
        {
            "registrations for the guest process"
        } else if crate::quota::admits(
            &share(l.bytes_per_file, l.bytes_per_vm),
            owner,
            obytes,
            bytes,
            self.bytes,
            l.bytes_per_vm,
        )
        .is_err()
            && self.bytes + bytes <= l.bytes_per_vm
        {
            "bytes for the guest process"
        } else if crate::quota::admits(
            // The separately mapped runs, a quarter each: one process's
            // one-page runs could otherwise take all of them, and every
            // other process's registration fail.
            &crate::quota::Share::quarter(l.vmas_per_vm as u64, 16),
            owner,
            ovmas as u64,
            vmas as u64,
            self.vmas as u64,
            l.vmas_per_vm as u64,
        )
        .is_err()
            && self.vmas + vmas <= l.vmas_per_vm
        {
            "separate mappings for the guest process"
        } else if self.regs.len() + self.released.len() >= l.regs_per_vm {
            "registrations for the VM"
        } else if fregs >= l.regs_per_file {
            "registrations for the file"
        } else if self.bytes + bytes > l.bytes_per_vm {
            "bytes for the VM"
        } else if fbytes + bytes > l.bytes_per_file {
            "bytes for the file"
        } else if self.vmas + vmas > l.vmas_per_vm {
            "separate mappings for the VM"
        } else {
            return Ok(());
        };
        log::warn!("OS descriptor of {bytes:#x} bytes refused: over the {why}");
        Err(libc::ENOMEM)
    }

    /// Record what RM made: object `object` of client `client`, under
    /// `parent`, on guest file `file`. Returns its id.
    pub(crate) fn add(
        &mut self,
        file: u32,
        client: u32,
        object: u32,
        parent: u32,
        pinned: Pinned,
    ) -> u64 {
        let key = Key { client, object };
        // RM made a new object under this handle, so whatever was there
        // before is gone. Object 0 is no handle (RM made one it did not
        // write back): a second registration answered so must not end the
        // first while RM still pins its pages; both then live until their
        // client goes.
        if object != 0 {
            self.drop_key(key);
        }
        let id = self.next_id;
        self.next_id += 1;
        let e = self.per_file.entry(file).or_default();
        e.0 += 1;
        e.1 += pinned.bytes;
        let owner = self.file_owners.get(&file).copied().unwrap_or_default();
        if owner != crate::quota::Owner::Unknown {
            let o = self.per_owner.entry(owner).or_default();
            o.0 += 1;
            o.1 += pinned.bytes;
            o.2 += pinned.vmas;
        }
        self.bytes += pinned.bytes;
        self.vmas += pinned.vmas;
        self.regs.insert(
            id,
            Reg {
                file,
                owner,
                keys: Vec::new(),
                uvm: 0,
                bytes: pinned.bytes,
                vmas: pinned.vmas,
                mapping: pinned.mapping,
            },
        );
        self.add_key(id, key, parent);
        id
    }

    /// `key`, made under `parent`, holds registration `id` too.
    fn add_key(&mut self, id: u64, key: Key, parent: u32) {
        let Some(r) = self.regs.get_mut(&id) else {
            return;
        };
        if r.keys.iter().any(|(k, _)| *k == key) {
            return;
        }
        r.keys.push((key, parent));
        self.by_key.entry(key).or_default().push(id);
    }

    /// Take `key` off whatever it holds, ending each registration nothing
    /// else holds.
    fn drop_key(&mut self, key: Key) {
        let Some(ids) = self.by_key.remove(&key) else {
            return;
        };
        for id in ids {
            let unheld = self.regs.get_mut(&id).is_none_or(|r| {
                r.keys.retain(|(k, _)| *k != key);
                r.unheld()
            });
            if unheld {
                self.release(id);
            }
        }
    }

    /// End registration `id`: its mapping goes, and the guest will be told.
    fn release(&mut self, id: u64) {
        let Some(r) = self.regs.remove(&id) else {
            return;
        };
        for (k, _) in &r.keys {
            if let Some(ids) = self.by_key.get_mut(k) {
                ids.retain(|&i| i != id);
                if ids.is_empty() {
                    self.by_key.remove(k);
                }
            }
        }
        if let Some(e) = self.per_file.get_mut(&r.file) {
            e.0 -= 1;
            e.1 -= r.bytes;
            if e.0 == 0 {
                self.per_file.remove(&r.file);
                // Set again before the file's next registration.
                self.file_owners.remove(&r.file);
            }
        }
        if let Some(e) = self.per_owner.get_mut(&r.owner) {
            e.0 -= 1;
            e.1 -= r.bytes;
            e.2 -= r.vmas;
            if e.0 == 0 {
                self.per_owner.remove(&r.owner);
            }
        }
        self.bytes -= r.bytes;
        self.vmas -= r.vmas;
        self.released.push_back((self.next_seq, id));
        self.next_seq += 1;
        // `r.mapping` is unmapped here.
    }

    /// RM freed object `object` of client `client` (RM_FREE succeeded). The
    /// client itself takes everything of it; any other object takes itself
    /// and what was made under it.
    pub(crate) fn freed(&mut self, client: u32, object: u32) {
        if client == object {
            self.forget_clients(&[client]);
            return;
        }
        let gone: Vec<Key> = self
            .regs
            .values()
            .flat_map(|r| r.keys.iter())
            .filter(|(k, parent)| k.client == client && (k.object == object || *parent == object))
            .map(|(k, _)| *k)
            .collect();
        for k in gone {
            self.drop_key(k);
        }
    }

    /// RM duplicated `(src_client, src)` as `(client, object)` under `parent`.
    pub(crate) fn duplicated(
        &mut self,
        src_client: u32,
        src: u32,
        client: u32,
        object: u32,
        parent: u32,
    ) {
        let key = Key { client, object };
        // Whatever the new handle named before is gone.
        self.drop_key(key);
        let src = Key {
            client: src_client,
            object: src,
        };
        for id in self.by_key.get(&src).cloned().unwrap_or_default() {
            self.add_key(id, key, parent);
        }
    }

    /// RM made `(client, object)` under `parent` over the objects `named`
    /// of the same client, and keeps a duplicate of each in a client of its
    /// own for as long as the new object lives: whatever registration one
    /// of them holds, the new object holds too.
    pub(crate) fn made_over(&mut self, client: u32, object: u32, parent: u32, named: &[u32]) {
        let key = Key { client, object };
        for &h in named {
            let src = Key { client, object: h };
            if h == 0 || src == key {
                continue;
            }
            for id in self.by_key.get(&src).cloned().unwrap_or_default() {
                self.add_key(id, key, parent);
            }
        }
    }

    /// RM handed the caller back, as `out` in the same client, duplicates
    /// of the memory `(client, object)` keeps in a client of its own
    /// (NV_SEMAPHORE_SURFACE_CTRL_CMD_REF_MEMORY on a semaphore surface):
    /// each holds whatever the surface holds. Which of its memories each is
    /// is not told apart -- a surface over two registrations has both held
    /// by either duplicate, late rather than early. The device they are
    /// made under is not known here, so each lasts until it or its client
    /// is freed.
    pub(crate) fn referenced(&mut self, client: u32, object: u32, out: &[u32]) {
        let ids = self
            .by_key
            .get(&Key { client, object })
            .cloned()
            .unwrap_or_default();
        let mut made: Vec<u32> = out
            .iter()
            .copied()
            .filter(|&h| h != 0 && h != object && h != client)
            .collect();
        made.sort_unstable();
        made.dedup();
        for &h in &made {
            // A handle RM just made: whatever it named before is gone.
            self.drop_key(Key { client, object: h });
        }
        for &h in &made {
            for &id in &ids {
                self.add_key(id, Key { client, object: h }, client);
            }
        }
    }

    /// RM made a new object at `(client, object)`: whatever the handle named
    /// before is gone (freed through an ancestor this never saw).
    pub(crate) fn reused(&mut self, client: u32, object: u32) {
        self.drop_key(Key { client, object });
    }

    /// Clients that hold a registration.
    pub(crate) fn clients(&self) -> HashSet<u32> {
        self.by_key.keys().map(|k| k.client).collect()
    }

    /// `clients` are gone, and everything they held.
    pub(crate) fn forget_clients(&mut self, clients: &[u32]) {
        let gone: Vec<Key> = self
            .by_key
            .keys()
            .filter(|k| clients.contains(&k.client))
            .copied()
            .collect();
        for k in gone {
            self.drop_key(k);
        }
    }

    // ── nvidia-uvm external mappings ──

    /// Whether UVM file `file`, charged to `owner`, may map `(client,
    /// memory)` at `[base, base + len)` as an external allocation: always,
    /// unless it is registered memory. Then the range must lie in an
    /// external range this file made and the backend recorded -- where UVM
    /// can map it at all (uvm_map_external_allocation: NV_ERR_INVALID_ADDRESS
    /// elsewhere), and where a UVM_FREE of the range, or the file's close,
    /// takes what the mapping holds (EINVAL); and the VM, and the process,
    /// must hold fewer such mappings than they may (ENOMEM).
    pub(crate) fn uvm_admit(
        &self,
        file: u32,
        owner: crate::quota::Owner,
        client: u32,
        memory: u32,
        base: u64,
        len: u64,
    ) -> Result<(), Errno> {
        if !self.holds(client, memory) {
            return Ok(());
        }
        let inside = base.checked_add(len).is_some_and(|end| {
            self.uvm_ranges.get(&file).is_some_and(|r| {
                r.range(..=base)
                    .next_back()
                    .is_some_and(|(&b, &l)| b.checked_add(l).is_some_and(|e| end <= e))
            })
        });
        if !inside {
            log::warn!(
                "UVM external mapping of registered memory {client:#x}/{memory:#x} at \
                 {base:#x}+{len:#x} refused: no external range of handle {file} recorded there"
            );
            return Err(libc::EINVAL);
        }
        let limit = self.limits.uvm_maps_per_vm;
        let of_owner = self.uvm.iter().filter(|h| h.owner == owner).count();
        if let Err(why) = crate::quota::admits(
            &uvm_share(limit),
            owner,
            of_owner as u64,
            1,
            self.uvm.len() as u64,
            limit as u64,
        ) {
            log::warn!(
                "UVM external mapping of registered memory {client:#x}/{memory:#x} refused: \
                 the VM holds {}, guest process {owner:?} {of_owner} ({why:?})",
                self.uvm.len()
            );
            return Err(libc::ENOMEM);
        }
        Ok(())
    }

    /// UVM file `file` (charged to `owner`) mapped `(client, memory)` at
    /// `[base, base + len)` on `gpus` (MAP_EXTERNAL_ALLOCATION): every
    /// registration the memory holds is held by the mapping as well. The
    /// caller says when UVM made it (`osdesc_uvm_after`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn uvm_mapped(
        &mut self,
        file: u32,
        owner: crate::quota::Owner,
        base: u64,
        len: u64,
        gpus: &[GpuUuid],
        client: u32,
        memory: u32,
    ) {
        let ids = self
            .by_key
            .get(&Key {
                client,
                object: memory,
            })
            .cloned()
            .unwrap_or_default();
        for id in ids {
            if let Some(r) = self.regs.get_mut(&id) {
                r.uvm += 1;
                self.uvm.push(UvmHold {
                    file: Some(file),
                    owner,
                    base,
                    len,
                    gpus: gpus.to_vec(),
                    id,
                });
            }
        }
    }

    /// Take UVM mapping number `i` away, ending its registration if nothing
    /// else holds it.
    fn drop_uvm(&mut self, i: usize) {
        let h = self.uvm.swap_remove(i);
        let unheld = self.regs.get_mut(&h.id).is_none_or(|r| {
            r.uvm -= 1;
            r.unheld()
        });
        if unheld {
            self.release(h.id);
        }
    }

    /// Drop every UVM mapping `f` picks, and whatever it alone held.
    fn drop_uvm_where(&mut self, f: impl Fn(&UvmHold) -> bool) {
        let mut i = 0;
        while i < self.uvm.len() {
            if f(&self.uvm[i]) {
                self.drop_uvm(i);
            } else {
                i += 1;
            }
        }
    }

    /// UVM file `file` unmapped `[base, base + len)` from `gpu`
    /// (UNMAP_EXTERNAL succeeded). A mapping wholly inside is gone from that
    /// GPU; one only partly inside keeps the rest, and so its duplicate.
    pub(crate) fn uvm_unmapped(&mut self, file: u32, base: u64, len: u64, gpu: &GpuUuid) {
        let mut i = 0;
        while i < self.uvm.len() {
            let h = &mut self.uvm[i];
            if h.file == Some(file) && h.within(base, len) && h.gpus.contains(gpu) {
                h.gpus.retain(|g| g != gpu);
                if h.gpus.is_empty() {
                    self.drop_uvm(i);
                    continue;
                }
            }
            i += 1;
        }
    }

    /// UVM file `file` made an external range (CREATE_EXTERNAL_RANGE
    /// succeeded). Past the VM's bound it is not recorded, and a mapping of
    /// registered memory inside it is refused (`uvm_admit`).
    #[cfg(test)]
    pub(crate) fn uvm_range_made(&mut self, file: u32, base: u64, len: u64) {
        self.uvm_range_made_by(file, base, len, crate::quota::Owner::Unknown);
    }

    /// [`OsDesc::uvm_range_made`], on a UVM file charged to guest process
    /// `owner`, which may record only its share of the VM's bound: past
    /// it, as past the VM's, the range is not recorded, and only that
    /// process's mappings in it are affected.
    pub(crate) fn uvm_range_made_by(
        &mut self,
        file: u32,
        base: u64,
        len: u64,
        owner: crate::quota::Owner,
    ) {
        let limit = self.limits.uvm_ranges_per_vm as u64;
        let owner = *self.uvm_range_files.entry(file).or_insert(owner);
        let mine = self.uvm_ranges_by_owner.get(&owner).copied().unwrap_or(0);
        if let Err(crate::quota::Over::Owner | crate::quota::Over::Reserve) = crate::quota::admits(
            &crate::quota::Share::quarter(limit, 16),
            owner,
            mine as u64,
            1,
            self.uvm_range_count as u64,
            limit,
        ) {
            log::debug!(
                "UVM external ranges: guest process {owner:?} has {mine} recorded; one more \
                 is not"
            );
            return;
        }
        if self.uvm_range_count >= self.limits.uvm_ranges_per_vm {
            if !self.uvm_ranges_warned {
                self.uvm_ranges_warned = true;
                log::warn!(
                    "UVM external ranges: {} recorded; registered memory may not be mapped \
                     in a later one",
                    self.uvm_range_count
                );
            }
            return;
        }
        if self
            .uvm_ranges
            .entry(file)
            .or_default()
            .insert(base, len)
            .is_none()
        {
            self.uvm_range_count += 1;
            *self.uvm_ranges_by_owner.entry(owner).or_default() += 1;
        }
    }

    /// `n` of UVM file `file`'s recorded ranges are gone.
    fn uvm_ranges_gone(&mut self, file: u32, n: usize) {
        self.uvm_range_count -= n;
        if let Some(&o) = self.uvm_range_files.get(&file)
            && let Some(c) = self.uvm_ranges_by_owner.get_mut(&o)
        {
            *c -= n;
            if *c == 0 {
                self.uvm_ranges_by_owner.remove(&o);
            }
        }
    }

    /// UVM file `file` freed the range at `base` (UVM_FREE succeeded): an
    /// external range takes every mapping inside it.
    pub(crate) fn uvm_range_freed(&mut self, file: u32, base: u64) {
        let Some(len) = self.uvm_ranges.get_mut(&file).and_then(|m| m.remove(&base)) else {
            return;
        };
        self.uvm_ranges_gone(file, 1);
        self.drop_uvm_where(|h| h.file == Some(file) && h.within(base, len));
    }

    /// What to take down before UVM file `file` closes, while it is still
    /// ours: the recorded external ranges holding a mapping of registered
    /// memory, as (base, length), to UVM_FREE.
    pub(crate) fn uvm_ranges_held(&self, file: u32) -> Vec<(u64, u64)> {
        let Some(ranges) = self.uvm_ranges.get(&file) else {
            return Vec::new();
        };
        ranges
            .iter()
            .filter(|&(&b, &l)| {
                self.uvm
                    .iter()
                    .any(|h| h.file == Some(file) && h.within(b, l))
            })
            .map(|(&b, &l)| (b, l))
            .collect()
    }

    /// The mappings of registered memory UVM file `file` still has, one
    /// (base, length, GPU) for each GPU, to UNMAP_EXTERNAL.
    pub(crate) fn uvm_maps_held(&self, file: u32) -> Vec<(u64, u64, GpuUuid)> {
        self.uvm
            .iter()
            .filter(|h| h.file == Some(file))
            .flat_map(|h| h.gpus.iter().map(|g| (h.base, h.len, *g)))
            .collect()
    }

    /// The guest UVM files holding a mapping of registered memory.
    pub(crate) fn uvm_files(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.uvm.iter().filter_map(|h| h.file).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// UVM file `file` is closing. What it could not take down stays held,
    /// by no file, until the session ends: its last reference may be
    /// dropped later than this (the event pump holds a duplicate of it),
    /// and a later file under the same handle must not end it.
    pub(crate) fn uvm_file_closed(&mut self, file: u32) {
        if let Some(r) = self.uvm_ranges.remove(&file) {
            self.uvm_ranges_gone(file, r.len());
        }
        self.uvm_range_files.remove(&file);
        for h in &mut self.uvm {
            if h.file == Some(file) {
                log::warn!(
                    "OS descriptor {}: UVM mapping at {:#x}+{:#x} was not taken down before its \
                     file closed; the pages stay pinned until the session ends",
                    h.id,
                    h.base,
                    h.len
                );
                h.file = None;
            }
        }
    }

    /// The session is gone: every registration and every release with it. A
    /// guest that says HELLO again starts with none.
    pub(crate) fn clear(&mut self) {
        self.uvm.clear();
        self.uvm_ranges.clear();
        self.uvm_range_count = 0;
        self.uvm_range_files.clear();
        self.uvm_ranges_by_owner.clear();
        let ids: Vec<u64> = self.regs.keys().copied().collect();
        for id in ids {
            self.release(id);
        }
        self.released.clear();
        self.file_owners.clear();
    }

    /// The guest has read every release up to `ack`: forget those, and name
    /// the next ones. Returns (the sequence number of the last named, or
    /// `ack`; the ids).
    pub(crate) fn reap(&mut self, ack: u64) -> (u64, Vec<u64>) {
        while self.released.front().is_some_and(|&(s, _)| s <= ack) {
            self.released.pop_front();
        }
        let named: Vec<(u64, u64)> = self
            .released
            .iter()
            .take(OSDESC_REAP_MAX as usize)
            .copied()
            .collect();
        let last = named.last().map_or(ack, |&(s, _)| s);
        (last, named.into_iter().map(|(_, id)| id).collect())
    }
}

#[cfg(test)]
pub(crate) mod test_ram {
    //! Guest RAM for tests: a memfd with a known pattern, mapped shared, cut
    //! into regions the way a VMM splits RAM around the PCI hole.
    use super::*;
    use crate::sys::mem::Mapping as Map;

    /// The byte at offset `i` of the memfd: page number and offset mixed, so
    /// no two pages read alike.
    pub fn byte(i: u64) -> u8 {
        ((i / PAGE).wrapping_mul(131) ^ (i % PAGE).wrapping_mul(7)) as u8
    }

    /// A memfd of `pages` pages holding [`byte`], and the backend's mapping.
    pub fn memfd(pages: u64) -> (Arc<File>, Arc<Map>) {
        let file = File::from(crate::sys::fd::memfd(c"osdesc-test", libc::MFD_CLOEXEC).unwrap());
        let len = (pages * PAGE) as usize;
        file.set_len(len as u64).unwrap();
        let map = Map::shared(&file, len, 0, true).unwrap();
        let bytes: Vec<u8> = (0..len as u64).map(byte).collect();
        map.write(0, &bytes);
        (Arc::new(file), map)
    }

    /// Low RAM: 64 pages at 0, file offset 0. High RAM: 64 pages at 4 GiB,
    /// file offset 64 pages -- adjacent in the file, not in guest-physical
    /// space.
    pub const LOW: u64 = 0;
    pub const HIGH: u64 = 1 << 32;
    pub const REGION_PAGES: u64 = 64;

    pub fn ram() -> GuestRam {
        let (file, map) = memfd(2 * REGION_PAGES);
        let span = map.span();
        GuestRam::new(
            vec![
                RamRegion {
                    gpa: LOW,
                    len: REGION_PAGES * PAGE,
                    host: span.sub(0, REGION_PAGES * PAGE).unwrap(),
                    file: Some((file.clone(), 0)),
                },
                RamRegion {
                    gpa: HIGH,
                    len: REGION_PAGES * PAGE,
                    host: span.sub(REGION_PAGES * PAGE, REGION_PAGES * PAGE).unwrap(),
                    file: Some((file, REGION_PAGES * PAGE)),
                },
            ],
            map,
        )
    }

    /// The file offset of guest-physical `gpa` in [`ram`].
    pub fn file_off(gpa: u64) -> u64 {
        if gpa >= HIGH {
            gpa - HIGH + REGION_PAGES * PAGE
        } else {
            gpa
        }
    }

    /// What [`ram`] holds at `gpa`, `len` bytes of it.
    pub fn expect(gpa: u64, len: u64) -> Vec<u8> {
        (0..len).map(|i| byte(file_off(gpa) + i)).collect()
    }

    /// A page list.
    pub fn list(flags: u32, runs: &[(u64, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        for &(gpa, pages) in runs {
            out.extend_from_slice(&gpa.to_le_bytes());
            out.extend_from_slice(&pages.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out
    }

    /// Whether any page of `[addr, addr+len)` is mapped in this process.
    #[cfg(feature = "vhost-user")]
    pub fn mapped(addr: u64, len: u64) -> bool {
        crate::sys::mem::any_mapped(addr, len)
    }
}

// ─────────────────────────── The backend ───────────────────────────

impl NvidiaBackend {
    /// One of the three calls that register memory the caller already has
    /// -- ALLOC_MEMORY or RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR,
    /// VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR -- sent with the
    /// guest-physical pages behind it (`list`). `param_in` is the top-level
    /// block (`outer_len` bytes) and, for RM_ALLOC, the class parameters
    /// after it. RM is handed an address of this process's that maps
    /// exactly those pages; the caller reads back its own. On RM's NV_OK the
    /// reply carries the registration's id as its deep block.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_osdesc(
        &mut self,
        host_fd: RawFd,
        request: u64,
        outer_len: usize,
        param_in: &[u8],
        list: &[u8],
    ) -> V1 {
        use crate::osdesc::{self as od, Shape};
        let Some(ram) = self
            .guest_ram
            .clone()
            .filter(|_| self.session.v2 && self.config.allow_compute)
        else {
            log::warn!("OS descriptor page list from a guest never offered BCAP_OS_DESC; refused");
            return Err(libc::EINVAL);
        };
        let cmd = request as u32;
        let (outer, nested) = param_in.split_at(outer_len.min(param_in.len()));
        self.osdesc
            .set_file_owner(self.current_handle, self.handles.owner(self.current_handle));
        // Over a registration budget the answer is RM's own out-of-memory
        // status at `.1` of the caller's block, the ioctl succeeding, as RM
        // answers when it cannot pin: an
        // errno read to libnvidia as a generic OS failure.
        enum Refused {
            Errno(i32),
            Budget(usize),
        }
        let prepared = od::describe(cmd, outer, nested)
            .map_err(Refused::Errno)
            .and_then(|call| {
                let runs = od::parse_runs(&call, list).map_err(Refused::Errno)?;
                let resolved = od::resolve(&ram, &runs).map_err(Refused::Errno)?;
                if self
                    .osdesc
                    .admit(self.current_handle, resolved.bytes(), resolved.vmas())
                    .is_err()
                {
                    return Err(Refused::Budget(match call.shape {
                        Shape::AllocMemory => NVOS02_STATUS,
                        Shape::VidHeap => NVOS32_STATUS,
                        Shape::RmAlloc => NVOS64_STATUS,
                    }));
                }
                let pinned = resolved
                    .map(call.in_page(), call.writable)
                    .map_err(Refused::Errno)?;
                Ok((call, pinned))
            });
        let (call, pinned) = match prepared {
            Ok(p) => p,
            Err(Refused::Budget(at)) => {
                let out = nvos::with_status(param_in, at, NV_ERR_NO_MEMORY);
                return Ok(IoctlOut::ok(out));
            }
            Err(Refused::Errno(e)) => return Err(e),
        };
        log::debug!(
            "OS descriptor {cmd:#x}: {:#x} bytes of guest RAM at {:#x} of ours",
            call.size,
            pinned.addr
        );

        // What records the memory an escape makes (rmmem.rs) sees the call
        // too: a handle made here is no longer whatever it named before.
        let escape = hostfd::ioc_nr(cmd);
        let mut seen = param_in.to_vec();
        let rm_pending = self.rmmem.before(escape, &mut seen);

        // The blocks the host is handed: the guest's, with the address RM
        // pins declared and pointed at the backend's own mapping of exactly
        // the pages named (osdesc.rs), and the caller's values in the reply.
        let mut a = Arena::new();
        let built = (|| -> std::result::Result<(BufId, Option<BufId>, usize, usize), i32> {
            let top = a.block(outer, ioctl_arg_len(request, outer.len()))?;
            match call.shape {
                Shape::AllocMemory => {
                    a.ptr(top, NVOS02_P_MEMORY)?;
                    a.point_span(top, NVOS02_P_MEMORY, &pinned.span, pinned.at)?;
                    // RM reads the descriptor only to arm a mapping of
                    // NV01_MEMORY_SYSTEM (escape.c:415-431); a guest number
                    // goes no further than here.
                    a.fd(top, NVOS02_WITH_FD_FD, 4)?;
                    a.set_no_fd(top, NVOS02_WITH_FD_FD, -1)?;
                    Ok((top, None, NVOS02_STATUS, NVOS02_H_OBJECT_NEW))
                }
                Shape::VidHeap => {
                    a.ptr(top, NVOS32_ALLOC_OS_DESC_DESCRIPTOR)?;
                    a.point_span(
                        top,
                        NVOS32_ALLOC_OS_DESC_DESCRIPTOR,
                        &pinned.span,
                        pinned.at,
                    )?;
                    Ok((top, None, NVOS32_STATUS, NVOS32_ALLOC_OS_DESC_H_MEMORY))
                }
                Shape::RmAlloc => {
                    let n = a.block(nested, nested.len())?;
                    a.ptr(n, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR)?;
                    a.point_span(
                        n,
                        NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR,
                        &pinned.span,
                        pinned.at,
                    )?;
                    a.slot(top, NVOS64_P_ALLOC_PARMS, 8, SlotKind::Ptr, Restore::Yes)?;
                    a.point(top, NVOS64_P_ALLOC_PARMS, n)?;
                    // As guestptr::rm_escape: RM grants the default rights.
                    a.slot(
                        top,
                        NVOS64_P_RIGHTS_REQUESTED,
                        8,
                        SlotKind::Ptr,
                        Restore::Yes,
                    )?;
                    Ok((top, Some(n), NVOS64_STATUS, NVOS64_H_OBJECT_NEW))
                }
            }
        })();
        let (top, class_params, status_at, handle_at) = built?;
        // The class parameters and the range RM pins live past the call: the
        // arena holds the one, `pinned` the other.
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("OS descriptor {cmd:#x}: the host refused the call (errno {errno})");
            return Err(errno);
        }

        // The caller's own values back where ours were.
        let mut out = a.reply(top)[..outer.len()].to_vec();
        if let Some(n) = class_params {
            out.extend_from_slice(&a.reply(n));
        }
        drop(a);
        self.rmmem.after(rm_pending, &mut out);

        // Both inside the block the host was handed, whose size the escape's
        // number fixes (`ioctl_arg_len`).
        let status = le::u32_at(&out, status_at);
        let deep = if let (Some(NV_OK), Some(object)) = (status, le::u32_at(&out, handle_at)) {
            let id = self.osdesc.add(
                self.current_handle,
                call.client,
                object,
                call.parent,
                pinned,
            );
            log::debug!(
                "OS descriptor {id}: {:#x} bytes as {:#x}/{:#x}",
                call.size,
                call.client,
                object
            );
            if call.shape == Shape::RmAlloc {
                self.rm_classes.add(NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
            }
            out.extend_from_slice(&id.to_le_bytes());
            8
        } else {
            // Nothing made, nothing pinned by RM: the range goes now.
            log::debug!("OS descriptor {cmd:#x}: RM answered {status:#x?}; nothing registered");
            drop(pinned);
            0
        };
        Ok(IoctlOut::deep(out, deep))
    }

    /// What RM freed, duplicated or made anew, from a successful reply's
    /// parameters (`reply`), for the registrations that live through RM
    /// objects (osdesc.rs).
    pub(super) fn osdesc_observe(&mut self, escape: u32, reply: &[u8]) {
        use abi::ioctl::*;
        if self.osdesc.live() == 0 {
            return;
        }
        let r = |o: usize| le::u32_at(reply, o);
        // NVOS64 and NVOS02 alike: hRoot, hObjectParent, hObjectNew, hClass,
        // ..., status.
        const _: () = assert!(
            NVOS64_H_ROOT == NVOS02_H_ROOT
                && NVOS64_H_OBJECT_PARENT == NVOS02_H_OBJECT_PARENT
                && NVOS64_H_OBJECT_NEW == NVOS02_H_OBJECT_NEW
                && NVOS64_H_CLASS == NVOS02_H_CLASS
                && NVOS64_STATUS == NVOS02_STATUS
        );
        match escape {
            NV_ESC_RM_FREE if r(NVOS00_STATUS) == Some(NV_OK) => {
                if let (Some(c), Some(o)) = (r(NVOS00_H_ROOT), r(NVOS00_H_OBJECT_OLD)) {
                    self.osdesc.freed(c, o);
                }
            }
            NV_ESC_RM_DUP_OBJECT if r(NVOS55_STATUS) == Some(NV_OK) => {
                if let (Some(c), Some(p), Some(o), Some(sc), Some(so)) = (
                    r(NVOS55_H_CLIENT),
                    r(NVOS55_H_PARENT),
                    r(NVOS55_H_OBJECT),
                    r(NVOS55_H_CLIENT_SRC),
                    r(NVOS55_H_OBJECT_SRC),
                ) {
                    self.osdesc.duplicated(sc, so, c, o, p);
                }
            }
            // The parameters follow the NVOS54. A semaphore surface holding
            // registered memory hands the caller duplicates of it
            // (osdesc.rs, `SEMSURF_REF_MEMORY`): each holds it too.
            NV_ESC_RM_CONTROL
                if r(NVOS54_STATUS) == Some(NV_OK)
                    && r(NVOS54_CMD) == Some(crate::osdesc::SEMSURF_REF_MEMORY) =>
            {
                if let (Some(c), Some(o)) = (r(NVOS54_H_CLIENT), r(NVOS54_H_OBJECT))
                    && self.osdesc.holds(c, o)
                {
                    let out: Vec<u32> = [0, 4]
                        .iter()
                        .filter_map(|&at| r(NVOS54_SIZE + at))
                        .collect();
                    self.osdesc.referenced(c, o, &out);
                }
            }
            // A zero hObjectNew is no handle: RM made the object under one it
            // generated and, through ALLOC_MEMORY, never wrote back.
            //
            // A root class (NV01_ROOT and its kin) makes a new *client*
            // named hObjectNew, and RM ignores hRoot: nothing of `c` was
            // made, and forgetting `(c, o)` would release a registration RM
            // still pins.
            NV_ESC_RM_ALLOC
                if r(NVOS64_STATUS) == Some(NV_OK)
                    && r(NVOS64_H_CLASS).is_some_and(|c| ROOT_CLASSES.contains(&c)) => {}
            NV_ESC_RM_ALLOC | NV_ESC_RM_ALLOC_MEMORY if r(NVOS64_STATUS) == Some(NV_OK) => {
                if let (Some(c), Some(p), Some(o)) = (
                    r(NVOS64_H_ROOT),
                    r(NVOS64_H_OBJECT_PARENT),
                    r(NVOS64_H_OBJECT_NEW),
                ) && o != 0
                {
                    self.osdesc.reused(c, o);
                    // An object RM keeps a duplicate of registered memory
                    // for, in a client of its own (osdesc.rs,
                    // `holding_fields`): the class parameters follow the
                    // NVOS64.
                    if escape == NV_ESC_RM_ALLOC
                        && let Some(class) = r(NVOS64_H_CLASS)
                    {
                        let named: Vec<u32> = crate::osdesc::holding_fields(class)
                            .iter()
                            .filter_map(|&off| r(NVOS64_SIZE + off))
                            .collect();
                        if !named.is_empty() {
                            self.osdesc.made_over(c, o, p, &named);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Free `clients` on `host_fd`, the file they were allocated on, if they
    /// hold memory registered by its pages, and forget what they held. RM
    /// lets go of the pages in the free (the file's own last close may be
    /// later: the event pump holds a duplicate of a watched file), and only
    /// then may the guest unpin them.
    pub(crate) fn osdesc_end_clients(&mut self, host_fd: RawFd, clients: &[u32], why: &str) {
        let holders = self.osdesc.clients();
        let ending: Vec<u32> = clients
            .iter()
            .copied()
            .filter(|c| holders.contains(c))
            .collect();
        if ending.is_empty() {
            return;
        }
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_FREE, NVOS00_SIZE as u32);
        // Only a client RM says it freed is forgotten. One it would not free
        // (on another file, or a failed call) may still hold the pages, and
        // the guest must not be told it may unpin them: its registrations
        // stay, released late -- by a later free RM does confirm, or with the
        // session -- never early.
        let mut freed = Vec::with_capacity(ending.len());
        for &c in &ending {
            // The client names itself.
            let mut p = [0u8; NVOS00_SIZE];
            for at in [NVOS00_H_ROOT, NVOS00_H_OBJECT_OLD] {
                le::put_u32(&mut p, at, c).expect("in the block");
            }
            let mut a = Arena::new();
            let top = a.small(&p);
            let rc = match self.host_call(&mut a, host_fd, request, top) {
                Ok(r) => r,
                Err(e) => -e,
            };
            let status = le::u32_at(a.bytes(top), NVOS00_STATUS).expect("in the block");
            if rc < 0 || status != NV_OK {
                log::warn!(
                    "{why}: freeing RM client {c:#x}, which holds registered guest memory: \
                     rc {rc}, status {status:#x}; its registrations stay"
                );
                continue;
            }
            freed.push(c);
        }
        self.osdesc.forget_clients(&freed);
    }

    /// Whether RM control `param_in` (NVOS54 and its parameters) would hand
    /// an object holding registered memory to an RM export descriptor or an
    /// NV_MEMORY_EXPORT object: a duplicate the backend never sees made or
    /// freed, which NVKMS, nvidia-drm or another RM client can import in
    /// turn. Refused (EPERM) rather than followed.
    pub(super) fn osdesc_export_refused(&self, param_in: &[u8]) -> bool {
        if self.osdesc.live() == 0 {
            return false;
        }
        let (Some(client), Some(cmd), Some(params)) = (
            le::u32_at(param_in, NVOS54_H_CLIENT),
            le::u32_at(param_in, NVOS54_CMD),
            param_in.get(NVOS54_SIZE..),
        ) else {
            return false;
        };
        let Some(named) = crate::osdesc::exported(cmd, params) else {
            return false;
        };
        match named.iter().find(|&&h| self.osdesc.holds(client, h)) {
            Some(h) => {
                log::warn!(
                    "RM control {cmd:#x} refused: it exports {client:#x}/{h:#x}, which holds \
                     guest memory registered by its pages"
                );
                true
            }
            None => false,
        }
    }

    /// Before UVM command `cmd` (`params`, the block the host will get):
    /// ALLOC_DEVICE_P2P of registered memory is refused (it is for video
    /// memory, and its duplicate outlives a UVM_FREE while a CPU mapping of
    /// the range holds it); a MAP_EXTERNAL_ALLOCATION of it, what the
    /// mapping will hold once UVM has made it.
    pub(super) fn osdesc_uvm_before(
        &self,
        cmd: u32,
        params: &[u8],
    ) -> std::result::Result<Option<crate::osdesc::UvmMap>, i32> {
        use crate::osdesc::{UVM_ALLOC_DEVICE_P2P, UVM_MAP_EXTERNAL_ALLOCATION};
        if self.osdesc.live() == 0 {
            return Ok(None);
        }
        let (UVM_MAP_EXTERNAL_ALLOCATION | UVM_ALLOC_DEVICE_P2P) = cmd else {
            return Ok(None);
        };
        // hClient and hMemory follow rmCtrlFd in both.
        let Some(fd_off) = crate::uvmfd::field(self.driver, cmd).map(|f| f.offset as usize) else {
            return Ok(None);
        };
        let rd64 = |o: usize| le::u64_at(params, o);
        let (Some(client), Some(memory)) = (
            le::u32_at(params, fd_off + 4),
            le::u32_at(params, fd_off + 8),
        ) else {
            return Ok(None);
        };
        if !self.osdesc.holds(client, memory) {
            return Ok(None);
        }
        if cmd == UVM_ALLOC_DEVICE_P2P {
            log::warn!(
                "UVM ALLOC_DEVICE_P2P of {client:#x}/{memory:#x} refused: guest memory \
                 registered by its pages"
            );
            return Err(libc::EPERM);
        }
        let (base, len) = (rd64(0).unwrap_or(0), rd64(8).unwrap_or(0));
        let file = self.current_handle;
        self.osdesc
            .uvm_admit(file, self.handles.owner(file), client, memory, base, len)?;
        // UVM_MAP_EXTERNAL_ALLOCATION_PARAMS: base, length, offset, then
        // perGpuAttributes[] at 24 (36 bytes each, the UUID first), and
        // gpuAttributesCount just before rmCtrlFd.
        const ATTRS: usize = 24;
        const ATTR_SIZE: usize = 36;
        let max = fd_off.saturating_sub(8 + ATTRS) / ATTR_SIZE;
        let count = fd_off
            .checked_sub(8)
            .and_then(rd64)
            .unwrap_or(0)
            .min(max as u64) as usize;
        let gpus = (0..count)
            .filter_map(|i| {
                let at = ATTRS + i * ATTR_SIZE;
                params.get(at..at + 16)?.try_into().ok()
            })
            .collect();
        Ok(Some(crate::osdesc::UvmMap {
            base,
            len,
            gpus,
            client,
            memory,
            // rmStatus, after rmCtrlFd, hClient and hMemory.
            status_at: fd_off + 12,
        }))
    }

    /// After UVM command `cmd` on the current handle: what it made, mapped,
    /// unmapped or freed, for the registrations UVM external mappings hold
    /// (osdesc.rs). `params` is the block the host was handed.
    pub(super) fn osdesc_uvm_after(
        &mut self,
        cmd: u32,
        params: &[u8],
        map: Option<crate::osdesc::UvmMap>,
        reply: Option<&[u8]>,
    ) {
        use crate::osdesc::{
            UVM_CREATE_EXTERNAL_RANGE, UVM_MAP_EXTERNAL_ALLOCATION, UVM_UNMAP_EXTERNAL,
        };
        // UVM's own status, where the block keeps it; None when the call
        // failed before UVM answered, or the reply is short.
        let status = |at: usize| le::u32_at(reply?, at);
        let handle = self.current_handle;
        let word = |o: usize| le::u64_at(params, o).unwrap_or(0);
        let size = params.len();
        match cmd {
            UVM_MAP_EXTERNAL_ALLOCATION => {
                let Some(m) = map else { return };
                // Held when UVM made it: NV_OK, or a failure of its wait for
                // the page-table writes, after which it leaves every mapping
                // up -- a channel's RC or ECC error, or the GPU gone
                // (uvm_map_external_allocation, uvm_channel_get_status).
                // Every other failure is answered before anything is mapped,
                // or after tearing down what the call made; and with no
                // answer at all the call never reached UVM. A mapping held
                // that was never made lasts until its range is freed or its
                // file closes (it lies in a recorded range, uvm_admit): late,
                // not early.
                const NV_ERR_ECC_ERROR: u32 = 0x0b;
                const NV_ERR_GPU_IS_LOST: u32 = 0x0f;
                const NV_ERR_RC_ERROR: u32 = 0x60;
                if let Some(0 | NV_ERR_ECC_ERROR | NV_ERR_GPU_IS_LOST | NV_ERR_RC_ERROR) =
                    status(m.status_at)
                {
                    let owner = self.handles.owner(handle);
                    self.osdesc
                        .uvm_mapped(handle, owner, m.base, m.len, &m.gpus, m.client, m.memory);
                }
            }
            UVM_CREATE_EXTERNAL_RANGE if size >= 24 && status(size - 8) == Some(0) => {
                let owner = self.handles.owner(handle);
                self.osdesc
                    .uvm_range_made_by(handle, word(0), word(8), owner);
            }
            crate::uvmmap::FREE if size >= 16 && status(size - 8) == Some(0) => {
                self.osdesc.uvm_range_freed(handle, word(0));
            }
            // UVM_UNMAP_EXTERNAL_PARAMS: base, length, gpuUuid.
            UVM_UNMAP_EXTERNAL if size >= 40 && status(size - 8) == Some(0) => {
                if let Some(gpu) = params.get(16..32).and_then(|g| g.try_into().ok()) {
                    self.osdesc.uvm_unmapped(handle, word(0), word(8), &gpu);
                }
            }
            _ => {}
        }
    }

    /// Take down, on UVM file `host_fd` (guest handle `handle`) while it is
    /// still ours, every external mapping of registered memory it holds, and
    /// forget its ranges: UVM_FREE of each recorded range holding one, then
    /// UNMAP_EXTERNAL of what is left, GPU by GPU. Its last reference may be
    /// dropped later than the close (the event pump holds a duplicate), so
    /// the close itself says nothing of when UVM lets go. What will not come
    /// down stays held until the session ends.
    pub(crate) fn osdesc_uvm_close(&mut self, host_fd: RawFd, handle: u32, why: &str) {
        use crate::osdesc::UVM_UNMAP_EXTERNAL;
        for (base, len) in self.osdesc.uvm_ranges_held(handle) {
            // UVM_FREE_PARAMS: base, then (before 590.44.01) length.
            let ok = self.uvm_call(host_fd, crate::uvmmap::FREE, |b, size| {
                b[0..8].copy_from_slice(&base.to_le_bytes());
                if size >= 24 {
                    b[8..16].copy_from_slice(&len.to_le_bytes());
                }
            });
            if ok {
                self.osdesc.uvm_range_freed(handle, base);
            } else {
                log::warn!("{why}: UVM_FREE of the external range at {base:#x} failed");
            }
        }
        for (base, len, gpu) in self.osdesc.uvm_maps_held(handle) {
            // UVM_UNMAP_EXTERNAL_PARAMS: base, length, gpuUuid.
            let ok = self.uvm_call(host_fd, UVM_UNMAP_EXTERNAL, |b, _| {
                b[0..8].copy_from_slice(&base.to_le_bytes());
                b[8..16].copy_from_slice(&len.to_le_bytes());
                b[16..32].copy_from_slice(&gpu);
            });
            if ok {
                self.osdesc.uvm_unmapped(handle, base, len, &gpu);
            } else {
                log::warn!("{why}: UVM_UNMAP_EXTERNAL of {base:#x}+{len:#x} failed");
            }
        }
        self.osdesc.uvm_file_closed(handle);
    }

    /// Call UVM command `cmd` on `host_fd` with the block the host's release
    /// has for it, filled by `fill` (handed at least 40 bytes, and the
    /// block's size): whether the ioctl and UVM (its status, `size - 8` into
    /// the block, for every command called here) both succeeded.
    pub(super) fn uvm_call(
        &self,
        host_fd: RawFd,
        cmd: u32,
        fill: impl FnOnce(&mut [u8], usize),
    ) -> bool {
        let Some(size) = self
            .driver
            .and_then(abi::schema::uvm_table)
            .and_then(|t| t.lookup(cmd))
            .map(|c| c.size as usize)
            .filter(|&s| s >= 16)
        else {
            return false;
        };
        let mut b = vec![0u8; size.max(40)];
        fill(&mut b, size);
        // Exactly the block UVM copies each way for `cmd` on this release;
        // none of the commands called here holds a pointer.
        let mut a = Arena::new();
        let top = a.small(&b[..size]);
        let rc = self.host_call(&mut a, host_fd, u64::from(cmd), top);
        rc == Ok(0) && a.bytes(top)[size - 8..size - 4] == [0; 4]
    }

    /// OP_OSDESC_REAP: the releases after `ack`, and forget those up to it.
    pub(crate) fn osdesc_reap(&mut self, ack: u64) -> (u64, Vec<u64>) {
        self.osdesc.reap(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::test_ram::*;
    use super::*;
    use crate::hostfd::{IOC_RW, ioc};

    const ALLOC_MEMORY: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC_MEMORY, 56);
    const VID_HEAP: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_VID_HEAP_CONTROL, 184);
    const RM_ALLOC: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 48);

    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn os02(va: u64, size: u64, flags: u32) -> Vec<u8> {
        let mut p = vec![0u8; 56];
        put32(&mut p, NVOS02_H_OBJECT_NEW, 0x5000_0001);
        put32(&mut p, NVOS02_H_CLASS, 0x71);
        put32(&mut p, NVOS02_FLAGS, flags);
        put64(&mut p, NVOS02_P_MEMORY, va);
        put64(&mut p, NVOS02_LIMIT, size - 1);
        p
    }

    #[test]
    fn a_call_says_how_many_pages_rm_pins_and_whether_for_writing() {
        let c = describe(ALLOC_MEMORY, &os02(0x7f00_0000_0000, 2 << 20, 0), &[]).unwrap();
        assert_eq!(
            (c.shape, c.pages(), c.in_page(), c.writable),
            (Shape::AllocMemory, 512, 0, true)
        );
        // Unaligned: the offset inside the first page counts.
        let c = describe(ALLOC_MEMORY, &os02(0x7f00_0000_0ff0, 0x20, 0), &[]).unwrap();
        assert_eq!((c.pages(), c.in_page()), (2, 0xff0));
        // ALLOC_USER_READ_ONLY.
        let c = describe(ALLOC_MEMORY, &os02(0x1000, 1, 1 << 21), &[]).unwrap();
        assert!(!c.writable);

        let mut p = vec![0u8; 184];
        put32(&mut p, NVOS32_FUNCTION, 27);
        put64(&mut p, NVOS32_ALLOC_OS_DESC_DESCRIPTOR, 0x2000);
        put64(&mut p, NVOS32_ALLOC_OS_DESC_LIMIT, 0x2fff);
        put32(&mut p, NVOS32_ALLOC_OS_DESC_ATTR2, 1 << 22);
        let c = describe(VID_HEAP, &p, &[]).unwrap();
        assert_eq!((c.shape, c.pages(), c.writable), (Shape::VidHeap, 3, false));

        let mut o = vec![0u8; 48];
        put32(&mut o, NVOS64_H_CLASS, 0x71);
        put32(&mut o, NVOS64_PARAMS_SIZE, 40);
        let mut n = vec![0u8; 40];
        put64(&mut n, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR, 0x3000);
        put64(&mut n, NV_OS_DESC_MEMORY_ALLOCATION_LIMIT, 0xfff);
        put32(
            &mut n,
            NV_OS_DESC_MEMORY_ALLOCATION_FLAGS,
            NVOS32_ALLOC_FLAGS_USER_READ_ONLY,
        );
        let c = describe(RM_ALLOC, &o, &n).unwrap();
        assert_eq!((c.shape, c.pages(), c.writable), (Shape::RmAlloc, 1, false));
    }

    #[test]
    fn only_the_virtual_address_descriptor_type_is_taken() {
        for t in 1..=7u32 {
            let mut p = vec![0u8; 184];
            put32(&mut p, NVOS32_FUNCTION, 27);
            put32(&mut p, NVOS32_ALLOC_OS_DESC_DESCRIPTOR_TYPE, t);
            assert_eq!(describe(VID_HEAP, &p, &[]), Err(libc::EPERM), "type {t}");
            let mut o = vec![0u8; 48];
            put32(&mut o, NVOS64_H_CLASS, 0x71);
            let mut n = vec![0u8; 40];
            put32(&mut n, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR_TYPE, t);
            assert_eq!(describe(RM_ALLOC, &o, &n), Err(libc::EPERM), "type {t}");
        }
    }

    #[test]
    fn calls_that_are_not_exactly_one_of_the_three_are_refused() {
        // Another class, another function, another size, a limit that wraps.
        let mut p = os02(0x1000, 1, 0);
        put32(&mut p, NVOS02_H_CLASS, 0x3e);
        assert_eq!(describe(ALLOC_MEMORY, &p, &[]), Err(libc::EINVAL));
        assert_eq!(
            describe(
                ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC_MEMORY, 48),
                &os02(0x1000, 1, 0)[..48],
                &[]
            ),
            Err(libc::EINVAL)
        );
        let mut p = os02(0x1000, 1, 0);
        put64(&mut p, NVOS02_LIMIT, u64::MAX);
        assert_eq!(describe(ALLOC_MEMORY, &p, &[]), Err(libc::EINVAL));
        // A handle RM would generate and never write back.
        let mut p = os02(0x1000, 1, 0);
        put32(&mut p, NVOS02_H_OBJECT_NEW, 0);
        assert_eq!(describe(ALLOC_MEMORY, &p, &[]), Err(libc::EINVAL));
        let mut o = vec![0u8; 48];
        put32(&mut o, NVOS64_H_CLASS, 0x71);
        assert_eq!(describe(RM_ALLOC, &o, &[0; 32]), Err(libc::EINVAL));
        // More than one registration may name.
        let p = os02(0, (u64::from(OSDESC_MAX_PAGES) + 1) * PAGE, 0);
        assert_eq!(describe(ALLOC_MEMORY, &p, &[]), Err(libc::E2BIG));
    }

    #[test]
    fn a_page_list_must_cover_exactly_the_pages_rm_pins() {
        let c = describe(ALLOC_MEMORY, &os02(0x10ff0, 0x20, 0), &[]).unwrap();
        let w = OSDESC_F_WRITE;
        assert_eq!(
            parse_runs(&c, &list(w, &[(0x1000, 1), (0x5000, 1)])),
            Ok(vec![(0x1000, 1), (0x5000, 1)])
        );
        for bad in [
            list(w, &[(0x1000, 1)]),              // too few
            list(w, &[(0x1000, 3)]),              // too many
            list(0, &[(0x1000, 2)]),              // read-only for a writing call
            list(w | 2, &[(0x1000, 2)]),          // unknown flag
            list(w, &[(0x1800, 2)]),              // unaligned
            list(w, &[(0x1000, 0), (0x2000, 2)]), // an empty run
            list(w, &[(!0xfff_u64, 2)]),          // wraps
            list(w, &[]),                         // none
        ] {
            assert_eq!(parse_runs(&c, &bad), Err(libc::EINVAL), "{bad:?}");
        }
        let mut odd = list(w, &[(0x1000, 2)]);
        odd.push(0);
        assert_eq!(parse_runs(&c, &odd), Err(libc::EINVAL));
    }

    #[test]
    fn contiguous_pages_are_the_backends_own_mapping_and_scattered_ones_a_range_of_its_own() {
        let ram = ram();
        // Three pages in a row of low RAM: no mapping of its own.
        let r = resolve(&ram, &[(LOW + 5 * PAGE, 3)]).unwrap();
        assert_eq!(r.vmas(), 0);
        let p = r.map(0x10, true).unwrap();
        assert!(matches!(p.mapping, Mapping::Direct { .. }));
        assert_eq!(p.addr, ram.regions()[0].host.addr() + 5 * PAGE + 0x10);

        // Scattered, and across both regions: read through the range the
        // bytes of exactly those pages, in list order.
        let runs = [(HIGH + 7 * PAGE, 2), (LOW + 3 * PAGE, 1), (HIGH, 1)];
        let r = resolve(&ram, &runs).unwrap();
        assert_eq!(r.vmas(), 3);
        let p = r.map(0, false).unwrap();
        let (base, len) = match &p.mapping {
            Mapping::Reserved(r) => (r.addr(), r.len() as u64),
            _ => panic!("not reserved"),
        };
        assert_eq!((p.addr, len), (base, 4 * PAGE));
        let got = p.span.read(0, len as usize);
        let mut want = expect(HIGH + 7 * PAGE, 2 * PAGE);
        want.extend(expect(LOW + 3 * PAGE, PAGE));
        want.extend(expect(HIGH, PAGE));
        assert_eq!(got, &want[..]);
        assert!(reserved_live(base));
        drop(p);
        assert!(!reserved_live(base), "the range goes with the registration");
    }

    #[test]
    fn a_run_across_the_end_of_a_region_is_split_there() {
        // Two regions adjacent in guest-physical space whose file offsets
        // are not: one run over the seam is two pieces.
        let (file, map) = memfd(8);
        let span = map.span();
        let ram = GuestRam::new(
            vec![
                RamRegion {
                    gpa: 0,
                    len: 4 * PAGE,
                    host: span.sub(4 * PAGE, 4 * PAGE).unwrap(),
                    file: Some((file.clone(), 4 * PAGE)),
                },
                RamRegion {
                    gpa: 4 * PAGE,
                    len: 4 * PAGE,
                    host: span.sub(0, 4 * PAGE).unwrap(),
                    file: Some((file, 0)),
                },
            ],
            map,
        );
        let r = resolve(&ram, &[(2 * PAGE, 4)]).unwrap();
        assert_eq!(r.vmas(), 2);
        let p = r.map(0, true).unwrap();
        let got = p.span.read(p.at, 4 * PAGE as usize);
        let want: Vec<u8> = (6 * PAGE..8 * PAGE).chain(0..2 * PAGE).map(byte).collect();
        assert_eq!(got, &want[..]);
    }

    #[test]
    fn a_page_outside_guest_ram_is_refused() {
        let ram = ram();
        // The last page of low RAM and on: nothing follows it at 256 KiB.
        assert_eq!(
            resolve(&ram, &[(LOW + (REGION_PAGES - 1) * PAGE, 2)]).err(),
            Some(libc::EFAULT)
        );
        assert_eq!(resolve(&ram, &[(HIGH - PAGE, 1)]).err(), Some(libc::EFAULT));
        assert_eq!(
            resolve(&ram, &[(HIGH + REGION_PAGES * PAGE, 1)]).err(),
            Some(libc::EFAULT)
        );
    }

    fn pinned(ram: &GuestRam, runs: &[(u64, u64)]) -> Pinned {
        resolve(ram, runs).unwrap().map(0, true).unwrap()
    }

    #[test]
    fn a_registration_ends_with_its_object_its_parent_its_client_and_its_last_duplicate() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1), (HIGH, 1)]));
        let b = o.add(1, 0xc1, 0x11, 0xd1, pinned(&ram, &[(LOW, 1)]));
        let c = o.add(2, 0xc2, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        assert_eq!(o.live(), 3);
        o.freed(0xc1, 0x10);
        assert_eq!(o.reap(0), (1, vec![a]));
        o.freed(0xc1, 0xd1); // b's parent
        assert_eq!(o.reap(1), (2, vec![b]));
        // Duplicated into another client: both handles hold it.
        o.duplicated(0xc2, 0x10, 0xc3, 0x99, 0xd9);
        o.freed(0xc2, 0x10);
        assert_eq!(o.reap(2), (2, vec![]));
        o.freed(0xc3, 0xc3); // the duplicate's client
        assert_eq!(o.reap(2), (3, vec![c]));
        assert_eq!((o.live(), o.bytes, o.vmas), (0, 0, 0));
        assert!(o.per_file.is_empty());
    }

    #[test]
    fn a_reap_names_releases_until_they_are_acknowledged() {
        let ram = ram();
        let mut o = OsDesc::default();
        let ids: Vec<u64> = (0..(OSDESC_REAP_MAX + 3))
            .map(|i| o.add(1, 0xc1, 0x100 + i, 0, pinned(&ram, &[(LOW, 1)])))
            .collect();
        o.forget_clients(&[0xc1]);
        let (last, got) = o.reap(0);
        assert_eq!(got.len(), OSDESC_REAP_MAX as usize);
        // Lost: asked again with the old ack, the same answer.
        assert_eq!(o.reap(0), (last, got.clone()));
        let (last2, rest) = o.reap(last);
        assert_eq!(rest.len(), 3);
        let mut all = got;
        all.extend(rest);
        all.sort();
        assert_eq!(all, ids);
        assert_eq!(o.reap(last2), (last2, vec![]));
        assert_eq!(o.unreaped(), 0);
    }

    #[test]
    fn budgets_count_unreaped_releases_and_are_given_back() {
        let ram = ram();
        let mut o = OsDesc::with_limits(Limits {
            regs_per_vm: 2,
            regs_per_file: 2,
            bytes_per_vm: 3 * PAGE,
            bytes_per_file: 2 * PAGE,
            vmas_per_vm: 2,
            ..Limits::default()
        });
        assert_eq!(o.admit(1, 3 * PAGE, 0), Err(libc::ENOMEM), "bytes per file");
        assert_eq!(o.admit(1, PAGE, 3), Err(libc::ENOMEM), "mappings per VM");
        o.add(1, 0xc1, 1, 0, pinned(&ram, &[(LOW, 1)]));
        o.add(2, 0xc1, 2, 0, pinned(&ram, &[(LOW, 1)]));
        assert_eq!(
            o.admit(3, PAGE, 0),
            Err(libc::ENOMEM),
            "registrations per VM"
        );
        o.freed(0xc1, 1);
        assert_eq!(
            o.admit(3, PAGE, 0),
            Err(libc::ENOMEM),
            "released, not reaped"
        );
        let (last, _) = o.reap(0);
        o.reap(last);
        assert_eq!(o.admit(3, PAGE, 0), Ok(()));
    }

    /// One process's files together hold at most a file's budget (B5).
    #[test]
    fn one_guest_process_holds_at_most_a_files_budget_across_its_files() {
        use crate::quota::Owner;
        let ram = ram();
        let mut o = OsDesc::with_limits(Limits {
            regs_per_vm: 64,
            regs_per_file: 16,
            ..Limits::default()
        });
        let p = |t: u32| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        for f in 1..=4 {
            o.set_file_owner(f, p(1));
        }
        o.set_file_owner(9, p(2));
        let mut n = 0u32;
        'files: for f in 1..=4u32 {
            loop {
                if o.admit(f, PAGE, 0).is_err() {
                    continue 'files;
                }
                n += 1;
                o.add(f, 0xc1, n, 0, pinned(&ram, &[(LOW, 1)]));
            }
        }
        assert_eq!(n, 16, "a file's budget, over four files");
        assert_eq!(o.admit(9, PAGE, 0), Ok(()), "another process has its own");
        // Releases give it back.
        o.freed(0xc1, 1);
        let (last, _) = o.reap(0);
        o.reap(last);
        assert_eq!(o.admit(2, PAGE, 0), Ok(()));
    }

    /// Object 0 names nothing: a second registration keyed by it does not
    /// end the first, and both go with their client.
    #[test]
    fn a_registration_under_no_handle_is_ended_by_nothing_but_its_client() {
        let ram = ram();
        let mut o = OsDesc::default();
        o.add(1, 0xc1, 0, 0, pinned(&ram, &[(LOW, 1)]));
        o.add(1, 0xc1, 0, 0, pinned(&ram, &[(LOW, 1)]));
        assert_eq!(o.reap(0).1, Vec::<u64>::new());
        assert_eq!(o.live(), 2);
        o.forget_clients(&[0xc1]);
        assert_eq!(o.reap(0).1.len(), 2);
    }

    #[test]
    fn a_handle_made_again_ends_what_it_named() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 5, 0, pinned(&ram, &[(LOW, 1)]));
        o.reused(0xc1, 5);
        assert_eq!(o.reap(0).1, vec![a]);
        let b = o.add(1, 0xc1, 6, 0, pinned(&ram, &[(LOW, 1)]));
        let c = o.add(1, 0xc1, 6, 0, pinned(&ram, &[(LOW, 1)]));
        assert_eq!(o.reap(1).1, vec![b]);
        assert_eq!(o.live(), 1);
        o.clear();
        assert_eq!((o.live(), o.unreaped()), (0, 0));
        let _ = c;
    }

    const G1: GpuUuid = [1; 16];
    const G2: GpuUuid = [2; 16];
    const U: crate::quota::Owner = crate::quota::Owner::Unknown;

    #[test]
    fn a_uvm_mapping_holds_a_registration_past_its_handle_until_every_gpu_unmaps_it() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        // Mapped on two GPUs by UVM file 7, at 1 MiB.
        o.uvm_mapped(7, U, 1 << 20, 2 * PAGE, &[G1, G2], 0xc1, 0x10);
        // Memory nothing registered is not held.
        o.uvm_mapped(7, U, 4 << 20, PAGE, &[G1], 0xc1, 0x99);
        assert_eq!(o.uvm_held(), 1);
        o.freed(0xc1, 0x10);
        assert_eq!(o.reap(0), (0, vec![]), "UVM still has it");
        assert!(!o.holds(0xc1, 0x10), "the handle itself is gone");
        // An unmap that covers only part of it, or another file's, or one
        // GPU of two: still held.
        o.uvm_unmapped(7, 1 << 20, PAGE, &G1);
        o.uvm_unmapped(8, 1 << 20, 2 * PAGE, &G1);
        o.uvm_unmapped(7, 1 << 20, 2 * PAGE, &G1);
        assert_eq!((o.live(), o.reap(0).1), (1, vec![]));
        o.uvm_unmapped(7, 0, 8 << 20, &G2);
        assert_eq!(o.reap(0), (1, vec![a]));
        assert_eq!((o.live(), o.uvm_held(), o.bytes), (0, 0, 0));
    }

    #[test]
    fn freeing_the_external_range_takes_the_mappings_inside_it() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        o.uvm_range_made(7, 1 << 20, 4 << 20);
        o.uvm_range_made(7, 8 << 20, 1 << 20);
        o.uvm_mapped(7, U, 2 << 20, PAGE, &[G1], 0xc1, 0x10);
        assert_eq!(o.uvm_ranges_held(7), vec![(1 << 20, 4 << 20)]);
        assert_eq!(o.uvm_maps_held(7), vec![(2 << 20, PAGE, G1)]);
        o.freed(0xc1, 0x10);
        // Another range, a range of another file, a base no range starts at.
        o.uvm_range_freed(7, 8 << 20);
        o.uvm_range_freed(8, 1 << 20);
        o.uvm_range_freed(7, 2 << 20);
        assert_eq!(o.reap(0).1, Vec::<u64>::new());
        o.uvm_range_freed(7, 1 << 20);
        assert_eq!(o.reap(0).1, vec![a]);
        assert_eq!(o.uvm_range_count, 0);
    }

    #[test]
    fn a_uvm_mapping_and_the_handle_both_hold_whichever_goes_first() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        o.uvm_mapped(7, U, 1 << 20, PAGE, &[G1], 0xc1, 0x10);
        o.uvm_unmapped(7, 1 << 20, PAGE, &G1);
        assert_eq!(o.reap(0).1, Vec::<u64>::new(), "RM's handle still holds it");
        // A duplicate mapped holds it as the original would.
        o.duplicated(0xc1, 0x10, 0xc2, 0x20, 0xd2);
        o.uvm_mapped(9, U, 1 << 20, PAGE, &[G1], 0xc2, 0x20);
        o.freed(0xc1, 0xc1);
        o.freed(0xc2, 0xc2);
        assert_eq!(o.reap(0).1, Vec::<u64>::new());
        assert!(o.clients().is_empty());
        assert_eq!(o.uvm_files(), vec![9]);
        o.uvm_unmapped(9, 0, 4 << 20, &G1);
        assert_eq!(o.reap(0).1, vec![a]);
    }

    #[test]
    fn a_uvm_file_that_closes_with_a_mapping_up_holds_it_until_the_session_ends() {
        let ram = ram();
        let mut o = OsDesc::default();
        o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        o.uvm_range_made(7, 1 << 20, 4 << 20);
        o.uvm_mapped(7, U, 1 << 20, PAGE, &[G1], 0xc1, 0x10);
        o.freed(0xc1, 0x10);
        o.uvm_file_closed(7);
        assert!(o.uvm_files().is_empty() && o.uvm_ranges.is_empty());
        // A later file under the same handle ends nothing of the old one.
        o.uvm_range_made(7, 1 << 20, 4 << 20);
        o.uvm_range_freed(7, 1 << 20);
        o.uvm_unmapped(7, 0, 8 << 20, &G1);
        assert_eq!((o.live(), o.uvm_held()), (1, 1));
        o.clear();
        assert_eq!((o.live(), o.uvm_held(), o.unreaped()), (0, 0, 0));
    }

    /// One process's one-page runs, or its external ranges, take no more
    /// than a quarter of the VM's: another process still registers, and
    /// still has its ranges recorded.
    #[test]
    fn one_process_takes_a_quarter_of_the_runs_and_the_ranges() {
        let ram = ram();
        let mut o = OsDesc::with_limits(Limits {
            vmas_per_vm: 64,
            uvm_ranges_per_vm: 64,
            ..Limits::default()
        });
        let (a, b) = (
            crate::quota::Owner::Proc {
                tgid: 1,
                start_ns: 1,
            },
            crate::quota::Owner::Proc {
                tgid: 2,
                start_ns: 1,
            },
        );
        o.set_file_owner(1, a);
        o.set_file_owner(2, b);
        let mut n = 0u32;
        // Two pages apart: two separately mapped runs each.
        while o.admit(1, 2 * PAGE, 2).is_ok() {
            n += 1;
            o.add(1, 0xc1, n, 0, pinned(&ram, &[(LOW, 1), (HIGH, 1)]));
            assert!(n <= 32);
        }
        assert_eq!(n, 8, "a quarter of the 64 runs");
        assert_eq!(
            o.admit(2, 2 * PAGE, 2),
            Ok(()),
            "another process still registers"
        );

        for i in 0..64u64 {
            o.uvm_range_made_by(7, i << 20, 1 << 20, a);
        }
        assert_eq!(o.uvm_range_count, 16);
        o.uvm_range_made_by(8, 1 << 40, 1 << 20, b);
        assert_eq!(o.uvm_range_count, 17, "another process's range is recorded");
        o.uvm_range_freed(7, 0);
        o.uvm_range_made_by(7, 99 << 20, 1 << 20, a);
        assert_eq!(o.uvm_range_count, 17, "freed, one more fits");
        o.uvm_file_closed(7);
        assert_eq!(o.uvm_range_count, 1);
        assert!(!o.uvm_ranges_by_owner.contains_key(&a));
    }

    #[test]
    fn uvm_mappings_and_ranges_are_bounded() {
        let ram = ram();
        let mut o = OsDesc::with_limits(Limits {
            uvm_maps_per_vm: 1,
            uvm_ranges_per_vm: 1,
            ..Limits::default()
        });
        o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        o.uvm_range_made(7, 1 << 20, 1 << 20);
        o.uvm_range_made(7, 4 << 20, 1 << 20);
        assert_eq!(o.uvm_range_count, 1);
        let admit = |o: &OsDesc, memory| o.uvm_admit(7, U, 0xc1, memory, 1 << 20, PAGE);
        assert_eq!(admit(&o, 0x10), Ok(()));
        o.uvm_mapped(7, U, 1 << 20, PAGE, &[G1], 0xc1, 0x10);
        assert_eq!(admit(&o, 0x10), Err(libc::ENOMEM));
        assert_eq!(admit(&o, 0x99), Ok(()), "memory nothing registered");
    }

    /// A mapping of registered memory is held until its range is freed or
    /// its file closes, so it must lie in a range the backend recorded: a
    /// guest calling MAP_EXTERNAL_ALLOCATION anywhere else would leave
    /// holds that nothing takes down, and one process that filled the VM's
    /// bound with them would refuse every other's.
    #[test]
    fn a_uvm_mapping_of_registered_memory_lies_in_a_recorded_range_and_a_process_share() {
        use crate::quota::Owner;
        let ram = ram();
        let mut o = OsDesc::with_limits(Limits {
            uvm_maps_per_vm: 64,
            ..Limits::default()
        });
        o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        let (a, b) = (
            Owner::Proc {
                tgid: 1,
                start_ns: 1,
            },
            Owner::Proc {
                tgid: 2,
                start_ns: 1,
            },
        );
        o.uvm_range_made(7, 1 << 20, 4 << 20);
        o.uvm_range_made(8, 1 << 20, 4 << 20);
        let admit =
            |o: &OsDesc, file, owner, base, len| o.uvm_admit(file, owner, 0xc1, 0x10, base, len);
        // Outside every range, across a range's end, in another file's, or
        // wrapping.
        assert_eq!(admit(&o, 7, a, 8 << 20, PAGE), Err(libc::EINVAL));
        assert_eq!(admit(&o, 7, a, 4 << 20, 2 << 20), Err(libc::EINVAL));
        assert_eq!(admit(&o, 9, a, 1 << 20, PAGE), Err(libc::EINVAL));
        assert_eq!(admit(&o, 7, a, 1 << 20, u64::MAX), Err(libc::EINVAL));
        assert_eq!(admit(&o, 7, a, 1 << 20, 4 << 20), Ok(()));
        // A quarter of the VM's bound per process.
        let mut made = 0;
        while admit(&o, 7, a, 1 << 20, PAGE).is_ok() {
            o.uvm_mapped(7, a, 1 << 20, PAGE, &[G1], 0xc1, 0x10);
            made += 1;
        }
        assert_eq!(made, 16);
        assert_eq!(admit(&o, 7, a, 1 << 20, PAGE), Err(libc::ENOMEM));
        assert_eq!(admit(&o, 8, b, 1 << 20, PAGE), Ok(()), "another process");
        // Freeing the range takes them all.
        o.uvm_range_freed(7, 1 << 20);
        assert_eq!(o.uvm_held(), 0);
        // A hold whose end would wrap lies in no range, and frees nothing
        // it does not.
        o.uvm_range_made(7, 1 << 20, 4 << 20);
        o.uvm_mapped(7, a, u64::MAX - 8, PAGE, &[G1], 0xc1, 0x10);
        o.uvm_range_freed(7, 1 << 20);
        assert_eq!(o.uvm_held(), 1);
    }

    #[test]
    fn a_semaphore_surface_over_registered_memory_holds_it_and_so_does_a_mapper_over_that() {
        let ram = ram();
        let mut o = OsDesc::default();
        let a = o.add(1, 0xc1, 0x10, 0xd0, pinned(&ram, &[(LOW, 1)]));
        let b = o.add(1, 0xc1, 0x11, 0xd0, pinned(&ram, &[(LOW, 1)]));
        // A semaphore surface naming both, under subdevice 0xd5.
        o.made_over(0xc1, 0x20, 0xd5, &[0x10, 0x11]);
        // A memory mapper naming the surface; one naming nothing registered.
        o.made_over(0xc1, 0x30, 0xd5, &[0x20]);
        o.made_over(0xc1, 0x31, 0xd5, &[0x77]);
        assert!(o.holds(0xc1, 0x30) && !o.holds(0xc1, 0x31));
        o.freed(0xc1, 0x10);
        o.freed(0xc1, 0x11);
        o.freed(0xc1, 0x20);
        assert_eq!(o.reap(0).1, Vec::<u64>::new());
        o.freed(0xc1, 0x30);
        let mut got = o.reap(0).1;
        got.sort();
        assert_eq!(got, vec![a, b]);
        // Freeing the parent a surface was made under ends it too.
        let c = o.add(1, 0xc1, 0x12, 0xd0, pinned(&ram, &[(LOW, 1)]));
        o.made_over(0xc1, 0x21, 0xd5, &[0x12]);
        o.freed(0xc1, 0x12);
        o.freed(0xc1, 0xd5);
        assert_eq!(o.reap(2).1, vec![c]);
    }

    #[test]
    fn the_objects_an_export_control_names_are_read_where_rm_reads_them() {
        let mut p = vec![0u8; 24];
        put32(&mut p, 12, 0x10);
        assert_eq!(exported(EXPORT_OBJECT_TO_FD, &p), Some(vec![0x10]));
        let mut p = vec![0u8; 2128];
        put32(&mut p, 76, 0x10);
        put32(&mut p, 80, 0x11);
        put32(&mut p, 84, 0x12);
        p[2124..2126].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(exported(EXPORT_OBJECTS_TO_FD, &p), Some(vec![0x10, 0x11]));
        let mut p = vec![0u8; 1048];
        put32(&mut p, 8, 0x10);
        p[1032..1034].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(exported(EXPORT_MEM, &p), Some(vec![0x10]));
        // A block of another layout names all it holds.
        assert_eq!(
            exported(EXPORT_MEM, &[1, 0, 0, 0, 2, 0, 0, 0]),
            Some(vec![1, 2])
        );
        assert_eq!(exported(0x2080_0101, &p), None);
    }
}

/// Through the whole v1 path, against a fake guest memory (a GuestMemoryMmap
/// over a memfd of known contents, as vhost-user hands the backend one) and
/// a fake host that reads what RM would pin through the address it is
/// handed: every byte must be the guest's, from exactly the pages the list
/// named, in order.
#[cfg(all(test, feature = "vhost-user"))]
mod backend_tests {
    use super::test_ram::{byte, list, mapped};
    use super::*;
    use crate::hostfd::{HandleKind, IOC_RW, ioc};
    use crate::nvidia::NvidiaBackend;
    use protocol::messages::{
        BCAP_OS_DESC, DEEP_PAGE_LIST, DeviceKind, HELLO_F_FRESH, HelloReq, HostOpReq, HostOpResp,
        IoctlResp, MsgHeader, MsgType, OP_OSDESC_REAP, PROTO_V2,
    };
    use std::cell::{Cell, RefCell};
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::FileExt;
    use vm_memory::{FileOffset, GuestAddress, GuestMemoryMmap};

    const ALLOC_MEMORY: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC_MEMORY, 56);
    const VID_HEAP: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_VID_HEAP_CONTROL, 184);
    const RM_ALLOC: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 48);
    const RM_FREE: u32 = ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_FREE, 16);
    const CONTROL: u32 = ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_CONTROL, 32);

    /// The caller's address: never mapped here, so the host must never be
    /// handed it.
    const GUEST_VA: u64 = 0x4141_4140_0000;
    const CLIENT: u32 = 0xc1d0_0001;
    const DEVICE: u32 = 0xde00_0001;
    const LOW: u64 = 0;
    const HIGH: u64 = 1 << 32;
    const PAGES: u64 = 64;

    #[derive(Debug, Clone, PartialEq)]
    enum Seen {
        /// One of the three calls: the address RM was handed, the bytes RM
        /// would pin through it (`limit + 1` from there), and the fields the
        /// guest's values must not reach.
        Register {
            nr: u32,
            addr: u64,
            bytes: Vec<u8>,
            fd: i32,
            rights: u64,
        },
        Free {
            client: u32,
            object: u32,
        },
        /// RM_ALLOC of any other class.
        Alloc {
            class: u32,
        },
        Control {
            cmd: u32,
        },
        /// MAP_MEMORY_DMA: the flags RM was handed.
        MapDma {
            flags: u32,
        },
        /// A UVM command, and the first word of its block.
        Uvm {
            cmd: u32,
            base: u64,
        },
    }

    std::thread_local! {
        static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
        /// RM's answer to the next registrations.
        static STATUS: Cell<u32> = const { Cell::new(0) };
        /// RM's answer to the next frees.
        static FREE_STATUS: Cell<u32> = const { Cell::new(0) };
        /// UVM's answer to the next UVM commands.
        static UVM_STATUS: Cell<u32> = const { Cell::new(0) };
    }

    fn seen() -> Vec<Seen> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn rd32(a: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(a[o..o + 4].try_into().unwrap())
    }
    fn rd64(a: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(a[o..o + 8].try_into().unwrap())
    }
    fn put32(a: &mut [u8], o: usize, v: u32) {
        a[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(a: &mut [u8], o: usize, v: u64) {
        a[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Read what RM would pin: `size` bytes from `addr`, which must not be
    /// the guest's, and must be memory the call handed the host.
    fn pinned_bytes(others: &crate::sys::block::Others<'_>, addr: u64, size: u64) -> Vec<u8> {
        assert!(
            addr.abs_diff(GUEST_VA) > 1 << 30,
            "the guest's address reached RM: {addr:#x}"
        );
        others
            .read(addr, size as usize)
            .expect("RM was handed an address the call did not map for it")
    }

    fn fake_host(
        _fd: std::os::fd::RawFd,
        request: u64,
        arg: &mut crate::sys::block::Arg<'_>,
    ) -> i32 {
        let request = request as u32;
        let (a, others) = arg.split();
        if hostfd::ioc_type(request) == 0 {
            // UVM: the number alone, no size; every block called here ends
            // with its status 8 bytes from the end.
            let size = abi::schema::uvm_table(abi::version::DriverVersion::parse(DRIVER).unwrap())
                .and_then(|t| t.lookup(request))
                .map(|c| c.size as usize)
                .expect("a UVM command of the table");
            let a = &mut a[..size];
            let st = if request == UVM_MAP_EXTERNAL_ALLOCATION {
                size - 4
            } else {
                size - 8
            };
            put32(a, st, UVM_STATUS.with(|s| s.get()));
            SEEN.with(|v| {
                v.borrow_mut().push(Seen::Uvm {
                    cmd: request,
                    base: rd64(a, 0),
                })
            });
            return 0;
        }
        let len = hostfd::ioc_size(request);
        let a = &mut a[..len];
        let status = STATUS.with(|s| s.get());
        let s = match hostfd::ioc_nr(request) {
            NV_ESC_RM_ALLOC_MEMORY => {
                let addr = rd64(a, NVOS02_P_MEMORY);
                let bytes = pinned_bytes(&others, addr, rd64(a, 32) + 1);
                put32(a, NVOS02_STATUS, status);
                Seen::Register {
                    nr: NV_ESC_RM_ALLOC_MEMORY,
                    addr,
                    bytes,
                    fd: rd32(a, NVOS02_WITH_FD_FD) as i32,
                    rights: 0,
                }
            }
            NV_ESC_RM_VID_HEAP_CONTROL => {
                let addr = rd64(a, NVOS32_ALLOC_OS_DESC_DESCRIPTOR);
                let bytes = pinned_bytes(&others, addr, rd64(a, 72) + 1);
                put32(a, NVOS32_STATUS, status);
                if rd32(a, NVOS32_ALLOC_OS_DESC_H_MEMORY) == 0 {
                    put32(a, NVOS32_ALLOC_OS_DESC_H_MEMORY, 0xbeef_0001);
                }
                Seen::Register {
                    nr: NV_ESC_RM_VID_HEAP_CONTROL,
                    addr,
                    bytes,
                    fd: 0,
                    rights: 0,
                }
            }
            NV_ESC_RM_ALLOC if rd32(a, NVOS64_H_CLASS) != NV01_MEMORY_SYSTEM_OS_DESCRIPTOR => {
                put32(a, NVOS64_STATUS, 0);
                Seen::Alloc {
                    class: rd32(a, NVOS64_H_CLASS),
                }
            }
            abi::ioctl::NV_ESC_RM_CONTROL => {
                put32(a, 28, 0);
                Seen::Control { cmd: rd32(a, 8) }
            }
            abi::ioctl::NV_ESC_RM_MAP_MEMORY_DMA => {
                put32(a, 56, 0);
                Seen::MapDma { flags: rd32(a, 32) }
            }
            NV_ESC_RM_ALLOC => {
                let params = rd64(a, NVOS64_P_ALLOC_PARMS);
                assert!(params != 0 && params.abs_diff(GUEST_VA) > 1 << 30);
                // The backend's copy of the class parameters.
                let n = others.read(params, 40).expect("the class parameters");
                let addr = rd64(&n, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR);
                let bytes = pinned_bytes(&others, addr, rd64(&n, 24) + 1);
                put32(a, NVOS64_STATUS, status);
                Seen::Register {
                    nr: NV_ESC_RM_ALLOC,
                    addr,
                    bytes,
                    fd: 0,
                    rights: rd64(a, NVOS64_P_RIGHTS_REQUESTED),
                }
            }
            abi::ioctl::NV_ESC_RM_FREE => {
                put32(a, 12, FREE_STATUS.with(|s| s.get()));
                Seen::Free {
                    client: rd32(a, 0),
                    object: rd32(a, 8),
                }
            }
            nr => panic!("the fake host has no escape {nr:#x}"),
        };
        SEEN.with(|v| v.borrow_mut().push(s));
        0
    }

    /// Guest memory as vhost-user gives it: low RAM at 0 and high RAM at
    /// 4 GiB, both from one memfd, 64 pages each, holding `byte`.
    fn guest_memory() -> GuestRam {
        let file = File::from(crate::sys::fd::memfd(c"osdesc-guest", libc::MFD_CLOEXEC).unwrap());
        let len = 2 * PAGES * PAGE;
        let bytes: Vec<u8> = (0..len).map(byte).collect();
        file.write_all_at(&bytes, 0).unwrap();
        let file = Arc::new(file);
        let mem = GuestMemoryMmap::from_ranges_with_files([
            (
                GuestAddress(LOW),
                (PAGES * PAGE) as usize,
                Some(FileOffset::from_arc(file.clone(), 0)),
            ),
            (
                GuestAddress(HIGH),
                (PAGES * PAGE) as usize,
                Some(FileOffset::from_arc(file, PAGES * PAGE)),
            ),
        ])
        .unwrap();
        GuestRam::from_vm_memory(Arc::new(mem))
    }

    /// What guest memory holds at `gpa`.
    fn at(gpa: u64, len: u64) -> Vec<u8> {
        let off = if gpa >= HIGH {
            gpa - HIGH + PAGES * PAGE
        } else {
            gpa
        };
        (off..off + len).map(byte).collect()
    }

    fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
        let mut req = Vec::new();
        for v in [t as u32, handle, 0, 0x55] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(body);
        // Room for UVM_MAP_EXTERNAL_ALLOCATION's 9,264 bytes.
        let mut resp = vec![0u8; 16384];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn status(resp: &[u8]) -> i32 {
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    }

    struct Vm {
        be: NvidiaBackend,
        ctl: u32,
        gpu: u32,
    }

    /// A v2 session with guest RAM, a control file and a GPU file, and the
    /// client allocated on the control file.
    fn vm() -> Vm {
        seen();
        STATUS.with(|s| s.set(0));
        FREE_STATUS.with(|s| s.set(0));
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_host);
        be.set_guest_ram(Some(guest_memory()));
        be.config_mut().allow_compute = true;
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: 0,
            uvm_aperture_mib: 0,
        };
        let hb = crate::sys::pod::bytes(&hello);
        let r = call(&mut be, MsgType::Hello, 0, hb);
        assert_eq!(status(&r), 0);
        assert_ne!(rd32(&r, 16 + 4) & BCAP_OS_DESC, 0, "offered with guest RAM");
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let ctl = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        let gpu = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Gpu(0)));
        be.semsurf.client_allocated(ctl, CLIENT);
        Vm { be, ctl, gpu }
    }

    /// A v1 IOCTL with a page list; (status, reply header, parameters,
    /// deep block).
    fn ioctl(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: Option<&[u8]>,
    ) -> (i32, IoctlResp, Vec<u8>, Vec<u8>) {
        let mut body = Vec::new();
        let deep_bytes = deep.unwrap_or(&[]);
        for v in [
            cmd,
            outer.len() as u32,
            if nested.is_empty() {
                0
            } else {
                outer.len() as u32
            },
            nested.len() as u32,
            if deep.is_some() { DEEP_PAGE_LIST } else { 0 },
            deep_bytes.len() as u32,
        ] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(outer);
        body.extend_from_slice(nested);
        body.extend_from_slice(deep_bytes);
        let r = call(be, MsgType::Ioctl, handle, &body);
        let st = status(&r);
        let mut resp = IoctlResp::default();
        let h = size_of::<MsgHeader>();
        if r.len() >= h + 12 {
            (resp.data_len, resp.nested_len, resp.deep_len) =
                (rd32(&r, h), rd32(&r, h + 4), rd32(&r, h + 8));
        }
        let at = (h + 12).min(r.len());
        let params_end = at + (resp.data_len + resp.nested_len) as usize;
        let params = r.get(at..params_end).unwrap_or(&[]).to_vec();
        let d = r.get(params_end..).unwrap_or(&[]).to_vec();
        (st, resp, params, d)
    }

    fn os02(va: u64, size: u64, fd: i32) -> Vec<u8> {
        let mut p = vec![0u8; 56];
        put32(&mut p, 0, CLIENT);
        put32(&mut p, 4, DEVICE);
        put32(&mut p, 8, 0x5000_0001);
        put32(&mut p, 12, 0x71);
        put64(&mut p, NVOS02_P_MEMORY, va);
        put64(&mut p, 32, size - 1);
        put32(&mut p, NVOS02_WITH_FD_FD, fd as u32);
        p
    }

    fn os32(va: u64, size: u64, attr2: u32, dtype: u32) -> Vec<u8> {
        let mut p = vec![0u8; 184];
        put32(&mut p, 0, CLIENT);
        put32(&mut p, 4, DEVICE);
        put32(&mut p, 8, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
        put32(&mut p, 56, attr2);
        put64(&mut p, NVOS32_ALLOC_OS_DESC_DESCRIPTOR, va);
        put64(&mut p, 72, size - 1);
        put32(&mut p, 80, dtype);
        p
    }

    fn os64() -> Vec<u8> {
        let mut o = vec![0u8; 48];
        put32(&mut o, 0, CLIENT);
        put32(&mut o, 4, DEVICE);
        put32(&mut o, 8, 0x5000_0002);
        put32(&mut o, 12, 0x71);
        put64(&mut o, NVOS64_P_ALLOC_PARMS, GUEST_VA + (8 << 20));
        put64(&mut o, NVOS64_P_RIGHTS_REQUESTED, GUEST_VA + (9 << 20));
        put32(&mut o, 32, 40);
        o
    }

    fn osd(va: u64, size: u64, attr2: u32) -> Vec<u8> {
        let mut n = vec![0u8; 40];
        put32(&mut n, 12, attr2);
        put64(&mut n, NV_OS_DESC_MEMORY_ALLOCATION_DESCRIPTOR, va);
        put64(&mut n, 24, size - 1);
        n
    }

    fn reap(be: &mut NvidiaBackend, ack: u64) -> (u64, Vec<u64>) {
        let mut req = HostOpReq {
            op: OP_OSDESC_REAP,
            nargs: 1,
            ..HostOpReq::default()
        };
        req.args[0] = ack;
        let b = crate::sys::pod::bytes(&req);
        let r = call(be, MsgType::HostOp, 0, b);
        assert_eq!(status(&r), 0);
        let h = size_of::<MsgHeader>();
        let n = rd64(&r, h + 16) as usize;
        let at = h + size_of::<HostOpResp>();
        (
            rd64(&r, h + 8),
            (0..n).map(|i| rd64(&r, at + 8 * i)).collect(),
        )
    }

    /// A file whose registration was refused before anything was
    /// registered leaves no owner behind when it closes: a loop of open,
    /// refused registration, close does not grow the backend's memory.
    #[test]
    fn a_closed_file_with_no_registration_keeps_no_owner() {
        let mut vm = vm();
        let owner = crate::quota::Owner::Proc {
            tgid: 1,
            start_ns: 1,
        };
        for i in 0..64u32 {
            let null = std::fs::File::open("/dev/null").unwrap().into();
            let h = vm
                .be
                .adopt_for_test_as(null, HandleKind::Dev(DeviceKind::Gpu(0)), owner);
            // A page list that describes nothing: refused.
            let (st, ..) = ioctl(
                &mut vm.be,
                h,
                ALLOC_MEMORY,
                &os02(GUEST_VA, 3 * PAGE, 7),
                &[],
                Some(&[0u8; 8]),
            );
            assert_ne!(st, 0, "{i}");
            vm.be.close_handle(h).unwrap();
        }
        assert!(vm.be.osdesc.file_owners.is_empty());
    }

    /// Three pages in a row of low RAM: RM is handed the backend's own
    /// mapping of them, reads exactly their bytes, and the caller reads back
    /// its address and its descriptor, and the registration's id.
    #[test]
    fn contiguous_pages_reach_rm_as_the_backends_mapping_of_exactly_them() {
        let mut vm = vm();
        let gpu = vm.gpu;
        let host = vm.be.guest_ram.as_ref().unwrap().regions()[0].host.addr();
        let size = 3 * PAGE;
        let (st, resp, params, deep) = ioctl(
            &mut vm.be,
            gpu,
            ALLOC_MEMORY,
            &os02(GUEST_VA, size, 7),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(LOW + 5 * PAGE, 3)])),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [
                Seen::Register {
                    addr, bytes, fd, ..
                },
            ] => {
                assert_eq!(*addr, host + 5 * PAGE);
                assert_eq!(bytes, &at(LOW + 5 * PAGE, size));
                assert_eq!(
                    *fd, -1,
                    "the guest's descriptor number never reaches the host"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!((resp.data_len, resp.nested_len, resp.deep_len), (56, 0, 8));
        assert_eq!(rd64(&params, NVOS02_P_MEMORY), GUEST_VA);
        assert_eq!(rd32(&params, NVOS02_WITH_FD_FD), 7);
        assert_eq!(vm.be.osdesc.live(), 1);
        let id = rd64(&deep, 0);
        // RM_FREE of it: released, and a reap names it.
        let mut f = vec![0u8; 16];
        put32(&mut f, 0, CLIENT);
        put32(&mut f, 8, 0x5000_0001);
        let ctl = vm.ctl;
        let (st, ..) = ioctl(&mut vm.be, ctl, RM_FREE, &f, &[], None);
        assert_eq!(st, 0);
        assert_eq!(reap(&mut vm.be, 0), (1, vec![id]));
        assert_eq!(reap(&mut vm.be, 1), (1, vec![]));
    }

    /// A new client whose handle is the number of a registration's object
    /// is no new object of the registration's client: RM ignores hRoot for
    /// the root classes, and the registration, still pinned by RM, stays.
    #[test]
    fn a_client_allocated_under_a_registrations_handle_leaves_it_registered() {
        let mut vm = vm();
        let gpu = vm.gpu;
        let (st, ..) = ioctl(
            &mut vm.be,
            gpu,
            ALLOC_MEMORY,
            &os02(GUEST_VA, PAGE, 7),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(LOW + 5 * PAGE, 1)])),
        );
        assert_eq!(st, 0);
        seen();
        for class in ROOT_CLASSES {
            let mut o = vec![0u8; 48];
            put32(&mut o, 0, CLIENT);
            put32(&mut o, 8, 0x5000_0001);
            put32(&mut o, 12, class);
            let ctl = vm.ctl;
            let (st, ..) = ioctl(&mut vm.be, ctl, RM_ALLOC, &o, &[], None);
            assert_eq!(st, 0);
        }
        assert_eq!(vm.be.osdesc.live(), 1, "still registered");
        assert_eq!(reap(&mut vm.be, 0), (0, vec![]), "nothing to unpin");
        // Another class at that handle does end it: RM made a new object
        // there, so the old one is gone.
        let mut o = vec![0u8; 48];
        put32(&mut o, 0, CLIENT);
        put32(&mut o, 4, DEVICE);
        put32(&mut o, 8, 0x5000_0001);
        put32(&mut o, 12, 0x3e);
        let ctl = vm.ctl;
        let (st, ..) = ioctl(&mut vm.be, ctl, RM_ALLOC, &o, &[], None);
        assert_eq!(st, 0);
        assert_eq!(reap(&mut vm.be, 0).1.len(), 1);
    }

    /// A registration whose reply would not fit what the guest posted is
    /// refused before RM is asked, not after RM has pinned the pages.
    #[test]
    fn a_registration_with_no_room_for_its_reply_never_reaches_rm() {
        let mut vm = vm();
        let gpu = vm.gpu;
        let mut body = Vec::new();
        let outer = os02(GUEST_VA, PAGE, 7);
        let l = list(OSDESC_F_WRITE, &[(LOW + 5 * PAGE, 1)]);
        for v in [
            ALLOC_MEMORY,
            outer.len() as u32,
            0,
            0,
            DEEP_PAGE_LIST,
            l.len() as u32,
        ] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&outer);
        body.extend_from_slice(&l);
        let mut req = Vec::new();
        for v in [MsgType::Ioctl as u32, gpu, 0, 0x55] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&body);
        // Room for the struct, not for the registration's id.
        let mut resp = vec![0u8; size_of::<MsgHeader>() + size_of::<IoctlResp>() + 56 + 4];
        vm.be.dispatch(&req, &mut resp);
        assert_eq!(status(&resp), -libc::ENOSPC);
        assert!(seen().is_empty(), "RM was never asked");
        assert_eq!(vm.be.osdesc.live(), 0);
    }

    /// Scattered pages across both regions, from an address inside its
    /// first page: RM is handed a range of the backend's own that maps those
    /// pages in list order, at the caller's offset, and reads the guest's
    /// bytes through it; RM_FREE of the object unmaps it.
    #[test]
    fn scattered_pages_reach_rm_mapped_in_order_at_the_callers_offset() {
        let mut vm = vm();
        let ctl = vm.ctl;
        let (off, size) = (0x234, 2 * PAGE);
        let runs = [(HIGH + 7 * PAGE, 1), (LOW + 2 * PAGE, 1), (HIGH + PAGE, 1)];
        let (st, _, params, deep) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA + off, size, 0, 0),
            &[],
            Some(&list(OSDESC_F_WRITE, &runs)),
        );
        assert_eq!(st, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, bytes, .. }] => {
                let mut whole = Vec::new();
                for (gpa, _) in runs {
                    whole.extend(at(gpa, PAGE));
                }
                assert_eq!(addr % PAGE, off);
                assert_eq!(bytes, &whole[off as usize..(off + size) as usize]);
                *addr
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            rd64(&params, NVOS32_ALLOC_OS_DESC_DESCRIPTOR),
            GUEST_VA + off
        );
        let h_memory = rd32(&params, NVOS32_ALLOC_OS_DESC_H_MEMORY);
        assert_eq!(h_memory, 0xbeef_0001);
        let id = rd64(&deep, 0);
        assert!(reserved_live(addr) && mapped(addr, size));
        // Freeing the device it was made under frees it too.
        let mut f = vec![0u8; 16];
        put32(&mut f, 0, CLIENT);
        put32(&mut f, 8, DEVICE);
        assert_eq!(ioctl(&mut vm.be, ctl, RM_FREE, &f, &[], None).0, 0);
        assert!(!reserved_live(addr), "the reserved range is gone");
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// RM_ALLOC of the class: the class parameters reach RM as a block of
    /// ours naming our range, read-only here, and the caller reads back its
    /// own pointers. (RM itself answers NV_ERR_NOT_SUPPORTED to this form.)
    #[test]
    fn rm_alloc_reaches_rm_with_our_parameters_and_our_range() {
        let mut vm = vm();
        let ctl = vm.ctl;
        let (st, resp, params, deep) = ioctl(
            &mut vm.be,
            ctl,
            RM_ALLOC,
            &os64(),
            &osd(GUEST_VA, PAGE, 1 << 22),
            Some(&list(0, &[(HIGH + 9 * PAGE, 1)])),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [Seen::Register { bytes, rights, .. }] => {
                assert_eq!(bytes, &at(HIGH + 9 * PAGE, PAGE));
                assert_eq!(*rights, 0);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!((resp.data_len, resp.nested_len, resp.deep_len), (48, 40, 8));
        assert_eq!(&params[..48], &os64()[..], "the caller's own block");
        assert_eq!(&params[48..], &osd(GUEST_VA, PAGE, 1 << 22)[..]);
        assert_eq!(deep.len(), 8);
    }

    /// Without --allow-compute, BCAP_OS_DESC is never offered, and a page
    /// list that comes anyway never reaches RM: the surface is what it was
    /// before registration by pages existed (an address alone is refused as
    /// it always was).
    #[test]
    fn without_allow_compute_memory_is_never_registered_by_its_pages() {
        let mut vm = vm();
        vm.be.config_mut().allow_compute = false;
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: 0,
            guest_caps: 0,
            uvm_aperture_mib: 0,
        };
        let hb = crate::sys::pod::bytes(&hello);
        let r = call(&mut vm.be, MsgType::Hello, 0, hb);
        assert_eq!(status(&r), 0);
        assert_eq!(rd32(&r, 16 + 4) & BCAP_OS_DESC, 0, "guest RAM or not");
        let ctl = vm.ctl;
        seen();
        let (st, ..) = ioctl(
            &mut vm.be,
            ctl,
            RM_ALLOC,
            &os64(),
            &osd(GUEST_VA, PAGE, 1 << 22),
            Some(&list(0, &[(HIGH + 9 * PAGE, 1)])),
        );
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty(), "RM never called");
        let (st, ..) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA, 2 * PAGE, 0, 0),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(LOW, 1), (HIGH, 1)])),
        );
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    /// RM refused: nothing is registered, the range goes at once, and the
    /// reply has no id.
    #[test]
    fn a_registration_rm_refuses_leaves_nothing_behind() {
        let mut vm = vm();
        let ctl = vm.ctl;
        STATUS.with(|s| s.set(0x56));
        let (st, resp, params, _) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA, 2 * PAGE, 0, 0),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(LOW, 1), (HIGH, 1)])),
        );
        assert_eq!(st, 0);
        assert_eq!(rd32(&params, NVOS32_STATUS), 0x56);
        assert_eq!(resp.deep_len, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, .. }] => *addr,
            other => panic!("{other:?}"),
        };
        assert!(!reserved_live(addr));
        assert_eq!(vm.be.osdesc.live(), 0);
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
    }

    #[test]
    fn a_page_that_is_not_guest_ram_never_reaches_rm() {
        let mut vm = vm();
        let ctl = vm.ctl;
        // 256 KiB is past low RAM; 3 GiB is the PCI hole, where a window
        // or an aperture would be.
        for gpa in [PAGES * PAGE, 3 << 30, HIGH + PAGES * PAGE, !(PAGE - 1)] {
            let (st, ..) = ioctl(
                &mut vm.be,
                ctl,
                VID_HEAP,
                &os32(GUEST_VA, 2 * PAGE, 0, 0),
                &[],
                Some(&list(OSDESC_F_WRITE, &[(LOW, 1), (gpa, 1)])),
            );
            assert!(st == -libc::EFAULT || st == -libc::EINVAL, "{gpa:#x}: {st}");
        }
        assert!(seen().is_empty());
        assert_eq!(vm.be.osdesc.live(), 0);
    }

    #[test]
    fn a_descriptor_that_is_not_a_virtual_address_never_reaches_rm() {
        let mut vm = vm();
        let ctl = vm.ctl;
        for dtype in [1, 2, 3, 4, 5, 6, 7] {
            let (st, ..) = ioctl(
                &mut vm.be,
                ctl,
                VID_HEAP,
                &os32(GUEST_VA, PAGE, 0, dtype),
                &[],
                Some(&list(OSDESC_F_WRITE, &[(LOW, 1)])),
            );
            assert_eq!(st, -libc::EPERM, "type {dtype}");
            let mut n = osd(GUEST_VA, PAGE, 0);
            put32(&mut n, 32, dtype);
            let (st, ..) = ioctl(
                &mut vm.be,
                ctl,
                RM_ALLOC,
                &os64(),
                &n,
                Some(&list(OSDESC_F_WRITE, &[(LOW, 1)])),
            );
            assert_eq!(st, -libc::EPERM, "type {dtype}");
        }
        assert!(seen().is_empty());
    }

    /// An old guest sends the address alone: refused on all three, as
    /// before. And a page list the backend never offered to take, on a call
    /// that registers nothing, or disagreeing with the call, is refused too.
    #[test]
    fn an_address_alone_and_a_list_out_of_place_never_reach_rm() {
        let mut vm = vm();
        let (ctl, gpu) = (vm.ctl, vm.gpu);
        assert_eq!(
            ioctl(
                &mut vm.be,
                gpu,
                ALLOC_MEMORY,
                &os02(GUEST_VA, PAGE, -1),
                &[],
                None
            )
            .0,
            -libc::EPERM
        );
        assert_eq!(
            ioctl(
                &mut vm.be,
                ctl,
                VID_HEAP,
                &os32(GUEST_VA, PAGE, 0, 0),
                &[],
                None
            )
            .0,
            -libc::EPERM
        );
        assert_eq!(
            ioctl(
                &mut vm.be,
                ctl,
                RM_ALLOC,
                &os64(),
                &osd(GUEST_VA, PAGE, 0),
                None
            )
            .0,
            -libc::EPERM
        );
        // Read-only pins for a call RM pins for writing.
        assert_eq!(
            ioctl(
                &mut vm.be,
                ctl,
                VID_HEAP,
                &os32(GUEST_VA, PAGE, 0, 0),
                &[],
                Some(&list(0, &[(LOW, 1)]))
            )
            .0,
            -libc::EINVAL
        );
        // A list on RM_FREE.
        let mut f = vec![0u8; 16];
        put32(&mut f, 0, CLIENT);
        assert_eq!(
            ioctl(
                &mut vm.be,
                ctl,
                RM_FREE,
                &f,
                &[],
                Some(&list(OSDESC_F_WRITE, &[(LOW, 1)]))
            )
            .0,
            -libc::EINVAL
        );
        assert!(seen().is_empty());
        // A backend with no guest RAM never offered it.
        vm.be.set_guest_ram(None);
        assert_eq!(
            ioctl(
                &mut vm.be,
                ctl,
                VID_HEAP,
                &os32(GUEST_VA, PAGE, 0, 0),
                &[],
                Some(&list(OSDESC_F_WRITE, &[(LOW, 1)]))
            )
            .0,
            -libc::EINVAL
        );
        assert!(seen().is_empty());
    }

    #[test]
    fn a_registration_over_the_budget_never_reaches_rm() {
        let mut vm = vm();
        let ctl = vm.ctl;
        vm.be.osdesc = OsDesc::with_limits(Limits {
            regs_per_vm: 8,
            regs_per_file: 8,
            bytes_per_vm: 1 << 30,
            bytes_per_file: 2 * PAGE,
            vmas_per_vm: 64,
            ..Limits::default()
        });
        let (st, _, params, _) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA, 3 * PAGE, 0, 0),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(LOW, 3)])),
        );
        // RM's own out-of-memory answer in the caller's block, the ioctl
        // succeeding.
        assert_eq!(st, 0);
        assert_eq!(rd32(&params, NVOS32_STATUS), NV_ERR_NO_MEMORY);
        assert_eq!(
            rd64(&params, NVOS32_ALLOC_OS_DESC_DESCRIPTOR),
            GUEST_VA,
            "the caller's own"
        );
        assert!(seen().is_empty());
        assert_eq!(vm.be.osdesc.live(), 0);
    }

    /// The file the client was allocated on closes: the backend frees the
    /// client there first, the range goes, and a reap names the
    /// registration.
    #[test]
    fn closing_the_clients_file_frees_the_client_and_releases_the_pages() {
        let mut vm = vm();
        let (ctl, gpu) = (vm.ctl, vm.gpu);
        let (st, _, _, deep) = ioctl(
            &mut vm.be,
            gpu,
            ALLOC_MEMORY,
            &os02(GUEST_VA, 2 * PAGE, -1),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(HIGH, 1), (LOW, 1)])),
        );
        assert_eq!(st, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, .. }] => *addr,
            other => panic!("{other:?}"),
        };
        let id = rd64(&deep, 0);
        // The GPU file's close changes nothing: the object is the client's.
        assert_eq!(status(&call(&mut vm.be, MsgType::Close, gpu, &[])), 0);
        assert!(seen().is_empty());
        assert!(reserved_live(addr));
        assert_eq!(status(&call(&mut vm.be, MsgType::Close, ctl, &[])), 0);
        assert_eq!(
            seen(),
            vec![Seen::Free {
                client: CLIENT,
                object: CLIENT
            }]
        );
        assert!(!reserved_live(addr), "the reserved range is gone");
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// The backend's own free of a closing file's client fails: RM may still
    /// hold the pages, so nothing is released -- late, with the session, not
    /// early.
    #[test]
    fn a_client_rm_would_not_free_keeps_its_registrations() {
        let mut vm = vm();
        let ctl = vm.ctl;
        let (st, ..) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA, 2 * PAGE, 0, 0),
            &[],
            Some(&list(OSDESC_F_WRITE, &[(HIGH, 1), (LOW, 1)])),
        );
        assert_eq!(st, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, .. }] => *addr,
            other => panic!("{other:?}"),
        };
        // NV_ERR_INVALID_CLIENT: what RM answers for a client on another file.
        FREE_STATUS.with(|s| s.set(0x23));
        assert_eq!(status(&call(&mut vm.be, MsgType::Close, ctl, &[])), 0);
        assert_eq!(
            seen(),
            vec![Seen::Free {
                client: CLIENT,
                object: CLIENT
            }]
        );
        assert!(reserved_live(addr), "the range stays while RM may pin it");
        assert_eq!((vm.be.osdesc.live(), reap(&mut vm.be, 0).1), (1, vec![]));
        // The session takes it.
        vm.be.session_reset("test");
        assert!(!reserved_live(addr));
        assert_eq!(vm.be.osdesc.live(), 0);
    }

    /// A session reset frees every client that holds a registration, on the
    /// file it was made on, and forgets them all: the new guest starts with
    /// nothing to reap.
    #[test]
    fn a_session_reset_frees_the_clients_and_forgets_the_registrations() {
        let mut vm = vm();
        let ctl = vm.ctl;
        let (st, ..) = ioctl(
            &mut vm.be,
            ctl,
            VID_HEAP,
            &os32(GUEST_VA, 2 * PAGE, 0, 0),
            &[],
            Some(&list(
                OSDESC_F_WRITE,
                &[(HIGH + 3 * PAGE, 1), (LOW + 3 * PAGE, 1)],
            )),
        );
        assert_eq!(st, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, .. }] => *addr,
            other => panic!("{other:?}"),
        };
        vm.be.session_reset("test");
        assert_eq!(
            seen(),
            vec![Seen::Free {
                client: CLIENT,
                object: CLIENT
            }]
        );
        assert!(!reserved_live(addr));
        assert_eq!((vm.be.osdesc.live(), vm.be.osdesc.unreaped()), (0, 0));
    }

    // ── What else holds the pages: UVM, exports, objects RM makes over it ──

    const DRIVER: &str = "610.57.04";
    const HANDLE: u32 = 0x5000_0001;
    const G1: GpuUuid = [0x61; 16];
    const G2: GpuUuid = [0x62; 16];
    /// Where the guest's UVM external range is.
    const UVA: u64 = 0x7f00_0000_0000;

    /// [`vm`] on a host whose release is [`DRIVER`], with a UVM file.
    fn vm_610() -> (Vm, u32) {
        let mut vm = vm();
        vm.be.set_host_driver_version(DRIVER);
        UVM_STATUS.with(|s| s.set(0));
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let uvm = vm
            .be
            .adopt_for_test(null(), HandleKind::Dev(protocol::messages::DeviceKind::Uvm));
        (vm, uvm)
    }

    /// Register two scattered pages as HANDLE through ALLOC_MEMORY: (the
    /// address RM was handed, the id).
    fn register(vm: &mut Vm) -> (u64, u64) {
        let gpu = vm.gpu;
        let (st, _, _, deep) = ioctl(
            &mut vm.be,
            gpu,
            ALLOC_MEMORY,
            &os02(GUEST_VA, 2 * PAGE, -1),
            &[],
            Some(&list(
                OSDESC_F_WRITE,
                &[(HIGH + 5 * PAGE, 1), (LOW + PAGE, 1)],
            )),
        );
        assert_eq!(st, 0);
        let addr = match &seen()[..] {
            [Seen::Register { addr, .. }] => *addr,
            other => panic!("{other:?}"),
        };
        (addr, rd64(&deep, 0))
    }

    fn uvm_size(cmd: u32) -> usize {
        abi::schema::uvm_table(abi::version::DriverVersion::parse(DRIVER).unwrap())
            .and_then(|t| t.lookup(cmd))
            .unwrap()
            .size as usize
    }

    /// A UVM command on `uvm`: its status as the guest reads it (the reply
    /// header's, then UVM's own at `status_at`).
    fn uvm_ioctl(vm: &mut Vm, uvm: u32, cmd: u32, p: &[u8], status_at: usize) -> (i32, u32) {
        let (st, _, params, _) = ioctl(&mut vm.be, uvm, cmd, p, &[], None);
        let rm = if st == 0 { rd32(&params, status_at) } else { 0 };
        (st, rm)
    }

    fn create_range(vm: &mut Vm, uvm: u32, base: u64, len: u64) {
        let mut p = vec![0u8; uvm_size(UVM_CREATE_EXTERNAL_RANGE)];
        put64(&mut p, 0, base);
        put64(&mut p, 8, len);
        let at = p.len() - 8;
        assert_eq!(
            uvm_ioctl(vm, uvm, UVM_CREATE_EXTERNAL_RANGE, &p, at),
            (0, 0)
        );
    }

    /// UVM_MAP_EXTERNAL_ALLOCATION of (CLIENT, `memory`) at `base`, on
    /// `gpus`, as cuMemHostRegister's mapping would ask.
    fn map_external(
        vm: &mut Vm,
        uvm: u32,
        base: u64,
        len: u64,
        memory: u32,
        gpus: &[GpuUuid],
    ) -> (i32, u32) {
        let fd_off = crate::uvmfd::field(
            abi::version::DriverVersion::parse(DRIVER),
            UVM_MAP_EXTERNAL_ALLOCATION,
        )
        .unwrap()
        .offset as usize;
        let mut p = vec![0u8; uvm_size(UVM_MAP_EXTERNAL_ALLOCATION)];
        put64(&mut p, 0, base);
        put64(&mut p, 8, len);
        for (i, g) in gpus.iter().enumerate() {
            p[24 + 36 * i..40 + 36 * i].copy_from_slice(g);
        }
        put64(&mut p, fd_off - 8, gpus.len() as u64);
        put32(&mut p, fd_off, vm.ctl);
        put32(&mut p, fd_off + 4, CLIENT);
        put32(&mut p, fd_off + 8, memory);
        uvm_ioctl(vm, uvm, UVM_MAP_EXTERNAL_ALLOCATION, &p, fd_off + 12)
    }

    /// UVM duplicates what REGISTER_GPU_VASPACE, REGISTER_CHANNEL,
    /// MAP_EXTERNAL_ALLOCATION (and the rest with an `rmCtrlFd`) name from a
    /// kernel client of its own, where RM's only check is that the source
    /// client's process is the caller's -- the backend's, for every guest
    /// process. So the client must be one this VM made on the control file
    /// the call names: another guest process's client, named through the
    /// caller's own control file, is refused before UVM sees it, and so is
    /// a client no file of this VM made.
    #[test]
    fn a_uvm_command_names_only_a_client_made_on_the_control_file_it_names() {
        const OTHER: u32 = 0xc1d0_0002;
        let (mut vm, uvm) = vm_610();
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let other_ctl = vm
            .be
            .adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        vm.be.semsurf.client_allocated(other_ctl, OTHER);
        let v = abi::version::DriverVersion::parse(DRIVER);
        let map_off = crate::uvmfd::field(v, UVM_MAP_EXTERNAL_ALLOCATION)
            .unwrap()
            .offset as usize;
        let map = |ctl: u32, client: u32| {
            let mut p = vec![0u8; uvm_size(UVM_MAP_EXTERNAL_ALLOCATION)];
            put64(&mut p, 0, 1 << 32);
            put64(&mut p, 8, PAGE);
            put32(&mut p, map_off, ctl);
            put32(&mut p, map_off + 4, client);
            put32(&mut p, map_off + 8, 0x5000_0009);
            p
        };
        const REGISTER_GPU_VASPACE: u32 = 25;
        let va_off = crate::uvmfd::field(v, REGISTER_GPU_VASPACE).unwrap().offset as usize;
        let vaspace = |ctl: u32, client: u32| {
            let mut p = vec![0u8; uvm_size(REGISTER_GPU_VASPACE)];
            put32(&mut p, va_off, ctl);
            put32(&mut p, va_off + 4, client);
            put32(&mut p, va_off + 8, 0x5c00_0008);
            p
        };
        seen();
        let (ctl, map_at) = (vm.ctl, map_off + 12);
        for (p, cmd, at) in [
            (map(ctl, OTHER), UVM_MAP_EXTERNAL_ALLOCATION, map_at),
            (map(ctl, 0xc1d0_0077), UVM_MAP_EXTERNAL_ALLOCATION, map_at),
            (map(u32::MAX, CLIENT), UVM_MAP_EXTERNAL_ALLOCATION, map_at),
            (vaspace(ctl, OTHER), REGISTER_GPU_VASPACE, va_off + 12),
            (
                vaspace(other_ctl, CLIENT),
                REGISTER_GPU_VASPACE,
                va_off + 12,
            ),
        ] {
            assert_eq!(
                uvm_ioctl(&mut vm, uvm, cmd, &p, at).0,
                -libc::EPERM,
                "cmd {cmd}"
            );
        }
        assert_eq!(seen(), vec![], "UVM was asked");
        // Each client through the file it was made on: UVM is asked.
        for (p, cmd, at) in [
            (map(ctl, CLIENT), UVM_MAP_EXTERNAL_ALLOCATION, map_at),
            (map(other_ctl, OTHER), UVM_MAP_EXTERNAL_ALLOCATION, map_at),
            (vaspace(ctl, CLIENT), REGISTER_GPU_VASPACE, va_off + 12),
            // No client, and no control file: names nothing.
            (vaspace(u32::MAX, 0), REGISTER_GPU_VASPACE, va_off + 12),
        ] {
            assert_eq!(uvm_ioctl(&mut vm, uvm, cmd, &p, at), (0, 0), "cmd {cmd}");
        }
        assert_eq!(seen().len(), 4);
    }

    fn rm_free(vm: &mut Vm, object: u32) {
        let mut f = vec![0u8; 16];
        put32(&mut f, 0, CLIENT);
        put32(&mut f, 8, object);
        let ctl = vm.ctl;
        assert_eq!(ioctl(&mut vm.be, ctl, RM_FREE, &f, &[], None).0, 0);
    }

    /// cuMemHostRegister's shape: the memory registered (ALLOC_MEMORY of
    /// 0x71), an external range, the memory mapped into it by UVM, and the
    /// handle freed. UVM's duplicate still holds the pages, so nothing is
    /// released -- until the UVM file closes, when the backend takes the
    /// range down on it first, while it is still ours.
    #[test]
    fn a_uvm_external_mapping_holds_registered_memory_until_its_uvm_file_closes() {
        let (mut vm, uvm) = vm_610();
        let (addr, id) = register(&mut vm);
        create_range(&mut vm, uvm, UVA, 4 << 20);
        assert_eq!(
            map_external(&mut vm, uvm, UVA, 2 * PAGE, HANDLE, &[G1]),
            (0, 0)
        );
        seen();
        rm_free(&mut vm, HANDLE);
        assert_eq!(
            reap(&mut vm.be, 0).1,
            Vec::<u64>::new(),
            "UVM still maps it"
        );
        assert!(reserved_live(addr), "and the backend's range stays");
        assert_eq!(vm.be.osdesc.live(), 1);
        seen();
        assert_eq!(status(&call(&mut vm.be, MsgType::Close, uvm, &[])), 0);
        assert_eq!(
            seen(),
            vec![Seen::Uvm {
                cmd: crate::uvmmap::FREE,
                base: UVA
            }]
        );
        assert!(!reserved_live(addr));
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// And cuMemHostUnregister's: the range freed, or the mapping unmapped
    /// from every GPU, before or after the handle -- released when the last
    /// of them goes, not before.
    #[test]
    fn freeing_or_unmapping_the_uvm_mapping_releases_what_the_handle_no_longer_holds() {
        let (mut vm, uvm) = vm_610();
        let (_, id) = register(&mut vm);
        create_range(&mut vm, uvm, UVA, 4 << 20);
        assert_eq!(
            map_external(&mut vm, uvm, UVA, 2 * PAGE, HANDLE, &[G1]),
            (0, 0)
        );
        // UVM_FREE first: the handle still holds it.
        let mut p = vec![0u8; uvm_size(crate::uvmmap::FREE)];
        put64(&mut p, 0, UVA);
        let at = p.len() - 8;
        assert_eq!(uvm_ioctl(&mut vm, uvm, crate::uvmmap::FREE, &p, at), (0, 0));
        assert_eq!(vm.be.osdesc.uvm_held(), 0);
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
        rm_free(&mut vm, HANDLE);
        let (ack, got) = reap(&mut vm.be, 0);
        assert_eq!(got, vec![id]);

        // Handle first, then UNMAP_EXTERNAL from each of two GPUs.
        seen();
        let (_, id) = register(&mut vm);
        create_range(&mut vm, uvm, UVA, 4 << 20);
        assert_eq!(
            map_external(&mut vm, uvm, UVA, 2 * PAGE, HANDLE, &[G1, G2]),
            (0, 0)
        );
        rm_free(&mut vm, HANDLE);
        let unmap = |vm: &mut Vm, g: &GpuUuid| {
            let mut p = vec![0u8; uvm_size(UVM_UNMAP_EXTERNAL)];
            put64(&mut p, 0, UVA);
            put64(&mut p, 8, 4 << 20);
            p[16..32].copy_from_slice(g);
            assert_eq!(uvm_ioctl(vm, uvm, UVM_UNMAP_EXTERNAL, &p, 32), (0, 0));
        };
        unmap(&mut vm, &G1);
        assert_eq!(reap(&mut vm.be, ack).1, Vec::<u64>::new());
        unmap(&mut vm, &G2);
        assert_eq!(reap(&mut vm.be, ack).1, vec![id]);
    }

    /// A mapping of registered memory is held when UVM made it: on NV_OK,
    /// and on the failures of its wait for the page-table writes, which
    /// leave every mapping up (an RC error here) -- until its range is
    /// freed. Refused (an invalid argument), UVM made nothing, and nothing
    /// is held; outside every range this file made, UVM would refuse it,
    /// and it never gets there. One of memory
    /// nothing registered is not followed; ALLOC_DEVICE_P2P of registered
    /// memory never reaches UVM.
    #[test]
    fn a_uvm_mapping_of_registered_memory_is_held_when_uvm_made_it() {
        let (mut vm, uvm) = vm_610();
        let (_, id) = register(&mut vm);
        create_range(&mut vm, uvm, UVA, 4 << 20);
        seen();
        UVM_STATUS.with(|s| s.set(0x1f));
        assert_eq!(
            map_external(&mut vm, uvm, UVA, PAGE, HANDLE, &[G1]),
            (0, 0x1f)
        );
        assert_eq!(
            vm.be.osdesc.uvm_held(),
            0,
            "NV_ERR_INVALID_ARGUMENT made nothing"
        );
        UVM_STATUS.with(|s| s.set(0x60));
        assert_eq!(
            map_external(&mut vm, uvm, UVA, PAGE, HANDLE, &[G1]),
            (0, 0x60)
        );
        assert_eq!(vm.be.osdesc.uvm_held(), 1, "NV_ERR_RC_ERROR left it up");
        UVM_STATUS.with(|s| s.set(0));
        assert_eq!(seen().len(), 2);
        assert_eq!(
            map_external(&mut vm, uvm, UVA + (8 << 20), PAGE, HANDLE, &[G1]).0,
            -libc::EINVAL,
            "no range there"
        );
        assert!(seen().is_empty());
        assert_eq!(
            map_external(&mut vm, uvm, UVA, PAGE, 0x5000_0099, &[G1]),
            (0, 0)
        );
        assert_eq!(vm.be.osdesc.uvm_held(), 1);
        seen();
        let mut p = vec![0u8; uvm_size(UVM_ALLOC_DEVICE_P2P)];
        put32(&mut p, 40, vm.ctl);
        put32(&mut p, 44, CLIENT);
        put32(&mut p, 48, HANDLE);
        assert_eq!(
            ioctl(&mut vm.be, uvm, UVM_ALLOC_DEVICE_P2P, &p, &[], None).0,
            -libc::EPERM
        );
        assert!(seen().is_empty());
        rm_free(&mut vm, HANDLE);
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
        let mut p = vec![0u8; uvm_size(crate::uvmmap::FREE)];
        put64(&mut p, 0, UVA);
        let at = p.len() - 8;
        assert_eq!(uvm_ioctl(&mut vm, uvm, crate::uvmmap::FREE, &p, at), (0, 0));
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// The range will not come down when the UVM file closes: the pages
    /// stay pinned, and the session takes them.
    #[test]
    fn a_uvm_mapping_that_will_not_come_down_holds_the_pages_until_the_session_ends() {
        let (mut vm, uvm) = vm_610();
        let (addr, _) = register(&mut vm);
        create_range(&mut vm, uvm, UVA, 4 << 20);
        assert_eq!(map_external(&mut vm, uvm, UVA, PAGE, HANDLE, &[G1]), (0, 0));
        rm_free(&mut vm, HANDLE);
        UVM_STATUS.with(|s| s.set(0x1f));
        seen();
        assert_eq!(status(&call(&mut vm.be, MsgType::Close, uvm, &[])), 0);
        // UVM_FREE of the range, then UNMAP_EXTERNAL of the mapping.
        assert_eq!(
            seen(),
            vec![
                Seen::Uvm {
                    cmd: crate::uvmmap::FREE,
                    base: UVA
                },
                Seen::Uvm {
                    cmd: UVM_UNMAP_EXTERNAL,
                    base: UVA
                }
            ]
        );
        assert!(reserved_live(addr));
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
        vm.be.session_reset("test");
        assert!(!reserved_live(addr));
        assert_eq!((vm.be.osdesc.live(), vm.be.osdesc.uvm_held()), (0, 0));
    }

    /// Exported to an RM descriptor or attached to an NV_MEMORY_EXPORT
    /// object, registered memory would be duplicated where the backend
    /// cannot follow: refused, the handle, a duplicate of it and an object
    /// made over it alike. Anything else still goes.
    #[test]
    fn registered_memory_is_never_exported() {
        let (mut vm, _) = vm_610();
        // This module's gate alone: NV_MEMORY_EXPORT's EXPORT_MEM is not in
        // the RM allowlist (rmallow.rs), which would answer it first.
        vm.be.set_rm_allowlist(crate::rmallow::Mode::Log);
        register(&mut vm);
        let ctl = vm.ctl;
        let control = |cmd: u32, size: usize| {
            let mut o = vec![0u8; 32];
            put32(&mut o, 0, CLIENT);
            put32(&mut o, 4, CLIENT);
            put32(&mut o, 8, cmd);
            put64(&mut o, 16, GUEST_VA);
            put32(&mut o, 24, size as u32);
            (o, vec![0u8; size])
        };
        // A duplicate of it, in the same client.
        vm.be
            .osdesc
            .duplicated(CLIENT, HANDLE, CLIENT, 0x5000_0002, DEVICE);
        for h in [HANDLE, 0x5000_0002] {
            let (o, mut n) = control(EXPORT_OBJECT_TO_FD, 24);
            put32(&mut n, 12, h);
            assert_eq!(
                ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0,
                -libc::EPERM
            );
            let (o, mut n) = control(EXPORT_OBJECTS_TO_FD, 2128);
            put32(&mut n, 76, 0x5000_0077);
            put32(&mut n, 80, h);
            n[2124..2126].copy_from_slice(&2u16.to_le_bytes());
            assert_eq!(
                ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0,
                -libc::EPERM
            );
            let (o, mut n) = control(EXPORT_MEM, 1048);
            put32(&mut n, 8, h);
            n[1032..1034].copy_from_slice(&1u16.to_le_bytes());
            assert_eq!(
                ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0,
                -libc::EPERM
            );
        }
        assert!(seen().is_empty(), "none reached RM");
        // Past numObjects, or another object: RM's to answer. The export
        // file must be one of the VM's control files (rmctl.rs, R1).
        let (o, mut n) = control(EXPORT_OBJECTS_TO_FD, 2128);
        put32(&mut n, 0, ctl);
        put32(&mut n, 76, 0x5000_0077);
        put32(&mut n, 80, HANDLE);
        n[2124..2126].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0, 0);
        assert_eq!(
            seen(),
            vec![Seen::Control {
                cmd: EXPORT_OBJECTS_TO_FD
            }]
        );
    }

    /// A semaphore surface over registered memory keeps a duplicate of it
    /// in RM's own client: the memory's handle freed, the surface holds it
    /// until it is freed itself.
    #[test]
    fn a_semaphore_surface_over_registered_memory_holds_it_until_the_surface_goes() {
        let (mut vm, _) = vm_610();
        let (addr, id) = register(&mut vm);
        let ctl = vm.ctl;
        let mut o = vec![0u8; 48];
        put32(&mut o, 0, CLIENT);
        put32(&mut o, 4, DEVICE);
        put32(&mut o, 8, 0x5000_0020);
        put32(&mut o, 12, NV_SEMAPHORE_SURFACE);
        put64(&mut o, NVOS64_P_ALLOC_PARMS, GUEST_VA);
        put32(&mut o, 32, 16);
        let mut n = vec![0u8; 16];
        put32(&mut n, 0, HANDLE);
        assert_eq!(ioctl(&mut vm.be, ctl, RM_ALLOC, &o, &n, None).0, 0);
        assert_eq!(
            seen(),
            vec![Seen::Alloc {
                class: NV_SEMAPHORE_SURFACE
            }]
        );
        rm_free(&mut vm, HANDLE);
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
        assert!(reserved_live(addr));
        rm_free(&mut vm, 0x5000_0020);
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// A semaphore surface over registered memory hands the caller a
    /// duplicate of it on request (REF_MEMORY): with the handle and the
    /// surface freed, that duplicate still holds it, and is never exported.
    #[test]
    fn memory_a_semaphore_surface_hands_back_holds_it_too() {
        let (mut vm, _) = vm_610();
        let (addr, id) = register(&mut vm);
        let ctl = vm.ctl;
        let mut o = vec![0u8; 48];
        put32(&mut o, 0, CLIENT);
        put32(&mut o, 4, DEVICE);
        put32(&mut o, 8, 0x5000_0020);
        put32(&mut o, 12, NV_SEMAPHORE_SURFACE);
        put64(&mut o, NVOS64_P_ALLOC_PARMS, GUEST_VA);
        put32(&mut o, 32, 16);
        let mut n = vec![0u8; 16];
        put32(&mut n, 0, HANDLE);
        assert_eq!(ioctl(&mut vm.be, ctl, RM_ALLOC, &o, &n, None).0, 0);
        seen();
        let control = |object: u32, cmd: u32, n: Vec<u8>| {
            let mut o = vec![0u8; 32];
            put32(&mut o, 0, CLIENT);
            put32(&mut o, 4, object);
            put32(&mut o, 8, cmd);
            put64(&mut o, 16, GUEST_VA);
            put32(&mut o, 24, n.len() as u32);
            (o, n)
        };
        // REF_MEMORY: RM makes 0x5000_0030 in the caller's client.
        let mut p = vec![0u8; 8];
        put32(&mut p, 0, 0x5000_0030);
        let (o, n) = control(0x5000_0020, SEMSURF_REF_MEMORY, p);
        assert_eq!(ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0, 0);
        assert!(vm.be.osdesc.holds(CLIENT, 0x5000_0030));
        rm_free(&mut vm, HANDLE);
        rm_free(&mut vm, 0x5000_0020);
        assert_eq!(reap(&mut vm.be, 0).1, Vec::<u64>::new());
        assert!(reserved_live(addr));
        let mut p = vec![0u8; 24];
        put32(&mut p, 12, 0x5000_0030);
        let (o, n) = control(CLIENT, EXPORT_OBJECT_TO_FD, p);
        assert_eq!(
            ioctl(&mut vm.be, ctl, CONTROL, &o, &n, None).0,
            -libc::EPERM
        );
        rm_free(&mut vm, 0x5000_0030);
        assert_eq!(reap(&mut vm.be, 0).1, vec![id]);
    }

    /// Guest RAM is write-back to the guest whatever RM thinks (rmmem.rs):
    /// a GPU mapping of registered memory snoops, and the caller reads back
    /// the flags it sent.
    #[test]
    fn a_gpu_mapping_of_registered_memory_snoops() {
        let (mut vm, _) = vm_610();
        register(&mut vm);
        let ctl = vm.ctl;
        let map_dma = ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_MAP_MEMORY_DMA, 64);
        let mut p = vec![0u8; 64];
        put32(&mut p, 0, CLIENT);
        put32(&mut p, 12, HANDLE);
        put32(&mut p, 32, 0x1);
        let (st, _, back, _) = ioctl(&mut vm.be, ctl, map_dma, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen(), vec![Seen::MapDma { flags: 0x11 }]);
        assert_eq!(rd32(&back, 32), 0x1);
        // Memory nothing registered is left alone.
        put32(&mut p, 12, 0x5000_0099);
        ioctl(&mut vm.be, ctl, map_dma, &p, &[], None);
        assert_eq!(seen(), vec![Seen::MapDma { flags: 0x1 }]);
    }
}
