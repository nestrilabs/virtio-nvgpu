// SPDX-License-Identifier: Apache-2.0
//! Fences on the backend: what a guest's syncobj and semaphore-surface calls
//! may do to a host thread, and the shared wait registrations that let the
//! guest sleep on a syncobj point without one (ARCHITECTURE.md, "Fences").
//!
//! Fence objects live on the host (Design A): a guest sync_file is a proxy for
//! a host sync_file, a guest syncobj handle *is* the host handle in the file's
//! render node. Everything that only creates, exports, imports or signals
//! those objects is forwarded as it stands. What must never be forwarded as it
//! stands is a *wait*: `drm_syncobj_array_wait_timeout` parks the calling
//! thread for as long as the guest asks (drm_syncobj.c:1136-1164), and so does
//! `drm_syncobj_find_fence` with `WAIT_FOR_SUBMIT`, for up to five seconds
//! (drm_syncobj.c:420, 442). On the queue thread that stops every guest
//! process; on an executor it still pins a thread per waiter. So [`before`]
//! turns every forwarded wait into a poll, and the guest does its sleeping
//! itself, woken by an event.
//!
//! The event comes from `DRM_IOCTL_SYNCOBJ_EVENTFD`: the host kernel signals
//! an eventfd when a point signals (or, with `WAIT_AVAILABLE`, when a fence
//! for it appears), and the pump reports that eventfd once as `EV_READY`. The
//! catch is that the kernel entry cannot be taken back: it lives until the
//! point fires or the syncobj is freed (drm_syncobj.c:1445-1456, 533-538),
//! whatever happens to the eventfd, and there is no unregister ioctl. One
//! registration per guest wait would let a guest polling a never-signalled
//! point with a short timeout grow host kernel memory without bound while
//! staying inside the backend's descriptor limit (RV:eventfd). So:
//!
//! - **one registration per (render file, syncobj, point, flags)**, shared: a
//!   second waiter is told the cookie of the first, and the guest fans the one
//!   `EV_READY` out to every waiter it has on that cookie;
//! - **a per-VM cap** ([`REGISTRATION_CAP`]) on registrations that have not
//!   fired, counted until they fire or are let go, and **a share of it per
//!   guest process** ([`REGISTRATION_SHARE`], quota.rs), orphans included, so
//!   one process's waits cannot use up every other's. Over either the guest
//!   falls back to polling with a short backoff, which costs it latency and
//!   the host nothing;
//! - **the syncobj kept alive** by the registration (a syncobj file of our
//!   own) for as long as anything of the guest's can reach it, so an entry
//!   ends only by firing -- which we see -- or with the syncobj, when we let
//!   it go: without that, destroy-and-reimport would let a guest leave
//!   entries on a syncobj it keeps alive through an exported file, uncounted;
//! - **let go with the syncobj, as natively.** A guest reaches a host syncobj
//!   only through what the backend holds for it -- a handle in one of its
//!   render files, a syncobj file in the handle table -- or through a Wayland
//!   channel it sent one over, whose compositor may hold it until the channel
//!   closes ([`Reach`]). When the last of these goes (the DESTROY of its last
//!   handle, the close of its last file; a process's exit closes both),
//!   nothing but our own files holds it and nobody can ever signal it. Its
//!   registrations are then let go, and closing our files frees the syncobj
//!   and the kernel its entries (drm_syncobj.c:528-541), exactly what happens
//!   to a native process's. One whose point already had a fence is the
//!   exception: its entry is on that fence now (:1419-1456), outlives the
//!   syncobj, and stays counted until it fires. So does every registration on
//!   a syncobj something outside may hold: the capture helper's, or one that
//!   came from the compositor.
//!
//! Userspace SYNCOBJ_EVENTFD (a compositor in the guest waiting on acquire
//! points) is served by the same registrations; the guest signals its own
//! eventfd when the shared one fires. The ioctl itself is never forwarded.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering::Relaxed;

use crate::hostfd::{self, HandleKind, IOC_RW, ioc};
use crate::le;
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;
use crate::pump::{PumpCmd, WatchMode};
use crate::quota::{Charge, Owner, Pool, Share};
use crate::sys::block::{Arena, BufId};
use crate::xfer::Errno;

/// Registrations that have not fired, per VM. Each holds three backend
/// descriptors (the eventfd, twice: the table's and ours; and the syncobj
/// file) and one host kernel entry with an eventfd context. A compositor has
/// a handful outstanding per surface; this is far above any honest workload
/// and far below anything that matters to the host.
pub const REGISTRATION_CAP: usize = 1024;

/// What one guest process may hold of [`REGISTRATION_CAP`] (quota.rs): a
/// quarter, the last sixteenth kept for processes holding at most 16.
pub const REGISTRATION_SHARE: Share = Share::quarter(REGISTRATION_CAP as u64, 16);

/// `DRM_SYNCOBJ_WAIT_FLAGS_*`, include/uapi/drm/drm.h:1002-1005.
pub const WAIT_FOR_SUBMIT: u32 = 1 << 1;
pub const WAIT_AVAILABLE: u32 = 1 << 2;

/// The fence calls the policy looks inside (include/uapi/drm/drm.h:1290-1315;
/// sizes are the 7.2.7 structs, which the schema requires exactly).
pub const SYNCOBJ_WAIT: u32 = ioc(IOC_RW, b'd', 0xc3, 40);
pub const SYNCOBJ_TIMELINE_WAIT: u32 = ioc(IOC_RW, b'd', 0xca, 48);
pub const SYNCOBJ_TRANSFER: u32 = ioc(IOC_RW, b'd', 0xcc, 32);
pub const SYNCOBJ_EVENTFD: u32 = ioc(IOC_RW, b'd', 0xcf, 24);

/// `timeout_nsec` in `drm_syncobj_wait` and `drm_syncobj_timeline_wait`, and
/// `flags` in `drm_syncobj_transfer`.
const WAIT_TIMEOUT_AT: usize = 8;
const TIMELINE_WAIT_TIMEOUT_AT: usize = 16;
const TRANSFER_FLAGS_AT: usize = 24;

// ─────────────────────────────── the policy ───────────────────────────────

/// The FENCES policy for one fence call, on the backend's own copy of its
/// argument (`Hooks::before`, policy.rs). Everything a guest may ask of the
/// host's fence objects is allowed, as long as no host thread waits on it:
///
/// - SYNCOBJ_WAIT / TIMELINE_WAIT run with `timeout_nsec = 0`, a poll
///   (drm_timeout_abs_to_jiffies(0) is 0, drm_syncobj.c:1207-1208, and a zero
///   timeout returns -ETIME at once, :1156-1159). The guest's timeout is in
///   the guest's clock anyway, and would be misread here. Its deadline hint
///   (WAIT_DEADLINE) is still applied (:1122-1129) -- the guest converts it
///   to the host's clock before sending;
/// - TRANSFER with `WAIT_FOR_SUBMIT` is refused: it waits up to five seconds
///   for the source point to be submitted. The guest waits for availability
///   itself first and sends the call without the flag;
/// - SYNCOBJ_EVENTFD is refused: each call leaves a kernel entry nobody can
///   remove. Registrations go through HOST_OP SYNCOBJ_WATCH, which shares and
///   caps them ([`Registrations`]).
///
/// The semaphore-surface calls (nvidia-drm 0x54-0x57) never wait: FENCE_WAIT
/// registers a callback (nvidia-drm-fence.c:1713-1732), FENCE_CREATE and
/// ATTACH arm a timer (:1443-1446). 0x55-0x57 pass as they are. 0x54 does not
/// come here: the index it carries is added to a host kernel mapping
/// unchecked (:1257-1261) and the client it names is dup'd at kernel
/// privilege, so it is bounded, owned and counted first (`semsurf`, called
/// from the policy hooks in its place).
pub fn before(cmd: u32, arg: &mut [u8]) -> Result<(), Errno> {
    let zero = |arg: &mut [u8], at: usize| -> Result<(), Errno> {
        arg.get_mut(at..at + 8)
            .ok_or(libc::EINVAL)?
            .copy_from_slice(&0u64.to_le_bytes());
        Ok(())
    };
    match cmd {
        SYNCOBJ_WAIT => zero(arg, WAIT_TIMEOUT_AT),
        SYNCOBJ_TIMELINE_WAIT => zero(arg, TIMELINE_WAIT_TIMEOUT_AT),
        SYNCOBJ_TRANSFER => {
            let flags = le::u32_at(arg, TRANSFER_FLAGS_AT).ok_or(libc::EINVAL)?;
            if flags & WAIT_FOR_SUBMIT != 0 {
                log::warn!(
                    "SYNCOBJ_TRANSFER with WAIT_FOR_SUBMIT would park a host thread for up to \
                     5 s; the guest must wait for the point itself"
                );
                return Err(libc::EINVAL);
            }
            Ok(())
        }
        SYNCOBJ_EVENTFD => {
            log::warn!(
                "SYNCOBJ_EVENTFD forwarded as an ioctl; waits are registered through \
                 HOST_OP SYNCOBJ_WATCH"
            );
            Err(libc::EPERM)
        }
        _ => Ok(()),
    }
}

// ─────────────────────────────── host calls ───────────────────────────────

/// What one registration waits for. Syncobj handles are per host file, so
/// the file is part of the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegKey {
    pub render: u32,
    pub syncobj: u32,
    pub point: u64,
    /// 0 (the point signals) or `WAIT_AVAILABLE` (a fence for it appears).
    pub flags: u32,
}

/// The answer to a SYNCOBJ_WATCH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watched {
    /// A new registration, reported under the caller's own cookie.
    New,
    /// The point was already registered: its `EV_READY` carries this cookie,
    /// from the guest's earlier registration.
    Joined(u64),
}

/// The host calls a registration makes, so the bookkeeping can be tested
/// without a DRM device.
pub trait SyncobjHost {
    /// A syncobj file for `syncobj` of the file `render`
    /// (DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, flags 0): a reference that keeps the
    /// syncobj alive for as long as we hold it. Fails ENOENT-like (the kernel
    /// says EINVAL, drm_syncobj.c:694) for a handle the file does not have.
    fn syncobj_file(&self, render: RawFd, syncobj: u32) -> io::Result<OwnedFd>;
    /// DRM_IOCTL_SYNCOBJ_EVENTFD on `render`.
    fn register(
        &self,
        render: RawFd,
        syncobj: u32,
        point: u64,
        flags: u32,
        eventfd: RawFd,
    ) -> io::Result<()>;
    /// Whether `point` of the syncobj behind our syncobj file `syncobj` has
    /// a fence (submitted, signalled or not), asked through any DRM file
    /// `probe`: imported there for the question and destroyed after it.
    /// A registration without WAIT_AVAILABLE on such a point is no longer
    /// on the syncobj's list but on that fence (drm_syncobj.c:1419-1456),
    /// and outlives the syncobj.
    fn available(&self, probe: RawFd, syncobj: BorrowedFd<'_>, point: u64) -> io::Result<bool>;
}

/// The host's.
pub struct HostSyncobj;

impl SyncobjHost for HostSyncobj {
    fn syncobj_file(&self, render: RawFd, syncobj: u32) -> io::Result<OwnedFd> {
        // struct drm_syncobj_handle { u32 handle, flags; s32 fd; u32 pad;
        // u64 point; }
        let mut arg = [0u8; 24];
        arg[0..4].copy_from_slice(&syncobj.to_le_bytes());
        // The kernel installs the descriptor it writes at 8 for us
        // (drm_syncobj_get_fd, drm_syncobj.c:687, with O_CLOEXEC).
        let mut a = Arena::new();
        let top = a.small(&arg);
        a.fd_out(top, 8, 4).map_err(io::Error::from_raw_os_error)?;
        drm_ioctl(render, hostfd::DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut a, top)?;
        a.claim_fd(top, 8)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))
    }

    fn register(
        &self,
        render: RawFd,
        syncobj: u32,
        point: u64,
        flags: u32,
        eventfd: RawFd,
    ) -> io::Result<()> {
        // struct drm_syncobj_eventfd { u32 handle, flags; u64 point; s32 fd;
        // u32 pad; }
        let mut arg = [0u8; 24];
        arg[0..4].copy_from_slice(&syncobj.to_le_bytes());
        arg[4..8].copy_from_slice(&flags.to_le_bytes());
        arg[8..16].copy_from_slice(&point.to_le_bytes());
        arg[16..20].copy_from_slice(&eventfd.to_le_bytes());
        let mut a = Arena::new();
        let top = a.small(&arg);
        drm_ioctl(render, SYNCOBJ_EVENTFD, &mut a, top)
    }

    fn available(&self, probe: RawFd, syncobj: BorrowedFd<'_>, point: u64) -> io::Result<bool> {
        let h = hostfd::syncobj_import(probe, syncobj.as_raw_fd())?;
        // struct drm_syncobj_timeline_wait { u64 handles, points; s64
        // timeout_nsec; u32 count_handles, flags, first_signaled, pad; u64
        // deadline_nsec; }: timeout 0 is a poll (drm_syncobj.c:1156-1159),
        // and WAIT_AVAILABLE counts a point with a fence as ready (:1100) and
        // one without as not yet, not EINVAL (:1084-1091).
        let mut arg = [0u8; 48];
        arg[24..28].copy_from_slice(&1u32.to_le_bytes());
        arg[28..32].copy_from_slice(&WAIT_AVAILABLE.to_le_bytes());
        let mut a = Arena::new();
        let top = a.small(&arg);
        let handles = a.small(&h.to_le_bytes());
        let points = a.small(&point.to_le_bytes());
        let r = (|| {
            a.ptr(top, 0).map_err(io::Error::from_raw_os_error)?;
            a.point(top, 0, handles)
                .map_err(io::Error::from_raw_os_error)?;
            a.ptr(top, 8).map_err(io::Error::from_raw_os_error)?;
            a.point(top, 8, points)
                .map_err(io::Error::from_raw_os_error)?;
            match drm_ioctl(probe, SYNCOBJ_TIMELINE_WAIT, &mut a, top) {
                Ok(()) => Ok(true),
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(false),
                Err(e) => Err(e),
            }
        })();
        // Destroyed whatever the answer; a DESTROY of a handle just made in
        // a file we hold fails only for a pad (drm_syncobj.c:1311).
        let _ = hostfd::syncobj_destroy(probe, h);
        r
    }
}

/// A DRM call on `fd`, its argument block `top` of `a` (exactly
/// _IOC_SIZE(cmd) bytes), retried across signals.
fn drm_ioctl(fd: RawFd, cmd: u32, a: &mut Arena, top: BufId) -> io::Result<()> {
    debug_assert_eq!(a.len(top), hostfd::ioc_size(cmd));
    let r = a.call(&crate::sys::ioctl::HostRetry, fd, u64::from(cmd), top);
    if r < 0 {
        Err(io::Error::from_raw_os_error(-r))
    } else {
        Ok(())
    }
}

/// Where a registration's eventfd is watched from: the handle table and the
/// event pump, which the backend owns.
pub trait RegTable {
    /// Give `eventfd` a handle, charged to guest process `owner`, and have
    /// the pump report it once, as EV_READY with `cookie`, when it becomes
    /// readable.
    fn publish(&mut self, eventfd: OwnedFd, cookie: u64, owner: Owner) -> Result<u32, Errno>;
    /// Take that handle back: the registration fired, or never will. Its
    /// report, if it fired, still reaches every waiter on its cookie.
    fn retire(&mut self, handle: u32);
}

// ─────────────────────────── what reaches a syncobj ───────────────────────────

/// One host syncobj, as the backend tells them apart: a number names a
/// syncobj only in one file and only until its DESTROY, a syncobj file
/// names one for its life, and several of each may name the same one.
type Sid = u64;

/// A syncobj the backend does not follow: what is registered on it is
/// never let go before it fires.
const UNKNOWN: Sid = 0;

/// Syncobj handles and files the backend follows at once, per VM
/// ([`Reach`]). The guest can make syncobj handles without bound, each
/// costing the host kernel about what an entry here costs the backend; this
/// bounds the backend's part. Past it, a handle or file the backend cannot
/// follow is taken for one something unseen may hold -- the registrations
/// on its syncobj wait out their firing -- and so is every handle of a
/// render file an import into which went unfollowed.
pub const TRACKED_MAX: usize = 65_536;

/// What of the guest's may still reach one host syncobj.
#[derive(Default)]
struct Obj {
    /// Handles in the guest's render files and syncobj files in its handle
    /// table that name it.
    refs: u32,
    /// Wayland channels it was sent over: the compositor may hold its
    /// import, and signal it, until the channel closes.
    channels: Vec<u32>,
    /// Something outside the backend may hold it for as long as it likes:
    /// the capture helper's (INJECT_OPEN_SYNCOBJ), a syncobj file the
    /// backend did not make (one the compositor sent), or a number the
    /// backend found out of step with the host.
    foreign: bool,
}

/// What became of a syncobj when one of the guest's ways to it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Left {
    /// Still reachable.
    Reachable,
    /// Only our own files hold it: nobody can signal it again.
    Unreachable(Sid),
    /// No handle or file of the guest's names it, but a channel's
    /// compositor may hold it until the channel closes.
    Channels(Sid),
    /// Held, perhaps, by something the backend cannot see.
    Foreign,
}

/// Every way the guest can reach a host syncobj, and which syncobj each
/// names. Every call that makes or ends a syncobj handle or syncobj file
/// runs inline on the queue thread, under the backend lock, in the guest's
/// order (the schema's SYNCOBJ_* entries are `Exec::Inline`), so this is
/// the host's own picture of them.
///
/// A handle not in `handles` that the host has is one a SYNCOBJ_CREATE
/// made: only its file names it. It gets an entry when something needs to
/// know which syncobj it is -- a registration on it, an export of it -- so
/// the map holds only handles that are watched or were exported or
/// imported, and each entry is a handle the host holds too.
struct Reach {
    next: Sid,
    /// (render handle, syncobj handle) -> the syncobj.
    handles: HashMap<(u32, u32), Sid>,
    /// Syncobj-file handle of the table -> the syncobj.
    files: HashMap<u32, Sid>,
    objs: HashMap<Sid, Obj>,
    /// Wayland channel -> what it was sent.
    sent: HashMap<u32, Vec<Sid>>,
    /// Render files with an import the backend could not follow: a handle
    /// of theirs it has no entry for may be anyone's syncobj.
    untracked: HashSet<u32>,
    /// At most this many entries in `handles` and `files` together.
    max: usize,
}

impl Default for Reach {
    fn default() -> Self {
        Self::with_max(TRACKED_MAX)
    }
}

impl Reach {
    fn with_max(max: usize) -> Self {
        Self {
            next: UNKNOWN,
            handles: HashMap::new(),
            files: HashMap::new(),
            objs: HashMap::new(),
            sent: HashMap::new(),
            untracked: HashSet::new(),
            max,
        }
    }

    fn room(&self) -> bool {
        self.handles.len() + self.files.len() < self.max
    }

    fn fresh(&mut self, foreign: bool) -> Sid {
        self.next += 1;
        self.objs.insert(
            self.next,
            Obj {
                foreign,
                ..Obj::default()
            },
        );
        self.next
    }

    /// The syncobj handle `n` of `render` names, which the host has: one a
    /// CREATE made, if the backend has not heard of it otherwise and every
    /// import into the file was followed. [`UNKNOWN`] if it cannot say.
    fn of_handle(&mut self, render: u32, n: u32) -> Sid {
        if let Some(&s) = self.handles.get(&(render, n)) {
            return s;
        }
        if self.untracked.contains(&render) || !self.room() {
            return UNKNOWN;
        }
        let s = self.fresh(false);
        self.handles.insert((render, n), s);
        self.obj(s).refs += 1;
        s
    }

    fn obj(&mut self, s: Sid) -> &mut Obj {
        self.objs.entry(s).or_default()
    }

    /// Handle `n` of `render` now names `s`, a syncobj that may be held
    /// elsewhere (an import). A number already mapped means a DESTROY or a
    /// close went by unseen: neither syncobj is then one to let go.
    fn add_handle(&mut self, render: u32, n: u32, s: Sid) -> Vec<Left> {
        let mut left = Vec::new();
        if !self.handles.contains_key(&(render, n)) && !self.room() {
            // Not followed: the syncobj has a way to it the backend does
            // not count, and the file a handle it cannot name.
            log::warn!(
                "syncobj handles: {} followed, the most; registrations on handle {n} of render \
                 handle {render}'s syncobj wait out their firing",
                self.max
            );
            self.untracked.insert(render);
            if let Some(o) = self.objs.get_mut(&s) {
                o.foreign = true;
            }
            left.push(self.settle_obj(s));
            return left;
        }
        if let Some(old) = self.handles.insert((render, n), s) {
            log::warn!(
                "syncobj handle {n} of render handle {render} was already known; neither \
                 syncobj it named is let go before it fires"
            );
            self.obj(old).foreign = true;
            self.obj(s).foreign = true;
            left.push(self.unref(old));
        }
        self.obj(s).refs += 1;
        left
    }

    /// A SYNCOBJ_CREATE made handle `n` of `render`: nothing else names
    /// what it names. A number still mapped is one whose end went unseen.
    fn created(&mut self, render: u32, n: u32) -> Vec<Left> {
        match self.handles.remove(&(render, n)) {
            Some(old) => {
                log::warn!(
                    "SYNCOBJ_CREATE gave handle {n} of render handle {render}, which was \
                     already known; the syncobj it named is not let go before it fires"
                );
                self.obj(old).foreign = true;
                vec![self.unref(old)]
            }
            None => Vec::new(),
        }
    }

    /// Handle `n` of `render` was destroyed.
    fn drop_handle(&mut self, render: u32, n: u32) -> Left {
        match self.handles.remove(&(render, n)) {
            Some(s) => self.unref(s),
            // Never watched, exported or imported: nothing watches it.
            None => Left::Reachable,
        }
    }

    /// Render handle `render` closed, and every syncobj handle of it.
    fn drop_render(&mut self, render: u32) -> Vec<Left> {
        let gone: Vec<Sid> = self
            .handles
            .iter()
            .filter(|((r, _), _)| *r == render)
            .map(|(_, s)| *s)
            .collect();
        self.handles.retain(|(r, _), _| *r != render);
        self.untracked.remove(&render);
        gone.into_iter().map(|s| self.unref(s)).collect()
    }

    /// Handle `n` of `render` was exported as the syncobj file `file`.
    fn exported(&mut self, render: u32, n: u32, file: u32) -> Vec<Left> {
        let s = self.of_handle(render, n);
        let mut left = Vec::new();
        if s == UNKNOWN {
            // Not followed; an import of the file is someone else's.
            return left;
        }
        if !self.room() {
            self.obj(s).foreign = true;
            return left;
        }
        if let Some(old) = self.files.insert(file, s) {
            // The table never hands out a live number: its close went unseen.
            self.obj(old).foreign = true;
            left.push(self.unref(old));
        }
        self.obj(s).refs += 1;
        left
    }

    /// Syncobj file `file` (a table handle; None: one the backend cannot
    /// name) was imported as handle `n` of `render`. A file the backend did
    /// not make is someone else's syncobj.
    fn imported(&mut self, render: u32, n: u32, file: Option<u32>) -> Vec<Left> {
        let s = match file.and_then(|f| self.files.get(&f).copied()) {
            Some(s) => s,
            None => self.fresh(true),
        };
        self.add_handle(render, n, s)
    }

    /// Handle `n` of `render` is a syncobj someone else holds (the capture
    /// helper's).
    fn foreign_handle(&mut self, render: u32, n: u32) -> Vec<Left> {
        let s = self.fresh(true);
        self.add_handle(render, n, s)
    }

    /// Table handle `file` closed. Not every closed handle was a syncobj
    /// file of ours: nothing, then.
    fn drop_file(&mut self, file: u32) -> Left {
        match self.files.remove(&file) {
            Some(s) => self.unref(s),
            None => Left::Reachable,
        }
    }

    /// Syncobj file `file` was sent to the compositor over `channel`.
    fn sent(&mut self, file: u32, channel: u32) {
        let Some(&s) = self.files.get(&file) else {
            return;
        };
        let o = self.obj(s);
        if !o.channels.contains(&channel) {
            o.channels.push(channel);
            self.sent.entry(channel).or_default().push(s);
        }
    }

    /// Wayland channel `channel` closed: its compositor client, and every
    /// import it held, with it.
    fn drop_channel(&mut self, channel: u32) -> Vec<Left> {
        let Some(sids) = self.sent.remove(&channel) else {
            return Vec::new();
        };
        sids.into_iter()
            .map(|s| {
                if let Some(o) = self.objs.get_mut(&s) {
                    o.channels.retain(|&c| c != channel);
                }
                self.settle_obj(s)
            })
            .collect()
    }

    fn unref(&mut self, s: Sid) -> Left {
        if let Some(o) = self.objs.get_mut(&s) {
            o.refs = o.refs.saturating_sub(1);
        }
        self.settle_obj(s)
    }

    /// What `s` has left; its record goes once nothing the backend can see
    /// comes back to it.
    fn settle_obj(&mut self, s: Sid) -> Left {
        let Some(o) = self.objs.get(&s) else {
            return Left::Foreign;
        };
        if o.refs > 0 {
            return Left::Reachable;
        }
        if o.foreign {
            self.forget(s);
            return Left::Foreign;
        }
        if !o.channels.is_empty() {
            return Left::Channels(s);
        }
        self.objs.remove(&s);
        Left::Unreachable(s)
    }

    /// Stop tracking `s`: nothing watches it, or nothing can end what does.
    fn forget(&mut self, s: Sid) {
        if let Some(o) = self.objs.remove(&s) {
            for c in o.channels {
                if let Some(v) = self.sent.get_mut(&c) {
                    v.retain(|&x| x != s);
                    if v.is_empty() {
                        self.sent.remove(&c);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        self.handles.clear();
        self.files.clear();
        self.objs.clear();
        self.sent.clear();
        self.untracked.clear();
    }
}

// ───────────────────────────── registrations ─────────────────────────────

struct Reg {
    /// What it waits for; an orphan's names what it waited for.
    key: RegKey,
    /// The syncobj it is on.
    sid: Sid,
    /// Its slot, charged to the guest process that made it, given back
    /// when it is dropped.
    _charge: Charge,
    cookie: u64,
    /// The eventfd's handle, which the pump watches.
    handle: u32,
    /// Our own descriptor for the same eventfd, to see whether it fired: the
    /// pump's watch is one-shot and does not drain it, so once signalled it
    /// stays readable.
    eventfd: PrivateFd,
    /// Keeps the syncobj, and with it the kernel entry, from ending in any
    /// way but firing or our letting it go. None for one let go whose entry
    /// is on its point's fence, which ends only by firing.
    syncobj: Option<PrivateFd>,
}

impl Reg {
    fn fired(&self) -> bool {
        fired(self.eventfd.as_raw_fd())
    }
}

/// Every live registration of a session.
pub struct Registrations {
    regs: HashMap<RegKey, Reg>,
    /// Registrations whose key no longer names what they wait on: the
    /// syncobj handle was destroyed, or the render file closed. The host
    /// hands the number out again at once (lowest free, drm_syncobj.c:606),
    /// so a waiter on the new syncobj that joined one of these would sleep
    /// on a point of the old one, which may never fire (S-13). Never joined;
    /// swept once fired, or let go with their syncobj. They keep their slot
    /// until then, since the kernel entry is there too.
    orphans: Vec<Reg>,
    /// Registrations on syncobjs nothing of the guest's reaches any more,
    /// waiting to be let go ([`Registrations::settle`]): counted until then.
    releasing: Vec<Reg>,
    /// The cap, and each guest process's part of it, orphans included.
    slots: Pool,
    /// Handles of registrations that fired, or never will, not yet handed
    /// back: at the next call that reaches the table ([`Registrations::reap`]).
    /// Each was a registration's, so there are never more than the cap.
    ///
    /// A handle is not simply closed: an unwatch drops whatever the pump had
    /// not yet sent for it (`pump::Outbox::forget`), and a registration is
    /// seen to have fired when its eventfd is readable -- which may be a
    /// moment *before* the pump has read that readiness, and longer before
    /// the EV_READY every guest waiter on the cookie sleeps for has gone out.
    /// [`RegTable::retire`] keeps that report, and the descriptor, counted,
    /// until it has (`pump::PumpCmd::Retire`). A fixed grace instead kept
    /// every fired handle charged to its process for a second: at the rate a
    /// busy process fires them, more than its whole share of the table.
    retired: VecDeque<u32>,
    /// Which syncobj each handle and file names, and what still reaches it.
    reach: Reach,
}

impl Default for Registrations {
    fn default() -> Self {
        Self::with_cap(REGISTRATION_CAP)
    }
}

/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE` and
/// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`, both bit 0: a fence
/// moves, not the syncobj (include/uapi/drm/drm.h).
const SYNC_FILE_MODE: u32 = 1 << 0;

fn word(arg: Option<&[u8]>, at: usize) -> Option<u32> {
    le::u32_at(arg?, at)
}

/// Whether render-class IOCTL2 `name` makes or ends a syncobj handle or
/// syncobj file ([`Reach`]): its finish tells the registrations
/// ([`Registrations::after_ioctl2`], [`Registrations::exported`]).
pub fn changes_reach(name: &str) -> bool {
    matches!(
        name,
        "SYNCOBJ_CREATE" | "SYNCOBJ_DESTROY" | "SYNCOBJ_FD_TO_HANDLE" | "SYNCOBJ_HANDLE_TO_FD"
    )
}

/// The handle a SYNCOBJ_HANDLE_TO_FD that ran with host result `result`,
/// its argument `arg`, exported as a syncobj file; None for any other call,
/// a failed one, or a sync_file export (a fence moves, not the syncobj).
pub fn exported_handle(name: &str, arg: Option<&[u8]>, result: Option<i32>) -> Option<u32> {
    if name != "SYNCOBJ_HANDLE_TO_FD" || result != Some(0) {
        return None;
    }
    // drm_syncobj_handle { handle @0, flags @4, .. }.
    match (word(arg, 0), word(arg, 4)) {
        (Some(n), Some(f)) if f & SYNC_FILE_MODE == 0 => Some(n),
        _ => None,
    }
}

impl Registrations {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            regs: HashMap::new(),
            orphans: Vec::new(),
            releasing: Vec::new(),
            slots: Pool::new(
                cap as u64,
                if cap == REGISTRATION_CAP {
                    REGISTRATION_SHARE
                } else {
                    Share::quarter(cap as u64, 1)
                },
            ),
            retired: VecDeque::new(),
            reach: Reach::default(),
        }
    }

    /// What guest process `o` holds, orphans included.
    pub fn held_by(&self, o: Owner) -> u64 {
        self.slots.held(o)
    }

    /// Whether syncobj handle `syncobj` of render handle `render` names one
    /// the backend lets go of when the guest can no longer reach it (not
    /// the capture helper's, not the compositor's).
    #[cfg(test)]
    pub(crate) fn is_foreign_for_test(&self, render: u32, syncobj: u32) -> bool {
        match self.reach.handles.get(&(render, syncobj)) {
            Some(s) => self.reach.objs.get(s).is_none_or(|o| o.foreign),
            None => self.reach.untracked.contains(&render),
        }
    }

    /// A render-class IOCTL2 on `render` is about to run, its argument
    /// `arg` (the backend's copy). SYNCOBJ_DESTROY orphans the handle's
    /// registrations first ([`Registrations::orphan`]).
    pub fn before_ioctl2(&mut self, render: u32, name: &str, arg: Option<&[u8]>) {
        // drm_syncobj_destroy.handle @0.
        if name == "SYNCOBJ_DESTROY"
            && let Some(h) = word(arg, 0)
        {
            self.orphan(render, h);
        }
    }

    /// The call [`Registrations::before_ioctl2`] saw has run, with host
    /// result `result`, its argument `arg` as the host left it, and
    /// `file` the table handle it took as its descriptor, if any. What it
    /// did to the handles naming a syncobj:
    ///
    /// - SYNCOBJ_CREATE made a handle nothing else names;
    /// - SYNCOBJ_FD_TO_HANDLE of a syncobj file made another handle for the
    ///   syncobj behind `file` (someone else's, if `file` is not one the
    ///   backend made by an export);
    /// - SYNCOBJ_DESTROY ended one: if it was the last way to its syncobj,
    ///   the registrations on it are let go ([`Registrations::settle`]).
    ///
    /// A call that failed changed nothing. An export (HANDLE_TO_FD) is
    /// [`Registrations::exported`], once its file has a handle.
    pub fn after_ioctl2(
        &mut self,
        render: u32,
        name: &str,
        arg: Option<&[u8]>,
        result: Option<i32>,
        file: Option<u32>,
    ) {
        if result != Some(0) {
            return;
        }
        // A new handle under a number the backend thought live: whatever
        // was registered under it is some other syncobj's now.
        let made = match name {
            "SYNCOBJ_CREATE" => word(arg, 0),
            "SYNCOBJ_FD_TO_HANDLE" if word(arg, 4).is_some_and(|f| f & SYNC_FILE_MODE == 0) => {
                word(arg, 0)
            }
            _ => None,
        };
        if let Some(n) = made
            && self.reach.handles.contains_key(&(render, n))
        {
            self.orphan(render, n);
        }
        let left = match name {
            // drm_syncobj_create.handle @0, written by the host.
            "SYNCOBJ_CREATE" => match word(arg, 0) {
                Some(n) => self.reach.created(render, n),
                None => Vec::new(),
            },
            // drm_syncobj_destroy.handle @0.
            "SYNCOBJ_DESTROY" => match word(arg, 0) {
                Some(n) => vec![self.reach.drop_handle(render, n)],
                None => Vec::new(),
            },
            // drm_syncobj_handle { handle @0 (written), flags @4, fd @8 }.
            "SYNCOBJ_FD_TO_HANDLE" => match (word(arg, 0), word(arg, 4)) {
                (Some(_), Some(f)) if f & SYNC_FILE_MODE != 0 => Vec::new(),
                (Some(n), _) => self.reach.imported(render, n, file),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        self.release(left);
    }

    /// SYNCOBJ_HANDLE_TO_FD of handle `n` of `render` (without
    /// EXPORT_SYNC_FILE) gave the syncobj file the table holds as `file`.
    pub fn exported(&mut self, render: u32, n: u32, file: u32) {
        let left = self.reach.exported(render, n, file);
        self.release(left);
    }

    /// INJECT_OPEN_SYNCOBJ imported the capture helper's syncobj as handle
    /// `n` of `render`: the helper holds it as long as it likes, so what is
    /// registered on it waits out its firing.
    pub fn foreign_handle(&mut self, render: u32, n: u32) {
        let left = self.reach.foreign_handle(render, n);
        self.release(left);
    }

    /// Table handle `file` closed (the guest's CLOSE, an `I2_FD_CONSUME`, a
    /// reply never delivered): if it was a syncobj file of ours, one way to
    /// its syncobj fewer.
    pub fn file_closed(&mut self, file: u32) {
        let left = vec![self.reach.drop_file(file)];
        self.release(left);
    }

    /// Syncobj file `file` went to the compositor over Wayland channel
    /// `channel`: the syncobj stays reachable until the channel closes.
    pub fn sent(&mut self, file: u32, channel: u32) {
        self.reach.sent(file, channel);
    }

    /// Wayland channel (table handle) `channel` closed.
    pub fn channel_closed(&mut self, channel: u32) {
        let left = self.reach.drop_channel(channel);
        self.release(left);
    }

    /// The registrations on syncobjs `left` says nothing reaches any more
    /// go to be let go; a syncobj only a channel still holds, with no
    /// registration on it, is forgotten, since nothing could end them.
    fn release(&mut self, left: Vec<Left>) {
        let mut gone = HashSet::new();
        for l in left {
            match l {
                Left::Unreachable(s) => {
                    gone.insert(s);
                }
                Left::Channels(s) if !self.has_regs(s) => self.reach.forget(s),
                _ => {}
            }
        }
        if gone.is_empty() {
            return;
        }
        let keys: Vec<RegKey> = self
            .regs
            .iter()
            .filter(|(_, r)| gone.contains(&r.sid))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(r) = self.regs.remove(&k) {
                self.releasing.push(r);
            }
        }
        let (go, keep): (Vec<Reg>, Vec<Reg>) = std::mem::take(&mut self.orphans)
            .into_iter()
            .partition(|r| r.syncobj.is_some() && gone.contains(&r.sid));
        self.orphans = keep;
        self.releasing.extend(go);
    }

    fn has_regs(&self, s: Sid) -> bool {
        self.regs
            .values()
            .chain(&self.orphans)
            .chain(&self.releasing)
            .any(|r| r.sid == s)
    }

    /// Let go of the registrations on syncobjs nothing reaches any more,
    /// asking through DRM file `probe` what each still holds on the host:
    ///
    /// - one that fired, or that waits for its point to become available
    ///   and has not, holds nothing or an entry on the syncobj's list, which
    ///   goes with the syncobj: dropped, its slot back now;
    /// - one whose point has a fence holds an entry on that fence
    ///   (drm_syncobj.c:1419-1456), which outlives the syncobj: kept,
    ///   counted, until it fires -- fences end;
    /// - one whose point has none holds an entry on the syncobj's list:
    ///   dropped.
    ///
    /// Then our syncobj files close, and the last of them frees the syncobj
    /// and the entries on its list (:528-541). All of one syncobj's go
    /// together, so none is uncounted while another of ours still holds the
    /// syncobj up. A question the host cannot answer leaves that syncobj's
    /// registrations counted, to be settled at the next call with a DRM
    /// file (every SYNCOBJ_WATCH has one).
    pub fn settle(&mut self, host: &dyn SyncobjHost, probe: RawFd) {
        if self.releasing.is_empty() {
            return;
        }
        let mut by_sid: HashMap<Sid, Vec<Reg>> = HashMap::new();
        for r in std::mem::take(&mut self.releasing) {
            by_sid.entry(r.sid).or_default().push(r);
        }
        for (_, group) in by_sid {
            let on_fence: Option<Vec<bool>> = group
                .iter()
                .map(|r| {
                    if r.fired() || r.key.flags & WAIT_AVAILABLE != 0 {
                        return Some(false);
                    }
                    let Some(syncobj) = r.syncobj.as_ref() else {
                        return Some(true);
                    };
                    match host.available(probe, syncobj.as_fd(), r.key.point) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            log::debug!(
                                "syncobj wait registration: cannot tell whether point {} has a \
                                 fence ({e}); kept until the next try",
                                r.key.point
                            );
                            None
                        }
                    }
                })
                .collect();
            let Some(on_fence) = on_fence else {
                self.releasing.extend(group);
                continue;
            };
            for (mut r, fence) in group.into_iter().zip(on_fence) {
                if fence && !r.fired() {
                    r.syncobj = None;
                    self.orphans.push(r);
                } else {
                    self.drop_reg(r);
                }
            }
        }
    }

    /// A registration nothing will ever fire again, dropped: its slot and
    /// its owner's charge back at once, its descriptors closed now (our
    /// syncobj file with them, which may free the syncobj and its kernel
    /// entries), its handle at the next reap, as a fired one's.
    fn drop_reg(&mut self, r: Reg) {
        self.retired.push_back(r.handle);
        let sid = r.sid;
        drop(r);
        self.forget_if_idle(sid);
    }

    /// A syncobj no handle or file names, kept only for the channels that
    /// sent it, is forgotten once nothing is registered on it: nothing
    /// could be let go at the channels' close, and nothing new can be
    /// registered on what the guest cannot name.
    fn forget_if_idle(&mut self, s: Sid) {
        if self.reach.objs.get(&s).is_some_and(|o| o.refs == 0) && !self.has_regs(s) {
            self.reach.forget(s);
        }
    }

    /// Registrations that have not been seen to fire, orphans included.
    pub fn len(&self) -> usize {
        self.regs.len() + self.orphans.len() + self.releasing.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Syncobj handle `syncobj` of render handle `render` is being
    /// destroyed: its registrations stop being joinable, so a watch on the
    /// next syncobj to get that number starts a registration of its own.
    /// Called before the DESTROY runs, not after: between the host freeing
    /// the number and the reply, another guest thread may already import a
    /// syncobj under it and watch. A DESTROY that then fails leaves a live
    /// syncobj with an orphaned registration, which costs a second kernel
    /// entry for the next waiter and nothing else.
    pub fn orphan(&mut self, render: u32, syncobj: u32) {
        self.orphan_where(|k| k.render == render && k.syncobj == syncobj);
    }

    /// Render handle `render` is closing, and every syncobj handle in it:
    /// its registrations are orphaned, and those on syncobjs nothing else
    /// reaches go to be let go.
    pub fn orphan_file(&mut self, render: u32) {
        self.orphan_where(|k| k.render == render);
        let left = self.reach.drop_render(render);
        self.release(left);
    }

    fn orphan_where(&mut self, f: impl Fn(&RegKey) -> bool) {
        let keys: Vec<RegKey> = self.regs.keys().filter(|k| f(k)).copied().collect();
        for k in keys {
            if let Some(r) = self.regs.remove(&k) {
                self.orphans.push(r);
            }
        }
    }

    /// Have the guest woken, under `cookie`, when `key`'s point is ready --
    /// or tell it which earlier cookie already will be.
    ///
    /// Errors: EINVAL for flags other than WAIT_AVAILABLE, for a cookie in
    /// the legacy (handle) range or one another live registration reports
    /// under; EAGAIN over the cap; ENOENT for a syncobj the file does not
    /// have; otherwise what the host says.
    ///
    /// Charged to no process: for tests. The backend names the process
    /// ([`Registrations::watch_by`]).
    #[cfg(test)]
    pub fn watch(
        &mut self,
        host: &dyn SyncobjHost,
        table: &mut dyn RegTable,
        render_fd: RawFd,
        key: RegKey,
        cookie: u64,
    ) -> Result<Watched, Errno> {
        self.watch_by(host, table, render_fd, key, cookie, Owner::Unknown)
    }

    /// [`Registrations::watch`], a new registration charged to guest
    /// process `owner`, which may hold only its share of the cap. Joining
    /// makes no kernel entry, and is not charged.
    pub fn watch_by(
        &mut self,
        host: &dyn SyncobjHost,
        table: &mut dyn RegTable,
        render_fd: RawFd,
        key: RegKey,
        cookie: u64,
        owner: Owner,
    ) -> Result<Watched, Errno> {
        // What is waiting to be let go, while there is a DRM file to ask
        // through; what earlier calls retired, before this one takes a
        // handle; what this one retires (a fired registration it replaces,
        // the sweep's), after.
        self.settle(host, render_fd);
        self.reap(table);
        let r = self.watch_new(host, table, render_fd, key, cookie, owner);
        self.reap(table);
        r
    }

    fn watch_new(
        &mut self,
        host: &dyn SyncobjHost,
        table: &mut dyn RegTable,
        render_fd: RawFd,
        key: RegKey,
        cookie: u64,
        owner: Owner,
    ) -> Result<Watched, Errno> {
        if key.flags & !WAIT_AVAILABLE != 0 {
            return Err(libc::EINVAL);
        }
        // Cookies at or below u32::MAX name legacy watches by handle (the
        // guest routes them to an nvgpu_fd, nvgpu_events.c nvgpu_ev_record).
        if cookie <= u64::from(u32::MAX) {
            return Err(libc::EINVAL);
        }
        if let Some(r) = self.regs.get(&key) {
            if !r.fired() {
                return Ok(Watched::Joined(r.cookie));
            }
            // Fired, and so done: the pump has reported it (or is about to).
            // A waiter arriving now wants the point's *next* state, which a
            // fresh registration reports -- at once if it is still ready.
            self.retire(&key);
        }
        if self
            .regs
            .values()
            .chain(&self.orphans)
            .chain(&self.releasing)
            .any(|r| r.cookie == cookie)
        {
            // Two points under one cookie would wake each other's waiters
            // and, worse, retire one another's guest bookkeeping.
            return Err(libc::EINVAL);
        }
        let charge = match self.slots.try_take(owner, 1) {
            Ok(c) => c,
            Err(_) => {
                self.sweep();
                match self.slots.try_take(owner, 1) {
                    Ok(c) => c,
                    Err(why) => {
                        log::warn!(
                            "syncobj wait registrations: guest process {owner:?} holds {}, the \
                             VM {} of {} ({why:?}); it polls instead",
                            self.slots.held(owner),
                            self.len(),
                            self.slots.size()
                        );
                        return Err(libc::EAGAIN);
                    }
                }
            }
        };

        // The kernel's only EINVAL here is a handle the file does not have
        // (drm_syncobj.c:694); SYNCOBJ_EVENTFD itself says ENOENT for that
        // (:1479), and that is the call the guest is answering.
        let syncobj = PrivateFd::new(host.syncobj_file(render_fd, key.syncobj).map_err(|e| {
            match errno(&e) {
                libc::EINVAL => libc::ENOENT,
                other => other,
            }
        })?);
        // The handle is the host's: which syncobj it names.
        let sid = self.reach.of_handle(key.render, key.syncobj);
        let eventfd = PrivateFd::new(hostfd::new_eventfd().map_err(|e| errno(&e))?);
        let dup = eventfd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| errno(&e))?;
        // Watched before it is registered: the kernel may signal it inside
        // the ioctl (a point already signalled, drm_syncobj.c:1447-1453), and
        // the pump's watch looks at the descriptor once when it is armed.
        // The handle is charged to the process too, and a retired
        // registration's stays charged until the pump has sent its report:
        // uncharged, a process looping on signalled points while the guest
        // took no events would fill the table past its share.
        let handle = table.publish(dup, cookie, owner)?;
        if let Err(e) = host.register(
            render_fd,
            key.syncobj,
            key.point,
            key.flags,
            eventfd.as_raw_fd(),
        ) {
            table.retire(handle);
            return Err(errno(&e));
        }
        self.regs.insert(
            key,
            Reg {
                key,
                sid,
                _charge: charge,
                cookie,
                handle,
                eventfd,
                syncobj: Some(syncobj),
            },
        );
        Ok(Watched::New)
    }

    /// Forget every registration that has fired. Their slots are free at
    /// once; their handles go at the next reap.
    pub fn sweep(&mut self) {
        let done: Vec<RegKey> = self
            .regs
            .iter()
            .filter(|(_, r)| r.fired())
            .map(|(k, _)| *k)
            .collect();
        for k in done {
            self.retire(&k);
        }
        let mut fired = Vec::new();
        for list in [&mut self.orphans, &mut self.releasing] {
            let (f, live): (Vec<Reg>, Vec<Reg>) =
                std::mem::take(list).into_iter().partition(Reg::fired);
            *list = live;
            fired.extend(f);
        }
        for r in fired {
            self.drop_reg(r);
        }
    }

    fn retire(&mut self, key: &RegKey) {
        if let Some(r) = self.regs.remove(key) {
            self.drop_reg(r);
        }
    }

    /// Hand back the handles of registrations that fired, or never will
    /// ([`RegTable::retire`]).
    pub fn reap(&mut self, table: &mut dyn RegTable) {
        while let Some(h) = self.retired.pop_front() {
            table.retire(h);
        }
    }

    /// Handles retired and not yet handed back (tests).
    #[cfg(test)]
    pub fn retired_for_test(&self) -> usize {
        self.retired.len()
    }

    /// Whether registrations wait to be let go ([`Registrations::settle`]).
    pub fn releasing(&self) -> bool {
        !self.releasing.is_empty()
    }

    /// Registrations waiting to be let go (tests).
    #[cfg(test)]
    pub fn releasing_for_test(&self) -> usize {
        self.releasing.len()
    }

    /// With room to follow `max` syncobj handles and files (tests).
    #[cfg(test)]
    pub(crate) fn with_tracking_for_test(max: usize) -> Self {
        Self {
            reach: Reach::with_max(max),
            ..Self::default()
        }
    }

    /// The session is gone: drop every registration without touching the
    /// table, which the reset has emptied already (a handle number may belong
    /// to nobody now). Every syncobj the guest held dies with its files, and
    /// the kernel entries on it with it; any on a fence stays until that
    /// signals, as it would for a native process that exited.
    pub fn clear(&mut self) {
        self.regs.clear();
        self.orphans.clear();
        self.releasing.clear();
        self.retired.clear();
        self.reach.clear();
    }
}

fn errno(e: &io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// Whether an eventfd has been signalled: readable, without draining it.
fn fired(fd: RawFd) -> bool {
    crate::sys::fd::readable(fd, 0)
}

// ─────────────────────────────── the backend ───────────────────────────────

impl RegTable for NvidiaBackend {
    fn publish(&mut self, eventfd: OwnedFd, cookie: u64, owner: Owner) -> Result<u32, Errno> {
        let pump = eventfd.try_clone().map_err(|e| errno(&e))?;
        let handle = self
            .handles
            .insert_for(eventfd, HandleKind::Eventfd, owner)
            .map_err(|e| e.errno())?;
        self.pump_cmds.push(PumpCmd::Watch {
            handle,
            fd: pump,
            // Not drained, so that `fired` can still see it; one-shot, so the
            // level sweep never reports it again.
            mode: WatchMode::Ready {
                cookie,
                oneshot: true,
                consume: false,
            },
        });
        Ok(handle)
    }

    fn retire(&mut self, handle: u32) {
        // The guest can name this handle (it is in the same table), and a
        // CLOSE of it by a confused guest would have freed the number; only an
        // eventfd is ours to close.
        if self.handles.kind(handle) == Some(HandleKind::Eventfd) {
            let _ = self.retire_reported(handle);
        }
    }
}

impl NvidiaBackend {
    /// HOST_OP SYNCOBJ_WATCH: register (or join) a wait on `key`, reported
    /// under `cookie`. Returns `[reporting cookie, 1 if joined]`.
    pub(crate) fn syncobj_watch(&mut self, key: RegKey, cookie: u64) -> Result<Vec<u64>, Errno> {
        let render_fd = self.handles.get_raw(key.render).map_err(|_| libc::EBADF)?;
        // The process that asked (HOST_OP's trailer), else the file's opener.
        let owner = match self.current_owner {
            Owner::Unknown => self.handles.owner(key.render),
            o => o,
        };
        let host = self.syncobj_host.clone();
        let mut regs = std::mem::take(&mut self.syncobj_regs);
        let r = regs.watch_by(&*host, self, render_fd, key, cookie, owner);
        self.syncobj_regs = regs;
        let p = &crate::pacing::PACING;
        match r {
            Ok(Watched::New) => p.watch_new.fetch_add(1, Relaxed),
            Ok(Watched::Joined(_)) => p.watch_joined.fetch_add(1, Relaxed),
            Err(libc::EAGAIN) => p.watch_over_cap.fetch_add(1, Relaxed),
            Err(_) => 0,
        };
        match r? {
            Watched::New => Ok(vec![cookie, 0]),
            Watched::Joined(c) => Ok(vec![c, 1]),
        }
    }

    /// Let go of the registrations on syncobjs the guest can no longer
    /// reach ([`Registrations::settle`]), asking through DRM file `probe`,
    /// or any render file of the session; with none, at the next watch.
    /// Their handles go at the next reap (`reap_syncobj_regs`): not here,
    /// where a handle is being closed already.
    pub(crate) fn settle_syncobj_regs(&mut self, probe: Option<RawFd>) {
        if !self.syncobj_regs.releasing() {
            return;
        }
        let probe = probe.or_else(|| {
            self.handles
                .handles()
                .into_iter()
                .find(|&h| matches!(self.handles.kind(h), Some(HandleKind::DriRender(_))))
                .and_then(|h| self.handles.get_raw(h).ok())
        });
        if let Some(fd) = probe {
            self.syncobj_regs.settle(&*self.syncobj_host, fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn devnull() -> OwnedFd {
        crate::sys::fd::open(c"/dev/null", libc::O_RDONLY | libc::O_CLOEXEC).unwrap()
    }

    /// A host with syncobjs 1..=9, recording registrations, and able to fire
    /// the eventfd a registration was given.
    #[derive(Default)]
    struct Host {
        registered: RefCell<Vec<(u32, u64, u32, RawFd)>>,
        fail_register: RefCell<Option<i32>>,
        files: RefCell<usize>,
        /// The name of every syncobj file handed out (a memfd), in order:
        /// ours is open while this process has a descriptor by that name.
        /// (Not a pipe's far end: another test's fork holds a copy of ours
        /// for a moment.)
        ends: RefCell<Vec<String>>,
        /// Points that have a fence ([`SyncobjHost::available`]).
        fenced: RefCell<HashSet<u64>>,
        /// `available` cannot be answered.
        blind: RefCell<bool>,
        /// Syncobj files are /dev/null, not tracked (for many rounds).
        untracked: bool,
    }

    impl SyncobjHost for Host {
        fn syncobj_file(&self, _: RawFd, syncobj: u32) -> io::Result<OwnedFd> {
            if !(1..=9).contains(&syncobj) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            *self.files.borrow_mut() += 1;
            if self.untracked {
                return Ok(devnull());
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let name = format!("fence-test-syncobj-{}", NEXT.fetch_add(1, Relaxed));
            let c = std::ffi::CString::new(name.clone()).unwrap();
            let fd = crate::sys::fd::memfd(&c, libc::MFD_CLOEXEC).unwrap();
            self.ends.borrow_mut().push(name);
            Ok(fd)
        }

        fn available(&self, _: RawFd, _: BorrowedFd<'_>, point: u64) -> io::Result<bool> {
            if *self.blind.borrow() {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(self.fenced.borrow().contains(&point))
        }

        fn register(
            &self,
            _: RawFd,
            syncobj: u32,
            point: u64,
            flags: u32,
            eventfd: RawFd,
        ) -> io::Result<()> {
            if let Some(e) = *self.fail_register.borrow() {
                return Err(io::Error::from_raw_os_error(e));
            }
            self.registered
                .borrow_mut()
                .push((syncobj, point, flags, eventfd));
            Ok(())
        }
    }

    impl Host {
        /// What the kernel does when the point of registration `i` is ready.
        fn fire(&self, i: usize) {
            let fd = self.registered.borrow()[i].3;
            let one = 1u64.to_ne_bytes();
            assert_eq!(crate::sys::fd::write_raw(fd, &one).unwrap(), 8);
        }

        /// Whether the syncobj file registration `i` was given is still
        /// open: ours holds the syncobj up.
        fn holds(&self, i: usize) -> bool {
            let want = format!("/memfd:{} (deleted)", self.ends.borrow()[i]);
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .flatten()
                .any(|e| std::fs::read_link(e.path()).is_ok_and(|l| l.as_os_str() == want.as_str()))
        }
    }

    /// The host as the backend asks it itself (settling), shareable: no
    /// point has a fence.
    struct NoFences;

    impl SyncobjHost for NoFences {
        fn syncobj_file(&self, _: RawFd, _: u32) -> io::Result<OwnedFd> {
            Ok(devnull())
        }
        fn register(&self, _: RawFd, _: u32, _: u64, _: u32, _: RawFd) -> io::Result<()> {
            Ok(())
        }
        fn available(&self, _: RawFd, _: BorrowedFd<'_>, _: u64) -> io::Result<bool> {
            Ok(false)
        }
    }

    #[derive(Default)]
    struct Table {
        next: u32,
        live: HashSet<u32>,
        watched: Vec<(u32, u64)>,
        retired: Vec<u32>,
        full: bool,
    }

    impl RegTable for Table {
        fn publish(&mut self, _: OwnedFd, cookie: u64, _: Owner) -> Result<u32, Errno> {
            if self.full {
                return Err(libc::EMFILE);
            }
            self.next += 1;
            self.live.insert(self.next);
            self.watched.push((self.next, cookie));
            Ok(self.next)
        }

        fn retire(&mut self, handle: u32) {
            assert!(self.live.remove(&handle), "retired twice or never made");
            self.retired.push(handle);
        }
    }

    const C1: u64 = 1 << 32 | 1;
    const C2: u64 = 1 << 32 | 2;
    const C3: u64 = 1 << 32 | 3;

    fn key(syncobj: u32, point: u64, flags: u32) -> RegKey {
        RegKey {
            render: 20,
            syncobj,
            point,
            flags,
        }
    }

    #[test]
    fn a_second_waiter_on_the_same_point_joins_the_first_registration() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C1),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C2),
            Ok(Watched::Joined(C1))
        );
        assert_eq!(host.registered.borrow().len(), 1, "one kernel entry");
        assert_eq!(
            t.watched,
            vec![(1, C1)],
            "watched once, under the first cookie"
        );
    }

    #[test]
    fn a_different_point_flag_or_file_is_a_different_registration() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C1),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 6, 0), C2),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, WAIT_AVAILABLE), C3),
            Ok(Watched::New)
        );
        let other_file = RegKey {
            render: 21,
            ..key(1, 5, 0)
        };
        assert_eq!(
            r.watch(&host, &mut t, 3, other_file, 1 << 33),
            Ok(Watched::New)
        );
        assert_eq!(r.len(), 4);
        let flags: Vec<u32> = host.registered.borrow().iter().map(|e| e.2).collect();
        assert_eq!(flags, vec![0, 0, WAIT_AVAILABLE, 0]);
    }

    #[test]
    fn a_fired_registration_is_replaced_rather_than_joined() {
        let (host, mut t) = (Host::default(), Table::default());
        let mut r = Registrations::default();
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C1),
            Ok(Watched::New)
        );
        host.fire(0);
        // The point is ready now, and a new waiter wants to hear about its
        // state from here on: a fresh entry, which the kernel fires at once
        // if it still is.
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C2),
            Ok(Watched::New)
        );
        assert_eq!(t.watched, vec![(1, C1), (2, C2)]);
        assert_eq!(r.len(), 1);
        // The old handle goes back in the call that replaced it; its report
        // is the table's to keep (`RegTable::retire`).
        assert_eq!(t.retired, vec![1]);
        assert_eq!(r.retired_for_test(), 0);
    }

    #[test]
    fn over_the_cap_the_guest_is_told_to_poll_until_something_fires() {
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(2));
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), C1),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 1, 0), C2),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(3, 1, 0), C3),
            Err(libc::EAGAIN)
        );
        assert_eq!(host.registered.borrow().len(), 2, "nothing registered");
        // Joining costs nothing and is still allowed at the cap.
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), 1 << 40),
            Ok(Watched::Joined(C1))
        );
        // Once one fires its slot is swept and reused.
        host.fire(1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(3, 1, 0), C3),
            Ok(Watched::New)
        );
        assert_eq!(r.len(), 2);
        r.reap(&mut t);
        assert_eq!(t.retired, vec![2]);
    }

    #[test]
    fn an_unfired_registration_keeps_its_slot_whatever_the_guest_does() {
        // No path but firing gives a slot back: the syncobj file each holds
        // means the kernel entry cannot end any other way, so neither may
        // the count of it.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(1));
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), C1),
            Ok(Watched::New)
        );
        r.sweep();
        assert_eq!(r.len(), 1);
        assert_eq!(*host.files.borrow(), 1, "the syncobj is held");
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 1, 0), C2),
            Err(libc::EAGAIN)
        );
    }

    #[test]
    fn a_watch_after_the_syncobj_was_destroyed_never_joins_the_old_registration() {
        // Handle 1 destroyed and handed out again: the same key is another
        // syncobj now, whose point the old registration will never report.
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        r.orphan(20, 1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C2),
            Ok(Watched::New)
        );
        assert_eq!(host.registered.borrow().len(), 2, "its own kernel entry");
        // Another syncobj of the file is untouched.
        r.watch(&host, &mut t, 3, key(2, 5, 0), C3).unwrap();
        r.orphan(20, 1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 5, 0), 1 << 40),
            Ok(Watched::Joined(C3))
        );
    }

    #[test]
    fn a_closed_render_files_registrations_are_never_joined_by_its_successor() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        r.watch(&host, &mut t, 3, key(2, 5, 0), C2).unwrap();
        r.orphan_file(20);
        for (s, c) in [(1, C3), (2, 1 << 40)] {
            assert_eq!(r.watch(&host, &mut t, 3, key(s, 5, 0), c), Ok(Watched::New));
        }
    }

    #[test]
    fn an_orphan_holds_its_slot_and_its_cookie_until_it_fires() {
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(1));
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        r.orphan(20, 1);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 5, 0), C2),
            Err(libc::EAGAIN),
            "the kernel entry is still there, so is its count"
        );
        r.sweep();
        assert_eq!(r.len(), 1, "not fired, not swept");
        host.fire(0);
        r.sweep();
        assert!(r.is_empty());
        r.reap(&mut t);
        assert_eq!(t.retired, vec![1], "its handle is handed back");
        let mut r = Registrations::default();
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        r.orphan(20, 1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 5, 0), C1),
            Err(libc::EINVAL),
            "an orphan still reports under its cookie"
        );
    }

    fn p(tgid: u32) -> Owner {
        Owner::Proc {
            tgid,
            start_ns: u64::from(tgid),
        }
    }

    /// SYNCOBJ_DESTROY of handle `syncobj` of render handle 20, answered
    /// `result`, then what it let go settled.
    fn destroy(r: &mut Registrations, host: &Host, syncobj: u32, result: i32) {
        destroy_in(r, host, 20, syncobj, result);
    }

    fn destroy_in(r: &mut Registrations, host: &dyn SyncobjHost, render: u32, n: u32, result: i32) {
        let arg = n.to_le_bytes();
        r.before_ioctl2(render, "SYNCOBJ_DESTROY", Some(&arg));
        r.after_ioctl2(render, "SYNCOBJ_DESTROY", Some(&arg), Some(result), None);
        r.settle(host, 3);
    }

    fn handle_arg(syncobj: u32, flags: u32) -> [u8; 24] {
        let mut a = [0u8; 24];
        a[0..4].copy_from_slice(&syncobj.to_le_bytes());
        a[4..8].copy_from_slice(&flags.to_le_bytes());
        a
    }

    /// SYNCOBJ_FD_TO_HANDLE of table handle `file` (None: one the backend
    /// cannot name) into `render`, the host's answer handle `n`.
    fn import(r: &mut Registrations, render: u32, n: u32, file: Option<u32>) {
        let arg = handle_arg(n, 0);
        r.after_ioctl2(render, "SYNCOBJ_FD_TO_HANDLE", Some(&arg), Some(0), file);
    }

    #[test]
    fn one_process_holds_at_most_its_share_of_the_cap() {
        // A process polling never-signalled points cannot leave the others
        // of its VM without registrations.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(64));
        let share = Share::quarter(64, 1).per_owner as u32;
        for i in 0..share {
            let k = RegKey {
                point: u64::from(i),
                ..key(1, 0, 0)
            };
            assert_eq!(
                r.watch_by(&host, &mut t, 3, k, (2 << 32) | u64::from(i), p(1)),
                Ok(Watched::New)
            );
        }
        let k = RegKey {
            point: 999,
            ..key(1, 0, 0)
        };
        assert_eq!(
            r.watch_by(&host, &mut t, 3, k, 3 << 32, p(1)),
            Err(libc::EAGAIN)
        );
        assert_eq!(r.held_by(p(1)), u64::from(share));
        // Another process is not affected, and joining is free.
        assert_eq!(
            r.watch_by(&host, &mut t, 3, k, 3 << 32, p(2)),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch_by(&host, &mut t, 3, k, 4 << 32, p(1)),
            Ok(Watched::Joined(3 << 32))
        );
        // A fired one is given back to its owner.
        host.fire(0);
        r.sweep();
        assert_eq!(r.held_by(p(1)), u64::from(share) - 1);
    }

    #[test]
    fn a_destroyed_syncobj_only_its_file_could_reach_takes_its_registrations_along() {
        // Never exported: the handle was the only way to it, so the kernel
        // frees it -- and the entries on it -- once our syncobj file goes.
        // Nothing is left to count.
        let host = Host::default();
        // A share of one.
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(4));
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        destroy(&mut r, &host, 1, 0);
        assert!(r.is_empty());
        assert_eq!(r.held_by(p(1)), 0);
        assert!(!host.holds(0), "the syncobj is let go");
        assert_eq!(
            r.watch_by(&host, &mut t, 3, key(2, 5, 0), C2, p(1)),
            Ok(Watched::New),
            "its share is free at once"
        );
        r.reap(&mut t);
        assert_eq!(t.retired, vec![1], "its handle is handed back");
    }

    #[test]
    fn a_failed_destroy_leaves_the_orphan_counted() {
        // DESTROY with a pad, or of a handle the file does not have: the
        // syncobj (if any) lives on, and so does the kernel entry.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        destroy(&mut r, &host, 1, -libc::EINVAL);
        assert_eq!(r.len(), 1);
        assert_eq!(r.held_by(p(1)), 1);
        assert!(host.holds(0));
    }

    #[test]
    fn a_syncobj_that_got_out_is_let_go_with_the_last_way_to_it() {
        // Exported as a syncobj file and imported back: the syncobj outlives
        // the handle a registration was made on, and so does the
        // registration, counted. It goes when the last handle and file
        // naming the syncobj do: our file is then its last reference, nobody
        // can signal it, and the kernel frees its entries with it.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.watch_by(&host, &mut t, 3, key(2, 5, 0), C2, p(1))
            .unwrap();
        r.exported(20, 1, 50);
        destroy(&mut r, &host, 1, 0);
        assert_eq!(r.len(), 2, "the exported one is an orphan");
        assert_eq!(r.held_by(p(1)), 2, "charged to its maker");
        assert!(host.holds(0));
        // The file imported as handle 7: another way to the same syncobj.
        import(&mut r, 20, 7, Some(50));
        r.file_closed(50);
        r.settle(&host, 3);
        assert_eq!(r.len(), 2, "handle 7 still reaches it");
        destroy(&mut r, &host, 7, 0);
        assert_eq!(r.len(), 1, "gone with the last handle");
        assert_eq!(r.held_by(p(1)), 1);
        assert!(!host.holds(0) && host.holds(1));
        // A sync_file export moves a fence, not the syncobj: handle 2 is
        // still the only way to its syncobj.
        let mut sync_file = handle_arg(2, 1);
        sync_file[8..12].copy_from_slice(&51u32.to_le_bytes());
        r.after_ioctl2(20, "SYNCOBJ_HANDLE_TO_FD", Some(&sync_file), Some(0), None);
        r.after_ioctl2(
            20,
            "SYNCOBJ_FD_TO_HANDLE",
            Some(&handle_arg(2, 1)),
            Some(0),
            Some(51),
        );
        destroy(&mut r, &host, 2, 0);
        assert!(r.is_empty());
        assert_eq!(r.held_by(p(1)), 0);
    }

    #[test]
    fn closing_a_render_file_lets_go_of_what_only_it_and_its_files_reached() {
        // A process's exit closes its render file and every syncobj file it
        // held: after both, nothing of the guest's reaches any syncobj it
        // made, whatever it exported and imported.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        for (s, c) in [(1, C1), (2, C2), (3, C3)] {
            r.watch_by(&host, &mut t, 3, key(s, 5, 0), c, p(1)).unwrap();
        }
        r.exported(20, 2, 50);
        import(&mut r, 20, 4, Some(50));
        // A DESTROY that failed left handle 3 an orphan; the close takes it.
        destroy(&mut r, &host, 3, -libc::EINVAL);
        r.orphan_file(20);
        r.settle(&host, 3);
        assert_eq!(r.len(), 1, "only the one its file still reaches is left");
        assert_eq!(r.held_by(p(1)), 1);
        assert_eq!(
            (host.holds(0), host.holds(1), host.holds(2)),
            (false, true, false)
        );
        r.file_closed(50);
        r.settle(&host, 3);
        assert!(r.is_empty());
        assert_eq!(r.held_by(p(1)), 0);
        assert!(!host.holds(1));
        // The file's handle numbers start clean for its next owner: a new
        // private syncobj under an exported one's old number goes with its
        // DESTROY, and a destroy of another file's number 2 is not this one.
        let mut r2 = Registrations::default();
        r2.watch_by(&host, &mut t, 3, key(4, 5, 0), 1 << 41, p(1))
            .unwrap();
        r2.exported(20, 4, 52);
        destroy(&mut r2, &host, 4, 0);
        r2.watch_by(&host, &mut t, 3, key(4, 5, 0), 1 << 42, p(1))
            .unwrap();
        destroy(&mut r2, &host, 4, 0);
        assert_eq!(r2.len(), 1, "the exported one's orphan stays");
        r2.watch_by(
            &host,
            &mut t,
            3,
            RegKey {
                render: 21,
                ..key(2, 5, 0)
            },
            1 << 43,
            p(2),
        )
        .unwrap();
        destroy(&mut r2, &host, 2, 0);
        assert_eq!(r2.len(), 2);
        assert_eq!((r2.held_by(p(1)), r2.held_by(p(2))), (1, 1));
    }

    #[test]
    fn a_registration_whose_point_has_a_fence_outlives_its_syncobj_until_it_fires() {
        // Without WAIT_AVAILABLE, a registration on a point that has a fence
        // is on that fence (drm_syncobj.c:1419-1456), not on the syncobj's
        // list: freeing the syncobj leaves it, so it stays counted until the
        // fence signals. The others on the syncobj go with it.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.watch_by(&host, &mut t, 3, key(1, 6, WAIT_AVAILABLE), C2, p(1))
            .unwrap();
        r.watch_by(&host, &mut t, 3, key(1, 7, 0), C3, p(1))
            .unwrap();
        host.fenced.borrow_mut().extend([5, 6]);
        destroy(&mut r, &host, 1, 0);
        assert_eq!(r.len(), 1, "point 5's, on its fence");
        assert_eq!(r.held_by(p(1)), 1);
        assert!(
            (0..3).all(|i| !host.holds(i)),
            "every syncobj file of ours closed: the syncobj is freed"
        );
        r.sweep();
        assert_eq!(r.len(), 1);
        host.fire(0);
        r.sweep();
        assert!(r.is_empty());
        assert_eq!(r.held_by(p(1)), 0);
    }

    #[test]
    fn a_syncobj_the_host_cannot_be_asked_about_stays_counted_until_it_can() {
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.watch_by(&host, &mut t, 3, key(1, 6, 0), C2, p(1))
            .unwrap();
        *host.blind.borrow_mut() = true;
        destroy(&mut r, &host, 1, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r.releasing_for_test(), 2);
        assert!(
            host.holds(0) && host.holds(1),
            "none let go while any is unsure"
        );
        // The next watch has a DRM file to ask through.
        *host.blind.borrow_mut() = false;
        r.watch_by(&host, &mut t, 3, key(2, 5, 0), C3, p(1))
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r.held_by(p(1)), 1);
        assert!(!host.holds(0) && !host.holds(1));
    }

    #[test]
    fn a_syncobj_sent_to_the_compositor_is_let_go_when_its_channel_closes() {
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.exported(20, 1, 50);
        r.sent(50, 77);
        r.file_closed(50);
        destroy(&mut r, &host, 1, 0);
        assert_eq!(r.len(), 1, "the compositor may still hold and signal it");
        r.channel_closed(78);
        r.settle(&host, 3);
        assert_eq!(r.len(), 1);
        r.channel_closed(77);
        r.settle(&host, 3);
        assert!(r.is_empty());
        assert!(!host.holds(0));
        // One sent with nothing registered on it is not remembered for the
        // channel's sake: nothing could be let go at its close.
        r.exported(20, 2, 51);
        r.sent(51, 77);
        r.file_closed(51);
        destroy(&mut r, &host, 2, 0);
        assert!(r.reach.objs.is_empty() && r.reach.sent.is_empty());
        // Nor one whose registrations all fired.
        r.watch_by(&host, &mut t, 3, key(3, 5, 0), C2, p(1))
            .unwrap();
        r.exported(20, 3, 52);
        r.sent(52, 77);
        r.file_closed(52);
        destroy(&mut r, &host, 3, 0);
        assert_eq!(r.len(), 1);
        host.fire(1);
        r.sweep();
        assert!(r.is_empty());
        assert!(r.reach.objs.is_empty() && r.reach.sent.is_empty());
    }

    #[test]
    fn a_syncobj_from_outside_waits_out_its_firing() {
        // One the capture helper holds, or a syncobj file the backend did
        // not make: whoever that is may keep it alive, and signal it, as
        // long as they like; dropping a registration on it would leave an
        // uncounted entry.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        import(&mut r, 20, 4, Some(99));
        r.foreign_handle(20, 5);
        assert!(r.is_foreign_for_test(20, 4) && r.is_foreign_for_test(20, 5));
        r.watch_by(&host, &mut t, 3, key(4, 5, 0), C1, p(1))
            .unwrap();
        r.watch_by(&host, &mut t, 3, key(5, 5, 0), C2, p(1))
            .unwrap();
        destroy(&mut r, &host, 4, 0);
        r.orphan_file(20);
        r.settle(&host, 3);
        assert_eq!(r.len(), 2);
        assert!(host.holds(0) && host.holds(1));
        host.fire(0);
        host.fire(1);
        r.sweep();
        assert!(r.is_empty());
        assert_eq!(r.held_by(p(1)), 0);
    }

    #[test]
    fn a_number_whose_end_went_unseen_is_never_joined_nor_let_go() {
        // The host gave out a number the backend thought live: something
        // ended the old one unseen. Its registration is then another
        // syncobj's, and nothing says who else holds either.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::default());
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.after_ioctl2(
            20,
            "SYNCOBJ_CREATE",
            Some(&1u32.to_le_bytes()),
            Some(0),
            None,
        );
        assert_eq!(
            r.watch_by(&host, &mut t, 3, key(1, 5, 0), C2, p(1)),
            Ok(Watched::New)
        );
        r.orphan_file(20);
        r.settle(&host, 3);
        assert_eq!(r.len(), 1, "the old one waits out its firing");
        assert!(host.holds(0) && !host.holds(1));
    }

    #[test]
    fn only_a_syncobj_file_export_that_ran_is_an_export() {
        let file = handle_arg(7, 0);
        let sync_file = handle_arg(7, SYNC_FILE_MODE);
        let e = |name, arg: &[u8], r| exported_handle(name, Some(arg), r);
        assert_eq!(e("SYNCOBJ_HANDLE_TO_FD", &file, Some(0)), Some(7));
        assert_eq!(e("SYNCOBJ_HANDLE_TO_FD", &sync_file, Some(0)), None);
        assert_eq!(e("SYNCOBJ_HANDLE_TO_FD", &file, Some(-libc::EINVAL)), None);
        assert_eq!(e("SYNCOBJ_HANDLE_TO_FD", &file, None), None);
        assert_eq!(e("SYNCOBJ_FD_TO_HANDLE", &file, Some(0)), None);
        assert_eq!(
            exported_handle("SYNCOBJ_HANDLE_TO_FD", Some(&[0; 6]), Some(0)),
            None
        );
        for n in [
            "SYNCOBJ_CREATE",
            "SYNCOBJ_DESTROY",
            "SYNCOBJ_FD_TO_HANDLE",
            "SYNCOBJ_HANDLE_TO_FD",
        ] {
            assert!(changes_reach(n));
        }
        for n in [
            "SYNCOBJ_WAIT",
            "SYNCOBJ_QUERY",
            "SYNCOBJ_TIMELINE_SIGNAL",
            "GEM_CLOSE",
        ] {
            assert!(!changes_reach(n));
        }
    }

    #[test]
    fn past_the_tracking_bound_what_is_not_followed_waits_out_its_firing() {
        // Room for two: handle 1 and its file. The import of the file can
        // not be followed, so syncobj 1 may be reached through a handle the
        // backend does not count, and any handle of the file it has no
        // entry for may be that one.
        let host = Host::default();
        let mut t = Table::default();
        let mut r = Registrations::with_tracking_for_test(2);
        r.watch_by(&host, &mut t, 3, key(1, 5, 0), C1, p(1))
            .unwrap();
        r.exported(20, 1, 50);
        import(&mut r, 20, 2, Some(50));
        assert!(r.is_foreign_for_test(20, 1) && r.is_foreign_for_test(20, 2));
        r.watch_by(&host, &mut t, 3, key(2, 5, 0), C2, p(1))
            .unwrap();
        destroy(&mut r, &host, 1, 0);
        r.file_closed(50);
        destroy(&mut r, &host, 2, 0);
        r.orphan_file(20);
        r.settle(&host, 3);
        assert_eq!(r.len(), 2);
        assert!(host.holds(0) && host.holds(1));
        host.fire(0);
        host.fire(1);
        r.sweep();
        assert!(r.is_empty());
        assert!(r.reach.objs.is_empty() && r.reach.untracked.is_empty());
    }

    #[test]
    fn orphans_count_against_their_makers_share_and_the_reserve_still_serves_light_users() {
        // Each process's registrations, orphans on syncobjs its files keep
        // alive included, come out of its own share; once the heavy ones
        // have taken all but the reserve, a light process still gets its
        // floor of it.
        let host = Host::default();
        let (mut t, mut r) = (Table::default(), Registrations::with_cap(64));
        let share = Share::quarter(64, 1);
        let mut cookie = 1u64 << 32;
        let mut make = |r: &mut Registrations, t: &mut Table, o: Owner, render: u32| {
            let mut n = 0;
            loop {
                cookie += 1;
                let k = RegKey {
                    render,
                    syncobj: 1,
                    point: cookie,
                    flags: 0,
                };
                match r.watch_by(&host, t, 3, k, cookie, o) {
                    Ok(Watched::New) => n += 1,
                    Err(libc::EAGAIN) => return n,
                    other => panic!("{other:?}"),
                }
            }
        };
        let mut takes = Vec::new();
        for i in 0..4u32 {
            let render = 100 + i;
            takes.push(make(&mut r, &mut t, p(i + 1), render));
            // Exported, then its handle destroyed: every one an orphan its
            // file keeps counted.
            r.exported(render, 1, 200 + i);
            destroy_in(&mut r, &host, render, 1, 0);
        }
        assert_eq!(takes[..3], [share.per_owner; 3]);
        assert_eq!(
            takes[3],
            64 - share.reserve - 3 * share.per_owner,
            "down to the reserve"
        );
        assert_eq!(r.len() as u64, 64 - share.reserve);
        // A light process: its floor of the reserve.
        assert_eq!(make(&mut r, &mut t, p(9), 109), share.floor);
        // The heavy ones' processes exit: their files close, and everything
        // they made goes.
        for i in 0..4u32 {
            r.orphan_file(100 + i);
            r.file_closed(200 + i);
        }
        r.settle(&host, 3);
        for i in 0..4u32 {
            assert_eq!(r.held_by(p(i + 1)), 0);
        }
        assert_eq!(r.len() as u64, share.floor);
        assert_eq!(make(&mut r, &mut t, p(10), 110), share.per_owner);
    }

    #[test]
    fn flags_other_than_wait_available_are_refused() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        for flags in [WAIT_FOR_SUBMIT, 1, 1 << 3, u32::MAX] {
            assert_eq!(
                r.watch(&host, &mut t, 3, key(1, 1, flags), C1),
                Err(libc::EINVAL)
            );
        }
        assert!(host.registered.borrow().is_empty());
    }

    #[test]
    fn a_cookie_in_the_legacy_range_or_already_reporting_is_refused() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), 7),
            Err(libc::EINVAL),
            "a cookie <= u32::MAX would be delivered to a legacy handle"
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), C1),
            Ok(Watched::New)
        );
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 1, 0), C1),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn a_syncobj_the_file_does_not_have_registers_nothing() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        assert_eq!(
            r.watch(&host, &mut t, 3, key(42, 1, 0), C1),
            Err(libc::ENOENT),
            "as SYNCOBJ_EVENTFD answers for a handle the file does not have"
        );
        assert!(t.watched.is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn a_failed_registration_gives_its_handle_back() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        *host.fail_register.borrow_mut() = Some(libc::ENOMEM);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), C1),
            Err(libc::ENOMEM)
        );
        assert_eq!(t.retired, vec![1]);
        assert!(t.live.is_empty());
        assert!(r.is_empty());
        // And a full table is the table's error, with nothing registered.
        *host.fail_register.borrow_mut() = None;
        t.full = true;
        assert_eq!(
            r.watch(&host, &mut t, 3, key(1, 1, 0), C1),
            Err(libc::EMFILE)
        );
        assert!(host.registered.borrow().is_empty());
    }

    #[test]
    fn the_eventfd_is_watched_before_the_kernel_can_signal_it() {
        // SYNCOBJ_EVENTFD on a point that is already signalled signals inside
        // the ioctl; the pump's watch must exist by then or the edge is lost.
        struct Order<'a>(&'a RefCell<Vec<&'static str>>);
        impl SyncobjHost for Order<'_> {
            fn syncobj_file(&self, _: RawFd, _: u32) -> io::Result<OwnedFd> {
                Ok(devnull())
            }
            fn register(&self, _: RawFd, _: u32, _: u64, _: u32, _: RawFd) -> io::Result<()> {
                self.0.borrow_mut().push("register");
                Ok(())
            }
            fn available(&self, _: RawFd, _: BorrowedFd<'_>, _: u64) -> io::Result<bool> {
                Ok(false)
            }
        }
        struct OrderTable<'a>(&'a RefCell<Vec<&'static str>>);
        impl RegTable for OrderTable<'_> {
            fn publish(&mut self, _: OwnedFd, _: u64, _: Owner) -> Result<u32, Errno> {
                self.0.borrow_mut().push("publish");
                Ok(1)
            }
            fn retire(&mut self, _: u32) {}
        }
        let log = RefCell::new(Vec::new());
        let mut r = Registrations::default();
        r.watch(&Order(&log), &mut OrderTable(&log), 3, key(1, 1, 0), C1)
            .unwrap();
        assert_eq!(*log.borrow(), vec!["publish", "register"]);
    }

    #[test]
    fn clearing_forgets_everything_without_touching_the_table() {
        let (host, mut t, mut r) = (Host::default(), Table::default(), Registrations::default());
        r.watch(&host, &mut t, 3, key(1, 1, 0), C1).unwrap();
        r.watch(&host, &mut t, 3, key(2, 1, 0), C2).unwrap();
        host.fire(0);
        r.sweep();
        r.clear();
        assert!(r.is_empty());
        r.reap(&mut t);
        assert!(t.retired.is_empty());
    }

    // ── the backend's table ──

    #[test]
    fn a_published_eventfd_is_watched_once_and_not_drained() {
        let mut be = NvidiaBackend::for_test();
        let p = Owner::Proc {
            tgid: 7,
            start_ns: 1,
        };
        let h = be.publish(hostfd::new_eventfd().unwrap(), C1, p).unwrap();
        assert_eq!(be.handles.kind(h), Some(HandleKind::Eventfd));
        // Charged to the process that asked.
        assert_eq!(be.handles.owner(h), p);
        assert_eq!(be.handles.held_by(p), 1);
        let cmds = be.take_pump_cmds();
        assert!(matches!(
            cmds.as_slice(),
            [PumpCmd::Watch {
                handle,
                mode: WatchMode::Ready {
                    cookie: C1,
                    oneshot: true,
                    consume: false,
                },
                ..
            }] if *handle == h
        ));
        be.retire(h);
        assert_eq!(be.handles.kind(h), None);
        // Its report kept, and its slot until the pump lets it go.
        assert_eq!(be.handles.held_by(p), 0);
        assert_eq!(be.handles.charged_to(p), 1);
        let cmds = be.take_pump_cmds();
        assert!(matches!(
            cmds.as_slice(),
            [PumpCmd::Retire { handle, .. }] if *handle == h
        ));
        drop(cmds);
        assert_eq!(be.handles.charged_to(p), 0);
    }

    /// nvgpu-syncobj-race's owner round, on the backend's table: SYNCOBJ_
    /// EVENTFD on a fresh syncobj, its point signalled, HANDLE_TO_FD and the
    /// guest's CLOSE of that file, the posted DESTROY. The process holds a
    /// few objects at a time, so it may make as many rounds as it likes. A
    /// fired registration's handle used to stay a second after it was seen
    /// fired, charged to the process: at the rate the probe fires them
    /// (16 thousand a second) that was its whole quarter of the table, and
    /// both SYNCOBJ_EVENTFD and HANDLE_TO_FD said EMFILE. Now it goes when
    /// the next call finds it fired, and its charge when the pump has sent
    /// its report.
    #[test]
    fn a_process_firing_registrations_fast_holds_only_what_is_unfired_or_unsent() {
        let host = Host {
            untracked: true,
            ..Host::default()
        };
        let mut be = NvidiaBackend::for_test();
        // A quarter of it, 1024, per process; REGISTRATION_SHARE is 256.
        be.handles.set_limit(4096);
        let p = Owner::Proc {
            tgid: 148,
            start_ns: 1,
        };
        let render = be.adopt_for_test_as(devnull(), HandleKind::DriRender(0), p);
        let mut regs = std::mem::take(&mut be.syncobj_regs);
        let mut peak = (0, 0);
        for round in 0..4096u64 {
            let key = RegKey {
                render,
                syncobj: 1 + (round % 9) as u32,
                point: round + 1,
                flags: 0,
            };
            let before = be.handles.held_by(p);
            // SYNCOBJ_CREATE runs on the host alone. SYNCOBJ_EVENTFD: a
            // registration, and its eventfd's handle.
            assert_eq!(
                regs.watch_by(&host, &mut be, 3, key, (1 << 32) | (round + 1), p),
                Ok(Watched::New),
                "round {round}: held {}",
                be.handles.held_by(p)
            );
            if round == 0 {
                assert_eq!(be.handles.held_by(p), before + 1);
            }
            // The point signalled: the kernel writes the eventfd.
            host.fire(round as usize);
            // HANDLE_TO_FD: a syncobj file, adopted for the guest, which
            // closes it at once.
            let file = be.adopt_for_test_as(devnull(), HandleKind::Syncobj, p);
            regs.exported(render, key.syncobj, file);
            regs.file_closed(file);
            be.close_handle(file).unwrap();
            // The posted DESTROY. Its last handle, so the registration goes
            // to be let go, and is dropped once it is found fired.
            let d = key.syncobj.to_le_bytes();
            regs.before_ioctl2(render, "SYNCOBJ_DESTROY", Some(&d));
            regs.after_ioctl2(render, "SYNCOBJ_DESTROY", Some(&d), Some(0), None);
            if round == 0 {
                assert_eq!(be.handles.held_by(p), before + 1, "until swept");
            }
            peak.0 = peak.0.max(be.handles.held_by(p));
            peak.1 = peak.1.max(be.handles.charged_to(p));
            // The pump sends the reports it was handed.
            drop(be.take_pump_cmds());
            assert_eq!(be.handles.charged_to(p), be.handles.held_by(p));
        }
        // The render file, the share's worth of registrations not yet
        // swept, and the one the round made.
        let most = 1 + REGISTRATION_SHARE.per_owner + 1;
        assert!(peak.0 <= most, "{} handles held", peak.0);
        assert!(peak.1 <= most + 1, "{} charged", peak.1);
        be.syncobj_regs = regs;
    }

    /// A DESTROY that drops a registration (its syncobj only the file
    /// could reach) hands its handle back then, not at the next watch.
    #[test]
    fn a_dropped_registrations_handle_goes_back_with_the_destroy() {
        let host = Host::default();
        let mut be = NvidiaBackend::for_test();
        be.syncobj_host = std::sync::Arc::new(NoFences);
        let p = Owner::Proc {
            tgid: 9,
            start_ns: 1,
        };
        let render = be.adopt_for_test_as(devnull(), HandleKind::DriRender(0), p);
        let mut regs = std::mem::take(&mut be.syncobj_regs);
        let key = RegKey {
            render,
            syncobj: 1,
            point: 5,
            flags: 0,
        };
        regs.watch_by(&host, &mut be, 3, key, C1, p).unwrap();
        let d = 1u32.to_le_bytes();
        regs.before_ioctl2(render, "SYNCOBJ_DESTROY", Some(&d));
        regs.after_ioctl2(render, "SYNCOBJ_DESTROY", Some(&d), Some(0), None);
        be.syncobj_regs = regs;
        assert_eq!(be.handles.held_by(p), 2);
        be.take_pump_cmds();
        // As the IOCTL2's finish does: asked through the render file.
        be.settle_syncobj_regs(None);
        assert!(be.syncobj_regs.is_empty());
        be.reap_syncobj_regs();
        assert_eq!(be.handles.held_by(p), 1, "the render file only");
        assert!(matches!(
            be.take_pump_cmds().as_slice(),
            [PumpCmd::Retire { .. }]
        ));
        assert_eq!(be.handles.charged_to(p), 1);
    }

    /// nvgpu-syncobj-race's guessers, on the backend's table: a process
    /// subscribes to points of a syncobj it exported and imported back, and
    /// another of its threads destroys the handle it subscribed through
    /// before any point comes. Its registrations are orphans on a syncobj
    /// its import and its file keep alive, counted against its own share
    /// and no one else's. When it exits -- its render file and syncobj file
    /// close -- nothing of the guest's reaches the syncobj, and every one is
    /// let go: the next process has the whole of its share, however many
    /// went before. They used to be kept for good, and four runs of the
    /// probe filled the VM's pool.
    #[test]
    fn a_process_that_exits_leaves_no_registration_behind() {
        let host = Host {
            untracked: true,
            ..Host::default()
        };
        let mut be = NvidiaBackend::for_test();
        be.handles.set_limit(4096);
        be.syncobj_host = std::sync::Arc::new(NoFences);
        be.syncobj_regs = Registrations::with_cap(64);
        let share = Share::quarter(64, 1).per_owner;
        // Another process, with a render file of its own throughout.
        let q = Owner::Proc {
            tgid: 999,
            start_ns: 1,
        };
        let other = be.adopt_for_test_as(devnull(), HandleKind::DriRender(0), q);
        for run in 0..8u32 {
            let p = Owner::Proc {
                tgid: 200 + run,
                start_ns: 1,
            };
            let render = be.adopt_for_test_as(devnull(), HandleKind::DriRender(0), p);
            let file = be.adopt_for_test_as(devnull(), HandleKind::Syncobj, p);
            let mut regs = std::mem::take(&mut be.syncobj_regs);
            // Syncobj 1 exported as `file`, imported back as handle 2.
            regs.exported(render, 1, file);
            import(&mut regs, render, 2, Some(file));
            let cookie = |n: u64| (1 << 32) | (u64::from(run) << 16) | n;
            let mut n = 0u64;
            loop {
                let k = RegKey {
                    render,
                    syncobj: 1,
                    point: n + 1,
                    flags: 0,
                };
                match regs.watch_by(&host, &mut be, 3, k, cookie(n + 1), p) {
                    Ok(Watched::New) => n += 1,
                    Err(libc::EAGAIN) => break,
                    other => panic!("run {run}: {other:?}"),
                }
            }
            assert_eq!(n, share, "run {run}: its whole share, whatever went before");
            // The guesser's DESTROY: orphans, still reached through handle 2
            // and the file.
            destroy_in(&mut regs, &host, render, 1, 0);
            assert_eq!(regs.held_by(p), share);
            // Another process is not affected.
            let kq = RegKey {
                render: other,
                syncobj: 3,
                point: u64::from(run) + 1,
                flags: 0,
            };
            assert_eq!(
                regs.watch_by(&host, &mut be, 3, kq, cookie(0xffff), q),
                Ok(Watched::New)
            );
            be.syncobj_regs = regs;
            // The exit: its files close, in whatever order.
            let (a, b) = if run % 2 == 0 {
                (file, render)
            } else {
                (render, file)
            };
            be.close_handle(a).unwrap();
            be.close_handle(b).unwrap();
            be.reap_syncobj_regs();
            assert_eq!(be.syncobj_regs.held_by(p), 0, "run {run}");
            assert_eq!(be.syncobj_regs.held_by(q), u64::from(run) + 1, "run {run}");
            assert_eq!(
                be.syncobj_regs.len(),
                run as usize + 1,
                "run {run}: q's alone"
            );
            drop(be.take_pump_cmds());
            assert_eq!(be.handles.held_by(p), 0, "run {run}: every handle back");
        }
    }

    /// The host's own answer, on a real DRM file: a point with a fence
    /// and one without, and nothing left in the file after the question.
    /// Needs a render node (NVGPU_TEST_RENDER, default renderD128).
    #[test]
    #[ignore = "needs a DRM render node"]
    fn the_host_says_whether_a_point_has_a_fence_and_leaves_no_handle() {
        let path = std::env::var("NVGPU_TEST_RENDER").unwrap_or("/dev/dri/renderD128".into());
        let c = std::ffi::CString::new(path).unwrap();
        let render = crate::sys::fd::open(&c, libc::O_RDWR | libc::O_CLOEXEC).unwrap();
        let fd = render.as_raw_fd();
        // SYNCOBJ_CREATE, and its file.
        let mut create = [0u8; 8];
        let mut a = Arena::new();
        let top = a.small(&create);
        drm_ioctl(fd, ioc(IOC_RW, b'd', 0xbf, 8), &mut a, top).unwrap();
        create.copy_from_slice(a.bytes(top));
        let h = u32::from_le_bytes(create[0..4].try_into().unwrap());
        let file = HostSyncobj.syncobj_file(fd, h).unwrap();
        assert!(!HostSyncobj.available(fd, file.as_fd(), 7).unwrap());
        // TIMELINE_SIGNAL of point 7: { u64 handles, points; u32 count, flags }.
        let mut sig = [0u8; 24];
        sig[16..20].copy_from_slice(&1u32.to_le_bytes());
        let mut a = Arena::new();
        let top = a.small(&sig);
        let hs = a.small(&h.to_le_bytes());
        let ps = a.small(&7u64.to_le_bytes());
        a.ptr(top, 0).unwrap();
        a.point(top, 0, hs).unwrap();
        a.ptr(top, 8).unwrap();
        a.point(top, 8, ps).unwrap();
        drm_ioctl(fd, ioc(IOC_RW, b'd', 0xcd, 24), &mut a, top).unwrap();
        assert!(HostSyncobj.available(fd, file.as_fd(), 7).unwrap());
        assert!(!HostSyncobj.available(fd, file.as_fd(), 8).unwrap());
        // Only `h` is left: the next CREATE gets the number after it, and
        // a DESTROY of that number fails once it is gone.
        let mut a = Arena::new();
        let top = a.small(&[0u8; 8]);
        drm_ioctl(fd, ioc(IOC_RW, b'd', 0xbf, 8), &mut a, top).unwrap();
        let h2 = u32::from_le_bytes(a.bytes(top)[0..4].try_into().unwrap());
        assert_eq!(h2, h + 1);
        hostfd::syncobj_destroy(fd, h2).unwrap();
        hostfd::syncobj_destroy(fd, h).unwrap();
        assert!(hostfd::syncobj_destroy(fd, h + 1).is_err());
        assert!(hostfd::syncobj_destroy(fd, h + 2).is_err());
    }

    #[test]
    fn retiring_leaves_a_handle_that_is_no_longer_an_eventfd_alone() {
        // The guest closed the eventfd's handle and the number went to
        // something else: retiring must not close that.
        let mut be = NvidiaBackend::for_test();
        let h = be.adopt_for_test(devnull(), HandleKind::SyncFile);
        be.retire(h);
        assert_eq!(be.handles.kind(h), Some(HandleKind::SyncFile));
    }

    #[test]
    fn a_watch_on_a_file_that_is_not_a_drm_node_fails_on_the_host() {
        // /dev/null takes no DRM ioctl: the host's error comes back, and no
        // handle, watch or slot is left behind.
        let mut be = NvidiaBackend::for_test();
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let key = RegKey {
            render,
            syncobj: 1,
            point: 0,
            flags: 0,
        };
        assert_eq!(be.syncobj_watch(key, C1), Err(libc::ENOTTY));
        assert_eq!(be.handle_count(), 1);
        assert!(be.syncobj_regs.is_empty());
        let missing = RegKey { render: 999, ..key };
        assert_eq!(be.syncobj_watch(missing, C1), Err(libc::EBADF));
    }

    // ── the policy ──

    fn wait_arg(timeout: i64) -> Vec<u8> {
        let mut a = vec![0u8; 40];
        a[8..16].copy_from_slice(&timeout.to_le_bytes());
        a
    }

    #[test]
    fn a_forwarded_wait_becomes_a_poll() {
        let mut a = wait_arg(i64::MAX);
        a[24..28].copy_from_slice(&(WAIT_FOR_SUBMIT | 1).to_le_bytes());
        a[32..40].copy_from_slice(&1234u64.to_le_bytes());
        assert_eq!(before(SYNCOBJ_WAIT, &mut a), Ok(()));
        assert_eq!(&a[8..16], &[0; 8], "timeout_nsec = 0");
        assert_eq!(
            u32::from_le_bytes(a[24..28].try_into().unwrap()),
            WAIT_FOR_SUBMIT | 1,
            "flags as sent: with a zero timeout none of them waits"
        );
        assert_eq!(
            &a[32..40],
            &1234u64.to_le_bytes(),
            "the deadline hint stays"
        );

        let mut t = vec![0u8; 48];
        t[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        t[8..16].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(before(SYNCOBJ_TIMELINE_WAIT, &mut t), Ok(()));
        assert_eq!(&t[16..24], &[0; 8]);
        assert_eq!(&t[8..16], &7u64.to_le_bytes(), "points pointer untouched");
    }

    #[test]
    fn a_transfer_that_would_wait_for_submission_is_refused() {
        let mut a = vec![0u8; 32];
        assert_eq!(before(SYNCOBJ_TRANSFER, &mut a), Ok(()));
        a[24..28].copy_from_slice(&WAIT_FOR_SUBMIT.to_le_bytes());
        assert_eq!(before(SYNCOBJ_TRANSFER, &mut a), Err(libc::EINVAL));
    }

    #[test]
    fn syncobj_eventfd_is_never_forwarded() {
        assert_eq!(before(SYNCOBJ_EVENTFD, &mut [0u8; 24]), Err(libc::EPERM));
    }

    #[test]
    fn calls_that_never_wait_pass_untouched() {
        let create = ioc(IOC_RW, b'd', 0xbf, 8);
        let semsurf_wait = ioc(hostfd::IOC_W, b'd', 0x56, 24);
        for cmd in [create, semsurf_wait] {
            let mut a = vec![0xa5u8; hostfd::ioc_size(cmd)];
            assert_eq!(before(cmd, &mut a), Ok(()));
            assert!(a.iter().all(|&b| b == 0xa5));
        }
    }

    #[test]
    fn a_short_argument_is_refused_rather_than_read_past() {
        assert_eq!(before(SYNCOBJ_WAIT, &mut [0u8; 12]), Err(libc::EINVAL));
        assert_eq!(before(SYNCOBJ_TRANSFER, &mut [0u8; 26]), Err(libc::EINVAL));
    }
}
