// SPDX-License-Identifier: Apache-2.0
//! What a host descriptor is, and helper operations on them (protocol v2).
//!
//! Every descriptor the backend holds lives in the handle table under a
//! handle, and every handle has a kind. The kind decides which messages may
//! name it: a v1 IOCTL only reaches `Dev` and render handles, a KMS schema only
//! runs on `DrmCard`/`DrmLease`, a fence watch only on `SyncFile`. Anything the
//! guest cannot use is `Other`, and nothing can be done with it.
//!
//! Classification of a descriptor that arrives from outside (an ioctl's fd out,
//! a Wayland message) is by what the kernel says it is, never by what the guest
//! or the compositor claimed: `fstat` for DRM nodes (and the node must be an
//! nvidia-drm card node of our own GPU), `/proc/self/fd` for the anonymous
//! inodes.

#![forbid(unsafe_code)]

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use protocol::messages::*;

use crate::fence::RegKey;
use crate::pump::WatchMode;

/// The kind of a backend handle. Wire value in `HK_*` (protocol::messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandleKind {
    /// One of the NVIDIA character devices, opened by the guest.
    Dev(DeviceKind),
    /// A host render node (`renderD*`), by its index in the DRI list.
    DriRender(u32),
    /// A host card node the backend opened for a guest file (compositor-VM).
    DrmCard(u32),
    /// Any other nvidia-drm card-node file of our GPU: a lease, the drm_fd of
    /// a lease device, a CREATE_LEASE result. By card index.
    DrmLease(u32),
    SyncFile,
    Syncobj,
    Dmabuf,
    Eventfd,
    Memfd,
    /// A connection to the host Wayland compositor.
    Wayland,
    /// Anything else. Held (so it can be closed) but never usable.
    Other,
}

impl HandleKind {
    /// The `HK_*` wire value.
    pub fn wire(self) -> u32 {
        match self {
            Self::Dev(_) => HK_DEV,
            Self::DriRender(_) => HK_DRI_RENDER,
            Self::DrmCard(_) => HK_DRM_CARD,
            Self::DrmLease(_) => HK_DRM_LEASE,
            Self::SyncFile => HK_SYNC_FILE,
            Self::Syncobj => HK_SYNCOBJ,
            Self::Dmabuf => HK_DMABUF,
            Self::Eventfd => HK_EVENTFD,
            Self::Memfd => HK_MEMFD,
            Self::Wayland => HK_WAYLAND,
            Self::Other => HK_OTHER,
        }
    }

    /// A host file KMS ioctls may run on.
    pub fn is_kms(self) -> bool {
        matches!(self, Self::DrmCard(_) | Self::DrmLease(_))
    }

    /// Bit for schema `kinds` masks: `1 << wire()`.
    pub fn mask_bit(self) -> u32 {
        1u32 << self.wire()
    }

    /// The DRI or card index a DRM kind carries.
    pub fn index(self) -> Option<u32> {
        match self {
            Self::DriRender(i) | Self::DrmCard(i) | Self::DrmLease(i) => Some(i),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Host card nodes
// ---------------------------------------------------------------------------

/// A host card (primary) node of one of our GPUs.
///
/// Enumerated once, from `/sys/bus/pci/devices/<addr>/drm`, for two reasons:
/// compositor-VM mode offers these to the guest (GET_SYS_FILES section 3,
/// HOST_OP OPEN_KMS), and classification needs them in every mode, because a
/// lease fd that arrives over Wayland is only ours if its `st_rdev` is one of
/// these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardNode {
    /// `card1`, as it appears under `/dev/dri`.
    pub name: String,
    pub major: u32,
    pub minor: u32,
    /// Index of the render node on the same PCI device in the DRI list the
    /// guest was given, so it attaches the card to the same `drm_device`.
    pub render_index: u32,
}

impl CardNode {
    pub fn path(&self) -> String {
        format!("/dev/dri/{}", self.name)
    }
}

// ---------------------------------------------------------------------------
// ioctl numbers
// ---------------------------------------------------------------------------

/// `_IOC(dir, type, nr, size)` from include/uapi/asm-generic/ioctl.h.
pub const fn ioc(dir: u32, ty: u8, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr
}
pub const IOC_W: u32 = 1;
pub const IOC_R: u32 = 2;
pub const IOC_RW: u32 = IOC_W | IOC_R;

/// `_IOC_SIZE(cmd)`: how many bytes the kernel copies in and out of the
/// argument. DRM, RM and NVKMS all size their copies by it, whatever the
/// caller actually allocated.
pub const fn ioc_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}
pub const fn ioc_type(cmd: u32) -> u8 {
    (cmd >> 8) as u8
}
pub const fn ioc_nr(cmd: u32) -> u32 {
    cmd & 0xff
}

/// The DRM character-device major (include/uapi/drm/drm.h via drm_drv.c).
pub const DRM_MAJOR: u32 = 226;
/// Minors below this are primary (card) nodes; render nodes start at 128
/// (drm_drv.c `drm_minor_alloc`, DRM_MINOR_RENDER * 64 in older kernels and
/// the 128 base since).
pub const DRM_PRIMARY_MINOR_LIMIT: u32 = 128;

/// `struct drm_version` is 64 bytes on LP64 (include/uapi/drm/drm.h).
pub const DRM_IOCTL_VERSION: u32 = ioc(IOC_RW, b'd', 0x00, 64);
pub const DRM_IOCTL_GEM_CLOSE: u32 = ioc(IOC_W, b'd', 0x09, 8);
pub const DRM_IOCTL_GEM_FLINK: u32 = ioc(IOC_RW, b'd', 0x0a, 8);
pub const DRM_IOCTL_GEM_OPEN: u32 = ioc(IOC_RW, b'd', 0x0b, 16);
pub const DRM_IOCTL_DROP_MASTER: u32 = ioc(0, b'd', 0x1f, 0);
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = ioc(IOC_RW, b'd', 0x2d, 12);
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = ioc(IOC_RW, b'd', 0x2e, 12);
pub const DRM_IOCTL_SYNCOBJ_CREATE: u32 = ioc(IOC_RW, b'd', 0xbf, 8);
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u32 = ioc(IOC_RW, b'd', 0xc0, 8);
pub const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u32 = ioc(IOC_RW, b'd', 0xc1, 24);
/// nvidia-drm's GEM_IMPORT_USERSPACE_MEMORY, absolute nr 0x42. See
/// [`refused_everywhere`].
pub const DRM_NVIDIA_GEM_IMPORT_USERSPACE_MEMORY_NR: u32 = 0x42;

/// DRM commands refused on every handle, whatever the path.
///
/// nvidia-drm's GEM_IMPORT_USERSPACE_MEMORY wraps the calling process's memory
/// at an address the caller names -- in this process, the backend's own heap.
/// GEM_FLINK and GEM_OPEN trade in global names, which would let a guest open
/// objects belonging to any file on the host device, the host compositor's
/// included.
pub fn refused_everywhere(cmd: u32) -> bool {
    ioc_type(cmd) == b'd'
        && [
            DRM_NVIDIA_GEM_IMPORT_USERSPACE_MEMORY_NR,
            ioc_nr(DRM_IOCTL_GEM_FLINK),
            ioc_nr(DRM_IOCTL_GEM_OPEN),
        ]
        .contains(&ioc_nr(cmd))
}

/// `DRM_CLOEXEC | DRM_RDWR` (drm.h:912-913).
const DRM_PRIME_FLAGS: u32 = (libc::O_CLOEXEC | libc::O_RDWR) as u32;
const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1;
const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;

/// include/uapi/linux/sync_file.h:109-110.
pub const SYNC_IOC_MERGE: u32 = ioc(IOC_RW, b'>', 3, 48);
pub const SYNC_IOC_FILE_INFO: u32 = ioc(IOC_RW, b'>', 4, 56);

/// include/uapi/linux/dma-buf.h:180, and include/uapi/linux/udmabuf.h.
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u32 = ioc(IOC_RW, b'b', 2, 8);
const DMA_BUF_SYNC_RW: u32 = 3;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

use crate::sys::block::{Arena, BufId};
use crate::sys::ioctl::Host;

/// One call the backend makes itself: `bytes` (the whole argument, of the
/// size the command copies) with the fields at `ptrs` declared pointers --
/// null unless `fill` points them -- and the field at `fd_out` (width 4)
/// one the host writes a new descriptor into. The arena, for the answer.
fn call(
    fd: RawFd,
    cmd: u32,
    bytes: &[u8],
    ptrs: &[usize],
    fd_out: Option<usize>,
    fill: impl FnOnce(&mut Arena, BufId) -> io::Result<()>,
) -> io::Result<(Arena, BufId)> {
    let mut a = Arena::new();
    let top = a.small(bytes);
    let bad = |_| io::Error::from_raw_os_error(libc::EINVAL);
    for &p in ptrs {
        a.ptr(top, p).map_err(bad)?;
    }
    if let Some(o) = fd_out {
        a.fd_out(top, o, 4).map_err(bad)?;
    }
    fill(&mut a, top)?;
    let r = a.call(&Host, fd, u64::from(cmd), top);
    if r < 0 {
        return Err(io::Error::from_raw_os_error(-r));
    }
    Ok((a, top))
}

/// A call whose argument is data only.
fn flat(fd: RawFd, cmd: u32, bytes: &mut [u8]) -> io::Result<()> {
    let (a, top) = call(fd, cmd, bytes, &[], None, |_, _| Ok(()))?;
    bytes.copy_from_slice(a.bytes(top));
    Ok(())
}

/// A call whose answer is a new descriptor at `fd_out`.
fn making_fd(fd: RawFd, cmd: u32, bytes: &[u8], fd_out: usize) -> io::Result<OwnedFd> {
    let (mut a, top) = call(fd, cmd, bytes, &[], Some(fd_out), |_, _| Ok(()))?;
    a.claim_fd(top, fd_out)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// What an adopted descriptor is.
///
/// A DRM node is a `DrmLease` only if the kernel says it is a primary node
/// (major 226, minor < 128), the driver behind it says it is nvidia-drm, and
/// its `st_rdev` is one of our own GPU's card nodes. All three matter on a
/// hybrid host: Hyprland creates a `wp_drm_lease_device_v1` per DRM backend,
/// so the proxy receives the iGPU's drm_fd and lease fds too, and the same
/// ioctl numbers mean different things to i915 -- running nvidia-drm's
/// GEM_IDENTIFY_OBJECT on one would be running something else entirely.
///
/// Everything else is named by the filesystem it is on (`fstatfs`'s
/// `f_type`, which only the kernel sets) and, within that, by
/// `/proc/self/fd`: `anon_inode:<name>` for anonymous inodes
/// (fs/anon_inodes.c:74-78 prints the name given to `anon_inode_getfile`),
/// `/dmabuf:<name>` on the dma-buf filesystem (drivers/dma-buf/dma-buf.c:
/// 150-164, `dmabuffs_dname`) and `/memfd:<name> (deleted)` on shmem or
/// hugetlbfs for memfds. The link text alone is a path: a process of the
/// backend's uid with a mount namespace of its own -- an export-mode peer --
/// could hand over a FUSE file at `/dmabuf:x`, and the backend's first read
/// of it would wait on that process (review 2026-09-26, backend 13). A
/// descriptor that is none of these is `Other`: the guest may hold it and
/// close it, never use it.
pub fn classify(fd: BorrowedFd<'_>, cards: &[CardNode]) -> HandleKind {
    let raw = fd.as_raw_fd();
    let Ok(st) = crate::sys::fd::fstat(raw) else {
        return HandleKind::Other;
    };
    if st.st_mode & libc::S_IFMT == libc::S_IFCHR {
        let (major, minor) = (libc::major(st.st_rdev), libc::minor(st.st_rdev));
        if major == DRM_MAJOR && minor < DRM_PRIMARY_MINOR_LIMIT {
            if let Some(i) = cards
                .iter()
                .position(|c| c.major == major && c.minor == minor)
            {
                if drm_driver_name(raw).as_deref() == Some("nvidia-drm") {
                    return HandleKind::DrmLease(i as u32);
                }
            }
        }
        // Any other character device -- another GPU's node, a render node
        // from outside, a tty -- is nothing the guest may reach through us.
        return HandleKind::Other;
    }
    let Ok(fs) = crate::sys::fd::fstatfs_type(raw) else {
        return HandleKind::Other;
    };
    let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{raw}")) else {
        return HandleKind::Other;
    };
    kind_from_link(fs, &target.to_string_lossy())
}

/// Filesystem magic numbers (include/uapi/linux/magic.h).
const ANON_INODE_FS_MAGIC: i64 = 0x0904_1934;
const DMA_BUF_MAGIC: i64 = 0x444d_4142;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const HUGETLBFS_MAGIC: i64 = 0x9584_58f6;

/// The rest of [`classify`]: the filesystem `fs` a file is on, and within
/// it the `/proc/self/fd` link text.
fn kind_from_link(fs: i64, link: &str) -> HandleKind {
    match (fs, link) {
        (ANON_INODE_FS_MAGIC, "anon_inode:sync_file") => HandleKind::SyncFile,
        // drm_syncobj.c:673 names the file "syncobj_file"; the bare name is
        // accepted in case a kernel ever shortens it.
        (ANON_INODE_FS_MAGIC, "anon_inode:syncobj_file" | "anon_inode:syncobj") => {
            HandleKind::Syncobj
        }
        // A kernel before the dma-buf filesystem.
        (ANON_INODE_FS_MAGIC, "anon_inode:dmabuf") => HandleKind::Dmabuf,
        (ANON_INODE_FS_MAGIC, "anon_inode:[eventfd]") => HandleKind::Eventfd,
        (DMA_BUF_MAGIC, l) if l.starts_with("/dmabuf:") => HandleKind::Dmabuf,
        (TMPFS_MAGIC | HUGETLBFS_MAGIC, l) if l.starts_with("/memfd:") => HandleKind::Memfd,
        _ => HandleKind::Other,
    }
}

/// The driver name `DRM_IOCTL_VERSION` reports, e.g. "nvidia-drm".
///
/// VERSION is allowed on every DRM file, lessees included, and never blocks.
pub fn drm_driver_name(fd: RawFd) -> Option<String> {
    const NAME: usize = 32;
    // struct drm_version: three ints, a pad, then (len, ptr) for name, date
    // and desc. Only the name is asked for; the zero lengths make the kernel
    // skip the other two copies.
    let mut v = [0u8; 64];
    v[16..24].copy_from_slice(&(NAME as u64).to_le_bytes());
    let mut name = None;
    let (a, top) = call(fd, DRM_IOCTL_VERSION, &v, &[24, 40, 56], None, |a, top| {
        let n = a.small(&[0; NAME]);
        name = Some(n);
        a.point(top, 24, n)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    })
    .ok()?;
    let len = u64::from_le_bytes(a.bytes(top)[16..24].try_into().unwrap()) as usize;
    let len = len.min(NAME);
    Some(String::from_utf8_lossy(&a.bytes(name?)[..len]).into_owned())
}

/// Set `O_NONBLOCK` on a DRM card or lease file.
///
/// The pump reads DRM events from these, and `drm_read` on a blocking file
/// with an empty queue sleeps (drm_file.c:563-576): one lease fd adopted from
/// a compositor that created it without O_NONBLOCK -- aquamarine does -- would
/// park the pump and with it every event for the VM. DRM ioctls ignore
/// `f_flags`, so the flag changes nothing else.
///
/// Never applied to nvidia-modeset or the nvidia character devices:
/// `nvkms_poll` skips `poll_wait` entirely when the file is O_NONBLOCK
/// (nvidia-modeset-linux.c:2023-2025), so epoll would never be woken.
pub fn set_nonblock(fd: RawFd) -> io::Result<()> {
    crate::sys::fd::set_nonblock(fd)
}

// ---------------------------------------------------------------------------
// WATCH
// ---------------------------------------------------------------------------

/// Check a WATCH request against the handle's kind.
///
/// Each watch type does something to the descriptor, and each is only safe on
/// the kind it was made for. `W_DRM` makes the pump `read()` the file and ship
/// the bytes: on a Wayland socket that steals protocol, on an eventfd it eats
/// the counter, on a compositor's keymap memfd it hands the guest host data it
/// was never sent. `W_FENCE` issues SYNC_IOC_FILE_INFO, which means something
/// else on any other file. So: `W_DRM` only on card/lease files, `W_FENCE`
/// only on sync_files, `W_READY` only on eventfds, devices and Wayland
/// channels. Exactly one type per watch.
pub fn watch_mode(kind: HandleKind, flags: u32, cookie: u64) -> Result<WatchMode, i32> {
    const TYPES: u32 = W_FENCE | W_DRM | W_READY;
    if flags & !(TYPES | W_ONESHOT) != 0 || (flags & TYPES).count_ones() != 1 {
        return Err(libc::EINVAL);
    }
    let oneshot = flags & W_ONESHOT != 0;
    match flags & TYPES {
        // A DRM file produces events for as long as it lives; a one-shot
        // watch on one would silently stop delivering flips.
        W_DRM if kind.is_kms() && !oneshot => Ok(WatchMode::Drm),
        // A sync_file signals once, so a fence watch is one-shot whatever the
        // flag says.
        W_FENCE if kind == HandleKind::SyncFile => Ok(WatchMode::Fence { cookie }),
        W_READY
            if matches!(
                kind,
                HandleKind::Eventfd | HandleKind::Dev(_) | HandleKind::Wayland
            ) =>
        {
            Ok(WatchMode::Ready {
                cookie,
                oneshot,
                // Only an eventfd is ours to drain: its counter means nothing
                // to anyone but the watch, and leaving it set would make the
                // level sweep report it forever.
                consume: kind == HandleKind::Eventfd,
            })
        }
        _ => Err(libc::EINVAL),
    }
}

// ---------------------------------------------------------------------------
// HOST_OP
// ---------------------------------------------------------------------------

/// A HOST_OP request, with every argument checked against the kind it must
/// be. Handles are u32 on the wire; an argument that does not fit one is
/// refused rather than truncated into some other handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostOp {
    PrimeExport { file: u32, gem: u32 },
    DmabufImport { file: u32, dmabuf: u32 },
    SyncMerge { fences: Vec<u32> },
    NewEventfd,
    FdKind { handle: u32 },
    SignaledSyncFile,
    OpenKms { render: u32, card: u32 },
    DropIfMaster { card: u32 },
    CloseMany { handles: Vec<u32> },
    SyncobjWatch { key: RegKey, cookie: u64 },
}

/// Most handles SYNC_MERGE and CLOSE_MANY take: `args[0]` is the count and
/// the rest of the six slots are handles.
pub const OP_MAX_HANDLES: usize = OP_MAX_ARGS - 1;

/// Parse and type-check a HOST_OP.
///
/// `kind_of` looks a handle up in the session's table. `cards` is the card
/// list when compositor-VM mode is on and `None` otherwise: OPEN_KMS is
/// refused outright without it, so a guest cannot make a desktop host's
/// backend open the card its compositor is master of.
///
/// Errors: EINVAL for a malformed request (wrong argument count, unknown op),
/// EBADF for a handle that does not exist or is the wrong kind, EOPNOTSUPP for
/// OPEN_KMS without `--kms-card`, ENODEV for a card that does not exist.
pub fn check_host_op(
    req: &HostOpReq,
    kind_of: &dyn Fn(u32) -> Option<HandleKind>,
    cards: Option<&[CardNode]>,
) -> Result<HostOp, i32> {
    let nargs = req.nargs as usize;
    if nargs > OP_MAX_ARGS {
        return Err(libc::EINVAL);
    }
    let args = &req.args[..nargs];
    let want = |n: usize| {
        if nargs == n {
            Ok(())
        } else {
            Err(libc::EINVAL)
        }
    };
    let handle = |v: u64| u32::try_from(v).map_err(|_| libc::EBADF);
    let of_kind = |v: u64, ok: &dyn Fn(HandleKind) -> bool| -> Result<(u32, HandleKind), i32> {
        let h = handle(v)?;
        match kind_of(h) {
            Some(k) if ok(k) => Ok((h, k)),
            _ => Err(libc::EBADF),
        }
    };
    let is_render = |k: HandleKind| matches!(k, HandleKind::DriRender(_));
    // A counted list: args[0] = n, then n handles.
    let counted = || -> Result<Vec<u64>, i32> {
        let n = *args.first().ok_or(libc::EINVAL)? as usize;
        if n == 0 || n > OP_MAX_HANDLES || nargs != n + 1 {
            return Err(libc::EINVAL);
        }
        Ok(args[1..].to_vec())
    };

    match req.op {
        OP_PRIME_EXPORT => {
            want(2)?;
            let (file, _) = of_kind(args[0], &is_render)?;
            let gem = u32::try_from(args[1]).map_err(|_| libc::EINVAL)?;
            Ok(HostOp::PrimeExport { file, gem })
        }
        OP_DMABUF_IMPORT => {
            want(2)?;
            let (file, _) = of_kind(args[0], &is_render)?;
            let (dmabuf, _) = of_kind(args[1], &|k| k == HandleKind::Dmabuf)?;
            Ok(HostOp::DmabufImport { file, dmabuf })
        }
        OP_SYNC_MERGE => {
            let fences = counted()?
                .into_iter()
                .map(|v| of_kind(v, &|k| k == HandleKind::SyncFile).map(|(h, _)| h))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(HostOp::SyncMerge { fences })
        }
        OP_NEW_EVENTFD => want(0).map(|_| HostOp::NewEventfd),
        OP_FD_KIND => {
            want(1)?;
            let (h, _) = of_kind(args[0], &|_| true)?;
            Ok(HostOp::FdKind { handle: h })
        }
        OP_SIGNALED_SYNC_FILE => want(0).map(|_| HostOp::SignaledSyncFile),
        OP_OPEN_KMS => {
            want(2)?;
            let cards = cards.ok_or(libc::EOPNOTSUPP)?;
            let (render, kind) = of_kind(args[0], &is_render)?;
            let card = u32::try_from(args[1]).map_err(|_| libc::ENODEV)?;
            let node = cards.get(card as usize).ok_or(libc::ENODEV)?;
            // The card must be the same DRM device as the file's render node:
            // a guest file is one device, and the KMS handle it gets must not
            // be some other GPU's.
            if Some(node.render_index) != kind.index() {
                return Err(libc::EINVAL);
            }
            Ok(HostOp::OpenKms { render, card })
        }
        OP_DROP_IF_MASTER => {
            want(1)?;
            let (card, _) = of_kind(args[0], &|k| matches!(k, HandleKind::DrmCard(_)))?;
            Ok(HostOp::DropIfMaster { card })
        }
        OP_CLOSE_MANY => {
            let handles = counted()?
                .into_iter()
                .map(handle)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(HostOp::CloseMany { handles })
        }
        // A shared, capped syncobj wait registration (fence.rs): not a
        // descriptor operation, but checked here like every other op.
        OP_SYNCOBJ_WATCH => {
            want(5)?;
            let (render, _) = of_kind(args[0], &is_render)?;
            let syncobj = u32::try_from(args[1]).map_err(|_| libc::EINVAL)?;
            let flags = u32::try_from(args[3]).map_err(|_| libc::EINVAL)?;
            let key = RegKey {
                render,
                syncobj,
                point: args[2],
                flags,
            };
            Ok(HostOp::SyncobjWatch {
                key,
                cookie: args[4],
            })
        }
        _ => Err(libc::EINVAL),
    }
}

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD` with `DRM_CLOEXEC | DRM_RDWR`.
///
/// The dmabuf is an ordinary `drm_gem_prime_export` one (nvidia-drm-drv.c
/// sets `.gem_prime_export = drm_gem_prime_export`), so a host compositor on
/// the same device imports it as its own object.
pub fn prime_export(render: RawFd, gem: u32) -> io::Result<OwnedFd> {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&gem.to_le_bytes());
    p[4..8].copy_from_slice(&DRM_PRIME_FLAGS.to_le_bytes());
    making_fd(render, DRM_IOCTL_PRIME_HANDLE_TO_FD, &p, 8)
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: the GEM handle `dmabuf` has in `render`.
/// The flags field is ignored by the kernel on this direction (drm_prime.c,
/// `drm_prime_fd_to_handle_ioctl`), and is filled the same way for symmetry.
pub fn prime_import(render: RawFd, dmabuf: RawFd) -> io::Result<u32> {
    let mut p = [0u8; 12];
    p[4..8].copy_from_slice(&DRM_PRIME_FLAGS.to_le_bytes());
    p[8..12].copy_from_slice(&dmabuf.to_le_bytes());
    flat(render, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut p)?;
    Ok(u32::from_le_bytes(p[0..4].try_into().unwrap()))
}

/// `DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT`: `{u32 handle; u32 object_type}`.
pub const DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT: u32 = ioc(IOC_RW, b'd', 0x4e, 8);

/// `enum drm_nvidia_gem_object_type` (nv_drm_common_ioctl.h:316-322).
pub const NV_GEM_OBJECT_NVKMS: u32 = 0;
pub const NV_GEM_OBJECT_DMABUF: u32 = 1;
pub const NV_GEM_OBJECT_USERMEMORY: u32 = 2;
pub const NV_GEM_OBJECT_UNKNOWN: u32 = 0x7fff_ffff;

/// `DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT` on `gem` in `render`: what kind of
/// nvidia-drm object it is.
pub fn gem_identify(render: RawFd, gem: u32) -> io::Result<u32> {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&gem.to_le_bytes());
    flat(render, DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT, &mut p)?;
    Ok(u32::from_le_bytes(p[4..8].try_into().unwrap()))
}

/// The object type a DMABUF_IMPORT reports, from IDENTIFY on the imported
/// handle, or the errno that fails the import.
///
/// A PRIME import into nvidia-drm yields one of three kinds of object: the
/// exporter's own NVKMS memory when the dma-buf came from this device (the
/// self-import fast path), and otherwise a dma-buf-backed object
/// (nvidia-drm-gem-dma-buf.c:134-163) -- a host iGPU's buffer, a udmabuf, a
/// v4l2 frame -- or, for a re-imported user-memory export, that. The guest's
/// proxy used to call every one of them NVKMS, so the guest compositor's
/// EXPORT_NVKMS_MEMORY, which refuses a dma-buf object
/// (nvidia-drm-gem-nvkms-memory.c:595-603), failed where on bare metal it
/// never tries. UNKNOWN is a handle that is none of those, which an import
/// cannot legitimately produce: refused (EINVAL). IDENTIFY itself refuses
/// with EOPNOTSUPP on a node without DRIVER_MODESET (nvidia-drm-gem.c:318-320,
/// nvidia_drm.modeset=0), where no NVKMS object can exist -- they need the
/// NVKMS device -- so the import is a dma-buf object.
pub fn import_type(identified: io::Result<u32>) -> Result<u32, i32> {
    match identified {
        Ok(t @ (NV_GEM_OBJECT_NVKMS | NV_GEM_OBJECT_DMABUF | NV_GEM_OBJECT_USERMEMORY)) => Ok(t),
        Ok(t) => {
            log::warn!("DMABUF_IMPORT: the imported object identifies as {t:#x}; refused");
            Err(libc::EINVAL)
        }
        Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => Ok(NV_GEM_OBJECT_DMABUF),
        Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

/// `DRM_IOCTL_GEM_CLOSE`, for undoing an import whose reply cannot be sent.
pub fn gem_close(file: RawFd, gem: u32) -> io::Result<()> {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&gem.to_le_bytes());
    flat(file, DRM_IOCTL_GEM_CLOSE, &mut p)
}

/// A dmabuf's size. `dma_buf_llseek` answers SEEK_END with the size and
/// refuses everything but offset 0.
pub fn dmabuf_size(fd: RawFd) -> io::Result<u64> {
    crate::sys::fd::size(fd)
}

/// `SYNC_IOC_MERGE`: a new sync_file that signals when both inputs have.
pub fn sync_merge(a: RawFd, b: RawFd) -> io::Result<OwnedFd> {
    let mut p = [0u8; 48];
    p[..11].copy_from_slice(b"nvgpu-merge");
    p[32..36].copy_from_slice(&b.to_le_bytes());
    making_fd(a, SYNC_IOC_MERGE, &p, 36)
}

/// A sync_file's status: 1 signalled, 0 active, negative on error
/// (sync_file.h:57).
pub fn sync_file_status(fd: RawFd) -> io::Result<i32> {
    // num_fences 0 asks for the count only, so no pointer is followed.
    let (a, top) = call(fd, SYNC_IOC_FILE_INFO, &[0; 56], &[48], None, |_, _| Ok(()))?;
    Ok(i32::from_le_bytes(a.bytes(top)[32..36].try_into().unwrap()))
}

/// `struct sync_fence_info` (sync_file.h:46-52): obj_name[32],
/// driver_name[32], s32 status, u32 flags, u64 timestamp_ns.
const SYNC_FENCE_INFO_SIZE: usize = 80;
const SYNC_FENCE_INFO_STATUS: usize = 64;
const SYNC_FENCE_INFO_TIMESTAMP: usize = 72;

/// More fences than a sync_file is asked about one by one. A merge of more
/// is still reported, only without a timestamp.
const SYNC_FENCE_INFO_MAX: usize = 64;

/// A sync_file's status (as [`sync_file_status`]) and, once it has
/// signalled, when: the latest of its fences' signal times, in
/// `CLOCK_MONOTONIC` ns (`dma_fence_timestamp`, sync_file.c:268-292), or 0
/// where the kernel gave none.
///
/// The guest proxy signals with this time rather than when the record
/// reaches it (a third of a millisecond later), and NVIDIA's ICD reads it
/// back through FILE_INFO (glcore 0xa13870).
pub fn sync_file_signalled(fd: RawFd) -> io::Result<(i32, u64)> {
    let (a, top) = call(fd, SYNC_IOC_FILE_INFO, &[0; 56], &[48], None, |_, _| Ok(()))?;
    let p = a.bytes(top);
    let status = i32::from_le_bytes(p[32..36].try_into().unwrap());
    let n = u32::from_le_bytes(p[40..44].try_into().unwrap()) as usize;
    if status != 1 || n == 0 || n > SYNC_FENCE_INFO_MAX {
        return Ok((status, 0));
    }
    // The fences of a sync_file are fixed when it is made, so the count
    // just read is the count the kernel will fill.
    let mut q = [0u8; 56];
    q[40..44].copy_from_slice(&(n as u32).to_le_bytes());
    // The kernel writes n records to `infos`, which is exactly that long
    // (sync_file_ioctl_fence_info refuses an n smaller than the count).
    let mut infos = None;
    let Ok((a, _)) = call(fd, SYNC_IOC_FILE_INFO, &q, &[48], None, |a, top| {
        let b = a.small(&vec![0u8; n * SYNC_FENCE_INFO_SIZE]);
        infos = Some(b);
        a.point(top, 48, b)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }) else {
        return Ok((status, 0));
    };
    Ok((status, infos.map_or(0, |b| latest_signal(a.bytes(b)))))
}

/// The latest signal time among `struct sync_fence_info` records, counting
/// only signalled fences; 0 if none says.
fn latest_signal(infos: &[u8]) -> u64 {
    infos
        .chunks_exact(SYNC_FENCE_INFO_SIZE)
        .filter(|r| {
            i32::from_le_bytes(
                r[SYNC_FENCE_INFO_STATUS..SYNC_FENCE_INFO_STATUS + 4]
                    .try_into()
                    .unwrap(),
            ) == 1
        })
        .map(|r| {
            u64::from_le_bytes(
                r[SYNC_FENCE_INFO_TIMESTAMP..SYNC_FENCE_INFO_TIMESTAMP + 8]
                    .try_into()
                    .unwrap(),
            )
        })
        .max()
        .unwrap_or(0)
}

/// A fresh eventfd for a syncobj wait registration.
///
/// Non-blocking because the pump drains it when it reports readiness, and a
/// drain must never park the pump.
pub fn new_eventfd() -> io::Result<OwnedFd> {
    crate::sys::fd::eventfd(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)
}

/// `DRM_IOCTL_DROP_MASTER`; whether the file was master and now is not.
///
/// `drm_dropmaster_ioctl` answers -EINVAL for a file that is not the current
/// master (drm_auth.c:299-300), which is the ordinary "was not master" case.
pub fn drop_master(fd: RawFd) -> bool {
    crate::sys::ioctl::no_arg(fd, u64::from(DRM_IOCTL_DROP_MASTER)).is_ok()
}

/// A sync_file that is already signalled.
///
/// There is no general uapi for making one -- sw_sync lives in debugfs and is
/// off on most hosts -- so this borrows one from a subsystem that hands out
/// the kernel's always-signalled stub fence:
///
/// 1. A DRM syncobj created with `DRM_SYNCOBJ_CREATE_SIGNALED` holds the stub
///    fence (drm_syncobj.c:571-572, `drm_syncobj_assign_null_handle`), and
///    exporting it as a sync_file wraps that fence (drm_syncobj.c:759-790).
///    nvidia-drm sets `DRIVER_SYNCOBJ` (nvidia-drm-drv.c:1901-1902) wherever
///    the kernel has the syncobj API, so a render node of our own GPU works.
/// 2. Failing that, `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` on a dmabuf with no
///    fences returns the stub fence too (dma-buf.c:459-466); a one-page
///    udmabuf over a memfd is such a dmabuf, where `/dev/udmabuf` is
///    accessible.
///
/// The result is checked with SYNC_IOC_FILE_INFO before it is trusted.
pub fn signaled_sync_file(render_paths: &[String]) -> io::Result<OwnedFd> {
    let mut last = io::Error::from_raw_os_error(libc::ENODEV);
    for path in render_paths {
        match syncobj_signaled_sync_file(path) {
            Ok(fd) => return verified_signaled(fd),
            Err(e) => last = e,
        }
    }
    match udmabuf_signaled_sync_file() {
        Ok(fd) => verified_signaled(fd),
        Err(e) => {
            log::warn!(
                "no way to make a signalled sync_file: syncobj on the render nodes \
                 said {last}, udmabuf said {e}"
            );
            Err(e)
        }
    }
}

fn verified_signaled(fd: OwnedFd) -> io::Result<OwnedFd> {
    match sync_file_status(fd.as_raw_fd())? {
        1 => Ok(fd),
        s => Err(io::Error::other(format!("stub fence reports status {s}"))),
    }
}

fn open_path(path: &str, flags: i32) -> io::Result<OwnedFd> {
    // Fuzzing (device/src/fuzzing): no real device is ever opened.
    #[cfg(fuzzing)]
    if path != "/dev/null" {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    crate::sys::fd::open_path(path, flags | libc::O_CLOEXEC)
}

fn syncobj_signaled_sync_file(render_path: &str) -> io::Result<OwnedFd> {
    let node = open_path(render_path, libc::O_RDWR)?;
    let mut create = [0u8; 8];
    create[4..8].copy_from_slice(&DRM_SYNCOBJ_CREATE_SIGNALED.to_le_bytes());
    flat(node.as_raw_fd(), DRM_IOCTL_SYNCOBJ_CREATE, &mut create)?;
    let handle = u32::from_le_bytes(create[0..4].try_into().unwrap());
    let mut export = [0u8; 24];
    export[0..4].copy_from_slice(&handle.to_le_bytes());
    export[4..8].copy_from_slice(&DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE.to_le_bytes());
    let res = making_fd(node.as_raw_fd(), DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &export, 8);
    // The syncobj goes either way; closing the node would take it too, but
    // saying so is cheaper than reasoning about it.
    let mut destroy = [0u8; 8];
    destroy[0..4].copy_from_slice(&handle.to_le_bytes());
    let _ = flat(node.as_raw_fd(), DRM_IOCTL_SYNCOBJ_DESTROY, &mut destroy);
    res
}

fn udmabuf_signaled_sync_file() -> io::Result<OwnedFd> {
    let dev = open_path("/dev/udmabuf", libc::O_RDWR)?;
    let memfd = crate::sys::fd::memfd(
        c"nvgpu-stub-fence",
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
    )?;
    // udmabuf requires the memfd to be sealed against shrinking
    // (drivers/dma-buf/udmabuf.c, `udmabuf_create`), and a whole page.
    crate::sys::fd::ftruncate(&memfd, 4096)?;
    crate::sys::fd::add_seals(&memfd, libc::F_SEAL_SHRINK)?;
    let dmabuf =
        crate::sys::ioctl::udmabuf_create(dev.as_fd(), memfd.as_fd(), 4096, UDMABUF_FLAGS_CLOEXEC)?;
    let mut export = [0u8; 8];
    export[0..4].copy_from_slice(&DMA_BUF_SYNC_RW.to_le_bytes());
    making_fd(
        dmabuf.as_raw_fd(),
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
        &export,
        4,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence_info(status: i32, ts: u64) -> Vec<u8> {
        let mut r = vec![0u8; SYNC_FENCE_INFO_SIZE];
        r[..5].copy_from_slice(b"nvkms");
        r[SYNC_FENCE_INFO_STATUS..SYNC_FENCE_INFO_STATUS + 4]
            .copy_from_slice(&status.to_le_bytes());
        r[SYNC_FENCE_INFO_TIMESTAMP..SYNC_FENCE_INFO_TIMESTAMP + 8]
            .copy_from_slice(&ts.to_le_bytes());
        r
    }

    #[test]
    fn a_merged_fence_signalled_when_its_last_part_did() {
        let infos = [
            fence_info(1, 5_000),
            fence_info(1, 9_000),
            fence_info(1, 7_000),
        ]
        .concat();
        assert_eq!(latest_signal(&infos), 9_000);
    }

    #[test]
    fn unsignalled_and_failed_fences_give_no_time() {
        let infos = [fence_info(0, 0), fence_info(-110, 8_000)].concat();
        assert_eq!(latest_signal(&infos), 0);
        assert_eq!(latest_signal(&[]), 0);
    }

    #[test]
    fn identify_has_nvidia_drms_number() {
        assert_eq!(DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT, 0xC008_644E);
    }

    #[test]
    fn an_imported_foreign_dmabuf_is_reported_as_one() {
        assert_eq!(
            import_type(Ok(NV_GEM_OBJECT_DMABUF)),
            Ok(NV_GEM_OBJECT_DMABUF)
        );
        assert_eq!(
            import_type(Ok(NV_GEM_OBJECT_NVKMS)),
            Ok(NV_GEM_OBJECT_NVKMS)
        );
        assert_eq!(
            import_type(Ok(NV_GEM_OBJECT_USERMEMORY)),
            Ok(NV_GEM_OBJECT_USERMEMORY)
        );
    }

    #[test]
    fn an_import_that_identifies_as_unknown_is_refused() {
        assert_eq!(import_type(Ok(NV_GEM_OBJECT_UNKNOWN)), Err(libc::EINVAL));
        assert_eq!(import_type(Ok(3)), Err(libc::EINVAL));
    }

    #[test]
    fn a_node_without_modeset_imports_dmabuf_objects() {
        let e = io::Error::from_raw_os_error(libc::EOPNOTSUPP);
        assert_eq!(import_type(Err(e)), Ok(NV_GEM_OBJECT_DMABUF));
        let e = io::Error::from_raw_os_error(libc::EBADF);
        assert_eq!(import_type(Err(e)), Err(libc::EBADF));
    }

    /// The numbers are computed, so check a few against their uapi values.
    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(DRM_IOCTL_VERSION, 0xC040_6400);
        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(DRM_IOCTL_PRIME_HANDLE_TO_FD, 0xC00C_642D);
        assert_eq!(DRM_IOCTL_PRIME_FD_TO_HANDLE, 0xC00C_642E);
        assert_eq!(DRM_IOCTL_DROP_MASTER, 0x0000_641F);
        assert_eq!(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, 0xC018_64C1);
        assert_eq!(SYNC_IOC_MERGE, 0xC030_3E03);
        assert_eq!(SYNC_IOC_FILE_INFO, 0xC038_3E04);
        assert_eq!(ioc_size(0xC030_3E03), 48);
        assert_eq!(ioc_type(DRM_IOCTL_VERSION), b'd');
        assert_eq!(ioc_nr(DRM_IOCTL_SYNCOBJ_CREATE), 0xbf);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no /proc/self/fd to classify by")]
    fn an_eventfd_classifies_as_an_eventfd() {
        let fd = new_eventfd().unwrap();
        assert_eq!(classify(fd.as_fd(), &[]), HandleKind::Eventfd);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn a_memfd_classifies_as_a_memfd() {
        let fd = crate::sys::fd::memfd(c"keymap", libc::MFD_CLOEXEC).unwrap();
        assert_eq!(classify(fd.as_fd(), &[]), HandleKind::Memfd);
    }

    #[test]
    fn a_pipe_is_other_and_so_is_a_character_device_that_is_not_ours() {
        let (r, w) = crate::sys::fd::pipe2(libc::O_CLOEXEC).unwrap();
        assert_eq!(classify(r.as_fd(), &[]), HandleKind::Other);
        assert_eq!(classify(w.as_fd(), &[]), HandleKind::Other);
        let null = open_path("/dev/null", libc::O_RDONLY).unwrap();
        // Even claiming /dev/null's own numbers as a card does not make it
        // one: it is not major 226, and it is not nvidia-drm.
        let fake = CardNode {
            name: "null".into(),
            major: 1,
            minor: 3,
            render_index: 0,
        };
        assert_eq!(classify(null.as_fd(), &[fake]), HandleKind::Other);
    }

    #[test]
    fn anonymous_inode_links_name_their_kinds() {
        const ANON: i64 = ANON_INODE_FS_MAGIC;
        assert_eq!(
            kind_from_link(ANON, "anon_inode:sync_file"),
            HandleKind::SyncFile
        );
        assert_eq!(
            kind_from_link(ANON, "anon_inode:syncobj_file"),
            HandleKind::Syncobj
        );
        assert_eq!(
            kind_from_link(DMA_BUF_MAGIC, "/dmabuf:"),
            HandleKind::Dmabuf
        );
        assert_eq!(
            kind_from_link(DMA_BUF_MAGIC, "/dmabuf:scanout"),
            HandleKind::Dmabuf
        );
        assert_eq!(
            kind_from_link(ANON, "anon_inode:dmabuf"),
            HandleKind::Dmabuf
        );
        assert_eq!(
            kind_from_link(TMPFS_MAGIC, "/memfd:wl_shm (deleted)"),
            HandleKind::Memfd
        );
        assert_eq!(
            kind_from_link(HUGETLBFS_MAGIC, "/memfd:big (deleted)"),
            HandleKind::Memfd
        );
        assert_eq!(
            kind_from_link(ANON, "anon_inode:[eventpoll]"),
            HandleKind::Other
        );
        assert_eq!(
            kind_from_link(0x534f_434b, "socket:[1234]"),
            HandleKind::Other
        );
        // The link text is a path: on the wrong filesystem it names nothing.
        const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;
        assert_eq!(
            kind_from_link(FUSE_SUPER_MAGIC, "/dmabuf:x"),
            HandleKind::Other
        );
        assert_eq!(
            kind_from_link(FUSE_SUPER_MAGIC, "/memfd:x (deleted)"),
            HandleKind::Other
        );
        assert_eq!(
            kind_from_link(TMPFS_MAGIC, "anon_inode:sync_file"),
            HandleKind::Other
        );
    }

    /// The kinds of real descriptors, as the kernel makes them.
    #[test]
    #[cfg_attr(miri, ignore = "Miri models no fstatfs")]
    fn real_descriptors_are_what_the_kernel_says() {
        let m = crate::sys::fd::memfd(c"classify", libc::MFD_CLOEXEC).unwrap();
        assert_eq!(classify(m.as_fd(), &[]), HandleKind::Memfd);
        let e = new_eventfd().unwrap();
        assert_eq!(classify(e.as_fd(), &[]), HandleKind::Eventfd);
    }

    /// A process of the backend's uid with a mount namespace of its own --
    /// an export-mode peer -- names its files as it likes: here a plain
    /// tmpfs file whose `/proc/self/fd` link reads `/dmabuf:x/f`. It was
    /// classified a dma-buf; a FUSE file there would have had the backend's
    /// first read of it wait on its maker (review 2026-09-26, backend 13).
    #[test]
    #[cfg_attr(miri, ignore = "Miri runs no processes")]
    fn a_file_named_like_a_dmabuf_is_not_one() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("nvgpu-classify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display();
        // A tmpfs of its own, `dmabuf:x/f` in it, held open as fd 3, made
        // the root; then its pid, and a wait until told to go.
        let script = format!(
            "mount -t tmpfs none {d} && mkdir {d}/dmabuf:x {d}/old && echo hi > {d}/dmabuf:x/f \
             && exec 3<{d}/dmabuf:x/f && cd {d} && pivot_root . old && echo $$ && read x"
        );
        let child = Command::new("unshare")
            .args(["-Urm", "sh", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("SKIPPED a_file_named_like_a_dmabuf_is_not_one: no unshare");
            return;
        };
        let mut line = String::new();
        let _ = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
        let Ok(pid) = line.trim().parse::<u32>() else {
            eprintln!(
                "SKIPPED a_file_named_like_a_dmabuf_is_not_one: no user or mount namespace here"
            );
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir(&dir);
            return;
        };
        let f = std::fs::File::open(format!("/proc/{pid}/fd/3")).unwrap();
        let link = std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap();
        assert!(link.to_string_lossy().starts_with("/dmabuf:"), "{link:?}");
        assert_eq!(classify(f.as_fd(), &[]), HandleKind::Other);
        drop(child.stdin.take());
        let _ = child.wait();
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no F_GETFL on this file")]
    fn nonblock_is_set_on_request() {
        let fd = open_path("/dev/null", libc::O_RDONLY).unwrap();
        set_nonblock(fd.as_raw_fd()).unwrap();
        let fl = crate::sys::fd::status_flags(fd.as_raw_fd()).unwrap();
        assert_ne!(fl & libc::O_NONBLOCK, 0);
    }

    const MODESET: HandleKind = HandleKind::Dev(DeviceKind::Modeset);

    #[test]
    fn each_watch_type_is_only_accepted_on_its_own_kind() {
        use HandleKind::*;
        assert_eq!(watch_mode(DrmLease(0), W_DRM, 9), Ok(WatchMode::Drm));
        assert_eq!(watch_mode(DrmCard(0), W_DRM, 9), Ok(WatchMode::Drm));
        assert_eq!(
            watch_mode(SyncFile, W_FENCE, 9),
            Ok(WatchMode::Fence { cookie: 9 })
        );
        assert_eq!(
            watch_mode(Eventfd, W_READY | W_ONESHOT, 9),
            Ok(WatchMode::Ready {
                cookie: 9,
                oneshot: true,
                consume: true
            })
        );
        assert_eq!(
            watch_mode(MODESET, W_READY, 9),
            Ok(WatchMode::Ready {
                cookie: 9,
                oneshot: false,
                consume: false
            })
        );
        assert!(watch_mode(Wayland, W_READY, 9).is_ok());

        // Reading DRM events from anything else steals its bytes.
        for k in [
            Wayland,
            Eventfd,
            Memfd,
            Other,
            DriRender(0),
            SyncFile,
            MODESET,
        ] {
            assert_eq!(watch_mode(k, W_DRM, 1), Err(libc::EINVAL), "{k:?}");
        }
        for k in [DrmLease(0), Eventfd, Dmabuf, Other] {
            assert_eq!(watch_mode(k, W_FENCE, 1), Err(libc::EINVAL), "{k:?}");
        }
        for k in [Memfd, Dmabuf, Other, SyncFile, DrmLease(0), DriRender(0)] {
            assert_eq!(watch_mode(k, W_READY, 1), Err(libc::EINVAL), "{k:?}");
        }
    }

    #[test]
    fn a_watch_names_exactly_one_type_and_no_unknown_flags() {
        assert_eq!(watch_mode(HandleKind::Eventfd, 0, 1), Err(libc::EINVAL));
        assert_eq!(
            watch_mode(HandleKind::Eventfd, W_READY | W_FENCE, 1),
            Err(libc::EINVAL)
        );
        assert_eq!(
            watch_mode(HandleKind::Eventfd, W_READY | 1 << 9, 1),
            Err(libc::EINVAL)
        );
        // A DRM file keeps producing events; a one-shot watch would drop them.
        assert_eq!(
            watch_mode(HandleKind::DrmLease(0), W_DRM | W_ONESHOT, 1),
            Err(libc::EINVAL)
        );
    }

    fn op(op: u32, args: &[u64]) -> HostOpReq {
        let mut r = HostOpReq {
            op,
            nargs: args.len() as u32,
            args: [0; OP_MAX_ARGS],
        };
        r.args[..args.len()].copy_from_slice(args);
        r
    }

    /// Handles 1..=6 in a pretend table: render, dmabuf, two sync_files, a
    /// card, an eventfd.
    fn kinds(h: u32) -> Option<HandleKind> {
        use HandleKind::*;
        match h {
            1 => Some(DriRender(0)),
            2 => Some(Dmabuf),
            3 | 4 => Some(SyncFile),
            5 => Some(DrmCard(0)),
            6 => Some(Eventfd),
            7 => Some(DriRender(1)),
            _ => None,
        }
    }

    fn card(render_index: u32) -> CardNode {
        CardNode {
            name: "card1".into(),
            major: 226,
            minor: 1,
            render_index,
        }
    }

    #[test]
    fn host_op_arguments_are_checked_by_kind() {
        let k = &kinds;
        assert_eq!(
            check_host_op(&op(OP_PRIME_EXPORT, &[1, 42]), k, None),
            Ok(HostOp::PrimeExport { file: 1, gem: 42 })
        );
        // Exporting from anything but a render file, importing anything but
        // a dmabuf, merging anything but sync_files: all EBADF.
        assert_eq!(
            check_host_op(&op(OP_PRIME_EXPORT, &[5, 42]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_DMABUF_IMPORT, &[1, 3]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_DMABUF_IMPORT, &[2, 2]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_DMABUF_IMPORT, &[1, 2]), k, None),
            Ok(HostOp::DmabufImport { file: 1, dmabuf: 2 })
        );
        assert_eq!(
            check_host_op(&op(OP_SYNC_MERGE, &[2, 3, 4]), k, None),
            Ok(HostOp::SyncMerge { fences: vec![3, 4] })
        );
        assert_eq!(
            check_host_op(&op(OP_SYNC_MERGE, &[2, 3, 6]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_DROP_IF_MASTER, &[1]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_DROP_IF_MASTER, &[5]), k, None),
            Ok(HostOp::DropIfMaster { card: 5 })
        );
        assert_eq!(
            check_host_op(&op(OP_FD_KIND, &[99]), k, None),
            Err(libc::EBADF)
        );
        // A handle is a u32; a wider argument is not quietly truncated into one.
        assert_eq!(
            check_host_op(&op(OP_FD_KIND, &[(1u64 << 32) | 1]), k, None),
            Err(libc::EBADF)
        );
    }

    #[test]
    fn host_op_counts_must_match_the_arguments_sent() {
        let k = &kinds;
        assert_eq!(
            check_host_op(&op(OP_NEW_EVENTFD, &[1]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_SYNC_MERGE, &[3, 3, 4]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_SYNC_MERGE, &[0]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_CLOSE_MANY, &[6, 1, 2, 3, 4, 5]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_CLOSE_MANY, &[2, 99, 3]), k, None),
            Ok(HostOp::CloseMany {
                handles: vec![99, 3]
            })
        );
        let mut r = op(OP_NEW_EVENTFD, &[]);
        r.nargs = 7;
        assert_eq!(check_host_op(&r, k, None), Err(libc::EINVAL));
        assert_eq!(check_host_op(&op(77, &[]), k, None), Err(libc::EINVAL));
    }

    #[test]
    fn a_syncobj_watch_names_a_render_file_and_the_point_it_waits_for() {
        let k = &kinds;
        let cookie = 1u64 << 40;
        assert_eq!(
            check_host_op(&op(OP_SYNCOBJ_WATCH, &[1, 7, u64::MAX, 4, cookie]), k, None),
            Ok(HostOp::SyncobjWatch {
                key: RegKey {
                    render: 1,
                    syncobj: 7,
                    point: u64::MAX,
                    flags: 4,
                },
                cookie,
            })
        );
        // On a file that is not a render node; with a syncobj handle or flags
        // wider than the u32 they are; with an argument missing.
        assert_eq!(
            check_host_op(&op(OP_SYNCOBJ_WATCH, &[5, 7, 0, 0, cookie]), k, None),
            Err(libc::EBADF)
        );
        assert_eq!(
            check_host_op(&op(OP_SYNCOBJ_WATCH, &[1, 1 << 32, 0, 0, cookie]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_SYNCOBJ_WATCH, &[1, 7, 0, 1 << 32, cookie]), k, None),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_SYNCOBJ_WATCH, &[1, 7, 0, 0]), k, None),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn open_kms_needs_compositor_mode_and_a_card_on_the_same_device() {
        let k = &kinds;
        let cards = [card(0)];
        assert_eq!(
            check_host_op(&op(OP_OPEN_KMS, &[1, 0]), k, None),
            Err(libc::EOPNOTSUPP)
        );
        assert_eq!(
            check_host_op(&op(OP_OPEN_KMS, &[1, 0]), k, Some(&cards)),
            Ok(HostOp::OpenKms { render: 1, card: 0 })
        );
        assert_eq!(
            check_host_op(&op(OP_OPEN_KMS, &[1, 1]), k, Some(&cards)),
            Err(libc::ENODEV)
        );
        assert_eq!(
            check_host_op(&op(OP_OPEN_KMS, &[7, 0]), k, Some(&cards)),
            Err(libc::EINVAL)
        );
        assert_eq!(
            check_host_op(&op(OP_OPEN_KMS, &[5, 0]), k, Some(&cards)),
            Err(libc::EBADF)
        );
    }

    /// Only runs where a signalled fence can be made without a GPU; says so
    /// otherwise rather than passing silently.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn a_signaled_sync_file_reports_signalled() {
        match signaled_sync_file(&[]) {
            Ok(fd) => {
                assert_eq!(sync_file_status(fd.as_raw_fd()).unwrap(), 1);
                assert_eq!(classify(fd.as_fd(), &[]), HandleKind::SyncFile);
                // Merging two signalled fences gives a signalled fence.
                let m = sync_merge(fd.as_raw_fd(), fd.as_raw_fd()).unwrap();
                assert_eq!(sync_file_status(m.as_raw_fd()).unwrap(), 1);
                // ...and a signal time no later than now, from the per-fence
                // array the kernel really fills.
                let (status, at) = sync_file_signalled(m.as_raw_fd()).unwrap();
                assert_eq!(status, 1);
                assert!(at <= crate::session::monotonic_ns());
            }
            Err(e) => eprintln!("SKIP a_signaled_sync_file_reports_signalled: {e}"),
        }
    }
}
