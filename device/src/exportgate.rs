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
//! One gate, so that no path can miss a check another makes: with a copy
//! of the checks per path, a fence context HOST_OP refuses to export would
//! leave by the Wayland or the IOCTL2 door instead. Every path asks
//! [`ExportGate`], before the export and after it:
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
//! - **After**, with `--allow-dmabuf-export`: the dma-buf is not one RM's
//!   EXPORT_TO_DMABUF_FD made, by its exporter ([`GuestOnly`]). Those are
//!   the guest's own video memory, for the guest's own importers, and stay
//!   in the VM (rmexport.rs): the object a guest file holds of one exports
//!   as that very dma-buf, so this catches it on every path and through any
//!   handle, with no record to outlive or miss.
//!
//! A refusal is EINVAL on every path.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{Arc, OnceLock};

use crate::inject::{BackendInject, SharedTaint};
use crate::semsurf::SemsurfPolicy;

/// The exporter whose dma-bufs no export may hand on, once set: RM's
/// (`rmexport::RM_EXPORTER`) while `--allow-dmabuf-export` is on. Shared with
/// the IOCTL2 hooks, which run off the backend's lock; unset, nothing is
/// asked of any dma-buf.
pub type GuestOnly = Arc<OnceLock<&'static str>>;

/// What an export of a guest GEM object is checked against.
pub struct ExportGate<'a> {
    /// The VM's live fence contexts.
    pub semsurf: &'a SemsurfPolicy,
    /// INJECT_OPEN's handles, where the caller holds them.
    pub injected: Option<&'a BackendInject>,
    /// The injected capture buffers' dma-bufs.
    pub taint: &'a SharedTaint,
    /// The exporter that stays in the VM.
    pub guest_only: &'a GuestOnly,
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
        if let Some(&stays) = self.guest_only.get()
            && crate::hostfd::dmabuf_exporter(dmabuf.as_raw_fd()).as_deref() == Some(stays)
        {
            log::warn!("export of a dma-buf RM's EXPORT_TO_DMABUF_FD made ({stays}); refused");
            return Err(libc::EINVAL);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// A dma-buf of some exporter, where /dev/udmabuf is open to us.
    fn udmabuf() -> Option<std::os::fd::OwnedFd> {
        let dev = crate::sys::fd::open_path("/dev/udmabuf", libc::O_RDWR).ok()?;
        let memfd =
            crate::sys::fd::memfd(c"gate", libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING).ok()?;
        crate::sys::fd::ftruncate(&memfd, 4096).ok()?;
        crate::sys::fd::add_seals(&memfd, libc::F_SEAL_SHRINK).ok()?;
        crate::sys::ioctl::udmabuf_create(dev.as_fd(), memfd.as_fd(), 4096, 1).ok()
    }

    /// With an exporter marked as staying, its dma-bufs are refused and no
    /// other file is; unmarked (the switch off), nothing is asked.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no /dev/udmabuf")]
    fn a_guest_only_exporter_never_leaves() {
        let Some(d) = udmabuf() else {
            eprintln!("SKIPPED a_guest_only_exporter_never_leaves: no /dev/udmabuf");
            return;
        };
        let semsurf = SemsurfPolicy::new();
        let taint = SharedTaint::default();
        let gate = |g| ExportGate {
            semsurf: &semsurf,
            injected: None,
            taint: &taint,
            guest_only: g,
        };
        let off = GuestOnly::default();
        assert_eq!(gate(&off).may_leave(d.as_fd()), Ok(()));
        let on = GuestOnly::default();
        on.set("udmabuf").unwrap();
        assert_eq!(gate(&on).may_leave(d.as_fd()), Err(libc::EINVAL));
        let other = GuestOnly::default();
        other.set(crate::rmexport::RM_EXPORTER).unwrap();
        assert_eq!(gate(&other).may_leave(d.as_fd()), Ok(()));
        let memfd = crate::sys::fd::memfd(c"not-a-dmabuf", libc::MFD_CLOEXEC).unwrap();
        assert_eq!(gate(&on).may_leave(memfd.as_fd()), Ok(()));
    }
}
