// SPDX-License-Identifier: Apache-2.0
//! Capture injection (`--inject-socket PATH --inject-uid UID`): host buffers
//! a guest may open without a copy.
//!
//! A screen share on the host is a PipeWire stream of dma-bufs the desktop's
//! portal backend allocated and the compositor fills. A per-VM helper on the
//! host -- built elsewhere, run as a uid of its own -- asks the portal on the
//! guest application's behalf, consumes the stream, and hands each buffer to
//! this backend over a socket of its own ([`InjectServer`]). The backend
//! checks what it was given ([`Registry::import`]) and keeps it under an id
//! and a random token; a guest process that knows both opens it with HOST_OP
//! INJECT_OPEN, which imports the same object into the calling guest file's
//! host render file, where the guest module makes a proxy of it and a guest
//! dma-buf. PipeWire, the portal and every stream protocol stay out of this
//! process: what it parses is one fixed-size packet format
//! (`protocol::inject`).
//!
//! **Who may inject.** Only peers whose `SO_PEERCRED` uid is `--inject-uid`,
//! at most [`MAX_PEERS`] at once. The socket is bound in a private directory
//! and renamed into place, 0600, like the export socket; the operator opens
//! it to the helper's group (contrib/systemd/nvgpu-socket-open). The backend
//! cannot know whether the user consented to what the helper sends: the
//! helper uid is trusted for that, and for nothing else (SECURITY.md §18).
//!
//! **What is accepted.** Each plane's descriptor must be a dma-buf
//! (`fstatfs`'s magic), and must import into a render file of this GPU,
//! opened for the purpose and held by the backend, as nvidia-drm memory:
//! GEM_IDENTIFY_OBJECT says NVKMS, which a dma-buf of another device (an
//! iGPU's, a udmabuf, a v4l2 frame) does not -- nvidia-drm imports those as
//! dma-buf objects. Every plane must resolve to one object, whose size must
//! hold every plane the layout describes, all arithmetic checked. The
//! backend keeps the object's handle and the dma-buf while the id lives:
//! at most [`MAX_BUFFERS`] ids and [`MAX_BYTES`] per VM.
//!
//! **Who may open.** A guest message names an id and its token, compared in
//! constant time; nothing a guest sends lists, enumerates or makes an id. The
//! object is imported into the caller's own render file (not a handle of the
//! backend's): the guest's reference is then a GEM handle of that file, which
//! keeps the memory alive past the helper's RELEASE, as any importer's does.
//! RELEASE (and the helper's hangup) only stops new opens. What INJECT_OPEN
//! has made is bounded per VM and per guest process ([`MAX_OPENS`]).
//!
//! **Explicit sync.** IMPORT_SYNCOBJ hands over a DRM syncobj file, at most
//! [`MAX_SYNCOBJS`] per VM, under an id and a token of its own; HOST_OP
//! INJECT_OPEN_SYNCOBJ imports it into the caller's render file, whose
//! handle is the guest's (fences are the host's, fence.rs). What its points
//! mean is the helper's and the guest daemon's business: the backend reads
//! none of them, and a guest signalling any point, or none, reaches only the
//! helper and the stream it serves.
//!
//! **Read-only, for the CPU.** Every placement of the object's mmap offset in
//! the window is made read-only ([`BackendInject::read_only`]), whichever of
//! the VM's files maps it; the guest module refuses a writable mapping of a
//! read-only placement. The GPU is another matter: nvidia-drm and RM give an
//! importer read-write GPU mappings, and there is no read-only import to ask
//! for, so a guest process holding the buffer can write it with the GPU --
//! only its own stream's buffers, which only it and the helper see.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use protocol::inject::{
    INJ_F_ALL, INJ_MAX_PACKET, INJ_OP_HELLO, INJ_OP_IMPORT, INJ_OP_IMPORT_SYNCOBJ, INJ_OP_RELEASE,
    INJ_VERSION, InjImport, InjMalformed, InjReply, InjRequest, InjectInfo, parse_request,
};

use crate::hostfd;
use crate::privfd::PrivateFd;
use crate::quota::{Ledger, Owner, Share};

/// Injected buffers one VM may hold at once. A screen share wants four to
/// eight buffers a stream, and a VM a few streams.
pub const MAX_BUFFERS: usize = 32;
/// Bytes of injected objects one VM may hold at once: sixteen 2560x1440
/// ARGB buffers and room to spare.
pub const MAX_BYTES: u64 = 1 << 30;
/// Injected syncobjs one VM may hold at once: one or two a stream.
pub const MAX_SYNCOBJS: usize = 16;
/// Helper connections at once.
pub const MAX_PEERS: usize = 4;
/// The largest width or height accepted.
pub const MAX_DIM: u32 = 16384;
/// GEM handles INJECT_OPEN may have made in the VM's render files at once
/// (one per render file and object); a guest process a quarter
/// ([`Share::quarter`]).
pub const MAX_OPENS: u64 = 1024;

/// `DRM_FORMAT_MOD_INVALID`.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A DRM format this accepts: planes, bytes per pixel per plane, and the
/// chroma subsampling of the planes after the first.
#[derive(Clone, Copy, Debug)]
struct Format {
    fourcc: u32,
    planes: usize,
    cpp: [u64; 2],
    sub: u64,
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}

/// What screen capture produces: 8- and 10-bit RGB in both orders, 16-bit
/// float, RGB565, and the two 4:2:0 YUV layouts an encoder-side portal
/// might offer.
const FORMATS: &[Format] = &[
    Format {
        fourcc: fourcc(b'X', b'R', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'R', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'R', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'R', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'4', b'H'),
        planes: 1,
        cpp: [8, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'4', b'H'),
        planes: 1,
        cpp: [8, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'R', b'G', b'1', b'6'),
        planes: 1,
        cpp: [2, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'N', b'V', b'1', b'2'),
        planes: 2,
        cpp: [1, 2],
        sub: 2,
    },
    Format {
        fourcc: fourcc(b'P', b'0', b'1', b'0'),
        planes: 2,
        cpp: [2, 4],
        sub: 2,
    },
];

fn format(fourcc: u32) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.fourcc == fourcc)
}

/// The IMPORT's own fields, before any descriptor is looked at: EINVAL for
/// anything this does not take.
pub fn check_request(imp: &InjImport) -> Result<(), i32> {
    let f = format(imp.fourcc).ok_or(libc::EINVAL)?;
    let n = imp.nplanes as usize;
    if n != f.planes
        || imp.width == 0
        || imp.height == 0
        || imp.width > MAX_DIM
        || imp.height > MAX_DIM
        || imp.flags & !INJ_F_ALL != 0
        || imp.modifier == MOD_INVALID
    {
        return Err(libc::EINVAL);
    }
    // Planes past nplanes say nothing.
    if imp.offsets[n..]
        .iter()
        .chain(&imp.strides[n..])
        .any(|&v| v != 0)
    {
        return Err(libc::EINVAL);
    }
    Ok(())
}

/// Whether an object of `size` bytes holds every plane `imp` describes:
/// each plane's stride holds its row, and its offset plus stride times its
/// rows lies within the object. u64 throughout, every step checked.
pub fn check_layout(imp: &InjImport, size: u64) -> Result<(), i32> {
    check_request(imp)?;
    let f = format(imp.fourcc).ok_or(libc::EINVAL)?;
    for p in 0..f.planes {
        let sub = if p == 0 { 1 } else { f.sub };
        let w = u64::from(imp.width).div_ceil(sub);
        let h = u64::from(imp.height).div_ceil(sub);
        let row = w.checked_mul(f.cpp[p]).ok_or(libc::EINVAL)?;
        let stride = u64::from(imp.strides[p]);
        if stride < row {
            return Err(libc::EINVAL);
        }
        let end = stride
            .checked_mul(h)
            .and_then(|b| b.checked_add(u64::from(imp.offsets[p])))
            .ok_or(libc::EINVAL)?;
        if end > size {
            return Err(libc::EINVAL);
        }
    }
    Ok(())
}

/// Two tokens equal, in time that does not depend on where they differ.
fn token_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let diff = a
        .iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| acc | std::hint::black_box(x ^ y));
    std::hint::black_box(diff) == 0
}

// ---------------------------------------------------------------------------
// The host calls
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

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
        self.room_for_buffer(0)?;
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
        let dmabuf = fds.into_iter().next().expect("nplanes >= 1");
        *own.held.entry((gpu, gem)).or_insert(0) += 1;
        st.bytes += size;
        let taint = self.taint.hold(&dmabuf);
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
                taint,
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

    /// Whether one more buffer of `size` bytes fits (0: the count alone).
    fn room_for_buffer(&self, size: u64) -> Result<(), i32> {
        Self::room(&self.lock(), self.max_buffers, self.max_bytes, size)
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
        self.room_for_buffer(size)?;
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

fn overlaps(a: u64, alen: u64, b: u64, blen: u64) -> bool {
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
}

impl Taint {
    fn lock(&self) -> MutexGuard<'_, HashMap<FileId, (PrivateFd, u32)>> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hold `fd`'s file; `None` if it has no identity (then nothing is held).
    pub fn hold(&self, fd: &PrivateFd) -> Option<FileId> {
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

// ---------------------------------------------------------------------------
// The backend's side: what INJECT_OPEN made
// ---------------------------------------------------------------------------

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
}

impl crate::nvidia::NvidiaBackend {
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

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// Whether what is at the socket path may be replaced: a socket of ours
/// and nothing else (as the export socket, wl/export.rs).
fn may_replace(is_socket: bool, owner: u32, me: u32) -> io::Result<()> {
    if !is_socket {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "inject path exists and is not a socket",
        ));
    }
    if owner != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("inject path is a socket of uid {owner}, not ours"),
        ));
    }
    Ok(())
}

/// Bind at `path` with no moment in which anyone else may connect: in a new
/// directory only we can enter, made 0600 there, then renamed into place.
fn bind_private(path: &Path) -> io::Result<OwnedFd> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let mut n = 0;
    let dir = loop {
        let d = parent.join(format!(".{name}.{}.{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&d) {
            Ok(()) => break d,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 16 => n += 1,
            Err(e) => return Err(e),
        }
    };
    let inner = dir.join("s");
    let r = (|| {
        let l = crate::sys::net::seqpacket_listen(&inner, MAX_PEERS as i32)?;
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&inner, path)?;
        Ok(l)
    })();
    let _ = std::fs::remove_file(&inner);
    let _ = std::fs::remove_dir(&dir);
    r
}

struct Shared {
    registry: Arc<Registry>,
    uid: u32,
    stop: AtomicBool,
    peers: AtomicUsize,
    next_peer: AtomicU64,
    /// Every live connection, to shut down with the server.
    conns: Mutex<HashMap<u64, Arc<PrivateFd>>>,
}

/// The listener and its threads.
pub struct InjectServer {
    path: PathBuf,
    shared: Arc<Shared>,
    listener: Arc<OwnedFd>,
    idle: Mutex<bool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl InjectServer {
    /// Bind at `path` (replacing a stale socket of ours there) for peers of
    /// uid `uid`, without the accepting thread: the backend binds before its
    /// sandbox, which leaves it no directory to make a socket in, and starts
    /// threads after ([`InjectServer::start`]).
    pub fn bind_idle(path: &Path, uid: u32, registry: Arc<Registry>) -> io::Result<Self> {
        let me = crate::sys::proc::uid();
        match std::fs::symlink_metadata(path) {
            Ok(m) => may_replace(m.file_type().is_socket(), m.uid(), me)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let l = bind_private(path)?;
        crate::privfd::register(l.as_raw_fd());
        Ok(Self {
            path: path.to_path_buf(),
            shared: Arc::new(Shared {
                registry,
                uid,
                stop: AtomicBool::new(false),
                peers: AtomicUsize::new(0),
                next_peer: AtomicU64::new(1),
                conns: Mutex::new(HashMap::new()),
            }),
            listener: Arc::new(l),
            idle: Mutex::new(true),
            thread: Mutex::new(None),
        })
    }

    /// Start accepting. Once.
    pub fn start(&self) -> io::Result<()> {
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if !*idle {
            return Ok(());
        }
        let (s, l) = (self.shared.clone(), self.listener.clone());
        let t = std::thread::Builder::new()
            .name("nvgpu-inject".into())
            .spawn(move || accept_loop(s, l))?;
        *self.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(t);
        *idle = false;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.shared.registry
    }

    /// Stop accepting, hang up every peer (releasing what each imported),
    /// and wait for the threads. The socket file stays: the sandbox leaves
    /// the backend no unlink, and the next start replaces it.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = crate::sys::net::shutdown(self.listener.as_raw_fd());
        for c in self
            .shared
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            let _ = crate::sys::net::shutdown(c.as_raw_fd());
        }
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
        // Peer threads end on their shutdown; wait for them to have
        // released what they held.
        for _ in 0..200 {
            if self.shared.peers.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Drop for InjectServer {
    fn drop(&mut self) {
        crate::privfd::unregister(self.listener.as_raw_fd());
    }
}

fn accept_loop(s: Arc<Shared>, l: Arc<OwnedFd>) {
    loop {
        let conn = crate::sys::net::accept(l.as_raw_fd());
        if s.stop.load(Ordering::Relaxed) {
            return;
        }
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                match e.raw_os_error() {
                    Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
                        log::warn!("inject: accept: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Some(libc::EINTR | libc::ECONNABORTED) => {}
                    // The listener was shut down, or is gone.
                    _ => return,
                }
                continue;
            }
        };
        match crate::sys::fd::peer_cred(conn.as_raw_fd()) {
            Ok(c) if c.uid == s.uid => {}
            Ok(c) => {
                log::warn!(
                    "inject: refusing a connection from uid {} (pid {}); only uid {} may inject",
                    c.uid,
                    c.pid,
                    s.uid
                );
                continue;
            }
            Err(e) => {
                log::warn!("inject: SO_PEERCRED: {e}");
                continue;
            }
        }
        if s.peers.fetch_add(1, Ordering::AcqRel) >= MAX_PEERS {
            s.peers.fetch_sub(1, Ordering::AcqRel);
            log::warn!("inject: {MAX_PEERS} helpers connected already; refusing another");
            continue;
        }
        let peer = s.next_peer.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(PrivateFd::new(conn));
        s.conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(peer, conn.clone());
        let s2 = s.clone();
        let spawned = std::thread::Builder::new()
            .name("nvgpu-inject-peer".into())
            .spawn(move || {
                serve_peer(&s2, peer, &conn);
                s2.conns
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&peer);
                let n = s2.registry.release_peer(peer);
                if n > 0 {
                    log::info!("inject: a helper hung up; its {n} buffer(s) released");
                }
                s2.peers.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(e) = spawned {
            log::warn!("inject: no thread for a helper: {e}");
            s.conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&peer);
            s.peers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn reply(conn: &PrivateFd, r: &InjReply) -> bool {
    crate::sys::net::send_packet(conn.as_raw_fd(), &r.to_bytes(), &[]).is_ok()
}

/// One helper connection, until it hangs up or breaks the protocol.
fn serve_peer(s: &Shared, peer: u64, conn: &PrivateFd) {
    let mut hello = false;
    let mut buf = [0u8; INJ_MAX_PACKET];
    loop {
        let p = match crate::sys::net::recv_packet(conn.as_raw_fd(), &mut buf) {
            Ok(p) => p,
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => return,
        };
        // The helper's descriptors are the backend's own from here: no
        // IOCTL2 may adopt one of their numbers (privfd.rs).
        let fds: Vec<PrivateFd> = p.fds.into_iter().map(PrivateFd::new).collect();
        if p.len == 0 && fds.is_empty() && !p.truncated {
            return; // hangup
        }
        let r = serve_packet(
            &s.registry,
            peer,
            &mut hello,
            &buf[..p.len],
            p.truncated || p.fds_truncated,
            fds,
        );
        let (r, end) = match r {
            Served::Reply(r) => (r, false),
            Served::Last(r) => (r, true),
            Served::Hangup => return,
        };
        if !reply(conn, &r) || end {
            return;
        }
    }
}

/// What one packet gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    Reply(InjReply),
    /// A reply, and then the connection ends.
    Last(InjReply),
    /// The connection ends with no reply.
    Hangup,
}

/// One packet of helper connection `peer` (whose HELLO state is `hello`):
/// `bytes` as received, `truncated` if the kernel cut the packet or its
/// descriptors. Every framing rule is here, apart from the socket, for the
/// fuzzer to drive (fuzzing/inject.rs).
pub fn serve_packet(
    reg: &Registry,
    peer: u64,
    hello: &mut bool,
    bytes: &[u8],
    truncated: bool,
    fds: Vec<PrivateFd>,
) -> Served {
    if truncated {
        log::warn!("inject: a helper sent an oversized packet; disconnected");
        return Served::Hangup;
    }
    let req = match parse_request(bytes) {
        Ok(r) => r,
        Err(m) => {
            let why = match m {
                InjMalformed::Size => "a packet of the wrong size",
                InjMalformed::Op => "an unknown op",
                InjMalformed::Reserved => "a reserved field set",
            };
            log::warn!("inject: a helper sent {why}; disconnected");
            return Served::Hangup;
        }
    };
    let (op, takes_fds) = match req {
        InjRequest::Hello(_) => (INJ_OP_HELLO, false),
        InjRequest::Import(_) => (INJ_OP_IMPORT, true),
        InjRequest::Release(_) => (INJ_OP_RELEASE, false),
        InjRequest::ImportSyncobj(_) => (INJ_OP_IMPORT_SYNCOBJ, true),
    };
    if !takes_fds && !fds.is_empty() {
        log::warn!("inject: descriptors on a message that takes none; disconnected");
        return Served::Hangup;
    }
    if !*hello && op != INJ_OP_HELLO {
        log::warn!("inject: a request before HELLO; disconnected");
        return Served::Hangup;
    }
    let mut r = InjReply {
        op,
        ..InjReply::default()
    };
    match req {
        InjRequest::Hello(h) => {
            if *hello || h.version != INJ_VERSION || h.flags != 0 {
                r.status = -libc::EPROTO;
                r.version = INJ_VERSION;
                log::warn!(
                    "inject: HELLO for version {} (flags {:#x}); this backend speaks {INJ_VERSION}",
                    h.version,
                    h.flags
                );
                return Served::Last(r);
            }
            *hello = true;
            let (max_buffers, max_bytes, max_syncobjs) = reg.limits();
            r.version = INJ_VERSION;
            r.max_buffers = max_buffers as u32;
            r.max_bytes = max_bytes;
            r.max_syncobjs = max_syncobjs as u32;
        }
        InjRequest::Import(imp) => match reg.import(peer, &imp, fds) {
            Ok((id, token)) => {
                r.id = id;
                r.token = token;
            }
            Err(e) => r.status = -e,
        },
        InjRequest::ImportSyncobj(so) => match reg.import_syncobj(peer, so.flags, fds) {
            Ok((id, token)) => {
                r.id = id;
                r.token = token;
            }
            Err(e) => r.status = -e,
        },
        InjRequest::Release(rel) => {
            if let Err(e) = reg.release(peer, rel.id) {
                r.status = -e;
            }
        }
    }
    Served::Reply(r)
}

// ---------------------------------------------------------------------------
// A fake nvidia-drm, for the tests and the fuzz targets
// ---------------------------------------------------------------------------

#[cfg(any(test, fuzzing))]
pub mod fake {
    //! "dma-bufs" are memfds (or anything) registered as objects by inode;
    //! render files are any descriptors, told apart by the inode they are.
    use super::*;
    use std::collections::HashSet;

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
        fn syncobj_import(
            &self,
            render: BorrowedFd<'_>,
            syncobj: BorrowedFd<'_>,
        ) -> io::Result<u32> {
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
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeHost, Obj};
    use super::*;
    use protocol::inject::{INJ_REPLY_SIZE, InjHello, InjRelease};

    const XR24: u32 = fourcc(b'X', b'R', b'2', b'4');
    const NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
    const NVKMS: u32 = hostfd::NV_GEM_OBJECT_NVKMS;
    /// An NVIDIA block-linear modifier, as GBM gives.
    const MODIFIER: u64 = 0x0300_0000_0060_6015;

    fn obj(size: u64, offset: u64) -> Obj {
        Obj {
            ty: NVKMS,
            size,
            gpu: 0,
            offset,
        }
    }

    fn rgb(w: u32, h: u32) -> InjImport {
        InjImport {
            nplanes: 1,
            width: w,
            height: h,
            fourcc: XR24,
            flags: 0,
            modifier: MODIFIER,
            offsets: [0; 4],
            strides: [w * 4, 0, 0, 0],
        }
    }

    fn setup() -> (Arc<FakeHost>, Registry) {
        let host = Arc::new(FakeHost::new(1));
        let reg = Registry::new(host.clone());
        (host, reg)
    }

    #[test]
    fn a_well_formed_nvkms_buffer_is_accepted_and_opened_with_its_token() {
        let (host, reg) = setup();
        let d = host.dmabuf(obj(64 * 64 * 4, 0x10_0000));
        let (id, token) = reg.import(1, &rgb(64, 64), vec![d]).unwrap();
        assert_eq!((reg.live(), reg.bytes()), (1, 64 * 64 * 4));
        let o = reg.open(id, &token, 0).unwrap();
        // No IOCTL2 may adopt the number of a descriptor held here.
        assert!(crate::privfd::is_private(o.dmabuf.as_raw_fd()));
        assert_eq!((o.info.width, o.info.height, o.info.fourcc), (64, 64, XR24));
        assert_eq!(o.info.modifier, MODIFIER);
        assert_eq!(o.size, 64 * 64 * 4);
        assert!(reg.maps_live(0x10_0000, 4096));
        assert!(!reg.maps_live(0x10_0000 + 64 * 64 * 4, 4096));
    }

    #[test]
    fn without_the_token_nothing_opens_and_a_missing_id_looks_the_same() {
        let (host, reg) = setup();
        let d = host.dmabuf(obj(4096 * 4, 0));
        let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        let mut wrong = token;
        wrong[15] ^= 1;
        assert_eq!(reg.open(id, &wrong, 0).unwrap_err(), libc::ENOENT);
        assert_eq!(reg.open(id, &[0; 16], 0).unwrap_err(), libc::ENOENT);
        assert_eq!(reg.open(id + 1, &token, 0).unwrap_err(), libc::ENOENT);
        assert_eq!(reg.open(0, &[0xff; 16], 0).unwrap_err(), libc::ENOENT);
        // A render file of another device is told so.
        assert_eq!(reg.open(id, &token, 1).unwrap_err(), libc::ENODEV);
        assert!(reg.open(id, &token, 0).is_ok());
    }

    #[test]
    fn another_vms_backend_knows_nothing_of_this_ones_buffers() {
        let (host, reg) = setup();
        let d = host.dmabuf(obj(4096 * 4, 0));
        let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        let other = Registry::new(Arc::new(FakeHost::new(1)));
        assert_eq!(other.open(id, &token, 0).unwrap_err(), libc::ENOENT);
        assert_eq!(other.release(1, id).unwrap_err(), libc::ENOENT);
        assert_eq!(reg.live(), 1);
    }

    #[test]
    fn what_is_not_a_dmabuf_or_not_this_gpus_nvkms_memory_is_refused() {
        let (host, reg) = setup();
        assert_eq!(
            reg.import(1, &rgb(32, 32), vec![host.not_dmabuf()]),
            Err(libc::EBADF)
        );
        // A foreign device's buffer (a udmabuf, an iGPU's): nvidia-drm
        // imports it as a dma-buf object.
        let foreign = host.dmabuf(Obj {
            ty: hostfd::NV_GEM_OBJECT_DMABUF,
            ..obj(4096 * 4, 0)
        });
        assert_eq!(
            reg.import(1, &rgb(32, 32), vec![foreign]),
            Err(libc::ENODEV)
        );
        // Another NVIDIA GPU's memory, when this GPU list has only one.
        let other_gpu = host.dmabuf(Obj {
            gpu: 1,
            ..obj(4096 * 4, 0)
        });
        assert_eq!(
            reg.import(1, &rgb(32, 32), vec![other_gpu]),
            Err(libc::ENODEV)
        );
        let user = host.dmabuf(Obj {
            ty: hostfd::NV_GEM_OBJECT_USERMEMORY,
            ..obj(4096 * 4, 0)
        });
        assert_eq!(reg.import(1, &rgb(32, 32), vec![user]), Err(libc::ENODEV));
        assert_eq!(reg.live(), 0);
        // What the refused imports made in the backend's file is closed.
        assert_eq!(host.closes(), 3);
    }

    #[test]
    fn a_layout_the_object_cannot_hold_is_refused() {
        let (host, reg) = setup();
        let size = 64 * 64 * 4;
        let imp = |f: &dyn Fn(&mut InjImport)| {
            let mut i = rgb(64, 64);
            f(&mut i);
            let d = host.dmabuf(obj(size, 0));
            reg.import(1, &i, vec![d]).map(|_| ())
        };
        assert_eq!(imp(&|_| {}), Ok(()));
        assert_eq!(imp(&|i| i.height = 65), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.offsets[0] = 4), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.strides[0] = 64 * 4 - 1), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.strides[0] = u32::MAX), Err(libc::EINVAL));
        assert_eq!(
            imp(&|i| {
                i.offsets[0] = u32::MAX;
                i.strides[0] = u32::MAX;
            }),
            Err(libc::EINVAL)
        );
        assert_eq!(imp(&|i| i.width = 0), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.width = MAX_DIM + 1), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.fourcc = 0x1234_5678), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.flags = 2), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.modifier = MOD_INVALID), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.strides[1] = 4), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.nplanes = 2), Err(libc::EINVAL));
        assert_eq!(imp(&|i| i.nplanes = 5), Err(libc::EINVAL));
        assert_eq!(reg.live(), 1);
    }

    /// A helper whose buffer is slow to import holds up its own connection,
    /// not a guest's INJECT_OPEN, which the VM's queue thread serves.
    #[test]
    fn a_slow_import_does_not_hold_up_an_open() {
        let (host, reg) = setup();
        let (id, token) = reg
            .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
            .unwrap();
        let reg = Arc::new(reg);
        host.stall(true);
        let (h2, r2) = (host.clone(), reg.clone());
        let t = std::thread::spawn(move || {
            r2.import(2, &rgb(32, 32), vec![h2.dmabuf(obj(4096 * 4, 0x10000))])
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let t0 = std::time::Instant::now();
        assert!(reg.open(id, &token, 0).is_ok());
        assert!(reg.maps_live(0, 1));
        assert!(t0.elapsed() < std::time::Duration::from_millis(40));
        host.stall(false);
        assert!(t.join().unwrap().is_ok());
        assert_eq!(reg.live(), 2);
    }

    #[test]
    fn check_layout_wraps_nowhere() {
        let mut i = rgb(MAX_DIM, MAX_DIM);
        i.strides[0] = u32::MAX;
        i.offsets[0] = u32::MAX;
        assert_eq!(check_layout(&i, u64::MAX), Ok(()));
        assert_eq!(
            check_layout(&i, u64::from(u32::MAX) * 16384),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn two_planes_must_be_one_object_and_the_chroma_plane_must_fit() {
        let (host, reg) = setup();
        let (w, h) = (64u32, 64u32);
        let size = u64::from(w * h * 3 / 2);
        let nv12 = InjImport {
            nplanes: 2,
            width: w,
            height: h,
            fourcc: NV12,
            flags: 0,
            modifier: 0,
            offsets: [0, w * h, 0, 0],
            strides: [w, w, 0, 0],
        };
        let d = host.dmabuf(obj(size, 0));
        let d2 = host.same_object(&d);
        assert!(reg.import(1, &nv12, vec![d, d2]).is_ok());
        // Two objects for two planes.
        let (a, b) = (host.dmabuf(obj(size, 0)), host.dmabuf(obj(size, 0x100000)));
        assert_eq!(reg.import(1, &nv12, vec![a, b]), Err(libc::EINVAL));
        // A descriptor per plane, no more, no fewer.
        let d = host.dmabuf(obj(size, 0));
        assert_eq!(reg.import(1, &nv12, vec![d]), Err(libc::EINVAL));
        // The chroma plane past the end.
        let mut past = nv12;
        past.offsets[1] = w * h + 1;
        let d = host.dmabuf(obj(size, 0));
        let d2 = host.same_object(&d);
        assert_eq!(reg.import(1, &past, vec![d, d2]), Err(libc::EINVAL));
    }

    #[test]
    fn counts_and_bytes_are_bounded() {
        let host = Arc::new(FakeHost::new(1));
        let reg = Registry::with_limits(host.clone(), 3, 3 * 4096 * 4);
        for i in 0..3 {
            let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
            reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        }
        let d = host.dmabuf(obj(4096 * 4, 0x100000));
        assert_eq!(reg.import(1, &rgb(32, 32), vec![d]), Err(libc::ENOSPC));
        let reg = Registry::with_limits(host.clone(), 8, 2 * 4096 * 4);
        let d = host.dmabuf(obj(4096 * 4, 0));
        reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        let big = host.dmabuf(obj(2 * 4096 * 4, 0x10000));
        assert_eq!(reg.import(1, &rgb(32, 32), vec![big]), Err(libc::EDQUOT));
        assert_eq!(reg.bytes(), 4096 * 4);
    }

    #[test]
    fn release_is_the_importers_and_a_hangup_releases_everything_it_imported() {
        let (host, reg) = setup();
        let mut ids = Vec::new();
        for i in 0..3 {
            let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
            ids.push(reg.import(7, &rgb(32, 32), vec![d]).unwrap());
        }
        let d = host.dmabuf(obj(4096 * 4, 0x100000));
        let (other, other_tok) = reg.import(8, &rgb(32, 32), vec![d]).unwrap();
        assert_eq!(reg.release(8, ids[0].0), Err(libc::ENOENT));
        assert_eq!(reg.release(7, ids[0].0), Ok(()));
        assert_eq!(reg.open(ids[0].0, &ids[0].1, 0).unwrap_err(), libc::ENOENT);
        assert_eq!(reg.release_peer(7), 2);
        assert_eq!(reg.live(), 1);
        assert!(reg.open(other, &other_tok, 0).is_ok());
        assert_eq!(reg.bytes(), 4096 * 4);
    }

    #[test]
    fn one_dmabuf_injected_twice_is_one_handle_closed_with_the_last_id() {
        let (host, reg) = setup();
        let d = host.dmabuf(obj(4096 * 4, 0));
        let d2 = host.same_object(&d);
        let (a, _) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        let (b, tb) = reg.import(1, &rgb(32, 32), vec![d2]).unwrap();
        reg.release(1, a).unwrap();
        assert_eq!(host.closes(), 0, "the second id still holds the handle");
        assert!(reg.open(b, &tb, 0).is_ok());
        reg.release(1, b).unwrap();
        assert_eq!(host.closes(), 1);
    }

    #[test]
    fn opens_are_bounded_per_process_and_freed_by_close() {
        let mut bi = BackendInject::default();
        let p = |t| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        for g in 0..MAX_OPENS / 4 {
            bi.record(1, g as u32 + 1, 0, 4096, p(1), None).unwrap();
        }
        assert_eq!(bi.record(1, 9999, 0, 4096, p(1), None), Err(libc::EAGAIN));
        // The same handle again is not a second open.
        assert_eq!(bi.record(1, 1, 0, 4096, p(1), None), Ok(()));
        assert!(bi.record(2, 1, 0, 4096, p(2), None).is_ok());
        bi.gem_closed(1, 1);
        assert!(bi.record(1, 9999, 0, 4096, p(1), None).is_ok());
        bi.file_closed(1);
        assert_eq!(bi.opens(), 1);
        assert!(bi.read_only(0, 1));
        bi.file_closed(2);
        assert!(!bi.read_only(0, 1));
    }

    // ── INJECT_OPEN, through the dispatcher ──

    use crate::hostfd::HandleKind;
    use crate::nvidia::NvidiaBackend;
    use protocol::messages::*;

    fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
        let mut msg = crate::session::hdr(t, handle, 0, 0x77);
        msg.extend_from_slice(body);
        let mut resp = vec![0u8; 4096];
        let n = be.dispatch(&msg, &mut resp);
        resp.truncate(n);
        resp
    }

    fn status(resp: &[u8]) -> i32 {
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    }

    fn v2_backend(reg: Option<Arc<Registry>>) -> NvidiaBackend {
        let mut be = NvidiaBackend::for_test();
        be.set_inject(reg);
        let req = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID,
            uvm_aperture_mib: 0,
        };
        let r = call(&mut be, MsgType::Hello, 0, crate::sys::pod::bytes(&req));
        assert_eq!(status(&r), 0);
        be
    }

    /// INJECT_OPEN as guest process `tgid`: status, result words, and the
    /// description after them.
    fn inject_open(
        be: &mut NvidiaBackend,
        render: u32,
        id: u32,
        token: &[u8; 16],
        tgid: u32,
    ) -> (i32, [u64; OP_MAX_RES], Vec<u8>) {
        let mut req = HostOpReq {
            op: OP_INJECT_OPEN,
            nargs: 4,
            args: [0; OP_MAX_ARGS],
        };
        req.args[0] = u64::from(render);
        req.args[1] = u64::from(id);
        req.args[2] = u64::from_le_bytes(token[..8].try_into().unwrap());
        req.args[3] = u64::from_le_bytes(token[8..].try_into().unwrap());
        let mut body = crate::sys::pod::bytes(&req).to_vec();
        body.extend_from_slice(crate::sys::pod::bytes(&ProcId {
            start_ns: 1,
            tgid,
            euid: 1000,
        }));
        let r = call(be, MsgType::HostOp, 0, &body);
        let h = size_of::<MsgHeader>();
        let resp: HostOpResp = crate::sys::pod::read(&r, h).unwrap_or_default();
        (
            status(&r),
            resp.res,
            r.get(h + 40..).unwrap_or(&[]).to_vec(),
        )
    }

    #[derive(Clone, Default)]
    struct RecWindow(Arc<Mutex<Vec<(u64, bool)>>>);
    impl crate::shm::WindowPlacer for RecWindow {
        fn place(
            &self,
            off: u64,
            _len: u64,
            _fd: std::os::fd::RawFd,
            _fo: u64,
            w: bool,
        ) -> crate::error::Result<()> {
            self.0.lock().unwrap().push((off, w));
            Ok(())
        }
        fn withdraw(&self, _off: u64, _len: u64) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn mmap(be: &mut NvidiaBackend, handle: u32, offset: u64, size: u64) -> (i32, MmapResp) {
        let req = MmapReq {
            size,
            offset,
            prot: 3,
            padding: 0,
        };
        let r = call(be, MsgType::Mmap, handle, crate::sys::pod::bytes(&req));
        let resp = crate::sys::pod::read(&r, size_of::<MsgHeader>()).unwrap_or_default();
        (status(&r), resp)
    }

    #[test]
    fn a_guest_opens_an_injected_buffer_with_its_token_and_maps_it_read_only() {
        let host = Arc::new(FakeHost::new(1));
        let reg = Arc::new(Registry::new(host.clone()));
        let size = 32 * 32 * 4;
        let (id, token) = reg
            .import(1, &rgb(32, 32), vec![host.dmabuf(obj(size, 0x40_0000))])
            .unwrap();
        let mut be = v2_backend(Some(reg.clone()));
        let win = RecWindow::default();
        be.set_window(Box::new(win.clone()));
        let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));

        // Without the token, nothing: and nothing made in the file.
        let (st, _, _) = inject_open(&mut be, render, id, &[0; 16], 10);
        assert_eq!(st, -libc::ENOENT);
        assert_eq!(be.inject.opens(), 0);

        let (st, res, tail) = inject_open(&mut be, render, id, &token, 10);
        assert_eq!(st, 0);
        assert_ne!(res[0], 0, "a GEM handle");
        assert_eq!(res[1], size);
        assert_eq!(res[2], u64::from(NVKMS));
        assert_eq!(tail.len(), 64);
        assert_eq!(&tail[0..4], &32u32.to_le_bytes());
        assert_eq!(&tail[8..12], &XR24.to_le_bytes());
        assert_eq!(&tail[16..24], &MODIFIER.to_le_bytes());
        assert_eq!(&tail[40..44], &(32u32 * 4).to_le_bytes());
        assert_eq!(be.inject.opens(), 1);
        // The same open again is the same handle, one record.
        let (st, res2, _) = inject_open(&mut be, render, id, &token, 10);
        assert_eq!((st, res2[0]), (0, res[0]));
        assert_eq!(be.inject.opens(), 1);

        // Its mmap range is placed read-only, and the guest is told.
        let (st, m) = mmap(&mut be, render, 0x40_0000, size);
        assert_eq!(st, 0);
        assert_eq!(m.flags & MMAP_F_READ_ONLY, MMAP_F_READ_ONLY);
        assert_eq!(
            win.0.lock().unwrap().last(),
            Some(&(m.guest_phys_addr, false))
        );
        // Another object's is not.
        let (st, m) = mmap(&mut be, render, 0x80_0000, 4096);
        assert_eq!(st, 0);
        assert_eq!(m.flags & MMAP_F_READ_ONLY, 0);

        // After RELEASE, no new open; the guest's handle keeps the range
        // read-only while it is open.
        reg.release(1, id).unwrap();
        let other = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        assert_eq!(inject_open(&mut be, other, id, &token, 10).0, -libc::ENOENT);
        assert!(be.inject.read_only(0x40_0000, 4096));
        call(&mut be, MsgType::Close, render, &[]);
        assert_eq!(be.inject.opens(), 0);
        assert!(!be.inject.read_only(0x40_0000, 4096));
    }

    #[test]
    fn inject_open_is_refused_without_the_socket_on_other_files_and_other_devices() {
        let host = Arc::new(FakeHost::new(2));
        let reg = Arc::new(Registry::new(host.clone()));
        let (id, token) = reg
            .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
            .unwrap();
        // A backend without --inject-socket.
        let mut be = v2_backend(None);
        let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        assert_eq!(
            inject_open(&mut be, render, id, &token, 1).0,
            -libc::EOPNOTSUPP
        );
        // Another VM's backend, with an inject socket of its own.
        let mut other = v2_backend(Some(Arc::new(Registry::new(host.clone()))));
        let r = other.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        assert_eq!(inject_open(&mut other, r, id, &token, 1).0, -libc::ENOENT);

        let mut be = v2_backend(Some(reg));
        let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        let sync = be.adopt_for_test(host.not_dmabuf(), HandleKind::SyncFile);
        let second = be.adopt_for_test(host.render_file(1), HandleKind::DriRender(1));
        assert_eq!(inject_open(&mut be, sync, id, &token, 1).0, -libc::EBADF);
        assert_eq!(inject_open(&mut be, second, id, &token, 1).0, -libc::ENODEV);
        assert_eq!(inject_open(&mut be, render, id, &token, 1).0, 0);
    }

    #[test]
    fn hello_offers_inject_only_with_the_socket() {
        let caps = |reg| {
            let mut be = NvidiaBackend::for_test();
            be.set_inject(reg);
            let req = HelloReq {
                proto: PROTO_V2,
                flags: HELLO_F_FRESH,
                guest_caps: 0,
                uvm_aperture_mib: 0,
            };
            let r = call(&mut be, MsgType::Hello, 0, crate::sys::pod::bytes(&req));
            crate::sys::pod::read::<HelloResp>(&r, size_of::<MsgHeader>())
                .unwrap()
                .backend_caps
        };
        assert_eq!(caps(None) & BCAP_INJECT, 0);
        let reg = Arc::new(Registry::new(Arc::new(FakeHost::new(1))));
        assert_eq!(caps(Some(reg)) & BCAP_INJECT, BCAP_INJECT);
    }

    #[test]
    fn one_guest_process_cannot_take_every_open() {
        let host = Arc::new(FakeHost::new(1));
        let reg = Arc::new(Registry::new(host.clone()));
        let (id, token) = reg
            .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
            .unwrap();
        let mut be = v2_backend(Some(reg));
        let mut n = 0;
        loop {
            let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
            let (st, _, _) = inject_open(&mut be, r, id, &token, 10);
            if st != 0 {
                assert_eq!(st, -libc::EAGAIN);
                break;
            }
            n += 1;
        }
        assert_eq!(n, MAX_OPENS / 4);
        let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        assert_eq!(inject_open(&mut be, r, id, &token, 11).0, 0);
        // A refused open leaves no handle behind in the file.
        let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        assert_eq!(inject_open(&mut be, r, id, &token, 10).0, -libc::EAGAIN);
        let (fd, _) = be.handles.get(r).unwrap();
        assert_eq!(host.handles_in(fd), 0);
        // And a handle the file had before the call is left as it was.
        let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        let (fd, _) = be.handles.get(r).unwrap();
        let before = host.handles_in(fd);
        let opened = be.inject.registry().unwrap().open(id, &token, 0).unwrap();
        host.prime_import(fd, opened.dmabuf.as_fd()).unwrap();
        assert_eq!(host.handles_in(fd), before + 1);
        let closes = host.closes();
        assert_eq!(inject_open(&mut be, r, id, &token, 10).0, -libc::EAGAIN);
        assert_eq!(host.closes(), closes, "nothing closed in the caller's file");
    }

    /// nvidia-drm imports another NVIDIA device's buffer as a duplicate,
    /// an NVKMS object of its own device: IDENTIFY cannot tell. Only the
    /// device whose import is the helper's very object takes it.
    #[test]
    fn another_nvidia_devices_buffer_is_its_own_not_a_duplicates() {
        let host = Arc::new(FakeHost::new(2));
        let reg = Registry::new(host.clone());
        let (id, token) = reg
            .import(
                1,
                &rgb(32, 32),
                vec![host.dmabuf(Obj {
                    gpu: 1,
                    ..obj(4096 * 4, 0)
                })],
            )
            .unwrap();
        // Node 0's duplicate was made, refused and closed.
        assert_eq!(host.closes(), 1);
        assert_eq!(reg.open(id, &token, 0).unwrap_err(), libc::ENODEV);
        assert!(reg.open(id, &token, 1).is_ok());
        // With one GPU in the list, another GPU's buffer is nobody's.
        let h1 = Arc::new(FakeHost::new(1));
        let one = Registry::new(h1.clone());
        assert_eq!(
            one.import(
                1,
                &rgb(32, 32),
                vec![h1.dmabuf(Obj {
                    gpu: 1,
                    ..obj(4096 * 4, 0)
                })]
            ),
            Err(libc::ENODEV)
        );
    }

    /// No export hands out an injected buffer: HOST_OP PRIME_EXPORT of the
    /// handle INJECT_OPEN made is refused before the host is asked, and the
    /// dma-buf any other path would export is recognised (the taint set).
    #[test]
    fn an_injected_buffer_is_never_exported() {
        let host = Arc::new(FakeHost::new(1));
        let reg = Arc::new(Registry::new(host.clone()));
        let d = host.dmabuf(obj(4096 * 4, 0));
        let helper_copy = d.try_clone().unwrap();
        let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
        let mut be = v2_backend(Some(reg.clone()));
        let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        let (st, res, _) = inject_open(&mut be, render, id, &token, 10);
        assert_eq!(st, 0);
        let prime_export = |be: &mut NvidiaBackend, gem: u64| {
            let mut req = HostOpReq {
                op: OP_PRIME_EXPORT,
                nargs: 2,
                args: [0; OP_MAX_ARGS],
            };
            req.args[0] = u64::from(render);
            req.args[1] = gem;
            status(&call(be, MsgType::HostOp, 0, crate::sys::pod::bytes(&req)))
        };
        assert_eq!(prime_export(&mut be, res[0]), -libc::EINVAL);
        // Another handle reaches the host (a memfd: no ioctls).
        assert_eq!(prime_export(&mut be, res[0] + 100), -libc::ENOTTY);
        // The helper's dma-buf is tainted while the id or an open holds it.
        let taint = be.inject_taint.clone();
        assert!(!exportable(&taint, helper_copy.as_fd()));
        assert!(exportable(&taint, host.not_dmabuf().as_fd()));
        reg.release(1, id).unwrap();
        assert!(
            !exportable(&taint, helper_copy.as_fd()),
            "the open still holds it"
        );
        call(&mut be, MsgType::Close, render, &[]);
        assert!(exportable(&taint, helper_copy.as_fd()));
        assert!(reg.taint().is_empty());
    }

    #[test]
    fn a_syncobj_is_injected_opened_with_its_token_and_released() {
        let (host, reg) = setup();
        // Only a syncobj file, and exactly one.
        assert_eq!(
            reg.import_syncobj(1, 0, vec![host.not_dmabuf()]),
            Err(libc::EBADF)
        );
        assert_eq!(
            reg.import_syncobj(1, 0, Vec::<OwnedFd>::new()),
            Err(libc::EINVAL)
        );
        assert_eq!(
            reg.import_syncobj(1, 0, vec![host.syncobj(), host.syncobj()]),
            Err(libc::EINVAL)
        );
        assert_eq!(
            reg.import_syncobj(1, 1, vec![host.syncobj()]),
            Err(libc::EINVAL)
        );
        let (id, token) = reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
        assert!(reg.open_syncobj(id, &token).is_ok());
        let mut wrong = token;
        wrong[0] ^= 1;
        assert_eq!(reg.open_syncobj(id, &wrong).unwrap_err(), libc::ENOENT);
        // A syncobj id is no buffer's, and a buffer's no syncobj's.
        assert_eq!(reg.open(id, &token, 0).unwrap_err(), libc::ENOENT);
        let (bid, btok) = reg
            .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
            .unwrap();
        assert_ne!(bid, id);
        assert_eq!(reg.open_syncobj(bid, &btok).unwrap_err(), libc::ENOENT);
        // Only the importer releases it; its hangup does too.
        assert_eq!(reg.release(2, id), Err(libc::ENOENT));
        assert_eq!(reg.release(1, id), Ok(()));
        assert_eq!(reg.open_syncobj(id, &token).unwrap_err(), libc::ENOENT);
        reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
        assert_eq!(reg.release_peer(1), 2);
        assert_eq!(reg.syncobjs(), 0);
        // Bounded.
        for _ in 0..MAX_SYNCOBJS {
            reg.import_syncobj(3, 0, vec![host.syncobj()]).unwrap();
        }
        assert_eq!(
            reg.import_syncobj(3, 0, vec![host.syncobj()]),
            Err(libc::ENOSPC)
        );
    }

    #[test]
    fn a_guest_opens_an_injected_syncobj_into_its_render_file() {
        let host = Arc::new(FakeHost::new(1));
        let reg = Arc::new(Registry::new(host.clone()));
        let (id, token) = reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
        let mut be = v2_backend(Some(reg.clone()));
        let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        let open = |be: &mut NvidiaBackend, render: u32, token: &[u8; 16]| {
            let mut req = HostOpReq {
                op: OP_INJECT_OPEN_SYNCOBJ,
                nargs: 4,
                args: [0; OP_MAX_ARGS],
            };
            req.args[0] = u64::from(render);
            req.args[1] = u64::from(id);
            req.args[2] = u64::from_le_bytes(token[..8].try_into().unwrap());
            req.args[3] = u64::from_le_bytes(token[8..].try_into().unwrap());
            let r = call(be, MsgType::HostOp, 0, crate::sys::pod::bytes(&req));
            let resp: HostOpResp =
                crate::sys::pod::read(&r, size_of::<MsgHeader>()).unwrap_or_default();
            (status(&r), resp.res[0])
        };
        assert_eq!(open(&mut be, render, &[0; 16]).0, -libc::ENOENT);
        let (st, h) = open(&mut be, render, &token);
        assert_eq!(st, 0);
        assert_ne!(h, 0);
        let (fd, _) = be.handles.get(render).unwrap();
        assert_eq!(host.syncobjs_in(fd), 1);
        // The file is an importer now: its syncobjs' registrations are not
        // dropped by a DESTROY (fence.rs).
        assert!(!be.syncobj_regs.is_private_for_test(render, h as u32));
        // Not on a file that is not a render file.
        let sync = be.adopt_for_test(host.not_dmabuf(), HandleKind::SyncFile);
        assert_eq!(open(&mut be, sync, &token).0, -libc::EBADF);
    }

    // ── the socket ──

    /// A directory of this test's own (the tests run in parallel).
    fn tmpdir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "nvinject-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn server(uid: u32) -> (PathBuf, Arc<FakeHost>, InjectServer) {
        let dir = tmpdir();
        let path = dir.join("inject.sock");
        let host = Arc::new(FakeHost::new(1));
        let reg = Arc::new(Registry::new(host.clone()));
        let s = InjectServer::bind_idle(&path, uid, reg).unwrap();
        s.start().unwrap();
        (path, host, s)
    }

    fn roundtrip(c: &OwnedFd, req: &[u8], fds: &[std::os::fd::RawFd]) -> Option<InjReply> {
        crate::sys::net::send_packet(c.as_raw_fd(), req, fds).ok()?;
        let mut b = [0u8; 64];
        let p = crate::sys::net::recv_packet(c.as_raw_fd(), &mut b).ok()?;
        (p.len == INJ_REPLY_SIZE).then(|| InjReply::from_bytes(&b[..p.len]).unwrap())
    }

    fn hello_bytes() -> [u8; 16] {
        InjHello {
            version: INJ_VERSION,
            flags: 0,
        }
        .to_bytes()
    }

    #[test]
    fn the_socket_is_private_and_speaks_hello_import_release() {
        let (path, host, s) = server(crate::sys::proc::uid());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        // Nothing before HELLO.
        let d = host.dmabuf(obj(4096 * 4, 0));
        assert!(roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()]).is_none());
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        let h = roundtrip(&c, &hello_bytes(), &[]).unwrap();
        assert_eq!((h.status, h.version), (0, INJ_VERSION));
        assert_eq!(h.max_buffers as usize, MAX_BUFFERS);
        assert_eq!(h.max_syncobjs as usize, MAX_SYNCOBJS);
        assert_eq!(h.max_bytes, MAX_BYTES);
        let r = roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()]).unwrap();
        assert_eq!((r.op, r.status), (INJ_OP_IMPORT, 0));
        assert_ne!(r.id, 0);
        assert!(s.registry().open(r.id, &r.token, 0).is_ok());
        // A refusal keeps the connection.
        let bad = roundtrip(
            &c,
            &rgb(32, 32).to_bytes(),
            &[host.not_dmabuf().as_raw_fd()],
        );
        assert_eq!(bad.unwrap().status, -libc::EBADF);
        let rel = roundtrip(&c, &InjRelease { id: r.id }.to_bytes(), &[]).unwrap();
        assert_eq!(rel.status, 0);
        assert_eq!(s.registry().live(), 0);
        let rel = roundtrip(&c, &InjRelease { id: r.id }.to_bytes(), &[]).unwrap();
        assert_eq!(rel.status, -libc::ENOENT);
        s.shutdown();
    }

    #[test]
    fn a_hangup_releases_what_the_helper_imported() {
        let (path, host, s) = server(crate::sys::proc::uid());
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        roundtrip(&c, &hello_bytes(), &[]).unwrap();
        for i in 0..3 {
            let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
            assert_eq!(
                roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()])
                    .unwrap()
                    .status,
                0
            );
        }
        assert_eq!(s.registry().live(), 3);
        drop(c);
        for _ in 0..200 {
            if s.registry().live() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(s.registry().live(), 0);
        s.shutdown();
    }

    #[test]
    fn a_peer_of_another_uid_is_hung_up_on() {
        let (path, _host, s) = server(crate::sys::proc::uid().wrapping_add(1));
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        assert!(roundtrip(&c, &hello_bytes(), &[]).is_none());
        s.shutdown();
    }

    #[test]
    fn malformed_packets_end_the_connection() {
        let (path, host, s) = server(crate::sys::proc::uid());
        let send = |bytes: &[u8], fds: &[std::os::fd::RawFd]| {
            let c = crate::sys::net::seqpacket_connect(&path).unwrap();
            roundtrip(&c, &hello_bytes(), &[]).unwrap();
            roundtrip(&c, bytes, fds)
        };
        let d = host.dmabuf(obj(4096 * 4, 0));
        // A short IMPORT, an unknown op, descriptors on RELEASE, a second
        // HELLO, an oversized packet.
        assert!(send(&rgb(32, 32).to_bytes()[..60], &[d.as_raw_fd()]).is_none());
        assert!(send(&[9, 0, 0, 0, 0, 0, 0, 0], &[]).is_none());
        assert!(send(&InjRelease { id: 1 }.to_bytes(), &[d.as_raw_fd()]).is_none());
        assert_eq!(send(&hello_bytes(), &[]).unwrap().status, -libc::EPROTO);
        assert!(send(&[2u8; 80], &[]).is_none());
        // More descriptors than planes: refused, the connection kept.
        let d2 = host.dmabuf(obj(4096 * 4, 0x10000));
        assert_eq!(
            send(&rgb(32, 32).to_bytes(), &[d.as_raw_fd(), d2.as_raw_fd()])
                .unwrap()
                .status,
            -libc::EINVAL
        );
        // A wrong version.
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        let v2 = InjHello {
            version: 2,
            flags: 0,
        };
        assert_eq!(
            roundtrip(&c, &v2.to_bytes(), &[]).unwrap().status,
            -libc::EPROTO
        );
        s.shutdown();
    }

    #[test]
    fn no_more_than_max_peers_at_once() {
        let (path, _host, s) = server(crate::sys::proc::uid());
        let conns: Vec<_> = (0..MAX_PEERS)
            .map(|_| {
                let c = crate::sys::net::seqpacket_connect(&path).unwrap();
                roundtrip(&c, &hello_bytes(), &[]).unwrap();
                c
            })
            .collect();
        let extra = crate::sys::net::seqpacket_connect(&path).unwrap();
        assert!(roundtrip(&extra, &hello_bytes(), &[]).is_none());
        drop(conns);
        s.shutdown();
    }

    #[test]
    fn only_a_socket_of_our_own_is_replaced() {
        assert!(may_replace(true, 5, 5).is_ok());
        assert!(may_replace(true, 6, 5).is_err());
        assert!(may_replace(false, 5, 5).is_err());
    }
}
