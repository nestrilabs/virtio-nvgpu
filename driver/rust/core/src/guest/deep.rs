// SPDX-License-Identifier: GPL-2.0-only
//! Deep segments: several pointers of one RM parameter block, each sent with
//! what it addresses (`NVGPU_DEEP_SEGMENTED`, `nvgpu_wire.h`).
//!
//! One deep block carries one pointer, and some calls hold more, every one of
//! which RM follows (FIFO_GET_CHANNELLIST's two lists, IDLE_CHANNELS' three
//! arrays). The table of such controls, and how much RM copies through each
//! pointer, is generated from RM's own sources with the backend's
//! (`gen/nvgpu_rm_deep.h`); the backend computes every size again from what
//! it is sent and refuses the call if one differs, so this side only has to
//! be right, not trusted. The plan is made from the one copy of the block
//! that is also what is sent, so the sizes the backend checks are computed
//! from the bytes they were planned from.

use super::wire::{copy, le64, le_n, Errno, DEEP_SEGS_MAX, DEEP_SEGS_MAX_BYTES, EFAULT};

/// `NVGPU_RM_DEEP_PTRS_MAX`.
pub const PTRS_MAX: usize = 4;
/// `NVGPU_RM_DEEP_COUNTS_MAX`.
pub const COUNTS_MAX: usize = 2;
/// `NVGPU_RM_DEEP_IN`: RM copies the buffer in.
pub const DEEP_IN: u8 = 1;
/// `NVGPU_RM_DEEP_OUT`: RM copies it back out.
pub const DEEP_OUT: u8 = 2;

/// `struct nvgpu_rm_deep_count`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    /// Offset in the control's parameters.
    pub offset: u16,
    /// Bytes, little-endian.
    pub width: u8,
}

/// `struct nvgpu_rm_deep_ptr`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ptr {
    /// Offset of the NvP64.
    pub ptr: u16,
    /// `DEEP_IN` / `DEEP_OUT`.
    pub flags: u8,
    /// Counts used.
    pub ncounts: u8,
    /// The counts.
    pub counts: [Count; COUNTS_MAX],
    /// Multiplier.
    pub scale: u32,
    /// Element size.
    pub elem: u32,
}

/// `struct nvgpu_rm_deep_control`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Control {
    /// The RM control (unused for IDLE_CHANNELS).
    pub cmd: u32,
    /// Pointers used.
    pub nptrs: u32,
    /// The pointers.
    pub ptrs: [Ptr; PTRS_MAX],
}

/// `nvgpu_rm_deep_size()`: how much RM copies through pointer `p` of block
/// `blk`, as RM computes it: the counts multiplied in NvU32, which wraps,
/// then by the element size, which RM checks (portSafeMulU32). `None` for a
/// count outside the block or an overflow. The backend's copy of this is
/// `DeepPtr::size` in gen/src/rmctrl/mod.rs.
pub fn size(p: &Ptr, blk: &[u8]) -> Option<u32> {
    let mut n = p.scale;
    for c in p.counts.iter().take(usize::from(p.ncounts)) {
        let v = match c.width {
            1 | 2 | 4 => le_n(blk, usize::from(c.offset), usize::from(c.width))? as u32,
            _ => return None,
        };
        n = n.wrapping_mul(v);
    }
    n.checked_mul(p.elem)
}

/// One planned segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seg {
    /// The caller's pointer.
    pub uptr: u64,
    /// Offset of the pointer in the block holding it.
    pub ptr: u32,
    /// Bytes RM copies through it.
    pub len: u32,
    /// `DEEP_IN` / `DEEP_OUT`.
    pub flags: u8,
}

/// `struct nvgpu_deep_plan`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Segments planned; 0 for none.
    pub n: u32,
    /// The deep block: header, table and segments.
    pub bytes: u32,
    /// The segments.
    pub seg: [Seg; DEEP_SEGS_MAX],
}

impl Plan {
    /// The planned segments.
    pub fn segs(&self) -> &[Seg] {
        self.seg.get(..self.n as usize).unwrap_or(&[])
    }
}

/// `nvgpu_deep_plan()`: one segment for each pointer of `ctl` the caller
/// set in `blk`, of RM's size for it. A pointer whose size is not known, or
/// is zero, gets none and is zeroed by the backend. Past
/// `NVGPU_DEEP_SEGS_MAX_BYTES` in all, nothing is planned.
pub fn plan(ctl: &Control, blk: &[u8]) -> Plan {
    let mut plan = Plan::default();
    let mut total: u32 = 0;
    let nptrs = usize::try_from(ctl.nptrs).unwrap_or(usize::MAX);
    for p in ctl.ptrs.iter().take(nptrs) {
        // A pointer this release's block does not have.
        let Some(uptr) = le64(blk, usize::from(p.ptr)) else {
            continue;
        };
        let Some(size) = size(p, blk) else { continue };
        if uptr == 0 || size == 0 {
            continue;
        }
        if size > DEEP_SEGS_MAX_BYTES.saturating_sub(total) || plan.n as usize == DEEP_SEGS_MAX {
            return Plan::default();
        }
        total = total.saturating_add(size);
        if let Some(s) = plan.seg.get_mut(plan.n as usize) {
            *s = Seg {
                uptr,
                ptr: u32::from(p.ptr),
                len: size,
                flags: p.flags,
            };
        }
        plan.n = plan.n.saturating_add(1);
    }
    if plan.n != 0 {
        // 8 + 4 * 8 + 1 MiB at most.
        plan.bytes = plan
            .n
            .saturating_mul(8)
            .saturating_add(8)
            .saturating_add(total);
    }
    plan
}

/// The caller's memory, as deep segments need it.
pub trait UserMem {
    /// Copy `dst.len()` bytes in from the caller's `src`; `-EFAULT` if they
    /// are not there.
    fn copy_from_user(&mut self, dst: &mut [u8], src: u64) -> Result<(), Errno>;
    /// Copy `src` out to the caller's `dst`; `-EFAULT` if it cannot.
    fn copy_to_user(&mut self, dst: u64, src: &[u8]) -> Result<(), Errno>;
}

/// `nvgpu_deep_fill()`: lay the planned deep block out in `dst`
/// (`plan.bytes` long). Every segment goes with the caller's bytes, a buffer
/// RM only writes too: the reply carries each back as RM left it, so one RM
/// did not get to write is copied back to the caller unchanged, as natively.
pub fn fill<M: UserMem + ?Sized>(plan: &Plan, mem: &mut M, dst: &mut [u8]) -> Result<(), Errno> {
    let mut hdr = [0u8; 8];
    copy(&mut hdr, &plan.n.to_le_bytes());
    let mut at = 0usize;
    let put = |dst: &mut [u8], at: &mut usize, b: &[u8]| -> Result<(), Errno> {
        let end = at.checked_add(b.len()).ok_or(-EFAULT)?;
        copy(dst.get_mut(*at..end).ok_or(-EFAULT)?, b);
        *at = end;
        Ok(())
    };
    put(dst, &mut at, &hdr)?;
    for s in plan.segs() {
        let mut e = [0u8; 8];
        copy(&mut e, &s.ptr.to_le_bytes());
        copy(e.get_mut(4..).unwrap_or(&mut []), &s.len.to_le_bytes());
        put(dst, &mut at, &e)?;
    }
    for s in plan.segs() {
        let end = at.checked_add(s.len as usize).ok_or(-EFAULT)?;
        mem.copy_from_user(dst.get_mut(at..end).ok_or(-EFAULT)?, s.uptr)?;
        at = end;
    }
    Ok(())
}

/// `nvgpu_deep_copy_back()`: copy back each segment RM writes, from `src`,
/// the reply's deep block. One not laid out as ours was is not read at all.
pub fn copy_back<M: UserMem + ?Sized>(plan: &Plan, mem: &mut M, src: &[u8]) -> Result<(), Errno> {
    if src.len() != plan.bytes as usize {
        return Ok(());
    }
    let mut at = 8usize.saturating_add((plan.n as usize).saturating_mul(8));
    for s in plan.segs() {
        let end = at.saturating_add(s.len as usize);
        if s.flags & DEEP_OUT != 0 {
            mem.copy_to_user(s.uptr, src.get(at..end).ok_or(-EFAULT)?)?;
        }
        at = end;
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// FIFO_GET_CHANNELLIST, as gen/nvgpu_rm_deep.h has it.
    fn channellist() -> Control {
        let c = |off| Count {
            offset: off,
            width: 4,
        };
        Control {
            cmd: 0x0080_170d,
            nptrs: 2,
            ptrs: [
                Ptr {
                    ptr: 8,
                    flags: DEEP_IN,
                    ncounts: 1,
                    counts: [c(0), Count::default()],
                    scale: 1,
                    elem: 4,
                },
                Ptr {
                    ptr: 16,
                    flags: DEEP_IN | DEEP_OUT,
                    ncounts: 1,
                    counts: [c(0), Count::default()],
                    scale: 1,
                    elem: 4,
                },
                Ptr::default(),
                Ptr::default(),
            ],
        }
    }

    fn blk(n: u32, p1: u64, p2: u64) -> Vec<u8> {
        let mut b = vec![0u8; 24];
        b[0..4].copy_from_slice(&n.to_le_bytes());
        b[8..16].copy_from_slice(&p1.to_le_bytes());
        b[16..24].copy_from_slice(&p2.to_le_bytes());
        b
    }

    #[test]
    fn plans_each_set_pointer() {
        let p = plan(&channellist(), &blk(3, 0x1000, 0x2000));
        assert_eq!(p.n, 2);
        assert_eq!(p.bytes, 8 + 16 + 24);
        assert_eq!(
            p.seg[0],
            Seg {
                uptr: 0x1000,
                ptr: 8,
                len: 12,
                flags: DEEP_IN
            }
        );
        assert_eq!(
            p.seg[1],
            Seg {
                uptr: 0x2000,
                ptr: 16,
                len: 12,
                flags: DEEP_IN | DEEP_OUT
            }
        );
        // A NULL pointer gets no segment; a zero count none at all.
        assert_eq!(plan(&channellist(), &blk(3, 0, 0x2000)).n, 1);
        assert_eq!(plan(&channellist(), &blk(0, 0x1000, 0x2000)).n, 0);
    }

    #[test]
    fn refuses_past_the_budget_and_short_blocks() {
        // 2^18 entries of 4 bytes: exactly 1 MiB for the first, nothing left.
        assert_eq!(plan(&channellist(), &blk(1 << 18, 0x1000, 0x2000)).n, 0);
        assert_eq!(plan(&channellist(), &blk(1 << 18, 0x1000, 0)).n, 1);
        // An element count that overflows u32 once multiplied by 4.
        assert_eq!(plan(&channellist(), &blk(0x4000_0000, 0x1000, 0)).n, 0);
        // The second pointer is past the end of a 16-byte block.
        assert_eq!(plan(&channellist(), &blk(1, 0x1000, 0x2000)[..16]).n, 1);
    }

    #[test]
    fn counts_multiply_in_u32() {
        // P2P_CAPS: counts at 128 twice, squared in NvU32.
        let c = Count {
            offset: 128,
            width: 4,
        };
        let p = Ptr {
            ptr: 160,
            flags: DEEP_OUT,
            ncounts: 2,
            counts: [c, c],
            scale: 1,
            elem: 4,
        };
        let mut b = vec![0u8; 176];
        b[128..132].copy_from_slice(&0x1_0000u32.to_le_bytes());
        // 0x10000^2 wraps to 0 in u32: size 0, no segment.
        assert_eq!(size(&p, &b), Some(0));
        b[128..132].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(size(&p, &b), Some(256));
        // A count width RM does not have.
        let bad = Ptr {
            counts: [
                Count {
                    offset: 0,
                    width: 3,
                },
                c,
            ],
            ..p
        };
        assert_eq!(size(&bad, &b), None);
    }
}
