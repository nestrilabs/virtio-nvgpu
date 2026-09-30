// SPDX-License-Identifier: Apache-2.0
//! RM's EXPORT_TO_DMABUF_FD, with `--allow-dmabuf-export` (off by default):
//! video memory of the calling guest process's own RM client as a dma-buf,
//! which reaches the guest as a guest dma-buf and never leaves the VM.
//!
//! **What RM does.** nvidia.ko serves the escape on a GPU file
//! (nv-dmabuf.c, `nv_dma_buf_export`): with `fd` -1 it makes a dma-buf of
//! `handles[0..numObjects]` of client `hClient`, duplicating each into an
//! RM-internal client (osapi.c, `rm_dma_buf_dup_mem_handle`, through
//! `RMAPI_GPU_LOCK_INTERNAL`), and installs it in the *caller's* descriptor
//! table; with `fd` >= 0 it looks that number up in the caller's table and
//! appends to the dma-buf there. `RmDmabufVerifyMemHandle` checks that each
//! handle is Memory of this GPU, video memory on a discrete GPU, and that
//! offset and size are page-aligned and inside it. It does not check that
//! `hClient` is the caller's: any client on the host is looked up by
//! handle (`serverGetClientUnderLock`), so a caller naming another
//! process's client -- another VM's, the host compositor's -- gets that
//! client's video memory as a dma-buf. Natively that is RM's to answer for;
//! here every guest process is the backend, and handles are small numbers.
//!
//! **What the backend adds** (`check`, and `serve_dmabuf_export` in
//! nvidia/dmabuf.rs):
//!
//! - `hClient` is a client this VM allocated and the calling guest process
//!   made (rmshare.rs's `Ownership`, from BCAP_PROC_ID): the handles RM then
//!   resolves are that client's, and nothing else's. A guest that does not
//!   say which process calls gets no export.
//! - One call, the whole dma-buf: `fd` -1, `index` 0, `numObjects` equal to
//!   `totalObjects`, at most 128, and `totalSize` the sum of the sizes.
//!   The append form (`fd` >= 0) would have RM look a guest number up in
//!   the backend's descriptor table, and a dma-buf made in parts is one the
//!   host could not import until its last part came (nv_dma_buf_map fails
//!   while `num_objects != total_objects`); it is refused.
//! - `mappingType` DEFAULT and `bAllowMmap` 0: FORCE_PCIE bypasses the
//!   IOMMU for the importer (`skip_iommu`), and a CPU mapping of video
//!   memory through the dma-buf is not one the window places.
//! - A share of a per-VM count and byte budget ([`MAX_EXPORTS`],
//!   [`MAX_BYTES`]), per guest process ([`EXPORTS_SHARE`], [`BYTES_SHARE`]):
//!   an importer's mapping of the dma-buf takes BAR1 the host shares.
//! - What RM made is checked to be its dma-buf (the `exp_name` only
//!   nvidia.ko's exporter writes, [`RM_EXPORTER`]) of `totalSize` bytes,
//!   imported at once into a render file the calling process opened, and
//!   closed: the guest never holds a backend handle of the dma-buf itself,
//!   only the GEM object it became, which is a proxy's in the guest. Any
//!   failure after RM made it closes it, which undoes the export
//!   (nv_dma_buf_release).
//! - It never leaves: with the switch on, the export gate refuses every
//!   dma-buf of this exporter on every path out of the backend
//!   (exportgate.rs, `GuestOnly`), however it is reached.
//!
//! The records here are for the budgets: one per GEM object an export made,
//! charged to the process, given back when the guest closes the object or
//! the file.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::le;
use crate::nvos::{NV_ERR_INSUFFICIENT_RESOURCES, NV_ERR_INVALID_ARGUMENT, NV_ERR_NOT_SUPPORTED};
use crate::quota::{Ledger, Owner, Share};

/// The `exp_name` nvidia.ko gives its dma-bufs (nv-dmabuf.c,
/// `nv_dma_buf_create`: `exp_info.exp_name = "nv_dmabuf"`).
pub const RM_EXPORTER: &str = "nv_dmabuf";

/// `NV_DMABUF_EXPORT_MAX_HANDLES` (nv-ioctl.h, 535.129.03 through 610.57.04).
pub const MAX_HANDLES: u32 = 128;

/// Exports a VM may hold at once, and bytes of them.
pub const MAX_EXPORTS: u64 = 256;
pub const MAX_BYTES: u64 = 8 << 30;

/// Each guest process's part: a quarter of the count, half the bytes.
pub const EXPORTS_SHARE: Share = Share::quarter(MAX_EXPORTS, 4);
pub const BYTES_SHARE: Share = Share::half(MAX_BYTES);

const _: () = assert!(EXPORTS_SHARE.fits(MAX_EXPORTS) && BYTES_SHARE.fits(MAX_BYTES));

// nv_ioctl_export_to_dma_buf_fd_t (nv-ioctl.h): the fields every release
// has where they are.
pub const FD: usize = 0;
pub const H_CLIENT: usize = 4;
pub const TOTAL_OBJECTS: usize = 8;
pub const NUM_OBJECTS: usize = 12;
pub const INDEX: usize = 16;
pub const TOTAL_SIZE: usize = 24;

/// Where the rest of the block is, by its size: 2600 bytes in 535.129.03,
/// 2608 from 570.86.15 on, where `mappingType` and (580.65.06 on, in what
/// was padding) `bAllowMmap` follow `totalSize` and push the arrays 4 bytes
/// on. The ABI profile has already held the size to the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub len: usize,
    /// `mappingType`, with `bAllowMmap` after it; none before 570.
    pub mapping: Option<usize>,
    pub handles: usize,
    pub offsets: usize,
    pub sizes: usize,
    pub status: usize,
}

impl Layout {
    pub fn for_len(len: usize) -> Option<Self> {
        match len {
            2600 => Some(Self {
                len,
                mapping: None,
                handles: 32,
                offsets: 544,
                sizes: 1568,
                status: 2592,
            }),
            2608 => Some(Self {
                len,
                mapping: Some(32),
                handles: 36,
                offsets: 552,
                sizes: 1576,
                status: 2600,
            }),
            _ => None,
        }
    }
}

/// An export the checks let through: what the backend goes on with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
    pub client: u32,
    pub objects: u32,
    pub total_size: u64,
}

/// The checks on the block alone ([`Ask`]), or the RM status the call is
/// answered with, RM never asked: what RM itself answers where it would
/// refuse too (INVALID_ARGUMENT), NOT_SUPPORTED for what it would take and
/// this does not.
pub fn check(l: &Layout, b: &[u8]) -> Result<Ask, u32> {
    if b.len() < l.len {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let w = |at| le::u32_at(b, at).ok_or(NV_ERR_INVALID_ARGUMENT);
    let fd = le::i32_at(b, FD).ok_or(NV_ERR_INVALID_ARGUMENT)?;
    let client = w(H_CLIENT)?;
    let (total, num, index) = (w(TOTAL_OBJECTS)?, w(NUM_OBJECTS)?, w(INDEX)?);
    let total_size = le::u64_at(b, TOTAL_SIZE).ok_or(NV_ERR_INVALID_ARGUMENT)?;
    match fd {
        -1 => {}
        0.. => return Err(NV_ERR_NOT_SUPPORTED),
        _ => return Err(NV_ERR_INVALID_ARGUMENT),
    }
    // What nv_dma_buf_export refuses itself.
    if total_size == 0 || num == 0 || total == 0 || num > MAX_HANDLES || num > total {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    if num != total || index != 0 {
        return Err(NV_ERR_NOT_SUPPORTED);
    }
    if let Some(m) = l.mapping {
        let (mapping_type, allow_mmap) = (b[m], b[m + 1]);
        if mapping_type != 0 || allow_mmap != 0 {
            return Err(NV_ERR_NOT_SUPPORTED);
        }
    }
    let mut sum: u64 = 0;
    for i in 0..num as usize {
        let at = l.sizes.checked_add(i * 8).ok_or(NV_ERR_INVALID_ARGUMENT)?;
        let size = le::u64_at(b, at).ok_or(NV_ERR_INVALID_ARGUMENT)?;
        sum = sum.checked_add(size).ok_or(NV_ERR_INVALID_ARGUMENT)?;
    }
    if sum != total_size {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    Ok(Ask {
        client,
        objects: num,
        total_size,
    })
}

/// What the backend asks of the host after RM made the dma-buf, behind a
/// trait so the steps can be tested against a fake nvidia-drm.
pub trait ExportHost: Send + Sync {
    /// The dma-buf's exporter (`exp_name:` in its fdinfo), if it is one.
    fn exporter(&self, fd: BorrowedFd<'_>) -> Option<String>;
    /// Its size (`lseek` end).
    fn size(&self, fd: BorrowedFd<'_>) -> io::Result<u64>;
    /// PRIME_FD_TO_HANDLE.
    fn prime_import(&self, render: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32>;
    /// GEM_IDENTIFY_OBJECT.
    fn identify(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u32>;
    fn gem_close(&self, render: BorrowedFd<'_>, gem: u32);
}

/// The real host.
pub struct SysExportHost;

impl ExportHost for SysExportHost {
    fn exporter(&self, fd: BorrowedFd<'_>) -> Option<String> {
        crate::hostfd::dmabuf_exporter(fd.as_raw_fd())
    }
    fn size(&self, fd: BorrowedFd<'_>) -> io::Result<u64> {
        crate::hostfd::dmabuf_size(fd.as_raw_fd())
    }
    fn prime_import(&self, render: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32> {
        crate::hostfd::prime_import(render.as_raw_fd(), dmabuf.as_raw_fd())
    }
    fn identify(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u32> {
        crate::hostfd::gem_identify(render.as_raw_fd(), gem)
    }
    fn gem_close(&self, render: BorrowedFd<'_>, gem: u32) {
        let _ = crate::hostfd::gem_close(render.as_raw_fd(), gem);
    }
}

/// One GEM object an export made, in a guest render file.
#[derive(Clone, Copy, Debug)]
struct Rec {
    owner: Owner,
    bytes: u64,
}

/// The exports a VM holds, against its budgets.
#[derive(Debug, Default)]
pub struct RmExports {
    records: HashMap<(u32, u32), Rec>,
    count: Ledger,
    bytes: Ledger,
    bytes_in_use: u64,
}

impl RmExports {
    /// Whether `owner` may hold one more export of `bytes`, asked before RM
    /// is: the status to answer with if not.
    pub fn admits(&self, owner: Owner, bytes: u64) -> Result<(), u32> {
        let n = self.records.len() as u64;
        self.count
            .admits(&EXPORTS_SHARE, owner, 1, n, MAX_EXPORTS)
            .and_then(|()| {
                self.bytes
                    .admits(&BYTES_SHARE, owner, bytes, self.bytes_in_use, MAX_BYTES)
            })
            .map_err(|over| {
                log::warn!(
                    "EXPORT_TO_DMABUF_FD of {bytes} bytes: the VM holds {n} exports of {} bytes, \
                     this process {} of {} bytes ({over:?}); refused",
                    self.bytes_in_use,
                    self.count.held(owner),
                    self.bytes.held(owner),
                );
                NV_ERR_INSUFFICIENT_RESOURCES
            })
    }

    /// GEM `gem` of render handle `render` is an export `owner` made. A
    /// record already there (a number the file reused past a close the
    /// backend did not see) is replaced.
    pub fn record(&mut self, render: u32, gem: u32, owner: Owner, bytes: u64) {
        self.forget(render, gem);
        self.count.charge(owner, 1);
        self.bytes.charge(owner, bytes);
        self.bytes_in_use = self.bytes_in_use.saturating_add(bytes);
        self.records.insert((render, gem), Rec { owner, bytes });
    }

    fn forget(&mut self, render: u32, gem: u32) {
        if let Some(r) = self.records.remove(&(render, gem)) {
            self.count.refund(r.owner, 1);
            self.bytes.refund(r.owner, r.bytes);
            self.bytes_in_use = self.bytes_in_use.saturating_sub(r.bytes);
        }
    }

    /// GEM_CLOSE of `gem` on `render` succeeded.
    pub fn gem_closed(&mut self, render: u32, gem: u32) {
        self.forget(render, gem);
    }

    /// Render handle `render` closed, and its objects with it.
    pub fn file_closed(&mut self, render: u32) {
        let gone: Vec<(u32, u32)> = self
            .records
            .keys()
            .filter(|(r, _)| *r == render)
            .copied()
            .collect();
        for (r, g) in gone {
            self.forget(r, g);
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn bytes_in_use(&self) -> u64 {
        self.bytes_in_use
    }

    /// Whether `(render, gem)` is an export's.
    pub fn is_export(&self, render: u32, gem: u32) -> bool {
        self.records.contains_key(&(render, gem))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvos::NV_ERR_INSUFFICIENT_RESOURCES;

    /// A block of `l` exporting `sizes` (one handle each) of `client`.
    pub(crate) fn block(l: &Layout, fd: i32, client: u32, sizes: &[u64]) -> Vec<u8> {
        let mut b = vec![0u8; l.len];
        b[FD..FD + 4].copy_from_slice(&fd.to_le_bytes());
        le::put_u32(&mut b, H_CLIENT, client);
        le::put_u32(&mut b, TOTAL_OBJECTS, sizes.len() as u32);
        le::put_u32(&mut b, NUM_OBJECTS, sizes.len() as u32);
        le::put_u64(&mut b, TOTAL_SIZE, sizes.iter().sum());
        for (i, s) in sizes.iter().enumerate() {
            le::put_u32(&mut b, l.handles + i * 4, 0x100 + i as u32);
            le::put_u64(&mut b, l.sizes + i * 8, *s);
        }
        b
    }

    fn both() -> [Layout; 2] {
        [
            Layout::for_len(2600).unwrap(),
            Layout::for_len(2608).unwrap(),
        ]
    }

    /// The offsets are nv-ioctl.h's: the arrays end at the status, and the
    /// status and its padding end the block.
    #[test]
    fn the_layouts_are_the_headers() {
        for l in both() {
            assert_eq!(
                l.offsets - l.handles,
                128 * 4 + if l.len == 2608 { 4 } else { 0 }
            );
            assert_eq!(l.sizes - l.offsets, 128 * 8);
            assert_eq!(l.status - l.sizes, 128 * 8);
            assert_eq!(l.len - l.status, 8);
            assert_eq!(l.offsets % 8, 0);
        }
        assert_eq!(Layout::for_len(2604), None);
        assert_eq!(Layout::for_len(0), None);
    }

    #[test]
    fn a_whole_export_in_one_call_passes() {
        for l in both() {
            let b = block(&l, -1, 0xc1d0_0001, &[1 << 20, 2 << 20]);
            assert_eq!(
                check(&l, &b),
                Ok(Ask {
                    client: 0xc1d0_0001,
                    objects: 2,
                    total_size: 3 << 20
                })
            );
        }
    }

    #[test]
    fn the_append_form_and_parts_are_refused() {
        for l in both() {
            let b = block(&l, 3, 1, &[4096]);
            assert_eq!(check(&l, &b), Err(NV_ERR_NOT_SUPPORTED));
            let b = block(&l, -2, 1, &[4096]);
            assert_eq!(check(&l, &b), Err(NV_ERR_INVALID_ARGUMENT));
            // One of two parts.
            let mut b = block(&l, -1, 1, &[4096]);
            le::put_u32(&mut b, TOTAL_OBJECTS, 2);
            le::put_u64(&mut b, TOTAL_SIZE, 8192);
            assert_eq!(check(&l, &b), Err(NV_ERR_NOT_SUPPORTED));
            // A part not at the start.
            let mut b = block(&l, -1, 1, &[4096]);
            le::put_u32(&mut b, INDEX, 1);
            assert_eq!(check(&l, &b), Err(NV_ERR_NOT_SUPPORTED));
        }
    }

    #[test]
    fn what_rm_refuses_itself_is_refused_as_rm_would() {
        for l in both() {
            assert_eq!(
                check(&l, &block(&l, -1, 1, &[])),
                Err(NV_ERR_INVALID_ARGUMENT)
            );
            let many = vec![4096u64; 129];
            let mut b = block(&l, -1, 1, &many[..128]);
            le::put_u32(&mut b, NUM_OBJECTS, 129);
            le::put_u32(&mut b, TOTAL_OBJECTS, 129);
            assert_eq!(check(&l, &b), Err(NV_ERR_INVALID_ARGUMENT));
            // Sizes that do not add up to the total, or overflow.
            let mut b = block(&l, -1, 1, &[4096, 4096]);
            le::put_u64(&mut b, TOTAL_SIZE, 4096);
            assert_eq!(check(&l, &b), Err(NV_ERR_INVALID_ARGUMENT));
            let mut b = block(&l, -1, 1, &[4096, 4096]);
            le::put_u64(&mut b, l.sizes, u64::MAX);
            assert_eq!(check(&l, &b), Err(NV_ERR_INVALID_ARGUMENT));
            // Short.
            assert_eq!(check(&l, &b[..l.len - 1]), Err(NV_ERR_INVALID_ARGUMENT));
        }
    }

    #[test]
    fn force_pcie_and_mmap_are_refused() {
        let l = Layout::for_len(2608).unwrap();
        let m = l.mapping.unwrap();
        let mut b = block(&l, -1, 1, &[4096]);
        b[m] = 1;
        assert_eq!(check(&l, &b), Err(NV_ERR_NOT_SUPPORTED));
        let mut b = block(&l, -1, 1, &[4096]);
        b[m + 1] = 1;
        assert_eq!(check(&l, &b), Err(NV_ERR_NOT_SUPPORTED));
    }

    #[test]
    fn the_budgets_hold_per_process_and_come_back() {
        let a = Owner::Proc {
            tgid: 1,
            start_ns: 1,
        };
        let b = Owner::Proc {
            tgid: 2,
            start_ns: 2,
        };
        let mut x = RmExports::default();
        // Half the bytes to one process, and no more.
        x.record(1, 1, a, MAX_BYTES / 2);
        assert_eq!(x.admits(a, 4096), Err(NV_ERR_INSUFFICIENT_RESOURCES));
        assert_eq!(x.admits(b, 4096), Ok(()));
        x.gem_closed(1, 1);
        assert_eq!(x.admits(a, 4096), Ok(()));
        assert_eq!(x.bytes_in_use(), 0);
        // A quarter of the count.
        for g in 0..EXPORTS_SHARE.per_owner as u32 {
            assert_eq!(x.admits(a, 4096), Ok(()));
            x.record(2, g, a, 4096);
        }
        assert_eq!(x.admits(a, 4096), Err(NV_ERR_INSUFFICIENT_RESOURCES));
        assert_eq!(x.admits(b, 4096), Ok(()));
        // The file takes them all back.
        x.file_closed(2);
        assert!(x.is_empty());
        assert_eq!(x.admits(a, 4096), Ok(()));
        // A number recorded twice is one record.
        x.record(3, 7, a, 4096);
        x.record(3, 7, a, 8192);
        assert_eq!((x.len(), x.bytes_in_use()), (1, 8192));
        assert!(x.is_export(3, 7) && !x.is_export(3, 8));
    }
}
