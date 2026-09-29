// SPDX-License-Identifier: Apache-2.0
//! What injection asks of the host kernel: dma-buf and syncobj checks, and
//! the render-node calls the registry and INJECT_OPEN make.

#![forbid(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use crate::hostfd;

/// What injection asks of the host kernel, behind a trait so the rules can
/// be tested against a fake nvidia-drm.
pub trait InjectHost: Send + Sync {
    /// Whether `fd` is a dma-buf: `fstatfs`'s `f_type` is the dma-buf
    /// filesystem's magic.
    fn is_dmabuf(&self, fd: BorrowedFd<'_>) -> bool;
    /// A dma-buf's size (its `lseek` end).
    fn dmabuf_size(&self, fd: BorrowedFd<'_>) -> io::Result<u64>;
    /// How many render nodes this GPU list has (`HostNodes::dri`).
    fn render_nodes(&self) -> u32;
    /// A new file of render node `index`, for the backend's own imports.
    fn open_render(&self, index: u32) -> io::Result<OwnedFd>;
    /// PRIME_FD_TO_HANDLE: the handle `dmabuf`'s object has in `render`.
    fn prime_import(&self, render: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32>;
    /// GEM_IDENTIFY_OBJECT.
    fn identify(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u32>;
    /// PRIME_HANDLE_TO_FD: the dma-buf of handle `gem` of `render`.
    fn prime_export(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<OwnedFd>;
    /// GEM_MAP_OFFSET: the object's mmap offset.
    fn map_offset(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u64>;
    fn gem_close(&self, render: BorrowedFd<'_>, gem: u32);
    fn random(&self, buf: &mut [u8]) -> io::Result<()>;
    /// Whether `fd` is a DRM syncobj file (`anon_inode:syncobj_file`,
    /// `hostfd::classify`).
    fn is_syncobj(&self, fd: BorrowedFd<'_>) -> bool;
    /// SYNCOBJ_FD_TO_HANDLE: a new handle in `render` for the syncobj.
    fn syncobj_import(&self, render: BorrowedFd<'_>, syncobj: BorrowedFd<'_>) -> io::Result<u32>;
    fn syncobj_destroy(&self, render: BorrowedFd<'_>, handle: u32);
}

/// The real host: the render nodes `/dev/dri/<name>` of this GPU list.
pub struct SysInjectHost {
    render_paths: std::sync::OnceLock<Vec<String>>,
}

impl SysInjectHost {
    /// `names` are the render nodes' names (`renderD128`), in the backend's
    /// render index order (`HostNodes::dri`).
    pub fn new(names: Vec<String>) -> Self {
        let s = Self::for_this_host();
        let _ = s
            .render_paths
            .set(names.into_iter().map(|n| format!("/dev/dri/{n}")).collect());
        s
    }

    /// This host's render nodes, enumerated as the dispatcher does
    /// (`nvidia::host_render_names`) when first needed: after the sandbox,
    /// which leaves /proc/driver/nvidia and the GPUs' sysfs readable.
    pub fn for_this_host() -> Self {
        Self {
            render_paths: std::sync::OnceLock::new(),
        }
    }

    fn paths(&self) -> &[String] {
        self.render_paths.get_or_init(|| {
            crate::nvidia::host_render_names()
                .into_iter()
                .map(|n| format!("/dev/dri/{n}"))
                .collect()
        })
    }
}

impl InjectHost for SysInjectHost {
    fn is_dmabuf(&self, fd: BorrowedFd<'_>) -> bool {
        crate::sys::fd::fstatfs_type(fd.as_raw_fd()).is_ok_and(|t| t == hostfd::DMA_BUF_MAGIC)
    }
    fn dmabuf_size(&self, fd: BorrowedFd<'_>) -> io::Result<u64> {
        hostfd::dmabuf_size(fd.as_raw_fd())
    }
    fn render_nodes(&self) -> u32 {
        self.paths().len() as u32
    }
    fn open_render(&self, index: u32) -> io::Result<OwnedFd> {
        let p = self
            .paths()
            .get(index as usize)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODEV))?;
        crate::sys::fd::open_path(p, libc::O_RDWR | libc::O_CLOEXEC)
    }
    fn prime_import(&self, render: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> io::Result<u32> {
        hostfd::prime_import(render.as_raw_fd(), dmabuf.as_raw_fd())
    }
    fn identify(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u32> {
        hostfd::gem_identify(render.as_raw_fd(), gem)
    }
    fn prime_export(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<OwnedFd> {
        hostfd::prime_export(render.as_raw_fd(), gem)
    }
    fn map_offset(&self, render: BorrowedFd<'_>, gem: u32) -> io::Result<u64> {
        hostfd::gem_map_offset(render.as_raw_fd(), gem)
    }
    fn gem_close(&self, render: BorrowedFd<'_>, gem: u32) {
        let _ = hostfd::gem_close(render.as_raw_fd(), gem);
    }
    fn random(&self, buf: &mut [u8]) -> io::Result<()> {
        crate::sys::proc::getrandom(buf)
    }
    fn is_syncobj(&self, fd: BorrowedFd<'_>) -> bool {
        hostfd::classify(fd, &[]) == hostfd::HandleKind::Syncobj
    }
    fn syncobj_import(&self, render: BorrowedFd<'_>, syncobj: BorrowedFd<'_>) -> io::Result<u32> {
        hostfd::syncobj_import(render.as_raw_fd(), syncobj.as_raw_fd())
    }
    fn syncobj_destroy(&self, render: BorrowedFd<'_>, handle: u32) {
        let _ = hostfd::syncobj_destroy(render.as_raw_fd(), handle);
    }
}
