//! Fences on the backend: what a guest's syncobj and semaphore-surface calls
//! may do to a host thread, and the shared wait registrations that let the
//! guest sleep on a syncobj point without one (DESIGN §6, R:fences §3.5).
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
//!   fired, counted until they fire. Over it the guest falls back to polling
//!   with a short backoff, which costs it latency and the host nothing;
//! - **the syncobj kept alive** by the registration (a syncobj file of our
//!   own), so an entry can only ever end by firing -- which we see -- and never
//!   by the syncobj being freed behind our back, which we would not: without
//!   it, destroy-and-reimport would let a guest leave entries on a syncobj it
//!   keeps alive through an exported fd, uncounted.
//!
//! Userspace SYNCOBJ_EVENTFD (a compositor in the guest waiting on acquire
//! points) is served by the same registrations; the guest signals its own
//! eventfd when the shared one fires. The ioctl itself is never forwarded.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crate::hostfd::{self, HandleKind, IOC_RW, ioc};
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;
use crate::pump::{PumpCmd, WatchMode};
use crate::xfer::Errno;

/// Registrations that have not fired, per VM. Each holds two backend
/// descriptors (the eventfd, twice: the table's and ours; and the syncobj
/// file) and one host kernel entry with an eventfd context. A compositor has
/// a handful outstanding per surface; this is far above any honest workload
/// and far below anything that matters to the host.
pub const REGISTRATION_CAP: usize = 1024;

/// How long a fired registration's eventfd handle outlives the registration.
///
/// Closing a handle unwatches it in the pump, and an unwatch drops whatever
/// the pump had not yet reported for it (`pump::Outbox::forget`). A
/// registration is seen to have fired when its eventfd is readable -- which
/// may be a moment *before* the pump has read that readiness and queued the
/// EV_READY every guest waiter on the cookie sleeps for. Closing then would
/// lose their wakeup. The pump reports within one round of its loop, far
/// under this; after it, the one-shot watch is gone and the close is only a
/// close.
pub const RETIRE_GRACE: Duration = Duration::from_secs(1);

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
            let flags = arg
                .get(TRANSFER_FLAGS_AT..TRANSFER_FLAGS_AT + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or(libc::EINVAL)?;
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

// ───────────────────────────── registrations ─────────────────────────────

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

/// The two host calls a registration makes, so the bookkeeping can be tested
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
}

/// The host's.
pub struct HostSyncobj;

impl SyncobjHost for HostSyncobj {
    fn syncobj_file(&self, render: RawFd, syncobj: u32) -> io::Result<OwnedFd> {
        // struct drm_syncobj_handle { u32 handle, flags; s32 fd; u32 pad;
        // u64 point; }
        let mut arg = [0u8; 24];
        arg[0..4].copy_from_slice(&syncobj.to_le_bytes());
        arg[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        drm_ioctl(render, hostfd::DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut arg)?;
        let fd = i32::from_le_bytes(arg[8..12].try_into().unwrap());
        if fd < 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        // SAFETY: the kernel just installed `fd` for us (drm_syncobj_get_fd,
        // drm_syncobj.c:687, with O_CLOEXEC); nothing else owns it.
        Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) })
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
        drm_ioctl(render, SYNCOBJ_EVENTFD, &mut arg)
    }
}

fn drm_ioctl(fd: RawFd, cmd: u32, arg: &mut [u8]) -> io::Result<()> {
    debug_assert_eq!(arg.len(), hostfd::ioc_size(cmd));
    loop {
        // SAFETY: `arg` is exactly _IOC_SIZE(cmd) bytes and holds no pointer.
        let r = unsafe { libc::ioctl(fd, cmd as libc::Ioctl, arg.as_mut_ptr()) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Where a registration's eventfd is watched from: the handle table and the
/// event pump, which the backend owns.
pub trait RegTable {
    /// Give `eventfd` a handle and have the pump report it once, as EV_READY
    /// with `cookie`, when it becomes readable.
    fn publish(&mut self, eventfd: OwnedFd, cookie: u64) -> Result<u32, Errno>;
    /// Take that handle back.
    fn retire(&mut self, handle: u32);
}

struct Reg {
    cookie: u64,
    /// The eventfd's handle, which the pump watches.
    handle: u32,
    /// Our own descriptor for the same eventfd, to see whether it fired: the
    /// pump's watch is one-shot and does not drain it, so once signalled it
    /// stays readable.
    eventfd: PrivateFd,
    /// Keeps the syncobj, and with it the kernel entry, from ending in any
    /// way but firing.
    _syncobj: PrivateFd,
}

/// Every live registration of a session.
pub struct Registrations {
    regs: HashMap<RegKey, Reg>,
    /// Registrations whose key no longer names what they wait on: the
    /// syncobj handle was destroyed, or the render file closed. The host
    /// hands the number out again at once (lowest free, drm_syncobj.c:606),
    /// so a waiter on the new syncobj that joined one of these would sleep
    /// on a point of the old one, which may never fire (S-13). Never joined,
    /// only swept once fired; they keep their cap slot until then, since
    /// the kernel entry does too.
    orphans: Vec<Reg>,
    cap: usize,
    /// Handles of fired registrations, closed once [`RETIRE_GRACE`] has
    /// passed. Bounded by how fast registrations fire.
    retired: VecDeque<(u32, Instant)>,
    grace: Duration,
}

impl Default for Registrations {
    fn default() -> Self {
        Self::with_cap(REGISTRATION_CAP)
    }
}

impl Registrations {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            regs: HashMap::new(),
            orphans: Vec::new(),
            cap,
            retired: VecDeque::new(),
            grace: RETIRE_GRACE,
        }
    }

    /// A different grace before fired handles are closed (tests).
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Registrations that have not been seen to fire, orphans included.
    pub fn len(&self) -> usize {
        self.regs.len() + self.orphans.len()
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

    /// Render handle `render` is closing: every registration it made.
    pub fn orphan_file(&mut self, render: u32) {
        self.orphan_where(|k| k.render == render);
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
    pub fn watch(
        &mut self,
        host: &dyn SyncobjHost,
        table: &mut dyn RegTable,
        render_fd: RawFd,
        key: RegKey,
        cookie: u64,
    ) -> Result<Watched, Errno> {
        self.reap(table, Instant::now());
        if key.flags & !WAIT_AVAILABLE != 0 {
            return Err(libc::EINVAL);
        }
        // Cookies at or below u32::MAX name legacy watches by handle (the
        // guest routes them to an nvgpu_fd, nvgpu_xfer.c nvgpu_ev_record).
        if cookie <= u64::from(u32::MAX) {
            return Err(libc::EINVAL);
        }
        if let Some(r) = self.regs.get(&key) {
            if !fired(r.eventfd.as_raw_fd()) {
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
            .any(|r| r.cookie == cookie)
        {
            // Two points under one cookie would wake each other's waiters
            // and, worse, retire one another's guest bookkeeping.
            return Err(libc::EINVAL);
        }
        if self.len() >= self.cap {
            self.sweep();
            if self.len() >= self.cap {
                log::warn!(
                    "syncobj wait registrations at the cap ({}); the guest polls instead",
                    self.cap
                );
                return Err(libc::EAGAIN);
            }
        }

        // The kernel's only EINVAL here is a handle the file does not have
        // (drm_syncobj.c:694); SYNCOBJ_EVENTFD itself says ENOENT for that
        // (:1479), and that is the call the guest is answering.
        let syncobj = PrivateFd::new(host.syncobj_file(render_fd, key.syncobj).map_err(|e| {
            match errno(&e) {
                libc::EINVAL => libc::ENOENT,
                other => other,
            }
        })?);
        let eventfd = PrivateFd::new(hostfd::new_eventfd().map_err(|e| errno(&e))?);
        let dup = eventfd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| errno(&e))?;
        // Watched before it is registered: the kernel may signal it inside
        // the ioctl (a point already signalled, drm_syncobj.c:1447-1453), and
        // the pump's watch looks at the descriptor once when it is armed.
        let handle = table.publish(dup, cookie)?;
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
                cookie,
                handle,
                eventfd,
                _syncobj: syncobj,
            },
        );
        Ok(Watched::New)
    }

    /// Forget every registration that has fired. Their slots are free at
    /// once; their handles close after the grace.
    pub fn sweep(&mut self) {
        let done: Vec<RegKey> = self
            .regs
            .iter()
            .filter(|(_, r)| fired(r.eventfd.as_raw_fd()))
            .map(|(k, _)| *k)
            .collect();
        for k in done {
            self.retire(&k);
        }
        let now = Instant::now();
        let (fired_orphans, live): (Vec<Reg>, Vec<Reg>) = std::mem::take(&mut self.orphans)
            .into_iter()
            .partition(|r| fired(r.eventfd.as_raw_fd()));
        self.orphans = live;
        self.retired
            .extend(fired_orphans.into_iter().map(|r| (r.handle, now)));
    }

    fn retire(&mut self, key: &RegKey) {
        if let Some(r) = self.regs.remove(key) {
            self.retired.push_back((r.handle, Instant::now()));
        }
    }

    /// Close the handles of registrations retired at least a grace ago.
    fn reap(&mut self, table: &mut dyn RegTable, now: Instant) {
        while let Some(&(h, at)) = self.retired.front() {
            if now.duration_since(at) < self.grace {
                break;
            }
            self.retired.pop_front();
            table.retire(h);
        }
    }

    /// The session is gone: drop every registration without touching the
    /// table, which the reset has emptied already (a handle number may belong
    /// to nobody now). The kernel entries on syncobjs that die with their
    /// files go with them; any other stays until its point fires, as it
    /// would for a native process that exited.
    pub fn clear(&mut self) {
        self.regs.clear();
        self.orphans.clear();
        self.retired.clear();
    }
}

fn errno(e: &io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// Whether an eventfd has been signalled: readable, without draining it.
fn fired(fd: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, zero timeout.
    unsafe { libc::poll(&mut p, 1, 0) > 0 && p.revents & libc::POLLIN != 0 }
}

// ─────────────────────────────── the backend ───────────────────────────────

impl RegTable for NvidiaBackend {
    fn publish(&mut self, eventfd: OwnedFd, cookie: u64) -> Result<u32, Errno> {
        let pump = eventfd.try_clone().map_err(|e| errno(&e))?;
        let handle = self
            .handles
            .insert(eventfd, HandleKind::Eventfd)
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
            let _ = self.close_handle(handle);
        }
    }
}

impl NvidiaBackend {
    /// HOST_OP SYNCOBJ_WATCH: register (or join) a wait on `key`, reported
    /// under `cookie`. Returns `[reporting cookie, 1 if joined]`.
    pub(crate) fn syncobj_watch(&mut self, key: RegKey, cookie: u64) -> Result<Vec<u64>, Errno> {
        let render_fd = self.handles.get_raw(key.render).map_err(|_| libc::EBADF)?;
        let mut regs = std::mem::take(&mut self.syncobj_regs);
        let r = regs.watch(&HostSyncobj, self, render_fd, key, cookie);
        self.syncobj_regs = regs;
        match r? {
            Watched::New => Ok(vec![cookie, 0]),
            Watched::Joined(c) => Ok(vec![c, 1]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::os::fd::FromRawFd;

    fn devnull() -> OwnedFd {
        // SAFETY: a NUL-terminated path; the result is owned below.
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: a descriptor open() just returned.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    /// A host with syncobjs 1..=9, recording registrations, and able to fire
    /// the eventfd a registration was given.
    #[derive(Default)]
    struct Host {
        registered: RefCell<Vec<(u32, u64, u32, RawFd)>>,
        fail_register: RefCell<Option<i32>>,
        files: RefCell<usize>,
    }

    impl SyncobjHost for Host {
        fn syncobj_file(&self, _: RawFd, syncobj: u32) -> io::Result<OwnedFd> {
            if !(1..=9).contains(&syncobj) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            *self.files.borrow_mut() += 1;
            Ok(devnull())
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
            // SAFETY: an 8-byte write to an eventfd the registration holds.
            assert_eq!(unsafe { libc::write(fd, one.as_ptr().cast(), 8) }, 8);
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
        fn publish(&mut self, _: OwnedFd, cookie: u64) -> Result<u32, Errno> {
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
        let mut r = Registrations::default().with_grace(Duration::ZERO);
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
        // The old handle closes on the next call, the grace (zero here)
        // having passed.
        assert!(t.retired.is_empty());
        r.watch(&host, &mut t, 3, key(1, 5, 0), C3).unwrap();
        assert_eq!(t.retired, vec![1]);
    }

    #[test]
    fn a_fired_handle_stays_open_until_the_pump_has_had_its_chance() {
        // Closing unwatches, and an unwatch drops a report the pump has not
        // queued yet -- the wakeup every waiter on the cookie sleeps for.
        let (host, mut t) = (Host::default(), Table::default());
        let mut r = Registrations::default();
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        host.fire(0);
        r.watch(&host, &mut t, 3, key(1, 5, 0), C2).unwrap();
        r.watch(&host, &mut t, 3, key(2, 5, 0), C3).unwrap();
        assert!(t.retired.is_empty(), "within the grace nothing is closed");
        r.reap(&mut t, Instant::now() + RETIRE_GRACE);
        assert_eq!(t.retired, vec![1]);
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
        r.reap(&mut t, Instant::now() + RETIRE_GRACE);
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
        r.reap(&mut t, Instant::now() + RETIRE_GRACE);
        assert_eq!(t.retired, vec![1], "its handle closes after the grace");
        let mut r = Registrations::default();
        r.watch(&host, &mut t, 3, key(1, 5, 0), C1).unwrap();
        r.orphan(20, 1);
        assert_eq!(
            r.watch(&host, &mut t, 3, key(2, 5, 0), C1),
            Err(libc::EINVAL),
            "an orphan still reports under its cookie"
        );
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
        }
        struct OrderTable<'a>(&'a RefCell<Vec<&'static str>>);
        impl RegTable for OrderTable<'_> {
            fn publish(&mut self, _: OwnedFd, _: u64) -> Result<u32, Errno> {
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
        r.reap(&mut t, Instant::now() + RETIRE_GRACE);
        assert!(t.retired.is_empty());
    }

    // ── the backend's table ──

    #[test]
    fn a_published_eventfd_is_watched_once_and_not_drained() {
        let mut be = NvidiaBackend::for_test();
        let h = be.publish(hostfd::new_eventfd().unwrap(), C1).unwrap();
        assert_eq!(be.handles.kind(h), Some(HandleKind::Eventfd));
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
        assert!(matches!(
            be.take_pump_cmds().as_slice(),
            [PumpCmd::Unwatch { handle }] if *handle == h
        ));
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
