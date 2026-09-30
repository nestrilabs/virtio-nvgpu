// SPDX-License-Identifier: Apache-2.0
//! One VM's injected buffers and syncobjs: what IMPORT and IMPORT_SYNCOBJ
//! checked and keep under an id and a token, what INJECT_OPEN looks up, and
//! the taint set no export may hand out.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::inject::{InjImport, InjectInfo};

use super::check::{check_layout, check_request, token_eq};
use super::host::InjectHost;
use super::{MAX_BUFFERS, MAX_BYTES, MAX_SYNCOBJS};
use crate::hostfd;
use crate::privfd::PrivateFd;

/// One injected buffer.
#[derive(Debug)]
struct Injected {
    token: [u8; 16],
    /// The helper connection that imported it: only it may RELEASE it, and
    /// its hangup releases it.
    peer: u64,
    info: InjectInfo,
    /// The dma-buf of plane 0 (every plane is the same object). Private:
    /// no IOCTL2 may adopt its number (privfd.rs).
    dmabuf: PrivateFd,
    /// Which render node's device it is memory of.
    gpu: u32,
    /// Its handle in the backend's own file of that node.
    gem: u32,
    size: u64,
    /// Its mmap offset: the object's, the same from every file.
    offset: u64,
    /// Its hold on the taint set.
    taint: Option<FileId>,
}

/// One injected syncobj.
#[derive(Debug)]
struct InjectedSyncobj {
    token: [u8; 16],
    peer: u64,
    /// The syncobj file, which keeps the syncobj alive while the id lives.
    file: PrivateFd,
}

/// What only the helpers' side touches: the backend's own render files and
/// the handles ids hold in them. Held for the whole of an IMPORT, an
/// IMPORT_SYNCOBJ, a RELEASE or a hangup -- kernel calls on the helper's
/// descriptors included, one of which (the import of another device's
/// dma-buf) attaches it to its exporter -- and never by INJECT_OPEN, which
/// takes [`State`] alone, briefly: a helper's slow import stalls its own
/// connection, not the VM's queue. Always taken before `State`.
#[derive(Debug, Default)]
struct Own {
    /// The backend's own render file of each node, opened on first use.
    renders: Vec<Option<PrivateFd>>,
    /// Ids holding each (node, handle) of those files: a dma-buf imported
    /// twice is one handle there, closed when the last id goes.
    held: HashMap<(u32, u32), u32>,
}

#[derive(Debug, Default)]
struct State {
    live: BTreeMap<u32, Injected>,
    syncobjs: BTreeMap<u32, InjectedSyncobj>,
    next_id: u32,
    bytes: u64,
}

/// What a guest's INJECT_OPEN is given to import.
#[derive(Debug)]
pub struct Opened {
    pub dmabuf: PrivateFd,
    pub info: InjectInfo,
    pub size: u64,
}

/// One VM's injected buffers.
pub struct Registry {
    host: Arc<dyn InjectHost>,
    taint: Arc<Taint>,
    own: Mutex<Own>,
    state: Mutex<State>,
    max_buffers: usize,
    max_bytes: u64,
    max_syncobjs: usize,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("Registry")
            .field("live", &st.live.len())
            .field("bytes", &st.bytes)
            .field("syncobjs", &st.syncobjs.len())
            .finish()
    }
}

impl Registry {
    pub fn new(host: Arc<dyn InjectHost>) -> Self {
        Self::with_limits(host, MAX_BUFFERS, MAX_BYTES)
    }

    pub fn with_limits(host: Arc<dyn InjectHost>, max_buffers: usize, max_bytes: u64) -> Self {
        Self {
            host,
            taint: Arc::default(),
            own: Mutex::new(Own::default()),
            state: Mutex::new(State {
                next_id: 1,
                ..State::default()
            }),
            max_buffers,
            max_bytes,
            max_syncobjs: MAX_SYNCOBJS,
        }
    }

    pub fn host(&self) -> &Arc<dyn InjectHost> {
        &self.host
    }

    /// Buffer ids, their bytes, and syncobj ids, per VM.
    pub fn limits(&self) -> (usize, u64, usize) {
        (self.max_buffers, self.max_bytes, self.max_syncobjs)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_own(&self) -> MutexGuard<'_, Own> {
        self.own.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Open the backend's own file of node `gpu`, if it is not yet.
    fn ensure_render(&self, own: &mut Own, gpu: u32) -> io::Result<()> {
        let i = gpu as usize;
        if own.renders.len() <= i {
            own.renders.resize_with(i + 1, || None);
        }
        if own.renders[i].is_none() {
            own.renders[i] = Some(PrivateFd::new(self.host.open_render(gpu)?));
        }
        Ok(())
    }

    /// Close handle `gem` of node `gpu`'s file unless an id holds it.
    fn drop_unheld(&self, own: &mut Own, gpu: u32, gem: u32) {
        if own.held.contains_key(&(gpu, gem)) || self.ensure_render(own, gpu).is_err() {
            return;
        }
        self.host.gem_close(render_fd(own, gpu), gem);
    }

    /// An id's hold on its handle is gone: closed with the last.
    fn unhold(&self, own: &mut Own, gpu: u32, gem: u32) {
        let key = (gpu, gem);
        if let Some(n) = own.held.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                own.held.remove(&key);
                self.drop_unheld(own, gpu, gem);
            }
        }
    }

    /// IMPORT from helper connection `peer`: the checked buffer's id and
    /// token, or the errno the helper is answered with.
    pub fn import<F: Into<PrivateFd>>(
        &self,
        peer: u64,
        imp: &InjImport,
        fds: Vec<F>,
    ) -> Result<(u32, [u8; 16]), i32> {
        // The backend's own descriptors from here (privfd.rs).
        let fds: Vec<PrivateFd> = fds.into_iter().map(Into::into).collect();
        check_request(imp)?;
        if fds.len() != imp.nplanes as usize {
            return Err(libc::EINVAL);
        }
        if let Some(i) = fds.iter().position(|f| !self.host.is_dmabuf(f.as_fd())) {
            log::warn!("inject: plane {i} of an IMPORT is not a dma-buf; refused");
            return Err(libc::EBADF);
        }
        let mut own = self.lock_own();
        // No kernel call for a VM already at its count. What the buffer adds
        // is checked once its size is known, under the lock the id is made
        // under.
        self.room_for_one()?;
        // Which of this GPU list's devices the memory is NVKMS memory of.
        // nvidia-drm hands a dma-buf of its own device back as the very
        // object it exported (the PRIME self-import); anything else becomes
        // a dma-buf object, which IDENTIFY names as such.
        let mut found = None;
        for gpu in 0..self.host.render_nodes() {
            if let Err(e) = self.ensure_render(&mut own, gpu) {
                log::warn!("inject: render node {gpu}: {e}");
                continue;
            }
            let r = render_fd(&own, gpu);
            let Ok(gem) = self.host.prime_import(r, fds[0].as_fd()) else {
                continue;
            };
            match self.host.identify(r, gem) {
                // NVKMS is not enough: nvidia-drm imports another NVIDIA
                // device's buffer by duplicating it into an NVKMS object of
                // this one (nv_drm_gem_prime_import -> prime_dup ->
                // dupMemory), which identifies as NVKMS too. Only a
                // self-import is the helper's very object: its export from
                // our file is the helper's own dma-buf, the one file
                // (drm_gem_prime_handle_to_dmabuf reuses obj->dma_buf).
                Ok(hostfd::NV_GEM_OBJECT_NVKMS) if self.self_import(r, gem, &fds[0]) => {
                    found = Some((gpu, gem));
                    break;
                }
                t => {
                    log::debug!(
                        "inject: on render node {gpu} the buffer identifies as {t:?}, \
                         or is not this device's own object"
                    );
                    self.drop_unheld(&mut own, gpu, gem);
                }
            }
        }
        let Some((gpu, gem)) = found else {
            log::warn!(
                "inject: an IMPORT's buffer is not nvidia-drm (NVKMS) memory of this GPU; refused"
            );
            return Err(libc::ENODEV);
        };
        let checked = self
            .check_import(&mut own, imp, &fds, gpu, gem)
            .and_then(|v| {
                let mut token = [0u8; 16];
                self.host.random(&mut token).map_err(|e| {
                    log::warn!("inject: getrandom: {e}");
                    libc::EIO
                })?;
                Ok((v, token))
            });
        let ((size, offset), token) = match checked {
            Ok(v) => v,
            Err(e) => {
                self.drop_unheld(&mut own, gpu, gem);
                return Err(e);
            }
        };
        let mut st = self.lock();
        if let Err(e) = Self::room(&st, self.max_buffers, self.max_bytes, size) {
            drop(st);
            self.drop_unheld(&mut own, gpu, gem);
            return Err(e);
        }
        let dmabuf = fds.into_iter().next().expect("nplanes >= 1");
        // No id without its hold on the taint set: an injected object no
        // export refuses (out of descriptors to keep its dma-buf by, say)
        // is refused instead, never kept untainted.
        let Some(taint) = self.taint.hold(&dmabuf) else {
            drop(st);
            self.drop_unheld(&mut own, gpu, gem);
            log::warn!("inject: no hold on the taint set for an IMPORT's buffer; refused");
            return Err(libc::EMFILE);
        };
        let id = next_id(&mut st);
        let info = InjectInfo {
            width: imp.width,
            height: imp.height,
            fourcc: imp.fourcc,
            nplanes: imp.nplanes,
            modifier: imp.modifier,
            offsets: imp.offsets,
            strides: imp.strides,
            flags: imp.flags,
            reserved: 0,
        };
        *own.held.entry((gpu, gem)).or_insert(0) += 1;
        st.bytes += size;
        st.live.insert(
            id,
            Injected {
                token,
                peer,
                info,
                dmabuf,
                gpu,
                gem,
                size,
                offset,
                taint: Some(taint),
            },
        );
        log::debug!(
            "inject: id {id}: {}x{} {:#x} modifier {:#x}, {size} bytes on render node {gpu}",
            imp.width,
            imp.height,
            imp.fourcc,
            imp.modifier
        );
        Ok((id, token))
    }

    /// Whether handle `gem` of `render` is the object `helper` is a dma-buf
    /// of: its export is the same file.
    fn self_import(&self, render: BorrowedFd<'_>, gem: u32, helper: &PrivateFd) -> bool {
        match self.host.prime_export(render, gem) {
            Ok(back) => same_file(back.as_fd(), helper.as_fd()),
            Err(e) => {
                log::debug!("inject: PRIME export of the import for its proof: {e}");
                false
            }
        }
    }

    /// The taint set: dma-bufs of injected objects, which no path exports
    /// to anyone (`BackendInject::exportable`).
    pub fn taint(&self) -> &Arc<Taint> {
        &self.taint
    }

    /// Whether the count leaves room for one more buffer, whatever its size.
    fn room_for_one(&self) -> Result<(), i32> {
        Self::room(&self.lock(), self.max_buffers, self.max_bytes, 0)
    }

    fn room(st: &State, max_buffers: usize, max_bytes: u64, size: u64) -> Result<(), i32> {
        if st.live.len() >= max_buffers {
            log::warn!(
                "inject: {} buffers are held already; an IMPORT is refused",
                st.live.len()
            );
            return Err(libc::ENOSPC);
        }
        if st.bytes.saturating_add(size) > max_bytes {
            log::warn!(
                "inject: {} bytes held, {size} more would pass {max_bytes}; refused",
                st.bytes
            );
            return Err(libc::EDQUOT);
        }
        Ok(())
    }

    /// The rest of an IMPORT's checks, once plane 0 is known to be handle
    /// `gem` of node `gpu`: its size and offset.
    fn check_import(
        &self,
        own: &mut Own,
        imp: &InjImport,
        fds: &[PrivateFd],
        gpu: u32,
        gem: u32,
    ) -> Result<(u64, u64), i32> {
        self.ensure_render(own, gpu).map_err(|_| libc::EIO)?;
        let r = render_fd(own, gpu);
        // One object: the guest gets one dma-buf and the planes' offsets
        // into it.
        for (p, fd) in fds.iter().enumerate().skip(1) {
            let other = self
                .host
                .prime_import(r, fd.as_fd())
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            if other != gem {
                log::warn!("inject: plane {p} is another object than plane 0; refused");
                if !own.held.contains_key(&(gpu, other)) {
                    self.host.gem_close(r, other);
                }
                return Err(libc::EINVAL);
            }
        }
        let size = self
            .host
            .dmabuf_size(fds[0].as_fd())
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        if let Err(e) = check_layout(imp, size) {
            log::warn!(
                "inject: a {}x{} layout (offsets {:?}, strides {:?}) does not fit a \
                 {size}-byte buffer; refused",
                imp.width,
                imp.height,
                imp.offsets,
                imp.strides
            );
            return Err(e);
        }
        let offset = self
            .host
            .map_offset(r, gem)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        Ok((size, offset))
    }

    /// RELEASE from `peer`: only an id it imported (ENOENT otherwise).
    pub fn release(&self, peer: u64, id: u32) -> Result<(), i32> {
        let mut own = self.lock_own();
        let mut st = self.lock();
        if st.syncobjs.get(&id).is_some_and(|o| o.peer == peer) {
            st.syncobjs.remove(&id);
            return Ok(());
        }
        match st.live.get(&id) {
            Some(b) if b.peer == peer => {}
            _ => return Err(libc::ENOENT),
        }
        let b = forget(&mut st, id).expect("live");
        drop(st);
        self.taint.release(b.taint);
        self.unhold(&mut own, b.gpu, b.gem);
        Ok(())
    }

    /// Everything `peer` imported, at its hangup.
    pub fn release_peer(&self, peer: u64) -> usize {
        let mut own = self.lock_own();
        let mut st = self.lock();
        let ids: Vec<u32> = st
            .live
            .iter()
            .filter(|(_, b)| b.peer == peer)
            .map(|(&id, _)| id)
            .collect();
        let gone: Vec<Injected> = ids.iter().filter_map(|&id| forget(&mut st, id)).collect();
        let before = st.syncobjs.len();
        st.syncobjs.retain(|_, o| o.peer != peer);
        let n = gone.len() + before - st.syncobjs.len();
        drop(st);
        for b in gone {
            self.taint.release(b.taint);
            self.unhold(&mut own, b.gpu, b.gem);
        }
        n
    }

    /// IMPORT_SYNCOBJ from `peer`: the syncobj file's id and token.
    pub fn import_syncobj<F: Into<PrivateFd>>(
        &self,
        peer: u64,
        flags: u32,
        fds: Vec<F>,
    ) -> Result<(u32, [u8; 16]), i32> {
        let fds: Vec<PrivateFd> = fds.into_iter().map(Into::into).collect();
        if flags != 0 || fds.len() != 1 {
            return Err(libc::EINVAL);
        }
        let file = fds.into_iter().next().expect("one");
        if !self.host.is_syncobj(file.as_fd()) {
            log::warn!("inject: an IMPORT_SYNCOBJ's descriptor is not a syncobj file; refused");
            return Err(libc::EBADF);
        }
        let mut own = self.lock_own();
        let room = |st: &State| {
            if st.syncobjs.len() >= self.max_syncobjs {
                log::warn!(
                    "inject: {} syncobjs are held already; an IMPORT_SYNCOBJ is refused",
                    st.syncobjs.len()
                );
                return Err(libc::ENOSPC);
            }
            Ok(())
        };
        room(&self.lock())?;
        // What the kernel says it is: a syncobj file imports (the DRM core
        // checks its file operations), anything else is refused. The handle
        // is not kept: the file keeps the syncobj.
        self.ensure_render(&mut own, 0).map_err(|_| libc::EIO)?;
        let r = render_fd(&own, 0);
        match self.host.syncobj_import(r, file.as_fd()) {
            Ok(h) => self.host.syncobj_destroy(r, h),
            Err(e) => {
                log::warn!("inject: the kernel refused an IMPORT_SYNCOBJ's syncobj: {e}");
                return Err(libc::EBADF);
            }
        }
        let mut token = [0u8; 16];
        self.host.random(&mut token).map_err(|_| libc::EIO)?;
        let mut st = self.lock();
        room(&st)?;
        let id = next_id(&mut st);
        st.syncobjs
            .insert(id, InjectedSyncobj { token, peer, file });
        log::debug!("inject: syncobj id {id}");
        Ok((id, token))
    }

    /// INJECT_OPEN_SYNCOBJ's lookup: a new descriptor of syncobj id `id`'s
    /// file, for its token (ENOENT alike for a missing id, a buffer's id and
    /// a wrong token).
    pub fn open_syncobj(&self, id: u32, token: &[u8; 16]) -> Result<PrivateFd, i32> {
        let st = self.lock();
        let o = st.syncobjs.get(&id);
        let ok = token_eq(o.map_or(&[0xff; 16], |o| &o.token), token) && o.is_some();
        let Some(o) = o.filter(|_| ok) else {
            return Err(libc::ENOENT);
        };
        o.file.try_clone().map_err(|_| libc::EMFILE)
    }

    /// Syncobj ids held.
    pub fn syncobjs(&self) -> usize {
        self.lock().syncobjs.len()
    }

    /// INJECT_OPEN's lookup: id `id` with token `token`, for a render file
    /// of node `gpu`. ENOENT alike for an id that is not live and a token
    /// that does not match; ENODEV for a file of another device.
    pub fn open(&self, id: u32, token: &[u8; 16], gpu: u32) -> Result<Opened, i32> {
        let st = self.lock();
        let b = st.live.get(&id);
        // Compared even when there is no such id, against a token no id
        // has, so both refusals take the same path.
        let ok = token_eq(b.map_or(&[0xff; 16], |b| &b.token), token) && b.is_some();
        let Some(b) = b.filter(|_| ok) else {
            return Err(libc::ENOENT);
        };
        if b.gpu != gpu {
            return Err(libc::ENODEV);
        }
        let dmabuf = b.dmabuf.try_clone().map_err(|_| libc::EMFILE)?;
        Ok(Opened {
            dmabuf,
            info: b.info,
            size: b.size,
        })
    }

    /// Whether `[off, off + len)` of a render node overlaps a live buffer's
    /// mmap range.
    pub fn maps_live(&self, off: u64, len: u64) -> bool {
        let st = self.lock();
        st.live
            .values()
            .any(|b| overlaps(off, len, b.offset, b.size))
    }

    /// Ids held (for tests and the log).
    pub fn live(&self) -> usize {
        self.lock().live.len()
    }

    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }
}

/// Take id `id` out of the live buffers, with its bytes.
fn forget(st: &mut State, id: u32) -> Option<Injected> {
    let b = st.live.remove(&id)?;
    st.bytes -= b.size;
    log::debug!("inject: id {id} released");
    Some(b)
}

/// The backend's own file of node `gpu`, once [`Registry::ensure_render`]
/// has opened it.
fn render_fd(own: &Own, gpu: u32) -> BorrowedFd<'_> {
    own.renders[gpu as usize]
        .as_ref()
        .expect("opened by ensure_render")
        .as_fd()
}

fn next_id(st: &mut State) -> u32 {
    loop {
        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1).max(1);
        if !st.live.contains_key(&id) && !st.syncobjs.contains_key(&id) {
            return id;
        }
    }
}

pub(super) fn overlaps(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    a < b.saturating_add(blen) && b < a.saturating_add(alen.max(1))
}

/// A file's identity: its device and inode (a dma-buf's inode is its own).
pub type FileId = (u64, u64);

fn file_id(fd: BorrowedFd<'_>) -> Option<FileId> {
    crate::sys::fd::fstat(fd.as_raw_fd())
        .ok()
        .map(|st| (st.st_dev, st.st_ino))
}

/// Whether two descriptors are one file.
fn same_file(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> bool {
    matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// The dma-bufs of injected objects, while an id or a guest open holds
/// them: what no PRIME export may hand anyone (HOST_OP PRIME_EXPORT, a
/// Wayland buffer for the host compositor, an IOCTL2 re-home into a KMS
/// file). An export of an injected object from any file is the helper's
/// own dma-buf (nvidia-drm keeps the object's `dma_buf` while the file is
/// open), so one held here is recognised by its identity; each entry keeps
/// its dma-buf open so that identity stays the object's.
#[derive(Debug, Default)]
pub struct Taint {
    held: Mutex<HashMap<FileId, (PrivateFd, u32)>>,
    /// Tests: every hold fails, as with no descriptor left to keep one by.
    #[cfg(test)]
    pub(crate) refuse_holds: std::sync::atomic::AtomicBool,
}

impl Taint {
    fn lock(&self) -> MutexGuard<'_, HashMap<FileId, (PrivateFd, u32)>> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hold `fd`'s file; `None` if it has no identity, or no descriptor is
    /// left to keep it by (then nothing is held, and the caller refuses
    /// what it was for).
    pub fn hold(&self, fd: &PrivateFd) -> Option<FileId> {
        #[cfg(test)]
        if self.refuse_holds.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let id = file_id(fd.as_fd())?;
        let mut m = self.lock();
        if let Some(e) = m.get_mut(&id) {
            e.1 += 1;
        } else {
            m.insert(id, (fd.try_clone().ok()?, 1));
        }
        Some(id)
    }

    pub fn release(&self, id: Option<FileId>) {
        let Some(id) = id else { return };
        let mut m = self.lock();
        if let Some(e) = m.get_mut(&id) {
            e.1 -= 1;
            if e.1 == 0 {
                m.remove(&id);
            }
        }
    }

    /// Whether `fd` is an injected object's dma-buf.
    pub fn contains(&self, fd: BorrowedFd<'_>) -> bool {
        file_id(fd).is_some_and(|id| self.lock().contains_key(&id))
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a PRIME export may leave the backend with, shared with the IOCTL2
/// hooks (policy.rs), which run off the backend's lock: set once, by
/// `set_inject`.
pub type SharedTaint = Arc<std::sync::OnceLock<Arc<Taint>>>;

/// Whether dma-buf `fd`, just exported, may go anywhere (see [`Taint`]).
pub fn exportable(taint: &SharedTaint, fd: BorrowedFd<'_>) -> bool {
    !taint.get().is_some_and(|t| t.contains(fd))
}
