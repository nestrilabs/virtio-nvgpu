// SPDX-License-Identifier: Apache-2.0
//! A fake nvidia-drm, for the tests and the fuzz target.
//!
//! "dma-bufs" are memfds (or anything) registered as objects by inode;
//! render files are any descriptors, told apart by the inode they are.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::{Mutex, MutexGuard};

use super::host::InjectHost;

#[derive(Clone, Copy, Debug)]
pub struct Obj {
    /// GEM_IDENTIFY_OBJECT's answer.
    pub ty: u32,
    pub size: u64,
    /// The render node (index) whose device the object is memory of.
    /// Imported on another node, an NVKMS object is duplicated there
    /// as a new NVKMS object (nvidia-drm's prime_dup: IDENTIFY says
    /// NVKMS, but its export is another dma-buf), and anything else
    /// becomes a dma-buf object.
    pub gpu: u32,
    pub offset: u64,
}

#[derive(Default)]
struct St {
    /// inode -> object.
    objs: HashMap<u64, Obj>,
    /// dma-bufs, by inode (fstatfs says so).
    dmabufs: HashSet<u64>,
    /// A descriptor of each object's own dma-buf, what an export of a
    /// self-import gives back.
    exports: HashMap<u64, OwnedFd>,
    /// (render file inode, object inode) -> handle.
    handles: HashMap<(u64, u64), u32>,
    /// render file inode -> its node.
    files: HashMap<u64, u32>,
    next_gem: u32,
    closed: Vec<(u64, u32)>,
    next_rand: u8,
    /// Syncobj files, by inode; handles made of them.
    syncobjs: HashSet<u64>,
    syncobj_handles: HashMap<(u64, u32), u64>,
    next_syncobj: u32,
}

#[derive(Default)]
pub struct FakeHost {
    st: Mutex<St>,
    pub nodes: u32,
    /// While set, PRIME imports wait (an exporter slow to attach).
    stall: (Mutex<bool>, std::sync::Condvar),
}

fn ino(fd: BorrowedFd<'_>) -> u64 {
    crate::sys::fd::fstat(fd.as_raw_fd()).map_or(0, |s| s.st_ino)
}

impl FakeHost {
    pub fn new(nodes: u32) -> Self {
        Self {
            nodes,
            ..Self::default()
        }
    }

    fn st(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A new "dma-buf" of object `o` (a memfd, sized as the object).
    pub fn dmabuf(&self, o: Obj) -> OwnedFd {
        let fd = crate::sys::fd::memfd(c"fake-dmabuf", libc::MFD_CLOEXEC).unwrap();
        crate::sys::fd::ftruncate(&fd, o.size).unwrap();
        let i = ino(fd.as_fd());
        let mut st = self.st();
        st.objs.insert(i, o);
        st.dmabufs.insert(i);
        st.exports.insert(i, fd.try_clone().unwrap());
        fd
    }

    /// Another descriptor of the same object (a second export).
    pub fn same_object(&self, of: &OwnedFd) -> OwnedFd {
        of.try_clone().unwrap()
    }

    /// Something that is not a dma-buf.
    pub fn not_dmabuf(&self) -> OwnedFd {
        crate::sys::fd::memfd(c"not-dmabuf", libc::MFD_CLOEXEC).unwrap()
    }

    /// A render file of node `gpu` (a memfd standing for it).
    pub fn render_file(&self, gpu: u32) -> OwnedFd {
        let fd = crate::sys::fd::memfd(c"fake-render", libc::MFD_CLOEXEC).unwrap();
        let i = ino(fd.as_fd());
        self.st().files.insert(i, gpu);
        fd
    }

    /// Handles `file` holds.
    pub fn handles_in(&self, file: BorrowedFd<'_>) -> usize {
        let f = ino(file);
        self.st().handles.keys().filter(|(r, _)| *r == f).count()
    }

    /// Make PRIME imports wait until `stall(false)`.
    pub fn stall(&self, on: bool) {
        *self.stall.0.lock().unwrap() = on;
        self.stall.1.notify_all();
    }

    /// GEM_CLOSEs so far.
    pub fn closes(&self) -> usize {
        self.st().closed.len()
    }

    /// A new "syncobj file".
    pub fn syncobj(&self) -> OwnedFd {
        let fd = crate::sys::fd::memfd(c"fake-syncobj", libc::MFD_CLOEXEC).unwrap();
        let i = ino(fd.as_fd());
        self.st().syncobjs.insert(i);
        fd
    }

    /// Syncobj handles `file` holds.
    pub fn syncobjs_in(&self, file: BorrowedFd<'_>) -> usize {
        let f = ino(file);
        self.st()
            .syncobj_handles
            .keys()
            .filter(|(r, _)| *r == f)
            .count()
    }
}

impl InjectHost for FakeHost {
    fn is_dmabuf(&self, fd: BorrowedFd<'_>) -> bool {
        self.st().dmabufs.contains(&ino(fd))
    }
    fn dmabuf_size(&self, fd: BorrowedFd<'_>) -> io::Result<u64> {
        crate::sys::fd::size(fd.as_raw_fd())
    }
    fn render_nodes(&self) -> u32 {
        self.nodes
    }
    fn open_render(&self, index: u32) -> io::Result<OwnedFd> {
        if index >= self.nodes {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        Ok(self.render_file(index))
    }
    fn prime_import(&self, render: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32> {
        let mut g = self.stall.0.lock().unwrap();
        while *g {
            g = self.stall.1.wait(g).unwrap();
        }
        drop(g);
        let (r, d) = (ino(render), ino(dmabuf));
        let mut st = self.st();
        if !st.files.contains_key(&r) || !st.dmabufs.contains(&d) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if let Some(&h) = st.handles.get(&(r, d)) {
            return Ok(h);
        }
        st.next_gem += 1;
        let h = st.next_gem;
        st.handles.insert((r, d), h);
        Ok(h)
    }
    fn identify(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u32> {
        let r = ino(render);
        let st = self.st();
        let d = st
            .handles
            .iter()
            .find(|((f, _), h)| *f == r && **h == gem)
            .map(|((_, d), _)| *d)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let o = st.objs[&d];
        let node = st.files[&r];
        // An NVKMS object imported on another NVIDIA device is its
        // duplicate there, NVKMS too; what is not NVKMS memory stays
        // what it is.
        let _ = node;
        Ok(o.ty)
    }
    fn map_offset(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u64> {
        let r = ino(render);
        let st = self.st();
        st.handles
            .iter()
            .find(|((f, _), h)| *f == r && **h == gem)
            .map(|((_, d), _)| st.objs[d].offset)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
    fn prime_export(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<OwnedFd> {
        let r = ino(render);
        let st = self.st();
        let d = st
            .handles
            .iter()
            .find(|((f, _), h)| *f == r && **h == gem)
            .map(|((_, d), _)| *d)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if st.objs[&d].gpu == st.files[&r] {
            // The object's own dma-buf, again.
            st.exports[&d].try_clone()
        } else {
            // A duplicate's (prime_dup), or a dma-buf object's: another
            // file.
            crate::sys::fd::memfd(c"fake-dup-export", libc::MFD_CLOEXEC)
        }
    }
    fn gem_close(&self, render: BorrowedFd<'_>, gem: u32) {
        let r = ino(render);
        let mut st = self.st();
        st.handles.retain(|(f, _), h| !(*f == r && *h == gem));
        st.closed.push((r, gem));
    }
    fn random(&self, buf: &mut [u8]) -> io::Result<()> {
        let mut st = self.st();
        for b in buf {
            st.next_rand = st.next_rand.wrapping_add(37);
            *b = st.next_rand;
        }
        Ok(())
    }
    fn is_syncobj(&self, fd: BorrowedFd<'_>) -> bool {
        self.st().syncobjs.contains(&ino(fd))
    }
    fn syncobj_import(&self, render: BorrowedFd<'_>, syncobj: BorrowedFd<'_>) -> io::Result<u32> {
        let (r, o) = (ino(render), ino(syncobj));
        let mut st = self.st();
        if !st.files.contains_key(&r) || !st.syncobjs.contains(&o) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // A new handle every time, as drm_syncobj_fd_to_handle makes.
        st.next_syncobj += 1;
        let h = st.next_syncobj;
        st.syncobj_handles.insert((r, h), o);
        Ok(h)
    }
    fn syncobj_destroy(&self, render: BorrowedFd<'_>, handle: u32) {
        let r = ino(render);
        self.st().syncobj_handles.remove(&(r, handle));
    }
}
