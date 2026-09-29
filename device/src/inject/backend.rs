// SPDX-License-Identifier: Apache-2.0
//! The backend's side: HOST_OP INJECT_OPEN and INJECT_OPEN_SYNCOBJ, and the
//! handles INJECT_OPEN made, which keep their objects' mmap ranges
//! read-only and their dma-bufs tainted while they are open.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use protocol::inject::InjectInfo;

use super::MAX_OPENS;
use super::registry::{FileId, Registry, overlaps};
use crate::hostfd;
use crate::privfd::PrivateFd;
use crate::quota::{Ledger, Owner, Share};

/// One GEM handle INJECT_OPEN made in a guest file's render file.
#[derive(Clone, Copy, Debug)]
struct OpenRec {
    offset: u64,
    size: u64,
    owner: Owner,
    taint: Option<FileId>,
}

/// The backend's injection state: the registry (with `--inject-socket`),
/// and the handles INJECT_OPEN made, which keep the objects' mmap ranges
/// read-only for as long as they are open.
#[derive(Debug, Default)]
pub struct BackendInject {
    registry: Option<Arc<Registry>>,
    opens: HashMap<(u32, u32), OpenRec>,
    ledger: Ledger,
}

/// The split of [`MAX_OPENS`] among guest processes.
const OPENS_SHARE: Share = Share::quarter(MAX_OPENS, 4);

impl BackendInject {
    pub fn set_registry(&mut self, r: Option<Arc<Registry>>) {
        self.registry = r;
    }

    fn untaint(&self, o: &OpenRec) {
        if let Some(r) = &self.registry {
            r.taint().release(o.taint);
        }
    }

    /// Whether `owner` may make one more open (INJECT_OPEN asks before it
    /// imports anything, so a refusal leaves the file as it was).
    pub fn admits(&self, owner: Owner) -> Result<(), i32> {
        let used = self.opens.len() as u64;
        self.ledger
            .admits(&OPENS_SHARE, owner, 1, used, MAX_OPENS)
            .map_err(|over| {
                log::warn!(
                    "INJECT_OPEN: {used} opens held in the VM, {} by this process: {over:?}; \
                     refused",
                    self.ledger.held(owner)
                );
                libc::EAGAIN
            })
    }

    pub fn registry(&self) -> Option<&Arc<Registry>> {
        self.registry.as_ref()
    }

    /// Whether a mapping of `[off, off + len)` on one of the VM's render or
    /// card files is an injected buffer's, and so placed read-only.
    pub fn read_only(&self, off: u64, len: u64) -> bool {
        self.opens
            .values()
            .any(|o| overlaps(off, len, o.offset, o.size))
            || self
                .registry
                .as_ref()
                .is_some_and(|r| r.maps_live(off, len))
    }

    /// Whether `(render, gem)` is a handle INJECT_OPEN made and still open.
    pub fn is_open(&self, render: u32, gem: u32) -> bool {
        self.opens.contains_key(&(render, gem))
    }

    /// Record a handle INJECT_OPEN made (or found: an import of an object
    /// the file holds gives its handle again), charged to `owner`. EAGAIN
    /// past the VM's or the owner's share.
    pub fn record(
        &mut self,
        render: u32,
        gem: u32,
        offset: u64,
        size: u64,
        owner: Owner,
        dmabuf: Option<&PrivateFd>,
    ) -> Result<(), i32> {
        if self.opens.contains_key(&(render, gem)) {
            return Ok(());
        }
        self.admits(owner)?;
        self.ledger.charge(owner, 1);
        let taint = match (&self.registry, dmabuf) {
            (Some(r), Some(d)) => r.taint().hold(d),
            _ => None,
        };
        self.opens.insert(
            (render, gem),
            OpenRec {
                offset,
                size,
                owner,
                taint,
            },
        );
        Ok(())
    }

    /// Whether handle `gem` of render handle `render` may be PRIME-exported
    /// (HOST_OP PRIME_EXPORT): not one INJECT_OPEN made. The dma-buf an
    /// export makes is checked as well ([`exportable`]), which also catches
    /// an injected object reached through another handle.
    pub fn exportable_handle(&self, render: u32, gem: u32) -> bool {
        !self.is_open(render, gem)
    }

    /// GEM_CLOSE of `gem` on `render` succeeded.
    pub fn gem_closed(&mut self, render: u32, gem: u32) {
        if let Some(o) = self.opens.remove(&(render, gem)) {
            self.ledger.refund(o.owner, 1);
            self.untaint(&o);
        }
    }

    /// Render file `render` closed: its handles went with it.
    pub fn file_closed(&mut self, render: u32) {
        let gone: Vec<(u32, u32)> = self
            .opens
            .keys()
            .filter(|(r, _)| *r == render)
            .copied()
            .collect();
        for k in gone {
            if let Some(o) = self.opens.remove(&k) {
                self.ledger.refund(o.owner, 1);
                self.untaint(&o);
            }
        }
    }

    pub fn reset(&mut self) {
        let opens: Vec<OpenRec> = self.opens.drain().map(|(_, o)| o).collect();
        for o in &opens {
            self.untaint(o);
        }
        self.ledger.clear();
    }

    pub fn opens(&self) -> usize {
        self.opens.len()
    }
}

impl crate::nvidia::NvidiaBackend {
    /// `--inject-socket`'s registry; without one INJECT_OPEN is EOPNOTSUPP
    /// and HELLO offers no BCAP_INJECT.
    pub fn set_inject(&mut self, r: Option<Arc<Registry>>) {
        if let Some(r) = &r {
            let _ = self.inject_taint.set(r.taint().clone());
        }
        self.inject.set_registry(r);
    }

    /// HOST_OP INJECT_OPEN: import id `id`'s object into render handle
    /// `file`'s host file, for a guest that knows its token. Returns the
    /// result words (GEM handle, size, type) and the buffer's description.
    pub(crate) fn inject_open(
        &mut self,
        file: u32,
        id: u32,
        token: &[u8; 16],
    ) -> Result<(Vec<u64>, InjectInfo), i32> {
        let reg = self.inject.registry().cloned().ok_or(libc::EOPNOTSUPP)?;
        let Some(crate::hostfd::HandleKind::DriRender(gpu)) = self.handles.kind(file) else {
            return Err(libc::EBADF);
        };
        let opened = reg.open(id, token, gpu).inspect_err(|e| {
            log::warn!(
                "INJECT_OPEN of id {id} refused: {}",
                io::Error::from_raw_os_error(*e)
            )
        })?;
        let host = reg.host().clone();
        let owner = self.current_owner;
        // The share first: a refusal must leave the file as it was.
        self.inject.admits(owner)?;
        let (render, _) = self.handles.get(file).ok_or(libc::EBADF)?;
        let errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
        let gem = host
            .prime_import(render, opened.dmabuf.as_fd())
            .map_err(errno)?;
        // From here nothing is closed on failure. An import of an object the
        // file already holds gives back the handle it has -- made by any
        // path, a GETFB, a DMABUF_IMPORT -- which is not ours to close; and
        // a new one stays in the caller's own file, closed with it, the same
        // number again on a retry (drm_prime.c's per-file lookup).
        match host.identify(render, gem) {
            Ok(hostfd::NV_GEM_OBJECT_NVKMS) => {}
            t => {
                log::warn!("INJECT_OPEN of id {id}: the import identifies as {t:?}");
                return Err(libc::EIO);
            }
        }
        let offset = host.map_offset(render, gem).map_err(errno)?;
        self.inject
            .record(file, gem, offset, opened.size, owner, Some(&opened.dmabuf))?;
        log::debug!("INJECT_OPEN of id {id}: GEM {gem} of render handle {file}");
        Ok((
            vec![
                u64::from(gem),
                opened.size,
                u64::from(hostfd::NV_GEM_OBJECT_NVKMS),
            ],
            opened.info,
        ))
    }

    /// HOST_OP INJECT_OPEN_SYNCOBJ: import syncobj id `id` into render
    /// handle `file`'s host file, for a guest that knows its token. The
    /// handle is the guest file's too (fences are the host's, fence.rs).
    pub(crate) fn inject_open_syncobj(
        &mut self,
        file: u32,
        id: u32,
        token: &[u8; 16],
    ) -> Result<u64, i32> {
        let reg = self.inject.registry().cloned().ok_or(libc::EOPNOTSUPP)?;
        if !matches!(
            self.handles.kind(file),
            Some(crate::hostfd::HandleKind::DriRender(_))
        ) {
            return Err(libc::EBADF);
        }
        let syncobj = reg.open_syncobj(id, token).inspect_err(|e| {
            log::warn!(
                "INJECT_OPEN_SYNCOBJ of id {id} refused: {}",
                io::Error::from_raw_os_error(*e)
            )
        })?;
        // The file now holds a syncobj someone else holds too: its handles'
        // wait registrations wait out their firing rather than go with a
        // DESTROY (fence.rs, `Registrations::before_ioctl2`), as after any
        // import of a syncobj file.
        self.syncobj_regs
            .before_ioctl2(file, "SYNCOBJ_FD_TO_HANDLE", None);
        let (render, _) = self.handles.get(file).ok_or(libc::EBADF)?;
        let h = reg
            .host()
            .syncobj_import(render, syncobj.as_fd())
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        log::debug!("INJECT_OPEN_SYNCOBJ of id {id}: handle {h} of render handle {file}");
        Ok(u64::from(h))
    }
}
