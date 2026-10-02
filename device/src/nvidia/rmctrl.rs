//! Pointers inside an RM control's parameters.
//!
//! For most controls the parameter block is flat and the backend's job is to
//! hand RM a copy of it. For the ones in `abi::rmctrl` it is not: the block
//! holds an `NvP64`, and RM copies through it from the caller's address space
//! before and after the control runs. The caller is this process, so a guest's
//! value in that field is an address RM reads and writes in the backend's
//! memory -- the open item from v0.1.1.
//!
//! Nothing a guest puts in such a field is forwarded. For every pointer the
//! table describes:
//!
//! * the length comes from the backend's own reading of the count field, in
//!   the guest's block, and is bounded;
//! * the buffer is the backend's, zeroed before use;
//! * the guest's bytes are copied in, from a segment of the deep block, only
//!   where RM reads the buffer;
//! * the guest's own value goes back into the field before the reply, because
//!   userspace compares what it gets back with what it passed.
//!
//! A control the table cannot describe is refused. The refusal is RM's own
//! `NV_ERR_NOT_SUPPORTED`, written into the status word of the parameter
//! block, with the ioctl itself succeeding: a driver that asks for a feature
//! and gets an errno where it expects a status does not fall back, it hangs.

use abi::rmctrl::{MAX_EMBEDDED_BYTES, RmCtrlEntry};
use protocol::segments::Segments;

use crate::guarded::{GuardPool, Lease};

/// Status codes, from `nvstatuscodes.h`.
pub(super) const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
pub(super) const NV_ERR_NO_MEMORY: u32 = 0x51;

/// Every embedded pointer of one call together. Bounds what a single control
/// can ask the backend to hold, whatever its count fields say.
const MAX_TOTAL_BYTES: usize = 2 * MAX_EMBEDDED_BYTES;

/// One pointer of a control, resolved against the block the guest sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub ptr_offset: usize,
    /// What RM will copy, from the count field in the guest's own block.
    pub len: usize,
    pub copy_in: bool,
    pub copy_out: bool,
}

/// What a control's pointers need, or the status to refuse it with.
///
/// Pure: it reads the guest's block and the table and decides. Everything that
/// allocates or copies is in [`Embedded::install`], so the decisions can be
/// tested without a host driver.
pub(super) fn plan(
    entry: &RmCtrlEntry,
    params: &[u8],
    segs: Option<Segments<'_>>,
) -> Result<Vec<Slot>, u32> {
    if entry.refuse {
        return Err(NV_ERR_NOT_SUPPORTED);
    }
    // The offsets in the table are the release's struct. A block shorter than
    // that cannot be read with them, and reading it anyway is how a count
    // comes out of whatever follows the block in memory.
    if params.len() < entry.params_size {
        return Err(NV_ERR_NOT_SUPPORTED);
    }

    let mut slots = Vec::with_capacity(entry.ptrs.len());
    let mut total = 0usize;
    for p in entry.ptrs {
        // A null pointer is RM's own "nothing here": `RMAPI_PARAM_COPY_INIT`
        // sizes the copy to zero and the control reads the field's null-ness
        // in its own right. It stays null.
        let guest = u64::from_le_bytes(
            params[p.ptr_offset..p.ptr_offset + 8]
                .try_into()
                .expect("params_size covers the pointer field"),
        );
        if guest == 0 {
            continue;
        }

        let Some(len) = p.bytes(params) else {
            // The count is one a guest wrote. Over the bound, or in a field
            // the block is too short for, it is refused rather than clamped:
            // a clamped buffer and RM's own idea of the length are a write
            // past the end of ours.
            return Err(NV_ERR_NOT_SUPPORTED);
        };
        total = total.saturating_add(len);
        if total > MAX_TOTAL_BYTES {
            return Err(NV_ERR_NOT_SUPPORTED);
        }

        if p.copy_in {
            // More bytes than the length says means the guest and this table
            // disagree about the layout, which is not something to resolve by
            // taking the smaller number.
            if let Some(sent) = segs.and_then(|s| s.find(p.ptr_offset as u32))
                && sent.len() > len
            {
                return Err(NV_ERR_NOT_SUPPORTED);
            }
        }

        slots.push(Slot {
            ptr_offset: p.ptr_offset,
            len,
            copy_in: p.copy_in,
            copy_out: p.copy_out,
        });
    }
    Ok(slots)
}

/// Whether the reply the guest left room for holds what RM will write back.
///
/// The guest sizes its deep block from the same tables, so this is an
/// agreement check rather than a limit. It disagreeing means the two halves
/// read the call differently, and the call is refused rather than answered
/// with a piece of what RM produced.
pub(super) fn reply_fits(slots: &[Slot], have: usize) -> bool {
    let lens: Vec<usize> = slots
        .iter()
        .filter(|s| s.copy_out && s.len > 0)
        .map(|s| s.len)
        .collect();
    if lens.is_empty() {
        // Nothing goes back this way, so a guest that sent no deep block at
        // all -- every pointer null, as a sizing call makes them -- is fine.
        return true;
    }
    protocol::segments::encoded_len(&lens).is_some_and(|n| n <= have)
}

/// The buffers one call's pointers address, held until the ioctl returns.
pub(super) struct Embedded<'a> {
    bufs: Vec<(Slot, Lease<'a>, [u8; 8])>,
}

impl<'a> Embedded<'a> {
    /// Give every pointer in `params` a buffer of this process's.
    ///
    /// `params` is the backend's copy of the guest's block, and comes back
    /// with host addresses in the pointer fields. The guest's own values are
    /// kept here and go back in [`Embedded::restore`].
    pub fn install(
        slots: &[Slot],
        params: &mut [u8],
        segs: Option<Segments<'_>>,
        pool: &'a std::cell::RefCell<GuardPool>,
    ) -> Result<Self, u32> {
        let mut bufs = Vec::with_capacity(slots.len());
        for &slot in slots {
            // A pointer RM copies nothing through still has to be an address
            // of ours rather than the guest's: the control may read whether
            // the field is null, and a guest address in it is the thing this
            // whole path exists to prevent.
            let mut buf = GuardPool::lease(pool, slot.len.max(1)).ok_or(NV_ERR_NO_MEMORY)?;
            buf.as_mut_slice().fill(0);

            if slot.copy_in {
                if let Some(sent) = segs.and_then(|s| s.find(slot.ptr_offset as u32)) {
                    // `plan` refused anything longer than the slot.
                    buf.as_mut_slice()[..sent.len()].copy_from_slice(sent);
                } else if slot.len > 0 {
                    // The guest never sent the bytes: an older guest, or one
                    // that does not describe this pointer. RM reads zeroes
                    // rather than the guest's data, which is wrong for the
                    // caller and safe for this process.
                    log::debug!(
                        "RM_CONTROL: no segment for the pointer at {}, {} bytes read as zero",
                        slot.ptr_offset,
                        slot.len
                    );
                }
            }

            let saved: [u8; 8] = params[slot.ptr_offset..slot.ptr_offset + 8]
                .try_into()
                .expect("plan checked the block covers every pointer field");
            let host = buf.as_mut_ptr() as u64;
            params[slot.ptr_offset..slot.ptr_offset + 8].copy_from_slice(&host.to_le_bytes());
            bufs.push((slot, buf, saved));
        }
        Ok(Self { bufs })
    }

    /// Put the guest's own pointer values back, before the block is replied
    /// with. A host address there is both meaningless to the caller and this
    /// process's layout.
    pub fn restore(&self, params: &mut [u8]) {
        for (slot, _, saved) in &self.bufs {
            params[slot.ptr_offset..slot.ptr_offset + 8].copy_from_slice(saved);
        }
    }

    /// What goes back to the guest: one segment per pointer RM wrote.
    pub fn reply(&self) -> Vec<(u32, &[u8])> {
        self.bufs
            .iter()
            .filter(|(s, _, _)| s.copy_out && s.len > 0)
            .map(|(s, b, _)| (s.ptr_offset as u32, &b.as_slice()[..s.len]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::rmctrl::{Count, EmbeddedPtr};

    /// A block of `n` bytes with a count of `count` at offset 0 and a non-null
    /// pointer at `ptr_offset`.
    fn block(n: usize, count: u32, ptr_offset: usize) -> Vec<u8> {
        let mut p = vec![0u8; n];
        p[0..4].copy_from_slice(&count.to_le_bytes());
        p[ptr_offset..ptr_offset + 8].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
        p
    }

    fn entry(ptrs: &'static [EmbeddedPtr], params_size: usize) -> RmCtrlEntry {
        RmCtrlEntry {
            cmd: 0x0041_0110,
            params_size,
            ptrs,
            refuse: false,
        }
    }

    static LIST: &[EmbeddedPtr] = &[EmbeddedPtr {
        ptr_offset: 8,
        count: Count::Field {
            offset: 0,
            width: 4,
        },
        elem_size: 8,
        copy_in: true,
        copy_out: true,
    }];

    #[test]
    fn a_length_comes_from_the_count_the_guest_sent() {
        let e = entry(LIST, 16);
        let p = block(16, 3, 8);
        assert_eq!(
            plan(&e, &p, None).unwrap(),
            vec![Slot {
                ptr_offset: 8,
                len: 24,
                copy_in: true,
                copy_out: true
            }]
        );
    }

    #[test]
    fn a_refused_control_is_not_supported() {
        let e = RmCtrlEntry {
            cmd: 0x2080_1336,
            params_size: 0,
            ptrs: &[],
            refuse: true,
        };
        assert_eq!(plan(&e, &[0u8; 64], None), Err(NV_ERR_NOT_SUPPORTED));
    }

    #[test]
    fn a_block_shorter_than_the_release_struct_is_refused() {
        let e = entry(LIST, 16);
        assert_eq!(plan(&e, &[0u8; 12], None), Err(NV_ERR_NOT_SUPPORTED));
    }

    /// The count is a guest's number. This is the case the table exists for.
    #[test]
    fn an_enormous_count_is_refused_rather_than_allocated() {
        let e = entry(LIST, 16);
        let p = block(16, u32::MAX, 8);
        assert_eq!(plan(&e, &p, None), Err(NV_ERR_NOT_SUPPORTED));
    }

    #[test]
    fn a_null_pointer_stays_null_and_asks_for_nothing() {
        let e = entry(LIST, 16);
        let mut p = block(16, 3, 8);
        p[8..16].fill(0);
        assert_eq!(plan(&e, &p, None).unwrap(), vec![]);
    }

    #[test]
    fn a_segment_longer_than_the_count_is_refused() {
        let e = entry(LIST, 16);
        let p = block(16, 1, 8);
        let mut wire = [0u8; 64];
        let n = protocol::segments::encode(&mut wire, &[(8, &[7u8; 16])]).unwrap();
        let s = Segments::parse(&wire[..n]).unwrap();
        assert_eq!(plan(&e, &p, Some(s)), Err(NV_ERR_NOT_SUPPORTED));
    }

    /// Several pointers, each with its own count, is the shape that one
    /// `deep_ptr_offset` could not describe.
    #[test]
    fn two_pointers_each_get_their_own_length() {
        static TWO: &[EmbeddedPtr] = &[
            EmbeddedPtr {
                ptr_offset: 8,
                count: Count::Field {
                    offset: 0,
                    width: 4,
                },
                elem_size: 1,
                copy_in: true,
                copy_out: false,
            },
            EmbeddedPtr {
                ptr_offset: 24,
                count: Count::Fixed(4),
                elem_size: 4,
                copy_in: false,
                copy_out: true,
            },
        ];
        let e = entry(TWO, 32);
        let mut p = block(32, 10, 8);
        p[24..32].copy_from_slice(&0xcafe_u64.to_le_bytes());

        let slots = plan(&e, &p, None).unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].len, 10);
        assert!(slots[0].copy_in && !slots[0].copy_out);
        assert_eq!(slots[1].len, 16);
        assert!(!slots[1].copy_in && slots[1].copy_out);
    }

    #[test]
    fn the_buffers_are_this_process_and_the_guest_gets_its_own_value_back() {
        let pool = std::cell::RefCell::new(GuardPool::default());
        let e = entry(LIST, 16);
        let mut p = block(16, 2, 8);
        let guest = p[8..16].to_vec();

        let mut wire = [0u8; 64];
        let n = protocol::segments::encode(&mut wire, &[(8, &[1u8, 2, 3, 4])]).unwrap();
        let s = Segments::parse(&wire[..n]).unwrap();

        let slots = plan(&e, &p, Some(s)).unwrap();
        let emb = Embedded::install(&slots, &mut p, Some(s), &pool).unwrap();

        // The field now holds an address of ours, not the guest's.
        let host = u64::from_le_bytes(p[8..16].try_into().unwrap());
        assert_ne!(host, 0xdead_beef);
        assert_ne!(host, 0);

        // What RM would read: the guest's four bytes, then zeroes to the
        // length the count asked for.
        let out = emb.reply();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 8);
        assert_eq!(out[0].1, &[1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        emb.restore(&mut p);
        assert_eq!(&p[8..16], &guest[..]);
    }

    #[test]
    fn a_sizing_call_needs_no_room_and_a_short_reply_does_not_fit() {
        assert!(reply_fits(&[], 0));
        let out = Slot {
            ptr_offset: 8,
            len: 64,
            copy_in: false,
            copy_out: true,
        };
        assert!(!reply_fits(&[out], 8));
        assert!(reply_fits(&[out], 1024));
        // A pointer RM never writes back needs no room either.
        let inp = Slot {
            copy_out: false,
            copy_in: true,
            ..out
        };
        assert!(reply_fits(&[inp], 0));
    }

    #[test]
    fn a_pointer_rm_only_writes_is_not_filled_from_the_guest() {
        static OUT_ONLY: &[EmbeddedPtr] = &[EmbeddedPtr {
            ptr_offset: 8,
            count: Count::Fixed(2),
            elem_size: 4,
            copy_in: false,
            copy_out: true,
        }];
        let pool = std::cell::RefCell::new(GuardPool::default());
        let e = entry(OUT_ONLY, 16);
        let mut p = block(16, 0, 8);

        let mut wire = [0u8; 64];
        let n = protocol::segments::encode(&mut wire, &[(8, &[0xffu8; 8])]).unwrap();
        let s = Segments::parse(&wire[..n]).unwrap();

        let slots = plan(&e, &p, Some(s)).unwrap();
        let emb = Embedded::install(&slots, &mut p, Some(s), &pool).unwrap();
        assert_eq!(emb.reply()[0].1, &[0u8; 8]);
    }
}
