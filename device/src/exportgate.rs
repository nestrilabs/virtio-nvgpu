// SPDX-License-Identifier: Apache-2.0
//! The one export gate: whether a guest's GEM object may leave the backend
//! as a dma-buf.
//!
//! Three paths PRIME-export an object a guest file holds, and hand the
//! dma-buf on to something outside the guest's own render file:
//!
//! - HOST_OP PRIME_EXPORT (session.rs), which gives the guest a dma-buf
//!   handle to pass anywhere;
//! - a Wayland WL_SEND whose dma-buf descriptor names a render file's GEM
//!   (wl/serve.rs `TableSend`), which hands the dma-buf to the compositor;
//! - an IOCTL2 re-home (xfer.rs), which imports it into a KMS file.
//!
//! One gate, so that no path can miss a check another has: separate
//! copies of the checks once let the Wayland and IOCTL2 paths export a
//! fence context HOST_OP refused. Every path asks [`ExportGate`], before
//! the export and after it:
//!
//! - **Before:** not a fence context (0x54). A context is counted against
//!   its file's and the session's caps only until its GEM handle closes
//!   (semsurf.rs), and holds a host kthread, a timer and an NVKMS duplicate.
//!   A dma-buf of it keeps all of that alive past the close, uncounted, as
//!   often as the guest likes. Nothing needs one: the object has no pages
//!   (nv_fence_context_gem_ops has no sg table), and the guest driver
//!   refuses to export one itself.
//! - **Before:** not a handle INJECT_OPEN made, where the caller has those
//!   records (the backend does; the IOCTL2 executors, off its lock, do not,
//!   and rely on the taint, which catches the same objects).
//! - **After:** the dma-buf is not an injected capture buffer's, by file
//!   identity (inject/registry.rs `Taint`). That also catches an injected
//!   object reached through another handle. A capture buffer is the guest's
//!   to read, never the host's to show (SECURITY.md, "Capture injection").
//!
//! A refusal is EINVAL on every path.

#![forbid(unsafe_code)]

use std::os::fd::BorrowedFd;

use crate::inject::{BackendInject, SharedTaint};
use crate::semsurf::SemsurfPolicy;

/// What an export of a guest GEM object is checked against.
pub struct ExportGate<'a> {
    /// The VM's live fence contexts.
    pub semsurf: &'a SemsurfPolicy,
    /// INJECT_OPEN's handles, where the caller holds them.
    pub injected: Option<&'a BackendInject>,
    /// The injected capture buffers' dma-bufs.
    pub taint: &'a SharedTaint,
}

impl ExportGate<'_> {
    /// Whether `gem` of guest file `file` may be exported at all: asked
    /// before the export, so a refused object is never exported.
    pub fn may_export(&self, file: u32, gem: u32) -> Result<(), i32> {
        if self.semsurf.is_ctx(file, gem) {
            log::warn!("export of GEM {gem} of handle {file}, a fence context; refused");
            return Err(libc::EINVAL);
        }
        if self
            .injected
            .is_some_and(|i| !i.exportable_handle(file, gem))
        {
            log::warn!("export of GEM {gem} of handle {file}, injected; refused");
            return Err(libc::EINVAL);
        }
        Ok(())
    }

    /// Whether `dmabuf`, just exported, may leave: asked after the export,
    /// and the caller closes a refused one.
    pub fn may_leave(&self, dmabuf: BorrowedFd<'_>) -> Result<(), i32> {
        if !crate::inject::exportable(self.taint, dmabuf) {
            log::warn!("export of an injected capture buffer's dma-buf; refused");
            return Err(libc::EINVAL);
        }
        Ok(())
    }
}
