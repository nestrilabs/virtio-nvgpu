// SPDX-License-Identifier: GPL-2.0-only
//! Memory the caller already has, registered with RM by its guest-physical
//! pages (`driver/nvgpu_osdesc.c`, whose pinning, unpinning and reaping stay
//! C): which of the three calls a block is, the range it asks RM to pin, the
//! page runs of what was pinned, the request that carries them, and the
//! reply's registration id.
//!
//! RM registers existing memory by CPU address (NV01_MEMORY_SYSTEM_OS_
//! DESCRIPTOR through ALLOC_MEMORY or RM_ALLOC, or VID_HEAP_CONTROL's
//! ALLOC_OS_DESCRIPTOR) and pins whatever that address maps in the calling
//! process -- the backend's. So the module pins the caller's range the way RM
//! would, and sends the guest-physical page list with the call
//! (`NVGPU_DEEP_PAGE_LIST`). Anything that is not exactly one of the three,
//! with the user virtual address descriptor type, goes the old way, and the
//! backend refuses it.

use super::rm;
use super::wire::{
    copy, has, ioctl_req_header, le32, le64, put32, sum, Errno, IoctlResp, DEEP_PAGE_LIST, EFAULT,
    EIO, ENOMEM, IOCTL_REQ_LEN, IOCTL_RESP_LEN, OSDESC_F_WRITE, OSDESC_MAX_PAGES, OSDESC_MAX_RUNS,
};

/// `NV_ESC_RM_ALLOC_MEMORY`.
pub const ESC_RM_ALLOC_MEMORY: u32 = 0x27;
/// `NV_ESC_RM_ALLOC`.
pub const ESC_RM_ALLOC: u32 = 0x2b;
/// `NV_ESC_RM_VID_HEAP_CONTROL`.
pub const ESC_RM_VID_HEAP_CONTROL: u32 = 0x4a;

const CLASS_OS_DESCRIPTOR: u32 = 0x71;
const OS32_ALLOC_OS_DESCRIPTOR: u32 = 27;
/// NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS: the only type registered here.
const DESCRIPTOR_VIRTUAL_ADDRESS: u32 = 0;
/// What RM answers when the pages would not pin.
const NV_ERR_INVALID_ADDRESS: u32 = 0x1e;

// Parameter layouts (nvos.h), as device/src/osdesc.rs reads them.
/// NVOS02, with its fd.
pub const OS02_SIZE: usize = 56;
const OS02_CLASS: usize = 12;
const OS02_FLAGS: usize = 16;
const OS02_MEMORY: usize = 24;
const OS02_LIMIT: usize = 32;
const OS02_STATUS: usize = 40;
const OS02_USER_READ_ONLY: u32 = 1 << 21;
/// NVOS32.
pub const OS32_SIZE: usize = 184;
const OS32_FUNCTION: usize = 8;
const OS32_STATUS: usize = 20;
const OS32_ATTR2: usize = 56;
const OS32_DESCRIPTOR: usize = 64;
const OS32_LIMIT: usize = 72;
const OS32_DESCRIPTOR_TYPE: usize = 80;
/// NVOS64.
pub const OS64_SIZE: usize = 48;
const OS64_CLASS: usize = 12;
const OS64_PARAMS: usize = 16;
const OS64_PARAMS_SIZE: usize = 32;
const OS64_STATUS: usize = 40;
/// NV_OS_DESC_MEMORY_ALLOCATION_PARAMS.
pub const OSD_SIZE: usize = 40;
const OSD_FLAGS: usize = 4;
const OSD_ATTR2: usize = 12;
const OSD_DESCRIPTOR: usize = 16;
const OSD_LIMIT: usize = 24;
const OSD_DESCRIPTOR_TYPE: usize = 32;
const ATTR2_USER_READ_ONLY: u32 = 1 << 22;
const OS32_FLAGS_USER_READ_ONLY: u32 = 0x0400_0000;

/// The guest's page size.
pub const PAGE_SIZE: u64 = 4096;

/// `sizeof(struct nvgpu_osdesc_hdr)` / `_run`.
const HDR_LEN: usize = 8;
const RUN_LEN: usize = 16;

/// What one of the three calls asks RM to pin (`struct nvgpu_osdesc_call`).
#[derive(Clone, Copy, Debug)]
pub struct Call {
    /// `_IOC_NR`.
    pub nr: u32,
    /// The caller's block, as copied (the largest of the three).
    pub outer: [u8; OS32_SIZE],
    /// Its length.
    pub outer_len: usize,
    /// RM_ALLOC's class parameters.
    pub nested: [u8; OSD_SIZE],
    /// Their length (0 for the other two calls).
    pub nested_len: usize,
    /// Where they came from.
    pub unested: u64,
    /// Where RM's status is in the block.
    pub status_at: usize,
    /// The first byte.
    pub va: u64,
    /// Bytes.
    pub size: u64,
    /// Pinned for writing (the call did not ask for read-only memory).
    pub write: bool,
}

impl Call {
    /// The first page's offset of `va`, and how many pages the range spans.
    pub fn pages(&self) -> (u64, u64) {
        let off = self.va & (PAGE_SIZE - 1);
        // describe() bounded this: at most OSDESC_MAX_PAGES.
        (off, off.saturating_add(self.size).div_ceil(PAGE_SIZE))
    }
}

/// The parameter block of the call `nr` (`_IOC_NR`), and where the word that
/// makes it ours is, and what it must say.
fn layout(nr: u32) -> Option<(usize, usize, u32)> {
    match nr {
        ESC_RM_ALLOC_MEMORY => Some((OS02_SIZE, OS02_CLASS, CLASS_OS_DESCRIPTOR)),
        ESC_RM_VID_HEAP_CONTROL => Some((OS32_SIZE, OS32_FUNCTION, OS32_ALLOC_OS_DESCRIPTOR)),
        ESC_RM_ALLOC => Some((OS64_SIZE, OS64_CLASS, CLASS_OS_DESCRIPTOR)),
        _ => None,
    }
}

/// Whether an RM escape of number `nr` and size `sz` can be one of the
/// three at all, before anything is read: the block that would be read
/// ([`describe`]) is exactly `sz` bytes.
pub fn candidate(nr: u32, sz: u32) -> bool {
    matches!(layout(nr), Some((len, _, _)) if len == sz as usize)
}

/// What describing a block found. (One lives on the stack for one call:
/// its size is the copied block's, which is the point.)
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Described {
    /// Not one of the three, well-formed: it goes the usual way.
    NotOurs,
    /// Ours, to register.
    Ours(Call),
    /// Ours, and failed with this before anything was sent.
    Failed(Errno),
}

/// `nvgpu_osdesc_describe()`: read the call from `outer`, the caller's
/// block of `_IOC_NR` `nr`, already copied in whole. RM_ALLOC's class
/// parameters are copied in here, once, through `mem`.
pub fn describe<M: super::deep::UserMem + ?Sized>(mem: &mut M, nr: u32, outer: &[u8]) -> Described {
    let Some((outer_len, at, want)) = layout(nr) else {
        return Described::NotOurs;
    };
    if outer.len() != outer_len || le32(outer, at) != Some(want) {
        return Described::NotOurs;
    }
    let mut c = Call {
        nr,
        outer: [0; OS32_SIZE],
        outer_len,
        nested: [0; OSD_SIZE],
        nested_len: 0,
        unested: 0,
        status_at: 0,
        va: 0,
        size: 0,
        write: false,
    };
    if let Some(d) = c.outer.get_mut(..outer_len) {
        copy(d, outer);
    }
    let o = outer;
    let w = |b: &[u8], off| le32(b, off).unwrap_or(0);
    let q = |b: &[u8], off| le64(b, off).unwrap_or(0);
    let limit;
    match nr {
        ESC_RM_ALLOC_MEMORY => {
            c.va = q(o, OS02_MEMORY);
            limit = q(o, OS02_LIMIT);
            // ALLOC_USER_READ_ONLY makes RM pin read-only (escape.c:260).
            c.write = w(o, OS02_FLAGS) & OS02_USER_READ_ONLY == 0;
            c.status_at = OS02_STATUS;
        }
        ESC_RM_VID_HEAP_CONTROL => {
            if w(o, OS32_DESCRIPTOR_TYPE) != DESCRIPTOR_VIRTUAL_ADDRESS {
                return Described::NotOurs;
            }
            c.va = q(o, OS32_DESCRIPTOR);
            limit = q(o, OS32_LIMIT);
            c.write = w(o, OS32_ATTR2) & ATTR2_USER_READ_ONLY == 0;
            c.status_at = OS32_STATUS;
        }
        _ => {
            let psize = w(o, OS64_PARAMS_SIZE);
            c.unested = q(o, OS64_PARAMS);
            if c.unested == 0 || (psize != 0 && psize as usize != OSD_SIZE) {
                return Described::NotOurs;
            }
            if mem.copy_from_user(&mut c.nested, c.unested).is_err() {
                return Described::Failed(-EFAULT);
            }
            let n = &c.nested;
            if w(n, OSD_DESCRIPTOR_TYPE) != DESCRIPTOR_VIRTUAL_ADDRESS {
                return Described::NotOurs;
            }
            c.nested_len = OSD_SIZE;
            c.va = q(n, OSD_DESCRIPTOR);
            limit = q(n, OSD_LIMIT);
            // osdescConstruct takes either as read-only (os_desc_mem.c:75).
            c.write = w(n, OSD_ATTR2) & ATTR2_USER_READ_ONLY == 0
                && w(n, OSD_FLAGS) & OS32_FLAGS_USER_READ_ONLY == 0;
            c.status_at = OS64_STATUS;
        }
    }
    let Some(size) = limit.checked_add(1) else {
        return Described::NotOurs;
    };
    if c.va.checked_add(size).is_none() {
        return Described::NotOurs;
    }
    c.size = size;
    // The pages the range spans, counted without rounding up past 2^64 (a
    // size within a page of it must be past the limit, not zero pages).
    let off = c.va & (PAGE_SIZE - 1);
    if off.saturating_add(size) > OSDESC_MAX_PAGES * PAGE_SIZE {
        return Described::NotOurs;
    }
    Described::Ours(c)
}

/// Runs of guest-physically contiguous pages, in order: `(gpa, pages)`.
/// `phys(i)` is page `i`'s guest-physical address. Calls `out` for each
/// run and answers how many there are, or `None` past
/// `NVGPU_OSDESC_MAX_RUNS` (`nvgpu_osdesc_runs()`, which counts them all
/// first; so does this, `out` being told the count is too high only after).
pub fn runs(
    npages: u64,
    phys: &mut dyn FnMut(u64) -> u64,
    out: &mut dyn FnMut(u64, u64),
) -> Option<u64> {
    let mut n: u64 = 0;
    let (mut gpa, mut len) = (0u64, 0u64);
    for i in 0..npages {
        let pa = phys(i);
        if len != 0
            && Some(pa) == gpa.checked_add(len.saturating_mul(PAGE_SIZE))
            && len < u64::from(u32::MAX)
        {
            len = len.saturating_add(1);
            continue;
        }
        if len != 0 {
            out(gpa, len);
            n = n.saturating_add(1);
        }
        gpa = pa;
        len = 1;
    }
    out(gpa, len);
    n = n.saturating_add(1);
    (n <= OSDESC_MAX_RUNS).then_some(n)
}

/// Why pinning failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// No memory for the page array (-ENOMEM, nothing pinned).
    NoMemory,
    /// The pages would not pin; RM would have failed the same way.
    NotPinned,
}

/// The pinning `nvgpu_osdesc.c` keeps in C.
pub trait Env: rm::Env {
    /// The pinned pages of one registration.
    type Pin;
    /// `nvgpu_osdesc_ok()`: v2 with `NVGPU_BCAP_OS_DESC`.
    fn os_desc(&self) -> bool;
    /// `nvgpu_osdesc_reap()`: unpin what RM has let go of.
    fn reap(&mut self);
    /// Pin `npages` pages from the page-aligned `start`, all of them.
    fn pin(&mut self, start: u64, npages: u64, write: bool) -> Result<Self::Pin, PinError>;
    /// Page `i`'s guest-physical address.
    fn page_phys(&self, pin: &Self::Pin, i: u64) -> u64;
    /// Keep the pages pinned under registration `id` (non-zero) until a
    /// reap names it.
    fn keep(&mut self, id: u64, pin: Self::Pin);
    /// `nvgpu_osdesc_send()`: send the registration with its pins riding
    /// along. The pins come back with the answer, except when the request
    /// was abandoned ([`i2::abandons`](super::i2::abandons)): then they are
    /// the transport's -- unpinned at once if the request never reached the
    /// ring, else kept until its late reply says what RM registered.
    fn send_pinned(
        &mut self,
        req: &[u8],
        resp: &mut [u8],
        pin: Self::Pin,
    ) -> (Result<u32, Errno>, Option<Self::Pin>);
    /// Unpin them now.
    fn unpin(&mut self, pin: Self::Pin);
}

/// `nvgpu_osdesc_register()`: pin, send the page list with the call, and
/// keep the pins under the id the reply names.
pub fn register<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, c: &Call) -> i32 {
    let (off, npages) = c.pages();
    let params = sum(&[c.outer_len, c.nested_len]);

    // What RM has let go of first, so its budget is back.
    env.reap();
    let pin = match env.pin(c.va.wrapping_sub(off), npages, c.write) {
        Ok(p) => p,
        Err(PinError::NoMemory) => return -ENOMEM,
        Err(PinError::NotPinned) => {
            // RM would have failed to pin them too: its status, in a call
            // that succeeded, as natively.
            return match env.copy_to_user(
                uarg.wrapping_add(c.status_at as u64),
                &NV_ERR_INVALID_ADDRESS.to_le_bytes(),
            ) {
                Ok(()) => 0,
                Err(_) => -EFAULT,
            };
        }
    };

    let nruns = match runs(npages, &mut |i| env.page_phys(&pin, i), &mut |_, _| {}) {
        Some(n) => n as usize,
        None => {
            env.unpin(pin);
            return -ENOMEM;
        }
    };
    let deep_len = sum(&[HDR_LEN, nruns.saturating_mul(RUN_LEN)]);
    let req_len = sum(&[IOCTL_REQ_LEN, params, deep_len]);
    let resp_len = sum(&[IOCTL_RESP_LEN, params, 8]);
    let (Some(mut req), Some(mut resp)) = (env.alloc(req_len), env.alloc(resp_len)) else {
        env.unpin(pin);
        return -ENOMEM;
    };
    {
        let r = req.as_mut();
        let h = ioctl_req_header(
            env.handle(),
            cmd,
            c.outer_len as u32,
            if c.nested_len != 0 {
                c.outer_len as u32
            } else {
                0
            },
            c.nested_len as u32,
            DEEP_PAGE_LIST,
            deep_len as u32,
        );
        let mut at = 0usize;
        let mut put = |b: &[u8]| {
            if let Some(d) = r.get_mut(at..at.saturating_add(b.len())) {
                copy(d, b);
            }
            at = at.saturating_add(b.len());
        };
        put(&h);
        put(c.outer.get(..c.outer_len).unwrap_or(&[]));
        put(c.nested.get(..c.nested_len).unwrap_or(&[]));
        let mut hdr = [0u8; HDR_LEN];
        put32(&mut hdr, 0, nruns as u32);
        put32(&mut hdr, 4, if c.write { OSDESC_F_WRITE } else { 0 });
        put(&hdr);
        runs(npages, &mut |i| env.page_phys(&pin, i), &mut |gpa, len| {
            let mut run = [0u8; RUN_LEN];
            copy(&mut run, &gpa.to_le_bytes());
            put32(&mut run, 8, len as u32);
            put(&run);
        });
    }

    let (used, pin) = env.send_pinned(req.as_ref(), resp.as_mut(), pin);
    drop(req);
    let (used, pin) = match (used, pin) {
        (Ok(u), Some(p)) => (u, p),
        // Abandoned: the pins went with the request.
        (Err(e), None) => return e,
        (Err(e), Some(p)) => {
            env.unpin(p);
            return e;
        }
        (Ok(_), None) => return -EIO,
    };
    let Some(h) = IoctlResp::parse(resp.as_ref(), used) else {
        env.unpin(pin);
        return -EIO;
    };
    let ret = h.status;
    if ret < 0 {
        // Refused before RM saw it.
        env.unpin(pin);
        return ret;
    }
    let usedz = used as usize;
    let r = resp.as_ref();
    let data_len = h.data_len as usize;
    let (nested_len, deep_len) = if data_len != 0 {
        (h.nested_len as usize, h.deep_len as usize)
    } else {
        (0, 0)
    };
    let at = IOCTL_RESP_LEN
        .saturating_add(data_len)
        .saturating_add(nested_len);
    let id = if deep_len == 8 && has(usedz, at, 8) {
        le64(r, at).unwrap_or(0)
    } else {
        0
    };
    if id != 0 {
        // Registered: RM has the pages until a reap says otherwise.
        env.keep(id, pin);
    } else {
        env.unpin(pin);
    }

    // The caller's block back, its own address in it, and RM's status.
    if data_len != c.outer_len || nested_len != c.nested_len || !has(usedz, IOCTL_RESP_LEN, params)
    {
        return -EIO;
    }
    let back = r
        .get(IOCTL_RESP_LEN..sum(&[IOCTL_RESP_LEN, c.outer_len]))
        .unwrap_or(&[]);
    if env.copy_to_user(uarg, back).is_err() {
        return -EFAULT;
    }
    if c.nested_len != 0 {
        let nb = r
            .get(sum(&[IOCTL_RESP_LEN, c.outer_len])..sum(&[IOCTL_RESP_LEN, params]))
            .unwrap_or(&[]);
        if env.copy_to_user(c.unested, nb).is_err() {
            return -EFAULT;
        }
    }
    ret
}

/// `nvgpu_osdesc_ioctl()` on a block already copied in whole (`outer`,
/// `_IOC_SIZE` bytes, which [`candidate`] said is the call's size): `None`
/// if it is not one of the three, and goes the usual way with the same
/// bytes.
pub fn ioctl<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, outer: &[u8]) -> Option<i32> {
    if !env.os_desc() {
        return None;
    }
    match describe(env, cmd & 0xff, outer) {
        Described::NotOurs => None,
        Described::Failed(e) => Some(e),
        Described::Ours(c) => Some(register(env, cmd, uarg, &c)),
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use std::vec::Vec;

    struct NoMem;
    impl super::super::deep::UserMem for NoMem {
        fn copy_from_user(&mut self, _: &mut [u8], _: u64) -> Result<(), Errno> {
            Err(-EFAULT)
        }
        fn copy_to_user(&mut self, _: u64, _: &[u8]) -> Result<(), Errno> {
            Err(-EFAULT)
        }
    }

    fn os02(va: u64, limit: u64, flags: u32) -> [u8; OS02_SIZE] {
        let mut b = [0u8; OS02_SIZE];
        b[OS02_CLASS..OS02_CLASS + 4].copy_from_slice(&CLASS_OS_DESCRIPTOR.to_le_bytes());
        b[OS02_FLAGS..OS02_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        b[OS02_MEMORY..OS02_MEMORY + 8].copy_from_slice(&va.to_le_bytes());
        b[OS02_LIMIT..OS02_LIMIT + 8].copy_from_slice(&limit.to_le_bytes());
        b
    }

    #[test]
    fn describes_alloc_memory() {
        let Described::Ours(c) =
            describe(&mut NoMem, ESC_RM_ALLOC_MEMORY, &os02(0x1234, 0x2000, 0))
        else {
            panic!("not ours")
        };
        assert_eq!(
            (c.va, c.size, c.write, c.status_at),
            (0x1234, 0x2001, true, OS02_STATUS)
        );
        assert_eq!(c.pages(), (0x234, 3));
        let Described::Ours(c) = describe(
            &mut NoMem,
            ESC_RM_ALLOC_MEMORY,
            &os02(0, 0, OS02_USER_READ_ONLY),
        ) else {
            panic!("not ours")
        };
        assert!(!c.write);
        assert_eq!(c.pages(), (0, 1));
    }

    #[test]
    fn refuses_what_is_not_a_registration() {
        let mut b = os02(0x1000, 0xfff, 0);
        b[OS02_CLASS] = 0x70;
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC_MEMORY, &b),
            Described::NotOurs
        ));
        // Wrong size for the call.
        assert!(matches!(
            describe(
                &mut NoMem,
                ESC_RM_ALLOC_MEMORY,
                &os02(0x1000, 0xfff, 0)[..48]
            ),
            Described::NotOurs
        ));
        // A range that wraps, and one past the page budget.
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC_MEMORY, &os02(u64::MAX, 0, 0)),
            Described::NotOurs
        ));
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC_MEMORY, &os02(0, u64::MAX, 0)),
            Described::NotOurs
        ));
        assert!(matches!(
            describe(
                &mut NoMem,
                ESC_RM_ALLOC_MEMORY,
                &os02(0, OSDESC_MAX_PAGES * PAGE_SIZE, 0)
            ),
            Described::NotOurs
        ));
        assert!(matches!(
            describe(
                &mut NoMem,
                ESC_RM_ALLOC_MEMORY,
                &os02(0, OSDESC_MAX_PAGES * PAGE_SIZE - 1, 0)
            ),
            Described::Ours(_)
        ));
        // Within a page of 2^64: the page count must not round to zero.
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC_MEMORY, &os02(0, u64::MAX - 1, 0)),
            Described::NotOurs
        ));
        // RM_ALLOC's parameters are read through the caller's memory.
        let mut a = [0u8; OS64_SIZE];
        a[OS64_CLASS..OS64_CLASS + 4].copy_from_slice(&CLASS_OS_DESCRIPTOR.to_le_bytes());
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC, &a),
            Described::NotOurs
        ));
        a[OS64_PARAMS] = 1;
        assert!(
            matches!(describe(&mut NoMem, ESC_RM_ALLOC, &a), Described::Failed(e) if e == -EFAULT)
        );
        a[OS64_PARAMS_SIZE] = 41;
        assert!(matches!(
            describe(&mut NoMem, ESC_RM_ALLOC, &a),
            Described::NotOurs
        ));
    }

    #[test]
    fn coalesces_runs() {
        let pas = [0x10000u64, 0x11000, 0x12000, 0x20000, 0x5000, 0x6000];
        let mut got = Vec::new();
        let n = runs(pas.len() as u64, &mut |i| pas[i as usize], &mut |g, l| {
            got.push((g, l))
        });
        assert_eq!(n, Some(3));
        assert_eq!(got, [(0x10000, 3), (0x20000, 1), (0x5000, 2)]);
        // Every page apart: one run each, and past the limit, none.
        let n = runs(
            OSDESC_MAX_RUNS + 1,
            &mut |i| i * 2 * PAGE_SIZE,
            &mut |_, _| {},
        );
        assert_eq!(n, None);
        let n = runs(OSDESC_MAX_RUNS, &mut |i| i * 2 * PAGE_SIZE, &mut |_, _| {});
        assert_eq!(n, Some(OSDESC_MAX_RUNS));
    }
}
