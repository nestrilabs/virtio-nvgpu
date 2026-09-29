// SPDX-License-Identifier: Apache-2.0
//! One proxied connection to the host compositor.
//!
//! **Eager draining.** Hyprland gives each client a 1 MiB output buffer and
//! disconnects a client whose buffer fills (`Compositor.cpp:306`,
//! `wayland-server.c:228-256`). The guest reads only when it gets round to a
//! WL_RECV, so the connection cannot be read on the guest's schedule: a reader
//! thread per connection reads the socket as soon as it is readable, runs the
//! engine, and queues the translated records here until the guest takes them.
//! The queue is bounded (`max_queue`); a guest that stops reading altogether
//! loses the connection rather than the backend its memory.
//!
//! **Backpressure the other way** is the guest's: WL_SEND is refused with
//! `EAGAIN` while more than `max_backlog` bytes wait for a compositor that is
//! not reading, and the daemon stops reading its client until the backlog
//! drains -- the client's own libwayland buffer is where it waits.
//!
//! **Export mode** faces a host client, whose commits and blobs the engine
//! reads here and carries to the guest compositor. Those are not drained
//! eagerly: the engine reads them only as the guest's queue has room
//! ([`EXPORT_QUEUE`]), and takes no more of the client's input while
//! [`wlwire::engine::CHANNEL_HIGH_WATER`] bytes wait for the channel; the
//! rest waits in the client's socket, as it would for a slow compositor. A
//! client that commits its whole pool every frame costs the backend a queue's
//! worth, not a pool per commit.
//!
//! **Readiness** is an eventfd, readable while anything is queued for the
//! guest (records, or the HANGUP that ends the connection).
//!
//! **What a VM may hold** ([`WlLimits`]). Every limit above is per connection,
//! and a guest opens as many connections as it likes, so each one that costs
//! the host something is also counted per VM: channels (each a compositor
//! client, a reader thread and a handful of descriptors), shm buffer memory
//! (`wlwire::shm`, memfd pages the host OOM killer does not see as ours) and
//! bytes queued for the guest. A connection that would pass the VM's queue
//! budget is dropped like one that passes its own, and what it had queued is
//! let go with it: a guest that is not reading has no use for it.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wlwire::engine::{Blame, Engine, EngineConfig, Fatal, Local, Platform, Side};
use wlwire::frame::{self, Desc, DescOut, Unit};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::shm::{ShmBudget, ShmCharge};
use wlwire::sys;

use crate::hostfd::HandleKind;
use crate::wl::probe::{self, LeaseCache};

/// What the backend knows about a host descriptor.
pub trait HostFds: Send + Sync {
    /// `hostfd` classification: a DRM file must come out as `DrmLease` of
    /// *our* GPU to be carried at all.
    fn classify(&self, fd: BorrowedFd<'_>) -> HandleKind;
    /// A connection to the host compositor ended. The compositor is the
    /// usual lessor of the guest's leases, and its exit ends them in a way
    /// nvidia-drm does not follow (kms.rs, "lease ends"): the backend asks
    /// its leases now rather than at its next tick.
    fn compositor_hung_up(&self) {}
}

/// Called while a WL_SEND is processed.
pub trait SendOps {
    /// PRIME-export host GEM `gem` of the file behind backend handle `owner`
    /// (a guest file's render handle). The dma-buf goes to the compositor and
    /// is closed here after sending.
    fn prime_export(&mut self, owner: u32, gem: u32) -> io::Result<OwnedFd>;
    /// The host syncobj behind backend handle `handle`, for the compositor
    /// (explicit sync: `wp_linux_drm_syncobj_manager_v1.import_timeline`,
    /// offered only with `WlConfig::fences` and a guest that says
    /// `HELLO_G_SYNCOBJ`). Closed here after sending, like an export.
    fn syncobj(&mut self, _handle: u32) -> io::Result<OwnedFd> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Called while a WL_RECV is built.
pub trait RecvOps {
    /// Put a descriptor the compositor sent into the handle table, for the
    /// guest to adopt; `desc_kind` is `DESC_DRM_FILE` or `DESC_DMABUF`, and
    /// the descriptor must classify as that (a lease of our GPU; a dma-buf).
    /// Returns (handle, `HK_*`).
    fn adopt(&mut self, fd: OwnedFd, desc_kind: u16) -> io::Result<(u32, u32)>;
}

#[derive(Clone)]
pub struct WlConfig {
    /// The host compositor's socket (`--wayland-socket`).
    pub socket: PathBuf,
    /// Offer `wp_drm_lease_device_v1` (only ever for a device whose drm_fd is
    /// a file of our GPU, and only to a guest that can adopt DRM files).
    pub allow_lease: bool,
    /// Bytes waiting for the compositor before WL_SEND says EAGAIN.
    pub max_backlog: usize,
    /// Bytes waiting for the guest before the connection is dropped.
    pub max_queue: usize,
    /// Which lease-device globals are ours, shared by every connection.
    pub lease_cache: Arc<LeaseCache>,
    /// The backend serves fences (BCAP_FENCES): offer
    /// `wp_linux_drm_syncobj_manager_v1` to a guest that can name its
    /// syncobjs' host objects (`HELLO_G_SYNCOBJ`). Normal mode only: a host
    /// client's syncobj has no guest object to stand for it.
    pub fences: bool,
    /// What every connection of the VM shares. The dispatcher puts its own
    /// in (`WlState`), whichever configuration a connection was made from.
    pub limits: WlLimits,
    /// The guest process the connection is for (quota.rs): what its queue
    /// is charged to. Set by the dispatcher.
    pub owner: crate::quota::Owner,
    /// That process's share of the VM's shm budget, which every connection
    /// of it draws on in the VM's place ([`ShmShares`]). Set by the
    /// dispatcher; without it a connection draws on the VM's alone.
    pub owner_shm: Option<Arc<dyn ShmCharge>>,
}

impl WlConfig {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            allow_lease: false,
            max_backlog: 4 << 20,
            max_queue: 64 << 20,
            lease_cache: Arc::new(LeaseCache::default()),
            fences: false,
            limits: WlLimits::default(),
            owner: crate::quota::Owner::Unknown,
            owner_shm: None,
        }
    }
}

/// Bytes queued for the guest, over every connection of a VM, and what each
/// guest process's connections hold of it.
///
/// A connection past the budget is dropped. Counted only per VM, the one
/// dropped was whichever connection's output crossed the line -- often not
/// the one holding the unread bytes: a guest process that opened channels
/// and never read them made the next event of any other process's
/// connection hang that one up (W3). So each guest process the channels are
/// charged to (quota.rs) holds at most half the budget, and the last quarter
/// only while it holds at most a quarter: the connection that crosses its
/// process's share is that process's own.
#[derive(Debug)]
pub struct QueueBudget {
    max: usize,
    used: AtomicUsize,
    held: Mutex<crate::quota::Ledger>,
    share: crate::quota::Share,
}

impl QueueBudget {
    pub fn new(max: usize) -> Self {
        let m = max as u64;
        Self {
            max,
            used: AtomicUsize::new(0),
            held: Mutex::new(crate::quota::Ledger::default()),
            share: crate::quota::Share {
                per_owner: m / 2,
                reserve: m / 4,
                floor: m / 4,
            },
        }
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Bytes `owner`'s connections have queued.
    pub fn held_by(&self, owner: crate::quota::Owner) -> usize {
        self.ledger().held(owner) as usize
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, crate::quota::Ledger> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `n` more bytes for `owner`, if that stays within the VM's budget and
    /// the owner's share of it.
    fn take(&self, owner: crate::quota::Owner, n: usize) -> bool {
        let mut held = self.ledger();
        let ok = held
            .admits(
                &self.share,
                owner,
                n as u64,
                self.used() as u64,
                self.max as u64,
            )
            .is_ok();
        if ok {
            self.used.fetch_add(n, Ordering::AcqRel);
            held.charge(owner, n as u64);
        }
        ok
    }

    /// `n` more bytes whatever the budget says: the few bytes of the ERROR
    /// and HANGUP records that end a connection, which must reach the guest.
    fn force(&self, owner: crate::quota::Owner, n: usize) {
        let mut held = self.ledger();
        self.used.fetch_add(n, Ordering::AcqRel);
        held.charge(owner, n as u64);
    }

    fn give(&self, owner: crate::quota::Owner, n: usize) {
        let mut held = self.ledger();
        self.used.fetch_sub(n, Ordering::AcqRel);
        held.refund(owner, n as u64);
    }
}

/// The VM's shm budget as guest processes share it (quota.rs): what each
/// may hold of its bytes and of its pool count, with the last part of both
/// kept for processes that hold little.
///
/// A flat quarter each, with no reserve, let four processes -- one that
/// forked three times -- take the whole VM's pools or bytes, at no cost to
/// themselves (sparse buffers are charged by the pages they cover, never
/// written), and every other process's first pool was then fatal to it (the
/// 2026-09-29 review, S5). Now it takes four at their full share to reach
/// the reserve, and a process holding at most the floor can still make its
/// first pools and buffers out of that.
#[derive(Debug)]
pub struct ShmShares {
    vm: Arc<ShmBudget>,
    bytes: crate::quota::Share,
    pools: crate::quota::Share,
    /// (bytes, pools) each process holds.
    held: Mutex<(crate::quota::Ledger, crate::quota::Ledger)>,
}

impl ShmShares {
    pub fn new(vm: Arc<ShmBudget>) -> Self {
        let (bytes, pools) = vm.limits();
        Self {
            vm,
            bytes: crate::quota::Share::quarter(bytes, 1),
            pools: crate::quota::Share::quarter(pools, 8),
            held: Mutex::default(),
        }
    }

    /// (bytes, pools) `o`'s connections hold.
    pub fn held_by(&self, o: crate::quota::Owner) -> (u64, u64) {
        let h = self.held.lock().unwrap_or_else(|p| p.into_inner());
        (h.0.held(o), h.1.held(o))
    }

    /// The split of the VM's bytes, for tests and logs.
    pub fn byte_share(&self) -> crate::quota::Share {
        self.bytes
    }

    /// What `o`'s connections draw on: the VM's budget, within `o`'s share.
    pub fn for_owner(self: &Arc<Self>, o: crate::quota::Owner) -> Arc<dyn ShmCharge> {
        Arc::new(OwnerShm {
            shares: self.clone(),
            owner: o,
        })
    }
}

struct OwnerShm {
    shares: Arc<ShmShares>,
    owner: crate::quota::Owner,
}

impl ShmCharge for OwnerShm {
    fn take(&self, bytes: u64, pools: u64) -> bool {
        let s = &*self.shares;
        let mut held = s.held.lock().unwrap_or_else(|p| p.into_inner());
        let (used, (max_bytes, max_pools)) = (s.vm.used(), s.vm.limits());
        let ok = held
            .0
            .admits(&s.bytes, self.owner, bytes, used.0, max_bytes)
            .and_then(|()| {
                held.1
                    .admits(&s.pools, self.owner, pools, used.1, max_pools)
            })
            .is_ok();
        // The VM's own count is the outer bound, and atomic: another
        // process's connection may have taken from it since `used`.
        if !ok || !ShmCharge::take(&*s.vm, bytes, pools) {
            return false;
        }
        held.0.charge(self.owner, bytes);
        held.1.charge(self.owner, pools);
        true
    }

    fn give(&self, bytes: u64, pools: u64) {
        let s = &*self.shares;
        let mut held = s.held.lock().unwrap_or_else(|p| p.into_inner());
        ShmCharge::give(&*s.vm, bytes, pools);
        held.0.refund(self.owner, bytes);
        held.1.refund(self.owner, pools);
    }
}

/// A guest process's share of the queue budget, as the engine's stream sinks
/// draw on it: bytes a guest sends into a pipe the compositor's side has not
/// read yet are host memory the guest decides, like its unread output.
struct OwnerQueue {
    queue: Arc<QueueBudget>,
    owner: crate::quota::Owner,
}

impl wlwire::stream::ByteBudget for OwnerQueue {
    fn take(&self, n: usize) -> bool {
        self.queue.take(self.owner, n)
    }
    fn give(&self, n: usize) {
        self.queue.give(self.owner, n)
    }
}

/// Limits over every Wayland channel of one VM (one backend). Cloning shares
/// the budgets.
#[derive(Clone, Debug)]
pub struct WlLimits {
    /// Channels open at once (CONNECT and ACCEPT; the one LISTEN is not
    /// counted). Each is a host compositor client with up to 131072 objects,
    /// a reader thread and about five descriptors (`--wayland-max-conns`).
    pub max_conns: usize,
    /// Shm memory (what live buffers cover) and pool count, over every
    /// connection (`--wayland-shm-budget`).
    pub shm: Arc<ShmBudget>,
    /// The same, as guest processes share it.
    pub shm_shares: Arc<ShmShares>,
    /// Bytes queued for the guest, over every connection
    /// (`--wayland-queue-budget`).
    pub queue: Arc<QueueBudget>,
    /// How often the VM's clients may submit a lease request
    /// (`--wayland-lease-interval`).
    pub lease: Arc<LeaseThrottle>,
}

/// How often one VM may ask the compositor for a lease.
///
/// A lease of a desktop monitor (`leasable` in the Hyprland patch) costs the
/// host a blocking modeset to take the output away, workspaces moved off it,
/// and a full modeset to take it back when the lease ends, all on the
/// compositor's main thread, plus a LEASE uevent to every listener. Nothing
/// in the protocol limits how often a client may do that, so a guest looping
/// request, submit, destroy stalls the host desktop and every other VM's
/// clients. Submits are therefore admitted at one per `interval` on average,
/// with `burst` at once (a Vulkan client acquiring two displays makes two
/// requests), over every connection of the VM together -- which bounds each
/// connection too.
///
/// A frame that carries a submit past the rate is refused whole with EAGAIN
/// before anything in it is looked at: the guest daemon keeps the frame and
/// retries it (as it does for a compositor that is not reading), so the
/// client's request is delayed, never lost or reordered, and the compositor
/// still creates and owns every lease object. Every submit of a frame must fit
/// the rate, not only its first, and a frame with more submits than `burst`
/// could never fit and ends the connection. How long a lease is then held is
/// not limited: holding the output is what a lease is for, and the host takes
/// it back by un-marking the monitor leasable or closing the VM.
#[derive(Debug)]
pub struct LeaseThrottle {
    interval: Duration,
    burst: u32,
    /// When the next submit is due at the average rate (GCRA's theoretical
    /// arrival time); a submit may go up to `burst - 1` intervals early.
    due: Mutex<Option<Instant>>,
    /// A refusal was logged since the last admission.
    logged: AtomicBool,
}

/// Why [`LeaseThrottle::admit`] said no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseRefusal {
    /// The frame's submits fit the rate once this has passed.
    Wait(Duration),
    /// More submits in one frame than the burst: they never fit.
    OverBurst,
}

impl LeaseThrottle {
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
    pub const DEFAULT_BURST: u32 = 3;

    /// `interval` zero admits everything.
    pub fn new(interval: Duration, burst: u32) -> Self {
        Self {
            interval,
            burst: burst.max(1),
            due: Mutex::new(None),
            logged: AtomicBool::new(false),
        }
    }

    /// Admit `n` submits at `now`, all of them or none: the last of them must
    /// conform to the rate, not only the first.
    pub fn admit(&self, n: usize, now: Instant) -> Result<(), LeaseRefusal> {
        if n == 0 || self.interval.is_zero() {
            return Ok(());
        }
        if n > self.burst as usize {
            return Err(LeaseRefusal::OverBurst);
        }
        let n = n as u32;
        let mut due = self.due.lock().unwrap_or_else(|p| p.into_inner());
        let t = due.map_or(now, |d| d.max(now));
        let early = self.interval * (self.burst - 1);
        let last = t + self.interval * (n - 1);
        if last > now + early {
            return Err(LeaseRefusal::Wait(last - early - now));
        }
        *due = Some(t + self.interval * n);
        self.logged.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Default for LeaseThrottle {
    fn default() -> Self {
        Self::new(Self::DEFAULT_INTERVAL, Self::DEFAULT_BURST)
    }
}

impl WlLimits {
    /// A guest desktop proxies a few dozen clients at most.
    pub const DEFAULT_MAX_CONNS: usize = 64;
    /// Ten triple-buffered 4K shm windows. GPU clients present dma-bufs,
    /// which cost nothing here; shm is for software rendering and cursors.
    pub const DEFAULT_SHM_BYTES: u64 = 1 << 30;
    /// Pools over the VM: every one is a memfd held open in the backend.
    pub const DEFAULT_SHM_POOLS: u64 = 1024;
    /// Four connections' worth of the per-connection queue limit.
    pub const DEFAULT_QUEUE_BYTES: usize = 256 << 20;

    pub fn new(max_conns: usize, shm_bytes: u64, queue_bytes: usize) -> Self {
        let shm = Arc::new(ShmBudget::new(shm_bytes, Self::DEFAULT_SHM_POOLS));
        Self {
            max_conns,
            shm_shares: Arc::new(ShmShares::new(shm.clone())),
            shm,
            queue: Arc::new(QueueBudget::new(queue_bytes)),
            lease: Arc::new(LeaseThrottle::default()),
        }
    }

    /// Lease submits at one per `interval`, `burst` at once.
    pub fn with_lease_rate(mut self, interval: Duration, burst: u32) -> Self {
        self.lease = Arc::new(LeaseThrottle::new(interval, burst));
        self
    }
}

impl Default for WlLimits {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_MAX_CONNS,
            Self::DEFAULT_SHM_BYTES,
            Self::DEFAULT_QUEUE_BYTES,
        )
    }
}

/// Export mode: bytes queued for the guest past which the engine's pending
/// commits and blobs are left unread until the guest takes some.
pub const EXPORT_QUEUE: usize = 8 << 20;

struct State {
    engine: Engine,
    to_guest: VecDeque<Unit>,
    to_guest_bytes: usize,
    /// The compositor side is finished (EOF, error, or a fatal protocol
    /// error); HANGUP is queued.
    closed: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    sock: UnixStream,
    /// Readable while `to_guest` is not empty.
    ready: OwnedFd,
    /// Wakes the reader thread (new streams, output waiting for POLLOUT, stop).
    wake: OwnedFd,
    host: Arc<dyn HostFds>,
    cfg: WlConfig,
    /// The registry filter asks `cfg.lease_cache` about lease devices, so the
    /// reader answers first, outside the lock.
    probe_leases: bool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Whatever the guest never took is off the VM's queue budget.
        let st = self.state.get_mut().unwrap_or_else(|p| p.into_inner());
        self.cfg
            .limits
            .queue
            .give(self.cfg.owner, st.to_guest_bytes);
    }
}

pub struct WlConn {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Descriptors on the compositor's side of the engine: classification for
/// what the compositor sends, PRIME export for what the guest sends.
struct HostPlat<'a> {
    host: &'a dyn HostFds,
    send: Option<&'a mut dyn SendOps>,
}

impl Platform for HostPlat<'_> {
    fn dmabuf_out(&mut self, fd: OwnedFd) -> DescOut {
        // Export mode: a host client's buffer, for the guest compositor.
        match self.host.classify(fd.as_fd()) {
            HandleKind::Dmabuf => {
                let size = crate::sys::fd::size(fd.as_raw_fd()).unwrap_or(0);
                DescOut {
                    desc: Desc {
                        c: size,
                        ..Desc::new(frame::DESC_DMABUF)
                    },
                    fd: Some(fd),
                }
            }
            k => {
                log::warn!("wayland: a dma-buf argument was a {k:?}; sending a placeholder");
                DescOut::plain(Desc::invalid(frame::DESC_DMABUF))
            }
        }
    }

    fn dmabuf_in(&mut self, desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        let ops = self
            .send
            .as_mut()
            .ok_or_else(|| io::Error::other("no export path"))?;
        ops.prime_export(desc.a, desc.b).inspect_err(|e| {
            log::warn!(
                "wayland: PRIME export of gem {} on handle {} failed: {e}",
                desc.b,
                desc.a
            )
        })
    }

    fn drm_file_out(&mut self, fd: OwnedFd) -> DescOut {
        match self.host.classify(fd.as_fd()) {
            k @ HandleKind::DrmLease(_) => DescOut {
                desc: Desc {
                    b: k.wire(),
                    ..Desc::new(frame::DESC_DRM_FILE)
                },
                fd: Some(fd),
            },
            k => {
                // Only files of our own GPU can be driven from the guest; the
                // lease global was meant to be hidden for anything else.
                log::warn!(
                    "wayland: the compositor sent a DRM file that is a {k:?}, not a lease of ours"
                );
                DescOut::plain(Desc::invalid(frame::DESC_DRM_FILE))
            }
        }
    }

    fn drm_file_in(&mut self, _desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        Err(io::Error::other("the guest does not send DRM files"))
    }

    fn syncobj_in(&mut self, desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        let ops = self
            .send
            .as_mut()
            .ok_or_else(|| io::Error::other("no export path"))?;
        ops.syncobj(desc.a).inspect_err(|e| {
            log::warn!("wayland: syncobj handle {} for the compositor: {e}", desc.a)
        })
    }
}

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.state.lock().unwrap_or_else(|p| p.into_inner())
}

impl WlConn {
    /// Connect to the host compositor for a new guest client. Returns the
    /// connection and a duplicate of its readiness eventfd for the event pump.
    pub fn open(cfg: &WlConfig, host: Arc<dyn HostFds>) -> io::Result<(WlConn, OwnedFd)> {
        let sock = UnixStream::connect(&cfg.socket)?;
        let lease = if cfg.allow_lease {
            // Only a lookup: the reader resolved every lease-device global in
            // what it read before the engine (and this) ran (`probe.rs`).
            let cache = cfg.lease_cache.clone();
            LeaseGate::Check(Arc::new(move |name| cache.lookup(name).unwrap_or(false)))
        } else {
            LeaseGate::Deny
        };
        Self::start(
            sock,
            cfg,
            host,
            Local::Server,
            Policy {
                drm_file: false,
                lease,
                // Kept only if the guest's HELLO says HELLO_G_SYNCOBJ.
                fences: cfg.fences,
            },
        )
    }

    /// A host client connected to the export socket (`--wayland-export`): the
    /// compositor is the guest's, and this side faces a client.
    pub fn from_export(
        sock: UnixStream,
        cfg: &WlConfig,
        host: Arc<dyn HostFds>,
    ) -> io::Result<(WlConn, OwnedFd)> {
        // A guest compositor's leases are not offered to host clients.
        Self::start(sock, cfg, host, Local::Client, Policy::default())
    }

    fn start(
        sock: UnixStream,
        cfg: &WlConfig,
        host: Arc<dyn HostFds>,
        local: Local,
        policy: Policy,
    ) -> io::Result<(WlConn, OwnedFd)> {
        sock.set_nonblocking(true)?;
        let probe_leases = matches!(policy.lease, LeaseGate::Check(_));
        let mut engine = Engine::new(EngineConfig {
            side: Side::Host,
            local,
            policy,
            rewrites: None,
            synth_released: false,
        });
        // A process's share takes from the VM's budget itself.
        match &cfg.owner_shm {
            Some(b) => engine.set_shm_budget(b.clone()),
            None => engine.set_shm_budget(cfg.limits.shm.clone()),
        }
        engine.set_stream_budget(Arc::new(OwnerQueue {
            queue: cfg.limits.queue.clone(),
            owner: cfg.owner,
        }));
        engine.hello(0);
        let ready = sys::eventfd()?;
        let wake = sys::eventfd()?;
        let mut st = State {
            engine,
            to_guest: VecDeque::new(),
            to_guest_bytes: 0,
            closed: false,
            stop: false,
        };
        let hello = st.engine.take_units();
        let n = hello.iter().map(|u| u.bytes()).sum::<usize>();
        cfg.limits.queue.force(cfg.owner, n);
        st.to_guest_bytes += n;
        st.to_guest.extend(hello);
        sys::eventfd_signal(ready.as_raw_fd());
        let shared = Arc::new(Shared {
            state: Mutex::new(st),
            sock,
            ready,
            wake,
            host,
            cfg: cfg.clone(),
            probe_leases,
        });
        let ready_dup = shared.ready.try_clone()?;
        let s2 = shared.clone();
        let thread = std::thread::Builder::new()
            .name("nvgpu-wl".into())
            .spawn(move || reader(s2))?;
        Ok((
            WlConn {
                shared,
                thread: Some(thread),
            },
            ready_dup,
        ))
    }

    /// WL_SEND: one frame from the guest. On a protocol violation the
    /// connection is ended (the guest learns why from an ERROR record on its
    /// next WL_RECV) and `EPROTO` returned.
    pub fn send(
        &self,
        frame_bytes: &[u8],
        ops: &mut dyn SendOps,
    ) -> Result<protocol::messages::WlSendResp, i32> {
        let s = &*self.shared;
        let mut st = lock(s);
        if st.closed {
            return Err(libc::EPIPE);
        }
        let backlog = st.engine.local_out().len();
        if backlog > s.cfg.max_backlog {
            return Err(libc::EAGAIN);
        }
        // Lease requests at the VM's rate (`LeaseThrottle`): a frame with one
        // too many waits whole, like one for a compositor that is not reading.
        let submits = st.engine.lease_submits(frame_bytes);
        match s.cfg.limits.lease.admit(submits, Instant::now()) {
            Ok(()) => {}
            Err(LeaseRefusal::Wait(wait)) => {
                if !s.cfg.limits.lease.logged.swap(true, Ordering::Relaxed) {
                    log::info!(
                        "wayland: the guest asks for leases faster than one per {:?}; \
                         holding its next request for {wait:?}",
                        s.cfg.limits.lease.interval
                    );
                }
                return Err(libc::EAGAIN);
            }
            Err(LeaseRefusal::OverBurst) => {
                fail(
                    s,
                    &mut st,
                    Fatal::new(
                        Blame::Channel,
                        1,
                        wlwire::engine::ERR_IMPLEMENTATION,
                        format!(
                            "{submits} lease submits in one frame, more than the {} the VM may \
                             make at once",
                            s.cfg.limits.lease.burst
                        ),
                    ),
                );
                return Err(libc::EPROTO);
            }
        }
        // And the engine lets through no more than were counted: a submit
        // the count missed ends the connection instead of passing the rate.
        st.engine.allow_lease_submits(Some(submits));
        let mut plat = HostPlat {
            host: &*s.host,
            send: Some(ops),
        };
        let r = st.engine.from_channel(frame_bytes, Vec::new(), &mut plat);
        st.engine.allow_lease_submits(Some(0));
        if let Err(f) = r {
            fail(s, &mut st, f);
            return Err(libc::EPROTO);
        }
        collect(s, &mut st);
        if let Err(e) = st.engine.local_out().flush(s.sock.as_raw_fd()) {
            hangup(s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
        }
        let backlog = st.engine.local_out().len() as u32;
        drop(st);
        // New streams to watch, or output waiting for POLLOUT.
        sys::eventfd_signal(s.wake.as_raw_fd());
        Ok(protocol::messages::WlSendResp {
            accepted: frame_bytes.len() as u32,
            backlog,
        })
    }

    /// WL_RECV: as much as fits in `max_bytes` / `max_desc`, as a frame. An
    /// empty frame when nothing is waiting.
    pub fn recv(
        &self,
        max_bytes: u32,
        max_desc: u32,
        ops: &mut dyn RecvOps,
    ) -> Result<Vec<u8>, i32> {
        if (max_bytes as usize) < frame::MIN_FRAME {
            return Err(libc::EINVAL);
        }
        let s = &*self.shared;
        let mut st = lock(s);
        let (mut f, fds) = frame::pack(
            &mut st.to_guest,
            max_bytes as usize,
            max_desc as usize,
            true,
        );
        let left: usize = st.to_guest.iter().map(|u| u.bytes()).sum();
        s.cfg
            .limits
            .queue
            .give(s.cfg.owner, st.to_guest_bytes - left);
        st.to_guest_bytes = left;
        for (i, fd) in fds.into_iter().enumerate() {
            let Some(fd) = fd else { continue };
            let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
            let mut d = Desc::read(&f[at..at + frame::DESC_LEN]);
            match ops.adopt(fd, d.kind) {
                Ok((handle, hk)) => {
                    d.a = handle;
                    d.b = hk;
                }
                Err(e) => {
                    log::warn!(
                        "wayland: adopting a descriptor of kind {} failed: {e}",
                        d.kind
                    );
                    d.flags |= frame::DESC_F_INVALID;
                }
            }
            let mut b = Vec::with_capacity(frame::DESC_LEN);
            d.write(&mut b);
            f[at..at + frame::DESC_LEN].copy_from_slice(&b);
        }
        if st.engine.local_is_client() {
            // Export mode: what the host client sent waits in the engine
            // for room here, and its input for the engine to take more.
            collect(s, &mut st);
            sys::eventfd_signal(s.wake.as_raw_fd());
        }
        if st.to_guest.is_empty() {
            sys::eventfd_clear(s.ready.as_raw_fd());
        }
        Ok(f)
    }

    /// The readiness eventfd (the one `open` returned a duplicate of).
    pub fn ready_fd(&self) -> BorrowedFd<'_> {
        self.shared.ready.as_fd()
    }

    /// Engine counters, for logs and tests.
    pub fn stats(&self) -> wlwire::engine::Stats {
        lock(&self.shared).engine.stats.clone()
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.shared).closed
    }
}

impl Drop for WlConn {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        sys::eventfd_signal(self.shared.wake.as_raw_fd());
        let _ = self.shared.sock.shutdown(std::net::Shutdown::Both);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Move the engine's channel output to the guest queue, within this
/// connection's limit and the VM's budget. Past either the connection is
/// dropped, and what it had queued with it: kept, it would pin memory until
/// the guest closes the handle, for a guest that has shown it is not reading.
/// In export mode only what fits [`EXPORT_QUEUE`] is taken, and the rest of
/// what the host client sent waits in the engine, unread.
fn collect(s: &Shared, st: &mut State) {
    // HANGUP is queued: the guest stops at it, so nothing after it is ever
    // read, and keeping it would only hold memory (and descriptors).
    if st.closed {
        st.engine.drop_channel_output();
        return;
    }
    let units = if st.engine.local_is_client() {
        let room = export_room(s, st);
        if room == 0 {
            return;
        }
        st.engine.take_units_upto(room)
    } else {
        st.engine.take_units()
    };
    if units.is_empty() {
        return;
    }
    let n: usize = units.iter().map(|u| u.bytes()).sum();
    let own = st.to_guest_bytes + n <= s.cfg.max_queue;
    if !own || !s.cfg.limits.queue.take(s.cfg.owner, n) {
        log::warn!(
            "wayland: the guest has not read {} bytes ({}); dropping the connection",
            st.to_guest_bytes + n,
            if own {
                "the VM's queue budget, or its guest process's share of it, is spent"
            } else {
                "past the connection's limit"
            }
        );
        s.cfg.limits.queue.give(s.cfg.owner, st.to_guest_bytes);
        st.to_guest.clear();
        st.to_guest_bytes = 0;
        hangup(s, st, libc::ENOBUFS);
        return;
    }
    st.to_guest_bytes += n;
    st.to_guest.extend(units);
    sys::eventfd_signal(s.ready.as_raw_fd());
}

/// Export mode: bytes the guest queue may still take from the engine.
fn export_room(s: &Shared, st: &State) -> usize {
    EXPORT_QUEUE
        .min(s.cfg.max_queue / 2)
        .saturating_sub(st.to_guest_bytes)
}

/// Queue a record that ends the connection (ERROR, HANGUP): small, and owed
/// to the guest whatever the budget says.
fn push_final(s: &Shared, st: &mut State, u: Unit) {
    let n = u.bytes();
    s.cfg.limits.queue.force(s.cfg.owner, n);
    st.to_guest_bytes += n;
    st.to_guest.push_back(u);
}

/// End the connection on a protocol error: the guest is told why.
fn fail(s: &Shared, st: &mut State, f: Fatal) {
    // Guest text: quoted and escaped, and cut short (the engine made it
    // printable already), and the logger meters this line like any other
    // a guest can cause (ratelimit.rs).
    let text = wlwire::engine::printable(&f.message, 256);
    match f.blame {
        Blame::Channel | Blame::Remote => {
            log::warn!("wayland: guest protocol error, closing: {text:?}")
        }
        Blame::Local => log::warn!("wayland: compositor protocol error, closing: {text:?}"),
    }
    // Export mode: the host client is the one to tell, as libwayland would
    // have, after whatever it already has (Engine::end_with).
    let record = st.engine.end_with(&f);
    if st.engine.local_is_client() {
        let _ = st.engine.local_out().flush(s.sock.as_raw_fd());
    }
    push_final(s, st, record);
    hangup(s, st, libc::EPROTO);
}

fn hangup(s: &Shared, st: &mut State, errno: i32) {
    if st.closed {
        return;
    }
    st.closed = true;
    let _ = s.sock.shutdown(std::net::Shutdown::Both);
    // The pools' memfds and any half-received blobs are for a live
    // connection only; the VM's shm budget has them back now, not when the
    // guest closes the handle.
    st.engine.shed();
    // Export mode's peer is a host client, not the compositor.
    if !st.engine.local_is_client() {
        s.host.compositor_hung_up();
    }
    push_final(
        s,
        st,
        Unit {
            rec: frame::record(frame::REC_HANGUP, 0, errno as u32, &[]),
            descs: Vec::new(),
        },
    );
    sys::eventfd_signal(s.ready.as_raw_fd());
}

fn reader(s: Arc<Shared>) {
    let sock = s.sock.as_raw_fd();
    let mut buf = vec![0u8; 64 * 1024];
    // What the compositor sent that the engine has not taken yet (a partial
    // message), and its descriptors. Only this thread reads the socket, so
    // neither needs the lock -- which lets the lease-device probe run between
    // the read and the engine without it.
    let mut inbuf: Vec<u8> = Vec::new();
    let mut infds: VecDeque<OwnedFd> = VecDeque::new();
    loop {
        let (want_out, streams, closed, blocked) = {
            let mut st = lock(&s);
            if st.stop {
                return;
            }
            // Export mode: input the engine left when the guest's queue
            // was full, taken now that there may be room.
            if !st.closed && !inbuf.is_empty() && !st.engine.input_blocked() {
                let mut plat = HostPlat {
                    host: &*s.host,
                    send: None,
                };
                match st.engine.from_local(&mut inbuf, &mut infds, &mut plat) {
                    Ok(()) => collect(&s, &mut st),
                    Err(f) => fail(&s, &mut st, f),
                }
            }
            // Streams that ended: this loop polls afresh each time, so there
            // is no registration to take out first.
            drop(st.engine.take_closed_streams());
            (
                st.engine.local_out_len() > 0,
                st.engine.stream_interest(),
                st.closed,
                st.engine.input_blocked(),
            )
        };
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(2 + streams.len());
        let mut ev = 0;
        if !closed {
            // Not readable while the engine takes no input: the client
            // waits in its socket. Nor watched at all then unless there is
            // output, or its hangup would wake every poll.
            if !blocked {
                ev |= libc::POLLIN;
            }
            if want_out {
                ev |= libc::POLLOUT;
            }
        }
        pfds.push(libc::pollfd {
            fd: if closed || ev == 0 { -1 } else { sock },
            events: ev,
            revents: 0,
        });
        pfds.push(libc::pollfd {
            fd: s.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for i in &streams {
            let mut e = 0;
            if i.read {
                e |= libc::POLLIN;
            }
            if i.write {
                e |= libc::POLLOUT;
            }
            pfds.push(libc::pollfd {
                fd: if e == 0 { -1 } else { i.fd },
                events: e,
                revents: 0,
            });
        }
        if let Err(e) = crate::sys::fd::poll(&mut pfds, 1000) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("wayland: poll: {e}");
            return;
        }
        if pfds[1].revents != 0 {
            sys::eventfd_clear(s.wake.as_raw_fd());
        }
        let rev = pfds[0].revents;
        if !closed && !blocked && rev & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            // Read until the socket is empty: never leave the compositor's
            // buffer to fill.
            loop {
                let mut fds = Vec::new();
                let r = sys::recv_with_fds(sock, &mut buf, &mut fds);
                let mut overflow = false;
                if let Ok(n @ 1..) = r {
                    inbuf.extend_from_slice(&buf[..n]);
                    infds.extend(fds);
                    // Descriptors no message has taken, held for ever
                    // otherwise; libwayland closes the connection too.
                    overflow = infds.len() > wlwire::wire::MAX_FDS_QUEUED;
                    // Lease-device globals are answered here, before the
                    // engine's registry filter asks and with no lock held: a
                    // probe can take seconds, and the connection's lock is
                    // what WL_SEND and WL_RECV wait on under the backend
                    // mutex (`probe.rs`).
                    if s.probe_leases {
                        let names = probe::lease_globals(&inbuf);
                        if !names.is_empty() {
                            s.cfg.lease_cache.resolve(&s.cfg.socket, &*s.host, &names);
                        }
                    }
                }
                let mut st = lock(&s);
                if st.stop {
                    return;
                }
                if st.closed {
                    break;
                }
                if overflow {
                    infds.clear();
                    fail(
                        &s,
                        &mut st,
                        Fatal::new(
                            Blame::Local,
                            1,
                            wlwire::engine::ERR_NO_MEMORY,
                            format!(
                                "more than {} file descriptors that no message takes",
                                wlwire::wire::MAX_FDS_QUEUED
                            ),
                        ),
                    );
                    break;
                }
                match r {
                    Ok(0) => {
                        hangup(&s, &mut st, 0);
                        break;
                    }
                    Ok(_) => {
                        let mut plat = HostPlat {
                            host: &*s.host,
                            send: None,
                        };
                        if let Err(f) = st.engine.from_local(&mut inbuf, &mut infds, &mut plat) {
                            fail(&s, &mut st, f);
                            break;
                        }
                        collect(&s, &mut st);
                        // Export mode: read no more than the engine takes.
                        if st.closed || st.engine.input_blocked() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        log::warn!("wayland: reading the compositor: {e}");
                        hangup(&s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
                        break;
                    }
                }
            }
        }
        let mut st = lock(&s);
        if st.stop {
            return;
        }
        // A blocked socket is watched for output alone, and its hangup is
        // found by the write.
        let out_ready = if blocked {
            libc::POLLOUT | libc::POLLHUP | libc::POLLERR
        } else {
            libc::POLLOUT
        };
        if !st.closed
            && want_out
            && rev & out_ready != 0
            && let Err(e) = st.engine.local_out().flush(sock)
        {
            hangup(&s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
        }
        for (i, p) in streams.iter().zip(&pfds[2..]) {
            if p.revents != 0 {
                let rd = p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
                let wr = p.revents & (libc::POLLOUT | libc::POLLERR) != 0;
                st.engine.stream_io(i.id, rd, wr);
            }
        }
        collect(&s, &mut st);
    }
}

/// Raw descriptor of the compositor socket, for tests.
#[cfg(test)]
pub(crate) fn sock_fd(c: &WlConn) -> std::os::fd::RawFd {
    c.shared.sock.as_raw_fd()
}

/// Bytes queued for the guest, and whether the engine takes no more of the
/// local peer's input, for tests.
#[cfg(test)]
pub(crate) fn export_state(c: &WlConn) -> (usize, bool) {
    let st = lock(&c.shared);
    (st.to_guest_bytes, st.engine.input_blocked())
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::quota::Owner;

    fn proc(tgid: u32) -> Owner {
        Owner::Proc { tgid, start_ns: 1 }
    }

    /// Four processes at their full share of the VM's shm pools and bytes
    /// leave the reserve, and a fifth still makes its first pools and
    /// buffers out of it. Before, each process had a flat quarter and no
    /// reserve: four took everything, and every other process's first pool
    /// was refused, which is fatal to it (the 2026-09-29 review, S5).
    #[test]
    fn four_processes_at_their_share_leave_room_for_a_fifth() {
        let limits = WlLimits::new(64, 1 << 30, 1 << 20);
        let shares = &limits.shm_shares;
        let (per_pool, per_byte) = (1024 / 4, (1u64 << 30) / 4);
        for p in 1..=4 {
            let b = shares.for_owner(proc(p));
            let mut pools = 0;
            while pools < per_pool + 1 && b.take(0, 1) {
                pools += 1;
            }
            assert!(pools <= per_pool, "past a quarter of the pools");
            let mut bytes = 0;
            while bytes < per_byte + (1 << 20) && b.take(1 << 20, 0) {
                bytes += 1 << 20;
            }
            assert!(bytes <= per_byte, "past a quarter of the bytes");
        }
        let (bytes, pools) = limits.shm.used();
        assert!(pools <= 1024 - 64 && bytes <= (1 << 30) - (1 << 26));
        // The fifth: its first pools and buffers, up to the floor.
        let fifth = shares.for_owner(proc(5));
        assert!(fifth.take(0, 1), "a fifth process's first pool");
        assert!(fifth.take(8 << 20, 0), "and a buffer in it");
        // What goes back goes back to the process and the VM both.
        fifth.give(8 << 20, 1);
        assert_eq!(shares.held_by(proc(5)), (0, 0));
        assert_eq!(limits.shm.used(), (bytes, pools));
    }

    /// A process whose channels are never read fills at most its share of
    /// the queue budget, so the connection that crosses the line is its own
    /// and never another process's (W3).
    #[test]
    fn a_process_that_never_reads_hits_its_own_share_not_everyones() {
        let q = QueueBudget::new(256);
        let (a, b) = (
            Owner::Proc {
                tgid: 1,
                start_ns: 1,
            },
            Owner::Proc {
                tgid: 2,
                start_ns: 1,
            },
        );
        assert!(q.take(a, 128));
        assert!(!q.take(a, 1), "past half the budget");
        assert_eq!(q.held_by(a), 128);
        // Another process still queues, down to the last quarter...
        assert!(q.take(b, 64));
        assert!(
            !q.take(b, 64),
            "the last quarter is not for one holding a quarter"
        );
        // ...which is kept for a process that holds little.
        let c = Owner::Proc {
            tgid: 3,
            start_ns: 1,
        };
        assert!(q.take(c, 64));
        assert!(!q.take(c, 1), "the VM's budget is the outer bound");
        q.give(a, 128);
        q.give(b, 64);
        q.give(c, 64);
        assert_eq!((q.used(), q.held_by(a), q.held_by(b)), (0, 0, 0));
        // A guest that does not say is held to the VM's budget alone.
        assert!(q.take(Owner::Unknown, 256));
        assert!(!q.take(Owner::Unknown, 1));
        // The records that end a connection are owed whatever the budget.
        q.force(a, 16);
        assert_eq!(q.used(), 272);
    }
}
