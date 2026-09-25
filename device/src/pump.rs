//! The event pump: host readiness and DRM events, carried to the guest.
//!
//! The event queue is the one place the host speaks first. The guest posts
//! empty buffers on it; this module decides what goes into them.
//!
//! Two dialects, chosen per session:
//!
//! - **v1** (no successful HELLO): one bare 16-byte `EVENT_READY` header per
//!   buffer, naming a handle whose host descriptor became readable. This is
//!   all a v1 guest understands -- it posts 16-byte buffers and drops anything
//!   that is not EVENT_READY -- so nothing else is ever sent to one, whatever
//!   was watched.
//! - **v2**: `EVENT_DATA`, a header whose `req_id` carries the payload length,
//!   then as many [`EvRec`] records as fit the posted buffer: readiness
//!   (`EV_READY`), fence completion (`EV_FENCE`), raw DRM events read from a
//!   card or lease file (`EV_DRM`), hotplug (`EV_HOTPLUG`).
//!
//! What must not happen, and what the structure below is for:
//!
//! - **Unbounded memory.** Readiness and fence records are coalesced per
//!   cookie, so their number is bounded by the number of watches. DRM bytes
//!   are bounded per handle by [`DRM_BUDGET`]: over it the pump stops reading
//!   that file and takes it out of epoll, so the host kernel's own per-file
//!   `event_space` (4 KiB, drm_file.c:158) applies backpressure to whoever
//!   queues events there, exactly as it would for a native client that stopped
//!   reading. Dropping DRM events instead would lose flip completions a
//!   compositor waits on forever.
//! - **Split events.** A record never splits a `struct drm_event`: reads take
//!   at most [`DRM_READ_MAX`] bytes, which `drm_read` fills with whole events
//!   only (drm_file.c:580-591 puts back an event that does not fit), and
//!   records are cut on the event lengths in each header.
//! - **A parked pump.** Every DRM read is preceded by `poll(fd, 0)`, and DRM
//!   files are O_NONBLOCK anyway (see `hostfd::set_nonblock`). One blocking
//!   read would stop every event for the VM.
//! - **A spinning pump.** Level-readable descriptors nobody drains (an RM
//!   event fd until the guest's ioctl consumes it) are edge-triggered, with a
//!   1 ms level sweep as the safety net for a missed edge. One-shot watches
//!   are `EPOLLONESHOT` and never swept: a sync_file stays readable forever
//!   once signalled, and sweeping it would report it forever.
//! - **Latency when buffers run out.** A kick on the event queue (the guest
//!   posting buffers) writes an eventfd this loop waits on, and the pump
//!   enables notifications on the queue whenever it finds none posted -- with
//!   EVENT_IDX the guest otherwise never kicks at all.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use protocol::messages::*;

use crate::privfd::PrivateFd;

/// Undelivered DRM event bytes per handle before the pump stops reading.
///
/// The host's own per-file event space: past this the kernel would refuse to
/// queue more events for a native client that did not read, and letting the
/// backend buffer more would only move that queue somewhere unbounded.
pub const DRM_BUDGET: usize = 4096;

/// Most bytes one read takes, so that one read always fits one record in one
/// guest buffer: the message header and the record header are 16 bytes each.
pub const DRM_READ_MAX: usize = EVENT_BUF_SIZE - 32;

const HDR: usize = size_of::<MsgHeader>();
const REC: usize = size_of::<EvRec>();

/// How often level-readable descriptors are re-checked, as a net under a
/// missed edge. Not the notification path.
const SWEEP: Duration = Duration::from_millis(1);

/// What a watch reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    /// Every OPEN'd device handle, watched without being asked, as v1 always
    /// did: `EVENT_READY(handle)` to a v1 guest, `EV_READY` with cookie =
    /// handle to a v2 one.
    Legacy,
    /// `EV_READY` with the WATCH cookie. `consume` drains an eventfd's counter
    /// when reporting it, so the level sweep does not report it again.
    Ready {
        cookie: u64,
        oneshot: bool,
        consume: bool,
    },
    /// `EV_FENCE` with the sync_file's status, once.
    Fence { cookie: u64 },
    /// `EV_DRM`: the file's DRM events, read and forwarded, cookie = handle.
    Drm,
}

impl WatchMode {
    fn oneshot(self) -> bool {
        matches!(self, Self::Fence { .. } | Self::Ready { oneshot: true, .. })
    }

    fn swept(self) -> bool {
        matches!(self, Self::Legacy | Self::Ready { oneshot: false, .. })
    }

    fn cookie(self) -> Option<u64> {
        match self {
            Self::Ready { cookie, .. } | Self::Fence { cookie } => Some(cookie),
            Self::Legacy | Self::Drm => None,
        }
    }
}

/// Instructions from the backend. Sent in the order the backend made them, so
/// a close always reaches the pump after the watch it ends.
pub enum PumpCmd {
    /// Start (or replace) a watch. `fd` is the pump's own duplicate, so the
    /// handle table closing its copy cannot leave epoll watching a number that
    /// has since been reused.
    Watch {
        handle: u32,
        fd: OwnedFd,
        mode: WatchMode,
    },
    /// Stop watching, and drop whatever this handle still had queued: the
    /// consumer on the guest side is gone.
    Unwatch { handle: u32 },
    /// Whether the session speaks v2 (EVENT_DATA) or v1 (EVENT_READY only).
    SetV2(bool),
    /// The session was reset: every watch and every queued record goes.
    Reset,
    /// A card was hotplugged or its lease state changed (`EV_HOTPLUG_F_*`).
    Hotplug { card: u32, flags: u32 },
}

// ---------------------------------------------------------------------------
// The outbox: what is waiting for a buffer
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Legacy(u32),
    Ready(u64),
    Fence(u64),
    Hotplug(u32),
    Drm(u32),
}

/// Records waiting for a guest buffer, in arrival order, coalesced.
#[derive(Default)]
pub struct Outbox {
    v2: bool,
    order: VecDeque<Key>,
    queued: HashSet<Key>,
    fence: HashMap<u64, i32>,
    hotplug: HashMap<u32, u32>,
    drm: HashMap<u32, Vec<u8>>,
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch dialect. Leaving v2 drops everything a v1 guest cannot read.
    pub fn set_v2(&mut self, v2: bool) {
        self.v2 = v2;
        if !v2 {
            self.fence.clear();
            self.hotplug.clear();
            self.drm.clear();
            self.order.retain(|k| matches!(k, Key::Legacy(_)));
            self.queued.retain(|k| matches!(k, Key::Legacy(_)));
        }
    }

    pub fn is_v2(&self) -> bool {
        self.v2
    }

    pub fn clear(&mut self) {
        let v2 = self.v2;
        *self = Self::default();
        self.v2 = v2;
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn enqueue(&mut self, k: Key) {
        if self.queued.insert(k) {
            self.order.push_back(k);
        }
    }

    /// A legacy handle became readable. Coalesced: one pending report per
    /// handle, however often it fires before a buffer arrives.
    pub fn ready_legacy(&mut self, handle: u32) {
        self.enqueue(Key::Legacy(handle));
    }

    pub fn ready(&mut self, cookie: u64) {
        if self.v2 {
            self.enqueue(Key::Ready(cookie));
        }
    }

    /// A fence completed. A second report for the same cookie replaces the
    /// status rather than queueing another record.
    pub fn fence(&mut self, cookie: u64, status: i32) {
        if self.v2 {
            self.fence.insert(cookie, status);
            self.enqueue(Key::Fence(cookie));
        }
    }

    pub fn hotplug(&mut self, card: u32, flags: u32) {
        if self.v2 {
            *self.hotplug.entry(card).or_insert(0) |= flags;
            self.enqueue(Key::Hotplug(card));
        }
    }

    /// Bytes read from a DRM file: whole `struct drm_event`s.
    pub fn drm(&mut self, handle: u32, bytes: &[u8]) {
        if self.v2 && !bytes.is_empty() {
            self.drm.entry(handle).or_default().extend_from_slice(bytes);
            self.enqueue(Key::Drm(handle));
        }
    }

    /// DRM bytes read from `handle` and not yet delivered.
    pub fn drm_backlog(&self, handle: u32) -> usize {
        self.drm.get(&handle).map_or(0, Vec::len)
    }

    /// Drop everything queued for a handle that is no longer watched.
    pub fn forget(&mut self, handle: u32, cookie: Option<u64>) {
        let mut gone = vec![Key::Legacy(handle), Key::Drm(handle)];
        if let Some(c) = cookie {
            gone.extend([Key::Ready(c), Key::Fence(c)]);
            self.fence.remove(&c);
        }
        self.drm.remove(&handle);
        for k in &gone {
            self.queued.remove(k);
        }
        self.order.retain(|k| !gone.contains(k));
    }

    fn pop(&mut self) {
        if let Some(k) = self.order.pop_front() {
            self.queued.remove(&k);
        }
    }

    /// Build the message for one posted buffer of `cap` bytes, taking what is
    /// sent out of the outbox. Empty when nothing fits.
    pub fn build(&mut self, cap: usize) -> Vec<u8> {
        if !self.v2 || cap < HDR + REC {
            // A v1 guest, or a buffer too small for any record -- such as one
            // a v1 driver posted before this session said HELLO. Only a
            // legacy readiness report can travel in it.
            return self.build_legacy(cap);
        }
        let mut out = vec![0u8; HDR];
        while let Some(&key) = self.order.front() {
            let space = cap - out.len();
            match key {
                Key::Legacy(h) => {
                    if !push_rec(&mut out, space, EV_READY, h as u64, &[]) {
                        break;
                    }
                    self.pop();
                }
                Key::Ready(c) => {
                    if !push_rec(&mut out, space, EV_READY, c, &[]) {
                        break;
                    }
                    self.pop();
                }
                Key::Fence(c) => {
                    let status = self.fence.get(&c).copied().unwrap_or(1);
                    let mut p = [0u8; 8];
                    p[..4].copy_from_slice(&status.to_le_bytes());
                    if !push_rec(&mut out, space, EV_FENCE, c, &p) {
                        break;
                    }
                    self.fence.remove(&c);
                    self.pop();
                }
                Key::Hotplug(card) => {
                    let flags = self.hotplug.get(&card).copied().unwrap_or(0);
                    let mut p = [0u8; 8];
                    p[..4].copy_from_slice(&flags.to_le_bytes());
                    if !push_rec(&mut out, space, EV_HOTPLUG, card as u64, &p) {
                        break;
                    }
                    self.hotplug.remove(&card);
                    self.pop();
                }
                Key::Drm(h) => {
                    let backlog = self.drm.get(&h).map(Vec::as_slice).unwrap_or(&[]);
                    let limit = space.saturating_sub(REC).min(DRM_READ_MAX);
                    let take = match whole_events(backlog, limit) {
                        Ok(0) if out.len() == HDR && backlog.len() >= 8 => {
                            // Not even the first event fits an empty buffer.
                            // No kernel event is anywhere near this size; drop
                            // it rather than wedge the queue behind it.
                            let len = event_len(backlog);
                            log::warn!(
                                "event pump: a {len}-byte DRM event on handle {h} cannot fit a \
                                 {cap}-byte buffer; dropped"
                            );
                            self.drain_drm(h, len.clamp(8, backlog.len()));
                            continue;
                        }
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(()) => {
                            log::warn!(
                                "event pump: malformed DRM event stream on handle {h}; \
                                 dropped {} bytes",
                                backlog.len()
                            );
                            self.drain_drm(h, backlog.len());
                            continue;
                        }
                    };
                    let bytes = backlog[..take].to_vec();
                    push_rec(&mut out, space, EV_DRM, h as u64, &bytes);
                    self.drain_drm(h, take);
                    if self.drm_backlog(h) != 0 {
                        // More than this buffer holds: the rest keeps its place.
                        break;
                    }
                }
            }
        }
        if out.len() == HDR {
            return Vec::new();
        }
        let payload = (out.len() - HDR) as u32;
        out[..HDR].copy_from_slice(&hdr_bytes(MsgType::EventData, 0, payload));
        out
    }

    /// Remove `n` delivered bytes from a handle's backlog, and the handle from
    /// the queue once it is empty.
    fn drain_drm(&mut self, handle: u32, n: usize) {
        let empty = match self.drm.get_mut(&handle) {
            Some(b) => {
                b.drain(..n.min(b.len()));
                b.is_empty()
            }
            None => true,
        };
        if empty {
            self.drm.remove(&handle);
            let k = Key::Drm(handle);
            self.queued.remove(&k);
            self.order.retain(|x| *x != k);
        }
    }

    fn build_legacy(&mut self, cap: usize) -> Vec<u8> {
        if cap < HDR {
            return Vec::new();
        }
        let Some(pos) = self.order.iter().position(|k| matches!(k, Key::Legacy(_))) else {
            return Vec::new();
        };
        let Some(Key::Legacy(h)) = self.order.remove(pos) else {
            unreachable!("position() found a legacy key")
        };
        self.queued.remove(&Key::Legacy(h));
        hdr_bytes(MsgType::EventReady, h, 0).to_vec()
    }
}

/// Append one record if it fits in `space`; false if it does not.
fn push_rec(out: &mut Vec<u8>, space: usize, kind: u32, cookie: u64, payload: &[u8]) -> bool {
    let padded = payload.len().next_multiple_of(8);
    if REC + padded > space {
        return false;
    }
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&cookie.to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + padded - payload.len(), 0);
    true
}

fn hdr_bytes(t: MsgType, handle: u32, req_id: u32) -> [u8; HDR] {
    let mut b = [0u8; HDR];
    b[0..4].copy_from_slice(&(t as u32).to_le_bytes());
    b[4..8].copy_from_slice(&handle.to_le_bytes());
    b[12..16].copy_from_slice(&req_id.to_le_bytes());
    b
}

/// The length field of the `struct drm_event` at the start of `buf`.
fn event_len(buf: &[u8]) -> usize {
    u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize
}

/// How many leading bytes of `buf` are whole `struct drm_event`s totalling at
/// most `limit`. `Err` for a stream whose headers make no sense (a length
/// shorter than the header itself), which the kernel never produces.
pub fn whole_events(buf: &[u8], limit: usize) -> Result<usize, ()> {
    let mut off = 0;
    while buf.len() - off >= 8 {
        let len = event_len(&buf[off..]);
        if len < 8 {
            return Err(());
        }
        if off + len > limit || off + len > buf.len() {
            break;
        }
        off += len;
    }
    Ok(off)
}

// ---------------------------------------------------------------------------
// The queue the pump fills
// ---------------------------------------------------------------------------

/// What happened to one attempt to fill a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// A message went out.
    Filled,
    /// A buffer was taken but nothing pending fit in it.
    Empty,
    /// The guest has no buffer posted.
    NoBuffer,
}

/// The event virtqueue, as the pump sees it. The transport implements it.
pub trait EventQueue: Send {
    /// Take the next posted buffer, have `build` produce the message for its
    /// capacity, write it and hand the buffer back.
    fn fill(&mut self, build: &mut dyn FnMut(usize) -> Vec<u8>) -> Fill;
    /// Ask to be kicked when the guest posts more. True when it already has.
    fn want_kick(&mut self) -> bool;
}

// ---------------------------------------------------------------------------
// The pump
// ---------------------------------------------------------------------------

/// The epoll data word of the wake eventfd; handles never take this value.
const WAKE: u64 = u64::MAX;

struct Watched {
    /// The pump's own duplicate of the handle's descriptor: a number the
    /// backend holds that is in no handle table, so registered as private.
    fd: PrivateFd,
    mode: WatchMode,
}

/// How the backend and the transport talk to a running pump.
#[derive(Clone)]
pub struct PumpHandle {
    tx: Sender<PumpCmd>,
    wake: Arc<PrivateFd>,
}

impl PumpHandle {
    /// Queue an instruction and wake the pump so it applies it now rather
    /// than at the next sweep.
    pub fn send(&self, cmd: PumpCmd) {
        if self.tx.send(cmd).is_ok() {
            self.kick();
        }
    }

    /// The guest posted event buffers: flush whatever is waiting for one.
    pub fn kick(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: an 8-byte write to an eventfd we own. EAGAIN (counter
        // saturated) means a wake is already pending, which is all we want.
        unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), 8) };
    }
}

pub struct Pump<Q: EventQueue> {
    epfd: PrivateFd,
    wake: Arc<PrivateFd>,
    rx: Receiver<PumpCmd>,
    queue: Q,
    outbox: Outbox,
    watches: HashMap<u32, Watched>,
    /// DRM handles taken out of epoll for being over budget.
    paused: HashSet<u32>,
    last_sweep: Instant,
    buf: Vec<u8>,
}

impl<Q: EventQueue + 'static> Pump<Q> {
    /// Start a pump on its own thread.
    pub fn spawn(queue: Q) -> io::Result<PumpHandle> {
        let (pump, handle) = Self::new(queue)?;
        std::thread::Builder::new()
            .name("nvgpu-events".into())
            .spawn(move || pump.run())?;
        Ok(handle)
    }

    fn run(mut self) {
        while self.step() {}
        log::info!("event pump: backend gone, stopping");
    }
}

impl<Q: EventQueue> Pump<Q> {
    pub fn new(queue: Q) -> io::Result<(Self, PumpHandle)> {
        // SAFETY: plain syscalls; both results are owned below.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let epfd = PrivateFd::new(unsafe { OwnedFd::from_raw_fd(epfd) });
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(io::Error::last_os_error());
        }
        let wake = Arc::new(PrivateFd::new(unsafe { OwnedFd::from_raw_fd(wake) }));
        let (tx, rx) = channel();
        let pump = Self {
            epfd,
            wake: wake.clone(),
            rx,
            queue,
            outbox: Outbox::new(),
            watches: HashMap::new(),
            paused: HashSet::new(),
            last_sweep: Instant::now(),
            buf: vec![0u8; DRM_READ_MAX],
        };
        pump.ctl(
            libc::EPOLL_CTL_ADD,
            pump.wake.as_raw_fd(),
            libc::EPOLLIN as u32,
            WAKE,
        );
        Ok((pump, PumpHandle { tx, wake }))
    }

    fn ctl(&self, op: i32, fd: RawFd, events: u32, data: u64) -> bool {
        let mut ev = libc::epoll_event { events, u64: data };
        // SAFETY: `ev` outlives the call; epoll copies it.
        unsafe { libc::epoll_ctl(self.epfd.as_raw_fd(), op, fd, &mut ev) == 0 }
    }

    fn arm(&self, handle: u32, w: &Watched) -> bool {
        let events = if w.mode.oneshot() {
            libc::EPOLLIN | libc::EPOLLONESHOT
        } else {
            // Edge-triggered. Level-triggered would report a descriptor as
            // readable until the *guest* consumes the event, through an ioctl
            // this thread never sees, and the pump would spin between
            // notifying and being believed. Parking the descriptor for a
            // millisecond instead cost 11% of the frames in an encode run.
            libc::EPOLLIN | libc::EPOLLET
        };
        self.ctl(
            libc::EPOLL_CTL_ADD,
            w.fd.as_raw_fd(),
            events as u32,
            handle as u64,
        )
    }

    fn unwatch(&mut self, handle: u32) -> Option<Watched> {
        let w = self.watches.remove(&handle)?;
        self.paused.remove(&handle);
        self.ctl(libc::EPOLL_CTL_DEL, w.fd.as_raw_fd(), 0, 0);
        Some(w)
    }

    /// One round: wait, apply instructions, read what is ready, sweep, fill
    /// buffers. False once the backend has gone away.
    pub fn step(&mut self) -> bool {
        let sweeping = self.watches.values().any(|w| w.mode.swept());
        let timeout = if sweeping {
            // Rounded up: a remainder under a millisecond rounded down is a
            // zero timeout, and the loop would spin until the sweep is due.
            SWEEP
                .saturating_sub(self.last_sweep.elapsed())
                .as_micros()
                .div_ceil(1000) as i32
        } else {
            -1
        };
        self.step_with_timeout(timeout)
    }

    fn step_with_timeout(&mut self, timeout_ms: i32) -> bool {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 32];
        // SAFETY: `events` is writable for its whole length.
        let n = unsafe {
            libc::epoll_wait(
                self.epfd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                timeout_ms,
            )
        };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            log::error!("event pump: epoll_wait: {}", io::Error::last_os_error());
            return false;
        }

        // Instructions first: a descriptor closed on the queue thread must
        // leave the set before it can be reported again.
        if !self.apply_commands() {
            return false;
        }

        for ev in events.iter().take(n.max(0) as usize) {
            // Copied out first: epoll_event is packed, so its field cannot be
            // borrowed.
            let data = { ev.u64 };
            if data == WAKE {
                let mut b = [0u8; 8];
                // SAFETY: an 8-byte read from our own non-blocking eventfd.
                unsafe { libc::read(self.wake.as_raw_fd(), b.as_mut_ptr().cast(), 8) };
                continue;
            }
            self.on_ready(data as u32);
        }

        if self.last_sweep.elapsed() >= SWEEP {
            self.last_sweep = Instant::now();
            self.sweep();
        }
        self.flush();
        self.resume_drained();
        true
    }

    fn apply_commands(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(PumpCmd::Watch { handle, fd, mode }) => {
                    if let Some(old) = self.unwatch(handle) {
                        if old.mode.cookie() != mode.cookie() {
                            self.outbox.forget(handle, old.mode.cookie());
                        }
                    }
                    let w = Watched {
                        fd: PrivateFd::new(fd),
                        mode,
                    };
                    if self.arm(handle, &w) {
                        self.watches.insert(handle, w);
                        // An edge that fired before the watch existed is not
                        // coming back; look now.
                        self.on_ready_if_readable(handle);
                    } else {
                        log::warn!(
                            "event pump: cannot watch handle {handle}: {}",
                            io::Error::last_os_error()
                        );
                    }
                }
                Ok(PumpCmd::Unwatch { handle }) => {
                    let cookie = self.unwatch(handle).and_then(|w| w.mode.cookie());
                    self.outbox.forget(handle, cookie);
                }
                Ok(PumpCmd::SetV2(v2)) => self.outbox.set_v2(v2),
                Ok(PumpCmd::Reset) => {
                    for h in self.watches.keys().copied().collect::<Vec<_>>() {
                        self.unwatch(h);
                    }
                    self.outbox.clear();
                }
                Ok(PumpCmd::Hotplug { card, flags }) => self.outbox.hotplug(card, flags),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn on_ready_if_readable(&mut self, handle: u32) {
        if self
            .watches
            .get(&handle)
            .is_some_and(|w| readable(w.fd.as_raw_fd()))
        {
            self.on_ready(handle);
        }
    }

    fn on_ready(&mut self, handle: u32) {
        let Some(w) = self.watches.get(&handle) else {
            return;
        };
        let (fd, mode) = (w.fd.as_raw_fd(), w.mode);
        match mode {
            WatchMode::Legacy => self.outbox.ready_legacy(handle),
            WatchMode::Ready {
                cookie,
                oneshot,
                consume,
            } => {
                if consume && readable(fd) {
                    let mut b = [0u8; 8];
                    // SAFETY: an 8-byte read from an eventfd after poll said
                    // it would not block.
                    unsafe { libc::read(fd, b.as_mut_ptr().cast(), 8) };
                }
                self.outbox.ready(cookie);
                if oneshot {
                    self.unwatch(handle);
                }
            }
            WatchMode::Fence { cookie } => {
                let status = crate::hostfd::sync_file_status(fd)
                    .unwrap_or_else(|e| -e.raw_os_error().unwrap_or(libc::EIO));
                if status == 0 {
                    // Readable yet still active: nothing to report. One-shot
                    // armed, so arm it again.
                    self.ctl(
                        libc::EPOLL_CTL_MOD,
                        fd,
                        (libc::EPOLLIN | libc::EPOLLONESHOT) as u32,
                        handle as u64,
                    );
                    return;
                }
                self.outbox.fence(cookie, status);
                self.unwatch(handle);
            }
            WatchMode::Drm => self.read_drm(handle),
        }
    }

    /// Read a DRM file's events until it is drained or over budget.
    fn read_drm(&mut self, handle: u32) {
        let Some(fd) = self.watches.get(&handle).map(|w| w.fd.as_raw_fd()) else {
            return;
        };
        loop {
            if self.outbox.drm_backlog(handle) >= DRM_BUDGET {
                // Out of epoll until the guest takes some: the host kernel's
                // event space fills behind us and pushes back on the producer.
                if self.paused.insert(handle) {
                    self.ctl(libc::EPOLL_CTL_DEL, fd, 0, 0);
                }
                return;
            }
            if !readable(fd) {
                return;
            }
            // SAFETY: `buf` is DRM_READ_MAX bytes; poll said this will not
            // block, and the file is O_NONBLOCK besides.
            let n = unsafe { libc::read(fd, self.buf.as_mut_ptr().cast(), self.buf.len()) };
            if n <= 0 {
                return;
            }
            let bytes = self.buf[..n as usize].to_vec();
            self.outbox.drm(handle, &bytes);
        }
    }

    /// Put paused DRM handles back once their backlog has gone out.
    fn resume_drained(&mut self) {
        let ready: Vec<u32> = self
            .paused
            .iter()
            .copied()
            .filter(|&h| self.outbox.drm_backlog(h) < DRM_BUDGET)
            .collect();
        for h in ready {
            self.paused.remove(&h);
            let rearmed = self.watches.get(&h).is_some_and(|w| self.arm(h, w));
            if rearmed {
                self.read_drm(h);
            }
        }
        if !self.outbox.is_empty() {
            self.flush();
        }
    }

    /// The safety net under a missed edge: ask the swept descriptors directly.
    fn sweep(&mut self) {
        let due: Vec<u32> = self
            .watches
            .iter()
            .filter(|(_, w)| w.mode.swept() && readable(w.fd.as_raw_fd()))
            .map(|(&h, _)| h)
            .collect();
        for h in due {
            self.on_ready(h);
        }
    }

    /// Fill posted buffers until the outbox is empty or the guest has none.
    fn flush(&mut self) {
        let mut asked = false;
        while !self.outbox.is_empty() {
            let outbox = &mut self.outbox;
            match self.queue.fill(&mut |cap| outbox.build(cap)) {
                Fill::Filled => {}
                // What is waiting does not fit the buffers this guest posts.
                // Wait for the next round rather than burn through them all.
                Fill::Empty => return,
                Fill::NoBuffer => {
                    if asked || !self.queue.want_kick() {
                        return;
                    }
                    asked = true;
                }
            }
        }
    }
}

/// Whether reading `fd` would return something now.
fn readable(fd: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, zero timeout.
    unsafe { libc::poll(&mut p, 1, 0) > 0 && p.revents & libc::POLLIN != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn drm_event(kind: u32, len: usize) -> Vec<u8> {
        let mut e = vec![0xabu8; len];
        e[0..4].copy_from_slice(&kind.to_le_bytes());
        e[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        e
    }

    /// Parse an EVENT_DATA message into (kind, cookie, payload) records.
    fn records(msg: &[u8]) -> Vec<(u32, u64, Vec<u8>)> {
        assert_eq!(
            u32::from_le_bytes(msg[0..4].try_into().unwrap()),
            MsgType::EventData as u32
        );
        let payload = u32::from_le_bytes(msg[12..16].try_into().unwrap()) as usize;
        assert_eq!(
            payload,
            msg.len() - HDR,
            "req_id carries the payload length"
        );
        let mut out = Vec::new();
        let mut off = HDR;
        while off < msg.len() {
            let kind = u32::from_le_bytes(msg[off..off + 4].try_into().unwrap());
            let len = u32::from_le_bytes(msg[off + 4..off + 8].try_into().unwrap()) as usize;
            let cookie = u64::from_le_bytes(msg[off + 8..off + 16].try_into().unwrap());
            out.push((kind, cookie, msg[off + REC..off + REC + len].to_vec()));
            off += REC + len.next_multiple_of(8);
        }
        out
    }

    fn v2() -> Outbox {
        let mut o = Outbox::new();
        o.set_v2(true);
        o
    }

    #[test]
    fn a_v1_session_only_ever_gets_sixteen_byte_event_ready() {
        let mut o = Outbox::new();
        o.ready_legacy(7);
        o.ready_legacy(9);
        // Nothing but legacy readiness is even queued for a v1 guest.
        o.ready(1);
        o.fence(2, 1);
        o.drm(3, &drm_event(1, 32));
        let m = o.build(16);
        assert_eq!(m, hdr_bytes(MsgType::EventReady, 7, 0));
        assert_eq!(o.build(8192), hdr_bytes(MsgType::EventReady, 9, 0));
        assert!(o.is_empty());
        assert!(o.build(8192).is_empty());
    }

    #[test]
    fn records_are_batched_into_one_buffer_in_arrival_order() {
        let mut o = v2();
        o.ready_legacy(5);
        o.fence(0x100, 1);
        o.drm(3, &[drm_event(2, 32), drm_event(2, 24)].concat());
        o.ready(0x200);
        o.hotplug(1, EV_HOTPLUG_F_LEASE);
        let recs = records(&o.build(EVENT_BUF_SIZE));
        let kinds: Vec<(u32, u64)> = recs.iter().map(|r| (r.0, r.1)).collect();
        assert_eq!(
            kinds,
            vec![
                (EV_READY, 5),
                (EV_FENCE, 0x100),
                (EV_DRM, 3),
                (EV_READY, 0x200),
                (EV_HOTPLUG, 1)
            ]
        );
        assert_eq!(recs[1].2, [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(recs[2].2.len(), 56);
        assert_eq!(recs[4].2[..4], EV_HOTPLUG_F_LEASE.to_le_bytes());
        assert!(o.is_empty());
    }

    #[test]
    fn readiness_and_fences_coalesce_per_cookie() {
        let mut o = v2();
        for _ in 0..100 {
            o.ready(1);
            o.ready_legacy(2);
        }
        o.fence(3, 0);
        o.fence(3, -5);
        let recs = records(&o.build(EVENT_BUF_SIZE));
        assert_eq!(recs.len(), 3);
        assert_eq!(
            recs[2].2[..4],
            (-5i32).to_le_bytes(),
            "the latest status wins"
        );
    }

    #[test]
    fn a_record_never_splits_a_drm_event() {
        let mut o = v2();
        // Three 40-byte events; a buffer with room for two.
        o.drm(
            4,
            &[drm_event(1, 40), drm_event(1, 40), drm_event(1, 40)].concat(),
        );
        let cap = HDR + REC + 80 + 7;
        let first = records(&o.build(cap));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].2.len(), 80);
        assert_eq!(o.drm_backlog(4), 40);
        let second = records(&o.build(cap));
        assert_eq!(second[0].2.len(), 40);
        assert!(o.is_empty());
    }

    #[test]
    fn whole_events_stops_at_the_limit_and_rejects_nonsense() {
        let s = [drm_event(1, 32), drm_event(1, 32)].concat();
        assert_eq!(whole_events(&s, 64), Ok(64));
        assert_eq!(whole_events(&s, 63), Ok(32));
        assert_eq!(whole_events(&s, 31), Ok(0));
        assert_eq!(whole_events(&s[..40], 64), Ok(32), "a partial tail is left");
        let mut short = drm_event(1, 8);
        short[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(whole_events(&short, 64), Err(()));
    }

    #[test]
    fn a_buffer_too_small_for_a_record_falls_back_to_event_ready_for_legacy_watches() {
        let mut o = v2();
        o.ready(1);
        o.ready_legacy(6);
        // A 16-byte buffer left over from before HELLO.
        assert_eq!(o.build(16), hdr_bytes(MsgType::EventReady, 6, 0));
        // The cookie record waits for a real buffer.
        assert!(o.build(16).is_empty());
        assert_eq!(records(&o.build(64))[0].1, 1);
    }

    #[test]
    fn forgetting_a_handle_drops_what_it_had_queued() {
        let mut o = v2();
        o.drm(1, &drm_event(1, 32));
        o.ready(77);
        o.ready_legacy(2);
        o.forget(1, Some(77));
        let recs = records(&o.build(EVENT_BUF_SIZE));
        assert_eq!(recs.len(), 1);
        assert_eq!((recs[0].0, recs[0].1), (EV_READY, 2));
    }

    // ---- the pump itself, driven step by step against a fake queue ----

    #[derive(Clone, Default)]
    struct FakeQueue {
        posted: Arc<Mutex<VecDeque<usize>>>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl FakeQueue {
        fn post(&self, n: usize, cap: usize) {
            self.posted
                .lock()
                .unwrap()
                .extend(std::iter::repeat_n(cap, n));
        }
        fn take(&self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.sent.lock().unwrap())
        }
    }

    impl EventQueue for FakeQueue {
        fn fill(&mut self, build: &mut dyn FnMut(usize) -> Vec<u8>) -> Fill {
            let Some(cap) = self.posted.lock().unwrap().pop_front() else {
                return Fill::NoBuffer;
            };
            let m = build(cap);
            if m.is_empty() {
                return Fill::Empty;
            }
            self.sent.lock().unwrap().push(m);
            Fill::Filled
        }
        fn want_kick(&mut self) -> bool {
            false
        }
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut p = [0i32; 2];
        // SAFETY: `p` receives the two new descriptors.
        assert_eq!(
            unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) }
    }

    fn write_all(fd: &OwnedFd, b: &[u8]) {
        // SAFETY: writing from a live slice.
        let n = unsafe { libc::write(fd.as_raw_fd(), b.as_ptr().cast(), b.len()) };
        assert_eq!(n as usize, b.len());
    }

    #[test]
    fn a_readable_legacy_handle_reaches_a_v1_guest_as_event_ready() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 12,
            fd: r,
            mode: WatchMode::Legacy,
        });
        q.post(4, 16);
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        assert_eq!(
            q.take(),
            vec![hdr_bytes(MsgType::EventReady, 12, 0).to_vec()]
        );
    }

    /// The backpressure the spec asks for: with no buffers posted the pump
    /// reads at most a budget's worth and then leaves the rest in the host
    /// file, and resumes once the guest takes what it holds.
    #[test]
    fn a_drm_handle_over_budget_is_not_read_until_the_guest_drains_it() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 3,
            fd: r,
            mode: WatchMode::Drm,
        });
        // 256 events of 32 bytes = 8 KiB, twice the budget, in the "host".
        let ev = drm_event(0x80000001, 32);
        for _ in 0..256 {
            write_all(&w, &ev);
        }
        pump.step_with_timeout(0);
        let held = pump.outbox.drm_backlog(3);
        assert!(
            (DRM_BUDGET..DRM_BUDGET + DRM_READ_MAX).contains(&held),
            "held {held}"
        );
        assert!(pump.paused.contains(&3));
        assert!(
            readable(pump.watches[&3].fd.as_raw_fd()),
            "the rest stays in the host file"
        );

        // The guest posts buffers; the pump drains, resumes, reads the rest.
        q.post(8, EVENT_BUF_SIZE);
        h.kick();
        for _ in 0..4 {
            pump.step_with_timeout(0);
        }
        let total: usize = q
            .take()
            .iter()
            .flat_map(|m| records(m))
            .map(|(k, c, p)| {
                assert_eq!((k, c), (EV_DRM, 3));
                p.len()
            })
            .sum();
        assert_eq!(total, 256 * 32);
        assert!(!pump.paused.contains(&3));
    }

    #[test]
    fn a_oneshot_ready_watch_reports_once_and_drains_its_eventfd() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let efd = crate::hostfd::new_eventfd().unwrap();
        let dup = efd.try_clone().unwrap();
        h.send(PumpCmd::Watch {
            handle: 4,
            fd: dup,
            mode: WatchMode::Ready {
                cookie: 0xc0ffee,
                oneshot: true,
                consume: true,
            },
        });
        write_all(&efd, &1u64.to_ne_bytes());
        q.post(4, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        write_all(&efd, &1u64.to_ne_bytes());
        pump.step_with_timeout(0);
        pump.step_with_timeout(0);
        let sent = q.take();
        assert_eq!(sent.len(), 1);
        assert_eq!(records(&sent[0]), vec![(EV_READY, 0xc0ffee, vec![])]);
        assert!(
            pump.watches.is_empty(),
            "a one-shot watch ends when it fires"
        );
    }

    #[test]
    fn a_reset_drops_every_watch_and_everything_queued() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 1,
            fd: r,
            mode: WatchMode::Legacy,
        });
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        assert!(!pump.outbox.is_empty());
        h.send(PumpCmd::Reset);
        pump.step_with_timeout(0);
        assert!(pump.outbox.is_empty() && pump.watches.is_empty());
    }
}
