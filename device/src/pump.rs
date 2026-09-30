// SPDX-License-Identifier: Apache-2.0
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
//!   cookie, and a one-shot record not yet delivered goes when its handle is
//!   watched under another cookie or closed ([`Pump::fired`]), so their
//!   number is bounded by the number of handles. A retired handle's record
//!   ([`PumpCmd::Retire`]) outlives the handle, but not what the handle was
//!   charged, which it keeps until it is sent. DRM bytes
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
//! - **A pump woken for nobody.** An RM descriptor whose readiness the guest
//!   arms ([`WatchMode::LegacyArmed`]) is in no epoll set: a Vulkan game's
//!   driver posts over a hundred thousand events a second on descriptors
//!   nobody in the guest waits on, and each woke the pump. Such a descriptor
//!   is polled by the pump's own wait while it is armed, and by nothing at
//!   all while it is not ([`Pump::wait`]). It cannot be moved in and out of
//!   epoll instead: RM's `poll` clears a dataless event as it reports it
//!   (open-gpu-kernel-modules 595.99.02, kernel-open/nvidia/nv.c
//!   `nvidia_poll()`), and epoll polls a descriptor twice when it is added or
//!   modified with an event pending -- once at the insert, once to collect it
//!   (Linux 7.2.7, fs/eventpoll.c `ep_insert()` and `ep_modify()`, then
//!   `ep_send_events()`) -- so an event arriving as it was re-armed
//!   would be taken by the first poll and found by nobody, and a guest
//!   waiting on it would wait for good. `poll(2)` polls each descriptor once
//!   a pass and returns what that pass found, so every event RM clears is
//!   one the pump reports. A wait polls every armed descriptor, so at most
//!   [`POLLED_MAX`] watches are kept this way; one watched past that is in
//!   the epoll set from its start and for its life, as all were before.
//! - **A sweep that costs by the handle.** The sweep polls only handles
//!   that were reported readable and may still be: the guest consumes RM
//!   events one ioctl at a time, and a descriptor left readable after one
//!   makes no new edge, which is what the sweep is there for. A handle
//!   leaves that set the first sweep that finds it drained, and an idle
//!   handle is never in it. Sweeping every watch instead cost a `poll` per
//!   open device file per millisecond -- a guest holding a thousand idle
//!   nvidiactl files pinned a host core (S-21) -- and with nothing to sweep
//!   the pump now sleeps until an event or a kick.
//! - **A kick drained with its instruction left behind.** The backend
//!   queues an instruction and then kicks the wake eventfd; the pump drains
//!   that eventfd *before* it takes the instructions ([`Pump::step`]).
//!   Drained after, a kick for an instruction queued in between went with
//!   the drain, and the pump slept with the instruction in the channel until
//!   some other kick -- a syncobj wait's Watch, and every guest waiter on its
//!   point, asleep for as long as the guest made no other call.
//! - **Latency when buffers run out.** A kick on the event queue (the guest
//!   posting buffers) writes an eventfd this loop waits on, and the pump
//!   enables notifications on the queue whenever it finds none posted -- with
//!   EVENT_IDX the guest otherwise never kicks at all.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use protocol::messages::*;

use crate::le;
use crate::pacing::PACING;
use crate::privfd::PrivateFd;
use std::sync::atomic::Ordering::Relaxed;

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

/// Most [`WatchMode::LegacyArmed`] watches kept out of epoll and polled by
/// the pump's wait while armed. The wait polls every armed one each time it
/// returns, so without a bound a guest arming thousands of descriptors would
/// make every wake of its pump cost thousands of polls. A game holds a few
/// dozen RM descriptors.
pub const POLLED_MAX: usize = 256;

/// What a watch reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    /// Every OPEN'd device handle, watched without being asked, as v1 always
    /// did: `EVENT_READY(handle)` to a v1 guest, `EV_READY` with cookie =
    /// handle to a v2 one.
    Legacy,
    /// [`WatchMode::Legacy`] for an RM device handle of a guest that arms
    /// its readiness (`BCAP_ARMED_READY`): reported once per
    /// [`PumpCmd::Arm`], not on every host event. RM posts dozens of events
    /// a frame on descriptors nobody in the guest polls; each was a record
    /// and, most of the time, a guest interrupt.
    LegacyArmed,
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
            Self::Legacy | Self::LegacyArmed | Self::Drm => None,
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
    /// The backend let go of one-shot handle `handle` -- a syncobj wait
    /// registration's eventfd, fired or dead (fence.rs) -- whose guest
    /// waiters are still there. Unlike [`PumpCmd::Unwatch`] its report is
    /// kept: one the pump has not read yet is read now, and `hold` -- the
    /// handle's descriptor and what it is charged, `HandleTable::closing` --
    /// stays until that report has gone to the guest or been dropped. So a
    /// retired handle costs its process nothing once its record is out, and
    /// an undelivered record is still counted against it.
    Retire { handle: u32, hold: Box<dyn Send> },
    /// Whether the session speaks v2 (EVENT_DATA) or v1 (EVENT_READY only).
    SetV2(bool),
    /// The session was reset: every watch and every queued record goes.
    Reset,
    /// A card was hotplugged or its lease state changed (`EV_HOTPLUG_F_*`).
    Hotplug { card: u32, flags: u32 },
    /// The guest waits on [`WatchMode::LegacyArmed`] handle `handle`:
    /// report it once more.
    Arm { handle: u32 },
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
    /// cookie -> (status, host signal time in CLOCK_MONOTONIC ns, 0 unknown)
    fence: HashMap<u64, (i32, u64)>,
    hotplug: HashMap<u32, u32>,
    drm: HashMap<u32, Vec<u8>>,
    /// What a retired handle's queued record keeps (`PumpCmd::Retire`), by
    /// cookie: dropped when that record is sent or dropped.
    holds: HashMap<u64, Vec<Box<dyn Send>>>,
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
            self.holds.clear();
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
        self.fence_at(cookie, status, 0);
    }

    /// [`Outbox::fence`], with when the host's fences signalled
    /// (`CLOCK_MONOTONIC` ns, 0 for unknown), for the guest proxy to signal
    /// with that time rather than the time the record reached it.
    pub fn fence_at(&mut self, cookie: u64, status: i32, timestamp_ns: u64) {
        if self.v2 {
            self.fence.insert(cookie, (status, timestamp_ns));
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
            self.holds.remove(&c);
        }
        self.drm.remove(&handle);
        for k in &gone {
            self.queued.remove(k);
        }
        self.order.retain(|k| !gone.contains(k));
    }

    /// Drop a readiness or fence record queued under `cookie`: its handle
    /// is watched under another now, or gone.
    pub fn forget_cookie(&mut self, cookie: u64) {
        let gone = [Key::Ready(cookie), Key::Fence(cookie)];
        self.fence.remove(&cookie);
        self.holds.remove(&cookie);
        for k in &gone {
            self.queued.remove(k);
        }
        self.order.retain(|k| !gone.contains(k));
    }

    /// Keep `hold` until the record queued under `cookie` is sent or
    /// dropped; drop it now if none is.
    pub fn hold(&mut self, cookie: u64, hold: Box<dyn Send>) {
        if self.queued.contains(&Key::Ready(cookie)) || self.queued.contains(&Key::Fence(cookie)) {
            self.holds.entry(cookie).or_default().push(hold);
        }
    }

    /// Records a retired handle's hold keeps (tests).
    #[cfg(test)]
    fn held(&self) -> usize {
        self.holds.values().map(Vec::len).sum()
    }

    fn pop(&mut self) {
        if let Some(k) = self.order.pop_front() {
            self.queued.remove(&k);
            if let Key::Ready(c) | Key::Fence(c) = k {
                self.holds.remove(&c);
            }
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
                    PACING.ev_ready.fetch_add(1, Relaxed);
                    self.pop();
                }
                Key::Ready(c) => {
                    if !push_rec(&mut out, space, EV_READY, c, &[]) {
                        break;
                    }
                    PACING.ev_ready.fetch_add(1, Relaxed);
                    self.pop();
                }
                Key::Fence(c) => {
                    // {i32 status; u32 pad; u64 timestamp_ns}: a guest that
                    // predates the timestamp reads the first 8 bytes only.
                    let (status, ts) = self.fence.get(&c).copied().unwrap_or((1, 0));
                    let mut p = [0u8; 16];
                    p[..4].copy_from_slice(&status.to_le_bytes());
                    p[8..].copy_from_slice(&ts.to_le_bytes());
                    if !push_rec(&mut out, space, EV_FENCE, c, &p) {
                        break;
                    }
                    PACING.ev_fence.fetch_add(1, Relaxed);
                    if ts != 0 {
                        // Only a plausible one: some host fences carry a
                        // timestamp that is not CLOCK_MONOTONIC's.
                        let now = crate::session::monotonic_ns();
                        if now >= ts && now - ts < 10_000_000_000 {
                            PACING.fence_delivery.record_ns(now - ts);
                        }
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
                        Some(0) if out.len() == HDR && backlog.len() >= 8 => {
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
                        Some(0) => break,
                        Some(n) => n,
                        None => {
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
                    PACING.ev_drm.fetch_add(1, Relaxed);
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
    le::u32_at(buf, 4).map_or(0, |n| n as usize)
}

/// How many leading bytes of `buf` are whole `struct drm_event`s totalling at
/// most `limit`. `None` for a stream whose headers make no sense (a length
/// shorter than the header itself), which the kernel never produces.
pub fn whole_events(buf: &[u8], limit: usize) -> Option<usize> {
    let mut off = 0;
    while buf.len() - off >= 8 {
        let len = event_len(&buf[off..]);
        if len < 8 {
            return None;
        }
        if off + len > limit || off + len > buf.len() {
            break;
        }
        off += len;
    }
    Some(off)
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
    /// [`WatchMode::LegacyArmed`]: the guest waits on it, so the next event
    /// is reported.
    armed: bool,
    /// [`WatchMode::LegacyArmed`]: out of epoll, polled by the wait while
    /// armed (at most [`POLLED_MAX`] watches); else in epoll for its life.
    polled: bool,
    /// [`WatchMode::LegacyArmed`]: an event was taken from the descriptor
    /// while it was not armed -- only the look when the watch starts takes
    /// one -- so the descriptor's own readiness cannot say so afterwards
    /// (polling RM's file takes its dataless-event flag), and this does.
    dirty: bool,
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
        // EAGAIN (counter saturated) means a wake is already pending, which
        // is all we want.
        crate::sys::fd::eventfd_signal(self.wake.as_raw_fd());
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
    /// Swept handles reported readable that may still be: the only ones the
    /// sweep polls.
    stale: HashSet<u32>,
    /// Handles whose one-shot watch fired, with the cookie its record may
    /// still be queued under. The cookie is the guest's: without this, a
    /// guest re-watching one signalled sync_file under a fresh cookie each
    /// time, and posting no buffers, would grow the outbox by a record a
    /// watch with no bound.
    fired: HashMap<u32, u64>,
    /// Armed [`WatchMode::LegacyArmed`] handles kept out of epoll: the
    /// descriptors the pump's wait polls besides its epoll set.
    waiting: Vec<u32>,
    /// Watches kept out of epoll (`Watched::polled`), at most [`POLLED_MAX`].
    polled: usize,
    /// That wait's `poll` array, kept between rounds: the epoll descriptor,
    /// then one for each of `waiting`, in its order.
    pollfds: Vec<libc::pollfd>,
    last_sweep: Instant,
    buf: Vec<u8>,
    /// Run in each round just after the instructions are taken (tests: an
    /// instruction sent from another thread at that moment).
    #[cfg(test)]
    after_commands: Option<Box<dyn FnMut() + Send>>,
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
        let epfd = PrivateFd::new(crate::sys::fd::epoll_create()?);
        let wake = Arc::new(PrivateFd::new(crate::sys::fd::eventfd(
            libc::EFD_CLOEXEC | libc::EFD_NONBLOCK,
        )?));
        let (tx, rx) = channel();
        let pump = Self {
            epfd,
            wake: wake.clone(),
            rx,
            queue,
            outbox: Outbox::new(),
            watches: HashMap::new(),
            paused: HashSet::new(),
            stale: HashSet::new(),
            fired: HashMap::new(),
            waiting: Vec::new(),
            polled: 0,
            pollfds: Vec::new(),
            last_sweep: Instant::now(),
            buf: vec![0u8; DRM_READ_MAX],
            #[cfg(test)]
            after_commands: None,
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
        crate::sys::fd::epoll_ctl(self.epfd.as_raw_fd(), op, fd, events, data).is_ok()
    }

    fn arm(&self, handle: u32, w: &Watched) -> bool {
        if w.polled {
            // Never in epoll: the wait polls it while armed (Pump::wait).
            return true;
        }
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

    /// Stop watching `handle`, returning what it was watched for. The pump's
    /// duplicate goes to the closer: after the queue thread's CLOSE it is
    /// usually the file's last reference, and the last close of a card,
    /// lease or modeset file can run a modeset, which on this thread would
    /// stop every event of the VM (closer.rs, S-33).
    fn unwatch(&mut self, handle: u32) -> Option<WatchMode> {
        let w = self.watches.remove(&handle)?;
        self.paused.remove(&handle);
        self.stale.remove(&handle);
        self.waiting.retain(|&h| h != handle);
        if w.polled {
            self.polled -= 1;
        } else {
            self.ctl(libc::EPOLL_CTL_DEL, w.fd.as_raw_fd(), 0, 0);
        }
        let mode = w.mode;
        crate::closer::close(w.fd);
        Some(mode)
    }

    /// One round: wait, apply instructions, read what is ready, sweep, fill
    /// buffers. False once the backend has gone away.
    pub fn step(&mut self) -> bool {
        let timeout = if !self.stale.is_empty() {
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
        let Some((n, armed_ready)) = self.wait(&mut events, timeout_ms) else {
            return false;
        };
        PACING.pump_wakes.fetch_add(1, Relaxed);

        // The kick is taken before the instructions are, never after. A
        // sender queues its instruction and then kicks; drained after the
        // last instruction was taken, the kick of one queued in between is
        // lost with the drain, the instruction stays in the channel, and
        // the pump sleeps until some other kick -- a Watch whose eventfd has
        // fired, and every guest waiter on it, left asleep. Drained first,
        // a kick either comes with an instruction the loop below takes, or
        // stays and wakes the next wait at once.
        if events.iter().take(n).any(|ev| ({ ev.u64 }) == WAKE) {
            crate::sys::fd::eventfd_drain(self.wake.as_raw_fd());
        }
        // Instructions first: a descriptor closed on the queue thread must
        // leave the set before it can be reported again.
        if !self.apply_commands() {
            return false;
        }
        #[cfg(test)]
        if let Some(f) = self.after_commands.as_mut() {
            f();
        }
        // What the wait took from armed descriptors: each is reported (or,
        // if an instruction just now disarmed it, remembered), never dropped.
        for h in armed_ready {
            PACING.ev_edge.fetch_add(1, Relaxed);
            self.on_ready(h);
        }

        let woke = Instant::now();
        for ev in events.iter().take(n) {
            // Copied out first: epoll_event is packed, so its field cannot be
            // borrowed.
            let data = { ev.u64 };
            if data == WAKE {
                // Drained already, before the instructions were taken.
                continue;
            }
            PACING.ev_edge.fetch_add(1, Relaxed);
            self.on_ready(data as u32);
        }

        if self.last_sweep.elapsed() >= SWEEP {
            self.last_sweep = Instant::now();
            self.sweep();
        }
        if self.flush() {
            PACING.pump_delivery.since(woke);
        }
        self.resume_drained();
        true
    }

    /// Wait for the epoll set, and for every armed
    /// [`WatchMode::LegacyArmed`] descriptor, up to `timeout_ms`: the epoll
    /// events into `events` and their count, and the armed handles the wait
    /// found ready -- each an event RM's `poll` has now cleared, which the
    /// caller must report. `None` once the pump cannot wait at all.
    ///
    /// With nothing armed this is `epoll_wait`, as it always was. With
    /// something armed it is one `poll(2)` over the epoll descriptor and the
    /// armed ones, which polls each once a pass and returns what that pass
    /// found (then a non-blocking `epoll_wait` if the set had events).
    /// Unarmed, a descriptor is in neither, and its events wake nobody; RM
    /// keeps them, and the next arm's own look finds them (`arm_legacy`).
    fn wait(
        &mut self,
        events: &mut [libc::epoll_event],
        timeout_ms: i32,
    ) -> Option<(usize, Vec<u32>)> {
        let epoll_wait = |events: &mut [libc::epoll_event], epfd: RawFd, t: i32| {
            match crate::sys::fd::epoll_wait(epfd, events, t) {
                Ok(n) => Some(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => Some(0),
                Err(e) => {
                    log::error!("event pump: epoll_wait: {e}");
                    None
                }
            }
        };
        let epfd = self.epfd.as_raw_fd();
        if self.waiting.is_empty() {
            return Some((epoll_wait(events, epfd, timeout_ms)?, Vec::new()));
        }
        let pollfd = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLPRI,
            revents: 0,
        };
        self.pollfds.clear();
        self.pollfds.push(pollfd(epfd));
        for h in &self.waiting {
            // Every handle in `waiting` is watched (an unwatch takes it out),
            // so this is the pump's own descriptor, open; -1 is skipped.
            let fd = self.watches.get(h).map_or(-1, |w| w.fd.as_raw_fd());
            self.pollfds.push(pollfd(fd));
        }
        match crate::sys::fd::poll(&mut self.pollfds, timeout_ms) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Some((0, Vec::new())),
            Err(e) => {
                log::error!("event pump: poll: {e}");
                return None;
            }
        }
        let n = if self.pollfds[0].revents != 0 {
            epoll_wait(events, epfd, 0)?
        } else {
            0
        };
        // Anything but nothing: a readable descriptor, and a hung-up or
        // failed one too, which the guest must hear of to find out.
        let ready = self
            .waiting
            .iter()
            .zip(&self.pollfds[1..])
            .filter(|(_, p)| p.fd >= 0 && p.revents != 0)
            .map(|(&h, _)| h)
            .collect();
        Some((n, ready))
    }

    fn apply_commands(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(PumpCmd::Watch { handle, fd, mode }) => {
                    if let Some(old) = self.unwatch(handle)
                        && old.cookie() != mode.cookie()
                    {
                        self.outbox.forget(handle, old.cookie());
                    }
                    if let Some(c) = self.fired.remove(&handle)
                        && Some(c) != mode.cookie()
                    {
                        self.outbox.forget_cookie(c);
                    }
                    // Decided once, as the watch starts: a descriptor is
                    // put in epoll only now, before the guest can have
                    // waited on it, never later (see the module's notes).
                    let polled = mode == WatchMode::LegacyArmed && self.polled < POLLED_MAX;
                    let w = Watched {
                        fd: PrivateFd::new(fd),
                        mode,
                        armed: false,
                        polled,
                        dirty: false,
                    };
                    if self.arm(handle, &w) {
                        self.polled += usize::from(polled);
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
                    let cookie = self.unwatch(handle).and_then(|m| m.cookie());
                    self.outbox.forget(handle, cookie);
                    if let Some(c) = self.fired.remove(&handle) {
                        self.outbox.forget_cookie(c);
                    }
                }
                Ok(PumpCmd::Retire { handle, hold }) => self.retire(handle, hold),
                Ok(PumpCmd::SetV2(v2)) => self.outbox.set_v2(v2),
                Ok(PumpCmd::Reset) => {
                    for h in self.watches.keys().copied().collect::<Vec<_>>() {
                        self.unwatch(h);
                    }
                    self.fired.clear();
                    self.outbox.clear();
                }
                Ok(PumpCmd::Arm { handle }) => self.arm_legacy(handle),
                Ok(PumpCmd::Hotplug { card, flags }) => self.outbox.hotplug(card, flags),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// [`PumpCmd::Retire`]: a readiness not read yet is reported now (the
    /// descriptor is one-shot and not drained, so it is still readable if
    /// it ever fired), whatever is left of the watch ends, and `hold` stays
    /// with the record, if one is queued.
    fn retire(&mut self, handle: u32, hold: Box<dyn Send>) {
        self.on_ready_if_readable(handle);
        // Never fired: nothing to report, ever.
        self.unwatch(handle);
        match self.fired.remove(&handle) {
            Some(c) => self.outbox.hold(c, hold),
            None => drop(hold),
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

    /// The guest waits on [`WatchMode::LegacyArmed`] handle `handle`: an
    /// event it has not been told of goes out now, else the next one will.
    fn arm_legacy(&mut self, handle: u32) {
        let Some(w) = self.watches.get_mut(&handle) else {
            return;
        };
        if w.mode != WatchMode::LegacyArmed {
            return;
        }
        PACING.arms.fetch_add(1, Relaxed);
        // The look that decides: one poll, which reports what it clears.
        if w.dirty || readable(w.fd.as_raw_fd()) {
            w.dirty = false;
            w.armed = false;
            self.waiting.retain(|&h| h != handle);
            self.outbox.ready_legacy(handle);
        } else if !w.armed {
            w.armed = true;
            if w.polled {
                self.waiting.push(handle);
            }
        }
    }

    fn on_ready(&mut self, handle: u32) {
        let Some(w) = self.watches.get_mut(&handle) else {
            return;
        };
        if w.mode == WatchMode::LegacyArmed {
            // Once per arm. Not swept: an event taken while unarmed is
            // remembered instead, and the arm looks at the descriptor too.
            if w.armed {
                w.armed = false;
                w.dirty = false;
                self.waiting.retain(|&h| h != handle);
                self.outbox.ready_legacy(handle);
            } else {
                w.dirty = true;
                PACING.ev_unarmed.fetch_add(1, Relaxed);
            }
            return;
        }
        let (fd, mode) = (w.fd.as_raw_fd(), w.mode);
        if mode.swept() {
            // Reported; the sweep looks again until it finds it drained.
            self.stale.insert(handle);
        }
        match mode {
            WatchMode::Legacy | WatchMode::LegacyArmed => self.outbox.ready_legacy(handle),
            WatchMode::Ready {
                cookie,
                oneshot,
                consume,
            } => {
                if consume && readable(fd) {
                    // Poll said the read would not block.
                    crate::sys::fd::eventfd_drain(fd);
                }
                self.outbox.ready(cookie);
                if oneshot {
                    self.unwatch(handle);
                    self.fired.insert(handle, cookie);
                }
            }
            WatchMode::Fence { cookie } => {
                let (status, signalled_at) = crate::hostfd::sync_file_signalled(fd)
                    .unwrap_or_else(|e| (-e.raw_os_error().unwrap_or(libc::EIO), 0));
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
                self.outbox.fence_at(cookie, status, signalled_at);
                self.unwatch(handle);
                self.fired.insert(handle, cookie);
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
            // Poll said this will not block, and the file is O_NONBLOCK
            // besides.
            let n = match crate::sys::fd::read_raw(fd, &mut self.buf) {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let bytes = self.buf[..n].to_vec();
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

    /// The safety net under a missed edge: ask the descriptors reported
    /// readable since they were last found drained, and no others. One still
    /// readable is reported again (coalesced with anything undelivered); one
    /// drained leaves the set, and its next edge brings it back.
    fn sweep(&mut self) {
        let mut due = Vec::new();
        self.stale.retain(|h| {
            let still = self
                .watches
                .get(h)
                .is_some_and(|w| w.mode.swept() && readable(w.fd.as_raw_fd()));
            if still {
                due.push(*h);
            }
            still
        });
        PACING.sweeps.fetch_add(1, Relaxed);
        PACING.ev_sweep.fetch_add(due.len() as u64, Relaxed);
        for h in due {
            self.on_ready(h);
        }
    }

    /// Fill posted buffers until the outbox is empty or the guest has none.
    /// Whether anything went out.
    fn flush(&mut self) -> bool {
        let mut asked = false;
        let mut filled = false;
        while !self.outbox.is_empty() {
            let outbox = &mut self.outbox;
            match self.queue.fill(&mut |cap| outbox.build(cap)) {
                Fill::Filled => filled = true,
                // What is waiting does not fit the buffers this guest posts.
                // Wait for the next round rather than burn through them all.
                Fill::Empty => return filled,
                Fill::NoBuffer => {
                    if asked || !self.queue.want_kick() {
                        PACING.no_buffer.fetch_add(1, Relaxed);
                        return filled;
                    }
                    asked = true;
                }
            }
        }
        filled
    }
}

/// Whether reading `fd` would return something now.
fn readable(fd: RawFd) -> bool {
    crate::sys::fd::readable(fd, 0)
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
        assert_eq!(recs[1].2, [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(recs[2].2.len(), 56);
        assert_eq!(recs[4].2[..4], EV_HOTPLUG_F_LEASE.to_le_bytes());
        assert!(o.is_empty());
    }

    #[test]
    fn a_fence_record_carries_the_hosts_signal_time_after_the_status() {
        let mut o = v2();
        o.fence_at(7, 1, 0x1122_3344_5566_7788);
        let recs = records(&o.build(EVENT_BUF_SIZE));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].2.len(), 16);
        assert_eq!(recs[0].2[..8], [1, 0, 0, 0, 0, 0, 0, 0], "the old 8 bytes");
        assert_eq!(recs[0].2[8..], 0x1122_3344_5566_7788u64.to_le_bytes());
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
        assert_eq!(whole_events(&s, 64), Some(64));
        assert_eq!(whole_events(&s, 63), Some(32));
        assert_eq!(whole_events(&s, 31), Some(0));
        assert_eq!(
            whole_events(&s[..40], 64),
            Some(32),
            "a partial tail is left"
        );
        let mut short = drm_event(1, 8);
        short[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(whole_events(&short, 64), None);
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
        crate::sys::fd::pipe2(libc::O_CLOEXEC | libc::O_NONBLOCK).unwrap()
    }

    fn write_all(fd: &OwnedFd, b: &[u8]) {
        assert_eq!(crate::sys::fd::write(fd, b).unwrap(), b.len());
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

    fn drain(fd: &OwnedFd) {
        let mut b = [0u8; 64];
        while crate::sys::fd::read_raw(fd.as_raw_fd(), &mut b).is_ok_and(|n| n > 0) {}
    }

    fn legacy_reports(q: &FakeQueue, handle: u32) -> usize {
        q.take()
            .iter()
            .flat_map(|m| records(m))
            .filter(|(k, c, _)| *k == EV_READY && *c == handle as u64)
            .count()
    }

    /// Armed readiness (BCAP_ARMED_READY): an RM descriptor's events go
    /// out once per arm. Nobody waiting, nothing is sent however many come,
    /// and nothing looks at the descriptor, so the events stay in it; the
    /// arm then reports at once what came meanwhile, and after a report the
    /// next event waits for the next arm.
    #[test]
    fn armed_readiness_reports_once_per_arm_and_leaves_unarmed_events_in_the_file() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 12,
            fd: r.try_clone().unwrap(),
            mode: WatchMode::LegacyArmed,
        });
        q.post(16, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        // Nobody waits: a burst of events sends nothing, wakes nothing, and
        // is no sweep's.
        for _ in 0..5 {
            write_all(&w, b"x");
        }
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let woke = pump.wait(&mut events, 0).map(|(n, ready)| (n, ready.len()));
        assert_eq!(
            woke,
            Some((0, 0)),
            "the descriptor is in nothing the pump waits on"
        );
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 0);
        assert!(pump.stale.is_empty() && pump.waiting.is_empty());
        // The arm reports what came unarmed: it is still in the file.
        h.send(PumpCmd::Arm { handle: 12 });
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 1);
        // Reported: disarmed until the next arm.
        drain(&r);
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 0);
        h.send(PumpCmd::Arm { handle: 12 });
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 1, "the event after the report");
        // Armed with nothing new: the wait polls it, and the next event is
        // reported, once.
        drain(&r);
        h.send(PumpCmd::Arm { handle: 12 });
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 0);
        assert_eq!(pump.waiting, vec![12]);
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 12), 1);
        assert!(pump.waiting.is_empty());
    }

    /// Past POLLED_MAX, an armed-readiness watch is in epoll from its start,
    /// as every one was before: an event wakes the pump and is remembered
    /// while nobody waits, and the arm reports it.
    #[test]
    fn armed_watches_past_the_bound_stay_in_epoll() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        pump.polled = POLLED_MAX;
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 9,
            fd: r.try_clone().unwrap(),
            mode: WatchMode::LegacyArmed,
        });
        q.post(16, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        assert!(!pump.watches[&9].polled);
        write_all(&w, b"x");
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 9), 0);
        assert!(pump.watches[&9].dirty, "the edge was seen, and remembered");
        drain(&r);
        h.send(PumpCmd::Arm { handle: 9 });
        pump.step_with_timeout(0);
        assert_eq!(legacy_reports(&q, 9), 1);
        assert!(pump.waiting.is_empty());
        h.send(PumpCmd::Unwatch { handle: 9 });
        pump.step_with_timeout(0);
        assert_eq!(pump.polled, POLLED_MAX, "an epoll watch was never counted");
    }

    /// An armed descriptor that becomes readable while the pump sleeps wakes
    /// it: the wait polls it besides the epoll set.
    #[test]
    fn an_armed_descriptor_wakes_a_sleeping_pump() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 5,
            fd: r.try_clone().unwrap(),
            mode: WatchMode::LegacyArmed,
        });
        q.post(16, EVENT_BUF_SIZE);
        h.send(PumpCmd::Arm { handle: 5 });
        pump.step_with_timeout(0);
        assert_eq!(pump.waiting, vec![5]);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            write_all(&w, b"x");
            w
        });
        let t = Instant::now();
        pump.step_with_timeout(5_000);
        assert!(
            t.elapsed() < Duration::from_secs(4),
            "woken by the event, not the timeout"
        );
        assert_eq!(legacy_reports(&q, 5), 1);
        drop(writer.join());
        drop(r);
    }

    /// A plain legacy watch (an older guest, the modeset device, a Wayland
    /// channel) reports every event, as before, and an arm of it is ignored.
    #[test]
    fn a_plain_legacy_watch_reports_every_event_and_ignores_arms() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 7,
            fd: r.try_clone().unwrap(),
            mode: WatchMode::Legacy,
        });
        q.post(16, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        h.send(PumpCmd::Arm { handle: 7 });
        for _ in 0..3 {
            write_all(&w, b"x");
            pump.step_with_timeout(0);
            drain(&r);
        }
        assert_eq!(legacy_reports(&q, 7), 3);
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

    /// S-21: idle handles cost the sweep nothing. Only a handle reported
    /// readable is polled again, until a sweep finds it drained; with none
    /// the pump waits without a timeout.
    #[test]
    fn the_sweep_polls_only_handles_left_readable_after_a_report() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        let mut ends = Vec::new();
        for handle in 0..100 {
            let (r, w) = pipe();
            h.send(PumpCmd::Watch {
                handle,
                fd: r,
                mode: WatchMode::Legacy,
            });
            ends.push(w);
        }
        pump.step_with_timeout(0);
        assert!(pump.stale.is_empty(), "a hundred idle handles, none swept");

        // One becomes readable: reported, and kept for the sweep while it
        // stays so, as a guest that has not consumed its event yet.
        write_all(&ends[7], b"x");
        pump.step_with_timeout(0);
        assert_eq!(pump.stale, HashSet::from([7]));
        pump.sweep();
        assert_eq!(pump.stale, HashSet::from([7]));

        // Drained: the next sweep drops it, and nothing is left to poll.
        let mut b = [0u8; 1];
        let _ = crate::sys::fd::read_raw(pump.watches[&7].fd.as_raw_fd(), &mut b);
        pump.sweep();
        assert!(pump.stale.is_empty());
    }

    /// S-33: the pump's duplicate is usually a file's last reference once
    /// the queue thread has closed its own; it goes to the closer, and is
    /// closed there, not on the pump thread.
    #[test]
    fn an_unwatched_files_duplicate_is_closed_by_the_closer() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        let (r, w) = pipe();
        h.send(PumpCmd::Watch {
            handle: 5,
            fd: r,
            mode: WatchMode::Drm,
        });
        pump.step_with_timeout(0);
        h.send(PumpCmd::Unwatch { handle: 5 });
        pump.step_with_timeout(0);
        assert!(crate::closer::wait_idle(Duration::from_secs(5)));
        // With the only read end closed, a write finds no reader.
        match crate::sys::fd::write(&w, b"x") {
            // A forked test child's copy (testfd.rs).
            Ok(_) => assert!(crate::testfd::only_end_here(std::os::fd::AsFd::as_fd(&w))),
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EPIPE)),
        }
    }

    /// A guest re-watching one signalled handle under a fresh cookie each
    /// time, and posting no buffers, queues one record, not one a watch;
    /// and closing the handle takes it.
    #[test]
    fn re_watching_a_fired_handle_under_new_cookies_queues_no_more_records() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let (r, w) = pipe();
        write_all(&w, b"x");
        for cookie in (1u64 << 33)..(1 << 33) + 100 {
            h.send(PumpCmd::Watch {
                handle: 9,
                fd: r.try_clone().unwrap(),
                mode: WatchMode::Ready {
                    cookie,
                    oneshot: true,
                    consume: false,
                },
            });
            pump.step_with_timeout(0);
        }
        assert_eq!(pump.outbox.order.len(), 1, "the last cookie's record only");
        h.send(PumpCmd::Unwatch { handle: 9 });
        pump.step_with_timeout(0);
        assert!(pump.outbox.is_empty());
    }

    /// nvgpu-syncobj-race's owners, every one asleep for a second on an
    /// eventfd whose point the host had signalled (the compat probe, twice
    /// in sixteen runs, with the host loaded): each owner's last WATCH was
    /// queued while the pump was between taking its instructions and
    /// draining its kick, the drain took those kicks, and the pump slept
    /// with the Watches still in the channel -- until an owner, given up on
    /// its eventfd, sent something else. The guest took no event record in
    /// all that time, nor for thirty seconds when the owners waited on
    /// (rig/guest-image/tools/syncobj-race.c's diagnosis). An instruction
    /// queued at that moment must leave the pump's next wait woken.
    #[test]
    fn an_instruction_queued_as_the_pump_takes_its_instructions_wakes_its_next_wait() {
        let q = FakeQueue::default();
        q.post(4, EVENT_BUF_SIZE);
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        // A registration's eventfd, its point already signalled; its Watch
        // is sent -- queued, then kicked -- just after the pump has taken
        // the instructions of the round the SetV2 woke.
        let efd = crate::hostfd::new_eventfd().unwrap();
        write_all(&efd, &1u64.to_ne_bytes());
        let cookie = 0xfeed_0000_0001;
        let mut late = Some((h.clone(), efd.try_clone().unwrap()));
        pump.after_commands = Some(Box::new(move || {
            if let Some((h, fd)) = late.take() {
                h.send(PumpCmd::Watch {
                    handle: 4,
                    fd,
                    mode: WatchMode::Ready {
                        cookie,
                        oneshot: true,
                        consume: false,
                    },
                });
            }
        }));
        pump.step_with_timeout(0);
        assert!(q.take().is_empty(), "the Watch came after this round's");
        assert!(
            readable(h.wake.as_raw_fd()),
            "its kick is still there for the next wait"
        );
        let t = Instant::now();
        pump.step_with_timeout(10_000);
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the next wait returned at once, not at its timeout ({:?})",
            t.elapsed()
        );
        assert_eq!(records(&q.take()[0]), vec![(EV_READY, cookie, vec![])]);
    }

    /// A hold whose drop a test can see: the count of `Arc` references.
    fn hold() -> (Arc<()>, Box<dyn Send>) {
        let a = Arc::new(());
        (a.clone(), Box::new(a))
    }

    fn ready_oneshot(h: &PumpHandle, handle: u32, cookie: u64) -> OwnedFd {
        let efd = crate::hostfd::new_eventfd().unwrap();
        h.send(PumpCmd::Watch {
            handle,
            fd: efd.try_clone().unwrap(),
            mode: WatchMode::Ready {
                cookie,
                oneshot: true,
                consume: false,
            },
        });
        efd
    }

    /// A syncobj wait registration's eventfd is retired as soon as it is
    /// seen to have fired -- possibly before the pump has read it. Its
    /// report still goes out, and what the handle held stays until it has.
    #[test]
    fn a_retired_handles_report_goes_out_and_its_hold_stays_until_it_has() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let efd = ready_oneshot(&h, 4, 0xc0ffee);
        pump.step_with_timeout(0);
        // Fired, and retired before the pump has looked.
        write_all(&efd, &1u64.to_ne_bytes());
        let (seen, held) = hold();
        h.send(PumpCmd::Retire {
            handle: 4,
            hold: held,
        });
        pump.step_with_timeout(0);
        assert!(pump.watches.is_empty() && pump.fired.is_empty());
        assert_eq!(pump.outbox.held(), 1, "no buffer yet: the record waits");
        assert_eq!(Arc::strong_count(&seen), 2, "and so does the hold");
        q.post(1, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        assert_eq!(records(&q.take()[0]), vec![(EV_READY, 0xc0ffee, vec![])]);
        assert_eq!(Arc::strong_count(&seen), 1, "sent: the hold is gone");
        assert_eq!(pump.outbox.held(), 0);

        // Reported already, and sent: nothing to keep.
        let efd = ready_oneshot(&h, 5, 0xbeef);
        write_all(&efd, &1u64.to_ne_bytes());
        q.post(1, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        assert_eq!(q.take().len(), 1);
        let (seen, held) = hold();
        h.send(PumpCmd::Retire {
            handle: 5,
            hold: held,
        });
        pump.step_with_timeout(0);
        assert_eq!(Arc::strong_count(&seen), 1);
        assert!(pump.fired.is_empty());
    }

    /// One that never fired (its syncobj freed with it) has nothing to
    /// report, ever: its watch ends and its hold goes at once.
    #[test]
    fn a_retired_handle_that_never_fired_keeps_nothing() {
        let q = FakeQueue::default();
        let (mut pump, h) = Pump::new(q.clone()).unwrap();
        h.send(PumpCmd::SetV2(true));
        let _efd = ready_oneshot(&h, 6, 1 << 33);
        let (seen, held) = hold();
        h.send(PumpCmd::Retire {
            handle: 6,
            hold: held,
        });
        q.post(1, EVENT_BUF_SIZE);
        pump.step_with_timeout(0);
        assert!(pump.watches.is_empty() && pump.outbox.is_empty());
        assert!(q.take().is_empty());
        assert_eq!(Arc::strong_count(&seen), 1);
    }

    /// Whatever drops a held record drops its hold too: the record is the
    /// only thing it is kept for.
    #[test]
    fn a_held_record_dropped_any_way_drops_its_hold() {
        for how in 0..3 {
            let q = FakeQueue::default();
            let (mut pump, h) = Pump::new(q.clone()).unwrap();
            h.send(PumpCmd::SetV2(true));
            let efd = ready_oneshot(&h, 7, 1 << 34);
            write_all(&efd, &1u64.to_ne_bytes());
            let (seen, held) = hold();
            h.send(PumpCmd::Retire {
                handle: 7,
                hold: held,
            });
            pump.step_with_timeout(0);
            assert_eq!(Arc::strong_count(&seen), 2);
            match how {
                0 => h.send(PumpCmd::Reset),
                1 => h.send(PumpCmd::SetV2(false)),
                // The cookie watched again on another handle, then closed.
                _ => {
                    let _efd = ready_oneshot(&h, 8, 1 << 34);
                    h.send(PumpCmd::Unwatch { handle: 8 });
                }
            }
            pump.step_with_timeout(0);
            assert_eq!(Arc::strong_count(&seen), 1, "case {how}");
            assert_eq!(pump.outbox.held(), 0);
        }
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
