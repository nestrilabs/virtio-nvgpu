// SPDX-License-Identifier: GPL-2.0-only
//! Wire constants (`driver/nvgpu_wire.h`), errno values, and little-endian
//! field access that cannot panic.

/// A negative errno, as the kernel returns it.
pub type Errno = i32;

/// `EPERM`.
pub const EPERM: Errno = 1;
/// `EINTR`.
pub const EINTR: Errno = 4;
/// `EIO`.
pub const EIO: Errno = 5;
/// `E2BIG`.
pub const E2BIG: Errno = 7;
/// `EBADF`.
pub const EBADF: Errno = 9;
/// `ENOMEM`.
pub const ENOMEM: Errno = 12;
/// `EFAULT`.
pub const EFAULT: Errno = 14;
/// `EINVAL`.
pub const EINVAL: Errno = 22;
/// `EMFILE`.
pub const EMFILE: Errno = 24;
/// `ENOTTY`.
pub const ENOTTY: Errno = 25;
/// `EPROTO`.
pub const EPROTO: Errno = 71;
/// `EOPNOTSUPP`.
pub const EOPNOTSUPP: Errno = 95;
/// `ETIMEDOUT`.
pub const ETIMEDOUT: Errno = 110;
/// `MAX_ERRNO`: a status in `[-MAX_ERRNO, -1]` is an errno.
pub const MAX_ERRNO: i32 = 4095;

/// `NVGPU_MSG_IOCTL`.
pub const MSG_IOCTL: u32 = 3;
/// `NVGPU_MSG_IOCTL2`.
pub const MSG_IOCTL2: u32 = 10;

/// `sizeof(struct nvgpu_msg_hdr)`.
pub const HDR_LEN: usize = 16;
/// `sizeof(struct nvgpu_ioctl_req)`.
pub const IOCTL_REQ_LEN: usize = 40;
/// `sizeof(struct nvgpu_ioctl_resp)`.
pub const IOCTL_RESP_LEN: usize = 28;
/// `sizeof(struct nvgpu_proc_id)`.
pub const PROC_ID_LEN: usize = 16;

/// `NVGPU_DEEP_SEGMENTED`.
pub const DEEP_SEGMENTED: u32 = 0xffff_ffff;
/// `NVGPU_DEEP_PAGE_LIST`.
pub const DEEP_PAGE_LIST: u32 = 0xffff_fffe;
/// `NVGPU_DEEP_SEGS_MAX`.
pub const DEEP_SEGS_MAX: usize = 4;
/// `NVGPU_DEEP_SEGS_MAX_BYTES`.
pub const DEEP_SEGS_MAX_BYTES: u32 = 1 << 20;
/// `NVGPU_IDLE_CHANNELS_MAX`.
pub const IDLE_CHANNELS_MAX: u32 = 4096;
/// `NVGPU_OSDESC_F_WRITE`.
pub const OSDESC_F_WRITE: u32 = 1;
/// `NVGPU_OSDESC_MAX_RUNS`.
pub const OSDESC_MAX_RUNS: u64 = 8192;
/// `NVGPU_OSDESC_MAX_PAGES`.
pub const OSDESC_MAX_PAGES: u64 = 1 << 20;

/// `NVGPU_I2_MAX_BUFS`.
pub const I2_MAX_BUFS: usize = 256;
/// `NVGPU_I2_MAX_RECS`.
pub const I2_MAX_RECS: usize = 256;
/// `NVGPU_I2_FD_CONSUME`.
pub const I2_FD_CONSUME: u32 = 1;
/// `sizeof(struct nvgpu_i2_req)`.
pub const I2_REQ_LEN: usize = 32;
/// `sizeof(struct nvgpu_i2_resp)`.
pub const I2_RESP_LEN: usize = 32;
/// `sizeof(struct nvgpu_i2_fd_in)`, `_gem_in`, `_dyn`, `_fd_out`.
pub const I2_REC_LEN: usize = 16;
/// `sizeof(struct nvgpu_i2_gem_out)`.
pub const I2_GEM_OUT_LEN: usize = 24;

/// `u32` at `off` of `b`, little-endian, if all four bytes are there.
pub fn le32(b: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = b.get(off..end)?;
    let mut v = [0u8; 4];
    copy(&mut v, s);
    Some(u32::from_le_bytes(v))
}

/// `u64` at `off` of `b`, little-endian, if all eight bytes are there.
pub fn le64(b: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let s = b.get(off..end)?;
    let mut v = [0u8; 8];
    copy(&mut v, s);
    Some(u64::from_le_bytes(v))
}

/// A little-endian unsigned field of 1, 2, 4 or 8 bytes at `off`; `None`
/// for any other width or a field past the end.
pub fn le_n(b: &[u8], off: usize, width: usize) -> Option<u64> {
    let end = off.checked_add(width)?;
    let s = b.get(off..end)?;
    let mut v = [0u8; 8];
    match width {
        1 | 2 | 4 | 8 => {
            copy(v.get_mut(..width)?, s);
            Some(u64::from_le_bytes(v))
        }
        _ => None,
    }
}

/// Write `v`'s low `width` bytes at `off`, little-endian; false (nothing
/// written) if they do not all fit.
pub fn put_le(b: &mut [u8], off: usize, width: usize, v: u64) -> bool {
    let Some(end) = off.checked_add(width) else {
        return false;
    };
    let bytes = v.to_le_bytes();
    match (b.get_mut(off..end), bytes.get(..width)) {
        (Some(d), Some(s)) => {
            copy(d, s);
            true
        }
        _ => false,
    }
}

/// `put_le(b, off, 4, v)`.
pub fn put32(b: &mut [u8], off: usize, v: u32) -> bool {
    put_le(b, off, 4, u64::from(v))
}

/// `put_le(b, off, 8, v)`.
pub fn put64(b: &mut [u8], off: usize, v: u64) -> bool {
    put_le(b, off, 8, v)
}

/// `dst[..n].copy_from_slice(&src[..n])`, `n` the shorter length: a copy
/// with no panic path (the lengths are equal wherever it is used).
pub fn copy(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s;
    }
}

/// `copy_from_slice` without its panic: [`copy`] as a method.
pub trait FillFrom {
    /// Copy `src` in; the lengths are equal wherever it is used.
    fn fill_from(&mut self, src: &[u8]);
}

impl FillFrom for [u8] {
    fn fill_from(&mut self, src: &[u8]) {
        copy(self, src);
    }
}

/// The sum of lengths that are each bounded far below `usize::MAX`;
/// saturates rather than wraps.
pub fn sum(parts: &[usize]) -> usize {
    parts.iter().fold(0usize, |a, &b| a.saturating_add(b))
}

/// `ALIGN(n, 8)` for a length that is at most `u32::MAX`, which every
/// length here is; saturates rather than wraps otherwise.
pub fn align8(n: u64) -> u64 {
    n.saturating_add(7) & !7
}

/// A `nvgpu_msg_hdr` + `nvgpu_ioctl_req` for `cmd` on backend `handle`.
#[allow(clippy::too_many_arguments)]
pub fn ioctl_req_header(
    handle: u32,
    cmd: u32,
    data_len: u32,
    nested_offset: u32,
    nested_len: u32,
    deep_ptr_offset: u32,
    deep_len: u32,
) -> [u8; IOCTL_REQ_LEN] {
    let mut h = [0u8; IOCTL_REQ_LEN];
    let fields = [
        MSG_IOCTL,
        handle,
        0,
        0,
        cmd,
        data_len,
        nested_offset,
        nested_len,
        deep_ptr_offset,
        deep_len,
    ];
    for (chunk, v) in h.chunks_exact_mut(4).zip(fields) {
        copy(chunk, &v.to_le_bytes());
    }
    h
}

/// What an `nvgpu_ioctl_resp` says, as far as `used` bytes of it go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoctlResp {
    /// `hdr.status`, as a signed value.
    pub status: i32,
    /// Whether the whole fixed part (28 bytes) is there; the three lengths
    /// below are zero if not.
    pub full: bool,
    /// `data_len`.
    pub data_len: u32,
    /// `nested_len`.
    pub nested_len: u32,
    /// `deep_len`.
    pub deep_len: u32,
}

impl IoctlResp {
    /// The header of a reply the device wrote `used` bytes of, or `None`
    /// when it wrote less than a header (the C paths' -EIO).
    pub fn parse(resp: &[u8], used: u32) -> Option<IoctlResp> {
        let used = usize::try_from(used).ok()?;
        if !has(used, 0, HDR_LEN) {
            return None;
        }
        let status = le32(resp, 8)? as i32;
        if !has(used, 0, IOCTL_RESP_LEN) {
            return Some(IoctlResp {
                status,
                ..IoctlResp::default()
            });
        }
        Some(IoctlResp {
            status,
            full: true,
            data_len: le32(resp, 16)?,
            nested_len: le32(resp, 20)?,
            deep_len: le32(resp, 24)?,
        })
    }
}

/// `nvgpu_resp_has()`: does a response of `used` bytes contain all of
/// `[off, off + len)`?
pub fn has(used: usize, off: usize, len: usize) -> bool {
    off <= used && len <= used.wrapping_sub(off)
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(le32(&b, 0), Some(0x0403_0201));
        assert_eq!(le32(&b, 6), None);
        assert_eq!(le32(&b, usize::MAX), None);
        assert_eq!(le64(&b, 1), Some(0x0908_0706_0504_0302));
        assert_eq!(le_n(&b, 8, 1), Some(9));
        assert_eq!(le_n(&b, 7, 2), Some(0x0908));
        assert_eq!(le_n(&b, 0, 3), None);
        assert_eq!(le_n(&b, 9, 1), None);
        let mut m = [0u8; 6];
        assert!(put32(&mut m, 2, 0xa1b2_c3d4));
        assert_eq!(m, [0, 0, 0xd4, 0xc3, 0xb2, 0xa1]);
        assert!(!put32(&mut m, 3, 1));
        assert!(!put64(&mut m, 0, 1));
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(16), 16);
        assert!(has(10, 10, 0));
        assert!(!has(10, 11, 0));
        assert!(!has(10, 4, 7));
    }

    #[test]
    fn ioctl_resp() {
        let mut r = [0u8; 28];
        r[8..12].copy_from_slice(&(-22i32).to_le_bytes());
        r[16] = 5;
        assert_eq!(IoctlResp::parse(&r, 15), None);
        let h = IoctlResp::parse(&r, 16).unwrap();
        assert_eq!((h.status, h.full, h.data_len), (-22, false, 0));
        let h = IoctlResp::parse(&r, 28).unwrap();
        assert_eq!((h.status, h.full, h.data_len), (-22, true, 5));
    }
}
