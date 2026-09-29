// SPDX-License-Identifier: Apache-2.0
//! Little-endian words of a byte block, read and written by offset.
//!
//! Every block these read is a guest's, or the host's answer to one, and
//! every offset is a table's or a guest's: a read past the end is `None`
//! and a write past it is refused, never a panic. The release profile
//! aborts on a panic (Cargo.toml), and an abort ends the VM's backend, so a
//! short block has to be an answer the caller chooses.

#![forbid(unsafe_code)]

/// The `N` bytes at `at`, if the block holds them.
fn bytes<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at.checked_add(N)?)?.try_into().ok()
}

pub fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    bytes(b, at).map(u32::from_le_bytes)
}

pub fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    bytes(b, at).map(i32::from_le_bytes)
}

pub fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    bytes(b, at).map(u64::from_le_bytes)
}

/// An unsigned word of `width` bytes (1 to 8), zero-extended.
pub fn uint_at(b: &[u8], at: usize, width: usize) -> Option<u64> {
    if !(1..=8).contains(&width) {
        return None;
    }
    let s = b.get(at..at.checked_add(width)?)?;
    let mut w = [0u8; 8];
    w[..width].copy_from_slice(s);
    Some(u64::from_le_bytes(w))
}

/// Write `v` at `at`; `None`, and nothing written, if the block is short.
pub fn put_u32(b: &mut [u8], at: usize, v: u32) -> Option<()> {
    b.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&v.to_le_bytes());
    Some(())
}

/// Write `v` at `at`; `None`, and nothing written, if the block is short.
pub fn put_u64(b: &mut [u8], at: usize, v: u64) -> Option<()> {
    b.get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&v.to_le_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_or_write_past_the_end_is_none_at_any_offset() {
        let mut b = [1u8, 0, 0, 0, 2, 0, 0, 0, 0xff];
        assert_eq!(u32_at(&b, 0), Some(1));
        assert_eq!(u64_at(&b, 0), Some(1 | 2 << 32));
        assert_eq!(i32_at(&b, 5), Some(-16_777_216));
        assert_eq!(u32_at(&b, 6), None);
        assert_eq!(u64_at(&b, 2), None);
        assert_eq!(u32_at(&b, usize::MAX - 1), None);
        assert_eq!(uint_at(&b, 4, 2), Some(2));
        assert_eq!(uint_at(&b, 8, 1), Some(0xff));
        assert_eq!(uint_at(&b, 0, 9), None);
        assert_eq!(uint_at(&b, 0, 0), None);
        assert_eq!(put_u32(&mut b, 6, 7), None);
        assert_eq!(b[6..], [0, 0, 0xff]);
        assert_eq!(put_u64(&mut b, usize::MAX, 7), None);
        assert_eq!(put_u32(&mut b, 4, 9), Some(()));
        assert_eq!(u32_at(&b, 4), Some(9));
    }
}
