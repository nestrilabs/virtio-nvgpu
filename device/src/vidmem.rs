// SPDX-License-Identifier: Apache-2.0
//! A VM's video memory, held to `--vram-limit`.
//!
//! Every guest process's RM calls are the backend's on the host, so RM sees
//! one client process per VM, with the whole of the GPU's video memory to
//! allocate from: without a limit, one VM can take all of it and leave the
//! host's desktop and every other VM with NV_ERR_NO_MEMORY. The shared
//! window (shm.rs) bounds only what a VM has CPU-mapped at once, not what it
//! holds. With `--vram-limit` set, the backend counts the video memory the
//! VM's RM calls allocate and refuses an allocation that would take the VM
//! past the limit, before RM sees it, with RM's own answer to an allocation
//! it cannot back: NV_ERR_NO_MEMORY in the caller's block, the ioctl itself
//! succeeding, as the other budget refusals answer (rmallow.rs, rmchan.rs).
//! The limit is also split among the VM's processes (quota.rs): one process
//! holds at most `--window-owner-share` percent of it, half by default, and
//! the last eighth is kept for processes that hold little.
//!
//! **What is counted.** Physical video memory the guest allocates by name:
//! RM_ALLOC of NV01_MEMORY_LOCAL_USER (the only video-memory class the RM
//! allowlist lets a guest make), and VID_HEAP_CONTROL's three allocating
//! functions when RM makes NV01_MEMORY_LOCAL_USER of them -- LOCATION_VIDMEM
//! and not VIRTUAL (open-gpu-kernel-modules 595.99.02,
//! src/nvidia/interface/deprecated/rmapi_deprecated_vidheapctrl.c
//! _rmVidHeapControlAllocCommon()). Virtual allocations (NV01_MEMORY_VIRTUAL,
//! NV50_MEMORY_VIRTUAL, VIRTUAL through the heap) have no backing of their
//! own and are not counted; neither is system memory. The size charged is
//! RM's, from the reply (RM rounds it up to the page size); the size asked
//! for is what is admitted, so the limit is passed by at most one
//! allocation's rounding.
//!
//! **Whose it is, and for how long.** A charge belongs to the process that
//! made the allocation, and lives as long as anything the backend can see
//! holds the memory: the handle it was made under, every duplicate of it
//! (NV_ESC_RM_DUP_OBJECT), every export slot of an OS_UNIX export
//! descriptor that holds it, and every handle imported from one. A
//! duplicate or an import is the same memory, charged once, to the maker.
//! Each goes with RM_FREE of it or of anything above it (rmmem.rs's tree),
//! VID_HEAP_CONTROL FREE or HW_FREE, RM_FREE of its client, the close of the
//! file its client lives on or of the export descriptor, and a session's
//! reset. The records are themselves a pool, split among processes as the
//! memory is; a duplicate, export or import that would outgrow a process's
//! part is refused as an allocation past the limit is.
//!
//! **What is not.** Memory RM allocates for a VM on its own account:
//! channels' and contexts' buffers, page tables, GSP's, the error notifiers,
//! and what nvidia-uvm migrates into video memory for CUDA's managed
//! allocations (UVM allocates through a kernel client of its own). Memory
//! kept alive past every RM handle and export descriptor by a kernel client
//! the backend does not follow -- an nvidia-drm GEM object imported from an
//! export descriptor, an NVKMS surface registered from one -- stops counting
//! when the last of those goes. SECURITY.md, "Video memory limit", has what
//! this leaves a VM able to do.
//!
//! **What the guest is told.** nvidia-smi and NVML read video memory sizes
//! from NV2080_CTRL_CMD_FB_GET_INFO_V2, and so do the Vulkan driver (heap
//! size and VK_EXT_memory_budget) and CUDA (cuMemGetInfo): measured with
//! rig/heavy/rmlog.c on the rig. The guest module turns the V1 control into
//! V2, so V2 is the one to rewrite. With a limit, its sizes -- RAM_SIZE,
//! TOTAL_RAM_SIZE, HEAP_SIZE, MAPPABLE_HEAP_SIZE, USABLE_RAM_SIZE -- say no
//! more than the limit, and its free ones -- HEAP_FREE, LARGEST_FREE_REGION
//! and HEAP_RECLAIMABLE -- no more than what the VM has left of it; each is
//! also never more than RM said, so a host with less free than the limit
//! leaves shows that. nvidia-smi's "Used" (HEAP_SIZE less HEAP_FREE) is then
//! what the VM holds, or more when the host is short, and "Reserved"
//! (TOTAL_RAM_SIZE less HEAP_SIZE) is 0. VID_HEAP_CONTROL INFO, which RM
//! answers from the same control, and ALLOC_MEMORY of NV01_MEMORY_LOCAL_USER,
//! whose limit RM sets to the heap's size, are rewritten the same way.
//! Every field is read and written where the host release's own layout puts
//! it (gen/vidmem_extract.py): the FB_INFO indices move between releases.
//! Without a limit nothing here runs, and nothing is rewritten.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use abi::ioctl::{
    NV_ESC_RM_ALLOC, NV_ESC_RM_ALLOC_MEMORY, NV_ESC_RM_CONTROL, NV_ESC_RM_DUP_OBJECT,
    NV_ESC_RM_VID_HEAP_CONTROL,
};
use abi::version::DriverVersion;
use abi::vidmem::Layout;

use crate::le;
use crate::nvos::{
    NV_ERR_INVALID_PARAM_STRUCT, NVOS55_H_CLIENT, NVOS55_H_CLIENT_SRC, NVOS55_H_OBJECT,
    NVOS55_H_OBJECT_SRC, NVOS55_SIZE, NVOS55_STATUS,
};
use crate::quota::{Charge, Owner, Pool, Share};

/// Records -- handles and export slots of counted memory -- the backend
/// keeps for a VM, in all. Each names memory of at least a page, so a VM
/// within its limit needs far fewer; duplicates and imports need none of
/// the limit, and this is what bounds them.
pub const MAX_RECORDS: u64 = 1 << 18;

/// Each process's part of [`MAX_RECORDS`] (quota.rs, as the other pools).
const RECORD_SHARE: Share = Share::quarter(MAX_RECORDS, 16);

/// The smallest limit the backend takes, in MiB: less than a desktop's
/// first surfaces.
pub const MIN_LIMIT_MIB: u64 = 64;

/// A piece of counted memory held by one thing the backend sees: the
/// memory's charge, shared with everything else that holds it, and the
/// record's own.
#[derive(Debug)]
struct Held {
    mem: Arc<Charge>,
    _record: Charge,
}

#[derive(Debug)]
struct On {
    l: &'static Layout,
    limit: u64,
    /// Bytes of video memory, split among processes.
    bytes: Pool,
    records: Pool,
    /// RM handles of counted memory, by (hClient, handle).
    handles: HashMap<(u32, u32), Held>,
    /// Export descriptors' slots that hold counted memory, by (the guest
    /// file's handle, slot).
    exports: HashMap<(u32, u32), Held>,
    refused: u64,
}

/// The limit, when there is one, and what is counted against it.
#[derive(Debug, Default)]
pub struct Vram {
    on: Option<Box<On>>,
}

/// What a call `Vram::before` looked at does once RM has answered.
#[derive(Debug, Default)]
pub(crate) enum Pending {
    #[default]
    Nothing,
    /// Refused before RM: this status at this offset of the top-level block.
    Refuse {
        at: usize,
        status: u32,
    },
    /// A new allocation of video memory: its client, handle and size are
    /// read from the reply at these offsets. `want` is what was asked,
    /// admitted by `reserved`.
    Alloc {
        client: usize,
        handle: usize,
        status: usize,
        size: usize,
        want: u64,
        status_in: usize,
        reserved: Option<Charge>,
        record: Option<Charge>,
    },
    /// VID_HEAP_CONTROL FREE or HW_FREE of (the client at, the handle at).
    Free {
        client: usize,
        handle: usize,
        status: usize,
    },
    /// A duplicate of counted memory.
    Dup {
        mem: Arc<Charge>,
        record: Option<Charge>,
    },
    /// OS_UNIX export of objects into slots `first..` of the file `fd`:
    /// each slot's memory, if counted.
    Export {
        fd: u32,
        first: u32,
        mem: Vec<Option<Arc<Charge>>>,
        records: Vec<Charge>,
    },
    /// OS_UNIX import from file `fd`'s slots: for each, the reply offset
    /// of the handle RM makes, and the memory the slot holds.
    Import {
        client: u32,
        parent: u32,
        at: Vec<(usize, Arc<Charge>)>,
        records: Vec<Charge>,
    },
    /// Replies to rewrite.
    FbInfo,
    HeapInfo,
    AllocMemoryLimit,
}

impl Pending {
    /// How many records the call needs, and where a refusal goes.
    fn needs(&self) -> Option<(u64, usize)> {
        match self {
            Pending::Dup { .. } => Some((1, NVOS55_STATUS)),
            Pending::Export { mem, .. } => Some((mem.iter().flatten().count() as u64, 0)),
            Pending::Import { at, .. } => Some((at.len() as u64, 0)),
            _ => None,
        }
    }
}

/// What `Vram::after` leaves to rmmem.rs, which keeps the tree of objects.
#[derive(Debug, Default)]
pub(crate) struct After {
    /// A VID_HEAP_CONTROL free: RM freed this (hClient, handle) and
    /// everything under it.
    pub(crate) freed: Option<(u32, u32)>,
    /// New handles: (hClient, handle, parent), for the tree.
    pub(crate) made: Vec<(u32, u32, u32)>,
}

/// Why `set_limit` would not take a limit.
#[derive(Debug, PartialEq, Eq)]
pub enum LimitError {
    TooSmall,
    NoLayout(DriverVersion),
    NoRelease,
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitError::TooSmall => write!(f, "--vram-limit is at least {MIN_LIMIT_MIB} MiB"),
            LimitError::NoLayout(v) => write!(
                f,
                "--vram-limit: host driver {v} was not measured by gen/vidmem_extract.py; its \
                 replies are nobody's to rewrite"
            ),
            LimitError::NoRelease => write!(f, "--vram-limit: the host driver release is unknown"),
        }
    }
}

fn bit(v: u32, shift: u32, mask: u32) -> u32 {
    v.checked_shr(shift).unwrap_or(0) & mask
}

impl Vram {
    /// Hold the VM to `mib` MiB of video memory on host release `v`, one
    /// process to `owner_percent` of it (`--window-owner-share`).
    pub fn set_limit(
        &mut self,
        mib: u64,
        owner_percent: u8,
        v: Option<DriverVersion>,
    ) -> Result<(), LimitError> {
        if mib < MIN_LIMIT_MIB {
            return Err(LimitError::TooSmall);
        }
        let v = v.ok_or(LimitError::NoRelease)?;
        let l = abi::vidmem::layout_for(v).ok_or(LimitError::NoLayout(v))?;
        let limit = mib.saturating_mul(1 << 20);
        self.on = Some(Box::new(On {
            l,
            limit,
            bytes: Pool::new(limit, Share::percent(limit, owner_percent)),
            records: Pool::new(MAX_RECORDS, RECORD_SHARE),
            handles: HashMap::new(),
            exports: HashMap::new(),
            refused: 0,
        }));
        Ok(())
    }

    /// The limit, in bytes, if there is one.
    pub fn limit(&self) -> Option<u64> {
        self.on.as_ref().map(|o| o.limit)
    }

    /// Bytes counted against the limit.
    pub fn in_use(&self) -> u64 {
        self.on.as_ref().map_or(0, |o| o.bytes.in_use())
    }

    /// Bytes counted to `o`.
    pub fn held(&self, o: Owner) -> u64 {
        self.on.as_ref().map_or(0, |o2| o2.bytes.held(o))
    }

    /// Allocations refused for the limit so far.
    pub fn refused(&self) -> u64 {
        self.on.as_ref().map_or(0, |o| o.refused)
    }

    /// Whether `escape` is one `before` wants to see, beyond rmmem.rs's own.
    pub(crate) fn watches(&self, escape: u32) -> bool {
        self.on.is_some() && escape == NV_ESC_RM_CONTROL
    }

    /// An RM_ALLOC not in the 48-byte NVOS64 form (only `--permissive-abi`
    /// or `--rm-allowlist=log` lets one this far) of the video-memory
    /// class, which `before` does not see: refused under a limit.
    pub(crate) fn refuses_unseen(&self, escape: u32, params: &[u8]) -> bool {
        let Some(o) = &self.on else {
            return false;
        };
        escape == NV_ESC_RM_ALLOC
            && le::u32_at(params, o.l.nvos64_h_class) == Some(o.l.nv01_memory_local_user)
    }

    /// Look at an RM escape on its way to the host: `params` is the whole
    /// block as the host will see it (for RM_ALLOC and RM_CONTROL, the
    /// top-level block and the parameters after it).
    pub(crate) fn before(&self, escape: u32, params: &[u8]) -> Pending {
        let Some(o) = &self.on else {
            return Pending::Nothing;
        };
        let l = o.l;
        match escape {
            NV_ESC_RM_ALLOC => {
                if le::u32_at(params, l.nvos64_h_class) != Some(l.nv01_memory_local_user) {
                    return Pending::Nothing;
                }
                // RM reads the class's whole parameter block; one the
                // guest cut short is not one the size can be read from.
                let size = l.nvos64_sizeof + l.nv_memory_allocation_size;
                let (true, Some(want)) = (
                    params.len() >= l.nvos64_sizeof + l.nv_memory_allocation_sizeof,
                    le::u64_at(params, size),
                ) else {
                    return Pending::Refuse {
                        at: l.nvos64_status,
                        status: NV_ERR_INVALID_PARAM_STRUCT,
                    };
                };
                Pending::Alloc {
                    client: l.nvos64_h_root,
                    handle: l.nvos64_h_object_new,
                    status: l.nvos64_status,
                    size,
                    want,
                    status_in: l.nvos64_status,
                    reserved: None,
                    record: None,
                }
            }
            NV_ESC_RM_VID_HEAP_CONTROL => o.before_heap(params),
            NV_ESC_RM_ALLOC_MEMORY
                if le::u32_at(params, l.nvos02_h_class) == Some(l.nv01_memory_local_user) =>
            {
                Pending::AllocMemoryLimit
            }
            NV_ESC_RM_DUP_OBJECT if params.len() >= NVOS55_SIZE => {
                let src = (
                    le::u32_at(params, NVOS55_H_CLIENT_SRC).unwrap_or(0),
                    le::u32_at(params, NVOS55_H_OBJECT_SRC).unwrap_or(0),
                );
                match o.handles.get(&src) {
                    Some(h) => Pending::Dup {
                        mem: h.mem.clone(),
                        record: None,
                    },
                    None => Pending::Nothing,
                }
            }
            NV_ESC_RM_CONTROL => o.before_control(params),
            _ => Pending::Nothing,
        }
    }

    /// Admit what `p` asks for, charged to `owner`, or say where RM's
    /// refusal goes in the top-level block and which status it is.
    pub(crate) fn admit(&mut self, p: &mut Pending, owner: Owner) -> Result<(), (usize, u32)> {
        let Some(o) = &mut self.on else {
            return Ok(());
        };
        let no_memory = o.l.nv_err_no_memory;
        let status_ctl = o.l.nvos54_status;
        if let Pending::Refuse { at, status } = *p {
            o.refused += 1;
            return Err((at, status));
        }
        if let Pending::Alloc {
            want,
            status_in,
            reserved,
            record,
            ..
        } = p
        {
            let (bytes, records) = (o.bytes.try_take(owner, *want), o.records.try_take(owner, 1));
            return match (bytes, records) {
                (Ok(b), Ok(r)) => {
                    *reserved = Some(b);
                    *record = Some(r);
                    Ok(())
                }
                (b, r) => {
                    o.refused += 1;
                    log::warn!(
                        "video memory: {want} bytes for guest process {owner:?} refused \
                         ({:?}); the VM holds {} of its {} MiB, the process {}",
                        b.err().or(r.err()),
                        o.bytes.in_use(),
                        o.limit >> 20,
                        o.bytes.held(owner)
                    );
                    Err((*status_in, no_memory))
                }
            };
        }
        let Some((n, at)) = p.needs() else {
            return Ok(());
        };
        let at = if at == 0 { status_ctl } else { at };
        let taken: Result<Vec<Charge>, _> = (0..n).map(|_| o.records.try_take(owner, 1)).collect();
        let Ok(mut taken) = taken else {
            o.refused += 1;
            log::warn!(
                "video memory: guest process {owner:?} holds its share of the {MAX_RECORDS} \
                 records of counted memory; a duplicate, export or import of it is refused"
            );
            return Err((at, no_memory));
        };
        match p {
            Pending::Dup { record, .. } => *record = taken.pop(),
            Pending::Export { records, .. } | Pending::Import { records, .. } => *records = taken,
            _ => {}
        }
        Ok(())
    }

    /// The reply to a call `before` looked at, laid out as its parameters
    /// were: record what RM did, and rewrite what it says of video memory.
    pub(crate) fn after(&mut self, p: Pending, owner: Owner, reply: &mut [u8]) -> After {
        let mut out = After::default();
        let Some(o) = &mut self.on else {
            return out;
        };
        let l = o.l;
        let ok = |at: usize, reply: &[u8]| le::u32_at(reply, at) == Some(l.nv_ok);
        match p {
            Pending::Nothing | Pending::Refuse { .. } => {}
            Pending::Alloc {
                client,
                handle,
                status,
                size,
                want,
                record,
                ..
            } => {
                let (true, Some(c), Some(h)) = (
                    ok(status, reply),
                    le::u32_at(reply, client),
                    le::u32_at(reply, handle),
                ) else {
                    return out;
                };
                // Admitted in `admit`; a caller that did not ask (none
                // does for video memory) still gets it counted.
                let record = record.unwrap_or_else(|| o.records.hold(owner, 1));
                // RM's size, rounded up to its page; never less than what
                // was admitted. Counted whatever the pool says: RM holds it.
                let got = le::u64_at(reply, size).unwrap_or(want).max(want);
                let mem = Arc::new(o.bytes.hold(owner, got));
                o.handles.insert(
                    (c, h),
                    Held {
                        mem,
                        _record: record,
                    },
                );
            }
            Pending::Free {
                client,
                handle,
                status,
            } => {
                if let (true, Some(c), Some(h)) = (
                    ok(status, reply),
                    le::u32_at(reply, client),
                    le::u32_at(reply, handle),
                ) {
                    out.freed = Some((c, h));
                }
            }
            Pending::Dup { mem, record } => {
                let get = |at| le::u32_at(reply, at);
                if let (Some(0), Some(c), Some(h)) = (
                    get(NVOS55_STATUS),
                    get(NVOS55_H_CLIENT),
                    get(NVOS55_H_OBJECT),
                ) {
                    let record = record.unwrap_or_else(|| o.records.hold(owner, 1));
                    o.handles.insert(
                        (c, h),
                        Held {
                            mem,
                            _record: record,
                        },
                    );
                }
            }
            Pending::Export {
                fd,
                first,
                mem,
                mut records,
            } => {
                if !ok(l.nvos54_status, reply) {
                    return out;
                }
                for (i, m) in mem.into_iter().enumerate() {
                    let slot = first.saturating_add(i as u32);
                    // A slot exported again lets go of what it held.
                    o.exports.remove(&(fd, slot));
                    if let Some(mem) = m {
                        let record = records.pop().unwrap_or_else(|| o.records.hold(owner, 1));
                        o.exports.insert(
                            (fd, slot),
                            Held {
                                mem,
                                _record: record,
                            },
                        );
                    }
                }
            }
            Pending::Import {
                client,
                parent,
                at,
                mut records,
            } => {
                if !ok(l.nvos54_status, reply) {
                    return out;
                }
                for (off, mem) in at {
                    let Some(h) = le::u32_at(reply, off).filter(|&h| h != 0) else {
                        continue;
                    };
                    let record = records.pop().unwrap_or_else(|| o.records.hold(owner, 1));
                    o.handles.insert(
                        (client, h),
                        Held {
                            mem,
                            _record: record,
                        },
                    );
                    out.made.push((client, h, parent));
                }
            }
            Pending::FbInfo => o.rewrite_fb_info(reply),
            Pending::HeapInfo => o.rewrite_heap_info(reply),
            Pending::AllocMemoryLimit => {
                if ok(l.nvos02_status, reply)
                    && let Some(lim) = le::u64_at(reply, l.nvos02_limit)
                {
                    let _ = le::put_u64(reply, l.nvos02_limit, lim.min(o.limit.saturating_sub(1)));
                }
            }
        }
        out
    }

    /// RM freed (client, handle): what it held is no longer this handle's.
    pub(crate) fn forget(&mut self, client: u32, handle: u32) {
        if let Some(o) = &mut self.on {
            o.handles.remove(&(client, handle));
        }
    }

    /// `gone`, clients RM freed with everything they held.
    pub(crate) fn forget_clients(&mut self, gone: &[u32]) {
        if let Some(o) = &mut self.on
            && !gone.is_empty()
        {
            o.handles.retain(|(c, _), _| !gone.contains(c));
        }
    }

    /// Guest file `handle` closed: an export descriptor's slots go with it.
    pub(crate) fn forget_fd(&mut self, handle: u32) {
        if let Some(o) = &mut self.on {
            o.exports.retain(|(fd, _), _| *fd != handle);
        }
    }

    /// Session reset: every client and file is gone.
    pub(crate) fn clear(&mut self) {
        if let Some(o) = &mut self.on {
            o.handles.clear();
            o.exports.clear();
        }
    }
}

impl On {
    fn before_heap(&self, params: &[u8]) -> Pending {
        let l = self.l;
        let Some(f) = le::u32_at(params, l.nvos32_function) else {
            return Pending::Nothing;
        };
        // (hMemory, flags, attr, size) of the allocating functions, and
        // the size RM computes where it computes one.
        let alloc = if f == l.nvos32_function_alloc_size {
            Some((
                l.nvos32_alloc_size_h_memory,
                l.nvos32_alloc_size_flags,
                l.nvos32_alloc_size_attr,
                l.nvos32_alloc_size_size,
            ))
        } else if f == l.nvos32_function_alloc_size_range {
            Some((
                l.nvos32_alloc_size_range_h_memory,
                l.nvos32_alloc_size_range_flags,
                l.nvos32_alloc_size_range_attr,
                l.nvos32_alloc_size_range_size,
            ))
        } else if f == l.nvos32_function_alloc_tiled_pitch_height {
            Some((
                l.nvos32_alloc_tiled_pitch_height_h_memory,
                l.nvos32_alloc_tiled_pitch_height_flags,
                l.nvos32_alloc_tiled_pitch_height_attr,
                l.nvos32_alloc_tiled_pitch_height_size,
            ))
        } else {
            None
        };
        if let Some((handle, flags, attr, size)) = alloc {
            let (Some(flags), Some(attr), Some(asked)) = (
                le::u32_at(params, flags),
                le::u32_at(params, attr),
                le::u64_at(params, size),
            ) else {
                return Pending::Refuse {
                    at: l.nvos32_status,
                    status: NV_ERR_INVALID_PARAM_STRUCT,
                };
            };
            let virt = flags & l.nvos32_alloc_flags_virtual != 0;
            let loc = bit(
                attr,
                l.nvos32_attr_location_shift,
                l.nvos32_attr_location_mask,
            );
            if virt || loc != l.nvos32_attr_location_vidmem {
                return Pending::Nothing;
            }
            // TILED_PITCH_HEIGHT's size is RM's (NvU64)height * pitch, the
            // pitch an NvS32 (rmapi_deprecated_vidheapctrl.c
            // _nvos32FunctionAllocTiledPitchHeight()).
            let want = if f == l.nvos32_function_alloc_tiled_pitch_height {
                let h = le::u32_at(params, l.nvos32_alloc_tiled_pitch_height_height);
                let p = le::i32_at(params, l.nvos32_alloc_tiled_pitch_height_pitch);
                match (h, p) {
                    (Some(h), Some(p)) => u64::from(h).wrapping_mul(i64::from(p) as u64),
                    _ => {
                        return Pending::Refuse {
                            at: l.nvos32_status,
                            status: NV_ERR_INVALID_PARAM_STRUCT,
                        };
                    }
                }
            } else {
                asked
            };
            return Pending::Alloc {
                client: l.nvos32_h_root,
                handle,
                status: l.nvos32_status,
                size,
                want,
                status_in: l.nvos32_status,
                reserved: None,
                record: None,
            };
        }
        if f == l.nvos32_function_free {
            Pending::Free {
                client: l.nvos32_h_root,
                handle: l.nvos32_free_h_memory,
                status: l.nvos32_status,
            }
        } else if f == l.nvos32_function_hw_free {
            Pending::Free {
                client: l.nvos32_h_root,
                handle: l.nvos32_hw_free_h_resource_handle,
                status: l.nvos32_status,
            }
        } else if f == l.nvos32_function_info {
            Pending::HeapInfo
        } else {
            Pending::Nothing
        }
    }

    /// An RM_CONTROL: the NVOS54 block, then its parameters.
    fn before_control(&self, params: &[u8]) -> Pending {
        let l = self.l;
        let (Some(cmd), Some(client), Some(size)) = (
            le::u32_at(params, l.nvos54_cmd),
            le::u32_at(params, l.nvos54_h_client),
            le::u32_at(params, l.nvos54_params_size),
        ) else {
            return Pending::Nothing;
        };
        let base = l.nvos54_sizeof;
        let size = size as usize;
        let Some(p) = params.get(base..).filter(|p| p.len() >= size) else {
            return Pending::Nothing;
        };
        let p = &p[..size];
        let word = |at: usize| le::u32_at(p, at);
        let half = |at: usize| le::uint_at(p, at, 2).map(|v| v as u32);
        let counted = |h: u32| self.handles.get(&(client, h)).map(|x| x.mem.clone());
        if cmd == l.nv2080_ctrl_cmd_fb_get_info_v2 {
            return Pending::FbInfo;
        }
        if cmd == l.nv0000_ctrl_cmd_os_unix_export_object_to_fd
            && size == l.unix_export_object_to_fd_sizeof
        {
            let (Some(kind), Some(obj), Some(fd), Some(flags)) = (
                word(l.unix_export_object_to_fd_type),
                word(l.unix_export_object_to_fd_rm_object_h_object),
                word(l.unix_export_object_to_fd_fd),
                word(l.unix_export_object_to_fd_flags),
            ) else {
                return Pending::Nothing;
            };
            let empty = bit(
                flags,
                l.nv0000_ctrl_os_unix_export_object_to_fd_flags_empty_fd_shift,
                l.nv0000_ctrl_os_unix_export_object_to_fd_flags_empty_fd_mask,
            ) != 0;
            if kind != l.nv0000_ctrl_os_unix_export_object_type_rm || empty {
                return Pending::Nothing;
            }
            return Pending::Export {
                fd,
                first: 0,
                mem: vec![counted(obj)],
                records: Vec::new(),
            };
        }
        if cmd == l.nv0000_ctrl_cmd_os_unix_export_objects_to_fd
            && size == l.unix_export_objects_to_fd_sizeof
        {
            let (Some(fd), Some(n), Some(first)) = (
                word(l.unix_export_objects_to_fd_fd),
                half(l.unix_export_objects_to_fd_num_objects),
                half(l.unix_export_objects_to_fd_index),
            ) else {
                return Pending::Nothing;
            };
            // RM refuses more, and changes nothing.
            if n > l.nv0000_ctrl_os_unix_export_objects_to_fd_max_objects {
                return Pending::Nothing;
            }
            let mem = (0..n as usize)
                .map(|i| {
                    word(l.unix_export_objects_to_fd_objects + 4 * i)
                        .filter(|&h| h != 0)
                        .and_then(counted)
                })
                .collect();
            return Pending::Export {
                fd,
                first,
                mem,
                records: Vec::new(),
            };
        }
        let import = if cmd == l.nv0000_ctrl_cmd_os_unix_import_object_from_fd
            && size == l.unix_import_object_from_fd_sizeof
        {
            word(l.unix_import_object_from_fd_fd).zip(
                word(l.unix_import_object_from_fd_rm_object_h_parent)
                    .map(|p| (p, 0, 1, l.unix_import_object_from_fd_rm_object_h_object)),
            )
        } else if cmd == l.nv0000_ctrl_cmd_os_unix_import_objects_from_fd
            && size == l.unix_import_objects_from_fd_sizeof
        {
            match (
                word(l.unix_import_objects_from_fd_fd),
                word(l.unix_import_objects_from_fd_h_parent),
                half(l.unix_import_objects_from_fd_index),
                half(l.unix_import_objects_from_fd_num_objects),
            ) {
                (Some(fd), Some(parent), Some(first), Some(n))
                    if n <= l.nv0000_ctrl_os_unix_import_objects_to_fd_max_objects =>
                {
                    Some((
                        fd,
                        (parent, first, n, l.unix_import_objects_from_fd_objects),
                    ))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((fd, (parent, first, n, objects))) = import {
            let at = (0..n)
                .filter_map(|i| {
                    let m = self.exports.get(&(fd, first.checked_add(i)?))?;
                    Some((base + objects + 4 * i as usize, m.mem.clone()))
                })
                .collect();
            return Pending::Import {
                client,
                parent,
                at,
                records: Vec::new(),
            };
        }
        Pending::Nothing
    }

    /// What the VM may be told of video memory, in KiB: the limit, and what
    /// it has left of it.
    fn kib(&self) -> (u32, u32) {
        let limit = self.limit >> 10;
        let used = self.bytes.in_use().div_ceil(1024);
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        (clamp(limit), clamp(limit.saturating_sub(used)))
    }

    /// NV2080_CTRL_CMD_FB_GET_INFO_V2's reply: the NVOS54 block, then the
    /// list RM filled in.
    fn rewrite_fb_info(&self, reply: &mut [u8]) {
        let l = self.l;
        if le::u32_at(reply, l.nvos54_status) != Some(l.nv_ok)
            || le::u32_at(reply, l.nvos54_params_size) != Some(l.fb_get_info_v2_sizeof as u32)
        {
            return;
        }
        let base = l.nvos54_sizeof;
        let Some(p) = reply.get_mut(base..base + l.fb_get_info_v2_sizeof) else {
            return;
        };
        let n = le::u32_at(p, l.fb_get_info_v2_fb_info_list_size)
            .unwrap_or(0)
            .min(l.nv2080_ctrl_fb_info_max_list_size) as usize;
        let (limit, free) = self.kib();
        let sizes = [
            l.nv2080_ctrl_fb_info_index_ram_size,
            l.nv2080_ctrl_fb_info_index_total_ram_size,
            l.nv2080_ctrl_fb_info_index_heap_size,
            l.nv2080_ctrl_fb_info_index_mappable_heap_size,
            l.nv2080_ctrl_fb_info_index_usable_ram_size,
        ];
        let frees = [
            Some(l.nv2080_ctrl_fb_info_index_heap_free),
            Some(l.nv2080_ctrl_fb_info_index_largest_free_region_size_kb),
            l.nv2080_ctrl_fb_info_index_heap_reclaimable,
        ];
        for i in 0..n {
            let e = l.fb_get_info_v2_fb_info_list + i * l.fb_info_sizeof;
            let (Some(index), Some(data)) = (
                le::u32_at(p, e + l.fb_info_index),
                le::u32_at(p, e + l.fb_info_data),
            ) else {
                break;
            };
            let cap = if sizes.contains(&index) {
                limit
            } else if frees.contains(&Some(index)) {
                free
            } else {
                continue;
            };
            let _ = le::put_u32(p, e + l.fb_info_data, data.min(cap));
        }
    }

    /// VID_HEAP_CONTROL INFO: `free` and `total` in bytes, and the largest
    /// free region's size.
    fn rewrite_heap_info(&self, reply: &mut [u8]) {
        let l = self.l;
        if le::u32_at(reply, l.nvos32_status) != Some(l.nv_ok) {
            return;
        }
        let free = self.limit.saturating_sub(self.bytes.in_use());
        for (at, cap) in [
            (l.nvos32_total, self.limit),
            (l.nvos32_free, free),
            (l.nvos32_info_size, free),
        ] {
            if let Some(v) = le::u64_at(reply, at) {
                let _ = le::put_u64(reply, at, v.min(cap));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::le::{put_u32, put_u64, u32_at, u64_at};

    const MIB: u64 = 1 << 20;
    const CLIENT: u32 = 0xc1d0_0001;

    fn p(tgid: u32) -> Owner {
        Owner::Proc {
            tgid,
            start_ns: u64::from(tgid),
        }
    }

    fn layouts() -> impl Iterator<Item = (&'static Layout, DriverVersion)> {
        abi::vidmem::LAYOUTS
            .iter()
            .map(|l| (l, DriverVersion::new(l.version.0, l.version.1, l.version.2)))
    }

    fn vram(mib: u64, v: DriverVersion) -> Vram {
        let mut v2 = Vram::default();
        v2.set_limit(mib, 50, Some(v)).unwrap();
        v2
    }

    /// An RM_ALLOC of NV01_MEMORY_LOCAL_USER, `size` bytes, as sent.
    fn local_user(l: &Layout, handle: u32, size: u64) -> Vec<u8> {
        let mut b = vec![0u8; l.nvos64_sizeof + l.nv_memory_allocation_sizeof];
        put_u32(&mut b, l.nvos64_h_root, CLIENT);
        put_u32(&mut b, l.nvos64_h_object_new, handle);
        put_u32(&mut b, l.nvos64_h_class, l.nv01_memory_local_user);
        put_u64(&mut b, l.nvos64_sizeof + l.nv_memory_allocation_size, size);
        b
    }

    /// Run `params` through before, admit, a host that answers `status`
    /// (and, for an allocation, rounds its size up to 64 KiB), and after.
    /// `Err` is the refusal.
    fn run(
        v: &mut Vram,
        o: Owner,
        escape: u32,
        params: &[u8],
        status_at: usize,
        status: u32,
    ) -> Result<(Vec<u8>, After), (usize, u32)> {
        let mut pend = v.before(escape, params);
        v.admit(&mut pend, o)?;
        let mut reply = params.to_vec();
        put_u32(&mut reply, status_at, status);
        if let Pending::Alloc { size, .. } = &pend
            && let Some(s) = u64_at(&reply, *size)
        {
            put_u64(&mut reply, *size, s.div_ceil(64 << 10) * (64 << 10));
        }
        let a = v.after(pend, o, &mut reply);
        Ok((reply, a))
    }

    fn alloc(v: &mut Vram, l: &Layout, o: Owner, h: u32, size: u64) -> Result<(), (usize, u32)> {
        run(
            v,
            o,
            NV_ESC_RM_ALLOC,
            &local_user(l, h, size),
            l.nvos64_status,
            0,
        )
        .map(|_| ())
    }

    #[test]
    fn nothing_is_counted_or_rewritten_without_a_limit() {
        let (l, _) = layouts().next().unwrap();
        let mut v = Vram::default();
        let req = local_user(l, 1, 1 << 40);
        let mut pend = v.before(NV_ESC_RM_ALLOC, &req);
        assert!(matches!(pend, Pending::Nothing));
        assert_eq!(v.admit(&mut pend, p(1)), Ok(()));
        let mut reply = req.clone();
        v.after(pend, p(1), &mut reply);
        assert_eq!(reply, req);
        assert!(!v.watches(NV_ESC_RM_CONTROL));
        assert_eq!((v.limit(), v.in_use()), (None, 0));
    }

    #[test]
    fn a_limit_needs_the_exact_release_and_a_sane_size() {
        let mut v = Vram::default();
        assert_eq!(
            v.set_limit(4096, 50, Some(DriverVersion::new(600, 1, 0))),
            Err(LimitError::NoLayout(DriverVersion::new(600, 1, 0)))
        );
        assert_eq!(v.set_limit(4096, 50, None), Err(LimitError::NoRelease));
        let (_, ver) = layouts().next().unwrap();
        assert_eq!(v.set_limit(32, 50, Some(ver)), Err(LimitError::TooSmall));
        assert_eq!(v.set_limit(4096, 50, Some(ver)), Ok(()));
        assert_eq!(v.limit(), Some(4096 * MIB));
    }

    /// Allocations are counted at RM's size and freed with their handle;
    /// past the limit RM is not asked, and the caller's block says
    /// NV_ERR_NO_MEMORY; one process takes at most its share.
    #[test]
    fn allocations_are_counted_refused_past_the_limit_and_split_per_process() {
        for (l, ver) in layouts() {
            let mut v = vram(1024, ver);
            // RM rounds 100 KiB up to 128 KiB.
            alloc(&mut v, l, p(1), 0x10, 100 << 10).unwrap();
            assert_eq!(v.in_use(), 128 << 10, "{ver}");
            assert_eq!(v.held(p(1)), 128 << 10);
            // Half the limit per process (`--window-owner-share` 50).
            assert_eq!(
                alloc(&mut v, l, p(1), 0x11, 512 * MIB),
                Err((l.nvos64_status, 0x51)),
                "{ver}"
            );
            alloc(&mut v, l, p(1), 0x11, 400 * MIB).unwrap();
            alloc(&mut v, l, p(2), 0x20, 450 * MIB).unwrap();
            // The VM's limit: 1024 MiB.
            assert_eq!(
                alloc(&mut v, l, p(3), 0x30, 200 * MIB),
                Err((l.nvos64_status, 0x51))
            );
            assert_eq!(v.refused(), 2);
            v.forget(CLIENT, 0x11);
            alloc(&mut v, l, p(3), 0x30, 200 * MIB).unwrap();
            // A failed allocation holds nothing.
            let before = v.in_use();
            run(
                &mut v,
                p(4),
                NV_ESC_RM_ALLOC,
                &local_user(l, 0x40, MIB),
                l.nvos64_status,
                0x51,
            )
            .unwrap();
            assert_eq!(v.in_use(), before);
            // Not video memory: not counted.
            let mut sys = local_user(l, 0x41, 900 * MIB);
            put_u32(&mut sys, l.nvos64_h_class, 0x3e);
            run(&mut v, p(4), NV_ESC_RM_ALLOC, &sys, l.nvos64_status, 0).unwrap();
            assert_eq!(v.in_use(), before);
            v.clear();
            assert_eq!(v.in_use(), 0, "a reset gives everything back");
        }
    }

    #[test]
    fn a_block_too_short_for_the_size_is_refused() {
        for (l, ver) in layouts() {
            let mut v = vram(1024, ver);
            let mut req = local_user(l, 1, MIB);
            req.truncate(l.nvos64_sizeof + l.nv_memory_allocation_size + 4);
            assert_eq!(
                run(&mut v, p(1), NV_ESC_RM_ALLOC, &req, l.nvos64_status, 0).unwrap_err(),
                (l.nvos64_status, NV_ERR_INVALID_PARAM_STRUCT)
            );
            assert!(v.refuses_unseen(NV_ESC_RM_ALLOC, &req));
        }
    }

    fn heap(l: &Layout, func: u32, loc: u32, virt: bool, size: u64) -> Vec<u8> {
        let mut b = vec![0u8; l.nvos32_sizeof];
        put_u32(&mut b, l.nvos32_h_root, CLIENT);
        put_u32(&mut b, l.nvos32_function, func);
        let (h, f, a, s) = if func == l.nvos32_function_alloc_size {
            (
                l.nvos32_alloc_size_h_memory,
                l.nvos32_alloc_size_flags,
                l.nvos32_alloc_size_attr,
                l.nvos32_alloc_size_size,
            )
        } else if func == l.nvos32_function_alloc_size_range {
            (
                l.nvos32_alloc_size_range_h_memory,
                l.nvos32_alloc_size_range_flags,
                l.nvos32_alloc_size_range_attr,
                l.nvos32_alloc_size_range_size,
            )
        } else {
            (
                l.nvos32_alloc_tiled_pitch_height_h_memory,
                l.nvos32_alloc_tiled_pitch_height_flags,
                l.nvos32_alloc_tiled_pitch_height_attr,
                l.nvos32_alloc_tiled_pitch_height_size,
            )
        };
        put_u32(&mut b, h, 0x500 + func);
        put_u32(
            &mut b,
            f,
            if virt {
                l.nvos32_alloc_flags_virtual
            } else {
                0
            },
        );
        put_u32(&mut b, a, loc << l.nvos32_attr_location_shift);
        put_u64(&mut b, s, size);
        b
    }

    /// VID_HEAP_CONTROL's allocating functions count what RM makes video
    /// memory of -- LOCATION_VIDMEM, not VIRTUAL -- and FREE and HW_FREE
    /// say which handle RM freed.
    #[test]
    fn vid_heap_allocations_of_video_memory_are_counted() {
        for (l, ver) in layouts() {
            for func in [
                l.nvos32_function_alloc_size,
                l.nvos32_function_alloc_size_range,
            ] {
                for (loc, virt, counted) in [
                    (0, false, true),
                    (1, false, false),
                    (3, false, false),
                    (0, true, false),
                ] {
                    let mut v = vram(1024, ver);
                    let b = heap(l, func, loc, virt, 3 * MIB);
                    run(
                        &mut v,
                        p(1),
                        NV_ESC_RM_VID_HEAP_CONTROL,
                        &b,
                        l.nvos32_status,
                        0,
                    )
                    .unwrap();
                    assert_eq!(
                        v.in_use() == 3 * MIB,
                        counted,
                        "{ver} function {func} location {loc} virtual {virt}"
                    );
                    let over = heap(l, func, loc, virt, 2048 * MIB);
                    assert_eq!(
                        run(
                            &mut v,
                            p(1),
                            NV_ESC_RM_VID_HEAP_CONTROL,
                            &over,
                            l.nvos32_status,
                            0
                        )
                        .is_err(),
                        counted
                    );
                }
            }
            // TILED_PITCH_HEIGHT: the size is height * pitch, whatever the
            // size field says.
            let mut v = vram(1024, ver);
            let mut t = heap(l, l.nvos32_function_alloc_tiled_pitch_height, 0, false, 0);
            put_u32(&mut t, l.nvos32_alloc_tiled_pitch_height_height, 1 << 20);
            put_u32(&mut t, l.nvos32_alloc_tiled_pitch_height_pitch, 1 << 11);
            assert_eq!(
                run(
                    &mut v,
                    p(1),
                    NV_ESC_RM_VID_HEAP_CONTROL,
                    &t,
                    l.nvos32_status,
                    0
                )
                .unwrap_err(),
                (l.nvos32_status, 0x51),
                "{ver}: 2 GiB of pitch by height"
            );
            put_u32(&mut t, l.nvos32_alloc_tiled_pitch_height_pitch, u32::MAX);
            assert!(
                run(
                    &mut v,
                    p(1),
                    NV_ESC_RM_VID_HEAP_CONTROL,
                    &t,
                    l.nvos32_status,
                    0
                )
                .is_err(),
                "a negative pitch is RM's huge size"
            );
            // FREE and HW_FREE name the handle RM freed.
            for (func, at) in [
                (l.nvos32_function_free, l.nvos32_free_h_memory),
                (
                    l.nvos32_function_hw_free,
                    l.nvos32_hw_free_h_resource_handle,
                ),
            ] {
                let mut b = vec![0u8; l.nvos32_sizeof];
                put_u32(&mut b, l.nvos32_h_root, CLIENT);
                put_u32(&mut b, l.nvos32_function, func);
                put_u32(&mut b, at, 0x77);
                let (_, a) = run(
                    &mut v,
                    p(1),
                    NV_ESC_RM_VID_HEAP_CONTROL,
                    &b,
                    l.nvos32_status,
                    0,
                )
                .unwrap();
                assert_eq!(a.freed, Some((CLIENT, 0x77)), "{ver}");
                let (_, a) = run(
                    &mut v,
                    p(1),
                    NV_ESC_RM_VID_HEAP_CONTROL,
                    &b,
                    l.nvos32_status,
                    0x1f,
                )
                .unwrap();
                assert_eq!(a.freed, None, "RM freed nothing");
            }
        }
    }

    fn dup(dst: (u32, u32), src: (u32, u32)) -> Vec<u8> {
        let mut d = vec![0u8; NVOS55_SIZE];
        put_u32(&mut d, NVOS55_H_CLIENT, dst.0);
        put_u32(&mut d, NVOS55_H_OBJECT, dst.1);
        put_u32(&mut d, NVOS55_H_CLIENT_SRC, src.0);
        put_u32(&mut d, NVOS55_H_OBJECT_SRC, src.1);
        d
    }

    /// A duplicate is the same memory: counted once, to the maker, until
    /// the last handle of it goes, whichever that is.
    #[test]
    fn a_duplicate_is_counted_once_to_the_owner_until_the_last_handle_goes() {
        for (l, ver) in layouts() {
            let mut v = vram(1024, ver);
            alloc(&mut v, l, p(1), 0x10, 64 * MIB).unwrap();
            for h in [0x20, 0x21] {
                run(
                    &mut v,
                    p(2),
                    NV_ESC_RM_DUP_OBJECT,
                    &dup((0xbeef, h), (CLIENT, 0x10)),
                    NVOS55_STATUS,
                    0,
                )
                .unwrap();
            }
            assert_eq!(
                (v.in_use(), v.held(p(1)), v.held(p(2))),
                (64 * MIB, 64 * MIB, 0)
            );
            v.forget(CLIENT, 0x10);
            v.forget(0xbeef, 0x20);
            assert_eq!(v.in_use(), 64 * MIB, "{ver}: one duplicate still holds it");
            v.forget_clients(&[0xbeef]);
            assert_eq!(v.in_use(), 0, "{ver}");
            // A duplicate RM refused, or of memory not counted, records
            // nothing.
            alloc(&mut v, l, p(1), 0x10, MIB).unwrap();
            run(
                &mut v,
                p(2),
                NV_ESC_RM_DUP_OBJECT,
                &dup((0xbeef, 0x22), (CLIENT, 0x10)),
                NVOS55_STATUS,
                0x1f,
            )
            .unwrap();
            v.forget(CLIENT, 0x10);
            assert_eq!(v.in_use(), 0);
        }
    }

    /// A process that duplicates its memory in a loop fills its part of
    /// the records, not the table: past it the duplicate is refused.
    #[test]
    fn duplicates_are_bounded_per_process() {
        let (l, ver) = layouts().next().unwrap();
        let mut v = vram(1024, ver);
        alloc(&mut v, l, p(1), 0x10, MIB).unwrap();
        let share = RECORD_SHARE.per_owner;
        let mut refused = None;
        for i in 0..share as u32 + 10 {
            if let Err(e) = run(
                &mut v,
                p(1),
                NV_ESC_RM_DUP_OBJECT,
                &dup((CLIENT, 0x10_0000 + i), (CLIENT, 0x10)),
                NVOS55_STATUS,
                0,
            ) {
                refused = Some((i, e));
                break;
            }
        }
        // The allocation holds one record.
        assert_eq!(refused, Some((share as u32 - 1, (NVOS55_STATUS, 0x51))));
        // Another process still records its own.
        alloc(&mut v, l, p(2), 0x20, MIB).unwrap();
    }

    fn control(l: &Layout, cmd: u32, params: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; l.nvos54_sizeof];
        put_u32(&mut b, l.nvos54_h_client, CLIENT);
        put_u32(&mut b, l.nvos54_cmd, cmd);
        put_u32(&mut b, l.nvos54_params_size, params.len() as u32);
        b.extend_from_slice(params);
        b
    }

    /// Memory exported to a descriptor stays counted while the descriptor
    /// holds it; a handle imported from it is the same memory.
    #[test]
    fn exported_and_imported_memory_stays_counted_to_its_maker() {
        for (l, ver) in layouts() {
            let mut v = vram(1024, ver);
            alloc(&mut v, l, p(1), 0x10, 32 * MIB).unwrap();
            alloc(&mut v, l, p(1), 0x11, 16 * MIB).unwrap();
            const FD: u32 = 9;
            // EXPORT_OBJECTS_TO_FD: 0x10 at slot 3, 0x11 at slot 4, a
            // handle not counted at slot 5.
            let mut e = vec![0u8; l.unix_export_objects_to_fd_sizeof];
            put_u32(&mut e, l.unix_export_objects_to_fd_fd, FD);
            put_u32(&mut e, l.unix_export_objects_to_fd_objects, 0x10);
            put_u32(&mut e, l.unix_export_objects_to_fd_objects + 4, 0x11);
            put_u32(&mut e, l.unix_export_objects_to_fd_objects + 8, 0x99);
            e[l.unix_export_objects_to_fd_num_objects] = 3;
            e[l.unix_export_objects_to_fd_index] = 3;
            let cmd = control(l, l.nv0000_ctrl_cmd_os_unix_export_objects_to_fd, &e);
            run(&mut v, p(1), NV_ESC_RM_CONTROL, &cmd, l.nvos54_status, 0).unwrap();
            v.forget_clients(&[CLIENT]);
            assert_eq!(v.in_use(), 48 * MIB, "{ver}: the descriptor holds both");
            // IMPORT_OBJECTS_FROM_FD of slots 3 and 4 into another client.
            let mut i = vec![0u8; l.unix_import_objects_from_fd_sizeof];
            put_u32(&mut i, l.unix_import_objects_from_fd_fd, FD);
            put_u32(&mut i, l.unix_import_objects_from_fd_h_parent, 0xde7);
            put_u32(&mut i, l.unix_import_objects_from_fd_objects, 0x30);
            put_u32(&mut i, l.unix_import_objects_from_fd_objects + 4, 0x31);
            i[l.unix_import_objects_from_fd_num_objects] = 2;
            i[l.unix_import_objects_from_fd_index] = 3;
            let mut cmd = control(l, l.nv0000_ctrl_cmd_os_unix_import_objects_from_fd, &i);
            put_u32(&mut cmd, l.nvos54_h_client, 0xbeef);
            let (_, a) = run(&mut v, p(2), NV_ESC_RM_CONTROL, &cmd, l.nvos54_status, 0).unwrap();
            assert_eq!(a.made, vec![(0xbeef, 0x30, 0xde7), (0xbeef, 0x31, 0xde7)]);
            v.forget_fd(FD);
            assert_eq!(v.in_use(), 48 * MIB, "{ver}: the imports hold both");
            assert_eq!(v.held(p(1)), 48 * MIB, "still the maker's");
            v.forget(0xbeef, 0x30);
            assert_eq!(v.in_use(), 16 * MIB);
            v.clear();
            assert_eq!(v.in_use(), 0);
            // EXPORT_OBJECT_TO_FD and IMPORT_OBJECT_FROM_FD: slot 0.
            alloc(&mut v, l, p(1), 0x12, 8 * MIB).unwrap();
            let mut e = vec![0u8; l.unix_export_object_to_fd_sizeof];
            put_u32(
                &mut e,
                l.unix_export_object_to_fd_type,
                l.nv0000_ctrl_os_unix_export_object_type_rm,
            );
            put_u32(&mut e, l.unix_export_object_to_fd_rm_object_h_object, 0x12);
            put_u32(&mut e, l.unix_export_object_to_fd_fd, FD + 1);
            let cmd = control(l, l.nv0000_ctrl_cmd_os_unix_export_object_to_fd, &e);
            run(&mut v, p(1), NV_ESC_RM_CONTROL, &cmd, l.nvos54_status, 0).unwrap();
            let mut i = vec![0u8; l.unix_import_object_from_fd_sizeof];
            put_u32(&mut i, l.unix_import_object_from_fd_fd, FD + 1);
            put_u32(
                &mut i,
                l.unix_import_object_from_fd_rm_object_h_object,
                0x40,
            );
            let cmd = control(l, l.nv0000_ctrl_cmd_os_unix_import_object_from_fd, &i);
            run(&mut v, p(2), NV_ESC_RM_CONTROL, &cmd, l.nvos54_status, 0).unwrap();
            v.forget(CLIENT, 0x12);
            v.forget_fd(FD + 1);
            assert_eq!(v.in_use(), 8 * MIB, "{ver}: the import holds it");
            v.forget(CLIENT, 0x40);
            assert_eq!(v.in_use(), 0);
        }
    }

    fn fb_info(l: &Layout, list: &[(u32, u32)]) -> Vec<u8> {
        let mut p = vec![0u8; l.fb_get_info_v2_sizeof];
        put_u32(
            &mut p,
            l.fb_get_info_v2_fb_info_list_size,
            list.len() as u32,
        );
        for (i, (index, data)) in list.iter().enumerate() {
            let e = l.fb_get_info_v2_fb_info_list + i * l.fb_info_sizeof;
            put_u32(&mut p, e + l.fb_info_index, *index);
            put_u32(&mut p, e + l.fb_info_data, *data);
        }
        control(l, l.nv2080_ctrl_cmd_fb_get_info_v2, &p)
    }

    fn fb_data(l: &Layout, reply: &[u8], i: usize) -> u32 {
        let e = l.nvos54_sizeof + l.fb_get_info_v2_fb_info_list + i * l.fb_info_sizeof;
        u32_at(reply, e + l.fb_info_data).unwrap()
    }

    /// FB_GET_INFO_V2, as the RTX 5090 answers nvidia-smi and CUDA: the
    /// sizes say the limit, the free ones what is left of it, each never
    /// more than RM said; every other index is RM's.
    #[test]
    fn fb_get_info_reports_the_limit_per_release() {
        for (l, ver) in layouts() {
            let mut v = vram(4096, ver);
            alloc(&mut v, l, p(1), 0x10, 1000 * MIB).unwrap();
            let mut list = vec![
                (l.nv2080_ctrl_fb_info_index_total_ram_size, 33_389_568),
                (l.nv2080_ctrl_fb_info_index_ram_size, 33_389_568),
                (l.nv2080_ctrl_fb_info_index_heap_size, 32_847_104),
                (l.nv2080_ctrl_fb_info_index_usable_ram_size, 33_011_840),
                (l.nv2080_ctrl_fb_info_index_mappable_heap_size, 32_847_104),
                (l.nv2080_ctrl_fb_info_index_heap_free, 28_344_768),
                (
                    l.nv2080_ctrl_fb_info_index_largest_free_region_size_kb,
                    28_000_000,
                ),
                // BUS_WIDTH (0xb in every release measured): not ours.
                (0xb, 512),
                // A host short of video memory: less than the VM has left.
                (l.nv2080_ctrl_fb_info_index_heap_free, 1_000),
            ];
            if let Some(r) = l.nv2080_ctrl_fb_info_index_heap_reclaimable {
                list.push((r, 146_116_000));
            }
            let req = fb_info(l, &list);
            assert!(v.watches(NV_ESC_RM_CONTROL));
            let (reply, _) =
                run(&mut v, p(2), NV_ESC_RM_CONTROL, &req, l.nvos54_status, 0).unwrap();
            let got: Vec<u32> = (0..list.len()).map(|i| fb_data(l, &reply, i)).collect();
            let (limit, free) = (4096 * 1024, (4096 - 1000) * 1024);
            let mut want = vec![limit, limit, limit, limit, limit, free, free, 512, 1_000];
            if l.nv2080_ctrl_fb_info_index_heap_reclaimable.is_some() {
                want.push(free);
            }
            assert_eq!(got, want, "{ver}");
            // A failed call, or a list of another size, is left alone.
            let (reply, _) =
                run(&mut v, p(2), NV_ESC_RM_CONTROL, &req, l.nvos54_status, 0x1f).unwrap();
            assert_eq!(fb_data(l, &reply, 0), 33_389_568);
            let mut short = req.clone();
            short.truncate(req.len() - 4);
            put_u32(
                &mut short,
                l.nvos54_params_size,
                (l.fb_get_info_v2_sizeof - 4) as u32,
            );
            let (reply, _) =
                run(&mut v, p(2), NV_ESC_RM_CONTROL, &short, l.nvos54_status, 0).unwrap();
            assert_eq!(fb_data(l, &reply, 0), 33_389_568);
            // A limit above the GPU: RM's own sizes.
            let mut big = vram(65536, ver);
            let (reply, _) =
                run(&mut big, p(2), NV_ESC_RM_CONTROL, &req, l.nvos54_status, 0).unwrap();
            assert_eq!(fb_data(l, &reply, 0), 33_389_568);
            assert_eq!(fb_data(l, &reply, 5), 28_344_768);
        }
    }

    /// A list count past RM's maximum, or past the block, reads no further
    /// than the block.
    #[test]
    fn a_list_count_is_bounded_by_the_block() {
        for (l, ver) in layouts() {
            let mut v = vram(4096, ver);
            let mut req = fb_info(l, &[(l.nv2080_ctrl_fb_info_index_heap_size, u32::MAX)]);
            put_u32(
                &mut req,
                l.nvos54_sizeof + l.fb_get_info_v2_fb_info_list_size,
                u32::MAX,
            );
            let (reply, _) =
                run(&mut v, p(1), NV_ESC_RM_CONTROL, &req, l.nvos54_status, 0).unwrap();
            assert_eq!(fb_data(l, &reply, 0), 4096 * 1024, "{ver}");
            assert_eq!(reply.len(), req.len());
        }
    }

    #[test]
    fn heap_info_and_alloc_memory_report_the_limit() {
        for (l, ver) in layouts() {
            let mut v = vram(2048, ver);
            alloc(&mut v, l, p(1), 0x10, 48 * MIB).unwrap();
            let mut b = vec![0u8; l.nvos32_sizeof];
            put_u32(&mut b, l.nvos32_function, l.nvos32_function_info);
            put_u64(&mut b, l.nvos32_total, 32 << 30);
            put_u64(&mut b, l.nvos32_free, 28 << 30);
            put_u64(&mut b, l.nvos32_info_size, 27 << 30);
            let (r, _) = run(
                &mut v,
                p(1),
                NV_ESC_RM_VID_HEAP_CONTROL,
                &b,
                l.nvos32_status,
                0,
            )
            .unwrap();
            assert_eq!(u64_at(&r, l.nvos32_total), Some(2048 * MIB), "{ver}");
            assert_eq!(u64_at(&r, l.nvos32_free), Some(2000 * MIB));
            assert_eq!(u64_at(&r, l.nvos32_info_size), Some(2000 * MIB));
            let mut m = vec![0u8; crate::nvos::NVOS02_WITH_FD_SIZE];
            put_u32(&mut m, l.nvos02_h_class, l.nv01_memory_local_user);
            put_u64(&mut m, l.nvos02_limit, (32 << 30) - 1);
            let (r, _) = run(&mut v, p(1), NV_ESC_RM_ALLOC_MEMORY, &m, l.nvos02_status, 0).unwrap();
            assert_eq!(u64_at(&r, l.nvos02_limit), Some(2048 * MIB - 1));
            put_u32(&mut m, l.nvos02_h_class, 0x3e);
            let (r, _) = run(&mut v, p(1), NV_ESC_RM_ALLOC_MEMORY, &m, l.nvos02_status, 0).unwrap();
            assert_eq!(
                u64_at(&r, l.nvos02_limit),
                Some((32 << 30) - 1),
                "system memory"
            );
        }
    }

    /// The FB_INFO indices are the host release's own: 610.57.04 has no
    /// HEAP_RECLAIMABLE, and 595.99.02's (0x3c) is not 615.71.09's (0x44).
    #[test]
    fn the_indices_rewritten_are_the_releases_own() {
        let idx = |v: (u32, u32, u32)| {
            abi::vidmem::layout_for(DriverVersion::new(v.0, v.1, v.2))
                .unwrap()
                .nv2080_ctrl_fb_info_index_heap_reclaimable
        };
        assert_eq!(idx((595, 99, 2)), Some(0x3c));
        assert_eq!(idx((610, 57, 4)), None);
        assert_eq!(idx((615, 71, 9)), Some(0x44));
        // On 610.57.04 index 0x3c is not rewritten.
        let l = abi::vidmem::layout_for(DriverVersion::new(610, 57, 4)).unwrap();
        let mut v = vram(4096, DriverVersion::new(610, 57, 4));
        let req = fb_info(l, &[(0x3c, u32::MAX)]);
        let (reply, _) = run(&mut v, p(1), NV_ESC_RM_CONTROL, &req, l.nvos54_status, 0).unwrap();
        assert_eq!(fb_data(l, &reply, 0), u32::MAX);
    }
}

/// Whole calls through the backend's v1 RM path, against the fake host RM
/// (testing/rm.rs): a refusal is one the host never saw, and a free is the
/// tree's, cascading as RM's does.
#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::hostfd::{HandleKind, IOC_RW, ioc};
    use crate::le::{put_u32, put_u64, u32_at};
    use crate::nvidia::NvidiaBackend;
    use crate::nvos::{
        NV01_DEVICE_0, NVOS00_SIZE, NVOS54_SIZE, NVOS54_STATUS, NVOS64_SIZE, NVOS64_STATUS,
    };
    use crate::testing::rm;
    use protocol::messages::{
        DeviceKind, GCAP_PROC_EUID, GCAP_PROC_ID, HELLO_F_FRESH, HelloReq, MsgType, PROTO_V2,
        ProcId,
    };
    use std::os::fd::OwnedFd;

    const RELEASE: &str = "595.99.02";
    const ALLOC: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
    const CONTROL: u32 = ioc(IOC_RW, b'F', 0x2a, 32);
    const FREE: u32 = ioc(IOC_RW, b'F', 0x29, 16);
    const HEAP: u32 = ioc(IOC_RW, b'F', 0x4a, 184);
    /// Where a v1 reply's parameters start: MsgHeader, IoctlResp.
    const BODY: usize = 16 + 12;
    const DEVICE: u32 = 0xde7;
    const SUBDEVICE: u32 = 0x2080;
    const MIB: u64 = 1 << 20;

    fn layout() -> &'static Layout {
        abi::vidmem::layout_for(DriverVersion::parse(RELEASE).unwrap()).unwrap()
    }

    fn pid(tgid: u32) -> ProcId {
        ProcId {
            start_ns: 1_000_000 + u64::from(tgid),
            tgid,
            euid: 1000,
        }
    }

    /// A v2 session on host release 595.99.02 that says who calls, a
    /// `limit` of video memory if any, and a control file.
    fn vm(limit: Option<u64>) -> (NvidiaBackend, u32) {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        rm::install(&mut be);
        be.set_host_driver_version(RELEASE);
        if let Some(mib) = limit {
            be.set_vram_limit(mib, 50).unwrap();
        }
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID | GCAP_PROC_EUID,
            uvm_aperture_mib: 0,
        };
        let mut req = Vec::new();
        for v in [MsgType::Hello as u32, 0, 0, 1] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            hello.proto,
            hello.flags,
            hello.guest_caps,
            hello.uvm_aperture_mib,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        let mut resp = vec![0u8; 256];
        be.dispatch(&req, &mut resp);
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let f = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        (be, f)
    }

    fn call(
        be: &mut NvidiaBackend,
        f: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        by: u32,
    ) -> Vec<u8> {
        let mut req = Vec::new();
        let nested_offset = if nested.is_empty() { 0 } else { outer.len() };
        for v in [
            MsgType::Ioctl as u32,
            f,
            0,
            0,
            cmd,
            outer.len() as u32,
            nested_offset as u32,
            nested.len() as u32,
            0,
            0,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        let p = pid(by);
        req.extend_from_slice(&p.start_ns.to_le_bytes());
        req.extend_from_slice(&p.tgid.to_le_bytes());
        req.extend_from_slice(&p.euid.to_le_bytes());
        let mut resp = vec![0u8; 8192];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        assert_eq!(
            u32_at(&resp, 8),
            Some(0),
            "the ioctl succeeds; RM's status says"
        );
        resp
    }

    fn nvos64(client: u32, parent: u32, h: u32, class: u32, params: usize) -> Vec<u8> {
        let mut b = vec![0u8; NVOS64_SIZE];
        put_u32(&mut b, 0, client);
        put_u32(&mut b, 4, parent);
        put_u32(&mut b, 8, h);
        put_u32(&mut b, 12, class);
        put_u32(&mut b, 32, params as u32);
        b
    }

    /// A client on `f`, and its device, allocated through the backend; a
    /// subdevice RM has.
    fn client(be: &mut NvidiaBackend, f: u32) -> u32 {
        let r = call(be, f, ALLOC, &nvos64(0, 0, 0, 0x41, 0), &[], 1);
        let c = u32_at(&r, BODY + 8).unwrap();
        let r = call(
            be,
            f,
            ALLOC,
            &nvos64(c, c, DEVICE, NV01_DEVICE_0, 0),
            &[],
            1,
        );
        assert_eq!(u32_at(&r, BODY + NVOS64_STATUS), Some(0));
        rm::with(|rm| rm.alloc(c, DEVICE, SUBDEVICE, 0x2080)).unwrap();
        c
    }

    /// NV01_MEMORY_LOCAL_USER of `size` bytes under the device, by process
    /// `by`: RM's status.
    fn vidmem(be: &mut NvidiaBackend, f: u32, c: u32, h: u32, size: u64, by: u32) -> u32 {
        let l = layout();
        let mut p = vec![0u8; l.nv_memory_allocation_sizeof];
        put_u64(&mut p, l.nv_memory_allocation_size, size);
        let o = nvos64(c, DEVICE, h, l.nv01_memory_local_user, p.len());
        let r = call(be, f, ALLOC, &o, &p, by);
        u32_at(&r, BODY + NVOS64_STATUS).unwrap()
    }

    fn free(be: &mut NvidiaBackend, f: u32, c: u32, h: u32) {
        let mut b = vec![0u8; NVOS00_SIZE];
        put_u32(&mut b, 0, c);
        put_u32(&mut b, 8, h);
        let r = call(be, f, FREE, &b, &[], 1);
        assert_eq!(u32_at(&r, BODY + 12), Some(0));
    }

    fn held(be: &NvidiaBackend) -> u64 {
        be.vram_usage().map_or(0, |(h, ..)| h)
    }

    #[test]
    fn past_the_limit_rm_is_not_asked_and_the_caller_reads_no_memory() {
        let (mut be, f) = vm(Some(1024));
        let c = client(&mut be, f);
        rm::seen();
        assert_eq!(vidmem(&mut be, f, c, 0x100, 400 * MIB, 1), 0);
        assert_eq!(held(&be), 400 * MIB);
        // Process 1's share is half the limit.
        assert_eq!(vidmem(&mut be, f, c, 0x101, 200 * MIB, 1), 0x51);
        assert_eq!(vidmem(&mut be, f, c, 0x102, 400 * MIB, 2), 0);
        // The VM's limit.
        assert_eq!(vidmem(&mut be, f, c, 0x103, 300 * MIB, 3), 0x51);
        let reached: Vec<u32> = rm::seen().iter().map(|c| c.key).collect();
        assert_eq!(
            reached,
            vec![0x40, 0x40],
            "the refused two never reached RM"
        );
        assert_eq!(be.vram_usage(), Some((800 * MIB, 1024 * MIB, 2)));
    }

    /// RM frees an object's subtree, a client's objects, and the clients of
    /// a closed file: each takes its memory's charge with it, and a session
    /// reset takes the rest.
    #[test]
    fn every_way_rm_frees_memory_gives_its_charge_back() {
        let (mut be, f) = vm(Some(1024));
        let c = client(&mut be, f);
        // The object, and the device above it.
        vidmem(&mut be, f, c, 0x100, 100 * MIB, 1);
        vidmem(&mut be, f, c, 0x101, 100 * MIB, 2);
        free(&mut be, f, c, 0x100);
        assert_eq!(held(&be), 100 * MIB);
        free(&mut be, f, c, DEVICE);
        assert_eq!(held(&be), 0, "freed with the device above it");
        // The client.
        let r = call(
            &mut be,
            f,
            ALLOC,
            &nvos64(c, c, DEVICE, NV01_DEVICE_0, 0),
            &[],
            1,
        );
        assert_eq!(u32_at(&r, BODY + NVOS64_STATUS), Some(0));
        vidmem(&mut be, f, c, 0x100, 100 * MIB, 1);
        free(&mut be, f, c, c);
        assert_eq!(held(&be), 0, "freed with its client");
        // The file the client lives on.
        let c = client(&mut be, f);
        vidmem(&mut be, f, c, 0x100, 100 * MIB, 1);
        be.close_handle(f).unwrap();
        assert_eq!(held(&be), 0, "freed with the file");
        // A session reset.
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let f = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Ctl));
        let c = client(&mut be, f);
        vidmem(&mut be, f, c, 0x100, 100 * MIB, 1);
        be.release_all();
        assert_eq!(held(&be), 0, "freed with the session");
    }

    fn get_fb_info(be: &mut NvidiaBackend, f: u32, c: u32, list: &[(u32, u32)]) -> Vec<u32> {
        let l = layout();
        let mut p = vec![0u8; l.fb_get_info_v2_sizeof];
        put_u32(
            &mut p,
            l.fb_get_info_v2_fb_info_list_size,
            list.len() as u32,
        );
        for (i, (index, data)) in list.iter().enumerate() {
            let e = l.fb_get_info_v2_fb_info_list + i * l.fb_info_sizeof;
            put_u32(&mut p, e + l.fb_info_index, *index);
            put_u32(&mut p, e + l.fb_info_data, *data);
        }
        let mut o = vec![0u8; NVOS54_SIZE];
        put_u32(&mut o, 0, c);
        put_u32(&mut o, 4, SUBDEVICE);
        put_u32(&mut o, 8, l.nv2080_ctrl_cmd_fb_get_info_v2);
        put_u32(&mut o, 24, p.len() as u32);
        let r = call(be, f, CONTROL, &o, &p, 1);
        assert_eq!(u32_at(&r, BODY + NVOS54_STATUS), Some(0));
        (0..list.len())
            .map(|i| {
                let e = BODY + NVOS54_SIZE + l.fb_get_info_v2_fb_info_list + i * l.fb_info_sizeof;
                u32_at(&r, e + l.fb_info_data).unwrap()
            })
            .collect()
    }

    /// What nvidia-smi -q -d MEMORY asks, as the RTX 5090 answers: with a
    /// limit, Total is the limit and Used what the VM holds; without one,
    /// RM's answer reaches the guest unchanged.
    #[test]
    fn nvidia_smi_sees_the_limit_and_without_one_the_host() {
        let l = layout();
        let list = [
            (l.nv2080_ctrl_fb_info_index_total_ram_size, 33_389_568),
            (l.nv2080_ctrl_fb_info_index_heap_size, 32_847_104),
            (l.nv2080_ctrl_fb_info_index_heap_free, 28_344_768),
        ];
        let (mut be, f) = vm(None);
        let c = client(&mut be, f);
        assert_eq!(
            get_fb_info(&mut be, f, c, &list),
            vec![33_389_568, 32_847_104, 28_344_768]
        );
        let (mut be, f) = vm(Some(4096));
        let c = client(&mut be, f);
        vidmem(&mut be, f, c, 0x100, 1000 * MIB, 1);
        let got = get_fb_info(&mut be, f, c, &list);
        assert_eq!(got, vec![4096 << 10, 4096 << 10, 3096 << 10]);
        // nvidia-smi: Total, Reserved, Used, Free in MiB.
        let (total, heap, free) = (got[0] >> 10, got[1] >> 10, got[2] >> 10);
        assert_eq!(
            (total, total - heap, heap - free, free),
            (4096, 0, 1000, 3096)
        );
    }

    /// VID_HEAP_CONTROL, as the Vulkan driver allocates: counted and
    /// refused the same way.
    #[test]
    fn vid_heap_allocations_are_held_to_the_limit() {
        let l = layout();
        let (mut be, f) = vm(Some(1024));
        let c = client(&mut be, f);
        let heap = |size: u64, h: u32| {
            let mut b = vec![0u8; l.nvos32_sizeof];
            put_u32(&mut b, l.nvos32_h_root, c);
            put_u32(&mut b, l.nvos32_h_object_parent, DEVICE);
            put_u32(&mut b, l.nvos32_function, l.nvos32_function_alloc_size);
            put_u32(&mut b, l.nvos32_alloc_size_h_memory, h);
            put_u64(&mut b, l.nvos32_alloc_size_size, size);
            b
        };
        let r = call(&mut be, f, HEAP, &heap(64 * MIB, 0x200), &[], 1);
        assert_eq!(u32_at(&r, BODY + l.nvos32_status), Some(0));
        assert_eq!(held(&be), 64 * MIB);
        let r = call(&mut be, f, HEAP, &heap(1024 * MIB, 0x201), &[], 1);
        assert_eq!(u32_at(&r, BODY + l.nvos32_status), Some(0x51));
        assert_eq!(held(&be), 64 * MIB);
    }
}
