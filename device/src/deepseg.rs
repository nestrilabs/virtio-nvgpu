// SPDX-License-Identifier: Apache-2.0
//! Deep segments: several pointers of one parameter block, each sent with
//! the bytes it addresses.
//!
//! The single deep block carries what one pointer inside an RM control's
//! parameters addresses. Some calls hold more than one, and RM follows every
//! one: NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST copies `numChannels` handles in
//! through one list and a channel list both ways through another, and
//! cuCtxCreate asks for it; with one of the two zeroed RM answers
//! NV_ERR_INVALID_ARGUMENT. NV_ESC_RM_IDLE_CHANNELS names three handle
//! arrays in its top-level block. A segmented deep block
//! (`protocol::messages::DEEP_SEGMENTED`) carries one segment per pointer.
//!
//! The guest says which pointer each segment is for and how long it is, and
//! is believed about neither. A segment must name a pointer RM follows in
//! that block (`abi::rmctrl::DeepControl`, measured from RM's
//! embeddedParamCopyIn and RmDeprecatedIdleChannels), a pointer the caller
//! set, at most once; and its length must be exactly what RM will copy
//! through it, computed here from the counts in the very block RM is about to
//! be handed -- the backend's own copy, which the guest cannot change after
//! this reads it. So RM copies neither more nor less than the buffer it is
//! given, and the buffers are guarded (`guarded.rs`) all the same. Anything
//! else refuses the whole call, before anything reaches the host.

#![forbid(unsafe_code)]

use abi::rmctrl::DeepPtr;
use protocol::messages::{DEEP_SEGS_MAX, DEEP_SEGS_MAX_BYTES, DeepSeg, DeepSegHdr};

use crate::sys::block::{Arena, BufId, Restore, SlotKind};
use crate::sys::pod;

/// A refusal: the errno the guest's ioctl returns.
pub type Errno = i32;

const HDR: usize = size_of::<DeepSegHdr>();
const ENTRY: usize = size_of::<DeepSeg>();

struct Seg {
    /// Where the pointer sits in the block.
    ptr: usize,
    /// The block of the call's arena it addresses.
    buf: BufId,
    len: usize,
}

/// The blocks one call's segments were given, in the call's arena: each is
/// what a pointer in the block now addresses.
pub(crate) struct Segments {
    table: Vec<u8>,
    segs: Vec<Seg>,
}

impl Segments {
    /// Check the guest's segmented deep block, `deep`, against `rules` --
    /// the pointers RM follows in `block` (a block of `a`, the bytes the host
    /// will read) and how much it copies through each -- and give each
    /// pointer it names a block of `a` holding the guest's bytes. On `Err`
    /// nothing in `a` has changed.
    pub(crate) fn relocate(
        what: &str,
        rules: &[DeepPtr],
        a: &mut Arena,
        block: BufId,
        deep: &[u8],
    ) -> Result<Self, Errno> {
        let refuse = |why: String| {
            log::warn!("{what}: deep segments refused: {why}");
            Err(libc::EINVAL)
        };
        let Some(DeepSegHdr { count, reserved }) = pod::read(deep, 0) else {
            return refuse(format!("{} bytes, no header", deep.len()));
        };
        if count == 0 || count > DEEP_SEGS_MAX || reserved != 0 {
            return refuse(format!(
                "a header of {count} segments, reserved {reserved:#x}"
            ));
        }
        let table_end = HDR + count as usize * ENTRY;
        if deep.len() < table_end {
            return refuse(format!("{count} segments in {} bytes", deep.len()));
        }
        let bytes = a.bytes(block);
        let mut plan: Vec<(usize, usize)> = Vec::with_capacity(count as usize);
        let mut total = 0usize;
        for i in 0..count as usize {
            let Some(DeepSeg { ptr_offset, len }) = pod::read(deep, HDR + i * ENTRY) else {
                return refuse(format!("segment {i} past the table"));
            };
            let (ptr, len) = (ptr_offset as usize, len as usize);
            let Some(rule) = rules.iter().find(|r| r.ptr == ptr) else {
                return refuse(format!("no pointer RM follows at {ptr}"));
            };
            if plan.iter().any(|&(p, _)| p == ptr) {
                return refuse(format!("the pointer at {ptr} twice"));
            }
            let Some(set) = bytes.get(ptr..ptr + 8) else {
                return refuse(format!(
                    "the pointer at {ptr} is past the {}-byte block",
                    bytes.len()
                ));
            };
            if set.iter().all(|&b| b == 0) {
                return refuse(format!("the pointer at {ptr} is null"));
            }
            // RM's own size, from the block it will be handed.
            match rule.size(bytes) {
                Some(want) if want as usize == len && len > 0 => {}
                want => {
                    return refuse(format!(
                        "{len} bytes for the pointer at {ptr}, where RM copies {want:?}"
                    ));
                }
            }
            total += len;
            if total > DEEP_SEGS_MAX_BYTES as usize {
                return refuse(format!("over {DEEP_SEGS_MAX_BYTES} bytes in all"));
            }
            plan.push((ptr, len));
        }
        if deep.len() != table_end + total {
            return refuse(format!(
                "a table of {total} bytes and {} bytes after it",
                deep.len() - table_end
            ));
        }

        // Only now, when nothing about the guest's bytes can fail.
        let mut segs = Vec::with_capacity(plan.len());
        let mut at = table_end;
        for (ptr, len) in plan {
            let buf = a.block(&deep[at..at + len], len)?;
            at += len;
            a.slot(block, ptr, 8, SlotKind::Ptr, Restore::Yes)?;
            a.point(block, ptr, buf)?;
            segs.push(Seg { ptr, buf, len });
        }
        Ok(Self {
            table: deep[..table_end].to_vec(),
            segs,
        })
    }

    /// The offsets of the pointers relocated.
    pub(crate) fn offsets(&self) -> Vec<usize> {
        self.segs.iter().map(|s| s.ptr).collect()
    }

    /// The reply's deep block: laid out as the request's, with each
    /// segment's bytes as the host left them in `a`.
    pub(crate) fn reply(&self, a: &Arena) -> Vec<u8> {
        let mut out = self.table.clone();
        for s in &self.segs {
            out.extend_from_slice(&a.bytes(s.buf)[..s.len]);
        }
        out
    }
}

/// A segmented deep block for tests: `(pointer offset, bytes)` in order.
#[cfg(test)]
pub(crate) fn build(segs: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(segs.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (off, b) in segs {
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    }
    for (_, b) in segs {
        out.extend_from_slice(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: u64 = 0x4141_4141_4000;

    fn channellist(n: u32) -> Vec<u8> {
        let mut p = vec![0u8; 24];
        p[..4].copy_from_slice(&n.to_le_bytes());
        p[8..16].copy_from_slice(&GUEST.to_le_bytes());
        p[16..24].copy_from_slice(&(GUEST + 0x100).to_le_bytes());
        p
    }

    fn rules() -> &'static [DeepPtr] {
        abi::rmctrl::deep_control(0x0080_170d).unwrap().ptrs
    }

    /// `relocate` on an arena block holding `p`: the arena and the block.
    fn relocate(p: &[u8], deep: &[u8]) -> (Result<Segments, Errno>, Arena, BufId) {
        let mut a = Arena::new();
        let b = a.block(p, p.len()).unwrap();
        let r = Segments::relocate("t", rules(), &mut a, b, deep);
        (r, a, b)
    }

    #[test]
    fn each_pointer_gets_its_own_buffer_with_the_guests_bytes() {
        let p = channellist(2);
        let deep = build(&[(8, &[1, 0, 0, 0, 2, 0, 0, 0]), (16, &[9; 8])]);
        let (s, a, b) = relocate(&p, &deep);
        let s = s.unwrap();
        let host = a.bytes(b);
        let x = u64::from_le_bytes(host[8..16].try_into().unwrap());
        let y = u64::from_le_bytes(host[16..24].try_into().unwrap());
        assert!(x != GUEST && y != GUEST + 0x100 && x != y && x != 0 && y != 0);
        // Each segment's block holds the guest's bytes, and the reply
        // carries them back laid out as sent.
        assert_eq!(s.reply(&a), deep);
        // The caller's own pointers come back, never ours.
        assert_eq!(a.reply(b), channellist(2));
    }

    #[test]
    fn a_length_that_is_not_rms_is_refused_and_leaves_the_block_alone() {
        for deep in [
            build(&[(8, &[0; 4]), (16, &[0; 8])]),  // short
            build(&[(8, &[0; 8]), (16, &[0; 12])]), // long
            build(&[(8, &[0; 8]), (8, &[0; 8])]),   // the same pointer twice
            build(&[(0, &[0; 8])]),                 // not a pointer
            build(&[(24, &[0; 8])]),                // past the block
            build(&[]),                             // no segments
            build(&[
                (8, &[0; 8]),
                (16, &[0; 8]),
                (8, &[0; 8]),
                (16, &[0; 8]),
                (8, &[0; 8]),
            ]),
        ] {
            let p = channellist(2);
            let (r, a, b) = relocate(&p, &deep);
            assert_eq!(r.err(), Some(libc::EINVAL), "{deep:?}");
            assert_eq!(a.bytes(b), &channellist(2)[..]);
        }
        // Bytes that do not add up to the table.
        let mut deep = build(&[(8, &[0; 8])]);
        deep.push(0);
        assert!(relocate(&channellist(2), &deep).0.is_err());
        // A segment for a pointer the caller left null.
        let mut p = channellist(2);
        p[16..24].fill(0);
        let deep = build(&[(16, &[0; 8])]);
        assert!(relocate(&p, &deep).0.is_err());
        // A count RM would refuse (0) sizes nothing to send.
        assert!(relocate(&channellist(0), &build(&[(8, &[])])).0.is_err());
    }

    #[test]
    fn the_total_is_bounded() {
        let n = DEEP_SEGS_MAX_BYTES / 4 / 2 + 1;
        let p = channellist(n);
        let a = vec![0u8; n as usize * 4];
        let deep = build(&[(8, &a), (16, &a)]);
        assert_eq!(relocate(&p, &deep).0.err(), Some(libc::EINVAL));
    }
}
